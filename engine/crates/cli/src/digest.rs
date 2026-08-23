//! `drums digest` (spec §14 "the morning message"): "For most users on most
//! days, this is the entire product. If it is good, they never open anything
//! else." Written in the order a person cares — what broke, what Drums did,
//! what still needs them — never a digest of metrics.
//!
//! [`Digest::build`] is a PURE function over the append-only record (via
//! `engine_record`'s tolerant reader — same torn-line honesty every other
//! read-only view in this crate already gives). Rendering
//! ([`render_stdout`]/[`render_slack_markdown`]) is kept separate from that
//! read, matching `record_cmd.rs`'s split of `load` from `render_lines` —
//! rendering is unit-testable against a [`Digest`] literal without a real
//! record file.
//!
//! ## What counts as a headline event vs. what needs a human
//!
//! A repair's whole story lives across up to three record lines joined by
//! `failure_id`: `repair_ready` (the proposal), then either `shipped` or
//! `reverted` (the outcome). [`Digest::build`] joins them the same way
//! `ship.rs` already does for `drums ship`/`drums revert`.
//!
//! - A `failure_id` with a `shipped` or `reverted` line recorded WITHIN the
//!   `--since` window is a **headline event** — something Drums finished
//!   doing during the period. A rolled-back repair renders with exactly the
//!   same paragraph shape, section, and prominence as a shipped one (spec
//!   §9: "hiding failures is how a tool like this loses a team
//!   permanently") — never softened, never demoted to a footnote.
//! - A `failure_id` with a `repair_ready` line but NO `shipped`/`reverted`
//!   line ANYWHERE in the record (not just the window) is a **proposal
//!   awaiting approval**. This is deliberately NOT time-windowed: a repair
//!   that has been blocking production since before `--since` must not
//!   silently age out of "One thing needs you" just because the digest
//!   window is short — that would hide exactly the thing this surface
//!   exists to surface.
//!
//! ## The one honest gap: raw `event` lines have no `failure_id`
//!
//! `engine_core::ErrorEvent` (the `event` record kind, written by
//! `engine-ingest` at intake) carries no `failure_id`, and `engine_core::
//! Repair` (`repair_ready`) carries no error signature — the two record
//! kinds share no join key today. A reproduction that fails is not
//! currently persisted as its own record line at all (`engine.rs`'s
//! `EngineEvent::ReproFailed` is rendered live in the terminal but never
//! appended to the record); the only trace such a failure leaves behind is
//! its raw `event` line, orphaned.
//!
//! Rather than guess at a correlation the schema doesn't support (the same
//! discipline `engine-record`'s own redaction code applies: "never guess at
//! a structure that isn't there"), this module treats the two record kinds
//! as separate signal streams. Raw `event` lines are surfaced individually,
//! under "One thing needs you" as an unresolved reproduction, only when the
//! record contains NO `repair_ready` line at all — i.e. nothing in this
//! record has ever gone on to become a repair, so every observed failure is
//! definitionally still open. Once a repair pipeline is producing
//! `repair_ready` lines for anything, raw orphan events are left out rather
//! than mislabeled by a guess; closing that gap for real needs a persisted
//! link (or a persisted "reproduction failed" line) from the ingest/engine
//! lane, not a guess made here.

use std::path::Path;
use std::time::Duration;

use engine_core::Provenance;
use serde::Deserialize;
use serde_json::Value;

use crate::record_cmd::relative_when;
use crate::render::{chip_ansi, ship_command_line, RenderContext, BOLD, RESET};

// -- building ----------------------------------------------------------------

/// One completed repair saga within the digest window: `repair_ready`
/// followed by a `shipped` or `reverted` line for the same `failure_id`.
#[derive(Debug, Clone)]
pub struct DigestEvent {
    pub failure_id: String,
    /// The repair's own `summary` field — the one human-authored
    /// description of what Drums did, restated here rather than invented.
    pub title: String,
    pub outcome: Outcome,
    /// The outcome's own claims, joined — the evidence, never fabricated
    /// narrative. Empty when the record line carried no claims at all.
    pub claim_text: String,
    /// Aggregated the same way `record_cmd.rs::aggregate_claims` already
    /// aggregates a claims array for display: `Unresolved` if ANY claim is
    /// unresolved, else the first claim's provenance, else `None`.
    pub chip: Option<Provenance>,
    pub recorded_at_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Repaired,
    RolledBack,
}

/// Something genuinely blocked on a human — the ONLY things that belong
/// here (spec: "proposals awaiting approval, always-ask paths, unresolved
/// reproductions"). Always-ask paths aren't modeled by the current record
/// schema (no autonomy/authority data is persisted yet) and are
/// deliberately NOT fabricated here.
#[derive(Debug, Clone)]
pub enum NeedsYou {
    AwaitingApproval {
        failure_id: String,
        title: String,
        recorded_at_ms: u64,
    },
    /// One failure, however many raw `event` lines it produced: grouped by
    /// the same signature identity the engine's detector uses, with
    /// `event_count` carrying how many events fed it and `occurred_at_ms`
    /// the most recent. The `drums-init-verify` self-check is excluded — an
    /// install proving its own pipe is not a thing that needs anybody.
    UnresolvedReproduction {
        service: String,
        error_name: String,
        occurred_at_ms: u64,
        event_count: usize,
    },
    /// A failure class has earned the right to be PROPOSED for act-alone.
    ///
    /// It belongs here, in the surface the spec ranks highest (§4), rather
    /// than only in `drums authority list`: a promotion nobody sees is a
    /// promotion that never happens, and the whole ladder would quietly
    /// stall at propose forever. It is genuinely "needs you" — it is the one
    /// decision in the product that software is not allowed to make for
    /// itself.
    PromotionEarned {
        class: String,
        streak: u32,
        because: String,
        /// Add the one line saying act-alone is paid (see
        /// [`crate::license::digest_paid_note`]).
        ///
        /// True only when there is no valid license AND the promotion was
        /// earned inside this digest's own window — so it is said on the
        /// morning it becomes true and not again. The candidate itself keeps
        /// appearing under "needs you" for as long as it is unactioned,
        /// because it is a real open decision; what stops repeating is the
        /// sentence about money. The evidence is the pitch, and a digest
        /// that asks every morning is a digest people mute.
        act_alone_is_paid: bool,
    },
}

#[derive(Debug, Clone)]
pub struct Digest {
    pub since_ms: u64,
    pub now_ms: u64,
    /// Newest-first, matching the record's own convention (spec §9).
    pub events: Vec<DigestEvent>,
    /// Newest-first.
    pub needs_you: Vec<NeedsYou>,
    /// Unreadable (torn/corrupt) lines skipped while reading the record —
    /// disclosed honestly, never silently.
    pub skipped: usize,
    /// Set only on a genuine IO error reading the record (a directory at
    /// `record_path`, permission denied, ...) — NOT set for a missing file,
    /// which reads as an honest empty period (`engine_record::read_all`'s
    /// own contract: nothing recorded yet is not a failure to read).
    pub read_error: Option<String>,
}

#[derive(Deserialize)]
struct RClaim {
    text: String,
    provenance: String,
}

#[derive(Deserialize)]
struct RRepairReady {
    failure_id: String,
    summary: String,
}

#[derive(Deserialize)]
struct RShipOutcome {
    failure_id: String,
    #[serde(default)]
    claims: Vec<RClaim>,
}

/// Deliberately partial, same discipline as `why.rs`'s `RecordedEvent`: no
/// `request` field at all, so a request body or query string is
/// structurally impossible to deserialize into memory here, let alone
/// print. `error_message`/`stack` are read ONLY to compute the same
/// signature identity the detector groups by — they are never rendered.
#[derive(Deserialize)]
struct RErrorEvent {
    service: String,
    error_name: String,
    #[serde(default)]
    error_message: String,
    #[serde(default)]
    stack: String,
    occurred_at_ms: u64,
}

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

