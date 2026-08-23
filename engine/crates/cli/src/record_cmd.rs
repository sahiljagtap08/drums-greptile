//! `drums record` (spec §9 "the record"): a read-only terminal view over
//! `.drums/record.jsonl`, newest first. The record has been append-only and
//! write-only since Stage 1 — this is the first thing that reads it back, so
//! Drums can actually be seen to remember what it has already handled.
//!
//! Pure rendering (`render_lines`/`render_json`) is separated from the small
//! amount of IO (`load`) the same way `render.rs` separates narration from
//! `main.rs`'s wiring, so the rendering is unit-testable against fixture
//! record files without spinning up a real `drums watch`.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use engine_core::Provenance;
use serde_json::Value;

use crate::render::{chip_ansi, sanitize, short_sha, DIM, RESET};

/// One decoded record line: the `kind` and `recorded_at_ms` engine-record
/// stamps on every line, plus the full JSON `value` (still carrying `kind`/
/// `recorded_at_ms` too — kept whole so `--json` can hand it back unedited).
#[derive(Debug, Clone)]
pub struct Entry {
    pub kind: String,
    pub recorded_at_ms: u64,
    pub value: Value,
}

/// `SystemTime::now()` as epoch milliseconds. Never fails the command over
/// an unreadable clock (unlike `engine.rs`'s own `now_ms()`, which refuses
/// to *write* a record line with a fabricated timestamp) — this is a
/// read-only display command; the worst case of a clock read failing here
/// is a wrong "when" column, not a corrupted compliance artifact, so
/// `unwrap_or(0)` (which would just render every "when" as "a very long
/// time ago") is the honest trade-off, not a silent lie.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Read `record_path`, newest line first.
///
/// Distinguishes "the file does not exist" from "the file exists and is
/// empty": `engine_record::read_all`'s own contract treats a missing file as
/// a clean empty read (correct for the *engine*, which must not error out of
/// `drums watch` startup just because nothing has been recorded yet) — but a
/// human running `drums record`/`drums why` against the wrong `--repo`, or
/// before ever running `drums watch` there, needs to be told plainly that
/// there is nothing to read AT ALL, not shown a silent empty table. So this
/// checks existence itself, before delegating to `read_all` for the
/// torn-line-tolerant parse of a file that does exist.
pub fn load(record_path: &Path) -> Result<(Vec<Entry>, usize), String> {
    if !record_path.exists() {
        return Err(format!(
            "no record found at {} — `drums watch` has not run against this repo yet",
            record_path.display()
        ));
    }
    let read = engine_record::read_all(record_path).map_err(|e| {
        format!(
            "could not read the record at {}: {e}",
            record_path.display()
        )
    })?;
    let mut entries: Vec<Entry> = read
        .lines
        .into_iter()
        .map(|(kind, value)| {
            let recorded_at_ms = value
                .get("recorded_at_ms")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            Entry {
                kind,
                recorded_at_ms,
                value,
            }
        })
        .collect();
    // The file is append-only in write order (oldest first on disk);
    // reversing gives newest-first without assuming anything stronger about
    // `recorded_at_ms` ordering than "the engine stamps each line when it
    // writes it".
    entries.reverse();
    Ok((entries, read.skipped))
}

/// Keep only entries recorded within `since_ms` of `now_ms`. A line recorded
/// AFTER `now_ms` (clock skew) is kept, not dropped — `saturating_sub` never
/// makes a recent line look old enough to filter out.
pub fn filter_since(entries: Vec<Entry>, now_ms: u64, since_ms: u64) -> Vec<Entry> {
    entries
        .into_iter()
        .filter(|e| now_ms.saturating_sub(e.recorded_at_ms) <= since_ms)
        .collect()
}

pub fn apply_limit(mut entries: Vec<Entry>, limit: Option<usize>) -> Vec<Entry> {
    if let Some(n) = limit {
        entries.truncate(n);
    }
    entries
}

/// Parses `--since` values of the shape the spec names: a plain integer
/// followed by one of `s`/`m`/`h`/`d` (seconds/minutes/hours/days) — `24h`,
/// `7d`, `30m`, `45s`. No decimals, no compound durations (`1h30m`): the
/// small vocabulary the command line names is the whole contract.
pub fn parse_duration_ms(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err(
            "invalid --since value \"\": expected a number followed by s/m/h/d, e.g. 24h or 7d"
                .to_string(),
        );
    }
    let split_at = s.len() - s.chars().last().map(|c| c.len_utf8()).unwrap_or(1);
    let (num, unit) = s.split_at(split_at);
    let n: u64 = num.parse().map_err(|_| {
        format!(
            "invalid --since value {s:?}: expected a number followed by s/m/h/d, e.g. 24h or 7d"
        )
    })?;
    let ms_per_unit: u64 = match unit {
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        other => {
            return Err(format!(
                "invalid --since unit {other:?} in {s:?}: expected one of s/m/h/d"
            ))
        }
    };
    // HIGH fix-round: `n * ms_per_unit` overflow-panicked (exit 101,
    // engine/Cargo.toml sets no release `overflow-checks`, so a release
    // build silently WRAPPED instead — a confidently wrong window, which is
    // worse than a crash) on a value like `999999999999d`. A duration that
    // doesn't fit in a `u64` of milliseconds is a typed refusal, exactly
    // like every other malformed `--since` shape this function already
    // refuses, not a panic and not a silently wrong window.
    n.checked_mul(ms_per_unit)
        .ok_or_else(|| format!("invalid --since value {s:?}: duration is too large to represent"))
}

