//! `drums bet` — the Product Bet surface.
//!
//! # Why the bet is a command and not a form
//!
//! Same sorting rule as `drums hypothesize`: a bet is a product judgment, and
//! judgment stays with the human, visibly, in the terminal, under their own
//! hands. Drums (or the dashboard, or a concierge) can *draft* the command;
//! only a person runs it, and only a person confirms it.
//!
//! # What creating a bet writes
//!
//! One `bet` row plus the full hypothesis protocol underneath it — the bet's
//! belief becomes the hypothesis statement, its evidence the citations, its
//! expectation the evaluation plan. The plan is **not optional** here the way
//! it is for a bare hypothesis: a pre-registration that does not say what it
//! expects is a diary entry, not a bet.
//!
//! # The lifecycle, and who moves it
//!
//! - `create` → Proposed. A draft. Nothing is committed to yet.
//! - `confirm` → Confirmed, and the hypothesis underneath is accepted. This is
//!   the pre-registration moment — the belief is now on record, timestamped,
//!   before the outcome exists.
//! - `decline` → Declined, with the reason. Kept forever; a decision not to
//!   act is a decision, and next year's team deserves to find it.
//! - the engine → Evaluated, when the linked change's outcome lands. The
//!   verdict is derived, never asserted — see [`engine_core::bet`].
//! - `learn` → the human's note about what the evaluation taught. Only after
//!   a verdict or a decline: before the outcome, a "learning" is a prediction
//!   wearing the wrong name.

use engine_core::bet::{
    BetId, BetStatus, BetStatusChanged, ProductBet, RolloutDesign, Support, Verdict, RECORD_KIND,
    STATUS_KIND,
};
use engine_core::change::{Change, OutcomeRecorded, OUTCOME_KIND};
use engine_core::hypothesis::{Hypothesis, Status as HypothesisStatus};

use crate::change_cmd;
use crate::hypothesize::{self, HypothesizeArgs, Refusal};

/// Everything `drums bet create` was asked, before validation.
#[derive(Debug, Clone, Default)]
pub struct BetCreateArgs {
    /// What we believe. Becomes the hypothesis statement.
    pub belief: String,
    /// Why — the evidence and reasoning, as the team would say it.
    pub because: String,
    /// Who this should affect. Optional; absent means "everyone", honestly.
    pub audience: Option<String>,
    /// Roads not taken. Each `--alt` is one.
    pub alternatives: Vec<String>,
    /// Observation ids the belief cites — at least one, from the record.
    pub cites: Vec<String>,
    /// The expectation — required, whole: name, start, success, metric,
    /// optional guardrails and window. Same vocabulary as `drums hypothesize`.
    pub plan: HypothesizeArgs,
}

/// Validate and build the bet plus the hypothesis underneath it. Pure.
pub fn build(
    args: &BetCreateArgs,
    lines: &[(String, serde_json::Value)],
    bet_id: &str,
    hyp_id: &str,
    plan_id: &str,
    now_ms: u64,
) -> Result<(ProductBet, Hypothesis), Refusal> {
    // Validate the bet's own vocabulary before anything touches the
    // hypothesis machinery underneath — a person who typed --belief must
    // never be corrected about a "statement" they never wrote (audit R12).
    if args.belief.trim().is_empty() {
        return Err(Refusal(
            "a bet needs --belief — what you believe will get better, and how".into(),
        ));
    }
    if args.because.trim().is_empty() {
        return Err(Refusal(
            "a bet records why it was made — pass --because with the evidence and reasoning, in your own words".into(),
        ));
    }
    let mut hyp_args = args.plan.clone();
    hyp_args.statement = args.belief.clone();
    hyp_args.cites = args.cites.clone();
    let hypothesis = hypothesize::build(&hyp_args, lines, hyp_id, plan_id, now_ms)?;
    if hypothesis.plan.is_none() {
        return Err(Refusal(
            "a bet pre-registers what it expects to happen — the plan is not optional here: add --name, --start, --success, and --metric (guardrails and window recommended)".into(),
        ));
    }
    let bet = ProductBet::new(
        bet_id,
        &args.belief,
        &args.because,
        hypothesis.id.clone(),
        now_ms,
    )
    .ok_or_else(|| Refusal("a bet needs a non-empty belief and rationale".into()))?;
    let bet = match &args.audience {
        Some(a) if !a.trim().is_empty() => bet.with_audience(a.clone()),
        _ => bet,
    };
    let bet = bet.with_alternatives(args.alternatives.iter().cloned());
    Ok((bet, hypothesis))
}

