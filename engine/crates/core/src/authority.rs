//! The ship gate: what Drums is allowed to DO with a finished repair
//! (spec §3 "Authority", §11 "autonomy is earned, not configured").
//!
//! # SEAM — this module is meant to be MOVED
//!
//! The per-class autonomy ladder (earned rungs, evidence-based promotion,
//! automatic demotion) is a crate of its own: `engine-authority`, at
//! `engine/crates/authority/src/lib.rs`. It did not exist when the intake
//! taxonomy landed, so the ONE rule that must not wait — a failure with no
//! replayable request can never ship alone — lives here, deliberately
//! self-contained: no dependency beyond [`crate::Intake`], no I/O, no config
//! reading. When `engine-authority` lands, move this file verbatim into
//! `engine/crates/authority/src/lib.rs`, keep `ship_decision` as the single
//! entry point the engine calls, and re-export it from here (or update
//! `engine/crates/cli/src/engine.rs`'s one call site) so the gate keeps being
//! evaluated in exactly one place.
//!
//! The engine's only current call site is `run_repair_pipeline` in
//! `engine/crates/cli/src/engine.rs` — search for `ship_decision`.

use crate::Intake;

/// A rung on the autonomy ladder (spec §11). Each failure CLASS sits on one and
/// climbs independently.
///
/// All four rungs are modeled because the ladder is four rungs and a two-variant
/// stand-in would quietly become the product's definition of authority. Only the
/// distinction the gate actually enforces today — [`Rung::ActAlone`] versus
/// everything below it — is wired to behavior; `engine-authority` owns promotion
/// and demotion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rung {
    /// Watch and say what would have been done. Change nothing.
    Observe,
    /// Generate and verify repairs in isolation. Never ship.
    Shadow,
    /// A verified repair waits on a person. The default, and a success state.
    Propose,
    /// Ship, verify, and report afterward.
    ActAlone,
}

/// What may happen to a finished, verified repair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShipDecision {
    /// The class earned act-alone AND the failure's intake can back a
    /// `verified` replay of the original request. Ship it.
    MayShip,
    /// Stop for a human, for the named reason.
    Propose(ProposeReason),
}

/// Why a repair stopped for a human instead of shipping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProposeReason {
    /// No claim in the chain is strong enough to act on. Either nothing
    /// reached `Verified`, or everything that did was reported rather than
    /// observed and could not be attested.
    ///
    /// This is the reason that did not exist until review pointed out that the
    /// gate never looked at the evidence at all — see the doc comment on
    /// [`ship_decision`].
    NoEligibleEvidence { detail: String },
    /// THE ABSOLUTE GATE. The failure's intake carries no replayable request,
    /// so nothing in its chain was ever `verified` against the request that was
    /// actually failing. No earned rung overrides this.
    IntakeNotReplayable { source: String },
    /// The class simply has not earned the act-alone rung. The default path.
    RungBelowActAlone { rung: Rung },
}

impl ProposeReason {
    /// One sentence for narration and for the record — why a ship the operator
    /// asked for did not happen. A withheld ship is a miss a human must be able
    /// to see (spec §13), never silence.
    pub fn withheld_text(&self) -> String {
        match self {
            ProposeReason::IntakeNotReplayable { source } => format!(
                "not shipping on its own: this failure arrived from {source} with no replayable request, so the original request was never replayed against the repair — proposing instead"
            ),
            ProposeReason::RungBelowActAlone { rung } => {
                format!("not shipping on its own: this failure class is on {rung:?}, not act-alone — proposing instead")
            }
            ProposeReason::NoEligibleEvidence { detail } => {
                format!("not shipping on its own: {detail} — proposing instead")
            }
        }
    }
}

/// What the gate needs to know about the evidence behind a repair.
///
/// A trait rather than a concrete type so `engine-core` does not have to
/// depend on `engine-plane`, and so the gate cannot be handed a bag of
/// booleans a caller assembled. Implementors answer for the WHOLE chain.
pub trait Evidence {
    /// Is there at least one claim that is both `Verified` AND eligible to act
    /// on — observed firsthand, or attested by a verifier and bound to this
    /// exact job?
    ///
    /// Note both halves. Review found the gate would have accepted a firsthand
    /// `Unresolved` (the claim `LocalPlane::timed_out` produces) and an
    /// attested remote `Observed`. Evidence that explicitly established
    /// nothing must not support acting without a human.
    fn has_actionable_verified_claim(&self) -> bool;

    /// Why not, when the answer above is false. Used verbatim in narration, so
    /// it should name the actual obstacle rather than restating the rule.
    fn ineligibility_detail(&self) -> String;
}

