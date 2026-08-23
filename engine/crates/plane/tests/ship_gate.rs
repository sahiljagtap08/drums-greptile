//! What can and cannot reach an unattended deploy, asserted through the REAL
//! `ship_decision` rather than a helper beside it.
//!
//! Review found that the previous attestation logic had only test callers
//! while production called a gate that never saw the evidence. Every test here
//! therefore goes through `engine_core::authority::ship_decision` — the same
//! function `run_repair_pipeline` calls — because a safety property proven
//! against a different function is not proven.

use engine_core::authority::{ship_decision, ProposeReason, Rung, ShipDecision};
use engine_core::{
    Claim, ErrorEvent, ErrorSignature, Failure, Intake, Provenance,
};
use engine_plane::{
    AssertedClaim, Attestation, ConsumedRuns, JobOutcome, OutcomeEvidence, RepairJob, RepoRef,
};

const DIGEST: &str = "sha256:e3b0c44298fc1c14";

fn repo() -> RepoRef {
    RepoRef {
        account: Some("acme".into()),
        slug: "acme/api".into(),
        repository_id: "R_kgDOAbc123".into(),
        environment: Some("production".into()),
    }
}

fn job() -> RepairJob {
    RepairJob {
        job_id: "job-42".into(),
        repo: repo(),
        rev: "abc1234def".into(),
        failure: failure(),
        attribution: None,
        acceptance: vec!["the failing request returns 200".into()],
        expected_workflow: "drums-repair.yml".into(),
        timeout_secs: 900,
    }
}

fn failure() -> Failure {
    Failure {
        id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
        service: "shop".into(),
        signature: ErrorSignature {
            error_name: "TypeError".into(),
            top_frame_file: "server.js".into(),
            top_frame_function: None,
        },
        first_seen_ms: 1,
        event_count: 12,
        sample: ErrorEvent {
            service: "shop".into(),
            occurred_at_ms: 1,
            error_name: "TypeError".into(),
            error_message: "boom".into(),
            stack: String::new(),
            request: Some(engine_core::CapturedRequest {
                method: "POST".into(),
                path: "/api/checkout".into(),
                content_type: None,
                body: None,
            }),
            intake: Intake::Snippet,
        },
        intake: Intake::Snippet,
        claim: Claim { text: "observed".into(), provenance: Provenance::Observed },
    }
}

fn verified(text: &str) -> Claim {
    Claim { text: text.into(), provenance: Provenance::Verified }
}

fn good_attestation() -> Attestation {
    Attestation {
        job_id: "job-42".into(),
        repository: "acme/api".into(),
        repository_id: "R_kgDOAbc123".into(),
        sha: "abc1234def".into(),
        workflow: "drums-repair.yml".into(),
        run_id: "run-9001".into(),
        evidence_digest: DIGEST.into(),
    }
}

fn outcome(claims: Vec<AssertedClaim>) -> JobOutcome {
    JobOutcome {
        job_id: "job-42".into(),
        repaired: true,
        branch: Some("drums/repair-abc1234".into()),
        sha: Some("def5678".into()),
        summary: "guard body.promo".into(),
        claims,
        failure_reason: None,
        workspace: None,
    }
}

/// Runs the real gate at the top rung, so anything that proposes here is being
/// stopped by the EVIDENCE and not by the rung or the intake.
fn decide(out: &JobOutcome, consumed: &ConsumedRuns) -> ShipDecision {
    let j = job();
    let evidence = OutcomeEvidence {
        job: &j,
        outcome: out,
        expected_evidence_digest: DIGEST,
        consumed,
    };
    ship_decision(Rung::ActAlone, &j.failure.intake, &evidence)
}

fn assert_proposes(d: ShipDecision, because: &str) {
    match d {
        ShipDecision::Propose(ProposeReason::NoEligibleEvidence { detail }) => {
            assert!(!detail.is_empty(), "a refusal must say why");
        }
        ShipDecision::Propose(other) => {
            panic!("stopped for the wrong reason ({other:?}) — expected evidence: {because}")
        }
        ShipDecision::MayShip => panic!("SHOULD NOT SHIP: {because}"),
    }
}

// -- what may ship -----------------------------------------------------------

#[test]
fn a_firsthand_verified_claim_may_ship() {
    let out = outcome(vec![AssertedClaim::local(verified("the failing request now returns 200"))]);
    assert_eq!(decide(&out, &ConsumedRuns::new()), ShipDecision::MayShip);
}

// -- what may not ------------------------------------------------------------

/// The finding. A remote plane asserting `Verified` in a JSON payload must not
/// reach an unattended deploy.
#[test]
fn an_unattested_remote_verified_claim_cannot_ship() {
    let out = outcome(vec![AssertedClaim::remote(
        verified("the app's own test script passed"),
        "github-actions",
        None,
    )]);
    assert_proposes(
        decide(&out, &ConsumedRuns::new()),
        "a payload saying `verified` is not evidence that anything ran",
    );
}

