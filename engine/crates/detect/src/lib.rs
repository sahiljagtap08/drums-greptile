//! Threshold failure detection: N matching error signatures inside a sliding
//! window open a Failure with an `observed` claim (spec §7 "checkout failing").

pub mod observe;

use std::collections::HashMap;

use engine_core::{Claim, ErrorEvent, ErrorSignature, Failure, Intake, Provenance};

/// Per-signature tracking state: recent occurrence timestamps plus the
/// latest-by-`occurred_at_ms` event seen so far (order-independent of arrival).
#[derive(Default)]
struct SignatureState {
    times: Vec<u64>,
    latest_event: Option<ErrorEvent>,
}

pub struct Detector {
    threshold: usize,
    window_ms: u64,
    app_root: String,
    /// per-signature window state, keyed by signature
    seen: HashMap<ErrorSignature, SignatureState>,
    opened: HashMap<ErrorSignature, ()>,
}

impl Detector {
    pub fn new(threshold: usize, window_ms: u64, app_root: String) -> Self {
        Self { threshold, window_ms, app_root, seen: HashMap::new(), opened: HashMap::new() }
    }

    /// Clear a signature's opened state so a future occurrence can open a
    /// new `Failure` for it (spec §22: "a bad repair must be re-detectable").
    /// Called after a `--repair auto` ship completes — the record shows what
    /// was tried, but detection itself must not stay permanently gated on a
    /// signature just because Drums already attempted a fix for it once.
    /// Also drops the accumulated occurrence-time window: a stale window
    /// from before the repair must not let one post-ship error instantly
    /// re-cross the threshold on leftover history.
    pub fn reopen(&mut self, sig: &ErrorSignature) {
        self.opened.remove(sig);
        self.seen.remove(sig);
    }

    /// Mark `sig` opened directly, bypassing `observe`'s threshold/window
    /// accumulation. Used only to seed a freshly-constructed `Detector` with
    /// state reconstructed elsewhere (see `drums_watch::restore`, which
    /// replays a process's own event history through a throwaway detector
    /// and reads its `opened_signatures()` back out) — the two-step split
    /// exists so replay logic can live outside this crate while still
    /// producing a detector that behaves EXACTLY as if it had lived through
    /// that history itself: `observe` on an already-`mark_opened` signature
    /// returns `None`, same as one this detector opened on its own, until an
    /// explicit `reopen`.
    pub fn mark_opened(&mut self, sig: ErrorSignature) {
        self.opened.insert(sig, ());
    }

    /// The signatures currently gated (opened and not yet reopened). Read by
    /// `drums_watch::restore` after replaying a record's `event` lines
    /// through a throwaway detector, to hand that reconstructed state to the
    /// real detector the engine actually runs with via `mark_opened`.
    pub fn opened_signatures(&self) -> Vec<ErrorSignature> {
        self.opened.keys().cloned().collect()
    }

