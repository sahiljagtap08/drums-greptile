//! The telemetry contract, executed against a real local server rather than
//! asserted about in prose (same shape as `engine/crates/track/tests/linear.rs`).
//!
//! The load-bearing test in this file is
//! [`the_payload_carries_exactly_these_fields`]: it pins the payload's key set
//! against a literal list, so a field added to `telemetry::Payload` without a
//! deliberate visit to that list fails the build. Everything else here checks
//! that the opt-out is real at the seam where bytes actually leave the
//! machine, and that a slow or dead endpoint cannot touch the loop.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use drums_watch::engine::{EngineEvent, RepairFailure};
use drums_watch::telemetry::{self, Counters, Decision, Payload, Telemetry, PAYLOAD_FIELDS};
use engine_core::{
    Attribution, CapturedRequest, Claim, DeployRecord, ErrorEvent, ErrorSignature, Failure, Intake,
    Provenance, Repair, ShipOutcome,
};
use serde_json::Value;

#[derive(Clone, Default)]
struct Seen(Arc<Mutex<Vec<Value>>>);

/// A real server on a real loopback port. `delay` lets one test prove the
/// heartbeat cannot hold the caller: the handler sleeps far longer than the
/// assertion window.
async fn serve(delay: Duration) -> (SocketAddr, Seen) {
    let seen = Seen::default();
    let state = (seen.clone(), delay);
    let app = Router::new()
        .route(
            "/v1/install",
            post(|State((seen, delay)): State<(Seen, Duration)>, Json(body): Json<Value>| async move {
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                seen.0.lock().unwrap().push(body);
                axum::http::StatusCode::NO_CONTENT
            }),
        )
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (addr, seen)
}

fn payload() -> Payload {
    Payload {
        install_id: "0123456789abcdef0123456789abcdef".into(),
        drums_version: "0.1.0".into(),
        os: "linux".into(),
        arch: "aarch64".into(),
        failures_detected: 7,
        repairs_attempted: 4,
        repairs_verified: 3,
        repairs_shipped: 1,
    }
}

