//! Record sync — the opt-in courier that mirrors `.drums/record.jsonl` to
//! the hosted plane, so the console can render the Bet Feed for the team.
//!
//! # The privacy invariant (pinned by test)
//!
//! Sync sends the record lines EXACTLY as stored — byte for byte, with no
//! new fields derived from requests — because redaction already happened at
//! capture time (`engine_record::redact_body` masks sensitive values before
//! a line is ever appended). The record is stored redacted, so what leaves
//! this machine is redacted; this module never re-derives a line and could
//! not un-redact one if it tried. [`WireLine::line`] is a
//! [`serde_json::value::RawValue`] holding the stored bytes precisely so
//! re-serialisation cannot even reorder a key, let alone add one. The test
//! `the_wire_carries_the_stored_line_byte_for_byte_and_nothing_unredacted`
//! holds this sentence to the payload that actually goes out.
//!
//! # Consent
//!
//! The record is local-first. Nothing here runs unless
//! `<repo>/.drums/config.toml` says `sync_record = true` (see
//! [`crate::config::Config::sync_record`] for the trust terms, stated in the
//! file itself) AND this machine has a `drums login` credential. The flag
//! without the credential is narrated — by `drums doctor`'s `sync` check and
//! by the watch banner — never silently treated as either state.
//!
//! # Server-authoritative resume
//!
//! Every pass starts by asking the plane what it already holds
//! (`GET {app_base}/api/record/sync?repo=<slug>` → `server_max`, `-1` when
//! nothing has ever synced) and sends only lines with `seq > server_max`, in
//! batches of [`BATCH`]. `seq` is the 0-based index of the decoded line in
//! file order — the record is append-only and a torn line can never heal
//! back into a decoded one, so an index assigned once never moves. On a 409
//! (the plane sees a gap) the pass re-anchors ONCE from the `server_max` the
//! refusal carried; any error stops the pass. The engine logs a failed pass
//! via `tracing::warn` and simply tries again next tick — never within a
//! pass — because sync must never block or fail the loop it rides on: the
//! local record is the source of truth and the hosted copy is a courtesy
//! mirror, exactly the discipline `notify.rs` holds for Slack.
//!
//! # The credential and the base URL
//!
//! Both come from the ONE auth path the CLI already has: the bearer token is
//! `drums login`'s (`~/.drums/credentials.toml`, or `$DRUMS_HOME`), and the
//! app base is [`crate::login::console_url`] (`DRUMS_CONSOLE_URL` override,
//! default `https://app.drums.sh`) — the same resolution `dispatch.rs` uses,
//! including its refusal of a credential minted against a different console.
//! [`RecordSync`] has no `Debug` derive for the same reason
//! `dispatch::RemoteRepairs` has none: a derived `Debug` is how a token ends
//! up in a tracing field or a panic message.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use serde_json::value::RawValue;

/// The one route, both verbs.
pub const SYNC_PATH: &str = "/api/record/sync";

/// Batch cap. 500 lines per `POST` keeps a first sync of a long-lived record
/// from becoming one giant request a proxy somewhere refuses.
pub const BATCH: usize = 500;

/// Per-request bound — same figure as `dispatch::DISPATCH_TIMEOUT`, for the
/// same reason: a plane that has gone away must not hold the loop open.
const SYNC_TIMEOUT: Duration = Duration::from_secs(20);

/// The refusal a 401 produces, wherever it happens in a pass.
pub const NOT_ACCEPTED: &str =
    "the hosted plane did not accept this machine's sign-in — run `drums login` again";

/// One record line on the wire: `{"seq":…,"kind":…,"recorded_at_ms":…,"line":…}`.
///
/// `line` is the stored line's own bytes ([`RawValue`]), not a re-parse —
/// see the module doc's privacy invariant.
#[derive(Serialize)]
pub struct WireLine {
    /// 0-based index of this decoded line in file order. Append-only, so
    /// stable: see the module doc.
    pub seq: u64,
    pub kind: String,
    pub recorded_at_ms: u64,
    pub line: Box<RawValue>,
}