/// THE gate. Decides whether a finished repair may ship without a human.
///
/// # Why evidence is a required argument
///
/// An earlier version took only `(rung, intake)`. The attestation logic lived
/// in a helper next to this function, and review found what that implies: the
/// only callers of the helper were tests, while the production path called
/// this. A remote plane asserting `Verified` in a JSON payload would have
/// reached act-alone exactly as if Drums had watched the check run.
///
/// The lesson is not "remember to call the helper". It is that a safety
/// property enforced beside the path is not enforced. So evidence is a
/// parameter here: there is no signature of this function that lets a caller
/// skip it, and adding one would be a visible change to the one place
/// autonomy is decided.
///
/// The order of checks is load-bearing and unchanged at the top: the intake
/// gate is evaluated FIRST and unconditionally, so no arrangement of rungs,
/// config, evidence, or class history reads as an override of it.
pub fn ship_decision(rung: Rung, intake: &Intake, evidence: &dyn Evidence) -> ShipDecision {
    if !intake.is_replayable() {
        return ShipDecision::Propose(ProposeReason::IntakeNotReplayable {
            source: intake.source().to_string(),
        });
    }
    if rung != Rung::ActAlone {
        return ShipDecision::Propose(ProposeReason::RungBelowActAlone { rung });
    }
    if !evidence.has_actionable_verified_claim() {
        return ShipDecision::Propose(ProposeReason::NoEligibleEvidence {
            detail: evidence.ineligibility_detail(),
        });
    }
    ShipDecision::MayShip
}

/// Evidence stubs for tests.
///
/// Deliberately `#[doc(hidden)]` and named so that a production caller reading
/// `testing::evidence(true)` at a ship site knows immediately that something is
/// wrong. The real implementations are `LocalEvidence` in the engine and
/// `OutcomeEvidence` in `engine-plane`; neither can be short-circuited.
#[doc(hidden)]
pub mod testing {
    use super::Evidence;

    pub struct StubEvidence(pub bool);

    impl Evidence for StubEvidence {
        fn has_actionable_verified_claim(&self) -> bool {
            self.0
        }
        fn ineligibility_detail(&self) -> String {
            "stub evidence says no".to_string()
        }
    }

    /// Evidence that is eligible (`true`) or not (`false`). TESTS ONLY.
    pub fn evidence(actionable: bool) -> StubEvidence {
        StubEvidence(actionable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trigger() -> Intake {
        Intake::Trigger {
            source: "hyperdx".to_string(),
        }
    }

    fn reported() -> Intake {
        Intake::Reported {
            source: "linear".to_string(),
        }
    }

    #[test]
    fn act_alone_with_a_replayable_snippet_failure_may_ship() {
        assert_eq!(
            ship_decision(Rung::ActAlone, &Intake::Snippet, &testing::evidence(true)),
            ShipDecision::MayShip
        );
    }

    /// The absolute gate. The class has earned the top rung and it still does
    /// not matter.
    #[test]
    fn act_alone_with_a_trigger_intake_failure_can_never_ship() {
        let decision = ship_decision(Rung::ActAlone, &trigger(), &testing::evidence(true));
        assert_eq!(
            decision,
            ShipDecision::Propose(ProposeReason::IntakeNotReplayable {
                source: "hyperdx".to_string()
            }),
            "an act-alone class must still propose when the intake carries no replayable request"
        );
    }

    #[test]
    fn act_alone_with_a_reported_intake_failure_can_never_ship() {
        assert!(matches!(
            ship_decision(Rung::ActAlone, &reported(), &testing::evidence(true)),
            ShipDecision::Propose(ProposeReason::IntakeNotReplayable { .. })
        ));
    }

    #[test]
    fn every_rung_below_act_alone_proposes_even_for_a_replayable_failure() {
        for rung in [Rung::Observe, Rung::Shadow, Rung::Propose] {
            assert_eq!(
                ship_decision(rung, &Intake::Snippet, &testing::evidence(true)),
                ShipDecision::Propose(ProposeReason::RungBelowActAlone { rung }),
                "{rung:?} must not ship"
            );
        }
    }

    /// When BOTH would block, the intake gate is the reason reported — it is the
    /// one that cannot be lifted by earning a rung, so it is the one the human
    /// needs to read.
    #[test]
    fn intake_gate_is_reported_ahead_of_the_rung_when_both_block() {
        assert!(matches!(
            ship_decision(Rung::Propose, &trigger(), &testing::evidence(true)),
            ShipDecision::Propose(ProposeReason::IntakeNotReplayable { .. })
        ));
    }

    #[test]
    fn withheld_text_names_the_source_and_the_missing_replay() {
        let text = ProposeReason::IntakeNotReplayable {
            source: "otel".into(),
        }
        .withheld_text();
        assert!(text.contains("otel"), "{text}");
        assert!(text.contains("no replayable request"), "{text}");
        assert!(text.contains("proposing instead"), "{text}");
    }
}
