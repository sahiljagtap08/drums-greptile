//! The account gate: the long-running command runs for somebody.
//!
//! ## The decision
//!
//! `drums watch` used to start with no account at all — `curl drums.sh/install
//! | sh`, then a loop running for the rest of the week on a machine nobody
//! here could name. That is not a privacy win, it is a product that cannot
//! tell its own founder who is using it. The anonymous heartbeat
//! ([`crate::telemetry`]) does not close that gap and was never meant to: it
//! counts INSTALLS, and an install is not a person. So the one command that
//! runs for hours asks for an account before it settles in.
//!
//! ## What this deliberately does NOT do
//!
//! It does not gate the CLI. `drums init` — the single-shot command somebody
//! runs to watch Drums prove itself before they have decided anything — stays
//! open, and ends by INVITING a sign-in (see [`invitation`]) rather than
//! demanding one. A wall in front of the first command deletes the whole
//! promise that you can see this work before you sign up for it, and a
//! product that has to be trusted before it can be evaluated is asking for
//! the thing it is supposed to be earning.
//!
//! ## Why the credential is CHECKED rather than merely read
//!
//! A credentials file is not evidence. The token in it can have been revoked
//! from the console, expired, or minted against a different deployment
//! entirely; reading the file proves only that a file exists. So startup asks
//! the console whether the credential is still good, and then draws the one
//! distinction that matters:
//!
//! * the console ANSWERED and refused the credential — refuse to start, at
//!   second zero, while the operator is still looking at the terminal. A stale
//!   token that fails silently at the first dispatch an hour later is worse in
//!   every way: the failure arrives detached from its cause.
//! * the console could not be REACHED — say so and carry on. Detect,
//!   attribute, reproduce, repair and verify need nothing from us to run, and
//!   a local loop that stops working when our uptime does would have traded a
//!   real guarantee for a login screen.
//!
//! The obvious alternative — treat every non-success the same and refuse —
//! makes our outage into every customer's outage, which is the single most
//! expensive way to discover that a gate was written carelessly.

use std::time::Duration;

use crate::login::Credentials;

/// The environment variable that turns the gate off entirely.
///
/// An environment variable, and only that — not a flag as well. The places
/// that genuinely cannot sign in are CI jobs, air-gapped images and build
/// containers, and in all three the ENVIRONMENT is the thing configured once
/// while the command line belongs to whatever Makefile, script or supervisor
/// already types it. A flag has to be threaded through every one of those call
/// sites and re-typed on every invocation; a variable is declared once in a
/// job definition and inherited by every `drums` beneath it. `drums watch`
/// already carries `DRUMS_PLAIN` for exactly this reason ("for scripts that
/// can't easily pass a flag"), so this is a spelling a reader of this command
/// has already met.
///
/// It is named in `drums watch --help` AND in the refusal itself, because an
/// escape hatch that only exists in a changelog is folklore — and folklore is
/// how a tool gets deleted from a pipeline instead of configured.
pub const NO_ACCOUNT_ENV: &str = "DRUMS_NO_ACCOUNT";

/// The console route the check calls.
///
/// `GET /api/repairs` is the one console route that authenticates a CLI token
/// and changes nothing (`console/app/api/repairs/route.ts`): called with no
/// `?id=` it answers 400 to a caller it recognised and 401 to one it did not,
/// which is exactly the two-way answer this check needs. A dedicated
/// `/api/whoami` would read better and is not worth shipping a console deploy
/// for — this probe reads no job, returns no data, and leaves nothing behind.
const CHECK_PATH: &str = "/api/repairs";

/// How long to wait for the console before starting anyway.
///
/// Short on purpose. This sits directly between a person pressing return and
/// the loop existing, and the answer can only ever REFUSE a start — it never
/// enables one. Waiting longer buys certainty about a question whose "I don't
/// know" answer is already "carry on", at the cost of the thing the operator
/// actually asked for.
const CHECK_TIMEOUT: Duration = Duration::from_secs(8);