    pub fn observe(&mut self, event: ErrorEvent) -> Option<Failure> {
        let sig = ErrorSignature::from_error(&event.error_name, &event.error_message, &event.stack, &self.app_root);
        if self.opened.contains_key(&sig) {
            return None;
        }
        let state = self.seen.entry(sig.clone()).or_default();
        state.times.push(event.occurred_at_ms);
        if state.latest_event.as_ref().is_none_or(|latest| event.occurred_at_ms >= latest.occurred_at_ms) {
            state.latest_event = Some(event);
        }
        let window_max = *state.times.iter().max().expect("just pushed");
        let cutoff = window_max.saturating_sub(self.window_ms);
        state.times.retain(|t| *t >= cutoff);
        if state.times.len() < self.threshold {
            return None;
        }
        let first_seen_ms = *state.times.iter().min().expect("non-empty window");
        let count = state.times.len();
        let sample = state.latest_event.clone().expect("latest event tracked");
        self.opened.insert(sig.clone(), ());
        // The failure's intake is the sample event's DECLARED intake, reconciled
        // against whether a replayable request actually arrived on it
        // (`Intake::resolve` only ever downgrades). This is the single place a
        // `Failure` acquires an intake in the live pipeline, which is what makes
        // "no adapter can accidentally produce a shippable failure from
        // unprovable input" a property of the code rather than a rule each
        // adapter has to remember.
        let intake = Intake::resolve(sample.intake.clone(), sample.request.is_some());
        Some(Failure {
            id: ulid::Ulid::new().to_string(),
            service: sample.service.clone(),
            signature: sig.clone(),
            first_seen_ms,
            event_count: count,
            claim: Claim {
                text: format!(
                    "{count} errors matching {} in {} within {}s",
                    sig.error_name,
                    sig.top_frame_file,
                    self.window_ms / 1000
                ),
                provenance: Provenance::Observed,
            },
            sample,
            intake,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_core::{CapturedRequest, ErrorEvent};

    fn event(at_ms: u64, name: &str) -> ErrorEvent {
        ErrorEvent {
            service: "shop".into(),
            occurred_at_ms: at_ms,
            error_name: name.into(),
            error_message: "boom".into(),
            stack: format!("{name}: boom\n    at computeTotal (/srv/shop/server.js:4:2)"),
            request: Some(CapturedRequest { method: "POST".into(), path: "/api/checkout".into(), content_type: None, body: Some("{}".into()) }),
            intake: Intake::Snippet,
        }
    }

    /// The same signature arriving with NO replayable request, declared by a
    /// trigger adapter — an OTel span or a HyperDX log alert.
    fn trigger_event(at_ms: u64, name: &str, source: &str) -> ErrorEvent {
        ErrorEvent {
            request: None,
            intake: Intake::Trigger { source: source.into() },
            ..event(at_ms, name)
        }
    }

    #[test]
    fn opens_failure_at_threshold_within_window() {
        let mut d = Detector::new(3, 60_000, "/srv/shop".into());
        assert!(d.observe(event(1_000, "TypeError")).is_none());
        assert!(d.observe(event(2_000, "TypeError")).is_none());
        let f = d.observe(event(3_000, "TypeError")).expect("failure at threshold");
        assert_eq!(f.event_count, 3);
        assert_eq!(f.first_seen_ms, 1_000);
        assert_eq!(f.claim.provenance, engine_core::Provenance::Observed);
        assert_eq!(f.sample.occurred_at_ms, 3_000); // latest event is the replay sample
    }

    #[test]
    fn a_snippet_failure_carries_a_replayable_intake() {
        let mut d = Detector::new(2, 60_000, "/srv/shop".into());
        d.observe(event(1_000, "TypeError"));
        let f = d.observe(event(2_000, "TypeError")).expect("failure at threshold");
        assert_eq!(f.intake, Intake::Snippet);
        assert!(f.replayable_request().is_some(), "the snippet path must still yield a replay candidate");
    }

    #[test]
    fn a_trigger_failure_carries_its_source_and_no_replay_candidate() {
        let mut d = Detector::new(2, 60_000, "/srv/shop".into());
        d.observe(trigger_event(1_000, "TypeError", "hyperdx"));
        let f = d.observe(trigger_event(2_000, "TypeError", "hyperdx")).expect("failure at threshold");
        assert_eq!(f.intake, Intake::Trigger { source: "hyperdx".into() });
        assert!(!f.intake.is_replayable());
        assert!(f.replayable_request().is_none(), "a trigger failure must never offer a request to replay");
    }

    /// An adapter that forgets to declare its intake but posts no request must
    /// not get a replayable failure out of the detector — the downgrade is
    /// applied here, at the one construction site, not left to the adapter.
    #[test]
    fn an_undeclared_intake_with_no_request_is_downgraded_to_trigger_unknown() {
        let mut d = Detector::new(2, 60_000, "/srv/shop".into());
        let undeclared = |at| ErrorEvent { request: None, intake: Intake::Snippet, ..event(at, "TypeError") };
        d.observe(undeclared(1_000));
        let f = d.observe(undeclared(2_000)).expect("failure at threshold");
        assert_eq!(f.intake, Intake::Trigger { source: engine_core::UNKNOWN_INTAKE_SOURCE.into() });
        assert!(f.replayable_request().is_none());
    }

    #[test]
    fn does_not_reopen_same_signature() {
        let mut d = Detector::new(2, 60_000, "/srv/shop".into());
        d.observe(event(1_000, "TypeError"));
        assert!(d.observe(event(2_000, "TypeError")).is_some());
        assert!(d.observe(event(3_000, "TypeError")).is_none());
    }

    #[test]
    fn events_outside_window_do_not_accumulate() {
        let mut d = Detector::new(3, 10_000, "/srv/shop".into());
        d.observe(event(1_000, "TypeError"));
        d.observe(event(2_000, "TypeError"));
        assert!(d.observe(event(50_000, "TypeError")).is_none(), "stale events dropped");
    }

    #[test]
    fn different_signatures_tracked_independently() {
        let mut d = Detector::new(2, 60_000, "/srv/shop".into());
        d.observe(event(1_000, "TypeError"));
        assert!(d.observe(event(1_500, "RangeError")).is_none());
        assert!(d.observe(event(2_000, "TypeError")).is_some());
    }

    #[test]
    fn reopen_lets_a_previously_opened_signature_open_again() {
        let mut d = Detector::new(2, 60_000, "/srv/shop".into());
        let f1 = d.observe(event(1_000, "TypeError"));
        let f2 = d.observe(event(1_500, "TypeError"));
        assert!(f1.is_none() && f2.is_some(), "opens once at threshold");
        assert!(d.observe(event(2_000, "TypeError")).is_none(), "stays gated until reopened");

        let sig = f2.unwrap().signature;
        d.reopen(&sig);

        assert!(d.observe(event(10_000, "TypeError")).is_none(), "one occurrence after reopen is below threshold again");
        let f3 = d.observe(event(10_500, "TypeError"));
        assert!(f3.is_some(), "reopened signature can open a new Failure once threshold is met again");
    }

    #[test]
    fn reopen_drops_stale_window_so_leftover_history_cannot_instantly_retrigger() {
        let mut d = Detector::new(3, 60_000, "/srv/shop".into());
        d.observe(event(1_000, "TypeError"));
        d.observe(event(2_000, "TypeError"));
        let f = d.observe(event(3_000, "TypeError")).expect("opens at threshold");
        d.reopen(&f.signature);

        // Only ONE new event after reopen: if the old window survived, this
        // would combine with leftover timestamps and instantly re-cross the
        // threshold (3), which must not happen.
        assert!(d.observe(event(4_000, "TypeError")).is_none(), "one post-reopen event alone must not retrigger");
    }

    #[test]
    fn mark_opened_gates_observe_exactly_like_a_live_threshold_crossing() {
        let mut d = Detector::new(3, 60_000, "/srv/shop".into());
        let sig = ErrorSignature { error_name: "TypeError".into(), top_frame_file: "server.js".into(), top_frame_function: Some("computeTotal".into()) };
        d.mark_opened(sig.clone());
        assert!(d.observe(event(1_000, "TypeError")).is_none(), "already-opened signature must not re-trigger, even at 1 occurrence");
        assert!(d.observe(event(2_000, "TypeError")).is_none());
        assert!(d.observe(event(3_000, "TypeError")).is_none(), "still gated even past the raw threshold count");
        assert!(d.opened_signatures().contains(&sig));

        d.reopen(&sig);
        assert!(d.observe(event(10_000, "TypeError")).is_none());
        assert!(d.observe(event(10_500, "TypeError")).is_none());
        assert!(d.observe(event(11_000, "TypeError")).is_some(), "reopen must still let it open again on a fresh threshold crossing");
    }

    #[test]
    fn opened_signatures_reflects_only_currently_gated_signatures() {
        let mut d = Detector::new(2, 60_000, "/srv/shop".into());
        assert!(d.opened_signatures().is_empty());
        let f = d.observe(event(1_000, "TypeError"));
        assert!(f.is_none());
        let f = d.observe(event(1_500, "TypeError")).expect("opens at threshold");
        let sigs = d.opened_signatures();
        assert_eq!(sigs, vec![f.signature.clone()]);

        d.reopen(&f.signature);
        assert!(d.opened_signatures().is_empty(), "reopen must remove it from the gated set");
    }

    #[test]
    fn reopen_on_a_signature_never_seen_is_a_harmless_no_op() {
        let mut d = Detector::new(2, 60_000, "/srv/shop".into());
        let never_seen = ErrorSignature { error_name: "RangeError".into(), top_frame_file: "x.js".into(), top_frame_function: None };
        d.reopen(&never_seen); // must not panic
        assert!(d.observe(event(1_000, "TypeError")).is_none());
    }

    #[test]
    fn out_of_order_arrival_still_detects_with_correct_first_seen_and_latest_sample() {
        let mut d = Detector::new(3, 60_000, "/srv/shop".into());
        assert!(d.observe(event(3_000, "TypeError")).is_none());
        assert!(d.observe(event(1_000, "TypeError")).is_none());
        let f = d.observe(event(2_000, "TypeError")).expect("failure at threshold");
        assert_eq!(f.first_seen_ms, 1_000);
        assert_eq!(f.event_count, 3);
        assert_eq!(f.sample.occurred_at_ms, 3_000);
    }

    #[test]
    fn stale_event_arriving_late_does_not_reset_window() {
        let mut d = Detector::new(3, 10_000, "/srv/shop".into());
        assert!(d.observe(event(50_000, "TypeError")).is_none());
        assert!(d.observe(event(51_000, "TypeError")).is_none());
        assert!(d.observe(event(1_000, "TypeError")).is_none(), "stale event outside window anchored at max must be dropped");
    }
}
