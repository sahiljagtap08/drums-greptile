//! Integration tests for `engine-repair`, driven entirely by the fake agent
//! fixture at `tests/fixtures/fake-agent.sh` — no test here requires live
//! `claude`/`codex` auth (see the `#[ignore]`d live smoke test at the bottom).

use std::fs;
use std::path::Path;
use std::time::Duration;

use engine_core::{
    Attribution, CapturedRequest, Claim, DeployRecord, ErrorEvent, ErrorSignature, Failure, Intake,
    Provenance,
};
use engine_repair::{
    CliRepairAgent, RepairAgent, RepairContext, RepairError, CODEX_DEFAULT_TEMPLATE,
    DEFAULT_TIMEOUT_MS,
};

const SERVER_JS: &str = r#"const http = require("http");
function computeTotal(body) {
  let total = 0;
  for (const item of body.items || []) total += item.price * item.qty;
  const code = body.promo.code; // BUG: promo may be absent
  if (code === "TEN") total = Math.round(total * 0.9);
  return total;
}
module.exports = { computeTotal };
"#;

fn fixture_path() -> String {
    format!(
        "{}/tests/fixtures/fake-agent.sh",
        env!("CARGO_MANIFEST_DIR")
    )
}

fn run_git(dir: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .status()
        .expect("git spawn");
    assert!(status.success(), "git {args:?} failed in {}", dir.display());
}

/// A tempdir that is itself a git working tree with a committed `server.js`
/// carrying the promo-guard bug the fake agent knows how to fix.
fn init_worktree_fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    run_git(dir.path(), &["init", "-q"]);
    run_git(dir.path(), &["config", "user.email", "test@example.com"]);
    run_git(dir.path(), &["config", "user.name", "Test"]);
    fs::write(dir.path().join("server.js"), SERVER_JS).unwrap();
    run_git(dir.path(), &["add", "server.js"]);
    run_git(dir.path(), &["commit", "-q", "-m", "init"]);
    dir
}

fn sample_failure_with_body(body: &str) -> Failure {
    Failure {
        id: "f1".into(),
        service: "shop".into(),
        signature: ErrorSignature {
            error_name: "TypeError".into(),
            top_frame_file: "server.js".into(),
            top_frame_function: Some("computeTotal".into()),
        },
        first_seen_ms: 1,
        event_count: 3,
        sample: ErrorEvent {
            service: "shop".into(),
            occurred_at_ms: 2,
            error_name: "TypeError".into(),
            error_message: "Cannot read properties of undefined (reading 'code')".into(),
            stack: "TypeError: Cannot read properties of undefined (reading 'code')\n    at computeTotal (/srv/shop/server.js:5:20)".into(),
            request: Some(CapturedRequest {
                method: "POST".into(),
                path: "/api/checkout".into(),
                content_type: Some("application/json".into()),
                body: Some(body.to_string()),
            }),
            intake: Intake::Snippet,
        },
        intake: Intake::Snippet,
        claim: Claim { text: "3 occurrences of TypeError".into(), provenance: Provenance::Observed },
    }
}

fn sample_attribution() -> Attribution {
    Attribution {
        deploy: DeployRecord {
            sha: "deadbeef00".into(),
            description: "loosen promo validation".into(),
            author: "maya".into(),
            deployed_at_ms: 1,
        },
        overlap_files: vec!["server.js".into()],
        minutes_after_deploy: 4,
        claim: Claim {
            text: "deploy deadbee touched server.js 4m before".into(),
            provenance: Provenance::Inferred,
        },
    }
}

fn sample_acceptance() -> Vec<String> {
    vec![
        "POST /api/checkout with the captured body returns 2xx".to_string(),
        "GET /health returns 200".to_string(),
        "keep the diff minimal; do not reformat unrelated code".to_string(),
    ]
}

// -- detect() precedence -------------------------------------------------

