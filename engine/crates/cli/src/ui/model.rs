//! Pure view-model for `drums watch`'s TUI: `EngineEvent` -> in-place stage
//! updates. No terminal/rendering code lives here — this module never
//! touches a TTY, so it's fully unit-testable without one, and the `ui::view`
//! module (which does the actual ratatui drawing) can stay a thin, untested
//! consumer of it.
//!
//! One [`FailureCard`] per failure, updated IN PLACE as its events arrive —
//! never appended to as a growing log. Each card owns an ordered
//! [`StageRow`] list (`detected`, `attributed`, `reproduced`, `repairing`,
//! `shipped`), each carrying its own provenance chip and a timer that is
//! live while the stage is running and frozen the instant it finishes.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Instant;

use crate::engine::EngineEvent;
use crate::render::{short_sha, RenderContext};
use engine_core::{Claim, Provenance};

/// How many most-recent quiet/background lines (deploys, mainly) the dim
/// ticker keeps. Old ones simply fall off — the ticker is a glance, not a log.
pub const TICKER_CAPACITY: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageStatus {
    Running,
    Done,
    Failed,
}

#[derive(Debug, Clone)]
pub struct StageRow {
    pub label: &'static str,
    pub status: StageStatus,
    pub chip: Option<Provenance>,
    pub detail: String,
    started: Instant,
    /// `None` while running (elapsed is computed live from `started`);
    /// `Some(ms)` once finished, frozen at the value observed (or reported
    /// by the engine, for stages whose event already carries an authoritative
    /// elapsed_ms) at finish time — never recomputed afterward, so it can
    /// never silently drift or go backwards on a later render.
    finished_ms: Option<u64>,
}

impl StageRow {
    fn running(label: &'static str, now: Instant, detail: String) -> Self {
        StageRow {
            label,
            status: StageStatus::Running,
            chip: None,
            detail,
            started: now,
            finished_ms: None,
        }
    }

    fn instant(
        label: &'static str,
        now: Instant,
        status: StageStatus,
        chip: Option<Provenance>,
        detail: String,
        elapsed_ms: u64,
    ) -> Self {
        StageRow {
            label,
            status,
            chip,
            detail,
            started: now,
            finished_ms: Some(elapsed_ms),
        }
    }

    fn finish(
        &mut self,
        elapsed_ms: u64,
        status: StageStatus,
        chip: Option<Provenance>,
        detail: String,
    ) {
        self.status = status;
        self.chip = chip;
        self.detail = detail;
        self.finished_ms = Some(elapsed_ms);
    }

    /// Elapsed time for this row at `now`. Monotonic for a monotonic `now`:
    /// a running row's value only grows (derived straight from `Instant`
    /// subtraction), and a finished row's value is frozen and never
    /// recomputed, so it can never move at all once set — the fixed-width
    /// timer column this feeds must never jump or shrink between frames.
    pub fn elapsed_ms(&self, now: Instant) -> u64 {
        match self.finished_ms {
            Some(ms) => ms,
            None => now.saturating_duration_since(self.started).as_millis() as u64,
        }
    }

    pub fn is_running(&self) -> bool {
        self.status == StageStatus::Running
    }
}

#[derive(Debug, Clone)]
pub struct FailureCard {
    pub failure_id: String,
    pub service: String,
    pub method: String,
    pub path: String,
    pub claim_text: String,
    pub detected_at_label: String,
    pub stages: Vec<StageRow>,
    /// The one actionable next step, set once a repair is ready to ship or
    /// has shipped (spec §19: propose stops at `drums ship <id>`). `None`
    /// until then, and while the card ends in a failure with nothing further
    /// to run — the honest reason lives inline on the failed stage row.
    pub ready_line: Option<String>,
    last_transition: Instant,
}

impl FailureCard {
    fn stage_index(&self, label: &str) -> Option<usize> {
        self.stages.iter().position(|s| s.label == label)
    }
}

/// Finishes the named `Running` stage row on `card` (or, defensively, adds
/// it as an already-finished row if one was never started — the engine's own
/// event ordering guarantees a prior `Running` push, but a view-model must
/// never drop an outcome silently just because that invariant somehow didn't
/// hold). `elapsed_override` lets a caller hand in the engine's own
/// authoritative elapsed_ms (RepairReady/RepairFailed both carry one) rather
/// than recomputing a slightly-different number from this process's own
/// receive-time timer.
fn finish_running(
    card: &mut FailureCard,
    now: Instant,
    label: &'static str,
    status: StageStatus,
    chip: Option<Provenance>,
    detail: String,
    elapsed_override: Option<u64>,
) {
    match card.stage_index(label) {
        Some(idx) => {
            let elapsed = elapsed_override.unwrap_or_else(|| {
                now.saturating_duration_since(card.stages[idx].started)
                    .as_millis() as u64
            });
            card.stages[idx].finish(elapsed, status, chip, detail);
        }
        None => {
            card.stages.push(StageRow::instant(
                label,
                now,
                status,
                chip,
                detail,
                elapsed_override.unwrap_or(0),
            ));
        }
    }
    card.last_transition = now;
}

#[derive(Debug)]
pub struct ViewModel {
    /// Insertion order = detection order; oldest first.
    pub cards: Vec<FailureCard>,
    /// Dim one-line-at-a-time background narration: deploys and other quiet
    /// events that aren't worth a card of their own.
    pub ticker: VecDeque<String>,
    pub selected: usize,
    pub event_count: u64,
    pub show_help: bool,
    pub record_path: PathBuf,
}

impl ViewModel {
    pub fn new(record_path: PathBuf) -> Self {
        ViewModel {
            cards: Vec::new(),
            ticker: VecDeque::new(),
            selected: 0,
            event_count: 0,
            show_help: false,
            record_path,
        }
    }

    pub fn select_next(&mut self) {
        if !self.cards.is_empty() {
            self.selected = (self.selected + 1) % self.cards.len();
        }
    }