/// Same rule as `record_cmd.rs::aggregate_claims`, restated here over a
/// `&[RClaim]` rather than a `serde_json::Value`: `Unresolved` wins over
/// everything else, otherwise the first recognizable claim's provenance.
fn aggregate(claims: &[RClaim]) -> Option<Provenance> {
    let mut first = None;
    for c in claims {
        let Some(p) = parse_provenance(&c.provenance) else {
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

fn join_claim_text(claims: &[RClaim]) -> String {
    claims
        .iter()
        .map(|c| c.text.trim().trim_end_matches('.'))
        .collect::<Vec<_>>()
        .join("; ")
}

fn recorded_at_ms(v: &Value) -> u64 {
    v.get("recorded_at_ms")
        .and_then(|x| x.as_u64())
        .unwrap_or(0)
}

fn within(recorded_at_ms: u64, now_ms: u64, since_ms: u64) -> bool {
    now_ms.saturating_sub(recorded_at_ms) <= since_ms
}

impl Digest {
    /// Pure: `now_ms` is data, not a wall-clock read (mirrors
    /// `record_cmd.rs`'s own split of `now_ms()` from `filter_since`) — the
    /// caller (`drums digest`, or a test) supplies both, so this function
    /// never touches the system clock itself.
    ///
    /// `act_alone_licensed` is data for the same reason: the license lives
    /// on the machine, not in the record, and this function must not start
    /// reading ambient state to find it. It affects exactly one line — the
    /// note under an earned promotion — and NOTHING else in the digest. Every
    /// failure, repair, rollback and open proposal renders identically
    /// whatever it is set to.
    pub fn build(
        record_path: &Path,
        since_ms: u64,
        now_ms: u64,
        act_alone_licensed: bool,
    ) -> Digest {
        let read = match engine_record::read_all(record_path) {
            Ok(r) => r,
            Err(e) => {
                return Digest {
                    since_ms,
                    now_ms,
                    events: Vec::new(),
                    needs_you: Vec::new(),
                    skipped: 0,
                    read_error: Some(format!(
                        "could not read the record at {}: {e}",
                        record_path.display()
                    )),
                };
            }
        };

        // Pass 1: every repair_ready line, unwindowed — a proposal's title
        // (and whether it's STILL open) must not depend on when it was
        // proposed, only on whether it's since been actioned. Later lines
        // win on a repeat `failure_id` (forward-compat: a corrected re-append
        // of the same id should not resurrect a stale title).
        let mut repairs: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        let mut repair_ready_seen_at_all = false;
        for (kind, v) in &read.lines {
            if kind != "repair_ready" {
                continue;
            }
            repair_ready_seen_at_all = true;
            if let Ok(r) = serde_json::from_value::<RRepairReady>(v.clone()) {
                repairs.insert(r.failure_id, r.summary);
            }
        }

        // Pass 2: every shipped/reverted line, unwindowed (for the OPEN/
        // CLOSED question) but each remembers its own recorded_at_ms so pass
        // 3 can decide which ones are headline events THIS period.
        struct OutcomeLine {
            claims: Vec<RClaim>,
            recorded_at_ms: u64,
        }
        let mut shipped: std::collections::HashMap<String, OutcomeLine> =
            std::collections::HashMap::new();
        let mut reverted: std::collections::HashMap<String, OutcomeLine> =
            std::collections::HashMap::new();
        for (kind, v) in &read.lines {
            if kind != "shipped" && kind != "reverted" {
                continue;
            }
            let t = recorded_at_ms(v);
            let Ok(s) = serde_json::from_value::<RShipOutcome>(v.clone()) else {
                continue;
            };
            let entry = OutcomeLine {
                claims: s.claims,
                recorded_at_ms: t,
            };
            if kind == "shipped" {
                shipped.insert(s.failure_id, entry);
            } else {
                reverted.insert(s.failure_id, entry);
            }
        }

        // Headline events: a failure whose outcome landed IN the window.
        let mut events = Vec::new();
        for (failure_id, outcome) in &reverted {
            if !within(outcome.recorded_at_ms, now_ms, since_ms) {
                continue;
            }
            let title = repairs
                .get(failure_id)
                .cloned()
                .unwrap_or_else(|| format!("repair for failure {failure_id}"));
            events.push(DigestEvent {
                failure_id: failure_id.clone(),
                title,
                outcome: Outcome::RolledBack,
                claim_text: join_claim_text(&outcome.claims),
                chip: aggregate(&outcome.claims),
                recorded_at_ms: outcome.recorded_at_ms,
            });
        }
        for (failure_id, outcome) in &shipped {
            if reverted.contains_key(failure_id) {
                // A shipped repair that was later reverted is told as ONE
                // story — the rollback — not two, matching how `drums
                // record` narrates `shipped` and `reverted` as distinct
                // lines but a digest reader cares about the CURRENT state.
                continue;
            }
            if !within(outcome.recorded_at_ms, now_ms, since_ms) {
                continue;
            }
            let title = repairs
                .get(failure_id)
                .cloned()
                .unwrap_or_else(|| format!("repair for failure {failure_id}"));
            events.push(DigestEvent {
                failure_id: failure_id.clone(),
                title,
                outcome: Outcome::Repaired,
                claim_text: join_claim_text(&outcome.claims),
                chip: aggregate(&outcome.claims),
                recorded_at_ms: outcome.recorded_at_ms,
            });
        }
        events.sort_by_key(|e| std::cmp::Reverse(e.recorded_at_ms));

        // Needs you, part 1: proposals still open (no shipped, no reverted,
        // anywhere) — not time-windowed, see module doc.
        let mut needs_you = Vec::new();
        let mut awaiting: Vec<(String, String, u64)> = Vec::new();
        for (kind, v) in &read.lines {
            if kind != "repair_ready" {
                continue;
            }
            let Ok(r) = serde_json::from_value::<RRepairReady>(v.clone()) else {
                continue;
            };
            if shipped.contains_key(&r.failure_id) || reverted.contains_key(&r.failure_id) {
                continue;
            }
            let t = recorded_at_ms(v);
            awaiting.push((r.failure_id, r.summary, t));
        }
        // Keep only the newest repair_ready per still-open failure_id.
        let mut newest_awaiting: std::collections::HashMap<String, (String, u64)> =
            std::collections::HashMap::new();
        for (failure_id, title, t) in awaiting {
            newest_awaiting
                .entry(failure_id)
                .and_modify(|(existing_title, existing_t)| {
                    if t >= *existing_t {
                        *existing_title = title.clone();
                        *existing_t = t;
                    }
                })
                .or_insert((title, t));
        }
        for (failure_id, (title, recorded_at_ms)) in newest_awaiting {
            needs_you.push(NeedsYou::AwaitingApproval {
                failure_id,
                title,
                recorded_at_ms,
            });
        }

        // Needs you, part 2: orphan `event` lines — only when NOTHING in
        // this record has ever become a repair (see module doc for why this
        // is the honest limit of what's derivable from the current schema).
        // Grouped by the detector's own signature identity, because five raw
        // events of one failure are one thing needing you, not five.
        if !repair_ready_seen_at_all {
            let mut open: std::collections::HashMap<
                (String, engine_core::ErrorSignature),
                (u64, usize),
            > = std::collections::HashMap::new();
            for (kind, v) in &read.lines {
                if kind != "event" {
                    continue;
                }
                let t = recorded_at_ms(v);
                if !within(t, now_ms, since_ms) {
                    continue;
                }
                let Ok(e) = serde_json::from_value::<RErrorEvent>(v.clone()) else {
                    continue;
                };
                if e.service == engine_detect::observe::SELF_TEST_SERVICE {
                    continue;
                }
                let sig = engine_core::ErrorSignature::from_error(
                    &e.error_name,
                    &e.error_message,
                    &e.stack,
                    "",
                );
                let entry = open
                    .entry((e.service, sig))
                    .or_insert((e.occurred_at_ms, 0));
                entry.0 = entry.0.max(e.occurred_at_ms);
                entry.1 += 1;
            }
            for ((service, sig), (latest, count)) in open {
                needs_you.push(NeedsYou::UnresolvedReproduction {
                    service,
                    error_name: sig.error_name,
                    occurred_at_ms: latest,
                    event_count: count,
                });
            }
        }
        // The ladder is folded from this same record, so a candidate costs
        // one more pass over lines that are already in memory — and a
        // promotion that only ever appears in `drums authority list` is one
        // nobody will read.
        if let Ok(ladder) = engine_authority::Ladder::load(record_path) {
            for c in ladder.promotion_candidates() {
                needs_you.push(NeedsYou::PromotionEarned {
                    class: c.class.key(),
                    streak: c.streak,
                    because: c.because,
                    // Said on the morning it becomes true, then not again:
                    // the candidate keeps showing (it is still an open
                    // decision) but the sentence about money ages out with
                    // the window, the same way every other event here does.
                    act_alone_is_paid: !act_alone_licensed
                        && within(c.earned_at_ms, now_ms, since_ms),
                });
            }
        }
        needs_you.sort_by_key(|n| std::cmp::Reverse(needs_you_sort_key(n)));

        Digest {
            since_ms,
            now_ms,
            events,
            needs_you,
            skipped: read.skipped,
            read_error: None,
        }
    }
}

fn needs_you_sort_key(n: &NeedsYou) -> u64 {
    match n {
        NeedsYou::AwaitingApproval { recorded_at_ms, .. } => *recorded_at_ms,
        NeedsYou::UnresolvedReproduction { occurred_at_ms, .. } => *occurred_at_ms,
        // No timestamp of its own: a promotion candidate is a standing state,
        // not an event. Sorted last (0) so time-ordered items — the things
        // that actually happened — come first.
        NeedsYou::PromotionEarned { .. } => 0,
    }
}

// -- rendering ----------------------------------------------------------------

/// The two textual styles `render_stdout`/`render_slack_markdown` share the
/// exact same composition logic through — only how "bold" and a provenance
/// chip are spelled differs.
trait Style {
    fn bold(&self, s: &str) -> String;
    fn chip(&self, p: Provenance) -> String;
}

struct PlainStyle {
    color: bool,
}

impl Style for PlainStyle {
    fn bold(&self, s: &str) -> String {
        if self.color {
            format!("{BOLD}{s}{RESET}")
        } else {
            s.to_string()
        }
    }
    fn chip(&self, p: Provenance) -> String {
        if self.color {
            format!(" {}[{}]{RESET}", chip_ansi(p), p.chip())
        } else {
            format!(" [{}]", p.chip())
        }
    }
}

struct SlackStyle;

impl Style for SlackStyle {
    fn bold(&self, s: &str) -> String {
        format!("*{s}*")
    }
    fn chip(&self, p: Provenance) -> String {
        format!(" `{}`", p.chip())
    }
}

fn word_for_count(n: usize) -> String {
    match n {
        1 => "one".to_string(),
        2 => "two".to_string(),
        3 => "three".to_string(),
        4 => "four".to_string(),
        5 => "five".to_string(),
        n => n.to_string(),
    }
}

fn period_label(since_ms: u64) -> String {
    const H: u64 = 3_600_000;
    const D: u64 = 86_400_000;
    if since_ms <= 30 * H {
        "Overnight".to_string()
    } else if since_ms.is_multiple_of(D) {
        let days = since_ms / D;
        if days == 1 {
            "In the last day".to_string()
        } else {
            format!("In the last {days} days")
        }
    } else {
        format!("In the last {}h", since_ms / H)
    }
}

/// Honest, counted, never softened: the outcome mix is spelled out in full
/// rather than collapsed into a single reassuring word the instant a
/// rollback is in the mix.
fn headline(since_ms: u64, n_ok: usize, n_unresolved: usize, n_rb: usize) -> String {
    let period = period_label(since_ms);
    let total = n_ok + n_unresolved + n_rb;
    if total == 0 {
        return format!("{period}: nothing happened.");
    }
    if total == 1 {
        let word = if n_rb == 1 {
            "rolled back"
        } else if n_unresolved == 1 {
            "shipped, but the post-deploy check is unresolved"
        } else {
            "repaired"
        };
        return format!("{period}: one failure, {word}.");
    }
    let count_word = word_for_count(total);
    if n_ok == total {
        let word = if total == 2 {
            "both repaired"
        } else {
            "all repaired"
        };
        return format!("{period}: {count_word} failures, {word}.");
    }
    if n_rb == total {
        let word = if total == 2 {
            "both rolled back"
        } else {
            "all rolled back"
        };
        return format!("{period}: {count_word} failures, {word}.");
    }
    let mut parts = Vec::new();
    if n_ok > 0 {
        parts.push(format!("{} repaired", word_for_count(n_ok)));
    }
    if n_unresolved > 0 {
        parts.push(format!(
            "{} shipped with an unresolved check",
            word_for_count(n_unresolved)
        ));
    }
    if n_rb > 0 {
        parts.push(format!("{} rolled back", word_for_count(n_rb)));
    }
    format!(
        "{period}: {count_word} failures \u{2014} {}.",
        parts.join(", ")
    )
}

fn event_paragraph(e: &DigestEvent, now_ms: u64, style: &dyn Style) -> String {
    let when = relative_when(e.recorded_at_ms, now_ms);
    let lead = match e.outcome {
        Outcome::Repaired => format!("{} — {when}.", style.bold(&e.title)),
        Outcome::RolledBack => format!("{} — rolled back {when}.", style.bold(&e.title)),
    };
    let claim = if e.claim_text.is_empty() {
        String::new()
    } else {
        format!(" {}.", e.claim_text)
    };
    let chip = e.chip.map(|p| style.chip(p)).unwrap_or_default();
    format!("{lead}{claim}{chip}")
}

fn needs_you_line(n: &NeedsYou, now_ms: u64, style: &dyn Style) -> String {
    match n {
        NeedsYou::AwaitingApproval {
            failure_id,
            title,
            recorded_at_ms,
        } => {
            let when = relative_when(*recorded_at_ms, now_ms);
            format!(
                "- {} (failure {failure_id}) — ready {when}, still awaiting a decision.{}",
                style.bold(title),
                style.chip(Provenance::Unresolved)
            )
        }
        NeedsYou::UnresolvedReproduction {
            service,
            error_name,
            occurred_at_ms,
            event_count,
        } => {
            let when = relative_when(*occurred_at_ms, now_ms);
            let reported = if *event_count == 1 {
                format!("reported {when}")
            } else {
                format!("{event_count} events, the last {when}")
            };
            format!(
                "- {} in {service} — {reported}; never reproduced or repaired.{}",
                style.bold(error_name),
                style.chip(Provenance::Unresolved)
            )
        }
        // `Approved` is the right chip: this is a decision about consent, not
        // a technical claim. Nothing here was measured — a human is being
        // asked to grant something.
        NeedsYou::PromotionEarned {
            class,
            streak,
            because,
            act_alone_is_paid,
        } => {
            let paid = if *act_alone_is_paid {
                format!("\n  {}", crate::license::digest_paid_note())
            } else {
                String::new()
            };
            format!(
                "- {} has earned act-alone — {streak} clean ships in a row. {because}\n  Run `drums authority promote {class}` to grant it, or ignore this and it stays where it is.{paid}{}",
                style.bold(class),
                style.chip(Provenance::Approved)
            )
        }
    }
}

fn needs_you_header(n: usize) -> String {
    if n == 1 {
        return "One thing needs you:".to_string();
    }
    // Capitalized like the n == 1 line — "One thing needs you:" beside
    // "four things need you:" read as two different surfaces.
    let word = word_for_count(n);
    let mut chars = word.chars();
    let capitalized = match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => word,
    };
    format!("{capitalized} things need you:")
}

fn skipped_disclosure(skipped: usize) -> String {
    if skipped == 0 {
        return String::new();
    }
    let plural = if skipped == 1 { "" } else { "s" };
    format!("\n{skipped} unreadable line{plural} skipped while reading the record \u{2014} this digest may be incomplete.\n")
}

/// The composition shared by both public renderers: headline, one paragraph
/// per headline event, the "needs you" section, available actions, then the
/// skipped-line disclosure. When nothing happened AND nothing is
/// outstanding, this is a single line and stops — spec: "never manufacture
/// activity."
fn compose(d: &Digest, ctx: &RenderContext, style: &dyn Style) -> String {
    if let Some(err) = &d.read_error {
        return format!("could not build the digest: {err}\n");
    }

    if d.events.is_empty() && d.needs_you.is_empty() {
        let mut out = format!("{}\n", headline(d.since_ms, 0, 0, 0));
        out.push_str(&skipped_disclosure(d.skipped));
        return out;
    }

    let n_ok = d
        .events
        .iter()
        .filter(|e| e.outcome == Outcome::Repaired && e.chip != Some(Provenance::Unresolved))
        .count();
    let n_unresolved = d
        .events
        .iter()
        .filter(|e| e.outcome == Outcome::Repaired && e.chip == Some(Provenance::Unresolved))
        .count();
    let n_rb = d
        .events
        .iter()
        .filter(|e| e.outcome == Outcome::RolledBack)
        .count();

    let mut out = String::new();
    if d.events.is_empty() {
        // Reached only when `needs_you` is non-empty (both empty returned
        // above), so "nothing happened" would be contradicted by the very
        // next section. Say what is true: nothing FINISHED this period.
        out.push_str(&format!(
            "{}: nothing shipped or rolled back.",
            period_label(d.since_ms)
        ));
    } else {
        out.push_str(&headline(d.since_ms, n_ok, n_unresolved, n_rb));
    }
    out.push('\n');

    for e in &d.events {
        out.push('\n');
        out.push_str(&event_paragraph(e, d.now_ms, style));
        out.push('\n');
    }

    if !d.needs_you.is_empty() {
        out.push('\n');
        out.push_str(&needs_you_header(d.needs_you.len()));
        out.push('\n');
        for n in &d.needs_you {
            out.push_str(&needs_you_line(n, d.now_ms, style));
            out.push('\n');
        }
    }

    out.push('\n');
    out.push_str("Actions:\n");
    for n in &d.needs_you {
        if let NeedsYou::AwaitingApproval {
            failure_id, title, ..
        } = n
        {
            out.push_str(&format!(
                "  {}    approve & ship: {title}\n",
                ship_command_line(failure_id, ctx)
            ));
        }
    }
    out.push_str(&format!(
        "  drums record --repo {}    open the full record\n",
        ctx.repo.display()
    ));

    out.push_str(&skipped_disclosure(d.skipped));
    out
}

/// `drums digest --to stdout` (the default).
pub fn render_stdout(d: &Digest, ctx: &RenderContext, use_color: bool) -> String {
    compose(d, ctx, &PlainStyle { color: use_color })
}

/// `drums digest --to slack`: clean mrkdwn, sent as an incoming-webhook
/// `{"text": ...}` payload. Slack renders mrkdwn `text` directly with no
/// further setup — the whole point of shipping this before email is that it
/// needs zero new accounts beyond a webhook URL the team likely already has,
/// and it reuses the exact same composition (and therefore the exact same
/// honesty rules) as stdout instead of a second, drift-prone renderer.
pub fn render_slack_markdown(d: &Digest, ctx: &RenderContext) -> String {
    compose(d, ctx, &SlackStyle)
}

/// `drums digest --send`: the whole digest as ONE four-kind notification
/// (`crate::notify`). Kind selection is the honest split the vocabulary
/// draws: **FYI** when nothing needs anyone (investigated; nothing needs
/// you), **Decision** when the needs-you list is non-empty — and then the
/// title carries the count, because "how many things are waiting on me" is
/// the entire question a header answers. The body is the same composition
/// (and therefore the same honesty rules — rollbacks at full prominence, no
/// request data, torn lines disclosed) as `render_slack_markdown`, never a
/// second renderer.
///
/// Pure. The caller refuses to send a digest whose `read_error` is set —
/// "FYI, nothing needs you" about a record that could not be read would be a
/// lie, and that refusal belongs where the exit code lives (`main.rs`).
pub fn notification(
    d: &Digest,
    ctx: &RenderContext,
    repo_name: &str,
) -> crate::notify::Notification {
    use crate::notify::{Kind, Notification};
    let body = render_slack_markdown(d, ctx);
    if d.needs_you.is_empty() {
        Notification {
            kind: Kind::Fyi,
            title: "nothing needs you".to_string(),
            body,
            repo: repo_name.to_string(),
        }
    } else {
        // `needs_you_header` minus its trailing colon: "One thing needs you",
        // "Two things need you" — the count in words, same as the section
        // header the body itself carries.
        let title = needs_you_header(d.needs_you.len())
            .trim_end_matches(':')
            .to_string();
        Notification {
            kind: Kind::Decision,
            title,
            body,
            repo: repo_name.to_string(),
        }
    }
}

// -- delivery ------------------------------------------------------------------

#[derive(Debug)]
pub enum NotifyError {
    Http(String),
    NonSuccessStatus(u16),
}

impl std::fmt::Display for NotifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NotifyError::Http(e) => write!(f, "request failed: {e}"),
            NotifyError::NonSuccessStatus(code) => write!(f, "webhook returned status {code}"),
        }
    }
}