/// The body `POST /api/record/sync` reads, field for field against the
/// pinned contract the console is built to.
#[derive(Serialize)]
pub struct BatchBody<'a> {
    pub repo: &'a str,
    pub from_seq: u64,
    pub lines: &'a [WireLine],
}

/// What one pass accomplished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Synced {
    /// How many lines the plane newly acknowledged this pass.
    pub sent: usize,
    /// The highest seq the plane holds after this pass (`-1` when it holds
    /// nothing and there was nothing to send).
    pub through: i64,
}

// -- naming the repo ---------------------------------------------------------

/// The hosted plane's name for this repo: `org/repo` parsed from the git
/// `origin` remote when one exists, else the repo directory's basename — a
/// repo with no remote yet still needs SOME stable name, and the basename is
/// the one a human would use.
pub fn repo_slug(repo: &Path) -> String {
    slug_for(crate::dispatch::origin_url(repo).as_deref(), repo)
}

/// The pure half of [`repo_slug`]: `origin` is whatever
/// `git remote get-url origin` printed (`None` when there is no remote).
/// Remote parsing is [`crate::dispatch::parse_slug`] — one home for the
/// https/ssh/scp-like/`.git` spellings — and anything it cannot read as
/// `org/repo` falls back to the basename rather than a guessed slug.
pub fn slug_for(origin: Option<&str>, repo: &Path) -> String {
    origin
        .and_then(crate::dispatch::parse_slug)
        .unwrap_or_else(|| {
            repo.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("repo")
                .to_string()
        })
}

// -- reading the record for the wire -----------------------------------------

/// Every decoded record line with its stable seq, in file order.
///
/// Mirrors `engine_record::read_all`'s decoding rules exactly (raw bytes
/// split on `\n`; a torn, non-UTF-8, non-JSON or kind-less line is skipped)
/// — but keeps the stored BYTES of each surviving line rather than its
/// parse, because the wire carries the bytes. A missing file reads as empty:
/// nothing recorded yet is a normal state, not an error.
pub fn read_wire_lines(path: &Path) -> io::Result<Vec<WireLine>> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut lines = Vec::new();
    for raw in bytes.split(|&b| b == b'\n') {
        if raw.is_empty() {
            continue;
        }
        let Ok(text) = std::str::from_utf8(raw) else {
            continue;
        };
        if text.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
            continue;
        };
        let Some(kind) = value.get("kind").and_then(|k| k.as_str()) else {
            continue;
        };
        // Just parsed as JSON above, so `RawValue` accepts it; a skip here
        // (rather than a panic) keeps the two judgements consistent anyway.
        let Ok(line) = RawValue::from_string(text.to_string()) else {
            continue;
        };
        lines.push(WireLine {
            seq: lines.len() as u64,
            kind: kind.to_string(),
            recorded_at_ms: value
                .get("recorded_at_ms")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            line,
        });
    }
    Ok(lines)
}

/// The suffix the plane does not hold yet: every line with
/// `seq > server_max`. `server_max` of `-1` — the plane's "nothing synced" —
/// therefore takes everything, and a plane claiming MORE than this file
/// holds takes nothing rather than having lines invented for it.
pub fn pending(lines: &[WireLine], server_max: i64) -> &[WireLine] {
    let start = lines.partition_point(|l| (l.seq as i64) <= server_max);
    &lines[start..]
}

/// Split what's pending into wire bodies of at most [`BATCH`] lines, each
/// carrying its own `from_seq` (its first line's seq).
pub fn batch_bodies<'a>(repo: &'a str, todo: &'a [WireLine]) -> Vec<BatchBody<'a>> {
    todo.chunks(BATCH)
        .map(|chunk| BatchBody {
            repo,
            from_seq: chunk[0].seq,
            lines: chunk,
        })
        .collect()
}

// -- interpreting the plane's answers ----------------------------------------

