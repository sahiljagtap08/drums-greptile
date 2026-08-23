//! `drums hypothesis accept|reject` and `drums change` — closing the loop's
//! third link, plus the measurement the fourth depends on.
//!
//! # Accepting is a decision, so it is a command
//!
//! The hypothesis status lines existed before these commands; what was missing
//! was the deliberate act. Accept and reject are one-line appends with a named
//! human meaning behind them — and rejection requires a reason, because a
//! rejected hypothesis with its reason is exactly what the next person
//! proposing about the same observations should read first.
//!
//! # A change freezes its terms
//!
//! `drums change` cites one accepted, planned hypothesis, takes the baseline
//! reading at that moment, snapshots the plan, and appends the change. The
//! plan you ship under is the plan you are measured under.
//!
//! # Only what the record can honestly measure
//!
//! Today Drums can read exactly one metric from its own record with zero
//! configuration: `error_event_rate`, with hours as the denominator. A plan on
//! any other metric is refused at `drums change` with the limit stated —
//! never approximated, never silently accepted and never measured. When the
//! window fully elapses, the watch loop takes the after-reading, lets
//! `Outcome::measure` decide what may be claimed, and appends the outcome
//! beside the change. Guardrails the record cannot read are listed as unread
//! on the outcome line, so "held" can never quietly mean "unwatched".

use std::path::Path;

use engine_core::change::{Change, ChangeRefused, OutcomeRecorded, Revisit};
use engine_core::evaluation::{Metric, Outcome};
use engine_core::hypothesis::{Hypothesis, HypothesisId, Status, StatusChanged, STATUS_KIND};

use crate::hypothesize::Refusal;

/// Refuse a `--commit` sha the repository does not actually hold, before
/// anything is built or appended — a change recorded against a commit git
/// cannot resolve is a record line pointing at nothing, forever.
///
/// Lives beside [`build_change`] but is deliberately NOT called by it: the
/// builder stays a pure function of record lines, and the command handler
/// runs this check against the real repo first.
pub fn verify_commit_exists(repo: &Path, sha: &str) -> Result<(), Refusal> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{sha}^{{commit}}"),
        ])
        .output();
    match out {
        Ok(o) if o.status.success() => Ok(()),
        Ok(_) => Err(Refusal(format!(
            "commit {sha:?} does not exist in the repository at {} — check the sha with `git log`, or fetch the branch that carries it first",
            repo.display()
        ))),
        Err(e) => Err(Refusal(format!(
            "could not run git to verify commit {sha:?} in {}: {e}",
            repo.display()
        ))),
    }
}

/// Append an accept/reject line for a hypothesis that exists and is still
/// open. Pure build; the caller appends.
pub fn decide(
    lines: &[(String, serde_json::Value)],
    id: &str,
    decision: Status,
) -> Result<(String, serde_json::Value), Refusal> {
    let hid = HypothesisId(id.to_string());
    let all = Hypothesis::all(lines.iter());
    if !all.iter().any(|h| h.id == hid) {
        return Err(Refusal(format!(
            "hypothesis {id:?} is not in this repo's record — `drums record` lists what is"
        )));
    }
    match Hypothesis::current_status(lines.iter(), &hid) {
        Some(Status::Open) | None => {}
        Some(Status::Accepted) => {
            return Err(Refusal(format!("hypothesis {id:?} is already accepted")))
        }
        Some(Status::Rejected { reason }) => {
            return Err(Refusal(format!(
                "hypothesis {id:?} was already rejected: {reason:?} — propose a new hypothesis rather than re-litigating a decided one"
            )))
        }
    }
    let line = StatusChanged {
        hypothesis: hid,
        status: decision,
    };
    Ok((
        STATUS_KIND.to_string(),
        serde_json::to_value(&line).map_err(|e| Refusal(e.to_string()))?,
    ))
}