/// Serializes tests that mutate PROCESS-GLOBAL env vars (`DRUMS_AGENT_CMD`,
/// `PATH`) that `CliRepairAgent::detect()` reads. `cargo test` runs `#[test]`
/// functions concurrently on threads that all share one process's
/// environment (env vars are not thread-local); without this lock, two such
/// tests running at the same moment could observe or clobber each other's
/// temporary mutation and flake nondeterministically.
static ENV_MUTATION_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn detect_prefers_drums_agent_cmd_env_override() {
    let _guard = ENV_MUTATION_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    // Restored before the guard drops so this process-global mutation can't
    // leak into any other test in this binary.
    let prior = std::env::var("DRUMS_AGENT_CMD").ok();
    std::env::set_var("DRUMS_AGENT_CMD", "some-custom-agent --flag {prompt}");

    let agent = CliRepairAgent::detect().expect("env override must always resolve to an agent");
    assert_eq!(agent.command_template, "some-custom-agent --flag {prompt}");

    match prior {
        Some(v) => std::env::set_var("DRUMS_AGENT_CMD", v),
        None => std::env::remove_var("DRUMS_AGENT_CMD"),
    }
}

/// Proves `detect()` picks `codex` — with the exact `-s workspace-write`
/// template `CliRepairAgent::repair` needs to actually write files (see the
/// `CODEX_DEFAULT_TEMPLATE` doc comment) — when `codex` is on `PATH` and
/// nothing overrides it. Deterministic and fast: `PATH` is narrowed to a
/// scratch dir containing only a stub `codex` file (existence is all
/// `detect()`'s `on_path` checks), so this never depends on what agent CLIs
/// happen to be installed on the machine running the test, and never
/// shells out to a real agent.
#[cfg(unix)]
#[test]
fn detect_prefers_codex_and_builds_the_workspace_write_template_when_only_codex_is_on_path() {
    use std::os::unix::fs::PermissionsExt;

    let _guard = ENV_MUTATION_LOCK.lock().unwrap_or_else(|p| p.into_inner());

    let dir = tempfile::tempdir().unwrap();
    let stub = dir.path().join("codex");
    fs::write(&stub, "#!/bin/sh\nexit 0\n").unwrap();
    let mut perms = fs::metadata(&stub).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&stub, perms).unwrap();

    let prior_path =
        std::env::var_os("PATH").expect("PATH must be set for this test to be meaningful");
    let prior_agent_cmd = std::env::var("DRUMS_AGENT_CMD").ok();

    // Narrow PATH to hide only the directory `claude` actually resolves
    // from, keeping every OTHER directory (notably whatever houses `git`
    // and `sh`) intact: PATH is process-global and `cargo test` runs other
    // tests concurrently on other threads, several of which spawn real
    // `git` subprocesses via the *inherited* PATH — wholesale-replacing
    // PATH with only this test's stub dir starved those concurrent spawns
    // of `git` and made THEM fail with ENOENT, not this test (caught
    // empirically: this is the fix for that regression, not a
    // hypothetical).
    let without_claude: Vec<_> = std::env::split_paths(&prior_path)
        .filter(|p| !p.join("claude").is_file())
        .collect();
    let new_path =
        std::env::join_paths(std::iter::once(dir.path().to_path_buf()).chain(without_claude))
            .expect("join_paths");
    std::env::remove_var("DRUMS_AGENT_CMD");
    std::env::set_var("PATH", &new_path);

    let agent = CliRepairAgent::detect();

    std::env::set_var("PATH", &prior_path);
    match prior_agent_cmd {
        Some(v) => std::env::set_var("DRUMS_AGENT_CMD", v),
        None => std::env::remove_var("DRUMS_AGENT_CMD"),
    }

    let agent = agent.expect("PATH with codex but without claude must still resolve to an agent");
    assert_eq!(agent.command_template, CODEX_DEFAULT_TEMPLATE);
    assert_eq!(agent.name(), "codex");
}

// -- fake agent: produces changes + diff_stat -----------------------------

