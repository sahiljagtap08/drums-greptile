//! What Drums thinks could be better. The second field of
//! `Observation → Hypothesis → Change → Outcome`.
//!
//! # This is where interpretation lives
//!
//! An observation is forbidden from diagnosing (`crate::observation`): it
//! records that 24% abandoned between upload and quote, and stops. The
//! hypothesis is the object that is *allowed* to have an opinion — "role
//! selection is confusing", "the promo field breaks checkout for returning
//! users" — because it is labeled as an opinion, carries the evidence it is an
//! opinion *about*, and can be wrong without contaminating the record it cites.
//!
//! # Held in the type system
//!
//! - **A hypothesis cites at least one observation.** [`Hypothesis::new`]
//!   refuses an empty citation list. An interpretation with no observed fact
//!   underneath it is a hunch, and a hunch that can enter the durable record
//!   dressed as a hypothesis is how the loop starts reasoning from vibes.
//! - **Provenance is `Inferred`, always.** [`Hypothesis::provenance`] has one
//!   arm, exactly as an observation's is always `Observed` and only an outcome
//!   can earn `Verified`. The three stages of the record carry the three
//!   provenance states, and none of them can borrow the next one's.
//! - **A change needs a plan.** The evaluation plan — which outcome should
//!   move, from what baseline, inside which guardrails, over what window — is
//!   attached to the hypothesis, and [`Hypothesis::ready_for_change`] is false
//!   until it is. This is where the plan lives in the corrected model: not at
//!   onboarding, not on the observation, but at the moment Drums decides a
//!   fact is worth acting on. A change that shipped from a plan-less
//!   hypothesis could never be measured, which is how "verified improvement"
//!   would quietly decay into "merged".
//! - **Status transitions are append-only lines**, exactly like an
//!   observation's: the row never mutates; a [`StatusChanged`] line lands
//!   beside it and the last one wins.

use serde::{Deserialize, Serialize};

use crate::evaluation::EvaluationTarget;
use crate::observation::{self, ObservationId, StatusChanged as ObservationStatusChanged};
use crate::Provenance;

/// The record `kind` a [`Hypothesis`] is appended under.
pub const RECORD_KIND: &str = "hypothesis";
/// The record `kind` a [`StatusChanged`] line is appended under.
pub const STATUS_KIND: &str = "hypothesis_status";

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HypothesisId(pub String);

impl std::fmt::Display for HypothesisId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where a hypothesis is in its life. Note what is absent: there is no
/// variant that claims the hypothesis was *right* — being right is an
/// outcome's verdict about a shipped change, never a status a hypothesis can
/// award itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Status {
    /// Stated, not yet decided on.
    Open,
    /// A human decided it is worth acting on. The change that follows cites
    /// this hypothesis.
    Accepted,
    /// A human decided against it. Kept, not deleted — a rejected hypothesis
    /// and its reason are exactly what the next hypothesis about the same
    /// observations should read first.
    Rejected { reason: String },
}

/// A status transition, as its own append-only record line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusChanged {
    pub hypothesis: HypothesisId,
    #[serde(flatten)]
    pub status: Status,
}

/// An interpretation of observed facts, with the plan for testing it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hypothesis {
    pub id: HypothesisId,
    /// The opinion, in a sentence or two: what could be better and why. This
    /// is deliberately the only free-prose judgment field in the whole record.
    pub statement: String,
    /// The observations this interprets. Never empty — enforced at
    /// construction and re-checked at deserialization time by readers that
    /// use [`Hypothesis::all`].
    pub cites: Vec<ObservationId>,
    /// The evaluation plan: what outcome should move, inside which guardrails,
    /// over what window. Absent while the hypothesis is being formed;
    /// required before a change may follow from it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<EvaluationTarget>,
    pub proposed_at_ms: u64,
    pub status: Status,
}

