//! F4 + F6 (Task-3 review rounds 3 and 4): every next-command Drums PRINTS
//! must actually run.
//!
//! Both defects were the same shape and both were invisible to the unit tests,
//! because a test that constructs the command itself can never catch it
//! (review round 4, n18): `Commands::Ship` and `Commands::Revert` each declare
//! `deploy_cmd: String` — a REQUIRED clap arg, not an `Option` — and each
//! resolves its record path from `--repo`/cwd, so the bare `drums ship <id>` /
//! `drums revert <id>` the renderer used to print exited 2 with "the following
//! required arguments were not provided" before doing anything at all. The
//! rollback one was worse: it is read AFTER an autonomous deploy has already
//! gone out.
//!
//! So this test does not assert on a string it wrote. It takes the exact line
//! `drums watch` would print (through the real `render`), extracts the command
//! out of that narration, argv-splits it the way a shell would, and RUNS the
//! real `drums` binary with it from an unrelated cwd — the only test shape that
//! can prove the printed line is runnable.

use std::path::{Path, PathBuf};
use std::process::Command;

use drums_watch::engine::EngineEvent;
use drums_watch::render::{render, RenderContext};
use engine_core::{
    CapturedRequest, Claim, ErrorEvent, ErrorSignature, Failure, Provenance, Repair, ShipOutcome,
};

/// Strip ANSI SGR sequences so the printed command can be read back out of the
/// styled narration exactly as a human's eyes (or a copy-paste) would see it.
fn strip_ansi(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // ESC [ <params> <final byte in @..~> — the `[` is itself in that
            // range, so it has to be consumed before the scan starts.
            if chars.next() == Some('[') {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Pull the `drums …` invocation for `subcommand` out of rendered narration.
fn printed_command(narration: &str, subcommand: &str) -> String {
    let plain = strip_ansi(narration);
    let needle = format!("drums {subcommand} ");
    let start = plain
        .find(&needle)
        .unwrap_or_else(|| panic!("narration prints no `{needle}` command:\n{plain}"));
    let rest = &plain[start..];
    let end = rest.find('\n').unwrap_or(rest.len());
    rest[..end].trim().to_string()
}

/// POSIX single-quote-aware argv split — what a shell does to the line the
/// operator copy-pastes. Deliberately minimal: the renderer only ever emits
/// bare words and `'…'`-quoted values (with `'\''` for an embedded quote).
fn shell_split(line: &str) -> Vec<String> {
    let mut argv = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut started = false;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_quotes = !in_quotes;
                started = true;
            }
            // Outside single quotes a backslash escapes the next character —
            // this is how POSIX `'\''` (close, literal quote, reopen) puts a
            // single quote inside a single-quoted string.
            '\\' if !in_quotes => {
                if let Some(escaped) = chars.next() {
                    cur.push(escaped);
                    started = true;
                }
            }
            c if c.is_whitespace() && !in_quotes => {
                if started {
                    argv.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            c => {
                cur.push(c);
                started = true;
            }
        }
    }
    if started {
        argv.push(cur);
    }
    argv
}

fn sample_failure() -> Failure {
    Failure {
        intake: engine_core::Intake::Snippet,
        id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
        service: "shop".into(),
        signature: ErrorSignature {
            error_name: "TypeError".into(),
            top_frame_file: "server.js".into(),
            top_frame_function: None,
        },
        first_seen_ms: 0,
        event_count: 3,
        sample: ErrorEvent {
            service: "shop".into(),
            occurred_at_ms: 0,
            error_name: "TypeError".into(),
            error_message: "m".into(),
            stack: String::new(),
            request: Some(CapturedRequest {
                method: "POST".into(),
                path: "/api/checkout".into(),
                content_type: None,
                body: None,
            }),
            intake: engine_core::Intake::Snippet,
        },
        claim: Claim {
            text: "3 errors".into(),
            provenance: Provenance::Observed,
        },
    }
}

/// A repo the printed `--repo` points at, holding an EMPTY record — so both
/// commands reach their own honest record refusal (exit 1) instead of running
/// any deploy. Nothing here can deploy anything: `ship`/`revert` both refuse
/// before their first side effect.
fn watched_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".drums")).unwrap();
    std::fs::write(dir.path().join(".drums").join("record.jsonl"), "").unwrap();
    dir
}

