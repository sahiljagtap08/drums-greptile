//! The evaluation target — what an intervention is measured against.
//!
//! # Where this sits after the 2026-08-11 correction
//!
//! This used to be called `Workflow`, and it used to be the root: every
//! observation required one, which meant Drums could not see anything a
//! customer had not configured first. The root is now the observation
//! (`crate::observation`), anchored to the product, and *this* object is what
//! it became: **the evaluation plan** — created when a hypothesis or a human
//! decides an intervention is worth measuring, or pre-declared by a team that
//! already knows what matters. It is the yardstick, not the door. Journeys —
//! the inferred multi-step paths Drums discovers in the product model — are a
//! different, future object; they carry no guardrails or windows and must not
//! reuse this type.
//!
//! # Why this is a struct and not a sentence
//!
//! Every product in this category asks the customer what matters, and most of
//! them ask for it as prose: a "business context" textarea holding something
//! like *"our primary conversion metric is activated workspaces"*. That is a
//! prompt, not an objective. Nothing downstream can compute a before and an
//! after from it, which is why products built that way stop their claim at "a
//! fix is ready to ship" — the sentence they were given cannot be measured
//! against, so shipping is the last thing they can honestly report.
//!
//! A [`EvaluationTarget`] is the machine-readable version: where the workflow starts,
//! what counts as finishing it, which number should move, which numbers may not
//! get worse, and how long to watch before saying anything. Given that, each
//! step of `Observation → Hypothesis → Change → Outcome` has somewhere concrete
//! to attach — "I saw this inside *this* workflow", "changing this should move
//! *this* metric", "it went from A to B and the guardrails held". Without it
//! that sequence is a diagram.
//!
//! An intervention that is measured references a [`EvaluationId`]. An
//! observation may — when it was read against a declared target — but is never
//! required to: discovery starts unsituated, and requiring a target here would
//! make that unrepresentable.
//!
//! # The rule this module exists to enforce
//!
//! A change that shipped is not a change that worked. When the measurement
//! window closes without enough traffic to detect the effect that was declared,
//! the result is [`Outcome::Unmeasured`] — never a verified improvement. This
//! is the same discipline as refusing to ship on "could not reproduce", and it
//! is enforced the same way: [`Outcome::measure`] is the only constructor, and
//! it decides. There is no path from a thin sample to a verified outcome,
//! because the honest majority case early in a deployment is that there was not
//! enough traffic, and a product that rounds that up to success is producing
//! traction rather than measuring it.

use serde::{Deserialize, Serialize};

/// Stable identity for a workflow. Everything the loop produces — a candidate,
/// a hypothesis, a change, an outcome — carries one of these, so a finding is
/// always attached to the objective it costs something against.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EvaluationId(pub String);

impl std::fmt::Display for EvaluationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Which direction counts as better for a metric. Stored on the metric rather
/// than asked of the user, because "should error rate go up or down" is not a
/// real question and asking it is how a setup form grows to five minutes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Better {
    Higher,
    Lower,
}

/// What a workflow is trying to improve.
///
/// Deliberately a closed set rather than a free-text metric name: the engine
/// has to be able to compute each of these from the events it already sees, and
/// a metric it cannot compute is a metric it cannot verify an outcome against.
/// Widening this list means teaching the engine to measure the new one, which is
/// the correct amount of friction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Metric {
    /// Share of entries that reach the success event.
    CompletionRate,
    /// Median wall-clock time from start event to success event.
    TimeToComplete,
    /// Share of entries that hit an error inside the workflow.
    ErrorRate,
    /// Share of entries that repeat a step before moving on.
    StepRetries,
    /// Share of entries that stop without reaching success and do not return.
    Abandonment,
    /// Support contacts attributable to this workflow.
    SupportContacts,
    /// Error events per hour, computed from Drums' own record — the one metric
    /// the engine can read with zero customer configuration, which is why it is
    /// the metric the first real observations carry. Time is the denominator,
    /// so it needs no traffic instrumentation to be honest; the event count it
    /// was computed from rides in [`Sample::entries`] so a thin reading can
    /// still be refused.
    ErrorEventRate,
}

