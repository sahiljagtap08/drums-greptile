//! `drums login` — connect this machine to a Drums account.
//!
//! ## Why a device grant rather than a pasted token
//!
//! The obvious design is a settings page that prints a token and a CLI flag
//! that accepts one. It is also the design that teaches people to paste
//! long-lived credentials into terminals, shell history, and screen shares.
//! RFC 8628 exists precisely so a machine can be authorised without any
//! secret being handled by a human.
//!
//! ## The attack this flow invites, and what we do about it
//!
//! Section 5.4 of the RFC is blunt about it: an attacker starts a device flow,
//! sends the victim the "click here, it's already filled in" link, and the
//! victim approves the ATTACKER's session. The console's approval screen is
//! built to make that visible — it shows the code, names the account, and
//! says the code must match the terminal.
//!
//! This side does its share by printing the plain verification URI and the
//! code as two separate things. A person who is told "open this page, then
//! check the code matches" has been given something to check. One who is
//! handed a single link that does everything has not.
//!
//! ## Where the token goes
//!
//! `~/.drums/credentials.toml`, mode 0600, outside any repository. Not in
//! `<repo>/.drums/`, because that directory is committed by some teams and a
//! credential in a repository is a credential in a backup, a CI cache, and
//! somebody's fork.

use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Where the console lives unless told otherwise.
pub const DEFAULT_CONSOLE_URL: &str = "https://app.drums.sh";

/// Give up rather than poll forever. The server expires codes well before
/// this; the bound is here so a broken server cannot spin a terminal all day.
const MAX_POLL_DURATION: Duration = Duration::from_secs(15 * 60);

/// What a completed login stores.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Credentials {
    /// The bearer token. Never logged, never printed after this command.
    pub token: String,
    /// Which account it belongs to, so `drums whoami` needs no network call.
    pub account: String,
    /// Recorded so a token minted against a staging console is not silently
    /// used against production.
    pub console_url: String,
}

/// The console to talk to.
///
/// `DRUMS_CONSOLE_URL` exists so the flow can be exercised against a local
/// console — the tests in `tests/login.rs` depend on it — and so a
/// self-hosted deployment is configuration rather than a fork.
pub fn console_url() -> String {
    std::env::var("DRUMS_CONSOLE_URL")
        .ok()
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_CONSOLE_URL.to_string())
}

/// `~/.drums/credentials.toml`, or `$DRUMS_HOME/credentials.toml`.
///
/// `DRUMS_HOME` is honoured first so a test never writes to the real home
/// directory of whoever is running it.
pub fn credentials_path() -> Result<PathBuf, String> {
    if let Ok(home) = std::env::var("DRUMS_HOME") {
        let home = home.trim();
        if !home.is_empty() {
            return Ok(PathBuf::from(home).join("credentials.toml"));
        }
    }
    let home = std::env::var("HOME")
        .map_err(|_| "HOME is not set, so there is nowhere to store credentials".to_string())?;
    Ok(PathBuf::from(home).join(".drums").join("credentials.toml"))
}

/// Write credentials so only this user can read them.
///
/// The permissions are set on the file BEFORE the token is written, not after.
/// Creating a world-readable file and then chmod-ing it leaves a window in
/// which anyone on the machine can open it, and on a shared build host that
/// window is enough.
pub fn save(creds: &Credentials) -> Result<PathBuf, String> {
    let path = credentials_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
    }

    let body = toml::to_string_pretty(creds).map_err(|e| format!("could not serialise: {e}"))?;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let mut file = options
        .open(&path)
        .map_err(|e| format!("could not open {}: {e}", path.display()))?;
    file.write_all(body.as_bytes())
        .map_err(|e| format!("could not write {}: {e}", path.display()))?;

    // An existing file keeps its old mode through OpenOptions, so tighten it
    // explicitly for the re-login case.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("could not secure {}: {e}", path.display()))?;
    }

    Ok(path)
}

/// Read stored credentials. `Ok(None)` when simply not logged in, which is a
/// normal state and not an error.
pub fn load() -> Result<Option<Credentials>, String> {
    let path = credentials_path()?;
    match std::fs::read_to_string(&path) {
        Ok(body) => toml::from_str(&body)
            .map(Some)
            .map_err(|e| format!("{} is not readable as credentials: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("could not read {}: {e}", path.display())),
    }
}

/// Remove stored credentials. Reports whether there was anything to remove, so
/// `drums logout` can tell the truth either way.
pub fn forget() -> Result<bool, String> {
    let path = credentials_path()?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("could not remove {}: {e}", path.display())),
    }
}