/// The record lines creating a bet appends: the hypothesis protocol first
/// (row + citation status lines), then the bet row that cites it.
pub fn record_lines(bet: &ProductBet, hypothesis: &Hypothesis) -> Vec<(String, serde_json::Value)> {
    let mut lines = hypothesis.record_lines();
    if let Ok(v) = serde_json::to_value(bet) {
        lines.push((RECORD_KIND.to_string(), v));
    }
    lines
}

fn find_bet(
    lines: &[(String, serde_json::Value)],
    id: &str,
) -> Result<(ProductBet, BetStatus), Refusal> {
    let bid = BetId(id.to_string());
    let bet = ProductBet::all(lines.iter())
        .into_iter()
        .find(|b| b.id == bid)
        .ok_or_else(|| {
            Refusal(format!(
                "bet {id:?} is not in this repo's record — `drums bet show` lists what is"
            ))
        })?;
    let status = ProductBet::current_status(lines.iter(), &bid).unwrap_or(BetStatus::Proposed);
    Ok((bet, status))
}

/// Confirm a proposed bet: the human pre-registration moment. Appends the bet
/// status line and accepts the hypothesis underneath, so the chain to a change
/// is open the moment the bet is confirmed.
pub fn confirm(
    lines: &[(String, serde_json::Value)],
    id: &str,
) -> Result<Vec<(String, serde_json::Value)>, Refusal> {
    let (bet, status) = find_bet(lines, id)?;
    match status {
        BetStatus::Proposed => {}
        BetStatus::Confirmed => return Err(Refusal(format!("bet {id:?} is already confirmed"))),
        BetStatus::Declined { reason } => {
            return Err(Refusal(format!(
                "bet {id:?} was declined: {reason:?} — a declined bet stays declined; create a new one if the decision changed"
            )))
        }
        BetStatus::Evaluated { .. } | BetStatus::Learned { .. } => {
            return Err(Refusal(format!("bet {id:?} has already been evaluated")))
        }
    }
    let mut out = Vec::new();
    // Accept the hypothesis underneath. If it was already accepted by hand,
    // that is agreement rather than conflict — the bet line still lands.
    match change_cmd::decide(lines, &bet.hypothesis.0, HypothesisStatus::Accepted) {
        Ok(line) => out.push(line),
        Err(e) if e.0.contains("already accepted") => {}
        Err(e) => return Err(e),
    }
    let status = BetStatusChanged {
        bet: bet.id,
        status: BetStatus::Confirmed,
    };
    out.push((
        STATUS_KIND.to_string(),
        serde_json::to_value(&status).map_err(|e| Refusal(e.to_string()))?,
    ));
    Ok(out)
}