impl Metric {
    pub fn better(&self) -> Better {
        match self {
            Metric::CompletionRate => Better::Higher,
            Metric::TimeToComplete
            | Metric::ErrorRate
            | Metric::StepRetries
            | Metric::Abandonment
            | Metric::SupportContacts
            | Metric::ErrorEventRate => Better::Lower,
        }
    }

    /// Human label, for the console and for claim text.
    pub fn label(&self) -> &'static str {
        match self {
            Metric::CompletionRate => "completion rate",
            Metric::TimeToComplete => "time to complete",
            Metric::ErrorRate => "error rate",
            Metric::StepRetries => "step retries",
            Metric::Abandonment => "abandonment",
            Metric::SupportContacts => "support contacts",
            Metric::ErrorEventRate => "error event rate",
        }
    }

    /// Signed improvement for a raw delta: positive means the workflow got
    /// better, whichever direction the metric happens to run in.
    pub fn improvement(&self, delta: f64) -> f64 {
        match self.better() {
            Better::Higher => delta,
            Better::Lower => -delta,
        }
    }
}

/// A number that may not get worse while the target metric improves.
///
/// Guardrails are what separate an improvement from a trade: completion rate can
/// always be raised by removing a validation step, and the way you find out that
/// is what happened is that the error rate moved with it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Guardrail {
    pub metric: Metric,
    /// How much worsening is tolerated before this counts as a regression,
    /// in the metric's own unit. Zero means any worsening at all regresses.
    pub tolerance: f64,
}

/// Declared BEFORE the rollout: how long to watch, how much traffic makes the
/// result readable, and how large a move is worth calling a direction.
///
/// All three are part of the same honesty. A window chosen after seeing the
/// numbers can be chosen to flatter them; a result reported without a minimum
/// sample is a coin flip with a narrative; and a product with no minimum effect
/// size will report every wobble as an improvement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MeasurementWindow {
    /// How long after rollout to keep watching.
    pub days: u32,
    /// Entries required, both before and after, for the comparison to be
    /// reported at all. Below this the outcome is [`Outcome::Unmeasured`].
    pub min_entries: u32,
    /// Smallest move, in the metric's own unit, worth calling a direction.
    /// Anything smaller is [`Direction::Neutral`] — a real and common answer.
    pub min_effect: f64,
}

impl Default for MeasurementWindow {
    /// The values the creation UI starts from. Seven days is a week of the
    /// customer's own traffic shape rather than a slice of it; a hundred entries
    /// is small enough to be reachable for an early design partner and large
    /// enough that a handful of sessions cannot swing the answer.
    fn default() -> Self {
        MeasurementWindow {
            days: 7,
            min_entries: 100,
            min_effect: 0.02,
        }
    }
}

/// One workflow under observation.
///
/// v1 is deliberately the five fields a person can answer in under a minute —
/// name, start, success, metric, and optionally guardrails. Segment, baseline
/// and window have defaults and live behind an advanced section, because a setup
/// form nobody finishes describes no workflow at all.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvaluationTarget {
    pub id: EvaluationId,
    /// What a person on the team would call it: "new customer creates first
    /// quote", not "funnel_3".
    pub name: String,
    /// The event that means someone entered.
    pub start_event: String,
    /// The event that means they finished. The definition of done for this
    /// workflow, and the thing completion is measured against.
    pub success_event: String,
    pub metric: Metric,
    #[serde(default)]
    pub guardrails: Vec<Guardrail>,
    #[serde(default)]
    pub window: MeasurementWindow,
}