/// What the CLI should do with a poll response.
///
/// Split out from the request loop so every branch can be tested without a
/// server: these are the states that decide whether a person waits, retries,
/// or is told to start again, and getting one wrong means a login that hangs.
#[derive(Debug, Clone, PartialEq)]
pub enum PollAction {
    /// Keep waiting at the current interval.
    KeepWaiting,
    /// Keep waiting, but slower — the server said we are being impatient.
    SlowDown,
    /// Finished. The token is ready.
    Done { token: String, account: String },
    /// Stop, and tell the person why.
    Stop(String),
}

/// Decide what a poll response means.
///
/// Takes the parsed body rather than a response type so it stays pure.
pub fn interpret_poll(status: u16, body: &serde_json::Value) -> PollAction {
    if (200..300).contains(&status) {
        let token = body.get("access_token").and_then(|v| v.as_str());
        let account = body
            .get("account")
            .and_then(|a| a.get("slug"))
            .and_then(|v| v.as_str());
        return match token {
            Some(t) if !t.is_empty() => PollAction::Done {
                token: t.to_string(),
                account: account.unwrap_or("").to_string(),
            },
            // A 200 with no token is a server bug. Saying so beats storing an
            // empty credential that fails confusingly on the next command.
            _ => PollAction::Stop(
                "the console reported success but sent no token — please try again".into(),
            ),
        };
    }

    let error = body.get("error").and_then(|v| v.as_str()).unwrap_or("");
    let described = body
        .get("error_description")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());

    match error {
        "authorization_pending" => PollAction::KeepWaiting,
        "slow_down" => PollAction::SlowDown,
        "expired_token" => {
            PollAction::Stop("that sign-in expired — run `drums login` again".into())
        }
        "access_denied" => PollAction::Stop("that sign-in was declined in the browser".into()),
        "invalid_grant" => {
            PollAction::Stop("that sign-in is no longer valid — run `drums login` again".into())
        }
        // An unrecognised refusal is still a refusal. Repeat what the server
        // said rather than inventing a reason.
        other => PollAction::Stop(match described {
            Some(d) => format!("the console refused: {d}"),
            None if !other.is_empty() => format!("the console refused: {other}"),
            None => format!("the console refused the sign-in (HTTP {status})"),
        }),
    }
}

/// Try to put the verification page in front of the person.
///
/// Best-effort by design: a failure here is not a failure of the login, since
/// the URL was printed first. Never propagates an error for that reason.
fn try_open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let candidates: [&str; 1] = ["open"];
    #[cfg(all(unix, not(target_os = "macos")))]
    let candidates: [&str; 2] = ["xdg-open", "gio"];
    #[cfg(not(unix))]
    let candidates: [&str; 0] = [];

    for cmd in candidates {
        let ok = std::process::Command::new(cmd)
            .arg(url)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            return;
        }
    }
}