/// Build the change against the record: resolve the hypothesis, require
/// accepted + planned, take the baseline, freeze the terms.
pub fn build_change(
    lines: &[(String, serde_json::Value)],
    hypothesis_id: &str,
    sha: &str,
    change_id: &str,
    now_ms: u64,
    behavior_baseline: Option<engine_core::evaluation::Sample>,
) -> Result<Change, Refusal> {
    let hid = HypothesisId(hypothesis_id.to_string());
    let all = Hypothesis::all(lines.iter());
    let Some(hypothesis) = all.iter().find(|h| h.id == hid) else {
        return Err(Refusal(
            ChangeRefused::UnknownHypothesis(hypothesis_id.to_string()).to_string(),
        ));
    };
    let folded = Hypothesis::current_status(lines.iter(), &hid);

    let metric = hypothesis.plan.as_ref().map(|p| p.metric);
    let window_days = hypothesis.plan.as_ref().map(|p| p.window.days).unwrap_or(7);

    // Which source reads this metric, and therefore where the baseline comes
    // from. Refused here, at change time, rather than discovered at
    // measurement time a week later.
    let baseline = match metric {
        Some(Metric::ErrorEventRate) | None => {
            let span_ms = (window_days as u64).saturating_mul(86_400_000);
            engine_detect::observe::error_event_rate(
                lines,
                now_ms.saturating_sub(span_ms),
                now_ms,
            )
            .ok_or_else(|| Refusal("could not take a baseline over an empty window".into()))?
        }
        Some(Metric::CompletionRate) | Some(Metric::Abandonment) => behavior_baseline
            .ok_or_else(|| Refusal(
                "this plan's metric is read from PostHog, and no baseline reading was supplied — configure posthog_host + posthog_project (config or env) and DRUMS_POSTHOG_API_KEY (env only), then re-run".into(),
            ))?,
        Some(other) => {
            return Err(Refusal(format!(
                "the plan's metric is {} — no source can measure it yet: the record reads error_event_rate, PostHog reads completion_rate and abandonment, and the rest wait for their sources",
                other.label()
            )))
        }
    };

    let mut change = Change::new(change_id, hypothesis, folded, sha, baseline, now_ms)
        .map_err(|e| Refusal(e.to_string()))?;
    if matches!(metric, Some(Metric::ErrorEventRate) | None) {
        // For a time-denominated metric the sample floor means "hours
        // watched": the outcome is only taken once the window fully elapses,
        // so the floor is exactly the window's hours. Behavior metrics keep
        // their declared entrant floors — their denominators are people.
        change.plan.window.min_entries = window_days.saturating_mul(24);
    }
    Ok(change)
}

/// One reading over an explicit window, from whichever source reads this
/// change's metric. `Err` is a named skip reason, never an invented sample —
/// traffic may be fine when only our read broke, so the caller skips and
/// retries next tick. Shared by [`measure_due_changes`] and
/// [`revisit_due_changes`]: the revisit re-reads the SAME metric through the
/// SAME plumbing, only over a later window.
async fn observed_sample(
    lines: &[(String, serde_json::Value)],
    change: &Change,
    from_ms: u64,
    to_ms: u64,
    behavior: Option<&dyn engine_behavior::BehaviorSource>,
) -> Result<engine_core::evaluation::Sample, String> {
    match change.plan.metric {
        Metric::ErrorEventRate => engine_detect::observe::error_event_rate(lines, from_ms, to_ms)
            .ok_or_else(|| "the reading window is empty".to_string()),
        Metric::CompletionRate | Metric::Abandonment => match behavior {
            Some(source) => source
                .sample_between(&change.plan, from_ms, to_ms)
                .await
                .map_err(|e| e.to_string()),
            None => Err(
                "the plan's metric is read from PostHog and no source is configured — posthog_host + posthog_project and DRUMS_POSTHOG_API_KEY".to_string(),
            ),
        },
        other => Err(format!("no source can measure {} yet", other.label())),
    }
}

/// Guardrail readings over an explicit after-window: an error_event_rate
/// guardrail can always be read from the record, whatever the target metric;
/// everything else has no reader yet and is honestly listed as unread rather
/// than silently held. The "before" is always the window preceding ship —
/// the same period the baseline froze — so an original measurement and a
/// later revisit compare against the same past.
fn guardrail_readings(
    lines: &[(String, serde_json::Value)],
    change: &Change,
    after_from_ms: u64,
    after_to_ms: u64,
) -> (Vec<engine_core::evaluation::GuardrailReading>, Vec<String>) {
    let window_ms = (change.plan.window.days as u64).saturating_mul(86_400_000);
    let mut readings = Vec::new();
    let mut unread: Vec<String> = Vec::new();
    for g in &change.plan.guardrails {
        if g.metric == Metric::ErrorEventRate {
            let before = engine_detect::observe::error_event_rate(
                lines,
                change.shipped_at_ms.saturating_sub(window_ms),
                change.shipped_at_ms,
            );
            let after = engine_detect::observe::error_event_rate(lines, after_from_ms, after_to_ms);
            if let (Some(b), Some(a)) = (before, after) {
                readings.push(engine_core::evaluation::GuardrailReading {
                    metric: Metric::ErrorEventRate,
                    before: b.value,
                    after: a.value,
                });
            } else {
                unread.push(g.metric.label().to_string());
            }
        } else if g.metric != change.plan.metric {
            unread.push(g.metric.label().to_string());
        }
    }
    (readings, unread)
}

/// Measure every due, unmeasured change. Pure: returns the outcome lines to
/// append. Called from the watch tick beside the observation producer.
pub async fn measure_due_changes(
    lines: &[(String, serde_json::Value)],
    now_ms: u64,
    behavior: Option<&dyn engine_behavior::BehaviorSource>,
) -> (Vec<OutcomeRecorded>, Vec<(String, String)>) {
    let mut out = Vec::new();
    let mut skipped: Vec<(String, String)> = Vec::new();
    for change in Change::all(lines.iter()) {
        if !change.due(now_ms) {
            continue;
        }
        if OutcomeRecorded::for_change(lines.iter(), &change.id).is_some() {
            continue;
        }
        let window_ms = (change.plan.window.days as u64).saturating_mul(86_400_000);
        let from = change.shipped_at_ms;
        let to = change.shipped_at_ms.saturating_add(window_ms);

        // The after-reading, from whichever source reads this metric — over
        // the FROZEN window, never a clock-relative one, however late this
        // measurement runs.
        let observed = match observed_sample(lines, &change, from, to, behavior).await {
            Ok(s) => s,
            Err(why) => {
                skipped.push((change.id.0.clone(), why));
                continue;
            }
        };
        let (guard_readings, unread) = guardrail_readings(lines, &change, from, to);
        let outcome = Outcome::measure(&change.plan, change.baseline, observed, &guard_readings);
        out.push(OutcomeRecorded {
            change: change.id.clone(),
            measured_at_ms: now_ms,
            outcome,
            unread_guardrails: unread,
        });
    }
    (out, skipped)
}