impl std::error::Error for NotifyError {}

#[async_trait::async_trait]
pub trait Notifier {
    async fn send(&self, d: &Digest) -> Result<(), NotifyError>;
    fn name(&self) -> &str;
}

/// The DEFAULT notifier — works tonight with zero accounts, zero
/// configuration, and is what every test in this module asserts against
/// verbatim, since this is the surface a human is meant to actually judge.
pub struct StdoutNotifier {
    pub ctx: RenderContext,
    pub use_color: bool,
}

#[async_trait::async_trait]
impl Notifier for StdoutNotifier {
    async fn send(&self, d: &Digest) -> Result<(), NotifyError> {
        use std::io::Write;
        print!("{}", render_stdout(d, &self.ctx, self.use_color));
        std::io::stdout().flush().ok();
        Ok(())
    }
    fn name(&self) -> &str {
        "stdout"
    }
}

/// POSTs `{"text": <mrkdwn>}` to a Slack incoming-webhook URL. Email is
/// skipped entirely (see the lane report): a webhook is a single URL a team
/// already has for practically any other alerting tool, needs no SMTP
/// credentials or deliverability setup, and Slack is where spec §14 itself
/// stages the whole worked example (`#eng · Drums`) — building the surface
/// the spec actually describes was a better use of the same afternoon than
/// a third, unasked-for channel.
pub struct SlackWebhookNotifier {
    pub url: String,
    pub ctx: RenderContext,
    client: reqwest::Client,
}