/// What somebody sees when they run `drums watch` on a machine that has never
/// signed in — most often the very next thing they type after
/// `curl drums.sh/install | sh`.
///
/// Three lines and no error code: what to type, what it buys, and the way out
/// for a machine that cannot sign in at all. The second line exists because
/// "login is required" answers a question nobody asked; the reason an account
/// is worth having is that the repairs belong to it.
pub const NOT_SIGNED_IN: &str = "\
drums watch needs an account — run `drums login` first.
  One browser approval, and every repair Drums makes belongs to an account: yours to
  see, to ship, and to revoke.
  No account possible (CI, air-gapped)? Set DRUMS_NO_ACCOUNT=1 to run the local loop alone.";

/// What the console's answer said about the credential.
///
/// Split out and pure, exactly like [`crate::login::interpret_poll`] and
/// [`crate::dispatch::interpret`], because these three branches are the whole
/// gate — whether an operator is stopped, warned, or waved through — and every
/// one of them has to be exercisable without a server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The console saw the credential and did not object to it.
    Accepted,
    /// The console saw the credential and refused it.
    Rejected,
    /// Nothing was established either way.
    Inconclusive(String),
}

/// Decide what an HTTP status from [`CHECK_PATH`] means.
pub fn interpret_check(status: u16) -> Verdict {
    match status {
        // The only answer that means "I looked at this credential and I do not
        // accept it". Nothing else is treated as a rejection: a 403 from a
        // corporate proxy, a 502 from a bad deploy or a captive portal's 200
        // must not be able to lock a working local loop out of its own repo.
        401 => Verdict::Rejected,
        // Everything else in 2xx/4xx got PAST authentication. A 400 is the
        // expected answer here — `GET /api/repairs` with no `?id=` is an
        // invalid request from a caller the console recognised — and 403 is
        // HTTP for "authenticated, but not allowed", which is still the
        // credential being accepted as a credential.
        200..=499 => Verdict::Accepted,
        // A 5xx says the console is having a bad day. It says nothing at all
        // about the token, so it must not be read as either answer.
        other => Verdict::Inconclusive(format!("the console answered HTTP {other}")),
    }
}

/// What startup should do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    /// Start. `note` is present only when the console could not be reached to
    /// confirm the credential, and is the sentence the operator is owed.
    Start {
        account: String,
        note: Option<String>,
    },
    /// Start with no account at all, because the operator said so.
    Bypassed,
    /// Do not start. Print this, exactly as it is.
    Refused(String),
}

/// Whether this run was told to proceed with no account.
///
/// Only `1`, `true` and `yes` count. An unrecognised value leaves the gate ON:
/// a typo in a CI variable must never be the thing that quietly turns a
/// requirement off, since the failure mode is silent and permanent.
pub fn bypassed() -> bool {
    matches!(
        std::env::var(NO_ACCOUNT_ENV)
            .ok()
            .map(|v| v.trim().to_ascii_lowercase())
            .as_deref(),
        Some("1") | Some("true") | Some("yes")
    )
}