/// Decide what `GET /api/record/sync` answered: the anchor, or a refusal.
///
/// Split out and pure, exactly like `login::interpret_poll` and
/// `dispatch::interpret`, so every branch is exercisable without a server.
pub fn interpret_head(status: u16, body: &serde_json::Value) -> Result<i64, String> {
    match status {
        200..=299 => body.get("server_max").and_then(|v| v.as_i64()).ok_or_else(|| {
            "the hosted plane answered without a server_max — there is nothing to anchor a sync on"
                .to_string()
        }),
        401 => Err(NOT_ACCEPTED.to_string()),
        other => Err(format!("the hosted plane answered HTTP {other}")),
    }
}

/// What `POST /api/record/sync` said about one batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Posted {
    /// Accepted; the plane now holds everything through this seq.
    Through(i64),
    /// 409 — the batch's `from_seq` skipped past what the plane holds
    /// (`from_seq > server_max + 1`). Re-anchor from `server_max + 1`.
    Gap { server_max: i64 },
}

/// Decide what a `POST /api/record/sync` response means.
pub fn interpret_post(status: u16, body: &serde_json::Value) -> Result<Posted, String> {
    match status {
        200..=299 => body
            .get("synced_through")
            .and_then(|v| v.as_i64())
            .map(Posted::Through)
            .ok_or_else(|| {
                "the hosted plane accepted a batch but sent no synced_through — refusing to guess \
                 what landed"
                    .to_string()
            }),
        409 => body
            .get("server_max")
            .and_then(|v| v.as_i64())
            .map(|server_max| Posted::Gap { server_max })
            .ok_or_else(|| {
                "the hosted plane reported a gap without saying where it ends (409 with no \
                 server_max)"
                    .to_string()
            }),
        401 => Err(NOT_ACCEPTED.to_string()),
        other => Err(format!("the hosted plane answered HTTP {other}")),
    }
}

// -- the courier -------------------------------------------------------------

/// Everything a sync pass needs, resolved once.
///
/// Deliberately NO `#[derive(Debug)]` — this struct holds a bearer token
/// (see the module doc).
pub struct RecordSync {
    app_base: String,
    /// Never printed, never logged, never included in an error.
    token: String,
    repo_slug: String,
    record_path: PathBuf,
    client: reqwest::Client,
}

impl RecordSync {
    /// Build from the credentials `drums login` stored and the app base
    /// [`crate::login::console_url`] resolves — the exact machinery
    /// `dispatch::RemoteRepairs::from_login` uses, refusals included.
    /// Every failure names its fix, because the caller's next line is
    /// either a banner or a refusal.
    pub fn from_login(repo: &Path, record_path: PathBuf) -> Result<Self, String> {
        let creds = crate::login::load()?
            .ok_or_else(|| "this machine is not signed in — run `drums login` first".to_string())?;
        let app_base = crate::login::console_url();
        // A token minted against a staging console is not a token for
        // production — same named refusal as dispatch, for the same reason.
        if !creds.console_url.is_empty() && creds.console_url != app_base {
            return Err(format!(
                "the stored credential is for {} but this run would talk to {app_base} — run \
                 `drums login` again",
                creds.console_url
            ));
        }
        Self::new(app_base, creds.token, repo_slug(repo), record_path)
    }

    /// For tests and overrides: everything explicit, nothing read from disk.
    pub fn new(
        app_base: impl Into<String>,
        token: impl Into<String>,
        repo_slug: impl Into<String>,
        record_path: PathBuf,
    ) -> Result<Self, String> {
        Ok(Self {
            app_base: app_base.into().trim_end_matches('/').to_string(),
            token: token.into(),
            repo_slug: repo_slug.into(),
            record_path,
            client: reqwest::Client::builder()
                .timeout(SYNC_TIMEOUT)
                .build()
                .map_err(|e| format!("could not build an HTTP client: {e}"))?,
        })
    }

    pub fn repo_slug(&self) -> &str {
        &self.repo_slug
    }

    pub fn app_base(&self) -> &str {
        &self.app_base
    }