/// Decline a proposed bet, with the reason — recorded, not deleted.
pub fn decline(
    lines: &[(String, serde_json::Value)],
    id: &str,
    reason: &str,
) -> Result<Vec<(String, serde_json::Value)>, Refusal> {
    if reason.trim().is_empty() {
        return Err(Refusal(
            "declining a bet records why — pass --reason; next year's team will want it".into(),
        ));
    }
    let (bet, status) = find_bet(lines, id)?;
    match status {
        BetStatus::Proposed => {}
        other => {
            return Err(Refusal(format!(
                "only a proposed bet can be declined — bet {id:?} is {}",
                status_word(&other)
            )))
        }
    }
    let mut out = Vec::new();
    match change_cmd::decide(
        lines,
        &bet.hypothesis.0,
        HypothesisStatus::Rejected {
            reason: reason.to_string(),
        },
    ) {
        Ok(line) => out.push(line),
        Err(e) if e.0.contains("already rejected") => {}
        Err(e) => return Err(e),
    }
    let status = BetStatusChanged {
        bet: bet.id,
        status: BetStatus::Declined {
            reason: reason.to_string(),
        },
    };
    out.push((
        STATUS_KIND.to_string(),
        serde_json::to_value(&status).map_err(|e| Refusal(e.to_string()))?,
    ));
    Ok(out)
}

/// Record what the team learned. Gated on a verdict or a decline: before the
/// outcome exists, a "learning" is a prediction wearing the wrong name.
pub fn learn(
    lines: &[(String, serde_json::Value)],
    id: &str,
    note: &str,
) -> Result<(String, serde_json::Value), Refusal> {
    if note.trim().is_empty() {
        return Err(Refusal("a learning needs words — pass --note".into()));
    }
    let (bet, status) = find_bet(lines, id)?;
    match status {
        BetStatus::Evaluated { .. } | BetStatus::Declined { .. } | BetStatus::Learned { .. } => {}
        BetStatus::Proposed | BetStatus::Confirmed => {
            return Err(Refusal(format!(
                "bet {id:?} has no outcome yet — a learning comes after the evaluation (or after a decline), not before"
            )))
        }
    }
    let line = BetStatusChanged {
        bet: bet.id,
        status: BetStatus::Learned {
            note: note.to_string(),
        },
    };
    Ok((
        STATUS_KIND.to_string(),
        serde_json::to_value(&line).map_err(|e| Refusal(e.to_string()))?,
    ))
}

/// A confirmed bet whose chain has reached an outcome, with the verdict
/// derived from it. What the engine appends on its tick.
pub struct DueBet {
    pub bet: ProductBet,
    pub verdict: Verdict,
    /// From/to/entries of the measurement, for narration — `None` when the
    /// outcome was unmeasured.
    pub measured: Option<(f64, f64, u32)>,
}

/// Find every confirmed bet whose linked change has a recorded outcome, and
/// derive its verdict. Pure; the caller appends the returned status lines.
///
/// Today every change is a full rollout, so every verdict carries Low causal
/// confidence with the reason spelled out. When holdouts exist, the design
/// will be read from the change; it is never a flag someone can set to sound
/// more certain.
pub fn evaluate_due(lines: &[(String, serde_json::Value)]) -> Vec<DueBet> {
    let changes = Change::all(lines.iter());
    let outcomes: Vec<OutcomeRecorded> = lines
        .iter()
        .filter(|(kind, _)| kind == OUTCOME_KIND)
        .filter_map(|(_, v)| serde_json::from_value(v.clone()).ok())
        .collect();

    ProductBet::all(lines.iter())
        .into_iter()
        .filter(|b| {
            matches!(
                ProductBet::current_status(lines.iter(), &b.id),
                Some(BetStatus::Confirmed)
            )
        })
        .filter_map(|bet| {
            let change = changes.iter().find(|c| c.hypothesis == bet.hypothesis)?;
            let recorded = outcomes.iter().find(|o| o.change == change.id)?;
            let verdict = Verdict::from_outcome(recorded, RolloutDesign::Full);
            let measured = match &recorded.outcome {
                engine_core::evaluation::Outcome::Measured {
                    from, to, entries, ..
                } => Some((*from, *to, *entries)),
                engine_core::evaluation::Outcome::Unmeasured(_) => None,
            };
            Some(DueBet {
                bet,
                verdict,
                measured,
            })
        })
        .collect()
}

