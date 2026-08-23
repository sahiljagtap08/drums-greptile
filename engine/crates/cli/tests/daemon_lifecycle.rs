//! `drums daemon {start,status,stop}` against the REAL built binaries
//! (`drums` spawning the real `drumsd`) — the end-to-end version of the
//! pidfile/status/stop plumbing `daemon.rs`'s unit tests exercise as pure
//! functions.

#![cfg(unix)]

use std::process::Command;
use std::time::{Duration, Instant};

fn drums_daemon(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_drums"))
        .args(["daemon"])
        .args(args)
        // Points `drums daemon start` at the exact `drumsd` this test run
        // just built, rather than relying on sibling-path lookup off
        // `current_exe()` (which also happens to work under `cargo test`,
        // since both `[[bin]]`s land in the same `target/<profile>/`
        // directory — this override just removes any doubt).
        .env("DRUMS_DAEMON_BIN", env!("CARGO_BIN_EXE_drumsd"))
        .output()
        .expect("drums binary must run")
}

fn read_pidfile(repo: &std::path::Path) -> serde_json::Value {
    let content = std::fs::read_to_string(repo.join(".drums/drumsd.pid"))
        .expect("pidfile must exist after a successful start");
    serde_json::from_str(&content).expect("pidfile must be valid JSON")
}

fn wait_until(mut cond: impl FnMut() -> bool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    cond()
}

#[test]
fn start_reports_running_and_stop_reports_not_running() {
    let dir = tempfile::tempdir().unwrap();

    let start = drums_daemon(&[
        "start",
        "--repo",
        &dir.path().display().to_string(),
        "--ingest-port",
        "0",
        "--threshold",
        "3",
        "--window-secs",
        "60",
    ]);
    let start_stdout = String::from_utf8_lossy(&start.stdout).into_owned();
    assert!(
        start.status.success(),
        "daemon start must exit 0: stdout={start_stdout} stderr={}",
        String::from_utf8_lossy(&start.stderr)
    );
    assert!(start_stdout.contains("drumsd started"), "{start_stdout}");
    assert!(start_stdout.contains("pidfile:"), "{start_stdout}");
    assert!(start_stdout.contains("log:"), "{start_stdout}");

    let record = read_pidfile(dir.path());
    let pid = record["pid"].as_u64().expect("pidfile must carry a pid");

    // `drums daemon start` must have returned immediately: the daemon keeps
    // running after the spawning command has already exited, which is
    // exactly what just happened above (`start.status` is already known).
    assert!(
        wait_until(
            || unsafe { libc::kill(pid as libc::pid_t, 0) } == 0,
            Duration::from_secs(3)
        ),
        "the pid recorded in the pidfile must actually be alive"
    );

    let status = drums_daemon(&["status", "--repo", &dir.path().display().to_string()]);
    let status_out = String::from_utf8_lossy(&status.stdout).into_owned();
    assert!(status.status.success());
    assert!(status_out.contains("running"), "{status_out}");
    assert!(status_out.contains(&pid.to_string()), "{status_out}");
    assert!(!status_out.contains("not running"), "{status_out}");

    assert!(
        dir.path().join(".drums/drumsd.log").exists(),
        "the logfile must exist once the daemon has started"
    );

    let stop = drums_daemon(&["stop", "--repo", &dir.path().display().to_string()]);
    let stop_out = String::from_utf8_lossy(&stop.stdout).into_owned();
    assert!(
        stop.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&stop.stderr)
    );
    assert!(stop_out.contains("stopped"), "{stop_out}");

    assert!(
        wait_until(
            || unsafe { libc::kill(pid as libc::pid_t, 0) } != 0,
            Duration::from_secs(3)
        ),
        "the process must actually be gone after `drums daemon stop`"
    );

    let status_after = drums_daemon(&["status", "--repo", &dir.path().display().to_string()]);
    let status_after_out = String::from_utf8_lossy(&status_after.stdout).into_owned();
    assert!(
        status_after_out.contains("not running"),
        "{status_after_out}"
    );
    assert!(
        !dir.path().join(".drums/drumsd.pid").exists(),
        "`drums daemon stop` must clean up the pidfile it no longer names a live process"
    );
}

#[test]
fn status_on_a_repo_that_was_never_started_is_honest_and_starting_twice_is_a_harmless_no_op() {
    let dir = tempfile::tempdir().unwrap();

    let status = drums_daemon(&["status", "--repo", &dir.path().display().to_string()]);
    let out = String::from_utf8_lossy(&status.stdout).into_owned();
    assert!(
        !status.status.success(),
        "`daemon status` with nothing running must exit nonzero so scripts can gate on it: {out}"
    );
    assert!(out.contains("not running"), "{out}");

    let first = drums_daemon(&[
        "start",
        "--repo",
        &dir.path().display().to_string(),
        "--ingest-port",
        "0",
        "--threshold",
        "3",
        "--window-secs",
        "60",
    ]);
    assert!(first.status.success());
    let first_pid = read_pidfile(dir.path())["pid"].as_u64().unwrap();

    // A second `start` against the same, still-running repo must be a
    // no-op — never a second drumsd spawned on top of a live one.
    let second = drums_daemon(&[
        "start",
        "--repo",
        &dir.path().display().to_string(),
        "--ingest-port",
        "0",
        "--threshold",
        "3",
        "--window-secs",
        "60",
    ]);
    let second_out = String::from_utf8_lossy(&second.stdout).into_owned();
    assert!(second.status.success());
    assert!(second_out.contains("already running"), "{second_out}");
    let second_pid = read_pidfile(dir.path())["pid"].as_u64().unwrap();
    assert_eq!(
        first_pid, second_pid,
        "the pidfile must still name the ORIGINAL daemon, not a freshly spawned second one"
    );

    let stop = drums_daemon(&["stop", "--repo", &dir.path().display().to_string()]);
    assert!(stop.status.success());
}