impl Hypothesis {
    /// Refuses an empty citation list — `None` rather than a hunch in the
    /// record.
    pub fn new(
        id: impl Into<String>,
        statement: impl Into<String>,
        cites: Vec<ObservationId>,
        proposed_at_ms: u64,
    ) -> Option<Hypothesis> {
        if cites.is_empty() {
            return None;
        }
        Some(Hypothesis {
            id: HypothesisId(id.into()),
            statement: statement.into(),
            cites,
            plan: None,
            proposed_at_ms,
            status: Status::Open,
        })
    }

    pub fn with_plan(mut self, plan: EvaluationTarget) -> Self {
        self.plan = Some(plan);
        self
    }

    /// Always [`Provenance::Inferred`]. A hypothesis is Drums' interpretation;
    /// it is never something that was observed, and it can never be verified —
    /// only the outcome of a change made because of it can be.
    pub fn provenance(&self) -> Provenance {
        Provenance::Inferred
    }

    /// Whether a change may proceed from this hypothesis. False until the
    /// evaluation plan exists, because a change with no declared outcome,
    /// baseline, and window is a change that can never be measured.
    pub fn ready_for_change(&self) -> bool {
        self.plan.is_some()
    }

    /// Everything to append when this hypothesis enters the record: its own
    /// line, plus an `observation_status` line marking each cited observation
    /// `Hypothesized` — so the observations' own fold
    /// ([`observation::Observation::current_status`]) reflects the citation
    /// without any row ever mutating.
    pub fn record_lines(&self) -> Vec<(String, serde_json::Value)> {
        let mut lines = Vec::with_capacity(1 + self.cites.len());
        if let Ok(v) = serde_json::to_value(self) {
            lines.push((RECORD_KIND.to_string(), v));
        }
        for obs in &self.cites {
            let status = ObservationStatusChanged {
                observation: obs.clone(),
                status: observation::Status::Hypothesized {
                    hypothesis: self.id.0.clone(),
                },
            };
            if let Ok(v) = serde_json::to_value(&status) {
                lines.push((observation::STATUS_KIND.to_string(), v));
            }
        }
        lines
    }

