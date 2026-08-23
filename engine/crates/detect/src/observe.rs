//! The first real observation producer: error-rate shifts, read from the
//! record Drums already keeps.
//!
//! # Why this producer is first
//!
//! Every other observation source needs something from the customer — a
//! PostHog connection, behavioral events, a declared target. This one needs
//! nothing: `drums watch` already appends every error event and every deploy
//! to the record, so "error events for this signature went from A per hour to
//! B per hour after deploy X" is computable from data Drums already holds, on
//! every installed repo, with zero new configuration. The first entries in
//! `Observation → Hypothesis → Change → Outcome` should come from evidence
//! nobody had to be asked for.
//!
//! # What it deliberately is not
//!
//! Not detection. `Detector` opens a `Failure` when a signature crosses a
//! threshold — that is the repair path. This records a *fact about a window*
//! for the improvement loop: no severity, no diagnosis, and `since_deploy` is
//! a correlation in the type's own words, never a cause. The hypothesis that
//! cites it is where blame is allowed to be proposed.
//!
//! # Honesty rules, enforced here
//!
//! - **Thin evidence says nothing.** Fewer than [`RateShiftParams::min_after_events`]
//!   events after the deploy produces no observation at all — two errors are
//!   an anecdote, and an anecdote written down as a rate shift is a confident
//!   wrong claim.
//! - **Small moves say nothing.** The after-rate must exceed the before-rate
//!   by [`RateShiftParams::min_ratio`]; without a declared minimum effect,
//!   every wobble becomes an observation and the record becomes noise.
//! - **Deterministic and idempotent.** IDs are derived from (deploy, signature),
//!   the caller supplies `now_ms`, and anything already in the record is not
//!   re-emitted — running the producer twice appends nothing new.

use std::collections::HashMap;

use engine_core::evaluation::{Metric, Sample};
use engine_core::observation::{
    EvidenceKind, EvidenceRef, Kind, Measure, Observation, ScopeRef, Source, Window,
};
use engine_core::{DeployRecord, ErrorEvent, ErrorSignature};

/// The service name `drums init`'s wire-check event reports under (see the
/// CLI's `wire::verify_round_trip`). A self-test proves the pipe works; it is
/// not a production error, so every reading in this module excludes it — a
/// rate or a shift computed over install self-checks would be a claim about
/// traffic that never happened.
pub const SELF_TEST_SERVICE: &str = "drums-init-verify";

/// Tuning for the shift test. Defaults are deliberately conservative: the
/// first observations a customer ever sees must not be wobbles.
#[derive(Debug, Clone)]
pub struct RateShiftParams {
    /// How far back before the deploy the baseline is read.
    pub before_window_ms: u64,
    /// How long after the deploy counts toward the after-rate. Clamped to
    /// "now" — the window that has not elapsed yet is not counted as silence.
    pub after_window_ms: u64,
    /// Events required after the deploy before anything is said.
    pub min_after_events: u32,
    /// The after-rate must be at least this multiple of the before-rate.
    pub min_ratio: f64,
}

impl Default for RateShiftParams {
    fn default() -> Self {
        RateShiftParams {
            before_window_ms: 7 * 24 * 3_600_000,
            after_window_ms: 24 * 3_600_000,
            min_after_events: 5,
            min_ratio: 3.0,
        }
    }
}

const HOUR_MS: f64 = 3_600_000.0;

/// Stable, content-derived observation id: same deploy + same signature →
/// same id, which is what makes re-running the producer idempotent against
/// the record. FNV-1a, because the requirement is stability, not security.
fn shift_id(sha: &str, sig: &ErrorSignature) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in sig
        .error_name
        .bytes()
        .chain([0u8])
        .chain(sig.top_frame_file.bytes())
    {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    let short: String = sha.chars().take(12).collect();
    format!("obs_rshift_{short}_{h:016x}")
}

