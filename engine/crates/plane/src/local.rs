//! The in-process execution plane: what `drums watch` on a laptop already is.
//!
//! This exists to keep the seam honest. A trait with one remote implementation
//! is a trait shaped by that implementation, and `docs/CONTRACTS.md` is
//! explicit that a seam needs two implementations before it is believed. More
//! practically: every behaviour a remote plane must get right — a job that
//! times out, a repair that does not clear the bar, a workspace left for
//! inspection — can be exercised here, on a laptop, with no cloud and no
//! network.
//!
//! It also means local mode is not a special case that bypasses the seam. The
//! same pipeline runs the same trait either way, and the only thing that
//! changes is who executes and therefore who asserts the claims.

use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use engine_core::{Claim, Provenance};

use crate::{
    AssertedClaim, DispatchError, ExecutionPlane, JobOutcome, RepairJob, LOCAL_PLANE,
};

/// What the caller's repair pipeline produced, before it is labelled.
///
/// A struct rather than eight positional arguments: `outcome_from(job, true,
/// None, None, s, claims, None, None)` is a transposition bug waiting to
/// happen, and two of those fields are `Option<String>` sitting next to each
/// other.
#[derive(Debug, Default)]
pub struct PipelineResult {
    pub repaired: bool,
    pub branch: Option<String>,
    pub sha: Option<String>,
    pub summary: String,
    pub claims: Vec<Claim>,
    /// Why it did not work. Defaulted honestly by `outcome_from` when a
    /// failure arrives without one.
    pub failure_reason: Option<String>,
    /// Left on disk for a human to inspect, when the pipeline kept it.
    pub workspace: Option<std::path::PathBuf>,
}

/// Runs repairs in this process, against a repository on this machine.
///
/// Deliberately takes the repo path at construction rather than reading it
/// from the job: a job names a repository, and mapping that name to a
/// directory is the caller's business, not the plane's. A remote plane will
/// clone; this one already has it.
pub struct LocalPlane {
    repo: std::path::PathBuf,
}

impl LocalPlane {
    pub fn new(repo: impl AsRef<Path>) -> Self {
        Self { repo: repo.as_ref().to_path_buf() }
    }

    pub fn repo(&self) -> &Path {
        &self.repo
    }
}

#[async_trait]
impl ExecutionPlane for LocalPlane {
    fn name(&self) -> &'static str {
        LOCAL_PLANE
    }

    /// A directory that is not a git repository cannot host a repair, and
    /// finding that out at startup is much better than finding it out after a
    /// failure has already been detected and an agent already run.
    async fn available(&self) -> Result<(), DispatchError> {
        if !self.repo.join(".git").exists() {
            return Err(DispatchError::NotConfigured {
                plane: LOCAL_PLANE,
                detail: format!("{} is not a git repository", self.repo.display()),
            });
        }
        Ok(())
    }

    async fn run(&self, job: &RepairJob) -> Result<JobOutcome, DispatchError> {
        self.available().await?;

        // The real repair pipeline lives in `drums-watch`, which depends on
        // this crate — so wiring it here would be circular. The caller
        // supplies it instead; see `crates/cli/src/engine.rs`. What this
        // implementation owns is the CONTRACT: bounded time, an outcome that
        // is never ambiguous, and claims that say who observed them.
        Err(DispatchError::NotConfigured {
            plane: LOCAL_PLANE,
            detail: format!(
                "LocalPlane::run is a contract stub — job {} for {} at {} should be executed \
                 by the caller's pipeline. Use LocalPlane::outcome_from to build its result.",
                job.job_id,
                job.repo.key(),
                job.rev
            ),
        })
    }
}

impl LocalPlane {
    /// Build an outcome from a pipeline result, stamping every claim as
    /// firsthand.
    ///
    /// This is the one place local claims are labelled, so no caller can
    /// forget and no caller can mislabel. A claim that reaches the record
    /// saying `local` really was observed by this process.
    pub fn outcome_from(job: &RepairJob, result: PipelineResult) -> JobOutcome {
        let PipelineResult {
            repaired,
            branch,
            sha,
            summary,
            claims,
            failure_reason,
            workspace,
        } = result;

        // An outcome that says it failed but will not say why is the miss
        // `docs/CONTRACTS.md` names. Rather than trusting every caller to
        // remember, supply the honest default here.
        let failure_reason = match (repaired, failure_reason) {
            (true, r) => r,
            (false, Some(r)) => Some(r),
            (false, None) => Some(
                "the repair did not clear its acceptance bar, and the pipeline gave no reason"
                    .to_string(),
            ),
        };

        JobOutcome {
            job_id: job.job_id.clone(),
            repaired,
            branch,
            sha,
            summary,
            claims: claims.into_iter().map(AssertedClaim::local).collect(),
            failure_reason,
            workspace,
        }
    }