/// Run the whole flow. Returns the path the credentials were written to.
pub async fn run(open_browser: bool) -> Result<PathBuf, String> {
    let base = console_url();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| format!("could not build an HTTP client: {e}"))?;

    let start = client
        .post(format!("{base}/api/device/code"))
        .send()
        .await
        .map_err(|e| format!("could not reach {base}: {e}"))?;

    let status = start.status();
    let body: serde_json::Value = start
        .json()
        .await
        .map_err(|e| format!("{base} sent a reply that was not JSON: {e}"))?;

    if !status.is_success() {
        let detail = body
            .get("error_description")
            .and_then(|v| v.as_str())
            .unwrap_or("the console would not start a sign-in");
        return Err(detail.to_string());
    }

    let device_code = body
        .get("device_code")
        .and_then(|v| v.as_str())
        .ok_or("the console did not send a device code")?
        .to_string();
    let user_code = body
        .get("user_code")
        .and_then(|v| v.as_str())
        .ok_or("the console did not send a user code")?
        .to_string();
    let verification_uri = body
        .get("verification_uri")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let mut interval = Duration::from_secs(
        body.get("interval")
            .and_then(|v| v.as_u64())
            .unwrap_or(5)
            .clamp(1, 60),
    );

    // The URI and the code, separately and in that order. Not a single
    // prefilled link: the person is being asked to check that these two things
    // match, and they cannot check something they were never shown.
    println!();
    println!("  Open:  {verification_uri}");
    println!("  Code:  {user_code}");
    println!();
    println!("  Approve it there, and check the code matches this terminal.");
    println!();
    let _ = std::io::stdout().flush();

    if open_browser && !verification_uri.is_empty() {
        try_open_browser(&verification_uri);
    }

    let deadline = std::time::Instant::now() + MAX_POLL_DURATION;
    loop {
        if std::time::Instant::now() >= deadline {
            return Err("gave up waiting for approval — run `drums login` again".into());
        }
        tokio::time::sleep(interval).await;

        let resp = client
            .post(format!("{base}/api/device/token"))
            .json(&serde_json::json!({ "device_code": device_code }))
            .send()
            .await;

        let (status, body) = match resp {
            Ok(r) => {
                let s = r.status().as_u16();
                let b: serde_json::Value = r.json().await.unwrap_or(serde_json::Value::Null);
                (s, b)
            }
            // A dropped connection mid-wait is not a refusal. Keep waiting;
            // the deadline above is what ends this.
            Err(_) => continue,
        };

        match interpret_poll(status, &body) {
            PollAction::KeepWaiting => continue,
            PollAction::SlowDown => {
                interval = (interval + Duration::from_secs(5)).min(Duration::from_secs(60));
            }
            PollAction::Stop(why) => return Err(why),
            PollAction::Done { token, account } => {
                let path = save(&Credentials {
                    token,
                    account: account.clone(),
                    console_url: base.clone(),
                })?;
                println!("  Signed in to {account}.");
                println!("  Credentials: {}", path.display());
                return Ok(path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_success_yields_the_token_and_account() {
        let action = interpret_poll(
            200,
            &json!({"access_token": "drums_pat_abc", "account": {"slug": "acme"}}),
        );
        assert_eq!(
            action,
            PollAction::Done {
                token: "drums_pat_abc".into(),
                account: "acme".into()
            }
        );
    }

    #[test]
    fn a_success_without_a_token_is_refused_rather_than_stored() {
        // Storing an empty credential would turn a server bug into a confusing
        // failure on some later, unrelated command.
        match interpret_poll(200, &json!({"account": {"slug": "acme"}})) {
            PollAction::Stop(why) => assert!(why.contains("no token"), "{why}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
        match interpret_poll(200, &json!({"access_token": ""})) {
            PollAction::Stop(_) => {}
            other => panic!("an empty token must not be stored: {other:?}"),
        }
    }

    #[test]
    fn pending_and_slow_down_both_keep_waiting() {
        assert_eq!(
            interpret_poll(400, &json!({"error": "authorization_pending"})),
            PollAction::KeepWaiting
        );
        assert_eq!(
            interpret_poll(400, &json!({"error": "slow_down"})),
            PollAction::SlowDown
        );
    }

    #[test]
    fn every_terminal_refusal_says_what_to_do_next() {
        for code in ["expired_token", "access_denied", "invalid_grant"] {
            match interpret_poll(400, &json!({ "error": code })) {
                PollAction::Stop(why) => {
                    assert!(!why.is_empty(), "{code} produced an empty reason");
                    assert!(
                        why.contains("drums login") || why.contains("declined"),
                        "{code} should tell the person what happens now: {why}"
                    );
                }
                other => panic!("{code} should stop, got {other:?}"),
            }
        }
    }

    #[test]
    fn an_unrecognised_refusal_repeats_the_server_rather_than_inventing_one() {
        match interpret_poll(
            400,
            &json!({"error": "teapot", "error_description": "the console is a teapot"}),
        ) {
            PollAction::Stop(why) => assert!(why.contains("teapot"), "{why}"),
            other => panic!("expected a stop, got {other:?}"),
        }
        // No description: still names the code rather than saying nothing.
        match interpret_poll(400, &json!({"error": "teapot"})) {
            PollAction::Stop(why) => assert!(why.contains("teapot"), "{why}"),
            other => panic!("expected a stop, got {other:?}"),
        }
        // Neither: still actionable.
        match interpret_poll(503, &json!({})) {
            PollAction::Stop(why) => assert!(why.contains("503"), "{why}"),
            other => panic!("expected a stop, got {other:?}"),
        }
    }

    #[test]
    fn a_null_body_does_not_panic() {
        match interpret_poll(500, &serde_json::Value::Null) {
            PollAction::Stop(_) => {}
            other => panic!("expected a stop, got {other:?}"),
        }
    }

    #[test]
    fn the_console_url_can_be_pointed_elsewhere_and_trailing_slashes_do_not_double_up() {
        // Serialised through one test because these mutate process env.
        let restore = std::env::var("DRUMS_CONSOLE_URL").ok();

        std::env::remove_var("DRUMS_CONSOLE_URL");
        assert_eq!(console_url(), DEFAULT_CONSOLE_URL);

        std::env::set_var("DRUMS_CONSOLE_URL", "http://localhost:3000/");
        assert_eq!(console_url(), "http://localhost:3000");

        std::env::set_var("DRUMS_CONSOLE_URL", "  http://localhost:3000  ");
        assert_eq!(console_url(), "http://localhost:3000");

        // An empty value means "unset", not "talk to an empty host".
        std::env::set_var("DRUMS_CONSOLE_URL", "");
        assert_eq!(console_url(), DEFAULT_CONSOLE_URL);

        match restore {
            Some(v) => std::env::set_var("DRUMS_CONSOLE_URL", v),
            None => std::env::remove_var("DRUMS_CONSOLE_URL"),
        }
    }
}
