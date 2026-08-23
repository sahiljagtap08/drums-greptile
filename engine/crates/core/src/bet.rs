//! The Product Bet — a product decision, captured before the outcome is known.
//!
//! # Why this object exists
//!
//! Six weeks after a launch, anyone — and especially a language model — can
//! rationalize what the team "obviously intended." The only defense is the
//! pre-registered belief: what we believed, why, for whom, and what we expected
//! to happen, written down while the outcome was still unknown. The bet is that
//! record. The loop underneath it (`Observation → Hypothesis → Change →
//! Outcome`) is the evidence machinery; the bet is the *decision* wrapped
//! around it, with the one field none of the substrate objects carry: what the
//! team learned.
//!
//! The full chain: **Signal → Decision → Change → Outcome → Learning.**
//!
//! # Measurement is not causality, and the type refuses to conflate them
//!
//! A frozen window proves *measurement integrity* — nobody moved the goalposts
//! after seeing the result. It does not prove the change caused the move.
//! Causal confidence depends on the rollout design: a full rollout admits every
//! confounder; a holdout or randomized exposure excludes some of them. So a
//! [`Verdict`] always carries a [`CausalConfidence`], and the confidence is
//! **derived from the design, never asserted**: [`CausalConfidence::from_design`]
//! is the only constructor, and today's only shipped design ([`RolloutDesign::Full`])
//! can never yield more than `Low`. "Supported" means *the pre-registered
//! expectation was met by the measurement* — it never means "proven caused".
//!
//! # Pre-registration is enforced by append-only time
//!
//! The bet row freezes at proposal. Confirmation, decline, evaluation, and
//! learning arrive as `bet_status` lines beside it. Whether the belief truly
//! preceded the outcome is checkable from timestamps in the record, not from
//! anyone's memory.

use serde::{Deserialize, Serialize};

use crate::change::OutcomeRecorded;
use crate::evaluation::{Direction, Guardrails, Outcome};
use crate::hypothesis::HypothesisId;
use crate::Provenance;

/// The record `kind` a [`ProductBet`] is appended under.
pub const RECORD_KIND: &str = "bet";
/// The record `kind` a [`BetStatusChanged`] line is appended under.
pub const STATUS_KIND: &str = "bet_status";

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BetId(pub String);

impl std::fmt::Display for BetId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// How the change was exposed to users. This is what causal confidence is
/// derived from — the design either excludes confounders or it does not, and
/// no amount of wanting the result changes which.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RolloutDesign {
    /// Everyone got the change at once. The only design Drums ships today.
    Full,
    /// A holdout kept some traffic on the old behavior. Not shipped —
    /// modeled so the ladder above `Low` exists and is visibly out of reach.
    Holdout,
    /// Exposure was randomized. Not shipped.
    Randomized,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    Low,
    Medium,
    High,
}

/// How much the measurement says about *causation*, with the reason stated.
///
/// [`CausalConfidence::from_design`] is the only constructor. There is no way
/// to hold a `High` without a design that earns it, which is the point: the
/// product can never "round up" a full-rollout metric move into proof.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CausalConfidence {
    pub level: Confidence,
    /// One sentence of why, always present — a confidence without its basis is
    /// a number wearing a lab coat.
    pub basis: String,
}

impl CausalConfidence {
    pub fn from_design(design: RolloutDesign) -> CausalConfidence {
        match design {
            RolloutDesign::Full => CausalConfidence {
                level: Confidence::Low,
                basis: "full rollout — the metric moved after the change; no holdout or randomization excludes other explanations".into(),
            },
            RolloutDesign::Holdout => CausalConfidence {
                level: Confidence::Medium,
                basis: "a holdout kept part of the traffic on the old behavior for comparison".into(),
            },
            RolloutDesign::Randomized => CausalConfidence {
                level: Confidence::High,
                basis: "exposure was randomized, so systematic confounders are excluded by design".into(),
            },
        }
    }
}