/// Run the printed line and assert clap ACCEPTED it: no exit code 2, no
/// "required arguments were not provided", and the process got far enough to
/// print `<action> failed —` (the honest record refusal `ship.rs` owns).
fn assert_runnable(line: &str, action: &str, cwd: &Path) {
    let argv = shell_split(line);
    assert_eq!(
        argv.first().map(String::as_str),
        Some("drums"),
        "printed line must start with `drums`: {line}"
    );
    let out = Command::new(env!("CARGO_BIN_EXE_drums"))
        .args(&argv[1..])
        .current_dir(cwd)
        .output()
        .expect("the drums binary must run");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stderr.contains("required arguments were not provided"),
        "the printed `{line}` is not runnable — clap refused it:\n{stderr}"
    );
    assert_ne!(
        out.status.code(),
        Some(2),
        "the printed `{line}` exited 2 (clap usage error):\n{stderr}"
    );
    assert!(
        stderr.contains(&format!("{action} failed")),
        "the printed `{line}` must reach {action}'s own honest refusal, not a usage error:\nstdout: {stdout}\nstderr: {stderr}"
    );
}

/// F6: the `reversible: drums revert <id>` line printed the instant an
/// autonomous deploy lands.
#[test]
fn the_revert_command_printed_on_a_successful_ship_actually_runs() {
    let repo = watched_repo();
    let elsewhere = tempfile::tempdir().unwrap(); // the operator is NOT cd'd into the watched repo
    let ctx = RenderContext {
        repo: repo.path().to_path_buf(),
        deploy_cmd: Some("echo would-deploy {sha}".to_string()),
    };
    let outcome = ShipOutcome {
        failure_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
        repair_sha: "deadbeef00".into(),
        action: "shipped".into(),
        deploy_cmd: "echo would-deploy deadbeef00".into(),
        claims: vec![Claim {
            text: "http://127.0.0.1:1/health returns 200".into(),
            provenance: Provenance::Verified,
        }],
    };
    let narration = render(&EngineEvent::Shipped(sample_failure(), outcome), &ctx);
    let line = printed_command(&narration, "revert");
    assert_runnable(&line, "revert", elsewhere.path());
}

/// F4's pin, re-asserted by EXECUTION rather than by string equality (n18).
#[test]
fn the_ship_command_printed_on_repair_ready_actually_runs() {
    let repo = watched_repo();
    let elsewhere = tempfile::tempdir().unwrap();
    let ctx = RenderContext {
        repo: repo.path().to_path_buf(),
        deploy_cmd: Some("echo would-deploy {sha}".to_string()),
    };
    let repair = Repair {
        id: "r1".into(),
        failure_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
        sha: "deadbeef00".into(),
        branch: "drums/repair-01ARZ3ND".into(),
        agent: "claude".into(),
        summary: "fixed the promo guard".into(),
        diff_stat: "server.js | 1 +".into(),
        claims: vec![Claim {
            text: "original failing request now returns 200".into(),
            provenance: Provenance::Verified,
        }],
    };
    let narration = render(
        &EngineEvent::RepairReady(sample_failure(), repair, 1_234),
        &ctx,
    );
    let line = printed_command(&narration, "ship");
    assert_runnable(&line, "ship", elsewhere.path());
}

/// The quoting has to survive the round trip: a deploy command containing a
/// space and a single quote must come back out of the printed line as ONE argv
/// element, or the pasted command silently deploys with a different template.
#[test]
fn shell_quoting_in_the_printed_line_round_trips_to_one_argv_element() {
    let ctx = RenderContext {
        repo: PathBuf::from("/srv/my shop"),
        deploy_cmd: Some("echo it's {sha}".to_string()),
    };
    let outcome = ShipOutcome {
        failure_id: "f1".into(),
        repair_sha: "deadbeef00".into(),
        action: "shipped".into(),
        deploy_cmd: "x".into(),
        claims: vec![],
    };
    let narration = render(&EngineEvent::Shipped(sample_failure(), outcome), &ctx);
    let argv = shell_split(&printed_command(&narration, "revert"));
    let deploy_idx = argv
        .iter()
        .position(|a| a == "--deploy-cmd")
        .expect("--deploy-cmd must be printed");
    assert_eq!(
        argv[deploy_idx + 1],
        "echo it's {sha}",
        "the quoted deploy command must survive as one argument: {argv:?}"
    );
    let repo_idx = argv
        .iter()
        .position(|a| a == "--repo")
        .expect("--repo must be printed");
    assert_eq!(
        argv[repo_idx + 1],
        "/srv/my shop",
        "a repo path with a space must survive as one argument: {argv:?}"
    );
}
