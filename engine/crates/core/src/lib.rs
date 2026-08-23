//! Domain objects for the repair loop (spec §3): Change, Failure, and the
//! claims that carry provenance. Stage 1 covers detect → attribute → reproduce.

use serde::{Deserialize, Serialize};

pub mod authority;
pub mod bet;
pub mod change;
pub mod evaluation;
pub mod hypothesis;
pub mod observation;

/// The five provenance states (spec §1). The product's trust vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provenance {
    Verified,
    Observed,
    Inferred,
    Approved,
    Unresolved,
}

impl Provenance {
    pub fn chip(&self) -> &'static str {
        match self {
            Provenance::Verified => "verified",
            Provenance::Observed => "observed",
            Provenance::Inferred => "inferred",
            Provenance::Approved => "approved",
            Provenance::Unresolved => "unresolved",
        }
    }
}

/// An assertion with a provenance state (spec §3 "Claim").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claim {
    pub text: String,
    pub provenance: Provenance,
}

/// A deploy and the diff behind it (spec §3 "Change"). Posted by the deploy
/// hook; `changed_files` is computed by the engine from git, never trusted
/// from the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployRecord {
    pub sha: String,
    pub description: String,
    pub author: String,
    pub deployed_at_ms: u64,
}

/// The replayable request snapshot captured with an error.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapturedRequest {
    pub method: String,
    pub path: String,
    pub content_type: Option<String>,
    pub body: Option<String>,
}

/// The source string [`Intake::resolve`] stamps when a declared
/// [`Intake::Snippet`] turns out to carry no replayable request — including a
/// legacy record line written before this field existed. Deliberately not a
/// guess at which adapter it was: what is actually known is "something opened
/// this failure and no request came with it".
pub const UNKNOWN_INTAKE_SOURCE: &str = "unknown";

/// How a failure entered the loop — and therefore what the engine is even
/// allowed to attempt on it (spec §1 provenance chips, §9 reproduction, §11
/// the autonomy ladder).
///
/// THE LOAD-BEARING DISTINCTION of the whole product: reproduction replays the
/// ACTUAL failing request against the rebuilt revision. That replay is the only
/// thing that earns `verified`. So intake sources come in exactly two kinds, and
/// the split lives *here*, in the type system, rather than in a convention an
/// adapter can forget:
///
/// - **Replayable** — [`Intake::Snippet`]: arrived through the reporting
///   snippet with a real [`CapturedRequest`] (method + path + body). Can reach
///   `verified`; can be eligible to act alone.
/// - **Trigger-only** — [`Intake::Trigger`] and [`Intake::Reported`]: something
///   OPENED a failure but carried no replayable request (an OTel span, a log
///   alert, a human filing an issue). Reaches at most `observed`; reproduction
///   must be SKIPPED rather than faked by synthesizing a body; and it can
///   NEVER auto-ship, whatever rung its class has earned
///   ([`authority::ship_decision`]).
///
/// Anything that blurs the two is a false-`verified` path, which is the cardinal
/// sin of this product.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Intake {
    /// The reporting snippet in the monitored app posted an error handler's
    /// capture: stack plus the request that produced it. The ONLY replayable
    /// kind today.
    Snippet,
    /// Telemetry or an alert opened the failure (OTel span, PostHog, HyperDX).
    /// `source` names which one.
    Trigger { source: String },
    /// A human said so (Linear, Agentation). `source` names where.
    Reported { source: String },
}

impl Intake {
    /// True ONLY for [`Intake::Snippet`]. This is the predicate the reproduction
    /// step and the ship gate both hang off; it must never grow a second `true`
    /// arm without a replay path to back it.
    pub fn is_replayable(&self) -> bool {
        matches!(self, Intake::Snippet)
    }

    /// Which named source opened the failure, for claim text and narration.
    pub fn source(&self) -> &str {
        match self {
            Intake::Snippet => "snippet",
            Intake::Trigger { source } | Intake::Reported { source } => source,
        }
    }

    /// Short human label: `snippet`, `trigger: hyperdx`, `reported: linear`.
    pub fn label(&self) -> String {
        match self {
            Intake::Snippet => "snippet".to_string(),
            Intake::Trigger { source } => format!("trigger: {source}"),
            Intake::Reported { source } => format!("reported: {source}"),
        }
    }

    /// Reconcile a DECLARED intake against whether a replayable request
    /// actually arrived. This only ever DOWNGRADES — there is no path here from
    /// trigger-only to replayable:
    ///
    /// - a declared `Snippet` with no request is not a snippet; it becomes
    ///   `Trigger { source: "unknown" }`, because a claim of replayability
    ///   nothing can back is exactly the false-`verified` path the taxonomy
    ///   exists to close;
    /// - a declared `Trigger`/`Reported` stays trigger-only even if a partial
    ///   request happens to be attached. An adapter can often reconstruct a
    ///   method and a path from span attributes; that is not the failing
    ///   request, and replaying it would prove nothing.
    ///
    /// Every construction site of an intake on a [`Failure`] goes through this,
    /// including deserialization (see [`Failure`]'s `serde(from)`).
    pub fn resolve(declared: Intake, has_request: bool) -> Intake {
        match declared {
            Intake::Snippet if !has_request => Intake::Trigger {
                source: UNKNOWN_INTAKE_SOURCE.to_string(),
            },
            other => other,
        }
    }

    /// The `unresolved` claim the engine emits INSTEAD of a reproduction when
    /// the intake carries no replayable request. Reproduction is not attempted
    /// and the record says so in as many words — never a synthesized replay,
    /// and never silence.
    pub fn no_replay_claim(&self) -> Claim {
        Claim {
            text: format!(
                "no replayable request captured for this {} failure — reproduction not attempted",
                self.source()
            ),
            provenance: Provenance::Unresolved,
        }
    }
}