impl SlackWebhookNotifier {
    pub fn new(url: String, ctx: RenderContext) -> Self {
        Self {
            url,
            ctx,
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait::async_trait]
impl Notifier for SlackWebhookNotifier {
    async fn send(&self, d: &Digest) -> Result<(), NotifyError> {
        let text = render_slack_markdown(d, &self.ctx);
        let body = serde_json::json!({ "text": text });
        let resp = self
            .client
            .post(&self.url)
            .json(&body)
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|e| NotifyError::Http(e.to_string()))?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(NotifyError::NonSuccessStatus(resp.status().as_u16()))
        }
    }
    fn name(&self) -> &str {
        "slack"
    }
}

/// Tests only, and any lane wiring a digest into a pipeline without wanting
/// output.
pub struct NoOpNotifier;

#[async_trait::async_trait]
impl Notifier for NoOpNotifier {
    async fn send(&self, _d: &Digest) -> Result<(), NotifyError> {
        Ok(())
    }
    fn name(&self) -> &str {
        "noop"
    }
}

// -- scheduling (pure; the daemon lane owns the actual cron loop) -------------

/// What the (not-yet-built) daemon lane needs to decide whether tonight's
/// digest is due. Deliberately minimal: a fixed interval, not a
/// time-of-day/cron expression — that richer scheduling is this OTHER
/// lane's call to make, not something to half-build here.
#[derive(Debug, Clone, Copy)]
pub struct ScheduleConfig {
    pub interval_ms: u64,
}