#[tokio::test]
async fn fake_agent_produces_changes_and_diff_stat() {
    let worktree = init_worktree_fixture();
    let agent = CliRepairAgent {
        command_template: format!("{} {{prompt}}", fixture_path()),
        timeout_ms: 30_000,
    };

    let ctx = RepairContext {
        failure: sample_failure_with_body(r#"{"items":[{"price":10,"qty":2}]}"#),
        attribution: sample_attribution(),
        acceptance: sample_acceptance(),
        remembered: Vec::new(),
    };

    let attempt = agent
        .repair(worktree.path(), &ctx)
        .await
        .expect("fake agent must produce a repair attempt");
    assert!(!attempt.summary.is_empty(), "summary must not be empty");
    assert!(!attempt.diff_stat.is_empty(), "diff_stat must not be empty");
    assert!(
        attempt.diff_stat.contains("server.js"),
        "diff_stat must mention the changed file: {}",
        attempt.diff_stat
    );

    let changed = fs::read_to_string(worktree.path().join("server.js")).unwrap();
    assert!(
        changed.contains("body.promo && body.promo.code"),
        "the fake agent's fix must land in the worktree file"
    );
}

// -- no-op fake -> NoChanges ----------------------------------------------

#[tokio::test]
async fn noop_fake_agent_yields_no_changes_error() {
    let worktree = init_worktree_fixture();
    let agent = CliRepairAgent {
        command_template: format!("{} --noop {{prompt}}", fixture_path()),
        timeout_ms: 30_000,
    };
    let ctx = RepairContext {
        failure: sample_failure_with_body("{}"),
        attribution: sample_attribution(),
        acceptance: sample_acceptance(),
        remembered: Vec::new(),
    };

    let result = agent.repair(worktree.path(), &ctx).await;
    assert!(
        matches!(result, Err(RepairError::NoChanges)),
        "expected NoChanges, got {result:?}"
    );
}

// -- timeout path -----------------------------------------------------------

#[tokio::test]
async fn sleeping_fake_agent_times_out_and_is_killed() {
    let worktree = init_worktree_fixture();
    // Sleeps far longer than the configured timeout.
    let agent = CliRepairAgent {
        command_template: format!("{} --sleep-secs 5 {{prompt}}", fixture_path()),
        timeout_ms: 200,
    };
    let ctx = RepairContext {
        failure: sample_failure_with_body("{}"),
        attribution: sample_attribution(),
        acceptance: sample_acceptance(),
        remembered: Vec::new(),
    };

    let started = std::time::Instant::now();
    let result = agent.repair(worktree.path(), &ctx).await;
    let elapsed = started.elapsed();

    assert!(
        matches!(result, Err(RepairError::Timeout)),
        "expected Timeout, got {result:?}"
    );
    assert!(elapsed < Duration::from_secs(4), "the child must be killed promptly on timeout, not waited out to completion; took {elapsed:?}");
}

// -- prompt content ----------------------------------------------------------

#[tokio::test]
async fn prompt_carries_raw_captured_body_and_every_acceptance_line() {
    let worktree = init_worktree_fixture();
    let dump = worktree.path().join("prompt-dump.txt");
    let agent = CliRepairAgent {
        command_template: format!(
            "{} --dump-prompt-to {} {{prompt}}",
            fixture_path(),
            dump.display()
        ),
        timeout_ms: 30_000,
    };

    let marker_body = r#"{"items":[{"price":10,"qty":2}],"note":"unique-marker-body-x7q2"}"#;
    let acceptance = sample_acceptance();
    let ctx = RepairContext {
        failure: sample_failure_with_body(marker_body),
        attribution: sample_attribution(),
        acceptance: acceptance.clone(),
        remembered: Vec::new(),
    };

    agent
        .repair(worktree.path(), &ctx)
        .await
        .expect("fake agent should succeed");

    let dumped =
        fs::read_to_string(&dump).expect("fake agent must have dumped the prompt it received");
    assert!(
        dumped.contains(marker_body),
        "prompt must carry the RAW captured request body verbatim:\n{dumped}"
    );
    for line in &acceptance {
        assert!(
            dumped.contains(line.as_str()),
            "prompt must include acceptance line {line:?}:\n{dumped}"
        );
    }
    assert!(
        dumped.contains("TypeError"),
        "prompt must include the failure signature:\n{dumped}"
    );
    assert!(
        dumped.contains("do not run git commands"),
        "prompt must include the worktree-boundary rule:\n{dumped}"
    );
}

// -- process-tree kill on timeout --------------------------------------------

/// CRITICAL: `child.start_kill()` alone only signals the direct child; a
/// real agent CLI's own subprocesses (tool calls, MCP servers) live on as
/// orphans and keep editing the worktree after `repair()` has already
/// returned `Err(Timeout)`. `CliRepairAgent` must kill the whole process
/// group so nothing survives past the declared failure.
#[tokio::test]
async fn timeout_kills_the_whole_process_tree_not_just_the_direct_child() {
    let worktree = init_worktree_fixture();
    let marker = worktree.path().join("grandchild-marker.txt");
    // The fake agent backgrounds a grandchild that sleeps 1s then writes
    // `marker`, and the fake agent itself sleeps 5s (far past the 200ms
    // timeout) so CliRepairAgent is guaranteed to declare Timeout first.
    let agent = CliRepairAgent {
        command_template: format!(
            "{} --sleep-secs 5 --spawn-grandchild-write {} {{prompt}}",
            fixture_path(),
            marker.display()
        ),
        timeout_ms: 200,
    };
    let ctx = RepairContext {
        failure: sample_failure_with_body("{}"),
        attribution: sample_attribution(),
        acceptance: sample_acceptance(),
        remembered: Vec::new(),
    };

    let result = agent.repair(worktree.path(), &ctx).await;
    assert!(
        matches!(result, Err(RepairError::Timeout)),
        "expected Timeout, got {result:?}"
    );

    // Wait well past the 1s the grandchild would need to write its marker
    // if it survived the timeout kill.
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        !marker.exists(),
        "a grandchild of the timed-out agent wrote into the worktree after Drums declared failure — the process tree was not fully killed"
    );
}

