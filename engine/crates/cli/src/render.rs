//! §7-style terminal narration. Pure: EngineEvent + [`RenderContext`] in,
//! ANSI string out. Every claim prints with its provenance chip — never a
//! bare green check.

use std::path::PathBuf;

use crate::engine::EngineEvent;
use engine_core::{Claim, Provenance};

/// What `render` needs beyond the event itself: what `drums watch` was
/// actually configured with, so the `RepairReady` line can print a `drums
/// ship` command that is actually runnable (F4, Task-3 review round 3)
/// instead of a bare `drums ship <id>` that requires a REQUIRED
/// `--deploy-cmd` clap arg neither this line nor a `cd`'d-elsewhere operator
/// supplied. Built once in `main.rs` from the same `EngineConfig` fields
/// already echoed in the `--repair auto` banner.
pub struct RenderContext {
    pub repo: PathBuf,
    pub deploy_cmd: Option<String>,
}

fn secs(elapsed_ms: u64) -> String {
    format!("{:.1}s", elapsed_ms as f64 / 1000.0)
}

// `pub(crate)`: reused by `record_cmd.rs` and `why.rs` so the plain-text
// palette has one definition, not a fourth copy (already duplicated once in
// `ui/mod.rs`) — new read-only views should not add a fifth.
/// Whether ANSI styling should be emitted at all: only when stdout is a
/// real terminal, `NO_COLOR` is unset, and TERM is not "dumb". Computed
/// once. `--plain` output piped to a file or CI was receiving raw escape
/// codes (audit R6); the constants stay — [`finish`] strips them at the
/// output boundary when styling is off, so every narration site keeps its
/// one format string.
pub(crate) fn color_enabled() -> bool {
    use std::io::IsTerminal;
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::io::stdout().is_terminal()
            && std::env::var_os("NO_COLOR").is_none()
            && std::env::var("TERM").map(|t| t != "dumb").unwrap_or(true)
    })
}