/// Whether the pre-registered expectation was met by the measurement.
/// Deliberately not named "worked" or "proven" — see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Support {
    /// The target moved as expected and the read guardrails held.
    Supported,
    /// The target moved against the expectation, or a guardrail regressed.
    NotSupported,
    /// The measurement could not produce a readable answer, or the move was
    /// below the declared minimum effect. A real and common verdict.
    Inconclusive,
}

/// The evaluation of a bet: the measured facts plus the honesty fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Verdict {
    pub support: Support,
    pub causal_confidence: CausalConfidence,
    /// Guardrails declared in the plan that no source could read. Copied from
    /// the outcome line so a bet card can never render "guardrails held" over
    /// guardrails nobody watched.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unread_guardrails: Vec<String>,
}

impl Verdict {
    /// Derive the verdict from a change's recorded outcome. The only
    /// constructor: support comes from the measurement, confidence from the
    /// design, and neither can be asserted directly.
    pub fn from_outcome(recorded: &OutcomeRecorded, design: RolloutDesign) -> Verdict {
        let support = match &recorded.outcome {
            Outcome::Measured {
                direction: Direction::Positive,
                guardrails: Guardrails::Held,
                ..
            } => Support::Supported,
            Outcome::Measured {
                direction: Direction::Negative,
                ..
            }
            | Outcome::Measured {
                guardrails: Guardrails::Regressed(_),
                ..
            } => Support::NotSupported,
            Outcome::Measured {
                direction: Direction::Neutral,
                ..
            }
            | Outcome::Unmeasured(_) => Support::Inconclusive,
        };
        Verdict {
            support,
            causal_confidence: CausalConfidence::from_design(design),
            unread_guardrails: recorded.unread_guardrails.clone(),
        }
    }
}

/// Where a bet is in its life. Confirmation is the load-bearing state: it is
/// the human, pre-registering the belief before the outcome exists.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum BetStatus {
    /// Drafted — by a human, or by Drums for a human to edit. Not yet a
    /// commitment.
    Proposed,
    /// A human said "yes, this is the bet we are making." The moment that
    /// makes the record a pre-registration rather than a note.
    Confirmed,
    /// A human said no. Kept, with the reason — a declined bet is a decision
    /// too, and the record keeps decisions.
    Declined { reason: String },
    /// The chain reached an outcome and the verdict was derived.
    Evaluated { verdict: Verdict },
    /// What the team took from it, in their own words. The last field of
    /// Signal → Decision → Change → Outcome → **Learning**, and the one only
    /// a human can write.
    Learned { note: String },
}

/// A status transition, as its own append-only record line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BetStatusChanged {
    pub bet: BetId,
    #[serde(flatten)]
    pub status: BetStatus,
}

/// A product decision, frozen at proposal time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProductBet {
    pub id: BetId,
    pub created_at_ms: u64,
    /// What we believe. ("We believe simplified project creation will raise
    /// onboarding completion for new self-serve users.")
    pub belief: String,
    /// Why — the evidence and reasoning that led to the decision, as prose.
    /// The structured evidence lives in the hypothesis's citations; this is
    /// the human account, kept verbatim.
    pub rationale: String,
    /// Who this should affect, when stated. `None` is honest for "everyone".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audience: Option<String>,
    /// Alternatives that were considered and not taken. Cheap to write down
    /// now, priceless when the same decision comes back in a year.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alternatives: Vec<String>,
    /// The hypothesis carrying the expectation: cited observations, the
    /// metric, the direction, guardrails, and the window. A bet without one
    /// cannot exist — "what do we expect to happen" is not optional in a
    /// pre-registration.
    pub hypothesis: HypothesisId,
    pub status: BetStatus,
}