/// The slow loop's sweep: for every change that HAS a recorded outcome, take
/// any 7/30/90-day revisit whose horizon has FULLY elapsed and has not been
/// taken before. Pure: returns the revisit lines to append, plus named skips
/// (`"chg_x@30d"`, why) for readings a source could not produce — retried
/// next tick, never invented. Each (change, horizon) is measured at most
/// once; the revisit line's presence in the record is the guard, so the sweep
/// is idempotent and restart-safe. A horizon the plan's own window covers is
/// skipped structurally ([`Change::revisit_horizons`]) — it would duplicate
/// the original outcome, which this sweep appends beside, never over.
pub async fn revisit_due_changes(
    lines: &[(String, serde_json::Value)],
    now_ms: u64,
    behavior: Option<&dyn engine_behavior::BehaviorSource>,
) -> (Vec<Revisit>, Vec<(String, String)>) {
    let mut out = Vec::new();
    let mut skipped: Vec<(String, String)> = Vec::new();
    for change in Change::all(lines.iter()) {
        // A revisit only exists for a change that has a recorded outcome —
        // there is nothing to re-read where nothing was ever read.
        if OutcomeRecorded::for_change(lines.iter(), &change.id).is_none() {
            continue;
        }
        for horizon in change.revisit_horizons() {
            if !change.revisit_due(horizon, now_ms) {
                continue;
            }
            if Revisit::recorded(lines.iter(), &change.id, horizon) {
                continue;
            }
            let (from, to) = change.revisit_window(horizon);
            // The SAME metric through the SAME plumbing as the original
            // measurement — only the window is later. The baseline stays the
            // one frozen at ship.
            let observed = match observed_sample(lines, &change, from, to, behavior).await {
                Ok(s) => s,
                Err(why) => {
                    skipped.push((format!("{}@{}d", change.id.0, horizon), why));
                    continue;
                }
            };
            let (guard_readings, _unread) = guardrail_readings(lines, &change, from, to);
            let outcome =
                Outcome::measure(&change.plan, change.baseline, observed, &guard_readings);
            out.push(Revisit {
                change: change.id.clone(),
                horizon_days: horizon,
                measured_at_ms: now_ms,
                outcome,
            });
        }
    }
    (out, skipped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_core::evaluation::{Direction, EvaluationTarget, MeasurementWindow};
    use engine_core::observation::ObservationId;

    const HOUR: u64 = 3_600_000;
    const DAY: u64 = 24 * HOUR;

    fn record_with_loop(window_days: u32) -> Vec<(String, serde_json::Value)> {
        let mut lines = Vec::new();
        // An observation to cite.
        let o = engine_core::observation::Observation::fact(
            "obs_1",
            engine_core::observation::Source::Runtime,
            engine_core::observation::Kind::RateShift {
                previous: 0.1,
                since_deploy: Some("d1".into()),
            },
            engine_core::observation::Window::new(0, 1).unwrap(),
            1,
        );
        lines.push((
            engine_core::observation::RECORD_KIND.to_string(),
            serde_json::to_value(&o).unwrap(),
        ));
        // A planned hypothesis citing it.
        let h = Hypothesis::new(
            "hyp_1",
            "the promo branch",
            vec![ObservationId("obs_1".into())],
            2,
        )
        .unwrap()
        .with_plan(
            EvaluationTarget::new("eval_1", "Errors", "n/a", "n/a", Metric::ErrorEventRate)
                .with_window(MeasurementWindow {
                    days: window_days,
                    min_entries: 1,
                    min_effect: 0.1,
                }),
        );
        for l in h.record_lines() {
            lines.push(l);
        }
        lines
    }

    fn accept(lines: &mut Vec<(String, serde_json::Value)>) {
        let l = decide(lines, "hyp_1", Status::Accepted).unwrap();
        lines.push(l);
    }

    fn burst_events(
        lines: &mut Vec<(String, serde_json::Value)>,
        from: u64,
        count: u64,
        spacing: u64,
    ) {
        for i in 0..count {
            let at = from + i * spacing;
            lines.push((
                "event".to_string(),
                serde_json::json!({
                    "service": "api", "occurred_at_ms": at,
                    "error_name": "TypeError", "error_message": "m", "stack": "TypeError: m"
                }),
            ));
        }
    }

    #[tokio::test]
    async fn accept_then_change_then_a_measured_improvement() {
        let ship_at = 1_000 * DAY;
        let mut lines = record_with_loop(7);
        // A noisy week before the change: 84 events (~0.5/h)…
        burst_events(&mut lines, ship_at - 7 * DAY, 84, 2 * HOUR);
        accept(&mut lines);

        let change = build_change(&lines, "hyp_1", "abc123", "chg_1", ship_at, None).unwrap();
        assert!((change.baseline.value - 0.5).abs() < 0.01);
        assert_eq!(
            change.plan.window.min_entries,
            7 * 24,
            "the floor is hours for a time-rate"
        );
        lines.push((
            engine_core::change::RECORD_KIND.to_string(),
            serde_json::to_value(&change).unwrap(),
        ));

        // …and a nearly quiet week after: 3 events.
        burst_events(&mut lines, ship_at + DAY, 3, HOUR);

        // Not due before the window closes.
        assert!(measure_due_changes(&lines, ship_at + 6 * DAY, None)
            .await
            .0
            .is_empty());

        let outcomes = measure_due_changes(&lines, ship_at + 7 * DAY, None).await.0;
        assert_eq!(outcomes.len(), 1);
        match &outcomes[0].outcome {
            Outcome::Measured {
                direction,
                from,
                to,
                ..
            } => {
                assert_eq!(*direction, Direction::Positive, "fewer errors is better");
                assert!(to < from);
            }
            other => panic!("expected measured, got {other:?}"),
        }
        assert!(outcomes[0].outcome.is_verified_improvement());

        // Once its line lands, it is never measured twice.
        lines.push((
            engine_core::change::OUTCOME_KIND.to_string(),
            serde_json::to_value(&outcomes[0]).unwrap(),
        ));
        assert!(measure_due_changes(&lines, ship_at + 8 * DAY, None)
            .await
            .0
            .is_empty());
    }

    /// R8: `drums change --commit deadbeef` used to record a sha nothing can
    /// ever resolve. The CLI path verifies against the real repo; the check
    /// must accept a real commit and refuse a fabricated one by name.
    #[test]
    fn a_sha_the_repo_does_not_hold_is_refused_by_name() {
        let repo = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            assert!(
                std::process::Command::new("git")
                    .arg("-C")
                    .arg(repo.path())
                    .args(args)
                    .output()
                    .unwrap()
                    .status
                    .success(),
                "git {args:?} must succeed"
            );
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(repo.path().join("a.txt"), "x").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "c"]);
        let head = std::process::Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let head = String::from_utf8_lossy(&head.stdout).trim().to_string();

        assert!(
            verify_commit_exists(repo.path(), &head).is_ok(),
            "the real HEAD must pass"
        );

        let err = verify_commit_exists(repo.path(), "deadbeef").unwrap_err();
        assert!(err.0.contains("deadbeef"), "names the sha: {err}");
        assert!(
            err.0.contains(&repo.path().display().to_string()),
            "names the repo: {err}"
        );
        assert!(err.0.contains("git log"), "names a next step: {err}");
    }

    #[test]
    fn a_change_against_an_open_or_rejected_hypothesis_is_refused() {
        let ship_at = 1_000 * DAY;
        let mut lines = record_with_loop(7);
        burst_events(&mut lines, ship_at - 7 * DAY, 10, HOUR);
        let err = build_change(&lines, "hyp_1", "abc", "chg_1", ship_at, None).unwrap_err();
        assert!(err.0.contains("open, not accepted"), "{err}");
        // Reject it; a change is refused with the decision visible.
        let l = decide(
            &lines,
            "hyp_1",
            Status::Rejected {
                reason: "bot traffic".into(),
            },
        )
        .unwrap();
        lines.push(l);
        let err = build_change(&lines, "hyp_1", "abc", "chg_1", ship_at, None).unwrap_err();
        assert!(err.0.contains("rejected"), "{err}");
    }

    #[test]
    fn behavior_metrics_need_a_baseline_and_the_rest_need_a_source_that_exists() {
        let ship_at = 1_000 * DAY;
        let mk = |metric: Metric| {
            let mut lines = Vec::new();
            let o = engine_core::observation::Observation::fact(
                "obs_1",
                engine_core::observation::Source::Runtime,
                engine_core::observation::Kind::MetricReading,
                engine_core::observation::Window::new(0, 1).unwrap(),
                1,
            );
            lines.push((
                engine_core::observation::RECORD_KIND.to_string(),
                serde_json::to_value(&o).unwrap(),
            ));
            let h = Hypothesis::new("hyp_1", "x", vec![ObservationId("obs_1".into())], 2)
                .unwrap()
                .with_plan(EvaluationTarget::new("e", "Checkout", "a", "b", metric));
            for l in h.record_lines() {
                lines.push(l);
            }
            let d = decide(&lines, "hyp_1", Status::Accepted).unwrap();
            lines.push(d);
            lines
        };

        // A PostHog-read metric with no baseline supplied: refused, with the
        // configuration named.
        let lines = mk(Metric::CompletionRate);
        let err = build_change(&lines, "hyp_1", "abc", "chg_1", ship_at, None).unwrap_err();
        assert!(err.0.contains("PostHog"), "{err}");
        assert!(
            err.0.contains("DRUMS_POSTHOG_API_KEY"),
            "names the fix: {err}"
        );

        // The same metric WITH a baseline: accepted, and the declared entrant
        // floor survives — the hours override is for time-rates only.
        let baseline = engine_core::evaluation::Sample {
            value: 0.61,
            entries: 340,
        };
        let change =
            build_change(&lines, "hyp_1", "abc", "chg_1", ship_at, Some(baseline)).unwrap();
        assert_eq!(change.baseline.entries, 340);
        assert_eq!(
            change.plan.window.min_entries, 100,
            "entrant floor, not hours"
        );

        // A metric no source reads yet: refused, with the map of who reads what.
        let lines = mk(Metric::TimeToComplete);
        let err = build_change(&lines, "hyp_1", "abc", "chg_1", ship_at, None).unwrap_err();
        assert!(err.0.contains("time to complete"), "{err}");
        assert!(err.0.contains("no source can measure it yet"), "{err}");
    }

    #[test]
    fn deciding_twice_is_refused_and_rejection_reasons_survive() {
        let mut lines = record_with_loop(7);
        accept(&mut lines);
        let err = decide(&lines, "hyp_1", Status::Accepted).unwrap_err();
        assert!(err.0.contains("already accepted"), "{err}");
        // A different record: reject with a reason, then try to accept.
        let mut lines2 = record_with_loop(7);
        let l = decide(
            &lines2,
            "hyp_1",
            Status::Rejected {
                reason: "seasonal".into(),
            },
        )
        .unwrap();
        lines2.push(l);
        let err = decide(&lines2, "hyp_1", Status::Accepted).unwrap_err();
        assert!(
            err.0.contains("seasonal"),
            "the reason travels with the refusal: {err}"
        );
    }

    /// The slow loop over the record's own metric: the horizon that equals
    /// the plan's window is skipped (it would duplicate the original outcome
    /// line), the 30-day revisit reads ONLY the later window's events against
    /// the frozen baseline, and a real regression there flips the direction
    /// the close window showed — appended beside the original, never over it.
    #[tokio::test]
    async fn a_revisit_reads_the_later_window_and_the_plans_own_horizon_is_skipped() {
        let ship_at = 1_000 * DAY;
        let mut lines = record_with_loop(7);
        // The baseline week: 84 events (~0.5/h).
        burst_events(&mut lines, ship_at - 7 * DAY, 84, 2 * HOUR);
        accept(&mut lines);
        let change = build_change(&lines, "hyp_1", "abc123", "chg_1", ship_at, None).unwrap();
        lines.push((
            engine_core::change::RECORD_KIND.to_string(),
            serde_json::to_value(&change).unwrap(),
        ));
        // A quiet first week: the original outcome is an improvement.
        burst_events(&mut lines, ship_at + DAY, 3, HOUR);
        let outcomes = measure_due_changes(&lines, ship_at + 7 * DAY, None).await.0;
        lines.push((
            engine_core::change::OUTCOME_KIND.to_string(),
            serde_json::to_value(&outcomes[0]).unwrap(),
        ));

        // Day 8: the 7d horizon equals the plan's window — skipped, and 30d
        // has not elapsed. Nothing is due, and nothing is a skip.
        let (due, skips) = revisit_due_changes(&lines, ship_at + 8 * DAY, None).await;
        assert!(
            due.is_empty(),
            "no horizon may duplicate the original outcome"
        );
        assert!(skips.is_empty());
        // One millisecond before ship+30d has FULLY elapsed: still nothing.
        assert!(
            revisit_due_changes(&lines, ship_at + 30 * DAY - 1, None)
                .await
                .0
                .is_empty(),
            "a revisit never runs early"
        );

        // The regression the close window could not see: 168 events (1/h)
        // inside (ship+23d, ship+30d), and noise before that window which
        // must NOT be read into it.
        burst_events(&mut lines, ship_at + 10 * DAY, 24, HOUR);
        burst_events(&mut lines, ship_at + 23 * DAY, 168, HOUR);
        let (due, skips) = revisit_due_changes(&lines, ship_at + 30 * DAY, None).await;
        assert!(skips.is_empty());
        assert_eq!(
            due.len(),
            1,
            "30d is due; 7d is redundant; 90d has not elapsed"
        );
        assert_eq!(due[0].horizon_days, 30);
        match &due[0].outcome {
            Outcome::Measured {
                direction,
                from,
                to,
                entries,
                ..
            } => {
                assert!(
                    (from - 0.5).abs() < 0.01,
                    "the SAME frozen baseline: {from}"
                );
                assert!(
                    (to - 1.0).abs() < 1e-9,
                    "only the later window's events are read: {to}"
                );
                assert_eq!(*entries, 7 * 24, "the plan's own width, in hours");
                assert_eq!(*direction, engine_core::evaluation::Direction::Negative);
            }
            other => panic!("expected measured, got {other:?}"),
        }
        // The direction flipped from the close reading: drift.
        let original = OutcomeRecorded::for_change(lines.iter(), &due[0].change).unwrap();
        assert!(due[0].drifted_from(&original));
        // And the original outcome line still stands, unedited, beside it.
        assert!(original.outcome.is_verified_improvement());
    }

    /// A change with no recorded outcome has nothing to re-read — the sweep
    /// never invents the missing original.
    #[tokio::test]
    async fn a_revisit_only_exists_for_a_change_with_a_recorded_outcome() {
        let ship_at = 1_000 * DAY;
        let mut lines = record_with_loop(7);
        burst_events(&mut lines, ship_at - 7 * DAY, 84, 2 * HOUR);
        accept(&mut lines);
        let change = build_change(&lines, "hyp_1", "abc123", "chg_1", ship_at, None).unwrap();
        lines.push((
            engine_core::change::RECORD_KIND.to_string(),
            serde_json::to_value(&change).unwrap(),
        ));
        let (due, skips) = revisit_due_changes(&lines, ship_at + 90 * DAY, None).await;
        assert!(
            due.is_empty() && skips.is_empty(),
            "no outcome, no revisit — and no skip either: nothing was due"
        );
    }

    /// The (change, horizon) guard through the REAL record: append everything
    /// with `engine_record::append`, sweep, append the revisits, re-read as a
    /// restarted process would, and sweep again — each pair measures exactly
    /// once, because the record line itself is the memory.
    #[tokio::test]
    async fn each_change_horizon_pair_is_revisited_exactly_once_across_restarts() {
        let ship_at = 1_000 * DAY;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        let mut lines = record_with_loop(7);
        burst_events(&mut lines, ship_at - 7 * DAY, 84, 2 * HOUR);
        accept(&mut lines);
        let change = build_change(&lines, "hyp_1", "abc123", "chg_1", ship_at, None).unwrap();
        lines.push((
            engine_core::change::RECORD_KIND.to_string(),
            serde_json::to_value(&change).unwrap(),
        ));
        let outcomes = measure_due_changes(&lines, ship_at + 7 * DAY, None).await.0;
        lines.push((
            engine_core::change::OUTCOME_KIND.to_string(),
            serde_json::to_value(&outcomes[0]).unwrap(),
        ));
        for (kind, value) in &lines {
            engine_record::append(&path, kind, value, ship_at).unwrap();
        }

        // First run: 90 days on, both surviving horizons are due at once.
        let read = engine_record::read_all(&path).unwrap();
        let (due, _) = revisit_due_changes(&read.lines, ship_at + 90 * DAY, None).await;
        assert_eq!(
            due.iter().map(|r| r.horizon_days).collect::<Vec<_>>(),
            vec![30, 90],
            "both matured horizons, the redundant 7d never"
        );
        for r in &due {
            engine_record::append(
                &path,
                engine_core::change::REVISIT_KIND,
                r,
                ship_at + 90 * DAY,
            )
            .unwrap();
        }

        // The restart: a fresh read of the same file finds nothing due.
        let reread = engine_record::read_all(&path).unwrap();
        assert_eq!(
            engine_core::change::Revisit::for_change(reread.lines.iter(), &change.id).len(),
            2,
            "the revisit lines survive the record wrapper"
        );
        let (again, skips) = revisit_due_changes(&reread.lines, ship_at + 91 * DAY, None).await;
        assert!(
            again.is_empty(),
            "each (change, horizon) measures exactly once"
        );
        assert!(skips.is_empty());
    }

    #[tokio::test]
    async fn unread_guardrails_are_named_on_the_outcome() {
        let ship_at = 1_000 * DAY;
        let mut lines = Vec::new();
        let o = engine_core::observation::Observation::fact(
            "obs_1",
            engine_core::observation::Source::Runtime,
            engine_core::observation::Kind::MetricReading,
            engine_core::observation::Window::new(0, 1).unwrap(),
            1,
        );
        lines.push((
            engine_core::observation::RECORD_KIND.to_string(),
            serde_json::to_value(&o).unwrap(),
        ));
        let h = Hypothesis::new("hyp_1", "x", vec![ObservationId("obs_1".into())], 2)
            .unwrap()
            .with_plan(
                EvaluationTarget::new("e", "Errors", "a", "b", Metric::ErrorEventRate)
                    .with_window(MeasurementWindow {
                        days: 1,
                        min_entries: 1,
                        min_effect: 0.01,
                    })
                    .with_guardrail(Metric::SupportContacts, 0.0),
            );
        for l in h.record_lines() {
            lines.push(l);
        }
        accept(&mut lines);
        burst_events(&mut lines, ship_at - DAY, 24, HOUR);
        let change = build_change(&lines, "hyp_1", "abc", "chg_1", ship_at, None).unwrap();
        lines.push((
            engine_core::change::RECORD_KIND.to_string(),
            serde_json::to_value(&change).unwrap(),
        ));
        let outcomes = measure_due_changes(&lines, ship_at + DAY, None).await.0;
        assert_eq!(
            outcomes[0].unread_guardrails,
            vec!["support contacts"],
            "a guardrail the record cannot read is named, never silently held"
        );
    }
}

