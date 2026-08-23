//! Adversarial regression tests from the lane-check review
//! (`.superpowers/sdd/lane-check-review.md`). The first two scenarios began
//! life as an untracked, reviewer-only probe file (also named
//! `tests/adversarial.rs`) left in a scratch review worktree — never part of
//! the commit under review. They are committed here for real, updated to
//! assert the FIXED behavior rather than the bug: scenario 2 used to prove
//! must-fix #1 was real (a bare `Err` with no branch/commit/worktree
//! mentioned); it now guards that the fix holds. Scenario 3 is new, guarding
//! must-fix #2 (process-group kill for timed-out build/test scripts).

use engine_check::{check_and_repair, check_revision, CheckError};
use engine_repair::{RepairAgent, RepairAttempt, RepairContext, RepairError};
use std::path::Path;
use std::time::Duration;

fn git(dir: &Path, args: &[&str]) {
    let st = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .status()
        .unwrap();
    assert!(st.success(), "git {args:?}");
}

fn fixture() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);
    git(dir.path(), &["config", "user.email", "t@t"]);
    git(dir.path(), &["config", "user.name", "t"]);
    std::fs::write(
        dir.path().join("package.json"),
        r#"{"scripts":{"build":"node build.js"}}"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("build.js"),
        "console.error('SyntaxError: boom');\nprocess.exit(1);\n",
    )
    .unwrap();
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-qm", "c1"]);
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir.path())
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    let sha = String::from_utf8(out.stdout).unwrap().trim().to_string();
    (dir, sha)
}

/// Agent "fixes" the build by rewriting scripts.build to a no-op.
struct NeuteringAgent;
#[async_trait::async_trait]
impl RepairAgent for NeuteringAgent {
    async fn repair(
        &self,
        worktree: &Path,
        _ctx: &RepairContext,
    ) -> Result<RepairAttempt, RepairError> {
        std::fs::write(
            worktree.join("package.json"),
            r#"{"scripts":{"build":"true"}}"#,
        )
        .map_err(RepairError::Io)?;
        Ok(RepairAttempt {
            summary: "neutered the build script".to_string(),
            diff_stat: "package.json | 1 +-".to_string(),
        })
    }
    fn name(&self) -> &str {
        "neutering-agent"
    }
}

/// Agent deletes scripts.build entirely — the re-check itself then errors
/// with `CheckError::NoBuildScript`, which is exactly the scenario must-fix
/// #1 in the lane-check review is about.
struct DeletingAgent;
#[async_trait::async_trait]
impl RepairAgent for DeletingAgent {
    async fn repair(
        &self,
        worktree: &Path,
        _ctx: &RepairContext,
    ) -> Result<RepairAttempt, RepairError> {
        std::fs::write(
            worktree.join("package.json"),
            r#"{"scripts":{"start":"node x.js"}}"#,
        )
        .map_err(RepairError::Io)?;
        Ok(RepairAttempt {
            summary: "removed the build script".to_string(),
            diff_stat: "package.json | 1 +-".to_string(),
        })
    }
    fn name(&self) -> &str {
        "deleting-agent"
    }
}

// -- scenario 1: known-weak acceptance bar, not a bug (review's "Note") ----

#[tokio::test]
async fn neutering_the_build_script_earns_repaired_true_and_a_verified_claim() {
    // Not a bug: the acceptance bar is literally "the script exits 0", so a
    // neutered build honestly clears it — the safety net is that this
    // crate is propose-only (never ships/deploys) and the
    // `package.json | 1 +-` diff on `drums/check-<short>` is trivially
    // visible to whoever reads the branch next. Kept as a committed
    // regression test so this known, accepted trade-off can't silently
    // drift (e.g. a future change that starts trusting `repaired` without
    // a human in the loop would need to revisit this).
    let (dir, sha) = fixture();
    let outcome = check_and_repair(dir.path(), &sha, 15_000, &NeuteringAgent)
        .await
        .expect("no pipeline error");
    assert!(outcome.repaired, "a script that genuinely exits 0 must be accepted — the acceptance bar is real, not fabricated");
}

// -- scenario 2: must-fix #1, evidence-preserving re-check failures --------

