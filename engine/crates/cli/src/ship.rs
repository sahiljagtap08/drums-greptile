//! `drums ship` / `drums revert` (spec §19): record-driven deploys, run as
//! their own process — entirely separate from `drums watch`, working only
//! from `.drums/record.jsonl` and `git`. No shell: the deploy-cmd template
//! is argv-split directly, the same discipline `engine-repair`'s agent
//! invocation and the engine's own `--repair auto` path already apply to
//! their command templates.

use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use engine_core::{CapturedRequest, Claim, Provenance, Repair, ShipOutcome};
use serde_json::Value;

// Process-group and bounded-drain discipline, shared with `engine.rs`'s
// test-script child (n15, review round 4): ONE copy of the `libc::kill`
// `unsafe` block and one copy of the drain in this crate. The POLICY for this
// call site is unchanged, and deliberately differs from `engine.rs`'s: a deploy
// command is group-killed only when it is being ABANDONED (a genuine timeout or
// wait error) — never merely because its pipes are still open, since a script
// that intentionally backgrounds the service it just deployed (exactly
// `demo/deploy.sh`'s `node server.js &`) is SUPPOSED to leave that process
// running past `run_deploy_cmd` returning.
use crate::proc::{drain_into, kill_process_group, set_new_process_group, take_text, DRAIN_GRACE};

#[derive(Debug, thiserror::Error)]
pub enum ShipError {
    /// Closing round (carried M-b): the old text said only "run `drums watch`
    /// until a repair completes", which misattributes the other real cause —
    /// the repair DID complete and its `repair_ready` append failed (the
    /// append is best-effort by design), or the line is present but
    /// unreadable. Telling an operator to re-run the loop when the record is
    /// the broken part sends them to the wrong place.
    #[error("no readable `repair_ready` line for failure {0} in the record — either no repair has completed yet (run `drums watch`), or one did and its record line could not be written or parsed")]
    NoRepairReady(String),
    /// Closing round (carried minor 8): a refusal should say what to do next.
    ///
    /// Trust-hardening review, m4: the old text always advised
    /// `drums revert {0}` regardless of which action already happened — for
    /// the `shipped` case that's the right next step, but for the `reverted`
    /// case it names the exact command that just refused: `drums revert`
    /// does not un-revert, and there is no `drums ship` to re-run either
    /// (the double-ship guard would refuse that too). The advice now
    /// branches on the recorded action; see [`already_actioned_message`].
    #[error("{}", already_actioned_message(.0, .1))]
    AlreadyActioned(String, String),
    #[error("nothing has been shipped for failure {0} — nothing to revert")]
    NothingToRevert(String),
    #[error("could not determine the deploy this repair was attributed to: {0}")]
    AttributionLookup(String),
    #[error("no deploy precedes the attributed deploy for failure {0} — nothing to revert to")]
    NoPriorDeploy(String),
    #[error("empty deploy command")]
    EmptyDeployCommand,
    #[error("deploy command failed to run: {0}")]
    DeployCommandIo(String),
    /// Closing round (carried minor 5 + M-n): carries whatever the deploy
    /// command had printed by the time it was abandoned. A command stalled on
    /// a `/dev/tty` prompt — the residual `Stdio::null()` cannot close, see
    /// [`run_deploy_cmd`] — has usually printed the prompt text itself, and
    /// that text is the operator's only clue about what it was waiting for.
    /// Ten silent minutes followed by a bare "timed out" was the worst
    /// version of this.
    #[error("deploy command timed out after 10 minutes — {0}")]
    DeployCommandTimeout(String),
    #[error("deploy command exited with {0}: {1}")]
    DeployCommandFailed(String, String),
    /// Fix round (I1): `engine_record::read_all` used to `panic!` on any
    /// non-`NotFound` IO error (a directory at the record path, permission
    /// denied, …) — a CLI whose entire pitch is honest, typed refusals must
    /// not stack-trace here instead.
    #[error("could not read the record at {0}: {1}")]
    RecordUnreadable(String, String),
    /// Closing round (F1's class fix, and carried minor 4): a sha read back
    /// out of `.drums/record.jsonl` is NOT trusted input, and this check
    /// stays in place as defense in depth even though `POST /v1/deploys` now
    /// validates its own `sha` field at the ingest boundary too
    /// (trust-hardening review, m3, `crates/ingest/src/lib.rs`'s
    /// `is_valid_git_sha`) — a record written before that fix existed, by an
    /// older client, or by any other writer of this file, can still carry a
    /// sha that reads as a git flag (`--upload-pack=…`), a path traversal, or
    /// a multi-byte string that panics a byte-slice. Refuse it before it
    /// reaches a deploy argv, a `git rev-parse {sha}^` argv, or a narration
    /// shortener. `0` is the offending value, `1` names where the record it
    /// came from lives.
    #[error(
        "refusing to act on sha {0:?} from {1}: a recorded sha must be 4-64 ASCII hex characters"
    )]
    InvalidRecordedSha(String, String),
}

/// Advice for [`ShipError::AlreadyActioned`], branched on which action
/// already happened (trust-hardening review, m4). The old text always said
/// "run `drums revert {0}`" — correct after a `shipped` line, but after a
/// `reverted` line it names the exact command that just refused: revert does
/// not un-revert, and the double-ship guard (`already_actioned`, called from
/// [`ship`] too) would refuse a fresh `drums ship` on this failure id just as
/// firmly. The honest next step in that case is not a command on this same
/// failure id at all — it's letting the underlying bug be freshly detected
/// (`Detector::reopen`) the next time it actually recurs.
fn already_actioned_message(failure_id: &str, action: &str) -> String {
    let next_step = if action == "reverted" {
        "the rollback is done; there is nothing further to ship or revert for this failure id — `drums revert` does not un-revert. If the underlying bug recurs, `drums watch` will detect it again as a fresh failure.".to_string()
    } else {
        format!("a ship can be rolled back with `drums revert {failure_id}`; neither action is ever re-run.")
    };
    format!("failure {failure_id} was already {action} — this is the double-action guard; the existing `{action}` line in `.drums/record.jsonl` names when it happened. {next_step}")
}

/// Split `template` on whitespace into argv, substituting the literal tokens
/// `{sha}`/`{repo}` PER ELEMENT — never into the joined template string
/// first. Fix round (I2): the prior implementation substituted into the
/// whole template string, then `split_whitespace()`d the result, so a
/// substituted value containing a space (an ordinary `--repo` path on macOS)
/// silently became several argv elements. Mirrors
/// `engine_repair::build_argv`'s discipline for `{prompt}` — a
/// developer-controlled template gets untrusted values substituted into it,
/// and the result must never be re-tokenized on whitespace. No shell is ever
/// involved: the result is run directly by [`run_deploy_cmd`].
fn build_argv(template: &str, sha: &str, repo: &Path) -> Vec<String> {
    let repo_str = repo.display().to_string();
    template
        .split_whitespace()
        .map(|tok| tok.replace("{sha}", sha).replace("{repo}", &repo_str))
        .collect()
}

/// The deploy command's own process must exit within this bound. This is
/// NOT the same as "the deployed service must come up" — a script that
/// backgrounds a long-lived server (`node server.js &`, `demo/deploy.sh`'s
/// own shape) is expected to exit almost immediately itself; only a script
/// that never returns at all hits this. Fix round (round-3 R1): that used to
/// be an over-claim — a deploy command reading INHERITED stdin never returns
/// either, and that is an ordinary deploy idiom, not a pathological script.
/// The bound is now genuinely last-resort because
/// [`run_deploy_cmd`] gives the child `/dev/null` for stdin.
const DEPLOY_TIMEOUT: Duration = Duration::from_secs(600);
/// What to tell the operator when a deploy command failed or was abandoned.
///
/// Closing round (carried minor 5): stdout was drained (it has to be — an
/// unread pipe blocks the writer) and then thrown away, so a script that
/// reports its problem on stdout produced `deploy command exited with exit
/// status: 1:` with an empty reason and no way to find out why. stderr stays
/// the primary channel so the usual message keeps its shape; stdout is a
/// labelled fallback used only when stderr had nothing at all; and "printed
/// nothing" is itself said out loud rather than rendering as blank.
fn failure_detail(stdout_buf: &Arc<Mutex<Vec<u8>>>, stderr_buf: &Arc<Mutex<Vec<u8>>>) -> String {
    let stderr = take_text(stderr_buf).trim().to_string();
    if !stderr.is_empty() {
        return stderr;
    }
    let stdout = take_text(stdout_buf).trim().to_string();
    if stdout.is_empty() {
        "the deploy command printed nothing on stdout or stderr".to_string()
    } else {
        format!("nothing on stderr; stdout said: {stdout}")
    }
}

/// Run `argv` (already split — see [`build_argv`]) with NO shell.
///
/// Fix round (C2): the prior implementation used
/// `child.wait_with_output()`, which does not return until BOTH piped
/// stdout/stderr reach EOF. A deploy script that backgrounds a long-lived
/// server — `demo/deploy.sh`'s own `… node "$PROD/server.js" &` — hands its
/// inherited pipes to a grandchild that is still alive long after the
/// deploy script itself has exited, so `drums ship` blocked for the full 10
/// minutes on a deploy that had already succeeded. Fix, reusing
/// `engine_repair::CliRepairAgent`'s already-fixed pattern: the pipe drains
/// start BEFORE anything waits on the child; ONLY `child.wait()` carries the
/// timeout (never the drains); once the exit status is known, the drains get
/// a short BOUNDED grace to flush their last bytes and are then abandoned
/// (never killed — see [`kill_process_group`]'s doc); a genuine timeout or
/// wait error, by contrast, DOES group-kill, since at that point the whole
/// deploy attempt is being abandoned and nothing should be left running
/// unaccounted for.
///
/// Fix round (round-3 R1, CONFIRMED live by the reviewer): stdin is
/// `Stdio::null()`, not inherited. A `--deploy-cmd` is NON-INTERACTIVE by
/// contract, and inheriting stdin broke that contract in the worst possible
/// way: every ordinary stdin-consuming deploy idiom (`ssh host 'bash -s'`,
/// `kubectl apply -f -`, `docker login --password-stdin`, any passphrase /
/// host-key / "are you sure" prompt) blocked on input that never arrives, so
/// `drums ship` and `--repair auto` sat for the full [`DEPLOY_TIMEOUT`]
/// printing NOTHING (stdout is piped and only surfaced on failure) and then
/// reported a 10-minute timeout for a deploy that had never begun.
/// [`set_new_process_group`] made it stranger still: a child in a background
/// process group that reads the controlling terminal is sent `SIGTTIN` and
/// STOPPED, so `child.wait()` could not complete and the operator never even
/// saw the prompt explaining the stall. `/dev/null` turns that into an
/// immediate EOF — the script takes its no-input path, or fails fast and
/// says so. Pinned by `crates/cli/tests/ship_stdin.rs` (the parent's stdin
/// has to be controlled to test an INHERITED fd, so that pin drives the real
/// binary with a never-closing stdin pipe).
///
/// Closing round (M-n) — the honest residual, since the sentence above used
/// to claim `/dev/null` fixes "all of that": it fixes every reader of fd 0,
/// which is the common case, but NOT a program that opens the controlling
/// terminal directly. `ssh` without `-n`/`BatchMode`, `sudo` without `-n`,
/// and `git`'s credential prompts all read `/dev/tty` when fd 0 is not a
/// terminal, and this process still has a controlling terminal for them to
/// find. Such a command is still in a background process group, so reading
/// `/dev/tty` earns `SIGTTIN` and stops it, and [`DEPLOY_TIMEOUT`] remains
/// the only thing that ends the stall. What has changed is that the stall is
/// no longer silent: on that timeout the operator now gets the deploy
/// command's own output (see [`failure_detail`]), which is where the prompt
/// text lands. The durable cure is a non-interactive `--deploy-cmd`, which
/// the flag's own help text now states as the contract.
async fn run_deploy_cmd(argv: &[String]) -> Result<(), ShipError> {
    let Some((prog, args)) = argv.split_first() else {
        return Err(ShipError::EmptyDeployCommand);
    };
    let mut command = tokio::process::Command::new(prog);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    set_new_process_group(&mut command);
    let mut child = command
        .spawn()
        .map_err(|e| ShipError::DeployCommandIo(e.to_string()))?;
    let pgid = child.id();
    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");

    let stdout_buf = Arc::new(Mutex::new(Vec::new()));
    let stderr_buf = Arc::new(Mutex::new(Vec::new()));
    // Order is load-bearing (C2): draining starts BEFORE anything waits on
    // the child.
    let mut stdout_task = tokio::spawn(drain_into(stdout, stdout_buf.clone()));
    let mut stderr_task = tokio::spawn(drain_into(stderr, stderr_buf.clone()));

    // ONLY the wait carries the 10-minute timeout.
    let status = match tokio::time::timeout(DEPLOY_TIMEOUT, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(e)) => {
            if let Some(pgid) = pgid {
                kill_process_group(pgid);
            }
            stdout_task.abort();
            stderr_task.abort();
            return Err(ShipError::DeployCommandIo(e.to_string()));
        }
        Err(_) => {
            // The deploy command's own process never exited — genuinely
            // abandoning this attempt, so cleaning up its whole process
            // group (unlike the residual-drain case below) is correct here.
            if let Some(pgid) = pgid {
                kill_process_group(pgid);
            }
            let _ = child.start_kill();
            stdout_task.abort();
            stderr_task.abort();
            return Err(ShipError::DeployCommandTimeout(failure_detail(
                &stdout_buf,
                &stderr_buf,
            )));
        }
    };

    // The exit status is known: give the drains a short, BOUNDED grace to
    // flush their last bytes, then stop waiting on an EOF a backgrounded
    // grandchild (the just-deployed service itself) may never produce.
    // Deliberately NOT a group-kill on this path — see this function's doc
    // and [`kill_process_group`]'s.
    let joined = tokio::time::timeout(DRAIN_GRACE, async {
        tokio::join!(&mut stdout_task, &mut stderr_task)
    })
    .await;
    if joined.is_err() {
        stdout_task.abort();
        stderr_task.abort();
    }

    if !status.success() {
        return Err(ShipError::DeployCommandFailed(
            status.to_string(),
            failure_detail(&stdout_buf, &stderr_buf),
        ));
    }
    Ok(())
}

