//! Proactive Slack notifications — the four-kind vocabulary.
//!
//! Every message Drums sends without being asked is exactly one of four
//! kinds, and the copy discipline is absolute (this vocabulary replaces any
//! "findings" language):
//!
//! - **FYI** — investigated; nothing needs you.
//! - **Learning** — a prior Bet matured and changed Drums' understanding.
//! - **Working** — Drums found something worth investigating and is working
//!   on it.
//! - **Decision** — Drums has completed the work; production approval
//!   required.
//!
//! The target a reader should walk away with is "an autonomous product
//! engineer works here" — never "an analytics tool sends findings". That is
//! why the kind leads every message ("[DECISION] …"), why there are exactly
//! four, and why bodies keep the record's own epistemics: verdicts stay
//! `supported`/`not supported`/`inconclusive` with their causal-confidence
//! line, and the words "worked"/"proved"/"caused" never appear — a full
//! rollout cannot prove causation, and a notification must not round it up.
//!
//! # What never goes in a body
//!
//! Raw secrets. Anything derived from a captured request goes through the
//! same redaction the record itself uses (`engine_record::redact_query_string`
//! for paths; bodies are never included at all) BEFORE it reaches a
//! [`Notification`] — Slack is a broadcast channel, and a webhook message is
//! pasted into threads and screenshots. The construction sites
//! (`engine::notification_for`, `digest::notification`) hold that rule; this
//! module's builders are pure and add nothing of their own.
//!
//! # Delivery discipline
//!
//! The record is the source of truth; Slack is a courtesy copy. So delivery
//! is fire-and-forget ([`Sink::deliver`]): one attempt per event, failures
//! logged via `tracing::warn`, no retries, and an event id never notifies
//! twice within a run. A Slack outage must never block, crash, or slow the
//! loop that noticed the failure in the first place.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The four kinds. There is deliberately no fifth: a message that fits none
/// of these is a message Drums should not be sending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Fyi,
    Learning,
    Working,
    Decision,
}

impl Kind {
    /// The bracketed label that leads every message header.
    pub fn label(self) -> &'static str {
        match self {
            Kind::Fyi => "FYI",
            Kind::Learning => "LEARNING",
            Kind::Working => "WORKING",
            Kind::Decision => "DECISION",
        }
    }

    /// The one-line framing each kind promises — what a reader may assume
    /// from the label alone, before reading a word of the body. Kept here,
    /// beside the labels, so the vocabulary and its meaning cannot drift
    /// apart in call sites.
    pub fn framing(self) -> &'static str {
        match self {
            Kind::Fyi => "investigated; nothing needs you.",
            Kind::Learning => "a prior Bet matured and changed Drums' understanding.",
            Kind::Working => "Drums found something worth investigating and is working on it.",
            Kind::Decision => "Drums has completed the work; production approval required.",
        }
    }
}

/// One proactive message. `repo` is the repository's display name (the
/// directory name, not a path) — it renders in the context line so a team
/// with several products knows which one is talking.
#[derive(Debug, Clone)]
pub struct Notification {
    pub kind: Kind,
    pub title: String,
    pub body: String,
    pub repo: String,
}

/// Slack's Block Kit limits: a `header` block's plain text tops out at 150
/// characters and a `section`'s mrkdwn at 3000 — messages over either are
/// REJECTED (HTTP 400), not truncated by Slack, so truncating here (char-safe,
/// with an honest ellipsis) is what keeps a long belief from silently costing
/// the whole notification.
const HEADER_MAX: usize = 150;
const SECTION_MAX: usize = 3000;

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// The Block Kit payload, as a pure function of the notification — header
/// `[KIND] title`, section body, context `drums · <repo>` — so tests pin the
/// exact JSON shape without a network. The top-level `text` is Slack's
/// notification-tray fallback for clients that don't render blocks.
pub fn payload(n: &Notification) -> serde_json::Value {
    let header = clip(&format!("[{}] {}", n.kind.label(), n.title), HEADER_MAX);
    serde_json::json!({
        "text": header,
        "blocks": [
            {
                "type": "header",
                "text": { "type": "plain_text", "text": header, "emoji": false }
            },
            {
                "type": "section",
                "text": { "type": "mrkdwn", "text": clip(&n.body, SECTION_MAX) }
            },
            {
                "type": "context",
                "elements": [
                    { "type": "mrkdwn", "text": format!("drums · {}", n.repo) }
                ]
            }
        ]
    })
}

/// The three words the vocabulary bans, checked as whole words (a "networked
/// retry" is fine; "the fix worked" is not). Used by tests to hold every
/// notification builder to the discipline — verdict language stays
/// `supported`/`not supported`/`inconclusive` with its causal-confidence
/// line, never a claim of proof or causation.
pub fn contains_banned_word(text: &str) -> Option<&'static str> {
    const BANNED: [&str; 3] = ["worked", "proved", "caused"];
    let lower = text.to_lowercase();
    lower
        .split(|c: char| !c.is_alphanumeric())
        .find_map(|word| BANNED.iter().find(|b| word == **b))
        .copied()
}