/// The whole decision, with everything already resolved.
///
/// Takes the credentials and the console rather than reading them, so every
/// branch below is reachable from a test without touching process-wide
/// environment — the hazard `login.rs`'s own tests document.
pub async fn decide(creds: Option<Credentials>, console_url: &str, bypass: bool) -> Gate {
    if bypass {
        return Gate::Bypassed;
    }
    let Some(creds) = creds else {
        return Gate::Refused(NOT_SIGNED_IN.to_string());
    };

    // A token minted against a staging console is not a token for production.
    // `drums login` records which console it came from precisely so this can be
    // a named refusal instead of a confusing 401 later — the same check
    // `dispatch::RemoteRepairs::from_login` makes, for the same reason.
    if !creds.console_url.is_empty() && creds.console_url != console_url {
        return Gate::Refused(format!(
            "drums watch: this machine is signed in to {} but this run would talk to {console_url}.\n  \
             Run `drums login` again to sign in to the one you meant.",
            creds.console_url
        ));
    }

    match confirm(console_url, &creds.token).await {
        Verdict::Accepted => Gate::Start {
            account: creds.account,
            note: None,
        },
        Verdict::Rejected => Gate::Refused(format!(
            "drums watch: {console_url} did not accept this machine's sign-in — run `drums login` again.\n  \
             The token was revoked, has expired, or belongs to another account. Nothing local was changed.\n  \
             No account possible (CI, air-gapped)? Set {NO_ACCOUNT_ENV}=1 to run the local loop alone."
        )),
        Verdict::Inconclusive(why) => Gate::Start {
            account: creds.account,
            note: Some(format!(
                "could not reach {console_url} to confirm this sign-in ({why}) — starting anyway. \
                 That is an outage or a network gap, not a refused credential, and the local loop \
                 does not depend on our uptime."
            )),
        },
    }
}

/// The gate as `drums watch` runs it: credentials off disk, console from the
/// environment, escape hatch from the environment.
pub async fn gate() -> Gate {
    if bypassed() {
        return Gate::Bypassed;
    }
    let creds = match crate::login::load() {
        Ok(c) => c,
        // A credentials file that cannot be parsed is not "signed in", and
        // guessing which half of it survived would be worse than saying so.
        Err(why) => {
            return Gate::Refused(format!(
                "drums watch: {why}\n  Run `drums login` to write a fresh one."
            ))
        }
    };
    decide(creds, &crate::login::console_url(), false).await
}

/// The invitation a command that needed no account prints when this machine
/// has none.
///
/// `None` once they are signed in — an invitation that keeps arriving after it
/// has been accepted is not an invitation, and the whole reason this line is
/// allowed to exist at all is that the command it follows asked for nothing.
pub fn invitation(signed_in: bool) -> Option<&'static str> {
    if signed_in {
        return None;
    }
    Some(
        "Everything above ran with no account. `drums login` adds one — it is what\n\
         `drums watch` runs under, and where the repairs Drums makes become yours.",
    )
}

/// Ask the console whether this credential is still good.
///
/// Never returns the token, and never puts it anywhere it could be printed.
async fn confirm(console_url: &str, token: &str) -> Verdict {
    let client = match reqwest::Client::builder().timeout(CHECK_TIMEOUT).build() {
        Ok(c) => c,
        // Our own client failing to build says nothing about their credential,
        // so it must not refuse their start.
        Err(e) => return Verdict::Inconclusive(format!("could not build an HTTP client: {e}")),
    };
    match client
        .get(format!("{console_url}{CHECK_PATH}"))
        .bearer_auth(token)
        .send()
        .await
    {
        Ok(response) => interpret_check(response.status().as_u16()),
        // The error is over a URL we built ourselves; the token travels in a
        // header reqwest does not format here, so it cannot reach this string.
        Err(e) => Verdict::Inconclusive(with_cause(&e)),
    }
}

