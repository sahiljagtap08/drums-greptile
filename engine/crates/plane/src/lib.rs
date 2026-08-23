//! Where a repair actually runs (`docs/RUNNER.md`).
//!
//! ## Why this seam exists
//!
//! Everything else in the workspace can run anywhere: detection, attribution,
//! the record, the authority ladder, proposals, notifications. Only two steps
//! need the customer's code and the customer's credentials — reproduce and
//! repair — and those are exactly the two that must NOT move into our
//! infrastructure.
//!
//! Spec §20 says Team mode is "Drums cloud, reproductions in customer CI".
//! Anthropic's terms say the same thing from the other direction: a third
//! party may not route requests through a user's Pro or Max credentials on
//! their behalf. Both follow from one principle — **a credential belongs to
//! whoever owns it, and should be exercised where they control it.**
//!
//! So this trait is the line between "decide what should happen" (ours) and
//! "do the thing that needs your secrets" (theirs).
//!
//! ## The honesty problem this creates
//!
//! When Drums runs a check in-process, a `Verified` claim means *this code
//! executed that check and observed the result*. When a remote plane reports
//! one, it means *something told us it ran*. Those are not the same, and
//! collapsing them would be the same class of dishonesty as fabricating a
//! claim outright.
//!
//! [`AssertedClaim`] therefore carries **who asserted it**. A claim from
//! `local` was observed here. A claim from `github-actions` was reported by a
//! runner we do not control. The record keeps that distinction, the PR body
//! shows it, and no code path is permitted to drop it — see
//! [`AssertedClaim::provenance_note`].

pub mod local;

use std::path::PathBuf;