    pub fn select_prev(&mut self) {
        if !self.cards.is_empty() {
            self.selected = (self.selected + self.cards.len() - 1) % self.cards.len();
        }
    }

    pub fn toggle_help(&mut self) {
        self.show_help = !self.show_help;
    }

    fn card_mut(&mut self, failure_id: &str) -> Option<&mut FailureCard> {
        self.cards.iter_mut().find(|c| c.failure_id == failure_id)
    }

    fn push_ticker(&mut self, line: String) {
        self.ticker.push_back(line);
        while self.ticker.len() > TICKER_CAPACITY {
            self.ticker.pop_front();
        }
    }

    /// Applies one engine event to the model. `now` drives every stage
    /// timer (monotonic — the only thing that matters for a live elapsed
    /// column). `wall_clock_label` is a human time string the CALLER already
    /// formatted (kept out of this module so the model stays deterministic
    /// under test: no `SystemTime::now()` reads in here). `render_ctx` is
    /// the same context `render.rs`'s plain renderer uses, so the printed
    /// `drums ship` command is the identical, actually-runnable invocation
    /// (F4) in both the plain and TUI paths — one source of truth, not two
    /// that can drift.
    pub fn apply(
        &mut self,
        event: &EngineEvent,
        now: Instant,
        wall_clock_label: impl FnOnce() -> String,
        render_ctx: &RenderContext,
    ) {
        self.event_count += 1;
        match event {
            EngineEvent::DeployRecorded(d) => {
                let line = format!(
                    "deploy {} \"{}\" · {}",
                    short_sha(&d.sha),
                    d.description,
                    d.author
                );
                self.push_ticker(line);
            }
            EngineEvent::OutcomeMeasured(out) => {
                let line = format!("outcome {}", out.change.0);
                self.push_ticker(line);
            }
            EngineEvent::RevisitMeasured {
                change,
                horizon_days,
                drifted,
                ..
            } => {
                let line = if *drifted {
                    format!("revisit {change} at {horizon_days}d — drifted from the close reading")
                } else {
                    format!("revisit {change} at {horizon_days}d")
                };
                self.push_ticker(line);
            }
            EngineEvent::BetEvaluated { bet, verdict, .. } => {
                let line = format!(
                    "bet {} {}",
                    bet,
                    crate::bet_cmd::support_word(verdict.support)
                );
                self.push_ticker(line);
            }
            EngineEvent::BetDrafted { bet, .. } => {
                let line = format!("bet {bet} drafted — awaiting confirmation");
                self.push_ticker(line);
            }
            EngineEvent::ObservationRecorded(o) => {
                let line = format!("observed {}", o.id.0);
                self.push_ticker(line);
            }
            EngineEvent::FailureDetected(f) => {
                let card = FailureCard {
                    failure_id: f.id.clone(),
                    service: f.service.clone(),
                    // No replayable request on trigger intake — the card shows
                    // the signature rather than inventing a method/path.
                    // `replayable_request()`, not `sample.request`: a trigger
                    // intake carrying a request reconstructed from span
                    // attributes must not be displayed as the failing request.
                    method: f
                        .replayable_request()
                        .map(|r| r.method.clone())
                        .unwrap_or_else(|| f.signature.error_name.clone()),
                    path: f
                        .replayable_request()
                        .map(|r| r.path.clone())
                        .unwrap_or_else(|| {
                            format!("in {} · no replayable request", f.signature.top_frame_file)
                        }),
                    claim_text: f.claim.text.clone(),
                    detected_at_label: wall_clock_label(),
                    // Detection latency isn't modeled from any event pair
                    // today (there's no separate "ingest started" moment) —
                    // 0ms is an honest "instantaneous by construction", not a
                    // fabricated number.
                    stages: vec![StageRow::instant(
                        "detected",
                        now,
                        StageStatus::Done,
                        Some(f.claim.provenance),
                        f.claim.text.clone(),
                        0,
                    )],
                    ready_line: None,
                    last_transition: now,
                };
                self.cards.push(card);
                self.selected = self.cards.len() - 1;
            }
            EngineEvent::Attributed(f, a) => {
                if let Some(card) = self.card_mut(&f.id) {
                    let elapsed = now
                        .saturating_duration_since(card.last_transition)
                        .as_millis() as u64;
                    let detail = format!(
                        "deploy {} \"{}\"",
                        short_sha(&a.deploy.sha),
                        a.deploy.description
                    );
                    card.stages.push(StageRow::instant(
                        "attributed",
                        now,
                        StageStatus::Done,
                        Some(a.claim.provenance),
                        detail,
                        elapsed,
                    ));
                    card.last_transition = now;
                }
            }
            EngineEvent::AttributionMissing(f) => self.finish_instant(
                &f.id,
                now,
                "attributed",
                StageStatus::Failed,
                Some(Provenance::Unresolved),
                "no deploy precedes this failure".to_string(),
            ),
            EngineEvent::AttributionErrored(f, why) => self.finish_instant(
                &f.id,
                now,
                "attributed",
                StageStatus::Failed,
                Some(Provenance::Unresolved),
                format!("attribution failed: {why}"),
            ),
            EngineEvent::Reproducing(f, a) => {
                if let Some(card) = self.card_mut(&f.id) {
                    let detail = format!(
                        "building revision {}, replaying the captured request",
                        short_sha(&a.deploy.sha)
                    );
                    card.stages
                        .push(StageRow::running("reproduced", now, detail));
                }
            }
            EngineEvent::Reproduced(f, _a, r) => {
                if let Some(card) = self.card_mut(&f.id) {
                    let (status, chip) = if r.reproduced {
                        (StageStatus::Done, Some(Provenance::Verified))
                    } else {
                        (StageStatus::Failed, Some(Provenance::Unresolved))
                    };
                    finish_running(
                        card,
                        now,
                        "reproduced",
                        status,
                        chip,
                        r.detail.clone(),
                        None,
                    );
                }
            }
            EngineEvent::ReproFailed(f, _a, why) => {
                if let Some(card) = self.card_mut(&f.id) {
                    finish_running(
                        card,
                        now,
                        "reproduced",
                        StageStatus::Failed,
                        Some(Provenance::Unresolved),
                        why.clone(),
                        None,
                    );
                }
            }
            // Reproduction was never STARTED for this failure (no `Reproducing`
            // event preceded this one), so there is no running stage row to
            // finish — the row is pushed as an already-completed
            // `Skipped`-shaped stage instead. `StageStatus::Failed` with an
            // `unresolved` chip is the closest honest rendering the existing two
            // statuses allow: nothing broke, but a check the chain needs did not
            // happen and never can for this input, and the human must see it as a
            // gap rather than as a stage still in flight.
            EngineEvent::ReproSkippedNotReplayable(f, _a, claim) => {
                if let Some(card) = self.card_mut(&f.id) {
                    let elapsed = now
                        .saturating_duration_since(card.last_transition)
                        .as_millis() as u64;
                    card.stages.push(StageRow::instant(
                        "reproduction skipped",
                        now,
                        StageStatus::Failed,
                        Some(claim.provenance),
                        claim.text.clone(),
                        elapsed,
                    ));
                    card.last_transition = now;
                }
            }
            EngineEvent::Repairing(f, agent) => {
                if let Some(card) = self.card_mut(&f.id) {
                    card.stages.push(StageRow::running(
                        "repairing",
                        now,
                        format!("agent: {agent}"),
                    ));
                }
            }
            EngineEvent::RepairFailed(f, detail) => {
                if let Some(card) = self.card_mut(&f.id) {
                    let mut msg = detail.why.clone();
                    if let Some(wt) = &detail.worktree {
                        msg.push_str(&format!(" · left for inspection: {wt}"));
                        if let Some(branch) = &detail.branch {
                            msg.push_str(&format!(" (branch {branch})"));
                        }
                    }
                    finish_running(
                        card,
                        now,
                        "repairing",
                        StageStatus::Failed,
                        Some(Provenance::Unresolved),
                        msg,
                        Some(detail.elapsed_ms),
                    );
                }
            }
            EngineEvent::RepairReady(f, repair, elapsed_ms) => {
                if let Some(card) = self.card_mut(&f.id) {
                    let mut detail = format!("{} · {}", repair.agent, repair.diff_stat.trim());
                    // I1 (Task-3 review round 5): this view collapses the
                    // repair's whole claim list into ONE stage chip, so that
                    // chip has to be the WEAKEST claim's, not a blanket
                    // `verified`. A `RepairReady` can honestly carry an
                    // `unresolved` claim — e.g. the app's `package.json`
                    // declares no runnable `scripts.test`, so the strongest
                    // check in `verify_repair` did not execute and says so —
                    // and the plain renderer prints every claim with its own
                    // chip. Here the un-executed check would otherwise vanish
                    // behind a green [verified], which is the §13
                    // design-the-miss failure the claim exists to prevent, so
                    // it is named inline too.
                    let unverified: Vec<&Claim> = repair
                        .claims
                        .iter()
                        .filter(|c| c.provenance != Provenance::Verified)
                        .collect();
                    let chip = match unverified.first() {
                        None => Provenance::Verified,
                        Some(weakest) => {
                            detail.push_str(&format!(" · {}", weakest.text));
                            weakest.provenance
                        }
                    };
                    finish_running(
                        card,
                        now,
                        "repairing",
                        StageStatus::Done,
                        Some(chip),
                        detail,
                        Some(*elapsed_ms),
                    );
                    card.ready_line = Some(format!(
                        "repair ready — run: {}",
                        crate::render::ship_command_line(&repair.failure_id, render_ctx)
                    ));
                }
            }
            EngineEvent::Shipped(f, outcome) => {
                if let Some(card) = self.card_mut(&f.id) {
                    let elapsed = now
                        .saturating_duration_since(card.last_transition)
                        .as_millis() as u64;
                    let mut detail = outcome.deploy_cmd.clone();
                    // Round-2 fix (must-fix 1): this view collapses the
                    // ship's whole claim list into ONE stage chip, exactly
                    // like `RepairReady` above — so, same as there, that chip
                    // has to be the WEAKEST claim's, not a blanket
                    // `verified`. A `Shipped` outcome can honestly carry an
                    // `unresolved` claim: no `--check-url` configured, a
                    // non-200 health check, or a replay of the originally
                    // failing request that still fails — and the plain
                    // renderer prints every claim with its own chip. Here the
                    // failing check would otherwise vanish behind a green
                    // [verified], the same §13 design-the-miss failure I1
                    // already fixed for `RepairReady`.
                    let unverified: Vec<&Claim> = outcome
                        .claims
                        .iter()
                        .filter(|c| c.provenance != Provenance::Verified)
                        .collect();
                    let chip = match unverified.first() {
                        None => Provenance::Verified,
                        Some(weakest) => {
                            detail.push_str(&format!(" · {}", weakest.text));
                            weakest.provenance
                        }
                    };
                    card.stages.push(StageRow::instant(
                        "shipped",
                        now,
                        StageStatus::Done,
                        Some(chip),
                        detail,
                        elapsed,
                    ));
                    card.last_transition = now;
                    // Round-2 fix (must-fix 2): built by `ship::shipped_ready_line`,
                    // not hand-formatted here — that function shares
                    // `ship.rs`'s `shipped_status` with the ANSI
                    // `shipped_footer` used by the plain narrator, so the
                    // round-2 N3 `record_write_failed` guard (a ship whose
                    // `shipped` record line failed to write must not promise
                    // a reversibility that `drums revert` will refuse with
                    // `NothingToRevert`) can't be independently re-decided —
                    // or silently skipped — by a third hand-written copy of
                    // that conditional. F6 (Task-3 review round 4) still
                    // applies: the runnable command it names carries
                    // `--deploy-cmd`/`--repo`, since a bare `drums revert
                    // <id>` exits 2.
                    card.ready_line = Some(crate::ship::shipped_ready_line(outcome, render_ctx));
                }
            }
            EngineEvent::ShipFailed(f, why) => {
                self.finish_instant(
                    &f.id,
                    now,
                    "shipped",
                    StageStatus::Failed,
                    Some(Provenance::Unresolved),
                    why.clone(),
                );
            }
            // The authority gate refused an unattended ship. The card keeps the
            // `drums ship` command `RepairReady` already put in `ready_line` — the
            // repair really is ready and a human really can ship it — and this row
            // says why Drums would not do it alone. Same reasoning as the plain
            // renderer: not an error, but never invisible.
            EngineEvent::ShipWithheld(f, why) => {
                self.finish_instant(
                    &f.id,
                    now,
                    "ship withheld",
                    StageStatus::Failed,
                    Some(Provenance::Unresolved),
                    why.clone(),
                );
            }
            EngineEvent::Proposed(f, p) => {
                // The card's ready line already names `drums ship`; the
                // proposal URL is the other thing a human wants to click.
                self.finish_instant(
                    &f.id,
                    now,
                    "proposed",
                    StageStatus::Done,
                    Some(p.claim.provenance),
                    p.url.clone(),
                );
            }
            EngineEvent::ProposalFailed(f, why) => {
                self.finish_instant(
                    &f.id,
                    now,
                    "proposal failed",
                    StageStatus::Failed,
                    Some(Provenance::Unresolved),
                    why.clone(),
                );
            }
            EngineEvent::Demoted(class, why) => {
                self.push_ticker(format!("authority reduced · {class} · {why}"));
            }
            EngineEvent::AuthorityWriteFailed(class, why) => {
                self.push_ticker(format!("could not record authority for {class} · {why}"));
            }
            EngineEvent::ReportedRepairReady(issue, branch, _, _) => {
                self.push_ticker(format!("repaired the {} report · {}", issue.source, branch));
            }
            EngineEvent::ReportedRepairFailed(issue, why) => {
                self.push_ticker(format!(
                    "could not repair the {} report · {why}",
                    issue.source
                ));
            }
            EngineEvent::ReportedCommented(issue, _) => {
                self.push_ticker(format!("commented back on the {} issue", issue.source));
            }
            EngineEvent::ReportedCommentFailed(issue, why) => {
                self.push_ticker(format!(
                    "could not comment on the {} issue · {why}",
                    issue.source
                ));
            }
            // The repair left this machine. The card gets a row saying so, and
            // — when the ladder asked a human first — the approval URL becomes
            // the card's ready line, because clicking it is the one thing left
            // for a person to do.
            EngineEvent::RepairDispatched(f, accepted) => match &accepted.approval_url {
                Some(url) => {
                    self.finish_instant(
                        &f.id,
                        now,
                        "waiting on approval",
                        StageStatus::Failed,
                        Some(Provenance::Unresolved),
                        format!("job {}", accepted.job_id),
                    );
                    if let Some(card) = self.card_mut(&f.id) {
                        card.ready_line = Some(format!("approve the repair: {url}"));
                    }
                }
                None => self.finish_instant(
                    &f.id,
                    now,
                    "dispatched",
                    StageStatus::Done,
                    Some(Provenance::Observed),
                    format!("job {} · repairing in your CI", accepted.job_id),
                ),
            },
            EngineEvent::RepairDispatchFailed(f, why) => {
                self.finish_instant(
                    &f.id,
                    now,
                    "not dispatched",
                    StageStatus::Failed,
                    Some(Provenance::Unresolved),
                    why.clone(),
                );
            }
            EngineEvent::Reported(issue) => {
                // Narration only. A human-reported issue has no failure card
                // because it never enters the failure/repair pipeline — it
                // has no stack, no attribution and nothing replayable. It
                // scrolls past in the ticker exactly like a deploy does, and
                // inventing a card for it would imply a pipeline that is not
                // running.
                self.push_ticker(format!("reported via {} · {}", issue.source, issue.title));
            }
        }
    }

