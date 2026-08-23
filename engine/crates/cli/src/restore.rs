//! Restart-idempotence: rebuild the detector's `opened` state from the
//! append-only record after a process restart. This is the difference
//! between a script and a service (spec: "software that maintains itself"
//! must survive restarts of ITSELF, not merely of the terminal it was
//! started from) — without it, `drumsd` restarting mid-day would forget
//! every failure it had already detected and either re-open one that
//! already has a `repair_ready`/`shipped` line, or start a second, redundant
//! repair attempt for a failure whose reproduction/repair was still in
//! flight when the process died.
//!
//! Pure and synchronous — no engine, no ingest server, no tokio runtime
//! required — so it's testable as plain data in, plain data out.

use std::path::Path;

use engine_core::{ErrorEvent, ErrorSignature};
use engine_detect::Detector;

/// Replays every readable `event` record line — in on-disk/write order,
/// i.e. the exact order the live process originally observed them — through
/// a throwaway [`Detector`] built with the SAME `threshold`/`window_ms`/
/// `app_root` the real engine is about to run with, then returns the
/// resulting set of already-opened signatures.
///
/// `Detector::observe` is a pure function of (its own config, the sequence
/// of events it's fed): replaying the identical history a previous run of
/// this process saw reconstructs the identical `opened` state that run had,
/// with no need to separately persist "opened" as its own record kind. A
/// signature that crossed the threshold before restart — in particular, one
/// that already earned a `repair_ready`/`shipped` line, since that could
/// only have happened via an earlier threshold crossing — comes back
/// opened; nothing else does.
///
/// A record line whose `kind` isn't `event`, or whose payload doesn't parse
/// as [`ErrorEvent`], is skipped: it carries no signature information this
/// function needs, and one unreadable/foreign line must never abort startup
/// — the same tolerant-reader discipline `record_cmd::load` already applies
/// for the human-facing `drums record` view. A record that doesn't exist yet
/// (the very first `drumsd` start against this repo) replays as empty,
/// matching `engine_record::read_all`'s own "missing file = clean empty
/// read" contract.
pub fn rebuild_opened_signatures(
    record_path: &Path,
    threshold: usize,
    window_ms: u64,
    app_root: &str,
) -> Vec<ErrorSignature> {
    let mut detector = Detector::new(threshold, window_ms, app_root.to_string());
    let Ok(read) = engine_record::read_all(record_path) else {
        // A genuine read failure (permission denied, not a regular file, …)
        // here is not this function's to report — `drumsd`'s startup logs
        // the record-load outcome separately. Replaying nothing is the safe
        // default: it never falsely gates a signature, it can only fail to
        // restore one that should have been gated (the same direction every
        // other degraded-input case in this module fails).
        return Vec::new();
    };
    for (kind, value) in read.lines {
        if kind != "event" {
            continue;
        }
        let Ok(ev) = serde_json::from_value::<ErrorEvent>(value) else {
            continue;
        };
        let _ = detector.observe(ev);
    }
    detector.opened_signatures()
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_core::{CapturedRequest, Intake};

    fn event(at: u64) -> ErrorEvent {
        ErrorEvent {
            service: "shop".into(),
            occurred_at_ms: at,
            error_name: "TypeError".into(),
            error_message: "boom".into(),
            stack: "TypeError: boom\n    at computeTotal (server.js:4:2)".into(),
            request: Some(CapturedRequest {
                method: "POST".into(),
                path: "/api/checkout".into(),
                content_type: None,
                body: Some("{}".into()),
            }),
            intake: Intake::Snippet,
        }
    }

    #[test]
    fn a_missing_record_replays_as_no_opened_signatures() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        assert!(rebuild_opened_signatures(&path, 3, 60_000, "").is_empty());
    }

    #[test]
    fn events_that_crossed_the_threshold_come_back_opened() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        for (i, at) in [1_000, 2_000, 3_000].into_iter().enumerate() {
            engine_record::append(&path, "event", &event(at), at + i as u64).unwrap();
        }
        let opened = rebuild_opened_signatures(&path, 3, 60_000, "");
        assert_eq!(opened.len(), 1);
        assert_eq!(opened[0].error_name, "TypeError");
    }

    #[test]
    fn events_below_threshold_do_not_come_back_opened() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        for at in [1_000, 2_000] {
            engine_record::append(&path, "event", &event(at), at).unwrap();
        }
        // Threshold is 3; only 2 events were ever recorded.
        assert!(rebuild_opened_signatures(&path, 3, 60_000, "").is_empty());
    }

    /// The headline scenario the daemon work is pinned against: a repair
    /// already completed for this signature (a `repair_ready` line exists).
    /// That line itself carries no signature — it's the preceding `event`
    /// lines whose replay re-derives the gate.
    #[test]
    fn a_signature_with_an_existing_repair_ready_comes_back_opened() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        for at in [1_000, 2_000, 3_000] {
            engine_record::append(&path, "event", &event(at), at).unwrap();
        }
        engine_record::append(
            &path,
            "repair_ready",
            &engine_core::Repair {
                id: "r1".into(),
                failure_id: "f1".into(),
                sha: "deadbeef".into(),
                branch: "drums/repair-f1".into(),
                agent: "claude".into(),
                summary: "fixed it".into(),
                diff_stat: "server.js | 1 +".into(),
                claims: vec![],
            },
            3_500,
        )
        .unwrap();

        let opened = rebuild_opened_signatures(&path, 3, 60_000, "");
        assert_eq!(opened.len(), 1);
    }

    #[test]
    fn non_event_and_malformed_lines_are_skipped_without_aborting_replay() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        engine_record::append(
            &path,
            "deploy",
            &engine_core::DeployRecord {
                sha: "a".into(),
                description: "d".into(),
                author: "t".into(),
                deployed_at_ms: 1,
            },
            1,
        )
        .unwrap();
        // An "event" line that doesn't actually parse as ErrorEvent.
        engine_record::append(&path, "event", &serde_json::json!({"not": "an event"}), 2).unwrap();
        for at in [3_000, 4_000, 5_000] {
            engine_record::append(&path, "event", &event(at), at).unwrap();
        }
        let opened = rebuild_opened_signatures(&path, 3, 60_000, "");
        assert_eq!(
            opened.len(),
            1,
            "the well-formed events must still cross the threshold despite the noise around them"
        );
    }

    #[test]
    fn different_app_root_config_changes_the_reconstructed_signature() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        let ev = |at: u64| ErrorEvent {
            service: "shop".into(),
            occurred_at_ms: at,
            error_name: "TypeError".into(),
            error_message: "boom".into(),
            stack: "TypeError: boom\n    at computeTotal (/srv/app/server.js:4:2)".into(),
            request: Some(CapturedRequest {
                method: "POST".into(),
                path: "/api/checkout".into(),
                content_type: None,
                body: Some("{}".into()),
            }),
            intake: Intake::Snippet,
        };
        for at in [1_000, 2_000, 3_000] {
            engine_record::append(&path, "event", &ev(at), at).unwrap();
        }
        let opened_stripped = rebuild_opened_signatures(&path, 3, 60_000, "/srv/app");
        assert_eq!(opened_stripped[0].top_frame_file, "server.js");
        // `ErrorSignature::from_error`'s own contract (engine-core): an
        // empty `app_root` matches every string as a prefix and then trims
        // the leading `/` from what's left — so "no app_root configured"
        // still yields a leading-slash-free path, just not stripped of the
        // deployment directory itself. This is engine-core's existing,
        // deliberate behavior; this test just pins that `rebuild_opened_signatures`
        // passes `app_root` through to it unchanged rather than reinterpreting it.
        let opened_unstripped = rebuild_opened_signatures(&path, 3, 60_000, "");
        assert_eq!(opened_unstripped[0].top_frame_file, "srv/app/server.js");
        assert_ne!(
            opened_stripped[0], opened_unstripped[0],
            "a different app_root must reconstruct a different signature"
        );
    }
}
