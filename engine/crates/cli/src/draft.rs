//! `drums draft` — Drums drafts a bet; a human decides.
//!
//! # Where this sits on the judgment ladder
//!
//! The sorting rule gives plumbing to the agent, health to Drums, and
//! consent to the human. Drafting is the agent doing *preparation of
//! judgment*: it reads the record and the memory, writes the strongest
//! candidate bet as a proposal, and stops. A proposal commits nobody
//! (`BetStatus::Proposed` is a draft by definition), confirmation stays a
//! human act, and a draft the agent declines to make ("skip") is treated as
//! a good outcome, not a failure.
//!
//! # Agent neutrality
//!
//! The drafting runs on the customer's own coding-agent CLI — `agent_cmd`
//! from config, `DRUMS_AGENT_CMD`, or `claude`/`codex` found on PATH — in
//! plain prompt mode with no file-editing permissions. Drums brings the
//! prompt (versioned in this repo at `prompts/draft_bet.md`) and the record
//! excerpts; the model and its credentials are the customer's, always.
//!
//! # The output is validated, not trusted
//!
//! Whatever the agent returns goes through the same [`bet_cmd::build`] gate
//! a human's `drums bet create` goes through: citations must exist in the
//! record, the plan must be whole, metrics must parse. A draft that cites a
//! hallucinated observation dies at the same wall a typo would.

use std::path::Path;
use std::process::Stdio;

use engine_core::bet::{BetStatus, ProductBet};
use engine_core::hypothesis::Hypothesis;
use engine_core::observation::Observation;
use serde::Deserialize;

use crate::bet_cmd::{self, BetCreateArgs};
use crate::hypothesize::{HypothesizeArgs, Refusal};

/// The versioned system prompt. `include_str!` so the binary always carries
/// exactly the prompt the release was tested with — a prompt loaded from
/// disk at runtime is a prompt nobody reviewed.
const SYSTEM_PROMPT: &str = include_str!("../prompts/draft_bet.md");

const TIMEOUT_MS: u64 = 180_000;

/// What the agent must return. Mirrors the output contract in the prompt.
#[derive(Debug, Deserialize)]
pub struct Drafted {
    #[serde(default)]
    pub skip: bool,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub belief: Option<String>,
    #[serde(default)]
    pub because: Option<String>,
    #[serde(default)]
    pub audience: Option<String>,
    #[serde(default)]
    pub alternatives: Vec<String>,
    #[serde(default)]
    pub cite: Vec<String>,
    #[serde(default)]
    pub plan: Option<DraftedPlan>,
}

#[derive(Debug, Deserialize)]
pub struct DraftedPlan {
    pub name: String,
    pub start: String,
    pub success: String,
    pub metric: String,
    #[serde(default)]
    pub guardrails: Vec<String>,
    #[serde(default)]
    pub window_days: Option<u32>,
    #[serde(default)]
    pub min_entries: Option<u32>,
    #[serde(default)]
    pub min_effect: Option<f64>,
}

/// Assemble the context block appended to the system prompt: open
/// observations (the material), and the memory (evaluated bets with their
/// verdicts and learnings, grouped by metric). Pure, so tests can pin what
/// the agent actually sees.
pub fn context(lines: &[(String, serde_json::Value)]) -> String {
    let mut out = String::new();

    out.push_str("\n## Observations (cite by id — only these ids exist)\n\n");
    let mut any = false;
    for o in Observation::all(lines.iter()) {
        // Only observations no hypothesis has claimed: drafting against
        // already-interpreted evidence produces duplicate bets.
        let taken = matches!(
            Observation::current_status(lines.iter(), &o.id),
            Some(engine_core::observation::Status::Hypothesized { .. })
        );
        if taken {
            continue;
        }
        any = true;
        let fact = serde_json::to_value(&o.kind)
            .map(|v| v.to_string())
            .unwrap_or_default();
        let measure = o
            .measure
            .map(|m| {
                format!(
                    " · {} {} over {}",
                    m.metric.label(),
                    m.sample.value,
                    m.sample.entries
                )
            })
            .unwrap_or_default();
        out.push_str(&format!("- {} — {}{}\n", o.id.0, fact, measure));
    }
    if !any {
        out.push_str("(none unclaimed)\n");
    }

    out.push_str("\n## Memory (evaluated bets — build on these, cite by bet id in prose)\n\n");
    let mut any = false;
    for b in ProductBet::all(lines.iter()) {
        let Some(BetStatus::Evaluated { verdict }) =
            ProductBet::current_status(lines.iter(), &b.id)
        else {
            continue;
        };
        any = true;
        let word = bet_cmd::support_word(verdict.support);
        let metric = Hypothesis::all(lines.iter())
            .into_iter()
            .find(|h| h.id == b.hypothesis)
            .and_then(|h| h.plan)
            .map(|p| p.metric.label())
            .unwrap_or("unknown metric");
        out.push_str(&format!(
            "- {} on {} was {} — \"{}\"\n",
            b.id.0, metric, word, b.belief
        ));
        for note in ProductBet::learnings(lines.iter(), &b.id) {
            out.push_str(&format!("    learned: {note}\n"));
        }
    }
    if !any {
        out.push_str("(no evaluated bets yet — this would be the product's first)\n");
    }

    out.push_str("\n## Open bets (do not duplicate these)\n\n");
    let mut any = false;
    for b in ProductBet::all(lines.iter()) {
        if matches!(
            ProductBet::current_status(lines.iter(), &b.id),
            Some(BetStatus::Proposed) | Some(BetStatus::Confirmed)
        ) {
            any = true;
            out.push_str(&format!("- {} — \"{}\"\n", b.id.0, b.belief));
        }
    }
    if !any {
        out.push_str("(none)\n");
    }
    out
}

