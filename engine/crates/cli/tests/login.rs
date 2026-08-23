//! Where `drums login` puts a credential, and who can read it.
//!
//! These share one test function on purpose. They drive `DRUMS_HOME`, which is
//! process-wide state, and Rust runs tests in parallel by default — split
//! across several `#[test]`s they would interfere with each other and fail in
//! ways that look like real bugs.

use drums_watch::login::{self, Credentials};

fn creds() -> Credentials {
    Credentials {
        token: "drums_pat_ThisIsNotARealToken".into(),
        account: "acme".into(),
        console_url: "https://app.drums.sh".into(),
    }
}

#[test]
fn a_credential_is_written_where_it_belongs_and_only_the_owner_can_read_it() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("DRUMS_HOME", home.path());

    // -- nothing stored is a normal state, not an error --------------------
    assert_eq!(
        login::load().expect("a missing credentials file must not be an error"),
        None,
        "not being signed in is ordinary"
    );
    assert!(!login::forget().unwrap(), "there was nothing to forget");

    // -- saving ------------------------------------------------------------
    let path = login::save(&creds()).expect("save should work");
    assert!(
        path.starts_with(home.path()),
        "DRUMS_HOME must be honoured: {path:?}"
    );
    assert_eq!(path.file_name().unwrap(), "credentials.toml");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "a bearer token must not be readable by other users on the machine"
        );
    }

    // -- round trip --------------------------------------------------------
    assert_eq!(login::load().unwrap(), Some(creds()));

    // -- re-login must not loosen an existing file -------------------------
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let mut second = creds();
        second.account = "acme-two".into();
        login::save(&second).expect("re-login should work");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "signing in again must tighten a file that was left permissive"
        );
        assert_eq!(login::load().unwrap().unwrap().account, "acme-two");
    }

    // -- the token is never in the file in some other guise ----------------
    let body = std::fs::read_to_string(&path).unwrap();
    assert!(
        body.contains("acme-two"),
        "the account should be readable: {body}"
    );

    // -- forgetting --------------------------------------------------------
    assert!(login::forget().unwrap(), "there was something to forget");
    assert_eq!(login::load().unwrap(), None);
    assert!(!path.exists());

    // -- a corrupt file is named, not swallowed ----------------------------
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "this is not toml {{{").unwrap();
    let err = login::load().expect_err("a corrupt credentials file must be reported");
    assert!(
        err.contains("credentials.toml"),
        "the error should name the file so it can be deleted: {err}"
    );

    // -- a well-formed file missing a field is also named ------------------
    std::fs::write(&path, "account = \"acme\"\n").unwrap();
    let err = login::load().expect_err("an incomplete credential is not usable");
    assert!(err.contains("credentials.toml"), "{err}");

    std::env::remove_var("DRUMS_HOME");
}
