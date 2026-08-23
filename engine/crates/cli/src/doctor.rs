//! `drums doctor` — which precondition is broken, said out loud.
//!
//! # Why this exists
//!
//! Somebody wired Drums into a real application, broke an endpoint on purpose,
//! watched the terminal, and saw nothing. Nothing is what Drums prints when it
//! is working perfectly and nothing has broken, and it is also what Drums
//! prints when the snippet was never wired, when the ingest URL was never set
//! on the deployed service, when the daemon is not running, and when no coding
//! agent is installed. Five states, one output.
//!
//! That is the failure this command exists to end. Every check here answers a
//! question somebody asks at the exact moment they are losing confidence in
//! the product, and every failing check names the CONSEQUENCE rather than the
//! state — "no events have ever arrived, so nothing can be detected" tells you
//! what to do; "events: 0" does not.
//!
//! # Read-only, always
//!
//! `doctor` never writes, never starts a daemon, never fixes anything. A
//! diagnostic that changes the system it is diagnosing cannot be run twice with
//! confidence, and the first thing anybody does with a diagnostic is run it
//! twice.
//!
//! # Why unknown is a real verdict
//!
//! Several of these facts genuinely cannot be established from this machine —
//! whether the deployed service has `DRUMS_INGEST_URL` set is a fact about a
//! container somewhere else. Reporting that as a failure would cry wolf, and
//! reporting it as a pass would be a lie. [`Status::Unknown`] says what was not
//! checked, which is the only honest third option and matches how the rest of
//! the product treats an unresolved claim.

use std::fmt;
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::time::Duration;

/// What a single check concluded.
pub enum Status {
    /// Established, first-hand, on this machine.
    Ok(String),
    /// Established, first-hand, and it is broken. Carries what to do.
    Bad { finding: String, fix: String },
    /// Not a failure and not a pass — this fact is not knowable from here.
    Unknown(String),
}

pub struct Check {
    pub name: &'static str,
    pub status: Status,
}

impl fmt::Display for Check {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.status {
            Status::Ok(detail) => write!(f, "  ok       {:<14} {detail}", self.name),
            Status::Unknown(detail) => write!(f, "  unknown  {:<14} {detail}", self.name),
            Status::Bad { finding, fix } => {
                write!(
                    f,
                    "  BROKEN   {:<14} {finding}\n           {:<14} → {fix}",
                    self.name, ""
                )
            }
        }
    }
}

/// Every check, in the order somebody would ask them: what am I running, where,
/// can it hear anything, has it ever heard anything, and can it act.
pub fn run(repo: &Path, version: &str) -> Vec<Check> {
    let mut checks = vec![Check {
        name: "version",
        status: Status::Ok(version.to_string()),
    }];

    checks.push(check_repo(repo));
    checks.push(check_config(repo));
    checks.push(check_agent());
    checks.push(check_agent_credentials(repo));
    checks.push(check_ingest(repo));
    checks.extend(check_record(repo));
    checks.push(check_behavior_source(repo));
    checks.push(check_notifications(repo));
    checks.push(check_sync(repo));
    checks.push(check_semantic(repo));
    checks.push(check_account());
    checks
}

/// Whether this repo's record mirrors to the hosted plane (`crate::sync`).
/// `ok`/`unknown`, NEVER broken: sync is opt-in and the local loop is
/// complete without it — this names a capability boundary, not a fault,
/// exactly like notifications above it.
fn check_sync(repo: &Path) -> Check {
    let cfg = crate::config::load(repo).ok().flatten().unwrap_or_default();
    sync_status(
        cfg.sync_record.unwrap_or(false),
        matches!(crate::login::load(), Ok(Some(_))),
        &crate::sync::repo_slug(repo),
    )
}

/// The pure half of [`check_sync`], split out so all three verdicts are
/// testable without touching the credentials file or a git remote.
fn sync_status(flag_on: bool, signed_in: bool, slug: &str) -> Check {
    let status = match (flag_on, signed_in) {
        (true, true) => Status::Ok(format!(
            "record sync on — through the hosted plane for {slug}"
        )),
        (false, _) => Status::Unknown(
            "record sync off — set sync_record = true in .drums/config.toml to mirror the record \
             to the hosted plane; it is stored redacted, so it arrives redacted"
                .into(),
        ),
        (true, false) => Status::Unknown(
            "sync_record = true but this machine is not signed in — run `drums login` to start \
             syncing"
                .into(),
        ),
    };
    Check {
        name: "sync",
        status,
    }
}