/// One error report from the monitored app. Carries NO deploy id — attribution
/// is the engine's job (spec §22: "attribution is real, not a guess").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorEvent {
    pub service: String,
    pub occurred_at_ms: u64,
    pub error_name: String,
    pub error_message: String,
    pub stack: String,
    /// The replayable request, when one was captured. `None` for a
    /// trigger/reported event: an OTel span or a Linear issue opens a failure
    /// without one, and the honest representation of that is an absent request,
    /// not a fabricated one.
    ///
    /// `skip_serializing_if` keeps a snippet event's record line byte-identical
    /// to what it was before this became an `Option` (`Some(r)` serializes
    /// exactly as `r` did), and omits the key entirely rather than writing
    /// `"request":null` when there is nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<CapturedRequest>,
    /// What the posting adapter DECLARES this event's intake to be. Absent on
    /// the reporting snippet's payload (the snippet predates the taxonomy and
    /// is the replayable kind by definition), so it defaults to
    /// [`Intake::Snippet`] — safely, because [`Intake::resolve`] downgrades a
    /// declared snippet with no `request` to `Trigger { source: "unknown" }`.
    /// Every NON-snippet adapter must set this explicitly.
    #[serde(default = "declared_intake_default")]
    pub intake: Intake,
}

/// serde default for [`ErrorEvent::intake`] — see that field's doc comment for
/// why `Snippet` is the safe default and where it gets re-checked. Deliberately
/// a named function rather than a `Default` impl on [`Intake`]: nothing should
/// be able to acquire a replayable-looking intake from `..Default::default()`.
fn declared_intake_default() -> Intake {
    Intake::Snippet
}

/// Normalized identity of an error class: name + first application frame.
/// Line numbers are deliberately excluded (they shift between revisions).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ErrorSignature {
    pub error_name: String,
    pub top_frame_file: String,
    pub top_frame_function: Option<String>,
}

impl ErrorSignature {
    /// Parse a stack trace, detecting CPython vs V8 format. `app_root` strips
    /// the deployment prefix so signatures are stable across checkouts.
    /// Falls back to an empty file if no application frame exists.
    pub fn from_error(error_name: &str, _message: &str, stack: &str, app_root: &str) -> Self {
        // V8 stacks carry the exception message on line 0 and every frame as
        // a `    at ...` line beneath it. Prefer that anchor whenever it's
        // present, even if the message elsewhere happens to contain literal
        // `File "..."` text -- e.g. a Node service that re-throws a Python
        // worker's captured stderr, the polyglot case this lane exists to
        // serve. Routing that to the Python parser on the old unanchored
        // `contains("File \"")` check regressed the working V8 path (M2).
        let has_v8_frame = stack
            .lines()
            .any(|line| line.trim_start().starts_with("at "));
        if has_v8_frame {
            return Self::from_v8_stack(error_name, stack, app_root);
        }
        // Only take the Python branch when an anchored frame header --
        // `File "<path>", line <N>` -- is actually present. The bare
        // substring `File "` is not enough: a message-embedded
        // pseudo-traceback (subprocess/celery/ExceptionGroup output quoted
        // inside the final exception message) can contain that text without
        // being a real frame, and would otherwise out-scan the genuine one
        // (M5 -- same anchoring fix as M2).
        if stack
            .lines()
            .any(|line| Self::parse_python_frame_line(line).is_some())
        {
            Self::from_python_traceback(error_name, stack, app_root)
        } else {
            Self::from_v8_stack(error_name, stack, app_root)
        }
    }

    /// Parse a V8-style stack trace. Frames from `node:` internals and
    /// `node_modules/` are skipped; the frame order is innermost-first, so
    /// the first surviving frame wins.
    fn from_v8_stack(error_name: &str, stack: &str, app_root: &str) -> Self {
        for line in stack.lines().skip(1) {
            let line = line.trim();
            let Some(rest) = line.strip_prefix("at ") else {
                continue;
            };
            if rest.contains("node:") || rest.contains("node_modules/") {
                continue;
            }
            // Forms: "fn (/path/file.js:1:2)" or "/path/file.js:1:2"
            let (func, loc) = match rest.split_once(" (") {
                Some((f, l)) => (Some(f.trim().to_string()), l.trim_end_matches(')')),
                None => (None, rest),
            };
            let file = loc.rsplitn(3, ':').nth(2).unwrap_or(loc);
            return ErrorSignature {
                error_name: error_name.to_string(),
                top_frame_file: Self::apply_app_root(file, app_root),
                top_frame_function: func,
            };
        }
        Self::empty(error_name)
    }

    /// Parse a CPython traceback. Frames appear OUTERMOST-first (the reverse
    /// of V8), so the most relevant application frame is the LAST
    /// `File "..."` line. Library frames (site-packages, dist-packages, the
    /// stdlib, frozen/importlib machinery) are skipped; the first
    /// application frame scanning FROM THE END wins.
    fn from_python_traceback(error_name: &str, stack: &str, app_root: &str) -> Self {
        let frames: Vec<(&str, Option<&str>)> = stack
            .lines()
            .filter_map(Self::parse_python_frame_line)
            .collect();

        for (file, func) in frames.into_iter().rev() {
            if Self::is_python_library_frame(file) {
                continue;
            }
            let func = func.filter(|f| *f != "<module>").map(|f| f.to_string());
            return ErrorSignature {
                error_name: error_name.to_string(),
                top_frame_file: Self::apply_app_root(file, app_root),
                top_frame_function: func,
            };
        }
        Self::empty(error_name)
    }

    /// Parses a trimmed traceback frame header:
    /// `File "/path/to/file.py", line 42, in func_name`
    /// into `(path, Some("func_name"))`. Returns `None` for lines that
    /// aren't frame headers (code-context lines, the `Traceback (...)`
    /// banner, the final exception line).
    ///
    /// The `, line <N>` component right after the closing quote is
    /// mandatory: it is what distinguishes a real anchored frame header
    /// from message-embedded text that merely starts with `File "<path>"`
    /// (M5) -- e.g. a build tool or subprocess echoing a `File "..."`
    /// reference of its own inside the final exception message, with no
    /// traceback frame behind it. `, in <func>` stays optional: module-level
    /// frames spell it `in <module>`, but nothing in the CPython grammar
    /// guarantees a function name follows the line number.
    fn parse_python_frame_line(line: &str) -> Option<(&str, Option<&str>)> {
        let line = line.trim();
        let rest = line.strip_prefix("File \"")?;
        let (path, rest) = rest.split_once('"')?;
        let rest = rest.strip_prefix(", line ")?;
        let digit_count = rest.chars().take_while(|c| c.is_ascii_digit()).count();
        if digit_count == 0 {
            return None;
        }
        let func = rest[digit_count..].strip_prefix(", in ").map(|f| f.trim());
        Some((path, func))
    }