/// The status line an evaluated bet appends.
pub fn evaluated_line(due: &DueBet) -> Option<(String, serde_json::Value)> {
    let line = BetStatusChanged {
        bet: due.bet.id.clone(),
        status: BetStatus::Evaluated {
            verdict: due.verdict.clone(),
        },
    };
    Some((STATUS_KIND.to_string(), serde_json::to_value(&line).ok()?))
}

fn status_word(s: &BetStatus) -> &'static str {
    match s {
        BetStatus::Proposed => "proposed",
        BetStatus::Confirmed => "confirmed",
        BetStatus::Declined { .. } => "declined",
        BetStatus::Evaluated { .. } => "evaluated",
        // Unreachable through the fold (learning never displaces a terminal
        // status) — honest anyway for a hand-forged record.
        BetStatus::Learned { .. } => "learned",
    }
}

pub fn level_word(level: engine_core::bet::Confidence) -> &'static str {
    match level {
        engine_core::bet::Confidence::Low => "low",
        engine_core::bet::Confidence::Medium => "medium",
        engine_core::bet::Confidence::High => "high",
    }
}

pub fn support_word(s: Support) -> &'static str {
    match s {
        Support::Supported => "supported",
        Support::NotSupported => "not supported",
        Support::Inconclusive => "inconclusive",
    }
}