/// Parse the agent's reply. Tolerates the one deviation every CLI agent
/// makes — prose or fences around the JSON — by extracting the outermost
/// object; everything inside it is held to the contract strictly.
pub fn parse(output: &str) -> Result<Drafted, Refusal> {
    let start = output.find('{');
    let end = output.rfind('}');
    let json = match (start, end) {
        (Some(s), Some(e)) if e > s => &output[s..=e],
        _ => {
            return Err(Refusal(format!(
                "the agent returned no JSON object — first lines were: {:?}",
                output.lines().take(3).collect::<Vec<_>>().join(" / ")
            )))
        }
    };
    serde_json::from_str(json)
        .map_err(|e| Refusal(format!("the agent's JSON does not match the contract: {e}")))
}

/// Turn a parsed draft into the same arguments a human would have typed.
/// From here on, `bet_cmd::build` owns validation — one gate for both hands.
pub fn to_args(d: Drafted) -> Result<BetCreateArgs, Refusal> {
    if d.skip {
        return Err(Refusal(format!(
            "the agent declined to draft: {}",
            d.reason.as_deref().unwrap_or("(no reason given)")
        )));
    }
    let plan = d
        .plan
        .ok_or_else(|| Refusal("the draft has no plan — the expectation is not optional".into()))?;
    Ok(BetCreateArgs {
        belief: d.belief.unwrap_or_default(),
        because: d.because.unwrap_or_default(),
        audience: d.audience,
        alternatives: d.alternatives,
        cites: d.cite,
        plan: HypothesizeArgs {
            name: Some(plan.name),
            start: Some(plan.start),
            success: Some(plan.success),
            metric: Some(plan.metric),
            guardrails: plan.guardrails,
            window_days: plan.window_days,
            min_entries: plan.min_entries,
            min_effect: plan.min_effect,
            ..Default::default()
        },
    })
}