async fn wait_for_one(seen: &Seen) -> Value {
    for _ in 0..100 {
        if let Some(v) = seen.0.lock().unwrap().first().cloned() {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the server never received a heartbeat");
}

#[tokio::test]
async fn a_heartbeat_arrives_as_the_exact_documented_payload() {
    let (addr, seen) = serve(Duration::ZERO).await;
    telemetry::send_once(&format!("http://{addr}/v1/install"), &payload()).await;

    let body = wait_for_one(&seen).await;
    assert_eq!(
        body,
        serde_json::json!({
            "install_id": "0123456789abcdef0123456789abcdef",
            "drums_version": "0.1.0",
            "os": "linux",
            "arch": "aarch64",
            "failures_detected": 7,
            "repairs_attempted": 4,
            "repairs_verified": 3,
            "repairs_shipped": 1
        }),
        "the bytes on the wire are the contract, not the struct's doc comment"
    );
}

/// **The guard.** The payload's key set, pinned to a literal list.
///
/// If you are here because this test failed after you added a field: that is
/// what it is for. Adding a field to `telemetry::Payload` means adding one
/// more thing that leaves every customer's machine, and it needs the same
/// scrutiny the original eight got — re-read the never-send list on
/// `Payload`'s doc comment before you touch the list below, and update the
/// first-run disclosure text and `website/scripts/build-pages.py` in the same
/// change, because the notice and the docs both enumerate these fields.
#[test]
fn the_payload_carries_exactly_these_fields() {
    let json = serde_json::to_value(payload()).unwrap();
    let mut got: Vec<&str> = json
        .as_object()
        .unwrap()
        .keys()
        .map(|k| k.as_str())
        .collect();
    got.sort_unstable();

    let mut allowed = vec![
        "install_id",
        "drums_version",
        "os",
        "arch",
        "failures_detected",
        "repairs_attempted",
        "repairs_verified",
        "repairs_shipped",
    ];
    allowed.sort_unstable();

    assert_eq!(
        got, allowed,
        "the payload gained or lost a field — see this test's doc comment"
    );

    let mut declared: Vec<&str> = PAYLOAD_FIELDS.to_vec();
    declared.sort_unstable();
    assert_eq!(
        declared, allowed,
        "telemetry::PAYLOAD_FIELDS drifted from what is actually serialised"
    );
}

/// The other half of the guard: prove that customer-derived text cannot make
/// it into the payload even when it is flowing through the counter fold in
/// realistic shapes. Every string below is the kind of thing the never-send
/// list names — a repo path, a branch, a stack trace, an error message, a
/// service name, a request body, a URL, an agent's output.
#[test]
fn nothing_derived_from_customer_code_can_reach_the_payload() {
    const SECRETS: &[&str] = &[
        "acme-payments",                                 // repository / service name
        "/Users/dana/src/acme-payments",                 // absolute path
        "src/billing/charge.ts",                         // repo-relative file path
        "drums/repair-4242",                             // branch name
        "9f8e7d6c5b4a",                                  // commit sha
        "TypeError",                                     // error name
        "cannot read property total of undefined",       // error message
        "at computeTotal (src/billing/charge.ts:14:31)", // stack frame
        "{\"card\":\"4242424242424242\"}",               // request body
        "https://api.acme.internal/v2/charge",           // url
        "the agent rewrote the null check",              // agent output
        "acme-payments/TypeError",                       // failure-class key
    ];

    let counters = Counters::default();
    let failure = failure_stuffed_with(SECRETS);
    counters.observe(&EngineEvent::FailureDetected(failure.clone()));
    counters.observe(&EngineEvent::Repairing(
        failure.clone(),
        SECRETS[10].to_string(),
    ));
    counters.observe(&EngineEvent::RepairReady(
        failure.clone(),
        repair_stuffed_with(SECRETS),
        1234,
    ));
    counters.observe(&EngineEvent::Shipped(
        failure.clone(),
        ship_stuffed_with(SECRETS),
    ));
    counters.observe(&EngineEvent::Attributed(
        failure.clone(),
        attribution_stuffed_with(SECRETS),
    ));
    counters.observe(&EngineEvent::RepairFailed(
        failure.clone(),
        RepairFailure {
            why: SECRETS[6].to_string(),
            worktree: Some(SECRETS[1].to_string()),
            branch: Some(SECRETS[3].to_string()),
            elapsed_ms: 1,
        },
    ));
    counters.observe(&EngineEvent::Demoted(
        SECRETS[11].to_string(),
        SECRETS[6].to_string(),
    ));
    counters.observe(&EngineEvent::DeployRecorded(DeployRecord {
        sha: SECRETS[4].into(),
        description: SECRETS[10].into(),
        author: "dana@acme.example".into(),
        deployed_at_ms: 1,
    }));

    let totals = counters.totals();
    assert_eq!(totals.failures_detected, 1);
    assert_eq!(totals.repairs_attempted, 1);
    assert_eq!(
        totals.repairs_verified, 1,
        "only RepairReady counts as verified"
    );
    assert_eq!(totals.repairs_shipped, 1);

    let t = Telemetry::new(
        Decision::On,
        Some("0123456789abcdef0123456789abcdef".into()),
        "http://127.0.0.1:1/",
    );
    let serialised =
        serde_json::to_string(&t.payload().expect("on + an id means a payload")).unwrap();
    for secret in SECRETS {
        assert!(
            !serialised.contains(secret),
            "customer-derived text reached the payload: {secret:?} in {serialised}"
        );
    }
    // And the four author-controlled strings are the only strings at all.
    let json: Value = serde_json::from_str(&serialised).unwrap();
    for (key, value) in json.as_object().unwrap() {
        if value.is_string() {
            assert!(
                ["install_id", "drums_version", "os", "arch"].contains(&key.as_str()),
                "a new string-valued field appeared ({key}); strings are where customer data hides"
            );
        }
    }
}

#[tokio::test]
async fn an_opted_out_install_sends_nothing_at_all() {
    let (addr, seen) = serve(Duration::ZERO).await;
    for decision in [
        Decision::OffByEnv,
        Decision::OffByConfig,
        Decision::OffByUnrecognised {
            source: "DRUMS_TELEMETRY",
            value: "maybe".into(),
        },
    ] {
        let t = Telemetry::new(
            decision.clone(),
            Some("id".into()),
            format!("http://{addr}/v1/install"),
        );
        t.spawn_heartbeat();
    }
    // Generous relative to a loopback round trip, which the on-path tests
    // above complete in single-digit milliseconds.
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        seen.0.lock().unwrap().is_empty(),
        "an opted-out install must not reach the network: {:?}",
        seen.0.lock().unwrap()
    );
}

#[tokio::test]
async fn the_heartbeat_never_makes_the_caller_wait() {
    // A server that takes three seconds to answer. `spawn_heartbeat` must
    // return in the time it takes to spawn a task, not in three seconds.
    let (addr, seen) = serve(Duration::from_secs(3)).await;
    let t = Telemetry::new(
        Decision::On,
        Some("id".into()),
        format!("http://{addr}/v1/install"),
    );

    let started = std::time::Instant::now();
    t.spawn_heartbeat();
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(100),
        "spawning the heartbeat blocked the caller for {elapsed:?} — the loop must never wait on telemetry"
    );
    assert!(
        seen.0.lock().unwrap().is_empty(),
        "and it must not have completed synchronously either"
    );
}