    /// One pass: ask the plane where it is, send what it lacks, stop on any
    /// error. Never retries within a pass — a failure is returned for the
    /// caller to narrate, and the NEXT pass re-anchors from scratch anyway,
    /// which is what makes stopping early always safe.
    pub async fn pass(&self) -> Result<Synced, String> {
        let url = format!("{}{SYNC_PATH}", self.app_base);
        let head = self
            .client
            .get(&url)
            .query(&[("repo", self.repo_slug.as_str())])
            .bearer_auth(&self.token)
            .send()
            .await
            // The error is over a URL we built; the token travels in a header
            // reqwest does not format here, so it cannot reach this string.
            .map_err(|e| format!("could not reach {}: {e}", self.app_base))?;
        let status = head.status().as_u16();
        let body: serde_json::Value = head.json().await.unwrap_or(serde_json::Value::Null);
        let mut anchor = interpret_head(status, &body)?;

        let lines = read_wire_lines(&self.record_path)
            .map_err(|e| format!("could not read {}: {e}", self.record_path.display()))?;

        let mut sent = 0usize;
        let mut reanchored = false;
        loop {
            let todo = pending(&lines, anchor);
            if todo.is_empty() {
                return Ok(Synced {
                    sent,
                    through: anchor,
                });
            }
            let chunk = &todo[..todo.len().min(BATCH)];
            let batch = BatchBody {
                repo: &self.repo_slug,
                from_seq: chunk[0].seq,
                lines: chunk,
            };
            let resp = self
                .client
                .post(&url)
                .bearer_auth(&self.token)
                .json(&batch)
                .send()
                .await
                .map_err(|e| format!("could not reach {}: {e}", self.app_base))?;
            let status = resp.status().as_u16();
            let parsed: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
            match interpret_post(status, &parsed)? {
                Posted::Through(through) => {
                    // The plane must move forward or this loop would resend
                    // the same chunk forever — an answer that stands still is
                    // treated as the error it is.
                    if through <= anchor {
                        return Err(format!(
                            "the hosted plane accepted a batch but did not advance \
                             (synced_through {through}, anchor already {anchor}) — stopping \
                             rather than resending the same lines"
                        ));
                    }
                    sent += (through - anchor) as usize;
                    anchor = through;
                }
                Posted::Gap { server_max } => {
                    // Once per pass: the 409 carries the true anchor, and one
                    // re-anchor is the contract's own resume path. A second
                    // gap in the same pass means the plane and this loop
                    // disagree about arithmetic, and that is a stop, not a
                    // retry.
                    if reanchored {
                        return Err(
                            "the hosted plane reported a second gap in one pass — stopping; the \
                             next pass re-anchors from scratch"
                                .to_string(),
                        );
                    }
                    reanchored = true;
                    anchor = server_max;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    fn wire_lines(n: usize) -> Vec<WireLine> {
        (0..n)
            .map(|i| WireLine {
                seq: i as u64,
                kind: "widget".into(),
                recorded_at_ms: i as u64,
                line: RawValue::from_string(format!(r#"{{"kind":"widget","recorded_at_ms":{i}}}"#))
                    .unwrap(),
            })
            .collect()
    }

    // -- naming the repo ------------------------------------------------------

    /// Every remote spelling lands on `org/repo`; no remote (and an origin
    /// that is not `org/repo`-shaped at all) falls back to the basename
    /// rather than a guess.
    #[test]
    fn every_remote_spelling_yields_org_repo_and_no_remote_falls_back_to_the_basename() {
        let repo = Path::new("/work/checkouts/shop");
        for url in [
            "https://github.com/acme/api.git",
            "https://github.com/acme/api",
            "git@github.com:acme/api.git",
            "git@github.com:acme/api",
            "ssh://git@github.com/acme/api.git",
        ] {
            assert_eq!(slug_for(Some(url), repo), "acme/api", "{url}");
        }
        assert_eq!(slug_for(None, repo), "shop");
        assert_eq!(
            slug_for(Some("/srv/local/mirror"), repo),
            "shop",
            "an unparseable origin is the basename, never a guessed slug"
        );
    }

    /// The impure wrapper, against a real repo: reads the origin remote when
    /// one exists, basename when nothing does.
    #[test]
    fn repo_slug_reads_the_origin_remote_and_falls_back_without_one() {
        let dir = tempfile::tempdir().unwrap();
        let basename = dir
            .path()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert_eq!(
            repo_slug(dir.path()),
            basename,
            "no git at all → the directory's name"
        );

        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(args)
                .output()
                .expect("git must run");
            assert!(out.status.success(), "git {args:?} failed");
        };
        git(&["init", "-q"]);
        assert_eq!(
            repo_slug(dir.path()),
            basename,
            "a repo with no remote → the directory's name"
        );
        git(&["remote", "add", "origin", "https://github.com/acme/api.git"]);
        assert_eq!(repo_slug(dir.path()), "acme/api");
    }

    // -- seq ------------------------------------------------------------------

    /// `seq` is the 0-based index of the DECODED line in file order — the
    /// same skip rules as `engine_record::read_all`, so the record the
    /// console mirrors is the record every other reader sees.
    #[test]
    fn seq_is_the_zero_based_decoded_index_in_file_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"kind\":\"event\",\"recorded_at_ms\":1,\"service\":\"a\"}\n",
                "{\"kind\":\"deploy\",\"recorded_at_ms\":2,\"sha\":\"abc\"}\n",
                "{\"recorded_at_ms\":3,\"no\":\"kind field\"}\n",
                "{\"kind\":\"torn\",\"recorded_a\n",
                "{\"kind\":\"event\",\"recorded_at_ms\":4,\"service\":\"b\"}\n",
            ),
        )
        .unwrap();
        let lines = read_wire_lines(&path).unwrap();
        assert_eq!(
            lines.len(),
            3,
            "corrupt and kind-less lines are skipped, exactly as read_all skips them"
        );
        assert_eq!(
            (
                lines[0].seq,
                lines[0].kind.as_str(),
                lines[0].recorded_at_ms
            ),
            (0, "event", 1)
        );
        assert_eq!(
            (
                lines[1].seq,
                lines[1].kind.as_str(),
                lines[1].recorded_at_ms
            ),
            (1, "deploy", 2)
        );
        assert_eq!(
            (
                lines[2].seq,
                lines[2].kind.as_str(),
                lines[2].recorded_at_ms
            ),
            (2, "event", 4)
        );
    }

    #[test]
    fn a_missing_record_reads_as_empty_not_as_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_wire_lines(&dir.path().join("record.jsonl"))
            .unwrap()
            .is_empty());
    }