    /// Mirrors the JS `node:`/`node_modules/` skip rules for CPython:
    /// pseudo-files, third-party packages, the standard library, and
    /// importlib's bootstrap frames are never the application frame.
    fn is_python_library_frame(path: &str) -> bool {
        // Any pseudo-file path -- `<string>`, `<stdin>`, `<frozen ...>`, and
        // anything else CPython synthesizes for exec()/eval()/frozen
        // imports/the REPL -- is never real application source. Two
        // unrelated failures that happen to raise inside the same generated
        // code (e.g. two different dataclasses' `__setattr__`, both
        // reported as `File "<string>", line 4, in __setattr__`) must not
        // collapse to one signature (M3).
        if path.starts_with('<') {
            return true;
        }
        path.contains("/site-packages/")
            || path.contains("/dist-packages/")
            || Self::is_stdlib_python_path(path)
            || Self::is_importlib_frame(path)
    }

    /// Matches the interpreter's own standard-library layout --
    /// `/lib/python<version>/...` or `/lib64/python<version>/...`, e.g.
    /// `/usr/local/lib/python3.11/...` -- anchored on a digit immediately
    /// after `python` so a real application directory that merely contains
    /// the literal path segment `lib/python/` (a legitimate polyglot
    /// layout, e.g. `<repo>/lib/python/dispatch.py`) is not mistaken for
    /// the stdlib (M4).
    fn is_stdlib_python_path(path: &str) -> bool {
        ["/lib/python", "/lib64/python"].into_iter().any(|marker| {
            path.match_indices(marker).any(|(idx, _)| {
                path[idx + marker.len()..].starts_with(|c: char| c.is_ascii_digit())
            })
        })
    }

    /// `importlib` only counts as bootstrap machinery when it is a real path
    /// component (e.g. `.../importlib/_bootstrap_external.py`) -- a bare
    /// substring match also fires on an application file that merely
    /// happens to be *named* with that word, like `api/importlib_compat.py`
    /// (M4). `<frozen importlib...>`-style frames are already caught above
    /// by the pseudo-file check, since they start with `<`.
    fn is_importlib_frame(path: &str) -> bool {
        path.split('/').any(|segment| segment == "importlib")
    }

    /// Strips a `file://` scheme (ESM stacks) and then the `app_root`
    /// deployment prefix, shared by both parsers so signatures are stable
    /// across checkouts regardless of source language.
    fn apply_app_root(file: &str, app_root: &str) -> String {
        let file = file.strip_prefix("file://").unwrap_or(file);
        file.strip_prefix(app_root)
            .map(|s| s.trim_start_matches('/'))
            .unwrap_or(file)
            .to_string()
    }

    fn empty(error_name: &str) -> Self {
        ErrorSignature {
            error_name: error_name.to_string(),
            top_frame_file: String::new(),
            top_frame_function: None,
        }
    }

    pub fn matches(&self, other: &ErrorSignature) -> bool {
        // An empty `top_frame_file` means no application frame was found (an
        // all-node_modules stack, a missing `stack` field, ...). Two such
        // signatures must never be considered a match on error_name alone —
        // that degeneration is exactly what lets a reproduction claim
        // `verified` for a 500 whose stack was never actually compared.
        if self.top_frame_file.is_empty() || other.top_frame_file.is_empty() {
            return false;
        }
        self.error_name == other.error_name && self.top_frame_file == other.top_frame_file
    }
}

/// Something wrong in a running application, with the evidence that says so
/// (spec §3 "Failure").
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(from = "FailureWire")]
pub struct Failure {
    pub id: String,
    pub service: String,
    pub signature: ErrorSignature,
    pub first_seen_ms: u64,
    pub event_count: usize,
    /// The most recent event seen for this signature. Its `request` is the
    /// replay candidate — but only [`Failure::replayable_request`] may be used
    /// to reach for it, since a request being present is not on its own
    /// permission to replay it (see [`Intake`]).
    pub sample: ErrorEvent,
    /// How this failure entered the loop. Decides whether reproduction is even
    /// attemptable and whether the failure can ever ship alone.
    pub intake: Intake,
    pub claim: Claim, // provenance: Observed
}

impl Failure {
    /// The request to replay against a rebuilt revision, or `None`.
    ///
    /// `Some` requires BOTH that the intake is replayable AND that a request was
    /// actually captured — the two conditions that together are what let a claim
    /// reach `verified`. Callers must not read `sample.request` directly; going
    /// through here is what makes "trigger-only failures are never replayed" a
    /// property of the type rather than of every call site remembering.
    pub fn replayable_request(&self) -> Option<&CapturedRequest> {
        if !self.intake.is_replayable() {
            return None;
        }
        self.sample.request.as_ref()
    }
}

/// Deserialization shape for [`Failure`]. Exists for exactly one reason: a
/// record line written before `intake` existed has no such field, and the
/// default cannot be a constant — it depends on the rest of the line.
#[derive(Deserialize)]
struct FailureWire {
    id: String,
    service: String,
    signature: ErrorSignature,
    first_seen_ms: u64,
    event_count: usize,
    sample: ErrorEvent,
    #[serde(default)]
    intake: Option<Intake>,
    claim: Claim,
}

impl From<FailureWire> for Failure {
    fn from(w: FailureWire) -> Self {
        // LEGACY DEFAULT (stated out loud because it is a trust decision, not a
        // serde convenience): when a record line lacks `intake`, it is
        // `Snippet` only if a request is actually present on the sample,
        // otherwise `Trigger { source: "unknown" }`. Every line written before
        // this field existed came from the reporting snippet, which always
        // carried a request — so a legacy line WITH a request is genuinely
        // replayable and keeps its old behavior byte for byte; a line without
        // one cannot retroactively be treated as replayable just because it is
        // old. `Intake::resolve` also re-checks an intake that WAS present, so
        // deserialization can never reintroduce a snippet-without-request.
        let has_request = w.sample.request.is_some();
        let intake = Intake::resolve(w.intake.unwrap_or(Intake::Snippet), has_request);
        Failure {
            id: w.id,
            service: w.service,
            signature: w.signature,
            first_seen_ms: w.first_seen_ms,
            event_count: w.event_count,
            sample: w.sample,
            intake,
            claim: w.claim,
        }
    }
}

/// The engine's answer to "which deploy caused this" (inferred until reproduced).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attribution {
    pub deploy: DeployRecord,
    pub overlap_files: Vec<String>,
    pub minutes_after_deploy: u64,
    pub claim: Claim, // provenance: Inferred
}

