//! `drums hypothesize` — the proposing surface.
//!
//! # Why this is a CLI command and not a dashboard button
//!
//! Proposing a hypothesis is a judgment call, and the sorting rule
//! (`docs/INSTALL_FLOW.md`) puts judgment with the human, deliberately visible:
//! plumbing goes to the agent, health goes to Drums, consent and product
//! judgment stay human. The local dashboard is read-only by contract — it
//! renders the record and never writes it — so the write happens here, in the
//! terminal, under the person's own hands. The dashboard's part is Elu's move:
//! it hands over a ready-made command to edit and paste, not a button that
//! commits an opinion nobody typed.
//!
//! # What the command enforces
//!
//! Everything the [`engine_core::hypothesis`] module promises, checked before
//! anything is written:
//!
//! - **Citations must exist.** Every cited observation id is resolved against
//!   the record first; an id nothing matches is refused by name. A hypothesis
//!   cites evidence, and evidence is something the record actually holds.
//! - **The plan comes whole or not at all.** `--name/--start/--success/--metric`
//!   define an evaluation plan together; passing some of them is refused rather
//!   than guessed at. Without a plan the hypothesis is still recorded — and the
//!   output says plainly that no change can proceed until one is attached.
//! - **The record is append-only.** Recording a hypothesis appends its line
//!   plus one `observation_status` line per citation, exactly the shape
//!   [`Hypothesis::record_lines`] defines. Nothing is mutated.

use std::path::Path;

use engine_core::evaluation::{EvaluationTarget, Guardrail, MeasurementWindow, Metric};
use engine_core::hypothesis::Hypothesis;
use engine_core::observation::{Observation, ObservationId};

/// Everything `drums hypothesize` was asked to do, before validation.
#[derive(Debug, Clone, Default)]
pub struct HypothesizeArgs {
    pub statement: String,
    /// The observation ids to cite — at least one.
    pub cites: Vec<String>,
    // The evaluation plan, all-or-nothing:
    pub name: Option<String>,
    pub start: Option<String>,
    pub success: Option<String>,
    pub metric: Option<String>,
    /// `metric` or `metric:tolerance` — tolerance in the metric's own unit.
    pub guardrails: Vec<String>,
    pub window_days: Option<u32>,
    pub min_entries: Option<u32>,
    pub min_effect: Option<f64>,
}

/// A refusal, with the sentence the person reads. Every arm names what was
/// wrong and what would fix it — the doctor rule, applied to arguments.
#[derive(Debug, PartialEq)]
pub struct Refusal(pub String);

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Parse a metric the way it appears everywhere else — the serde snake_case
/// name. The closed set is the point: a metric the engine cannot compute is a
/// metric an outcome could never be measured against.
pub fn parse_metric(s: &str) -> Result<Metric, Refusal> {
    match s {
        "completion_rate" => Ok(Metric::CompletionRate),
        "time_to_complete" => Ok(Metric::TimeToComplete),
        "error_rate" => Ok(Metric::ErrorRate),
        "step_retries" => Ok(Metric::StepRetries),
        "abandonment" => Ok(Metric::Abandonment),
        "support_contacts" => Ok(Metric::SupportContacts),
        "error_event_rate" => Ok(Metric::ErrorEventRate),
        other => Err(Refusal(format!(
            "unknown metric {other:?} — one of: completion_rate, time_to_complete, error_rate, step_retries, abandonment, support_contacts, error_event_rate"
        ))),
    }
}

/// `error_rate` or `error_rate:0.02`.
pub fn parse_guardrail(s: &str) -> Result<Guardrail, Refusal> {
    let (metric, tolerance) = match s.split_once(':') {
        Some((m, t)) => {
            let tol: f64 = t.parse().map_err(|_| {
                Refusal(format!(
                    "guardrail tolerance {t:?} is not a number (in {s:?})"
                ))
            })?;
            if tol < 0.0 {
                return Err(Refusal(format!(
                    "guardrail tolerance must be zero or positive, got {t} (in {s:?})"
                )));
            }
            (m, tol)
        }
        None => (s, 0.0),
    };
    Ok(Guardrail {
        metric: parse_metric(metric)?,
        tolerance,
    })
}

