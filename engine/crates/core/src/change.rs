//! What was tried. The third field of `Observation → Hypothesis → Change →
//! Outcome`, and the one that makes the fourth possible.
//!
//! # A change cites an accepted hypothesis
//!
//! The chain is enforced at every link: an observation is a fact, a hypothesis
//! cites observations, and a change cites exactly one hypothesis — one that a
//! human **accepted** and that carries an evaluation plan. A change against a
//! rejected hypothesis is someone overriding a decision silently; a change
//! against a plan-less hypothesis is a change that can never be measured.
//! [`Change::new`] refuses both, by name.
//!
//! # The plan is snapshotted, and the baseline is taken at ship time
//!
//! The change carries a copy of the evaluation plan *as it stood when the
//! change shipped*, plus the baseline reading taken then. Both are frozen on
//! purpose: a window widened after the numbers disappoint, or a baseline
//! recomputed from a friendlier period, is how measurement becomes marketing.
//! The plan you ship under is the plan you are measured under.
//!
//! # The outcome is written once, when the window closes
//!
//! [`Change::due`] says when a change may be measured: not before its window
//! has fully elapsed. Measuring early flatters whichever direction the first
//! hours happened to lean. When it is due, `Outcome::measure` — the only
//! constructor, with all its refusals — decides what may be claimed, and the
//! outcome is appended beside the change, never written into it.

use serde::{Deserialize, Serialize};

use crate::evaluation::{EvaluationTarget, Outcome, Sample};
use crate::hypothesis::{Hypothesis, HypothesisId, Status as HypothesisStatus};
use crate::Provenance;

/// The record `kind` a [`Change`] is appended under.
pub const RECORD_KIND: &str = "change";
/// The record `kind` an [`OutcomeRecorded`] line is appended under.
pub const OUTCOME_KIND: &str = "outcome";
/// The record `kind` a [`Revisit`] is appended under.
pub const REVISIT_KIND: &str = "revisit";

/// When the slow loop re-reads a matured outcome: 7, 30, and 90 days after
/// ship. A horizon that does not reach past the plan's own window is skipped —
/// at equality it would duplicate the original outcome line, and below it the
/// "later" window would begin before the ship it claims to follow.
pub const REVISIT_HORIZONS_DAYS: [u32; 3] = [7, 30, 90];

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ChangeId(pub String);

impl std::fmt::Display for ChangeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why a change could not be created. Each arm is a sentence a person can act
/// on — the same refusal discipline as everywhere else in the loop.
#[derive(Debug, PartialEq)]
pub enum ChangeRefused {
    /// The hypothesis id matches nothing in the record.
    UnknownHypothesis(String),
    /// The hypothesis exists but its current status is not `Accepted`.
    NotAccepted { id: String, status: String },
    /// The hypothesis has no evaluation plan, so nothing could ever be
    /// measured against this change.
    NoPlan(String),
}

impl std::fmt::Display for ChangeRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChangeRefused::UnknownHypothesis(id) => write!(
                f,
                "hypothesis {id:?} is not in this repo's record — `drums record` lists what is"
            ),
            ChangeRefused::NotAccepted { id, status } => write!(
                f,
                "hypothesis {id:?} is {status}, not accepted — `drums hypothesis accept {id}` is the deliberate step this refusal protects"
            ),
            ChangeRefused::NoPlan(id) => write!(
                f,
                "hypothesis {id:?} carries no evaluation plan — a change with no declared outcome can never be measured; re-propose with --name --start --success --metric"
            ),
        }
    }
}

/// A shipped change, frozen with the terms it will be judged under.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Change {
    pub id: ChangeId,
    /// Exactly one, accepted, with a plan — enforced by [`Change::new`].
    pub hypothesis: HypothesisId,
    /// The commit that carries the change.
    pub sha: String,
    pub shipped_at_ms: u64,
    /// The evaluation plan as it stood at ship time. Frozen: the plan you ship
    /// under is the plan you are measured under.
    pub plan: EvaluationTarget,
    /// The reading taken at ship time, with its denominator. Frozen for the
    /// same reason the plan is.
    pub baseline: Sample,
}

