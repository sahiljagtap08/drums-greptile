//! `drums sync` — the consent gate, held through the real binary.
//!
//! Record sync is opt-in twice over: `sync_record = true` in the repo's
//! config AND a `drums login` credential. Each refusal must name the exact
//! half that is missing — the flag-off refusal names `sync_record`, the
//! token-less refusal names `drums login` — because a refusal that does not
//! name its fix is where a new user stops.
//!
//! Both tests run the real binary in a child process so `DRUMS_HOME` (where
//! `login::credentials_path` looks first) can be pointed at an empty
//! directory without mutating this process's environment — the same
//! discipline `tests/login.rs` documents.

use std::process::Command;

fn drums() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_drums"));
    // An empty, throwaway home: whatever machine runs this test, its real
    // credentials must be neither read nor needed.
    cmd.env("DRUMS_HOME", std::env::temp_dir());
    cmd
}

/// Flag off (the default) means `drums sync` is a refusal, not a quiet
/// no-op: an explicit command that silently moved nothing would teach the
/// operator the feature is broken, and the refusal names the one key that
/// turns it on.
#[test]
fn sync_with_the_flag_off_refuses_naming_sync_record() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".drums")).unwrap();
    // A config that exists but never opted in.
    std::fs::write(dir.path().join(".drums/config.toml"), "threshold = 3\n").unwrap();

    let out = drums()
        .args(["sync", "--repo"])
        .arg(dir.path())
        .output()
        .expect("must run");

    assert!(!out.status.success(), "flag off must exit nonzero");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("sync_record"),
        "the refusal must name the config key: {err}"
    );
    assert!(
        err.contains("leaves this machine only when you set it"),
        "the refusal states the trust term, so opting in is informed: {err}"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("synced"),
        "a refused sync must not also claim success: {stdout}"
    );
}

/// The flag alone is not enough: without a credential nothing can be pushed
/// anywhere attributable, and the refusal names the command that mints one.
#[test]
fn sync_with_the_flag_on_but_no_token_refuses_naming_drums_login() {
    let dir = tempfile::tempdir().unwrap();
    let empty_home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".drums")).unwrap();
    std::fs::write(
        dir.path().join(".drums/config.toml"),
        "sync_record = true\n",
    )
    .unwrap();

    let out = drums()
        .env("DRUMS_HOME", empty_home.path())
        .args(["sync", "--repo"])
        .arg(dir.path())
        .output()
        .expect("must run");

    assert!(!out.status.success(), "no credential must exit nonzero");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("drums login"),
        "the refusal must name the command that fixes it: {err}"
    );
}