/// reqwest's own `Display` names the URL and stops there; the actual reason —
/// DNS, connection refused, TLS, timeout — sits one or more levels down the
/// source chain. The note this text ends up in exists to answer "is this my
/// network or theirs", and it cannot answer that without the cause.
fn with_cause(e: &reqwest::Error) -> String {
    let mut text = e.to_string();
    let mut source = std::error::Error::source(e);
    while let Some(cause) = source {
        text = format!("{text}: {cause}");
        source = cause.source();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn creds() -> Credentials {
        Credentials {
            token: "drums_pat_ThisIsNotARealToken".into(),
            account: "acme".into(),
            console_url: String::new(),
        }
    }

    /// A console that answers one fixed status and hands back the request it
    /// received, so the check can be driven end to end — reqwest included —
    /// without a network or a real console.
    async fn fake_console(status: u16) -> (String, tokio::sync::oneshot::Receiver<String>) {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut buf = vec![0u8; 2048];
            let n = socket.read(&mut buf).await.unwrap_or(0);
            let _ = tx.send(String::from_utf8_lossy(&buf[..n]).to_string());
            let _ = socket
                .write_all(
                    format!(
                        "HTTP/1.1 {status} X\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .await;
            let _ = socket.shutdown().await;
        });
        (format!("http://{addr}"), rx)
    }

    /// Nothing is listening on loopback port 1 — binding it needs root — so a
    /// connection there is refused instantly. That makes "the console is
    /// unreachable" a fast, deterministic test rather than one that sits out
    /// the timeout.
    const UNREACHABLE: &str = "http://127.0.0.1:1";

    // -- the refusal ----------------------------------------------------------

    /// The message somebody reads at the end of `curl drums.sh/install | sh`.
    /// It has to name the command that fixes it, or the install ends there.
    #[tokio::test]
    async fn watch_refuses_with_no_credentials_and_names_the_command_that_fixes_it() {
        let gate = decide(None, "https://app.drums.sh", false).await;
        let Gate::Refused(why) = gate else {
            panic!("an anonymous machine must not start a watch: {gate:?}");
        };
        assert!(why.contains("drums login"), "{why}");
        assert!(
            why.contains("belongs to an account"),
            "the refusal must say what signing in buys, not only that it is required: {why}"
        );
        assert!(
            why.contains(NO_ACCOUNT_ENV),
            "a hard requirement with no exit is how a tool gets removed from a pipeline: {why}"
        );
        assert!(
            why.lines().count() <= 4,
            "this is the first thing a new user reads; it is not a stack trace: {why}"
        );
    }

    /// A token minted against a staging console is not a token for production,
    /// and finding that out as a 401 later reads as our bug rather than their
    /// configuration.
    #[tokio::test]
    async fn a_credential_for_another_console_is_refused_by_name() {
        let mut c = creds();
        c.console_url = "http://localhost:3000".into();
        let gate = decide(Some(c), "https://app.drums.sh", false).await;
        let Gate::Refused(why) = gate else {
            panic!("a credential for another console must not start a watch: {gate:?}");
        };
        assert!(why.contains("localhost:3000"), "{why}");
        assert!(why.contains("drums login"), "{why}");
    }

    // -- starting -------------------------------------------------------------

    #[tokio::test]
    async fn watch_starts_when_the_console_accepts_the_credential() {
        // 400 is what a GOOD token gets from `GET /api/repairs` with no `?id=`:
        // recognised caller, incomplete request.
        let (url, request) = fake_console(400).await;
        let gate = decide(Some(creds()), &url, false).await;
        assert_eq!(
            gate,
            Gate::Start {
                account: "acme".into(),
                note: None
            },
            "an accepted credential starts cleanly and says nothing extra"
        );

        let head = request
            .await
            .expect("the check must actually send a request");
        assert!(
            head.to_ascii_lowercase().contains("authorization: bearer "),
            "a check that presents no credential proves nothing: {head}"
        );
    }

    /// The escape hatch, and the reason it exists: a machine that cannot sign
    /// in must still be able to run the loop.
    #[tokio::test]
    async fn the_escape_hatch_starts_with_no_account_and_asks_nothing() {
        assert_eq!(decide(None, UNREACHABLE, true).await, Gate::Bypassed);
        // And it does not need the console to be there either — the whole point
        // is an air-gapped or offline machine.
        assert_eq!(
            decide(Some(creds()), UNREACHABLE, true).await,
            Gate::Bypassed
        );
    }

    #[test]
    fn only_a_deliberate_value_turns_the_gate_off() {
        // `bypassed()` reads process-wide state, so this is the only test that
        // touches it and it restores what it found.
        let restore = std::env::var(NO_ACCOUNT_ENV).ok();

        std::env::remove_var(NO_ACCOUNT_ENV);
        assert!(!bypassed(), "the gate is on unless somebody says otherwise");
        for on in ["1", "true", "YES", " 1 "] {
            std::env::set_var(NO_ACCOUNT_ENV, on);
            assert!(bypassed(), "{on:?} must turn the gate off");
        }
        for off in ["", "0", "no", "flase"] {
            std::env::set_var(NO_ACCOUNT_ENV, off);
            assert!(
                !bypassed(),
                "{off:?} must leave the gate on — a typo in a CI variable cannot be how a \
                 requirement disappears"
            );
        }

        match restore {
            Some(v) => std::env::set_var(NO_ACCOUNT_ENV, v),
            None => std::env::remove_var(NO_ACCOUNT_ENV),
        }
    }

    // -- an outage is not a refusal -------------------------------------------

    /// THE property this module could most easily have gotten wrong: our
    /// console being down must not stop a customer's loop. Detect, attribute,
    /// reproduce, repair and verify all run on their machine and need nothing
    /// from us.
    #[tokio::test]
    async fn an_unreachable_console_does_not_block_startup() {
        let gate = decide(Some(creds()), UNREACHABLE, false).await;
        let Gate::Start { account, note } = gate else {
            panic!("our outage must never become their outage: {gate:?}");
        };
        assert_eq!(account, "acme");
        let note = note.expect("an unconfirmed sign-in has to be said out loud");
        assert!(note.contains("could not reach"), "{note}");
        assert!(
            note.contains("not a refused credential"),
            "the two cases have to be distinguishable by the person reading the line: {note}"
        );
    }

    /// And the other half of the same distinction: a console that ANSWERS and
    /// refuses the credential stops the watch there and then.
    #[tokio::test]
    async fn a_401_blocks_startup() {
        let (url, _request) = fake_console(401).await;
        let gate = decide(Some(creds()), &url, false).await;
        let Gate::Refused(why) = gate else {
            panic!(
                "a revoked token must refuse at second zero, not at the first dispatch: {gate:?}"
            );
        };
        assert!(why.contains("drums login"), "{why}");
        assert!(
            !why.contains("drums_pat_"),
            "no refusal may ever quote a credential: {why}"
        );
    }

    /// A console that is up but broken tells us nothing about the token, so it
    /// falls on the "carry on" side with everything else we did not learn.
    #[tokio::test]
    async fn a_console_having_a_bad_day_is_inconclusive_rather_than_a_refusal() {
        for status in [500, 502, 503] {
            assert_eq!(
                interpret_check(status),
                Verdict::Inconclusive(format!("the console answered HTTP {status}"))
            );
        }
        let (url, _request) = fake_console(503).await;
        assert!(
            matches!(decide(Some(creds()), &url, false).await, Gate::Start { .. }),
            "a 5xx is our problem, not theirs"
        );
    }

    #[test]
    fn only_a_401_is_read_as_a_rejection() {
        assert_eq!(interpret_check(401), Verdict::Rejected);
        for ok in [200, 201, 400, 403, 404, 429] {
            assert_eq!(interpret_check(ok), Verdict::Accepted, "HTTP {ok}");
        }
    }

    // -- the invitation -------------------------------------------------------

    /// `drums init` runs with no account and must keep doing so. The line it
    /// prints afterwards is an invitation, and it stops once it is accepted.
    #[test]
    fn the_invitation_is_earned_and_stops_once_it_is_accepted() {
        let invite = invitation(false).expect("an anonymous run should invite a sign-in");
        assert!(invite.contains("drums login"), "{invite}");
        assert!(
            invite.contains("ran with no account"),
            "the invitation has to be honest that nothing here demanded one: {invite}"
        );
        assert!(
            invite.lines().count() <= 2,
            "short, or it is a demand: {invite}"
        );
        assert_eq!(invitation(true), None, "signing in must end the asking");
    }
}