impl Change {
    /// Build a change against the record's view of a hypothesis. `status` is
    /// the *folded* status ([`Hypothesis::current_status`]) — the row's own
    /// field is stale the moment an accept/reject line lands after it.
    pub fn new(
        id: impl Into<String>,
        hypothesis: &Hypothesis,
        folded_status: Option<HypothesisStatus>,
        sha: impl Into<String>,
        baseline: Sample,
        shipped_at_ms: u64,
    ) -> Result<Change, ChangeRefused> {
        match folded_status {
            Some(HypothesisStatus::Accepted) => {}
            Some(HypothesisStatus::Open) | None => {
                return Err(ChangeRefused::NotAccepted {
                    id: hypothesis.id.0.clone(),
                    status: "open".into(),
                })
            }
            Some(HypothesisStatus::Rejected { .. }) => {
                return Err(ChangeRefused::NotAccepted {
                    id: hypothesis.id.0.clone(),
                    status: "rejected".into(),
                })
            }
        }
        let Some(plan) = hypothesis.plan.clone() else {
            return Err(ChangeRefused::NoPlan(hypothesis.id.0.clone()));
        };
        Ok(Change {
            id: ChangeId(id.into()),
            hypothesis: hypothesis.id.clone(),
            sha: sha.into(),
            shipped_at_ms,
            plan,
            baseline,
        })
    }

    /// When this change may be measured: only once its window has fully
    /// elapsed. Measuring early flatters whichever direction the first hours
    /// leaned.
    pub fn due(&self, now_ms: u64) -> bool {
        let window_ms = (self.plan.window.days as u64).saturating_mul(86_400_000);
        now_ms >= self.shipped_at_ms.saturating_add(window_ms)
    }

    /// All changes in a decoded record.
    pub fn all<'a>(
        lines: impl IntoIterator<Item = &'a (String, serde_json::Value)>,
    ) -> Vec<Change> {
        lines
            .into_iter()
            .filter(|(kind, _)| kind == RECORD_KIND)
            .filter_map(|(_, v)| serde_json::from_value::<Change>(v.clone()).ok())
            .collect()
    }

    /// The window a revisit at `horizon_days` re-reads: the plan's own width,
    /// ending at ship + horizon. The 30-day revisit of a 7d-window change
    /// reads (ship+23d, ship+30d) — the same yardstick, held against a later
    /// slice of reality, compared against the same frozen baseline.
    pub fn revisit_window(&self, horizon_days: u32) -> (u64, u64) {
        let to = self
            .shipped_at_ms
            .saturating_add((horizon_days as u64).saturating_mul(86_400_000));
        let from = to.saturating_sub((self.plan.window.days as u64).saturating_mul(86_400_000));
        (from, to)
    }

    /// When a revisit at `horizon_days` may be taken: only once ship + horizon
    /// has FULLY elapsed — the same no-early-reads rule as [`Change::due`].
    pub fn revisit_due(&self, horizon_days: u32, now_ms: u64) -> bool {
        let horizon_ms = (horizon_days as u64).saturating_mul(86_400_000);
        now_ms >= self.shipped_at_ms.saturating_add(horizon_ms)
    }

    /// The horizons this change will ever be revisited at: 7/30/90, minus any
    /// horizon the plan's own window already covers. A horizon equal to the
    /// window would duplicate the original outcome; one below it would read a
    /// window that starts before the ship.
    pub fn revisit_horizons(&self) -> impl Iterator<Item = u32> + '_ {
        REVISIT_HORIZONS_DAYS
            .into_iter()
            .filter(|h| *h > self.plan.window.days)
    }
}

/// The outcome line appended beside a change when its window closes. The
/// change itself never mutates — same append-only rule as every other object
/// in the loop.
///
/// `unread_guardrails` is the honesty field: `Outcome::measure` does not
/// evaluate a guardrail nobody supplied a reading for, and a verdict that
/// stayed silent about which guardrails were actually read would let "held"
/// mean "unwatched". Anything listed here was declared in the plan and NOT
/// measured, and every rendering of this outcome is expected to say so.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutcomeRecorded {
    pub change: ChangeId,
    pub measured_at_ms: u64,
    #[serde(flatten)]
    pub outcome: Outcome,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unread_guardrails: Vec<String>,
}

impl OutcomeRecorded {
    /// The provenance the outcome itself earned: `verified` only for a
    /// measured comparison, `observed` for shipped-but-unmeasured.
    pub fn provenance(&self) -> Provenance {
        self.outcome.provenance()
    }

    /// The outcome for one change, if its line has landed.
    pub fn for_change<'a>(
        lines: impl IntoIterator<Item = &'a (String, serde_json::Value)>,
        change: &ChangeId,
    ) -> Option<OutcomeRecorded> {
        lines
            .into_iter()
            .filter(|(kind, _)| kind == OUTCOME_KIND)
            .filter_map(|(_, v)| serde_json::from_value::<OutcomeRecorded>(v.clone()).ok())
            .filter(|o| &o.change == change)
            .last()
    }
}

