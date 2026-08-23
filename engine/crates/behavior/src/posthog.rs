//! Reading workflow behavior out of a customer's existing PostHog.
//!
//! `POST {host}/api/projects/{project}/query/` with a personal API key, body
//! `{"query": {"kind": "HogQLQuery", "query": "..."}}`, answering with
//! `{"results": [[...]], "columns": [...]}`.
//!
//! # Shape of this module
//!
//! Everything except the one HTTP call is pure, and deliberately so: the parts
//! that can be wrong in ways a customer would notice — how the completion query
//! defines completion, how event names are escaped, how a response is read — are
//! all testable without a network or an account. The async methods are thin
//! wrappers that send a string and hand the JSON to a pure parser.

use crate::{rate_for, BehaviorError, BehaviorSource, EvaluationEntry, SeenEvent};
use engine_core::evaluation::{EvaluationId, EvaluationTarget, Sample};
use serde_json::Value;

/// A customer's PostHog project.
///
/// `api_key` is a personal API key with query-read scope. It is never logged and
/// never rendered — hence the hand-written [`std::fmt::Debug`], because deriving
/// it would put the key into every `tracing` line that ever formats this struct,
/// and that is exactly the kind of leak nobody notices until it is in a support
/// transcript.
#[derive(Clone)]
pub struct PostHog {
    host: String,
    project: String,
    api_key: String,
    http: reqwest::Client,
}

impl std::fmt::Debug for PostHog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostHog")
            .field("host", &self.host)
            .field("project", &self.project)
            .field("api_key", &"<redacted>")
            .finish()
    }
}

impl PostHog {
    /// `host` is the customer's PostHog app host — `https://us.posthog.com`,
    /// `https://eu.posthog.com`, or a self-hosted origin. Trailing slashes are
    /// tolerated because people paste them.
    pub fn new(
        host: impl Into<String>,
        project: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        PostHog {
            host: host.into().trim_end_matches('/').to_string(),
            project: project.into(),
            api_key: api_key.into(),
            http: reqwest::Client::new(),
        }
    }

    fn query_url(&self) -> String {
        format!("{}/api/projects/{}/query/", self.host, self.project)
    }

    /// Send one HogQL query. The only impure function in this module.
    async fn run(&self, hogql: &str) -> Result<Value, BehaviorError> {
        let body = serde_json::json!({ "query": { "kind": "HogQLQuery", "query": hogql } });
        let res = self
            .http
            .post(self.query_url())
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| BehaviorError::Unreachable(e.to_string()))?;

        let status = res.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(BehaviorError::Unauthorized);
        }
        let text = res
            .text()
            .await
            .map_err(|e| BehaviorError::Unreachable(e.to_string()))?;
        if !status.is_success() {
            // PostHog puts a readable reason in the body for a bad query. Passing
            // it through is what turns "query failed" into something the person
            // who wrote the workflow can act on.
            return Err(BehaviorError::Rejected(truncate(&text, 400)));
        }
        serde_json::from_str(&text).map_err(|e| BehaviorError::Shape(e.to_string()))
    }
}

/// Event names go into a SQL string, so they are validated rather than trusted.
///
/// The customer types these into the workflow creation screen, which makes them
/// untrusted input on a path that builds a query. Escaping alone would probably
/// be enough; an allowlist plus escaping is cheap, and the failure mode of being
/// too strict is a clear error message rather than a query that does something
/// nobody asked for.
///
/// PostHog's own conventions are covered: `quote_started`, `$pageview`,
/// `user signed up`, `checkout:step-2`.
pub fn validate_event_name(name: &str) -> Result<&str, BehaviorError> {
    let ok = !name.is_empty()
        && name.len() <= 200
        && name.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '$' | ':' | '/' | ' ')
        });
    if ok {
        Ok(name)
    } else {
        Err(BehaviorError::UnsafeEventName(name.to_string()))
    }
}

/// Single-quote a validated string for HogQL. Doubling is the escape ClickHouse
/// accepts; the allowlist above already excludes quotes, so this is the second
/// of two locks rather than the only one.
fn quoted(name: &str) -> String {
    format!("'{}'", name.replace('\'', "''"))
}