impl EvaluationTarget {
    /// The minimum viable objective. Guardrails and window take their defaults
    /// so the creation UI can be five fields.
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        start_event: impl Into<String>,
        success_event: impl Into<String>,
        metric: Metric,
    ) -> Self {
        EvaluationTarget {
            id: EvaluationId(id.into()),
            name: name.into(),
            start_event: start_event.into(),
            success_event: success_event.into(),
            metric,
            guardrails: Vec::new(),
            window: MeasurementWindow::default(),
        }
    }

    pub fn with_guardrail(mut self, metric: Metric, tolerance: f64) -> Self {
        self.guardrails.push(Guardrail { metric, tolerance });
        self
    }

    pub fn with_window(mut self, window: MeasurementWindow) -> Self {
        self.window = window;
        self
    }
}

/// A metric reading over some number of workflow entries.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    pub value: f64,
    /// How many workflow entries this reading was computed from. The reason a
    /// sample is a struct and not an `f64`: a number without its denominator
    /// cannot be refused, and refusing thin numbers is the point.
    pub entries: u32,
}

/// A guardrail's before-and-after, supplied alongside the target metric's.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GuardrailReading {
    pub metric: Metric,
    pub before: f64,
    pub after: f64,
}

/// Why an outcome could not be stated. Each variant is a different thing to
/// tell the user, and collapsing them into one "no result" is how a product
/// stops being able to explain itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum Unmeasured {
    /// The window closed and not enough people went through the workflow.
    /// Nothing is wrong with the change; there is simply nothing to conclude.
    NotEnoughTraffic { entries: u32, needed: u32 },
    /// There was no readable "before" to compare against — the workflow was
    /// defined too close to the rollout, or traffic was thin then too.
    NoBaseline { entries: u32, needed: u32 },
}

impl Unmeasured {
    /// Plain sentence for the console. Says what is true and what would change
    /// it, never apologising for the absence of a result.
    pub fn sentence(&self) -> String {
        match self {
            Unmeasured::NotEnoughTraffic { entries, needed } => format!(
                "change is live — {entries} of the {needed} entries needed to measure it have arrived"
            ),
            Unmeasured::NoBaseline { entries, needed } => format!(
                "change is live — the period before it has {entries} of the {needed} entries needed to compare against"
            ),
        }
    }
}

/// Which way a measured result went.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Positive,
    /// The move was smaller than the effect the window declared worth calling.
    /// A real answer, and the most common one for a change that did nothing.
    Neutral,
    Negative,
}

/// Whether the numbers that were not allowed to get worse got worse.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Guardrails {
    Held,
    /// At least one regressed past its tolerance. Named, because "a guardrail
    /// regressed" is not actionable and "error rate rose 4 points" is.
    Regressed(Vec<Metric>),
}

/// The last field of `Observation → Hypothesis → Change → Outcome`, and the one
/// that makes the record worth keeping.
///
/// Construct only through [`Outcome::measure`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Outcome {
    /// Shipped, and the window could not produce a readable answer. This is not
    /// a failure state and it is not a pending state — it is a final, honest
    /// result that a great many changes will end in.
    Unmeasured(Unmeasured),
    /// Shipped, watched, and comparable.
    Measured {
        direction: Direction,
        from: f64,
        to: f64,
        entries: u32,
        guardrails: Guardrails,
    },
}

impl Outcome {
    /// Decide what may be said about a shipped change. THE ONLY CONSTRUCTOR.
    ///
    /// Refuses before it compares. A reading taken over fewer entries than the
    /// window declared is not a weaker version of a result, it is not a result,
    /// and the difference matters because the alternative — reporting it with a
    /// caveat — is how "verified improvement" quietly comes to mean "we shipped
    /// something and the number happened to be higher afterwards".
    pub fn measure(
        workflow: &EvaluationTarget,
        baseline: Sample,
        observed: Sample,
        guardrails: &[GuardrailReading],
    ) -> Outcome {
        let needed = workflow.window.min_entries;
        if baseline.entries < needed {
            return Outcome::Unmeasured(Unmeasured::NoBaseline {
                entries: baseline.entries,
                needed,
            });
        }
        if observed.entries < needed {
            return Outcome::Unmeasured(Unmeasured::NotEnoughTraffic {
                entries: observed.entries,
                needed,
            });
        }

        let improvement = workflow.metric.improvement(observed.value - baseline.value);
        let direction = if improvement >= workflow.window.min_effect {
            Direction::Positive
        } else if improvement <= -workflow.window.min_effect {
            Direction::Negative
        } else {
            Direction::Neutral
        };

        Outcome::Measured {
            direction,
            from: baseline.value,
            to: observed.value,
            entries: observed.entries,
            guardrails: Self::check_guardrails(workflow, guardrails),
        }
    }