/// The slow loop's line: the same metric, the same frozen plan, the same
/// baseline, re-read over a later window ending at ship + horizon days — and
/// appended BESIDE the original outcome, never over it. A verdict is what was
/// measured *then*; the revisit is what reality says *now*, and both stand.
///
/// A revisit is DIRECT_MEASUREMENT — the highest authority class in the
/// belief-conflict rule (authority × evidence quality × scope × temporal
/// relevance, never recency alone) — which is why it may legitimately change
/// future guidance while never editing the past: the original verdict remains
/// the honest record of what the declared window showed.
///
/// A revisit only exists for a change that HAS a recorded outcome, and each
/// (change, horizon) pair is measured at most once — this line's presence in
/// the record is the guard.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Revisit {
    pub change: ChangeId,
    /// Days after ship the re-read window ends: one of
    /// [`REVISIT_HORIZONS_DAYS`], always past the plan's own window.
    pub horizon_days: u32,
    pub measured_at_ms: u64,
    /// The later window's reading, decided by `Outcome::measure` under the
    /// same refusals as the original — flattened, the same serde shape the
    /// outcome line uses.
    #[serde(flatten)]
    pub outcome: Outcome,
}

impl Revisit {
    /// Same rule as the outcome's: `verified` only for a measured comparison.
    pub fn provenance(&self) -> Provenance {
        self.outcome.provenance()
    }

    /// Whether this revisit's direction differs from the original outcome's.
    /// Only two measured readings can disagree about direction — an
    /// unmeasured revisit (or an unmeasured original) is never "drifted",
    /// because absence of a reading is not a reading.
    pub fn drifted_from(&self, original: &OutcomeRecorded) -> bool {
        match (&self.outcome, &original.outcome) {
            (
                Outcome::Measured { direction: now, .. },
                Outcome::Measured {
                    direction: then, ..
                },
            ) => now != then,
            _ => false,
        }
    }

