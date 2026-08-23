//! The Linear contract, executed against a real local server rather than
//! asserted about in prose. Every test here runs an axum handler that mimics
//! one of Linear's actual response shapes and checks what this crate does with
//! it — including the two shapes that look like success and are not.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use engine_core::{Claim, Provenance};
use engine_track::{render_comment, IssueRef, IssueTracker, LinearTracker, TrackError};
use serde_json::{json, Value};

/// One request the fake Linear received. The auth header and the query shape
/// are part of the contract, so tests assert on the REQUEST as well as on what
/// this crate does with the response.
type SeenRequest = (Option<String>, Value);

#[derive(Clone, Default)]
struct Seen(Arc<Mutex<Vec<SeenRequest>>>);

type ServerState = (Seen, Value, axum::http::StatusCode);

async fn serve(reply: Value, status: axum::http::StatusCode) -> (SocketAddr, Seen) {
    let seen = Seen::default();
    let state = (seen.clone(), reply, status);
    let app = Router::new()
        .route(
            "/graphql",
            post(
                |State((seen, reply, status)): State<ServerState>,
                 headers: axum::http::HeaderMap,
                 Json(body): Json<Value>| async move {
                    let auth = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .map(|s| s.to_string());
                    seen.0.lock().unwrap().push((auth, body));
                    (status, Json(reply))
                },
            ),
        )
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (addr, seen)
}

fn issue() -> IssueRef {
    IssueRef { id: "e1f2a3b4-0000-4000-8000-000000000000".into(), source: "linear".into() }
}

#[tokio::test]
async fn a_successful_comment_returns_an_observed_claim_carrying_the_url() {
    let (addr, seen) = serve(
        json!({"data":{"commentCreate":{"success":true,"comment":{"url":"https://linear.app/drums/issue/DRM-42#comment-9"}}}}),
        axum::http::StatusCode::OK,
    )
    .await;

    let t = LinearTracker::with_endpoint("lin_api_secret", format!("http://{addr}/graphql"));
    let posted = t.comment(&issue(), "body").await.expect("should post");

    assert_eq!(posted.url.as_deref(), Some("https://linear.app/drums/issue/DRM-42#comment-9"));
    assert_eq!(
        posted.claim.provenance,
        Provenance::Observed,
        "posting a comment is an ACTION that was observed, never a check whose outcome was measured — it must never be `verified`"
    );
    assert!(posted.claim.text.contains("DRM-42"), "{}", posted.claim.text);

    // The request shape is part of the contract too.
    let reqs = seen.0.lock().unwrap();
    let (auth, body) = reqs.first().expect("the server should have been called");
    assert_eq!(
        auth.as_deref(),
        Some("lin_api_secret"),
        "Linear personal API keys go in Authorization with NO `Bearer ` prefix"
    );
    assert_eq!(
        body["variables"]["issueId"], "e1f2a3b4-0000-4000-8000-000000000000",
        "the issue id must travel as a GraphQL VARIABLE, never interpolated into the query"
    );
    assert!(
        body["query"].as_str().unwrap().contains("$issueId"),
        "the query must be parameterised: {}",
        body["query"]
    );
}

#[tokio::test]
async fn graphql_errors_are_a_failure_even_though_the_status_is_200() {
    // The shape that would silently pass a status-code-only check.
    let (addr, _) = serve(
        json!({"errors":[{"message":"Entity not found: Issue"}]}),
        axum::http::StatusCode::OK,
    )
    .await;

    let t = LinearTracker::with_endpoint("k", format!("http://{addr}/graphql"));
    let err = t.comment(&issue(), "body").await.expect_err("200 + errors[] is not success");
    match err {
        TrackError::Rejected { detail, .. } => assert!(detail.contains("Entity not found"), "{detail}"),
        other => panic!("expected Rejected, got {other:?}"),
    }
}

