//! What Drums calls a service when nobody told it.
//!
//! The directory name is a bad default and it showed: a checkout API set up in
//! a temp directory reported itself as `tmp.zD1DjDq0Sj`. The team already chose
//! a name in `package.json`, and that name is the one already in their logs.

use drums_watch::setup::default_service_name;

fn repo(pkg: Option<&str>) -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    if let Some(p) = pkg {
        std::fs::write(d.path().join("package.json"), p).unwrap();
    }
    d
}

#[test]
fn the_declared_package_name_wins_over_the_directory() {
    let d = repo(Some(r#"{"name":"checkout-api"}"#));
    assert_eq!(default_service_name(d.path()), "checkout-api");
}

#[test]
fn a_scoped_package_drops_its_scope() {
    let d = repo(Some(r#"{"name":"@acme/payments"}"#));
    assert_eq!(
        default_service_name(d.path()),
        "payments",
        "`@acme/payments` reads badly in every log line it appears in"
    );
}

#[test]
fn the_directory_name_is_the_fallback() {
    let d = repo(None);
    assert_eq!(
        default_service_name(d.path()),
        d.path().file_name().unwrap().to_string_lossy()
    );
}

/// A manifest that is malformed, nameless, or blank must fall back rather than
/// panic or produce an empty service name — an empty name would make every
/// claim in the record ambiguous about what it describes.
#[test]
fn a_malformed_or_nameless_manifest_falls_back() {
    for pkg in [
        r#"{"version":"1.0.0"}"#,
        r#"{"name":""}"#,
        r#"{"name":"  "}"#,
        r#"{"name":"@acme/"}"#,
        "not json at all",
    ] {
        let d = repo(Some(pkg));
        assert_eq!(
            default_service_name(d.path()),
            d.path().file_name().unwrap().to_string_lossy(),
            "{pkg} should fall back, not panic or yield an empty name"
        );
    }
}
