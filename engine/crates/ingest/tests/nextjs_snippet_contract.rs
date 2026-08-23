//! Contract test: proves `integrations/nextjs/drums-report.ts` emits JSON
//! that actually deserializes as `engine_core::ErrorEvent` — the type
//! `POST /v1/events` accepts (see `engine-ingest`'s `post_event`).
//!
//! Sibling of `express_snippet_contract.rs`. The fixture is not hand-written:
//! it is the literal payload `integrations/nextjs/test/test-drums-report.js`
//! captured from a real run of `withDrums` against a local HTTP server (same
//! pattern as the Express test and `demo/test-demo.sh`), then wrote to
//! `integrations/nextjs/test/fixture-event.json`. This test also pins the
//! query-string-preservation fix in `request.path` (`url.pathname +
//! url.search`, not `url.pathname` alone) — the bug the Express fixture
//! can't catch, since only the Next.js snippet's fixture carries a `path`
//! with a query string in it.

use engine_core::ErrorEvent;

const FIXTURE: &str = include_str!("../../../../integrations/nextjs/test/fixture-event.json");

#[test]
fn nextjs_snippet_fixture_deserializes_as_error_event() {
    let ev: ErrorEvent = serde_json::from_str(FIXTURE).expect(
        "integrations/nextjs/test/fixture-event.json must deserialize as engine_core::ErrorEvent \
         — the Next.js snippet's field names must match the wire contract exactly",
    );

    assert_eq!(ev.service, "checkout-demo");
    assert_eq!(ev.error_name, "TypeError");
    assert!(!ev.error_message.is_empty());
    assert!(ev.stack.contains("computeTotal"), "stack must be forwarded verbatim: {}", ev.stack);

    assert_eq!(ev.request.as_ref().expect("a snippet-sourced event must carry a replayable request").method, "POST");
    // The fixed behavior this test pins: request.path must carry the query
    // string (url.pathname + url.search), not just url.pathname.
    assert_eq!(ev.request.as_ref().expect("a snippet-sourced event must carry a replayable request").path, "/api/checkout?promo=TEN", "request.path must include the query string");
    assert_eq!(ev.request.as_ref().expect("a snippet-sourced event must carry a replayable request").content_type.as_deref(), Some("application/json"));

    let body = ev.request.as_ref().expect("a snippet-sourced event must carry a replayable request").body.clone().expect("captured request body must be present");
    let parsed_body: serde_json::Value = serde_json::from_str(&body).expect("captured body must itself be valid JSON text");
    assert_eq!(parsed_body["items"][0]["price"], 100);
    assert_eq!(parsed_body["promo"]["code"], "TEN");
}

#[tokio::test]
async fn nextjs_snippet_fixture_round_trips_through_the_real_ingest_router() {
    // Belt-and-suspenders: not just "does it deserialize as the type" but
    // "does the real /v1/events handler accept it and return 202".
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    let dir = tempfile::tempdir().unwrap();
    let (state, mut rx) = engine_ingest::IngestState::new(dir.path().join("record.jsonl"));
    let app = engine_ingest::router(state);
    let res = app
        .oneshot(
            Request::post("/v1/events")
                .header("content-type", "application/json")
                .body(Body::from(FIXTURE.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::ACCEPTED, "the real ingest router must accept the Next.js snippet's payload");
    let forwarded = rx.recv().await.unwrap();
    let engine_ingest::Ingested::Error(e) = forwarded else { panic!("expected an error event") };
    assert_eq!(e.service, "checkout-demo");
    assert_eq!(e.request.as_ref().expect("a snippet-sourced event must carry a replayable request").path, "/api/checkout?promo=TEN", "the raw channel-forwarded path must keep the query string for replay");
}