/// Build the evaluation plan from the flags — whole or not at all.
fn build_plan(args: &HypothesizeArgs, plan_id: &str) -> Result<Option<EvaluationTarget>, Refusal> {
    let core = [&args.name, &args.start, &args.success, &args.metric];
    let given = core.iter().filter(|f| f.is_some()).count();
    if given == 0 {
        // Window/guardrail flags without a plan are a half-plan too.
        if !args.guardrails.is_empty()
            || args.window_days.is_some()
            || args.min_entries.is_some()
            || args.min_effect.is_some()
        {
            return Err(Refusal(
                "guardrail/window flags describe an evaluation plan, but the plan itself is missing — a plan needs --name, --start, --success, and --metric together".into(),
            ));
        }
        return Ok(None);
    }
    if given < 4 {
        return Err(Refusal(
            "an evaluation plan comes whole or not at all: --name, --start, --success, and --metric together (guardrails and window optional)".into(),
        ));
    }

    let mut window = MeasurementWindow::default();
    if let Some(d) = args.window_days {
        window.days = d;
    }
    if let Some(n) = args.min_entries {
        window.min_entries = n;
    }
    if let Some(e) = args.min_effect {
        if e <= 0.0 {
            return Err(Refusal(
                "--min-effect must be positive — a zero minimum effect reports every wobble as a direction".into(),
            ));
        }
        window.min_effect = e;
    }

    let mut target = EvaluationTarget::new(
        plan_id,
        args.name.clone().unwrap(),
        args.start.clone().unwrap(),
        args.success.clone().unwrap(),
        parse_metric(args.metric.as_deref().unwrap())?,
    )
    .with_window(window);
    for g in &args.guardrails {
        let g = parse_guardrail(g)?;
        target = target.with_guardrail(g.metric, g.tolerance);
    }
    Ok(Some(target))
}

/// Validate against the record and build the hypothesis. Pure — the caller
/// supplies the decoded record lines, the ids, and the clock, so every refusal
/// is testable without a filesystem.
pub fn build(
    args: &HypothesizeArgs,
    lines: &[(String, serde_json::Value)],
    hyp_id: &str,
    plan_id: &str,
    now_ms: u64,
) -> Result<Hypothesis, Refusal> {
    let statement = args.statement.trim();
    if statement.is_empty() {
        return Err(Refusal(
            "a hypothesis needs a statement — what could be better, and why".into(),
        ));
    }
    if args.cites.is_empty() {
        return Err(Refusal(
            "a hypothesis cites at least one observation — see `drums record` for what has been observed".into(),
        ));
    }

    // Citations must exist. A typo'd id refused here is a one-second fix; a
    // typo'd id recorded forever is a citation to nothing.
    let known: std::collections::HashSet<String> = Observation::all(lines.iter())
        .into_iter()
        .map(|o| o.id.0)
        .collect();
    let mut cites = Vec::with_capacity(args.cites.len());
    for id in &args.cites {
        if !known.contains(id) {
            // `Observation::all` silently drops lines that fail to decode,
            // so an id can be IN the record and still not be citable.
            // "not in this repo's record" would be a falsehood about a
            // record that holds the line — say what is actually wrong.
            if let Some(why) = undecodable_observation(lines, id) {
                return Err(Refusal(format!(
                    "observation {id} exists in the record but cannot be read ({why}) — the line may predate this drums version"
                )));
            }
            return Err(Refusal(format!(
                "observation {id:?} is not in this repo's record — `drums record` lists what is"
            )));
        }
        cites.push(ObservationId(id.clone()));
    }
    // The same observation cited twice is almost certainly a slip.
    let mut seen = std::collections::HashSet::new();
    for c in &cites {
        if !seen.insert(&c.0) {
            return Err(Refusal(format!("observation {:?} is cited twice", c.0)));
        }
    }

    let plan = build_plan(args, plan_id)?;
    let hypothesis =
        Hypothesis::new(hyp_id, statement, cites, now_ms).expect("cites checked non-empty above");
    Ok(match plan {
        Some(p) => hypothesis.with_plan(p),
        None => hypothesis,
    })
}

/// If an observation line with this raw `id` exists but does not decode,
/// return the decode error. `None` when no such line exists at all — the id
/// is genuinely absent.
fn undecodable_observation(lines: &[(String, serde_json::Value)], id: &str) -> Option<String> {
    let (_, v) = lines.iter().find(|(kind, v)| {
        kind == engine_core::observation::RECORD_KIND
            && v.get("id").and_then(|i| i.as_str()) == Some(id)
    })?;
    serde_json::from_value::<Observation>(v.clone())
        .err()
        .map(|e| e.to_string())
}

