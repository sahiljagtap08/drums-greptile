//! The autonomy ladder (spec §11: "autonomy is earned, not configured").
//!
//! `engine_core::authority` held the one rule that could not wait — a failure
//! with no replayable request can never ship alone — and named this crate as
//! the seam it should move into. This is that crate. The gate itself stays in
//! core and is re-exported here, because it must be evaluated in exactly one
//! place; what lives here is everything ABOUT the rung that gate reads.
//!
//! # Four decisions, each of which could have gone the other way
//!
//! **1. A class is `service` + `error_name`, not a file path.** A repair moves
//! code between files; a class that keyed on the file would reset its earned
//! history the first time an agent extracted a helper. Trust should survive
//! refactoring, because the thing being trusted is "Drums handles this KIND of
//! failure well here", not "Drums handles line 41 well".
//!
//! **2. Promotion is only ever PROPOSED.** Nothing in this crate raises a rung
//! on its own. A clean streak produces a *candidate*, and a human applies it.
//! The alternative — software that grants itself more authority the longer it
//! goes without being caught — is the exact shape of the thing every operator
//! is afraid of, and no amount of evidence makes it a good idea.
//!
//! **3. Demotion is automatic and immediate.** One bad outcome drops the class
//! out of act-alone the moment it is recorded. Asymmetry is the point:
//! earning authority takes a human and a streak, losing it takes one mistake
//! and no meeting.
//!
//! **4. State is the append-only record, not a side file.** Rungs are rebuilt
//! by folding the same `.drums/record.jsonl` that everything else reads. A
//! separate `authority.json` would be a second source of truth that could
//! disagree with the audit trail — and the audit trail is the thing a
//! regulated customer is actually buying.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

pub use engine_core::authority::{ship_decision, ProposeReason, Rung, ShipDecision};

/// How many consecutive clean outcomes a class needs before promotion is even
/// PROPOSED. Not a tuning knob today, and deliberately not configurable: a
/// customer who can set it to 1 has bought a checkbox that says "act alone",
/// which is what this whole mechanism exists to avoid selling.
pub const PROMOTION_STREAK: u32 = 5;

/// What Drums handles autonomously is decided per class, not per repo.
///
/// `service` + `error_name` — see the module docs for why the file is
/// deliberately not part of the key.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FailureClass {
    pub service: String,
    pub error_name: String,
}

impl FailureClass {
    pub fn new(service: impl Into<String>, error_name: impl Into<String>) -> Self {
        Self { service: service.into(), error_name: error_name.into() }
    }

    /// Stable, greppable, and safe to put in a CLI argument.
    pub fn key(&self) -> String {
        format!("{}/{}", self.service, self.error_name)
    }

    /// Parse a `service/ErrorName` key back. Returns `None` rather than
    /// guessing when the shape is wrong — a mistyped class must not silently
    /// create a NEW class that starts at the default rung.
    pub fn parse(key: &str) -> Option<Self> {
        let (service, error_name) = key.split_once('/')?;
        if service.is_empty() || error_name.is_empty() {
            return None;
        }
        Some(Self::new(service, error_name))
    }
}

/// What actually happened to a shipped repair. Only these three outcomes move
/// a class: everything else in the loop (a proposal, a failed reproduction) is
/// not evidence about autonomy, because autonomy is only ever exercised at the
/// ship.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Shipped, and the post-deploy checks passed. The only thing that builds
    /// a streak.
    ShippedClean,
    /// Shipped and then rolled back. The strongest possible evidence that this
    /// class is not ready.
    Reverted,
    /// The deploy itself failed. Not the repair's fault necessarily — but the
    /// class demonstrably cannot complete an unattended ship, which is exactly
    /// what act-alone claims it can.
    ShipFailed,
}

impl Outcome {
    fn is_clean(self) -> bool {
        matches!(self, Outcome::ShippedClean)
    }
}

/// One class's standing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Standing {
    pub rung: Rung,
    /// Consecutive clean outcomes since the last bad one.
    pub streak: u32,
    /// Every outcome ever recorded, for the "over 6 weeks you had 9 incidents"
    /// conversation the pricing doc is built on.
    pub total_clean: u32,
    pub total_bad: u32,
    /// When the streak reached [`PROMOTION_STREAK`] — the instant this class
    /// earned the right to be PROPOSED for act-alone. `None` until it does,
    /// and cleared again by any bad outcome, because a reset streak has to
    /// re-earn the moment as well as the count.
    ///
    /// It exists so a reader can tell "this was earned overnight" apart from
    /// "this has been sitting here unactioned for a fortnight" — a promotion
    /// candidate is otherwise a standing state with no time on it at all, and
    /// a surface that repeats a standing state every morning becomes noise.
    pub earned_at_ms: Option<u64>,
}

impl Default for Standing {
    fn default() -> Self {
        // Propose, not Observe. A class nobody has said anything about should
        // still produce verified repairs and stop for a human — that is the
        // product's default and a success state, not a restricted mode.
        Self { rung: Rung::Propose, streak: 0, total_clean: 0, total_bad: 0, earned_at_ms: None }
    }
}