/// Outcome of rebuilding the revision and replaying the request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reproduction {
    pub sha: String,
    pub reproduced: bool,
    /// Whether the parent commit serves the same request cleanly. `true` is
    /// what upgrades the attribution from a guess to something actionable
    /// (spec §10).
    pub parent_clean: Option<bool>,
    pub detail: String,
    pub claims: Vec<Claim>, // provenance: Verified on success
}

/// A repair the agent produced for a failure: a committed diff on its own
/// branch, plus the claims earned while verifying it (spec §17 "git is the
/// record").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Repair {
    pub id: String,
    pub failure_id: String,
    pub sha: String,
    pub branch: String,
    pub agent: String,
    pub summary: String,
    pub diff_stat: String,
    pub claims: Vec<Claim>,
}

/// The captured request behind a repair, persisted as its own record line
/// (kind `repair_context`) alongside `repair_ready` — deliberately separate
/// from [`Repair`] itself so Task 1/3's shape stays unchanged. `drums ship`
/// / `drums revert` (spec §19) run as their own process with only the
/// append-only record to work from; this is what lets a standalone ship
/// replay the exact request that was originally failing against the
/// deployed instance, the same one `Repair`'s own verification step already
/// proved fixed inside the repair worktree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepairSample {
    pub failure_id: String,
    pub request: CapturedRequest,
}

/// The record of a ship or revert action taken against a repair (spec §19:
/// record-driven `drums ship` / `drums revert`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShipOutcome {
    pub failure_id: String,
    pub repair_sha: String,
    /// "shipped" | "reverted"
    pub action: String,
    pub deploy_cmd: String,
    pub claims: Vec<Claim>,
}

