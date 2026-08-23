//! `drums why <path>[:<line>]` (spec §17: "blame, except it knows about
//! production"): the production failure history for a source file, mined
//! from the record's own `event` lines — the only record kind that carries
//! a stack trace, and therefore the only honest source of "which file" for
//! a past production error. (`repair_ready`/`shipped`/`reverted` lines carry
//! a `failure_id`, not a file path — there is no persisted link back to a
//! specific file from those lines, so this deliberately does not guess at
//! one.)
//!
//! Matching reuses `engine_attribute::paths_overlap` — the exact
//! component-aligned suffix match `engine-attribute` uses to line up a
//! deployment-relative stack-trace path against a repo-root-relative git
//! path — rather than a second, drift-prone copy of that discipline.

use engine_core::{ErrorSignature, Provenance};
use serde::Deserialize;

use crate::record_cmd::Entry;
use crate::render::{chip_ansi, sanitize, DIM, RESET};

/// Whether `s` is non-empty and every character is an ASCII digit — the
/// shape of a raw line/column number lifted off a stack trace, before it's
/// checked to actually fit in a `u32`.
fn is_all_ascii_digits(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_digit())
}

/// Splits `arg` into `(path, line)`, understanding `path`, `path:LINE`, and
/// `path:LINE:COL` — the exact shape a JS stack trace prints, and the exact
/// shape stored in the record's own `stack` strings, e.g.
/// `lib/cart/total.js:14:31` (MEDIUM fix-round: this is precisely what a
/// human pastes straight out of a stack trace, and the old single-colon
/// split left `lib/cart/total.js:14` as the query — never matching the
/// bare-path key `find_history` actually indexes on, so `drums why` always
/// answered "no production history found" for the exact shape it should
/// handle best).
///
/// Trailing colon segments are peeled off the right as long as they are
/// all-ASCII-digit, up to two of them (`:LINE:COL`). The column, if present,
/// is discarded (matching is per-file, not per-line-or-column — see
/// [`render`]'s own note about that). A trailing segment that ISN'T
/// all-digit is left as part of the path, exactly as before (e.g. a
/// Windows-style drive letter — not that this CLI targets Windows).
///
/// A trailing segment that IS all-digit but too large to fit a `u32` is a
/// different case from "not a line number at all": it is unambiguously
/// *intended* as a line, just malformed, so it must be refused explicitly
/// (`Err`) rather than silently folded back into the path — where it can
/// never match any recorded stack-trace path and the command would answer a
/// confidently wrong "no production history found" instead of naming the
/// actual problem.
pub fn parse_path_arg(arg: &str) -> Result<(String, Option<u32>), String> {
    let mut path = arg;
    let mut digit_tails: Vec<&str> = Vec::new();
    for _ in 0..2 {
        let Some((head, tail)) = path.rsplit_once(':') else {
            break;
        };
        if !is_all_ascii_digits(tail) {
            break;
        }
        digit_tails.push(tail);
        path = head;
    }

    let Some(&line_str) = digit_tails.last() else {
        // No trailing all-digit colon segment at all — not this shape.
        return Ok((arg.to_string(), None));
    };

    match line_str.parse::<u32>() {
        Ok(line) => Ok((path.to_string(), Some(line))),
        Err(_) => Err(format!(
            "invalid line number {line_str:?} in {arg:?}: expected a value that fits in 32 bits"
        )),
    }
}

/// The fields of a recorded `event` line this command actually needs.
/// Deliberately omits `request.body` and `request.content_type` — not
/// merely "doesn't print" the body, but never deserializes it into memory in
/// the first place, so it is structurally impossible for a future edit to
/// this file to accidentally echo one.
#[derive(Deserialize)]
struct RecordedEvent {
    service: String,
    occurred_at_ms: u64,
    error_name: String,
    error_message: String,
    stack: String,
    /// Absent on a trigger/reported `event` line — an OTel span or a Linear
    /// issue records no replayable request (see `engine_core::Intake`). This is
    /// `Option` rather than required so such a line reads as a real occurrence
    /// with no request, instead of being counted `undecodable` and disclosed as
    /// "could not be read as a well-shaped event" — which would be a lie about a
    /// line that is perfectly well-shaped.
    #[serde(default)]
    request: Option<RecordedRequest>,
}

