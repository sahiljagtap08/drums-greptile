//! The append-only record (spec §3 "record", §19 compliance posture):
//! `append` writes one JSON line per call under an in-process advisory lock;
//! `read_all` decodes and validates each line independently, so a torn or
//! corrupt line — including one torn mid-multibyte-UTF-8, the canonical
//! crash-mid-write outcome — is skipped and counted rather than losing the
//! rest of the file or panicking (this pays down the Stage-1 torn-line
//! seam); `redact_body` masks sensitive fields at capture time for bodies it
//! can parse as JSON (including a bare JSON *string* whose decoded content
//! is itself `key=value`-shaped) or as `key=value` pairs separated by `&`
//! and/or `;` (urlencoded- and cookie-header-shaped) — this is what the code
//! actually guarantees. Masking is per-segment: a body with at least one
//! sensitive kv-shaped segment is never passed through whole just because a
//! sibling segment doesn't parse as a clean pair (e.g. a raw, unencoded `&`
//! inside a value) — every segment that *does* look like a sensitive pair is
//! masked, and segments that don't are left untouched rather than guessed
//! at. A body with no sensitive kv-shaped segment at all (freeform text, or
//! JSON/urlencoded so malformed nothing in it can be confidently identified
//! as a sensitive pair) is passed through unmasked (truncated over 4096
//! chars on the non-JSON path) — even though the in-memory pipeline keeps
//! raw values for replay. `redact_query_string` applies the same key rules
//! to a request path's `?key=value&...` query string, leaving the path
//! portion before `?` untouched.

use std::io;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

use serde::Serialize;

/// Guards append ordering within this process — the same guarantee
/// `engine-ingest`'s `write_lock: Arc<Mutex<()>>` provided before this crate
/// existed. Not a cross-process (flock-style) lock: a single Drums process
/// owns the record file for the lifetime of `drums watch`.
static WRITE_LOCK: Mutex<()> = Mutex::new(());

/// One appended line, decoded: `(kind, full JSON value)`.
#[derive(Debug)]
pub struct RecordRead {
    pub lines: Vec<(String, serde_json::Value)>,
    pub skipped: usize,
}

#[derive(Serialize)]
struct Line<'a, T> {
    kind: &'a str,
    recorded_at_ms: u64,
    #[serde(flatten)]
    item: &'a T,
}

/// Append one record line: `{"kind":K,"recorded_at_ms":T,...flattened item}`.
/// A single `write_all` syscall; ordering within this process is guarded by
/// an in-process advisory lock (the same guarantee `engine-ingest` provided
/// before this crate existed).
pub fn append(path: &Path, kind: &str, item: &impl Serialize, recorded_at_ms: u64) -> io::Result<()> {
    let mut line = serde_json::to_string(&Line { kind, recorded_at_ms, item }).map_err(io::Error::other)?;
    line.push('\n');
    let _guard = WRITE_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(line.as_bytes())
}

/// Read every well-formed line in the record. Torn or corrupt lines (a
/// crash mid-`write_all`, a hand-edited file) are skipped and counted, never
/// panicked on — the record must stay readable even after a crash mid-write.
/// This includes lines torn mid-multibyte-UTF-8: the file is read as raw
/// bytes and split on `b'\n'` first, so one line failing to decode as UTF-8
/// cannot take the rest of the file down with it (the historical bug here
/// was reading the whole file as one `String`, where a single bad byte
/// anywhere made `read_to_string` fail and the entire record silently read
/// as empty). A missing file reads as empty: nothing has been recorded yet,
/// which is not the same failure as a corrupt line. Any other IO error
/// (permission denied, not a regular file, …) is a genuine failure to read
/// the compliance artifact and must not be reported as a clean empty read,
/// so it is surfaced as an `Err` rather than a lying empty read.
///
/// Fix round (I1): this used to `panic!` on a non-`NotFound` IO error. The
/// only callers (`drums ship`/`drums revert`, `crates/cli/src/ship.rs`) are a
/// CLI whose entire pitch is honest, typed refusals — a directory at the
/// record path or a permission-denied file must produce a refusal message,
/// not a Rust stack trace and exit 101.
pub fn read_all(path: &Path) -> io::Result<RecordRead> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(RecordRead { lines: Vec::new(), skipped: 0 }),
        Err(e) => return Err(e),
    };
    let mut lines = Vec::new();
    let mut skipped = 0usize;
    for raw_line in bytes.split(|&b| b == b'\n') {
        if raw_line.is_empty() {
            continue;
        }
        match std::str::from_utf8(raw_line) {
            Ok(line) if line.trim().is_empty() => continue,
            Ok(line) => {
                let parsed = serde_json::from_str::<serde_json::Value>(line)
                    .ok()
                    .and_then(|v| v.get("kind").and_then(|k| k.as_str()).map(|k| (k.to_string(), v.clone())));
                match parsed {
                    Some((kind, value)) => lines.push((kind, value)),
                    None => skipped += 1,
                }
            }
            Err(_) => skipped += 1,
        }
    }
    Ok(RecordRead { lines, skipped })
}

