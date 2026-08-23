//! Which commands need an account, driven through the real binary.
//!
//! `crates/cli/src/account.rs` unit-tests the decision. This tests the WIRING:
//! that the gate is actually in front of `drums watch` and actually is not in
//! front of `drums init`. A gate wired to the wrong command, or to every
//! command, would pass every test in that module.
//!
//! `DRUMS_HOME` is set per-child with `Command::env` rather than
//! `std::env::set_var`, so these two can run in parallel with each other and
//! with `tests/login.rs` — which does mutate this process's own environment.

use std::path::Path;
use std::process::Command;

/// A home directory with no credentials in it: a machine that has never run
/// `drums login`.
fn signed_out_home() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

fn git_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    assert!(Command::new("git")
        .arg("-C")
        .arg(dir.path())
        .args(["init", "-q"])
        .status()
        .expect("git must be available")
        .success());
    dir
}

fn drums(home: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_drums"));
    cmd.env("DRUMS_HOME", home);
    cmd
}

/// The end of `curl drums.sh/install | sh`: the installer says `next: drums
/// watch`, and this is what that now hits. It has to refuse fast — before any
/// listener, state directory or install id exists — and it has to say the one
/// thing that fixes it.
#[test]
fn watch_refuses_without_an_account_and_says_exactly_what_to_type() {
    let home = signed_out_home();
    let repo = tempfile::tempdir().unwrap();

    let out = drums(home.path())
        .args(["watch", "--ingest-port", "0", "--repo"])
        .arg(repo.path())
        .output()
        .expect("the drums binary must run");

    assert!(!out.status.success(), "an anonymous watch must not start");
    assert_ne!(
        out.status.code(),
        Some(2),
        "not being signed in is not a usage error — clap must not be the thing refusing this"
    );

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("drums login"), "{stderr}");
    assert!(
        stderr.contains("DRUMS_NO_ACCOUNT"),
        "the escape hatch has to be discoverable at the moment somebody is blocked: {stderr}"
    );
    assert!(
        !stderr.contains("panicked") && !stderr.contains("RUST_BACKTRACE"),
        "a refusal is a sentence, not a stack trace: {stderr}"
    );

    // Nothing was started and nothing was left behind.
    assert!(
        !repo.path().join(".drums").exists(),
        "a refused start must not have created state in the repo"
    );
}

/// The other half, and the one that is easy to break by accident: the
/// single-shot command somebody runs to see Drums work stays open. If this
/// ever fails, the landing page is promising something the CLI no longer does.
#[test]
fn init_still_runs_with_no_account_and_invites_one_afterwards() {
    let home = signed_out_home();
    let repo = git_repo();

    let out = drums(home.path())
        .args(["init", "--yes", "--repo"])
        .arg(repo.path())
        .output()
        .expect("the drums binary must run");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "`drums init` must work with no account:\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        !stdout.contains("needs an account") && !stderr.contains("needs an account"),
        "nothing in `drums init` may demand a sign-in:\n{stdout}"
    );
    assert!(
        stdout.contains("drums login"),
        "an anonymous run should invite a sign-in once it has earned the right to ask:\n{stdout}"
    );
    assert!(
        repo.path().join(".drums").join("config.toml").exists(),
        "the command still did its actual job"
    );
}