/// Whether proactive messages (Decisions, Learnings — see `crate::notify`)
/// have anywhere to go. `ok`/`unknown`, NEVER broken: the loop, the terminal
/// narration and the record are all complete without a webhook — this names
/// a capability boundary, not a fault, exactly like the behavior source
/// above it.
fn check_notifications(repo: &Path) -> Check {
    let cfg = crate::config::load(repo).ok().flatten().unwrap_or_default();
    notifications_status(crate::notify::resolve_webhook(cfg.slack_webhook_url.as_deref()).is_some())
}

/// The pure half of [`check_notifications`], split out so both verdicts are
/// testable without touching process-global environment state.
fn notifications_status(webhook_configured: bool) -> Check {
    let status = if webhook_configured {
        Status::Ok("slack configured — decisions and learnings will be delivered".into())
    } else {
        Status::Unknown(
            "no slack webhook — set slack_webhook_url in .drums/config.toml (or DRUMS_SLACK_WEBHOOK_URL) \
             to have decisions and learnings delivered; the terminal and the record show everything either way"
                .into(),
        )
    };
    Check {
        name: "notifications",
        status,
    }
}

/// The behavior source — PostHog — that completion-rate and abandonment plans
/// read their baselines and outcomes through. `unknown` rather than broken
/// when absent: the error-path loop and error_event_rate measurement run
/// without it, so this names a capability boundary, not a fault.
/// The semantic index and its embedding source. Both degrade gracefully —
/// the graph and lexical search need nothing; vector search needs the
/// environment to name an endpoint — so this is `ok`/`unknown`, never
/// `broken`.
fn check_semantic(repo: &Path) -> Check {
    let db = repo.join(".drums").join("semantic.db");
    let missing = engine_semantic::embed::missing_env();
    let status = match (db.exists(), missing.is_empty()) {
        (true, true) => Status::Ok("index built · embedding endpoint configured".into()),
        (true, false) => Status::Ok(format!(
            "index built · lexical-only until {} are set (env only)",
            missing.join(", ")
        )),
        (false, _) => Status::Unknown(
            "no semantic index yet — `drums index` builds it, and `drums watch` keeps it current"
                .into(),
        ),
    };
    Check {
        name: "semantic",
        status,
    }
}

fn check_behavior_source(repo: &Path) -> Check {
    let cfg = crate::config::load(repo).ok().flatten().unwrap_or_default();
    let missing = crate::behavior_source::missing(&cfg);
    if missing.is_empty() {
        Check {
            name: "behavior",
            status: Status::Ok("posthog configured — completion-rate plans are measurable".into()),
        }
    } else {
        Check {
            name: "behavior",
            status: Status::Unknown(format!(
                "posthog not configured — error-rate plans work without it; completion-rate plans need: {}",
                missing.join(", ")
            )),
        }
    }
}