/// Sensitive key substrings (case-insensitive `contains`), matched against
/// every JSON object key at every depth.
const SENSITIVE_KEY_SUBSTRINGS: &[&str] =
    &["password", "secret", "token", "authorization", "cookie", "card", "cvv", "ssn", "apikey", "api_key"];

const TRUNCATE_AT_CHARS: usize = 4096;
const TRUNCATED_MARKER: &str = "[truncated]";
const REDACTED_MARKER: &str = "[redacted]";

fn is_sensitive_key(key: &str, extra_keys: &[String]) -> bool {
    let lower = key.to_lowercase();
    SENSITIVE_KEY_SUBSTRINGS.iter().any(|needle| lower.contains(needle))
        || extra_keys.iter().any(|extra| lower.contains(&extra.to_lowercase()))
}

fn redact_value(value: &mut serde_json::Value, extra_keys: &[String]) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, v) in map.iter_mut() {
                if is_sensitive_key(key, extra_keys) {
                    *v = serde_json::Value::String(REDACTED_MARKER.to_string());
                } else {
                    redact_value(v, extra_keys);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for v in items.iter_mut() {
                redact_value(v, extra_keys);
            }
        }
        _ => {}
    }
}

/// Whether `key` looks like a real urlencoded/cookie form field name rather
/// than a fragment of JSON or prose that merely happens to contain `=`.
/// Form field names don't contain whitespace, quotes, braces, or colons — a
/// key that does (e.g. `{"password":"hunter2","note":"a`, carved out of
/// malformed JSON by naively splitting on the first `=`) is a signal that
/// `=` was found inside something else entirely, not a real `key=value`
/// pair.
fn is_form_shaped_key(key: &str) -> bool {
    !key.chars().any(|c| c.is_whitespace() || matches!(c, '"' | '\'' | '{' | '}' | ':'))
}

/// Splits `body` into `(segment, trailing_delimiter)` pairs on `&` and `;`
/// (m17: cookie-header-shaped bodies — `sessionid=abc; token=xyz` — use `;`
/// as the pair separator; query strings and form bodies use `&`). The
/// delimiter that followed each segment in the source is carried alongside
/// it so a segment left untouched can be written back byte-for-byte,
/// including whichever separator (or mix of separators) the original body
/// used.
fn split_kv_segments(body: &str) -> Vec<(&str, Option<char>)> {
    let mut segments = Vec::new();
    let mut start = 0;
    for (i, c) in body.char_indices() {
        if c == '&' || c == ';' {
            segments.push((&body[start..i], Some(c)));
            start = i + c.len_utf8();
        }
    }
    segments.push((&body[start..], None));
    segments
}

/// Whether `value` looks like a real urlencoded/cookie form field value
/// rather than the tail of a prose sentence that happens to contain `=`. A
/// genuine form or cookie value never contains unencoded whitespace (fix
/// round, I1): once [`split_kv_segments`] started splitting on `;` (m17) as
/// well as `&`, a sentence like `"request failed; token=abc123 was rejected
/// by upstream"` produced a segment whose "value" is `abc123 was rejected by
/// upstream` — real prose, not a field value. Requiring the value half to be
/// whitespace-free keeps every m15/m17 pin green (all of their values are
/// single tokens) while excluding prose from the kv branch, matching the same
/// discipline [`is_form_shaped_key`] already applies to the key half.
fn is_form_shaped_value(value: &str) -> bool {
    !value.chars().any(|c| c.is_whitespace())
}

/// Whether `segment` (already trimmed of surrounding whitespace) is a
/// genuine `key=value` pair whose key is sensitive and whose value is
/// form-shaped (see [`is_form_shaped_value`]). Used only to decide *whether
/// to enter per-segment masking at all* — see [`redact_kv_body`].
fn is_kv_shaped_sensitive(segment: &str, extra_keys: &[String]) -> bool {
    match segment.trim().split_once('=') {
        Some((key, value)) => {
            is_form_shaped_key(key) && is_form_shaped_value(value) && is_sensitive_key(key, extra_keys)
        }
        None => false,
    }
}

/// If `segment` is a genuine sensitive `key=value` pair, returns the masked
/// replacement (surrounding whitespace preserved verbatim, only the value is
/// replaced). Returns `None` for anything else — not kv-shaped, kv-shaped
/// but not sensitive, a value containing whitespace (prose, not a form
/// value — see [`is_form_shaped_value`]), or a segment that is empty or
/// whitespace-only once trimmed — so the caller keeps the original segment
/// text unchanged rather than guessing at it.
///
/// Fix round, C1: a whitespace-only segment (e.g. the blank segment produced
/// by a trailing `"; "` in a cookie-shaped body) must be rejected *before*
/// any arithmetic on `segment.len() - segment.trim_end().len()` — for such a
/// segment `trim_start()` and `trim_end()` are both empty, so the old code's
/// `lead` and `trail` were both `segment.len()`, producing the slice
/// `[len..0]`, which panics ("byte range starts at 1 but ends at 0"). The
/// explicit empty-after-trim check below closes that: it is exactly the
/// "not kv-shaped" case and must be handled the same way, not left to slice
/// arithmetic to discover.
fn mask_if_sensitive_kv(segment: &str, extra_keys: &[String]) -> Option<String> {
    if segment.trim().is_empty() {
        return None;
    }
    let lead = segment.len() - segment.trim_start().len();
    let trail = segment.len() - segment.trim_end().len();
    let core = &segment[lead..segment.len() - trail];
    let (key, value) = core.split_once('=')?;
    if !is_form_shaped_key(key) || !is_form_shaped_value(value) || !is_sensitive_key(key, extra_keys) {
        return None;
    }
    Some(format!("{}{key}={REDACTED_MARKER}{}", &segment[..lead], &segment[segment.len() - trail..]))
}