/// Read error-rate shifts out of decoded record lines.
///
/// `lines` is what `engine_record::read_all` returned; `app_root` is the same
/// prefix the detector uses, so this producer and detection agree on what a
/// signature is.
pub fn rate_shifts(
    lines: &[(String, serde_json::Value)],
    app_root: &str,
    now_ms: u64,
    params: &RateShiftParams,
) -> Vec<Observation> {
    // The deploys, oldest first. A record with no deploy has nothing to
    // correlate against — honestly, silence.
    let mut deploys: Vec<DeployRecord> = lines
        .iter()
        .filter(|(kind, _)| kind == "deploy")
        .filter_map(|(_, v)| serde_json::from_value::<DeployRecord>(v.clone()).ok())
        .collect();
    deploys.sort_by_key(|d| d.deployed_at_ms);
    let Some(latest) = deploys.last().cloned() else {
        return Vec::new();
    };

    // Event timestamps per signature. The record's event lines are redacted
    // ErrorEvents; the signature is derived exactly the way detection derives
    // it, so an observation and a failure about the same errors agree on
    // identity.
    let mut per_sig: HashMap<ErrorSignature, Vec<u64>> = HashMap::new();
    for (_, v) in lines.iter().filter(|(kind, _)| kind == "event") {
        let Ok(e) = serde_json::from_value::<ErrorEvent>(v.clone()) else {
            continue;
        };
        if e.service == SELF_TEST_SERVICE {
            continue;
        }
        let sig = ErrorSignature::from_error(&e.error_name, &e.error_message, &e.stack, app_root);
        per_sig.entry(sig).or_default().push(e.occurred_at_ms);
    }

    // Everything already observed, so a second run emits nothing new.
    let existing: std::collections::HashSet<String> =
        Observation::all(lines.iter()).into_iter().map(|o| o.id.0).collect();

    let deploy_at = latest.deployed_at_ms;
    // The after-window is clamped to what has actually elapsed: a deploy ten
    // minutes old is judged on ten minutes, not debited a day of silence.
    let after_end = now_ms.min(deploy_at.saturating_add(params.after_window_ms));
    if after_end <= deploy_at {
        return Vec::new();
    }
    let after_hours = (after_end - deploy_at) as f64 / HOUR_MS;
    let before_start = deploy_at.saturating_sub(params.before_window_ms);
    let before_hours = (deploy_at - before_start) as f64 / HOUR_MS;
    if before_hours <= 0.0 {
        return Vec::new();
    }

    let mut found = Vec::new();
    for (sig, times) in per_sig {
        let after: u32 = times.iter().filter(|t| **t >= deploy_at && **t < after_end).count() as u32;
        if after < params.min_after_events {
            continue;
        }
        let before: u32 =
            times.iter().filter(|t| **t >= before_start && **t < deploy_at).count() as u32;
        let before_rate = before as f64 / before_hours;
        let after_rate = after as f64 / after_hours;
        // A brand-new error class (before_rate == 0) is always a shift once it
        // clears the volume floor; an existing one must clear the ratio.
        if before_rate > 0.0 && after_rate < params.min_ratio * before_rate {
            continue;
        }

        let id = shift_id(&latest.sha, &sig);
        if existing.contains(&id) {
            continue;
        }
        let Some(window) = Window::new(before_start, after_end) else { continue };

        let mut scope = vec![ScopeRef::Release(latest.sha.clone())];
        if !sig.top_frame_file.is_empty() {
            scope.push(ScopeRef::Code(sig.top_frame_file.clone()));
        }
        found.push(
            Observation::fact(
                id,
                Source::Runtime,
                Kind::RateShift {
                    previous: before_rate,
                    // Correlation, in the type's own words. Attribution is the
                    // repair path's job and carries its own evidence.
                    since_deploy: Some(latest.sha.clone()),
                },
                window,
                now_ms,
            )
            .with_scope(scope)
            .with_evidence([EvidenceRef {
                source: Source::Runtime,
                kind: EvidenceKind::Deploy,
                id: latest.sha.clone(),
            }])
            .with_measure(Measure {
                metric: Metric::ErrorEventRate,
                sample: Sample { value: after_rate, entries: after },
            }),
        );
    }
    // Deterministic output order — HashMap iteration must not decide what the
    // record looks like.
    found.sort_by(|a, b| a.id.0.cmp(&b.id.0));
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: u64 = 3_600_000;
    const DEPLOY_AT: u64 = 1_000 * HOUR;

    fn deploy_line(sha: &str, at: u64) -> (String, serde_json::Value) {
        (
            "deploy".to_string(),
            serde_json::json!({
                "sha": sha, "description": "d", "author": "a", "deployed_at_ms": at
            }),
        )
    }

    fn event_line(name: &str, file: &str, at: u64) -> (String, serde_json::Value) {
        (
            "event".to_string(),
            serde_json::json!({
                "service": "api",
                "occurred_at_ms": at,
                "error_name": name,
                "error_message": "boom",
                "stack": format!("{name}: boom\n    at handler (/app/{file}:10:5)"),
            }),
        )
    }

    /// A burst after the deploy against a quiet week before.
    fn burst() -> Vec<(String, serde_json::Value)> {
        let mut lines = vec![deploy_line("8f32a1c00000", DEPLOY_AT)];
        // one stray event three days before — before_rate barely above zero
        lines.push(event_line("TypeError", "checkout.js", DEPLOY_AT - 72 * HOUR));
        // eight events in the two hours after
        for i in 0..8 {
            lines.push(event_line("TypeError", "checkout.js", DEPLOY_AT + 10 + i * 900_000));
        }
        lines
    }

    #[test]
    fn a_burst_after_a_deploy_becomes_one_rate_shift_observation() {
        let obs = rate_shifts(&burst(), "/app", DEPLOY_AT + 24 * HOUR, &RateShiftParams::default());
        assert_eq!(obs.len(), 1);
        let o = &obs[0];
        match &o.kind {
            Kind::RateShift { previous, since_deploy } => {
                assert!(*previous < 0.01, "one event over a week is nearly zero");
                assert_eq!(since_deploy.as_deref(), Some("8f32a1c00000"));
            }
            other => panic!("wrong kind: {other:?}"),
        }
        let m = o.measure.expect("a rate shift carries its measure");
        assert_eq!(m.metric, Metric::ErrorEventRate);
        assert_eq!(m.sample.entries, 8, "the count it was computed from");
        // 8 events over the 24h window = 1/3 per hour.
        assert!((m.sample.value - 8.0 / 24.0).abs() < 1e-9);
        // Scoped to the release and the code surface; affected users unknown,
        // never zero.
        assert!(o.scope.contains(&ScopeRef::Release("8f32a1c00000".into())));
        assert!(o.scope.contains(&ScopeRef::Code("checkout.js".into())));
        assert_eq!(o.affected.users, None);
        assert_eq!(o.source, Source::Runtime);
    }

    #[test]
    fn running_twice_emits_nothing_new() {
        let mut lines = burst();
        let first = rate_shifts(&lines, "/app", DEPLOY_AT + 24 * HOUR, &RateShiftParams::default());
        assert_eq!(first.len(), 1);
        // Append what the first run found, the way the engine would.
        for o in &first {
            lines.push((
                engine_core::observation::RECORD_KIND.to_string(),
                serde_json::to_value(o).unwrap(),
            ));
        }
        let second = rate_shifts(&lines, "/app", DEPLOY_AT + 25 * HOUR, &RateShiftParams::default());
        assert!(second.is_empty(), "idempotence against the record");
    }

    #[test]
    fn too_few_events_after_the_deploy_says_nothing() {
        let mut lines = vec![deploy_line("8f32a1c00000", DEPLOY_AT)];
        for i in 0..4 {
            lines.push(event_line("TypeError", "checkout.js", DEPLOY_AT + 10 + i * 60_000));
        }
        let obs = rate_shifts(&lines, "/app", DEPLOY_AT + 24 * HOUR, &RateShiftParams::default());
        assert!(obs.is_empty(), "four events are an anecdote, not a rate");
    }

    #[test]
    fn a_steady_error_rate_is_not_a_shift() {
        // ~1/hour before AND after: volume clears the floor, ratio does not.
        let mut lines = vec![deploy_line("8f32a1c00000", DEPLOY_AT)];
        for i in 0..168 {
            lines.push(event_line("TypeError", "checkout.js", DEPLOY_AT - (i + 1) * HOUR + 5));
        }
        for i in 0..24 {
            lines.push(event_line("TypeError", "checkout.js", DEPLOY_AT + i * HOUR + 5));
        }
        let obs = rate_shifts(&lines, "/app", DEPLOY_AT + 24 * HOUR, &RateShiftParams::default());
        assert!(obs.is_empty(), "steady is not a shift");
    }

    #[test]
    fn a_young_deploy_is_judged_on_elapsed_time_not_the_full_window() {
        // 6 events in the 30 minutes since deploying: 12/hour against a full
        // 24h window it would be 0.25/hour and might hide. Clamping the window
        // to elapsed time is what lets a fresh regression surface fast.
        let mut lines = vec![deploy_line("8f32a1c00000", DEPLOY_AT)];
        for i in 0..6 {
            lines.push(event_line("TypeError", "checkout.js", DEPLOY_AT + 60_000 + i * 240_000));
        }
        let obs =
            rate_shifts(&lines, "/app", DEPLOY_AT + 30 * 60_000, &RateShiftParams::default());
        assert_eq!(obs.len(), 1);
        let m = obs[0].measure.unwrap();
        assert!((m.sample.value - 12.0).abs() < 1e-9, "rate over 30 elapsed minutes");
    }

    #[test]
    fn signatures_shift_independently() {
        let mut lines = burst();
        // A second, steady signature must not ride the first one's shift.
        for i in 0..30 {
            lines.push(event_line("ValueError", "quote.py", DEPLOY_AT - (i + 1) * 5 * HOUR));
        }
        for i in 0..6 {
            lines.push(event_line("ValueError", "quote.py", DEPLOY_AT + i * 4 * HOUR));
        }
        let obs = rate_shifts(&lines, "/app", DEPLOY_AT + 24 * HOUR, &RateShiftParams::default());
        assert_eq!(obs.len(), 1, "only the burst shifted");
        assert!(obs[0].scope.contains(&ScopeRef::Code("checkout.js".into())));
    }

    #[test]
    fn no_deploy_means_no_correlation_and_no_observation() {
        let lines: Vec<_> = (0..10)
            .map(|i| event_line("TypeError", "checkout.js", DEPLOY_AT + i * 60_000))
            .collect();
        assert!(rate_shifts(&lines, "/app", DEPLOY_AT + HOUR, &RateShiftParams::default())
            .is_empty());
    }

    /// The `drums init` wire-check writes a real event line through the real
    /// ingest path — that is its whole point — and it must never read as
    /// production traffic afterwards.
    #[test]
    fn the_install_self_check_never_becomes_a_rate_shift() {
        let mut lines = vec![deploy_line("8f32a1c00000", DEPLOY_AT)];
        for i in 0..8 {
            lines.push((
                "event".to_string(),
                serde_json::json!({
                    "service": SELF_TEST_SERVICE,
                    "occurred_at_ms": DEPLOY_AT + 10 + i * 900_000,
                    "error_name": "DrumsWireCheck",
                    "error_message": "drums-wire-check-x",
                    "stack": "DrumsWireCheck: drums-wire-check-x\n    at drumsInitVerify (drums-report:1:1)",
                }),
            ));
        }
        let obs = rate_shifts(&lines, "/app", DEPLOY_AT + 24 * HOUR, &RateShiftParams::default());
        assert!(obs.is_empty(), "a self-test is not a burst of production errors: {obs:?}");
    }

    #[test]
    fn the_id_is_stable_and_the_output_order_is_deterministic() {
        let a = rate_shifts(&burst(), "/app", DEPLOY_AT + 24 * HOUR, &RateShiftParams::default());
        let b = rate_shifts(&burst(), "/app", DEPLOY_AT + 24 * HOUR, &RateShiftParams::default());
        assert_eq!(a[0].id, b[0].id, "same deploy + signature → same id");
        assert!(a[0].id.0.starts_with("obs_rshift_8f32a1c00000_"));
    }
}