#[derive(Deserialize)]
struct RecordedRequest {
    method: String,
    path: String,
}

/// One production occurrence whose signature's top application frame lines
/// up with the requested file.
pub struct Occurrence {
    pub occurred_at_ms: u64,
    pub error_name: String,
    pub service: String,
    /// `None` when the recorded event carried no replayable request. Rendered as
    /// an explicit "no replayable request" rather than an empty pair of
    /// parentheses, which would read as a request whose method and path were
    /// blank.
    pub request: Option<(String, String)>,
}

/// Scans `entries` for `event` lines whose derived [`ErrorSignature`] names
/// `target` as its top application frame (component-aligned suffix match,
/// via `engine_attribute::paths_overlap` — see the module doc for why a bare
/// `ends_with` is the wrong tool here). `entries` is assumed already
/// newest-first (as `record_cmd::load` returns it), so the result is too.
///
/// `app_root` is unknown at this layer (it is a `drums watch` startup flag,
/// never persisted to the record), so signatures are derived with an empty
/// app root — the stack trace's own path, whatever the live process
/// captured it as. This is exactly why the match is a suffix match and not
/// an exact match: a recorded absolute path like `/srv/shop/lib/cart/total.js`
/// still lines up with a `--repo`-relative query like `lib/cart/total.js`.
///
/// Returns `(history, undecodable)`. MEDIUM fix-round: an `event` line whose
/// JSON doesn't deserialize into [`RecordedEvent`] used to be silently
/// `continue`d past with no counter at all — invisible both to a human (who
/// sees it in the plain `drums record` view, so it's readable) and to the
/// disclosed `skipped` count (which only tracks `record_cmd::load`'s
/// torn/corrupt-line count, a completely different failure). `why` then
/// answered "no production history found" with full confidence over a
/// record it had silently thrown part of away. `undecodable` closes that:
/// every `event` line this function could not read into a `RecordedEvent`
/// is counted so [`render`] can disclose it the same honest way `skipped`
/// already is.
pub fn find_history(entries: &[Entry], target: &str) -> (Vec<Occurrence>, usize) {
    let mut out = Vec::new();
    let mut undecodable = 0usize;
    for e in entries {
        if e.kind != "event" {
            continue;
        }
        let ev = match serde_json::from_value::<RecordedEvent>(e.value.clone()) {
            Ok(ev) => ev,
            Err(_) => {
                undecodable += 1;
                continue;
            }
        };
        let sig = ErrorSignature::from_error(&ev.error_name, &ev.error_message, &ev.stack, "");
        if sig.top_frame_file.is_empty() {
            continue;
        }
        if !engine_attribute::paths_overlap(&sig.top_frame_file, target) {
            continue;
        }
        out.push(Occurrence {
            occurred_at_ms: ev.occurred_at_ms,
            error_name: ev.error_name,
            service: ev.service,
            request: ev.request.map(|r| (r.method, r.path)),
        });
    }
    (out, undecodable)
}

/// Days-since-epoch -> proleptic Gregorian (year, month, day), UTC. Howard
/// Hinnant's `civil_from_days` algorithm (public domain,
/// http://howardhinnant.github.io/date_algorithms.html) — used instead of
/// pulling in a date/time crate for one output column.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

fn format_date_utc(ms: u64) -> String {
    let days = (ms / 86_400_000) as i64;
    let secs_of_day = (ms % 86_400_000) / 1000;
    let (y, m, d) = civil_from_days(days);
    let hh = secs_of_day / 3600;
    let mm = (secs_of_day % 3600) / 60;
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02} UTC")
}

/// "once" / "twice" / "N times" — matches the spec's own sample phrasing
/// ("this file has failed in production twice in 60 days").
fn times_word(n: usize) -> String {
    match n {
        1 => "once".to_string(),
        2 => "twice".to_string(),
        _ => format!("{n} times"),
    }
}