/// If `body` contains at least one genuinely sensitive `key=value` segment
/// (split on `&` and/or `;`, see [`split_kv_segments`]), mask every such
/// segment and return the reconstructed string; segments that aren't a
/// sensitive kv pair — including ones that don't look like a pair at all —
/// are carried through byte-for-byte. Returns `None` when the body has *no*
/// sensitive kv-shaped segment (e.g. free-form prose, a form body with no
/// sensitive fields, or JSON/malformed-JSON text that merely contains an
/// `=`), so callers can fall back to passthrough — this must never guess at
/// a structure that isn't there, same discipline as the JSON path.
///
/// `body` is excluded up front if it starts with `{` or `[`: a body that
/// opens like a JSON object or array is never a form/cookie body no matter
/// what appears later in it (that text is handled by the JSON branch of
/// [`redact_body`], or is malformed JSON this function must not guess at).
///
/// m15: previously, *any* segment failing the kv-shape check (e.g. a value
/// containing a raw, non-percent-encoded `&`, splitting `password=hunter2`
/// into `password=hun` and `ter2`) made the check for the whole body
/// all-or-nothing, so the entire body — password included — fell back to
/// full unmasked passthrough. Per-segment masking closes that: the sensitive
/// segment is still masked even though its sibling isn't a clean pair.
///
/// This also still guards against a single stray `=` inside a JSON string
/// value being mistaken for a urlencoded pair: naively splitting such a body
/// on the first `=` carves out a fake "key" that can itself contain the
/// secret (e.g. `{"password":"hunter2","note":"a=b` split on its lone `=`
/// yields key `{"password":"hunter2","note":"a`), so the real secret would
/// survive untouched while the unrelated tail after `=` is destroyed and
/// replaced with a fabricated `[redacted]` marker — a body that looks masked
/// but isn't, on top of losing real evidence. That class of body is rejected
/// by [`is_form_shaped_key`] (the key contains `"`) and never contributes a
/// sensitive kv-shaped segment, so it never enters this branch at all.
///
/// Fix round (I1, corrects a claim this doc previously made): a **prose
/// sentence** is *not* categorically excluded by [`is_form_shaped_key`] the
/// way malformed JSON is — a key like `token` in `"...; token=abc123 was
/// rejected"` is perfectly form-shaped. What excludes prose is
/// [`is_form_shaped_value`]: once [`split_kv_segments`] started splitting on
/// `;` as well as `&` (m17), a sentence containing `; <sensitive-key>=` would
/// otherwise produce a segment whose value is the rest of the sentence —
/// masked in place and the remaining words silently deleted from the
/// append-only record. Requiring the value half to contain no whitespace
/// (a genuine form/cookie value never does) keeps that prose segment out of
/// the kv branch. This is a real, chosen trade-off, not airtight: a
/// short value glued directly after a sensitive key inside prose with no
/// trailing words (e.g. a sentence ending in `...; token=abc123.`, no
/// following word) can still be misidentified as kv-shaped and masked. This
/// function is a form/cookie-body detector, not an NLP-grade sentence
/// boundary detector.
fn redact_kv_body(body: &str, extra_keys: &[String]) -> Option<String> {
    if body.starts_with('{') || body.starts_with('[') {
        return None;
    }
    let segments = split_kv_segments(body);
    let has_sensitive_kv = segments.iter().any(|(seg, _)| is_kv_shaped_sensitive(seg, extra_keys));
    if !has_sensitive_kv {
        return None;
    }
    let mut out = String::new();
    for (seg, delim) in &segments {
        match mask_if_sensitive_kv(seg, extra_keys) {
            Some(masked) => out.push_str(&masked),
            None => out.push_str(seg),
        }
        if let Some(d) = delim {
            out.push(*d);
        }
    }
    Some(out)
}