/// One bet, rendered as the card the whole product is about. `verbose` adds
/// the chain and learnings; the list view passes false.
pub fn render_bet(
    lines: &[(String, serde_json::Value)],
    bet: &ProductBet,
    verbose: bool,
) -> String {
    let status = ProductBet::current_status(lines.iter(), &bet.id).unwrap_or(BetStatus::Proposed);
    let mut out = String::new();
    out.push_str(&format!(
        "bet {} · {} · inferred\n",
        bet.id,
        status_word(&status)
    ));
    out.push_str(&format!("  BET      {}\n", bet.belief));
    out.push_str(&format!("  BECAUSE  {}\n", bet.rationale));
    if let Some(a) = &bet.audience {
        out.push_str(&format!("  FOR      {a}\n"));
    }
    let hyp = Hypothesis::all(lines.iter())
        .into_iter()
        .find(|h| h.id == bet.hypothesis);
    if let Some(plan) = hyp.as_ref().and_then(|h| h.plan.as_ref()) {
        out.push_str(&format!(
            "  EXPECT   {} · {} → {} · window {}d, ≥{} entries, min effect {}\n",
            plan.metric.label(),
            plan.start_event,
            plan.success_event,
            plan.window.days,
            plan.window.min_entries,
            plan.window.min_effect
        ));
        if !plan.guardrails.is_empty() {
            let g = plan
                .guardrails
                .iter()
                .map(|g| format!("{} (tolerance {})", g.metric.label(), g.tolerance))
                .collect::<Vec<_>>()
                .join(", ");
            out.push_str(&format!("  GUARD    {g}\n"));
        }
    }
    if verbose {
        for alt in &bet.alternatives {
            out.push_str(&format!("  NOT      {alt}\n"));
        }
    }
    match &status {
        BetStatus::Evaluated { verdict } => {
            out.push_str(&format!(
                "  VERDICT  {} · causal confidence {}: {}\n",
                support_word(verdict.support),
                level_word(verdict.causal_confidence.level),
                verdict.causal_confidence.basis
            ));
            if !verdict.unread_guardrails.is_empty() {
                out.push_str(&format!(
                    "  UNREAD   guardrails not read: {}\n",
                    verdict.unread_guardrails.join(", ")
                ));
            }
        }
        BetStatus::Declined { reason } => {
            out.push_str(&format!("  DECLINED {reason}\n"));
        }
        _ => {}
    }
    // The slow loop's rows: what the same metric read at 7/30/90 days, beside
    // the verdict and never over it. A revisit describes the METRIC —
    // improved/neutral/regressed and the numbers — and issues no verdict, so
    // the supported/not-supported vocabulary never appears on these lines.
    if let Some(change) = Change::all(lines.iter())
        .into_iter()
        .find(|c| c.hypothesis == bet.hypothesis)
    {
        let original = OutcomeRecorded::for_change(lines.iter(), &change.id);
        let mut revisits = engine_core::change::Revisit::for_change(lines.iter(), &change.id);
        revisits.sort_by_key(|r| r.horizon_days);
        for r in revisits {
            use engine_core::evaluation::Outcome;
            let row = match &r.outcome {
                Outcome::Measured {
                    direction,
                    from,
                    to,
                    entries,
                    ..
                } => {
                    let reading = format!(
                        "{} {}",
                        crate::render::direction_word(*direction),
                        crate::render::metric_reading(change.plan.metric, *from, *to, *entries)
                    );
                    let then = original.as_ref().and_then(|o| match &o.outcome {
                        Outcome::Measured { direction, .. } => Some(*direction),
                        Outcome::Unmeasured(_) => None,
                    });
                    match then {
                        Some(then) if then != *direction => {
                            format!("{reading} — the window's move did not persist")
                        }
                        Some(_) => format!("{reading} — the window's move persisted"),
                        None => format!("{reading} — readable now; the original window was not"),
                    }
                }
                Outcome::Unmeasured(u) => u.sentence(),
            };
            out.push_str(&format!("  REVISIT  {}d · {row}\n", r.horizon_days));
        }
    }
    for note in ProductBet::learnings(lines.iter(), &bet.id) {
        out.push_str(&format!("  LEARNED  {note}\n"));
    }
    if verbose {
        // Prior evaluated bets on the same metric: the memory the record earns.
        if let Some(metric) = hyp.as_ref().and_then(|h| h.plan.as_ref()).map(|p| p.metric) {
            for other in ProductBet::all(lines.iter()) {
                if other.id == bet.id {
                    continue;
                }
                let same_metric = Hypothesis::all(lines.iter())
                    .into_iter()
                    .find(|h| h.id == other.hypothesis)
                    .and_then(|h| h.plan)
                    .is_some_and(|p| p.metric == metric);
                if !same_metric {
                    continue;
                }
                if let Some(BetStatus::Evaluated { verdict }) =
                    ProductBet::current_status(lines.iter(), &other.id)
                {
                    out.push_str(&format!(
                        "  PRIOR    bet {} on {} was {} — \"{}\"\n",
                        other.id,
                        metric.label(),
                        support_word(verdict.support),
                        other.belief
                    ));
                }
            }
        }
    }
    out
}