impl ProductBet {
    /// Refuses an empty belief or rationale — a bet is a decision with its
    /// reasons, and a blank reason recorded forever is worse than none.
    pub fn new(
        id: impl Into<String>,
        belief: impl Into<String>,
        rationale: impl Into<String>,
        hypothesis: HypothesisId,
        created_at_ms: u64,
    ) -> Option<ProductBet> {
        let belief = belief.into();
        let rationale = rationale.into();
        if belief.trim().is_empty() || rationale.trim().is_empty() {
            return None;
        }
        Some(ProductBet {
            id: BetId(id.into()),
            created_at_ms,
            belief,
            rationale,
            audience: None,
            alternatives: Vec::new(),
            hypothesis,
            status: BetStatus::Proposed,
        })
    }

    pub fn with_audience(mut self, audience: impl Into<String>) -> Self {
        self.audience = Some(audience.into());
        self
    }

    pub fn with_alternatives(mut self, alts: impl IntoIterator<Item = String>) -> Self {
        self.alternatives = alts.into_iter().collect();
        self
    }

    /// A bet is an interpretation and a commitment — `inferred` like the
    /// hypothesis it wraps, never `observed`, never `verified`. Only the
    /// outcome inside its chain can earn `verified`, and only for what was
    /// measured.
    pub fn provenance(&self) -> Provenance {
        Provenance::Inferred
    }

