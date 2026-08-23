//! Reading behavior through the drums console — the plane bridge.
//!
//! # Why a bridge exists at all
//!
//! The account already granted PostHog access ONCE, by OAuth, in the console.
//! Asking the same person to also mint a personal API key and thread it into
//! an environment variable is the same access with worse consent and a second
//! credential to rotate. So a logged-in watch sends its queries to the
//! console, and the console spends the account's own grant — refreshing it at
//! the moment of use, exactly as it does for every other PostHog read.
//!
//! The engine keeps building queries and parsing answers with the same pure,
//! tested functions the direct path uses ([`crate::posthog`],
//! [`crate::frustration`]); the console passes PostHog's response through
//! verbatim. The two ends of the bridge therefore cannot drift: there is no
//! translation layer to disagree with.
//!
//! # Shape of this module
//!
//! Same as [`crate::posthog`]: one impure function ([`Plane::run`]) and pure
//! everything else. The interesting part — mapping the console's refusals
//! onto [`BehaviorError`] — is [`map_refusal`], a pure function with tests.

use serde_json::Value;

use engine_core::evaluation::{EvaluationTarget, Sample};

use crate::{
    frustration, posthog, rate_for, BehaviorError, BehaviorSource, EvaluationEntry, SeenEvent,
};

/// A logged-in machine's line to the console's behavior endpoint.
///
/// `token` is the CLI credential from `drums login`. Never logged and never
/// rendered — the hand-written [`std::fmt::Debug`] exists for the same reason
/// the one on [`crate::PostHog`] does.
#[derive(Clone)]
pub struct Plane {
    base: String,
    token: String,
    http: reqwest::Client,
}

impl std::fmt::Debug for Plane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Plane")
            .field("base", &self.base)
            .field("token", &"<redacted>")
            .finish()
    }
}

impl Plane {
    /// `base` is the console origin — `https://app.drums.sh` unless
    /// `DRUMS_CONSOLE_URL` says otherwise. Trailing slashes tolerated.
    pub fn new(base: impl Into<String>, token: impl Into<String>) -> Self {
        Plane {
            base: base.into().trim_end_matches('/').to_string(),
            token: token.into(),
            http: reqwest::Client::new(),
        }
    }

    fn query_url(&self) -> String {
        format!("{}/api/behavior/query", self.base)
    }

    /// Send one query across the bridge. The only impure function here.
    async fn run(&self, hogql: &str) -> Result<Value, BehaviorError> {
        let res = self
            .http
            .post(self.query_url())
            .bearer_auth(&self.token)
            .json(&serde_json::json!({ "query": hogql }))
            .send()
            .await
            .map_err(|e| BehaviorError::Unreachable(e.to_string()))?;

        let status = res.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(BehaviorError::Unauthorized);
        }
        let text = res
            .text()
            .await
            .map_err(|e| BehaviorError::Unreachable(e.to_string()))?;
        if !status.is_success() {
            let body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            return Err(map_refusal(
                status.as_u16(),
                body.get("error").and_then(Value::as_str),
                body.get("detail").and_then(Value::as_str),
            ));
        }
        serde_json::from_str(&text).map_err(|e| BehaviorError::Shape(e.to_string()))
    }
}

/// The console's machine-readable refusals, mapped onto this crate's own
/// vocabulary — so the observe tick treats them exactly like a direct source's.
///
/// The first three are QUIET states on purpose: an account that has not
/// connected PostHog, holds the old narrow grant, or has not targeted a
/// project is not broken, and a watch that warned about it every ten minutes
/// would teach people to ignore warnings. `doctor` is where those states get
/// named to a human, not the tick log.
fn map_refusal(status: u16, error: Option<&str>, detail: Option<&str>) -> BehaviorError {
    match error {
        Some("not_connected") | Some("grant_too_narrow") | Some("no_project") => {
            BehaviorError::UnsupportedMetric {
                source_name: "the console's PostHog grant",
                metric: "behavioral reads",
            }
        }
        // The stored OAuth token died and could not be refreshed. A human fix
        // — reconnect — which is exactly what Unauthorized means here.
        Some("token_expired") => BehaviorError::Unauthorized,
        _ if status >= 500 => BehaviorError::Unreachable(
            detail
                .unwrap_or("the console could not reach the behavior source")
                .to_string(),
        ),
        _ => BehaviorError::Rejected(
            detail
                .or(error)
                .unwrap_or("the console refused the query")
                .to_string(),
        ),
    }
}