/// `LocalPlane::timed_out` produces exactly this claim. Its content is
/// "nothing was established", and the gate reported it could authorise a
/// deploy.
#[test]
fn a_local_unresolved_claim_cannot_ship() {
    let out = outcome(vec![AssertedClaim::local(Claim {
        text: "the repair did not finish within 900s — nothing was established".into(),
        provenance: Provenance::Unresolved,
    })]);
    assert_proposes(
        decide(&out, &ConsumedRuns::new()),
        "evidence that explicitly established nothing must not authorise a deploy",
    );
}

#[test]
fn a_repair_with_no_claims_at_all_cannot_ship() {
    assert_proposes(
        decide(&outcome(vec![]), &ConsumedRuns::new()),
        "silence is not evidence",
    );
}

/// Each of these is a GENUINE attestation that describes something else.
#[test]
fn an_attestation_bound_to_anything_else_cannot_ship() {
    /// One way an attestation can describe something other than this job.
    type Mutation = (&'static str, Box<dyn Fn(&mut Attestation)>);

    let cases: Vec<Mutation> = vec![
        ("a different job", Box::new(|a: &mut Attestation| a.job_id = "job-41".into())),
        ("a different commit", Box::new(|a: &mut Attestation| a.sha = "0000000".into())),
        ("a different repository", Box::new(|a: &mut Attestation| a.repository_id = "R_other".into())),
        ("a different workflow", Box::new(|a: &mut Attestation| a.workflow = "release.yml".into())),
        ("edited evidence", Box::new(|a: &mut Attestation| a.evidence_digest = "sha256:edited".into())),
        ("no run id", Box::new(|a: &mut Attestation| a.run_id = String::new())),
    ];

    for (what, mutate) in cases {
        let mut a = good_attestation();
        mutate(&mut a);
        let out = outcome(vec![AssertedClaim::remote(
            verified("tests passed"),
            "github-actions",
            Some(engine_plane::testing::attest(a)),
        )]);
        assert_proposes(decide(&out, &ConsumedRuns::new()), what);
    }
}

/// A valid attestation is valid forever unless something remembers it was
/// spent. Without the registry, one captured result authorises every
/// subsequent deploy.
#[test]
fn a_replayed_attestation_cannot_ship_twice() {
    let consumed = ConsumedRuns::new();
    let out = outcome(vec![AssertedClaim::remote(
        verified("the app's own test script passed"),
        "github-actions",
        Some(engine_plane::testing::attest(good_attestation())),
    )]);

    assert_eq!(
        decide(&out, &consumed),
        ShipDecision::MayShip,
        "the first use of a valid attestation is fine"
    );

    assert!(consumed.consume("run-9001"), "first consume records it");
    assert!(!consumed.consume("run-9001"), "a second consume must report it was already spent");

    assert_proposes(
        decide(&out, &consumed),
        "the same workflow run must not authorise a second deploy",
    );
}

/// An attested remote claim that is not `Verified` is not evidence a check
/// passed, however well-bound.
#[test]
fn an_attested_non_verified_claim_cannot_ship() {
    for p in [Provenance::Observed, Provenance::Inferred, Provenance::Unresolved, Provenance::Approved] {
        let out = outcome(vec![AssertedClaim::remote(
            Claim { text: "something happened".into(), provenance: p },
            "github-actions",
            Some(engine_plane::testing::attest(good_attestation())),
        )]);
        assert_proposes(decide(&out, &ConsumedRuns::new()), "a non-verified claim");
    }
}

// -- the checks that come BEFORE evidence, still first ----------------------

/// The intake gate is evaluated before anything else, so no amount of
/// attested, verified evidence overrides it.
#[test]
fn perfect_evidence_still_cannot_ship_an_unreplayable_intake() {
    let out = outcome(vec![AssertedClaim::local(verified("the failing request now returns 200"))]);
    let mut j = job();
    j.failure.intake = Intake::Reported { source: "linear".into() };
    let consumed = ConsumedRuns::new();
    let evidence = OutcomeEvidence {
        job: &j,
        outcome: &out,
        expected_evidence_digest: DIGEST,
        consumed: &consumed,
    };
    match ship_decision(Rung::ActAlone, &j.failure.intake, &evidence) {
        ShipDecision::Propose(ProposeReason::IntakeNotReplayable { .. }) => {}
        other => panic!("the intake gate must be evaluated first and unconditionally: {other:?}"),
    }
}

#[test]
fn perfect_evidence_still_cannot_ship_below_act_alone() {
    let out = outcome(vec![AssertedClaim::local(verified("the failing request now returns 200"))]);
    let j = job();
    let consumed = ConsumedRuns::new();
    let evidence = OutcomeEvidence {
        job: &j,
        outcome: &out,
        expected_evidence_digest: DIGEST,
        consumed: &consumed,
    };
    match ship_decision(Rung::Propose, &j.failure.intake, &evidence) {
        ShipDecision::Propose(ProposeReason::RungBelowActAlone { .. }) => {}
        other => panic!("evidence must not substitute for an earned rung: {other:?}"),
    }
}