/// Resolve the agent command template for drafting. Same resolution order as
/// repair, different defaults: drafting needs no edit permissions, so the
/// known CLIs run in their plainest read-only prompt mode.
pub fn agent_template(config_agent_cmd: Option<&str>) -> Option<String> {
    if let Some(cmd) = config_agent_cmd {
        if !cmd.trim().is_empty() {
            return Some(cmd.to_string());
        }
    }
    if let Ok(cmd) = std::env::var("DRUMS_AGENT_CMD") {
        if !cmd.trim().is_empty() {
            return Some(cmd);
        }
    }
    let on_path = |program: &str| {
        std::env::var_os("PATH")
            .map(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
            .unwrap_or(false)
    };
    if on_path("claude") {
        return Some("claude -p {prompt}".to_string());
    }
    if on_path("codex") {
        return Some("codex exec -s read-only {prompt}".to_string());
    }
    // The wider roster, in the same order the repair side detects it. Plain
    // prompt modes: drafting writes prose, not files.
    for (bin, template) in [
        ("gemini", "gemini -p {prompt}"),
        ("cursor-agent", "cursor-agent -p {prompt}"),
        ("amp", "amp -x {prompt}"),
        ("opencode", "opencode run {prompt}"),
    ] {
        if on_path(bin) {
            return Some(template.to_string());
        }
    }
    None
}

/// Run the customer's agent CLI with the full prompt. Argv is built by
/// whitespace-splitting the template and substituting `{prompt}` as a single
/// element — no shell, so nothing in a prompt can become a command.
pub async fn run_agent(template: &str, prompt: &str, repo: &Path) -> Result<String, Refusal> {
    let argv: Vec<String> = template
        .split_whitespace()
        .map(|tok| {
            if tok == "{prompt}" {
                prompt.to_string()
            } else {
                tok.to_string()
            }
        })
        .collect();
    if argv.is_empty() {
        return Err(Refusal("the agent command template is empty".into()));
    }
    let mut cmd = tokio::process::Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .current_dir(repo)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = tokio::time::timeout(std::time::Duration::from_millis(TIMEOUT_MS), async {
        cmd.output().await
    })
    .await
    .map_err(|_| {
        Refusal(format!(
            "the agent did not answer within {}s",
            TIMEOUT_MS / 1000
        ))
    })?
    .map_err(|e| Refusal(format!("could not run {:?}: {e}", argv[0])))?;
    if !output.status.success() {
        return Err(Refusal(format!(
            "the agent exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
                .lines()
                .take(3)
                .collect::<Vec<_>>()
                .join(" / ")
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The full prompt: the versioned system prompt plus the record context.
pub fn full_prompt(lines: &[(String, serde_json::Value)]) -> String {
    format!("{SYSTEM_PROMPT}{}", context(lines))
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_core::observation::{Kind, Source, Window};

    fn lines() -> Vec<(String, serde_json::Value)> {
        let o = Observation::fact(
            "obs_free",
            Source::Runtime,
            Kind::RateShift {
                previous: 0.1,
                since_deploy: Some("abc".into()),
            },
            Window::new(0, 1).unwrap(),
            1,
        );
        let claimed = Observation::fact(
            "obs_claimed",
            Source::Runtime,
            Kind::RateShift {
                previous: 0.2,
                since_deploy: None,
            },
            Window::new(0, 1).unwrap(),
            1,
        );
        let h = Hypothesis::new("hyp_1", "s", vec![claimed.id.clone()], 2).unwrap();
        let mut lines = vec![
            ("observation".to_string(), serde_json::to_value(&o).unwrap()),
            (
                "observation".to_string(),
                serde_json::to_value(&claimed).unwrap(),
            ),
        ];
        for (kind, value) in h.record_lines() {
            lines.push((kind, value));
        }
        lines
    }

    #[test]
    fn context_offers_only_unclaimed_observations() {
        let ctx = context(&lines());
        assert!(ctx.contains("obs_free"));
        assert!(
            !ctx.contains("- obs_claimed"),
            "claimed evidence is not offered again:\n{ctx}"
        );
        assert!(ctx.contains("first"), "an empty memory is said, not blank");
    }

    #[test]
    fn the_prompt_carries_the_contract_and_the_epistemics() {
        let p = full_prompt(&lines());
        for required in ["skip", "min_effect", "never \"proved\"", "cite by id"] {
            assert!(
                p.to_lowercase().contains(&required.to_lowercase()),
                "missing {required:?}"
            );
        }
    }

    #[test]
    fn fenced_or_prose_wrapped_json_still_parses_strictly() {
        let wrapped = "Here is the draft:\n```json\n{\"skip\": true, \"reason\": \"evidence is one quiet observation\"}\n```\nGood luck!";
        let d = parse(wrapped).unwrap();
        assert!(d.skip);
        assert!(parse("no json here at all").is_err());
        // Inside the object the contract is strict: unknown metric later
        // dies in bet_cmd::build, and malformed JSON dies here.
        assert!(
            parse("{\"skip\": \"yes\"}").is_err(),
            "a string is not a bool"
        );
    }

    #[test]
    fn a_skip_becomes_a_refusal_carrying_the_agents_reason() {
        let d = Drafted {
            skip: true,
            reason: Some("the only shift predates the last deploy".into()),
            belief: None,
            because: None,
            audience: None,
            alternatives: vec![],
            cite: vec![],
            plan: None,
        };
        let err = to_args(d).unwrap_err();
        assert!(err.0.contains("declined to draft"), "{err}");
        assert!(
            err.0.contains("predates"),
            "the reason survives verbatim: {err}"
        );
    }

    #[test]
    fn a_hallucinated_citation_dies_at_the_same_gate_a_typo_would() {
        let d = Drafted {
            skip: false,
            reason: None,
            belief: Some("batching will cut the error rate".into()),
            because: Some("the rate tripled after abc".into()),
            audience: None,
            alternatives: vec![],
            cite: vec!["obs_invented".into()],
            plan: Some(DraftedPlan {
                name: "API health".into(),
                start: "deploy".into(),
                success: "healthy".into(),
                metric: "error_event_rate".into(),
                guardrails: vec![],
                window_days: Some(7),
                min_entries: None,
                min_effect: None,
            }),
        };
        let args = to_args(d).unwrap();
        let err = bet_cmd::build(&args, &lines(), "b", "h", "e", 9).unwrap_err();
        assert!(err.0.contains("obs_invented"), "refused by name: {err}");
    }

    #[test]
    fn template_resolution_prefers_config_then_env_then_path() {
        assert_eq!(
            agent_template(Some("myagent {prompt}")),
            Some("myagent {prompt}".into())
        );
        assert_eq!(
            agent_template(Some("   ")).is_some(),
            agent_template(None).is_some(),
            "blank config falls through to the same resolution as none"
        );
    }
}