/// The completion query, built and returned as a string so it can be asserted on
/// in a test rather than only observed in production.
///
/// # How completion is defined, and why this shape
///
/// Group the window's events by person; keep only people who actually *entered*
/// inside it; count how many of those also reached success at or after their
/// start.
///
/// The obvious cheaper query — count distinct people who fired the start event,
/// count distinct people who fired the success event, divide — is wrong in a way
/// that shows. Someone who started yesterday and finished today is a completion
/// with no entry inside the window, so the ratio can exceed 1 and the console
/// reports a completion rate of 130%. Anchoring on entrants makes the numerator
/// a subset of the denominator by construction.
///
/// `>= started_at` rather than `>` because a workflow whose start and success
/// are emitted in the same request legitimately shares a timestamp.
pub fn completion_query(
    start_event: &str,
    success_event: &str,
    days: u32,
) -> Result<String, BehaviorError> {
    let start = quoted(validate_event_name(start_event)?);
    let success = quoted(validate_event_name(success_event)?);
    let days = days.clamp(1, 365);
    Ok(format!(
        "SELECT count() AS entered, countIf(succeeded_at >= started_at) AS completed FROM (\
           SELECT person_id, \
                  minIf(timestamp, event = {start}) AS started_at, \
                  maxIf(timestamp, event = {success}) AS succeeded_at \
           FROM events \
           WHERE timestamp >= now() - INTERVAL {days} DAY \
             AND event IN ({start}, {success}) \
           GROUP BY person_id \
           HAVING countIf(event = {start}) > 0\
         )"
    ))
}

/// Per-person passages, for attaching observations to a workflow. Same
/// entrant-anchored definition as [`completion_query`].
pub fn entries_query(
    start_event: &str,
    success_event: &str,
    days: u32,
    limit: u32,
) -> Result<String, BehaviorError> {
    let start = quoted(validate_event_name(start_event)?);
    let success = quoted(validate_event_name(success_event)?);
    let days = days.clamp(1, 365);
    let limit = limit.clamp(1, 10_000);
    Ok(format!(
        "SELECT toString(person_id) AS person, \
                toUnixTimestamp(started_at) AS started, \
                toUnixTimestamp(succeeded_at) AS succeeded FROM (\
           SELECT person_id, \
                  minIf(timestamp, event = {start}) AS started_at, \
                  maxIf(timestamp, event = {success}) AS succeeded_at \
           FROM events \
           WHERE timestamp >= now() - INTERVAL {days} DAY \
             AND event IN ({start}, {success}) \
           GROUP BY person_id \
           HAVING countIf(event = {start}) > 0\
         ) ORDER BY started_at DESC LIMIT {limit}"
    ))
}

/// The completion query over an EXPLICIT window — for outcome measurement,
/// where the window was declared before rollout and must not slide with the
/// clock. `now()`-relative queries drift: a measurement taken a day late would
/// quietly include a day that was never part of the declared window, and a
/// window that slides is a window that can be shopped.
pub fn completion_query_between(
    start_event: &str,
    success_event: &str,
    from_ms: u64,
    to_ms: u64,
) -> Result<String, BehaviorError> {
    if to_ms <= from_ms {
        return Err(BehaviorError::Shape("window ends before it starts".into()));
    }
    let start = quoted(validate_event_name(start_event)?);
    let success = quoted(validate_event_name(success_event)?);
    let (from_s, to_s) = (from_ms / 1000, to_ms / 1000);
    Ok(format!(
        "SELECT count() AS entered, countIf(succeeded_at >= started_at) AS completed FROM (\
           SELECT person_id, \
                  minIf(timestamp, event = {start}) AS started_at, \
                  maxIf(timestamp, event = {success}) AS succeeded_at \
           FROM events \
           WHERE timestamp >= toDateTime({from_s}) AND timestamp < toDateTime({to_s}) \
             AND event IN ({start}, {success}) \
           GROUP BY person_id \
           HAVING countIf(event = {start}) > 0\
         )"
    ))
}

/// Distinct event names, most frequent first, for the workflow creation picker.
pub fn seen_events_query(days: u32, limit: u32) -> String {
    let days = days.clamp(1, 365);
    let limit = limit.clamp(1, 1000);
    format!(
        "SELECT event, count() AS n FROM events \
         WHERE timestamp >= now() - INTERVAL {days} DAY \
         GROUP BY event ORDER BY n DESC LIMIT {limit}"
    )
}