// -- residual pipe drain is bounded independently of timeout_ms --------------

/// IMPORTANT (round-2 F2): the deadline over `child.wait()` and the deadline
/// over draining the residual stdout/stderr pipes must NOT be the same
/// budget. Here the fake agent applies its REAL fix and exits almost
/// immediately (well inside the small `timeout_ms`), but backgrounds a
/// process that holds the inherited stdout pipe open for far longer than
/// `timeout_ms`. A single shared deadline over wait+drain (the pre-fix
/// shape) discards this already-successful repair as a false `Timeout`
/// after the full `timeout_ms` elapses — exactly the bug this test now
/// pins closed: the fix on disk is real and must be reported, bounded only
/// by the short residual-drain grace, not by `timeout_ms` and not by the
/// background process's own lifetime.
#[tokio::test]
async fn stdout_held_open_by_a_background_process_does_not_discard_a_successful_repair() {
    let worktree = init_worktree_fixture();
    let agent = CliRepairAgent {
        command_template: format!("{} --hold-stdout-secs 5 {{prompt}}", fixture_path()),
        timeout_ms: 1_000,
    };
    let ctx = RepairContext {
        failure: sample_failure_with_body("{}"),
        attribution: sample_attribution(),
        acceptance: sample_acceptance(),
        remembered: Vec::new(),
    };

    let started = std::time::Instant::now();
    // Outer safety-net bound so a real hang fails this test in seconds
    // instead of the whole gate timing out.
    let outer =
        tokio::time::timeout(Duration::from_secs(6), agent.repair(worktree.path(), &ctx)).await;
    let elapsed = started.elapsed();

    let result = outer.expect("repair() must return well within the outer 6s bound, not hang while a background process holds stdout open");
    let attempt = result.expect(
        "a real, already-applied fix must not be discarded as a Timeout just because a grandchild still holds stdout open after the agent's own exit",
    );
    assert!(
        elapsed < Duration::from_secs(4),
        "the residual drain must be bounded by its own short grace, independent of timeout_ms and of the 5s stdout holder; took {elapsed:?}"
    );
    assert!(
        attempt.diff_stat.contains("server.js"),
        "diff_stat must still reflect the real repair: {}",
        attempt.diff_stat
    );
    let changed = fs::read_to_string(worktree.path().join("server.js")).unwrap();
    assert!(
        changed.contains("body.promo && body.promo.code"),
        "the fix must be present on disk even though the pipe drain was cut short: {changed}"
    );
}

// -- a chatty agent must not deadlock on a full stdout pipe ------------------