use async_trait::async_trait;
use engine_core::{Attribution, Claim, Failure, Provenance};
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
    #[error("{plane} is not configured: {detail}")]
    NotConfigured { plane: &'static str, detail: String },
    #[error("{plane} refused the job: {detail}")]
    Refused { plane: &'static str, detail: String },
    #[error("could not reach {plane}: {detail}")]
    Unreachable { plane: &'static str, detail: String },
    #[error("{plane} accepted the job but it never reported back within {secs}s")]
    TimedOut { plane: &'static str, secs: u64 },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Which repository, in which account. Present from the first version because
/// retrofitting a repo scope onto the record and the authority ladder later is
/// the kind of migration nobody enjoys — see `docs/RUNNER.md` §4b.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RepoRef {
    /// The account that owns this repository. `None` in local mode, which
    /// deliberately has no account at all.
    pub account: Option<String>,
    /// `owner/name` as the forge spells it. A LABEL: it changes when a repo is
    /// renamed or transferred, so it is for humans and never for binding.
    pub slug: String,
    /// The forge's immutable identifier. This is what an attestation binds to
    /// and what the authority ladder keys on, because a rename must not move
    /// an earned rung to whatever now occupies the old name.
    pub repository_id: String,
    /// Which deployment this scope covers — `production`, `staging`. A class
    /// that earned act-alone against staging has earned NOTHING about
    /// production: same repo, same service, same error name, entirely
    /// different consequence. `None` is its own scope, never a wildcard.
    pub environment: Option<String>,
}

impl RepoRef {
    pub fn local(slug: impl Into<String>) -> Self {
        let slug = slug.into();
        Self { account: None, repository_id: slug.clone(), slug, environment: None }
    }

    /// Stable key for scoping the record and the authority ladder.
    ///
    /// Built from the immutable id and the environment, NOT the slug: a
    /// rename must not silently reset a class's earned history, and staging
    /// must never share a scope with production.
    pub fn key(&self) -> String {
        let env = self.environment.as_deref().unwrap_or("unknown");
        match &self.account {
            Some(a) => format!("{a}/{}/{env}", self.repository_id),
            None => format!("{}/{env}", self.repository_id),
        }
    }
}

/// One unit of work handed to an execution plane.
///
/// Deliberately carries no credential of any kind. Whatever the plane needs to
/// clone the repo, drive an agent, or run tests is already present where the
/// plane runs — that is the entire point of the arrangement, and a secret
/// field on this struct would be the first step back toward the design the
/// terms forbid.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepairJob {
    /// Stable id, so a plane reporting back can be matched to what was sent.
    pub job_id: String,
    pub repo: RepoRef,
    /// The revision to repair at.
    pub rev: String,
    pub failure: Failure,
    pub attribution: Option<Attribution>,
    /// What the repair has to achieve, in the plane's own terms. Restated from
    /// `engine_repair::RepairContext` so a remote plane does not need to
    /// deserialise our internal types.
    pub acceptance: Vec<String>,
    /// The workflow this job must run in. An attestation naming a different
    /// workflow describes a different execution, however genuine its
    /// signature.
    pub expected_workflow: String,
    /// How long before the caller stops waiting and records an honest
    /// `unresolved` rather than leaving the failure stuck at "repairing".
    pub timeout_secs: u64,
}

/// What binds a remote outcome to the job that was dispatched.
///
/// Without this, a remote `Verified` is a sentence in a JSON payload. With it,
/// the forge attests that this specific run happened on this specific commit.
/// The fields are exactly the ones GitHub Actions' OIDC token carries, because
/// that is the mechanism this is designed around — see `docs/RUNNER.md` §5b.
/// The CLAIMS a forge made, as they arrive on the wire.
///
/// Deserialising one of these proves nothing — it is attacker-controlled JSON
/// until a verifier says otherwise. That is why the gate cannot accept it:
/// only [`VerifiedAttestation`], which nothing outside a verifier can
/// construct, is admissible.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attestation {
    /// Must equal the dispatched `RepairJob::job_id`.
    pub job_id: String,
    /// `owner/name`, from the token's `repository` claim. Narration only.
    pub repository: String,
    /// From the token's `repository_id` claim — immutable across renames and
    /// transfers, and therefore the field that actually binds.
    pub repository_id: String,
    /// The commit actually checked out.
    pub sha: String,
    /// Identifies the execution, and makes a replay detectable.
    pub workflow: String,
    pub run_id: String,
    /// Digest over the check output the claims summarise, so a claim cannot be
    /// edited after the fact without invalidating this.
    pub evidence_digest: String,
}

impl Attestation {
    /// True when this attestation describes `job` in EVERY field that binds it.
    ///
    /// Review found an earlier version compared only job id, sha and
    /// repository — so replaying another run for the same job, or editing the
    /// evidence a claim summarises, left this true. Every field that
    /// distinguishes one execution from another is compared here.
    ///
    /// `repository_id` rather than `owner/name`: repositories get renamed and
    /// transferred, and a slug is a label while the id is the thing.
    pub fn binds(&self, job: &RepairJob, expected_evidence_digest: &str) -> bool {
        self.job_id == job.job_id
            && self.sha == job.rev
            && self.repository_id == job.repo.repository_id
            && self.workflow == job.expected_workflow
            && self.evidence_digest == expected_evidence_digest
            && !self.run_id.is_empty()
    }
}

/// An attestation a verifier has actually checked.
///
/// The point of this type is that **its field is private and its only
/// constructor lives behind signature verification**. A caller cannot build
/// one from deserialised JSON, cannot construct one in a hurry to make a test
/// pass, and cannot pass a raw [`Attestation`] to the gate by mistake — the
/// types do not line up.
///
/// This is the shape review asked for: "make raw attestations impossible to
/// pass directly into the gate."
#[derive(Debug, Clone)]
pub struct VerifiedAttestation(Attestation);

impl VerifiedAttestation {
    /// ONLY for implementors of [`AttestationVerifier`]. Calling this without
    /// having verified a signature defeats the entire type.
    ///
    /// It is not `pub` — a verifier lives in this crate or is given one via
    /// the trait, and nothing downstream can reach it.
    pub(crate) fn trusted(attestation: Attestation) -> Self {
        Self(attestation)
    }

    pub fn claims(&self) -> &Attestation {
        &self.0
    }
}

/// Turns wire bytes into a [`VerifiedAttestation`], or refuses.
///
/// Implementations verify a signature — for GitHub Actions, an OIDC token
/// against GitHub's JWKS — and then check that the resulting claims bind to
/// the job. Both halves are required: a valid signature over claims about a
/// different run is a genuine token and irrelevant evidence.
#[async_trait]
pub trait AttestationVerifier: Send + Sync {
    async fn verify(
        &self,
        raw_token: &str,
        job: &RepairJob,
        expected_evidence_digest: &str,
    ) -> Result<VerifiedAttestation, DispatchError>;
}

/// A claim, plus who is asserting it.
///
/// The whole reason this type exists instead of a bare [`Claim`]: a `Verified`
/// observed in-process and a `Verified` reported by somebody else's CI runner
/// are different strengths of the same word, and the difference must survive
/// all the way to the pull request a human reads.
#[derive(Debug, Clone)]
pub struct AssertedClaim {
    pub claim: Claim,
    /// The plane's `name()`. `local` means this process watched it happen.
    pub asserted_by: String,
    /// Present only when a verifier CHECKED a forge's attestation. `None` on
    /// every local claim (nothing to attest — this process watched it) and on
    /// any remote claim whose token was absent, unverifiable, or bound to a
    /// different run.
    pub attestation: Option<VerifiedAttestation>,
}

impl AssertedClaim {
    /// Observed by this process, in this process.
    pub fn local(claim: Claim) -> Self {
        Self { claim, asserted_by: LOCAL_PLANE.to_string(), attestation: None }
    }

    /// A claim reported by a remote plane, with whatever binding it came with.
    pub fn remote(claim: Claim, plane: &str, attestation: Option<VerifiedAttestation>) -> Self {
        Self { claim, asserted_by: plane.to_string(), attestation }
    }

    /// Whether this claim may support acting without a human.
    ///
    /// THE GATE this type exists for. A locally observed claim qualifies
    /// because this process watched the check run. A remote claim qualifies
    /// only when a forge attested the run and that attestation describes the
    /// job that was actually dispatched.
    ///
    /// Note what this is NOT: a claim that fails here is not discarded or
    /// disbelieved. It still reaches the pull request and still informs a
    /// human. It simply cannot unlock autonomy — see [`Self::demoted`].
    pub fn may_support_autonomy(&self, job: &RepairJob, expected_evidence_digest: &str) -> bool {
        // PROVENANCE FIRST. Review found this check missing entirely, which
        // meant the firsthand `Unresolved` that `LocalPlane::timed_out`
        // produces — a claim whose whole content is "nothing was established"
        // — reported that it could support acting without a human.
        //
        // Only `Verified` qualifies. `Approved` is a human's consent, not
        // evidence, and cannot stand in for a check that ran.
        if self.claim.provenance != Provenance::Verified {
            return false;
        }
        if self.is_firsthand() {
            return true;
        }
        match &self.attestation {
            Some(a) => a.claims().binds(job, expected_evidence_digest),
            None => false,
        }
    }

    /// This claim as it should be RECORDED when it cannot support autonomy.
    ///
    /// An unattested remote `Verified` becomes `Observed` with a note saying
    /// why. Refusing it outright would lose real information; keeping it as
    /// `Verified` would launder an assertion into a proof. `Observed` is the
    /// honest middle: something saw this, and we cannot check it.
    pub fn demoted(&self) -> Claim {
        if self.claim.provenance != Provenance::Verified {
            return self.claim.clone();
        }
        Claim {
            text: format!(
                "{} (reported by {} without a verifiable attestation, so recorded as observed)",
                self.claim.text, self.asserted_by
            ),
            provenance: Provenance::Observed,
        }
    }

    /// True when the claim was observed by the code making the assertion,
    /// rather than reported by something else.
    pub fn is_firsthand(&self) -> bool {
        self.asserted_by == LOCAL_PLANE
    }

    /// The sentence that goes next to a claim whose strength depends on who
    /// said it. `None` for firsthand claims — adding a caveat to every line
    /// trains readers to skip all of them, so it appears only where it means
    /// something.
    ///
    /// Note it fires only for `Verified`: `unresolved` reported remotely is
    /// still just unresolved, and a remote `observed` claims nothing a reader
    /// would over-trust.
    pub fn provenance_note(&self) -> Option<String> {
        if self.is_firsthand() || self.claim.provenance != Provenance::Verified {
            return None;
        }
        Some(format!(
            "reported by {}, not observed by Drums directly",
            self.asserted_by
        ))
    }
}

/// What a plane reports when the job is done.
#[derive(Debug, Clone)]
pub struct JobOutcome {
    pub job_id: String,
    /// True only when the plane says the repair cleared its acceptance bar.
    /// A plane that is unsure must say `false` — there is no third state,
    /// because an ambiguous outcome that defaults to success is how a bad
    /// repair reaches a person as a good one.
    pub repaired: bool,
    pub branch: Option<String>,
    pub sha: Option<String>,
    pub summary: String,
    pub claims: Vec<AssertedClaim>,
    /// Present whenever `repaired` is false. Never `None` on a failure —
    /// silence about why is the thing `docs/CONTRACTS.md` calls a miss.
    pub failure_reason: Option<String>,
    /// Where a human can look, when the plane left something behind.
    pub workspace: Option<PathBuf>,
}

/// The name every in-process implementation reports.
pub const LOCAL_PLANE: &str = "local";

#[async_trait]
pub trait ExecutionPlane: Send + Sync {
    /// Stable identifier, recorded on every claim this plane asserts.
    fn name(&self) -> &'static str;

    /// True when this plane can actually run a job right now. Checked at
    /// STARTUP by callers, so a misconfiguration is discovered before a
    /// failure is detected rather than after a repair was already needed.
    async fn available(&self) -> Result<(), DispatchError>;

    /// Run one repair job to completion.
    ///
    /// Implementations that dispatch remotely are expected to wait for the
    /// outcome here rather than returning early — the caller's pipeline is
    /// already spawned per failure, and a callback-shaped API would push the
    /// correlation problem into every caller. `RepairJob::timeout_secs` bounds
    /// the wait.
    async fn run(&self, job: &RepairJob) -> Result<JobOutcome, DispatchError>;
}

/// Fixtures shared between this crate's test modules.
#[cfg(test)]
pub(crate) mod tests_support {
    use engine_core::{Claim, ErrorEvent, ErrorSignature, Failure, Intake, Provenance};

    pub fn sample_failure() -> Failure {
        Failure {
            id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
            service: "shop".into(),
            signature: ErrorSignature {
                error_name: "TypeError".into(),
                top_frame_file: "server.js".into(),
                top_frame_function: None,
            },
            first_seen_ms: 1,
            event_count: 1,
            sample: ErrorEvent {
                service: "shop".into(),
                occurred_at_ms: 1,
                error_name: "TypeError".into(),
                error_message: "boom".into(),
                stack: String::new(),
                request: None,
                intake: Intake::Snippet,
            },
            intake: Intake::Snippet,
            claim: Claim { text: "observed".into(), provenance: Provenance::Observed },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claim(text: &str, p: Provenance) -> Claim {
        Claim { text: text.into(), provenance: p }
    }

    #[test]
    fn a_local_claim_is_firsthand_and_carries_no_caveat() {
        let c = AssertedClaim::local(claim("the failing request now returns 200", Provenance::Verified));
        assert!(c.is_firsthand());
        assert_eq!(
            c.provenance_note(),
            None,
            "a caveat on every line trains readers to skip all of them"
        );
    }

    #[test]
    fn a_remote_verified_claim_says_who_verified_it() {
        let c = AssertedClaim::remote(
            claim("the app's own test script passed", Provenance::Verified),
            "github-actions",
            None,
        );
        assert!(!c.is_firsthand());
        let note = c.provenance_note().expect("a remote verified claim must be qualified");
        assert!(note.contains("github-actions"), "{note}");
        assert!(
            note.contains("not observed by Drums directly"),
            "the qualification must say what is actually weaker: {note}"
        );
    }

    /// A remote `unresolved` is still just unresolved — qualifying it would
    /// add noise without adding information.
    #[test]
    fn only_verified_claims_get_a_remote_caveat() {
        for p in [Provenance::Unresolved, Provenance::Observed, Provenance::Inferred, Provenance::Approved] {
            let c = AssertedClaim::remote(claim("x", p), "github-actions", None);
            assert_eq!(
                c.provenance_note(),
                None,
                "{p:?} reported remotely claims nothing a reader would over-trust"
            );
        }
    }

    #[test]
    fn a_repo_key_is_stable_and_scopes_by_account_when_there_is_one() {
        assert_eq!(RepoRef::local("acme/api").key(), "acme/api/unknown");
        assert_eq!(
            RepoRef {
                account: Some("drums-labs".into()),
                slug: "acme/api".into(),
                repository_id: "R_kgDOA".into(),
                environment: Some("production".into()),
            }
            .key(),
            "drums-labs/R_kgDOA/production"
        );
    }

    /// The authority ladder keys off this. Two repos with the same service and
    /// error name must not share an earned rung — different code, different
    /// blast radius, different people on call.
    #[test]
    fn two_repos_are_never_the_same_scope() {
        let a = RepoRef {
            account: Some("acme".into()),
            slug: "acme/payments".into(),
            repository_id: "R_payments".into(),
            environment: Some("production".into()),
        };
        let b = RepoRef {
            account: Some("acme".into()),
            slug: "acme/web".into(),
            repository_id: "R_web".into(),
            environment: Some("production".into()),
        };
        assert_ne!(a.key(), b.key());
        assert_ne!(a, b);
    }

    /// A `RepairJob` must never grow a credential field.
    ///
    /// Structural, not stringly: the assertion reads the SERIALISED key set,
    /// so adding `token` or `api_key` to the struct fails this test. The whole
    /// arrangement — §20, and Anthropic's terms on routing subscription
    /// credentials — depends on the plane already having what it needs where
    /// it runs. A secret on this struct is the first step back to the design
    /// the terms forbid.
    #[test]
    fn a_repair_job_carries_no_credential() {
        let job = RepairJob {
            job_id: "j1".into(),
            repo: RepoRef::local("acme/api"),
            rev: "abc1234".into(),
            failure: crate::tests_support::sample_failure(),
            attribution: None,
            acceptance: vec!["the failing request returns 200".into()],
            expected_workflow: "drums-repair.yml".into(),
            timeout_secs: 900,
        };
        let value = serde_json::to_value(&job).expect("RepairJob must serialise");
        let keys: Vec<&String> = value.as_object().expect("an object").keys().collect();

        assert_eq!(
            keys.len(),
            8,
            "RepairJob gained or lost a field — if you added one, check it is not a \
             credential and update this count deliberately: {keys:?}"
        );
        for key in &keys {
            let k = key.to_lowercase();
            for banned in ["token", "secret", "key", "credential", "password", "auth"] {
                assert!(
                    !k.contains(banned),
                    "RepairJob must never carry a {banned} — the plane already has what \
                     it needs where it runs (docs/RUNNER.md §3). Found: {key}"
                );
            }
        }
    }

}

#[cfg(test)]
mod attestation_tests {
    use super::*;
    use crate::tests_support::sample_failure;

    fn job() -> RepairJob {
        RepairJob {
            job_id: "job-1".into(),
            repo: RepoRef {
                account: Some("acme".into()),
                slug: "acme/api".into(),
                repository_id: "R_api".into(),
                environment: Some("production".into()),
            },
            rev: "abc1234".into(),
            failure: sample_failure(),
            attribution: None,
            acceptance: vec![],
            expected_workflow: "drums-repair.yml".into(),
            timeout_secs: 900,
        }
    }

    const DIGEST: &str = "sha256:deadbeef";

    /// Only a verifier may mint one of these in production. Tests reach the
    /// crate-private constructor precisely so that production code cannot.
    fn attested(a: Attestation) -> VerifiedAttestation {
        VerifiedAttestation::trusted(a)
    }

    fn good_attestation() -> Attestation {
        Attestation {
            job_id: "job-1".into(),
            repository: "acme/api".into(),
            repository_id: "R_api".into(),
            sha: "abc1234".into(),
            workflow: "drums-repair.yml".into(),
            run_id: "1234567890".into(),
            evidence_digest: DIGEST.into(),
        }
    }

    fn verified(text: &str) -> Claim {
        Claim { text: text.into(), provenance: Provenance::Verified }
    }

    #[test]
    fn a_locally_observed_claim_may_support_autonomy() {
        let c = AssertedClaim::local(verified("the failing request now returns 200"));
        assert!(c.may_support_autonomy(&job(), DIGEST), "this process watched the check run");
    }

    #[test]
    fn an_attested_remote_claim_may_support_autonomy() {
        let c = AssertedClaim::remote(
            verified("the app's own test script passed"),
            "github-actions",
            Some(attested(good_attestation())),
        );
        assert!(c.may_support_autonomy(&job(), DIGEST));
    }

    /// THE GAP this type was built to close. Before the attestation field
    /// existed, a payload that simply said `Verified` would have unlocked
    /// act-alone exactly as if Drums had watched the check itself.
    #[test]
    fn an_unattested_remote_claim_can_never_support_autonomy() {
        let c = AssertedClaim::remote(
            verified("the app's own test script passed"),
            "github-actions",
            None,
        );
        assert!(
            !c.may_support_autonomy(&job(), DIGEST),
            "a payload asserting `verified` is not evidence that anything ran"
        );
    }

    /// A genuine token for a DIFFERENT commit describes different code.
    #[test]
    fn an_attestation_for_another_sha_is_rejected() {
        let mut a = good_attestation();
        a.sha = "0000000".into();
        let c = AssertedClaim::remote(verified("tests passed"), "github-actions", Some(attested(a)));
        assert!(!c.may_support_autonomy(&job(), DIGEST));
    }

    /// A replayed attestation from an earlier job is genuine and irrelevant.
    #[test]
    fn a_replayed_attestation_from_another_job_is_rejected() {
        let mut a = good_attestation();
        a.job_id = "job-0".into();
        let c = AssertedClaim::remote(verified("tests passed"), "github-actions", Some(attested(a)));
        assert!(!c.may_support_autonomy(&job(), DIGEST));
    }

    #[test]
    fn an_attestation_from_another_repository_is_rejected() {
        let mut a = good_attestation();
        a.repository_id = "R_web".into();
        let c = AssertedClaim::remote(verified("tests passed"), "github-actions", Some(attested(a)));
        assert!(
            !c.may_support_autonomy(&job(), DIGEST),
            "a run in another repo proves nothing about this one"
        );
    }

    /// The reason binding uses `repository_id` and not the slug: repositories
    /// get renamed and transferred. A rename must not invalidate evidence
    /// about the same code, and — more dangerously — the freed-up name must
    /// not let a DIFFERENT repository inherit the old one's attestations.
    #[test]
    fn a_renamed_repository_still_binds_because_the_id_did_not_change() {
        let mut a = good_attestation();
        a.repository = "acme/api-renamed".into(); // slug changed, id did not
        let c = AssertedClaim::remote(verified("tests passed"), "github-actions", Some(attested(a)));
        assert!(
            c.may_support_autonomy(&job(), DIGEST),
            "a rename does not change what code was tested"
        );
    }

    /// Replaying another run of the SAME job, on the same commit, with a
    /// genuine token. Only run_id differs — which is exactly the field an
    /// earlier version of `matches` ignored.
    #[test]
    fn a_different_workflow_is_rejected() {
        let mut a = good_attestation();
        a.workflow = "some-other.yml".into();
        let c = AssertedClaim::remote(verified("tests passed"), "github-actions", Some(attested(a)));
        assert!(!c.may_support_autonomy(&job(), DIGEST));
    }

    /// Editing the evidence after the check ran must invalidate the claim
    /// that summarises it.
    #[test]
    fn a_mismatched_evidence_digest_is_rejected() {
        let c = AssertedClaim::remote(
            verified("tests passed"),
            "github-actions",
            Some(attested(good_attestation())),
        );
        assert!(
            !c.may_support_autonomy(&job(), "sha256:something-else"),
            "a claim must not survive its evidence being edited"
        );
    }

    /// THE bug review found: `LocalPlane::timed_out` produces a firsthand
    /// claim whose entire content is "nothing was established", and the gate
    /// reported it could support acting without a human.
    #[test]
    fn a_firsthand_unresolved_claim_can_never_support_autonomy() {
        let c = AssertedClaim::local(Claim {
            text: "the repair did not finish within 900s — nothing was established".into(),
            provenance: Provenance::Unresolved,
        });
        assert!(
            !c.may_support_autonomy(&job(), DIGEST),
            "evidence that explicitly established nothing must not authorise a deploy"
        );
    }

    /// An attested remote claim that is merely `Observed` is still not
    /// evidence a check passed.
    #[test]
    fn an_attested_but_non_verified_claim_can_never_support_autonomy() {
        for p in [Provenance::Observed, Provenance::Inferred, Provenance::Unresolved, Provenance::Approved] {
            let c = AssertedClaim::remote(
                Claim { text: "x".into(), provenance: p },
                "github-actions",
                Some(attested(good_attestation())),
            );
            assert!(
                !c.may_support_autonomy(&job(), DIGEST),
                "{p:?} is not evidence that a check ran and passed"
            );
        }
    }

    /// Not refused, not believed: demoted, with the reason attached.
    #[test]
    fn an_unattested_verified_claim_is_recorded_as_observed_with_the_reason() {
        let c = AssertedClaim::remote(
            verified("the app's own test script passed"),
            "github-actions",
            None,
        );
        let recorded = c.demoted();
        assert_eq!(recorded.provenance, Provenance::Observed);
        assert!(recorded.text.contains("the app's own test script passed"), "{}", recorded.text);
        assert!(recorded.text.contains("without a verifiable attestation"), "{}", recorded.text);
        assert!(recorded.text.contains("github-actions"), "{}", recorded.text);
    }

    /// Demotion applies to `verified` only — everything else already claims
    /// no more than it can support, and rewording it would be noise.
    #[test]
    fn demotion_leaves_weaker_claims_untouched() {
        for p in [Provenance::Observed, Provenance::Inferred, Provenance::Unresolved, Provenance::Approved] {
            let c = AssertedClaim::remote(
                Claim { text: "x".into(), provenance: p },
                "github-actions",
                None,
            );
            let d = c.demoted();
            assert_eq!(d.provenance, p);
            assert_eq!(d.text, "x", "{p:?} should not be reworded");
        }
    }
}

/// Constructors that only integration tests may reach.
///
/// `VerifiedAttestation`'s point is that production code cannot mint one
/// without verifying a signature. Integration tests live outside the crate, so
/// they need a door — this is it, `#[doc(hidden)]` and named so that anything
/// calling it in production reads as obviously wrong in review.
#[doc(hidden)]
pub mod testing {
    use super::{Attestation, VerifiedAttestation};

    /// Mint a verified attestation WITHOUT verifying anything. Tests only.
    pub fn attest(a: Attestation) -> VerifiedAttestation {
        VerifiedAttestation::trusted(a)
    }
}

// -- replay defence ----------------------------------------------------------

use std::collections::HashSet;
use std::sync::Mutex;

/// Run ids whose attestations have already been spent.
///
/// A valid attestation is valid forever unless something remembers it was
/// used. Without this, a result captured once can be replayed to authorise a
/// second unattended deploy — the signature still checks out, the binding
/// still matches, and nothing notices.
///
/// In-memory here because step 2 runs from one process. The hosted control
/// plane needs this durable (see `docs/RUNNER.md` §9a), and the trait boundary
/// is where that swap happens.
#[derive(Debug, Default)]
pub struct ConsumedRuns {
    seen: Mutex<HashSet<String>>,
}

impl ConsumedRuns {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record this run as spent. Returns false if it had already been spent,
    /// which is the caller's signal to refuse.
    ///
    /// Deliberately one operation rather than `contains` then `insert`: two
    /// calls leave a window where concurrent dispatches both see "unused".
    pub fn consume(&self, run_id: &str) -> bool {
        let mut seen = self.seen.lock().unwrap_or_else(|p| p.into_inner());
        seen.insert(run_id.to_string())
    }

    pub fn was_consumed(&self, run_id: &str) -> bool {
        let seen = self.seen.lock().unwrap_or_else(|p| p.into_inner());
        seen.contains(run_id)
    }
}

/// The [`engine_core::authority::Evidence`] a finished job presents to the
/// ship gate.
///
/// This type is the ONLY way an outcome reaches `ship_decision`, which is what
/// makes the check unbypassable: there is no other constructor, and the gate
/// takes no other shape.
pub struct OutcomeEvidence<'a> {
    pub job: &'a RepairJob,
    pub outcome: &'a JobOutcome,
    pub expected_evidence_digest: &'a str,
    pub consumed: &'a ConsumedRuns,
}

impl engine_core::authority::Evidence for OutcomeEvidence<'_> {
    fn has_actionable_verified_claim(&self) -> bool {
        // A replayed run authorises nothing, however well-formed. Checked
        // before any claim is considered, so a spent attestation cannot be
        // rescued by a claim that happens to look strong.
        if let Some(run_id) = self
            .outcome
            .claims
            .iter()
            .find_map(|c| c.attestation.as_ref().map(|a| a.claims().run_id.clone()))
        {
            if self.consumed.was_consumed(&run_id) {
                return false;
            }
        }
        self.outcome
            .claims
            .iter()
            .any(|c| c.may_support_autonomy(self.job, self.expected_evidence_digest))
    }

    fn ineligibility_detail(&self) -> String {
        if self.outcome.claims.is_empty() {
            return "the repair produced no claims at all".to_string();
        }
        if let Some(run_id) = self
            .outcome
            .claims
            .iter()
            .find_map(|c| c.attestation.as_ref().map(|a| a.claims().run_id.clone()))
        {
            if self.consumed.was_consumed(&run_id) {
                return format!("workflow run {run_id} was already used to authorise a ship");
            }
        }
        let verified = self
            .outcome
            .claims
            .iter()
            .filter(|c| c.claim.provenance == Provenance::Verified)
            .count();
        if verified == 0 {
            return "nothing in the repair's evidence reached `verified`".to_string();
        }
        let unattested = self
            .outcome
            .claims
            .iter()
            .filter(|c| {
                c.claim.provenance == Provenance::Verified
                    && !c.is_firsthand()
                    && c.attestation.is_none()
            })
            .count();
        if unattested > 0 {
            return format!(
                "{unattested} verified claim(s) came from {} without a verifiable attestation",
                self.outcome
                    .claims
                    .first()
                    .map(|c| c.asserted_by.as_str())
                    .unwrap_or("a remote plane")
            );
        }
        "no verified claim is bound to this job, revision and workflow run".to_string()
    }
}
