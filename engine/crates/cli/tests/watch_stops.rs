//! `drums watch` is a foreground command whose documented stop is ctrl-c.
//! This spawns the real built binary, sends it a real SIGINT, and asserts it
//! actually stops — the end-to-end version of the property that forced every
//! signal handler out of the engine libraries and into this binary.

#![cfg(unix)]

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn watch_stops_on_ctrl_c_and_says_what_stopping_did() {
    let dir = tempfile::tempdir().unwrap();
    // Port 0 = an ephemeral port, so this test can never collide with
    // another run (or with a developer's own `drums watch`) on a fixed one.
    let mut watch = Command::new(env!("CARGO_BIN_EXE_drums"))
        .args(["watch", "--ingest-port", "0", "--repo"])
        .arg(dir.path())
        // This test is about the signal disposition, not the account gate
        // (`crates/cli/src/account.rs`), and it runs where nobody can sign in.
        .env("DRUMS_NO_ACCOUNT", "1")
        .stdout(Stdio::piped())
        .spawn()
        .expect("drums binary must spawn");

    // Collect the whole run's stdout on a thread; it arrives at EOF, i.e.
    // once the child has exited and closed the pipe.
    let mut stdout = watch.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stdout.read_to_string(&mut s);
        let _ = tx.send(s);
    });

    // Let it get past binding the ingest listener and printing its banner.
    std::thread::sleep(Duration::from_secs(1));

    let pid = watch.id();
    let killed = Command::new("kill")
        .arg("-INT")
        .arg(pid.to_string())
        .status()
        .expect("kill must run");
    assert!(
        killed.success(),
        "failed to send SIGINT to the watch process"
    );

    let deadline = Instant::now() + Duration::from_secs(3);
    let mut exited = None;
    while Instant::now() < deadline {
        if let Some(status) = watch.try_wait().expect("try_wait") {
            exited = Some(status);
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let Some(status) = exited else {
        let _ = watch.kill();
        let _ = watch.wait();
        panic!(
            "`drums watch` was still running 3s after SIGINT — ctrl-c is its documented stop, so nothing in the process (least of all a library) may hold on to the signal disposition"
        );
    };
    assert!(
        status.success(),
        "ctrl-c is a documented, expected stop: it must exit cleanly, got {status:?}"
    );

    let out = rx.recv_timeout(Duration::from_secs(3)).unwrap_or_default();
    assert!(
        out.contains("watching"),
        "watch should have printed its banner before being stopped: {out}"
    );
    assert!(
        out.contains("stopped —"),
        "stopping must narrate what it did to in-flight work rather than vanishing silently: {out}"
    );
}

/// SIGTERM (`kill <pid>`, no flag — the signal `demo/run-demo.sh`'s
/// `trap ... EXIT` sends, and Task 3's carried Task-2 gate finding: once a
/// repair agent process/worktree can be in flight, SIGTERM must stop the
/// watch, kill the agent's process group, and clean up worktrees the same
/// way ctrl-c does — not fall through to the default (untrapped, no
/// narration) lethal disposition.
#[test]
fn watch_stops_on_sigterm_and_says_what_stopping_did() {
    let dir = tempfile::tempdir().unwrap();
    let mut watch = Command::new(env!("CARGO_BIN_EXE_drums"))
        .args(["watch", "--ingest-port", "0", "--repo"])
        .arg(dir.path())
        .env("DRUMS_NO_ACCOUNT", "1")
        .stdout(Stdio::piped())
        .spawn()
        .expect("drums binary must spawn");

    let mut stdout = watch.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stdout.read_to_string(&mut s);
        let _ = tx.send(s);
    });

    std::thread::sleep(Duration::from_secs(1));

    let pid = watch.id();
    // Plain `kill` (no `-INT`/`-9`) sends SIGTERM by default.
    let killed = Command::new("kill")
        .arg(pid.to_string())
        .status()
        .expect("kill must run");
    assert!(
        killed.success(),
        "failed to send SIGTERM to the watch process"
    );

    let deadline = Instant::now() + Duration::from_secs(3);
    let mut exited = None;
    while Instant::now() < deadline {
        if let Some(status) = watch.try_wait().expect("try_wait") {
            exited = Some(status);
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let Some(status) = exited else {
        let _ = watch.kill();
        let _ = watch.wait();
        panic!("`drums watch` was still running 3s after SIGTERM — the demo's `trap ... EXIT` sends SIGTERM, so it must stop the watch the same way ctrl-c does");
    };
    assert!(
        status.success(),
        "SIGTERM must exit cleanly through the same graceful path as ctrl-c, got {status:?}"
    );

    let out = rx.recv_timeout(Duration::from_secs(3)).unwrap_or_default();
    assert!(
        out.contains("watching"),
        "watch should have printed its banner before being stopped: {out}"
    );
    assert!(
        out.contains("stopped —"),
        "SIGTERM must narrate what it did to in-flight work rather than vanishing silently: {out}"
    );
}