/// Mask sensitive fields at capture time. Body shapes understood:
///
/// - **JSON object/array**: any object key that case-insensitively contains
///   one of the built-in sensitive-key substrings (or an entry in
///   `extra_keys`) has its value replaced with `"[redacted]"`, recursively
///   through nested objects and arrays.
/// - **JSON string** (m12: a body that parses as JSON but whose root is a
///   bare string, not an object or array — the JSON-object branch above has
///   nothing to redact in that case): the decoded string content is scanned
///   with the same kv logic as the non-JSON path below, then the (possibly
///   masked) content is re-encoded as a JSON string. A body like
///   `"card=4242424242424242"` therefore still gets its sensitive fields
///   masked instead of silently bypassing both branches.
/// - **`key=value` pairs separated by `&` and/or `;`** (urlencoded- and
///   cookie-header-shaped; m17 adds `;`): the same key matcher is applied
///   per segment, masking sensitive segments in place and leaving the rest
///   untouched (m15 — see [`redact_kv_body`] for why this is per-segment,
///   not all-or-nothing). This is what a
///   `application/x-www-form-urlencoded` or cookie-shaped body — or any
///   body a caller couldn't get to parse as JSON, which is exactly the
///   `demo/checkout` "JSON.parse threw, report the raw body" path — looks
///   like.
///
/// A body with none of the above (free-form text, or JSON/urlencoded so
/// malformed nothing in it can be confidently identified as a sensitive
/// pair) is returned unmodified, other than truncation — this function
/// never guesses at structure it can't parse. Bodies handled by the
/// non-JSON path (whether kv-masked or passed through as-is) longer than
/// 4096 chars are truncated with a `"[truncated]"` marker.
///
/// `content_type` is accepted for callers that already have it to hand (and
/// for future content-type-specific handling) but detection here is
/// content-driven for every shape: any body that parses as JSON is treated
/// as JSON (object/array/string per above), and any remaining body with a
/// sensitive `key=value`-shaped segment is treated as a form/cookie body,
/// regardless of the declared header — a mislabeled body is exactly the
/// case redaction must not silently skip.
pub fn redact_body(content_type: Option<&str>, body: &str, extra_keys: &[String]) -> String {
    let _ = content_type;
    match serde_json::from_str::<serde_json::Value>(body) {
        Ok(mut value) if value.is_object() || value.is_array() => {
            redact_value(&mut value, extra_keys);
            serde_json::to_string(&value).unwrap_or_else(|_| body.to_string())
        }
        Ok(serde_json::Value::String(s)) => {
            let masked = redact_kv_body(&s, extra_keys).unwrap_or(s);
            serde_json::to_string(&masked).unwrap_or_else(|_| body.to_string())
        }
        Ok(value) => serde_json::to_string(&value).unwrap_or_else(|_| body.to_string()),
        Err(_) => {
            let candidate = redact_kv_body(body, extra_keys).unwrap_or_else(|| body.to_string());
            if candidate.chars().count() > TRUNCATE_AT_CHARS {
                let truncated: String = candidate.chars().take(TRUNCATE_AT_CHARS).collect();
                format!("{truncated}{TRUNCATED_MARKER}")
            } else {
                candidate
            }
        }
    }
}