/// The environment override. Takes precedence over the config file's
/// `slack_webhook_url` — same env-wins rule as `DRUMS_TELEMETRY` and
/// `DRUMS_AGENT_CMD`.
pub const WEBHOOK_ENV: &str = "DRUMS_SLACK_WEBHOOK_URL";

/// Env first, then config, blank values ignored. The pure half of
/// [`resolve_webhook`], split out so precedence is testable without touching
/// process-global environment state.
pub fn pick_webhook(env: Option<&str>, config: Option<&str>) -> Option<String> {
    fn non_blank(s: Option<&str>) -> Option<String> {
        let t = s?.trim();
        if t.is_empty() {
            None
        } else {
            Some(t.to_string())
        }
    }
    non_blank(env).or_else(|| non_blank(config))
}

/// Where notifications go, if anywhere: [`WEBHOOK_ENV`] wins over the config
/// file's `slack_webhook_url`; `None` means notifications are simply off —
/// a normal state, never an error.
pub fn resolve_webhook(config_value: Option<&str>) -> Option<String> {
    pick_webhook(std::env::var(WEBHOOK_ENV).ok().as_deref(), config_value)
}

/// Typed delivery failures. Two variants because they have two different
/// fixes: unreachable is about THIS machine (network, a mistyped URL that
/// isn't a URL at all); rejected is about the WEBHOOK (revoked, or a payload
/// Slack refused) and needs action in Slack, not here.
#[derive(Debug)]
pub enum NotifyError {
    /// The request never got an answer.
    Unreachable(String),
    /// Slack answered, and the answer was no.
    Rejected(u16),
}

impl std::fmt::Display for NotifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NotifyError::Unreachable(why) => write!(
                f,
                "could not reach the Slack webhook: {why} — check this machine's network access and that the URL is a full incoming-webhook address (https://hooks.slack.com/services/…)"
            ),
            NotifyError::Rejected(code) => write!(
                f,
                "the Slack webhook rejected the notification (HTTP {code}) — the webhook may have been revoked or the URL mistyped; create a new incoming webhook in Slack and update slack_webhook_url in .drums/config.toml (or {WEBHOOK_ENV})"
            ),
        }
    }
}

impl std::error::Error for NotifyError {}

/// POST one notification to a Slack incoming webhook. One attempt, bounded at
/// ten seconds — retrying belongs to nobody: the record already holds the
/// truth this message is a courtesy copy of.
pub async fn send(webhook: &str, n: &Notification) -> Result<(), NotifyError> {
    let resp = reqwest::Client::new()
        .post(webhook)
        .json(&payload(n))
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| NotifyError::Unreachable(e.to_string()))?;
    if resp.status().is_success() {
        Ok(())
    } else {
        Err(NotifyError::Rejected(resp.status().as_u16()))
    }
}

/// The engine's delivery handle: webhook plus a per-run set of event ids
/// already notified (the same shape as the engine's `measure_skips_narrated`
/// — shared across the loop and its spawned repair tasks, because a
/// `RepairReady` is emitted off-loop). Cheap to clone; clones share the set.
#[derive(Clone)]
pub struct Sink {
    webhook: String,
    sent: Arc<Mutex<HashSet<String>>>,
}