    // -- anchoring and batching -----------------------------------------------

    /// The three anchor positions the plane can answer with: nothing synced
    /// (`-1`) takes everything, a mid anchor takes the suffix, caught-up (or
    /// ahead) takes nothing.
    #[test]
    fn a_fresh_anchor_takes_everything_a_mid_anchor_the_suffix_and_a_caught_up_one_nothing() {
        let lines = wire_lines(5);
        assert_eq!(pending(&lines, -1).len(), 5);

        let suffix = pending(&lines, 2);
        assert_eq!(suffix.len(), 2);
        assert_eq!(suffix[0].seq, 3);

        assert!(pending(&lines, 4).is_empty());
        assert!(
            pending(&lines, 99).is_empty(),
            "a plane claiming more than this file holds gets nothing, never invented lines"
        );
    }

    #[test]
    fn batches_cap_at_500_and_each_carries_its_own_from_seq() {
        let lines = wire_lines(1201);
        let bodies = batch_bodies("acme/api", &lines);
        assert_eq!(bodies.len(), 3);
        assert_eq!((bodies[0].from_seq, bodies[0].lines.len()), (0, 500));
        assert_eq!((bodies[1].from_seq, bodies[1].lines.len()), (500, 500));
        assert_eq!((bodies[2].from_seq, bodies[2].lines.len()), (1000, 201));
        assert_eq!(bodies[0].repo, "acme/api");
    }