/// Read a HogQL response by column NAME rather than position.
///
/// Positional indexing works right up until a query grows a column, and then it
/// silently reads the wrong field instead of failing — which for this crate
/// would mean a plausible completion rate computed from the wrong numbers.
pub(crate) fn column_index(body: &Value, name: &str) -> Result<usize, BehaviorError> {
    body.get("columns")
        .and_then(Value::as_array)
        .ok_or_else(|| BehaviorError::Shape("response has no `columns`".into()))?
        .iter()
        .position(|c| c.as_str() == Some(name))
        .ok_or_else(|| BehaviorError::Shape(format!("response has no `{name}` column")))
}

pub(crate) fn rows(body: &Value) -> Result<&Vec<Value>, BehaviorError> {
    body.get("results")
        .and_then(Value::as_array)
        .ok_or_else(|| BehaviorError::Shape("response has no `results`".into()))
}

/// PostHog returns numbers as JSON numbers, but a count can come back as a
/// string from some deployments. Accept both rather than failing on a difference
/// that means nothing.
pub(crate) fn as_u64(v: Option<&Value>) -> Option<u64> {
    match v? {
        Value::Number(n) => n.as_u64().or_else(|| n.as_f64().map(|f| f.max(0.0) as u64)),
        Value::String(s) => s.parse::<u64>().ok(),
        _ => None,
    }
}

pub fn parse_completion(body: &Value) -> Result<(u64, u64), BehaviorError> {
    let entered_at = column_index(body, "entered")?;
    let completed_at = column_index(body, "completed")?;
    let row = rows(body)?
        .first()
        .and_then(Value::as_array)
        // No rows means nobody entered the workflow in the window. That is a
        // real and common answer, not a malformed response, and it must read as
        // zero traffic rather than as an error — the console draws those two
        // very differently.
        .map(|r| {
            (
                as_u64(r.get(entered_at)).unwrap_or(0),
                as_u64(r.get(completed_at)).unwrap_or(0),
            )
        })
        .unwrap_or((0, 0));
    Ok(row)
}

pub fn parse_seen_events(body: &Value) -> Result<Vec<SeenEvent>, BehaviorError> {
    let name_at = column_index(body, "event")?;
    let n_at = column_index(body, "n")?;
    Ok(rows(body)?
        .iter()
        .filter_map(Value::as_array)
        .filter_map(|r| {
            Some(SeenEvent {
                name: r.get(name_at)?.as_str()?.to_string(),
                count: as_u64(r.get(n_at)).unwrap_or(0),
            })
        })
        .collect())
}

pub fn parse_entries(
    body: &Value,
    workflow: &EvaluationId,
) -> Result<Vec<EvaluationEntry>, BehaviorError> {
    let person_at = column_index(body, "person")?;
    let started_at = column_index(body, "started")?;
    let succeeded_at = column_index(body, "succeeded")?;
    Ok(rows(body)?
        .iter()
        .filter_map(Value::as_array)
        .filter_map(|r| {
            let started = as_u64(r.get(started_at))?;
            // ClickHouse renders "this person never fired the success event" as
            // the zero timestamp, not as null. Treating 0 as a completion at the
            // epoch would mark every abandonment as a success.
            let succeeded = as_u64(r.get(succeeded_at)).filter(|s| *s > 0 && *s >= started);
            Some(EvaluationEntry {
                evaluation: workflow.clone(),
                person: r.get(person_at)?.as_str()?.to_string(),
                started_at_ms: started.saturating_mul(1000),
                completed_at_ms: succeeded.map(|s| s.saturating_mul(1000)),
            })
        })
        .collect())
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let end = (0..=max)
        .rev()
        .find(|i| s.is_char_boundary(*i))
        .unwrap_or(0);
    format!("{}…", &s[..end])
}

#[async_trait::async_trait]
impl BehaviorSource for PostHog {
    async fn seen_events(&self, days: u32, limit: u32) -> Result<Vec<SeenEvent>, BehaviorError> {
        parse_seen_events(&self.run(&seen_events_query(days, limit)).await?)
    }

    async fn sample(
        &self,
        workflow: &EvaluationTarget,
        days: u32,
    ) -> Result<Sample, BehaviorError> {
        // Refuse a metric we cannot compute BEFORE spending a query on it, so
        // the error names the real problem rather than arriving after a
        // round-trip that was never going to help.
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
        let q = completion_query(&workflow.start_event, &workflow.success_event, days)?;
        let (entered, completed) = parse_completion(&self.run(&q).await?)?;
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
        let q = completion_query_between(
            &workflow.start_event,
            &workflow.success_event,
            from_ms,
            to_ms,
        )?;
        let (entered, completed) = parse_completion(&self.run(&q).await?)?;
        rate_for(workflow.metric, entered, completed)
    }