#[cfg(test)]
mod record_shape_tests {
    use super::*;

    /// The regression that shipped a duplicate every tick: the record wrapper
    /// flattens the observation beside its own `"kind":"observation"`
    /// discriminator, and when the observation's variant field was ALSO named
    /// `kind`, one overwrote the other — read-back failed, dedup saw nothing,
    /// and the producer re-emitted forever. This test goes through the real
    /// `engine_record::append`/`read_all`, not a hand-built line, so the
    /// collision class cannot come back silently.
    #[test]
    fn idempotence_holds_through_the_real_record_wrapper() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        const HOUR: u64 = 3_600_000;
        let deploy_at: u64 = 1_000 * HOUR;

        engine_record::append(
            &path,
            "deploy",
            &serde_json::json!({
                "sha": "8f32a1c000000000", "description": "d", "author": "a",
                "deployed_at_ms": deploy_at
            }),
            deploy_at,
        )
        .unwrap();
        for i in 0..8u64 {
            engine_record::append(
                &path,
                "event",
                &serde_json::json!({
                    "service": "api", "occurred_at_ms": deploy_at + 10 + i * 900_000,
                    "error_name": "TypeError", "error_message": "boom",
                    "stack": "TypeError: boom\n    at handler (/app/lib/checkout.js:41:5)"
                }),
                deploy_at + 10 + i * 900_000,
            )
            .unwrap();
        }