#[cfg(test)]
mod behavior_measure_tests {
    use super::*;
    use engine_core::evaluation::{Direction, EvaluationTarget, MeasurementWindow, Metric, Sample};
    use engine_core::hypothesis::{Hypothesis, Status};
    use engine_core::observation::ObservationId;

    const DAY: u64 = 86_400_000;

    /// A behavior source that answers from a script, so the async measuring
    /// path is tested without a network or an account.
    struct Scripted {
        sample: Result<Sample, &'static str>,
        /// The window the source was asked for — asserted on, because a
        /// measurement over a slid window is the bug this pipeline refuses.
        asked: std::sync::Mutex<Option<(u64, u64)>>,
    }

    #[async_trait::async_trait]
    impl engine_behavior::BehaviorSource for Scripted {
        async fn seen_events(
            &self,
            _days: u32,
            _limit: u32,
        ) -> Result<Vec<engine_behavior::SeenEvent>, engine_behavior::BehaviorError> {
            Ok(Vec::new())
        }
        async fn sample(
            &self,
            _t: &EvaluationTarget,
            _days: u32,
        ) -> Result<Sample, engine_behavior::BehaviorError> {
            unreachable!("measurement must use the frozen window, never a relative one")
        }
        async fn sample_between(
            &self,
            _t: &EvaluationTarget,
            from_ms: u64,
            to_ms: u64,
        ) -> Result<Sample, engine_behavior::BehaviorError> {
            *self.asked.lock().unwrap() = Some((from_ms, to_ms));
            self.sample
                .map_err(|e| engine_behavior::BehaviorError::Unreachable(e.to_string()))
        }
        async fn entries(
            &self,
            _t: &EvaluationTarget,
            _days: u32,
            _limit: u32,
        ) -> Result<Vec<engine_behavior::EvaluationEntry>, engine_behavior::BehaviorError> {
            Ok(Vec::new())
        }
    }

