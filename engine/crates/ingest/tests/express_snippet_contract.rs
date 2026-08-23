//! Contract test: proves `integrations/node-express/drums-report.js` emits
//! JSON that actually deserializes as `engine_core::ErrorEvent` — the type
//! `POST /v1/events` accepts (see `engine-ingest`'s `post_event`).
//!
//! The fixture is not hand-written: it is the literal payload
//! `integrations/node-express/test/test-drums-report.js` captured from a real
//! run of the Express middleware against a local HTTP server (same pattern
//! as `demo/test-demo.sh`), then wrote to
//! `integrations/node-express/test/fixture-event.json`. If a field is ever
//! renamed on one side and not the other, this test is what catches it.

use engine_core::ErrorEvent;

const FIXTURE: &str = include_str!("../../../../integrations/node-express/test/fixture-event.json");

#[test]
fn express_snippet_fixture_deserializes_as_error_event() {
    let ev: ErrorEvent = serde_json::from_str(FIXTURE).expect(
        "integrations/node-express/test/fixture-event.json must deserialize as engine_core::ErrorEvent \
         — the Express snippet's field names must match the wire contract exactly",
    );

    assert_eq!(ev.service, "checkout-demo");
    assert_eq!(ev.error_name, "TypeError");
    assert!(!ev.error_message.is_empty());
    assert!(ev.stack.contains("computeTotal"), "stack must be forwarded verbatim: {}", ev.stack);

    assert_eq!(ev.request.as_ref().expect("a snippet-sourced event must carry a replayable request").method, "POST");
    assert_eq!(ev.request.as_ref().expect("a snippet-sourced event must carry a replayable request").path, "/api/checkout");
    assert_eq!(ev.request.as_ref().expect("a snippet-sourced event must carry a replayable request").content_type.as_deref(), Some("application/json"));

    let body = ev.request.as_ref().expect("a snippet-sourced event must carry a replayable request").body.clone().expect("captured request body must be present");
    let parsed_body: serde_json::Value = serde_json::from_str(&body).expect("captured body must itself be valid JSON text");
    assert_eq!(parsed_body["items"][0]["price"], 100);
    assert_eq!(parsed_body["promo"]["code"], "TEN");
}

#[tokio::test]
async fn express_snippet_fixture_round_trips_through_the_real_ingest_router() {
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
    assert_eq!(res.status(), StatusCode::ACCEPTED, "the real ingest router must accept the Express snippet's payload");
    let forwarded = rx.recv().await.unwrap();
    assert!(matches!(forwarded, engine_ingest::Ingested::Error(e) if e.service == "checkout-demo"));
}