    /// A guardrail regresses when it moved in its own worse direction by more
    /// than its tolerance. A guardrail with no reading is NOT treated as held —
    /// it is simply not evaluated, because claiming a number stayed flat without
    /// having looked at it is the same class of lie this module exists to
    /// prevent. Callers that need every guardrail read should check coverage
    /// before shipping, not infer it from a verdict.
    fn check_guardrails(workflow: &EvaluationTarget, readings: &[GuardrailReading]) -> Guardrails {
        let regressed: Vec<Metric> = workflow
            .guardrails
            .iter()
            .filter_map(|guard| {
                let reading = readings.iter().find(|r| r.metric == guard.metric)?;
                let worsening = -guard.metric.improvement(reading.after - reading.before);
                (worsening > guard.tolerance).then_some(guard.metric)
            })
            .collect();

        if regressed.is_empty() {
            Guardrails::Held
        } else {
            Guardrails::Regressed(regressed)
        }
    }

    /// Whether this counts toward "verified product improvements" — the primary
    /// KPI. Positive direction AND guardrails held. Nothing else qualifies:
    /// a target metric that moved while a guardrail regressed is a trade, and
    /// counting trades as improvements is how the KPI stops meaning anything.
    pub fn is_verified_improvement(&self) -> bool {
        matches!(
            self,
            Outcome::Measured {
                direction: Direction::Positive,
                guardrails: Guardrails::Held,
                ..
            }
        )
    }

    /// The provenance chip this outcome earns. An unmeasured outcome is
    /// `observed` — the change is genuinely live and that much was seen — never
    /// `verified`.
    pub fn provenance(&self) -> crate::Provenance {
        match self {
            Outcome::Unmeasured(_) => crate::Provenance::Observed,
            Outcome::Measured { .. } => crate::Provenance::Verified,
        }
    }
}

/// Where a workflow is, as one value, so the console and the CLI cannot disagree
/// about it.
///
/// The three quiet states at the top are the reason this exists. "No events" is
/// not one condition, and drawing them identically is how a partner sat with
/// events arriving and no deploy recorded — the loop stalled before attribution
/// and the screen looked exactly like a quiet afternoon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum WatchStatus {
    /// Nothing has ever arrived. The snippet is not installed, or not deployed.
    NotConnected,
    /// Installed and healthy; no user has entered the workflow yet. Nothing is
    /// wrong, and the screen should say so rather than implying a fault.
    Listening,
    /// Events are arriving but no deploy has been recorded, so nothing can be
    /// attributed to a change. The loop cannot proceed and it is not obvious
    /// from the outside — this is the state that must never be silent.
    Blocked { events: u32 },
    /// Behavior is flowing and being watched.
    Watching { entries: u32 },
}

impl WatchStatus {
    /// Derive the state from what has actually been seen. One rule set, called
    /// by whatever is drawing.
    pub fn derive(events: u32, deploys: u32, entries: u32) -> WatchStatus {
        match (events, deploys) {
            (0, _) => WatchStatus::NotConnected,
            (_, 0) => WatchStatus::Blocked { events },
            _ if entries == 0 => WatchStatus::Listening,
            _ => WatchStatus::Watching { entries },
        }
    }