#[test]
fn start_without_config_or_flags_is_an_honest_refusal_naming_drums_init() {
    let dir = tempfile::tempdir().unwrap();
    let start = Command::new(env!("CARGO_BIN_EXE_drums"))
        .args(["daemon", "start", "--repo"])
        .arg(dir.path())
        .output()
        .expect("must run");
    assert!(
        !start.status.success(),
        "no config and no flags must refuse, not silently start with hardcoded defaults"
    );
    let err = String::from_utf8_lossy(&start.stderr).into_owned();
    assert!(err.contains("config.toml"), "{err}");
    assert!(
        err.contains("drums init"),
        "the refusal must name the one command that fixes it: {err}"
    );
    assert!(
        !dir.path().join(".drums/drumsd.pid").exists(),
        "nothing must have been spawned"
    );
}

#[test]
fn init_writes_a_config_that_daemon_start_can_then_use_with_no_flags() {
    let dir = tempfile::tempdir().unwrap();
    // `drums init` requires a git repository, because reproduction rebuilds
    // revisions in worktrees. Refusing early with a clear message beats
    // succeeding here and failing at the first repair.
    Command::new("git")
        .arg("-C")
        .arg(dir.path())
        .arg("init")
        .arg("-q")
        .status()
        .expect("git init");
    let init = Command::new(env!("CARGO_BIN_EXE_drums"))
        .args(["init", "--yes", "--repo"])
        .arg(dir.path())
        .output()
        .expect("must run");
    assert!(
        init.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&init.stderr)
    );
    assert!(dir.path().join(".drums/config.toml").exists());

    let start = Command::new(env!("CARGO_BIN_EXE_drums"))
        .args(["daemon", "start", "--repo"])
        .arg(dir.path())
        .env("DRUMS_DAEMON_BIN", env!("CARGO_BIN_EXE_drumsd"))
        .output()
        .expect("must run");
    assert!(
        start.status.success(),
        "a repo with a config and no flags must just work: stderr={}",
        String::from_utf8_lossy(&start.stderr)
    );

    let _ = drums_daemon(&["stop", "--repo", &dir.path().display().to_string()]);
}

/// A stale pidfile (recorded pid no longer alive) must be reported honestly
/// by `status`, and `start` must treat it as "not running" and start fresh
/// rather than refusing because *a* pidfile exists.
#[test]
fn a_stale_pidfile_is_reported_honestly_and_does_not_block_a_fresh_start() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".drums")).unwrap();
    // A pid guaranteed dead: a real child, spawned and waited out.
    let mut dead = std::process::Command::new("true").spawn().unwrap();
    let dead_pid = dead.id();
    dead.wait().unwrap();
    let stale = serde_json::json!({
        "pid": dead_pid,
        "repo": dir.path(),
        "ingest_port": 7787,
        "started_at_ms": 0,
        "log_path": dir.path().join(".drums/drumsd.log"),
    });
    std::fs::write(
        dir.path().join(".drums/drumsd.pid"),
        serde_json::to_string(&stale).unwrap(),
    )
    .unwrap();

    let status = drums_daemon(&["status", "--repo", &dir.path().display().to_string()]);
    let out = String::from_utf8_lossy(&status.stdout).into_owned();
    assert!(
        out.contains("not running"),
        "a stale pidfile must never be reported as running: {out}"
    );
    assert!(
        out.contains(&dead_pid.to_string()),
        "the stale pid should still be named for diagnosis: {out}"
    );

    let start = drums_daemon(&[
        "start",
        "--repo",
        &dir.path().display().to_string(),
        "--ingest-port",
        "0",
        "--threshold",
        "3",
        "--window-secs",
        "60",
    ]);
    assert!(
        start.status.success(),
        "a stale pidfile must not block a fresh start: stderr={}",
        String::from_utf8_lossy(&start.stderr)
    );
    let new_pid = read_pidfile(dir.path())["pid"].as_u64().unwrap();
    assert_ne!(new_pid, dead_pid as u64);

    let _ = drums_daemon(&["stop", "--repo", &dir.path().display().to_string()]);
}

#[test]
fn logs_prints_the_narration_the_engine_wrote() {
    let dir = tempfile::tempdir().unwrap();
    let start = drums_daemon(&[
        "start",
        "--repo",
        &dir.path().display().to_string(),
        "--ingest-port",
        "0",
        "--threshold",
        "3",
        "--window-secs",
        "60",
    ]);
    assert!(start.status.success());

    // Give the daemon a moment to install its tracing subscriber and log
    // its own startup line.
    assert!(
        wait_until(
            || std::fs::metadata(dir.path().join(".drums/drumsd.log"))
                .map(|m| m.len() > 0)
                .unwrap_or(false),
            Duration::from_secs(3)
        ),
        "the daemon must have written at least its startup line to the logfile"
    );

    let logs = drums_daemon(&["logs", "--repo", &dir.path().display().to_string()]);
    let out = String::from_utf8_lossy(&logs.stdout).into_owned();
    assert!(logs.status.success());
    assert!(out.contains("drumsd starting"), "{out}");
    assert!(
        !out.contains("\x1b["),
        "the logfile content must carry no ANSI escapes: {out:?}"
    );

    let _ = drums_daemon(&["stop", "--repo", &dir.path().display().to_string()]);
}