impl Sink {
    pub fn new(webhook: String) -> Sink {
        Sink {
            webhook,
            sent: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// True exactly once per event id within this run — the de-duplication
    /// gate [`Sink::deliver`] consults before spending a request.
    pub fn first_time(&self, event_id: &str) -> bool {
        self.sent
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(event_id.to_string())
    }

    /// Fire-and-forget delivery: never blocks the caller, never crashes the
    /// loop, never retries. A failure is a `tracing::warn` and nothing else —
    /// the record is the source of truth; Slack is a courtesy copy. Must be
    /// called from within a tokio runtime (the engine always is).
    pub fn deliver(&self, event_id: &str, n: Notification) {
        if !self.first_time(event_id) {
            return;
        }
        let webhook = self.webhook.clone();
        tokio::spawn(async move {
            if let Err(e) = send(&webhook, &n).await {
                tracing::warn!(
                    kind = n.kind.label(),
                    error = %e,
                    "notification not delivered — the record still holds it; slack is a courtesy copy"
                );
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Notification {
        Notification {
            kind: Kind::Decision,
            title: "repair ready for shop — review and ship".into(),
            body: "widened the retry policy after an S3 latency spike".into(),
            repo: "shop".into(),
        }
    }

    /// The vocabulary is the product surface — pinned verbatim so a rewording
    /// is a deliberate act, not drift.
    #[test]
    fn the_four_kinds_carry_their_labels_and_framings_verbatim() {
        assert_eq!(Kind::Fyi.label(), "FYI");
        assert_eq!(Kind::Learning.label(), "LEARNING");
        assert_eq!(Kind::Working.label(), "WORKING");
        assert_eq!(Kind::Decision.label(), "DECISION");

        assert_eq!(Kind::Fyi.framing(), "investigated; nothing needs you.");
        assert_eq!(
            Kind::Learning.framing(),
            "a prior Bet matured and changed Drums' understanding."
        );
        assert_eq!(
            Kind::Working.framing(),
            "Drums found something worth investigating and is working on it."
        );
        assert_eq!(
            Kind::Decision.framing(),
            "Drums has completed the work; production approval required."
        );
    }

    #[test]
    fn payload_puts_the_kind_header_first_the_body_in_a_section_and_drums_repo_in_context() {
        let v = payload(&sample());
        let blocks = v["blocks"].as_array().expect("blocks array");
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0]["type"], "header");
        assert_eq!(
            blocks[0]["text"]["text"],
            "[DECISION] repair ready for shop — review and ship"
        );
        assert_eq!(blocks[1]["type"], "section");
        assert_eq!(
            blocks[1]["text"]["text"],
            "widened the retry policy after an S3 latency spike"
        );
        assert_eq!(blocks[2]["type"], "context");
        assert_eq!(blocks[2]["elements"][0]["text"], "drums · shop");
        // The tray-fallback text carries the same header, so a client that
        // renders no blocks still leads with the kind.
        assert_eq!(
            v["text"],
            "[DECISION] repair ready for shop — review and ship"
        );
    }

    /// Slack REJECTS a >150-char header rather than truncating it — a long
    /// belief must cost characters, never the whole notification.
    #[test]
    fn payload_clips_an_overlong_header_to_slacks_limit_instead_of_losing_the_message() {
        let mut n = sample();
        n.title = "x".repeat(400);
        let v = payload(&n);
        let header = v["blocks"][0]["text"]["text"].as_str().unwrap();
        assert_eq!(header.chars().count(), 150, "{header}");
        assert!(
            header.ends_with('…'),
            "truncation is disclosed, not silent: {header}"
        );
    }

    #[test]
    fn banned_words_are_matched_as_words_not_substrings() {
        assert_eq!(contains_banned_word("the fix worked"), Some("worked"));
        assert_eq!(
            contains_banned_word("this Proved the theory"),
            Some("proved")
        );
        assert_eq!(contains_banned_word("the deploy caused it"), Some("caused"));
        assert_eq!(
            contains_banned_word("a networked retry policy"),
            None,
            "'networked' is not 'worked'"
        );
        assert_eq!(
            contains_banned_word("supported; causal confidence low"),
            None
        );
    }

    #[test]
    fn webhook_resolution_prefers_env_over_config_and_ignores_blank_values() {
        assert_eq!(
            pick_webhook(Some("https://env"), Some("https://cfg")),
            Some("https://env".into())
        );
        assert_eq!(
            pick_webhook(None, Some("https://cfg")),
            Some("https://cfg".into())
        );
        assert_eq!(
            pick_webhook(Some("   "), Some("https://cfg")),
            Some("https://cfg".into()),
            "a blank env var is not a configuration"
        );
        assert_eq!(pick_webhook(None, Some("")), None);
        assert_eq!(pick_webhook(None, None), None);
    }

    /// Both errors must name the problem AND the fix, and the fixes differ:
    /// unreachable is about this machine; rejected is about the webhook.
    #[test]
    fn errors_name_the_problem_and_the_fix() {
        let unreachable = NotifyError::Unreachable("dns error".into()).to_string();
        assert!(unreachable.contains("dns error"), "{unreachable}");
        assert!(unreachable.contains("network"), "{unreachable}");
        assert!(unreachable.contains("hooks.slack.com"), "{unreachable}");

        let rejected = NotifyError::Rejected(404).to_string();
        assert!(rejected.contains("404"), "{rejected}");
        assert!(rejected.contains("revoked"), "{rejected}");
        assert!(
            rejected.contains("slack_webhook_url"),
            "the config key is the fix: {rejected}"
        );
        assert!(rejected.contains(WEBHOOK_ENV), "{rejected}");
    }

    #[test]
    fn an_event_id_passes_the_dedup_gate_exactly_once_per_run() {
        let sink = Sink::new("https://hooks.slack.com/services/T/B/x".into());
        assert!(sink.first_time("ev-1"));
        assert!(
            !sink.first_time("ev-1"),
            "the same event must never notify twice"
        );
        assert!(
            sink.first_time("ev-2"),
            "a different event is a different message"
        );
        // Clones share the set — a RepairReady deduped on a spawned task's
        // clone must stay deduped for the loop's own.
        let clone = sink.clone();
        assert!(!clone.first_time("ev-1"));
    }
}