/// Strip the escape sequences this module itself emits. Input text was
/// already `sanitize`d (C0 controls removed), so any ESC here is ours.
pub(crate) fn strip_style(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Consume a CSI sequence: ESC, then '[', then parameter bytes,
            // then one final byte in @-~. The '[' must be eaten before the
            // final-byte scan — '[' itself sits inside @-~, and treating it
            // as the final byte leaves fragments like "0m" in the output.
            if chars.peek() == Some(&'[') {
                chars.next();
                for f in chars.by_ref() {
                    if ('@'..='~').contains(&f) {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// The output boundary: styled for a terminal, plain for everything else.
pub(crate) fn finish(line: String) -> String {
    if color_enabled() {
        line
    } else {
        strip_style(&line)
    }
}

pub(crate) const DIM: &str = "\x1b[2m";
pub(crate) const RED: &str = "\x1b[31m";
pub(crate) const GREEN: &str = "\x1b[32m";
pub(crate) const BOLD: &str = "\x1b[1m";
pub(crate) const RESET: &str = "\x1b[0m";

fn chip(c: &Claim) -> String {
    format!("[{}]", c.provenance.chip())
}

/// ANSI truecolor for a provenance chip, matching the TUI's own legend
/// (`ui/model.rs::chip_color`'s `ratatui::style::Color` values, restated as
/// escape codes for the plain-text views that don't go through ratatui:
/// `record_cmd.rs` and `why.rs`). One legend, two renderers — kept in sync
/// by the `chip_ansi_matches_the_tui_legend` test rather than by hand.
pub(crate) fn chip_ansi(p: engine_core::Provenance) -> &'static str {
    use engine_core::Provenance::*;
    match p {
        Verified => "\x1b[32m",              // Color::Green
        Observed => "\x1b[34m",              // Color::Blue
        Inferred => "\x1b[38;2;215;154;0m",  // Color::Rgb(0xd7, 0x9a, 0x00) — amber
        Approved => "\x1b[38;2;155;89;208m", // Color::Rgb(0x9b, 0x59, 0xd0) — violet
        Unresolved => "\x1b[31m",            // Color::Red
    }
}

/// Strips every C0 control character (including ESC, `\x1b`) and every C1
/// control character (`char::is_control()` covers both ranges plus DEL) out
/// of `s` before it is ever interpolated into a printed line.
///
/// Every field this is applied to is attacker-controlled and reaches a
/// terminal verbatim: `error_name`/`error_message`/`service`/
/// `request.method`/`request.path` arrive over `POST /v1/events` with no
/// validation (`crates/ingest/src/lib.rs`), and `summary`/`agent` are the
/// repair LLM's own output. Without this, a crafted `error_name` such as
/// `TypeError\x1b[32m [verified]\x1b[0m\x1b[2m` forges a green `[verified]`
/// chip that was never earned — the exact provenance vocabulary (spec §1)
/// this product's trust claims depend on — and an embedded `\r` overwrites
/// the rest of the printed line. One sanitizer, applied everywhere a
/// record-derived or wire-derived string reaches a terminal
/// (`record_cmd::describe`, `why::render`, and this module's own `render`),
/// rather than a fourth ad hoc escaping scheme.
pub(crate) fn sanitize(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

/// Multi-line counterpart to [`sanitize`], for the diagnostic blobs that are
/// legitimately multi-line free text: `ReproError::BootTimeout`'s folded
/// boot stderr and `ReproError::Worktree`'s raw git stderr
/// (`crates/repro/src/lib.rs`), `RepairFailure.why`'s
/// `` test script `{t}` failed: {jest stderr} `` (`crates/cli/src/engine.rs`),
/// and `ShipError::DeployCommandFailed`/`DeployCommandTimeout`'s
/// `failure_detail` — the deploy command's raw trimmed stderr
/// (`crates/cli/src/ship.rs`). Those four arms are the ONLY call sites; every
/// other field this module prints is a single line and takes [`sanitize`].
/// These strings are process-local (this process spawned the child that
/// produced them) but NOT fully trusted — an app under repair can echo
/// attacker-controlled request data straight to its own stderr, so the same
/// forgery `sanitize` defends against is reachable here too, just one hop
/// removed.
///
/// `sanitize` cannot be reused for these fields: stripping every `\n`
/// collapses a genuinely multi-line diagnostic (a 6-line jest failure
/// report, say) into one unbroken run, which was `drums watch`'s only
/// operator-facing answer to "why was my repair refused" before this fix and
/// is worse than the escape-injection `sanitize` exists to stop. So this
/// keeps `\n` and `\t` — the two whitespace controls that make a multi-line
/// diagnostic readable — while still stripping `ESC`, `CR`, and every other
/// C0/C1 control character exactly as `sanitize` does.
///
/// Keeping `\n` reopens a narrower version of H1: an injected `\n` could
/// otherwise start a new line at column 0 that a scrollback-skimming
/// operator mistakes for a top-level narration row (`▲ ... failing`, a
/// `drums record` row, etc.) rather than a continuation of the diagnostic
/// that precedes it. So every line after the first is indented — a line
/// this function emits can never start at column 0.
///
/// Never apply this to a field that is displayed as a single line
/// (`record_cmd::describe` rows, `why` occurrence lines, claim text,
/// service/agent/author/description/sha, and `ShipWithheld`'s
/// `ProposeReason::withheld_text()` sentence): permitting `\n` there would
/// reopen a *row*-forgery vector, strictly worse than the chip-forgery H1
/// named. `ShipWithheld` is the one that was actually gotten wrong, and shows
/// why the rule is about the SHAPE of the field rather than how trusted its
/// source feels: `withheld_text()` looks internally generated, but it
/// interpolates `Intake::source()`, which `POST /v1/events` accepts unvalidated
/// — and the 3-space continuation indent this function adds is byte-identical
/// to the indent a claim row is printed with, so the injected line read as an
/// earned claim. Use plain [`sanitize`] for those.
pub(crate) fn sanitize_multiline(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .filter(|&c| c == '\n' || c == '\t' || !c.is_control())
        .collect();
    let mut lines = cleaned.split('\n');
    let mut out = lines.next().unwrap_or("").to_string();
    for line in lines {
        out.push_str("\n   ");
        out.push_str(line);
    }
    out
}

/// Shorten a sha for display without ever byte-slicing it.
///
/// Closing round (F1 and its audit): every string shortened for narration in
/// this crate arrives from outside the process — `DeployRecord.sha` comes
/// straight off `POST /v1/deploys`, which validates nothing
/// (`crates/ingest/src/lib.rs`'s `post_deploy`), and `ShipOutcome.repair_sha`
/// is read back out of `.drums/record.jsonl`. `&sha[..6]` panicked whenever
/// byte 6 landed mid-character, and in `drums watch` that panic is inside
/// the render loop, so one ingested record took the whole monitoring process
/// down. Shortening is a DISPLAY concern and must never be able to fail;
/// refusing a malformed sha is a separate, earlier decision (`ship.rs`'s
/// `validate_recorded_sha`), made where there is still something to refuse.
pub fn short(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// The 6-character form used everywhere a sha is narrated. Matches what
/// `ship.rs`'s `shipped_footer` already printed, so the fix changes no
/// well-formed output.
/// A page path for a sentence. Empty means the source could not say which
/// page, and "rage clicks on " with a hole reads like a rendering bug.
pub(crate) fn frustration_page(path: &str) -> String {
    if path.is_empty() {
        "an unknown page".to_string()
    } else {
        sanitize(path)
    }
}

pub fn short_sha(s: &str) -> String {
    short(s, 6)
}

/// Builds the exact `drums <sub> <failure_id>` invocation from `ctx` (F4, and
/// F6 for the `revert` side): always appends `--repo <repo>` — an absolute
/// path is correct regardless of the operator's cwd, closing the second F4
/// failure mode (`drums ship`/`drums revert` resolving their record path from
/// cwd while `drums watch --repo` wrote elsewhere) — and either the
/// `--deploy-cmd` actually configured on `drums watch` (shell-quoted, since it
/// is copy-pasted verbatim into a shell), or an explicit placeholder the
/// operator must fill in when none was configured — never a bare command that
/// LOOKS complete and silently depends on a REQUIRED clap arg nobody supplied.
///
/// `Commands::Ship` and `Commands::Revert` declare the SAME two arguments
/// (`main.rs`: `deploy_cmd: String` required, `repo: Option<PathBuf>` resolved
/// against cwd), so both printed commands are built here, once — F6 existed
/// precisely because the `revert` line was written by hand next to a
/// `ship_command_line` that already knew better.
fn command_line(subcommand: &str, failure_id: &str, ctx: &RenderContext) -> String {
    let deploy_arg = match &ctx.deploy_cmd {
        Some(cmd) => shell_quote(cmd),
        None => "'<your deploy command>'".to_string(),
    };
    format!(
        "drums {subcommand} {failure_id} --deploy-cmd {deploy_arg} --repo {}",
        shell_quote(&ctx.repo.display().to_string())
    )
}

/// The runnable `drums ship <id>` for the propose-mode handoff (F4).
pub(crate) fn ship_command_line(failure_id: &str, ctx: &RenderContext) -> String {
    command_line("ship", failure_id, ctx)
}

/// The runnable `drums revert <id>` printed with every successful ship (F6).
///
/// `Commands::Revert`'s `--deploy-cmd` is a REQUIRED clap arg, so the bare
/// `drums revert <id>` this used to print exited 2 — and it is read at the
/// worst possible moment: AFTER an autonomous deploy has already reached
/// production, when the operator is under time pressure and must not have to
/// reverse-engineer two flags before anything rolls back. The template is the
/// same one that deployed: `revert` substitutes `{sha}` with the ROLLBACK sha
/// it resolves from the record itself.
pub(crate) fn revert_command_line(failure_id: &str, ctx: &RenderContext) -> String {
    command_line("revert", failure_id, ctx)
}

/// POSIX single-quote escaping: wraps `s` in `'...'`, replacing any embedded
/// `'` with `'\''` — so a deploy command or repo path containing spaces,
/// `$`, or other shell metacharacters survives being copy-pasted into a
/// shell exactly as configured.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The metric vocabulary for a measured direction — improved/neutral/
/// regressed. These words describe what the NUMBER did; the verdict
/// vocabulary (supported/not supported/inconclusive) belongs to bets alone,
/// and a revisit or outcome line never borrows it.
pub(crate) fn direction_word(d: engine_core::evaluation::Direction) -> &'static str {
    use engine_core::evaluation::Direction;
    match d {
        Direction::Positive => "improved",
        Direction::Neutral => "neutral",
        Direction::Negative => "regressed",
    }
}

/// One reading, with its metric's own units: an error-event rate is per hour
/// over hours watched; behavior metrics are plain values over entries. One
/// definition, used by the watch narration, the Slack body, and the bet card.
pub(crate) fn metric_reading(
    metric: engine_core::evaluation::Metric,
    from: f64,
    to: f64,
    entries: u32,
) -> String {
    match metric {
        engine_core::evaluation::Metric::ErrorEventRate => {
            format!("{from:.2}/h → {to:.2}/h over {entries}h")
        }
        _ => format!("{from:.2} → {to:.2} over {entries} entries"),
    }
}

pub fn render(event: &EngineEvent, ctx: &RenderContext) -> String {
    finish(render_styled(event, ctx))
}

fn render_styled(event: &EngineEvent, ctx: &RenderContext) -> String {
    match event {
        EngineEvent::DeployRecorded(d) => {
            let short = short_sha(&d.sha);
            format!("{DIM}deploy {short} \"{}\" · {}{RESET}\n", sanitize(&d.description), sanitize(&d.author))
        }
        EngineEvent::OutcomeMeasured(out) => {
            use engine_core::evaluation::{Guardrails, Outcome};
            let verdict = match &out.outcome {
                Outcome::Measured { direction, from, to, guardrails, .. } => {
                    let dir = direction_word(*direction);
                    let guard = match guardrails {
                        Guardrails::Held => String::new(),
                        Guardrails::Regressed(ms) => format!(
                            " · guardrails regressed: {}",
                            ms.iter().map(|m| m.label()).collect::<Vec<_>>().join(", ")
                        ),
                    };
                    format!("{dir} {from:.2} → {to:.2}{guard}")
                }
                Outcome::Unmeasured(u) => u.sentence(),
            };
            let unread = if out.unread_guardrails.is_empty() {
                String::new()
            } else {
                format!(" · unread guardrails: {}", out.unread_guardrails.join(", "))
            };
            format!(
                "{DIM}outcome{RESET} {} · {}{}\n",
                sanitize(&out.change.0),
                verdict,
                unread,
            )
        }
        // The slow loop's line: both readings stated, the original verdict
        // never re-labeled. The revisit describes what the metric reads NOW;
        // "(was: … at close)" is what the declared window read then, and both
        // stand in the record side by side.
        EngineEvent::RevisitMeasured { change, horizon_days, outcome, metric, was, .. } => {
            use engine_core::evaluation::Outcome;
            let now_reads = match outcome {
                Outcome::Measured { direction, from, to, entries, .. } => format!(
                    "{} {}",
                    direction_word(*direction),
                    metric_reading(*metric, *from, *to, *entries)
                ),
                Outcome::Unmeasured(u) => u.sentence(),
            };
            let then_read = match was {
                Some(d) => format!("{} at close", direction_word(*d)),
                None => "unmeasured at close".to_string(),
            };
            format!(
                "{DIM}revisit{RESET} {} at {}d — {} {DIM}(was: {}){RESET}\n",
                sanitize(change),
                horizon_days,
                now_reads,
                then_read,
            )
        }
        EngineEvent::BetEvaluated { bet, belief, verdict, measured } => {
            let m = match measured {
                Some((from, to, entries)) => format!("{from:.2} → {to:.2} over {entries}"),
                None => "outcome unmeasured".into(),
            };
            let level = crate::bet_cmd::level_word(verdict.causal_confidence.level);
            let unread = if verdict.unread_guardrails.is_empty() {
                String::new()
            } else {
                format!("\n  guardrails not read: {}", verdict.unread_guardrails.join(", "))
            };
            format!(
                "{DIM}bet{RESET} {} · {} · {}\n  \"{}\"\n  causal confidence {}: {}{}\n",
                sanitize(bet),
                crate::bet_cmd::support_word(verdict.support),
                m,
                sanitize(belief),
                level,
                verdict.causal_confidence.basis,
                unread,
            )
        }
        // A draft is an interpretation — `inferred`, the same chip a bet
        // itself carries — and the line ends in the one command that turns it
        // into a commitment, because confirmation is a human act.
        EngineEvent::BetDrafted { bet, belief, by } => {
            format!(
                "{}[inferred]{RESET} Drums drafted bet {} — \"{}\" {DIM}· by {}{RESET} — confirm: {BOLD}drums bet confirm {}{RESET}\n",
                chip_ansi(engine_core::Provenance::Inferred),
                sanitize(bet),
                sanitize(belief),
                sanitize(by),
                sanitize(bet),
            )
        }
        EngineEvent::ObservationRecorded(o) => {
            // A fact, narrated as one: no severity, no diagnosis, correlation
            // said as correlation. The hypothesis is where opinions live.
            let sessions = o
                .affected
                .sessions
                .map(|s| format!(" across {s} sessions"))
                .unwrap_or_default();
            match &o.kind {
                engine_core::observation::Kind::RateShift { previous, since_deploy } => {
                    let now_rate = o.measure.map(|m| m.sample.value).unwrap_or(0.0);
                    let n = o.measure.map(|m| m.sample.entries).unwrap_or(0);
                    format!(
                        "{DIM}observed{RESET} error rate {:.2}/h → {:.2}/h ({} events) after deploy {} · {}\n",
                        previous,
                        now_rate,
                        n,
                        short_sha(&since_deploy.clone().unwrap_or_default()),
                        sanitize(&o.id.0),
                    )
                }
                engine_core::observation::Kind::RageClick { path, clicks } => format!(
                    "{DIM}observed{RESET} rage clicks on {} — {} clicks{} · {}\n",
                    frustration_page(path),
                    clicks,
                    sessions,
                    sanitize(&o.id.0),
                ),
                engine_core::observation::Kind::DeadClick { path, clicks } => format!(
                    "{DIM}observed{RESET} dead clicks on {} — {} clicks that did nothing{} · {}\n",
                    frustration_page(path),
                    clicks,
                    sessions,
                    sanitize(&o.id.0),
                ),
                _ => format!("{DIM}observed{RESET} {}\n", sanitize(&o.id.0)),
            }
        }
        EngineEvent::FailureDetected(f) => format!(
            // A trigger-intake failure (OTel span, log alert, human report)
            // arrives with no replayable request, so there is no method+path to
            // show. Name the signature instead — and never imply we hold a
            // request we do not have, because that request is what reproduction
            // would replay.
            "{RED}▲{RESET} {BOLD}{} failing{RESET}\n  {} · {} {}\n",
            sanitize(&f.service),
            // `replayable_request()`, not `sample.request`: a trigger intake that
            // happens to carry a request reconstructed from span attributes must
            // not be shown as though we hold the request that failed.
            match f.replayable_request() {
                // The record redacts query-string secrets at capture time;
                // narration must not undo that by printing the raw path into
                // a terminal, a pipe, or a CI log (audit B3).
                Some(r) => format!(
                    "{} {}",
                    sanitize(&r.method),
                    sanitize(&engine_record::redact_query_string(&r.path, &[]))
                ),
                None => format!(
                    "{} in {} (no replayable request captured)",
                    sanitize(&f.signature.error_name),
                    sanitize(&f.signature.top_frame_file)
                ),
            },
            sanitize(&f.claim.text),
            chip(&f.claim)
        ),
        EngineEvent::Attributed(_f, a) => {
            let short = short_sha(&a.deploy.sha);
            format!(
                "{DIM}├─{RESET} attributing\n   deploy {BOLD}{short}{RESET} · \"{}\" · {}\n   {} {}\n",
                sanitize(&a.deploy.description),
                sanitize(&a.deploy.author),
                sanitize(&a.claim.text),
                chip(&a.claim)
            )
        }
        EngineEvent::AttributionMissing(_f) => {
            format!("{DIM}├─{RESET} attributing\n   no deploy precedes this failure [unresolved]\n")
        }
        EngineEvent::AttributionErrored(_f, why) => {
            format!("{DIM}├─{RESET} attributing\n   attribution failed: {} [unresolved]\n", sanitize_multiline(why))
        }
        EngineEvent::Reproducing(_f, a) => {
            let short = short_sha(&a.deploy.sha);
            format!("{DIM}├─{RESET} reproducing\n   {DIM}building revision {short}, replaying the captured request{RESET}\n")
        }
        // `Reproduced` is the event for "the reproduction attempt COMPLETED",
        // not for "it reproduced" — `Reproduction::reproduced` carries that,
        // and it is false whenever the replay's status or signature did not
        // match. This footer used to say "reproduction confirmed"
        // unconditionally, so a run that had just printed "could not reproduce
        // ... [unresolved]" contradicted itself on the very next line and
        // ended on the reassuring one. Read the flag.
        EngineEvent::Reproduced(_f, _a, r) => {
            let mut out = String::new();
            for c in &r.claims {
                let arrow = if r.reproduced { GREEN } else { RED };
                out.push_str(&format!("   {arrow}→{RESET} {} {}\n", sanitize(&c.text), chip(c)));
            }
            if r.reproduced {
                out.push_str(&format!("{DIM}└─{RESET} {GREEN}reproduction confirmed{RESET}\n"));
            } else {
                out.push_str(&format!(
                    "{DIM}└─{RESET} {RED}not reproduced{RESET} — {} {DIM}(no repair attempted: a fix cannot be verified against a failure Drums could not make happen){RESET}\n",
                    sanitize(&r.detail),
                ));
            }
            out
        }
        EngineEvent::ReproFailed(_f, _a, why) => {
            format!("{DIM}└─{RESET} {RED}reproduction failed{RESET} — {} [unresolved]\n", sanitize_multiline(why))
        }
        // Deliberately worded as "not attempted", not "failed": there was never
        // a request to replay, so there is nothing here that a retry or a
        // less-flaky environment would fix. The line names the intake source so
        // the reader knows which adapter opened it and why the strongest
        // evidence in the loop is unavailable for this failure.
        EngineEvent::ReproSkippedNotReplayable(f, _a, claim) => format!(
            "{DIM}├─{RESET} reproducing\n   {DIM}skipped — {} carries no replayable request{RESET}\n   {} {}\n",
            sanitize(&f.intake.label()),
            sanitize(&claim.text),
            chip(claim)
        ),
        EngineEvent::Repairing(_f, agent) => {
            format!("{DIM}├─{RESET} repairing\n   {DIM}agent: {}{RESET}\n", sanitize(agent))
        }
        EngineEvent::RepairFailed(_f, detail) => {
            let mut out = format!(
                "{DIM}└─{RESET} {RED}repair failed{RESET} — {} [unresolved] ({}){RESET}\n",
                sanitize_multiline(&detail.why),
                secs(detail.elapsed_ms)
            );
            if let Some(wt) = &detail.worktree {
                out.push_str(&format!("   {DIM}left for inspection: {}", sanitize(wt)));
                if let Some(branch) = &detail.branch {
                    out.push_str(&format!(" (branch {})", sanitize(branch)));
                }
                out.push_str(&format!("{RESET}\n"));
            }
            out
        }
        EngineEvent::RepairReady(_f, repair, elapsed_ms) => {
            let mut out = format!("{DIM}├─{RESET} repaired by {BOLD}{}{RESET} ({})\n", sanitize(&repair.agent), secs(*elapsed_ms));
            for c in &repair.claims {
                out.push_str(&format!("   {GREEN}→{RESET} {} {}\n", sanitize(&c.text), chip(c)));
            }
            out.push_str(&format!(
                "{DIM}└─{RESET} {GREEN}repair ready{RESET} — run: {BOLD}{}{RESET}\n",
                ship_command_line(&repair.failure_id, ctx)
            ));
            out
        }
        EngineEvent::Shipped(_f, outcome) => {
            let mut out = format!("{DIM}├─{RESET} shipping\n");
            for c in &outcome.claims {
                out.push_str(&format!("   {GREEN}→{RESET} {} {}\n", sanitize(&c.text), chip(c)));
            }
            // Shared with `ship::narrate_shipped` (fix round, round-2 N3 +
            // carried minor 9): both the `--repair auto` path here and the
            // standalone `drums ship` process go through the ONE
            // `ship::ship` implementation, so the footer — including whether
            // the `reversible: drums revert <id>` promise is honest at all,
            // which depends on the record line having actually been written
            // — must be the one function too.
            out.push_str(&crate::ship::shipped_footer(outcome, ctx));
            out
        }
        // `sanitize_multiline`, not `sanitize`: this `why` is `ShipError`'s
        // Display, and `DeployCommandFailed`/`DeployCommandTimeout` carry
        // `ship::failure_detail` — the deploy command's raw trimmed stderr,
        // multi-line by nature. Same R1 class as `RepairFailed`'s jest report:
        // this is the operator's only account of why a production deploy was
        // refused, and flattening it is worse than the escape injection the
        // sanitizer exists to stop.
        EngineEvent::ShipFailed(_f, why) => {
            format!("{DIM}└─{RESET} {RED}ship failed{RESET} — {} [unresolved]\n", sanitize_multiline(why))
        }
        // `--repair auto` was asked for and the authority gate refused. Not an
        // error (nothing went wrong; the repair is ready and waiting), so it is
        // not red — but it must be visible, because the operator asked for an
        // unattended ship and did not get one. `RepairReady`'s `drums ship`
        // command was already printed above, so this only has to say why.
        //
        // Plain `sanitize`, NOT `sanitize_multiline`: this `why` is
        // `ProposeReason::withheld_text()`, a single-line sentence that
        // interpolates `Intake::source()` — a string deserialized straight off
        // `POST /v1/events`, which validates nothing. Permitting `\n` here let
        // a posted `source` open a second line, and `sanitize_multiline`'s
        // 3-space continuation indent is exactly the indent every claim row
        // above is printed with, so the forged line read as an earned claim.
        // Diagnostic blobs get the multi-line sanitizer; single-line sentences
        // never do.
        // A proposal is the moment the work leaves the terminal. One line,
        // ending in the URL, so it is the thing a human's eye lands on.
        EngineEvent::Proposed(_, p) => {
            format!("{GREEN}proposed{RESET} {} {DIM}·{RESET} {}\n", sanitize(&p.branch), sanitize(&p.url))
        }
        // Never silence: a verified repair nobody can see is exactly the
        // failure mode this surface exists to prevent.
        EngineEvent::ProposalFailed(_, why) => {
            format!("{RED}proposal failed{RESET} {DIM}·{RESET} {} {DIM}(the repair still exists on its branch){RESET}\n", sanitize(why))
        }
        // A reported-issue repair. The unresolved claim is printed like any
        // other claim rather than being summarised away — it is the whole
        // point of this class.
        EngineEvent::ReportedRepairReady(issue, branch, claims, ms) => {
            let mut out = format!(
                "{DIM}├─{RESET} repaired a {} report {DIM}({}){RESET}\n",
                sanitize(&issue.source),
                secs(*ms)
            );
            for c in claims {
                let arrow = if c.provenance == Provenance::Unresolved { RED } else { GREEN };
                out.push_str(&format!("   {arrow}→{RESET} {} {}\n", sanitize(&c.text), chip(c)));
            }
            out.push_str(&format!(
                "{DIM}└─{RESET} on {} {DIM}· nothing ships: a reported issue has nothing to replay{RESET}\n",
                sanitize(branch)
            ));
            out
        }
        EngineEvent::ReportedRepairFailed(issue, why) => {
            format!(
                "{DIM}└─{RESET} {RED}could not repair the {} report{RESET} — {} [unresolved]\n",
                sanitize(&issue.source),
                sanitize(why)
            )
        }
        EngineEvent::ReportedCommented(_, claim) => {
            format!("   {GREEN}→{RESET} {} {}\n", sanitize(&claim.text), chip(claim))
        }
        EngineEvent::ReportedCommentFailed(issue, why) => {
            format!(
                "   {RED}→{RESET} could not comment on the {} issue — {} {DIM}(the repair still exists){RESET}\n",
                sanitize(&issue.source),
                sanitize(why)
            )
        }
        // A class losing act-alone is a change to what this software may do
        // without asking. It gets its own line, in red, every time.
        EngineEvent::Demoted(class, why) => {
            format!(
                "{RED}authority reduced{RESET} {} {DIM}·{RESET} {} {DIM}— it now proposes instead of shipping{RESET}\n",
                sanitize(class),
                sanitize(why)
            )
        }
        EngineEvent::AuthorityWriteFailed(class, why) => {
            format!(
                "{RED}could not record the authority outcome for {}{RESET} — {} {DIM}(the ladder is rebuilt from the record, so this class may keep authority it should have lost){RESET}\n",
                sanitize(class),
                sanitize(why)
            )
        }
        EngineEvent::ShipWithheld(_f, why) => {
            format!("{DIM}└─{RESET} ship withheld — {} [unresolved]\n", sanitize(why))
        }
        EngineEvent::Reported(issue) => {
            // Quiet, one-line narration — a `Reported` item never enters the
            // failure/repair pipeline, so it gets none of the multi-line
            // ├─/└─ narration those events do; just an honest record that a
            // human said so.
            let claim = &issue.claim;
            format!("{DIM}◦{RESET} reported via {} · \"{}\" · {} {}\n", issue.source, issue.title, claim.text, chip(claim))
        }
        // The repair left this machine. Two outcomes, and the second one is the
        // whole reason the authority ladder is consulted before the POST: a job
        // that is waiting on a person is NOT running, and the URL is the only
        // way the operator can unblock it from here.
        EngineEvent::RepairDispatched(_f, accepted) => match &accepted.approval_url {
            Some(url) => {
                let mut out = format!(
                    "{DIM}└─{RESET} repair waiting on a human {DIM}· job {}{RESET} [unresolved]\n   {BOLD}approve:{RESET} {}\n",
                    sanitize(&accepted.job_id),
                    sanitize(url),
                );
                if let Some(expires) = &accepted.expires_at {
                    out.push_str(&format!("   {DIM}the link expires {}{RESET}\n", sanitize(expires)));
                }
                out
            }
            None => format!(
                "{DIM}└─{RESET} {GREEN}repair dispatched{RESET} {DIM}· job {} · running in your CI{RESET}\n",
                sanitize(&accepted.job_id),
            ),
        },
        // Never silence, and never fatal. `drums watch` keeps observing: the
        // failure was still detected, attributed and reproduced here, and all
        // of that is already in the record.
        EngineEvent::RepairDispatchFailed(_f, why) => format!(
            "{DIM}└─{RESET} {RED}repair not dispatched{RESET} — {} {DIM}(still watching){RESET} [unresolved]\n",
            sanitize_multiline(why),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_core::*;

    /// The default fixture context for every test not itself exercising F4's
    /// `RenderContext` plumbing: an arbitrary repo path and no configured
    /// `--deploy-cmd`.
    fn ctx() -> RenderContext {
        RenderContext {
            repo: PathBuf::from("/srv/shop"),
            deploy_cmd: None,
        }
    }

    /// Builds a full `Reproduced` event from engine-core types (coherent
    /// values, at least one Verified claim) and renders it.
    fn render_reproduced_fixture() -> String {
        let failure = Failure {
            intake: Intake::Snippet,
            id: "f1".into(),
            service: "shop".into(),
            signature: ErrorSignature { error_name: "TypeError".into(), top_frame_file: "lib/cart/total.js".into(), top_frame_function: Some("computeTotal".into()) },
            first_seen_ms: 1_753_000_360_000,
            event_count: 41,
            sample: ErrorEvent {
                service: "shop".into(),
                occurred_at_ms: 1_753_000_360_000,
                error_name: "TypeError".into(),
                error_message: "Cannot read properties of undefined (reading 'code')".into(),
                stack: "TypeError: Cannot read properties of undefined (reading 'code')\n    at computeTotal (/srv/shop/lib/cart/total.js:14:31)".into(),
                request: Some(CapturedRequest { method: "POST".into(), path: "/api/checkout".into(), content_type: Some("application/json".into()), body: Some(r#"{"items":[]}"#.into()) }),
                intake: Intake::Snippet,
            },
            claim: Claim { text: "41 errors matching TypeError in lib/cart/total.js within 60s".into(), provenance: Provenance::Observed },
        };
        let attribution = Attribution {
            deploy: DeployRecord {
                sha: "abc1234def".into(),
                description: "add promo code field".into(),
                author: "maya".into(),
                deployed_at_ms: 1_753_000_000_000,
            },
            overlap_files: vec!["lib/cart/total.js".into()],
            minutes_after_deploy: 6,
            claim: Claim {
                text:
                    "first error 6 min after deploy abc123; 1 of 1 changed files in the stack trace"
                        .into(),
                provenance: Provenance::Inferred,
            },
        };
        let reproduction = Reproduction {
            sha: "abc1234def".into(),
            reproduced: true,
            parent_clean: Some(true),
            detail: "replay 500 at deploy, 200 at parent".into(),
            claims: vec![
                Claim {
                    text: "replayed the captured request at abc123: same TypeError".into(),
                    provenance: Provenance::Verified,
                },
                Claim {
                    text: "parent of abc123 serves the same request cleanly".into(),
                    provenance: Provenance::Verified,
                },
            ],
        };
        render(
            &EngineEvent::Reproduced(failure, attribution, reproduction),
            &ctx(),
        )
    }

    #[test]
    fn failure_line_carries_observed_chip_and_counts() {
        let f = Failure {
            intake: Intake::Snippet,
            id: "f1".into(),
            service: "shop".into(),
            signature: ErrorSignature {
                error_name: "TypeError".into(),
                top_frame_file: "server.js".into(),
                top_frame_function: None,
            },
            first_seen_ms: 0,
            event_count: 41,
            sample: ErrorEvent {
                service: "shop".into(),
                occurred_at_ms: 0,
                error_name: "TypeError".into(),
                error_message: "m".into(),
                stack: String::new(),
                request: Some(CapturedRequest {
                    method: "POST".into(),
                    path: "/api/checkout".into(),
                    content_type: None,
                    body: None,
                }),
                intake: Intake::Snippet,
            },
            claim: Claim {
                text: "41 errors matching TypeError in server.js within 60s".into(),
                provenance: Provenance::Observed,
            },
        };
        let out = render(&EngineEvent::FailureDetected(f), &ctx());
        assert!(out.contains("POST /api/checkout"));
        assert!(out.contains("[observed]"));
        assert!(out.contains("41 errors"));
    }

    #[test]
    fn reproduced_line_carries_verified_chips_and_confirms_reproduction() {
        let out = render_reproduced_fixture(); // helper constructing Reproduced event
        assert!(out.contains("[verified]"));
        assert!(out.contains("reproduction confirmed"));
        // Repair now exists (Stage 2) — the line must not claim it doesn't.
        assert!(!out.contains("Stage 2"), "the reproduction line must not make a now-false claim about repair not existing yet: {out}");
    }

    fn sample_failure() -> Failure {
        Failure {
            intake: Intake::Snippet,
            id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
            service: "shop".into(),
            signature: ErrorSignature {
                error_name: "TypeError".into(),
                top_frame_file: "server.js".into(),
                top_frame_function: None,
            },
            first_seen_ms: 0,
            event_count: 3,
            sample: ErrorEvent {
                service: "shop".into(),
                occurred_at_ms: 0,
                error_name: "TypeError".into(),
                error_message: "m".into(),
                stack: String::new(),
                request: Some(CapturedRequest {
                    method: "POST".into(),
                    path: "/api/checkout".into(),
                    content_type: None,
                    body: None,
                }),
                intake: Intake::Snippet,
            },
            claim: Claim {
                text: "3 errors".into(),
                provenance: Provenance::Observed,
            },
        }
    }

    /// A minimal `Attribution` fixture for the arms that carry one alongside
    /// their `why` string (`ReproFailed`).
    fn sample_attribution() -> Attribution {
        Attribution {
            deploy: DeployRecord {
                sha: "abc1234def".into(),
                description: "add promo code field".into(),
                author: "maya".into(),
                deployed_at_ms: 1_753_000_000_000,
            },
            overlap_files: vec!["lib/cart/total.js".into()],
            minutes_after_deploy: 6,
            claim: Claim {
                text:
                    "first error 6 min after deploy abc123; 1 of 1 changed files in the stack trace"
                        .into(),
                provenance: Provenance::Inferred,
            },
        }
    }

    // -- the hosted seam ------------------------------------------------------

    #[test]
    fn a_dispatched_repair_names_the_job_and_says_where_it_is_running() {
        let out = render(
            &EngineEvent::RepairDispatched(
                sample_failure(),
                crate::dispatch::Accepted {
                    job_id: "8f0c2a1e".into(),
                    approval_url: None,
                    expires_at: None,
                },
            ),
            &ctx(),
        );
        assert!(out.contains("repair dispatched"), "{out}");
        assert!(
            out.contains("8f0c2a1e"),
            "the job id is how a human follows it: {out}"
        );
        assert!(out.contains("your CI"), "{out}");
    }

    /// The approval URL is the ONE thing the operator can act on, and while it
    /// is unanswered nothing is running. It must be printed, and the line must
    /// not read like a completed dispatch.
    #[test]
    fn a_repair_waiting_on_a_human_prints_the_approval_url() {
        let out = render(
            &EngineEvent::RepairDispatched(
                sample_failure(),
                crate::dispatch::Accepted {
                    job_id: "8f0c2a1e".into(),
                    approval_url: Some("https://app.drums.sh/approvals/tok".into()),
                    expires_at: Some("2026-08-03T12:00:00Z".into()),
                },
            ),
            &ctx(),
        );
        assert!(out.contains("https://app.drums.sh/approvals/tok"), "{out}");
        assert!(out.contains("waiting on a human"), "{out}");
        assert!(out.contains("[unresolved]"), "nothing has run yet: {out}");
        assert!(out.contains("2026-08-03T12:00:00Z"), "{out}");
        assert!(
            !out.contains("repair dispatched"),
            "a held job must not read as a dispatched one: {out}"
        );
    }

    /// A failed dispatch is narrated and explicitly says the loop is still
    /// running — the hosted half being down is not the local half stopping.
    #[test]
    fn a_failed_dispatch_says_so_and_says_watching_continues() {
        let out = render(
            &EngineEvent::RepairDispatchFailed(
                sample_failure(),
                "could not reach https://app.drums.sh: connection refused".into(),
            ),
            &ctx(),
        );
        assert!(out.contains("not dispatched"), "{out}");
        assert!(out.contains("connection refused"), "{out}");
        assert!(out.contains("still watching"), "{out}");
        assert!(out.contains("[unresolved]"), "{out}");
    }

    #[test]
    fn repairing_line_names_the_agent() {
        let out = render(
            &EngineEvent::Repairing(sample_failure(), "claude".to_string()),
            &ctx(),
        );
        assert!(out.contains("repairing"));
        assert!(out.contains("claude"));
    }

    #[test]
    fn repair_failed_line_carries_unresolved_chip_and_leaves_worktree_and_branch_for_inspection() {
        let detail = crate::engine::RepairFailure {
            why: "the original failing request still returns 500".to_string(),
            worktree: Some("/tmp/drums-repro-abc".to_string()),
            branch: Some("drums/repair-01ARZ3ND".to_string()),
            elapsed_ms: 4_200,
        };
        let out = render(&EngineEvent::RepairFailed(sample_failure(), detail), &ctx());
        assert!(out.contains("repair failed"));
        assert!(out.contains("[unresolved]"));
        assert!(out.contains("still returns 500"));
        assert!(
            out.contains("/tmp/drums-repro-abc"),
            "the worktree path must be printed so a human can find it: {out}"
        );
        assert!(
            out.contains("drums/repair-01ARZ3ND"),
            "the branch name must be printed: {out}"
        );
    }

    #[test]
    fn repair_failed_line_with_no_worktree_omits_the_inspection_line() {
        // No worktree was ever created (e.g. no agent available) — the
        // render must not fabricate a path that doesn't exist.
        let detail = crate::engine::RepairFailure {
            why: "no repair agent available".to_string(),
            worktree: None,
            branch: None,
            elapsed_ms: 0,
        };
        let out = render(&EngineEvent::RepairFailed(sample_failure(), detail), &ctx());
        assert!(!out.contains("left for inspection"));
    }

    fn sample_repair() -> Repair {
        Repair {
            id: "r1".into(),
            failure_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
            sha: "deadbeef00".into(),
            branch: "drums/repair-01ARZ3ND".into(),
            agent: "claude".into(),
            summary: "fixed the promo guard".into(),
            diff_stat: "server.js | 1 +".into(),
            claims: vec![
                Claim {
                    text: "original failing request now returns 200".into(),
                    provenance: Provenance::Verified,
                },
                Claim {
                    text: "GET /health returns 200".into(),
                    provenance: Provenance::Verified,
                },
            ],
        }
    }

    #[test]
    fn repair_ready_line_lists_every_verified_claim_and_the_ship_command() {
        let out = render(
            &EngineEvent::RepairReady(sample_failure(), sample_repair(), 12_345),
            &ctx(),
        );
        assert!(out.contains("claude"));
        assert!(out.contains("now returns 200"));
        assert!(out.contains("[verified]"));
        assert!(out.contains("repair ready"));
        assert!(
            out.contains("drums ship 01ARZ3NDEKTSV4RRFFQ69G5FAV"),
            "must print the exact next command: {out}"
        );
    }

    /// F4 (Task-3 review round 3): the printed line must actually be
    /// runnable, not just look like it. `Commands::Ship` declares
    /// `deploy_cmd: String` (a REQUIRED clap arg, not `Option`) — a bare
    /// `drums ship <id>` exits 2 with "the following required arguments were
    /// not provided: --deploy-cmd". When `drums watch` WAS configured with a
    /// `--deploy-cmd`, the printed line must carry that exact value (shell-
    /// quoted) so it can be run verbatim, and must always carry an explicit
    /// `--repo` too — closing F4's second failure mode, where `drums ship`
    /// resolves its record path from cwd while `drums watch --repo` wrote
    /// somewhere else entirely.
    #[test]
    fn repair_ready_line_prints_the_configured_deploy_cmd_and_repo_so_the_command_is_actually_runnable(
    ) {
        let render_ctx = RenderContext {
            repo: PathBuf::from("/srv/shop"),
            deploy_cmd: Some("bash deploy.sh {sha}".to_string()),
        };
        let out = render(
            &EngineEvent::RepairReady(sample_failure(), sample_repair(), 12_345),
            &render_ctx,
        );
        assert!(
            out.contains("drums ship 01ARZ3NDEKTSV4RRFFQ69G5FAV --deploy-cmd 'bash deploy.sh {sha}' --repo '/srv/shop'"),
            "the printed line must be the exact runnable `drums ship` invocation, quoting the configured --deploy-cmd and always naming --repo: {out}"
        );
    }

    /// F4's other honest arm: when `drums watch` was run WITHOUT
    /// `--deploy-cmd` (propose mode doesn't require one), the line must not
    /// fabricate a command that looks complete — it must show an explicit
    /// placeholder the operator has to fill in, while still naming the real
    /// `--repo`.
    #[test]
    fn repair_ready_line_prints_an_explicit_placeholder_when_no_deploy_cmd_was_configured() {
        let render_ctx = RenderContext {
            repo: PathBuf::from("/srv/shop"),
            deploy_cmd: None,
        };
        let out = render(
            &EngineEvent::RepairReady(sample_failure(), sample_repair(), 12_345),
            &render_ctx,
        );
        assert!(
            out.contains("drums ship 01ARZ3NDEKTSV4RRFFQ69G5FAV --deploy-cmd '<your deploy command>' --repo '/srv/shop'"),
            "must print an explicit placeholder, not a bare command that silently omits a required arg: {out}"
        );
    }

    /// A deploy command containing a single quote (e.g. an author's name in
    /// a git-notes-style message, or a shell-safe idiom the operator already
    /// wrote) must not break the surrounding quoting when copy-pasted.
    #[test]
    fn repair_ready_line_shell_quotes_a_deploy_cmd_containing_a_single_quote() {
        let render_ctx = RenderContext {
            repo: PathBuf::from("/srv/shop"),
            deploy_cmd: Some("echo it's deployed".to_string()),
        };
        let out = render(
            &EngineEvent::RepairReady(sample_failure(), sample_repair(), 12_345),
            &render_ctx,
        );
        assert!(
            out.contains(r"--deploy-cmd 'echo it'\''s deployed'"),
            "embedded single quotes must be POSIX-escaped, not left to break the quoting: {out}"
        );
    }

    #[test]
    fn shipped_line_carries_claims_and_reversibility() {
        let outcome = ShipOutcome {
            failure_id: "f1".into(),
            repair_sha: "deadbeef00".into(),
            action: "shipped".into(),
            deploy_cmd: "bash deploy.sh".into(),
            claims: vec![Claim {
                text: "http://x/health returns 200".into(),
                provenance: Provenance::Verified,
            }],
        };
        let out = render(&EngineEvent::Shipped(sample_failure(), outcome), &ctx());
        assert!(out.contains("shipped"));
        assert!(out.contains("[verified]"));
        assert!(
            out.contains("drums revert f1"),
            "reversibility must be printed at the moment of shipping: {out}"
        );
    }

    /// F6 (Task-3 review round 4): the ROLLBACK command printed at the moment
    /// an autonomous deploy has just gone out has to be runnable too —
    /// `Commands::Revert` declares `deploy_cmd: String` (a REQUIRED clap arg)
    /// and resolves its record path from `--repo`/cwd, exactly like
    /// `Commands::Ship`, so a bare `drums revert <id>` exits 2. This is the
    /// worst possible line to be unrunnable: the operator is reading it AFTER
    /// a bad repair reached production, under time pressure.
    #[test]
    fn shipped_line_prints_a_runnable_revert_command_with_the_configured_deploy_cmd_and_repo() {
        let outcome = ShipOutcome {
            failure_id: "f1".into(),
            repair_sha: "deadbeef00".into(),
            action: "shipped".into(),
            deploy_cmd: "bash deploy.sh deadbeef00".into(),
            claims: vec![Claim {
                text: "http://x/health returns 200".into(),
                provenance: Provenance::Verified,
            }],
        };
        let render_ctx = RenderContext {
            repo: PathBuf::from("/srv/shop"),
            deploy_cmd: Some("bash deploy.sh {sha}".to_string()),
        };
        let out = render(
            &EngineEvent::Shipped(sample_failure(), outcome),
            &render_ctx,
        );
        assert!(
            out.contains("drums revert f1 --deploy-cmd 'bash deploy.sh {sha}' --repo '/srv/shop'"),
            "the reversibility promise must print a command that actually runs: {out}"
        );
    }

    /// The honest arm: with no `--deploy-cmd` configured the line must show an
    /// explicit placeholder the operator has to fill in, never a command that
    /// looks complete and silently omits a required argument.
    #[test]
    fn shipped_line_prints_an_explicit_revert_placeholder_when_no_deploy_cmd_was_configured() {
        let outcome = ShipOutcome {
            failure_id: "f1".into(),
            repair_sha: "deadbeef00".into(),
            action: "shipped".into(),
            deploy_cmd: "bash deploy.sh deadbeef00".into(),
            claims: vec![],
        };
        let out = render(&EngineEvent::Shipped(sample_failure(), outcome), &ctx());
        assert!(
            out.contains("drums revert f1 --deploy-cmd '<your deploy command>' --repo '/srv/shop'"),
            "must print an explicit placeholder plus the real --repo, not a bare command: {out}"
        );
    }

    #[test]
    fn ship_failed_line_carries_unresolved_chip() {
        let out = render(
            &EngineEvent::ShipFailed(sample_failure(), "deploy command exited with 1".to_string()),
            &ctx(),
        );
        assert!(out.contains("ship failed"));
        assert!(out.contains("[unresolved]"));
    }

    // -- intake taxonomy ---------------------------------------------------------

    /// A trigger-intake failure has no method+path to print. The line must name
    /// the signature and say plainly that no request was captured — never imply
    /// Drums is holding a request it does not have, since that request is the
    /// only thing that could earn `verified`.
    #[test]
    fn failure_line_for_a_trigger_intake_says_no_replayable_request_was_captured() {
        let mut f = sample_failure();
        f.intake = Intake::Trigger {
            source: "hyperdx".into(),
        };
        f.sample.request = None;
        let out = render(&EngineEvent::FailureDetected(f), &ctx());
        assert!(out.contains("no replayable request captured"), "{out}");
        assert!(
            out.contains("TypeError"),
            "the signature must stand in for the missing method+path: {out}"
        );
        assert!(out.contains("server.js"), "{out}");
        assert!(
            !out.contains("POST"),
            "no method may be printed when none was captured: {out}"
        );
        assert!(
            out.contains("[observed]"),
            "the detection claim is still observed: {out}"
        );
    }

    /// The snippet path is untouched: byte-for-byte the line it always printed.
    #[test]
    fn failure_line_for_a_snippet_intake_is_unchanged() {
        let out = render(&EngineEvent::FailureDetected(sample_failure()), &ctx());
        assert!(out.contains("POST /api/checkout"), "{out}");
        assert!(!out.contains("no replayable request"), "{out}");
    }

    /// Worded as "skipped", never "failed". `ReproFailed` means a replay was
    /// attempted and did not reproduce — retryable in principle. This means there
    /// was never a request to replay, which no retry can change, and the reader
    /// must not confuse the two.
    #[test]
    fn repro_skipped_line_reads_as_not_attempted_not_as_a_failure() {
        let mut f = sample_failure();
        f.intake = Intake::Trigger {
            source: "hyperdx".into(),
        };
        f.sample.request = None;
        let claim = f.intake.no_replay_claim();
        let out = render(
            &EngineEvent::ReproSkippedNotReplayable(f, sample_attribution(), claim),
            &ctx(),
        );
        assert!(out.contains("skipped"), "{out}");
        assert!(out.contains("reproduction not attempted"), "{out}");
        assert!(out.contains("[unresolved]"), "{out}");
        assert!(
            !out.contains("reproduction failed"),
            "must not read as a failed reproduction: {out}"
        );
        assert!(!out.contains("[verified]"), "{out}");
    }

    /// A withheld ship must be visible and must say why — the operator asked for
    /// an unattended ship and did not get one (spec §13, design the miss).
    #[test]
    fn ship_withheld_line_names_the_reason_and_carries_an_unresolved_chip() {
        let why = engine_core::authority::ProposeReason::IntakeNotReplayable {
            source: "hyperdx".into(),
        }
        .withheld_text();
        let out = render(&EngineEvent::ShipWithheld(sample_failure(), why), &ctx());
        assert!(out.contains("ship withheld"), "{out}");
        assert!(out.contains("hyperdx"), "{out}");
        assert!(out.contains("no replayable request"), "{out}");
        assert!(out.contains("[unresolved]"), "{out}");
        assert!(
            !out.contains("ship failed"),
            "nothing failed — the gate refused: {out}"
        );
    }

    /// Closing round (F1's audit): every `sha` this module shortens for
    /// display comes straight off the wire — `POST /v1/deploys` deserializes
    /// `DeployRecord` and validates NOTHING (`crates/ingest/src/lib.rs`'s
    /// `post_deploy`), so a deploy hook posting a tag/branch name with any
    /// non-ASCII byte reaches all three of these arms. Byte-slicing it
    /// (`&sha[..sha.len().min(6)]`) panicked on a char boundary — and unlike
    /// `ship.rs`'s F1 site this panic lands inside `drums watch`'s own render
    /// loop (`main.rs` prints every event), so it takes the whole monitoring
    /// process down on ingest of a single record it accepted itself.
    #[test]
    fn every_sha_shortening_arm_survives_a_multi_byte_sha_from_the_wire() {
        // 'é' spans bytes 5..7, so byte index 6 is mid-character.
        let wire_sha = "abcdeéf012345";
        let deploy = DeployRecord {
            sha: wire_sha.into(),
            description: "tag deploy".into(),
            author: "ci".into(),
            deployed_at_ms: 1,
        };

        let out = render(&EngineEvent::DeployRecorded(deploy.clone()), &ctx());
        assert!(
            out.contains("abcde"),
            "the deploy line must still name the sha it was given: {out:?}"
        );

        let attribution = Attribution {
            deploy: deploy.clone(),
            overlap_files: vec!["server.js".into()],
            minutes_after_deploy: 6,
            claim: Claim {
                text: "first error 6 min after deploy".into(),
                provenance: Provenance::Inferred,
            },
        };
        let out = render(
            &EngineEvent::Attributed(sample_failure(), attribution.clone()),
            &ctx(),
        );
        assert!(out.contains("attributing"), "{out:?}");
        let out = render(
            &EngineEvent::Reproducing(sample_failure(), attribution),
            &ctx(),
        );
        assert!(out.contains("reproducing"), "{out:?}");
    }

    #[test]
    fn chip_ansi_matches_the_tui_legend() {
        // Mirrors `ui/model.rs`'s `every_provenance_variant_maps_to_the_spec_legend_color`
        // — same five colors, restated as ANSI escapes for the plain-text views.
        assert_eq!(chip_ansi(Provenance::Verified), "\x1b[32m");
        assert_eq!(chip_ansi(Provenance::Observed), "\x1b[34m");
        assert_eq!(chip_ansi(Provenance::Unresolved), "\x1b[31m");
        assert_ne!(
            chip_ansi(Provenance::Inferred),
            chip_ansi(Provenance::Approved)
        );
        assert_ne!(
            chip_ansi(Provenance::Inferred),
            chip_ansi(Provenance::Verified)
        );
    }

    #[test]
    fn reported_line_carries_observed_chip_source_and_title_and_is_one_line() {
        let issue = ReportedIssue {
            id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
            source: "agentation".into(),
            external_id: Some("agentation-42".into()),
            external_identifier: None,
            title: "button misaligned on checkout".into(),
            body_excerpt: "overlaps the price on mobile".into(),
            url: Some("https://agentation.example/i/42".into()),
            payload: serde_json::json!({"element": "#submit-btn"}),
            claim: Claim {
                text: "reported via agentation webhook".into(),
                provenance: Provenance::Observed,
            },
        };
        let out = render(&EngineEvent::Reported(issue), &ctx());
        assert!(out.contains("agentation"));
        assert!(out.contains("button misaligned on checkout"));
        assert!(out.contains("[observed]"));
        assert!(out.contains("reported via agentation webhook"));
        assert_eq!(out.matches('\n').count(), 1, "the Reported line must be quiet — exactly one line, not the multi-line ├─/└─ narration: {out:?}");
    }

    #[test]
    fn reported_line_for_linear_source_names_linear() {
        let issue = ReportedIssue {
            id: "id2".into(),
            source: "linear".into(),
            external_id: Some("11111111-2222-3333-4444-555555555555".into()),
            external_identifier: Some("ENG-123".into()),
            title: "crash on submit".into(),
            body_excerpt: String::new(),
            url: None,
            payload: serde_json::json!({}),
            claim: Claim {
                text: "reported via linear webhook".into(),
                provenance: Provenance::Observed,
            },
        };
        let out = render(&EngineEvent::Reported(issue), &ctx());
        assert!(out.contains("linear"));
        assert!(out.contains("reported via linear webhook"));
    }

    #[test]
    fn deploy_recorded_line_ends_with_newline() {
        // Task 8's main.rs prints every rendered event with `print!`, not
        // `println!` — a missing trailing `\n` here glues this line onto the
        // next event's leading glyph on the same terminal row.
        let d = DeployRecord {
            sha: "abc1234def".into(),
            description: "c1".into(),
            author: "t".into(),
            deployed_at_ms: 1_000,
        };
        let out = render(&EngineEvent::DeployRecorded(d), &ctx());
        assert!(
            out.ends_with('\n'),
            "DeployRecorded output must end with a newline, got {out:?}"
        );
    }

    // -- HIGH fix-round: terminal-escape injection forges provenance chips --

    #[test]
    fn strip_style_leaves_no_fragments_of_any_sequence() {
        // The regression: '[' is inside the final-byte range @-~, so a
        // stripper that scans for the final byte without first consuming
        // the '[' stops there and leaks "0m" into piped output.
        assert_eq!(strip_style("a\x1b[0mb"), "ab");
        assert_eq!(
            strip_style("\x1b[2mdim\x1b[0m \x1b[31mred\x1b[0m"),
            "dim red"
        );
        assert_eq!(strip_style("no escapes"), "no escapes");
        assert_eq!(strip_style("'/srv/my shop'\x1b[0m"), "'/srv/my shop'");
    }

    /// Audit B3: the narration must render the redacted path — the record
    /// redacts query-string secrets at capture time, and narrating the raw
    /// path into a pipe or CI log would undo exactly that protection.
    #[test]
    fn narration_redacts_query_secrets_like_the_record_does() {
        let redacted = engine_record::redact_query_string("/api/x?token=supersecret&item=1", &[]);
        assert!(!redacted.contains("supersecret"));
        assert!(redacted.contains("[redacted]"));
    }

    #[test]
    fn sanitize_strips_c0_controls_including_esc_and_carriage_return() {
        // The ESC byte and the CR byte are removed; the plain-text bracket
        // notation that followed each ESC in the source is NOT itself an
        // escape sequence once the ESC is gone, so it survives as inert
        // text — sanitize only removes control bytes, it doesn't try to
        // parse or strip whatever they introduced.
        let out = sanitize("TypeError\x1b[32m [verified]\x1b[0m\x1b[2m\r");
        assert!(!out.contains('\x1b'), "{out:?}");
        assert!(!out.contains('\r'), "{out:?}");
        assert_eq!(out, "TypeError[32m [verified][0m[2m");
    }

    /// Reproduces the review finding verbatim: a crafted `service` name
    /// (attacker-controlled, arrives over `POST /v1/events` with no
    /// validation) must not be able to forge a green `[verified]` chip that
    /// was never earned, nor use `\r` to overwrite the rest of the line.
    /// `render`'s own legitimate styling (`RED`/`BOLD`/`RESET`) also emits
    /// real ESC bytes, so the assertion targets the forged sequence
    /// specifically — the raw, un-neutralized bytes attacker-supplied
    /// `service` would have contributed — rather than the output as a
    /// whole, which is expected to still carry the renderer's own color.
    #[test]
    fn failure_line_does_not_let_a_forged_service_name_inject_ansi_escapes() {
        let mut f = sample_failure();
        let forged = "shop\x1b[32m [verified]\x1b[0m\rPWNED";
        f.service = forged.to_string();
        let out = render(&EngineEvent::FailureDetected(f), &ctx());
        assert!(
            !out.contains(forged),
            "the forged bytes must never survive verbatim: {out:?}"
        );
        assert!(
            !out.contains("shop\x1b[32m"),
            "the forged ESC immediately after \"shop\" must be stripped: {out:?}"
        );
        assert!(
            !out.contains('\r'),
            "carriage return must be stripped, not left to overwrite the line: {out:?}"
        );
        assert!(
            out.contains("shop[32m [verified][0mPWNED"),
            "the printable bytes survive, minus the control codes: {out:?}"
        );
    }

    // -- R1 fix-round: multi-line diagnostic blobs must not be flattened to
    // one line by the single-line `sanitize`, but must still be stripped of
    // ESC/CR and other C0/C1 controls, and every continuation line indented
    // so an injected `\n` can never start a line at column 0. --

    #[test]
    fn sanitize_multiline_keeps_newlines_and_tabs_but_strips_esc_and_cr() {
        let out =
            sanitize_multiline("line one\n\tline two\x1b[32m[verified]\x1b[0m\rPWNED\nline three");
        assert!(!out.contains('\x1b'), "{out:?}");
        assert!(!out.contains('\r'), "{out:?}");
        assert!(
            out.contains('\n'),
            "newlines must survive so a multi-line diagnostic stays readable: {out:?}"
        );
        assert!(out.contains('\t'), "tabs must survive: {out:?}");
    }

    #[test]
    fn sanitize_multiline_indents_every_continuation_line_so_none_starts_at_column_zero() {
        let out = sanitize_multiline("first line\nsecond line\nthird line");
        for (i, line) in out.split('\n').enumerate() {
            if i == 0 {
                continue;
            }
            assert!(
                line.starts_with(' '),
                "continuation line {i} must not start at column 0 (would look like a top-level narration row): {out:?}"
            );
        }
    }

    /// Reproduces the review finding verbatim: a jest-style multi-line
    /// failure report reaching `RepairFailed.why` (`engine.rs`'s
    /// `` test script `{t}` failed: {jest stderr} ``) must survive as
    /// multiple lines, not collapse into one unbroken run — the operator's
    /// only answer to "why was my repair refused".
    #[test]
    fn repair_failed_line_preserves_a_multiline_jest_report_instead_of_collapsing_it() {
        let jest_report = "FAIL test/cart.test.js\n  ✕ computeTotal handles empty cart (3 ms)\n\n  ● computeTotal handles empty cart\n\n    expected 0 to be undefined\n\n  1 failed, 4 passed";
        let detail = crate::engine::RepairFailure {
            why: format!("test script `npm test` failed: {jest_report}"),
            worktree: None,
            branch: None,
            elapsed_ms: 1_200,
        };
        let out = render(&EngineEvent::RepairFailed(sample_failure(), detail), &ctx());
        assert_eq!(
            out.matches('\n').count(),
            jest_report.matches('\n').count() + 1,
            "the jest report's line breaks must survive, not collapse to one line: {out:?}"
        );
        assert!(out.contains("computeTotal handles empty cart"), "{out:?}");
        assert!(out.contains("1 failed, 4 passed"), "{out:?}");
    }

    /// Same discipline for `ReproFailed`'s `why`, sourced from
    /// `ReproError::BootTimeout`'s folded line-by-line boot stderr
    /// (`crates/repro/src/lib.rs`).
    #[test]
    fn repro_failed_line_preserves_multiline_boot_stderr() {
        let boot_stderr = "app failed to boot within 5000ms: Error: cannot find module 'left-pad'\n    at Function.Module._resolveFilename (node:internal/modules/cjs/loader:1032:15)\n    at Function.Module._load (node:internal/modules/cjs/loader:901:27)";
        let out = render(
            &EngineEvent::ReproFailed(
                sample_failure(),
                sample_attribution(),
                boot_stderr.to_string(),
            ),
            &ctx(),
        );
        assert_eq!(
            out.matches('\n').count(),
            boot_stderr.matches('\n').count() + 1,
            "boot stderr's line breaks must survive: {out:?}"
        );
        assert!(out.contains("cannot find module 'left-pad'"), "{out:?}");
        assert!(out.contains("Module._load"), "{out:?}");
    }

    /// Same discipline for `AttributionErrored`'s `why`, sourced from
    /// `ReproError::Worktree`'s raw (possibly multi-line) git stderr.
    #[test]
    fn attribution_errored_line_preserves_multiline_git_stderr() {
        let git_stderr = "fatal: unable to create worktree\nerror: could not lock config file .git/config: File exists\nfatal: exiting because of an unresolved conflict";
        let out = render(
            &EngineEvent::AttributionErrored(sample_failure(), git_stderr.to_string()),
            &ctx(),
        );
        assert_eq!(
            out.matches('\n').count(),
            // AttributionErrored's template carries TWO fixed newlines of its
            // own (one after "attributing", one at the very end) — unlike
            // RepairFailed/ReproFailed's single trailing newline — so the
            // git stderr's own internal breaks are additive to 2, not 1.
            git_stderr.matches('\n').count() + 2,
            "git stderr's line breaks must survive: {out:?}"
        );
        assert!(out.contains("could not lock config file"), "{out:?}");
    }

    /// A malicious app-under-repair could echo request data straight to its
    /// own stderr, so `RepairFailed.why`, `ReproFailed.why`, and
    /// `AttributionErrored.why` are process-local but not fully trusted —
    /// the same forgery `sanitize` defends against for wire-sourced fields
    /// is reachable here too, one hop removed. Pin all three directions.
    #[test]
    fn multiline_why_arms_still_strip_forged_ansi_and_carriage_return() {
        let forged = "boot ok\n\x1b[32m[verified]\x1b[0m fake line\rPWNED";
        // The forged ESC immediately before "[verified]" must be gone, but
        // `render`'s own DIM/RED/RESET styling legitimately emits real ESC
        // bytes elsewhere in every one of these lines — so, exactly like
        // `failure_line_does_not_let_a_forged_service_name_inject_ansi_escapes`,
        // the assertion targets the forged sequence specifically rather than
        // asserting the output carries no ESC byte at all. CR is never
        // legitimately emitted by this renderer, so asserting its total
        // absence is valid on its own.
        let forged_escape = "\x1b[32m[verified]";

        let repair_detail = crate::engine::RepairFailure {
            why: forged.to_string(),
            worktree: None,
            branch: None,
            elapsed_ms: 0,
        };
        let out = render(
            &EngineEvent::RepairFailed(sample_failure(), repair_detail),
            &ctx(),
        );
        assert!(
            !out.contains(forged_escape),
            "RepairFailed must strip the forged ESC: {out:?}"
        );
        assert!(!out.contains('\r'), "RepairFailed must strip CR: {out:?}");
        assert!(
            out.contains("[32m[verified][0m fake linePWNED"),
            "the printable bytes survive, minus the control codes: {out:?}"
        );

        let out = render(
            &EngineEvent::ReproFailed(sample_failure(), sample_attribution(), forged.to_string()),
            &ctx(),
        );
        assert!(
            !out.contains(forged_escape),
            "ReproFailed must strip the forged ESC: {out:?}"
        );
        assert!(!out.contains('\r'), "ReproFailed must strip CR: {out:?}");

        let out = render(
            &EngineEvent::AttributionErrored(sample_failure(), forged.to_string()),
            &ctx(),
        );
        assert!(
            !out.contains(forged_escape),
            "AttributionErrored must strip the forged ESC: {out:?}"
        );
        assert!(
            !out.contains('\r'),
            "AttributionErrored must strip CR: {out:?}"
        );
    }

    /// R1's own boundary, enforced on the arm that sits closest to it:
    /// `ShipWithheld`'s payload is NOT a diagnostic blob. It is
    /// `ProposeReason::withheld_text()`, which interpolates
    /// `Intake::source()` — a `String` deserialized straight off
    /// `POST /v1/events` with no validation (`crates/ingest/src/lib.rs`'s
    /// `post_event`; `Intake` is internally tagged, so a posted
    /// `{"kind":"trigger","source":"…"}` sets it verbatim). The line is
    /// displayed as ONE line, so it takes plain [`sanitize`]: permitting `\n`
    /// here lets a posted `source` open a second line, and
    /// `sanitize_multiline`'s 3-space continuation indent is exactly the
    /// indent every claim row in this module is printed with
    /// (`"   {GREEN}→{RESET} {} {}"`), so the forged line would be
    /// shape-identical to an earned claim in the no-color view — the
    /// row-forgery vector `sanitize_multiline`'s own doc forbids, reached
    /// through the authority gate's narration.
    #[test]
    fn ship_withheld_line_cannot_be_split_into_a_forged_claim_row_by_a_posted_intake_source() {
        let forged_source = "hyperdx\n→ all tests passed [verified]";
        let why = engine_core::authority::ProposeReason::IntakeNotReplayable {
            source: forged_source.into(),
        }
        .withheld_text();
        let out = render(&EngineEvent::ShipWithheld(sample_failure(), why), &ctx());
        assert_eq!(
            out.matches('\n').count(),
            1,
            "the withheld line is one line — a posted intake source must not be able to open a second: {out:?}"
        );
        assert!(
            !out.contains("\n   →"),
            "a posted intake source must not be able to forge a claim row: {out:?}"
        );
    }

    /// The same R1 class as the jest report, one arm further along the loop:
    /// `ShipFailed`'s payload is `ShipError`'s `Display`, and
    /// `DeployCommandFailed`/`DeployCommandTimeout` carry
    /// `ship::failure_detail`'s output — the deploy command's raw trimmed
    /// stderr, multi-line by nature. Flattening it leaves the operator one
    /// unbroken run as their only account of why a production deploy was
    /// refused.
    #[test]
    fn ship_failed_line_preserves_multiline_deploy_stderr() {
        let deploy_stderr = "+ bash deploy.sh a1b2c3\nnpm ERR! code ELIFECYCLE\nnpm ERR! errno 1\nnpm ERR! shop@1.0.0 build: `webpack -p`";
        let why = format!("deploy command exited with exit status: 1: {deploy_stderr}");
        let out = render(&EngineEvent::ShipFailed(sample_failure(), why), &ctx());
        assert_eq!(
            out.matches('\n').count(),
            deploy_stderr.matches('\n').count() + 1,
            "the deploy command's stderr must keep its line breaks: {out:?}"
        );
        assert!(out.contains("npm ERR! errno 1"), "{out:?}");
    }

    fn repro_outcome(reproduced: bool) -> Reproduction {
        Reproduction {
            sha: "abc1234567890abcdef1234567890abcdef12345".into(),
            reproduced,
            parent_clean: None,
            detail: "replay at abc1234 returned 500; signature did not match".into(),
            claims: vec![Claim {
                text: if reproduced {
                    "replayed the captured request at abc1234: same TypeError".into()
                } else {
                    "could not reproduce at abc1234 (status 500)".into()
                },
                provenance: if reproduced {
                    Provenance::Verified
                } else {
                    Provenance::Unresolved
                },
            }],
        }
    }

    fn repro_attribution() -> Attribution {
        Attribution {
            deploy: DeployRecord {
                sha: "abc1234567890abcdef1234567890abcdef12345".into(),
                description: "add promo codes".into(),
                author: "sahil".into(),
                deployed_at_ms: 1,
            },
            overlap_files: vec![],
            minutes_after_deploy: 4,
            claim: Claim {
                text: "correlated".into(),
                provenance: Provenance::Inferred,
            },
        }
    }

    /// The regression, found running the real loop against a real repo: a
    /// failed reproduction printed "could not reproduce ... [unresolved]" and
    /// then "reproduction confirmed" on the very next line — so the run ENDED
    /// on the reassuring, false one.
    #[test]
    fn a_failed_reproduction_never_says_confirmed() {
        let out = render(
            &EngineEvent::Reproduced(sample_failure(), repro_attribution(), repro_outcome(false)),
            &ctx(),
        );
        assert!(
            !out.contains("reproduction confirmed"),
            "a failed reproduction must never claim confirmation:\n{out}"
        );
        assert!(out.contains("not reproduced"), "{out}");
        assert!(
            out.contains("no repair attempted"),
            "the operator must be told WHY nothing follows:\n{out}"
        );
    }

    #[test]
    fn a_successful_reproduction_still_says_confirmed() {
        let out = render(
            &EngineEvent::Reproduced(sample_failure(), repro_attribution(), repro_outcome(true)),
            &ctx(),
        );
        assert!(out.contains("reproduction confirmed"), "{out}");
        assert!(!out.contains("not reproduced"), "{out}");
    }

    /// The slow loop's narration states BOTH readings — what the metric reads
    /// at the horizon, and what the declared window read at close — and never
    /// re-labels the verdict: direction words only, no supported/not
    /// supported/inconclusive, whatever drifted.
    #[test]
    fn a_revisit_line_states_both_readings_and_never_relabels_the_verdict() {
        use engine_core::evaluation::{Direction, Guardrails, Metric, Outcome};
        let out = render(
            &EngineEvent::RevisitMeasured {
                change: "chg_x".into(),
                horizon_days: 30,
                drifted: true,
                outcome: Outcome::Measured {
                    direction: Direction::Neutral,
                    from: 0.11,
                    to: 0.12,
                    entries: 168,
                    guardrails: Guardrails::Held,
                },
                metric: Metric::ErrorEventRate,
                was: Some(Direction::Positive),
            },
            &ctx(),
        );
        assert!(out.contains("revisit chg_x at 30d"), "{out}");
        assert!(
            out.contains("neutral 0.11/h → 0.12/h over 168h"),
            "the horizon's own reading: {out}"
        );
        assert!(
            out.contains("(was: improved at close)"),
            "the close reading stands beside it: {out}"
        );
        for verdict_word in ["supported", "inconclusive"] {
            assert!(
                !out.to_lowercase().contains(verdict_word),
                "a revisit describes the metric, it issues no verdict: {out}"
            );
        }

        // A behavior metric keeps its own units, and an unmeasured original
        // is said as unmeasured — never given a direction it did not earn.
        let out = render(
            &EngineEvent::RevisitMeasured {
                change: "chg_y".into(),
                horizon_days: 90,
                drifted: false,
                outcome: Outcome::Measured {
                    direction: Direction::Positive,
                    from: 0.61,
                    to: 0.74,
                    entries: 380,
                    guardrails: Guardrails::Held,
                },
                metric: Metric::CompletionRate,
                was: None,
            },
            &ctx(),
        );
        assert!(
            out.contains("improved 0.61 → 0.74 over 380 entries"),
            "{out}"
        );
        assert!(out.contains("(was: unmeasured at close)"), "{out}");

        // An unmeasured revisit narrates the honest sentence, not a number.
        let u = engine_core::evaluation::Unmeasured::NotEnoughTraffic {
            entries: 12,
            needed: 100,
        };
        let out = render(
            &EngineEvent::RevisitMeasured {
                change: "chg_z".into(),
                horizon_days: 30,
                drifted: false,
                outcome: Outcome::Unmeasured(u.clone()),
                metric: Metric::CompletionRate,
                was: Some(Direction::Positive),
            },
            &ctx(),
        );
        assert!(out.contains(&u.sentence()), "{out}");
    }

    /// A proactive draft is an interpretation, so it carries the same
    /// `inferred` chip a bet does — and the line must end at the one command
    /// that turns the draft into a commitment, because that step is the
    /// human's, never Drums'.
    #[test]
    fn a_drafted_bet_line_carries_the_inferred_chip_and_the_confirm_command() {
        let out = render(
            &EngineEvent::BetDrafted {
                bet: "bet_01hx".into(),
                belief: "surfacing the retry button will cut abandonment".into(),
                by: "claude".into(),
            },
            &ctx(),
        );
        assert!(out.contains("[inferred]"), "{out}");
        assert!(out.contains("Drums drafted bet bet_01hx"), "{out}");
        assert!(
            out.contains("\"surfacing the retry button will cut abandonment\""),
            "{out}"
        );
        assert!(
            out.contains("drums bet confirm bet_01hx"),
            "the confirm command is the point of the line: {out}"
        );
        assert!(out.contains("claude"), "who drafted it is named: {out}");
        assert!(
            !out.to_lowercase().contains("confirmed"),
            "a draft must never read as already confirmed: {out}"
        );
    }
}