/// A promotion Drums thinks has been earned. Never applied by this crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromotionCandidate {
    pub class: FailureClass,
    pub from: Rung,
    pub to: Rung,
    pub streak: u32,
    /// When it was earned — see [`Standing::earned_at_ms`]. `0` only for a
    /// record whose outcome lines carry no timestamp at all.
    pub earned_at_ms: u64,
    /// The sentence a human reads before deciding.
    pub because: String,
}

/// A demotion that already happened. Returned so a caller can narrate it —
/// a class silently losing authority is as bad as silently gaining it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Demotion {
    pub class: FailureClass,
    pub from: Rung,
    pub to: Rung,
    pub because: String,
}

// -- the record lines this crate writes and reads -----------------------------

/// Record kind for an outcome that moves a class.
pub const KIND_OUTCOME: &str = "authority_outcome";
/// Record kind for a rung change that actually took effect.
pub const KIND_RUNG: &str = "authority_rung";

#[derive(Debug, Serialize, Deserialize)]
struct OutcomeLine {
    class: FailureClass,
    outcome: Outcome,
    failure_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct RungLine {
    class: FailureClass,
    rung: String,
    /// `promoted` (a human applied a candidate) or `demoted` (automatic).
    reason: String,
}

fn rung_name(r: Rung) -> &'static str {
    match r {
        Rung::Observe => "observe",
        Rung::Shadow => "shadow",
        Rung::Propose => "propose",
        Rung::ActAlone => "act-alone",
    }
}