    fn lines_with_behavior_change(ship_at: u64) -> Vec<(String, serde_json::Value)> {
        let mut lines = Vec::new();
        let o = engine_core::observation::Observation::fact(
            "obs_1",
            engine_core::observation::Source::PostHog,
            engine_core::observation::Kind::MetricReading,
            engine_core::observation::Window::new(0, 1).unwrap(),
            1,
        );
        lines.push((
            engine_core::observation::RECORD_KIND.to_string(),
            serde_json::to_value(&o).unwrap(),
        ));
        let h = Hypothesis::new(
            "hyp_1",
            "role selection confuses invited admins",
            vec![ObservationId("obs_1".into())],
            2,
        )
        .unwrap()
        .with_plan(
            EvaluationTarget::new(
                "e",
                "Invites",
                "invite_started",
                "invite_accepted",
                Metric::CompletionRate,
            )
            .with_window(MeasurementWindow {
                days: 7,
                min_entries: 100,
                min_effect: 0.02,
            }),
        );
        for l in h.record_lines() {
            lines.push(l);
        }
        let d = decide(&lines, "hyp_1", Status::Accepted).unwrap();
        lines.push(d);
        let change = build_change(
            &lines,
            "hyp_1",
            "abc123",
            "chg_1",
            ship_at,
            Some(Sample {
                value: 0.61,
                entries: 340,
            }),
        )
        .unwrap();
        lines.push((
            engine_core::change::RECORD_KIND.to_string(),
            serde_json::to_value(&change).unwrap(),
        ));
        lines
    }