/// The credential a repair needs when it runs in CI rather than on this machine.
///
/// A runner has no coding-agent CLI logged into it, so `drums repair` reads one
/// of three repository secrets and refuses with "no coding agent is available on
/// this runner" when none is set (see `repair_cmd.rs`). That refusal arrives at
/// the worst possible moment: a failure has already been detected, attributed
/// and REPRODUCED, and the dispatch is the step that fails. This check moves
/// that discovery to setup time, where it costs nothing.
///
/// # Names only, never values
///
/// `gh secret list` prints names and timestamps; there is no API that returns a
/// secret's value, to anyone, ever — GitHub's secrets endpoints are write-only
/// and the value is encrypted client-side against the repository's public key
/// before it is sent. So this check can confirm a credential EXISTS and can
/// never see what it is, which is the same property `drums-repair.yml` already
/// promises about the GitHub App.
///
/// # Why an absent workflow is `unknown` rather than broken
///
/// Local mode needs no secret at all — the agent is on this machine's PATH, which
/// [`check_agent`] already covers. Without `.github/workflows/drums-repair.yml`
/// there is no CI path to be broken, so reporting a missing secret as a fault
/// would cry wolf at every single-machine user.
pub fn check_agent_credentials(repo: &Path) -> Check {
    const NAME: &str = "agent secret";
    const WANTED: [&str; 3] = [
        "ANTHROPIC_API_KEY",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "OPENAI_API_KEY",
    ];

    if !repo.join(".github/workflows/drums-repair.yml").exists() {
        return Check {
            name: NAME,
            status: Status::Unknown(
                "no .github/workflows/drums-repair.yml — repairs run on this machine, \
                 which needs no secret"
                    .to_string(),
            ),
        };
    }

    // `gh`, not a token of our own: the same discipline `--propose-pr` uses.
    // Drums drives the tooling the user has already authenticated rather than
    // asking for a credential it would then have to hold.
    let Some(listed) = git_hub(repo, &["secret", "list"]) else {
        return Check {
            name: NAME,
            status: Status::Unknown(
                "could not ask GitHub which secrets this repo has (gh missing, logged out, \
                 or no access) — run `gh auth login` to include this check"
                    .to_string(),
            ),
        };
    };

    let present = present_secrets(&listed, &WANTED);

    if present.is_empty() {
        Check {
            name: NAME,
            status: Status::Bad {
                finding: "the repair workflow is committed but this repo has none of \
                          ANTHROPIC_API_KEY, CLAUDE_CODE_OAUTH_TOKEN or OPENAI_API_KEY"
                    .to_string(),
                fix: "a dispatched repair would reproduce the failure and then stop with \
                      \"no coding agent is available on this runner\" — run `drums init --wire` \
                      to set one, or `gh secret set ANTHROPIC_API_KEY`"
                    .to_string(),
            },
        }
    } else {
        Check {
            name: NAME,
            status: Status::Ok(present.join(", ")),
        }
    }
}

/// Which of `wanted` appear as secret NAMES in `gh secret list` output.
///
/// `gh secret list` prints `NAME<tab>UPDATED_AT`, so the name is the first
/// whitespace-delimited field. Matching on the whole field rather than with
/// `contains` is the whole point: `MY_ANTHROPIC_API_KEY_OLD` and
/// `ANTHROPIC_API_KEY_STAGING` are different secrets, and reporting either as
/// the credential the runner will read would make this check say a repo is
/// configured when the dispatched repair will still refuse.
fn present_secrets<'a>(listed: &str, wanted: &[&'a str]) -> Vec<&'a str> {
    let names: Vec<&str> = listed
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .collect();
    wanted
        .iter()
        .copied()
        .filter(|want| names.contains(want))
        .collect()
}

/// Runs `gh` in the repo, returning `None` for anything that is not a clean
/// success — not installed, not logged in, no access to this repository. Every
/// one of those means the same thing to the caller: this fact could not be
/// established from here, which is [`Status::Unknown`], not a failure.
fn git_hub(repo: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("gh")
        .current_dir(repo)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).to_string())
}

/// A repository, and one Drums can name. `--dispatch-repairs` needs `owner/name`
/// to find the customer's GitHub installation, and discovering that only when a
/// repair is already waiting is the worst moment to discover it.
fn check_repo(repo: &Path) -> Check {
    if !repo.join(".git").exists() {
        return Check {
            name: "repository",
            status: Status::Bad {
                finding: format!("{} is not a git repository", repo.display()),
                fix: "run drums from inside your repo, or pass --repo".to_string(),
            },
        };
    }
    match git(repo, &["remote", "get-url", "origin"]) {
        Some(url) if !url.is_empty() => Check {
            name: "repository",
            status: Status::Ok(format!("{} → {url}", repo.display())),
        },
        _ => Check {
            name: "repository",
            status: Status::Bad {
                finding: "no `origin` remote, so this repo has no owner/name".to_string(),
                fix: "git remote add origin … — hosted dispatch cannot resolve the repo without it"
                    .to_string(),
            },
        },
    }
}

fn check_config(repo: &Path) -> Check {
    let path = crate::config::config_path(repo);
    match crate::config::load(repo) {
        Ok(Some(_)) => Check {
            name: "config",
            status: Status::Ok(path.display().to_string()),
        },
        Ok(None) => Check {
            name: "config",
            status: Status::Bad {
                finding: "no .drums/config.toml".to_string(),
                fix: "drums init — the daemon needs it when no flags are passed".to_string(),
            },
        },
        Err(e) => Check {
            name: "config",
            status: Status::Bad {
                finding: e,
                fix: "fix or delete .drums/config.toml".to_string(),
            },
        },
    }
}