fn summary_line(history: &[Occurrence], now_ms: u64) -> String {
    let earliest = history
        .iter()
        .map(|o| o.occurred_at_ms)
        .min()
        .unwrap_or(now_ms);
    let days = now_ms.saturating_sub(earliest) / 86_400_000;
    if days == 0 {
        format!(
            "this file has failed in production {} today",
            times_word(history.len())
        )
    } else {
        let plural = if days == 1 { "" } else { "s" };
        format!(
            "this file has failed in production {} in {days} day{plural}",
            times_word(history.len())
        )
    }
}

/// The full rendered view: header, an honest note when `:<line>` was given
/// (matching is per-file — line-level history is not tracked, and this says
/// so rather than silently ignoring the line number or pretending to honor
/// it), the history newest-first, and the one-line summary. `skipped` — the
/// count of unreadable lines `record_cmd::load` reported — is disclosed the
/// same way `drums record` discloses it, since a `why` answer built on a
/// record with unreadable lines is answering from a known-incomplete view.
/// `undecodable` (MEDIUM fix-round) is [`find_history`]'s own count of
/// `event` lines it could read as JSON but not as a well-shaped event —
/// a distinct failure mode from `skipped`, disclosed the same honest way.
///
/// `target_display` and every occurrence field pulled off the record
/// (`error_name`/`service`/`method`/`path`) is run through
/// [`crate::render::sanitize`] before it reaches the format strings below —
/// HIGH fix-round: these are the identical attacker-controlled fields
/// `record_cmd::describe` had to sanitize, reached through a second command.
pub fn render(
    target_display: &str,
    requested_line: Option<u32>,
    history: &[Occurrence],
    skipped: usize,
    undecodable: usize,
    now_ms: u64,
    use_color: bool,
) -> String {
    let target_display = sanitize(target_display);
    let mut out = String::new();
    out.push_str(&format!("production history for {target_display}\n"));
    if let Some(line) = requested_line {
        let note = format!("matching is per-file \u{2014} line {line} is not tracked separately; showing all history for this file.");
        if use_color {
            out.push_str(&format!("{DIM}{note}{RESET}\n"));
        } else {
            out.push_str(&format!("{note}\n"));
        }
    }
    out.push('\n');

    if history.is_empty() {
        out.push_str(&format!(
            "no production history found for {target_display}\n"
        ));
    } else {
        for occ in history {
            let date = format_date_utc(occ.occurred_at_ms);
            let chip = if use_color {
                format!(" {}[observed]{RESET}", chip_ansi(Provenance::Observed))
            } else {
                " [observed]".to_string()
            };
            let request = match &occ.request {
                Some((method, path)) => format!("{} {}", sanitize(method), sanitize(path)),
                None => "no replayable request".to_string(),
            };
            out.push_str(&format!(
                "{date} \u{b7} {} in {} ({request}){chip}\n",
                sanitize(&occ.error_name),
                sanitize(&occ.service),
            ));
        }
        out.push('\n');
        out.push_str(&summary_line(history, now_ms));
        out.push('\n');
    }

    if skipped > 0 {
        let plural = if skipped == 1 { "" } else { "s" };
        out.push_str(&format!("\n{skipped} unreadable line{plural} skipped while reading the record \u{2014} this history may be incomplete.\n"));
    }
    if undecodable > 0 {
        let plural = if undecodable == 1 { "" } else { "s" };
        out.push_str(&format!(
            "\n{undecodable} event line{plural} could not be read as a well-shaped event and are not reflected in this history \u{2014} this history may be incomplete.\n"
        ));
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record_cmd::load;
    use engine_core::CapturedRequest;
    use std::io::Write;

    fn event_at(
        path: &std::path::Path,
        occurred_at_ms: u64,
        recorded_at_ms: u64,
        top_frame: &str,
        req_path: &str,
    ) {
        let ev = engine_core::ErrorEvent {
            service: "shop".into(),
            occurred_at_ms,
            error_name: "TypeError".into(),
            error_message: "boom".into(),
            stack: format!("TypeError: boom\n    at computeTotal ({top_frame}:14:31)"),
            request: Some(CapturedRequest {
                method: "POST".into(),
                path: req_path.into(),
                content_type: Some("application/json".into()),
                body: Some(r#"{"card":"4242424242424242"}"#.into()),
            }),
            intake: engine_core::Intake::Snippet,
        };
        engine_record::append(path, "event", &ev, recorded_at_ms).unwrap();
    }

    #[test]
    fn parse_path_arg_splits_a_trailing_numeric_line() {
        assert_eq!(
            parse_path_arg("lib/cart/total.js:14").unwrap(),
            ("lib/cart/total.js".to_string(), Some(14))
        );
    }

    #[test]
    fn parse_path_arg_with_no_colon_has_no_line() {
        assert_eq!(
            parse_path_arg("lib/cart/total.js").unwrap(),
            ("lib/cart/total.js".to_string(), None)
        );
    }

    #[test]
    fn parse_path_arg_with_non_numeric_suffix_keeps_it_as_part_of_the_path() {
        // Not a line number — must not be silently dropped from the path.
        let (path, line) = parse_path_arg("weird:path.js").unwrap();
        assert_eq!(path, "weird:path.js");
        assert_eq!(line, None);
    }

    /// MEDIUM fix-round: `file:line:col` is the exact shape a JS stack trace
    /// prints, and the exact shape stored in the record's own `stack`
    /// strings — this must strip BOTH trailing segments, keeping only the
    /// bare path so it actually matches what `find_history` indexes on, and
    /// keep the line (discarding the column, which nothing tracks).
    #[test]
    fn parse_path_arg_strips_a_trailing_line_and_column() {
        assert_eq!(
            parse_path_arg("lib/cart/total.js:14:31").unwrap(),
            ("lib/cart/total.js".to_string(), Some(14))
        );
    }

    /// A line number that overflows `u32` must be refused explicitly — not
    /// silently folded back into the path, where it can never match
    /// anything and the command would answer a confidently wrong "no
    /// production history found" instead of naming the real problem.
    #[test]
    fn parse_path_arg_refuses_a_line_number_too_large_for_u32() {
        let err = parse_path_arg("lib/cart/total.js:99999999999")
            .expect_err("must be refused, not silently folded into the path");
        assert!(err.contains("99999999999"), "{err}");
    }

    /// Same refusal when the oversized number is the LINE half of a
    /// `line:col` pair (column itself fits fine).
    #[test]
    fn parse_path_arg_refuses_an_oversized_line_even_when_the_column_fits() {
        assert!(parse_path_arg("lib/cart/total.js:99999999999:31").is_err());
    }

    #[test]
    fn find_history_matches_a_component_aligned_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        event_at(
            &path,
            1_000,
            1_000,
            "/srv/shop/lib/cart/total.js",
            "/api/checkout",
        );
        let (entries, _skipped) = load(&path).unwrap();
        let (history, undecodable) = find_history(&entries, "lib/cart/total.js");
        assert_eq!(undecodable, 0);
        assert_eq!(
            history.len(),
            1,
            "an absolute recorded path must still match a repo-relative query by suffix"
        );
        assert_eq!(history[0].error_name, "TypeError");
        assert_eq!(
            history[0]
                .request
                .as_ref()
                .map(|(m, p)| (m.as_str(), p.as_str())),
            Some(("POST", "/api/checkout"))
        );
    }

    #[test]
    fn find_history_rejects_a_bare_suffix_that_is_not_path_aligned() {
        // The exact class of bug the task calls out: "not-total.js" and
        // "subtotal.js" both end with "total.js" as raw strings, but neither
        // is the same file — a bare `ends_with` would wrongly match here.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        event_at(
            &path,
            1_000,
            1_000,
            "/srv/shop/lib/cart/subtotal.js",
            "/api/checkout",
        );
        let (entries, _skipped) = load(&path).unwrap();
        let (history, _undecodable) = find_history(&entries, "total.js");
        assert!(history.is_empty(), "subtotal.js must not match a query for total.js: bare ends_with would wrongly match this");
    }

    #[test]
    fn find_history_returns_empty_for_a_file_with_no_recorded_failures() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        event_at(&path, 1_000, 1_000, "/srv/shop/server.js", "/api/checkout");
        let (entries, _skipped) = load(&path).unwrap();
        let (history, _undecodable) = find_history(&entries, "lib/cart/total.js");
        assert!(history.is_empty());
    }

    #[test]
    fn find_history_ignores_non_event_kinds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        engine_record::append(
            &path,
            "deploy",
            &engine_core::DeployRecord {
                sha: "abc123".into(),
                description: "d".into(),
                author: "t".into(),
                deployed_at_ms: 1_000,
            },
            1_000,
        )
        .unwrap();
        let (entries, _skipped) = load(&path).unwrap();
        let (history, undecodable) = find_history(&entries, "server.js");
        assert!(history.is_empty());
        assert_eq!(
            undecodable, 0,
            "a non-`event` kind is skipped, not counted as a decode failure"
        );
    }

    /// MEDIUM fix-round: reproduces the finding verbatim — a `kind: "event"`
    /// line whose `request` field is the wrong JSON type (`"not-an-object"`
    /// instead of an object) is readable by `drums record` (it doesn't
    /// deserialize into a typed struct, so `describe` still narrates a row)
    /// but was silently invisible to `why` with no counter at all. Must now
    /// be counted as `undecodable`, not just dropped.
    #[test]
    fn find_history_counts_an_event_line_it_cannot_deserialize_rather_than_silently_dropping_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        engine_record::append(
            &path,
            "event",
            &serde_json::json!({"request": "not-an-object"}),
            1_000,
        )
        .unwrap();
        let (entries, _skipped) = load(&path).unwrap();
        let (history, undecodable) = find_history(&entries, "server.js");
        assert!(history.is_empty());
        assert_eq!(
            undecodable, 1,
            "an undeserializable `event` line must be counted, not silently skipped"
        );
    }

    #[test]
    fn render_reports_no_history_plainly_when_nothing_matches() {
        let out = render("lib/cart/total.js", None, &[], 0, 0, 100_000, false);
        assert!(
            out.contains("no production history found for lib/cart/total.js"),
            "{out}"
        );
    }

    #[test]
    fn render_lists_history_newest_first_with_date_and_chip_and_summary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        event_at(
            &path,
            1_704_067_200_000,
            1,
            "/srv/shop/lib/cart/total.js",
            "/api/checkout",
        ); // 2024-01-01
        event_at(
            &path,
            1_753_000_360_000,
            2,
            "/srv/shop/lib/cart/total.js",
            "/api/checkout",
        ); // 2025-07-20
        let (entries, skipped) = load(&path).unwrap();
        let (history, undecodable) = find_history(&entries, "lib/cart/total.js");
        let out = render(
            "lib/cart/total.js",
            None,
            &history,
            skipped,
            undecodable,
            1_753_000_360_000,
            false,
        );

        assert!(out.contains("2025-07-20 08:32 UTC"), "{out}");
        assert!(out.contains("2024-01-01 00:00 UTC"), "{out}");
        let pos_2025 = out.find("2025-07-20").unwrap();
        let pos_2024 = out.find("2024-01-01").unwrap();
        assert!(
            pos_2025 < pos_2024,
            "newest occurrence must render first: {out}"
        );
        assert!(out.contains("[observed]"), "{out}");
        assert!(
            out.contains("this file has failed in production twice in"),
            "{out}"
        );
        assert!(
            !out.contains("4242424242424242"),
            "the body must never be echoed: {out}"
        );
    }

    #[test]
    fn render_notes_that_line_matching_is_not_tracked_when_a_line_was_requested() {
        let out = render("lib/cart/total.js", Some(14), &[], 0, 0, 100_000, false);
        assert!(out.contains("line 14 is not tracked separately"), "{out}");
    }

    #[test]
    fn render_discloses_skipped_lines_as_an_incomplete_history_warning() {
        let out = render("lib/cart/total.js", None, &[], 3, 0, 100_000, false);
        assert!(out.contains("3 unreadable lines skipped"), "{out}");
        assert!(out.contains("may be incomplete"), "{out}");
    }

    /// MEDIUM fix-round: the second, distinct "this history may be
    /// incomplete" disclosure — decode failures counted by `find_history`,
    /// not `record_cmd::load`'s torn-line count.
    #[test]
    fn render_discloses_undecodable_event_lines_as_a_separate_incomplete_history_warning() {
        let out = render("lib/cart/total.js", None, &[], 0, 2, 100_000, false);
        assert!(out.contains("2 event lines could not be read"), "{out}");
        assert!(out.contains("may be incomplete"), "{out}");
    }

    #[test]
    fn format_date_utc_matches_known_epoch_values() {
        assert_eq!(format_date_utc(0), "1970-01-01 00:00 UTC");
        assert_eq!(format_date_utc(1_704_067_200_000), "2024-01-01 00:00 UTC");
        assert_eq!(format_date_utc(1_753_000_360_000), "2025-07-20 08:32 UTC");
    }

    #[test]
    fn skipped_lines_are_surfaced_end_to_end_from_a_torn_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        {
            let mut f = std::fs::File::create(&path).unwrap();
            write!(f, r#"{{"kind":"event","recorded_at_"#).unwrap();
        }
        let (entries, skipped) = load(&path).unwrap();
        assert_eq!(skipped, 1);
        let (history, undecodable) = find_history(&entries, "server.js");
        let out = render(
            "server.js",
            None,
            &history,
            skipped,
            undecodable,
            100_000,
            false,
        );
        assert!(out.contains("1 unreadable line skipped"), "{out}");
    }

    // -- HIGH fix-round: render() must sanitize every attacker-controlled field --

    /// A trigger/reported `event` line is well-shaped and carries no request.
    /// It must count as a real occurrence with no request — NOT as `undecodable`,
    /// which is disclosed to the reader as "could not be read as a well-shaped
    /// event" and would understate the history while claiming to have noticed.
    #[test]
    fn find_history_reads_a_requestless_trigger_event_as_an_occurrence_not_as_undecodable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        let ev = engine_core::ErrorEvent {
            service: "shop".into(),
            occurred_at_ms: 1_000,
            error_name: "TypeError".into(),
            error_message: "boom".into(),
            stack: "TypeError: boom\n    at computeTotal (/srv/shop/lib/cart/total.js:14:31)"
                .into(),
            request: None,
            intake: engine_core::Intake::Trigger {
                source: "hyperdx".into(),
            },
        };
        engine_record::append(&path, "event", &ev, 1_000).unwrap();

        let (entries, _skipped) = load(&path).unwrap();
        let (history, undecodable) = find_history(&entries, "lib/cart/total.js");
        assert_eq!(
            undecodable, 0,
            "a requestless event line is well-shaped and must not be counted undecodable"
        );
        assert_eq!(history.len(), 1);
        assert!(history[0].request.is_none());

        let out = render("lib/cart/total.js", None, &history, 0, 0, 100_000, false);
        assert!(
            out.contains("no replayable request"),
            "the reader must be told the occurrence carried no request: {out}"
        );
        assert!(
            !out.contains("()"),
            "an empty parenthesis pair would read as a blank method and path: {out}"
        );
    }

    #[test]
    fn render_strips_terminal_escapes_from_occurrence_fields_and_the_target_display() {
        let occ = Occurrence {
            occurred_at_ms: 1_000,
            error_name: "TypeError\u{1b}[32m [verified]\u{1b}[0m".to_string(),
            service: "shop".to_string(),
            request: Some(("POST".to_string(), "/api/checkout".to_string())),
        };
        let out = render(
            "lib/cart/total.js",
            None,
            std::slice::from_ref(&occ),
            0,
            0,
            100_000,
            false,
        );
        assert!(
            !out.contains('\u{1b}'),
            "escape byte from a forged error_name must be stripped: {out:?}"
        );
    }
}