    #[tokio::test]
    async fn a_behavior_change_is_measured_over_its_frozen_window() {
        let ship_at = 1_000 * DAY;
        let lines = lines_with_behavior_change(ship_at);
        let source = Scripted {
            sample: Ok(Sample {
                value: 0.78,
                entries: 350,
            }),
            asked: std::sync::Mutex::new(None),
        };
        let (out, skips) = measure_due_changes(&lines, ship_at + 9 * DAY, Some(&source)).await;
        assert!(skips.is_empty());
        assert_eq!(out.len(), 1);
        match &out[0].outcome {
            engine_core::evaluation::Outcome::Measured { direction, .. } => {
                assert_eq!(*direction, Direction::Positive)
            }
            other => panic!("expected measured: {other:?}"),
        }
        // The window the source was asked for is the DECLARED one — anchored
        // at ship time, not at measurement time two days late.
        let asked = source.asked.lock().unwrap().unwrap();
        assert_eq!(
            asked,
            (ship_at, ship_at + 7 * DAY),
            "the window does not slide"
        );
    }

    #[tokio::test]
    async fn no_source_means_a_named_skip_never_a_fake_outcome() {
        let ship_at = 1_000 * DAY;
        let lines = lines_with_behavior_change(ship_at);
        let (out, skips) = measure_due_changes(&lines, ship_at + 8 * DAY, None).await;
        assert!(out.is_empty(), "an unreadable change is not an unmeasured outcome — traffic may be fine, only our read is missing");
        assert_eq!(skips.len(), 1);
        assert!(skips[0].1.contains("PostHog"), "{}", skips[0].1);
    }

