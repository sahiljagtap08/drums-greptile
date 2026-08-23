//! Round-3 R1 (CONFIRMED live by the reviewer): `run_deploy_cmd` piped
//! stdout/stderr but left the deploy child's **stdin INHERITED**, so any
//! deploy command that reads stdin (`ssh host 'bash -s'`, `kubectl apply -f
//! -`, `docker login --password-stdin`, any passphrase / host-key / "are you
//! sure" prompt) blocked on input that never arrives: `drums ship` printed
//! nothing at all for the full 600s `DEPLOY_TIMEOUT` and then reported
//! `deploy command timed out after 10 minutes` for a deploy that had never
//! started doing anything. Worse, `process_group(0)` (added by the C2 fix)
//! means a child reading the *controlling terminal* gets `SIGTTIN` and is
//! STOPPED, so the operator never even sees the prompt that would explain
//! the stall.
//!
//! This is the deterministic pin, and it deliberately drives the REAL `drums`
//! binary rather than `ship()` in-process: the defect is about what the child
//! INHERITS, so the parent's own stdin has to be controlled, and inside
//! `cargo test` that is whatever the harness was given (commonly already
//! `/dev/null`, which would make an in-process assertion silently vacuous).
//! Here the parent is spawned with `stdin(Stdio::piped())` and the write end
//! is held open for the whole test — exactly what a terminal looks like to a
//! reader: never any data, never EOF.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Well under `ship.rs`'s 600s `DEPLOY_TIMEOUT`, and well over the ~0s a
/// `read`-and-exit script needs once its stdin is `/dev/null`.
const BOUND: Duration = Duration::from_secs(30);

fn write_script(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).unwrap();
    }
}

/// A `repair_ready` line for `f1`, hand-written so this test needs no git
/// repo and no `engine-record` dependency — `drums ship`'s happy path reads
/// only the record.
fn seed_repair_ready(repo: &Path) {
    let drums = repo.join(".drums");
    std::fs::create_dir_all(&drums).unwrap();
    let line = r#"{"kind":"repair_ready","recorded_at_ms":1,"id":"r1","failure_id":"f1","sha":"deadbeefdeadbeefdeadbeefdeadbeefdeadbeef","branch":"drums/repair-f1","agent":"fake","summary":"fixed it","diff_stat":"server.js | 1 +","claims":[]}
"#;
    std::fs::write(drums.join("record.jsonl"), line).unwrap();
}

#[test]
#[cfg(unix)]
fn ship_does_not_hang_when_the_deploy_command_reads_stdin() {
    let repo_dir = tempfile::tempdir().unwrap();
    let repo = repo_dir.path();
    seed_repair_ready(repo);

    let script = repo.join("stdin-reading-deploy.sh");
    // The shape of every ordinary stdin-consuming deploy idiom.
    write_script(&script, "#!/bin/sh\nread -r line\necho \"got: $line\"\n");

    let mut child = Command::new(env!("CARGO_BIN_EXE_drums"))
        .arg("ship")
        .arg("f1")
        .arg("--deploy-cmd")
        .arg(script.display().to_string())
        .arg("--repo")
        .arg(repo)
        // A pipe whose write end this test keeps open: no data, no EOF —
        // what an interactive terminal looks like to a reader.
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn drums ship");
    // Held (never dropped, never written to) until the assertions are done,
    // so the child can never see EOF by way of this test closing the pipe.
    let stdin = child.stdin.take().expect("stdin piped");

    let started = Instant::now();
    let status = loop {
        match child.try_wait().expect("try_wait") {
            Some(status) => break Some(status),
            None if started.elapsed() >= BOUND => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    };
    drop(stdin);

    let status = status.unwrap_or_else(|| {
        panic!(
            "`drums ship` must not block on a deploy command that reads stdin — it was still running after {}s. \
             The deploy child's stdin must be /dev/null (a `--deploy-cmd` is non-interactive by contract); \
             with stdin inherited it cannot return before the 600s DEPLOY_TIMEOUT, printing nothing.",
            BOUND.as_secs()
        )
    });
    assert!(
        status.success(),
        "the deploy script exits 0 once its `read` sees EOF, so ship must succeed (exit status: {status:?}); \
         a timeout/failure here means stdin was not /dev/null"
    );

    let record = std::fs::read_to_string(repo.join(".drums").join("record.jsonl")).unwrap();
    assert!(
        record.contains("\"kind\":\"shipped\""),
        "the ship really completed and was recorded: {record}"
    );
}

/// The same contract stated as a positive: the deploy command sees an
/// IMMEDIATE EOF on stdin (`/dev/null`), not a live terminal — so a script
/// that reads stdin takes the "no input" branch instead of waiting.
#[test]
#[cfg(unix)]
fn the_deploy_command_sees_an_immediately_closed_stdin() {
    let repo_dir = tempfile::tempdir().unwrap();
    let repo = repo_dir.path();
    seed_repair_ready(repo);

    let observed = repo.join("stdin-observed.txt");
    let script = repo.join("stdin-observing-deploy.sh");
    write_script(
        &script,
        &format!("#!/bin/sh\nif read -r line; then printf 'READ:%s\\n' \"$line\" > \"{p}\"; else printf 'EOF\\n' > \"{p}\"; fi\n", p = observed.display()),
    );

    let mut child = Command::new(env!("CARGO_BIN_EXE_drums"))
        .arg("ship")
        .arg("f1")
        .arg("--deploy-cmd")
        .arg(script.display().to_string())
        .arg("--repo")
        .arg(repo)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn drums ship");
    let stdin = child.stdin.take().expect("stdin piped");
    // Data that must NEVER reach the deploy child: if stdin were inherited,
    // this line would be consumed by the deploy script instead.
    {
        let mut w = stdin;
        let _ = w.write_all(b"leaked-from-the-parents-stdin\n");
        let _ = w.flush();
        // `w` is dropped here, but the child's own stdin is /dev/null either
        // way once the fix is in place; before the fix the child would have
        // read this line and this test would see `READ:` instead of `EOF`.
    }

    let started = Instant::now();
    loop {
        match child.try_wait().expect("try_wait") {
            Some(status) => {
                assert!(status.success(), "ship must succeed: {status:?}");
                break;
            }
            None if started.elapsed() >= BOUND => {
                let _ = child.kill();
                let _ = child.wait();
                panic!(
                    "`drums ship` was still running after {}s with a stdin-reading deploy command",
                    BOUND.as_secs()
                );
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }

    let seen = std::fs::read_to_string(&observed).unwrap_or_default();
    assert_eq!(
        seen.trim(),
        "EOF",
        "the deploy command's stdin must be /dev/null — it must never inherit (and consume) the drums process's own stdin: {seen:?}"
    );
}