/// Every bet in the record, newest first.
pub fn render_all(lines: &[(String, serde_json::Value)]) -> String {
    let mut bets = ProductBet::all(lines.iter());
    if bets.is_empty() {
        return "no bets in this repo's record yet — `drums bet create` makes the first one\n"
            .into();
    }
    bets.sort_by_key(|b| std::cmp::Reverse(b.created_at_ms));
    let mut out = String::new();
    for b in &bets {
        out.push_str(&render_bet(lines, b, false));
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_core::evaluation::Sample;
    use engine_core::observation::{Kind, Observation, Source, Window};

    fn seeded() -> Vec<(String, serde_json::Value)> {
        let o = Observation::fact(
            "obs_1",
            Source::Runtime,
            Kind::RateShift {
                previous: 0.1,
                since_deploy: Some("abc".into()),
            },
            Window::new(0, 1).unwrap(),
            1,
        );
        vec![(
            engine_core::observation::RECORD_KIND.to_string(),
            serde_json::to_value(&o).unwrap(),
        )]
    }

    fn create_args() -> BetCreateArgs {
        BetCreateArgs {
            belief: "simplified project creation will raise onboarding completion".into(),
            because: "abandonment rose after the March redesign; two support tickets a week mention the form".into(),
            audience: Some("new self-serve users".into()),
            alternatives: vec!["reorder the form fields instead".into()],
            cites: vec!["obs_1".into()],
            plan: HypothesizeArgs {
                name: Some("Onboarding".into()),
                start: Some("signup_completed".into()),
                success: Some("first_project_created".into()),
                metric: Some("completion_rate".into()),
                guardrails: vec!["error_event_rate".into()],
                window_days: Some(14),
                min_entries: Some(100),
                ..Default::default()
            },
        }
    }

    /// Record a created bet into lines, confirmed or not.
    fn recorded(confirm_it: bool) -> (Vec<(String, serde_json::Value)>, ProductBet) {
        let mut lines = seeded();
        let (bet, hyp) = build(&create_args(), &lines, "bet_1", "hyp_1", "eval_1", 1_000).unwrap();
        lines.extend(record_lines(&bet, &hyp));
        if confirm_it {
            lines.extend(confirm(&lines.clone(), "bet_1").unwrap());
        }
        (lines, bet)
    }

    #[test]
    fn a_bet_without_its_expectation_is_refused_not_recorded() {
        let lines = seeded();
        let mut a = create_args();
        a.plan = HypothesizeArgs::default();
        let err = build(&a, &lines, "b", "h", "e", 1).unwrap_err();
        assert!(err.0.contains("pre-registers"), "{err}");
        // And half an expectation is the hypothesize refusal, unchanged.
        let mut half = create_args();
        half.plan.metric = None;
        half.plan.success = None;
        assert!(build(&half, &lines, "b", "h", "e", 1).is_err());
    }

    #[test]
    fn a_bet_without_its_because_is_refused() {
        let lines = seeded();
        let mut a = create_args();
        a.because = "  ".into();
        let err = build(&a, &lines, "b", "h", "e", 1).unwrap_err();
        assert!(err.0.contains("--because"), "{err}");
    }

    #[test]
    fn creating_writes_the_hypothesis_protocol_plus_the_bet_row() {
        let (lines, bet) = recorded(false);
        assert_eq!(ProductBet::all(lines.iter()).len(), 1);
        assert_eq!(Hypothesis::all(lines.iter()).len(), 1);
        assert_eq!(bet.hypothesis.0, "hyp_1");
        // Proposed until a human says otherwise.
        assert!(matches!(
            ProductBet::current_status(lines.iter(), &bet.id),
            Some(BetStatus::Proposed)
        ));
    }

    #[test]
    fn confirming_accepts_the_hypothesis_and_marks_the_bet() {
        let (lines, bet) = recorded(true);
        assert!(matches!(
            ProductBet::current_status(lines.iter(), &bet.id),
            Some(BetStatus::Confirmed)
        ));
        assert_eq!(
            Hypothesis::current_status(lines.iter(), &bet.hypothesis),
            Some(HypothesisStatus::Accepted),
            "the chain to a change opens at confirmation"
        );
        // Confirming twice is refused, not repeated.
        assert!(confirm(&lines, "bet_1")
            .unwrap_err()
            .0
            .contains("already confirmed"));
    }

    #[test]
    fn declining_needs_a_reason_and_stays_declined() {
        let (mut lines, _) = recorded(false);
        assert!(decline(&lines, "bet_1", " ")
            .unwrap_err()
            .0
            .contains("--reason"));
        lines.extend(decline(&lines.clone(), "bet_1", "twentyfour26 asked us to hold").unwrap());
        assert!(matches!(
            ProductBet::current_status(lines.iter(), &BetId("bet_1".into())),
            Some(BetStatus::Declined { .. })
        ));
        let err = confirm(&lines, "bet_1").unwrap_err();
        assert!(err.0.contains("stays declined"), "{err}");
        // A declined bet may still teach — and the card keeps saying
        // declined, with the reason, after the learning lands (audit R9).
        lines.push(learn(&lines.clone(), "bet_1", "we defer to partner bandwidth").unwrap());
        let bet = ProductBet::all(lines.iter()).into_iter().next().unwrap();
        let card = render_bet(&lines, &bet, true);
        assert!(card.contains("· declined ·"), "{card}");
        assert!(
            card.contains("DECLINED twentyfour26 asked us to hold"),
            "{card}"
        );
        assert!(card.contains("LEARNED"), "{card}");
    }

    #[test]
    fn learning_before_the_outcome_is_refused_by_name() {
        let (lines, _) = recorded(true);
        let err = learn(&lines, "bet_1", "it worked great").unwrap_err();
        assert!(err.0.contains("no outcome yet"), "{err}");
    }

    fn shipped_and_measured(
        lines: &mut Vec<(String, serde_json::Value)>,
        bet: &ProductBet,
        direction: engine_core::evaluation::Direction,
    ) {
        let hyp = Hypothesis::all(lines.iter())
            .into_iter()
            .find(|h| h.id == bet.hypothesis)
            .unwrap();
        let change = Change::new(
            "chg_1",
            &hyp,
            Hypothesis::current_status(lines.iter(), &hyp.id),
            "deadbeef",
            Sample {
                value: 0.61,
                entries: 340,
            },
            2_000,
        )
        .unwrap();
        lines.push((
            engine_core::change::RECORD_KIND.to_string(),
            serde_json::to_value(&change).unwrap(),
        ));
        let outcome = OutcomeRecorded {
            change: change.id.clone(),
            measured_at_ms: 9_000,
            outcome: engine_core::evaluation::Outcome::Measured {
                direction,
                from: 0.61,
                to: 0.74,
                entries: 380,
                guardrails: engine_core::evaluation::Guardrails::Held,
            },
            unread_guardrails: vec!["support contacts".into()],
        };
        lines.push((
            OUTCOME_KIND.to_string(),
            serde_json::to_value(&outcome).unwrap(),
        ));
    }

    #[test]
    fn an_outcome_evaluates_the_bet_with_low_confidence_spelled_out() {
        let (mut lines, bet) = recorded(true);
        shipped_and_measured(
            &mut lines,
            &bet,
            engine_core::evaluation::Direction::Positive,
        );

        let due = evaluate_due(&lines);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].verdict.support, Support::Supported);
        assert_eq!(due[0].measured, Some((0.61, 0.74, 380)));
        // The pre-registration property holds and is checkable.
        assert!(due[0].bet.preregistered_before(2_000));

        lines.push(evaluated_line(&due[0]).unwrap());
        // Once evaluated, it is not due again — the tick is idempotent.
        assert!(evaluate_due(&lines).is_empty());
        // And the card says the honest thing.
        let card = render_bet(&lines, &bet, true);
        assert!(card.contains("supported"), "{card}");
        assert!(card.contains("no holdout"), "{card}");
        assert!(
            card.contains("guardrails not read: support contacts"),
            "{card}"
        );
        // A learning may now land, and the verdict survives it.
        lines.push(learn(&lines.clone(), "bet_1", "returning users need the same fix").unwrap());
        let card = render_bet(&lines, &bet, true);
        assert!(card.contains("LEARNED"), "{card}");
        assert!(card.contains("supported"), "{card}");
    }

    #[test]
    fn an_unconfirmed_bet_is_never_evaluated() {
        // A proposed bet is a draft; drafting is free and commits nobody.
        let (mut lines, bet) = recorded(false);
        // Ship the change anyway by accepting the hypothesis directly — the
        // operator did it by hand, outside the bet.
        lines.push(
            change_cmd::decide(
                &lines.clone(),
                &bet.hypothesis.0,
                HypothesisStatus::Accepted,
            )
            .unwrap(),
        );
        shipped_and_measured(
            &mut lines,
            &bet,
            engine_core::evaluation::Direction::Positive,
        );
        assert!(evaluate_due(&lines).is_empty(), "proposed is not confirmed");
    }

    /// The card's REVISIT rows describe the metric — direction words and both
    /// numbers — and never issue a verdict: no supported/not supported/
    /// inconclusive on those lines, whatever the horizons showed. Rows render
    /// in horizon order regardless of the order the lines landed, and the
    /// VERDICT line above them stands unedited.
    #[test]
    fn the_bet_card_renders_revisit_rows_without_verdict_vocabulary() {
        use engine_core::change::{Revisit, REVISIT_KIND};
        use engine_core::evaluation::{Direction, Guardrails, Outcome};
        let (mut lines, bet) = recorded(true);
        shipped_and_measured(&mut lines, &bet, Direction::Positive);
        let due = evaluate_due(&lines);
        lines.push(evaluated_line(&due[0]).unwrap());

        let revisit = |horizon: u32, direction: Direction, to: f64| Revisit {
            change: engine_core::change::ChangeId("chg_1".into()),
            horizon_days: horizon,
            measured_at_ms: 99_000,
            outcome: Outcome::Measured {
                direction,
                from: 0.61,
                to,
                entries: 400,
                guardrails: Guardrails::Held,
            },
        };
        // Landed out of order on purpose: 90 first, then 30.
        lines.push((
            REVISIT_KIND.to_string(),
            serde_json::to_value(revisit(90, Direction::Positive, 0.74)).unwrap(),
        ));
        lines.push((
            REVISIT_KIND.to_string(),
            serde_json::to_value(revisit(30, Direction::Neutral, 0.62)).unwrap(),
        ));

        let card = render_bet(&lines, &bet, true);
        assert!(card.contains("REVISIT  30d · neutral 0.61 → 0.62 over 400 entries — the window's move did not persist"), "{card}");
        assert!(card.contains("REVISIT  90d · improved 0.61 → 0.74 over 400 entries — the window's move persisted"), "{card}");
        let thirty = card.find("REVISIT  30d").unwrap();
        let ninety = card.find("REVISIT  90d").unwrap();
        assert!(thirty < ninety, "horizon order, not landing order: {card}");
        // The verdict above the rows is untouched…
        assert!(card.contains("VERDICT  supported"), "{card}");
        // …and no revisit row borrows its vocabulary.
        for line in card
            .lines()
            .filter(|l| l.trim_start().starts_with("REVISIT"))
        {
            for verdict_word in ["supported", "inconclusive"] {
                assert!(
                    !line.to_lowercase().contains(verdict_word),
                    "a revisit describes the metric, it issues no verdict: {line}"
                );
            }
        }
    }

    #[test]
    fn a_prior_bet_on_the_same_metric_surfaces_in_the_card() {
        let (mut lines, bet) = recorded(true);
        shipped_and_measured(
            &mut lines,
            &bet,
            engine_core::evaluation::Direction::Positive,
        );
        let due = evaluate_due(&lines);
        lines.push(evaluated_line(&due[0]).unwrap());

        // A second bet on the same metric, citing the same observation.
        let (bet2, hyp2) = build(
            &BetCreateArgs {
                belief: "a progress bar will raise onboarding completion further".into(),
                because:
                    "the first simplification was supported; the drop now concentrates at step two"
                        .into(),
                cites: vec!["obs_1".into()],
                plan: create_args().plan,
                ..Default::default()
            },
            &lines,
            "bet_2",
            "hyp_2",
            "eval_2",
            10_000,
        )
        .unwrap();
        lines.extend(record_lines(&bet2, &hyp2));
        let card = render_bet(&lines, &bet2, true);
        assert!(card.contains("PRIOR"), "{card}");
        assert!(card.contains("bet_1"), "the memory names the bet: {card}");
        assert!(
            !render_bet(&lines, &bet, true).contains("PRIOR    bet bet_1"),
            "a bet is not its own prior"
        );
    }
}