#[async_trait::async_trait]
impl BehaviorSource for Plane {
    async fn seen_events(&self, days: u32, limit: u32) -> Result<Vec<SeenEvent>, BehaviorError> {
        posthog::parse_seen_events(&self.run(&posthog::seen_events_query(days, limit)).await?)
    }

    async fn sample(
        &self,
        workflow: &EvaluationTarget,
        days: u32,
    ) -> Result<Sample, BehaviorError> {
        // Same pre-flight refusal as the direct source, for the same reason:
        // the error should name the metric, not arrive after a round trip.
        if !matches!(
            workflow.metric,
            engine_core::evaluation::Metric::CompletionRate
                | engine_core::evaluation::Metric::Abandonment
        ) {
            return Err(BehaviorError::UnsupportedMetric {
                source_name: "PostHog",
                metric: workflow.metric.label(),
            });
        }
        let q = posthog::completion_query(&workflow.start_event, &workflow.success_event, days)?;
        let (entered, completed) = posthog::parse_completion(&self.run(&q).await?)?;
        rate_for(workflow.metric, entered, completed)
    }

    async fn sample_between(
        &self,
        workflow: &EvaluationTarget,
        from_ms: u64,
        to_ms: u64,
    ) -> Result<Sample, BehaviorError> {
        if !matches!(
            workflow.metric,
            engine_core::evaluation::Metric::CompletionRate
                | engine_core::evaluation::Metric::Abandonment
        ) {
            return Err(BehaviorError::UnsupportedMetric {
                source_name: "PostHog",
                metric: workflow.metric.label(),
            });
        }
        let q = posthog::completion_query_between(
            &workflow.start_event,
            &workflow.success_event,
            from_ms,
            to_ms,
        )?;
        let (entered, completed) = posthog::parse_completion(&self.run(&q).await?)?;
        rate_for(workflow.metric, entered, completed)
    }

    async fn entries(
        &self,
        workflow: &EvaluationTarget,
        days: u32,
        limit: u32,
    ) -> Result<Vec<EvaluationEntry>, BehaviorError> {
        let q =
            posthog::entries_query(&workflow.start_event, &workflow.success_event, days, limit)?;
        posthog::parse_entries(&self.run(&q).await?, &workflow.id)
    }

    async fn frustration_between(
        &self,
        from_ms: u64,
        to_ms: u64,
        limit: u32,
    ) -> Result<Vec<frustration::FrustrationGroup>, BehaviorError> {
        let q = frustration::frustration_query(from_ms, to_ms, limit)?;
        frustration::parse_frustration(&self.run(&q).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cli_token_never_appears_in_debug_output() {
        let p = Plane::new("https://app.drums.sh", "drums_pat_supersecret");
        let shown = format!("{p:?}");
        assert!(
            !shown.contains("drums_pat_supersecret"),
            "token leaked: {shown}"
        );
        assert!(shown.contains("<redacted>"));
    }

    #[test]
    fn a_trailing_slash_on_the_base_does_not_double_up() {
        let p = Plane::new("https://app.drums.sh/", "t");
        assert_eq!(p.query_url(), "https://app.drums.sh/api/behavior/query");
    }

    #[test]
    fn quiet_states_stay_quiet_and_human_fixes_say_so() {
        for code in ["not_connected", "grant_too_narrow", "no_project"] {
            assert!(
                matches!(
                    map_refusal(409, Some(code), Some("whatever")),
                    BehaviorError::UnsupportedMetric { .. }
                ),
                "{code} must map to the tick's silent skip"
            );
        }
        assert!(matches!(
            map_refusal(409, Some("token_expired"), None),
            BehaviorError::Unauthorized
        ));
    }

    #[test]
    fn an_unknown_refusal_keeps_its_detail_and_a_500_reads_as_unreachable() {
        match map_refusal(400, Some("bad_query"), Some("one statement only")) {
            BehaviorError::Rejected(d) => assert_eq!(d, "one statement only"),
            other => panic!("expected Rejected, got {other:?}"),
        }
        assert!(matches!(
            map_refusal(502, None, None),
            BehaviorError::Unreachable(_)
        ));
    }
}