        let read = engine_record::read_all(&path).unwrap();
        let first =
            rate_shifts(&read.lines, "/app", deploy_at + 24 * HOUR, &RateShiftParams::default());
        assert_eq!(first.len(), 1, "the burst is observed once");

        // Append it exactly the way the engine does.
        engine_record::append(
            &path,
            engine_core::observation::RECORD_KIND,
            &first[0],
            deploy_at + 24 * HOUR,
        )
        .unwrap();

        let read = engine_record::read_all(&path).unwrap();
        // The observation must survive its own round trip through the wrapper…
        let seen = Observation::all(read.lines.iter());
        assert_eq!(seen.len(), 1, "the recorded observation must be readable back");
        assert!(
            matches!(seen[0].kind, Kind::RateShift { .. }),
            "the variant must survive the wrapper: {:?}",
            seen[0].kind
        );
        // …and the producer must therefore emit nothing new.
        let second =
            rate_shifts(&read.lines, "/app", deploy_at + 25 * HOUR, &RateShiftParams::default());
        assert!(second.is_empty(), "idempotence through the real record shape");
    }
}

/// Total error-event rate over `[from_ms, to_ms)`, read from record lines.
///
/// The one reading Drums can take with zero customer configuration, which is
/// why it is the first metric the outcome pipeline can measure. Time is the
/// denominator: `entries` is the number of whole hours observed, so
/// `Outcome::measure`'s sample floor reads as "hours watched" for this metric
/// — a quiet week after a fix must not be refused as thin evidence, because
/// for an error rate, quiet IS the evidence.
pub fn error_event_rate(
    lines: &[(String, serde_json::Value)],
    from_ms: u64,
    to_ms: u64,
) -> Option<engine_core::evaluation::Sample> {
    if to_ms <= from_ms {
        return None;
    }
    let hours = (to_ms - from_ms) as f64 / HOUR_MS;
    let events = lines
        .iter()
        .filter(|(kind, _)| kind == "event")
        .filter_map(|(_, v)| serde_json::from_value::<engine_core::ErrorEvent>(v.clone()).ok())
        .filter(|e| e.service != SELF_TEST_SERVICE)
        .filter(|e| e.occurred_at_ms >= from_ms && e.occurred_at_ms < to_ms)
        .count();
    Some(engine_core::evaluation::Sample {
        value: events as f64 / hours,
        entries: hours.floor().max(0.0) as u32,
    })
}