    /// A 409 re-anchor is nothing more than `pending` again at the plane's
    /// own max — the pure half of the resume path.
    #[test]
    fn re_anchoring_from_a_409s_server_max_resends_exactly_the_suffix_past_it() {
        let lines = wire_lines(7);
        // The plane said 409 { server_max: 3 }: everything from seq 4 goes again.
        let again = pending(&lines, 3);
        assert_eq!(again.first().map(|l| l.seq), Some(4));
        assert_eq!(again.len(), 3);
    }

    // -- the privacy invariant ------------------------------------------------

    /// THE invariant this module exists to keep: what leaves the machine is
    /// the stored line, byte for byte, with nothing derived from requests
    /// added — redaction happened at capture time, so the stored bytes are
    /// the redacted bytes, and sending exactly them is what makes "it
    /// arrives redacted because it is stored redacted" true.
    #[test]
    fn the_wire_carries_the_stored_line_byte_for_byte_and_nothing_unredacted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        // As the pipeline stores a captured request AFTER capture-time
        // redaction: the body already reads `token=[redacted]`.
        engine_record::append(
            &path,
            "event",
            &json!({
                "service": "shop",
                "occurred_at_ms": 5,
                "request": {
                    "method": "POST",
                    "path": "/login",
                    "body": "user=jo&token=[redacted]"
                }
            }),
            6,
        )
        .unwrap();
        let stored = std::fs::read_to_string(&path).unwrap();
        let stored_line = stored.trim_end();

        let lines = read_wire_lines(&path).unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(
            lines[0].line.get(),
            stored_line,
            "the wire line must be the stored bytes, not a re-serialisation"
        );

