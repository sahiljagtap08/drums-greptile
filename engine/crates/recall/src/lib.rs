//! What the record remembers — the recall layer.
//!
//! # Why this exists
//!
//! The record compounds: every failure, repair, ship, revert, report and
//! observation is appended and never overwritten. But until this crate, the
//! agent started every repair cold — nothing whispered "a fix here was
//! shipped and rolled back", "this error has fired forty times since
//! Tuesday", or "users rage-clicked this page last week". Recall is the
//! difference between a loop that accumulates history and a product that
//! learns from it: fix N+1 starts where fix N left off.
//!
//! # The honesty rules
//!
//! - **Retrieval proposes, the agent verifies.** Every remembered line names
//!   WHY it was recalled (same file, same error name, a path the issue
//!   mentions) and carries its age. The prompt section that renders these
//!   says out loud that they are records, not instructions.
//! - **Only joins the record actually supports.** `repair_ready` and
//!   `shipped`/`reverted` share `failure_id` — that join is real. `event`
//!   lines carry no failure id, so tying a past repair to the *current*
//!   error goes through text the record really holds (a file named in the
//!   diff stat, an error name in a summary), never through an invented key.
//!   The digest module refuses to assume this structure; recall inherits the
//!   refusal.
//! - **Bounded hard.** The prompt rides a single argv element under Linux's
//!   `MAX_ARG_STRLEN`; recall is capped in lines and characters, most
//!   valuable first. Reverted repairs outrank everything: the most expensive
//!   mistake an agent can make is re-shipping a fix that already failed.

use std::collections::HashMap;

use serde_json::Value;

/// At most this many remembered lines reach a prompt.
pub const MAX_LINES: usize = 8;
/// And at most this many characters, whichever bound bites first.
pub const MAX_CHARS: usize = 1600;

const DAY_MS: u64 = 86_400_000;

fn s<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

fn age_days(v: &Value, now_ms: u64) -> Option<u64> {
    let at = v.get("recorded_at_ms").and_then(Value::as_u64)?;
    Some(now_ms.saturating_sub(at) / DAY_MS)
}

fn ago(days: Option<u64>) -> String {
    match days {
        Some(0) => "today".to_string(),
        Some(1) => "yesterday".to_string(),
        Some(d) => format!("{d} days ago"),
        None => "at an unknown time".to_string(),
    }
}

/// The last path segment, lowercased — what a diff stat or a summary would
/// plausibly name. Empty for URLs and synthetic ids, which must not match.
fn basename(path: &str) -> String {
    if path.contains("://") {
        return String::new();
    }
    path.rsplit('/').next().unwrap_or("").trim().to_lowercase()
}