/// Parse a captured request's HTTP method for the post-deploy replay. An
/// unparseable method is a hard error — never a silent `GET` substitution —
/// mirroring `engine_repro::parse_method`'s discipline for exactly the same
/// reason: a "verified" claim must describe the request that was actually
/// captured.
fn parse_method(method: &str) -> Result<reqwest::Method, String> {
    reqwest::Method::from_bytes(method.to_ascii_uppercase().as_bytes())
        .map_err(|_| format!("unsupported method {method:?}"))
}

/// GET `check_url` (verified/unresolved), then — if the original captured
/// request is available — replay it against `check_url`'s origin + the
/// request's own path (verified 2xx / unresolved). With no `check_url` at
/// all, the claim is honest about what was and wasn't checked: the deploy
/// command running is not itself evidence the deployed instance is healthy.
///
/// Fix round (I3): this used to accept ANY non-5xx as `verified`, which is
/// looser than the in-worktree verify the same branch already tightened to
/// 2xx for exactly this reason (`engine.rs`'s `verify_repair`,
/// `route_deletion_shaped_fix_returns_404_and_fails_verification_naming_the_status`)
/// — a fix that lands a 404 on the previously-failing route (the cheapest
/// way to make a 500 go away) must not earn `[verified]` here either, the
/// last gate before the record says `shipped`. 3xx/4xx are named and marked
/// `unresolved`.
///
/// The replayed request's body is what `find_repair_sample` read out of the
/// record's `repair_context` line, which is REDACTED (fix round, C1) — the
/// claim text says so, honestly, rather than implying a byte-identical
/// replay of the original request.
/// The URL the post-deploy replay actually targets: `check_url`'s ORIGIN
/// (scheme + host + port) with the captured request's own path and query
/// attached. `None` only when `check_url` itself doesn't parse.
///
/// Fix round (round-2 N2): this used to be `base.join(&req.path)`, which does
/// NOT keep the origin — `url`'s `join` resolves a network-path reference
/// (`//evil.example.com/api/checkout`) or an absolute reference
/// (`http://evil.example.com/x`) to THAT host, and `req.path` is not
/// developer-controlled: it is whatever the monitored app reported (`req.url`
/// in the reference reporter), which Node preserves verbatim for both of
/// those request-target forms. The consequences were (1) `drums ship` making
/// an outbound request to a host of the attacker's choosing from the
/// operator's machine or CI, with an attacker-chosen method, content-type and
/// body, while the operator believed it was probing their own deployment, and
/// (2) on a 2xx from that host, a `[verified]` provenance chip earned against
/// a server that is not the deployment, landing in the append-only record
/// next to `shipped`.
///
/// Host-pinning is now by CONSTRUCTION rather than by a rejection check, the
/// same shape `engine_repro` has always had
/// (`format!("http://127.0.0.1:{}{}", port, req.path)`): `set_path` can never
/// change the origin, so a hostile path becomes an odd-looking PATH on the
/// right server (`http://127.0.0.1:7211//evil.example.com/api/checkout`) —
/// which is also exactly how the monitored app itself saw the request — and
/// the claim text names that full URL. The query is split off first because
/// `set_path` percent-encodes `?` (a captured `/api/checkout?coupon=X` would
/// otherwise replay against a literal `%3F` path); `check_url`'s own query
/// and fragment are cleared so a health-probe URL's `?probe=1` can't leak
/// into the replayed request.
fn replay_url(check_url: &str, req_path: &str) -> Option<reqwest::Url> {
    let mut url = reqwest::Url::parse(check_url).ok()?;
    let (path, query) = match req_path.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (req_path, None),
    };
    url.set_path(path);
    url.set_query(query);
    url.set_fragment(None);
    Some(url)
}

/// Marker prefix for the one claim [`ship`]/[`revert`] add when the
/// best-effort record append failed (round-2 N3). Read back by
/// [`record_write_failed`] so narration can decline to promise a
/// reversibility the record can no longer deliver.
const RECORD_WRITE_FAILURE: &str = "the record line could not be written";

/// Whether this outcome's deploy really happened but its record line did not
/// get written (round-2 N3) — the one case in which `drums revert <id>` will
/// refuse with `NothingToRevert` despite a successful ship.
pub fn record_write_failed(outcome: &ShipOutcome) -> bool {
    outcome
        .claims
        .iter()
        .any(|c| c.text.starts_with(RECORD_WRITE_FAILURE))
}

async fn post_deploy_claims(
    check_url: Option<&str>,
    original_request: Option<&CapturedRequest>,
) -> Vec<Claim> {
    let Some(check_url) = check_url else {
        return vec![Claim {
            text: "deploy command ran; no post-deploy check configured".to_string(),
            provenance: Provenance::Unresolved,
        }];
    };
    let client = reqwest::Client::new();
    let mut claims = Vec::new();
    match client
        .get(check_url)
        .timeout(Duration::from_secs(10))
        .send()
        .await
    {
        Ok(resp) if resp.status().as_u16() == 200 => claims.push(Claim {
            text: format!("{check_url} returns 200"),
            provenance: Provenance::Verified,
        }),
        Ok(resp) => claims.push(Claim {
            text: format!(
                "{check_url} returned {} after deploy",
                resp.status().as_u16()
            ),
            provenance: Provenance::Unresolved,
        }),
        Err(e) => claims.push(Claim {
            text: format!("could not check {check_url} after deploy: {e}"),
            provenance: Provenance::Unresolved,
        }),
    }

    match original_request {
        None => claims.push(Claim {
            text: "no captured request available for this failure to replay post-deploy"
                .to_string(),
            provenance: Provenance::Unresolved,
        }),
        Some(req) => match replay_url(check_url, &req.path) {
            None => claims.push(Claim {
                text: "could not build a replay URL from check-url".to_string(),
                provenance: Provenance::Unresolved,
            }),
            Some(url) => match parse_method(&req.method) {
                Err(e) => claims.push(Claim {
                    text: format!("could not replay the original request: {e}"),
                    provenance: Provenance::Unresolved,
                }),
                Ok(method) => {
                    let mut builder = client.request(method, url.clone());
                    if let Some(ct) = &req.content_type {
                        builder = builder.header("content-type", ct.clone());
                    }
                    if let Some(body) = &req.body {
                        builder = builder.body(body.clone());
                    }
                    match builder.timeout(Duration::from_secs(10)).send().await {
                        Ok(resp) if (200..300).contains(&resp.status().as_u16()) => claims.push(Claim {
                            text: format!("replayed the originally failing request (redacted body) at {url}: now returns {}", resp.status().as_u16()),
                            provenance: Provenance::Verified,
                        }),
                        Ok(resp) => claims.push(Claim {
                            text: format!("replayed the originally failing request (redacted body) at {url}: still returns {}", resp.status().as_u16()),
                            provenance: Provenance::Unresolved,
                        }),
                        Err(e) => claims.push(Claim { text: format!("could not replay the original request against {url}: {e}"), provenance: Provenance::Unresolved }),
                    }
                }
            },
        },
    }
    claims
}

fn now_ms() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as u64)
}

/// Best-effort record append, mirroring `engine/crates/cli/src/engine.rs`'s
/// own `append_record`: a clock-read or write failure must not undo an
/// already-completed deploy — it's logged, not propagated. The deploy
/// itself is the primary, already-irreversible action by this point; the
/// record line is the compliance artifact behind it.
///
/// Fix round (round-2 N3): best-effort is still the right call, but it must
/// be SAID — this now returns the failure reason so the caller can turn it
/// into an honest `unresolved` claim, because "the deploy happened and the
/// record doesn't know" is exactly the divergence the ship narration's
/// unconditional `reversible: drums revert <id>` promise would otherwise lie
/// about. `tracing::error!` alone was invisible: the CLI installed no
/// subscriber at all (see `main.rs`'s `install_tracing`, added by the same
/// fix round).
fn append_record(
    record_path: &Path,
    kind: &'static str,
    item: &impl serde::Serialize,
) -> Result<(), String> {
    let Some(ms) = now_ms() else {
        tracing::error!(
            kind,
            "refusing to append record line: system clock unreadable"
        );
        return Err("system clock unreadable".to_string());
    };
    if let Err(e) = engine_record::append(record_path, kind, item, ms) {
        tracing::error!(kind, error = %e, "failed to append record line");
        return Err(e.to_string());
    }
    Ok(())
}

/// Append `outcome`'s own record line and, if that write failed, record the
/// failure IN the outcome as an `unresolved` claim (round-2 N3) — narrated to
/// the user and, unavoidably, absent from the record it's describing.
fn append_outcome(record_path: &Path, kind: &'static str, outcome: &mut ShipOutcome) {
    if let Err(e) = append_record(record_path, kind, outcome) {
        let action = outcome.action.clone();
        outcome.claims.push(Claim {
            text: format!(
                "{RECORD_WRITE_FAILURE}: {e} — the deploy DID happen, but `.drums/record.jsonl` has no `{action}` line for it, so `drums revert {}` will not find this ship",
                outcome.failure_id
            ),
            provenance: Provenance::Unresolved,
        });
    }
}

/// Fallible wrapper around `engine_record::read_all` (fix round, I1): the
/// crate function used to `panic!` on a non-`NotFound` IO error (a directory
/// at the record path, permission denied, …) — this is a CLI whose entire
/// pitch is honest, typed refusals, so that must become a `ShipError`, not a
/// Rust stack trace and exit 101.
fn read_record(record_path: &Path) -> Result<engine_record::RecordRead, ShipError> {
    engine_record::read_all(record_path)
        .map_err(|e| ShipError::RecordUnreadable(record_path.display().to_string(), e.to_string()))
}

/// Refuse a sha read back out of the append-only record before it reaches a
/// deploy argv, a `git rev-parse` argv, or a narration shortener.
///
/// Closing round (F1's class fix, carried minor 4). Deliberately the SAME
/// rule as `engine_repro`'s own `validate_sha` (ASCII hex, 4..=64), which
/// guards the worktree/git path on the reproduce side — this is the
/// ship/revert side of the same boundary, and the two must not disagree
/// about what a sha is. Being hex-only is what makes the `git rev-parse
/// {sha}^` argv safe by construction: a flag (`--upload-pack=…`), a
/// traversal (`../..`), a revision expression, and an empty string are all
/// non-hex, so none of them can reach git. `where_from` is the record path,
/// so the refusal names the file the operator has to go look at.
fn validate_recorded_sha(sha: &str, where_from: &Path) -> Result<(), ShipError> {
    let ok = (4..=64).contains(&sha.len()) && sha.bytes().all(|b| b.is_ascii_hexdigit());
    if ok {
        Ok(())
    } else {
        Err(ShipError::InvalidRecordedSha(
            sha.to_string(),
            where_from.display().to_string(),
        ))
    }
}