/// CRITICAL (round-3 R3-F1): stdout/stderr must be drained CONCURRENTLY with
/// the agent's own run, not after `child.wait()` returns. A piped stdout with
/// no reader fills its ~64KiB kernel buffer and then blocks the writer in
/// `write(2)` forever, so the agent can never exit and `wait()` can never
/// return — the repair burns the whole `timeout_ms` and is then reported as a
/// `Timeout` that describes nothing that actually happened, with the fix lost.
/// This crate exists to drive `codex exec` (prints its whole run trace) and
/// `claude -p` (prints a full final message, far more under `--verbose`);
/// 64KiB is a low bar for either, so this is the normal case, not an edge one.
///
/// The pin is COMPLETION, not a wall-clock budget: `timeout_ms` is a generous
/// 30s that a healthy run (sub-second here) can never approach, so the only
/// way this test fails is a genuine deadlock. The outer bound is a safety net
/// so a regression fails the gate in seconds instead of hanging it.
#[tokio::test]
async fn agent_that_floods_stdout_past_the_pipe_buffer_still_completes() {
    let worktree = init_worktree_fixture();
    // 256 KiB — four times a typical pipe buffer, so the writer is guaranteed
    // to block unless something is reading concurrently.
    let agent = CliRepairAgent {
        command_template: format!("{} --emit-stdout-kb 256 {{prompt}}", fixture_path()),
        timeout_ms: 30_000,
    };
    let ctx = RepairContext {
        failure: sample_failure_with_body("{}"),
        attribution: sample_attribution(),
        acceptance: sample_acceptance(),
        remembered: Vec::new(),
    };

    let outer =
        tokio::time::timeout(Duration::from_secs(20), agent.repair(worktree.path(), &ctx)).await;

    let result = outer.expect(
        "repair() deadlocked on a full stdout pipe: nothing was draining stdout while the agent ran, so the agent blocked in write() and could never exit",
    );
    let attempt = result.expect("an agent that writes more than one pipe buffer must still complete, not be reported as a Timeout");
    assert!(
        attempt.diff_stat.contains("server.js"),
        "diff_stat must reflect the real repair: {}",
        attempt.diff_stat
    );
    let changed = fs::read_to_string(worktree.path().join("server.js")).unwrap();
    assert!(
        changed.contains("body.promo && body.promo.code"),
        "the chatty agent's fix must land in the worktree: {changed}"
    );
}

// -- the library owns no signal handlers -------------------------------------

/// CRITICAL (round-3 R3-F2): `tokio::signal::ctrl_c()` and
/// `signal(SignalKind::terminate())` install an OS handler process-wide,
/// once, permanently — tokio never de-registers it or restores the default
/// disposition. A LIBRARY that calls either one silently converts SIGINT and
/// SIGTERM from "terminate the process" into "wake up a listener that no
/// longer exists" for the rest of the process's life: after a single repair,
/// `drums watch` (foreground, ctrl-c documented as the stop) could be stopped
/// by neither ctrl-c nor `kill`, only `kill -9`.
///
/// Asserted the only way that is honest — by observation, in a real process:
/// re-execute THIS test binary, have the child run one real `repair()`, then
/// raise SIGINT at itself. The child must die from the signal. If it prints
/// `STILL ALIVE`, some library in its dependency graph took the process's
/// signal disposition, which is exactly the regression this pins closed.
#[cfg(unix)]
#[tokio::test]
async fn a_completed_repair_leaves_sigint_lethal_to_the_whole_process() {
    use std::os::unix::process::ExitStatusExt;

    let out = std::process::Command::new(std::env::current_exe().expect("test binary path"))
        .args([
            "signal_disposition_probe_child",
            "--exact",
            "--nocapture",
            "--test-threads",
            "1",
        ])
        .env("DRUMS_TEST_SIGNAL_CHILD", "1")
        .output()
        .expect("re-executing this test binary as a child must work");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stdout.contains("STILL ALIVE"),
        "SIGINT no longer terminates a process that has run one repair() — a signal handler installed inside the library has taken the process's signal disposition away from the binary that owns it\nchild stdout:\n{stdout}\nchild stderr:\n{stderr}"
    );
    assert_eq!(
        out.status.signal(),
        Some(libc::SIGINT),
        "the child must die FROM SIGINT (default disposition intact) after running a repair; exit code was {:?}\nchild stdout:\n{stdout}\nchild stderr:\n{stderr}",
        out.status.code()
    );
}