#[tokio::test]
async fn success_false_is_a_failure_even_with_no_errors_array() {
    // The second shape that looks like success: HTTP 200, no errors, but the
    // mutation itself reports it did nothing.
    let (addr, _) = serve(
        json!({"data":{"commentCreate":{"success":false}}}),
        axum::http::StatusCode::OK,
    )
    .await;

    let t = LinearTracker::with_endpoint("k", format!("http://{addr}/graphql"));
    let err = t.comment(&issue(), "body").await.expect_err("success:false is not success");
    assert!(matches!(err, TrackError::Rejected { .. }), "{err:?}");
}

#[tokio::test]
async fn an_http_error_never_echoes_the_response_body() {
    // A tracker's error body can quote the request it is complaining about —
    // which here contains the comment, which contains production error text.
    // This string reaches a terminal, so only the status may be reported.
    let (addr, _) = serve(
        json!({"error":"invalid body: {\"card\":\"4242424242424242\"}"}),
        axum::http::StatusCode::BAD_REQUEST,
    )
    .await;

    let t = LinearTracker::with_endpoint("k", format!("http://{addr}/graphql"));
    let err = t.comment(&issue(), "body").await.expect_err("400 is a failure");
    let rendered = err.to_string();
    assert!(!rendered.contains("4242424242424242"), "the response body leaked: {rendered}");
    assert!(rendered.contains("400"), "{rendered}");
}

#[tokio::test]
async fn an_unreachable_tracker_is_distinguishable_from_a_rejection() {
    // Port 1 on loopback refuses immediately.
    let t = LinearTracker::with_endpoint("k", "http://127.0.0.1:1/graphql");
    let err = t.comment(&issue(), "body").await.expect_err("nothing is listening");
    assert!(
        matches!(err, TrackError::Unreachable { .. }),
        "a network failure must not be reported as the tracker rejecting us: {err:?}"
    );
}

#[tokio::test]
async fn an_empty_issue_id_is_refused_before_any_request_is_made() {
    let (addr, seen) = serve(json!({}), axum::http::StatusCode::OK).await;
    let t = LinearTracker::with_endpoint("k", format!("http://{addr}/graphql"));
    let err = t
        .comment(&IssueRef { id: "  ".into(), source: "linear".into() }, "body")
        .await
        .expect_err("an empty id has nothing to comment on");
    assert!(matches!(err, TrackError::NoIssueId), "{err:?}");
    assert!(
        seen.0.lock().unwrap().is_empty(),
        "no request should have been sent at all"
    );
}

// -- what the comment is allowed to say --------------------------------------

fn claim(text: &str, p: Provenance) -> Claim {
    Claim { text: text.into(), provenance: p }
}

#[test]
fn a_comment_always_says_appearance_is_unchecked_even_when_everything_passed() {
    // A reported issue is usually a VISUAL complaint. Replying "fixed" to one
    // on the strength of a passing test suite is the most dishonest thing this
    // code could do, so the disclaimer is unconditional.
    let body = render_comment(
        &[
            claim("the app's own test script passed (14 tests)", Provenance::Verified),
            claim("build passed", Provenance::Verified),
        ],
        Some("https://github.com/o/r/pull/3"),
        true,
    );
    assert!(body.contains("could **not** check"), "{body}");
    assert!(body.contains("looks* right") || body.contains("*looks*"), "{body}");
    assert!(body.contains("`unresolved`"), "{body}");
}

#[test]
fn a_comment_with_no_verified_claims_says_so_rather_than_listing_nothing() {
    let body = render_comment(&[claim("agent timed out", Provenance::Unresolved)], None, false);
    assert!(
        body.contains("nothing. No check completed"),
        "an empty list reads as 'nothing to report'; it must read as 'nothing is proven':\n{body}"
    );
    assert!(body.contains("could not produce a verified fix"), "{body}");
}

#[test]
fn no_proposal_means_the_comment_does_not_imply_somewhere_to_look() {
    let body = render_comment(&[], None, true);
    assert!(body.contains("No pull request was opened"), "{body}");
    assert!(!body.contains("Review the change:"), "{body}");
}

#[test]
fn a_comment_never_claims_anything_was_merged_or_deployed() {
    let body = render_comment(&[claim("build passed", Provenance::Verified)], None, true);
    assert!(body.contains("Nothing was merged or deployed automatically"), "{body}");
}
