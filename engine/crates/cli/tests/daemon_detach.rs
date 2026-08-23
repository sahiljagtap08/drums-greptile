//! Task requirement 2: "runs detached, with no terminal … survives the
//! terminal closing." This proves the actual mechanism that makes that true
//! — `spawn_detached` calling `setsid()` before `drumsd` execs — rather than
//! literally closing a pty (this workspace has no pty dependency to drive
//! one from a test):
//!
//! 1. `drumsd` outlives the very process that spawned it (`drums daemon
//!    start`, whose own `Command` handle this test `.wait()`s to
//!    completion) — the standard orphan-reparented-to-init shape a
//!    single-fork daemonize produces.
//! 2. `drumsd` is the LEADER OF ITS OWN SESSION (`getsid(pid) == pid`). By
//!    POSIX definition a session only has a controlling terminal once its
//!    leader opens one — `drumsd` never does — so a session leader with no
//!    controlling terminal can never be a target of the SIGHUP a terminal
//!    hangup delivers to the processes attached to it. This is the literal
//!    property "survives the terminal closing" rests on; asserting it
//!    directly is strictly stronger than script-killing one particular
//!    terminal emulator would prove.
//! 3. Its ingest port is still accepting connections after all of the
//!    above — "still alive and still serving its ingest port", the task's
//!    own words.

#![cfg(unix)]

use std::io::Read;
use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

fn getsid(pid: u32) -> i32 {
    // Exercises the real `drums_watch::daemon::session_id` rather than a
    // parallel local reimplementation.
    drums_watch::daemon::session_id(pid).expect("getsid must succeed for a live pid")
}

fn is_alive(pid: u32) -> bool {
    drums_watch::daemon::is_process_alive(pid)
}

#[test]
fn drumsd_outlives_its_own_parent_runs_in_its_own_session_and_keeps_serving() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let my_sid = getsid(std::process::id());

    let mut start = Command::new(env!("CARGO_BIN_EXE_drums"))
        .args(["daemon", "start", "--repo"])
        .arg(dir.path())
        .args([
            "--ingest-port",
            &port.to_string(),
            "--threshold",
            "3",
            "--window-secs",
            "60",
        ])
        .env("DRUMS_DAEMON_BIN", env!("CARGO_BIN_EXE_drumsd"))
        .stdout(Stdio::piped())
        .spawn()
        .expect("`drums daemon start` must spawn");

    let mut out = String::new();
    start
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .unwrap();

    // `drums daemon start` (this test's direct child) has now fully exited
    // — its own process is gone, reaped by the `.wait()` below. Whatever
    // `drumsd` it spawned is, from this point on, an orphan: nothing in
    // this test's own process tree is its parent anymore.
    let status = start.wait().expect("drums daemon start must exit");
    assert!(status.success(), "drums daemon start must exit 0: {out}");

    let pidfile =
        std::fs::read_to_string(dir.path().join(".drums/drumsd.pid")).expect("pidfile must exist");
    let record: serde_json::Value = serde_json::from_str(&pidfile).unwrap();
    let daemon_pid = record["pid"].as_u64().unwrap() as u32;

    // 1. Outlives its own parent.
    assert!(
        is_alive(daemon_pid),
        "drumsd must still be alive after `drums daemon start` (its own parent) has fully exited"
    );

    // 2. Runs in a session of its own — the actual "no controlling
    // terminal, so no SIGHUP on hangup" property.
    let daemon_sid = getsid(daemon_pid);
    assert_eq!(daemon_sid, daemon_pid as i32, "drumsd must be the leader of its own session (setsid at spawn) — otherwise it is still a member of whatever session could receive a terminal hangup");
    assert_ne!(
        daemon_sid, my_sid,
        "drumsd's session must differ from this test process's own session"
    );

    // 3. Still serving its ingest port. `drumsd` binds asynchronously after
    // exec, so this polls briefly rather than asserting on the very first
    // attempt.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut connected = false;
    while Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            connected = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(connected, "drumsd's ingest port must still accept connections after its parent shell has exited and it has been proven to run in its own session");

    // Cleanup.
    let stop = Command::new(env!("CARGO_BIN_EXE_drums"))
        .args(["daemon", "stop", "--repo"])
        .arg(dir.path())
        .output()
        .expect("stop must run");
    assert!(
        stop.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&stop.stderr)
    );
}