/// The child half of `a_completed_repair_leaves_sigint_lethal_to_the_whole_process`.
/// Inert unless `DRUMS_TEST_SIGNAL_CHILD=1`, which only that test sets, so a
/// normal `cargo test` run of this binary never raises a signal at itself.
#[cfg(unix)]
#[tokio::test]
async fn signal_disposition_probe_child() {
    if std::env::var("DRUMS_TEST_SIGNAL_CHILD").as_deref() != Ok("1") {
        return;
    }
    let worktree = init_worktree_fixture();
    let agent = CliRepairAgent {
        command_template: format!("{} {{prompt}}", fixture_path()),
        timeout_ms: 30_000,
    };
    let ctx = RepairContext {
        failure: sample_failure_with_body("{}"),
        attribution: sample_attribution(),
        acceptance: sample_acceptance(),
        remembered: Vec::new(),
    };
    agent
        .repair(worktree.path(), &ctx)
        .await
        .expect("the probe child's repair must succeed before it tests the signal");

    // Safety: raising a signal at our own process performs no memory access;
    // with the default disposition intact this call does not return.
    unsafe { libc::raise(libc::SIGINT) };
    tokio::time::sleep(Duration::from_millis(800)).await;
    println!("STILL ALIVE 800ms after SIGINT");
}

// -- diff_stat must include untracked (new-file-only) changes ----------------

/// IMPORTANT: `git diff --stat` alone never shows untracked files, so a
/// repair that only ADDS a new file (no tracked-file edits at all) must
/// still produce a non-empty, file-naming diff_stat.
#[tokio::test]
async fn new_file_only_repair_produces_a_non_empty_diff_stat() {
    let worktree = init_worktree_fixture();
    let agent = CliRepairAgent {
        command_template: format!("{} --create-file helper.js {{prompt}}", fixture_path()),
        timeout_ms: 30_000,
    };
    let ctx = RepairContext {
        failure: sample_failure_with_body("{}"),
        attribution: sample_attribution(),
        acceptance: sample_acceptance(),
        remembered: Vec::new(),
    };

    let attempt = agent
        .repair(worktree.path(), &ctx)
        .await
        .expect("fake agent must produce a repair attempt");
    assert!(
        !attempt.diff_stat.is_empty(),
        "diff_stat must not be empty for a new-file-only (untracked) repair"
    );
    assert!(
        attempt.diff_stat.contains("helper.js"),
        "diff_stat must name the new file: {}",
        attempt.diff_stat
    );
}

// -- minimal-env security coverage -------------------------------------------