/// A human-reported issue intake from an external tool (Agentation, Linear) —
/// recorded intake, honest about what it is: a human said so, nothing more.
/// Deliberately does NOT enter the failure/repair pipeline yet — there is no
/// attribution, no reproduction, no verification behind it, only the single
/// `Observed` claim that it was reported (real-world-scenarios plan, §C).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReportedIssue {
    /// Drums' own id for this intake, minted at ingest. Ours, stable, and what
    /// every downstream event and record line keys on — see `external_id` for
    /// why it is not the provider's id.
    pub id: String,
    /// "agentation" | "linear"
    pub source: String,
    /// The provider's OWN identifier for the issue this came from: Linear's
    /// GraphQL UUID, Agentation's report id. Not `ENG-123` — see
    /// `external_identifier`.
    ///
    /// Without this a repair has nowhere to answer. Closing the loop means
    /// commenting on the thread the person started, and a tracker addresses a
    /// comment by its own id; `id` above is a ULID Drums minted, which names
    /// nothing on the other side, so a write against it goes to no issue at
    /// all. That is not a hypothetical — it is what
    /// `engine-cli::repair_reported` did before this field existed.
    ///
    /// The obvious alternative — put the provider's id in `id` and drop ours —
    /// breaks three things at once: the record, the CLI events and the console
    /// all key on `id` and expect it to be a Drums identifier; an Agentation
    /// report may carry no id to borrow, leaving the intake unnameable; and two
    /// providers are free to hand us the same string, so `id` would stop being
    /// unique the day a second tracker is wired.
    ///
    /// `None` when the payload carried nothing that could be trusted as an id.
    /// Never fabricated and never filled from `id`: a repair that cannot
    /// address the issue it came from must stay quiet rather than write
    /// somewhere else. Record lines written before this field existed
    /// deserialize as `None`, which says exactly that.
    pub external_id: Option<String>,
    /// The identifier a PERSON recognises — Linear's `ENG-123`.
    ///
    /// Never used to address a write. Linear's API takes the UUID, and this
    /// string is a display name that changes when an issue moves team; using it
    /// to route a comment would work until somebody reorganised. It is carried
    /// so a human reading a record line or a narration knows which issue is
    /// being talked about, which `external_id` on its own cannot tell them.
    pub external_identifier: Option<String>,
    pub title: String,
    pub body_excerpt: String,
    pub url: Option<String>,
    /// The raw webhook payload, redacted before it's persisted to the record
    /// (same discipline as `ErrorEvent.request.body` — see `engine-ingest`).
    pub payload: serde_json::Value,
    /// Exactly ONE `Observed` claim ("a human said so") — modeled as a
    /// singular field, not `Vec<Claim>`, so no caller (render, a future
    /// adapter) has to assume/enforce a non-empty vec by convention.
    pub claim: Claim,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_from_stack_takes_error_name_and_first_app_frame() {
        let sig = ErrorSignature::from_error(
            "TypeError",
            "Cannot read properties of undefined (reading 'code')",
            "TypeError: Cannot read properties of undefined (reading 'code')\n    at computeTotal (/srv/shop/lib/cart/total.js:14:31)\n    at Server.handle (/srv/shop/server.js:40:9)\n    at node:internal/x:1:1",
            "/srv/shop",
        );
        assert_eq!(sig.error_name, "TypeError");
        assert_eq!(sig.top_frame_file, "lib/cart/total.js");
        assert_eq!(sig.top_frame_function.as_deref(), Some("computeTotal"));
    }

    #[test]
    fn signature_skips_node_internal_frames() {
        let sig = ErrorSignature::from_error(
            "TypeError",
            "boom",
            "TypeError: boom\n    at node:internal/streams/readable:1:1\n    at handler (/srv/shop/server.js:22:5)",
            "/srv/shop",
        );
        assert_eq!(sig.top_frame_file, "server.js");
    }

    #[test]
    fn signature_skips_node_modules_vendor_frames() {
        let sig = ErrorSignature::from_error(
            "TypeError",
            "boom",
            "TypeError: boom\n    at wrap (/srv/shop/node_modules/express/lib/router.js:5:1)\n    at handler (/srv/shop/server.js:22:5)",
            "/srv/shop",
        );
        assert_eq!(sig.top_frame_file, "server.js");
    }

    #[test]
    fn signature_strips_file_scheme_from_esm_frames() {
        let sig = ErrorSignature::from_error(
            "TypeError",
            "boom",
            "TypeError: boom\n    at handler (file:///srv/shop/server.js:22:5)",
            "/srv/shop",
        );
        assert_eq!(sig.top_frame_file, "server.js");
    }

    #[test]
    fn signature_match_ignores_message_and_line_numbers() {
        let a = ErrorSignature {
            error_name: "TypeError".into(),
            top_frame_file: "lib/cart/total.js".into(),
            top_frame_function: Some("computeTotal".into()),
        };
        let b = ErrorSignature {
            error_name: "TypeError".into(),
            top_frame_file: "lib/cart/total.js".into(),
            top_frame_function: Some("computeTotal".into()),
        };
        assert!(a.matches(&b));
        let c = ErrorSignature {
            error_name: "RangeError".into(),
            ..b.clone()
        };
        assert!(!a.matches(&c));
    }

    #[test]
    fn signature_match_rejects_empty_top_frame_file_on_either_side() {
        // A signature with no application frame (all-node_modules stack, missing
        // stack field, etc.) must never match anything on error_name alone —
        // that degeneration is what lets the reproducer stamp a false `verified`.
        let a = ErrorSignature {
            error_name: "TypeError".into(),
            top_frame_file: String::new(),
            top_frame_function: None,
        };
        let b = ErrorSignature {
            error_name: "TypeError".into(),
            top_frame_file: String::new(),
            top_frame_function: None,
        };
        assert!(
            !a.matches(&b),
            "two empty-file signatures with the same error_name must not match"
        );
        let c = ErrorSignature {
            error_name: "TypeError".into(),
            top_frame_file: "server.js".into(),
            top_frame_function: None,
        };
        assert!(
            !a.matches(&c),
            "empty file on one side must not match a populated file on the other"
        );
        assert!(!c.matches(&a), "matches must be symmetric in this respect");
    }

    #[test]
    fn error_event_round_trips_through_json() {
        let ev = ErrorEvent {
            service: "shop".into(),
            occurred_at_ms: 1_753_000_000_000,
            error_name: "TypeError".into(),
            error_message: "boom".into(),
            stack: "TypeError: boom\n    at f (/x/server.js:1:1)".into(),
            request: Some(CapturedRequest {
                method: "POST".into(),
                path: "/api/checkout".into(),
                content_type: Some("application/json".into()),
                body: Some(r#"{"items":[]}"#.into()),
            }),
            intake: Intake::Snippet,
        };
        let json = serde_json::to_string(&ev).unwrap();
        let back: ErrorEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back.request.unwrap().path, "/api/checkout");
    }

    #[test]
    fn repair_sample_round_trips_through_json() {
        let sample = RepairSample {
            failure_id: "f1".into(),
            request: CapturedRequest {
                method: "POST".into(),
                path: "/api/checkout".into(),
                content_type: Some("application/json".into()),
                body: Some(r#"{"items":[]}"#.into()),
            },
        };
        let json = serde_json::to_string(&sample).unwrap();
        let back: RepairSample = serde_json::from_str(&json).unwrap();
        assert_eq!(back.failure_id, "f1");
        assert_eq!(back.request.path, "/api/checkout");
    }

    // -- intake taxonomy ------------------------------------------------------

    fn snippet_request() -> CapturedRequest {
        CapturedRequest {
            method: "POST".into(),
            path: "/api/checkout".into(),
            content_type: Some("application/json".into()),
            body: Some(r#"{"items":[]}"#.into()),
        }
    }

    fn failure_with(intake: Intake, request: Option<CapturedRequest>) -> Failure {
        Failure {
            id: "f1".into(),
            service: "shop".into(),
            signature: ErrorSignature {
                error_name: "TypeError".into(),
                top_frame_file: "server.js".into(),
                top_frame_function: None,
            },
            first_seen_ms: 1,
            event_count: 3,
            sample: ErrorEvent {
                service: "shop".into(),
                occurred_at_ms: 1,
                error_name: "TypeError".into(),
                error_message: "boom".into(),
                stack: "TypeError: boom\n    at f (/x/server.js:1:1)".into(),
                request,
                intake: intake.clone(),
            },
            intake,
            claim: Claim {
                text: "3 errors".into(),
                provenance: Provenance::Observed,
            },
        }
    }

    #[test]
    fn only_snippet_intake_is_replayable() {
        assert!(Intake::Snippet.is_replayable());
        assert!(!Intake::Trigger {
            source: "hyperdx".into()
        }
        .is_replayable());
        assert!(!Intake::Reported {
            source: "linear".into()
        }
        .is_replayable());
    }

    #[test]
    fn intake_names_its_source_and_label() {
        assert_eq!(Intake::Snippet.source(), "snippet");
        assert_eq!(
            Intake::Trigger {
                source: "otel".into()
            }
            .source(),
            "otel"
        );
        assert_eq!(
            Intake::Reported {
                source: "linear".into()
            }
            .source(),
            "linear"
        );
        assert_eq!(Intake::Snippet.label(), "snippet");
        assert_eq!(
            Intake::Trigger {
                source: "otel".into()
            }
            .label(),
            "trigger: otel"
        );
        assert_eq!(
            Intake::Reported {
                source: "linear".into()
            }
            .label(),
            "reported: linear"
        );
    }

    #[test]
    fn resolve_downgrades_a_declared_snippet_that_carries_no_request() {
        // A claim of replayability nothing can back is the false-`verified`
        // path; there is no honest source name for it, so it is "unknown".
        assert_eq!(
            Intake::resolve(Intake::Snippet, false),
            Intake::Trigger {
                source: UNKNOWN_INTAKE_SOURCE.into()
            }
        );
        assert_eq!(Intake::resolve(Intake::Snippet, true), Intake::Snippet);
    }

    #[test]
    fn resolve_never_upgrades_a_trigger_or_reported_intake_even_with_a_request() {
        // An adapter can often reconstruct a method and a path from span
        // attributes. That is not the failing request, and replaying it would
        // prove nothing — so an attached request must not buy replayability.
        let t = Intake::Trigger {
            source: "hyperdx".into(),
        };
        assert_eq!(Intake::resolve(t.clone(), true), t);
        let r = Intake::Reported {
            source: "linear".into(),
        };
        assert_eq!(Intake::resolve(r.clone(), true), r);
    }

    #[test]
    fn replayable_request_is_some_only_for_a_snippet_failure_that_has_one() {
        assert!(failure_with(Intake::Snippet, Some(snippet_request()))
            .replayable_request()
            .is_some());
        assert!(failure_with(Intake::Snippet, None)
            .replayable_request()
            .is_none());
        // The dangerous case: a trigger failure that happens to carry a
        // reconstructed request must NOT hand it out as a replay candidate.
        assert!(
            failure_with(Intake::Trigger { source: "hyperdx".into() }, Some(snippet_request())).replayable_request().is_none(),
            "a trigger-intake failure must never yield a replay candidate, even with a request attached"
        );
        assert!(failure_with(
            Intake::Reported {
                source: "linear".into()
            },
            Some(snippet_request())
        )
        .replayable_request()
        .is_none());
    }

    #[test]
    fn no_replay_claim_is_unresolved_and_says_reproduction_was_not_attempted() {
        let c = Intake::Trigger {
            source: "hyperdx".into(),
        }
        .no_replay_claim();
        assert_eq!(c.provenance, Provenance::Unresolved);
        assert_eq!(
            c.text,
            "no replayable request captured for this hyperdx failure — reproduction not attempted"
        );
    }

    #[test]
    fn intake_round_trips_as_a_tagged_enum() {
        assert_eq!(
            serde_json::to_string(&Intake::Snippet).unwrap(),
            r#"{"kind":"snippet"}"#
        );
        assert_eq!(
            serde_json::to_string(&Intake::Trigger {
                source: "hyperdx".into()
            })
            .unwrap(),
            r#"{"kind":"trigger","source":"hyperdx"}"#
        );
        assert_eq!(
            serde_json::to_string(&Intake::Reported {
                source: "linear".into()
            })
            .unwrap(),
            r#"{"kind":"reported","source":"linear"}"#
        );
        let back: Intake =
            serde_json::from_str(r#"{"kind":"reported","source":"agentation"}"#).unwrap();
        assert_eq!(
            back,
            Intake::Reported {
                source: "agentation".into()
            }
        );
    }

    /// A snippet event's JSON is byte-identical to what it was before `request`
    /// became an `Option` and `intake` existed: `Some(r)` serializes exactly as
    /// `r` did, and `intake` is the one added key.
    #[test]
    fn snippet_error_event_keeps_its_request_shape_on_the_wire() {
        let ev = ErrorEvent {
            service: "shop".into(),
            occurred_at_ms: 1,
            error_name: "TypeError".into(),
            error_message: "boom".into(),
            stack: "s".into(),
            request: Some(snippet_request()),
            intake: Intake::Snippet,
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(
            json.contains(r#""request":{"method":"POST","path":"/api/checkout""#),
            "{json}"
        );
        assert!(json.contains(r#""intake":{"kind":"snippet"}"#), "{json}");
    }

    /// A trigger event omits the key entirely rather than writing
    /// `"request":null` — there is nothing to write.
    #[test]
    fn trigger_error_event_omits_the_request_key_entirely() {
        let ev = ErrorEvent {
            service: "shop".into(),
            occurred_at_ms: 1,
            error_name: "TypeError".into(),
            error_message: "boom".into(),
            stack: "s".into(),
            request: None,
            intake: Intake::Trigger {
                source: "hyperdx".into(),
            },
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(!json.contains("request"), "{json}");
        assert!(!json.contains("null"), "{json}");
    }

    /// The snippet's own payload has no `intake` key — it predates the taxonomy.
    #[test]
    fn a_snippet_payload_without_an_intake_key_deserializes_as_snippet() {
        let ev: ErrorEvent = serde_json::from_str(
            r#"{"service":"shop","occurred_at_ms":1,"error_name":"TypeError","error_message":"boom","stack":"s","request":{"method":"POST","path":"/api/checkout","content_type":null,"body":"{}"}}"#,
        )
        .unwrap();
        assert_eq!(ev.intake, Intake::Snippet);
        assert!(ev.request.is_some());
    }

    /// LEGACY RECORD LINE, request present: genuinely replayable, byte-for-byte
    /// the old behavior.
    #[test]
    fn a_legacy_failure_line_with_a_request_defaults_to_snippet() {
        let json = r#"{"id":"f1","service":"shop","signature":{"error_name":"TypeError","top_frame_file":"server.js","top_frame_function":null},"first_seen_ms":1,"event_count":3,"sample":{"service":"shop","occurred_at_ms":1,"error_name":"TypeError","error_message":"boom","stack":"s","request":{"method":"POST","path":"/api/checkout","content_type":null,"body":"{}"}},"claim":{"text":"3 errors","provenance":"observed"}}"#;
        let f: Failure = serde_json::from_str(json).unwrap();
        assert_eq!(f.intake, Intake::Snippet);
        assert!(f.replayable_request().is_some());
    }

    /// LEGACY RECORD LINE, no request: cannot retroactively become replayable
    /// just because it is old.
    #[test]
    fn a_legacy_failure_line_without_a_request_defaults_to_trigger_unknown() {
        let json = r#"{"id":"f1","service":"shop","signature":{"error_name":"TypeError","top_frame_file":"server.js","top_frame_function":null},"first_seen_ms":1,"event_count":3,"sample":{"service":"shop","occurred_at_ms":1,"error_name":"TypeError","error_message":"boom","stack":"s"},"claim":{"text":"3 errors","provenance":"observed"}}"#;
        let f: Failure = serde_json::from_str(json).unwrap();
        assert_eq!(
            f.intake,
            Intake::Trigger {
                source: UNKNOWN_INTAKE_SOURCE.into()
            }
        );
        assert!(f.replayable_request().is_none());
    }

    /// Deserialization re-runs `Intake::resolve`, so even a line that explicitly
    /// declares `snippet` without a request cannot smuggle replayability in.
    #[test]
    fn a_failure_line_declaring_snippet_without_a_request_is_downgraded_on_read() {
        let json = r#"{"id":"f1","service":"shop","signature":{"error_name":"TypeError","top_frame_file":"server.js","top_frame_function":null},"first_seen_ms":1,"event_count":3,"sample":{"service":"shop","occurred_at_ms":1,"error_name":"TypeError","error_message":"boom","stack":"s"},"intake":{"kind":"snippet"},"claim":{"text":"3 errors","provenance":"observed"}}"#;
        let f: Failure = serde_json::from_str(json).unwrap();
        assert_eq!(
            f.intake,
            Intake::Trigger {
                source: UNKNOWN_INTAKE_SOURCE.into()
            }
        );
        assert!(
            f.replayable_request().is_none(),
            "a declared snippet with no request must not be replayable"
        );
    }

    #[test]
    fn failure_round_trips_its_intake_through_json() {
        let f = failure_with(
            Intake::Reported {
                source: "linear".into(),
            },
            None,
        );
        let back: Failure = serde_json::from_str(&serde_json::to_string(&f).unwrap()).unwrap();
        assert_eq!(
            back.intake,
            Intake::Reported {
                source: "linear".into()
            }
        );
        assert!(back.replayable_request().is_none());
    }

    #[test]
    fn reported_issue_round_trips_through_json() {
        let issue = ReportedIssue {
            id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
            source: "agentation".into(),
            external_id: Some("42".into()),
            external_identifier: None,
            title: "button misaligned on checkout".into(),
            body_excerpt: "the submit button overlaps the price on mobile".into(),
            url: Some("https://agentation.example/issues/42".into()),
            payload: serde_json::json!({"element": "#submit-btn", "page": "/checkout"}),
            claim: Claim {
                text: "reported via agentation webhook".into(),
                provenance: Provenance::Observed,
            },
        };
        let json = serde_json::to_string(&issue).unwrap();
        let back: ReportedIssue = serde_json::from_str(&json).unwrap();
        assert_eq!(back.source, "agentation");
        assert_eq!(back.claim.provenance, Provenance::Observed);
        assert_eq!(back.payload["page"], "/checkout");
        assert_eq!(
            back.external_id.as_deref(),
            Some("42"),
            "the provider's own id must survive the record"
        );
    }

    /// Every `reported` line written before `external_id` existed is still in
    /// customers' records and still has to load. Serde reads a missing
    /// `Option` field as `None`, and `None` is the honest answer here: that
    /// intake has no provider id, so nothing may be written back for it.
    #[test]
    fn reported_issue_from_a_record_line_written_before_external_id_loads_as_none() {
        let line = r#"{"id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","source":"linear","title":"crash on submit","body_excerpt":"","url":null,"payload":{},"claim":{"text":"reported via linear webhook","provenance":"observed"}}"#;
        let back: ReportedIssue =
            serde_json::from_str(line).expect("an older record line must still load");
        assert_eq!(back.external_id, None);
        assert_eq!(back.external_identifier, None);
    }

    #[test]
    fn provenance_renders_lowercase_chip_names() {
        assert_eq!(Provenance::Verified.chip(), "verified");
        assert_eq!(Provenance::Observed.chip(), "observed");
        assert_eq!(Provenance::Inferred.chip(), "inferred");
        assert_eq!(Provenance::Approved.chip(), "approved");
        assert_eq!(Provenance::Unresolved.chip(), "unresolved");
    }

    // -- Python (CPython) traceback support -------------------------------
    //
    // CPython lists frames OUTERMOST-FIRST (opposite of V8), so the most
    // relevant application frame is the LAST `File "..."` line, not the
    // first. FastAPI/uvicorn/starlette wrap almost every app frame in
    // several library frames that must be skipped.

    #[test]
    fn python_traceback_fastapi_picks_last_application_frame() {
        let stack = "Traceback (most recent call last):\n  File \"/usr/local/lib/python3.11/site-packages/uvicorn/protocols/http/httptools_impl.py\", line 411, in run_asgi\n    result = await app(\n  File \"/usr/local/lib/python3.11/site-packages/starlette/applications.py\", line 113, in __call__\n    await self.middleware_stack(scope, receive, send)\n  File \"/usr/local/lib/python3.11/site-packages/starlette/routing.py\", line 718, in __call__\n    await route.handle(scope, receive, send)\n  File \"/usr/local/lib/python3.11/site-packages/fastapi/routing.py\", line 274, in app\n    raw_response = await run_endpoint_function(\n  File \"/app/api/app/routes/quotes.py\", line 42, in create_quote\n    return quote.total / quote.count\nZeroDivisionError: division by zero";
        let sig =
            ErrorSignature::from_error("ZeroDivisionError", "division by zero", stack, "/app/api");
        assert_eq!(sig.top_frame_file, "app/routes/quotes.py");
        assert_eq!(sig.top_frame_function.as_deref(), Some("create_quote"));
    }

    #[test]
    fn python_traceback_all_library_frames_yields_empty_file() {
        // site-packages, dist-packages, stdlib, and frozen importlib frames
        // all filtered out -- no application frame exists in this trace, so
        // the honesty guard must produce an empty file (never a false match).
        let stack = "Traceback (most recent call last):\n  File \"/usr/local/lib/python3.11/site-packages/starlette/routing.py\", line 718, in __call__\n    await route.handle(scope, receive, send)\n  File \"/usr/lib/python3/dist-packages/gunicorn/workers/sync.py\", line 134, in handle\n    self.handle_request(listener, req, client, addr)\n  File \"/usr/local/lib/python3.11/socketserver.py\", line 316, in _handle_request_noblock\n    self.process_request(request, client_address)\n  File \"<frozen importlib._bootstrap>\", line 219, in _call_with_frames_removed\n    pass\nRuntimeError: boom";
        let sig = ErrorSignature::from_error("RuntimeError", "boom", stack, "/app/api");
        assert_eq!(sig.top_frame_file, "");
        // Re-affirm the existing honesty guard for the Python path too: two
        // signatures with no application frame must never match.
        let other = ErrorSignature::from_error("RuntimeError", "boom", stack, "/app/api");
        assert!(!sig.matches(&other));
    }

    #[test]
    fn python_traceback_module_frame_maps_to_none_function() {
        let stack = "Traceback (most recent call last):\n  File \"/app/api/main.py\", line 5, in <module>\n    start_app()\nNameError: name 'start_app' is not defined";
        let sig = ErrorSignature::from_error(
            "NameError",
            "name 'start_app' is not defined",
            stack,
            "/app/api",
        );
        assert_eq!(sig.top_frame_file, "main.py");
        assert_eq!(sig.top_frame_function, None);
    }

    #[test]
    fn python_traceback_garbage_and_mixed_input_yields_empty_file() {
        // No `File "` lines and no `at ` V8 frames either -- must fall
        // through to an empty signature rather than panic or misparse.
        let sig = ErrorSignature::from_error(
            "Error",
            "boom",
            "not a real stack trace\njust noise",
            "/app/api",
        );
        assert_eq!(sig.top_frame_file, "");
        assert_eq!(sig.top_frame_function, None);
    }

    // -- Review findings M2/M3/M4/M5 (py-traceback-review.md) -------------

    #[test]
    fn v8_stack_wins_over_embedded_python_style_text_m2() {
        // A Node service re-throwing a Python worker's captured stderr: the
        // message contains literal `File "..."` text from the child
        // process, but real `at ` V8 frames are also present. Format
        // detection must prefer V8 whenever any `at ` frame anchor exists
        // -- unanchored `contains("File \"")` regresses this, the working
        // polyglot path (M2).
        let stack = "Error: worker exited\n    at spawnWorker (/srv/app/lib/worker.js:12:9)\n    at Server.handle (/srv/app/server.js:40:9)\nPython stderr was:\n  File \"/opt/worker/run.py\", line 5, in main\n    raise RuntimeError(\"boom\")";
        let sig = ErrorSignature::from_error("Error", "worker exited", stack, "/srv/app");
        assert_eq!(sig.top_frame_file, "lib/worker.js");
        assert_eq!(sig.top_frame_function.as_deref(), Some("spawnWorker"));
    }

    #[test]
    fn python_traceback_message_embedded_pseudo_frame_does_not_hijack_signature_m5() {
        // The real traceback's last application frame is
        // `app/routes/quote.py` / `create_quote`. The exception's own
        // (multi-line) message separately echoes a build-tool style `File
        // "<path>"` reference with no `, line <N>` after it -- a
        // message-embedded pseudo-frame, not a real header. Before
        // requiring the anchored `, line <N>` shape, `parse_python_frame_line`
        // accepted any line starting with `File "` regardless, and since
        // frames are scanned outermost-first (reversed), that trailing
        // pseudo-frame -- appearing later in the string -- won and hijacked
        // the signature (M5; same anchoring fix as M2).
        let stack = "Traceback (most recent call last):\n  File \"/app/api/app/routes/quote.py\", line 20, in create_quote\n    raise RuntimeError(f'build failed:\\nFile \"{worker_path}\" failed to build')\nRuntimeError: build failed:\nFile \"/tmp/build/worker.py\" failed to build";
        let sig = ErrorSignature::from_error("RuntimeError", "build failed", stack, "/app/api");
        assert_eq!(sig.top_frame_file, "app/routes/quote.py");
        assert_eq!(sig.top_frame_function.as_deref(), Some("create_quote"));
    }

    #[test]
    fn python_traceback_pseudo_file_frames_never_collapse_distinct_failures_m3() {
        // Verified against real CPython 3.12: assigning to a frozen
        // dataclass field raises inside generated code, and the last frame
        // is literally `File "<string>", line 4, in __setattr__`. Two
        // unrelated production failures (one in quote.py, one in
        // invoice.py) both bottom out in that same generated frame. Before
        // filtering `<string>` (and any other `<...>` pseudo-file) like
        // `<frozen ...>` already was, both signatures resolve to the same
        // `"<string>"` / `"__setattr__"` and incorrectly `matches()` (M3).
        let quote_stack = "Traceback (most recent call last):\n  File \"/app/api/app/routes/quote.py\", line 15, in create_quote\n    quote.total = compute_total(quote)\n  File \"<string>\", line 4, in __setattr__\ndataclasses.FrozenInstanceError: cannot assign to field 'total'";
        let invoice_stack = "Traceback (most recent call last):\n  File \"/app/api/app/routes/invoice.py\", line 22, in create_invoice\n    invoice.total = compute_total(invoice)\n  File \"<string>\", line 4, in __setattr__\ndataclasses.FrozenInstanceError: cannot assign to field 'total'";
        let quote_sig = ErrorSignature::from_error(
            "FrozenInstanceError",
            "cannot assign to field 'total'",
            quote_stack,
            "/app/api",
        );
        let invoice_sig = ErrorSignature::from_error(
            "FrozenInstanceError",
            "cannot assign to field 'total'",
            invoice_stack,
            "/app/api",
        );
        assert_eq!(quote_sig.top_frame_file, "app/routes/quote.py");
        assert_eq!(
            quote_sig.top_frame_function.as_deref(),
            Some("create_quote")
        );
        assert_eq!(invoice_sig.top_frame_file, "app/routes/invoice.py");
        assert_eq!(
            invoice_sig.top_frame_function.as_deref(),
            Some("create_invoice")
        );
        assert!(
            !quote_sig.matches(&invoice_sig),
            "two distinct failures must not collapse to one signature via the shared <string> pseudo-frame"
        );
    }

    #[test]
    fn python_traceback_lib_python_substring_does_not_filter_app_dir_m4() {
        // `<repo>/lib/python/dispatch.py` is a real (if unusual) polyglot
        // application layout, not the interpreter's standard library.
        // `contains("/lib/python")` matches it anyway and the genuine
        // raising frame is silently skipped in favor of the caller. The
        // filter must anchor on a digit immediately after `python`, the
        // stdlib's actual shape (`/lib/python3.11/...`) (M4).
        let stack = "Traceback (most recent call last):\n  File \"/app/api/api/handler.py\", line 10, in handle\n    dispatch(event)\n  File \"/app/api/lib/python/dispatch.py\", line 30, in dispatch\n    raise ValueError(\"bad event\")\nValueError: bad event";
        let sig = ErrorSignature::from_error("ValueError", "bad event", stack, "/app/api");
        assert_eq!(sig.top_frame_file, "lib/python/dispatch.py");
        assert_eq!(sig.top_frame_function.as_deref(), Some("dispatch"));
    }

    #[test]
    fn python_traceback_importlib_substring_does_not_filter_app_filename_m4() {
        // `api/importlib_compat.py` is an application file that merely
        // contains the substring `importlib` in its name.
        // `contains("importlib")` matches it anyway and the genuine
        // raising frame is silently skipped in favor of the caller. The
        // rule must require `importlib` as a real path component (or a
        // frozen `<frozen importlib...>` frame, already caught by the
        // pseudo-file check) (M4).
        let stack = "Traceback (most recent call last):\n  File \"/app/api/api/handler.py\", line 10, in handle\n    load_plugin(name)\n  File \"/app/api/api/importlib_compat.py\", line 8, in load_plugin\n    raise ImportError(\"missing plugin\")\nImportError: missing plugin";
        let sig = ErrorSignature::from_error("ImportError", "missing plugin", stack, "/app/api");
        assert_eq!(sig.top_frame_file, "api/importlib_compat.py");
        assert_eq!(sig.top_frame_function.as_deref(), Some("load_plugin"));
    }
}