    fn finish_instant(
        &mut self,
        failure_id: &str,
        now: Instant,
        label: &'static str,
        status: StageStatus,
        chip: Option<Provenance>,
        detail: String,
    ) {
        if let Some(card) = self.card_mut(failure_id) {
            let elapsed = now
                .saturating_duration_since(card.last_transition)
                .as_millis() as u64;
            card.stages
                .push(StageRow::instant(label, now, status, chip, detail, elapsed));
            card.last_transition = now;
        }
    }
}

/// Provenance -> color, spec §7's fixed legend: verified=green, observed=blue,
/// inferred=amber, approved=violet, unresolved=red. One function so the TUI
/// (and any future consumer) can never assign a chip a color that isn't this
/// legend.
pub fn chip_color(p: Provenance) -> ratatui::style::Color {
    use ratatui::style::Color;
    match p {
        Provenance::Verified => Color::Green,
        Provenance::Observed => Color::Blue,
        Provenance::Inferred => Color::Rgb(0xd7, 0x9a, 0x00), // amber
        Provenance::Approved => Color::Rgb(0x9b, 0x59, 0xd0), // violet
        Provenance::Unresolved => Color::Red,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_core::*;
    use std::path::PathBuf;
    use std::time::Duration;

    fn ctx() -> RenderContext {
        RenderContext {
            repo: PathBuf::from("/srv/shop"),
            deploy_cmd: None,
        }
    }

    fn label() -> String {
        "14:22:07".to_string()
    }

    fn failure(id: &str) -> Failure {
        Failure {
            intake: engine_core::Intake::Snippet,
            id: id.to_string(),
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
                intake: engine_core::Intake::Snippet,
            },
            claim: Claim {
                text: "3 errors matching TypeError in 60s".into(),
                provenance: Provenance::Observed,
            },
        }
    }

    fn attribution(sha: &str) -> Attribution {
        Attribution {
            deploy: DeployRecord {
                sha: sha.into(),
                description: "add promo code field".into(),
                author: "maya".into(),
                deployed_at_ms: 0,
            },
            overlap_files: vec!["server.js".into()],
            minutes_after_deploy: 6,
            claim: Claim {
                text: "first error 6 min after deploy".into(),
                provenance: Provenance::Inferred,
            },
        }
    }

    fn reproduction(sha: &str, reproduced: bool) -> Reproduction {
        Reproduction {
            sha: sha.into(),
            reproduced,
            parent_clean: Some(reproduced),
            detail: "replay 500 at deploy, 200 at parent".into(),
            claims: vec![Claim {
                text: "replayed".into(),
                provenance: Provenance::Verified,
            }],
        }
    }

    fn repair(failure_id: &str) -> Repair {
        Repair {
            id: "r1".into(),
            failure_id: failure_id.into(),
            sha: "deadbeef".into(),
            branch: "drums/repair-01".into(),
            agent: "claude".into(),
            summary: "fixed the promo guard".into(),
            diff_stat: "server.js | 3 files touched".into(),
            claims: vec![Claim {
                text: "original failing request now returns 200".into(),
                provenance: Provenance::Verified,
            }],
        }
    }

    #[test]
    fn failure_detected_creates_a_card_with_a_done_detected_stage_and_selects_it() {
        let mut vm = ViewModel::new(PathBuf::from("/srv/shop/.drums/record.jsonl"));
        let now = Instant::now();
        vm.apply(
            &EngineEvent::FailureDetected(failure("f1")),
            now,
            label,
            &ctx(),
        );

        assert_eq!(vm.cards.len(), 1);
        assert_eq!(vm.selected, 0);
        let card = &vm.cards[0];
        assert_eq!(card.failure_id, "f1");
        assert_eq!(card.method, "POST");
        assert_eq!(card.path, "/api/checkout");
        assert_eq!(card.stages.len(), 1);
        assert_eq!(card.stages[0].label, "detected");
        assert_eq!(card.stages[0].status, StageStatus::Done);
        assert_eq!(
            card.stages[0].chip,
            Some(Provenance::Observed),
            "chip mapping: FailureDetected carries the failure's own claim provenance"
        );
    }

    #[test]
    fn stage_transitions_append_in_order_and_update_the_same_card_in_place() {
        let mut vm = ViewModel::new(PathBuf::from("/x"));
        let now = Instant::now();
        vm.apply(
            &EngineEvent::FailureDetected(failure("f1")),
            now,
            label,
            &ctx(),
        );
        vm.apply(
            &EngineEvent::Attributed(failure("f1"), attribution("abc123")),
            now,
            label,
            &ctx(),
        );
        vm.apply(
            &EngineEvent::Reproducing(failure("f1"), attribution("abc123")),
            now,
            label,
            &ctx(),
        );
        vm.apply(
            &EngineEvent::Reproduced(
                failure("f1"),
                attribution("abc123"),
                reproduction("abc123", true),
            ),
            now,
            label,
            &ctx(),
        );

        assert_eq!(vm.cards.len(), 1, "a card must be updated in place, never duplicated by later events for the same failure_id");
        let labels: Vec<&str> = vm.cards[0].stages.iter().map(|s| s.label).collect();
        assert_eq!(labels, vec!["detected", "attributed", "reproduced"]);
        assert_eq!(
            vm.cards[0].stages[1].chip,
            Some(Provenance::Inferred),
            "chip mapping: Attributed carries [inferred]"
        );
        assert_eq!(
            vm.cards[0].stages[2].chip,
            Some(Provenance::Verified),
            "chip mapping: a successful Reproduced carries [verified]"
        );
        assert_eq!(vm.cards[0].stages[2].status, StageStatus::Done);
    }

    #[test]
    fn reproducing_starts_a_running_stage_with_a_live_monotonic_timer() {
        let mut vm = ViewModel::new(PathBuf::from("/x"));
        let t0 = Instant::now();
        vm.apply(
            &EngineEvent::FailureDetected(failure("f1")),
            t0,
            label,
            &ctx(),
        );
        vm.apply(
            &EngineEvent::Reproducing(failure("f1"), attribution("abc123")),
            t0,
            label,
            &ctx(),
        );

        let row = &vm.cards[0].stages[1];
        assert!(row.is_running());
        assert_eq!(
            row.chip, None,
            "no chip is claimed yet while a stage is still in flight"
        );

        let e0 = row.elapsed_ms(t0);
        let e1 = row.elapsed_ms(t0 + Duration::from_millis(50));
        let e2 = row.elapsed_ms(t0 + Duration::from_millis(120));
        assert!(
            e0 <= e1 && e1 <= e2,
            "elapsed for a running stage must be monotonic as `now` advances: {e0} {e1} {e2}"
        );
        assert!(e2 >= 100, "elapsed must reflect real time passed: {e2}");
    }

    #[test]
    fn finishing_a_stage_freezes_its_elapsed_time_forever_after() {
        let mut vm = ViewModel::new(PathBuf::from("/x"));
        let t0 = Instant::now();
        vm.apply(
            &EngineEvent::FailureDetected(failure("f1")),
            t0,
            label,
            &ctx(),
        );
        vm.apply(
            &EngineEvent::Reproducing(failure("f1"), attribution("abc123")),
            t0,
            label,
            &ctx(),
        );
        let t1 = t0 + Duration::from_millis(80);
        vm.apply(
            &EngineEvent::Reproduced(
                failure("f1"),
                attribution("abc123"),
                reproduction("abc123", true),
            ),
            t1,
            label,
            &ctx(),
        );

        let row = &vm.cards[0].stages[1];
        assert!(!row.is_running());
        let frozen = row.elapsed_ms(t1);
        // Even asking "what was the elapsed at a much later `now`" must return
        // the SAME frozen value — a finished stage's timer must never keep
        // climbing on later redraw frames.
        let later = row.elapsed_ms(t1 + Duration::from_secs(30));
        assert_eq!(
            frozen, later,
            "a finished stage's elapsed must be frozen, not recomputed on every frame"
        );
        assert!(
            (70..=500).contains(&frozen),
            "should reflect the ~80ms that actually elapsed: {frozen}"
        );
    }

    #[test]
    fn repair_ready_uses_the_engines_own_reported_elapsed_not_a_recomputed_one() {
        let mut vm = ViewModel::new(PathBuf::from("/x"));
        let t0 = Instant::now();
        vm.apply(
            &EngineEvent::FailureDetected(failure("f1")),
            t0,
            label,
            &ctx(),
        );
        vm.apply(
            &EngineEvent::Repairing(failure("f1"), "claude".to_string()),
            t0,
            label,
            &ctx(),
        );
        // A huge gap between Repairing and RepairReady arriving at this
        // process vs. the authoritative 12_345ms the engine itself measured
        // (worktree create through verify) -- the engine's own number must win.
        let t1 = t0 + Duration::from_secs(500);
        vm.apply(
            &EngineEvent::RepairReady(failure("f1"), repair("f1"), 12_345),
            t1,
            label,
            &ctx(),
        );

        let row = vm.cards[0]
            .stages
            .iter()
            .find(|s| s.label == "repairing")
            .unwrap();
        assert_eq!(
            row.elapsed_ms(t1),
            12_345,
            "must use the engine-reported elapsed_ms, not this process's own receive-time delta"
        );
        assert_eq!(row.chip, Some(Provenance::Verified));
        assert_eq!(row.status, StageStatus::Done);
    }

    /// I1 (Task-3 review round 5): a `RepairReady` can legitimately carry a
    /// non-verified claim — the app's `package.json` declares no runnable test
    /// script, so the strongest check did not run and says so. The plain
    /// renderer prints every claim with its own chip, so it stays honest for
    /// free; this view collapses the claims into ONE stage chip, so that chip
    /// must be the weakest claim's, never a blanket `verified`. Otherwise the
    /// default (live) UI shows a green verified repair for a repair whose test
    /// suite was never executed — exactly the "told nothing about a check that
    /// was expected and did not run" miss (§13).
    #[test]
    fn repair_ready_carrying_an_unresolved_claim_is_not_chipped_verified() {
        let mut vm = ViewModel::new(PathBuf::from("/x"));
        let now = Instant::now();
        let mut r = repair("f1");
        r.claims.push(Claim {
            text: "`package.json` declares no `scripts.test` — no test suite was executed".into(),
            provenance: Provenance::Unresolved,
        });
        vm.apply(
            &EngineEvent::FailureDetected(failure("f1")),
            now,
            label,
            &ctx(),
        );
        vm.apply(
            &EngineEvent::Repairing(failure("f1"), "claude".to_string()),
            now,
            label,
            &ctx(),
        );
        vm.apply(
            &EngineEvent::RepairReady(failure("f1"), r, 1_000),
            now,
            label,
            &ctx(),
        );

        let row = vm.cards[0]
            .stages
            .iter()
            .find(|s| s.label == "repairing")
            .unwrap();
        assert_eq!(
            row.chip,
            Some(Provenance::Unresolved),
            "a repair with an unexecuted check must not be chipped [verified]"
        );
        assert_eq!(
            row.status,
            StageStatus::Done,
            "it is still a completed repair — just not an all-verified one"
        );
        assert!(
            row.detail.contains("no test suite was executed"),
            "design-the-miss: the check that did not run must be visible on the card, not only in the claim list nobody sees here: {}",
            row.detail
        );
        assert!(
            vm.cards[0].ready_line.is_some(),
            "propose still stops with the runnable ship command"
        );
    }

    #[test]
    fn repair_ready_sets_the_actionable_ship_command_via_the_shared_render_context() {
        let mut vm = ViewModel::new(PathBuf::from("/x"));
        let now = Instant::now();
        let render_ctx = RenderContext {
            repo: PathBuf::from("/srv/shop"),
            deploy_cmd: Some("bash deploy.sh {sha}".to_string()),
        };
        vm.apply(
            &EngineEvent::FailureDetected(failure("f1")),
            now,
            label,
            &render_ctx,
        );
        vm.apply(
            &EngineEvent::Repairing(failure("f1"), "claude".to_string()),
            now,
            label,
            &render_ctx,
        );
        vm.apply(
            &EngineEvent::RepairReady(failure("f1"), repair("f1"), 1_000),
            now,
            label,
            &render_ctx,
        );

        let ready = vm.cards[0]
            .ready_line
            .as_ref()
            .expect("ready_line must be set");
        assert!(
            ready.contains("drums ship f1 --deploy-cmd 'bash deploy.sh {sha}' --repo '/srv/shop'"),
            "must reuse the exact runnable command render.rs builds: {ready}"
        );
    }

    #[test]
    fn failure_then_repair_failed_marks_the_repairing_stage_failed_with_the_honest_reason_inline() {
        let mut vm = ViewModel::new(PathBuf::from("/x"));
        let now = Instant::now();
        vm.apply(
            &EngineEvent::FailureDetected(failure("f1")),
            now,
            label,
            &ctx(),
        );
        vm.apply(
            &EngineEvent::Attributed(failure("f1"), attribution("abc123")),
            now,
            label,
            &ctx(),
        );
        vm.apply(
            &EngineEvent::Reproducing(failure("f1"), attribution("abc123")),
            now,
            label,
            &ctx(),
        );
        vm.apply(
            &EngineEvent::Reproduced(
                failure("f1"),
                attribution("abc123"),
                reproduction("abc123", true),
            ),
            now,
            label,
            &ctx(),
        );
        vm.apply(
            &EngineEvent::Repairing(failure("f1"), "claude".to_string()),
            now,
            label,
            &ctx(),
        );

        let detail = crate::engine::RepairFailure {
            why: "the original failing request still returns 500".to_string(),
            worktree: Some("/tmp/drums-repro-abc".to_string()),
            branch: Some("drums/repair-01ARZ3ND".to_string()),
            elapsed_ms: 4_200,
        };
        vm.apply(
            &EngineEvent::RepairFailed(failure("f1"), detail),
            now,
            label,
            &ctx(),
        );

        assert_eq!(
            vm.cards.len(),
            1,
            "still exactly one card for this failure_id"
        );
        let row = vm.cards[0]
            .stages
            .iter()
            .find(|s| s.label == "repairing")
            .unwrap();
        assert_eq!(row.status, StageStatus::Failed);
        assert_eq!(row.chip, Some(Provenance::Unresolved));
        assert!(row.detail.contains("still returns 500"), "{}", row.detail);
        assert!(
            row.detail.contains("/tmp/drums-repro-abc"),
            "the worktree must stay visible for inspection: {}",
            row.detail
        );
        assert!(
            row.detail.contains("drums/repair-01ARZ3ND"),
            "{}",
            row.detail
        );
        assert_eq!(row.elapsed_ms(now), 4_200);
        assert!(
            vm.cards[0].ready_line.is_none(),
            "a failed repair has no actionable next command, only the inline reason"
        );
    }

    #[test]
    fn concurrent_failures_get_separate_stacked_cards_updated_independently() {
        let mut vm = ViewModel::new(PathBuf::from("/x"));
        let now = Instant::now();
        vm.apply(
            &EngineEvent::FailureDetected(failure("f1")),
            now,
            label,
            &ctx(),
        );
        vm.apply(
            &EngineEvent::FailureDetected(failure("f2")),
            now,
            label,
            &ctx(),
        );
        assert_eq!(vm.cards.len(), 2);
        assert_eq!(
            vm.selected, 1,
            "the most recently detected card is selected"
        );

        vm.apply(
            &EngineEvent::Attributed(failure("f1"), attribution("abc123")),
            now,
            label,
            &ctx(),
        );

        let c1 = vm.cards.iter().find(|c| c.failure_id == "f1").unwrap();
        let c2 = vm.cards.iter().find(|c| c.failure_id == "f2").unwrap();
        assert_eq!(c1.stages.len(), 2, "f1 advanced to attributed");
        assert_eq!(
            c2.stages.len(),
            1,
            "f2 must be untouched by an event naming a different failure_id"
        );

        vm.select_prev();
        assert_eq!(vm.selected, 0);
        vm.select_prev();
        assert_eq!(vm.selected, 1, "selection wraps");
        vm.select_next();
        assert_eq!(vm.selected, 0, "selection wraps forward too");
    }

    #[test]
    fn deploy_recorded_goes_to_the_ticker_not_a_card() {
        let mut vm = ViewModel::new(PathBuf::from("/x"));
        let now = Instant::now();
        let d = DeployRecord {
            sha: "abc1234def".into(),
            description: "add promo code field".into(),
            author: "maya".into(),
            deployed_at_ms: 0,
        };
        vm.apply(&EngineEvent::DeployRecorded(d), now, label, &ctx());

        assert_eq!(vm.cards.len(), 0);
        assert_eq!(vm.ticker.len(), 1);
        assert!(vm.ticker[0].contains("abc123"));
        assert!(vm.ticker[0].contains("maya"));
    }

    #[test]
    fn ticker_is_bounded_and_drops_the_oldest_entries() {
        let mut vm = ViewModel::new(PathBuf::from("/x"));
        let now = Instant::now();
        for i in 0..(TICKER_CAPACITY + 5) {
            let d = DeployRecord {
                sha: format!("sha{i}"),
                description: "d".into(),
                author: "a".into(),
                deployed_at_ms: 0,
            };
            vm.apply(&EngineEvent::DeployRecorded(d), now, label, &ctx());
        }
        assert_eq!(vm.ticker.len(), TICKER_CAPACITY);
        assert!(
            vm.ticker
                .back()
                .unwrap()
                .contains(&format!("sha{}", TICKER_CAPACITY + 4)),
            "must keep the newest entries"
        );
    }

    #[test]
    fn attribution_missing_marks_the_attributed_stage_failed_and_unresolved() {
        let mut vm = ViewModel::new(PathBuf::from("/x"));
        let now = Instant::now();
        vm.apply(
            &EngineEvent::FailureDetected(failure("f1")),
            now,
            label,
            &ctx(),
        );
        vm.apply(
            &EngineEvent::AttributionMissing(failure("f1")),
            now,
            label,
            &ctx(),
        );

        let row = vm.cards[0]
            .stages
            .iter()
            .find(|s| s.label == "attributed")
            .unwrap();
        assert_eq!(row.status, StageStatus::Failed);
        assert_eq!(row.chip, Some(Provenance::Unresolved));
        assert!(row.detail.contains("no deploy precedes"));
    }

    #[test]
    fn repro_failed_marks_the_reproduced_stage_failed_with_the_reason() {
        let mut vm = ViewModel::new(PathBuf::from("/x"));
        let now = Instant::now();
        vm.apply(
            &EngineEvent::FailureDetected(failure("f1")),
            now,
            label,
            &ctx(),
        );
        vm.apply(
            &EngineEvent::Reproducing(failure("f1"), attribution("abc123")),
            now,
            label,
            &ctx(),
        );
        vm.apply(
            &EngineEvent::ReproFailed(
                failure("f1"),
                attribution("abc123"),
                "boot timed out".to_string(),
            ),
            now,
            label,
            &ctx(),
        );

        let row = vm.cards[0]
            .stages
            .iter()
            .find(|s| s.label == "reproduced")
            .unwrap();
        assert_eq!(row.status, StageStatus::Failed);
        assert_eq!(row.chip, Some(Provenance::Unresolved));
        assert!(row.detail.contains("boot timed out"));
    }

    #[test]
    fn shipped_appends_a_shipped_stage_and_sets_the_reversible_ready_line() {
        let mut vm = ViewModel::new(PathBuf::from("/x"));
        let now = Instant::now();
        vm.apply(
            &EngineEvent::FailureDetected(failure("f1")),
            now,
            label,
            &ctx(),
        );
        let outcome = ShipOutcome {
            failure_id: "f1".into(),
            repair_sha: "deadbeef00".into(),
            action: "shipped".into(),
            deploy_cmd: "bash deploy.sh".into(),
            claims: vec![],
        };
        vm.apply(
            &EngineEvent::Shipped(failure("f1"), outcome),
            now,
            label,
            &ctx(),
        );

        let row = vm.cards[0]
            .stages
            .iter()
            .find(|s| s.label == "shipped")
            .unwrap();
        assert_eq!(row.status, StageStatus::Done);
        assert_eq!(row.chip, Some(Provenance::Verified));
        let ready = vm.cards[0].ready_line.as_ref().unwrap();
        assert!(ready.contains("reversible: drums revert f1"), "{ready}");
    }

    /// Round-2 fix (must-fix 1): `EngineEvent::Shipped` used to hardcode
    /// `Some(Provenance::Verified)` for the `shipped` stage chip regardless of
    /// what `outcome.claims` actually said, so a ship with no `--check-url`,
    /// a 404 health check, or a still-failing replay all rendered as one
    /// green `shipped … [verified]` row on the default (TUI) surface. Mirrors
    /// `RepairReady`'s already-correct "weakest claim wins the chip, and its
    /// text stays visible" rule (I1, Task-3 review round 5) so the plain
    /// renderer (which prints every claim with its own chip) and the TUI
    /// (which collapses them into one) can never disagree about whether a
    /// ship was actually verified.
    #[test]
    fn shipped_carrying_an_unresolved_post_deploy_check_is_not_chipped_verified() {
        let mut vm = ViewModel::new(PathBuf::from("/x"));
        let now = Instant::now();
        vm.apply(
            &EngineEvent::FailureDetected(failure("f1")),
            now,
            label,
            &ctx(),
        );
        let outcome = ShipOutcome {
            failure_id: "f1".into(),
            repair_sha: "deadbeef00".into(),
            action: "shipped".into(),
            deploy_cmd: "bash deploy.sh".into(),
            claims: vec![
                Claim { text: "https://shop.example.com/healthz returned 404 after deploy".into(), provenance: Provenance::Unresolved },
                Claim {
                    text: "replayed the originally failing request (redacted body) at https://shop.example.com/api/checkout: still returns 500".into(),
                    provenance: Provenance::Unresolved,
                },
            ],
        };
        vm.apply(
            &EngineEvent::Shipped(failure("f1"), outcome),
            now,
            label,
            &ctx(),
        );

        let row = vm.cards[0]
            .stages
            .iter()
            .find(|s| s.label == "shipped")
            .unwrap();
        assert_ne!(
            row.chip,
            Some(Provenance::Verified),
            "a ship whose post-deploy check returned 404 must not be chipped [verified]"
        );
        assert_eq!(row.chip, Some(Provenance::Unresolved));
        assert!(row.detail.contains("404"), "design-the-miss: the failing check must be visible on the card, not only in a claim list nobody sees here: {}", row.detail);
    }

    /// Round-2 fix (must-fix 2): the TUI used to hand-format
    /// `shipped <sha> — reversible: <cmd>` unconditionally, bypassing
    /// round-2 N3's `record_write_failed` guard (`ship::shipped_footer`,
    /// ship.rs:695) — so a ship whose `shipped` record line failed to write
    /// still promised a rollback that `drums revert` refuses with
    /// `NothingToRevert`. The TUI owns stdout in the alternate screen, so
    /// N3's `tracing::error!` backstop has no surviving signal there either;
    /// the ready_line is the ONLY place this can be said.
    #[test]
    fn shipped_ready_line_does_not_promise_reversibility_when_the_record_write_failed() {
        let mut vm = ViewModel::new(PathBuf::from("/x"));
        let now = Instant::now();
        vm.apply(
            &EngineEvent::FailureDetected(failure("f1")),
            now,
            label,
            &ctx(),
        );
        let outcome = ShipOutcome {
            failure_id: "f1".into(),
            repair_sha: "deadbeef00".into(),
            action: "shipped".into(),
            deploy_cmd: "bash deploy.sh".into(),
            claims: vec![Claim {
                text: "the record line could not be written: EACCES — the deploy DID happen, but `.drums/record.jsonl` has no `shipped` line for it, so `drums revert f1` will not find this ship".into(),
                provenance: Provenance::Unresolved,
            }],
        };
        vm.apply(
            &EngineEvent::Shipped(failure("f1"), outcome),
            now,
            label,
            &ctx(),
        );

        let ready = vm.cards[0].ready_line.as_ref().unwrap();
        assert!(
            !ready.contains("reversible"),
            "a ship whose record line failed to write must not promise reversibility: {ready}"
        );
        assert!(ready.contains("not recorded"), "{ready}");
    }

    #[test]
    fn event_with_an_unknown_failure_id_is_ignored_rather_than_panicking() {
        let mut vm = ViewModel::new(PathBuf::from("/x"));
        let now = Instant::now();
        // No FailureDetected ever seen for "ghost" -- must not panic, must
        // not fabricate a card.
        vm.apply(
            &EngineEvent::Attributed(failure("ghost"), attribution("abc123")),
            now,
            label,
            &ctx(),
        );
        assert_eq!(vm.cards.len(), 0);
    }

    #[test]
    fn every_provenance_variant_maps_to_the_spec_legend_color() {
        use ratatui::style::Color;
        assert_eq!(chip_color(Provenance::Verified), Color::Green);
        assert_eq!(chip_color(Provenance::Observed), Color::Blue);
        assert_eq!(chip_color(Provenance::Unresolved), Color::Red);
        // Amber and violet aren't named ratatui::style::Color variants;
        // pinned as explicit RGB so a future edit can't accidentally collapse
        // two of the five legend colors onto the same value.
        assert_ne!(
            chip_color(Provenance::Inferred),
            chip_color(Provenance::Approved)
        );
        assert_ne!(
            chip_color(Provenance::Inferred),
            chip_color(Provenance::Verified)
        );
    }
}