/// IMPORTANT: the minimal-env security requirement (spec §19: blast radius
/// stays worktree + branch) had zero test coverage before this. Prove
/// `env_clear()` is actually in effect: an arbitrary secret-shaped var set
/// in THIS process, `DRUMS_INGEST_URL`, must never reach the agent child,
/// while the ANTHROPIC_*/CLAUDE_*/OPENAI_*/CODEX_* allowlist, PATH/HOME, and
/// (M-8: bisected empirically against a real `claude` CLI — see
/// `engine/crates/repair/src/lib.rs`'s comment on `USER`) USER still do.
#[tokio::test]
async fn agent_child_env_is_minimal_allowlist_only() {
    let prior_ingest = std::env::var("DRUMS_INGEST_URL").ok();
    let prior_leak = std::env::var("DRUMS_TEST_MUST_NOT_LEAK").ok();
    let prior_anthropic = std::env::var("ANTHROPIC_API_KEY").ok();
    let prior_user = std::env::var("USER").ok();

    std::env::set_var("DRUMS_INGEST_URL", "http://127.0.0.1:1/must-not-leak");
    std::env::set_var(
        "DRUMS_TEST_MUST_NOT_LEAK",
        "secret-value-outside-the-allowlist",
    );
    std::env::set_var("ANTHROPIC_API_KEY", "test-allowlisted-value");
    std::env::set_var("USER", "test-allowlisted-user");

    const ALLOWED_PREFIXES: &[&str] = &["ANTHROPIC_", "CLAUDE_", "OPENAI_", "CODEX_"];
    // Snapshotted NOW, at the exact moment the child is about to see this
    // process's env — not after the restore block below, and not as a
    // hardcoded literal list. This process may be running under a harness
    // that itself sets ambient CLAUDE_* vars (e.g. this suite running
    // inside a `claude` session), which the allowlist policy legitimately
    // forwards too; a hardcoded expected set would either miss those (false
    // failure) or have to special-case them (drift-prone). Computing it
    // from the real env at spawn time makes this assertion exact without
    // being fragile to whatever harness happens to be running the tests.
    let mut expected_keys: Vec<String> =
        vec!["HOME".to_string(), "PATH".to_string(), "USER".to_string()];
    for (k, _) in std::env::vars() {
        if ALLOWED_PREFIXES.iter().any(|p| k.starts_with(p)) {
            expected_keys.push(k);
        }
    }

    let worktree = init_worktree_fixture();
    let dump = worktree.path().join("env-dump.txt");
    let agent = CliRepairAgent {
        command_template: format!(
            "{} --dump-env-to {} {{prompt}}",
            fixture_path(),
            dump.display()
        ),
        timeout_ms: 30_000,
    };
    let ctx = RepairContext {
        failure: sample_failure_with_body("{}"),
        attribution: sample_attribution(),
        acceptance: sample_acceptance(),
        remembered: Vec::new(),
    };

    let run_result = agent.repair(worktree.path(), &ctx).await;

    // Restore process-global env before any assertion can early-return via panic.
    match prior_ingest {
        Some(v) => std::env::set_var("DRUMS_INGEST_URL", v),
        None => std::env::remove_var("DRUMS_INGEST_URL"),
    }
    match prior_leak {
        Some(v) => std::env::set_var("DRUMS_TEST_MUST_NOT_LEAK", v),
        None => std::env::remove_var("DRUMS_TEST_MUST_NOT_LEAK"),
    }
    match prior_anthropic {
        Some(v) => std::env::set_var("ANTHROPIC_API_KEY", v),
        None => std::env::remove_var("ANTHROPIC_API_KEY"),
    }
    match prior_user {
        Some(v) => std::env::set_var("USER", v),
        None => std::env::remove_var("USER"),
    }

    run_result.expect("fake agent should succeed");
    let dumped = fs::read_to_string(&dump).expect("fake agent must have dumped its environment");

    assert!(
        !dumped.contains("DRUMS_INGEST_URL"),
        "DRUMS_INGEST_URL must never reach the agent child:\n{dumped}"
    );
    assert!(
        !dumped.contains("DRUMS_TEST_MUST_NOT_LEAK"),
        "an unrelated var from the parent process must not leak to the child — env_clear() must still be in effect:\n{dumped}"
    );
    assert!(
        dumped.contains("ANTHROPIC_API_KEY=test-allowlisted-value"),
        "the ANTHROPIC_* allowlist must still pass through:\n{dumped}"
    );
    assert!(
        dumped.contains("PATH="),
        "PATH must be present so the CLI can resolve its own binaries:\n{dumped}"
    );
    assert!(
        dumped.contains("USER=test-allowlisted-user"),
        "USER must reach the agent child unchanged — a real `claude` CLI resolves its OS-keychain credential account from $USER (M-8), and without it a fully-authenticated parent shell still gets \"Not logged in\" in the child:\n{dumped}"
    );

    // The fake agent is itself a bash script; bash synthesizes PWD, SHLVL,
    // and `_` for its own bookkeeping regardless of the child's inherited
    // env, so these are not a leak from CliRepairAgent — they don't exist
    // in the parent process's env at all for `env_clear()` to have removed.
    const SHELL_SYNTHESIZED: &[&str] = &["PWD", "SHLVL", "_"];
    for line in dumped.lines() {
        let Some((key, _)) = line.split_once('=') else {
            continue;
        };
        let allowed = key == "PATH"
            || key == "HOME"
            || key == "USER"
            || SHELL_SYNTHESIZED.contains(&key)
            || ALLOWED_PREFIXES.iter().any(|p| key.starts_with(p));
        assert!(
            allowed,
            "unexpected env var leaked to the agent child: {key:?}\nfull dump:\n{dumped}"
        );
    }

    // FINAL exact allowlist (still an allowlist, not a relaxation of the
    // per-line check above): the child's key set must be exactly the
    // policy-computed `expected_keys` (snapshotted above, at spawn time)
    // plus bash's own synthesized bookkeeping vars — no more, no fewer.
    // This is the assertion that would catch a regression where the
    // per-prefix check above stays green (e.g. a var matching an allowed
    // prefix) but something ELSE also started leaking through, OR where an
    // expected var silently stopped arriving (e.g. `codex`/`claude` support
    // quietly regressed because the allowlist logic changed shape) — the
    // per-line loop above only ever catches the first kind.
    let mut got_keys: Vec<&str> = dumped
        .lines()
        .filter_map(|l| l.split_once('=').map(|(k, _)| k))
        .collect();
    got_keys.sort_unstable();
    got_keys.dedup();
    let mut expected_keys: Vec<&str> = expected_keys.iter().map(String::as_str).collect();
    expected_keys.extend_from_slice(SHELL_SYNTHESIZED);
    expected_keys.sort_unstable();
    expected_keys.dedup();
    assert_eq!(
        got_keys, expected_keys,
        "the agent child's FINAL env key set must be exactly this allowlist result (no unlisted var may leak, and no expected var may go missing):\nfull dump:\n{dumped}"
    );
}