#[tokio::test]
async fn deleting_the_build_script_no_longer_loses_the_committed_branch() {
    // Must-fix #1: previously, an agent "fix" that made the RE-CHECK itself
    // error (here: deleting scripts.build -> CheckError::NoBuildScript) hit
    // a bare `?` in `check_and_repair`, which returned a bare `Err` naming
    // neither the branch nor the commit `commit_check_repair` had already
    // created in the origin repo, and dropped the worktree (the only
    // on-disk evidence of what the agent did) via `ManagedWorktree`'s
    // `Drop` — `keep_on_drop` was never set on this path. Fixed:
    // `check_and_repair` now surfaces this as an honest
    // `Ok(CheckAndRepairOutcome { .. })`, the same shape its other three
    // failure branches already use, and keeps the worktree on disk.
    let (dir, sha) = fixture();
    let short = &sha[..8];
    let expected_branch = format!("drums/check-{short}");

    let outcome = check_and_repair(dir.path(), &sha, 15_000, &DeletingAgent)
        .await
        .expect("a re-check error must never bare-Err away a real commit — it must be surfaced as an honest outcome");

    assert!(
        !outcome.repaired,
        "a re-check that itself errored must never be reported as repaired"
    );
    assert_eq!(
        outcome.branch.as_deref(),
        Some(expected_branch.as_str()),
        "the branch a real commit landed on must be named in the outcome"
    );
    let commit_sha = outcome
        .commit_sha
        .clone()
        .expect("the commit sha must be carried, not dropped");
    assert!(!commit_sha.is_empty());

    let failure = outcome
        .repair_failure
        .clone()
        .expect("must explain honestly why repair wasn't earned");
    assert!(
        failure.contains(&expected_branch),
        "the failure message itself must name the branch, not just an opaque error: {failure:?}"
    );

    // git is the record: the branch must genuinely exist in the ORIGIN
    // repo, not only have existed transiently in a now-deleted worktree.
    let branches = std::process::Command::new("git")
        .arg("-C")
        .arg(dir.path())
        .args(["branch", "--list", &expected_branch])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&branches.stdout).contains(&expected_branch),
        "the branch must actually exist in the origin repo"
    );

    // The worktree — the only place a human can see exactly what the agent
    // did — must still be on disk, not removed by `ManagedWorktree`'s Drop.
    let worktree_dir = outcome
        .worktree_dir
        .clone()
        .expect("the worktree path must be carried so a human can go look");
    assert!(worktree_dir.exists(), "the worktree directory must still exist on disk for inspection, not have been deleted on drop");

    // Best-effort cleanup so the test doesn't litter the real tmp dir.
    let _ = std::process::Command::new("git")
        .arg("-C")
        .arg(dir.path())
        .args(["worktree", "remove", "--force"])
        .arg(&worktree_dir)
        .status();
    let _ = std::fs::remove_dir_all(&worktree_dir);
}

// -- scenario 3: must-fix #2, process-group kill for build scripts ---------

/// Whether `pid` names a live process. A SIGKILLed process stays visible to
/// `ps` as a zombie until it's reaped, so `Z` counts as dead too — mirrors
/// `engine-repair`'s own `process_is_alive` test helper exactly.
#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    let out = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .expect("ps must run");
    let state = String::from_utf8_lossy(&out.stdout).trim().to_string();
    !state.is_empty() && !state.starts_with('Z')
}