/// No agent means detect, attribute and reproduce all still work and every
/// repair stops dead. Worth knowing before a failure arrives, not after.
fn check_agent() -> Check {
    use engine_repair::RepairAgent as _;
    match engine_repair::CliRepairAgent::detect() {
        Some(agent) => Check { name: "agent", status: Status::Ok(agent.name().to_string()) },
        None => Check {
            name: "agent",
            status: Status::Bad {
                finding: "no coding agent found — failures will reproduce but never repair"
                    .to_string(),
                fix: "install claude, codex, gemini, cursor-agent, amp or opencode, or set DRUMS_AGENT_CMD".to_string(),
            },
        },
    }
}

/// Is anything actually listening where the snippet is told to post?
///
/// A TCP connect, not an HTTP request: this must not post anything into a
/// running loop just to look at it, and "something holds this port" is the
/// whole question. It cannot tell you the listener is Drums — that is stated
/// rather than assumed.
fn check_ingest(repo: &Path) -> Check {
    let port = crate::config::load(repo)
        .ok()
        .flatten()
        .and_then(|c| c.ingest_port)
        .unwrap_or(7787);

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    match TcpStream::connect_timeout(&addr, Duration::from_millis(300)) {
        Ok(_) => Check {
            name: "ingest",
            status: Status::Ok(format!("something is listening on 127.0.0.1:{port}")),
        },
        Err(_) => Check {
            name: "ingest",
            status: Status::Bad {
                finding: format!("nothing is listening on 127.0.0.1:{port}"),
                fix: "drums watch (or drums daemon start) — until then every report is dropped"
                    .to_string(),
            },
        },
    }
}

/// The two checks that answer "why have I seen nothing".
///
/// Deliberately two, because zero events and zero deploys mean different things
/// and have different fixes. No events means the snippet is not reaching Drums.
/// No deploys means attribution has nothing to attribute TO — a failure would be
/// detected and then stall with no deploy to blame.
fn check_record(repo: &Path) -> Vec<Check> {
    let path = repo.join(".drums").join("record.jsonl");
    let read = match engine_record::read_all(&path) {
        Ok(r) => r,
        Err(e) => {
            return vec![Check {
                name: "record",
                status: Status::Bad {
                    finding: format!("could not read {}: {e}", path.display()),
                    fix: "check permissions on .drums/record.jsonl".to_string(),
                },
            }]
        }
    };

    let count = |kind: &str| read.lines.iter().filter(|(k, _)| k == kind).count();
    let events = count("event");
    // `drums init`'s wire-check proves the pipe works; it says nothing about
    // whether the DEPLOYED service reports. Counting it as a received event
    // made this check read "ok" on an install no real error has ever reached.
    let self_checks = read
        .lines
        .iter()
        .filter(|(k, v)| {
            k == "event"
                && v.get("service").and_then(|s| s.as_str())
                    == Some(engine_detect::observe::SELF_TEST_SERVICE)
        })
        .count();
    let real_events = events.saturating_sub(self_checks);
    let deploys = count("deploy");

    vec![
        Check {
            name: "events",
            status: if real_events > 0 {
                Status::Ok(format!("{real_events} received"))
            } else if self_checks > 0 {
                Status::Unknown(
                    "no real error report has arrived yet (the install self-check did) — whether \
                     the deployed service reports cannot be known until it fails once"
                        .to_string(),
                )
            } else {
                Status::Bad {
                    finding: "no error report has ever arrived".to_string(),
                    fix: "wire the snippet (drums init --wire --verify), and set DRUMS_INGEST_URL \
                          on the DEPLOYED service — a reporter with no ingest URL is a no-op by \
                          design, and looks exactly like nothing being wrong"
                        .to_string(),
                }
            },
        },
        Check {
            name: "deploys",
            status: if deploys > 0 {
                Status::Ok(format!("{deploys} recorded"))
            } else {
                Status::Bad {
                    finding: "no deploy has ever been recorded".to_string(),
                    fix: "post to /v1/deploys from your deploy step — without it a failure can be \
                          detected but never attributed, so it can never be reproduced"
                        .to_string(),
                }
            },
        },
    ]
}