/// Append the hypothesis to the record and narrate what happened. The one
/// impure function; returns the lines it wrote for the caller to print.
pub fn record_and_narrate(
    record_path: &Path,
    hypothesis: &Hypothesis,
    now_ms: u64,
) -> std::io::Result<String> {
    for (kind, value) in hypothesis.record_lines() {
        engine_record::append(record_path, &kind, &value, now_ms)?;
    }

    let mut out = String::new();
    out.push_str(&format!(
        "hypothesis {} · inferred\n  \"{}\"\n",
        hypothesis.id, hypothesis.statement
    ));
    for c in &hypothesis.cites {
        out.push_str(&format!("  cites {c} — now marked hypothesized\n"));
    }
    match &hypothesis.plan {
        Some(p) => {
            let guardrails = if p.guardrails.is_empty() {
                "none".to_string()
            } else {
                p.guardrails
                    .iter()
                    .map(|g| format!("{} (tolerance {})", g.metric.label(), g.tolerance))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            out.push_str(&format!(
                "  plan: {} · {} → {} · {} · window {}d, ≥{} entries, min effect {}\n  guardrails: {}\n",
                p.name,
                p.start_event,
                p.success_event,
                p.metric.label(),
                p.window.days,
                p.window.min_entries,
                p.window.min_effect,
                guardrails
            ));
            out.push_str("  a change may proceed from this hypothesis once accepted\n");
        }
        None => {
            out.push_str(
                "  no evaluation plan yet — a change cannot proceed until one is attached\n  (re-run with --name --start --success --metric to attach one)\n",
            );
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_core::observation::{Kind, Source, Window};

    fn record_with(ids: &[&str]) -> Vec<(String, serde_json::Value)> {
        ids.iter()
            .map(|id| {
                let o = Observation::fact(
                    *id,
                    Source::Runtime,
                    Kind::RateShift {
                        previous: 0.1,
                        since_deploy: Some("abc".into()),
                    },
                    Window::new(0, 1).unwrap(),
                    1,
                );
                (
                    engine_core::observation::RECORD_KIND.to_string(),
                    serde_json::to_value(&o).unwrap(),
                )
            })
            .collect()
    }

    fn args(statement: &str, cites: &[&str]) -> HypothesizeArgs {
        HypothesizeArgs {
            statement: statement.into(),
            cites: cites.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn a_citation_the_record_does_not_hold_is_refused_by_name() {
        let lines = record_with(&["obs_real"]);
        let err = build(&args("x", &["obs_typo"]), &lines, "h", "e", 1).unwrap_err();
        assert!(err.0.contains("obs_typo"), "{err}");
        assert!(err.0.contains("drums record"), "points at the list: {err}");
    }

    /// R3: an observation line that FAILS to decode is silently dropped by
    /// `Observation::all`, so a `--cite` of its id used to refuse with "not
    /// in this repo's record" — a falsehood about a record that holds the
    /// line. The refusal must say the line exists and cannot be read.
    #[test]
    fn a_citation_of_an_undecodable_line_says_unreadable_not_absent() {
        let mut lines = record_with(&["obs_ok"]);
        // A source name this drums version does not know — the shape an
        // older or newer writer would leave behind.
        lines.push((
            engine_core::observation::RECORD_KIND.to_string(),
            serde_json::json!({
                "id": "obs_old",
                "source": "sentry",
                "kind": "metric_reading",
                "window": { "from_ms": 0, "to_ms": 1 },
                "observed_at_ms": 1
            }),
        ));
        let err = build(&args("x", &["obs_old"]), &lines, "h", "e", 1).unwrap_err();
        assert!(
            err.0.contains("exists in the record but cannot be read"),
            "{err}"
        );
        assert!(err.0.contains("obs_old"), "{err}");
        assert!(err.0.contains("predate this drums version"), "{err}");
        assert!(
            !err.0.contains("not in this repo's record"),
            "the falsehood this fix removes: {err}"
        );

        // A genuinely absent id still refuses as absent.
        let err = build(&args("x", &["obs_missing"]), &lines, "h", "e", 1).unwrap_err();
        assert!(err.0.contains("not in this repo's record"), "{err}");
    }

    #[test]
    fn an_empty_statement_or_no_citations_is_refused() {
        let lines = record_with(&["obs_1"]);
        assert!(build(&args("   ", &["obs_1"]), &lines, "h", "e", 1).is_err());
        assert!(build(&args("real statement", &[]), &lines, "h", "e", 1).is_err());
    }

    #[test]
    fn the_same_observation_cited_twice_is_a_slip_not_a_stronger_case() {
        let lines = record_with(&["obs_1"]);
        let err = build(&args("x", &["obs_1", "obs_1"]), &lines, "h", "e", 1).unwrap_err();
        assert!(err.0.contains("cited twice"), "{err}");
    }

    #[test]
    fn a_partial_plan_is_refused_rather_than_guessed_at() {
        let lines = record_with(&["obs_1"]);
        let mut a = args("x", &["obs_1"]);
        a.name = Some("Checkout".into());
        a.metric = Some("completion_rate".into());
        let err = build(&a, &lines, "h", "e", 1).unwrap_err();
        assert!(err.0.contains("whole or not at all"), "{err}");
        // Window flags alone are a half-plan too.
        let mut b = args("x", &["obs_1"]);
        b.window_days = Some(14);
        assert!(build(&b, &lines, "h", "e", 1).is_err());
    }

    #[test]
    fn no_plan_is_recorded_but_not_ready_for_change() {
        let lines = record_with(&["obs_1"]);
        let h = build(
            &args("the promo branch assumes a fresh cart", &["obs_1"]),
            &lines,
            "hyp_1",
            "eval_1",
            7,
        )
        .unwrap();
        assert!(!h.ready_for_change());
        assert_eq!(h.cites.len(), 1);
    }

    #[test]
    fn a_whole_plan_makes_the_hypothesis_ready_for_change() {
        let lines = record_with(&["obs_1", "obs_2"]);
        let mut a = args("checkout drops returning users", &["obs_1", "obs_2"]);
        a.name = Some("Checkout".into());
        a.start = Some("checkout_started".into());
        a.success = Some("payment_confirmed".into());
        a.metric = Some("completion_rate".into());
        a.guardrails = vec!["error_event_rate:0.5".into(), "support_contacts".into()];
        a.window_days = Some(14);
        a.min_entries = Some(200);
        a.min_effect = Some(0.03);
        let h = build(&a, &lines, "hyp_1", "eval_1", 7).unwrap();
        assert!(h.ready_for_change());
        let p = h.plan.as_ref().unwrap();
        assert_eq!(p.metric, Metric::CompletionRate);
        assert_eq!(p.guardrails.len(), 2);
        assert_eq!(p.guardrails[0].tolerance, 0.5);
        assert_eq!(p.guardrails[1].tolerance, 0.0);
        assert_eq!(p.window.days, 14);
        assert_eq!(p.window.min_entries, 200);
    }

    #[test]
    fn metric_names_are_the_serde_names_and_nothing_else() {
        assert!(parse_metric("completion_rate").is_ok());
        assert!(parse_metric("error_event_rate").is_ok());
        for bad in ["CompletionRate", "completion", "conversions", ""] {
            let err = parse_metric(bad).unwrap_err();
            assert!(
                err.0.contains("one of:"),
                "the refusal lists the vocabulary: {err}"
            );
        }
    }

    #[test]
    fn a_zero_min_effect_is_refused_with_the_reason() {
        let lines = record_with(&["obs_1"]);
        let mut a = args("x", &["obs_1"]);
        a.name = Some("C".into());
        a.start = Some("s".into());
        a.success = Some("e".into());
        a.metric = Some("completion_rate".into());
        a.min_effect = Some(0.0);
        let err = build(&a, &lines, "h", "e", 1).unwrap_err();
        assert!(err.0.contains("wobble"), "{err}");
    }

    #[test]
    fn recording_appends_the_full_protocol_and_the_status_folds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        // Seed the observation the hypothesis will cite, through the real
        // wrapper.
        let o = Observation::fact(
            "obs_9",
            Source::Runtime,
            Kind::RateShift {
                previous: 0.0,
                since_deploy: Some("abc".into()),
            },
            Window::new(0, 1).unwrap(),
            1,
        );
        engine_record::append(&path, engine_core::observation::RECORD_KIND, &o, 1).unwrap();

        let read = engine_record::read_all(&path).unwrap();
        let h = build(
            &args("the promo branch assumes a fresh cart", &["obs_9"]),
            &read.lines,
            "hyp_1",
            "eval_1",
            2,
        )
        .unwrap();
        let narration = record_and_narrate(&path, &h, 2).unwrap();
        assert!(narration.contains("hypothesis hyp_1 · inferred"));
        assert!(narration.contains("cannot proceed until one is attached"));

        let read = engine_record::read_all(&path).unwrap();
        assert_eq!(Hypothesis::all(read.lines.iter()).len(), 1);
        // The cited observation's fold now says hypothesized — through the
        // real wrapper, the collision class this record protocol was bitten
        // by once already.
        assert_eq!(
            Observation::current_status(read.lines.iter(), &o.id),
            Some(engine_core::observation::Status::Hypothesized {
                hypothesis: "hyp_1".into()
            })
        );
    }
}