/// `pub(crate)`: reused by `digest.rs` (the morning message) so "ready 4h
/// ago" / "reverted 8h ago" style relative timestamps have one definition,
/// not a second drifting copy.
pub(crate) fn relative_when(recorded_at_ms: u64, now_ms: u64) -> String {
    let secs = now_ms.saturating_sub(recorded_at_ms) / 1000;
    if secs < 5 {
        "just now".to_string()
    } else if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3_600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3_600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

/// What a claim's `provenance` field decodes to, tolerating anything that
/// isn't one of the five known chip names by simply not counting it — a
/// record line from a future version of the binary must never crash this
/// one.
fn parse_provenance(s: &str) -> Option<Provenance> {
    match s {
        "verified" => Some(Provenance::Verified),
        "observed" => Some(Provenance::Observed),
        "inferred" => Some(Provenance::Inferred),
        "approved" => Some(Provenance::Approved),
        "unresolved" => Some(Provenance::Unresolved),
        _ => None,
    }
}

/// One provenance chip standing in for a `claims: [...]` array: `unresolved`
/// if ANY claim is unresolved — the same whole-verify-fails-on-any-miss
/// discipline `engine.rs`'s `verify_repair` already applies when EARNING the
/// claims, restated here for how they're SHOWN — otherwise the first claim's
/// own provenance. `None` when there are no claims to summarize at all.
fn aggregate_claims(value: &Value) -> Option<Provenance> {
    let claims = value.get("claims")?.as_array()?;
    let mut first = None;
    for c in claims {
        let Some(p) = c
            .get("provenance")
            .and_then(|v| v.as_str())
            .and_then(parse_provenance)
        else {
            continue;
        };
        if p == Provenance::Unresolved {
            return Some(Provenance::Unresolved);
        }
        if first.is_none() {
            first = Some(p);
        }
    }
    first
}

#[derive(Debug)]
struct Described {
    text: String,
    chip: Option<Provenance>,
}

/// A row for a record line whose `kind` this binary recognizes but whose
/// fields don't match the shape that `kind` promises (missing, or present
/// with the wrong JSON type). Carries no chip: a provenance chip on a line
/// whose every field was unreadable would present unreadable data as fact,
/// which is worse than dropping it (MEDIUM fix-round finding). `kind` is
/// still named and sanitized — a malformed line must still be visible in
/// the narration, just never blank-filled into a confident sentence.
fn malformed_row(kind: &str) -> Described {
    Described {
        text: format!("malformed {} line (fields unreadable)", sanitize(kind)),
        chip: None,
    }
}

/// One human sentence per record `kind`, matching the product's own
/// vocabulary for each stage (spec §7). `None` means "not a narrated line" —
/// currently only `repair_context`, the redacted-request companion line
/// `engine.rs` writes alongside `repair_ready` purely so a standalone
/// `drums ship` can find the original request; it carries no fact a human
/// needs restated here. Any OTHER kind (including one this binary doesn't
/// know about yet) still gets a line — `"{kind} recorded"` — rather than
/// being silently dropped: an unrecognized kind is exactly the case that
/// must not vanish from the record's own narration of itself.
///
/// Every string pulled out of `value` is run through
/// [`crate::render::sanitize`] before it is ever interpolated into the
/// returned sentence — every field here is attacker- or LLM-controlled
/// (HIGH fix-round: terminal-escape injection could otherwise forge a green
/// `[verified]` chip via a crafted `error_name`, or use `\r` to overwrite
/// the line). And every field a `kind`'s sentence actually needs is
/// required to be present and string-typed, or the whole line falls back to
/// [`malformed_row`] rather than silently blank-filling missing/wrong-typed
/// fields into a sentence that reads as confident fact (MEDIUM fix-round).
fn describe(kind: &str, value: &Value) -> Option<Described> {
    let f = |field: &str| value.get(field).and_then(|v| v.as_str()).map(sanitize);
    let req = |field: &str| {
        value
            .pointer(&format!("/request/{field}"))
            .and_then(|v| v.as_str())
            .map(sanitize)
    };
    match kind {
        "deploy" => {
            let (Some(sha), Some(desc), Some(author)) = (f("sha"), f("description"), f("author"))
            else {
                return Some(malformed_row(kind));
            };
            Some(Described {
                text: format!("deploy {} \u{2014} \"{desc}\" by {author}", short_sha(&sha)),
                chip: None,
            })
        }
        "event" => {
            let (Some(error_name), Some(service)) = (f("error_name"), f("service")) else {
                return Some(malformed_row(kind));
            };
            // Request-borne events narrate their request; trigger-intake
            // events (drums init's self-check, external triggers) have no
            // request by design and narrate their source instead — the
            // record's first line must not read "malformed" (audit R1).
            match (req("method"), req("path")) {
                (Some(method), Some(path)) => Some(Described {
                    text: format!("{error_name} reported in {service} ({method} {path})"),
                    chip: Some(Provenance::Observed),
                }),
                _ => {
                    let source = value
                        .pointer("/intake/source")
                        .and_then(|v| v.as_str())
                        .map(sanitize);
                    let Some(source) = source else {
                        return Some(malformed_row(kind));
                    };
                    Some(Described {
                        text: format!("{error_name} reported in {service} (triggered by {source})"),
                        chip: Some(Provenance::Observed),
                    })
                }
            }
        }
        "repair_ready" => {
            let (Some(failure_id), Some(agent), Some(summary)) =
                (f("failure_id"), f("agent"), f("summary"))
            else {
                return Some(malformed_row(kind));
            };
            Some(Described {
                text: format!(
                    "repair ready for failure {failure_id} \u{2014} {agent}: \"{summary}\""
                ),
                chip: aggregate_claims(value),
            })
        }
        "shipped" => {
            let (Some(failure_id), Some(sha)) = (f("failure_id"), f("repair_sha")) else {
                return Some(malformed_row(kind));
            };
            Some(Described {
                text: format!(
                    "shipped repair for failure {failure_id} (sha {})",
                    short_sha(&sha)
                ),
                chip: aggregate_claims(value),
            })
        }
        "reverted" => {
            let (Some(failure_id), Some(sha)) = (f("failure_id"), f("repair_sha")) else {
                return Some(malformed_row(kind));
            };
            Some(Described {
                text: format!(
                    "reverted failure {failure_id} (rolled back to {})",
                    short_sha(&sha)
                ),
                chip: aggregate_claims(value),
            })
        }
        "repair_context" => None,
        "observation" => {
            let Some(id) = f("id") else {
                return Some(malformed_row(kind));
            };
            let fact = value
                .pointer("/fact/kind")
                .and_then(|v| v.as_str())
                .map(sanitize);
            let Some(fact) = fact else {
                return Some(malformed_row(kind));
            };
            let text = match fact.as_str() {
                "rate_shift" => {
                    let now = value
                        .pointer("/measure/sample/value")
                        .and_then(|v| v.as_f64())
                        .map(|v| format!(" to {v:.2}/h"))
                        .unwrap_or_default();
                    let prev = value
                        .pointer("/fact/previous")
                        .and_then(|v| v.as_f64())
                        .map(|v| format!(" from {v:.2}/h"))
                        .unwrap_or_default();
                    let since = value
                        .pointer("/fact/since_deploy")
                        .and_then(|v| v.as_str())
                        .map(|sha| format!(" since deploy {}", short_sha(&sanitize(sha))))
                        .unwrap_or_default();
                    format!("observation {id} — error-event rate shifted{prev}{now}{since}")
                }
                "rage_click" | "dead_click" => {
                    let path = value
                        .pointer("/fact/path")
                        .and_then(|v| v.as_str())
                        .filter(|p| !p.is_empty())
                        .map(sanitize)
                        .unwrap_or_else(|| "an unknown page".into());
                    let clicks = value
                        .pointer("/fact/clicks")
                        .and_then(|v| v.as_u64())
                        .map(|c| format!(" ({c} clicks"))
                        .unwrap_or_else(|| " (clicks".into());
                    let sessions = value
                        .pointer("/affected/sessions")
                        .and_then(|v| v.as_u64())
                        .map(|s| format!(", {s} sessions)"))
                        .unwrap_or_else(|| ")".into());
                    let what = if fact == "rage_click" {
                        "rage clicks"
                    } else {
                        "dead clicks"
                    };
                    format!("observation {id} — {what} on {path}{clicks}{sessions}")
                }
                other_fact => format!("observation {id} — {other_fact}"),
            };
            Some(Described {
                text,
                chip: Some(Provenance::Observed),
            })
        }
        "observation_status" => {
            let (Some(id), Some(status)) = (f("observation"), f("status")) else {
                return Some(malformed_row(kind));
            };
            let by = f("hypothesis")
                .map(|h| format!(" by {h}"))
                .unwrap_or_default();
            Some(Described {
                text: format!("observation {id} — {status}{by}"),
                chip: None,
            })
        }
        "hypothesis" => {
            let (Some(id), Some(statement)) = (f("id"), f("statement")) else {
                return Some(malformed_row(kind));
            };
            let cites = value
                .get("cites")
                .and_then(|v| v.as_array())
                .map(|c| c.len())
                .unwrap_or(0);
            Some(Described {
                text: format!("hypothesis {id} — \"{statement}\" (cites {cites})"),
                chip: Some(Provenance::Inferred),
            })
        }
        "hypothesis_status" => {
            let (Some(id), Some(status)) = (f("hypothesis"), f("status")) else {
                return Some(malformed_row(kind));
            };
            let reason = f("reason")
                .map(|r| format!(": \"{r}\""))
                .unwrap_or_default();
            Some(Described {
                text: format!("hypothesis {id} {status}{reason}"),
                chip: None,
            })
        }
        "bet" => {
            let (Some(id), Some(belief)) = (f("id"), f("belief")) else {
                return Some(malformed_row(kind));
            };
            Some(Described {
                text: format!("bet {id} — \"{belief}\""),
                chip: Some(Provenance::Inferred),
            })
        }
        "bet_status" => {
            let (Some(id), Some(status)) = (f("bet"), f("status")) else {
                return Some(malformed_row(kind));
            };
            let detail = match status.as_str() {
                "declined" => f("reason")
                    .map(|r| format!(": \"{r}\""))
                    .unwrap_or_default(),
                "evaluated" => value
                    .pointer("/verdict/support")
                    .and_then(|v| v.as_str())
                    .map(|w| format!(" — {}", sanitize(w).replace('_', " ")))
                    .unwrap_or_default(),
                "learned" => f("note").map(|n| format!(": \"{n}\"")).unwrap_or_default(),
                _ => String::new(),
            };
            Some(Described {
                text: format!("bet {id} {status}{detail}"),
                chip: None,
            })
        }
        "change" => {
            let (Some(id), Some(sha), Some(hyp)) = (f("id"), f("sha"), f("hypothesis")) else {
                return Some(malformed_row(kind));
            };
            Some(Described {
                text: format!("change {id} at {} acts on {hyp}", short_sha(&sha)),
                chip: None,
            })
        }
        "outcome" => {
            let (Some(chg), Some(state)) = (f("change"), f("state")) else {
                return Some(malformed_row(kind));
            };
            match state.as_str() {
                "measured" => {
                    let (from, to) = (
                        value.get("from").and_then(|v| v.as_f64()),
                        value.get("to").and_then(|v| v.as_f64()),
                    );
                    let direction = f("direction").unwrap_or_default();
                    let nums = match (from, to) {
                        (Some(a), Some(b)) => format!(" {a:.2} → {b:.2}"),
                        _ => String::new(),
                    };
                    Some(Described {
                        text: format!("outcome for {chg} — {direction}{nums}"),
                        // The one chip that means a comparison was actually
                        // readable — mirrors the Loop page's rule.
                        chip: Some(Provenance::Verified),
                    })
                }
                _ => Some(Described {
                    text: format!("outcome for {chg} — shipped, outcome unmeasured"),
                    chip: None,
                }),
            }
        }
        "revisit" => {
            let (Some(chg), Some(state)) = (f("change"), f("state")) else {
                return Some(malformed_row(kind));
            };
            let at = value
                .get("horizon_days")
                .and_then(|v| v.as_u64())
                .map(|h| format!(" at {h}d"))
                .unwrap_or_default();
            match state.as_str() {
                "measured" => {
                    let (from, to) = (
                        value.get("from").and_then(|v| v.as_f64()),
                        value.get("to").and_then(|v| v.as_f64()),
                    );
                    let direction = f("direction").unwrap_or_default();
                    let nums = match (from, to) {
                        (Some(a), Some(b)) => format!(" {a:.2} → {b:.2}"),
                        _ => String::new(),
                    };
                    Some(Described {
                        text: format!("revisit for {chg}{at} — {direction}{nums}"),
                        // Same rule as the outcome row: `verified` only when
                        // the later comparison was actually readable.
                        chip: Some(Provenance::Verified),
                    })
                }
                _ => Some(Described {
                    text: format!("revisit for {chg}{at} — the later window was unmeasured"),
                    chip: None,
                }),
            }
        }
        "authority_rung" => {
            let (Some(class), Some(rung)) = (f("class"), f("rung")) else {
                return Some(malformed_row(kind));
            };
            Some(Described {
                text: format!("authority: {class} → {rung}"),
                chip: None,
            })
        }
        other => Some(Described {
            text: format!("{} recorded", sanitize(other)),
            chip: None,
        }),
    }
}

fn chip_text(p: Provenance, use_color: bool) -> String {
    if use_color {
        format!(" {}[{}]{RESET}", chip_ansi(p), p.chip())
    } else {
        format!(" [{}]", p.chip())
    }
}

/// The default (non-`--json`) rendering: one line per narrated entry,
/// newest-first, `when · what happened · chip`, plus an honest disclosure of
/// how many lines in the underlying file could not be read at all.
///
/// `total_loaded` is `entries.len()` as returned by [`load`], BEFORE
/// `--since`/`--limit` narrowed it down to the `entries` actually passed
/// here — the only way to tell apart the three distinct reasons nothing
/// gets narrated (HIGH fix-round: all three used to collapse into the same
/// "no history yet — nothing has been recorded." claim, which is simply
/// false in two of the three cases):
///
/// 1. the record itself is empty (`total_loaded == 0`, nothing was ever
///    recorded) — the only case that actually says "no history yet";
/// 2. the record has lines, but `--since`/`--limit` narrowed the requested
///    window down to none of them (`entries.is_empty()` while
///    `total_loaded > 0`) — a filter answer, not an empty-record answer;
/// 3. the record has matching lines, but every one of them is a kind this
///    view doesn't narrate (currently only `repair_context`) — an honest
///    "hidden, not absent" disclosure, not a claim that nothing happened.
pub fn render_lines(
    entries: &[Entry],
    skipped: usize,
    total_loaded: usize,
    now_ms: u64,
    use_color: bool,
) -> String {
    let mut out = String::new();
    let mut shown = 0usize;
    let mut internal = 0usize;
    for e in entries {
        match describe(&e.kind, &e.value) {
            Some(d) => {
                shown += 1;
                let when = relative_when(e.recorded_at_ms, now_ms);
                let chip = d.chip.map(|p| chip_text(p, use_color)).unwrap_or_default();
                if use_color {
                    out.push_str(&format!("{DIM}{when:<8}{RESET}  {}{chip}\n", d.text));
                } else {
                    out.push_str(&format!("{when:<8}  {}{chip}\n", d.text));
                }
            }
            None => internal += 1,
        }
    }
    if shown == 0 {
        if total_loaded == 0 {
            // Case 1. When the record couldn't be read at all (`skipped >
            // 0`, zero good lines), the skip disclosure below already says
            // so honestly — claiming "nothing has been recorded" on top of
            // that would contradict it (something WAS recorded; it just
            // couldn't be read).
            if skipped == 0 {
                out.push_str("no history yet \u{2014} nothing has been recorded.\n");
            }
        } else if entries.is_empty() {
            // Case 2.
            out.push_str(&format!(
                "no record lines match the given --since/--limit filters ({total_loaded} recorded in total).\n"
            ));
        } else {
            // Case 3.
            let plural = if internal == 1 { "" } else { "s" };
            out.push_str(&format!(
                "{internal} internal record line{plural} hidden (nothing else to show).\n"
            ));
        }
    }
    if skipped > 0 {
        let plural = if skipped == 1 { "" } else { "s" };
        out.push_str(&format!(
            "\n{skipped} unreadable line{plural} skipped (torn or corrupt).\n"
        ));
    }
    out
}

/// Strips `request.body` out of one record entry before it is ever handed
/// to `--json`. CRITICAL fix-round: `render_json` used to hand back `value`
/// whole, so `event` and `repair_context` lines printed `request.body`
/// verbatim — and `engine-record`'s own redaction deliberately leaves prose
/// and malformed-JSON bodies UNMASKED at capture time (see
/// `engine_record::redact_body`'s module doc, and the
/// `redact_body_malformed_json_with_equals_in_string_value_passes_through_byte_identical`
/// test, which pins a 16-digit card number surviving byte-identical). The
/// plain view (`describe`, above) never reads `request.body` at all, so it
/// was never at risk; `--json` needs the identical guarantee, structurally —
/// not by convention, and not only for the `kind`s this binary currently
/// recognizes. Removing the key at the one JSON path it can ever appear at
/// (`.request.body`) covers `event`, `repair_context`, and any future kind
/// that ends up carrying a `request` object, without needing a hand-written
/// projection per kind that a new field could silently bypass.
fn scrub_for_json(value: &Value) -> Value {
    let mut v = value.clone();
    if let Some(request) = v.get_mut("request").and_then(|r| r.as_object_mut()) {
        request.remove("body");
    }
    v
}

/// `--json`: every readable entry (including `repair_context`, and any kind
/// `describe` doesn't narrate) handed back, for scripting — curation is a
/// display concern of the plain view, not something `--json` should hide
/// data behind. `request.body` is the one exception ([`scrub_for_json`]):
/// unlike every other field, it can carry an unmasked raw HTTP request body
/// (card numbers, bearer tokens, SSNs in prose — see that function's doc),
/// and no scripting use case for `drums record --json` needs it back. The
/// skipped count still ships alongside it: a script piping this must be
/// able to see the same honesty disclosure a human reading the plain view
/// sees.
pub fn render_json(entries: &[Entry], skipped: usize) -> String {
    let arr: Vec<Value> = entries.iter().map(|e| scrub_for_json(&e.value)).collect();
    let obj = serde_json::json!({ "skipped": skipped, "entries": arr });
    serde_json::to_string_pretty(&obj).unwrap_or_else(|_| "{}".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_core::{CapturedRequest, Claim, DeployRecord, ErrorEvent, Repair, ShipOutcome};
    use std::io::Write;

    /// Audit R1/R2: the record narrates the loop's own objects — the
    /// product's first line (init's self-check) is not "malformed", and
    /// observations, hypotheses, bets, changes, and outcomes carry their ids
    /// and words instead of "{kind} recorded".
    #[test]
    fn the_loop_vocabulary_narrates_instead_of_kind_recorded() {
        let d = |kind: &str, v: serde_json::Value| describe(kind, &v).unwrap().text;

        // A trigger-intake event has no request — narrated by source.
        let init = serde_json::json!({
            "service": "drums-init-verify", "error_name": "DrumsWireCheck",
            "error_message": "x", "intake": {"kind": "trigger", "source": "drums init"}
        });
        let text = d("event", init);
        assert!(text.contains("triggered by drums init"), "{text}");
        assert!(!text.contains("malformed"), "{text}");

        let obs = serde_json::json!({
            "id": "obs_1",
            "fact": {"kind": "rate_shift", "previous": 0.107, "since_deploy": "abc1234def0"},
            "measure": {"metric": "error_event_rate", "sample": {"value": 0.31, "entries": 168}}
        });
        let text = d("observation", obs);
        assert!(
            text.contains("obs_1") && text.contains("0.11/h") && text.contains("0.31/h"),
            "{text}"
        );

        let text = d(
            "hypothesis",
            serde_json::json!({
                "id": "hyp_1", "statement": "the retry queue is the cause candidate", "cites": ["obs_1"]
            }),
        );
        assert!(
            text.contains("hyp_1") && text.contains("retry queue") && text.contains("cites 1"),
            "{text}"
        );

        let text = d(
            "bet",
            serde_json::json!({"id": "bet_1", "belief": "batching will cut the rate"}),
        );
        assert!(
            text.contains("bet_1") && text.contains("batching"),
            "{text}"
        );

        let text = d(
            "bet_status",
            serde_json::json!({
                "bet": "bet_1", "status": "evaluated",
                "verdict": {"support": "not_supported", "causal_confidence": {"level": "low", "basis": "b"}}
            }),
        );
        assert!(
            text.contains("not supported"),
            "underscores are for wires, not people: {text}"
        );

        let text = d(
            "change",
            serde_json::json!({
                "id": "chg_1", "sha": "abc1234def05678", "hypothesis": "hyp_1"
            }),
        );
        assert!(text.contains("chg_1") && text.contains("hyp_1"), "{text}");

        let measured = describe(
            "outcome",
            &serde_json::json!({
                "change": "chg_1", "state": "measured", "direction": "neutral",
                "from": 0.31, "to": 0.30, "entries": 168, "guardrails": "held"
            }),
        )
        .unwrap();
        assert!(
            measured.text.contains("neutral 0.31 → 0.30"),
            "{}",
            measured.text
        );
        assert_eq!(
            measured.chip,
            Some(Provenance::Verified),
            "measured is the only verified row"
        );
        let unmeasured = describe(
            "outcome",
            &serde_json::json!({
                "change": "chg_1", "state": "unmeasured", "reason": "not_enough_traffic",
                "entries": 3, "needed": 100
            }),
        )
        .unwrap();
        assert!(
            unmeasured.text.contains("shipped, outcome unmeasured"),
            "{}",
            unmeasured.text
        );
        assert_eq!(unmeasured.chip, None);
    }

    /// The slow loop's row: a measured revisit narrates its horizon,
    /// direction and both numbers with the verified chip; an unmeasured one
    /// says so plainly, chip-less — same rule as the outcome row above it.
    #[test]
    fn a_revisit_row_narrates_its_horizon_and_earns_verified_only_when_measured() {
        let measured = describe(
            "revisit",
            &serde_json::json!({
                "change": "chg_1", "horizon_days": 30, "measured_at_ms": 9,
                "state": "measured", "direction": "neutral",
                "from": 0.11, "to": 0.12, "entries": 168, "guardrails": "held"
            }),
        )
        .unwrap();
        assert!(
            measured
                .text
                .contains("revisit for chg_1 at 30d — neutral 0.11 → 0.12"),
            "{}",
            measured.text
        );
        assert_eq!(measured.chip, Some(Provenance::Verified));

        let unmeasured = describe(
            "revisit",
            &serde_json::json!({
                "change": "chg_1", "horizon_days": 90, "measured_at_ms": 9,
                "state": "unmeasured", "reason": "not_enough_traffic",
                "entries": 3, "needed": 100
            }),
        )
        .unwrap();
        assert!(
            unmeasured
                .text
                .contains("revisit for chg_1 at 90d — the later window was unmeasured"),
            "{}",
            unmeasured.text
        );
        assert_eq!(
            unmeasured.chip, None,
            "no comparison read, no verified chip"
        );

        // A hand-forged line missing its fields is malformed, never a panic.
        assert!(describe("revisit", &serde_json::json!({"change": "chg_1"}))
            .unwrap()
            .text
            .contains("malformed"));
    }

    fn claim(text: &str, p: Provenance) -> Claim {
        Claim {
            text: text.into(),
            provenance: p,
        }
    }

    fn write_mixed_fixture(path: &std::path::Path) {
        engine_record::append(
            path,
            "deploy",
            &DeployRecord {
                sha: "abc1234def".into(),
                description: "add promo code field".into(),
                author: "maya".into(),
                deployed_at_ms: 1_000,
            },
            1_000,
        )
        .unwrap();
        engine_record::append(
            path,
            "event",
            &ErrorEvent {
                service: "shop".into(),
                occurred_at_ms: 2_000,
                error_name: "TypeError".into(),
                error_message: "boom".into(),
                stack: "TypeError: boom\n    at computeTotal (/srv/shop/lib/cart/total.js:14:31)"
                    .into(),
                request: Some(CapturedRequest {
                    method: "POST".into(),
                    path: "/api/checkout".into(),
                    content_type: Some("application/json".into()),
                    body: Some(r#"{"card":"4242424242424242"}"#.into()),
                }),
                intake: engine_core::Intake::Snippet,
            },
            2_000,
        )
        .unwrap();
        engine_record::append(
            path,
            "repair_ready",
            &Repair {
                id: "r1".into(),
                failure_id: "f1".into(),
                sha: "deadbeef00".into(),
                branch: "drums/repair-f1".into(),
                agent: "claude".into(),
                summary: "fixed the promo guard".into(),
                diff_stat: "server.js | 1 +".into(),
                claims: vec![claim("now returns 200", Provenance::Verified)],
            },
            3_000,
        )
        .unwrap();
        engine_record::append(
            path,
            "shipped",
            &ShipOutcome {
                failure_id: "f1".into(),
                repair_sha: "deadbeef00".into(),
                action: "shipped".into(),
                deploy_cmd: "bash deploy.sh".into(),
                claims: vec![claim(
                    "deploy command ran; no post-deploy check configured",
                    Provenance::Unresolved,
                )],
            },
            4_000,
        )
        .unwrap();
    }

    #[test]
    fn render_lines_narrates_every_known_kind_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        write_mixed_fixture(&path);

        let (entries, skipped) = load(&path).unwrap();
        assert_eq!(skipped, 0);
        let total_loaded = entries.len();
        let out = render_lines(&entries, skipped, total_loaded, 100_000, false);

        // Newest-first: "shipped" (4_000) must appear before "deploy" (1_000).
        let shipped_pos = out.find("shipped repair").unwrap();
        let deploy_pos = out.find("deploy abc123").unwrap();
        assert!(
            shipped_pos < deploy_pos,
            "newest entry must render first: {out}"
        );

        assert!(out.contains("deploy abc123"), "{out}");
        assert!(out.contains("add promo code field"), "{out}");
        assert!(
            out.contains("TypeError reported in shop (POST /api/checkout)"),
            "{out}"
        );
        assert!(out.contains("[observed]"), "{out}");
        assert!(out.contains("repair ready for failure f1"), "{out}");
        assert!(out.contains("claude"), "{out}");
        assert!(out.contains("[verified]"), "{out}");
        assert!(out.contains("shipped repair for failure f1"), "{out}");
        assert!(out.contains("[unresolved]"), "{out}");

        // Never the raw card number, no matter what got redacted upstream —
        // this view must not even have a body field to leak from.
        assert!(!out.contains("4242424242424242"), "{out}");
    }

    /// HIGH fix-round: a record whose only line is `repair_context` used to
    /// render "no history yet — nothing has been recorded." — false; a line
    /// WAS recorded, it's just internal plumbing this view doesn't narrate.
    /// The corrected message must disclose that distinction instead.
    #[test]
    fn render_lines_hides_repair_context_but_honestly_discloses_it_as_hidden_not_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        engine_record::append(
            &path,
            "repair_context",
            &engine_core::RepairSample {
                failure_id: "f1".into(),
                request: CapturedRequest {
                    method: "POST".into(),
                    path: "/api/checkout".into(),
                    content_type: None,
                    body: None,
                },
            },
            1_000,
        )
        .unwrap();
        let (entries, skipped) = load(&path).unwrap();
        let total_loaded = entries.len();
        let out = render_lines(&entries, skipped, total_loaded, 100_000, false);
        assert!(!out.contains("repair_context"), "{out}");
        assert!(
            !out.contains("no history yet"),
            "a line WAS recorded; this must not claim nothing was recorded: {out}"
        );
        assert!(out.contains("1 internal record line hidden"), "{out}");
    }

    #[test]
    fn render_lines_narrates_an_unknown_future_kind_rather_than_dropping_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        engine_record::append(
            &path,
            "boundary_changed",
            &serde_json::json!({"detail": "x"}),
            1_000,
        )
        .unwrap();
        let (entries, skipped) = load(&path).unwrap();
        let total_loaded = entries.len();
        let out = render_lines(&entries, skipped, total_loaded, 100_000, false);
        assert!(out.contains("boundary_changed recorded"), "{out}");
    }

    #[test]
    fn render_lines_discloses_skipped_torn_lines_honestly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        {
            let mut f = std::fs::File::create(&path).unwrap();
            writeln!(f, r#"{{"kind":"deploy","recorded_at_ms":1000,"sha":"abc123","description":"c1","author":"t","deployed_at_ms":1000}}"#).unwrap();
            // torn line, no trailing newline
            write!(f, r#"{{"kind":"deploy","recorded_at_"#).unwrap();
        }
        let (entries, skipped) = load(&path).unwrap();
        assert_eq!(skipped, 1);
        let total_loaded = entries.len();
        let out = render_lines(&entries, skipped, total_loaded, 100_000, false);
        assert!(
            out.contains("1 unreadable line skipped (torn or corrupt)."),
            "{out}"
        );
        assert!(
            !out.contains("no history yet"),
            "there WAS a readable line; must not also claim nothing was recorded: {out}"
        );
    }

    #[test]
    fn render_lines_on_empty_file_says_so_plainly_with_no_skip_disclosure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        std::fs::File::create(&path).unwrap();
        let (entries, skipped) = load(&path).unwrap();
        assert_eq!(skipped, 0);
        let out = render_lines(&entries, skipped, 0, 100_000, false);
        assert!(out.contains("no history yet"), "{out}");
        assert!(!out.contains("unreadable"), "{out}");
    }

    /// HIGH fix-round: `--since`/`--limit` narrowing a non-empty record down
    /// to zero matching lines used to render the same "no history yet"
    /// claim as a genuinely empty record. That's false — the record has
    /// history, the filters just didn't match any of it.
    #[test]
    fn render_lines_distinguishes_no_filter_matches_from_a_genuinely_empty_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        write_mixed_fixture(&path);
        let (entries, skipped) = load(&path).unwrap();
        let total_loaded = entries.len();
        assert_eq!(total_loaded, 4);

        // --limit 0: nothing passed through, but the record is not empty.
        let limited = apply_limit(entries, Some(0));
        assert!(limited.is_empty());
        let out = render_lines(&limited, skipped, total_loaded, 100_000, false);
        assert!(
            !out.contains("no history yet"),
            "the record has 4 lines; this must not claim nothing was recorded: {out}"
        );
        assert!(out.contains("no record lines match"), "{out}");
        assert!(
            out.contains('4'),
            "the total-recorded count must be disclosed: {out}"
        );

        // --since window narrower than every entry: same distinction.
        let (entries2, _skipped) = load(&path).unwrap();
        let total_loaded2 = entries2.len();
        let filtered = filter_since(entries2, 100_000, 1); // 1ms window: nothing this old survives
        assert!(filtered.is_empty());
        let out2 = render_lines(&filtered, 0, total_loaded2, 100_000, false);
        assert!(!out2.contains("no history yet"), "{out2}");
        assert!(out2.contains("no record lines match"), "{out2}");
    }

    /// A whole-file torn record (nothing but unreadable lines) must not
    /// claim "no history yet — nothing has been recorded": something WAS
    /// recorded, it just couldn't be read. The skip disclosure alone must
    /// carry that story.
    #[test]
    fn render_lines_on_a_wholly_torn_record_does_not_claim_nothing_was_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        std::fs::write(&path, r#"{"kind":"deploy","recorded_at_"#).unwrap();
        let (entries, skipped) = load(&path).unwrap();
        assert_eq!(skipped, 1);
        assert!(entries.is_empty());
        let out = render_lines(&entries, skipped, entries.len(), 100_000, false);
        assert!(!out.contains("no history yet"), "{out}");
        assert!(out.contains("1 unreadable line skipped"), "{out}");
    }

    #[test]
    fn load_on_missing_file_is_an_honest_refusal_naming_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        let err =
            load(&path).expect_err("a missing record must be a refusal, not a silent empty read");
        assert!(err.contains(&path.display().to_string()), "{err}");
    }

    #[test]
    fn since_filter_keeps_only_entries_within_the_window() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        // recorded_at_ms: deploy=1_000, event=2_000, repair_ready=3_000, shipped=4_000
        write_mixed_fixture(&path);
        let (entries, _skipped) = load(&path).unwrap();
        // now = 5_000; a 2_000ms window keeps recorded_at_ms >= 3_000 —
        // repair_ready (3_000) and shipped (4_000) only.
        let filtered = filter_since(entries, 5_000, 2_000);
        assert_eq!(
            filtered.len(),
            2,
            "expected only repair_ready and shipped within the window"
        );
        assert!(
            filtered.iter().all(|e| e.recorded_at_ms >= 3_000),
            "{filtered:?}"
        );
    }

    #[test]
    fn limit_truncates_to_the_n_most_recent_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        write_mixed_fixture(&path);
        let (entries, _skipped) = load(&path).unwrap();
        let limited = apply_limit(entries, Some(2));
        assert_eq!(limited.len(), 2);
        assert_eq!(limited[0].kind, "shipped", "newest first");
        assert_eq!(limited[1].kind, "repair_ready");
    }

    #[test]
    fn parse_duration_accepts_the_documented_units() {
        assert_eq!(parse_duration_ms("24h").unwrap(), 24 * 3_600_000);
        assert_eq!(parse_duration_ms("7d").unwrap(), 7 * 86_400_000);
        assert_eq!(parse_duration_ms("30m").unwrap(), 30 * 60_000);
        assert_eq!(parse_duration_ms("45s").unwrap(), 45 * 1_000);
    }

    #[test]
    fn parse_duration_rejects_garbage_and_overflow_without_panicking() {
        assert!(parse_duration_ms("").is_err());
        assert!(parse_duration_ms("h").is_err());
        assert!(parse_duration_ms("24x").is_err());
        assert!(parse_duration_ms("abc").is_err());
        // HIGH fix-round: `n * ms_per_unit` used to overflow-panic (exit
        // 101 in debug; a release build with no `overflow-checks` set would
        // silently WRAP to an arbitrary window instead — a confidently
        // wrong view, not merely a crash). Both must now be typed refusals.
        assert!(
            parse_duration_ms("999999999999d").is_err(),
            "must be a typed refusal, not an overflow panic"
        );
        assert!(
            parse_duration_ms("18446744073709551615d").is_err(),
            "a u64::MAX-scale numeral must not panic either"
        );
    }

    #[test]
    fn render_json_includes_repair_context_and_the_skipped_count() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        engine_record::append(
            &path,
            "repair_context",
            &engine_core::RepairSample {
                failure_id: "f1".into(),
                request: CapturedRequest {
                    method: "POST".into(),
                    path: "/api/checkout".into(),
                    content_type: None,
                    body: None,
                },
            },
            1_000,
        )
        .unwrap();
        let (entries, skipped) = load(&path).unwrap();
        let out = render_json(&entries, skipped);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["skipped"], 0);
        assert_eq!(v["entries"][0]["kind"], "repair_context");
    }

    // -- CRITICAL fix-round: `--json` must never leak `request.body` -------------

    /// Reproduces the review finding verbatim: a card number in an `event`
    /// body, an SSN in prose, and a bearer token — none of which
    /// `engine-record`'s own redaction masks (prose/malformed-JSON bodies
    /// pass through unmasked by design; see `engine_record::redact_body`'s
    /// module doc). `--json` must never re-surface any of them, while still
    /// handing back every other field a script might need.
    #[test]
    fn render_json_never_leaks_request_body_even_when_upstream_redaction_left_it_unmasked() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        let bodies = [
            r#"{"card":"4242424242424242","note":"unrelated free text with a lone = in it"#, // malformed JSON: passes through byte-identical upstream
            "SSN 123-45-6789 leaked in prose",
            "authorization: Bearer sk-live-DEADBEEF1234567890",
        ];
        for (i, body) in bodies.iter().enumerate() {
            engine_record::append(
                &path,
                "event",
                &ErrorEvent {
                    service: "shop".into(),
                    occurred_at_ms: 1_000 + i as u64,
                    error_name: "TypeError".into(),
                    error_message: "boom".into(),
                    stack: "TypeError: boom\n    at f (/srv/shop/server.js:1:1)".into(),
                    request: Some(CapturedRequest {
                        method: "POST".into(),
                        path: "/api/checkout".into(),
                        content_type: Some("text/plain".into()),
                        body: Some((*body).to_string()),
                    }),
                    intake: engine_core::Intake::Snippet,
                },
                1_000 + i as u64,
            )
            .unwrap();
        }
        // A `repair_context` line carries the same `request` shape and must
        // be scrubbed identically.
        engine_record::append(
            &path,
            "repair_context",
            &engine_core::RepairSample {
                failure_id: "f1".into(),
                request: CapturedRequest {
                    method: "POST".into(),
                    path: "/api/checkout".into(),
                    content_type: None,
                    body: Some("card=4242424242424242".into()),
                },
            },
            2_000,
        )
        .unwrap();

        let (entries, skipped) = load(&path).unwrap();
        let out = render_json(&entries, skipped);

        assert!(
            !out.contains("4242424242424242"),
            "card number must never appear in --json: {out}"
        );
        assert!(
            !out.contains("123-45-6789"),
            "SSN must never appear in --json: {out}"
        );
        assert!(
            !out.contains("sk-live-DEADBEEF1234567890"),
            "bearer token must never appear in --json: {out}"
        );
        assert!(
            !out.contains("\"body\""),
            "the body key itself must be absent, not merely its value: {out}"
        );

        // Everything else a script might need is still there.
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["entries"][3]["error_name"], "TypeError");
        assert_eq!(v["entries"][3]["request"]["method"], "POST");
        assert_eq!(v["entries"][3]["request"]["path"], "/api/checkout");
        assert_eq!(v["entries"][0]["kind"], "repair_context");
        assert_eq!(v["entries"][0]["request"]["method"], "POST");
    }

    // -- MEDIUM fix-round: unreadable fields render as an honest "malformed" row --

    #[test]
    fn describe_reports_a_malformed_row_instead_of_a_blank_filled_sentence() {
        // `error_name` missing entirely, `service` present but the wrong
        // JSON type — exactly the shape `.unwrap_or("")` used to smuggle
        // into `"{error_name} reported in {service} (...)"` as a
        // confident-looking sentence with silent blanks.
        let value = serde_json::json!({
            "service": 42,
            "request": {"method": "POST", "path": "/api/checkout"}
        });
        let d = describe("event", &value).expect("a line for this kind must still be produced");
        assert_eq!(d.text, "malformed event line (fields unreadable)");
        assert!(
            d.chip.is_none(),
            "an unreadable line must carry no provenance chip: {d:?}"
        );
    }

    // -- HIGH fix-round: describe() must sanitize every attacker-controlled field --

    #[test]
    fn describe_strips_terminal_escapes_from_event_fields_before_they_reach_a_sentence() {
        let value = serde_json::json!({
            "error_name": "TypeError\u{1b}[32m [verified]\u{1b}[0m",
            "service": "shop",
            "request": {"method": "POST", "path": "/api/checkout"}
        });
        let d = describe("event", &value).unwrap();
        assert!(
            !d.text.contains('\u{1b}'),
            "escape byte must be stripped: {:?}",
            d.text
        );
        assert_eq!(
            d.chip,
            Some(Provenance::Observed),
            "the REAL chip is Observed — the fake one must not appear as text either"
        );
    }

    #[test]
    fn relative_when_covers_seconds_minutes_hours_days() {
        let now: u64 = 1_000_000_000_000;
        assert_eq!(relative_when(now - 2_000, now), "just now");
        assert_eq!(relative_when(now - 60_000, now), "1m ago");
        assert_eq!(relative_when(now - 2 * 3_600_000, now), "2h ago");
        assert_eq!(relative_when(now - 3 * 86_400_000, now), "3d ago");
    }
}