/// Render an argv back into a single string for the record's `deploy_cmd`
/// field, quoting any element that needs it so argv boundaries survive.
///
/// Closing round (M-p): this was `argv.join(" ")`, which is lossy exactly
/// where it matters — `{repo}` substitutes a real filesystem path, and a
/// path with a space (`/Users/x/my repo`, ordinary on macOS) read back as
/// two arguments. The record's answer to "what did you run" has to be an
/// unambiguous reconstruction, not something an auditor has to guess at.
/// POSIX single-quote rules: wrap in `'…'` and write an embedded `'` as
/// `'\''`.
///
/// DELIBERATE CARVE-OUT from the record's redaction posture (closing round,
/// M-j), stated here because this function is what writes the string: unlike
/// request bodies and query strings — which come from the monitored app and
/// are masked by `engine_record::redact_body`/`redact_query_string` before
/// they are ever appended — `deploy_cmd` and `check_url` are recorded
/// VERBATIM. They are developer-authored flags, not captured traffic: the
/// operator typed them on their own command line, where they are already in
/// their shell history, and the record's only job for them is to answer
/// "what exactly ran". Masking guessed-at token shapes inside a command
/// template would make that answer wrong in a way the auditor could not
/// detect. The consequence, which belongs in the operator-facing docs and
/// not only here: a secret embedded directly in `--deploy-cmd` or
/// `--check-url` (`--token=…`, a URL with `?api_key=…`) lands in
/// `.drums/record.jsonl` in the clear, so pass secrets via the environment
/// or a file the command reads instead of as flag text.
fn render_argv(argv: &[String]) -> String {
    argv.iter()
        .map(|a| {
            let safe = !a.is_empty()
                && a.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_=/.:,+@".contains(&b));
            if safe {
                a.clone()
            } else {
                format!("'{}'", a.replace('\'', r"'\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn lines_of_kind<'a>(
    record: &'a engine_record::RecordRead,
    kind: &'a str,
) -> impl DoubleEndedIterator<Item = &'a Value> {
    record
        .lines
        .iter()
        .filter(move |(k, _)| k == kind)
        .map(|(_, v)| v)
}

fn field_str<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(|f| f.as_str())
}

/// Refuse when `failure_id` already has a terminal `shipped` or `reverted`
/// line — the double-ship / double-revert guard. `action` (from the line
/// itself) is what's reported back, since a `"shipped"`-kind line's `action`
/// field is always `"shipped"` and a `"reverted"`-kind line's is always
/// `"reverted"` — the field exists for forward-compatibility, but reading it
/// rather than hardcoding the kind keeps this honest if that ever changes.
///
/// Trust-hardening review (m4, caught by that fix's own new test): this used
/// to check `for kind in ["shipped", "reverted"]` — a fixed PRIORITY, not a
/// timeline. A failure that was shipped and later reverted has BOTH a
/// `shipped` and a `reverted` line, and the old loop always found `shipped`
/// first regardless of which happened more recently — so `ship()` on an
/// already-reverted failure refused with "already shipped" advice
/// (`drums revert f1`, the command that had already run and that revert
/// itself separately refuses) instead of the true, more recent "already
/// reverted" state. Scanning `record.lines` (append-order) from the end and
/// taking whichever of the two kinds appears LAST is what actually answers
/// "what happened most recently to this failure".
fn already_actioned(record: &engine_record::RecordRead, failure_id: &str) -> Result<(), ShipError> {
    let latest = record.lines.iter().rev().find(|(kind, v)| {
        (kind == "shipped" || kind == "reverted") && field_str(v, "failure_id") == Some(failure_id)
    });
    if let Some((kind, v)) = latest {
        let action = field_str(v, "action").unwrap_or(kind).to_string();
        return Err(ShipError::AlreadyActioned(failure_id.to_string(), action));
    }
    Ok(())
}

/// The newest `repair_ready` line for `failure_id` that actually
/// deserializes.
///
/// Closing round (carried minor 1): this used to `rfind` the newest matching
/// line and then `.ok()` its deserialization, so ONE unreadable newest line
/// (a forward-incompatible or truncated write) made the whole command report
/// `NoRepairReady` — misattributing a parse problem to "the repair was never
/// produced", while a perfectly good earlier `repair_ready` sat in the record
/// unused. Filtering instead of stopping is what makes the append-only record
/// actually append-only from the reader's side.
fn latest_repair_ready(record: &engine_record::RecordRead, failure_id: &str) -> Option<Repair> {
    lines_of_kind(record, "repair_ready")
        .rev()
        .filter(|v| field_str(v, "failure_id") == Some(failure_id))
        .find_map(|v| serde_json::from_value(v.clone()).ok())
}

/// The original captured request behind this failure, if a `repair_context`
/// line (appended by `engine/crates/cli/src/engine.rs` alongside
/// `repair_ready`) exists for it. Absent for records written before that
/// line existed, or if the append itself failed (best-effort) — `None` is
/// handled honestly by [`post_deploy_claims`], never guessed at.
///
/// The request this reads is the REDACTED copy the record stores (fix
/// round, C1): `engine.rs` never persists the raw in-memory request — the
/// same redact-at-capture posture `engine-ingest` already applies to the
/// `event` line applies to this line too, since it is a second writer of
/// request content into the same compliance record.
fn find_repair_sample(
    record: &engine_record::RecordRead,
    failure_id: &str,
) -> Option<CapturedRequest> {
    lines_of_kind(record, "repair_context")
        .rfind(|v| field_str(v, "failure_id") == Some(failure_id))
        .and_then(|v| v.get("request").cloned())
        .and_then(|v| serde_json::from_value(v).ok())
}

fn latest_shipped(record: &engine_record::RecordRead, failure_id: &str) -> Option<ShipOutcome> {
    lines_of_kind(record, "shipped")
        .rfind(|v| field_str(v, "failure_id") == Some(failure_id))
        .and_then(|v| serde_json::from_value(v.clone()).ok())
}

/// The sha of the deploy that PRECEDES `attributed_sha` in the record's
/// `deploy` lines, in append (chronological) order. `None` when
/// `attributed_sha` isn't found among recorded deploys, or is the first one
/// — both mean "no prior deploy to roll back to".
///
/// Closing round (carried minor 2): resolves the sha's LAST occurrence, not
/// its first. Shas legitimately repeat in this record — a revert re-deploys
/// an earlier sha, which appends a second `deploy` line for it — and the
/// deploy that preceded the attributed one IN PRODUCTION is the one before
/// its most recent appearance. Taking the first occurrence rolled back to
/// whatever happened to precede it historically, which after any revert is
/// not the state being rolled back from.
fn deploy_before(record: &engine_record::RecordRead, attributed_sha: &str) -> Option<String> {
    let deploys: Vec<&str> = lines_of_kind(record, "deploy")
        .filter_map(|v| field_str(v, "sha"))
        .collect();
    let idx = deploys.iter().rposition(|s| *s == attributed_sha)?;
    if idx == 0 {
        None
    } else {
        Some(deploys[idx - 1].to_string())
    }
}

/// `git -C repo rev-parse <repair_sha>^` — the repair commit's parent is
/// exactly the attributed deploy's sha: `engine/crates/cli/src/engine.rs`'s
/// `commit_repair` creates the repair branch from the worktree's detached
/// HEAD (checked out at the attributed sha) and commits directly on top of
/// it, so this is a real, git-verified lookup, not a guess.
fn attributed_sha(repo: &Path, repair_sha: &str) -> Result<String, ShipError> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", &format!("{repair_sha}^")])
        .output()
        .map_err(|e| ShipError::AttributionLookup(e.to_string()))?;
    if !out.status.success() {
        return Err(ShipError::AttributionLookup(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `drums ship <failure-id>`: locate the latest `repair_ready` for
/// `failure_id` (error honestly if absent, or if this failure was already
/// shipped/reverted), run the deploy command at the repair's sha, then run
/// whatever post-deploy checks `check_url` allows. Appends a `shipped`
/// record line on success.
pub async fn ship(
    record_path: &Path,
    repo: &Path,
    failure_id: &str,
    deploy_cmd_template: &str,
    check_url: Option<&str>,
) -> Result<ShipOutcome, ShipError> {
    let record = read_record(record_path)?;
    already_actioned(&record, failure_id)?;
    let repair = latest_repair_ready(&record, failure_id)
        .ok_or_else(|| ShipError::NoRepairReady(failure_id.to_string()))?;
    // Before ANY side effect: the sha about to be substituted into the deploy
    // argv came out of the record, not out of this process.
    validate_recorded_sha(&repair.sha, record_path)?;
    let original_request = find_repair_sample(&record, failure_id);

    let argv = build_argv(deploy_cmd_template, &repair.sha, repo);
    run_deploy_cmd(&argv).await?;
    let claims = post_deploy_claims(check_url, original_request.as_ref()).await;

    let mut outcome = ShipOutcome {
        failure_id: failure_id.to_string(),
        repair_sha: repair.sha.clone(),
        action: "shipped".to_string(),
        deploy_cmd: render_argv(&argv),
        claims,
    };
    append_outcome(record_path, "shipped", &mut outcome);
    Ok(outcome)
}

/// `drums revert <failure-id>`: find the deploy that PRECEDED the deploy
/// this failure's shipped repair was attributed to (the last `deploy` line
/// before it), and deploy that sha — a full rollback of both the buggy
/// deploy and its repair. Refuses (typed error) when nothing has been
/// shipped for this failure, it was already reverted, or no prior deploy
/// exists. Appends a `reverted` record line on success.
pub async fn revert(
    record_path: &Path,
    repo: &Path,
    failure_id: &str,
    deploy_cmd_template: &str,
    check_url: Option<&str>,
) -> Result<ShipOutcome, ShipError> {
    let record = read_record(record_path)?;
    if lines_of_kind(&record, "reverted").any(|v| field_str(v, "failure_id") == Some(failure_id)) {
        return Err(ShipError::AlreadyActioned(
            failure_id.to_string(),
            "reverted".to_string(),
        ));
    }
    let shipped = latest_shipped(&record, failure_id)
        .ok_or_else(|| ShipError::NothingToRevert(failure_id.to_string()))?;

    // Two record-sourced shas reach an argv on this path, so both are checked
    // before either is used: `shipped.repair_sha` goes into a `git rev-parse
    // {sha}^` argv, and the `rollback_sha` it resolves to goes into the deploy
    // argv. Both checks precede every side effect.
    validate_recorded_sha(&shipped.repair_sha, record_path)?;
    let attributed = attributed_sha(repo, &shipped.repair_sha)?;
    let rollback_sha = deploy_before(&record, &attributed)
        .ok_or_else(|| ShipError::NoPriorDeploy(failure_id.to_string()))?;
    validate_recorded_sha(&rollback_sha, record_path)?;
    let original_request = find_repair_sample(&record, failure_id);

    let argv = build_argv(deploy_cmd_template, &rollback_sha, repo);
    run_deploy_cmd(&argv).await?;
    let claims = post_deploy_claims(check_url, original_request.as_ref()).await;

    // `repair_sha` here names the sha this outcome DEPLOYED (the rollback
    // target), matching how `Shipped` already uses the field — not the
    // repair commit's own sha, which for a revert is exactly what's being
    // undone.
    let mut outcome = ShipOutcome {
        failure_id: failure_id.to_string(),
        repair_sha: rollback_sha,
        action: "reverted".to_string(),
        deploy_cmd: render_argv(&argv),
        claims,
    };
    append_outcome(record_path, "reverted", &mut outcome);
    Ok(outcome)
}

// -- §7-style narration for the standalone `drums ship`/`drums revert` CLI --

// Styled bodies below; the public functions strip ANSI at the output
// boundary when stdout is not a terminal (audit R6) — same rule as
// `render::render`.
pub fn shipped_footer(outcome: &ShipOutcome, ctx: &crate::render::RenderContext) -> String {
    crate::render::finish(shipped_footer_styled(outcome, ctx))
}

pub fn shipped_ready_line(outcome: &ShipOutcome, ctx: &crate::render::RenderContext) -> String {
    crate::render::finish(shipped_ready_line_styled(outcome, ctx))
}

pub fn narrate_shipped(outcome: &ShipOutcome, ctx: &crate::render::RenderContext) -> String {
    crate::render::finish(narrate_shipped_styled(outcome, ctx))
}

pub fn narrate_reverted(outcome: &ShipOutcome) -> String {
    crate::render::finish(narrate_reverted_styled(outcome))
}

pub fn narrate_error(action: &str, err: &ShipError) -> String {
    crate::render::finish(narrate_error_styled(action, err))
}

const DIM: &str = "\x1b[2m";
const RED: &str = "\x1b[31m";
const GREEN: &str = "\x1b[32m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";

fn chip(c: &Claim) -> String {
    format!("[{}]", c.provenance.chip())
}

/// The footer line for a successful ship. Round-2 N3: the
/// `reversible: drums revert <id>` promise is CONDITIONAL on the `shipped`
/// record line actually having been written — `drums revert` works purely off
/// the record, so with no line there it refuses with `NothingToRevert` and an
/// unconditional promise here is simply false.
/// Shared by [`narrate_shipped`] (the standalone `drums ship` process) and
/// `render.rs`'s `Shipped` arm (the `--repair auto` path inside `drums
/// watch`) so the SAME event can't render differently depending on which
/// process printed it — and, more importantly, so the N3 honesty rule above
/// applies to both, since `ship::ship` is the single implementation behind
/// both (I4).
///
/// F6 (review round 4): the command itself is now built by
/// `render::revert_command_line` from `ctx`, because `drums revert <id>` on
/// its own exits 2 — `Commands::Revert` declares `deploy_cmd: String` (a
/// REQUIRED clap arg) and resolves its record path from `--repo`/cwd, exactly
/// like `Commands::Ship` did before F4. Both callers know these two values:
/// `drums watch` from its own `EngineConfig`, the standalone `drums ship` from
/// its own flags.
/// Whether ANY claim on this outcome is `Verified` — the post-deploy checks
/// [`post_deploy_claims`] produced, or the record-write-failure claim
/// [`append_outcome`] adds (always `Unresolved`, so it can never make this
/// `true` on its own).
///
/// Trust-hardening review, m5: a ship whose `check_url` 404s or whose replay
/// still errors produces an all-`Unresolved` outcome — a real repair that
/// might not actually be fixed in production — and the footer used to print
/// an unconditional `{GREEN}shipped{RESET}` regardless, visually
/// indistinguishable from a ship every claim on which was actually verified.
/// The text below each claim was always honest (`[unresolved]` chips don't
/// lie); it was the footer's colour and headline word that overstated it.
fn has_verified_claim(outcome: &ShipOutcome) -> bool {
    outcome
        .claims
        .iter()
        .any(|c| c.provenance == Provenance::Verified)
}

/// The `(record_write_failed, any_claim_verified, short_sha, revert_command)`
/// quadruple both [`shipped_footer`] (ANSI narrator) and
/// [`shipped_ready_line`] (plain-text, for the TUI) build their message
/// from — kept as ONE computation so neither the N3 honesty check nor m5's
/// can be independently re-decided by a third hand-written copy of either
/// conditional (which is exactly how the TUI's `ready_line` used to bypass
/// N3's).
fn shipped_status(
    outcome: &ShipOutcome,
    ctx: &crate::render::RenderContext,
) -> (bool, bool, String, String) {
    let short = crate::render::short_sha(&outcome.repair_sha);
    let revert = crate::render::revert_command_line(&outcome.failure_id, ctx);
    (
        record_write_failed(outcome),
        has_verified_claim(outcome),
        short,
        revert,
    )
}

fn shipped_footer_styled(outcome: &ShipOutcome, ctx: &crate::render::RenderContext) -> String {
    let (failed, verified, short, revert) = shipped_status(outcome, ctx);
    // m5: the success colour and the bare "shipped" headline are reserved
    // for a ship with at least one Verified claim about the post-deploy
    // state. An all-unresolved outcome (a 404 check-url, an unreachable
    // service, no check configured at all) says so in the headline itself
    // rather than only in the claim lines above it.
    let label = if verified {
        format!("{GREEN}shipped{RESET}")
    } else {
        format!("{BOLD}shipped{RESET} — {RED}unverified{RESET}")
    };
    if failed {
        format!("{DIM}└─{RESET} {label} {short} — {RED}not recorded{RESET}: `{revert}` will refuse (see above)\n")
    } else {
        format!("{DIM}└─{RESET} {label} {short} — reversible: {BOLD}{revert}{RESET}\n")
    }
}

/// Plain-text (no ANSI escapes) form of [`shipped_footer`]'s content, for the
/// TUI's `ready_line` (`ui/model.rs`), which does its own ratatui styling on
/// top of a plain `String` rather than embedding terminal escape codes into a
/// `Span::raw` (those would render as literal control-character garbage in
/// the alternate screen, not colors). Reuses [`shipped_status`] so both the
/// `record_write_failed` guard (N3) and the m5 "no verified claim" guard
/// apply identically here.
fn shipped_ready_line_styled(outcome: &ShipOutcome, ctx: &crate::render::RenderContext) -> String {
    let (failed, verified, short, revert) = shipped_status(outcome, ctx);
    let label = if verified {
        "shipped"
    } else {
        "shipped — unverified"
    };
    if failed {
        format!("{label} {short} — not recorded: `{revert}` will refuse (see above)")
    } else {
        format!("{label} {short} — reversible: {revert}")
    }
}

fn narrate_shipped_styled(outcome: &ShipOutcome, ctx: &crate::render::RenderContext) -> String {
    let mut out = format!("{DIM}├─{RESET} shipping\n");
    for c in &outcome.claims {
        out.push_str(&format!("   {GREEN}→{RESET} {} {}\n", c.text, chip(c)));
    }
    // The styled composition stays styled; only the public wrappers strip.
    out.push_str(&shipped_footer_styled(outcome, ctx));
    out
}

fn narrate_reverted_styled(outcome: &ShipOutcome) -> String {
    let mut out = format!("{DIM}├─{RESET} reverting\n");
    for c in &outcome.claims {
        out.push_str(&format!("   {GREEN}→{RESET} {} {}\n", c.text, chip(c)));
    }
    let short = crate::render::short_sha(&outcome.repair_sha);
    out.push_str(&format!(
        "{DIM}└─{RESET} {GREEN}reverted{RESET} to {short}\n"
    ));
    out
}

/// Narrate a ship/revert failure as one row ending in its chip.
///
/// `err` is SANITIZED, and with [`crate::render::sanitize`] (which strips
/// newlines) rather than `sanitize_multiline`. Every reason this matters:
///
/// - `ShipError` variants interpolate deploy-command output, record text, and
///   git stderr. All three are attacker-influenced upstream — a deploy script
///   prints whatever it likes, and `POST /v1/events` accepts unvalidated
///   fields that reach the record.
/// - This is a SINGLE-LINE row that ends in `[unresolved]`. A newline in the
///   middle would put the chip on a different line from the claim it belongs
///   to, and would let injected text render as its own top-level row — the
///   same row-forgery vector that was already found and fixed on
///   `ShipWithheld`, which is the precedent this follows.
/// - Raw ESC would let deploy output repaint the terminal, including
///   overwriting the word "failed".
fn narrate_error_styled(action: &str, err: &ShipError) -> String {
    let detail = crate::render::sanitize(&err.to_string());
    format!("{DIM}└─{RESET} {RED}{action} failed{RESET} — {detail} [unresolved]\n")
}

#[cfg(test)]
mod tests {
    /// Audit R6: the public narration fns strip ANSI when stdout is not a
    /// terminal — which is exactly the situation under `cargo test`.
    #[test]
    fn public_narration_is_plain_when_stdout_is_not_a_terminal() {
        let outcome = super::ShipOutcome {
            failure_id: "f1".into(),
            repair_sha: "abc123def".into(),
            action: "reverted".into(),
            deploy_cmd: "true".into(),
            claims: vec![],
        };
        let out = super::narrate_reverted(&outcome);
        assert!(
            !out.contains('\x1b'),
            "piped output must carry no escape codes: {out:?}"
        );
    }

    use super::*;
    use engine_core::{DeployRecord, RepairSample};
    use std::path::PathBuf;

    fn run_git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .expect("git spawn");
        assert!(status.success(), "git {args:?} failed in {}", dir.display());
    }

    /// F6: the shipped footer's `drums revert` command is built from what the
    /// printing process was actually configured with — the standalone `drums
    /// ship` passes its own `--deploy-cmd`/`--repo`, `drums watch` its
    /// `EngineConfig`'s. Narration tests print through this one.
    fn render_ctx() -> crate::render::RenderContext {
        crate::render::RenderContext {
            repo: PathBuf::from("/srv/shop"),
            deploy_cmd: Some("bash deploy.sh {sha}".to_string()),
        }
    }

    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        run_git(dir.path(), &["init", "-q"]);
        run_git(dir.path(), &["config", "user.email", "t@t"]);
        run_git(dir.path(), &["config", "user.name", "t"]);
        dir
    }

    fn commit(dir: &Path, name: &str, contents: &str) -> String {
        std::fs::write(dir.join("server.js"), contents).unwrap();
        run_git(dir, &["add", "server.js"]);
        run_git(dir, &["commit", "-q", "-m", name]);
        String::from_utf8(
            std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string()
    }

    fn append_deploy(record_path: &Path, sha: &str, ms: u64) {
        engine_record::append(
            record_path,
            "deploy",
            &DeployRecord {
                sha: sha.to_string(),
                description: "c".into(),
                author: "t".into(),
                deployed_at_ms: ms,
            },
            ms,
        )
        .unwrap();
    }

    fn append_repair_ready(record_path: &Path, failure_id: &str, repair_sha: &str, ms: u64) {
        let repair = Repair {
            id: "r1".into(),
            failure_id: failure_id.to_string(),
            sha: repair_sha.to_string(),
            branch: "drums/repair-f1".into(),
            agent: "fake".into(),
            summary: "fixed it".into(),
            diff_stat: "server.js | 1 +".into(),
            claims: vec![Claim {
                text: "original failing request now returns 200".into(),
                provenance: Provenance::Verified,
            }],
        };
        engine_record::append(record_path, "repair_ready", &repair, ms).unwrap();
    }

    fn append_repair_context(record_path: &Path, failure_id: &str, ms: u64) {
        append_repair_context_with_path(record_path, failure_id, "/api/checkout", ms);
    }

    fn append_repair_context_with_path(record_path: &Path, failure_id: &str, path: &str, ms: u64) {
        let sample = RepairSample {
            failure_id: failure_id.to_string(),
            request: CapturedRequest {
                method: "POST".into(),
                path: path.to_string(),
                content_type: Some("application/json".into()),
                body: Some("{}".into()),
            },
        };
        engine_record::append(record_path, "repair_context", &sample, ms).unwrap();
    }

    /// Writes an executable fake deploy script that appends each of its
    /// argv elements, one per line, to `log_path` — lets a test observe the
    /// EXACT argv a real (non-shell) `Command` invocation produced.
    fn write_fake_deploy_script(dir: &Path, log_path: &Path) -> PathBuf {
        let script_path = dir.join("fake-deploy.sh");
        let content = format!(
            "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\" >> \"{}\"; done\n",
            log_path.display()
        );
        std::fs::write(&script_path, content).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script_path, perms).unwrap();
        }
        script_path
    }

    // -- ship: argv substitution / no shell --------------------------------

    #[tokio::test]
    async fn ship_substitutes_sha_and_repo_runs_the_deploy_and_appends_shipped() {
        let repo_dir = init_repo();
        let repo = repo_dir.path();
        let repair_sha = commit(repo, "c1", "x");

        let record_path = repo.join("record.jsonl");
        append_repair_ready(&record_path, "f1", &repair_sha, 1);
        append_repair_context(&record_path, "f1", 1);

        let scripts_dir = tempfile::tempdir().unwrap();
        let log_path = scripts_dir.path().join("deploy.log");
        let script = write_fake_deploy_script(scripts_dir.path(), &log_path);
        let template = format!("{} {{sha}} {{repo}}", script.display());

        let outcome = ship(&record_path, repo, "f1", &template, None)
            .await
            .expect("ship must succeed");
        assert_eq!(outcome.action, "shipped");
        assert_eq!(outcome.repair_sha, repair_sha);

        let log = std::fs::read_to_string(&log_path).unwrap_or_default();
        assert!(
            log.contains(&repair_sha),
            "the deploy command must receive the repair's sha, not the literal \"{{sha}}\": {log}"
        );
        assert!(
            log.contains(&repo.display().to_string()),
            "the deploy command must receive the repo path: {log}"
        );

        let record = std::fs::read_to_string(&record_path).unwrap();
        assert!(
            record.contains("\"kind\":\"shipped\""),
            "record.jsonl must carry a shipped line: {record}"
        );
        assert!(record.contains("\"action\":\"shipped\""));

        assert!(
            outcome
                .claims
                .iter()
                .any(|c| c.text.contains("no post-deploy check configured")),
            "{:?}",
            outcome.claims
        );
    }

    #[tokio::test]
    async fn ship_deploy_cmd_argv_is_never_shell_interpreted() {
        let repo_dir = init_repo();
        let repo = repo_dir.path();
        let repair_sha = commit(repo, "c1", "x");

        let record_path = repo.join("record.jsonl");
        append_repair_ready(&record_path, "f1", &repair_sha, 1);

        let scripts_dir = tempfile::tempdir().unwrap();
        let log_path = scripts_dir.path().join("deploy.log");
        let script = write_fake_deploy_script(scripts_dir.path(), &log_path);
        let marker_dir = scripts_dir.path().join("must-not-be-deleted");
        std::fs::create_dir_all(&marker_dir).unwrap();
        std::fs::write(marker_dir.join("canary.txt"), "still here").unwrap();

        let template = format!(
            "{} {{sha}} ; rm -rf {}",
            script.display(),
            marker_dir.display()
        );
        let outcome = ship(&record_path, repo, "f1", &template, None)
            .await
            .expect("ship must succeed (the fake script always exits 0)");
        assert_eq!(outcome.action, "shipped");

        assert!(
            marker_dir.exists(),
            "a shell-interpreted `; rm -rf` would have deleted this"
        );
        assert!(marker_dir.join("canary.txt").exists());

        let log = std::fs::read_to_string(&log_path).unwrap_or_default();
        let lines: Vec<&str> = log.lines().collect();
        assert!(
            lines.contains(&";"),
            "the literal \";\" must arrive as its own inert argv element: {log}"
        );
        assert!(lines.contains(&"rm"));
        assert!(lines.contains(&"-rf"));
    }

    /// I2 (CONFIRMED live by the reviewer): substituting `{repo}` into the
    /// joined template string BEFORE `split_whitespace()`-ing it let a
    /// substituted value containing a space (an ordinary `--repo` path on
    /// macOS, the primary dev platform here) silently become multiple argv
    /// elements — `ARGV CORRUPTED: got 5 args`. Fix: split the template
    /// first, substitute per element.
    #[tokio::test]
    async fn ship_deploy_cmd_repo_path_containing_a_space_arrives_as_one_argv_element() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("my repo with spaces");
        std::fs::create_dir_all(&repo).unwrap();
        run_git(&repo, &["init", "-q"]);
        run_git(&repo, &["config", "user.email", "t@t"]);
        run_git(&repo, &["config", "user.name", "t"]);
        let repair_sha = commit(&repo, "c1", "x");

        let record_path = repo.join("record.jsonl");
        append_repair_ready(&record_path, "f1", &repair_sha, 1);

        let scripts_dir = tempfile::tempdir().unwrap();
        let log_path = scripts_dir.path().join("deploy.log");
        let script = write_fake_deploy_script(scripts_dir.path(), &log_path);
        let template = format!("{} {{sha}} {{repo}}", script.display());

        let outcome = ship(&record_path, &repo, "f1", &template, None)
            .await
            .expect("ship must succeed");
        assert_eq!(outcome.action, "shipped");

        let log = std::fs::read_to_string(&log_path).unwrap_or_default();
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(
            lines,
            vec![repair_sha.as_str(), repo.display().to_string().as_str()],
            "the repo path must arrive as ONE argv element even though it contains a space, not be split into several: {lines:?}"
        );
    }

    /// C2 (CONFIRMED live by the reviewer against the repo's own
    /// `demo/deploy.sh`): `wait_with_output()` does not return until BOTH
    /// piped stdout/stderr reach EOF, and a deploy script that backgrounds a
    /// long-lived server hands its inherited pipes to a grandchild that
    /// outlives the deploy script itself — `drums ship` blocked for the
    /// full 10-minute timeout on a deploy that had already succeeded.
    #[tokio::test]
    async fn ship_does_not_block_on_a_deploy_script_that_backgrounds_a_long_lived_process() {
        let repo_dir = init_repo();
        let repo = repo_dir.path();
        let repair_sha = commit(repo, "c1", "x");
        let record_path = repo.join("record.jsonl");
        append_repair_ready(&record_path, "f1", &repair_sha, 1);

        let scripts_dir = tempfile::tempdir().unwrap();
        let script_path = scripts_dir.path().join("backgrounding-deploy.sh");
        // Mirrors demo/deploy.sh's own shape: backgrounds a long-lived
        // process that inherits the piped stdout/stderr, then the deploy
        // script itself exits immediately.
        std::fs::write(&script_path, "#!/bin/sh\nsleep 30 &\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script_path, perms).unwrap();
        }
        let template = script_path.display().to_string();

        let outcome = tokio::time::timeout(Duration::from_secs(10), ship(&record_path, repo, "f1", &template, None))
            .await
            .expect(
                "ship must return promptly even though the deploy script backgrounded a long-lived process — \
                 a wait_with_output()-style implicit EOF wait would block for as long as that process is alive",
            )
            .expect("ship (deploy) itself must succeed");
        assert_eq!(outcome.action, "shipped");
    }

    /// The flip side of C2: the deploy command's backgrounded process (a
    /// stand-in for `demo/deploy.sh`'s own `node server.js &`, i.e. the
    /// service that was just deployed) must survive `drums ship` returning —
    /// the C2 fix must never group-kill it just because its pipe outlived
    /// the deploy script's own exit.
    #[tokio::test]
    #[cfg(unix)]
    async fn ship_does_not_kill_the_deploy_scripts_backgrounded_process() {
        let repo_dir = init_repo();
        let repo = repo_dir.path();
        let repair_sha = commit(repo, "c1", "x");
        let record_path = repo.join("record.jsonl");
        append_repair_ready(&record_path, "f1", &repair_sha, 1);

        let scripts_dir = tempfile::tempdir().unwrap();
        let pid_path = scripts_dir.path().join("bg.pid");
        let script_path = scripts_dir.path().join("backgrounding-deploy.sh");
        // `$!` (captured immediately after backgrounding, OUTSIDE any
        // subshell) is the POSIX-portable way to get the backgrounded
        // process's own pid — `$$` inside a `(...)  &` subshell is a
        // well-known bash-ism that reports the PARENT script's pid instead,
        // which would make this test check a pid that's already exited.
        std::fs::write(
            &script_path,
            format!(
                "#!/bin/sh\nsleep 30 &\necho $! > \"{}\"\nexit 0\n",
                pid_path.display()
            ),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script_path, perms).unwrap();
        }
        let template = script_path.display().to_string();

        let outcome = tokio::time::timeout(
            Duration::from_secs(10),
            ship(&record_path, repo, "f1", &template, None),
        )
        .await
        .expect("ship must return promptly")
        .expect("ship (deploy) itself must succeed");
        assert_eq!(outcome.action, "shipped");

        for _ in 0..40 {
            if pid_path.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let pid: u32 = std::fs::read_to_string(&pid_path)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let ps = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        let state = String::from_utf8_lossy(&ps.stdout).trim().to_string();
        assert!(
            !state.is_empty() && !state.starts_with('Z'),
            "the deploy script's backgrounded process (standing in for the just-deployed service) must still be \
             running after ship() returns — killing it would tear down the very server the deploy just brought up. ps state: {state:?}"
        );

        // best-effort cleanup
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGKILL);
        }
    }

    /// Round-3 R1 (CONFIRMED live by the reviewer): a deploy command that
    /// reads stdin must not stall the ship — `--deploy-cmd` is
    /// non-interactive by contract and the child gets `/dev/null`, so `read`
    /// sees EOF at once instead of waiting out the 600s `DEPLOY_TIMEOUT` with
    /// nothing printed (or, under `process_group(0)`, being `SIGTTIN`-stopped
    /// while reading the controlling terminal).
    ///
    /// Honest note on this test's power: a unit test cannot choose what fd 0
    /// the test HARNESS was handed, and the defect is about what the child
    /// INHERITS — so where `cargo test` itself already runs with stdin at
    /// `/dev/null` this passes either way, while under a live or
    /// never-closing stdin (`sleep 90 | cargo test`, or a terminal) it fails
    /// hard without `Stdio::null()`. The environment-independent pin is
    /// `crates/cli/tests/ship_stdin.rs`, which spawns the real binary so it
    /// can control the parent's stdin. Both exist deliberately: this one
    /// keeps the contract visible next to the code it constrains.
    #[tokio::test]
    #[cfg(unix)]
    async fn ship_returns_promptly_when_the_deploy_command_reads_stdin() {
        let repo_dir = init_repo();
        let repo = repo_dir.path();
        let repair_sha = commit(repo, "c1", "x");
        let record_path = repo.join("record.jsonl");
        append_repair_ready(&record_path, "f1", &repair_sha, 1);

        let scripts_dir = tempfile::tempdir().unwrap();
        let script_path = scripts_dir.path().join("stdin-reading-deploy.sh");
        // `ssh host 'bash -s'` / `kubectl apply -f -` / `docker login
        // --password-stdin` / any prompt, in miniature.
        std::fs::write(
            &script_path,
            "#!/bin/sh\nread -r line\necho \"got: $line\"\n",
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script_path, perms).unwrap();
        }
        let template = script_path.display().to_string();

        let outcome = tokio::time::timeout(Duration::from_secs(20), ship(&record_path, repo, "f1", &template, None))
            .await
            .expect(
                "ship must return promptly even though the deploy command reads stdin — with stdin INHERITED it \
                 cannot return before the 600s DEPLOY_TIMEOUT, and prints nothing at all until then",
            )
            .expect("ship (deploy) itself must succeed: the script exits 0 once its `read` sees EOF");
        assert_eq!(outcome.action, "shipped");
    }

    // -- refusals -------------------------------------------------------------

    #[tokio::test]
    async fn ship_fails_when_no_repair_ready_exists() {
        let repo_dir = init_repo();
        let repo = repo_dir.path();
        let record_path = repo.join("record.jsonl"); // never written to

        let err = ship(&record_path, repo, "f1", "echo {sha}", None)
            .await
            .expect_err("must refuse");
        assert!(
            matches!(&err, ShipError::NoRepairReady(id) if id == "f1"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn ship_refuses_a_double_ship() {
        let repo_dir = init_repo();
        let repo = repo_dir.path();
        let repair_sha = commit(repo, "c1", "x");
        let record_path = repo.join("record.jsonl");
        append_repair_ready(&record_path, "f1", &repair_sha, 1);

        let scripts_dir = tempfile::tempdir().unwrap();
        let log_path = scripts_dir.path().join("deploy.log");
        let script = write_fake_deploy_script(scripts_dir.path(), &log_path);
        let template = format!("{} {{sha}}", script.display());

        ship(&record_path, repo, "f1", &template, None)
            .await
            .expect("first ship must succeed");
        let err = ship(&record_path, repo, "f1", &template, None)
            .await
            .expect_err("second ship must refuse");
        assert!(
            matches!(&err, ShipError::AlreadyActioned(id, action) if id == "f1" && action == "shipped"),
            "{err}"
        );
        // m4: the "already shipped" advice is the pre-existing, correct one.
        assert!(err.to_string().contains("drums revert f1"), "{err}");
    }

    // -- revert: target selection ----------------------------------------------

    #[tokio::test]
    async fn revert_rolls_back_to_the_deploy_that_preceded_the_attributed_one() {
        let repo_dir = init_repo();
        let repo = repo_dir.path();
        let sha_a = commit(repo, "deploy A (good)", "a");
        let sha_b = commit(repo, "deploy B (buggy, attributed)", "b");
        // The repair commit's parent is sha_b (mirrors how
        // `engine/crates/cli/src/engine.rs`'s `commit_repair` always commits
        // directly on top of the attributed sha it checked the worktree out at).
        let repair_sha = commit(repo, "repair: fixed it", "b-fixed");

        let record_path = repo.join("record.jsonl");
        append_deploy(&record_path, &sha_a, 1_000);
        append_deploy(&record_path, &sha_b, 2_000);
        append_repair_ready(&record_path, "f1", &repair_sha, 3_000);
        engine_record::append(
            &record_path,
            "shipped",
            &ShipOutcome {
                failure_id: "f1".into(),
                repair_sha: repair_sha.clone(),
                action: "shipped".into(),
                deploy_cmd: "x".into(),
                claims: vec![],
            },
            4_000,
        )
        .unwrap();

        let scripts_dir = tempfile::tempdir().unwrap();
        let log_path = scripts_dir.path().join("deploy.log");
        let script = write_fake_deploy_script(scripts_dir.path(), &log_path);
        let template = format!("{} {{sha}}", script.display());

        let outcome = revert(&record_path, repo, "f1", &template, None)
            .await
            .expect("revert must succeed");
        assert_eq!(outcome.action, "reverted");
        assert_eq!(outcome.repair_sha, sha_a, "must roll back to the deploy BEFORE the attributed (buggy) one, not the buggy one itself");

        let log = std::fs::read_to_string(&log_path).unwrap_or_default();
        assert!(
            log.contains(&sha_a),
            "the deploy command must receive deploy A's sha: {log}"
        );
        assert!(
            !log.contains(&sha_b),
            "must never deploy the buggy sha it's rolling back FROM: {log}"
        );

        let record = std::fs::read_to_string(&record_path).unwrap();
        assert!(
            record.contains("\"kind\":\"reverted\""),
            "record.jsonl must carry a reverted line: {record}"
        );
    }

    #[tokio::test]
    async fn revert_fails_when_nothing_has_been_shipped() {
        let repo_dir = init_repo();
        let repo = repo_dir.path();
        let record_path = repo.join("record.jsonl");

        let err = revert(&record_path, repo, "f1", "echo {sha}", None)
            .await
            .expect_err("must refuse");
        assert!(
            matches!(&err, ShipError::NothingToRevert(id) if id == "f1"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn revert_fails_when_no_prior_deploy_exists() {
        let repo_dir = init_repo();
        let repo = repo_dir.path();
        let sha_b = commit(repo, "deploy B (only deploy, attributed)", "b");
        let repair_sha = commit(repo, "repair: fixed it", "b-fixed");

        let record_path = repo.join("record.jsonl");
        append_deploy(&record_path, &sha_b, 1_000); // the ONLY deploy — nothing precedes it
        append_repair_ready(&record_path, "f1", &repair_sha, 2_000);
        engine_record::append(
            &record_path,
            "shipped",
            &ShipOutcome {
                failure_id: "f1".into(),
                repair_sha: repair_sha.clone(),
                action: "shipped".into(),
                deploy_cmd: "x".into(),
                claims: vec![],
            },
            3_000,
        )
        .unwrap();

        let err = revert(&record_path, repo, "f1", "echo {sha}", None)
            .await
            .expect_err("must refuse");
        assert!(
            matches!(&err, ShipError::NoPriorDeploy(id) if id == "f1"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn revert_refuses_a_double_revert() {
        let repo_dir = init_repo();
        let repo = repo_dir.path();
        let sha_a = commit(repo, "deploy A", "a");
        let sha_b = commit(repo, "deploy B", "b");
        let repair_sha = commit(repo, "repair", "b-fixed");

        let record_path = repo.join("record.jsonl");
        append_deploy(&record_path, &sha_a, 1_000);
        append_deploy(&record_path, &sha_b, 2_000);
        append_repair_ready(&record_path, "f1", &repair_sha, 3_000);
        engine_record::append(
            &record_path,
            "shipped",
            &ShipOutcome {
                failure_id: "f1".into(),
                repair_sha: repair_sha.clone(),
                action: "shipped".into(),
                deploy_cmd: "x".into(),
                claims: vec![],
            },
            4_000,
        )
        .unwrap();

        let scripts_dir = tempfile::tempdir().unwrap();
        let log_path = scripts_dir.path().join("deploy.log");
        let script = write_fake_deploy_script(scripts_dir.path(), &log_path);
        let template = format!("{} {{sha}}", script.display());

        revert(&record_path, repo, "f1", &template, None)
            .await
            .expect("first revert must succeed");
        let err = revert(&record_path, repo, "f1", &template, None)
            .await
            .expect_err("second revert must refuse");
        assert!(
            matches!(&err, ShipError::AlreadyActioned(id, action) if id == "f1" && action == "reverted"),
            "{err}"
        );
        // m4 (trust-hardening review): after a REVERT, the advice must not
        // name `drums revert` — that is the exact command that just
        // refused, and there is nothing to un-revert or re-ship for this
        // failure id.
        let msg = err.to_string();
        assert!(!msg.contains("drums revert f1"), "the advice after a revert must not tell the user to run the command that just refused: {msg}");
        assert!(
            msg.contains("freshly detected") || msg.contains("does not un-revert"),
            "the advice must say what the actual next step is: {msg}"
        );

        // Also pin the `ship()` side of the same guard (double-ship after a
        // revert), which hits the SAME `AlreadyActioned("reverted", ..)`
        // branch via `already_actioned` and must carry the same honest advice.
        let ship_err = ship(&record_path, repo, "f1", &template, None)
            .await
            .expect_err("ship after a revert must also refuse");
        assert!(
            matches!(&ship_err, ShipError::AlreadyActioned(id, action) if id == "f1" && action == "reverted"),
            "{ship_err}"
        );
        assert!(
            !ship_err.to_string().contains("drums revert f1"),
            "{ship_err}"
        );
    }

    // -- post-deploy checks: a real HTTP server, not a mock ---------------------

    async fn spawn_check_server(port: u16, checkout_status: u16) -> tokio::task::JoinHandle<()> {
        use axum::routing::{get, post};
        let app =
            axum::Router::new()
                .route("/health", get(|| async { axum::http::StatusCode::OK }))
                .route(
                    "/api/checkout",
                    post(move || async move {
                        axum::http::StatusCode::from_u16(checkout_status).unwrap()
                    }),
                );
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
            .await
            .expect("bind test check server");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        })
    }

    /// A server that is up but has no `/health` route — the shape of a
    /// deployment that came up wrong. Used to pin the check_url non-2xx arm.
    async fn spawn_check_server_without_health(port: u16) -> tokio::task::JoinHandle<()> {
        let app = axum::Router::new().route(
            "/other",
            axum::routing::get(|| async { axum::http::StatusCode::OK }),
        );
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
            .await
            .expect("bind test check server");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        })
    }

    #[tokio::test]
    async fn ship_checks_url_and_replays_the_original_request_when_repair_context_exists() {
        let repo_dir = init_repo();
        let repo = repo_dir.path();
        let repair_sha = commit(repo, "c1", "x");

        let record_path = repo.join("record.jsonl");
        append_repair_ready(&record_path, "f1", &repair_sha, 1);
        append_repair_context(&record_path, "f1", 1); // POST /api/checkout

        let port = 7150;
        let server = spawn_check_server(port, 200).await;

        let scripts_dir = tempfile::tempdir().unwrap();
        let log_path = scripts_dir.path().join("deploy.log");
        let script = write_fake_deploy_script(scripts_dir.path(), &log_path);
        let template = format!("{} {{sha}}", script.display());
        let check_url = format!("http://127.0.0.1:{port}/health");

        let outcome = ship(&record_path, repo, "f1", &template, Some(&check_url))
            .await
            .expect("ship must succeed");
        server.abort();

        // n2: assert the check_url claim SPECIFICALLY (it names the check_url
        // and nothing else) — a bare `contains("returns 200")` was also
        // satisfied by the replay claim below, leaving this arm unpinned.
        assert!(
            outcome
                .claims
                .iter()
                .any(|c| c.text == format!("{check_url} returns 200")
                    && c.provenance == Provenance::Verified),
            "the check_url GET arm must produce its own verified claim: {:?}",
            outcome.claims
        );
        assert!(
            outcome.claims.iter().any(|c| c.text.contains("redacted body") && c.text.contains("now returns 200") && c.provenance == Provenance::Verified),
            "must replay the original /api/checkout request found via repair_context, honestly naming it as the redacted-body copy (C1): {:?}",
            outcome.claims
        );
    }

    /// I3: a fix landing a 404 on the previously-failing route (the cheapest
    /// way to make a 500 go away) must NOT earn `[verified]` here — the last
    /// gate before the record says `shipped` must not be looser than the
    /// in-worktree verify the same branch already tightened to 2xx.
    #[tokio::test]
    async fn ship_marks_a_non_2xx_replay_as_unresolved_naming_the_status_never_verified() {
        let repo_dir = init_repo();
        let repo = repo_dir.path();
        let repair_sha = commit(repo, "c1", "x");

        let record_path = repo.join("record.jsonl");
        append_repair_ready(&record_path, "f1", &repair_sha, 1);
        append_repair_context(&record_path, "f1", 1); // POST /api/checkout

        let port = 7153;
        let server = spawn_check_server(port, 404).await; // route-deletion-shaped fix
        let scripts_dir = tempfile::tempdir().unwrap();
        let log_path = scripts_dir.path().join("deploy.log");
        let script = write_fake_deploy_script(scripts_dir.path(), &log_path);
        let template = format!("{} {{sha}}", script.display());
        let check_url = format!("http://127.0.0.1:{port}/health");

        let outcome = ship(&record_path, repo, "f1", &template, Some(&check_url))
            .await
            .expect("ship (deploy) itself must still succeed");
        server.abort();

        assert!(
            !outcome
                .claims
                .iter()
                .any(|c| c.text.contains("404") && c.provenance == Provenance::Verified),
            "a 404 (route deleted, not fixed) must never be reported verified: {:?}",
            outcome.claims
        );
        assert!(
            outcome
                .claims
                .iter()
                .any(|c| c.text.contains("still returns 404")
                    && c.provenance == Provenance::Unresolved),
            "the 404 must be named and marked unresolved: {:?}",
            outcome.claims
        );
    }

    #[tokio::test]
    async fn ship_honestly_marks_the_replay_unresolved_when_the_deployed_instance_still_fails() {
        let repo_dir = init_repo();
        let repo = repo_dir.path();
        let repair_sha = commit(repo, "c1", "x");

        let record_path = repo.join("record.jsonl");
        append_repair_ready(&record_path, "f1", &repair_sha, 1);
        append_repair_context(&record_path, "f1", 1);

        let port = 7151;
        let server = spawn_check_server(port, 500).await; // still fails post-deploy

        let scripts_dir = tempfile::tempdir().unwrap();
        let log_path = scripts_dir.path().join("deploy.log");
        let script = write_fake_deploy_script(scripts_dir.path(), &log_path);
        let template = format!("{} {{sha}}", script.display());
        let check_url = format!("http://127.0.0.1:{port}/health");

        let outcome = ship(&record_path, repo, "f1", &template, Some(&check_url))
            .await
            .expect("ship (deploy) itself must still succeed");
        server.abort();

        assert!(
            outcome
                .claims
                .iter()
                .any(|c| c.text.contains("still returns 500")
                    && c.provenance == Provenance::Unresolved),
            "a still-failing deployed instance must never be reported verified: {:?}",
            outcome.claims
        );
        assert!(
            !outcome
                .claims
                .iter()
                .any(|c| c.text.contains("500") && c.provenance == Provenance::Verified),
            "a 500 must never carry the verified chip: {:?}",
            outcome.claims
        );
    }

    #[tokio::test]
    async fn ship_marks_the_replay_claim_unresolved_when_no_repair_context_was_recorded() {
        let repo_dir = init_repo();
        let repo = repo_dir.path();
        let repair_sha = commit(repo, "c1", "x");

        let record_path = repo.join("record.jsonl");
        append_repair_ready(&record_path, "f1", &repair_sha, 1);
        // deliberately no repair_context line

        let scripts_dir = tempfile::tempdir().unwrap();
        let log_path = scripts_dir.path().join("deploy.log");
        let script = write_fake_deploy_script(scripts_dir.path(), &log_path);
        let template = format!("{} {{sha}}", script.display());

        let outcome = ship(
            &record_path,
            repo,
            "f1",
            &template,
            Some("http://127.0.0.1:1/unused"),
        )
        .await;
        // The check_url itself is unreachable here (nothing listening on
        // :1) — the point of this test is the SECOND claim, about the
        // missing repair_context, not the check_url outcome.
        let outcome = outcome.expect(
            "the deploy command itself must still succeed even if the check_url is unreachable",
        );
        assert!(
            outcome
                .claims
                .iter()
                .any(|c| c.text.contains("no captured request available")
                    && c.provenance == Provenance::Unresolved),
            "{:?}",
            outcome.claims
        );
        let _ = log_path;
    }

    // -- N2: the replay target must never leave check_url's origin -------------

    /// Round-2 N2 (CONFIRMED by the reviewer's `url`-crate probe): the brief
    /// says "replay the original captured request against **`check_url`'s
    /// origin** + original path", and `Url::join` does NOT guarantee that —
    /// a network-path reference (`//host/x`) or an absolute reference
    /// (`http://host/x`) resolves to that OTHER host. `req.path` is not
    /// developer-controlled: it is whatever the monitored app reported
    /// (`req.url` in the reference reporter), and Node preserves both of
    /// those target forms verbatim there. So `drums ship` could make an
    /// outbound, attacker-directed request from the operator's machine and,
    /// on a 2xx, write `replayed … now returns 200 [verified]` into the
    /// append-only record for a server that is not the deployment.
    #[test]
    fn replay_url_pins_the_replay_to_check_urls_origin() {
        let base = "http://127.0.0.1:7150/health";
        let origin = reqwest::Url::parse(base).unwrap().origin();

        // The intended case.
        assert_eq!(
            replay_url(base, "/api/checkout").unwrap().as_str(),
            "http://127.0.0.1:7150/api/checkout"
        );

        // A network-path reference: `join` yields http://evil.example.com/…
        let u =
            replay_url(base, "//evil.example.com/api/checkout").expect("must still produce a URL");
        assert_eq!(
            u.host_str(),
            Some("127.0.0.1"),
            "a `//host/x` path must NEVER move the replay to that host: {u}"
        );
        assert_eq!(u.origin(), origin);
        assert_eq!(
            u.as_str(),
            "http://127.0.0.1:7150//evil.example.com/api/checkout"
        );

        // An absolute reference: `join` yields http://evil.example.com/x
        let u = replay_url(base, "http://evil.example.com/x").expect("must still produce a URL");
        assert_eq!(
            u.host_str(),
            Some("127.0.0.1"),
            "an absolute-form path must NEVER move the replay to that host: {u}"
        );
        assert_eq!(u.origin(), origin);
    }

    #[test]
    fn replay_url_keeps_the_captured_query_string_and_drops_check_urls_own() {
        // `Url::set_path` percent-encodes `?`, so the query has to be split
        // off and set separately or the replay hits a literal `%3F` path.
        assert_eq!(
            replay_url(
                "http://127.0.0.1:7150/health?probe=1#frag",
                "/api/checkout?coupon=SAVE10"
            )
            .unwrap()
            .as_str(),
            "http://127.0.0.1:7150/api/checkout?coupon=SAVE10"
        );
        // No captured query → check_url's own query/fragment must not leak in.
        assert_eq!(
            replay_url("http://127.0.0.1:7150/health?probe=1#frag", "/api/checkout")
                .unwrap()
                .as_str(),
            "http://127.0.0.1:7150/api/checkout"
        );
    }

    #[test]
    fn replay_url_none_when_check_url_is_unparseable() {
        assert_eq!(replay_url("not a url at all", "/api/checkout"), None);
    }

    /// The same pin driven through the real `ship()` path with a real HTTP
    /// server: the claim that lands in the append-only record must name the
    /// deployment's own origin, never the path-supplied host.
    #[tokio::test]
    async fn ship_replay_never_leaves_the_check_url_origin_end_to_end() {
        let repo_dir = init_repo();
        let repo = repo_dir.path();
        let repair_sha = commit(repo, "c1", "x");

        let record_path = repo.join("record.jsonl");
        append_repair_ready(&record_path, "f1", &repair_sha, 1);
        // A hostile captured path, exactly as Node would preserve it in `req.url`.
        append_repair_context_with_path(&record_path, "f1", "//evil.example.com/api/checkout", 1);

        let port = 7154;
        let server = spawn_check_server(port, 200).await;
        let scripts_dir = tempfile::tempdir().unwrap();
        let log_path = scripts_dir.path().join("deploy.log");
        let script = write_fake_deploy_script(scripts_dir.path(), &log_path);
        let template = format!("{} {{sha}}", script.display());
        let check_url = format!("http://127.0.0.1:{port}/health");

        let outcome = ship(&record_path, repo, "f1", &template, Some(&check_url))
            .await
            .expect("ship must succeed");
        server.abort();

        assert!(
            !outcome
                .claims
                .iter()
                .any(|c| c.text.contains("//evil.example.com/api")
                    && !c.text.contains(&format!("127.0.0.1:{port}"))),
            "no claim may name evil.example.com as the host the replay went to: {:?}",
            outcome.claims
        );
        assert!(
            outcome.claims.iter().any(|c| c.text.contains(&format!("http://127.0.0.1:{port}//evil.example.com/api/checkout"))),
            "the replay must have gone to the DEPLOYMENT's origin with the captured path attached: {:?}",
            outcome.claims
        );
        // The server has no such route, so this is a 404 — which must not be
        // verified either (I3), and definitely must not be verified against a
        // host that isn't the deployment.
        assert!(
            !outcome.claims.iter().any(
                |c| c.provenance == Provenance::Verified && c.text.contains("evil.example.com")
            ),
            "a replay involving an attacker-influenced path must never earn a verified chip: {:?}",
            outcome.claims
        );
    }

    // -- N3: a failed `shipped` record append must be said out loud ------------

    /// Round-2 N3: the append is best-effort by design (a write failure must
    /// not undo an already-completed deploy) — but it was completely SILENT:
    /// `tracing::error!` with no subscriber installed anywhere in the CLI. So
    /// on ENOSPC / read-only FS / EACCES the deploy really happened, `ship()`
    /// returned `Ok`, and the narration printed green `shipped <sha> —
    /// reversible: drums revert <id>` while the record had no `shipped` line
    /// at all, so `drums revert` then refused with `NothingToRevert`: a
    /// printed promise that is false, and a compliance record diverged from
    /// production with no signal anywhere.
    #[tokio::test]
    #[cfg(unix)]
    async fn ship_says_so_when_the_shipped_record_line_cannot_be_written() {
        use std::os::unix::fs::PermissionsExt;
        let repo_dir = init_repo();
        let repo = repo_dir.path();
        let repair_sha = commit(repo, "c1", "x");
        let record_path = repo.join("record.jsonl");
        append_repair_ready(&record_path, "f1", &repair_sha, 1);

        let scripts_dir = tempfile::tempdir().unwrap();
        let log_path = scripts_dir.path().join("deploy.log");
        let script = write_fake_deploy_script(scripts_dir.path(), &log_path);
        let template = format!("{} {{sha}}", script.display());

        // Readable (so the repair_ready lookup still works) but not
        // writable — the append will fail with EACCES.
        std::fs::set_permissions(&record_path, std::fs::Permissions::from_mode(0o444)).unwrap();
        let outcome = ship(&record_path, repo, "f1", &template, None)
            .await
            .expect("the deploy really happened — ship must NOT report failure");
        let _ = std::fs::set_permissions(&record_path, std::fs::Permissions::from_mode(0o644));

        let record = std::fs::read_to_string(&record_path).unwrap();
        assert!(
            !record.contains("\"kind\":\"shipped\""),
            "precondition: the append really did fail: {record}"
        );

        assert!(
            outcome
                .claims
                .iter()
                .any(|c| c.text.contains("record line could not be written")
                    && c.provenance == Provenance::Unresolved),
            "a failed record append must become an honest unresolved claim naming it: {:?}",
            outcome.claims
        );
        assert!(
            outcome
                .claims
                .iter()
                .any(|c| c.text.contains("drums revert f1") && c.text.contains("will not find")),
            "the claim must say what the missing line costs the user: {:?}",
            outcome.claims
        );

        let narration = narrate_shipped_styled(&outcome, &render_ctx());
        assert!(
            !narration.contains(&format!("reversible: {BOLD}drums revert f1")),
            "narration must not promise reversibility for a ship the record has no line for — `drums revert` would refuse with NothingToRevert:\n{narration}"
        );
        assert!(
            !narration.contains("reversible"),
            "not even an unstyled reversibility promise:\n{narration}"
        );
        assert!(
            narration.contains("not recorded"),
            "narration must name the divergence:\n{narration}"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn revert_says_so_when_the_reverted_record_line_cannot_be_written() {
        use std::os::unix::fs::PermissionsExt;
        let repo_dir = init_repo();
        let repo = repo_dir.path();
        let sha_a = commit(repo, "deploy A", "a");
        let sha_b = commit(repo, "deploy B", "b");
        let repair_sha = commit(repo, "repair", "b-fixed");

        let record_path = repo.join("record.jsonl");
        append_deploy(&record_path, &sha_a, 1_000);
        append_deploy(&record_path, &sha_b, 2_000);
        append_repair_ready(&record_path, "f1", &repair_sha, 3_000);
        engine_record::append(
            &record_path,
            "shipped",
            &ShipOutcome {
                failure_id: "f1".into(),
                repair_sha: repair_sha.clone(),
                action: "shipped".into(),
                deploy_cmd: "x".into(),
                claims: vec![],
            },
            4_000,
        )
        .unwrap();

        let scripts_dir = tempfile::tempdir().unwrap();
        let log_path = scripts_dir.path().join("deploy.log");
        let script = write_fake_deploy_script(scripts_dir.path(), &log_path);
        let template = format!("{} {{sha}}", script.display());

        std::fs::set_permissions(&record_path, std::fs::Permissions::from_mode(0o444)).unwrap();
        let outcome = revert(&record_path, repo, "f1", &template, None)
            .await
            .expect("the rollback deploy really happened — revert must NOT report failure");
        let _ = std::fs::set_permissions(&record_path, std::fs::Permissions::from_mode(0o644));

        assert!(
            outcome
                .claims
                .iter()
                .any(|c| c.text.contains("record line could not be written")
                    && c.provenance == Provenance::Unresolved),
            "{:?}",
            outcome.claims
        );
        assert!(record_write_failed(&outcome));
    }

    #[tokio::test]
    async fn a_successful_ship_carries_no_record_write_failure_claim_and_promises_reversibility() {
        let repo_dir = init_repo();
        let repo = repo_dir.path();
        let repair_sha = commit(repo, "c1", "x");
        let record_path = repo.join("record.jsonl");
        append_repair_ready(&record_path, "f1", &repair_sha, 1);

        let scripts_dir = tempfile::tempdir().unwrap();
        let log_path = scripts_dir.path().join("deploy.log");
        let script = write_fake_deploy_script(scripts_dir.path(), &log_path);
        let template = format!("{} {{sha}}", script.display());

        let outcome = ship(&record_path, repo, "f1", &template, None)
            .await
            .expect("ship must succeed");
        assert!(!record_write_failed(&outcome), "{:?}", outcome.claims);
        let narration = narrate_shipped_styled(&outcome, &render_ctx());
        assert!(
            narration.contains(&format!("reversible: {BOLD}drums revert f1")),
            "{narration}"
        );
        assert!(!narration.contains("not recorded"), "{narration}");
        // The record really does carry the line the promise depends on.
        assert!(std::fs::read_to_string(&record_path)
            .unwrap()
            .contains("\"kind\":\"shipped\""));
    }

    // -- I1: record IO errors must refuse honestly, never panic ----------------

    #[tokio::test]
    async fn ship_refuses_honestly_instead_of_panicking_when_the_record_path_is_a_directory() {
        let repo_dir = init_repo();
        let repo = repo_dir.path();
        let record_path = repo.join("record.jsonl");
        std::fs::create_dir_all(&record_path).unwrap(); // a directory, not a file

        let err = ship(&record_path, repo, "f1", "echo {sha}", None)
            .await
            .expect_err("must refuse, not panic");
        assert!(matches!(&err, ShipError::RecordUnreadable(..)), "{err}");
    }

    #[tokio::test]
    async fn revert_refuses_honestly_instead_of_panicking_when_the_record_path_is_a_directory() {
        let repo_dir = init_repo();
        let repo = repo_dir.path();
        let record_path = repo.join("record.jsonl");
        std::fs::create_dir_all(&record_path).unwrap();

        let err = revert(&record_path, repo, "f1", "echo {sha}", None)
            .await
            .expect_err("must refuse, not panic");
        assert!(matches!(&err, ShipError::RecordUnreadable(..)), "{err}");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn revert_refuses_honestly_instead_of_panicking_when_the_record_is_permission_denied() {
        use std::os::unix::fs::PermissionsExt;
        let repo_dir = init_repo();
        let repo = repo_dir.path();
        let record_path = repo.join("record.jsonl");
        std::fs::write(&record_path, "").unwrap();
        std::fs::set_permissions(&record_path, std::fs::Permissions::from_mode(0o000)).unwrap();

        let err = revert(&record_path, repo, "f1", "echo {sha}", None)
            .await
            .expect_err("must refuse, not panic");
        // best-effort restore so the tempdir can be cleaned up
        let _ = std::fs::set_permissions(&record_path, std::fs::Permissions::from_mode(0o644));
        assert!(matches!(&err, ShipError::RecordUnreadable(..)), "{err}");
    }

    // -- F1: a record-sourced sha must never panic or reach a deploy argv ------

    /// Round-4 F1: `narrate_reverted` byte-sliced `repair_sha[..6]`. That
    /// string is READ BACK OUT OF `.drums/record.jsonl` (`deploy_before` →
    /// `ShipOutcome.repair_sha`), and `deploy` lines come from
    /// `POST /v1/deploys`, which validates nothing. A multi-byte byte at
    /// index <6 panicked — AFTER the rollback deploy had already run and the
    /// `reverted` line was already appended, so the operator saw a Rust
    /// stack trace at the exact moment they needed to know the rollback
    /// happened.
    #[test]
    fn narrate_reverted_survives_a_multi_byte_sha_from_the_record() {
        let outcome = ShipOutcome {
            failure_id: "f1".into(),
            // 'é' spans bytes 5..7 — byte index 6 is mid-character.
            repair_sha: "abcdeéf0".into(),
            action: "reverted".into(),
            deploy_cmd: "bash deploy.sh".into(),
            claims: vec![],
        };
        let out = narrate_reverted(&outcome);
        assert!(out.contains("reverted"), "{out}");
    }

    /// The class fix behind F1 (and carried minor 4): a sha that is not
    /// ASCII hex must be refused BEFORE it reaches a deploy argv or a
    /// `git rev-parse {sha}^` argv — not shortened more carefully on the way
    /// out. Nothing may be deployed and no `reverted` line may be written.
    #[tokio::test]
    async fn revert_refuses_a_non_hex_recorded_sha_before_deploying_anything() {
        let repo_dir = init_repo();
        let repo = repo_dir.path();
        let sha_b = commit(repo, "deploy B (attributed)", "b");
        let repair_sha = commit(repo, "repair", "b-fixed");

        let record_path = repo.join("record.jsonl");
        // The rollback TARGET is this line's sha — straight off `POST /v1/deploys`.
        append_deploy(&record_path, "abcdeéf0", 1_000);
        append_deploy(&record_path, &sha_b, 2_000);
        append_repair_ready(&record_path, "f1", &repair_sha, 3_000);
        engine_record::append(
            &record_path,
            "shipped",
            &ShipOutcome {
                failure_id: "f1".into(),
                repair_sha: repair_sha.clone(),
                action: "shipped".into(),
                deploy_cmd: "x".into(),
                claims: vec![],
            },
            4_000,
        )
        .unwrap();

        let scripts_dir = tempfile::tempdir().unwrap();
        let log_path = scripts_dir.path().join("deploy.log");
        let script = write_fake_deploy_script(scripts_dir.path(), &log_path);
        let template = format!("{} {{sha}}", script.display());

        let err = revert(&record_path, repo, "f1", &template, None)
            .await
            .expect_err("must refuse, not deploy garbage and then panic while narrating");
        assert!(
            matches!(&err, ShipError::InvalidRecordedSha(sha, _) if sha == "abcdeéf0"),
            "{err}"
        );
        assert!(
            !log_path.exists(),
            "nothing may have been deployed: {:?}",
            std::fs::read_to_string(&log_path)
        );
        let record = std::fs::read_to_string(&record_path).unwrap();
        assert!(
            !record.contains("\"kind\":\"reverted\""),
            "no reverted line may be written for a refused revert: {record}"
        );
    }

    #[tokio::test]
    async fn ship_refuses_a_non_hex_recorded_repair_sha_before_deploying_anything() {
        let repo_dir = init_repo();
        let repo = repo_dir.path();
        let record_path = repo.join("record.jsonl");
        append_repair_ready(&record_path, "f1", "--upload-pack=touch /tmp/pwned", 1);

        let scripts_dir = tempfile::tempdir().unwrap();
        let log_path = scripts_dir.path().join("deploy.log");
        let script = write_fake_deploy_script(scripts_dir.path(), &log_path);
        let template = format!("{} {{sha}}", script.display());

        let err = ship(&record_path, repo, "f1", &template, None)
            .await
            .expect_err("must refuse a repair sha that is not ASCII hex");
        assert!(matches!(&err, ShipError::InvalidRecordedSha(..)), "{err}");
        assert!(
            !log_path.exists(),
            "nothing may have been deployed: {:?}",
            std::fs::read_to_string(&log_path)
        );
    }

    // -- N1/N2: the post-deploy check_url arms, each pinned ---------------------

    /// Task-3 n2: the only coverage of the `check_url` GET was an `any(…
    /// "returns 200" …)` assertion that the *replay* claim also satisfies, so
    /// the check_url arm itself was effectively unpinned. Pin all three arms —
    /// 200 verified, non-2xx unresolved naming the status, unreachable
    /// unresolved — so the n1 tightening (2xx only, never `< 500`) cannot
    /// silently regress.
    #[tokio::test]
    async fn ship_never_verifies_a_non_200_check_url_and_names_the_status() {
        let repo_dir = init_repo();
        let repo = repo_dir.path();
        let repair_sha = commit(repo, "c1", "x");
        let record_path = repo.join("record.jsonl");
        append_repair_ready(&record_path, "f1", &repair_sha, 1);

        let port = 7160;
        // /health is not defined on this router, so the check_url 404s: a
        // deployment that came up wrong, never a verified health check.
        let server = spawn_check_server_without_health(port).await;
        let scripts_dir = tempfile::tempdir().unwrap();
        let log_path = scripts_dir.path().join("deploy.log");
        let script = write_fake_deploy_script(scripts_dir.path(), &log_path);
        let template = format!("{} {{sha}}", script.display());
        let check_url = format!("http://127.0.0.1:{port}/health");

        let outcome = ship(&record_path, repo, "f1", &template, Some(&check_url))
            .await
            .expect("the deploy itself must still succeed");
        server.abort();

        assert!(
            !outcome
                .claims
                .iter()
                .any(|c| c.provenance == Provenance::Verified),
            "a non-200 health check must leave NO verified claim: {:?}",
            outcome.claims
        );
        assert!(
            outcome.claims.iter().any(|c| c.text.contains(&check_url)
                && c.text.contains("returned 404 after deploy")
                && c.provenance == Provenance::Unresolved),
            "the status must be named, not summarised as a failure: {:?}",
            outcome.claims
        );
    }

    #[tokio::test]
    async fn ship_never_verifies_an_unreachable_check_url() {
        let repo_dir = init_repo();
        let repo = repo_dir.path();
        let repair_sha = commit(repo, "c1", "x");
        let record_path = repo.join("record.jsonl");
        append_repair_ready(&record_path, "f1", &repair_sha, 1);

        let scripts_dir = tempfile::tempdir().unwrap();
        let log_path = scripts_dir.path().join("deploy.log");
        let script = write_fake_deploy_script(scripts_dir.path(), &log_path);
        let template = format!("{} {{sha}}", script.display());

        // Port 1 on loopback: nothing is listening, connection refused.
        let outcome = ship(
            &record_path,
            repo,
            "f1",
            &template,
            Some("http://127.0.0.1:1/health"),
        )
        .await
        .expect("the deploy itself must still succeed");
        assert!(
            !outcome
                .claims
                .iter()
                .any(|c| c.provenance == Provenance::Verified),
            "{:?}",
            outcome.claims
        );
        assert!(
            outcome
                .claims
                .iter()
                .any(|c| c.text.contains("could not check")
                    && c.text.contains("127.0.0.1:1/health")
                    && c.provenance == Provenance::Unresolved),
            "an unreachable deployment must be said out loud, never treated as healthy: {:?}",
            outcome.claims
        );
    }

    // -- M-p: the recorded deploy_cmd must be an unambiguous reconstruction ----

    /// Round-4 M-p: `deploy_cmd` is recorded as `argv.join(" ")`, which is
    /// lossy exactly where it matters — an argv element containing a space
    /// (a repo path like `/Users/x/my repo`) reads back as two arguments, so
    /// the compliance record's "what we ran" cannot be replayed or audited.
    #[test]
    fn recorded_deploy_cmd_keeps_argv_boundaries_unambiguous() {
        let argv = vec![
            "/opt/my scripts/deploy.sh".to_string(),
            "abc123".to_string(),
            "--repo=/Users/x/my repo".to_string(),
        ];
        let recorded = render_argv(&argv);
        assert!(
            recorded.contains("'/opt/my scripts/deploy.sh'"),
            "an element with a space must be quoted: {recorded}"
        );
        assert!(recorded.contains("'--repo=/Users/x/my repo'"), "{recorded}");
        assert!(
            recorded.contains(" abc123"),
            "an element needing no quoting must stay bare: {recorded}"
        );
        // A single quote inside an element must not break out of the quoting.
        assert_eq!(render_argv(&["it's".to_string()]), r#"'it'\''s'"#);
    }

    // -- carried minor 5: a failing deploy's own output must be surfaced -------

    /// The drained stdout was discarded, so a deploy script that reports its
    /// problem on stdout (ordinary: `set -e` shell scripts, npm lifecycle
    /// output, `kubectl`) failed with an EMPTY reason string.
    #[tokio::test]
    async fn deploy_failure_surfaces_stdout_when_stderr_is_empty() {
        let repo_dir = init_repo();
        let repo = repo_dir.path();
        let repair_sha = commit(repo, "c1", "x");
        let record_path = repo.join("record.jsonl");
        append_repair_ready(&record_path, "f1", &repair_sha, 1);

        let scripts_dir = tempfile::tempdir().unwrap();
        let script = scripts_dir.path().join("noisy-fail.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\necho 'ERROR: cluster credentials expired'\nexit 3\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let err = ship(
            &record_path,
            repo,
            "f1",
            &format!("{} {{sha}}", script.display()),
            None,
        )
        .await
        .expect_err("a failing deploy must refuse");
        let text = err.to_string();
        assert!(
            text.contains("cluster credentials expired"),
            "the deploy command's own reason must reach the operator: {text}"
        );
    }

    // -- carried minor 1: one unreadable line must not hide a good repair ------

    #[test]
    fn latest_repair_ready_falls_back_when_the_newest_line_is_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let record_path = dir.path().join("record.jsonl");
        append_repair_ready(&record_path, "f1", "abc123", 1_000);
        // A newer `repair_ready` line for the same failure that does NOT
        // deserialize into `Repair` (a forward-incompatible or truncated
        // write). `rfind` stopped at it and `.ok()` turned it into None, so
        // the whole command reported "no repair_ready" — misattributing a
        // parse problem to the repair never having been produced.
        engine_record::append(
            &record_path,
            "repair_ready",
            &serde_json::json!({ "failure_id": "f1", "unexpected": true }),
            2_000,
        )
        .unwrap();

        let record = read_record(&record_path).unwrap();
        let repair = latest_repair_ready(&record, "f1")
            .expect("must fall back to the newest line that DOES deserialize");
        assert_eq!(repair.sha, "abc123");
    }

    // -- carried minor 2: a re-deployed sha resolves to its LAST occurrence ----

    #[test]
    fn deploy_before_uses_the_last_occurrence_of_a_redeployed_sha() {
        let dir = tempfile::tempdir().unwrap();
        let record_path = dir.path().join("record.jsonl");
        // A rollback re-deploys an older sha, so shas legitimately repeat.
        append_deploy(&record_path, "aaa111", 1_000);
        append_deploy(&record_path, "bbb222", 2_000);
        append_deploy(&record_path, "ccc333", 3_000);
        append_deploy(&record_path, "bbb222", 4_000); // re-deployed (e.g. a rollback)

        let record = read_record(&record_path).unwrap();
        assert_eq!(
            deploy_before(&record, "bbb222").as_deref(),
            Some("ccc333"),
            "the deploy that preceded the sha IN PRODUCTION is the one before its LAST occurrence, not its first"
        );
    }

    // -- narration --------------------------------------------------------------

    #[test]
    fn narrate_shipped_carries_chips_and_reversibility() {
        let outcome = ShipOutcome {
            failure_id: "f1".into(),
            repair_sha: "deadbeef00".into(),
            action: "shipped".into(),
            deploy_cmd: "bash deploy.sh".into(),
            claims: vec![Claim {
                text: "http://x/health returns 200".into(),
                provenance: Provenance::Verified,
            }],
        };
        let out = narrate_shipped_styled(&outcome, &render_ctx());
        assert!(out.contains("shipped"));
        assert!(out.contains("[verified]"));
        assert!(out.contains("drums revert f1"));
    }

    // -- m5: the footer's colour/wording must track the WEAKEST claim ----------

    /// Arm 1: at least one Verified claim on the outcome — the pre-existing
    /// green "shipped" headline, unchanged.
    #[test]
    fn shipped_footer_is_green_shipped_when_a_claim_is_verified() {
        let outcome = ShipOutcome {
            failure_id: "f1".into(),
            repair_sha: "deadbeef00".into(),
            action: "shipped".into(),
            deploy_cmd: "bash deploy.sh".into(),
            claims: vec![Claim {
                text: "http://x/health returns 200".into(),
                provenance: Provenance::Verified,
            }],
        };
        let out = shipped_footer_styled(&outcome, &render_ctx());
        assert!(out.contains(&format!("{GREEN}shipped{RESET}")), "{out}");
        assert!(
            !out.contains("unverified"),
            "a genuinely verified ship must not be hedged: {out}"
        );
    }

    /// Arm 2 (RED FIRST live repro, trust-hardening review m5): a 404
    /// check-url and a still-failing replay produce two `Unresolved` claims —
    /// zero Verified claims about the post-deploy state — and the footer used
    /// to print the exact same `{GREEN}shipped{RESET}` headline as the fully
    /// verified case, making the two visually indistinguishable. The footer
    /// must now say so: no success colour on "shipped", and the word
    /// "unverified" in the headline itself, not only in the claim lines above
    /// it.
    #[test]
    fn shipped_footer_says_unverified_and_drops_the_success_colour_when_no_claim_is_verified() {
        let outcome = ShipOutcome {
            failure_id: "f1".into(),
            repair_sha: "deadbeef00".into(),
            action: "shipped".into(),
            deploy_cmd: "bash deploy.sh".into(),
            claims: vec![
                Claim {
                    text: "http://x/health returned 404 after deploy".into(),
                    provenance: Provenance::Unresolved,
                },
                Claim {
                    text: "replayed the originally failing request: still returns 501".into(),
                    provenance: Provenance::Unresolved,
                },
            ],
        };
        let out = shipped_footer_styled(&outcome, &render_ctx());
        assert!(
            out.contains("unverified"),
            "the headline must say the ship is unverified: {out}"
        );
        assert!(
            !out.contains(&format!("{GREEN}shipped{RESET}")),
            "an all-unresolved outcome must not print the bare success-coloured \"shipped\": {out}"
        );
        // Still honest about reversibility — the record line itself did write.
        assert!(out.contains("drums revert f1"), "{out}");
    }

    /// Same two arms, for the TUI's plain-text `ready_line`.
    #[test]
    fn shipped_ready_line_says_unverified_when_no_claim_is_verified() {
        let verified = ShipOutcome {
            failure_id: "f1".into(),
            repair_sha: "deadbeef00".into(),
            action: "shipped".into(),
            deploy_cmd: "bash deploy.sh".into(),
            claims: vec![Claim {
                text: "http://x/health returns 200".into(),
                provenance: Provenance::Verified,
            }],
        };
        assert!(!shipped_ready_line(&verified, &render_ctx()).contains("unverified"));

        let unverified = ShipOutcome {
            failure_id: "f1".into(),
            repair_sha: "deadbeef00".into(),
            action: "shipped".into(),
            deploy_cmd: "bash deploy.sh".into(),
            claims: vec![Claim {
                text: "http://x/health returned 404 after deploy".into(),
                provenance: Provenance::Unresolved,
            }],
        };
        assert!(shipped_ready_line(&unverified, &render_ctx()).contains("unverified"));
    }

    /// The record-write-failure claim `append_outcome` adds is itself always
    /// `Unresolved` — it must never, on its own, count as evidence the ship
    /// was verified.
    #[test]
    fn the_record_write_failure_claim_alone_does_not_count_as_verified() {
        let outcome = ShipOutcome {
            failure_id: "f1".into(),
            repair_sha: "deadbeef00".into(),
            action: "shipped".into(),
            deploy_cmd: "bash deploy.sh".into(),
            claims: vec![Claim {
                text: format!("{RECORD_WRITE_FAILURE}: EACCES"),
                provenance: Provenance::Unresolved,
            }],
        };
        assert!(!has_verified_claim(&outcome));
    }

    /// F6 (Task-3 review round 4): the standalone `drums ship` process prints
    /// this footer too, and `drums revert <id>` on its own exits 2 —
    /// `Commands::Revert`'s `--deploy-cmd` is a REQUIRED clap arg and its
    /// record path comes from `--repo`/cwd. The footer must print the flags
    /// the process it belongs to already knows.
    #[test]
    fn shipped_footer_prints_a_revert_command_that_carries_the_required_flags() {
        let outcome = ShipOutcome {
            failure_id: "f1".into(),
            repair_sha: "deadbeef00".into(),
            action: "shipped".into(),
            deploy_cmd: "bash deploy.sh deadbeef00".into(),
            claims: vec![],
        };
        let out = shipped_footer_styled(&outcome, &render_ctx());
        assert!(
            out.contains("drums revert f1 --deploy-cmd 'bash deploy.sh {sha}' --repo '/srv/shop'"),
            "the reversibility promise must be a runnable command: {out}"
        );
    }

    /// The `not recorded` arm names the same command to explain what will
    /// refuse — it must name the real one, not a shape that would have exited
    /// 2 before ever reaching the record check.
    #[test]
    fn the_not_recorded_footer_names_the_same_runnable_revert_command() {
        let outcome = ShipOutcome {
            failure_id: "f1".into(),
            repair_sha: "deadbeef00".into(),
            action: "shipped".into(),
            deploy_cmd: "bash deploy.sh deadbeef00".into(),
            claims: vec![Claim {
                text: format!("{RECORD_WRITE_FAILURE}: EACCES"),
                provenance: Provenance::Unresolved,
            }],
        };
        let out = shipped_footer_styled(&outcome, &render_ctx());
        assert!(out.contains("not recorded"), "{out}");
        assert!(
            out.contains("drums revert f1 --deploy-cmd 'bash deploy.sh {sha}' --repo '/srv/shop'"),
            "even the refusing command must be the runnable one: {out}"
        );
    }

    #[test]
    fn narrate_reverted_carries_chips() {
        let outcome = ShipOutcome {
            failure_id: "f1".into(),
            repair_sha: "cafef00d00".into(),
            action: "reverted".into(),
            deploy_cmd: "bash deploy.sh".into(),
            claims: vec![Claim {
                text: "deploy command ran; no post-deploy check configured".into(),
                provenance: Provenance::Unresolved,
            }],
        };
        let out = narrate_reverted(&outcome);
        assert!(out.contains("reverted"));
        assert!(out.contains("[unresolved]"));
    }

    #[test]
    fn narrate_error_strips_control_bytes_and_newlines_from_the_reason() {
        // A deploy command prints whatever it likes, and its output reaches
        // ShipError. Without a sanitizer this row could repaint the terminal
        // and forge an additional claim row carrying its own chip.
        let hostile = ShipError::NoRepairReady(
            "f1\u{1b}[2K\r   \u{2500} deploy verified [verified]".to_string(),
        );
        let out = narrate_error_styled("ship", &hostile);
        // The row legitimately contains ESC for its OWN colours (DIM, RESET,
        // RED, RESET), so count them rather than banning ESC outright. Four
        // means every ESC present is one this function emitted and none came
        // from the error string. The payload's `[2K` survives as inert text,
        // which is cosmetic — the byte that made it an escape sequence is gone.
        assert_eq!(
            out.matches('\u{1b}').count(),
            4,
            "an ESC from the error string reached the terminal: {out:?}"
        );
        assert!(!out.contains('\r'), "CR reached the terminal: {out:?}");
        assert_eq!(
            out.matches('\n').count(),
            1,
            "exactly one newline — the trailing one. A second would put the chip \
             on a different line from its claim and let injected text render as \
             its own row: {out:?}"
        );
        assert!(out.trim_end().ends_with("[unresolved]"), "{out:?}");
    }

    #[test]
    fn narrate_error_carries_the_unresolved_chip() {
        let out = narrate_error_styled("ship", &ShipError::NoRepairReady("f1".to_string()));
        assert!(out.contains("ship failed"));
        assert!(out.contains("[unresolved]"));
        assert!(out.contains("f1"));
    }
}