fn parse_rung(s: &str) -> Option<Rung> {
    match s {
        "observe" => Some(Rung::Observe),
        "shadow" => Some(Rung::Shadow),
        "propose" => Some(Rung::Propose),
        "act-alone" => Some(Rung::ActAlone),
        _ => None,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AuthorityError {
    #[error("could not read the record: {0}")]
    Read(String),
    #[error("could not append to the record: {0}")]
    Append(String),
    #[error("`{0}` is not a class key — expected `service/ErrorName`")]
    BadClassKey(String),
}

/// Every class's standing, rebuilt by folding the append-only record.
#[derive(Debug, Default)]
pub struct Ladder {
    standings: HashMap<FailureClass, Standing>,
    /// Unreadable lines skipped while folding — disclosed, never silent.
    pub skipped: usize,
}

impl Ladder {
    /// Fold `.drums/record.jsonl` into per-class standings.
    ///
    /// A missing record is an empty ladder, not an error: nothing has happened
    /// yet, which is a normal state and the one every new install is in.
    pub fn load(record_path: &Path) -> Result<Self, AuthorityError> {
        let read = engine_record::read_all(record_path)
            .map_err(|e| AuthorityError::Read(e.to_string()))?;
        let mut ladder = Ladder { standings: HashMap::new(), skipped: read.skipped };

        for (kind, value) in &read.lines {
            match kind.as_str() {
                KIND_OUTCOME => {
                    if let Ok(line) = serde_json::from_value::<OutcomeLine>(value.clone()) {
                        // `recorded_at_ms` is written by `engine_record::append`
                        // on every line. A line missing it (hand-edited, or from
                        // a build that predates it) folds as 0 rather than being
                        // dropped: the OUTCOME is what moves a class, and losing
                        // it over a missing timestamp would silently rewrite the
                        // audit trail.
                        let at = value.get("recorded_at_ms").and_then(|v| v.as_u64()).unwrap_or(0);
                        ladder.fold_outcome(&line.class, line.outcome, at);
                    }
                }
                KIND_RUNG => {
                    if let Ok(line) = serde_json::from_value::<RungLine>(value.clone()) {
                        if let Some(rung) = parse_rung(&line.rung) {
                            ladder.standings.entry(line.class).or_default().rung = rung;
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(ladder)
    }

    /// Apply one outcome to the in-memory standing. Demotion happens HERE, not
    /// at some later reconciliation, so a class cannot ship twice on authority
    /// its first failure already cost it.
    fn fold_outcome(
        &mut self,
        class: &FailureClass,
        outcome: Outcome,
        recorded_at_ms: u64,
    ) -> Option<Demotion> {
        let standing = self.standings.entry(class.clone()).or_default();
        if outcome.is_clean() {
            standing.streak = standing.streak.saturating_add(1);
            standing.total_clean = standing.total_clean.saturating_add(1);
            // Stamped on the crossing only. A sixth and seventh clean ship do
            // not re-earn anything, so they must not move the timestamp — the
            // moment a promotion became available is a fact about the class,
            // not about the last thing that happened to it.
            if standing.streak == PROMOTION_STREAK {
                standing.earned_at_ms = Some(recorded_at_ms);
            }
            return None;
        }

        standing.streak = 0;
        standing.total_bad = standing.total_bad.saturating_add(1);
        standing.earned_at_ms = None;

        // Demotion is only meaningful out of act-alone: every lower rung
        // already stops for a human, so there is nothing to take away.
        if standing.rung != Rung::ActAlone {
            return None;
        }
        let from = standing.rung;
        standing.rung = Rung::Propose;
        Some(Demotion {
            class: class.clone(),
            from,
            to: Rung::Propose,
            because: match outcome {
                Outcome::Reverted => {
                    "a repair it shipped was rolled back".to_string()
                }
                Outcome::ShipFailed => {
                    "a ship it attempted did not complete".to_string()
                }
                Outcome::ShippedClean => unreachable!("clean outcomes return early"),
            },
        })
    }

    pub fn standing(&self, class: &FailureClass) -> Standing {
        self.standings.get(class).cloned().unwrap_or_default()
    }

    /// The rung the gate reads. Defaults to `Propose` for a class nobody has
    /// said anything about.
    pub fn rung(&self, class: &FailureClass) -> Rung {
        self.standing(class).rung
    }

    /// All known classes and their standings, for `drums authority`.
    pub fn all(&self) -> Vec<(FailureClass, Standing)> {
        let mut v: Vec<_> = self.standings.iter().map(|(c, s)| (c.clone(), s.clone())).collect();
        // Deterministic order — a listing that reshuffles between runs is
        // unreadable and makes diffs in an audit export meaningless.
        v.sort_by_key(|(c, _)| c.key());
        v
    }

    /// Classes that have earned a promotion PROPOSAL. Applying one is a human
    /// action (`drums authority promote <class>`); nothing here does it.
    pub fn promotion_candidates(&self) -> Vec<PromotionCandidate> {
        let mut out: Vec<PromotionCandidate> = self
            .standings
            .iter()
            .filter(|(_, s)| s.rung == Rung::Propose && s.streak >= PROMOTION_STREAK)
            .map(|(class, s)| PromotionCandidate {
                class: class.clone(),
                from: s.rung,
                to: Rung::ActAlone,
                streak: s.streak,
                earned_at_ms: s.earned_at_ms.unwrap_or(0),
                because: format!(
                    "{} consecutive repairs for {} shipped and stayed shipped. Promoting to \
                     act-alone means Drums deploys this class without waiting for you — and \
                     one rollback drops it straight back here.",
                    s.streak,
                    class.key()
                ),
            })
            .collect();
        out.sort_by_key(|c| c.class.key());
        out
    }
}

/// Record an outcome and return the demotion it caused, if any.
///
/// Appends BEFORE returning, so a caller that crashes between the outcome and
/// its narration still has the demotion durably recorded. The alternative
/// ordering loses authority changes on exactly the runs where something went
/// wrong — which is when they matter most.
pub fn record_outcome(
    record_path: &Path,
    class: &FailureClass,
    outcome: Outcome,
    failure_id: &str,
    now_ms: u64,
) -> Result<Option<Demotion>, AuthorityError> {
    let mut ladder = Ladder::load(record_path)?;
    let demotion = ladder.fold_outcome(class, outcome, now_ms);

    engine_record::append(
        record_path,
        KIND_OUTCOME,
        &OutcomeLine { class: class.clone(), outcome, failure_id: failure_id.to_string() },
        now_ms,
    )
    .map_err(|e| AuthorityError::Append(e.to_string()))?;

    if let Some(d) = &demotion {
        engine_record::append(
            record_path,
            KIND_RUNG,
            &RungLine {
                class: class.clone(),
                rung: rung_name(d.to).to_string(),
                reason: format!("demoted: {}", d.because),
            },
            now_ms,
        )
        .map_err(|e| AuthorityError::Append(e.to_string()))?;
    }
    Ok(demotion)
}

/// Apply a promotion a human decided on.
///
/// Takes the class KEY as typed, and refuses an unparseable one rather than
/// creating a new class at the default rung — a typo must not silently produce
/// a class that looks promoted in a listing.
pub fn promote(
    record_path: &Path,
    class_key: &str,
    now_ms: u64,
) -> Result<(FailureClass, Rung), AuthorityError> {
    let class = FailureClass::parse(class_key)
        .ok_or_else(|| AuthorityError::BadClassKey(class_key.to_string()))?;
    engine_record::append(
        record_path,
        KIND_RUNG,
        &RungLine {
            class: class.clone(),
            rung: rung_name(Rung::ActAlone).to_string(),
            reason: "promoted: applied by a human".to_string(),
        },
        now_ms,
    )
    .map_err(|e| AuthorityError::Append(e.to_string()))?;
    Ok((class, Rung::ActAlone))
}

/// Drop a class back to `Propose` on request. Always allowed, never requires a
/// reason, and never asks twice: reducing what software may do on its own must
/// be the easiest operation in the system.
pub fn demote(
    record_path: &Path,
    class_key: &str,
    now_ms: u64,
) -> Result<(FailureClass, Rung), AuthorityError> {
    let class = FailureClass::parse(class_key)
        .ok_or_else(|| AuthorityError::BadClassKey(class_key.to_string()))?;
    engine_record::append(
        record_path,
        KIND_RUNG,
        &RungLine {
            class: class.clone(),
            rung: rung_name(Rung::Propose).to_string(),
            reason: "demoted: requested by a human".to_string(),
        },
        now_ms,
    )
    .map_err(|e| AuthorityError::Append(e.to_string()))?;
    Ok((class, Rung::Propose))
}