#[tokio::test]
async fn an_unreachable_endpoint_is_not_an_error_and_does_not_hang() {
    // Port 1 on loopback refuses immediately. `send_once` has no error type
    // to return by construction; this asserts it also cannot panic or stall.
    let started = std::time::Instant::now();
    telemetry::send_once("http://127.0.0.1:1/v1/install", &payload()).await;
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "a refused connection must not sit on the timeout"
    );
}

#[tokio::test]
async fn a_server_that_rejects_the_heartbeat_is_still_not_an_error() {
    let app = Router::new().route(
        "/v1/install",
        post(|| async { axum::http::StatusCode::INTERNAL_SERVER_ERROR }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    // Returns `()` whatever happened: our analytics being broken is never
    // narrated to the operator as their repair being broken.
    telemetry::send_once(&format!("http://{addr}/v1/install"), &payload()).await;
}

// -- fixtures ---------------------------------------------------------------

fn failure_stuffed_with(s: &[&str]) -> Failure {
    Failure {
        id: "f_01J".into(),
        service: s[0].into(),
        signature: ErrorSignature {
            error_name: s[5].into(),
            top_frame_file: s[2].into(),
            top_frame_function: Some("computeTotal".into()),
        },
        first_seen_ms: 1_753_000_000_000,
        event_count: 3,
        intake: Intake::Snippet,
        sample: ErrorEvent {
            intake: Intake::Snippet,
            service: s[0].into(),
            occurred_at_ms: 1_753_000_000_000,
            error_name: s[5].into(),
            error_message: s[6].into(),
            stack: format!("{}: {}\n    {}", s[5], s[6], s[7]),
            request: Some(CapturedRequest {
                method: "POST".into(),
                path: "/v2/charge".into(),
                content_type: Some("application/json".into()),
                body: Some(s[8].into()),
            }),
        },
        claim: Claim {
            text: s[6].into(),
            provenance: Provenance::Observed,
        },
    }
}

fn repair_stuffed_with(s: &[&str]) -> Repair {
    Repair {
        id: "r_01J".into(),
        failure_id: "f_01J".into(),
        sha: s[4].into(),
        branch: s[3].into(),
        agent: "claude".into(),
        summary: s[10].into(),
        diff_stat: format!("{} | 2 +-", s[2]),
        claims: vec![Claim {
            text: s[10].into(),
            provenance: Provenance::Verified,
        }],
    }
}

fn ship_stuffed_with(s: &[&str]) -> ShipOutcome {
    ShipOutcome {
        failure_id: "f_01J".into(),
        repair_sha: s[4].into(),
        action: "shipped".into(),
        deploy_cmd: format!("bash deploy.sh {}", s[4]),
        claims: vec![Claim {
            text: s[9].into(),
            provenance: Provenance::Verified,
        }],
    }
}

fn attribution_stuffed_with(s: &[&str]) -> Attribution {
    Attribution {
        deploy: DeployRecord {
            sha: s[4].into(),
            description: s[10].into(),
            author: "dana@acme.example".into(),
            deployed_at_ms: 1,
        },
        minutes_after_deploy: 4,
        overlap_files: vec![s[2].into()],
        claim: Claim {
            text: s[2].into(),
            provenance: Provenance::Observed,
        },
    }
}