/// Polls until `f` holds or `bound` elapses; returns whether it held.
#[cfg(unix)]
async fn within(bound: Duration, mut f: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + bound;
    loop {
        if f() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[cfg(unix)]
fn pid_from_dump(dumped: &str, key: &str) -> u32 {
    dumped
        .lines()
        .find_map(|l| l.strip_prefix(key))
        .unwrap_or_else(|| panic!("pid dump missing {key:?}:\n{dumped}"))
        .trim()
        .parse()
        .expect("pid must parse")
}

/// Must-fix #2: a timed-out build script must group-kill the WHOLE process
/// tree, not just the direct `sh` child — mirrors the exact grandchild-kill
/// discipline already tested in `engine-repair::ChildGuard`
/// (`dropping_the_child_guard_kills_the_agent_and_its_grandchildren`) and
/// `engine-repro::BootedApp`, applied to `engine-check`'s own script spawn
/// site (`run_script`, driving `npm run build`/`test`, which almost always
/// forks at least one further child — npm's own process, then the actual
/// build tool). Asserted by PID, not a side-effect marker, and only after
/// both PIDs are observed genuinely alive so the test cannot pass vacuously.
#[cfg(unix)]
#[tokio::test]
async fn a_timed_out_build_script_group_kills_its_grandchild_not_just_the_direct_shell() {
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q"]);
    git(repo.path(), &["config", "user.email", "t@t"]);
    git(repo.path(), &["config", "user.name", "t"]);

    let pids_dump = repo.path().join("pids.txt");
    let fixture_script = format!(
        "{}/tests/fixtures/hanging-build.sh",
        env!("CARGO_MANIFEST_DIR")
    );
    let build_cmd = format!("'{fixture_script}' '{}'", pids_dump.display());
    std::fs::write(
        repo.path().join("package.json"),
        format!(r#"{{"scripts":{{"build":"{build_cmd}"}}}}"#),
    )
    .unwrap();
    git(repo.path(), &["add", "-A"]);
    git(repo.path(), &["commit", "-qm", "c1"]);
    let sha = String::from_utf8(
        std::process::Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();

    // `check_revision` blocks until it returns the timeout `Err`, so the
    // liveness check below has to run concurrently with it (in its own
    // task) rather than after it — by the time the call returns, the group
    // kill this test exists to verify has already happened.
    let repo_path = repo.path().to_path_buf();
    let handle = tokio::spawn(async move { check_revision(&repo_path, &sha, 500).await });

    assert!(
        within(Duration::from_secs(10), || pids_dump.exists()).await,
        "fixture never wrote its pid dump — the test would otherwise pass without a process tree to kill"
    );
    let dumped = std::fs::read_to_string(&pids_dump).unwrap();
    let (script_pid, grandchild_pid) = (
        pid_from_dump(&dumped, "parent="),
        pid_from_dump(&dumped, "grandchild="),
    );
    assert!(
        process_is_alive(script_pid),
        "the build script's own process must be alive before the timeout"
    );
    assert!(
        within(Duration::from_secs(3), || process_is_alive(grandchild_pid)).await,
        "the grandchild must be alive before the timeout — otherwise there was no process tree to kill"
    );

    let err = handle
        .await
        .unwrap()
        .expect_err("must time out, not hang for the 300s the fixture would otherwise sleep");
    assert!(
        matches!(err, CheckError::Timeout { what: "build", .. }),
        "expected a build timeout, got {err:?}"
    );

    assert!(
        within(Duration::from_secs(3), || !process_is_alive(script_pid)).await,
        "the build script's own process survived the timeout"
    );
    assert!(
        within(Duration::from_secs(3), || !process_is_alive(grandchild_pid)).await,
        "a grandchild of the build script ({grandchild_pid}) survived the timeout — the kill reached only the direct `sh` process, exactly the orphaned-bundler-worker regression this test guards against"
    );
}

// --- reported-issue repairs (Scenario C) ------------------------------------

/// A repo whose build (and test) genuinely pass — the starting state for a
/// reported-issue repair, which begins from a HEALTHY tree.
fn healthy_fixture() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);
    git(dir.path(), &["config", "user.email", "t@t"]);
    git(dir.path(), &["config", "user.name", "t"]);
    std::fs::write(
        dir.path().join("package.json"),
        r#"{"scripts":{"build":"node build.js","test":"node test.js"}}"#,
    )
    .unwrap();
    std::fs::write(dir.path().join("build.js"), "process.exit(0);\n").unwrap();
    std::fs::write(
        dir.path().join("test.js"),
        "console.log('1 passing');\nprocess.exit(0);\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("ui.js"), "// the button\n").unwrap();
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-qm", "c1"]);
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir.path())
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    let sha = String::from_utf8(out.stdout).unwrap().trim().to_string();
    (dir, sha)
}

/// Makes a small, harmless edit — enough to produce a commit, without
/// touching the build or the tests.
struct EditingAgent;
#[async_trait::async_trait]
impl RepairAgent for EditingAgent {
    async fn repair(
        &self,
        worktree: &Path,
        _ctx: &RepairContext,
    ) -> Result<RepairAttempt, RepairError> {
        std::fs::write(worktree.join("ui.js"), "// the button, moved up\n")
            .map_err(RepairError::Io)?;
        Ok(RepairAttempt {
            summary: "moved the button up".to_string(),
            diff_stat: " ui.js | 2 +-".to_string(),
        })
    }
    fn name(&self) -> &str {
        "editing"
    }
}

fn issue_task() -> engine_check::IssueTask {
    engine_check::IssueTask {
        id: "e1f2".into(),
        source: "linear".into(),
        title: "checkout button overlaps the price on mobile".into(),
        body: "on iPhone the button sits on top of the total".into(),
        url: Some("https://linear.app/drums/issue/DRM-42".into()),
    }
}

/// The invariant this whole path exists for: however green everything else is,
/// whether the change resolves what the human reported is NEVER claimed. A
/// reported issue is usually a visual complaint, and a passing test suite is
/// not evidence about it.
#[tokio::test]
async fn a_reported_issue_repair_is_always_unresolved_about_the_report_itself() {
    let (repo, rev) = healthy_fixture();
    let out = engine_check::repair_reported_issue(
        repo.path(),
        &rev,
        &issue_task(),
        60_000,
        &EditingAgent,
        Vec::new(),
    )
    .await
    .expect("the repair path should complete");

    assert!(
        out.repaired,
        "a non-regressing edit should pass the bar: {:?}",
        out.repair_failure
    );
    let verify = out.verify.expect("a re-check must have run");
    let unresolved: Vec<&engine_core::Claim> = verify
        .claims
        .iter()
        .filter(|c| c.provenance == engine_core::Provenance::Unresolved)
        .collect();

    assert!(
        unresolved
            .iter()
            .any(|c| c.text.contains("was NOT checked")),
        "no unresolved claim about the report survived: {:?}",
        verify.claims
    );
    assert!(
        unresolved
            .iter()
            .any(|c| c.text.contains("a human has to confirm")),
        "the unresolved claim must name who decides: {:?}",
        verify.claims
    );
    assert!(
        verify
            .claims
            .iter()
            .any(|c| c.provenance == engine_core::Provenance::Verified),
        "the executed build/test claims should still be verified: {:?}",
        verify.claims
    );
}

/// Structural, not conventional: the synthesized failure carries
/// `Intake::Reported`, whose `is_replayable()` is false, so the ship gate
/// refuses it whatever rung an operator sets. If this ever flips, a UI
/// complaint could reach a deploy command.
#[test]
fn a_reported_intake_can_never_ship_alone() {
    let intake = engine_core::Intake::Reported {
        source: "linear".into(),
    };
    assert!(
        !intake.is_replayable(),
        "a reported intake has nothing to replay; making it replayable would let a \
         visual complaint reach a deploy command"
    );
}

/// An agent asked to fix a button in a repo whose build is already broken
/// produces a change nobody can evaluate — and "the build passes" would be a
/// claim that could never be earned. Refuse before spending an agent run.
#[tokio::test]
async fn a_reported_issue_repair_refuses_when_the_build_is_already_broken() {
    let (repo, rev) = fixture(); // this fixture's build exits 1
    let out = engine_check::repair_reported_issue(
        repo.path(),
        &rev,
        &issue_task(),
        60_000,
        &EditingAgent,
        Vec::new(),
    )
    .await
    .expect("it should return an outcome, not an error");

    assert!(!out.repaired);
    assert!(
        out.commit_sha.is_none(),
        "nothing should have been committed"
    );
    let why = out.repair_failure.expect("it must say why");
    assert!(why.contains("already failing its own checks"), "{why}");
    assert!(
        why.contains("drums check"),
        "it must point at the command that fixes it: {why}"
    );
}

/// Found by running the real loop: `engine-check` required `scripts.build`,
/// so a repo declaring only `scripts.test` — most Node libraries, and plenty
/// of apps — was refused for a reason that had nothing to do with the issue.
#[tokio::test]
async fn a_repo_with_tests_but_no_build_script_is_still_repairable() {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);
    git(dir.path(), &["config", "user.email", "t@t"]);
    git(dir.path(), &["config", "user.name", "t"]);
    // No scripts.build at all.
    std::fs::write(
        dir.path().join("package.json"),
        r#"{"scripts":{"test":"node test.js"}}"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("test.js"),
        "console.log('1 passing');\nprocess.exit(0);\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("ui.js"), "// the button\n").unwrap();
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-qm", "c1"]);
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir.path())
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    let rev = String::from_utf8(out.stdout).unwrap().trim().to_string();

    let outcome =
        engine_check::repair_reported_issue(dir.path(), &rev, &issue_task(), 60_000, &EditingAgent, Vec::new())
            .await
            .expect("a repo with no build step is not a misconfiguration here");

    assert!(
        outcome.repaired,
        "a repo declaring only scripts.test must still be repairable: {:?}",
        outcome.repair_failure
    );
    let verify = outcome.verify.expect("a re-check must have run");
    assert_eq!(verify.tests_ok, Some(true));
    assert!(
        verify
            .claims
            .iter()
            .all(|c| !c.text.contains("build passed")),
        "no build ran, so nothing may claim one passed: {:?}",
        verify.claims
    );
}

/// With NO declared checks, "no worse than before" is not a bar, it is a wish.
#[tokio::test]
async fn a_repo_with_no_checks_at_all_is_refused_with_a_useful_reason() {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);
    git(dir.path(), &["config", "user.email", "t@t"]);
    git(dir.path(), &["config", "user.name", "t"]);
    std::fs::write(dir.path().join("package.json"), r#"{"name":"x"}"#).unwrap();
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-qm", "c1"]);
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir.path())
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    let rev = String::from_utf8(out.stdout).unwrap().trim().to_string();

    let outcome =
        engine_check::repair_reported_issue(dir.path(), &rev, &issue_task(), 60_000, &EditingAgent, Vec::new())
            .await
            .expect("it should refuse, not error");

    assert!(!outcome.repaired);
    let why = outcome.repair_failure.expect("it must say why");
    assert!(
        why.contains("neither scripts.build nor scripts.test"),
        "{why}"
    );
    assert!(
        why.contains("Declare one"),
        "the refusal must tell them how to make it work: {why}"
    );
}