    /// Every revisit recorded for one change, in record order.
    pub fn for_change<'a>(
        lines: impl IntoIterator<Item = &'a (String, serde_json::Value)>,
        change: &ChangeId,
    ) -> Vec<Revisit> {
        lines
            .into_iter()
            .filter(|(kind, _)| kind == REVISIT_KIND)
            .filter_map(|(_, v)| serde_json::from_value::<Revisit>(v.clone()).ok())
            .filter(|r| &r.change == change)
            .collect()
    }

    /// The at-most-once guard: has this (change, horizon) pair already been
    /// revisited? Idempotent and restart-safe because the record line itself
    /// is the memory, not any process state.
    pub fn recorded<'a>(
        lines: impl IntoIterator<Item = &'a (String, serde_json::Value)>,
        change: &ChangeId,
        horizon_days: u32,
    ) -> bool {
        Revisit::for_change(lines, change)
            .iter()
            .any(|r| r.horizon_days == horizon_days)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evaluation::{EvaluationTarget, Metric};
    use crate::observation::ObservationId;

    fn accepted_hypothesis_with_plan() -> Hypothesis {
        Hypothesis::new(
            "hyp_1",
            "the promo branch assumes a fresh cart",
            vec![ObservationId("obs_1".into())],
            1,
        )
        .unwrap()
        .with_plan(EvaluationTarget::new(
            "eval_1",
            "Checkout",
            "checkout_started",
            "payment_confirmed",
            Metric::CompletionRate,
        ))
    }

    #[test]
    fn a_change_against_an_unaccepted_hypothesis_is_refused() {
        let h = accepted_hypothesis_with_plan();
        let base = Sample {
            value: 0.6,
            entries: 300,
        };
        for (status, word) in [
            (Some(HypothesisStatus::Open), "open"),
            (None, "open"),
            (
                Some(HypothesisStatus::Rejected {
                    reason: "bot traffic".into(),
                }),
                "rejected",
            ),
        ] {
            let err = Change::new("chg_1", &h, status, "abc123", base, 10).unwrap_err();
            match err {
                ChangeRefused::NotAccepted { status, .. } => assert_eq!(status, word),
                other => panic!("wrong refusal: {other:?}"),
            }
        }
    }

    #[test]
    fn a_change_against_a_planless_hypothesis_is_refused() {
        let h = Hypothesis::new("hyp_2", "x", vec![ObservationId("obs_1".into())], 1).unwrap();
        let err = Change::new(
            "chg_1",
            &h,
            Some(HypothesisStatus::Accepted),
            "abc",
            Sample {
                value: 0.0,
                entries: 0,
            },
            10,
        )
        .unwrap_err();
        assert!(matches!(err, ChangeRefused::NoPlan(_)));
        // And the refusal sentence names the fix.
        assert!(err
            .to_string()
            .contains("--name --start --success --metric"));
    }

    #[test]
    fn the_plan_is_snapshotted_at_ship_time() {
        let h = accepted_hypothesis_with_plan();
        let c = Change::new(
            "chg_1",
            &h,
            Some(HypothesisStatus::Accepted),
            "abc123",
            Sample {
                value: 0.61,
                entries: 340,
            },
            10,
        )
        .unwrap();
        assert_eq!(c.plan.metric, Metric::CompletionRate);
        assert_eq!(
            c.baseline.entries, 340,
            "baseline frozen with its denominator"
        );
        assert_eq!(c.hypothesis, h.id);
    }

    #[test]
    fn a_change_is_not_due_until_its_window_fully_elapses() {
        let h = accepted_hypothesis_with_plan(); // default window: 7 days
        let shipped = 1_000_000_000_000u64;
        let c = Change::new(
            "chg_1",
            &h,
            Some(HypothesisStatus::Accepted),
            "abc",
            Sample {
                value: 0.6,
                entries: 300,
            },
            shipped,
        )
        .unwrap();
        let almost = shipped + 7 * 86_400_000 - 1;
        assert!(!c.due(almost), "measuring early flatters the first hours");
        assert!(c.due(almost + 1));
    }

    #[test]
    fn the_outcome_line_round_trips_with_its_unread_guardrails() {
        let o = OutcomeRecorded {
            change: ChangeId("chg_1".into()),
            measured_at_ms: 99,
            outcome: Outcome::measure(
                &accepted_hypothesis_with_plan().plan.unwrap(),
                Sample {
                    value: 0.61,
                    entries: 340,
                },
                Sample {
                    value: 0.78,
                    entries: 350,
                },
                &[],
            ),
            unread_guardrails: vec!["support contacts".into()],
        };
        let lines = [(OUTCOME_KIND.to_string(), serde_json::to_value(&o).unwrap())];
        let back = OutcomeRecorded::for_change(lines.iter(), &ChangeId("chg_1".into())).unwrap();
        assert_eq!(back, o);
        assert_eq!(back.provenance(), Provenance::Verified);
        assert_eq!(back.unread_guardrails, vec!["support contacts"]);
        // A change nothing measured yet has no outcome — absence, not a blank.
        assert!(OutcomeRecorded::for_change(lines.iter(), &ChangeId("chg_other".into())).is_none());
    }

    #[test]
    fn a_revisit_window_is_the_plans_width_ending_at_ship_plus_horizon() {
        let h = accepted_hypothesis_with_plan(); // 7-day window
        let ship = 1_000 * 86_400_000u64;
        let c = Change::new(
            "chg_1",
            &h,
            Some(HypothesisStatus::Accepted),
            "abc",
            Sample {
                value: 0.6,
                entries: 300,
            },
            ship,
        )
        .unwrap();
        const DAY: u64 = 86_400_000;
        assert_eq!(
            c.revisit_window(30),
            (ship + 23 * DAY, ship + 30 * DAY),
            "the 30-day revisit of a 7d-window change reads (ship+23d, ship+30d)"
        );
        assert_eq!(c.revisit_window(90), (ship + 83 * DAY, ship + 90 * DAY));
        // Due only once ship + horizon has FULLY elapsed.
        assert!(
            !c.revisit_due(30, ship + 30 * DAY - 1),
            "one millisecond early is early"
        );
        assert!(c.revisit_due(30, ship + 30 * DAY));
    }

    #[test]
    fn horizons_the_plans_window_already_covers_are_never_revisited() {
        let ship = 1_000u64;
        let mk = |days: u32| {
            let h = Hypothesis::new("hyp_1", "x", vec![ObservationId("obs_1".into())], 1)
                .unwrap()
                .with_plan(
                    EvaluationTarget::new("e", "Errors", "a", "b", Metric::ErrorEventRate)
                        .with_window(crate::evaluation::MeasurementWindow {
                            days,
                            min_entries: 1,
                            min_effect: 0.1,
                        }),
                );
            Change::new(
                "chg_1",
                &h,
                Some(HypothesisStatus::Accepted),
                "abc",
                Sample {
                    value: 0.6,
                    entries: 300,
                },
                ship,
            )
            .unwrap()
        };
        // A 7d window: the 7d horizon would duplicate the original outcome.
        assert_eq!(mk(7).revisit_horizons().collect::<Vec<_>>(), vec![30, 90]);
        // A 14d window: the 7d horizon's window would start before ship.
        assert_eq!(mk(14).revisit_horizons().collect::<Vec<_>>(), vec![30, 90]);
        assert_eq!(mk(30).revisit_horizons().collect::<Vec<_>>(), vec![90]);
        assert_eq!(mk(90).revisit_horizons().count(), 0);
    }

    #[test]
    fn drift_is_a_direction_change_never_new_numbers_and_never_an_unmeasured_side() {
        let plan = accepted_hypothesis_with_plan().plan.unwrap();
        let measured = |from: f64, to: f64| Outcome::Measured {
            direction: {
                let imp = plan.metric.improvement(to - from);
                if imp >= plan.window.min_effect {
                    crate::evaluation::Direction::Positive
                } else if imp <= -plan.window.min_effect {
                    crate::evaluation::Direction::Negative
                } else {
                    crate::evaluation::Direction::Neutral
                }
            },
            from,
            to,
            entries: 300,
            guardrails: crate::evaluation::Guardrails::Held,
        };
        let original = |o: Outcome| OutcomeRecorded {
            change: ChangeId("chg_1".into()),
            measured_at_ms: 1,
            outcome: o,
            unread_guardrails: vec![],
        };
        let revisit = |o: Outcome| Revisit {
            change: ChangeId("chg_1".into()),
            horizon_days: 30,
            measured_at_ms: 2,
            outcome: o,
        };
        // Direction changed: drifted.
        assert!(revisit(measured(0.61, 0.62)).drifted_from(&original(measured(0.61, 0.78))));
        // Neutral → neutral over different values is not drift.
        assert!(!revisit(measured(0.61, 0.615)).drifted_from(&original(measured(0.61, 0.605))));
        // An unmeasured revisit is never drifted.
        let thin = Outcome::Unmeasured(crate::evaluation::Unmeasured::NotEnoughTraffic {
            entries: 3,
            needed: 100,
        });
        assert!(!revisit(thin.clone()).drifted_from(&original(measured(0.61, 0.78))));
        // Nor is a revisit of an outcome that was never measured.
        assert!(!revisit(measured(0.61, 0.78)).drifted_from(&original(thin)));
    }

    #[test]
    fn a_revisit_lands_beside_the_outcome_and_guards_its_own_horizon() {
        let plan = accepted_hypothesis_with_plan().plan.unwrap();
        let r = Revisit {
            change: ChangeId("chg_1".into()),
            horizon_days: 30,
            measured_at_ms: 99,
            outcome: Outcome::measure(
                &plan,
                Sample {
                    value: 0.61,
                    entries: 340,
                },
                Sample {
                    value: 0.63,
                    entries: 350,
                },
                &[],
            ),
        };
        let v = serde_json::to_value(&r).unwrap();
        assert!(
            v.get("kind").is_none(),
            "no top-level kind field to collide with the wrapper"
        );
        assert_eq!(
            v["state"], "measured",
            "the flattened outcome keeps the outcome line's own shape"
        );
        let lines = [(REVISIT_KIND.to_string(), v)];
        let back = Revisit::for_change(lines.iter(), &ChangeId("chg_1".into()));
        assert_eq!(back, vec![r.clone()]);
        assert_eq!(back[0].provenance(), Provenance::Verified);
        // The presence of the line is the at-most-once guard, per horizon.
        assert!(Revisit::recorded(
            lines.iter(),
            &ChangeId("chg_1".into()),
            30
        ));
        assert!(
            !Revisit::recorded(lines.iter(), &ChangeId("chg_1".into()), 90),
            "a different horizon is a different revisit"
        );
        assert!(!Revisit::recorded(
            lines.iter(),
            &ChangeId("chg_other".into()),
            30
        ));
    }

    #[test]
    fn a_change_line_survives_the_record_wrapper() {
        // The kind-collision class: Change has no field named "kind" and
        // OutcomeRecorded flattens Outcome whose tag is "state" — assert the
        // wrapper shapes stay collision-free.
        let h = accepted_hypothesis_with_plan();
        let c = Change::new(
            "chg_1",
            &h,
            Some(HypothesisStatus::Accepted),
            "abc",
            Sample {
                value: 0.6,
                entries: 300,
            },
            10,
        )
        .unwrap();
        let v = serde_json::to_value(&c).unwrap();
        assert!(
            v.get("kind").is_none(),
            "no top-level kind field to collide with the wrapper"
        );
        let lines = [(RECORD_KIND.to_string(), v)];
        assert_eq!(Change::all(lines.iter()).len(), 1);
    }
}