/// The `shipped`/`reverted` fate per failure_id, LAST line winning — the
/// same recency rule `ship::already_actioned` uses, for the same reason: a
/// repair shipped, reverted, and shipped again is currently shipped.
fn fates(lines: &[(String, Value)]) -> HashMap<String, (&'static str, Option<u64>)> {
    let mut out: HashMap<String, (&'static str, Option<u64>)> = HashMap::new();
    for (kind, v) in lines {
        let verb = match kind.as_str() {
            "shipped" => "shipped",
            "reverted" => "reverted",
            _ => continue,
        };
        if let Some(fid) = s(v, "failure_id") {
            out.insert(
                fid.to_string(),
                (verb, v.get("recorded_at_ms").and_then(Value::as_u64)),
            );
        }
    }
    out
}

/// Recall for a detected failure: what the record holds about this error
/// name, this file, and the fate of past repairs that plausibly touched the
/// same ground.
pub fn for_failure(
    lines: &[(String, Value)],
    error_name: &str,
    top_frame_file: &str,
    now_ms: u64,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let file = basename(top_frame_file);
    let name = error_name.trim();
    let fate_by_failure = fates(lines);

    // 1. Reverted repairs whose summary or diff stat names this file or this
    //    error — the strongest warning the record can give.
    // 2. The same, shipped and still standing — precedent worth reading.
    for (kind, v) in lines.iter().rev() {
        if kind != "repair_ready" {
            continue;
        }
        let summary = s(v, "summary").unwrap_or("");
        let diff = s(v, "diff_stat").unwrap_or("");
        let hay = format!("{summary} {diff}").to_lowercase();
        let same_file = !file.is_empty() && hay.contains(&file);
        let same_error = !name.is_empty() && hay.contains(&name.to_lowercase());
        if !(same_file || same_error) {
            continue;
        }
        let why = if same_file {
            format!("names {file}")
        } else {
            format!("mentions {name}")
        };
        let fate = s(v, "failure_id").and_then(|fid| fate_by_failure.get(fid));
        let when = ago(age_days(v, now_ms));
        match fate {
            Some(("reverted", _)) => out.push(format!(
                "A past repair ({why}) was shipped and later REVERTED {when}: \"{}\". \
                 Whatever it tried did not hold — do not repeat it without \
                 understanding why it was rolled back.",
                truncate(summary, 140),
            )),
            Some(("shipped", _)) => out.push(format!(
                "A past repair ({why}) shipped {when} and stands: \"{}\".",
                truncate(summary, 140),
            )),
            _ => out.push(format!(
                "A past repair proposal ({why}) exists from {when}: \"{}\". It was \
                 never shipped.",
                truncate(summary, 140),
            )),
        }
        if out.len() >= 3 {
            break;
        }
    }

    // 3. How often this error has fired — event lines carry error_name
    //    top-level; that is a real join.
    let mut count = 0usize;
    let mut first: Option<u64> = None;
    for (kind, v) in lines {
        if kind == "event" && s(v, "error_name") == Some(name) && !name.is_empty() {
            count += 1;
            let at = v.get("recorded_at_ms").and_then(Value::as_u64);
            if first.is_none() {
                first = at;
            }
        }
    }
    if count > 1 {
        let since = ago(first.map(|f| now_ms.saturating_sub(f) / DAY_MS));
        out.push(format!(
            "{name} has produced {count} events on this record, the first {since} — \
             this is a repeat visitor, not a one-off."
        ));
    }

    bound(out)
}

/// Recall for a reported issue: repeated reports, and observed frustration
/// on any page the report names.
pub fn for_reported(
    lines: &[(String, Value)],
    title: &str,
    body: &str,
    now_ms: u64,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let hay = format!("{title} {body}").to_lowercase();

    // 1. Earlier reports with real word overlap in the title — the Nth
    //    report of something is itself information.
    let words: Vec<&str> = title
        .split_whitespace()
        .filter(|w| w.len() >= 5)
        .take(6)
        .collect();
    let mut earlier = 0usize;
    let mut first: Option<u64> = None;
    for (kind, v) in lines {
        if kind != "reported" {
            continue;
        }
        let t = s(v, "title").unwrap_or("").to_lowercase();
        let overlap = words
            .iter()
            .filter(|w| t.contains(&w.to_lowercase()))
            .count();
        if overlap >= 2 {
            earlier += 1;
            if first.is_none() {
                first = v.get("recorded_at_ms").and_then(Value::as_u64);
            }
        }
    }
    if earlier > 0 {
        let since = ago(first.map(|f| now_ms.saturating_sub(f) / DAY_MS));
        out.push(format!(
            "{earlier} earlier report(s) on this record share this title's words, the \
             first {since} — read them before assuming this is new."
        ));
    }

    // 2. Frustration observations whose route appears in the report's text.
    for (kind, v) in lines.iter().rev() {
        if kind != "observation" {
            continue;
        }
        let fact = v
            .pointer("/fact/kind")
            .and_then(Value::as_str)
            .unwrap_or("");
        if fact != "rage_click" && fact != "dead_click" {
            continue;
        }
        let path = v
            .pointer("/fact/path")
            .and_then(Value::as_str)
            .unwrap_or("");
        if path.is_empty() || !hay.contains(&path.to_lowercase()) {
            continue;
        }
        let clicks = v
            .pointer("/fact/clicks")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let what = if fact == "rage_click" {
            "rage clicks"
        } else {
            "dead clicks"
        };
        out.push(format!(
            "The record observed {what} on {path} ({clicks} clicks, {}) — the report \
             names the same page.",
            ago(age_days(v, now_ms)),
        ));
        if out.len() >= MAX_LINES {
            break;
        }
    }

    bound(out)
}

/// Enforce both caps, most valuable (earliest-pushed) first.
fn bound(mut lines: Vec<String>) -> Vec<String> {
    lines.truncate(MAX_LINES);
    let mut total = 0usize;
    lines.retain(|l| {
        total += l.len();
        total <= MAX_CHARS
    });
    lines
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let cut: String = text.chars().take(max).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const NOW: u64 = 1_755_500_000_000;
    const DAY: u64 = 86_400_000;

    fn repair(fid: &str, summary: &str, diff: &str, at: u64) -> (String, Value) {
        (
            "repair_ready".to_string(),
            json!({ "failure_id": fid, "summary": summary, "diff_stat": diff, "recorded_at_ms": at }),
        )
    }

    #[test]
    fn a_reverted_repair_on_the_same_file_is_the_loudest_memory() {
        let lines = vec![
            repair(
                "f1",
                "guard the pricing lookup in checkout.py",
                "api/checkout.py | 4 +-",
                NOW - 9 * DAY,
            ),
            (
                "reverted".to_string(),
                json!({ "failure_id": "f1", "recorded_at_ms": NOW - 8 * DAY }),
            ),
        ];
        let got = for_failure(&lines, "KeyError", "api/checkout.py", NOW);
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].contains("REVERTED"), "{}", got[0]);
        assert!(got[0].contains("checkout.py"));
        assert!(got[0].contains("9 days ago"));
    }

    #[test]
    fn shipped_and_standing_reads_as_precedent_not_warning() {
        let lines = vec![
            repair(
                "f2",
                "retry the webhook post",
                "src/hooks.ts | 9 ++",
                NOW - 3 * DAY,
            ),
            (
                "shipped".to_string(),
                json!({ "failure_id": "f2", "recorded_at_ms": NOW - 2 * DAY }),
            ),
        ];
        let got = for_failure(&lines, "TimeoutError", "src/hooks.ts", NOW);
        assert_eq!(got.len(), 1);
        assert!(got[0].contains("stands"), "{}", got[0]);
        assert!(!got[0].contains("REVERTED"));
    }

    #[test]
    fn shipped_then_reverted_then_shipped_again_reads_as_shipped() {
        // The ship module's recency rule, held here too.
        let lines = vec![
            repair(
                "f3",
                "fix parse in loader.js",
                "loader.js | 2 +-",
                NOW - 6 * DAY,
            ),
            (
                "reverted".to_string(),
                json!({ "failure_id": "f3", "recorded_at_ms": NOW - 5 * DAY }),
            ),
            (
                "shipped".to_string(),
                json!({ "failure_id": "f3", "recorded_at_ms": NOW - DAY }),
            ),
        ];
        let got = for_failure(&lines, "SyntaxError", "app/loader.js", NOW);
        assert!(got[0].contains("stands"), "{}", got[0]);
    }

    #[test]
    fn an_unrelated_repair_is_not_recalled() {
        let lines = vec![repair(
            "f4",
            "unrelated fix in billing.go",
            "billing.go | 3 +",
            NOW - DAY,
        )];
        let got = for_failure(&lines, "KeyError", "api/checkout.py", NOW);
        assert!(got.is_empty(), "{got:?}");
    }

    #[test]
    fn a_repeating_error_name_is_counted_with_its_first_sighting() {
        let mut lines: Vec<(String, Value)> = (0..5)
            .map(|i| {
                (
                    "event".to_string(),
                    json!({ "error_name": "KeyError", "recorded_at_ms": NOW - (10 - i) * DAY }),
                )
            })
            .collect();
        lines.push((
            "event".to_string(),
            json!({ "error_name": "Other", "recorded_at_ms": NOW }),
        ));
        let got = for_failure(&lines, "KeyError", "", NOW);
        assert_eq!(got.len(), 1);
        assert!(got[0].contains("5 events"), "{}", got[0]);
        assert!(got[0].contains("10 days ago"));
    }

    #[test]
    fn one_event_says_nothing() {
        let lines = vec![(
            "event".to_string(),
            json!({ "error_name": "KeyError", "recorded_at_ms": NOW }),
        )];
        assert!(for_failure(&lines, "KeyError", "", NOW).is_empty());
    }

    #[test]
    fn a_url_shaped_frame_file_matches_nothing() {
        // Reported-issue synthetic contexts put the issue URL in
        // top_frame_file; a URL must not text-match its way into recall.
        let lines = vec![repair(
            "f5",
            "fix https handling",
            "net.rs | 1 +",
            NOW - DAY,
        )];
        let got = for_failure(&lines, "", "https://linear.app/team/DRM-9", NOW);
        assert!(got.is_empty(), "{got:?}");
    }

    #[test]
    fn repeated_reports_are_counted_by_word_overlap() {
        let lines = vec![
            (
                "reported".to_string(),
                json!({ "title": "checkout button broken on mobile", "recorded_at_ms": NOW - 12 * DAY }),
            ),
            (
                "reported".to_string(),
                json!({ "title": "mobile checkout still broken", "recorded_at_ms": NOW - 4 * DAY }),
            ),
            (
                "reported".to_string(),
                json!({ "title": "dark mode looks wrong", "recorded_at_ms": NOW - DAY }),
            ),
        ];
        let got = for_reported(&lines, "checkout broken again on mobile safari", "", NOW);
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].contains("2 earlier report(s)"), "{}", got[0]);
        assert!(got[0].contains("12 days ago"));
    }

    #[test]
    fn frustration_on_a_page_the_report_names_is_recalled() {
        let lines = vec![(
            "observation".to_string(),
            json!({
                "id": "obs_rage_1", "recorded_at_ms": NOW - 2 * DAY,
                "fact": { "kind": "rage_click", "path": "/pricing", "clicks": 4 },
            }),
        )];
        let got = for_reported(
            &lines,
            "the /pricing page feels stuck",
            "clicking does nothing",
            NOW,
        );
        assert_eq!(got.len(), 1);
        assert!(got[0].contains("rage clicks on /pricing"), "{}", got[0]);
    }

    #[test]
    fn the_caps_hold_and_earliest_pushed_wins() {
        let lines: Vec<(String, Value)> = (0..40)
            .map(|i| {
                repair(
                    &format!("f{i}"),
                    &format!("fix number {i} in checkout.py {}", "x".repeat(120)),
                    "checkout.py | 1 +",
                    NOW - DAY,
                )
            })
            .collect();
        let got = for_failure(&lines, "KeyError", "checkout.py", NOW);
        assert!(got.len() <= MAX_LINES);
        assert!(got.iter().map(String::len).sum::<usize>() <= MAX_CHARS);
        assert!(!got.is_empty());
    }
}