#[cfg(test)]
mod rate_reading_tests {
    use super::*;

    #[test]
    fn the_rate_reading_counts_hours_as_its_denominator() {
        const HOUR: u64 = 3_600_000;
        let lines: Vec<_> = (0..12u64)
            .map(|i| {
                (
                    "event".to_string(),
                    serde_json::json!({
                        "service": "api", "occurred_at_ms": 1_000 * HOUR + i * 2 * HOUR,
                        "error_name": "E", "error_message": "m", "stack": "E: m"
                    }),
                )
            })
            .collect();
        let s = error_event_rate(&lines, 1_000 * HOUR, 1_024 * HOUR).unwrap();
        assert_eq!(s.entries, 24, "a day watched is 24 hours of denominator");
        assert!((s.value - 0.5).abs() < 1e-9, "12 events over 24h");
        // A perfectly quiet window is a zero rate over full hours — evidence,
        // not absence.
        let quiet = error_event_rate(&lines, 2_000 * HOUR, 2_024 * HOUR).unwrap();
        assert_eq!(quiet.value, 0.0);
        assert_eq!(quiet.entries, 24);
        // A backwards window is refused, not rendered.
        assert!(error_event_rate(&lines, 5, 4).is_none());
    }

    /// The install self-check must not move an `error_event_rate` reading —
    /// a baseline or an outcome that counts the wire-check is measuring the
    /// install, not the product.
    #[test]
    fn the_install_self_check_never_counts_toward_the_rate() {
        const HOUR: u64 = 3_600_000;
        let lines = vec![(
            "event".to_string(),
            serde_json::json!({
                "service": SELF_TEST_SERVICE, "occurred_at_ms": 1_001 * HOUR,
                "error_name": "DrumsWireCheck", "error_message": "m", "stack": "DrumsWireCheck: m"
            }),
        )];
        let s = error_event_rate(&lines, 1_000 * HOUR, 1_024 * HOUR).unwrap();
        assert_eq!(s.value, 0.0, "the self-check is not production traffic");
        assert_eq!(s.entries, 24, "the hours watched are unaffected");
    }
}