        let bodies = batch_bodies("acme/api", &lines);
        let payload = serde_json::to_string(&bodies[0]).unwrap();
        assert!(
            payload.contains(stored_line),
            "the stored line must appear verbatim inside the outgoing payload: {payload}"
        );
        // No unredacted variant anywhere in what goes out: every `token=` in
        // the payload is the redacted marker the record stored.
        for (i, _) in payload.match_indices("token=") {
            assert!(
                payload[i..].starts_with("token=[redacted]"),
                "an unredacted token variant appeared in the payload: {payload}"
            );
        }
        // And no new fields derived from the request: the line in the payload
        // parses back to exactly the stored line's own value, wrapped only in
        // the four contract fields.
        let parsed: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(
            parsed["lines"][0]["line"],
            serde_json::from_str::<serde_json::Value>(stored_line).unwrap()
        );
        assert_eq!(parsed["lines"][0]["seq"], 0);
        assert_eq!(parsed["lines"][0]["kind"], "event");
        assert_eq!(parsed["lines"][0]["recorded_at_ms"], 6);
        assert_eq!(
            parsed["lines"][0].as_object().unwrap().len(),
            4,
            "seq, kind, recorded_at_ms, line — and nothing else: {payload}"
        );
    }

    // -- interpreting answers -------------------------------------------------

    #[test]
    fn the_head_answer_yields_the_anchor_and_names_what_went_wrong_otherwise() {
        assert_eq!(interpret_head(200, &json!({"server_max": -1})), Ok(-1));
        assert_eq!(interpret_head(200, &json!({"server_max": 41})), Ok(41));

        let err = interpret_head(200, &json!({})).expect_err("no server_max is a server bug");
        assert!(err.contains("server_max"), "{err}");

        let err = interpret_head(401, &json!({})).expect_err("401 is a refusal");
        assert!(err.contains("drums login"), "{err}");

        let err = interpret_head(503, &serde_json::Value::Null).expect_err("5xx is a refusal");
        assert!(err.contains("503"), "{err}");
    }

    #[test]
    fn a_post_answer_yields_through_a_409_yields_the_gap_and_a_401_names_drums_login() {
        assert_eq!(
            interpret_post(200, &json!({"synced_through": 12})),
            Ok(Posted::Through(12))
        );
        assert_eq!(
            interpret_post(409, &json!({"server_max": 3})),
            Ok(Posted::Gap { server_max: 3 })
        );

        let err = interpret_post(200, &json!({})).expect_err("no synced_through is a server bug");
        assert!(err.contains("synced_through"), "{err}");

        let err = interpret_post(409, &json!({})).expect_err("a gap with no server_max is useless");
        assert!(err.contains("server_max"), "{err}");

        let err = interpret_post(401, &serde_json::Value::Null).expect_err("401 is a refusal");
        assert!(err.contains("drums login"), "{err}");

        let err = interpret_post(500, &json!({})).expect_err("5xx is a refusal");
        assert!(err.contains("500"), "{err}");
    }

    // -- whole passes, over the wire ------------------------------------------

    /// A plane that answers a fixed script of responses, one connection per
    /// request (`connection: close`), capturing every request it saw — the
    /// multi-request sibling of `account.rs`'s `fake_console`.
    async fn fake_plane(
        responses: Vec<(u16, serde_json::Value)>,
    ) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_writer = seen.clone();
        tokio::spawn(async move {
            for (status, body) in responses {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                // Read one full request: headers, then content-length bytes.
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                while let Ok(n) = socket.read(&mut tmp).await {
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    let Some(head_end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
                        continue;
                    };
                    let head = String::from_utf8_lossy(&buf[..head_end]).to_ascii_lowercase();
                    let clen = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if buf.len() >= head_end + 4 + clen {
                        break;
                    }
                }
                seen_writer
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buf).to_string());
                let body = body.to_string();
                let _ = socket
                    .write_all(
                        format!(
                            "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\n\
                             content-length: {}\r\nconnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await;
                let _ = socket.shutdown().await;
            }
        });
        (format!("http://{addr}"), seen)
    }

    fn seeded_record(n: usize) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        for i in 0..n {
            engine_record::append(
                &path,
                "event",
                &json!({"service": "shop", "n": i}),
                i as u64,
            )
            .unwrap();
        }
        (dir, path)
    }

    fn body_of(request: &str) -> serde_json::Value {
        let idx = request
            .find("\r\n\r\n")
            .expect("a full request has headers");
        serde_json::from_str(&request[idx + 4..]).expect("the body must be JSON")
    }

    #[tokio::test]
    async fn a_pass_against_a_fresh_plane_sends_everything_authenticated_and_reports_it() {
        let (_dir, path) = seeded_record(3);
        let (url, seen) = fake_plane(vec![
            (200, json!({"server_max": -1})),
            (200, json!({"synced_through": 2})),
        ])
        .await;
        let sync = RecordSync::new(url, "drums_pat_test", "acme/api", path).unwrap();

        let s = sync.pass().await.expect("the pass must succeed");
        assert_eq!(
            s,
            Synced {
                sent: 3,
                through: 2
            }
        );

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        let head = seen[0].to_ascii_lowercase();
        assert!(
            head.starts_with("get /api/record/sync?repo=acme%2fapi"),
            "{}",
            seen[0]
        );
        assert!(
            head.contains("authorization: bearer "),
            "an unauthenticated sync proves nothing about the account it lands in: {}",
            seen[0]
        );
        assert!(
            seen[1]
                .to_ascii_lowercase()
                .starts_with("post /api/record/sync"),
            "{}",
            seen[1]
        );
        assert!(
            seen[1]
                .to_ascii_lowercase()
                .contains("authorization: bearer "),
            "{}",
            seen[1]
        );
        let posted = body_of(&seen[1]);
        assert_eq!(posted["repo"], "acme/api");
        assert_eq!(posted["from_seq"], 0);
        assert_eq!(posted["lines"].as_array().unwrap().len(), 3);
    }

    /// Server-authoritative resume: the plane's own max decides what goes,
    /// not any state kept here.
    #[tokio::test]
    async fn a_mid_anchor_sends_only_the_suffix_the_plane_lacks() {
        let (_dir, path) = seeded_record(3);
        let (url, seen) = fake_plane(vec![
            (200, json!({"server_max": 0})),
            (200, json!({"synced_through": 2})),
        ])
        .await;
        let sync = RecordSync::new(url, "drums_pat_test", "acme/api", path).unwrap();

        let s = sync.pass().await.expect("the pass must succeed");
        assert_eq!(
            s,
            Synced {
                sent: 2,
                through: 2
            }
        );

        let posted = body_of(&seen.lock().unwrap()[1]);
        assert_eq!(posted["from_seq"], 1);
        assert_eq!(posted["lines"][0]["seq"], 1);
        assert_eq!(posted["lines"].as_array().unwrap().len(), 2);
    }

    /// The 409 path end to end: the plane refuses a gap and names its max;
    /// the pass re-anchors from `server_max + 1` and completes.
    #[tokio::test]
    async fn a_409_re_anchors_from_the_planes_max_and_the_pass_completes() {
        let (_dir, path) = seeded_record(3);
        let (url, seen) = fake_plane(vec![
            // A stale answer: the plane actually holds through seq 1.
            (200, json!({"server_max": -1})),
            (409, json!({"server_max": 1})),
            (200, json!({"synced_through": 2})),
        ])
        .await;
        let sync = RecordSync::new(url, "drums_pat_test", "acme/api", path).unwrap();

        let s = sync
            .pass()
            .await
            .expect("a 409 re-anchors; it does not fail the pass");
        assert_eq!(
            s,
            Synced {
                sent: 1,
                through: 2
            }
        );

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 3);
        assert_eq!(
            body_of(&seen[1])["from_seq"],
            0,
            "the first try honoured the stale anchor"
        );
        assert_eq!(
            body_of(&seen[2])["from_seq"],
            2,
            "the retry starts at server_max + 1"
        );
    }

    /// Stop on any error, never retry within a pass: one failing POST ends
    /// the pass with the reason, and nothing further is sent.
    #[tokio::test]
    async fn a_failing_post_stops_the_pass_without_a_retry() {
        let (_dir, path) = seeded_record(3);
        let (url, seen) = fake_plane(vec![
            (200, json!({"server_max": -1})),
            (500, json!({})),
            // Deliberately scripted so a retry WOULD succeed — reaching this
            // response is how a within-pass retry would show up.
            (200, json!({"synced_through": 2})),
        ])
        .await;
        let sync = RecordSync::new(url, "drums_pat_test", "acme/api", path).unwrap();

        let err = sync.pass().await.expect_err("a 500 stops the pass");
        assert!(err.contains("500"), "{err}");
        assert_eq!(
            seen.lock().unwrap().len(),
            2,
            "no retry may follow a failed batch"
        );
    }

    /// A 401 anywhere is the one refusal with a command for a fix, and it
    /// stops the pass before any line is sent.
    #[tokio::test]
    async fn a_401_stops_the_pass_naming_drums_login_before_anything_is_sent() {
        let (_dir, path) = seeded_record(2);
        let (url, seen) = fake_plane(vec![(401, json!({"error": "invalid_token"}))]).await;
        let sync = RecordSync::new(url, "drums_pat_test", "acme/api", path).unwrap();

        let err = sync.pass().await.expect_err("401 is a refusal");
        assert!(err.contains("drums login"), "{err}");
        assert_eq!(
            seen.lock().unwrap().len(),
            1,
            "no record line may follow a refused credential"
        );
    }

    /// Nothing to send is a clean, quiet pass — one GET, no POST — because
    /// this runs on every engine tick and an already-mirrored record is the
    /// steady state.
    #[tokio::test]
    async fn an_already_mirrored_record_is_one_get_and_no_post() {
        let (_dir, path) = seeded_record(3);
        let (url, seen) = fake_plane(vec![(200, json!({"server_max": 2}))]).await;
        let sync = RecordSync::new(url, "drums_pat_test", "acme/api", path).unwrap();

        let s = sync.pass().await.expect("caught up is success");
        assert_eq!(
            s,
            Synced {
                sent: 0,
                through: 2
            }
        );
        assert_eq!(seen.lock().unwrap().len(), 1);
    }
}