// -- live smoke test (never runs in CI/gates) --------------------------------

/// Exercises a real `claude`/`codex` CLI on PATH. Ignored by default — run
/// explicitly with `DRUMS_LIVE_AGENT=1 cargo test -p engine-repair -- --ignored`
/// once live agent auth is available (documented for the morning keys).
#[tokio::test]
#[ignore]
async fn live_agent_smoke_test() {
    if std::env::var("DRUMS_LIVE_AGENT").as_deref() != Ok("1") {
        eprintln!("skipping: set DRUMS_LIVE_AGENT=1 to run this against a real claude/codex CLI");
        return;
    }
    let worktree = init_worktree_fixture();
    let agent = CliRepairAgent::detect()
        .expect("a real claude or codex CLI must be on PATH for this smoke test");
    let ctx = RepairContext {
        failure: sample_failure_with_body(r#"{"items":[{"price":10,"qty":2}]}"#),
        attribution: sample_attribution(),
        acceptance: sample_acceptance(),
        remembered: Vec::new(),
    };
    let attempt = agent
        .repair(worktree.path(), &ctx)
        .await
        .expect("live agent should produce a repair attempt");
    assert!(!attempt.diff_stat.is_empty());
}

/// Same shape as `live_agent_smoke_test`, but pins the CODEX path
/// specifically rather than whichever agent `detect()` happens to prefer —
/// `detect()` picks `claude` first when both are on `PATH`
/// (`live_agent_smoke_test` already exercises that branch on a machine with
/// both installed), so proving codex actually drives a repair needs its own
/// test. Builds the exact template `detect()` would produce for codex
/// (`CODEX_DEFAULT_TEMPLATE`, via `on_path("codex")` in
/// `CliRepairAgent::detect()`) rather than duplicating the literal string,
/// so this can never silently drift from what production actually runs.
/// Together with `live_agent_smoke_test`, `DRUMS_LIVE_AGENT=1 cargo test -p
/// engine-repair -- --ignored` exercises both agents end-to-end.
#[tokio::test]
#[ignore]
async fn live_agent_smoke_test_codex() {
    if std::env::var("DRUMS_LIVE_AGENT").as_deref() != Ok("1") {
        eprintln!("skipping: set DRUMS_LIVE_AGENT=1 to run this against a real codex CLI");
        return;
    }
    let worktree = init_worktree_fixture();
    let agent = CliRepairAgent {
        command_template: CODEX_DEFAULT_TEMPLATE.to_string(),
        timeout_ms: DEFAULT_TIMEOUT_MS,
    };
    let ctx = RepairContext {
        failure: sample_failure_with_body(r#"{"items":[{"price":10,"qty":2}]}"#),
        attribution: sample_attribution(),
        acceptance: sample_acceptance(),
        remembered: Vec::new(),
    };
    let attempt = agent
        .repair(worktree.path(), &ctx)
        .await
        .expect("live codex agent should produce a repair attempt");
    assert!(!attempt.diff_stat.is_empty());
    assert!(
        attempt.diff_stat.contains("server.js"),
        "codex's fix must land in server.js: {}",
        attempt.diff_stat
    );
}