    async fn entries(
        &self,
        workflow: &EvaluationTarget,
        days: u32,
        limit: u32,
    ) -> Result<Vec<EvaluationEntry>, BehaviorError> {
        let q = entries_query(&workflow.start_event, &workflow.success_event, days, limit)?;
        parse_entries(&self.run(&q).await?, &workflow.id)
    }

    async fn frustration_between(
        &self,
        from_ms: u64,
        to_ms: u64,
        limit: u32,
    ) -> Result<Vec<crate::frustration::FrustrationGroup>, BehaviorError> {
        let q = crate::frustration::frustration_query(from_ms, to_ms, limit)?;
        crate::frustration::parse_frustration(&self.run(&q).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_core::evaluation::Metric;
    use serde_json::json;

    #[test]
    fn the_api_key_never_appears_in_debug_output() {
        let ph = PostHog::new("https://us.posthog.com", "12345", "phx_supersecret_value");
        let shown = format!("{ph:?}");
        assert!(
            !shown.contains("phx_supersecret_value"),
            "key leaked into Debug: {shown}"
        );
        assert!(shown.contains("<redacted>"));
    }

    #[test]
    fn a_trailing_slash_on_the_host_does_not_double_up_in_the_url() {
        let ph = PostHog::new("https://eu.posthog.com/", "9", "k");
        assert_eq!(
            ph.query_url(),
            "https://eu.posthog.com/api/projects/9/query/"
        );
    }

    #[test]
    fn posthog_style_event_names_are_accepted() {
        for name in [
            "quote_started",
            "$pageview",
            "user signed up",
            "checkout:step-2",
            "a/b.test",
        ] {
            assert!(
                validate_event_name(name).is_ok(),
                "{name} should be allowed"
            );
        }
    }

    #[test]
    fn an_event_name_that_could_escape_the_query_is_refused_before_sending() {
        for bad in [
            "'; DROP TABLE events--",
            "a' OR '1'='1",
            "ev\"il",
            "",
            "back\\slash",
        ] {
            assert!(
                matches!(
                    validate_event_name(bad),
                    Err(BehaviorError::UnsafeEventName(_))
                ),
                "{bad:?} should be refused"
            );
        }
        // And the refusal happens at query construction, not at send time.
        assert!(completion_query("ok_event", "'; DROP TABLE events--", 7).is_err());
    }

    #[test]
    fn the_completion_query_anchors_on_entrants() {
        // The property that keeps completion rate <= 100%: the population is
        // people who STARTED inside the window, and a completion only counts for
        // somebody already in that population.
        let q = completion_query("quote_started", "quote_submitted", 7).unwrap();
        assert!(q.contains("HAVING countIf(event = 'quote_started') > 0"));
        assert!(q.contains("countIf(succeeded_at >= started_at)"));
        assert!(q.contains("INTERVAL 7 DAY"));
    }

    #[test]
    fn window_and_limit_are_clamped_rather_than_interpolated_raw() {
        assert!(completion_query("a", "b", 0)
            .unwrap()
            .contains("INTERVAL 1 DAY"));
        assert!(completion_query("a", "b", 99_999)
            .unwrap()
            .contains("INTERVAL 365 DAY"));
        assert!(entries_query("a", "b", 7, 0).unwrap().contains("LIMIT 1"));
        assert!(entries_query("a", "b", 7, 10_000_000)
            .unwrap()
            .contains("LIMIT 10000"));
    }

    #[test]
    fn a_response_is_read_by_column_name_not_position() {
        // Same numbers, columns swapped. Positional reading would silently
        // return a completion rate computed from the wrong pair.
        let a = json!({"columns": ["entered", "completed"], "results": [[400, 312]]});
        let b = json!({"columns": ["completed", "entered"], "results": [[312, 400]]});
        assert_eq!(parse_completion(&a).unwrap(), (400, 312));
        assert_eq!(parse_completion(&b).unwrap(), (400, 312));
    }

    #[test]
    fn an_extra_column_does_not_shift_the_reading() {
        let body = json!({
            "columns": ["some_new_thing", "entered", "completed"],
            "results": [["x", 400, 312]]
        });
        assert_eq!(parse_completion(&body).unwrap(), (400, 312));
    }

    #[test]
    fn no_rows_is_zero_traffic_and_not_an_error() {
        // Nobody entered the workflow in the window. Common, and the console
        // draws "no traffic" and "query failed" very differently.
        let body = json!({"columns": ["entered", "completed"], "results": []});
        assert_eq!(parse_completion(&body).unwrap(), (0, 0));
        let s = rate_for(Metric::CompletionRate, 0, 0).unwrap();
        assert_eq!(s.entries, 0);
    }

    #[test]
    fn counts_returned_as_strings_are_still_read() {
        let body = json!({"columns": ["entered", "completed"], "results": [["400", "312"]]});
        assert_eq!(parse_completion(&body).unwrap(), (400, 312));
    }

    #[test]
    fn a_missing_column_is_a_shape_error_rather_than_a_zero() {
        // Silently reading this as 0/0 would render "no traffic" for a response
        // that actually means the query changed underneath us.
        let body = json!({"columns": ["entered"], "results": [[400]]});
        assert!(matches!(
            parse_completion(&body),
            Err(BehaviorError::Shape(_))
        ));
    }

    #[test]
    fn a_zero_success_timestamp_is_an_abandonment_not_an_epoch_completion() {
        // ClickHouse renders "never fired" as the zero timestamp, not null.
        // Reading it as a completion would mark every abandonment a success —
        // and completion rate would read 100% for a workflow nobody finishes.
        let body = json!({
            "columns": ["person", "started", "succeeded"],
            "results": [["p1", 1_760_000_000, 0], ["p2", 1_760_000_100, 1_760_000_400]]
        });
        let got = parse_entries(&body, &EvaluationId("wf_1".into())).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].completed_at_ms, None, "p1 abandoned");
        assert_eq!(got[1].completed_at_ms, Some(1_760_000_400_000));
        assert_eq!(got[0].evaluation, EvaluationId("wf_1".into()));
    }