    #[tokio::test]
    async fn a_failed_read_skips_and_will_retry_rather_than_recording() {
        let ship_at = 1_000 * DAY;
        let lines = lines_with_behavior_change(ship_at);
        let source = Scripted {
            sample: Err("dns"),
            asked: std::sync::Mutex::new(None),
        };
        let (out, skips) = measure_due_changes(&lines, ship_at + 8 * DAY, Some(&source)).await;
        assert!(out.is_empty());
        assert_eq!(skips.len(), 1);
        assert!(skips[0].1.contains("dns"), "{}", skips[0].1);
    }

    /// The behavior change with its outcome already recorded, ready to revisit.
    async fn lines_with_measured_behavior_change(ship_at: u64) -> Vec<(String, serde_json::Value)> {
        let mut lines = lines_with_behavior_change(ship_at);
        let source = Scripted {
            sample: Ok(Sample {
                value: 0.78,
                entries: 350,
            }),
            asked: std::sync::Mutex::new(None),
        };
        let outcomes = measure_due_changes(&lines, ship_at + 7 * DAY, Some(&source))
            .await
            .0;
        lines.push((
            engine_core::change::OUTCOME_KIND.to_string(),
            serde_json::to_value(&outcomes[0]).unwrap(),
        ));
        lines
    }

