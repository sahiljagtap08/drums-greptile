//! Scenario B (pre-release class): build-failure repair core. Checks that a
//! revision's declared build (and, if present, test) script actually pass,
//! then — on a build failure only — drives an [`engine_repair::RepairAgent`]
//! to fix it in a worktree and re-checks. This crate is PROPOSE-ONLY BY
//! DESIGN: it commits a repair to its own branch and stops there. It never
//! ships, deploys, or reverts anything — there is no deploy-command surface
//! here at all (see the `engine_check_exposes_no_ship_or_deploy_surface`
//! test at the bottom of this file).
//!
//! Claims are only ever earned from a command this crate actually executed
//! (spec's "verified only from executed runs"): a passing build/test script
//! earns a `Verified` "... passed" claim, a failing one earns a `Verified`
//! "... failed: <first error line>" claim (the FAILURE itself was observed
//! first-hand, so it is verified too — nothing here is ever a fabricated
//! pass). A missing `scripts.test` never manufactures a claim either way;
//! `tests_ok` stays `None`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use engine_core::{
    Attribution, Claim, DeployRecord, ErrorEvent, ErrorSignature, Failure, Intake, Provenance,
};
use engine_repair::{RepairAgent, RepairContext};
use engine_repro::ManagedWorktree;

#[derive(Debug, thiserror::Error)]
pub enum CheckError {
    #[error("git worktree failed: {0}")]
    Worktree(String),
    #[error("no build script declared in package.json (scripts.build)")]
    NoBuildScript,
    #[error("{what} timed out after {ms}ms")]
    Timeout { what: &'static str, ms: u64 },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Result of running a revision's declared build (and, if present, test)
/// script exactly once each. `claims` and `log_excerpt` describe only what
/// was actually executed — never a run that timed out or never happened.
#[derive(Debug, Clone)]
pub struct CheckOutcome {
    pub rev: String,
    pub build_ok: bool,
    /// `None` when no `scripts.test` is declared, or when the build itself
    /// failed and tests were never run — an untried test suite must never
    /// be reported as passing OR failing.
    pub tests_ok: Option<bool>,
    pub claims: Vec<Claim>,
    /// Tail of the failing command's combined stdout+stderr. Empty when
    /// build and tests (if any) both passed — there is nothing to excerpt.
    pub log_excerpt: String,
}

/// Rebuild `rev` in a fresh, detached worktree (reusing
/// [`engine_repro::ManagedWorktree`] — the same ulid-worktree-plus-sha-guard
/// primitive `engine-repro` itself uses, so the sha validation it enforces
/// is inherited here rather than re-implemented), run `scripts.build`
/// (required) and `scripts.test` (optional) once each with `timeout_ms`
/// each, and report exactly what happened. The worktree is always removed
/// before returning — nothing is committed here.
pub async fn check_revision(
    repo: &Path,
    rev: &str,
    timeout_ms: u64,
) -> Result<CheckOutcome, CheckError> {
    let worktree =
        ManagedWorktree::create(repo, rev).map_err(|e| CheckError::Worktree(e.to_string()))?;
    run_check_in_dir(&worktree.dir, rev, timeout_ms).await
}

/// What `check_and_repair` accomplished. `branch`/`commit_sha` are `Some`
/// only once a repair commit genuinely exists in the ORIGIN repo (the
/// worktree it was made in may already have been removed — same "git is the
/// record" discipline `engine-repair`'s callers already follow). `repaired`
/// is true only when the branch exists AND the re-check on it cleared the
/// acceptance bar; a commit that exists but didn't verify is surfaced
/// honestly via `repair_failure`, not silently dropped.
#[derive(Debug)]
pub struct CheckAndRepairOutcome {
    pub initial: CheckOutcome,
    pub branch: Option<String>,
    pub commit_sha: Option<String>,
    /// Re-run of `check_revision`'s underlying checks, against the repair
    /// commit, in the SAME worktree the agent edited — `None` when no
    /// repair was ever attempted (the initial build already passed) or the
    /// agent/commit step never produced one to verify.
    pub verify: Option<CheckOutcome>,
    pub repaired: bool,
    /// Honest reason repair didn't happen or didn't verify. `None` only
    /// when `repaired` is `true`, or when the initial build already passed
    /// and there was nothing to repair.
    pub repair_failure: Option<String>,
    /// Path of the worktree the repair happened in, ONLY when it was kept
    /// on disk (`ManagedWorktree::keep_on_drop`) for a human to inspect —
    /// `None` on the two paths where nothing is left to look at: the
    /// initial build already passed (no worktree was ever repaired in), or
    /// the repair fully succeeded (the worktree's contents are already
    /// captured by the commit, so removing the checkout on drop loses
    /// nothing). A rich outcome that names a branch/commit but not where to
    /// find the on-disk evidence is only half a handoff.
    pub worktree_dir: Option<PathBuf>,
}

/// Compose [`check_revision`]'s build/test check with an
/// [`engine_repair::RepairAgent`]: if the initial build fails, hand the
/// agent a worktree and an acceptance bar of "`npm run build` exits 0" (plus
/// "the test script exits 0" when one is declared), commit whatever it
/// produces on `drums/check-<short-rev>`, and re-run the check against that
/// commit to confirm it actually clears the bar before calling it
/// `repaired`. PROPOSE-ONLY: this function never ships, deploys, or reverts
/// anything — it stops at a branch + commit sha for a human (or a later,
/// separate ship stage) to act on.
pub async fn check_and_repair(
    repo: &Path,
    rev: &str,
    timeout_ms: u64,
    agent: &dyn RepairAgent,
) -> Result<CheckAndRepairOutcome, CheckError> {
    let mut worktree =
        ManagedWorktree::create(repo, rev).map_err(|e| CheckError::Worktree(e.to_string()))?;
    let initial = run_check_in_dir(&worktree.dir, rev, timeout_ms).await?;

    if initial.build_ok {
        // Nothing to repair — the worktree drops (removed) at the end of
        // this function; it never committed anything.
        return Ok(CheckAndRepairOutcome {
            initial,
            branch: None,
            commit_sha: None,
            verify: None,
            repaired: false,
            repair_failure: None,
            worktree_dir: None,
        });
    }

    let short = short_rev(rev);
    let ctx = build_repair_context(rev, &short, &initial, &worktree.dir);

    let attempt = match agent.repair(&worktree.dir, &ctx).await {
        Ok(a) => a,
        Err(e) => {
            // Leave the worktree on disk (spec §13 "design the miss") so a
            // human can inspect exactly what the agent saw and did — same
            // discipline the repair pipeline in `engine/crates/cli` already
            // follows on an agent failure.
            worktree.keep_on_drop = true;
            let worktree_dir = Some(worktree.dir.clone());
            return Ok(CheckAndRepairOutcome {
                initial,
                branch: None,
                commit_sha: None,
                verify: None,
                repaired: false,
                repair_failure: Some(format!("agent could not produce a fix: {e}")),
                worktree_dir,
            });
        }
    };

    let branch = format!("drums/check-{short}");
    let commit_sha = match commit_check_repair(&worktree.dir, &branch, &attempt.summary) {
        Ok(sha) => sha,
        Err(e) => {
            worktree.keep_on_drop = true;
            let worktree_dir = Some(worktree.dir.clone());
            return Ok(CheckAndRepairOutcome {
                initial,
                branch: None,
                commit_sha: None,
                verify: None,
                repaired: false,
                repair_failure: Some(format!("could not commit the repair: {e}")),
                worktree_dir,
            });
        }
    };

    // Re-run the same checks against the commit, in the SAME worktree the
    // agent edited — this is where `repaired` is actually earned, never
    // assumed from the agent's own report of success. Matched explicitly
    // (never a bare `?`): by this point `commit_check_repair` has already
    // created `branch`/`commit_sha` in the ORIGIN repo (worktrees share
    // refs), so if the re-check itself errors — e.g. the agent's "fix"
    // deleted `scripts.build` entirely (`CheckError::NoBuildScript`), or
    // the rebuild now hangs (`CheckError::Timeout`) — a bare `?` would
    // return an `Err` that names neither the branch nor the commit, AND
    // would drop the worktree (the only on-disk evidence of what the agent
    // did) via `ManagedWorktree`'s `Drop`, since `keep_on_drop` would never
    // get set on this path. That violates this very struct's own "a commit
    // that exists but didn't verify is surfaced honestly ... not silently
    // dropped" contract. Keep the worktree and return a rich `Ok(..)`
    // outcome instead — same shape the three failure branches above
    // already use, so a re-check error is handled no differently than any
    // other repair failure spec §13 asks this crate to "design the miss"
    // for.
    let verify = match run_check_in_dir(&worktree.dir, &commit_sha, timeout_ms).await {
        Ok(v) => v,
        Err(e) => {
            worktree.keep_on_drop = true;
            return Ok(CheckAndRepairOutcome {
                initial,
                branch: Some(branch.clone()),
                commit_sha: Some(commit_sha.clone()),
                verify: None,
                repaired: false,
                repair_failure: Some(format!(
                    "re-check on branch {branch} (commit {commit_sha}) failed: {e}"
                )),
                worktree_dir: Some(worktree.dir.clone()),
            });
        }
    };
    let ok = verify.build_ok && verify.tests_ok != Some(false);

    if !ok {
        // The commit is real (git is the record) even though it didn't
        // clear the bar; keep the worktree so a human can see why.
        worktree.keep_on_drop = true;
        let worktree_dir = Some(worktree.dir.clone());
        return Ok(CheckAndRepairOutcome {
            initial,
            branch: Some(branch),
            commit_sha: Some(commit_sha),
            verify: Some(verify),
            repaired: false,
            repair_failure: Some(
                "repair commit did not clear the acceptance bar on re-check".to_string(),
            ),
            worktree_dir,
        });
    }

    // Success: the branch and commit already persist in the origin repo
    // (worktrees share refs with it), so removing the checkout on drop
    // loses nothing — `keep_on_drop` stays false, and there is no worktree
    // path left to hand back.
    Ok(CheckAndRepairOutcome {
        initial,
        branch: Some(branch),
        commit_sha: Some(commit_sha),
        verify: Some(verify),
        repaired: true,
        repair_failure: None,
        worktree_dir: None,
    })
}

/// Build the [`RepairContext`] handed to the agent from a build failure that
/// was actually observed running `check_revision`'s own build step —
/// `engine-repair`'s `RepairContext` is shaped around a runtime `Failure` +
/// `Attribution` (spec §3), so this synthesizes minimal, honestly-labeled
/// stand-ins from the executed build failure: an `Observed` claim carrying
/// the real failure text, and an `Inferred` claim that only ever says "this
/// revision's build failed" (never anything about which prior change caused
/// it — that attribution question doesn't apply to a pre-release build
/// check). Neither synthetic claim is ever returned to a `check_and_repair`
/// caller; they exist only to drive the agent's prompt.
/// Run whatever checks the repo actually declares.
///
/// [`run_check_in_dir`] REQUIRES `scripts.build` and errors without it, which
/// is right for `drums check` — a pre-release build check with no build to run
/// is a misconfiguration. It is wrong here. Most Node libraries and plenty of
/// applications declare only `scripts.test`, and refusing to look at a
/// reported issue because a repo has no build step would exclude them for a
/// reason that has nothing to do with the issue.
///
/// So: run each declared script, claim only what ran, and refuse only when
/// there is NOTHING to run — because with no checks at all, "no worse than
/// before" is not a bar, it is a wish.
async fn run_declared_checks(
    dir: &Path,
    rev: &str,
    timeout_ms: u64,
) -> Result<CheckOutcome, CheckError> {
    let short = short_rev(rev);
    let build_script = read_script(dir, "build")?;
    let test_script = read_script(dir, "test")?;

    if build_script.is_none() && test_script.is_none() {
        return Err(CheckError::NoBuildScript);
    }

    let mut claims = Vec::new();
    let mut log_excerpt = String::new();

    // `build_ok` is true when the build passed OR when there is no build to
    // run. It means "the build is not broken", and a repo with no build step
    // has no broken build.
    let mut build_ok = true;
    if let Some(script) = build_script {
        let run = run_script(dir, &script, timeout_ms, "build").await?;
        if run.success {
            claims.push(Claim {
                text: format!("build passed at {short}"),
                provenance: Provenance::Verified,
            });
        } else {
            build_ok = false;
            claims.push(Claim {
                text: format!("build failed: {}", first_error_line(&run)),
                provenance: Provenance::Verified,
            });
            log_excerpt = tail(&run.combined(), 4000);
        }
    }

    // A failed build means the tests were never run, and an untried suite is
    // never reported as passing OR failing.
    let tests_ok = match (build_ok, test_script) {
        (true, Some(script)) => {
            let run = run_script(dir, &script, timeout_ms, "test").await?;
            if run.success {
                claims.push(Claim {
                    text: format!("tests passed at {short}"),
                    provenance: Provenance::Verified,
                });
                Some(true)
            } else {
                claims.push(Claim {
                    text: format!("tests failed: {}", first_error_line(&run)),
                    provenance: Provenance::Verified,
                });
                log_excerpt = tail(&run.combined(), 4000);
                Some(false)
            }
        }
        _ => None,
    };

    Ok(CheckOutcome {
        rev: rev.to_string(),
        build_ok,
        tests_ok,
        claims,
        log_excerpt,
    })
}

/// What a human said, as handed to the agent. Deliberately just the words
/// they wrote — Drums has no way to turn a complaint into a stack trace, and
/// pretending otherwise is how a `Reported` intake would start looking like a
/// `Snippet` one.
#[derive(Debug, Clone)]
pub struct IssueTask {
    /// The tracker's own id, so a caller can write back to the right thread.
    pub id: String,
    /// `linear` | `agentation`.
    pub source: String,
    pub title: String,
    pub body: String,
    pub url: Option<String>,
}

/// Repair a HUMAN-REPORTED issue (spec Scenario C).
///
/// This is `check_and_repair`'s sibling, and the differences are the whole
/// design:
///
/// - **The build is passing when we start.** There is no failing check to
///   diagnose from; the only input is a sentence someone typed. So the agent
///   is given the issue text as the task, and the baseline check exists to
///   establish what "no worse than before" means, not to find the bug.
/// - **The acceptance bar is non-regression, not resolution.** Build still
///   passes, tests still pass, and no fewer tests run than before. That is
///   everything this code can execute.
/// - **One claim is ALWAYS `Unresolved`, no matter how green everything
///   else is:** whether the change actually resolves what the person
///   reported. Drums has no visual check and no way to ask them. A reported
///   issue is usually a visual complaint, and a green test suite is not
///   evidence about it.
/// - **Propose-only, structurally.** The synthesized `Failure` carries
///   `Intake::Reported`, whose `is_replayable()` is false, so the ship gate
///   refuses it whatever rung the operator has set. It is not propose-only by
///   convention here; it is propose-only because nothing in its chain was
///   ever verified against a reproduction.
pub async fn repair_reported_issue(
    repo: &Path,
    rev: &str,
    task: &IssueTask,
    timeout_ms: u64,
    agent: &dyn RepairAgent,
    remembered: Vec<String>,
) -> Result<CheckAndRepairOutcome, CheckError> {
    let mut worktree =
        ManagedWorktree::create(repo, rev).map_err(|e| CheckError::Worktree(e.to_string()))?;

    // Baseline. If the tree is ALREADY broken, stop: an agent asked to fix a
    // button in a repo whose build is failing will produce a change nobody can
    // evaluate, and "the build passes" would be a claim we could never earn.
    let initial = match run_declared_checks(&worktree.dir, rev, timeout_ms).await {
        Ok(o) => o,
        // Nothing declared at all. Say what is missing and why it matters,
        // rather than reporting a build script as the requirement when a test
        // script would have done just as well.
        Err(CheckError::NoBuildScript) => {
            return Ok(CheckAndRepairOutcome {
                initial: CheckOutcome {
                    rev: rev.to_string(),
                    build_ok: true,
                    tests_ok: None,
                    claims: vec![],
                    log_excerpt: String::new(),
                },
                branch: None,
                commit_sha: None,
                verify: None,
                repaired: false,
                repair_failure: Some(
                    "this repo declares neither scripts.build nor scripts.test, so there is \
                     no check that could show the repair left things no worse than before. \
                     Declare one and Drums will run it."
                        .to_string(),
                ),
                worktree_dir: None,
            });
        }
        Err(e) => return Err(e),
    };

    if !initial.build_ok || initial.tests_ok == Some(false) {
        return Ok(CheckAndRepairOutcome {
            initial,
            branch: None,
            commit_sha: None,
            verify: None,
            repaired: false,
            repair_failure: Some(
                "this revision is already failing its own checks, so a reported-issue repair \
                 could not be evaluated against it — there would be no way to tell the \
                 repair's effect from the existing breakage. Fix that first (`drums check`)."
                    .to_string(),
            ),
            worktree_dir: None,
        });
    }

    let short = short_rev(rev);
    let mut ctx = build_issue_repair_context(rev, &short, task, &worktree.dir);
    ctx.remembered = remembered;

    let attempt = match agent.repair(&worktree.dir, &ctx).await {
        Ok(a) => a,
        Err(e) => {
            worktree.keep_on_drop = true;
            let worktree_dir = Some(worktree.dir.clone());
            return Ok(CheckAndRepairOutcome {
                initial,
                branch: None,
                commit_sha: None,
                verify: None,
                repaired: false,
                repair_failure: Some(format!("agent could not produce a fix: {e}")),
                worktree_dir,
            });
        }
    };

    let branch = format!("drums/issue-{short}");
    let commit_sha = match commit_check_repair(&worktree.dir, &branch, &attempt.summary) {
        Ok(sha) => sha,
        Err(e) => {
            worktree.keep_on_drop = true;
            let worktree_dir = Some(worktree.dir.clone());
            return Ok(CheckAndRepairOutcome {
                initial,
                branch: None,
                commit_sha: None,
                verify: None,
                repaired: false,
                repair_failure: Some(format!("could not commit the repair: {e}")),
                worktree_dir,
            });
        }
    };

    let mut verify = match run_declared_checks(&worktree.dir, &commit_sha, timeout_ms).await {
        Ok(v) => v,
        Err(e) => {
            worktree.keep_on_drop = true;
            let worktree_dir = Some(worktree.dir.clone());
            return Ok(CheckAndRepairOutcome {
                initial,
                branch: Some(branch),
                commit_sha: Some(commit_sha),
                verify: None,
                repaired: false,
                repair_failure: Some(format!("the re-check could not run after the repair: {e}")),
                worktree_dir,
            });
        }
    };

    // Non-regression, not resolution. `tests_ok == Some(false)` fails it;
    // `None` (no test script declared) is not a failure, but it is also not
    // evidence, and the unresolved claim below says so.
    let no_regression = verify.build_ok && verify.tests_ok != Some(false);

    // The claim this whole function exists to be honest about. Appended
    // unconditionally — a green build and a green suite say nothing about
    // whether the person who reported this would agree it is fixed.
    verify.claims.push(Claim {
        text: format!(
            "whether this resolves the {} report \"{}\" was NOT checked — Drums has no \
             visual or behavioural check for it, so a human has to confirm",
            task.source,
            truncate_chars(&task.title, 80)
        ),
        provenance: Provenance::Unresolved,
    });

    if !no_regression {
        worktree.keep_on_drop = true;
    }
    let worktree_dir = if no_regression {
        None
    } else {
        Some(worktree.dir.clone())
    };
    let repair_failure = if no_regression {
        None
    } else {
        Some("the repair made the build or the test suite worse than it was before".to_string())
    };

    Ok(CheckAndRepairOutcome {
        initial,
        branch: Some(branch),
        commit_sha: Some(commit_sha),
        verify: Some(verify),
        repaired: no_regression,
        repair_failure,
        worktree_dir,
    })
}

/// The agent's brief for a reported issue. The task is the human's own words,
/// quoted rather than paraphrased: a summary of a complaint written by the
/// code that is about to act on it is a game of telephone with one player.
/// The `Failure` a reported issue synthesizes, exposed so the proposal path
/// reuses THIS shape rather than building a second one that could drift from
/// it. Its intake is `Reported`, which is what makes it unshippable.
pub fn synthetic_failure_for_issue(task: &IssueTask) -> Failure {
    build_issue_repair_context("HEAD", "HEAD", task, Path::new(".")).failure
}

fn build_issue_repair_context(
    rev: &str,
    short: &str,
    task: &IssueTask,
    worktree_dir: &Path,
) -> RepairContext {
    let now = now_ms();
    let intake = Intake::Reported {
        source: task.source.clone(),
    };
    let headline = format!("{}: {}", task.source, truncate_chars(&task.title, 160));

    let sample = ErrorEvent {
        service: task.source.clone(),
        occurred_at_ms: now,
        error_name: "ReportedIssue".to_string(),
        error_message: truncate_chars(&task.title, 400),
        // Not a stack: the reporter's own description, which is the only
        // diagnostic input that exists. Labelling it `stack` is the field this
        // struct has; the intake says plainly where it came from.
        stack: truncate_chars(&task.body, 4000),
        request: None,
        intake: intake.clone(),
    };
    let failure = Failure {
        id: format!("issue-{}", task.id),
        service: task.source.clone(),
        signature: ErrorSignature {
            error_name: "ReportedIssue".to_string(),
            top_frame_file: task.url.clone().unwrap_or_else(|| task.id.clone()),
            top_frame_function: None,
        },
        first_seen_ms: now,
        event_count: 1,
        sample,
        intake,
        claim: Claim {
            text: format!("a human reported: {headline}"),
            provenance: Provenance::Observed,
        },
    };
    let attribution = Attribution {
        deploy: DeployRecord {
            sha: rev.to_string(),
            description: "current revision".to_string(),
            author: "drums".to_string(),
            deployed_at_ms: now,
        },
        overlap_files: vec![],
        minutes_after_deploy: 0,
        // Never `Inferred` here: nothing was correlated. A reported issue has
        // no timing evidence tying it to a deploy, and claiming otherwise
        // would manufacture an attribution out of the fact that some revision
        // happens to be checked out.
        claim: Claim {
            text: format!("repairing against {short}; no deploy was implicated"),
            provenance: Provenance::Unresolved,
        },
    };

    let mut acceptance = vec![
        "the reported problem is addressed".to_string(),
        "npm run build exits 0".to_string(),
    ];
    if declares_test_script(worktree_dir) {
        acceptance
            .push("the package's test script exits 0, with no fewer tests than before".to_string());
    }

    RepairContext {
        failure,
        attribution,
        acceptance,
        remembered: Vec::new(),
    }
}

fn build_repair_context(
    rev: &str,
    short: &str,
    initial: &CheckOutcome,
    worktree_dir: &Path,
) -> RepairContext {
    let now = now_ms();
    let error_message = initial
        .claims
        .first()
        .map(|c| c.text.clone())
        .unwrap_or_else(|| "build failed".to_string());

    // A build failure has no request that produced it, so it carries none. The
    // earlier stand-in here was a `CapturedRequest { method: "N/A", ... }`,
    // which is exactly the kind of plausible-looking fiction the intake
    // taxonomy exists to forbid: `Intake::Trigger` says "something told us
    // this broke, and there is nothing to replay". That is also precisely why
    // this class can never ship alone — `is_replayable()` is false, and the
    // ship gate reads it.
    let sample = ErrorEvent {
        service: "build".to_string(),
        occurred_at_ms: now,
        error_name: "BuildFailure".to_string(),
        error_message: error_message.clone(),
        stack: initial.log_excerpt.clone(),
        request: None,
        intake: Intake::Trigger {
            source: "drums check".to_string(),
        },
    };
    let failure = Failure {
        id: format!("check-{short}"),
        service: "build".to_string(),
        signature: ErrorSignature {
            error_name: "BuildFailure".to_string(),
            top_frame_file: "package.json".to_string(),
            top_frame_function: None,
        },
        first_seen_ms: now,
        event_count: 1,
        sample,
        intake: Intake::Trigger {
            source: "drums check".to_string(),
        },
        claim: Claim {
            text: error_message,
            provenance: Provenance::Observed,
        },
    };
    let attribution = Attribution {
        deploy: DeployRecord {
            sha: rev.to_string(),
            description: "pre-release build check".to_string(),
            author: "drums-check".to_string(),
            deployed_at_ms: now,
        },
        overlap_files: vec![],
        minutes_after_deploy: 0,
        claim: Claim {
            text: format!("build fails at {short}"),
            provenance: Provenance::Inferred,
        },
    };

    let mut acceptance = vec!["npm run build exits 0".to_string()];
    if declares_test_script(worktree_dir) {
        acceptance.push("the package's test script exits 0".to_string());
    }

    RepairContext {
        failure,
        attribution,
        acceptance,
        remembered: Vec::new(),
    }
}

/// Run `scripts.build` (required — its absence is [`CheckError::NoBuildScript`])
/// and, if declared, `scripts.test`, once each, against an already-checked-out
/// directory. Tests are skipped entirely (not run, not claimed either way)
/// when the build itself failed.
async fn run_check_in_dir(
    dir: &Path,
    rev: &str,
    timeout_ms: u64,
) -> Result<CheckOutcome, CheckError> {
    let build_script = read_script(dir, "build")?.ok_or(CheckError::NoBuildScript)?;
    let short = short_rev(rev);

    let build_run = run_script(dir, &build_script, timeout_ms, "build").await?;
    if !build_run.success {
        let claim = Claim {
            text: format!("build failed: {}", first_error_line(&build_run)),
            provenance: Provenance::Verified,
        };
        return Ok(CheckOutcome {
            rev: rev.to_string(),
            build_ok: false,
            tests_ok: None,
            claims: vec![claim],
            log_excerpt: tail(&build_run.combined(), 4000),
        });
    }

    let mut claims = vec![Claim {
        text: format!("build passed at {short}"),
        provenance: Provenance::Verified,
    }];
    let mut log_excerpt = String::new();

    let tests_ok = match read_script(dir, "test")? {
        Some(test_script) => {
            let test_run = run_script(dir, &test_script, timeout_ms, "test").await?;
            if test_run.success {
                claims.push(Claim {
                    text: format!("tests passed at {short}"),
                    provenance: Provenance::Verified,
                });
            } else {
                claims.push(Claim {
                    text: format!("tests failed: {}", first_error_line(&test_run)),
                    provenance: Provenance::Verified,
                });
                log_excerpt = tail(&test_run.combined(), 4000);
            }
            Some(test_run.success)
        }
        None => None,
    };

    Ok(CheckOutcome {
        rev: rev.to_string(),
        build_ok: true,
        tests_ok,
        claims,
        log_excerpt,
    })
}

/// Whether `dir`'s `package.json` declares a non-empty, non-placeholder
/// `scripts.test` — used only to decide whether to hand the repair agent a
/// test acceptance criterion; never itself a claim about test results.
fn declares_test_script(dir: &Path) -> bool {
    matches!(read_script(dir, "test"), Ok(Some(_)))
}

/// Read `scripts.<name>` out of `dir`'s `package.json`. `Ok(None)` covers
/// every honest "not declared" case uniformly — no `package.json`, invalid
/// JSON, missing `scripts`, missing the named script, or an empty/`"echo
/// \"Error: no test specified\" && exit 1"`-style placeholder (the npm
/// default for an uninitialized `test` script) — so callers never have to
/// tell "absent" apart from "unreadable"; both mean the same thing here.
fn read_script(dir: &Path, name: &str) -> Result<Option<String>, CheckError> {
    let Ok(content) = std::fs::read_to_string(dir.join("package.json")) else {
        return Ok(None);
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) else {
        return Ok(None);
    };
    let script = v
        .pointer(&format!("/scripts/{name}"))
        .and_then(|s| s.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.contains("no test specified"));
    Ok(script.map(str::to_string))
}

struct ScriptRun {
    success: bool,
    stdout: String,
    stderr: String,
}

impl ScriptRun {
    fn combined(&self) -> String {
        format!("{}\n{}", self.stdout, self.stderr)
            .trim()
            .to_string()
    }
}

/// Makes the script child the leader of a brand-new process group (pgid ==
/// its own pid), mirroring `engine-repair::ChildGuard` and
/// `engine-repro::BootedApp`'s identical helper. Any subprocess `npm run
/// <script>` forks (npm's own process, then the actual bundler/test
/// runner) inherits that group, which is what lets [`kill_process_group`]
/// reach the whole tree instead of only the direct `sh` process.
#[cfg(unix)]
fn set_new_process_group(cmd: &mut tokio::process::Command) {
    cmd.process_group(0);
}

#[cfg(not(unix))]
fn set_new_process_group(_cmd: &mut tokio::process::Command) {}

/// SIGKILLs an entire process group by pid (valid because
/// [`set_new_process_group`] made this pid its own group leader, so pgid ==
/// pid). A negative pid to `kill(2)` targets the whole group — the only way
/// to reach a grandchild `Child::start_kill`/`kill_on_drop` cannot.
#[cfg(unix)]
fn kill_process_group(pid: u32) {
    // Safety: `kill` with a negative pid signals the process group rather
    // than a single process; no memory is touched, only a syscall is made.
    unsafe {
        libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn kill_process_group(_pid: u32) {}

/// Group-kills a build/test script's ENTIRE process tree on drop — the
/// third spawn site in this codebase with the identical shape (`sh -c
/// "<script>"`, where `npm run build`/`test` virtually always forks at
/// least one further child) that already got this exact discipline twice
/// (`engine-repair::ChildGuard`, `engine-repro::BootedApp`'s doc comment:
/// "reproduction always used a bare `start_kill()`, which only reaches the
/// direct child; this closes that gap"). Holds only the pgid, captured
/// eagerly at spawn (`Child::id()` returns `None` once reaped) — not the
/// `Child` itself, which `run_script` needs to move into
/// `wait_with_output()` — so there is no borrow conflict, and `Drop` still
/// runs on every exit path of `run_script`: the timeout branch below, an
/// io error, a normal return, or the enclosing future being cancelled
/// entirely (a caller's own outer timeout, or the CLI shutting down).
struct ScriptGroupGuard {
    pgid: Option<u32>,
}

impl Drop for ScriptGroupGuard {
    fn drop(&mut self) {
        if let Some(pgid) = self.pgid {
            kill_process_group(pgid);
        }
    }
}

/// Run one package-script string through a shell exactly as `npm run
/// <script>` would (a script is opaque text that itself needs shell
/// operators like `&&`, not a template with untrusted substitutions — same
/// reasoning `engine/crates/cli`'s own `run_package_test_script` documents),
/// with this worktree's `node_modules/.bin` prepended to `PATH` and a
/// minimal env (`PATH` + `HOME` only, mirroring `engine-repair`'s discipline
/// so a build/test script run against agent-edited code can never leak
/// telemetry back into the running ingest).
async fn run_script(
    dir: &Path,
    script: &str,
    timeout_ms: u64,
    what: &'static str,
) -> Result<ScriptRun, CheckError> {
    let bin_dir = dir.join("node_modules").join(".bin");
    let existing_path = std::env::var_os("PATH").unwrap_or_default();
    let mut search_path = vec![bin_dir];
    search_path.extend(std::env::split_paths(&existing_path));
    let new_path = std::env::join_paths(search_path)
        .map_err(|e| CheckError::Io(std::io::Error::other(e.to_string())))?;

    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c")
        .arg(script)
        .current_dir(dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    cmd.env_clear();
    cmd.env("PATH", new_path);
    if let Some(home) = std::env::var_os("HOME") {
        cmd.env("HOME", home);
    }
    cmd.env_remove("DRUMS_INGEST_URL");
    // Belt-and-suspenders is `kill_on_drop(true)` above, reaching only the
    // direct `sh` process; the process-group leadership below plus
    // `ScriptGroupGuard` is the PRIMARY mechanism, reaching the whole tree.
    set_new_process_group(&mut cmd);

    let child = cmd.spawn()?;
    // Captured eagerly, before `wait_with_output` consumes `child` by
    // value below.
    let _group_guard = ScriptGroupGuard { pgid: child.id() };

    let output =
        match tokio::time::timeout(Duration::from_millis(timeout_ms), child.wait_with_output())
            .await
        {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => return Err(CheckError::Io(e)),
            // `kill_on_drop(true)` reaps the direct `sh` child when the
            // timed-out future (which owns it, via `wait_with_output`) is
            // dropped here. `_group_guard`'s own `Drop` — running at every
            // return point of this function, including this one — SIGKILLs the
            // whole process group, so a grandchild (a bundler worker, a hung
            // subprocess the build script itself started) can't outlive a
            // timeout the way a bare `kill_on_drop` alone would let it.
            Err(_) => {
                return Err(CheckError::Timeout {
                    what,
                    ms: timeout_ms,
                })
            }
        };
    Ok(ScriptRun {
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
    // `_group_guard` drops here too on the success path — a no-op (ESRCH)
    // once the group has already fully exited, but still catches a script
    // that intentionally left a background job running past its own exit.
}

/// The first non-empty line of stderr, falling back to stdout — the single
/// most useful line for a "build failed: ..." claim. Capped so one
/// pathologically long line can't dominate a claim's text.
fn first_error_line(run: &ScriptRun) -> String {
    let line = run
        .stderr
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .or_else(|| run.stdout.lines().map(str::trim).find(|l| !l.is_empty()))
        .unwrap_or("(no output)");
    truncate_chars(line, 200)
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

/// Last `max_chars` characters of `s`, on a char boundary — never splits a
/// multi-byte UTF-8 character.
fn tail(s: &str, max_chars: usize) -> String {
    let s = s.trim();
    let count = s.chars().count();
    if count <= max_chars {
        return s.to_string();
    }
    s.chars().skip(count - max_chars).collect()
}

fn short_rev(rev: &str) -> String {
    rev.trim_end_matches('^').chars().take(8).collect()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Create `branch` from the worktree's current (detached) HEAD, stage
/// everything the agent touched, and commit — mirrors
/// `engine/crates/cli/src/engine.rs`'s `commit_repair`, kept as its own copy
/// here rather than a shared dependency so this crate never has to depend on
/// the `cli` binary crate.
fn commit_check_repair(worktree: &Path, branch: &str, summary: &str) -> Result<String, String> {
    run_git(worktree, &["switch", "-c", branch])?;
    run_git(worktree, &["add", "-A"])?;
    // Identity stated per-invocation, same rule as commit_repair over in the
    // cli crate: a CI runner has no global git config, and the first repair
    // that ever reproduced in CI died on exactly that.
    run_git(
        worktree,
        &[
            "-c",
            "user.name=drums",
            "-c",
            "user.email=repairs@drums.sh",
            "commit",
            "-m",
            &format!("check-repair: {summary}"),
        ],
    )?;
    Ok(run_git(worktree, &["rev-parse", "HEAD"])?
        .trim()
        .to_string())
}

fn run_git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_repair::{RepairAttempt, RepairError};
    use std::sync::Arc;

    fn run_git_ok(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .expect("git spawn");
        assert!(status.success(), "git {args:?} failed in {}", dir.display());
    }

    fn commit_all(dir: &Path) -> String {
        run_git_ok(dir, &["add", "-A"]);
        run_git_ok(dir, &["commit", "-qm", "c1"]);
        String::from_utf8(
            std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string()
    }

    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        run_git_ok(dir.path(), &["init", "-q"]);
        run_git_ok(dir.path(), &["config", "user.email", "t@t"]);
        run_git_ok(dir.path(), &["config", "user.name", "t"]);
        dir
    }

    // -- fixture: build passes, no test script -------------------------------

    fn fixture_build_pass() -> (tempfile::TempDir, String) {
        let dir = init_repo();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"scripts":{"build":"node build.js"}}"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("build.js"),
            "console.log('build ok');\nprocess.exit(0);\n",
        )
        .unwrap();
        let sha = commit_all(dir.path());
        (dir, sha)
    }

    // -- fixture: no scripts.build at all ------------------------------------

    fn fixture_no_build_script() -> (tempfile::TempDir, String) {
        let dir = init_repo();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"scripts":{"start":"node server.js"}}"#,
        )
        .unwrap();
        std::fs::write(dir.path().join("server.js"), "// not used\n").unwrap();
        let sha = commit_all(dir.path());
        (dir, sha)
    }

    // -- fixture: build passes, declared test script fails ------------------

    fn fixture_test_script_fails() -> (tempfile::TempDir, String) {
        let dir = init_repo();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"scripts":{"build":"node build.js","test":"node test.js"}}"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("build.js"),
            "console.log('build ok');\nprocess.exit(0);\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("test.js"),
            "console.error('AssertionError: expected true');\nprocess.exit(1);\n",
        )
        .unwrap();
        let sha = commit_all(dir.path());
        (dir, sha)
    }

    // -- fixture: build fails, fixable by a fake agent -----------------------

    const BROKEN_BUILD_JS: &str =
        "console.error('SyntaxError: unexpected token )');\nprocess.exit(1);\n";
    const FIXED_BUILD_JS: &str = "console.log('build ok');\nprocess.exit(0);\n";

    fn fixture_build_fail_fixable() -> (tempfile::TempDir, String) {
        let dir = init_repo();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"scripts":{"build":"node build.js"}}"#,
        )
        .unwrap();
        std::fs::write(dir.path().join("build.js"), BROKEN_BUILD_JS).unwrap();
        let sha = commit_all(dir.path());
        (dir, sha)
    }