/// Only `--dispatch-repairs` needs an account, so being signed out is a
/// limitation rather than a fault, and says so.
fn check_account() -> Check {
    match crate::login::load() {
        // The account and the console it was minted against — never the token.
        // A diagnostic is the command people paste into a shared channel when
        // they are stuck, which makes it the last place a bearer token may
        // appear.
        Ok(Some(creds)) => Check {
            name: "account",
            status: Status::Ok(format!("{} · {}", creds.account, creds.console_url)),
        },
        Ok(None) => Check {
            name: "account",
            status: Status::Unknown(
                "not signed in — local mode works; --dispatch-repairs needs `drums login`"
                    .to_string(),
            ),
        },
        Err(e) => Check {
            name: "account",
            status: Status::Bad {
                finding: format!("credentials unreadable: {e}"),
                fix: "drums logout, then drums login".to_string(),
            },
        },
    }
}

fn git(repo: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .current_dir(repo)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Prints every check and returns the number that are BROKEN, so the caller can
/// exit non-zero. `unknown` deliberately does not count: exiting non-zero for
/// something we could not check would make `drums doctor` useless in CI, which
/// is the one place an exit code is read.
pub fn report(checks: &[Check]) -> usize {
    for c in checks {
        println!("{c}");
    }
    let broken = checks
        .iter()
        .filter(|c| matches!(c.status, Status::Bad { .. }))
        .count();
    println!();
    match broken {
        0 => println!("Nothing is broken that this machine can see."),
        1 => println!("1 thing is broken. Until it is fixed, silence from Drums means nothing."),
        n => println!(
            "{n} things are broken. Until they are fixed, silence from Drums means nothing."
        ),
    }
    broken
}

#[cfg(test)]
mod tests {
    use super::present_secrets;

    const WANTED: [&str; 3] = [
        "ANTHROPIC_API_KEY",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "OPENAI_API_KEY",
    ];

    /// `gh secret list` is NAME<tab>UPDATED_AT.
    #[test]
    fn reads_the_name_field_and_ignores_the_timestamp() {
        let listed =
            "CLAUDE_CODE_OAUTH_TOKEN\t2026-08-01T12:00:00Z\nRAILWAY_TOKEN\t2026-07-01T09:30:00Z\n";
        assert_eq!(
            present_secrets(listed, &WANTED),
            vec!["CLAUDE_CODE_OAUTH_TOKEN"]
        );
    }

    /// The case this function exists for. A substring match would report this
    /// repo as configured, and the dispatched repair would still refuse with
    /// "no coding agent is available on this runner" — a check that says
    /// "ok" about a setup that fails is worse than no check.
    #[test]
    fn a_name_that_merely_contains_a_wanted_one_does_not_count() {
        let listed = "MY_ANTHROPIC_API_KEY_OLD\t2026-08-01T12:00:00Z\nANTHROPIC_API_KEY_STAGING\t2026-08-01T12:00:00Z\n";
        assert!(present_secrets(listed, &WANTED).is_empty());
    }

    #[test]
    fn finds_every_wanted_secret_that_is_set() {
        let listed = "ANTHROPIC_API_KEY\tx\nOPENAI_API_KEY\tx\n";
        assert_eq!(
            present_secrets(listed, &WANTED),
            vec!["ANTHROPIC_API_KEY", "OPENAI_API_KEY"]
        );
    }

    /// A repo with no secrets at all prints nothing — not an error, just empty.
    #[test]
    fn empty_output_finds_nothing() {
        assert!(present_secrets("", &WANTED).is_empty());
        assert!(present_secrets("\n\n", &WANTED).is_empty());
    }

    fn append_event(repo: &std::path::Path, service: &str) {
        let record = repo.join(".drums").join("record.jsonl");
        std::fs::create_dir_all(record.parent().unwrap()).unwrap();
        engine_record::append(
            &record,
            "event",
            &serde_json::json!({
                "service": service, "occurred_at_ms": 1,
                "error_name": "E", "error_message": "m", "stack": "E: m"
            }),
            1,
        )
        .unwrap();
    }

    fn events_check(repo: &std::path::Path) -> super::Check {
        super::check_record(repo)
            .into_iter()
            .find(|c| c.name == "events")
            .expect("the events check always runs")
    }

    /// R11: the wire-check proves the pipe, not that the deployed service
    /// reports — a record holding only self-checks is unknown, never ok.
    #[test]
    fn a_record_holding_only_the_install_self_check_reads_unknown_not_ok() {
        let dir = tempfile::tempdir().unwrap();
        append_event(dir.path(), engine_detect::observe::SELF_TEST_SERVICE);
        match events_check(dir.path()).status {
            super::Status::Unknown(msg) => {
                assert!(
                    msg.contains("no real error report has arrived yet"),
                    "{msg}"
                );
                assert!(msg.contains("self-check"), "{msg}");
            }
            super::Status::Ok(detail) => {
                panic!("a self-check must not count as a real report: {detail}")
            }
            super::Status::Bad { finding, .. } => {
                panic!("the self-check DID arrive — 'never arrived' would be false too: {finding}")
            }
        }
    }

    /// The notifications check is a capability boundary, never a fault: `ok`
    /// with the delivery promise when configured, `unknown` NAMING THE CONFIG
    /// KEY when not — and structurally incapable of `Bad`, so a webhook-less
    /// install can never fail `drums doctor` over a courtesy channel.
    #[test]
    fn notifications_check_is_ok_when_configured_and_unknown_naming_the_key_when_not() {
        match super::notifications_status(true).status {
            super::Status::Ok(detail) => {
                assert!(
                    detail.contains("decisions and learnings will be delivered"),
                    "{detail}"
                )
            }
            _ => panic!("a configured webhook must read ok"),
        }
        match super::notifications_status(false).status {
            super::Status::Unknown(detail) => {
                assert!(
                    detail.contains("slack_webhook_url"),
                    "the config key is the fix: {detail}"
                );
                assert!(detail.contains("DRUMS_SLACK_WEBHOOK_URL"), "{detail}");
            }
            super::Status::Ok(detail) => panic!("unconfigured must not read ok: {detail}"),
            super::Status::Bad { finding, .. } => {
                panic!("a missing webhook is a boundary, never broken: {finding}")
            }
        }
    }

    /// The sync check is a capability boundary, never a fault, and each
    /// `unknown` names the exact half that is missing: the config flag when
    /// sync is off, `drums login` when the flag is on with no credential.
    /// Structurally incapable of `Bad` — a local-first install can never
    /// fail `drums doctor` over an opt-in mirror.
    #[test]
    fn sync_check_is_ok_with_flag_and_token_and_unknown_naming_the_missing_half_otherwise() {
        match super::sync_status(true, true, "acme/api").status {
            super::Status::Ok(detail) => {
                assert!(detail.contains("record sync on"), "{detail}");
                assert!(detail.contains("hosted plane"), "{detail}");
                assert!(
                    detail.contains("acme/api"),
                    "the slug says WHERE the record lands: {detail}"
                );
            }
            _ => panic!("flag + token must read ok"),
        }
        match super::sync_status(false, true, "acme/api").status {
            super::Status::Unknown(detail) => {
                assert!(
                    detail.contains("sync_record"),
                    "the config key is the fix: {detail}"
                );
            }
            super::Status::Ok(detail) => panic!("flag off must not read ok: {detail}"),
            super::Status::Bad { finding, .. } => {
                panic!("an opt-in left off is a boundary, never broken: {finding}")
            }
        }
        match super::sync_status(true, false, "acme/api").status {
            super::Status::Unknown(detail) => {
                assert!(
                    detail.contains("drums login"),
                    "the command is the fix: {detail}"
                );
            }
            super::Status::Ok(detail) => {
                panic!("a flag with no credential must not read ok: {detail}")
            }
            super::Status::Bad { finding, .. } => {
                panic!("a missing credential for an opt-in is a boundary, never broken: {finding}")
            }
        }
    }

    #[test]
    fn one_real_event_beside_the_self_check_reads_ok_and_counts_only_the_real_one() {
        let dir = tempfile::tempdir().unwrap();
        append_event(dir.path(), engine_detect::observe::SELF_TEST_SERVICE);
        append_event(dir.path(), "billing");
        match events_check(dir.path()).status {
            super::Status::Ok(detail) => assert_eq!(
                detail, "1 received",
                "the self-check must not inflate the count"
            ),
            _ => panic!("a real report arrived — the check must say ok"),
        }
    }
}