    pub fn all<'a>(
        lines: impl IntoIterator<Item = &'a (String, serde_json::Value)>,
    ) -> Vec<ProductBet> {
        lines
            .into_iter()
            .filter(|(kind, _)| kind == RECORD_KIND)
            .filter_map(|(_, v)| serde_json::from_value::<ProductBet>(v.clone()).ok())
            .filter(|b| !b.belief.trim().is_empty() && !b.rationale.trim().is_empty())
            .collect()
    }

    /// The latest status: the row's own unless `bet_status` lines landed after
    /// it — last one wins, except `Learned`, which never erases the verdict
    /// (learning is *additional* to evaluation, and a fold that let a note
    /// overwrite a verdict would delete the one thing the note is about).
    pub fn current_status<'a>(
        lines: impl IntoIterator<Item = &'a (String, serde_json::Value)>,
        id: &BetId,
    ) -> Option<BetStatus> {
        let mut status: Option<BetStatus> = None;
        for (kind, v) in lines {
            if kind == RECORD_KIND {
                if let Ok(b) = serde_json::from_value::<ProductBet>(v.clone()) {
                    if &b.id == id && status.is_none() {
                        status = Some(b.status);
                    }
                }
            } else if kind == STATUS_KIND {
                if let Ok(s) = serde_json::from_value::<BetStatusChanged>(v.clone()) {
                    if &s.bet == id {
                        match (&status, &s.status) {
                            // Learning is additional to a terminal status,
                            // never a replacement: a fold that let a note
                            // overwrite the verdict would delete the thing
                            // the note is about, and one that overwrote a
                            // decline would relabel a bet that was never
                            // evaluated. Notes are read via `learnings()`.
                            (
                                Some(BetStatus::Evaluated { .. })
                                | Some(BetStatus::Declined { .. }),
                                BetStatus::Learned { .. },
                            ) => {}
                            _ => status = Some(s.status),
                        }
                    }
                }
            }
        }
        status
    }

    /// Every learning note recorded for a bet, in record order.
    pub fn learnings<'a>(
        lines: impl IntoIterator<Item = &'a (String, serde_json::Value)>,
        id: &BetId,
    ) -> Vec<String> {
        lines
            .into_iter()
            .filter(|(kind, _)| kind == STATUS_KIND)
            .filter_map(|(_, v)| serde_json::from_value::<BetStatusChanged>(v.clone()).ok())
            .filter(|s| &s.bet == id)
            .filter_map(|s| match s.status {
                BetStatus::Learned { note } => Some(note),
                _ => None,
            })
            .collect()
    }

    /// Whether the pre-registration property actually holds against a shipped
    /// change: the bet must have existed before the change shipped. Checked
    /// from record timestamps, not from anyone's memory — a bet created after
    /// the ship is a note, not a pre-registration, and the difference is the
    /// entire value of the object.
    pub fn preregistered_before(&self, shipped_at_ms: u64) -> bool {
        self.created_at_ms < shipped_at_ms
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change::ChangeId;
    use crate::evaluation::Unmeasured;

    fn bet() -> ProductBet {
        ProductBet::new(
            "bet_1",
            "simplified project creation will raise onboarding completion for new self-serve users",
            "onboarding abandonment rose after the March redesign; support tickets mention the project form twice a week",
            HypothesisId("hyp_1".into()),
            1_000,
        )
        .unwrap()
        .with_audience("new self-serve users")
        .with_alternatives(["reorder the form fields instead".into(), "add an onboarding video".into()])
    }

    #[test]
    fn a_bet_needs_its_belief_and_its_reasons() {
        assert!(ProductBet::new("b", "  ", "reason", HypothesisId("h".into()), 1).is_none());
        assert!(ProductBet::new("b", "belief", "", HypothesisId("h".into()), 1).is_none());
        // And a hand-forged blank line is skipped by readers, same as the
        // hypothesis rule — the bypass buys nothing.
        let forged = serde_json::json!({
            "id": "bet_x", "created_at_ms": 1, "belief": " ", "rationale": "",
            "hypothesis": "hyp_1", "status": {"status": "proposed"}
        });
        let lines = [(RECORD_KIND.to_string(), forged)];
        assert!(ProductBet::all(lines.iter()).is_empty());
    }

    #[test]
    fn causal_confidence_cannot_be_asserted_only_derived() {
        // The load-bearing rule: a full rollout can never yield more than Low,
        // whatever the metric did. There is no constructor that takes a level.
        let c = CausalConfidence::from_design(RolloutDesign::Full);
        assert_eq!(c.level, Confidence::Low);
        assert!(c.basis.contains("no holdout"), "{}", c.basis);
        assert!(CausalConfidence::from_design(RolloutDesign::Randomized).level > c.level);
    }

    fn outcome(direction: Direction, guardrails: Guardrails) -> OutcomeRecorded {
        OutcomeRecorded {
            change: ChangeId("chg_1".into()),
            measured_at_ms: 9,
            outcome: Outcome::Measured {
                direction,
                from: 0.61,
                to: 0.78,
                entries: 340,
                guardrails,
            },
            unread_guardrails: vec!["support contacts".into()],
        }
    }

    #[test]
    fn supported_means_expectation_met_never_causation_proven() {
        let v = Verdict::from_outcome(
            &outcome(Direction::Positive, Guardrails::Held),
            RolloutDesign::Full,
        );
        assert_eq!(v.support, Support::Supported);
        assert_eq!(v.causal_confidence.level, Confidence::Low);
        // The unread guardrail travels with the verdict, so a card can never
        // say "held" about a guardrail nobody watched.
        assert_eq!(v.unread_guardrails, vec!["support contacts"]);
        // And the serialized verdict never contains causal language.
        let json = serde_json::to_string(&v).unwrap().to_lowercase();
        for banned in ["caused", "proved", "proven", "worked"] {
            assert!(!json.contains(banned), "{json}");
        }
    }

    #[test]
    fn a_guardrail_trade_or_a_regression_is_not_supported() {
        use crate::evaluation::Metric;
        let v = Verdict::from_outcome(
            &outcome(
                Direction::Positive,
                Guardrails::Regressed(vec![Metric::ErrorEventRate]),
            ),
            RolloutDesign::Full,
        );
        assert_eq!(
            v.support,
            Support::NotSupported,
            "a bought metric is a trade, not a win"
        );
        let v = Verdict::from_outcome(
            &outcome(Direction::Negative, Guardrails::Held),
            RolloutDesign::Full,
        );
        assert_eq!(v.support, Support::NotSupported);
    }

    #[test]
    fn neutral_and_unmeasured_are_inconclusive_not_failures() {
        let v = Verdict::from_outcome(
            &outcome(Direction::Neutral, Guardrails::Held),
            RolloutDesign::Full,
        );
        assert_eq!(v.support, Support::Inconclusive);
        let unmeasured = OutcomeRecorded {
            change: ChangeId("chg_1".into()),
            measured_at_ms: 9,
            outcome: Outcome::Unmeasured(Unmeasured::NotEnoughTraffic {
                entries: 7,
                needed: 100,
            }),
            unread_guardrails: vec![],
        };
        let v = Verdict::from_outcome(&unmeasured, RolloutDesign::Full);
        assert_eq!(v.support, Support::Inconclusive);
    }

    #[test]
    fn learning_never_erases_the_verdict() {
        let b = bet();
        let evaluated = BetStatusChanged {
            bet: b.id.clone(),
            status: BetStatus::Evaluated {
                verdict: Verdict::from_outcome(
                    &outcome(Direction::Positive, Guardrails::Held),
                    RolloutDesign::Full,
                ),
            },
        };
        let learned = BetStatusChanged {
            bet: b.id.clone(),
            status: BetStatus::Learned {
                note: "returning users need the same fix".into(),
            },
        };
        let lines = [
            (RECORD_KIND.to_string(), serde_json::to_value(&b).unwrap()),
            (
                STATUS_KIND.to_string(),
                serde_json::to_value(&evaluated).unwrap(),
            ),
            (
                STATUS_KIND.to_string(),
                serde_json::to_value(&learned).unwrap(),
            ),
        ];
        match ProductBet::current_status(lines.iter(), &b.id) {
            Some(BetStatus::Evaluated { .. }) => {}
            other => panic!("the verdict must survive the learning note: {other:?}"),
        }
        assert_eq!(
            ProductBet::learnings(lines.iter(), &b.id),
            vec!["returning users need the same fix"]
        );
    }

    #[test]
    fn learning_never_relabels_a_declined_bet_as_evaluated() {
        // Found by the release audit (R9): decline → learn rendered the bet
        // "evaluated", erasing the decline and its reason from the card.
        let b = bet();
        let declined = BetStatusChanged {
            bet: b.id.clone(),
            status: BetStatus::Declined {
                reason: "partner asked us to hold".into(),
            },
        };
        let learned = BetStatusChanged {
            bet: b.id.clone(),
            status: BetStatus::Learned {
                note: "we defer to partner bandwidth".into(),
            },
        };
        let lines = [
            (RECORD_KIND.to_string(), serde_json::to_value(&b).unwrap()),
            (
                STATUS_KIND.to_string(),
                serde_json::to_value(&declined).unwrap(),
            ),
            (
                STATUS_KIND.to_string(),
                serde_json::to_value(&learned).unwrap(),
            ),
        ];
        match ProductBet::current_status(lines.iter(), &b.id) {
            Some(BetStatus::Declined { reason }) => assert_eq!(reason, "partner asked us to hold"),
            other => panic!("declined must survive the learning note: {other:?}"),
        }
        assert_eq!(ProductBet::learnings(lines.iter(), &b.id).len(), 1);
    }

    #[test]
    fn preregistration_is_a_timestamp_fact_not_a_memory() {
        let b = bet(); // created at 1_000
        assert!(b.preregistered_before(2_000));
        assert!(
            !b.preregistered_before(1_000),
            "created at ship time is not before it"
        );
        assert!(
            !b.preregistered_before(500),
            "a bet written after the ship is a note"
        );
    }

    #[test]
    fn a_bet_is_inferred_and_its_row_round_trips() {
        let b = bet();
        assert_eq!(b.provenance(), Provenance::Inferred);
        let json = serde_json::to_string(&b).unwrap();
        assert!(!json.contains("verified"));
        let back: ProductBet = serde_json::from_str(&json).unwrap();
        assert_eq!(b, back);
        assert_eq!(
            back.alternatives.len(),
            2,
            "the road not taken is part of the record"
        );
    }
}