/// Pure: no clock read, no IO. `now_ms`/`last_sent_ms` are both caller-
/// supplied data, exactly like `Digest::build`'s own `now_ms`.
pub fn should_send_now(cfg: &ScheduleConfig, last_sent_ms: Option<u64>, now_ms: u64) -> bool {
    match last_sent_ms {
        None => true,
        Some(last) => now_ms.saturating_sub(last) >= cfg.interval_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_core::{Claim, Repair, ShipOutcome};
    use std::io::Write;

    const H: u64 = 3_600_000;
    const D: u64 = 86_400_000;

    /// The state of every machine that has never paid us — i.e. almost all of
    /// them, and the one every other test in this module runs under.
    const UNLICENSED: bool = false;
    const LICENSED: bool = true;

    fn ctx() -> RenderContext {
        RenderContext {
            repo: std::path::PathBuf::from("/srv/shop"),
            deploy_cmd: None,
        }
    }

    fn claim(text: &str, p: Provenance) -> Claim {
        Claim {
            text: text.into(),
            provenance: p,
        }
    }

    fn append_repair_ready(path: &Path, failure_id: &str, summary: &str, ms: u64) {
        let repair = Repair {
            id: format!("r-{failure_id}"),
            failure_id: failure_id.into(),
            sha: "deadbeef".into(),
            branch: format!("drums/repair-{failure_id}"),
            agent: "claude".into(),
            summary: summary.into(),
            diff_stat: "1 file changed".into(),
            claims: vec![claim(
                "reproduced and verified in staging",
                Provenance::Verified,
            )],
        };
        engine_record::append(path, "repair_ready", &repair, ms).unwrap();
    }

    fn append_shipped(path: &Path, failure_id: &str, claims: Vec<Claim>, ms: u64) {
        let outcome = ShipOutcome {
            failure_id: failure_id.into(),
            repair_sha: "deadbeef".into(),
            action: "shipped".into(),
            deploy_cmd: "bash deploy.sh {sha}".into(),
            claims,
        };
        engine_record::append(path, "shipped", &outcome, ms).unwrap();
    }

    fn append_reverted(path: &Path, failure_id: &str, claims: Vec<Claim>, ms: u64) {
        let outcome = ShipOutcome {
            failure_id: failure_id.into(),
            repair_sha: "abc1234".into(),
            action: "reverted".into(),
            deploy_cmd: "bash deploy.sh {sha}".into(),
            claims,
        };
        engine_record::append(path, "reverted", &outcome, ms).unwrap();
    }

    fn append_event(
        path: &Path,
        service: &str,
        error_name: &str,
        occurred_at_ms: u64,
        recorded_at_ms: u64,
    ) {
        let ev = engine_core::ErrorEvent {
            service: service.into(),
            occurred_at_ms,
            error_name: error_name.into(),
            error_message: "boom".into(),
            stack: "TypeError: boom\n    at f (/x/server.js:1:1)".into(),
            request: Some(engine_core::CapturedRequest {
                method: "POST".into(),
                path: "/api/webhook".into(),
                content_type: Some("application/json".into()),
                body: Some(r#"{"card":"4242424242424242","secret_token":"sk_live_abc123"}"#.into()),
            }),
            // Snippet intake: the only kind that may carry a captured request,
            // and the redaction assertions below depend on that body existing.
            intake: engine_core::Intake::Snippet,
        };
        engine_record::append(path, "event", &ev, recorded_at_ms).unwrap();
    }

    // ---------------------------------------------------------------------
    // The canonical "two-failure" scenario from spec §14: one repair
    // shipped clean, one repair rolled back, plus one proposal still
    // awaiting approval — the exact three ingredients spec §14's own
    // worked example renders. This is the scenario pasted into the lane
    // report.
    // ---------------------------------------------------------------------

    fn two_failure_example_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        let now = 100 * H;

        // Failure 1: image upload timeouts — shipped clean, 4h ago.
        append_repair_ready(
            &path,
            "f-image-upload",
            "Image upload timeouts: widened the retry policy after an S3 latency spike",
            now - 5 * H,
        );
        append_shipped(
            &path,
            "f-image-upload",
            vec![claim(
                "canaried 4 minutes; error rate back to zero",
                Provenance::Verified,
            )],
            now - 4 * H,
        );

        // Failure 2: search p95 regression — rolled back, 8h ago.
        append_repair_ready(
            &path,
            "f-search-p95",
            "Search p95 regression: widened the query timeout",
            now - 9 * H,
        );
        append_reverted(
            &path,
            "f-search-p95",
            vec![claim(
                "made p95 worse at 10% canary; reverted, handed to @maya",
                Provenance::Observed,
            )],
            now - 8 * H,
        );

        // Failure 3: session expiry — repair ready 10h ago, still nothing
        // shipped or reverted for it.
        append_repair_ready(
            &path,
            "f-session-expiry",
            "Session expiry regression: extends the refresh window",
            now - 10 * H,
        );

        dir
    }

    fn build_two_failure_example() -> (Digest, tempfile::TempDir) {
        let dir = two_failure_example_dir();
        let path = dir.path().join("record.jsonl");
        let now = 100 * H;
        let d = Digest::build(&path, 24 * H, now, UNLICENSED);
        (d, dir)
    }

    #[test]
    fn two_failure_example_headline_names_the_rollback_honestly() {
        let (d, _dir) = build_two_failure_example();
        let out = render_stdout(&d, &ctx(), false);
        assert!(
            out.starts_with("Overnight: two failures \u{2014} one repaired, one rolled back.\n"),
            "headline must not soften the mix into a false 'both repaired': {out}"
        );
    }

    #[test]
    fn two_failure_example_rollback_has_equal_prominence_to_the_success() {
        let (d, _dir) = build_two_failure_example();
        let out = render_stdout(&d, &ctx(), false);

        let repaired_pos = out
            .find("Image upload timeouts")
            .expect("shipped event must render");
        let rolled_back_pos = out
            .find("Search p95 regression")
            .expect("rolled-back event must render, not be hidden");
        assert!(
            rolled_back_pos > repaired_pos,
            "newest-first ordering: the more recent shipped repair leads: {out}"
        );

        // Same section, same paragraph shape, same chip mechanism — not a
        // muted footnote. Both carry a provenance chip right next to their
        // claim text.
        assert!(
            out.contains("canaried 4 minutes; error rate back to zero. [verified]"),
            "{out}"
        );
        assert!(
            out.contains("made p95 worse at 10% canary; reverted, handed to @maya. [observed]"),
            "{out}"
        );

        // Never softened language for the rollback.
        assert!(out.contains("rolled back"), "{out}");
        assert!(
            !out.to_lowercase().contains("fixed"),
            "the word 'fixed' must never appear unqualified: {out}"
        );
    }

    #[test]
    fn two_failure_example_shows_the_open_proposal_under_needs_you_not_the_headline() {
        let (d, _dir) = build_two_failure_example();
        let out = render_stdout(&d, &ctx(), false);

        assert!(out.contains("One thing needs you:"), "{out}");
        assert!(out.contains("Session expiry regression"), "{out}");
        assert!(out.contains("still awaiting a decision"), "{out}");

        // The awaiting-approval item must not ALSO appear as a headline
        // event — it hasn't shipped or reverted, there is no outcome yet.
        let needs_you_idx = out.find("One thing needs you:").unwrap();
        let headline_section = &out[..needs_you_idx];
        assert!(
            !headline_section.contains("Session expiry"),
            "an unshipped proposal must not appear in the headline events: {out}"
        );
    }

    #[test]
    fn two_failure_example_actions_include_a_runnable_ship_command_for_the_open_proposal() {
        let (d, _dir) = build_two_failure_example();
        let out = render_stdout(&d, &ctx(), false);
        assert!(out.contains("Actions:"), "{out}");
        assert!(out.contains("drums ship f-session-expiry"), "{out}");
        assert!(out.contains("drums record --repo /srv/shop"), "{out}");
    }

    #[test]
    fn two_failure_example_slack_markdown_uses_slack_bold_and_never_diverges_in_substance() {
        let (d, _dir) = build_two_failure_example();
        let out = render_slack_markdown(&d, &ctx());
        assert!(
            out.starts_with("Overnight: two failures \u{2014} one repaired, one rolled back.\n"),
            "{out}"
        );
        assert!(
            out.contains(
                "*Image upload timeouts: widened the retry policy after an S3 latency spike*"
            ),
            "{out}"
        );
        assert!(
            out.contains("*Search p95 regression: widened the query timeout*"),
            "{out}"
        );
        assert!(out.contains("rolled back"), "{out}");
        assert!(
            !out.contains("**"),
            "slack markdown must use single-asterisk bold, not markdown-style double: {out}"
        );
    }

    // ---------------------------------------------------------------------
    // Required scenario: two repairs where one was rolled back is already
    // covered end-to-end by the two-failure example above. The remaining
    // required scenarios each get their own minimal, focused fixture.
    // ---------------------------------------------------------------------

    #[test]
    fn a_proposal_awaiting_approval_appears_under_needs_you_with_an_unresolved_chip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        let now = 100 * H;
        append_repair_ready(
            &path,
            "f1",
            "Checkout guard: adds a null check before totaling",
            now - 2 * H,
        );

        let d = Digest::build(&path, 24 * H, now, UNLICENSED);
        assert_eq!(
            d.events.len(),
            0,
            "an unshipped proposal is never a headline event"
        );
        assert_eq!(d.needs_you.len(), 1);

        let out = render_stdout(&d, &ctx(), false);
        assert!(out.contains("One thing needs you:"), "{out}");
        assert!(
            out.contains("Checkout guard: adds a null check before totaling"),
            "{out}"
        );
        assert!(
            out.contains("[unresolved]"),
            "an open proposal must carry the unresolved chip: {out}"
        );
    }

    #[test]
    fn a_trigger_intake_failure_that_could_not_be_reproduced_is_shown_as_unresolved() {
        // Nothing else was ever recorded for this event — no repair_ready
        // anywhere in the record — so it is the only signal Digest has that
        // something was even seen. It must not be silently dropped, and it
        // must not be claimed as a headline "failure, repaired" event it
        // never became.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        let now = 100 * H;
        append_event(&path, "billing", "TypeError", now - 3 * H, now - 3 * H);

        let d = Digest::build(&path, 24 * H, now, UNLICENSED);
        assert_eq!(d.events.len(), 0);
        assert_eq!(d.needs_you.len(), 1);
        assert!(
            matches!(&d.needs_you[0], NeedsYou::UnresolvedReproduction { service, error_name, .. } if service == "billing" && error_name == "TypeError")
        );

        let out = render_stdout(&d, &ctx(), false);
        assert!(out.contains("One thing needs you:"), "{out}");
        assert!(out.contains("TypeError in billing"), "{out}");
        assert!(out.contains("never reproduced or repaired"), "{out}");
        assert!(out.contains("[unresolved]"), "{out}");
    }

    /// R4: "Overnight: nothing happened." directly above a needs-you list is
    /// a headline the next line contradicts. When something is outstanding,
    /// the headline says what is true — nothing FINISHED.
    #[test]
    fn an_open_proposal_is_never_headed_by_nothing_happened() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        let now = 100 * H;
        append_repair_ready(
            &path,
            "f1",
            "Checkout guard: adds a null check before totaling",
            now - 2 * H,
        );

        let d = Digest::build(&path, 24 * H, now, UNLICENSED);
        let out = render_stdout(&d, &ctx(), false);
        assert!(
            !out.contains("nothing happened"),
            "the needs-you list below contradicts it: {out}"
        );
        assert!(
            out.starts_with("Overnight: nothing shipped or rolled back.\n"),
            "{out}"
        );
        assert!(out.contains("One thing needs you:"), "{out}");
    }

    /// R4: five raw events of one failure are one thing needing you, not
    /// five. Grouped by the detector's own signature identity, count shown.
    #[test]
    fn raw_events_of_one_failure_group_into_one_needs_you_item_with_a_count() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        let now = 100 * H;
        for i in 0..3u64 {
            append_event(
                &path,
                "billing",
                "TypeError",
                now - 3 * H + i * 600_000,
                now - 3 * H + i * 600_000,
            );
        }

        let d = Digest::build(&path, 24 * H, now, UNLICENSED);
        assert_eq!(
            d.needs_you.len(),
            1,
            "one failure, one item: {:?}",
            d.needs_you
        );
        assert!(
            matches!(
                &d.needs_you[0],
                NeedsYou::UnresolvedReproduction { event_count: 3, .. }
            ),
            "{:?}",
            d.needs_you
        );

        let out = render_stdout(&d, &ctx(), false);
        assert!(out.contains("One thing needs you:"), "{out}");
        assert!(out.contains("3 events"), "the count must be shown: {out}");
        assert_eq!(out.matches("TypeError in billing").count(), 1, "{out}");
    }

    /// R4/R11: the `drums init` self-check proves the pipe works. It is not
    /// a production failure and must never be listed as needing anybody.
    #[test]
    fn the_install_self_check_is_not_a_thing_that_needs_you() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        let now = 100 * H;
        append_event(
            &path,
            engine_detect::observe::SELF_TEST_SERVICE,
            "DrumsWireCheck",
            now - H,
            now - H,
        );

        let d = Digest::build(&path, 24 * H, now, UNLICENSED);
        assert!(d.needs_you.is_empty(), "{:?}", d.needs_you);
        let out = render_stdout(&d, &ctx(), false);
        assert_eq!(out, "Overnight: nothing happened.\n", "{out:?}");
    }

    /// R4: "One thing needs you:" beside "four things need you:" read as two
    /// different surfaces — both are capitalized.
    #[test]
    fn the_needs_you_count_phrasing_is_capitalized_consistently() {
        assert_eq!(needs_you_header(1), "One thing needs you:");
        assert_eq!(needs_you_header(2), "Two things need you:");
        assert_eq!(needs_you_header(4), "Four things need you:");
        assert_eq!(needs_you_header(7), "7 things need you:");
    }

    #[test]
    fn an_empty_period_says_so_in_one_line_and_stops() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        // No file at all — nothing has ever been recorded.
        let now = 100 * H;
        let d = Digest::build(&path, 24 * H, now, UNLICENSED);
        assert_eq!(d.events.len(), 0);
        assert_eq!(d.needs_you.len(), 0);
        assert_eq!(d.skipped, 0);

        let out = render_stdout(&d, &ctx(), false);
        assert_eq!(
            out, "Overnight: nothing happened.\n",
            "empty period must be exactly one line, nothing manufactured: {out:?}"
        );
    }

    #[test]
    fn an_empty_period_with_only_old_history_outside_the_window_also_says_nothing_happened() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        let now = 100 * H;
        // A repair that fully resolved 3 days ago — well outside a 24h window.
        append_repair_ready(&path, "f-old", "an old, already-closed repair", now - 3 * D);
        append_shipped(
            &path,
            "f-old",
            vec![claim("verified", Provenance::Verified)],
            now - 3 * D,
        );

        let d = Digest::build(&path, 24 * H, now, UNLICENSED);
        let out = render_stdout(&d, &ctx(), false);
        assert_eq!(out, "Overnight: nothing happened.\n", "{out:?}");
    }

    #[test]
    fn a_record_with_a_torn_line_discloses_it_honestly_never_silently() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        {
            let mut f = std::fs::File::create(&path).unwrap();
            writeln!(
                f,
                r#"{{"kind":"shipped","recorded_at_ms":{},"failure_id":"f1","repair_sha":"abc","action":"shipped","deploy_cmd":"x","claims":[]}}"#,
                100 * H
            )
            .unwrap();
            // Torn: truncated mid-write, no trailing newline.
            write!(f, r#"{{"kind":"shipped","recorded_at_"#).unwrap();
        }
        let now = 100 * H;
        let d = Digest::build(&path, 24 * H, now, UNLICENSED);
        assert_eq!(d.skipped, 1);

        let out = render_stdout(&d, &ctx(), false);
        assert!(
            out.contains("1 unreadable line skipped while reading the record"),
            "{out}"
        );
        assert!(out.contains("this digest may be incomplete"), "{out}");
    }

    #[test]
    fn a_torn_line_is_disclosed_even_in_an_otherwise_empty_period() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        {
            let mut f = std::fs::File::create(&path).unwrap();
            write!(f, r#"{{"kind":"event","recorded_at_"#).unwrap();
        }
        let now = 100 * H;
        let d = Digest::build(&path, 24 * H, now, UNLICENSED);
        let out = render_stdout(&d, &ctx(), false);
        assert!(out.starts_with("Overnight: nothing happened.\n"), "{out}");
        assert!(out.contains("1 unreadable line skipped"), "the torn line must still be disclosed even when the rest of the period is empty: {out}");
    }

    #[test]
    fn no_request_body_or_payload_string_ever_appears_in_any_rendered_output() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        let now = 100 * H;

        // An event whose (redacted-at-capture, but still never-should-be-
        // read-by-digest) body carries recognizable secret material.
        append_event(&path, "billing", "TypeError", now - H, now - H);

        // A shipped repair whose deploy_cmd is a real, sensitive-looking
        // command string that must never be echoed either.
        append_repair_ready(&path, "f1", "Payment webhook guard", now - 2 * H);
        let outcome = ShipOutcome {
            failure_id: "f1".into(),
            repair_sha: "deadbeef".into(),
            action: "shipped".into(),
            deploy_cmd: "ssh prod.internal 'DEPLOY_TOKEN=sk_live_abc123 ./deploy.sh'".into(),
            claims: vec![claim(
                "https://shop.example.com/health returns 200",
                Provenance::Verified,
            )],
        };
        engine_record::append(&path, "shipped", &outcome, now - H).unwrap();

        let d = Digest::build(&path, 24 * H, now, UNLICENSED);
        let stdout_out = render_stdout(&d, &ctx(), false);
        let slack_out = render_slack_markdown(&d, &ctx());

        for out in [&stdout_out, &slack_out] {
            assert!(
                !out.contains("4242424242424242"),
                "card number must never appear: {out}"
            );
            assert!(
                !out.contains("sk_live_abc123"),
                "secret token must never appear: {out}"
            );
            assert!(
                !out.contains("DEPLOY_TOKEN"),
                "deploy command must never be echoed: {out}"
            );
            assert!(
                !out.contains("ssh prod.internal"),
                "deploy command must never be echoed: {out}"
            );
        }
    }

    // ---------------------------------------------------------------------
    // Focused unit coverage for the pure helpers.
    // ---------------------------------------------------------------------

    #[test]
    fn headline_covers_all_repaired_all_rolled_back_and_mixed_cases() {
        assert_eq!(headline(24 * H, 0, 0, 0), "Overnight: nothing happened.");
        assert_eq!(
            headline(24 * H, 1, 0, 0),
            "Overnight: one failure, repaired."
        );
        assert_eq!(
            headline(24 * H, 0, 0, 1),
            "Overnight: one failure, rolled back."
        );
        assert_eq!(
            headline(24 * H, 0, 1, 0),
            "Overnight: one failure, shipped, but the post-deploy check is unresolved."
        );
        assert_eq!(
            headline(24 * H, 2, 0, 0),
            "Overnight: two failures, both repaired."
        );
        assert_eq!(
            headline(24 * H, 0, 0, 2),
            "Overnight: two failures, both rolled back."
        );
        assert_eq!(
            headline(24 * H, 1, 0, 1),
            "Overnight: two failures \u{2014} one repaired, one rolled back."
        );
        assert_eq!(
            headline(24 * H, 3, 0, 0),
            "Overnight: three failures, all repaired."
        );
    }

    #[test]
    fn period_label_switches_from_overnight_past_thirty_hours() {
        assert_eq!(period_label(24 * H), "Overnight");
        assert_eq!(period_label(30 * H), "Overnight");
        assert_eq!(period_label(48 * H), "In the last 2 days");
        assert_eq!(period_label(7 * D), "In the last 7 days");
        assert_eq!(period_label(36 * H), "In the last 36h");
    }

    #[test]
    fn should_send_now_is_true_when_never_sent_and_respects_the_interval_otherwise() {
        let cfg = ScheduleConfig {
            interval_ms: 24 * H,
        };
        assert!(should_send_now(&cfg, None, 1_000));
        assert!(
            !should_send_now(&cfg, Some(100 * H), 110 * H),
            "10h since last send, 24h interval: not due yet"
        );
        assert!(
            should_send_now(&cfg, Some(100 * H), 124 * H),
            "exactly the interval: due"
        );
        assert!(
            should_send_now(&cfg, Some(100 * H), 200 * H),
            "well past the interval: due"
        );
    }

    #[tokio::test]
    async fn stdout_notifier_reports_its_name_and_succeeds() {
        let d = Digest {
            since_ms: 24 * H,
            now_ms: 100 * H,
            events: vec![],
            needs_you: vec![],
            skipped: 0,
            read_error: None,
        };
        let n = StdoutNotifier {
            ctx: ctx(),
            use_color: false,
        };
        assert_eq!(n.name(), "stdout");
        assert!(n.send(&d).await.is_ok());
    }

    #[tokio::test]
    async fn noop_notifier_always_succeeds_and_prints_nothing() {
        let d = Digest {
            since_ms: 24 * H,
            now_ms: 100 * H,
            events: vec![],
            needs_you: vec![],
            skipped: 0,
            read_error: None,
        };
        let n = NoOpNotifier;
        assert_eq!(n.name(), "noop");
        assert!(n.send(&d).await.is_ok());
    }

    // ---------------------------------------------------------------------
    // `drums digest --send`: the kind-selection logic (pure part). FYI and
    // Decision are the only two kinds a digest can be — a digest never
    // narrates in-flight work (Working) or a matured bet on its own
    // (Learning); those are the engine's per-event notifications.
    // ---------------------------------------------------------------------

    #[test]
    fn digest_send_is_fyi_when_nothing_needs_anyone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        let now = 100 * H;
        // A repair that fully resolved inside the window: real activity,
        // nothing outstanding — investigated; nothing needs you.
        append_repair_ready(
            &path,
            "f1",
            "Image upload timeouts: widened the retry policy",
            now - 5 * H,
        );
        append_shipped(
            &path,
            "f1",
            vec![claim(
                "canaried 4 minutes; error rate back to zero",
                Provenance::Verified,
            )],
            now - 4 * H,
        );

        let d = Digest::build(&path, 24 * H, now, UNLICENSED);
        assert!(d.needs_you.is_empty(), "{:?}", d.needs_you);
        let n = notification(&d, &ctx(), "shop");
        assert_eq!(n.kind, crate::notify::Kind::Fyi);
        assert_eq!(n.title, "nothing needs you");
        assert_eq!(n.repo, "shop");
        assert!(
            n.body.contains("Image upload timeouts"),
            "the body is still the full digest: {}",
            n.body
        );
    }

    #[test]
    fn digest_send_is_a_decision_with_the_count_in_the_title_when_something_needs_you() {
        let (d, _dir) = build_two_failure_example();
        assert_eq!(d.needs_you.len(), 1, "{:?}", d.needs_you);
        let n = notification(&d, &ctx(), "shop");
        assert_eq!(n.kind, crate::notify::Kind::Decision);
        assert_eq!(
            n.title, "One thing needs you",
            "the title carries the count: {}",
            n.title
        );
        assert!(n.body.contains("Session expiry regression"), "{}", n.body);
        assert!(n.body.contains("still awaiting a decision"), "{}", n.body);
        // Same honesty rules as every rendering of this digest: the rollback
        // is in the body at full prominence, never softened away.
        assert!(n.body.contains("rolled back"), "{}", n.body);
    }

    /// The digest body inherits the no-request-data rule wholesale — a
    /// webhook message is pasted into threads and screenshots.
    #[test]
    fn digest_send_body_never_carries_request_data_or_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        let now = 100 * H;
        append_event(&path, "billing", "TypeError", now - H, now - H);

        let d = Digest::build(&path, 24 * H, now, UNLICENSED);
        let n = notification(&d, &ctx(), "shop");
        assert_eq!(
            n.kind,
            crate::notify::Kind::Decision,
            "an unresolved reproduction needs someone"
        );
        assert!(!n.body.contains("4242424242424242"), "{}", n.body);
        assert!(!n.body.contains("sk_live_abc123"), "{}", n.body);
    }

    #[test]
    fn a_directory_at_the_record_path_is_an_honest_read_error_not_a_fabricated_empty_period() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not-a-file");
        std::fs::create_dir(&path).unwrap();
        let d = Digest::build(&path, 24 * H, 100 * H, UNLICENSED);
        assert!(d.read_error.is_some());
        let out = render_stdout(&d, &ctx(), false);
        assert!(out.contains("could not read the record"), "{out}");
    }

    /// A promotion that only appears in `drums authority list` is a promotion
    /// nobody reads, and the ladder stalls at propose forever. It belongs in
    /// the surface the spec ranks highest.
    #[test]
    fn an_earned_promotion_reaches_the_morning_message() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        let class = engine_authority::FailureClass::new("shop", "TypeError");
        for i in 0..engine_authority::PROMOTION_STREAK {
            engine_authority::record_outcome(
                &path,
                &class,
                engine_authority::Outcome::ShippedClean,
                &format!("f{i}"),
                1_000 + i as u64,
            )
            .unwrap();
        }

        let d = Digest::build(&path, 0, 100_000, UNLICENSED);
        let earned: Vec<&NeedsYou> = d
            .needs_you
            .iter()
            .filter(|n| matches!(n, NeedsYou::PromotionEarned { .. }))
            .collect();
        assert_eq!(earned.len(), 1, "{:?}", d.needs_you);

        let out = render_stdout(
            &d,
            &RenderContext {
                repo: std::path::PathBuf::from("/srv/shop"),
                deploy_cmd: None,
            },
            false,
        );
        assert!(out.contains("shop/TypeError"), "{out}");
        assert!(
            out.contains("drums authority promote shop/TypeError"),
            "the exact command must be there: {out}"
        );
        assert!(
            out.contains("ignore this and it stays where it is"),
            "declining must be presented as a real option, not a nag: {out}"
        );
    }

    // ---------------------------------------------------------------------
    // The upgrade line. Said once, on the morning it becomes true, and never
    // allowed to turn the morning message into a sales channel.
    // ---------------------------------------------------------------------

    /// A record whose class earns act-alone at `earned_at`, plus one open
    /// proposal so the digest has real work in it too.
    fn record_with_an_earned_promotion(earned_at: u64) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        let class = engine_authority::FailureClass::new("shop", "TypeError");
        let n = engine_authority::PROMOTION_STREAK;
        for i in 0..n {
            // The last outcome — the one that crosses the streak — lands at
            // `earned_at`; the ones before it are a second apart before that.
            let at = earned_at - u64::from(n - 1 - i) * 1_000;
            engine_authority::record_outcome(
                &path,
                &class,
                engine_authority::Outcome::ShippedClean,
                &format!("f{i}"),
                at,
            )
            .unwrap();
        }
        append_repair_ready(
            &path,
            "f-open",
            "Session expiry regression: extends the refresh window",
            earned_at,
        );
        dir
    }

    #[test]
    fn the_morning_a_promotion_is_earned_says_act_alone_is_paid_exactly_once() {
        let now = 100 * H;
        let dir = record_with_an_earned_promotion(now - 2 * H);
        let d = Digest::build(&dir.path().join("record.jsonl"), 24 * H, now, UNLICENSED);
        let out = render_stdout(&d, &ctx(), false);

        assert!(out.contains("Act-alone is the paid part of Drums"), "{out}");
        assert!(
            out.contains("everything you are running today stays free"),
            "{out}"
        );
        assert!(out.contains("https://drums.sh/#pricing"), "{out}");
        assert_eq!(
            out.matches("paid part of Drums").count(),
            1,
            "said once, not per line: {out}"
        );
        assert!(
            !out.contains('$'),
            "the digest must never quote a price: {out}"
        );

        // ...and it is an addition, never a replacement: the promotion, the
        // command, and the option to decline all still read exactly as before.
        assert!(
            out.contains("drums authority promote shop/TypeError"),
            "{out}"
        );
        assert!(
            out.contains("ignore this and it stays where it is"),
            "{out}"
        );
    }

    /// The next morning, and every morning after: the decision is still open
    /// and still shown, but the sentence about money has aged out with the
    /// window. Anything else is a daily nag.
    #[test]
    fn the_next_morning_shows_the_same_promotion_without_mentioning_money_again() {
        let now = 100 * H;
        // Earned three days ago; today's digest covers the last 24h.
        let dir = record_with_an_earned_promotion(now - 3 * D);
        let d = Digest::build(&dir.path().join("record.jsonl"), 24 * H, now, UNLICENSED);
        let out = render_stdout(&d, &ctx(), false);

        assert!(
            out.contains("shop/TypeError"),
            "the open decision must still be shown: {out}"
        );
        assert!(
            out.contains("drums authority promote shop/TypeError"),
            "{out}"
        );
        assert!(
            !out.contains("paid"),
            "the upgrade line must not repeat every morning: {out}"
        );
        assert!(!out.contains("drums.sh"), "{out}");
    }

    /// A customer who already pays is never told about paying.
    #[test]
    fn a_licensed_machine_is_never_shown_the_upgrade_line() {
        let now = 100 * H;
        let dir = record_with_an_earned_promotion(now - 2 * H);
        let path = dir.path().join("record.jsonl");

        let unlicensed = render_stdout(
            &Digest::build(&path, 24 * H, now, UNLICENSED),
            &ctx(),
            false,
        );
        let licensed = render_stdout(&Digest::build(&path, 24 * H, now, LICENSED), &ctx(), false);

        assert!(unlicensed.contains("paid part of Drums"));
        assert!(!licensed.contains("paid"), "{licensed}");
        assert!(!licensed.contains("drums.sh"), "{licensed}");
        // The ONLY difference is that one line. Everything else a digest
        // reports — what broke, what shipped, what is waiting — is identical.
        assert_eq!(
            unlicensed.replace(&format!("\n  {}", crate::license::digest_paid_note()), ""),
            licensed,
            "license state may change exactly one line of the morning message"
        );
    }

    /// Slack gets the same substance as stdout, or the two renderers have
    /// started disagreeing about what the product costs.
    #[test]
    fn the_upgrade_line_reaches_slack_in_the_same_words() {
        let now = 100 * H;
        let dir = record_with_an_earned_promotion(now - 2 * H);
        let out = render_slack_markdown(
            &Digest::build(&dir.path().join("record.jsonl"), 24 * H, now, UNLICENSED),
            &ctx(),
        );
        assert!(out.contains("Act-alone is the paid part of Drums"), "{out}");
        assert_eq!(out.matches("paid part of Drums").count(), 1, "{out}");
    }

    /// Below the streak, nothing is proposed — the digest must not nag about
    /// authority a class has not earned.
    #[test]
    fn a_class_short_of_the_streak_is_not_mentioned() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.jsonl");
        let class = engine_authority::FailureClass::new("shop", "TypeError");
        engine_authority::record_outcome(
            &path,
            &class,
            engine_authority::Outcome::ShippedClean,
            "f0",
            1_000,
        )
        .unwrap();

        let d = Digest::build(&path, 0, 100_000, UNLICENSED);
        assert!(
            !d.needs_you
                .iter()
                .any(|n| matches!(n, NeedsYou::PromotionEarned { .. })),
            "{:?}",
            d.needs_you
        );
    }
}
