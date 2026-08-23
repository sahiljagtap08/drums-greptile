//! The poll half of reported intake.
//!
//! Webhooks stay the fast path: an issue whose webhook fires arrives in
//! seconds. This is the half that cannot be misconfigured — nothing about it
//! lives in the tracker's settings. On the observe tick, a logged-in watch
//! asks the console for issues the team filed recently (the console spends
//! the account's own Linear grant, exactly like the behavior bridge), skips
//! every issue the record already holds, and hands the fresh ones to its own
//! local ingest — the same door the webhook uses, so both paths produce the
//! same record lines, the same dedup key, and the same repair eligibility.
//!
//! Idempotence is the record itself: `reported` lines carry the tracker's
//! `external_id`, and an issue whose id is already on the record is not
//! ingested again — across ticks and across daemon restarts, for the same
//! reason `rebuild_opened_signatures` replays the record at boot.

use std::collections::HashSet;

use serde_json::Value;

/// The states worth taking: work someone intends to do. Completed and
/// canceled issues are history, not intake — a watch that ingests a closed
/// issue proposes a repair for a problem the team already ended.
const OPEN_STATES: &[&str] = &["triage", "backlog", "unstarted", "started"];

pub struct TrackerPoll {
    console_url: String,
    token: String,
    ingest_port: u16,
    http: reqwest::Client,
}

impl std::fmt::Debug for TrackerPoll {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrackerPoll")
            .field("console_url", &self.console_url)
            .field("ingest_port", &self.ingest_port)
            .field("token", &"<redacted>")
            .finish()
    }
}

impl TrackerPoll {
    pub fn new(console_url: impl Into<String>, token: impl Into<String>, ingest_port: u16) -> Self {
        TrackerPoll {
            console_url: console_url.into().trim_end_matches('/').to_string(),
            token: token.into(),
            ingest_port,
            http: reqwest::Client::new(),
        }
    }

    /// One poll: fetch, dedup against the record, ingest what is new.
    /// Returns how many issues were taken. Quiet states — no Linear
    /// connected, the grant expired — come back as `Ok(0)`; the tick has no
    /// business warning every ten minutes about an integration nobody made.
    pub async fn tick(&self, lines: &[(String, Value)], now_ms: u64) -> Result<usize, String> {
        let seen = seen_external_ids(lines);
        let since = now_ms.saturating_sub(24 * 3_600_000);
        let url = format!("{}/api/tracker/issues?since_ms={since}", self.console_url);
        let res = self
            .http
            .get(&url)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| format!("console unreachable: {e}"))?;
        let status = res.status();
        let body: Value = res
            .json()
            .await
            .map_err(|e| format!("console answer unreadable: {e}"))?;
        if !status.is_success() {
            let code = body.get("error").and_then(Value::as_str).unwrap_or("");
            // Normal silences, not faults.
            if matches!(code, "not_connected" | "token_expired" | "grant_too_narrow") {
                return Ok(0);
            }
            return Err(format!(
                "console refused the poll: {}",
                if code.is_empty() {
                    status.as_str()
                } else {
                    code
                }
            ));
        }

        let mut taken = 0usize;
        for issue in fresh_issues(&body, &seen) {
            let ingest = format!("http://127.0.0.1:{}/v1/adapters/linear", self.ingest_port);
            let posted = self.http.post(&ingest).json(&issue).send().await;
            match posted {
                Ok(r) if r.status().is_success() => taken += 1,
                Ok(r) => return Err(format!("own ingest refused a polled issue: {}", r.status())),
                Err(e) => return Err(format!("own ingest unreachable: {e}")),
            }
        }
        Ok(taken)
    }
}

/// Every tracker id the record already holds a `reported` line for.
fn seen_external_ids(lines: &[(String, Value)]) -> HashSet<String> {
    lines
        .iter()
        .filter(|(kind, _)| kind == "reported")
        .filter_map(|(_, v)| v.get("external_id").and_then(Value::as_str))
        .map(str::to_string)
        .collect()
}

/// The console's issues, filtered to open work the record has not seen,
/// reshaped into exactly the payload the Linear adapter's extractors read —
/// `data.issueId` first in the external-id candidate list, `identifier` for
/// the human `DRM-42`. One shape for both intake paths.
fn fresh_issues(body: &Value, seen: &HashSet<String>) -> Vec<Value> {
    body.get("issues")
        .and_then(Value::as_array)
        .map(|issues| {
            issues
                .iter()
                .filter_map(|i| {
                    let id = i.get("id").and_then(Value::as_str)?;
                    if seen.contains(id) {
                        return None;
                    }
                    let state = i.get("stateType").and_then(Value::as_str);
                    if let Some(s) = state {
                        if !OPEN_STATES.contains(&s) {
                            return None;
                        }
                    }
                    Some(serde_json::json!({
                        "title": i.get("title").and_then(Value::as_str).unwrap_or(""),
                        "description": i.get("description").and_then(Value::as_str),
                        "url": i.get("url").and_then(Value::as_str),
                        "identifier": i.get("identifier").and_then(Value::as_str),
                        "data": { "issueId": id },
                    }))
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn record_with(ids: &[&str]) -> Vec<(String, Value)> {
        ids.iter()
            .map(|id| {
                (
                    "reported".to_string(),
                    json!({ "id": "01H_ULID", "external_id": id }),
                )
            })
            .collect()
    }

    #[test]
    fn an_issue_the_record_holds_is_not_ingested_again() {
        let body = json!({ "issues": [
            { "id": "uuid-1", "identifier": "DRM-1", "title": "a", "stateType": "started" },
            { "id": "uuid-2", "identifier": "DRM-2", "title": "b", "stateType": "started" },
        ]});
        let seen = seen_external_ids(&record_with(&["uuid-1"]));
        let fresh = fresh_issues(&body, &seen);
        assert_eq!(fresh.len(), 1);
        assert_eq!(fresh[0]["data"]["issueId"], "uuid-2");
    }

    #[test]
    fn closed_work_is_history_not_intake() {
        let body = json!({ "issues": [
            { "id": "u1", "identifier": "DRM-1", "title": "done", "stateType": "completed" },
            { "id": "u2", "identifier": "DRM-2", "title": "dead", "stateType": "canceled" },
            { "id": "u3", "identifier": "DRM-3", "title": "live", "stateType": "triage" },
        ]});
        let fresh = fresh_issues(&body, &HashSet::new());
        assert_eq!(fresh.len(), 1);
        assert_eq!(fresh[0]["data"]["issueId"], "u3");
    }

    #[test]
    fn the_payload_reads_back_through_the_adapters_candidate_lists() {
        // The adapter reads title, description, url, identifier, and takes
        // data.issueId as the external id; a drifted shape here would ingest
        // issues with no external id and the write-back would have nothing
        // to address.
        let body = json!({ "issues": [{
            "id": "uuid-9", "identifier": "DRM-9", "title": "t",
            "description": "d", "url": "https://linear.app/x", "stateType": "backlog",
        }]});
        let fresh = fresh_issues(&body, &HashSet::new());
        let p = &fresh[0];
        assert_eq!(p["title"], "t");
        assert_eq!(p["description"], "d");
        assert_eq!(p["url"], "https://linear.app/x");
        assert_eq!(p["identifier"], "DRM-9");
        assert_eq!(p["data"]["issueId"], "uuid-9");
    }

    #[test]
    fn the_login_token_never_appears_in_debug_output() {
        let t = TrackerPoll::new("https://app.drums.sh", "drums_pat_secret", 7787);
        let shown = format!("{t:?}");
        assert!(!shown.contains("drums_pat_secret"), "{shown}");
    }
}