    fn fixture_build_fail_with_test_script() -> (tempfile::TempDir, String) {
        let dir = init_repo();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"scripts":{"build":"node build.js","test":"node test.js"}}"#,
        )
        .unwrap();
        std::fs::write(dir.path().join("build.js"), BROKEN_BUILD_JS).unwrap();
        std::fs::write(dir.path().join("test.js"), "process.exit(0);\n").unwrap();
        let sha = commit_all(dir.path());
        (dir, sha)
    }

    // -- fake agents ----------------------------------------------------------

    /// Fixes `build.js` to the passing version — the "fake agent" the lane
    /// brief's `build-fail-fixable-by-fake-agent` fixture is fixed by.
    struct FixingAgent {
        seen_acceptance: Arc<std::sync::Mutex<Vec<String>>>,
    }
    impl FixingAgent {
        fn new() -> Self {
            FixingAgent {
                seen_acceptance: Arc::new(std::sync::Mutex::new(vec![])),
            }
        }
    }
    #[async_trait::async_trait]
    impl RepairAgent for FixingAgent {
        async fn repair(
            &self,
            worktree: &Path,
            ctx: &RepairContext,
        ) -> Result<RepairAttempt, RepairError> {
            *self.seen_acceptance.lock().unwrap() = ctx.acceptance.clone();
            std::fs::write(worktree.join("build.js"), FIXED_BUILD_JS).map_err(RepairError::Io)?;
            Ok(RepairAttempt {
                summary: "fixed the build".to_string(),
                diff_stat: "build.js | 2 +-".to_string(),
            })
        }
        fn name(&self) -> &str {
            "fake-fixing-agent"
        }
    }

    /// Edits an unrelated file — a real diff, but one that never touches
    /// `build.js`, so re-check must still fail.
    struct WrongFixAgent;
    #[async_trait::async_trait]
    impl RepairAgent for WrongFixAgent {
        async fn repair(
            &self,
            worktree: &Path,
            _ctx: &RepairContext,
        ) -> Result<RepairAttempt, RepairError> {
            std::fs::write(worktree.join("NOTES.md"), "tried\n").map_err(RepairError::Io)?;
            Ok(RepairAttempt {
                summary: "left a note".to_string(),
                diff_stat: "NOTES.md | 1 +".to_string(),
            })
        }
        fn name(&self) -> &str {
            "fake-wrong-fix-agent"
        }
    }

    struct FailingAgent;
    #[async_trait::async_trait]
    impl RepairAgent for FailingAgent {
        async fn repair(
            &self,
            _worktree: &Path,
            _ctx: &RepairContext,
        ) -> Result<RepairAttempt, RepairError> {
            Err(RepairError::NoChanges)
        }
        fn name(&self) -> &str {
            "fake-failing-agent"
        }
    }

    /// Panics if ever invoked — proves `check_and_repair` never calls the
    /// agent when the initial build already passed.
    struct NeverCalledAgent;
    #[async_trait::async_trait]
    impl RepairAgent for NeverCalledAgent {
        async fn repair(
            &self,
            _worktree: &Path,
            _ctx: &RepairContext,
        ) -> Result<RepairAttempt, RepairError> {
            panic!("agent must not be invoked when the build already passes");
        }
        fn name(&self) -> &str {
            "fake-never-called-agent"
        }
    }

    // -- check_revision ---------------------------------------------------

    #[tokio::test]
    async fn build_pass_with_no_test_script_yields_verified_build_claim_and_no_tests_ok() {
        let (dir, sha) = fixture_build_pass();
        let outcome = check_revision(dir.path(), &sha, 15_000)
            .await
            .expect("check must succeed");
        assert!(outcome.build_ok);
        assert_eq!(
            outcome.tests_ok, None,
            "no scripts.test declared — must never be Some either way"
        );
        assert!(outcome
            .claims
            .iter()
            .any(|c| c.text.contains("build passed") && c.provenance == Provenance::Verified));
        assert!(
            outcome.log_excerpt.is_empty(),
            "nothing failed — no excerpt to show"
        );
    }

    #[tokio::test]
    async fn no_build_script_is_an_honest_check_error_never_a_fabricated_pass() {
        let (dir, sha) = fixture_no_build_script();
        let err = check_revision(dir.path(), &sha, 15_000)
            .await
            .expect_err("must error, not fabricate a pass");
        assert!(
            matches!(err, CheckError::NoBuildScript),
            "expected NoBuildScript, got {err:?}"
        );
    }

    #[tokio::test]
    async fn test_script_failure_reports_tests_ok_false_never_a_fabricated_pass() {
        let (dir, sha) = fixture_test_script_fails();
        let outcome = check_revision(dir.path(), &sha, 15_000)
            .await
            .expect("check must succeed (build passed)");
        assert!(outcome.build_ok);
        assert_eq!(outcome.tests_ok, Some(false));
        assert!(outcome
            .claims
            .iter()
            .any(|c| c.text.starts_with("tests failed:") && c.text.contains("AssertionError")));
        assert!(
            outcome
                .claims
                .iter()
                .all(|c| c.provenance == Provenance::Verified),
            "the failure itself was observed executing — still verified"
        );
        assert!(outcome.log_excerpt.contains("AssertionError"));
    }

    #[tokio::test]
    async fn build_failure_reports_first_error_line_and_a_log_excerpt_and_never_runs_tests() {
        let (dir, sha) = fixture_build_fail_with_test_script();
        let outcome = check_revision(dir.path(), &sha, 15_000)
            .await
            .expect("check must succeed (it's the build that fails, not the pipeline)");
        assert!(!outcome.build_ok);
        assert_eq!(
            outcome.tests_ok, None,
            "tests must never run (or be claimed either way) once the build failed"
        );
        assert!(
            outcome.claims.len() == 1,
            "only the build failure claim, since tests never ran: {:?}",
            outcome.claims
        );
        assert!(outcome.claims[0].text.starts_with("build failed:"));
        assert!(outcome.claims[0].text.contains("SyntaxError"));
        assert!(outcome.claims[0].provenance == Provenance::Verified);
        assert!(outcome.log_excerpt.contains("SyntaxError"));
    }

    #[tokio::test]
    async fn a_non_hex_rev_is_rejected_by_the_inherited_sha_guard_not_run_as_a_git_argument() {
        let (dir, _sha) = fixture_build_pass();
        let err = check_revision(dir.path(), "--upload-pack=evil", 15_000)
            .await
            .expect_err("must reject a flag-shaped rev");
        assert!(matches!(err, CheckError::Worktree(_)));
    }

    #[tokio::test]
    async fn a_build_that_never_exits_times_out_honestly_rather_than_hanging() {
        let dir = init_repo();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"scripts":{"build":"node build.js"}}"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("build.js"),
            "setTimeout(() => {}, 60000);\n",
        )
        .unwrap();
        let sha = commit_all(dir.path());

        let err = check_revision(dir.path(), &sha, 300)
            .await
            .expect_err("must time out, not hang");
        assert!(
            matches!(
                err,
                CheckError::Timeout {
                    what: "build",
                    ms: 300
                }
            ),
            "expected a build timeout, got {err:?}"
        );
    }

    // -- check_and_repair ---------------------------------------------------

    #[tokio::test]
    async fn a_build_that_already_passes_never_invokes_the_agent_and_makes_no_branch() {
        let (dir, sha) = fixture_build_pass();
        let outcome = check_and_repair(dir.path(), &sha, 15_000, &NeverCalledAgent)
            .await
            .expect("must succeed");
        assert!(outcome.initial.build_ok);
        assert!(!outcome.repaired);
        assert!(outcome.branch.is_none());
        assert!(outcome.commit_sha.is_none());
        assert!(outcome.verify.is_none());
        assert!(outcome.repair_failure.is_none());
    }

    #[tokio::test]
    async fn a_fixable_build_failure_is_repaired_committed_and_verified() {
        let (dir, sha) = fixture_build_fail_fixable();
        let short = &sha[..8];
        let expected_branch = format!("drums/check-{short}");

        let outcome = check_and_repair(dir.path(), &sha, 15_000, &FixingAgent::new())
            .await
            .expect("must succeed");

        assert!(
            !outcome.initial.build_ok,
            "the fixture's build must genuinely fail before repair"
        );
        assert!(
            outcome.repaired,
            "a fixable failure with a real fix must end up repaired: {:?}",
            outcome.repair_failure
        );
        assert_eq!(outcome.branch.as_deref(), Some(expected_branch.as_str()));
        let commit_sha = outcome.commit_sha.expect("a commit must exist");
        assert!(!commit_sha.is_empty());
        let verify = outcome.verify.expect("a re-check must have run");
        assert!(verify.build_ok);
        assert!(outcome.repair_failure.is_none());

        // git is the record: the branch + commit persist in the ORIGIN repo,
        // not only in a (by now removed) worktree.
        let branches = std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["branch", "--list", &expected_branch])
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&branches.stdout).contains(&expected_branch),
            "the repair branch must exist in the origin repo"
        );
    }

    #[tokio::test]
    async fn the_agent_sees_a_test_acceptance_criterion_only_when_a_test_script_is_declared() {
        let (dir, sha) = fixture_build_fail_with_test_script();
        let agent = FixingAgent::new();
        let seen = Arc::clone(&agent.seen_acceptance);
        let _ = check_and_repair(dir.path(), &sha, 15_000, &agent)
            .await
            .expect("must succeed");
        let acceptance = seen.lock().unwrap().clone();
        assert!(
            acceptance.iter().any(|a| a.contains("build")),
            "build criterion must always be present: {acceptance:?}"
        );
        assert!(
            acceptance.iter().any(|a| a.to_lowercase().contains("test")),
            "a test script is declared — acceptance must mention it: {acceptance:?}"
        );
    }

    #[tokio::test]
    async fn an_agent_that_produces_no_fix_is_reported_honestly_with_no_branch() {
        let (dir, sha) = fixture_build_fail_fixable();
        let outcome = check_and_repair(dir.path(), &sha, 15_000, &FailingAgent)
            .await
            .expect("must succeed at the check_and_repair level");
        assert!(!outcome.repaired);
        assert!(outcome.branch.is_none());
        assert!(outcome.commit_sha.is_none());
        assert!(outcome.repair_failure.is_some());
    }

    #[tokio::test]
    async fn a_committed_fix_that_still_fails_the_rebuild_is_reported_honestly_but_the_branch_persists(
    ) {
        let (dir, sha) = fixture_build_fail_fixable();
        let outcome = check_and_repair(dir.path(), &sha, 15_000, &WrongFixAgent)
            .await
            .expect("must succeed at the check_and_repair level");
        assert!(
            !outcome.repaired,
            "an unrelated-file edit must never be reported as a repair"
        );
        assert!(
            outcome.branch.is_some(),
            "a real commit was made even though it didn't fix the build"
        );
        assert!(outcome.commit_sha.is_some());
        let verify = outcome
            .verify
            .expect("a re-check must have run against the commit");
        assert!(!verify.build_ok, "the rebuild must still genuinely fail");
        assert!(outcome.repair_failure.is_some());
    }

    #[tokio::test]
    async fn repair_never_triggers_on_a_test_only_failure_this_crate_is_the_build_failure_core() {
        let (dir, sha) = fixture_test_script_fails();
        let outcome = check_and_repair(dir.path(), &sha, 15_000, &NeverCalledAgent)
            .await
            .expect("must succeed");
        assert!(outcome.initial.build_ok);
        assert_eq!(outcome.initial.tests_ok, Some(false));
        assert!(!outcome.repaired);
        assert!(
            outcome.branch.is_none(),
            "a test-only failure must never trigger the build-failure repair path"
        );
    }

    // -- propose-only: no ship / deploy surface at all -----------------------

    #[test]
    fn engine_check_exposes_no_ship_or_deploy_surface() {
        let src = include_str!("lib.rs");
        // Code-shaped tokens only (identifiers / call sites), not the plain
        // English words this file's own doc comments use to say "we don't do
        // this" — those would otherwise trip the count on their own prose.
        for banned in [
            "deploy_cmd",
            "fn ship(",
            "fn deploy(",
            "fn revert(",
            "DeployCmd",
            "ShipOutcome",
            "check_url",
        ] {
            let hits = src.matches(banned).count();
            // Each banned token appears exactly once: right here, inside this
            // very assertion list — never anywhere else in the crate. A count
            // above 1 means the token leaked into real (non-test) code.
            assert!(hits <= 1, "engine-check must never gain a deploy/ship surface — found {hits} occurrences of {banned:?} (expected at most the 1 in this assertion itself)");
        }
    }
}