    /// The line under the workflow map.
    pub fn sentence(&self) -> String {
        match self {
            WatchStatus::NotConnected => {
                "not connected — install capture to start".to_string()
            }
            WatchStatus::Listening => {
                "listening — no product activity captured yet".to_string()
            }
            WatchStatus::Blocked { events } => format!(
                "{events} events arriving, no deploy recorded — Drums cannot attribute a change until one is"
            ),
            WatchStatus::Watching { entries } => {
                format!("watching {entries} users")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quotes() -> EvaluationTarget {
        EvaluationTarget::new(
            "wf_quote",
            "Create first quote",
            "quote_started",
            "quote_submitted",
            Metric::CompletionRate,
        )
        .with_guardrail(Metric::ErrorRate, 0.0)
    }

    #[test]
    fn thin_sample_is_unmeasured_not_verified() {
        // The case that motivated the whole module: a real improvement shipped
        // into a workflow that only 7 people entered afterwards. The number
        // moved a long way and it still means nothing.
        let wf = quotes();
        let outcome = Outcome::measure(
            &wf,
            Sample {
                value: 0.60,
                entries: 400,
            },
            Sample {
                value: 0.95,
                entries: 7,
            },
            &[],
        );
        assert_eq!(
            outcome,
            Outcome::Unmeasured(Unmeasured::NotEnoughTraffic {
                entries: 7,
                needed: 100
            })
        );
        assert!(!outcome.is_verified_improvement());
        assert_eq!(outcome.provenance(), crate::Provenance::Observed);
    }

    #[test]
    fn thin_baseline_is_unmeasured_even_with_plenty_of_traffic_after() {
        // Defining the workflow the day before the rollout leaves nothing to
        // compare against. Reporting the "after" alone would be a number
        // dressed as a result.
        let outcome = Outcome::measure(
            &quotes(),
            Sample {
                value: 0.60,
                entries: 12,
            },
            Sample {
                value: 0.78,
                entries: 900,
            },
            &[],
        );
        assert_eq!(
            outcome,
            Outcome::Unmeasured(Unmeasured::NoBaseline {
                entries: 12,
                needed: 100
            })
        );
    }

    #[test]
    fn a_real_move_with_guardrails_held_is_a_verified_improvement() {
        let outcome = Outcome::measure(
            &quotes(),
            Sample {
                value: 0.61,
                entries: 400,
            },
            Sample {
                value: 0.78,
                entries: 380,
            },
            &[GuardrailReading {
                metric: Metric::ErrorRate,
                before: 0.03,
                after: 0.03,
            }],
        );
        assert!(outcome.is_verified_improvement());
        assert_eq!(outcome.provenance(), crate::Provenance::Verified);
    }

    #[test]
    fn completion_bought_with_errors_is_not_an_improvement() {
        // Completion rate can always be raised by deleting a validation step.
        // The guardrail is how that shows up as the trade it is, and the KPI
        // must not count it.
        let outcome = Outcome::measure(
            &quotes(),
            Sample {
                value: 0.61,
                entries: 400,
            },
            Sample {
                value: 0.80,
                entries: 400,
            },
            &[GuardrailReading {
                metric: Metric::ErrorRate,
                before: 0.03,
                after: 0.11,
            }],
        );
        match &outcome {
            Outcome::Measured {
                direction,
                guardrails,
                ..
            } => {
                assert_eq!(*direction, Direction::Positive);
                assert_eq!(*guardrails, Guardrails::Regressed(vec![Metric::ErrorRate]));
            }
            other => panic!("expected a measured outcome, got {other:?}"),
        }
        assert!(!outcome.is_verified_improvement());
    }

    #[test]
    fn a_move_smaller_than_the_declared_effect_is_neutral() {
        // 0.5 points against a declared 2-point minimum. Reporting this as a
        // win is how every wobble becomes an improvement.
        let outcome = Outcome::measure(
            &quotes(),
            Sample {
                value: 0.610,
                entries: 400,
            },
            Sample {
                value: 0.615,
                entries: 400,
            },
            &[GuardrailReading {
                metric: Metric::ErrorRate,
                before: 0.03,
                after: 0.03,
            }],
        );
        match outcome {
            Outcome::Measured { direction, .. } => assert_eq!(direction, Direction::Neutral),
            other => panic!("expected a measured outcome, got {other:?}"),
        }
    }

    #[test]
    fn a_lower_is_better_metric_improves_by_going_down() {
        let wf = EvaluationTarget::new(
            "wf_x",
            "Checkout",
            "checkout_started",
            "payment_confirmed",
            Metric::Abandonment,
        );
        let outcome = Outcome::measure(
            &wf,
            Sample {
                value: 0.30,
                entries: 500,
            },
            Sample {
                value: 0.19,
                entries: 500,
            },
            &[],
        );
        match outcome {
            Outcome::Measured { direction, .. } => assert_eq!(direction, Direction::Positive),
            other => panic!("expected a measured outcome, got {other:?}"),
        }
    }

    #[test]
    fn an_unread_guardrail_is_not_reported_as_held() {
        // No reading was supplied for the declared guardrail. It is not
        // evaluated — but the caller must not be able to read the resulting
        // `Held` as "we checked and it was fine" for a metric nobody looked at,
        // which is why coverage is the caller's job and this test pins the
        // behavior rather than the interpretation.
        let outcome = Outcome::measure(
            &quotes(),
            Sample {
                value: 0.61,
                entries: 400,
            },
            Sample {
                value: 0.78,
                entries: 400,
            },
            &[],
        );
        match outcome {
            Outcome::Measured { guardrails, .. } => assert_eq!(guardrails, Guardrails::Held),
            other => panic!("expected a measured outcome, got {other:?}"),
        }
    }

    #[test]
    fn zero_events_is_not_connected_and_events_without_deploys_is_blocked() {
        assert_eq!(WatchStatus::derive(0, 0, 0), WatchStatus::NotConnected);
        assert_eq!(WatchStatus::derive(0, 4, 0), WatchStatus::NotConnected);
        // The twentyfour26 state: one event in, nothing to attribute it to.
        assert_eq!(
            WatchStatus::derive(1, 0, 0),
            WatchStatus::Blocked { events: 1 }
        );
        assert_eq!(WatchStatus::derive(40, 2, 0), WatchStatus::Listening);
        assert_eq!(
            WatchStatus::derive(400, 2, 84),
            WatchStatus::Watching { entries: 84 }
        );
    }

    #[test]
    fn the_three_quiet_states_do_not_share_a_sentence() {
        let quiet = [
            WatchStatus::derive(0, 0, 0),
            WatchStatus::derive(40, 2, 0),
            WatchStatus::derive(1, 0, 0),
        ];
        let sentences: Vec<String> = quiet.iter().map(|s| s.sentence()).collect();
        for (i, a) in sentences.iter().enumerate() {
            for b in sentences.iter().skip(i + 1) {
                assert_ne!(a, b, "two quiet states render the same line");
            }
        }
    }

    #[test]
    fn a_workflow_round_trips_through_json() {
        let wf = quotes();
        let json = serde_json::to_string(&wf).expect("serialize");
        let back: EvaluationTarget = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(wf, back);
    }

    #[test]
    fn guardrails_and_window_are_optional_on_the_wire() {
        // The creation UI writes five fields. A workflow saved before guardrails
        // or a window were chosen must still load, with the defaults.
        let json = r#"{"id":"wf_a","name":"Signup","start_event":"signup_started","success_event":"activated","metric":"completion_rate"}"#;
        let wf: EvaluationTarget = serde_json::from_str(json).expect("deserialize");
        assert!(wf.guardrails.is_empty());
        assert_eq!(wf.window, MeasurementWindow::default());
    }
}