    /// The outcome for a job that ran out of time.
    ///
    /// A timeout is `unresolved`, never a failure of the repair: nothing was
    /// established either way, and reporting "the repair did not work" for a
    /// job that never finished would be a claim about something nobody
    /// watched.
    pub fn timed_out(job: &RepairJob, elapsed: Duration) -> JobOutcome {
        JobOutcome {
            job_id: job.job_id.clone(),
            repaired: false,
            branch: None,
            sha: None,
            summary: String::new(),
            claims: vec![AssertedClaim::local(Claim {
                text: format!(
                    "the repair did not finish within {}s (gave up after {:.0}s) — nothing was \
                     established about whether it would have worked",
                    job.timeout_secs,
                    elapsed.as_secs_f64()
                ),
                provenance: Provenance::Unresolved,
            })],
            failure_reason: Some(format!("timed out after {}s", job.timeout_secs)),
            workspace: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RepoRef;

    fn job(dir: &Path) -> RepairJob {
        RepairJob {
            job_id: "j1".into(),
            repo: RepoRef::local(dir.display().to_string()),
            rev: "abc1234".into(),
            failure: crate::tests_support::sample_failure(),
            attribution: None,
            acceptance: vec!["the failing request returns 200".into()],
            expected_workflow: "drums-repair.yml".into(),
            timeout_secs: 900,
        }
    }

    #[tokio::test]
    async fn a_directory_that_is_not_a_repo_is_refused_at_startup() {
        let dir = tempfile::tempdir().unwrap();
        let plane = LocalPlane::new(dir.path());
        let err = plane
            .available()
            .await
            .expect_err("a non-repo cannot host a repair");
        assert!(err.to_string().contains("not a git repository"), "{err}");
    }

    #[tokio::test]
    async fn a_real_repo_is_available() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        LocalPlane::new(dir.path()).available().await.expect("should be available");
    }

    #[test]
    fn every_local_claim_is_stamped_firsthand() {
        let dir = tempfile::tempdir().unwrap();
        let out = LocalPlane::outcome_from(
            &job(dir.path()),
            PipelineResult {
                repaired: true,
                branch: Some("drums/repair-abc".into()),
                sha: Some("def456".into()),
                summary: "guard body.promo".into(),
                claims: vec![
                    Claim { text: "the failing request now returns 200".into(), provenance: Provenance::Verified },
                    Claim { text: "tests passed".into(), provenance: Provenance::Verified },
                ],
                ..Default::default()
            },
        );
        assert_eq!(out.claims.len(), 2);
        for c in &out.claims {
            assert!(c.is_firsthand(), "{:?}", c.asserted_by);
            assert_eq!(
                c.provenance_note(),
                None,
                "a firsthand claim needs no caveat — it was observed here"
            );
        }
    }

    /// An outcome that failed but will not say why is exactly the miss the
    /// contracts doc names. The default is supplied here rather than trusted
    /// to every caller.
    #[test]
    fn a_failed_outcome_always_has_a_reason() {
        let dir = tempfile::tempdir().unwrap();
        let out = LocalPlane::outcome_from(
            &job(dir.path()),
            // repaired: false, and the caller forgot to say why.
            PipelineResult::default(),
        );
        let why = out.failure_reason.expect("a failure must always say why");
        assert!(why.contains("gave no reason"), "{why}");
    }

    /// A timeout establishes nothing. Reporting it as "the repair did not
    /// work" would be a claim about something nobody watched.
    #[test]
    fn a_timeout_is_unresolved_not_a_failed_repair() {
        let dir = tempfile::tempdir().unwrap();
        let out = LocalPlane::timed_out(&job(dir.path()), Duration::from_secs(901));
        assert!(!out.repaired);
        assert_eq!(out.claims.len(), 1);
        assert_eq!(out.claims[0].claim.provenance, Provenance::Unresolved);
        assert!(
            out.claims[0].claim.text.contains("nothing was established"),
            "{}",
            out.claims[0].claim.text
        );
        assert!(
            !out.claims[0].claim.text.to_lowercase().contains("did not work"),
            "a timeout must not be reported as a repair that failed"
        );
    }
}