    /// The doctrine's window arithmetic, held exactly: the 30-day revisit of a
    /// 7d-window change asks the source for (ship+23d, ship+30d) — the plan's
    /// own width, ending at ship + horizon — and compares what comes back
    /// against the baseline frozen at ship.
    #[tokio::test]
    async fn a_behavior_revisit_asks_for_the_later_window_against_the_frozen_baseline() {
        let ship_at = 1_000 * DAY;
        let lines = lines_with_measured_behavior_change(ship_at).await;
        let source = Scripted {
            sample: Ok(Sample {
                value: 0.62,
                entries: 400,
            }),
            asked: std::sync::Mutex::new(None),
        };
        let (due, skips) = revisit_due_changes(&lines, ship_at + 30 * DAY, Some(&source)).await;
        assert!(skips.is_empty());
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].horizon_days, 30);
        let asked = source.asked.lock().unwrap().unwrap();
        assert_eq!(
            asked,
            (ship_at + 23 * DAY, ship_at + 30 * DAY),
            "the plan's width, ending at ship + horizon — never a clock-relative slice"
        );
        match &due[0].outcome {
            engine_core::evaluation::Outcome::Measured {
                direction,
                from,
                to,
                ..
            } => {
                assert_eq!(*from, 0.61, "the SAME baseline the change froze at ship");
                assert_eq!(*to, 0.62);
                assert_eq!(
                    *direction,
                    Direction::Neutral,
                    "0.01 against a declared 0.02 minimum effect"
                );
            }
            other => panic!("expected measured, got {other:?}"),
        }
        // Neutral now, positive at close: a direction change, so drift.
        let original = OutcomeRecorded::for_change(lines.iter(), &due[0].change).unwrap();
        assert!(due[0].drifted_from(&original));
    }

    /// An unreadable source is a named skip retried later — never an invented
    /// revisit. Both shapes: no source configured, and a source whose read
    /// failed.
    #[tokio::test]
    async fn an_unreadable_source_is_a_named_revisit_skip_never_an_invented_reading() {
        let ship_at = 1_000 * DAY;
        let lines = lines_with_measured_behavior_change(ship_at).await;

        let (out, skips) = revisit_due_changes(&lines, ship_at + 30 * DAY, None).await;
        assert!(out.is_empty(), "a missing source must not become a reading");
        assert_eq!(skips.len(), 1);
        assert_eq!(
            skips[0].0, "chg_1@30d",
            "the skip names the change AND the horizon"
        );
        assert!(skips[0].1.contains("PostHog"), "{}", skips[0].1);

        let source = Scripted {
            sample: Err("dns"),
            asked: std::sync::Mutex::new(None),
        };
        let (out, skips) = revisit_due_changes(&lines, ship_at + 30 * DAY, Some(&source)).await;
        assert!(out.is_empty());
        assert_eq!(skips.len(), 1);
        assert!(skips[0].1.contains("dns"), "{}", skips[0].1);

        // The skip retried on a later tick with the source back: measured,
        // over the same frozen window it would have read the first time.
        let healed = Scripted {
            sample: Ok(Sample {
                value: 0.80,
                entries: 500,
            }),
            asked: std::sync::Mutex::new(None),
        };
        let (out, skips) = revisit_due_changes(&lines, ship_at + 31 * DAY, Some(&healed)).await;
        assert!(skips.is_empty());
        assert_eq!(out.len(), 1);
        assert_eq!(
            healed.asked.lock().unwrap().unwrap(),
            (ship_at + 23 * DAY, ship_at + 30 * DAY),
            "however late the retry runs, the window does not slide"
        );
    }
}