    #[test]
    fn a_success_before_its_start_is_not_counted_as_a_completion() {
        let body = json!({
            "columns": ["person", "started", "succeeded"],
            "results": [["p1", 1_760_000_500, 1_760_000_100]]
        });
        let got = parse_entries(&body, &EvaluationId("wf_1".into())).unwrap();
        assert_eq!(got[0].completed_at_ms, None);
    }

    #[test]
    fn every_entry_carries_the_workflow_it_belongs_to() {
        // The whole point of item 1 in the build order: behavior enters Drums
        // already attached to a workflow, never as loose events.
        let body = json!({
            "columns": ["person", "started", "succeeded"],
            "results": [["p1", 1_760_000_000, 0]]
        });
        let wf = EvaluationId("wf_8fk2p1".into());
        for e in parse_entries(&body, &wf).unwrap() {
            assert_eq!(e.evaluation, wf);
        }
    }

    #[test]
    fn seen_events_come_back_most_frequent_first_for_the_picker() {
        let q = seen_events_query(30, 50);
        assert!(q.contains("ORDER BY n DESC"));
        let body = json!({
            "columns": ["event", "n"],
            "results": [["$pageview", 91_233], ["quote_started", 812]]
        });
        let got = parse_seen_events(&body).unwrap();
        assert_eq!(
            got[0],
            SeenEvent {
                name: "$pageview".into(),
                count: 91_233
            }
        );
        assert_eq!(got[1].name, "quote_started");
    }

    #[test]
    fn truncate_does_not_split_a_multibyte_character() {
        let s = "é".repeat(500);
        let t = truncate(&s, 400);
        assert!(t.len() <= 401 + 3);
        assert!(t.ends_with('…'));
    }
}

#[cfg(test)]
mod window_query_tests {
    use super::*;

    #[test]
    fn the_explicit_window_is_frozen_in_the_query_and_refuses_backwards() {
        let q = completion_query_between(
            "quote_started",
            "quote_submitted",
            1_754_000_000_000,
            1_754_604_800_000,
        )
        .unwrap();
        assert!(q.contains("toDateTime(1754000000)"), "{q}");
        assert!(q.contains("toDateTime(1754604800)"), "{q}");
        assert!(
            !q.contains("now()"),
            "a declared window must not slide with the clock: {q}"
        );
        assert!(
            q.contains("HAVING countIf(event = 'quote_started') > 0"),
            "entrant-anchored, same as the relative query"
        );
        assert!(completion_query_between("a", "b", 5, 5).is_err());
        assert!(
            completion_query_between("'; DROP--", "b", 1, 2).is_err(),
            "same allowlist"
        );
    }
}
