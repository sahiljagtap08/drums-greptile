//! HTTP ingest: deploy records and error events in, JSONL record + channel out.
//! The JSONL file is append-only — the Stage-1 seed of the §3 record.
//!
//! Redaction happens here, at capture, before the record is written (spec
//! §19): the persisted line never carries a raw request body. The event
//! forwarded on the in-process channel to the engine keeps the RAW body —
//! reproduction needs the real request to replay against a rebuilt revision;
//! the record just must never be the thing that stores it.

use std::io;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, SystemTimeError, UNIX_EPOCH};

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use engine_core::{DeployRecord, ErrorEvent, ReportedIssue};
use serde::Serialize;
use tokio::sync::mpsc;

pub mod adapters;

#[derive(Debug)]
pub enum Ingested {
    Deploy(DeployRecord),
    Error(ErrorEvent),
    /// A human-reported issue intake (Agentation/Linear webhooks). Carries
    /// its own `Observed` claim and never enters the failure/repair pipeline
    /// — see `engine-ingest::adapters` and the engine loop's narration-only
    /// handling of this variant.
    Reported(ReportedIssue),
}

#[derive(Clone)]
pub struct IngestState {
    record_path: PathBuf,
    tx: mpsc::UnboundedSender<Ingested>,
}

/// Maps a `SystemTime::now().duration_since(UNIX_EPOCH)` result to a
/// millisecond timestamp, surfacing clock failure as an error rather than
/// stamping a fabricated `0` (m6). `SystemTimeError` only occurs when the
/// system clock reads before the Unix epoch — essentially unreachable on
/// real hardware, but on a misconfigured clock `recorded_at_ms: 0` would
/// read downstream as a real (1970) timestamp rather than the honest signal
/// "this record line's time could not be established". Factored out so the
/// mapping is unit-testable without depending on the real system clock.
fn duration_since_epoch_to_ms(d: Result<Duration, SystemTimeError>) -> io::Result<u64> {
    d.map(|dur| dur.as_millis() as u64)
        .map_err(|e| io::Error::other(format!("system clock is before the Unix epoch, cannot stamp recorded_at_ms: {e}")))
}

pub(crate) fn now_ms() -> io::Result<u64> {
    duration_since_epoch_to_ms(SystemTime::now().duration_since(UNIX_EPOCH))
}

impl IngestState {
    pub fn new(record_path: PathBuf) -> (Self, mpsc::UnboundedReceiver<Ingested>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self { record_path, tx }, rx)
    }

    /// Appends `item` to the record under `kind`, stamped with the current
    /// time. Runs on a blocking thread since `engine_record::append` does
    /// synchronous file I/O. If the system clock can't be read (m6), the
    /// append is refused outright — a record line is never written with a
    /// fabricated timestamp; the caller surfaces this as a 500, same as any
    /// other append failure.
    async fn append<T: Serialize + Send + 'static>(&self, kind: &'static str, item: T) -> std::io::Result<()> {
        let path = self.record_path.clone();
        let recorded_at_ms = match now_ms() {
            Ok(ms) => ms,
            Err(e) => {
                tracing::error!(kind, error = %e, "refusing to append record line: system clock unreadable");
                return Err(e);
            }
        };
        tokio::task::spawn_blocking(move || engine_record::append(&path, kind, &item, recorded_at_ms))
            .await
            .expect("append blocking task panicked")
    }
}

pub fn router(state: IngestState) -> Router {
    Router::new()
        .route("/v1/deploys", post(post_deploy))
        .route("/v1/events", post(post_event))
        .with_state(state.clone())
        .merge(adapters::router(state))
}

/// Same ASCII-hex, 4-64-char rule `engine_repro::validate_sha` and
/// `crates/cli/src/ship.rs`'s `validate_recorded_sha` already apply before a
/// record-sourced sha is used — checked here too, at the INGEST boundary
/// (trust-hardening review, m3).
///
/// Before this, `POST /v1/deploys` accepted and recorded a `sha` field
/// unvalidated, so a misconfigured deploy hook that posts a tag
/// (`v2.1.0`) or a branch name (`main`) would record fine and only fail —
/// hard, with a typed refusal — at `drums revert` time, the single worst
/// moment to discover the record is unusable: the moment someone is trying
/// to roll a bad deploy back. Refusing the POST loudly, at deploy time,
/// means whoever configured the hook sees the problem immediately instead
/// of the on-call engineer weeks later. The later validation
/// (`validate_recorded_sha`) stays in place as defense in depth for records
/// written before this check existed, or by any other writer.
fn is_valid_git_sha(sha: &str) -> bool {
    (4..=64).contains(&sha.len()) && sha.bytes().all(|b| b.is_ascii_hexdigit())
}