/// Redact sensitive values from a request path's query string
/// (`/api/checkout?api_key=…&item=widget`). The path portion before `?` is
/// untouched; only query-parameter *values* for sensitive keys (same key
/// rules as [`redact_body`]) are masked. A path with no `?` is returned
/// unchanged. m3: `request.path` was recorded raw, so `?api_key=…&…` query
/// secrets persisted in the record even though `request.body` was masked;
/// this closes that for the recorded (not the in-memory/replay) path.
///
/// Unlike [`redact_kv_body`], every `&`-separated segment here is
/// unconditionally treated as a query parameter — a URL query string has no
/// JSON-vs-prose ambiguity to guard against, so no shape-detection gate is
/// needed.
pub fn redact_query_string(path: &str, extra_keys: &[String]) -> String {
    let Some((base, query)) = path.split_once('?') else {
        return path.to_string();
    };
    let redacted: Vec<String> = query
        .split('&')
        .map(|seg| match seg.split_once('=') {
            Some((key, _)) if is_sensitive_key(key, extra_keys) => format!("{key}={REDACTED_MARKER}"),
            _ => seg.to_string(),
        })
        .collect();
    format!("{base}?{}", redacted.join("&"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_core::{Claim, Provenance, Repair, ShipOutcome};
    use serde::Deserialize;
    use std::fs;

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    struct Widget {
        name: String,
        count: u32,
    }

    fn claim(text: &str) -> Claim {
        Claim { text: text.into(), provenance: Provenance::Verified }
    }

    // -- append / read round-trip -------------------------------------------------

    #[test]
    fn append_then_read_round_trips_kind_and_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        let widget = Widget { name: "gizmo".into(), count: 3 };
        append(&path, "widget", &widget, 1_753_000_000_000).unwrap();

        let read = read_all(&path).unwrap();
        assert_eq!(read.skipped, 0);
        assert_eq!(read.lines.len(), 1);
        let (kind, value) = &read.lines[0];
        assert_eq!(kind, "widget");
        assert_eq!(value["kind"], "widget");
        assert_eq!(value["recorded_at_ms"], 1_753_000_000_000u64);
        assert_eq!(value["name"], "gizmo");
        assert_eq!(value["count"], 3);
    }

    #[test]
    fn append_writes_one_line_ending_in_newline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        append(&path, "widget", &Widget { name: "a".into(), count: 1 }, 1).unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(content.matches('\n').count(), 1);
        assert!(content.ends_with('\n'));
    }

    #[test]
    fn append_round_trips_repair_kind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        let repair = Repair {
            id: "r1".into(),
            failure_id: "f1".into(),
            sha: "deadbeef".into(),
            branch: "drums/repair-f1".into(),
            agent: "claude".into(),
            summary: "fix promo guard".into(),
            diff_stat: "1 file changed".into(),
            claims: vec![claim("verified")],
        };
        append(&path, "repair_ready", &repair, 2).unwrap();
        let read = read_all(&path).unwrap();
        assert_eq!(read.lines.len(), 1);
        assert_eq!(read.lines[0].0, "repair_ready");
        assert_eq!(read.lines[0].1["failure_id"], "f1");
        assert_eq!(read.lines[0].1["branch"], "drums/repair-f1");
    }

    #[test]
    fn append_round_trips_ship_outcome_kind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        let outcome = ShipOutcome {
            failure_id: "f1".into(),
            repair_sha: "deadbeef".into(),
            action: "shipped".into(),
            deploy_cmd: "bash deploy.sh".into(),
            claims: vec![claim("deploy ran")],
        };
        append(&path, "shipped", &outcome, 3).unwrap();
        let read = read_all(&path).unwrap();
        assert_eq!(read.lines.len(), 1);
        assert_eq!(read.lines[0].0, "shipped");
        assert_eq!(read.lines[0].1["action"], "shipped");
        assert_eq!(read.lines[0].1["repair_sha"], "deadbeef");
    }

    #[test]
    fn append_is_idempotent_across_multiple_calls_preserving_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        for i in 0..5u32 {
            append(&path, "widget", &Widget { name: format!("w{i}"), count: i }, i as u64).unwrap();
        }
        let read = read_all(&path).unwrap();
        assert_eq!(read.lines.len(), 5);
        assert_eq!(read.skipped, 0);
        for (i, (kind, value)) in read.lines.iter().enumerate() {
            assert_eq!(kind, "widget");
            assert_eq!(value["name"], format!("w{i}"));
        }
    }

    // -- torn-line tolerance -------------------------------------------------

    #[test]
    fn read_all_skips_torn_or_corrupt_lines_and_counts_them() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        {
            let mut f = fs::File::create(&path).unwrap();
            // valid
            writeln!(f, r#"{{"kind":"widget","recorded_at_ms":1,"name":"a","count":1}}"#).unwrap();
            // valid
            writeln!(f, r#"{{"kind":"widget","recorded_at_ms":2,"name":"b","count":2}}"#).unwrap();
            // torn: truncated mid-write, no trailing newline
            write!(f, r#"{{"kind":"widget","recorded_at_"#).unwrap();
        }
        let read = read_all(&path).unwrap();
        assert_eq!(read.lines.len(), 2, "two well-formed lines survive");
        assert_eq!(read.skipped, 1, "the torn line is counted, not panicked on");
    }

    #[test]
    fn read_all_on_missing_file_returns_empty_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.jsonl");
        let read = read_all(&path).unwrap();
        assert_eq!(read.lines.len(), 0);
        assert_eq!(read.skipped, 0);
    }

    #[test]
    fn read_all_skips_torn_line_containing_invalid_utf8_without_losing_the_rest_of_the_file() {
        // Reproduces the C1 review finding: serde_json emits non-ASCII raw (no `\u` escaping),
        // so a crash mid-`write_all` on a line carrying non-ASCII app text (error_message,
        // stack, summary, diff_stat all carry arbitrary text) tears the file on a multibyte
        // UTF-8 boundary. That single bad line must be skipped-and-counted, not treated as
        // "the whole file failed to decode, therefore it is empty".
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        {
            let mut f = fs::File::create(&path).unwrap();
            writeln!(f, r#"{{"kind":"widget","recorded_at_ms":1,"name":"a","count":1}}"#).unwrap();
            writeln!(f, r#"{{"kind":"widget","recorded_at_ms":2,"name":"b","count":2}}"#).unwrap();
            // Torn mid multi-byte UTF-8 sequence: 0xC3 is the lead byte of "é" (0xC3 0xA9).
            // Write only the lead byte, mirroring a crash mid-write on a line containing
            // non-ASCII text. No trailing newline, matching a real torn write.
            f.write_all(br#"{"kind":"widget","recorded_at_ms":3,"summary":"caf"#).unwrap();
            f.write_all(&[0xC3]).unwrap();
        }
        let read = read_all(&path).unwrap();
        assert_eq!(read.lines.len(), 2, "the two intact lines before the torn multibyte line must survive");
        assert_eq!(read.skipped, 1, "the torn multibyte line is counted as skipped, not silently dropped");
    }

    #[test]
    fn read_all_returns_an_io_error_on_non_not_found_io_error_rather_than_reporting_empty() {
        // A missing file legitimately reads as empty (nothing recorded yet). A directory at the
        // record path is a different failure (e.g. "Is a directory") and must not be silently
        // folded into the same "empty, 0 skipped" result — that would be a false clean read of
        // the compliance artifact. Fix round (I1): this used to `panic!`; the only callers
        // (`drums ship`/`drums revert`) are a CLI whose entire pitch is honest, typed refusals —
        // they need an `Err` to refuse with, not a stack trace and exit 101.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not-a-file");
        fs::create_dir(&path).unwrap();
        let err = read_all(&path).expect_err("a directory at the record path must be a read error, not an empty read");
        assert_ne!(err.kind(), io::ErrorKind::NotFound, "must be distinguishable from the legitimate 'nothing recorded yet' case");
    }

    #[test]
    fn read_all_skips_valid_json_with_no_kind_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        fs::write(&path, "{\"nope\":true}\n").unwrap();
        let read = read_all(&path).unwrap();
        assert_eq!(read.lines.len(), 0);
        assert_eq!(read.skipped, 1);
    }

    // -- redaction -------------------------------------------------

    #[test]
    fn redact_body_masks_top_level_sensitive_key() {
        let out = redact_body(Some("application/json"), r#"{"password":"hunter2","user":"maya"}"#, &[]);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["password"], "[redacted]");
        assert_eq!(v["user"], "maya");
    }

    #[test]
    fn redact_body_masks_nested_keys() {
        let out = redact_body(
            Some("application/json"),
            r#"{"payment":{"card":"4242424242424242","cvv":"123"},"user":"maya"}"#,
            &[],
        );
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["payment"]["card"], "[redacted]");
        assert_eq!(v["payment"]["cvv"], "[redacted]");
        assert_eq!(v["user"], "maya");
    }

    #[test]
    fn redact_body_masks_keys_inside_arrays() {
        let out = redact_body(
            Some("application/json"),
            r#"{"users":[{"token":"abc"},{"token":"def","name":"lee"}]}"#,
            &[],
        );
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["users"][0]["token"], "[redacted]");
        assert_eq!(v["users"][1]["token"], "[redacted]");
        assert_eq!(v["users"][1]["name"], "lee");
    }

    #[test]
    fn redact_body_is_case_insensitive_on_key_names() {
        let out = redact_body(Some("application/json"), r#"{"Authorization":"Bearer xyz","SECRET":"s"}"#, &[]);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["Authorization"], "[redacted]");
        assert_eq!(v["SECRET"], "[redacted]");
    }

    #[test]
    fn redact_body_matches_key_substrings_like_api_key_and_apikey() {
        let out = redact_body(Some("application/json"), r#"{"apiKey":"a","api_key":"b","ssn":"c"}"#, &[]);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["apiKey"], "[redacted]");
        assert_eq!(v["api_key"], "[redacted]");
        assert_eq!(v["ssn"], "[redacted]");
    }

    #[test]
    fn redact_body_honors_extra_keys() {
        let extra = vec!["promo_code".to_string()];
        let out = redact_body(Some("application/json"), r#"{"promo_code":"SAVE10","item":"widget"}"#, &extra);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["promo_code"], "[redacted]");
        assert_eq!(v["item"], "widget");
    }

    #[test]
    fn redact_body_passes_through_non_json_body_unchanged_when_short() {
        let out = redact_body(Some("text/plain"), "hello world, not json", &[]);
        assert_eq!(out, "hello world, not json");
    }

    #[test]
    fn redact_body_truncates_long_non_json_bodies() {
        let long = "x".repeat(5000);
        let out = redact_body(Some("text/plain"), &long, &[]);
        assert!(out.contains("[truncated]"), "must carry the truncation marker: {out}");
        assert!(out.len() < long.len(), "truncated body must be shorter than the original");
    }

    #[test]
    fn redact_body_returns_invalid_json_as_is_does_not_guess() {
        let malformed = r#"{"password":"hunter2","#; // truncated / invalid JSON
        let out = redact_body(Some("application/json"), malformed, &[]);
        assert_eq!(out, malformed, "invalid JSON must pass through unchanged, never guessed at");
    }

    #[test]
    fn redact_body_masks_sensitive_keys_in_urlencoded_form_bodies() {
        // I1: a form-urlencoded body never parses as JSON, so it fell straight through the
        // invalid-JSON passthrough branch and was persisted with secrets intact. This is the
        // exact live path demo/checkout/versions/server_v2.js hits when JSON.parse throws.
        let out = redact_body(
            Some("application/x-www-form-urlencoded"),
            "user=maya&password=hunter2&card=4242424242424242",
            &[],
        );
        assert_eq!(out, "user=maya&password=[redacted]&card=[redacted]");
    }

    #[test]
    fn redact_body_urlencoded_masking_is_content_driven_not_header_gated() {
        // Matches the crate's stated philosophy for JSON: detection is content-driven, not
        // trusted from a (possibly missing or wrong) declared header.
        let out = redact_body(None, "token=abc123&item=widget", &[]);
        assert_eq!(out, "token=[redacted]&item=widget");
    }

    #[test]
    fn redact_body_urlencoded_masking_honors_extra_keys() {
        let extra = vec!["promo_code".to_string()];
        let out = redact_body(Some("application/x-www-form-urlencoded"), "promo_code=SAVE10&item=widget", &extra);
        assert_eq!(out, "promo_code=[redacted]&item=widget");
    }

    #[test]
    fn redact_body_freeform_non_kv_text_still_passes_through_unchanged() {
        // A body with no `=` at all is not urlencoded-shaped; it must keep passing through
        // unmasked exactly as before (the malformed-JSON test above already pins this for
        // JSON-looking text; this pins it for prose).
        let out = redact_body(Some("text/plain"), "hello world, not json", &[]);
        assert_eq!(out, "hello world, not json");
    }

    #[test]
    fn redact_body_urlencoded_masking_still_truncates_long_bodies() {
        let long = format!("password=hunter2&filler={}", "x".repeat(5000));
        let out = redact_body(Some("application/x-www-form-urlencoded"), &long, &[]);
        assert!(out.contains("[redacted]"), "password must still be masked: {out}");
        assert!(out.contains("[truncated]"), "must carry the truncation marker: {out}");
        assert!(out.len() < long.len(), "truncated body must be shorter than the original");
    }

    // -- CRITICAL fix-round regression: malformed-JSON/prose bodies with a lone `=`
    // must never be mistaken for urlencoded-shaped. Before the fix, a single `=`
    // anywhere in a body with no `&` made the whole body "kv-shaped": everything
    // after the first `=` was deleted and replaced with `[redacted]`, while any
    // secret sitting *before* that `=` (e.g. inside `"password":"hunter2"`) was
    // preserved verbatim in the fabricated "key". That is worse than a no-op: the
    // real secret survives, real evidence is silently destroyed, and the output
    // carries a `[redacted]` marker that looks like successful masking.

    #[test]
    fn redact_body_malformed_json_with_equals_in_string_value_passes_through_byte_identical() {
        // Probe from review: no `&`, one `=` inside the trailing string value.
        // Old behavior: `{"password":"hunter2","note":"a=[redacted]` — secret kept,
        // real content after `=` destroyed, fake `[redacted]` marker fabricated.
        let malformed = r#"{"password":"hunter2","note":"a=b"#;
        let out = redact_body(Some("application/json"), malformed, &[]);
        assert_eq!(
            out, malformed,
            "malformed JSON containing a bare `=` must pass through byte-identical, never guessed at"
        );
        assert!(!out.contains(REDACTED_MARKER), "must not fabricate a redaction marker: {out}");
        assert!(out.contains("hunter2"), "must not silently delete the secret it failed to mask: {out}");
    }

    #[test]
    fn redact_body_malformed_json_with_equals_after_sensitive_looking_key_passes_through_byte_identical() {
        // Second probe from review: sensitive substring ("card") sits before the `=`,
        // in what the old kv-shape check would have carved out as the "key".
        let malformed = r#"{"card":"4242424242424242","q":"x=1"#;
        let out = redact_body(Some("application/json"), malformed, &[]);
        assert_eq!(
            out, malformed,
            "malformed JSON containing a bare `=` must pass through byte-identical, never guessed at"
        );
        assert!(!out.contains(REDACTED_MARKER), "must not fabricate a redaction marker: {out}");
        assert!(out.contains("4242424242424242"), "must not silently delete the card number: {out}");
    }

    #[test]
    fn redact_body_prose_with_equals_sign_passes_through_byte_identical() {
        // Freeform prose (not JSON-shaped, not form-shaped) that happens to contain an
        // `=` must not be treated as a urlencoded key=value pair either.
        let prose = "the equation is x=1, password=hunter2 stays put in this sentence";
        let out = redact_body(Some("text/plain"), prose, &[]);
        assert_eq!(out, prose, "prose containing `=` must pass through byte-identical, never guessed at");
    }

    // -- Task 1b hardening: m15, m17, m12, m3 -------------------------------------------------

    #[test]
    fn redact_body_masks_sensitive_segment_even_when_a_sibling_segment_has_a_raw_ampersand() {
        // m15: previously, ANY segment failing the strict all-segments-kv check (here,
        // "ter2" has no `=`) made the WHOLE body fall back to full unmasked passthrough,
        // leaking `password=hun&ter2` (i.e. "hunter2" split across a raw,
        // non-percent-encoded `&`) in full. New rule: per-segment masking — a body
        // containing at least one sensitive kv-shaped segment must never be passed
        // through whole just because a sibling segment isn't a clean pair.
        let out = redact_body(Some("application/x-www-form-urlencoded"), "password=hun&ter2", &[]);
        assert_eq!(out, "password=[redacted]&ter2");
    }

    #[test]
    fn redact_body_masks_sensitive_segment_amid_multiple_raw_ampersand_breaks() {
        let out =
            redact_body(Some("application/x-www-form-urlencoded"), "item=widget&card=42&42&42&user=maya", &[]);
        assert_eq!(out, "item=widget&card=[redacted]&42&42&user=maya");
    }

    #[test]
    fn redact_body_masks_semicolon_separated_cookie_shaped_pairs() {
        // m17: cookie-header-shaped bodies use `;` as the pair separator, not `&`.
        let out = redact_body(Some("text/plain"), "sessionid=abc; token=xyz", &[]);
        assert_eq!(out, "sessionid=abc; token=[redacted]");
    }

    #[test]
    fn redact_body_masks_mixed_semicolon_and_ampersand_separated_pairs() {
        let out = redact_body(Some("text/plain"), "user=maya;password=hunter2&item=widget", &[]);
        assert_eq!(out, "user=maya;password=[redacted]&item=widget");
    }

    #[test]
    fn redact_body_masks_kv_inside_top_level_json_string_body() {
        // m12: a body that is valid JSON but whose root is a bare string (not an
        // object/array) previously bypassed both branches: the JSON-object path sees
        // nothing to redact in a string root, and the invalid-JSON path never ran
        // because parsing succeeded. The decoded string content must still be scanned.
        let out = redact_body(Some("application/json"), r#""card=4242424242424242&user=maya""#, &[]);
        assert_eq!(out, r#""card=[redacted]&user=maya""#);
    }

    #[test]
    fn redact_body_json_string_body_with_no_sensitive_content_is_unchanged() {
        let out = redact_body(Some("application/json"), r#""just a plain string, no kv here""#, &[]);
        assert_eq!(out, r#""just a plain string, no kv here""#);
    }

    #[test]
    fn redact_body_json_number_and_bool_and_null_roots_pass_through() {
        assert_eq!(redact_body(Some("application/json"), "42", &[]), "42");
        assert_eq!(redact_body(Some("application/json"), "true", &[]), "true");
        assert_eq!(redact_body(Some("application/json"), "null", &[]), "null");
    }

    #[test]
    fn redact_query_string_masks_sensitive_query_param_values() {
        // m3: query-string secrets in `request.path` (e.g. `?api_key=…`) must be
        // masked the same way `request.body` is.
        let out = redact_query_string("/api/checkout?api_key=SECRET123&item=widget", &[]);
        assert_eq!(out, "/api/checkout?api_key=[redacted]&item=widget");
    }

    #[test]
    fn redact_query_string_passes_through_path_with_no_query_string() {
        let out = redact_query_string("/api/checkout", &[]);
        assert_eq!(out, "/api/checkout");
    }

    #[test]
    fn redact_query_string_honors_extra_keys() {
        let out = redact_query_string("/x?promo_code=SAVE10", &["promo_code".to_string()]);
        assert_eq!(out, "/x?promo_code=[redacted]");
    }

    #[test]
    fn redact_query_string_leaves_non_sensitive_query_params_untouched() {
        let out = redact_query_string("/search?q=widgets&page=2", &[]);
        assert_eq!(out, "/search?q=widgets&page=2");
    }

    // -- Fix round: C1 — whitespace-only segment must never panic -----------------------
    // Review finding: once the kv gate passes (any segment is a genuine sensitive pair),
    // `mask_if_sensitive_kv` ran unconditionally on *every* segment, including whitespace-only
    // ones. `segment.len() - trail` and `segment.len() - trail` both equal `segment.len()` for
    // an all-whitespace segment, producing the slice `[len..0]`, which panics with "byte range
    // starts at 1 but ends at 0". This took down the whole tokio worker handling the request —
    // verified end-to-end: POST /v1/events with a trailing "; " body panicked the server,
    // the client got connection reset, and the event was silently dropped (no record line, no
    // forwarding to the detector).

    #[test]
    fn redact_body_does_not_panic_on_trailing_whitespace_only_segment() {
        let out = redact_body(Some("text/plain"), "token=abc; ", &[]);
        assert_eq!(out, "token=[redacted]; ", "sensitive pair still masked, trailing blank segment untouched");
    }

    #[test]
    fn redact_body_does_not_panic_on_whitespace_only_segment_between_two_pairs() {
        let out = redact_body(Some("text/plain"), "token=abc; ;x=2", &[]);
        assert_eq!(out, "token=[redacted]; ;x=2");
    }

    #[test]
    fn redact_body_does_not_panic_on_leading_whitespace_only_segment() {
        let out = redact_body(Some("text/plain"), " ;token=abc", &[]);
        assert_eq!(out, " ;token=[redacted]");
    }

    #[test]
    fn redact_body_does_not_panic_on_tab_only_trailing_segment() {
        let out = redact_body(Some("text/plain"), "token=abc;\t", &[]);
        assert_eq!(out, "token=[redacted];\t");
    }

    // -- Fix round: I1 — the `;` split must not destroy prose evidence in the record ------
    // Review finding: `redact_body(Some("text/plain"), "request failed; token=abc123 was
    // rejected by upstream")` returned `"request failed; token=[redacted]"`, silently deleting
    // "was rejected by upstream" from the append-only record, while the doc comment on
    // `redact_kv_body` asserted a prose sentence "never enters this branch at all". Fix: a
    // segment's *value* half (after `=`) must be whitespace-free to be treated as a genuine
    // form/cookie pair — a real urlencoded or cookie value never contains unencoded whitespace,
    // but prose does. This keeps every m15/m17 pin green (all of their values are
    // whitespace-free) while excluding prose.

    #[test]
    fn redact_body_prose_with_semicolon_and_sensitive_looking_kv_is_not_mangled() {
        let prose = "request failed; token=abc123 was rejected by upstream";
        let out = redact_body(Some("text/plain"), prose, &[]);
        assert_eq!(out, prose, "prose must pass through byte-identical, not be torn apart at the first `;`");
    }

    #[test]
    fn redact_body_kv_value_containing_whitespace_is_not_treated_as_kv_shaped() {
        // Direct pin on the whitespace-free-value rule: a segment whose value contains a space
        // is not a genuine form/cookie pair even though its key is sensitive-looking.
        let out = redact_body(Some("text/plain"), "token=abc def", &[]);
        assert_eq!(out, "token=abc def", "value containing whitespace is not kv-shaped, must not be masked");
    }
}