    /// All hypotheses in a decoded record. A line whose citation list is empty
    /// is skipped rather than surfaced — the constructor refuses to build one,
    /// and a hand-written record line does not get to bypass the rule.
    pub fn all<'a>(
        lines: impl IntoIterator<Item = &'a (String, serde_json::Value)>,
    ) -> Vec<Hypothesis> {
        lines
            .into_iter()
            .filter(|(kind, _)| kind == RECORD_KIND)
            .filter_map(|(_, v)| serde_json::from_value::<Hypothesis>(v.clone()).ok())
            .filter(|h| !h.cites.is_empty())
            .collect()
    }

    /// The latest status: the row's own unless [`StatusChanged`] lines were
    /// appended after it — last one wins.
    pub fn current_status<'a>(
        lines: impl IntoIterator<Item = &'a (String, serde_json::Value)>,
        id: &HypothesisId,
    ) -> Option<Status> {
        let mut status = None;
        for (kind, v) in lines {
            if kind == RECORD_KIND {
                if let Ok(h) = serde_json::from_value::<Hypothesis>(v.clone()) {
                    if &h.id == id && status.is_none() {
                        status = Some(h.status);
                    }
                }
            } else if kind == STATUS_KIND {
                if let Ok(s) = serde_json::from_value::<StatusChanged>(v.clone()) {
                    if &s.hypothesis == id {
                        status = Some(s.status);
                    }
                }
            }
        }
        status
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evaluation::{EvaluationTarget, Metric};
    use crate::observation::{Kind, Observation, Source, Window};

    fn cited() -> Vec<ObservationId> {
        vec![ObservationId("obs_184".into())]
    }

    fn hyp() -> Hypothesis {
        Hypothesis::new(
            "hyp_1",
            "returning users hit the promo-code branch that assumes a fresh cart",
            cited(),
            1_754_200_000_000,
        )
        .expect("non-empty citations")
    }

    #[test]
    fn a_hypothesis_cannot_cite_nothing() {
        // The load-bearing refusal: an interpretation with no observed fact
        // underneath it is a hunch, and hunches do not enter the record.
        assert!(Hypothesis::new("hyp_0", "everything is probably fine", vec![], 1).is_none());
    }

    #[test]
    fn a_hand_written_empty_citation_line_is_skipped_by_readers() {
        // The constructor can be bypassed by writing a record line directly;
        // the reader enforces the same rule so the bypass buys nothing.
        let forged = serde_json::json!({
            "id": "hyp_forged", "statement": "trust me", "cites": [],
            "proposed_at_ms": 1, "status": {"status": "open"}
        });
        let lines = [(RECORD_KIND.to_string(), forged)];
        assert!(Hypothesis::all(lines.iter()).is_empty());
    }

    #[test]
    fn provenance_is_always_inferred_and_never_serializes_verified() {
        let mut h = hyp();
        for status in [
            Status::Open,
            Status::Accepted,
            Status::Rejected {
                reason: "seasonal".into(),
            },
        ] {
            h.status = status;
            assert_eq!(h.provenance(), Provenance::Inferred);
            let json = serde_json::to_string(&h).unwrap();
            assert!(!json.contains("verified"), "leaked: {json}");
        }
    }

    #[test]
    fn a_change_needs_a_plan() {
        let h = hyp();
        assert!(!h.ready_for_change(), "no plan, no change");
        let planned = h.with_plan(EvaluationTarget::new(
            "eval_checkout",
            "Checkout",
            "checkout_started",
            "payment_confirmed",
            Metric::CompletionRate,
        ));
        assert!(planned.ready_for_change());
        // And the plan survives the wire.
        let back: Hypothesis =
            serde_json::from_str(&serde_json::to_string(&planned).unwrap()).unwrap();
        assert_eq!(back.plan.as_ref().unwrap().metric, Metric::CompletionRate);
    }

    #[test]
    fn record_lines_mark_every_cited_observation_hypothesized() {
        let obs = Observation::fact(
            "obs_184",
            Source::Runtime,
            Kind::RateShift {
                previous: 0.2,
                since_deploy: Some("8f32a1".into()),
            },
            Window::new(0, 1).unwrap(),
            1,
        );
        let h = hyp();
        let mut lines: Vec<(String, serde_json::Value)> = vec![(
            observation::RECORD_KIND.to_string(),
            serde_json::to_value(&obs).unwrap(),
        )];
        lines.extend(h.record_lines());

        // The hypothesis is readable back…
        assert_eq!(Hypothesis::all(lines.iter()), vec![h.clone()]);
        // …and the observation's own status fold now says Hypothesized,
        // without the observation row having changed at all.
        assert_eq!(
            Observation::current_status(lines.iter(), &obs.id),
            Some(crate::observation::Status::Hypothesized {
                hypothesis: "hyp_1".into()
            })
        );
    }

    #[test]
    fn status_lines_fold_last_wins() {
        let h = hyp();
        let rejected = StatusChanged {
            hypothesis: h.id.clone(),
            status: Status::Rejected {
                reason: "the burst was a bot".into(),
            },
        };
        let mut lines = h.record_lines();
        lines.push((
            STATUS_KIND.to_string(),
            serde_json::to_value(&rejected).unwrap(),
        ));
        assert_eq!(
            Hypothesis::current_status(lines.iter(), &h.id),
            Some(Status::Rejected {
                reason: "the burst was a bot".into()
            })
        );
        // With no transition lines, the row's own status stands.
        assert_eq!(
            Hypothesis::current_status(h.record_lines().iter(), &h.id),
            Some(Status::Open)
        );
    }

    #[test]
    fn being_right_is_not_a_status_a_hypothesis_can_award_itself() {
        // Compile-time-ish guard by serialization: no Status variant text can
        // read as a success claim. If someone adds Confirmed/Proven/Validated,
        // this is the test that makes them argue it out loud.
        for status in [
            Status::Open,
            Status::Accepted,
            Status::Rejected { reason: "x".into() },
        ] {
            let json = serde_json::to_string(&status).unwrap();
            for banned in ["confirmed", "proven", "validated", "succeeded", "worked"] {
                assert!(!json.to_lowercase().contains(banned), "{json}");
            }
        }
    }
}