async fn post_deploy(State(s): State<IngestState>, Json(d): Json<DeployRecord>) -> (StatusCode, String) {
    if !is_valid_git_sha(&d.sha) {
        return (
            StatusCode::BAD_REQUEST,
            format!("sha must be a git object id: 4-64 ASCII hex characters, got {:?}", d.sha),
        );
    }
    if s.append("deploy", d.clone()).await.is_err() {
        return (StatusCode::INTERNAL_SERVER_ERROR, String::new());
    }
    if s.tx.send(Ingested::Deploy(d)).is_err() {
        tracing::error!(kind = "deploy", "ingest channel receiver dropped; item not forwarded");
    }
    (StatusCode::ACCEPTED, String::new())
}

async fn post_event(State(s): State<IngestState>, Json(e): Json<ErrorEvent>) -> StatusCode {
    // Redact a copy for the record only; `e` keeps its raw body and path and
    // is what gets forwarded to the engine below for reproduction/replay.
    //
    // `request` is `None` for a trigger/reported event (an OTel span or a
    // Linear issue opens a failure with no replayable request) — there is
    // simply nothing to redact then, and nothing is fabricated to fill the
    // gap. The redaction discipline itself is unchanged for the snippet path.
    let mut for_record = e.clone();
    if let Some(request) = for_record.request.as_mut() {
        if let Some(body) = &request.body {
            request.body = Some(engine_record::redact_body(request.content_type.as_deref(), body, &[]));
        }
        // m3: query-string secrets in the path (e.g. `?api_key=…`) must be
        // masked in the record the same way the body is; the in-memory/channel
        // path stays raw for replay.
        request.path = engine_record::redact_query_string(&request.path, &[]);
    }
    if s.append("event", for_record).await.is_err() {
        return StatusCode::INTERNAL_SERVER_ERROR;
    }
    if s.tx.send(Ingested::Error(e)).is_err() {
        tracing::error!(kind = "event", "ingest channel receiver dropped; item not forwarded");
    }
    StatusCode::ACCEPTED
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    fn deploy_json() -> String {
        r#"{"sha":"abc123","description":"add promo code field","author":"maya","deployed_at_ms":1753000000000}"#.into()
    }

    fn event_json() -> String {
        r#"{"service":"shop","occurred_at_ms":1753000360000,"error_name":"TypeError","error_message":"boom","stack":"TypeError: boom\n    at computeTotal (/x/server.js:4:20)","request":{"method":"POST","path":"/api/checkout","content_type":"application/json","body":"{}"}}"#.into()
    }

    #[tokio::test]
    async fn deploy_post_appends_jsonl_and_forwards() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        let (state, mut rx) = IngestState::new(path.clone());
        let app = router(state);
        let res = app
            .oneshot(Request::post("/v1/deploys").header("content-type", "application/json").body(Body::from(deploy_json())).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::ACCEPTED);
        let got = rx.recv().await.unwrap();
        assert!(matches!(got, Ingested::Deploy(d) if d.sha == "abc123"));
        let line = std::fs::read_to_string(&path).unwrap();
        assert!(line.contains(r#""kind":"deploy""#));
        assert!(line.ends_with('\n'));
    }

    // -- m3: sha validation belongs at the ingest boundary -------------------

    #[test]
    fn is_valid_git_sha_accepts_hex_and_rejects_non_hex() {
        assert!(is_valid_git_sha("abc123"));
        assert!(is_valid_git_sha("0123456789abcdef0123456789abcdef01234567"));
        assert!(!is_valid_git_sha("v2.1.0"), "a tag must not pass as a sha");
        assert!(!is_valid_git_sha("main"), "a branch name must not pass as a sha");
        assert!(!is_valid_git_sha(""), "empty must not pass");
        assert!(!is_valid_git_sha("abc"), "below the 4-char floor");
        assert!(!is_valid_git_sha(&"a".repeat(65)), "above the 64-char ceiling");
    }

    /// RED FIRST (trust-hardening review, m3): before this fix, a deploy hook
    /// posting a tag or branch name instead of a sha was accepted (202) and
    /// appended to the record — the refusal only ever surfaced later, at
    /// `drums revert` time, the worst possible moment. This must now refuse
    /// at the POST itself, naming the field, and never append the line.
    #[tokio::test]
    async fn deploy_post_rejects_a_non_hex_sha_naming_the_field_and_records_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        let (state, mut rx) = IngestState::new(path.clone());
        let app = router(state);
        let body = r#"{"sha":"v2.1.0","description":"tag from a misconfigured hook","author":"maya","deployed_at_ms":1753000000000}"#;
        let res = app
            .oneshot(Request::post("/v1/deploys").header("content-type", "application/json").body(Body::from(body)).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST, "a non-hex sha must be refused at ingest, not silently recorded");

        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let msg = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(msg.contains("sha"), "the refusal must name the field: {msg}");

        assert!(!path.exists() || std::fs::read_to_string(&path).unwrap().is_empty(), "an invalid sha must never be appended to the record");
        assert!(rx.try_recv().is_err(), "an invalid sha must never be forwarded to the engine either");
    }

    #[tokio::test]
    async fn event_post_appends_jsonl_and_forwards() {
        let dir = tempfile::tempdir().unwrap();
        let (state, mut rx) = IngestState::new(dir.path().join("record.jsonl"));
        let app = router(state);
        let res = app
            .oneshot(Request::post("/v1/events").header("content-type", "application/json").body(Body::from(event_json())).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::ACCEPTED);
        assert!(matches!(rx.recv().await.unwrap(), Ingested::Error(e) if e.error_name == "TypeError"));
    }

    #[tokio::test]
    async fn event_post_redacts_body_in_record_but_forwards_raw_body_on_channel() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        let (state, mut rx) = IngestState::new(path.clone());
        let app = router(state);
        // The path carries a query-string secret (`?api_key=…`) alongside the JSON
        // body's card number, so this one request pins both halves of m3 and the
        // pre-existing body pin in a single two-sided assertion.
        let body = r#"{"service":"shop","occurred_at_ms":1753000360000,"error_name":"TypeError","error_message":"boom","stack":"TypeError: boom\n    at computeTotal (/x/server.js:4:20)","request":{"method":"POST","path":"/api/checkout?api_key=SECRET123","content_type":"application/json","body":"{\"card\":\"4242424242424242\",\"item\":\"widget\"}"}}"#;
        let res = app
            .oneshot(Request::post("/v1/events").header("content-type", "application/json").body(Body::from(body)).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::ACCEPTED);

        // The channel-forwarded event (used for reproduction/replay) keeps the RAW
        // body AND the RAW path — replay needs the real query-string secret too.
        let forwarded = rx.recv().await.unwrap();
        let Ingested::Error(ev) = forwarded else { panic!("expected an error event") };
        let request = ev.request.as_ref().expect("a snippet event carries a replayable request");
        assert!(
            request.body.as_deref().unwrap().contains("4242424242424242"),
            "channel-forwarded event must keep the raw body for replay: {:?}",
            request.body
        );
        assert_eq!(
            request.path, "/api/checkout?api_key=SECRET123",
            "channel-forwarded event must keep the raw path (query-string secret included) for replay"
        );

        // The persisted record line has the body AND the path's query-string
        // secret redacted — this is the compliance pin: the record never stores
        // the secret, only the in-memory replay path saw it.
        let record = std::fs::read_to_string(&path).unwrap();
        assert!(record.contains("[redacted]"), "record must contain the redaction marker: {record}");
        assert!(!record.contains("4242424242424242"), "record must never contain the raw card number: {record}");
        assert!(!record.contains("SECRET123"), "record must never contain the raw query-string secret: {record}");
        assert!(
            record.contains(r#""path":"/api/checkout?api_key=[redacted]""#),
            "record must mask the sensitive query-string value in the path: {record}"
        );
        assert!(record.contains(r#""kind":"event""#));
        assert!(record.contains(r#""recorded_at_ms""#), "record line must be stamped with recorded_at_ms: {record}");
    }

    /// A trigger-only post (OTel/HyperDX/PostHog: no replayable request) is
    /// accepted, recorded with its intake, and forwarded WITHOUT a request —
    /// nothing is synthesized to fill the gap, and the record line says which
    /// source opened it.
    #[tokio::test]
    async fn trigger_event_post_records_the_intake_and_forwards_no_request() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        let (state, mut rx) = IngestState::new(path.clone());
        let app = router(state);
        let body = r#"{"service":"shop","occurred_at_ms":1753000360000,"error_name":"TypeError","error_message":"boom","stack":"TypeError: boom\n    at computeTotal (/x/server.js:4:20)","intake":{"kind":"trigger","source":"hyperdx"}}"#;
        let res = app
            .oneshot(Request::post("/v1/events").header("content-type", "application/json").body(Body::from(body)).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::ACCEPTED);

        let Ingested::Error(ev) = rx.recv().await.unwrap() else { panic!("expected an error event") };
        assert!(ev.request.is_none(), "a trigger event must not arrive with a fabricated request");
        assert_eq!(ev.intake, engine_core::Intake::Trigger { source: "hyperdx".into() });

        let record = std::fs::read_to_string(&path).unwrap();
        assert!(record.contains(r#""intake":{"kind":"trigger","source":"hyperdx"}"#), "the record must carry the intake: {record}");
        assert!(!record.contains("request"), "no request key must be invented for a trigger event: {record}");
    }

    #[tokio::test]
    async fn malformed_body_is_rejected_not_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        let (state, _rx) = IngestState::new(path.clone());
        let app = router(state);
        let res = app
            .oneshot(Request::post("/v1/events").header("content-type", "application/json").body(Body::from("{nope")).unwrap())
            .await
            .unwrap();
        assert!(res.status().is_client_error());
        assert!(!path.exists() || std::fs::read_to_string(&path).unwrap().is_empty());
    }

    // -- m6: clock-failure handling -------------------------------------------------

    #[test]
    fn duration_since_epoch_to_ms_converts_a_valid_duration() {
        let ms = duration_since_epoch_to_ms(Ok(Duration::from_millis(1_753_000_000_000))).unwrap();
        assert_eq!(ms, 1_753_000_000_000);
    }

    #[test]
    fn duration_since_epoch_to_ms_surfaces_clock_before_epoch_as_an_error_not_a_zero_stamp() {
        // m6: `now_ms().unwrap_or(0)` used to stamp epoch 0 on clock failure, which
        // reads downstream as a real (if very old) timestamp rather than the honest
        // signal "the clock could not be read". A `SystemTimeError` only arises when
        // `SystemTime::now()` reads before `UNIX_EPOCH`; that's not reproducible via
        // the real clock in a test, so the pure mapping function is exercised directly
        // with a constructed error, the same value `SystemTime::duration_since` would
        // hand back.
        let err = UNIX_EPOCH.duration_since(UNIX_EPOCH + Duration::from_secs(1)).unwrap_err();
        let result = duration_since_epoch_to_ms(Err(err));
        assert!(result.is_err(), "clock failure must surface as an error, never a fabricated 0 timestamp");
    }

    #[tokio::test]
    async fn event_post_returns_500_and_does_not_append_when_record_path_is_unwritable() {
        // `IngestState::append` refuses to write a record line whenever the write
        // itself fails, for any reason — a directory that doesn't exist here, or
        // `now_ms()`'s clock-read failure (m6, unit-tested above via
        // `duration_since_epoch_to_ms` directly, since the real clock can't be forced
        // before the epoch in a test). Both routes through `append` must refuse
        // cleanly (500, nothing recorded) rather than silently writing degraded data.
        let dir = tempfile::tempdir().unwrap();
        let unwritable_dir = dir.path().join("no-such-parent").join("record.jsonl");
        let (state, _rx) = IngestState::new(unwritable_dir.clone());
        let app = router(state);
        let res = app
            .oneshot(Request::post("/v1/events").header("content-type", "application/json").body(Body::from(event_json())).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(!unwritable_dir.exists());
    }
}
