//! Read-only HTTP API (spec §17 "where Drums lives"): the dashboard's only
//! source of truth. Mounted onto the SAME axum server `engine-ingest` already
//! runs (`drums watch` binds exactly one port) — every route here is a GET
//! under `/v1/`, alongside the existing `POST /v1/deploys` / `POST
//! /v1/events`.
//!
//! State is built by folding the exact [`crate::engine::EngineEvent`] stream
//! `render.rs` narrates to the terminal (`ApiState::apply`, called once per
//! event from the same `main.rs` loop that already prints it) — the API
//! never re-derives anything, it just also *keeps* the events the CLI was
//! already discarding after printing.
//!
//! SECURITY (non-negotiable, spec §19): these routes never return a request
//! body or captured payload. [`FailureRecord`] only ever stores
//! method/path/claims/metadata — it has no field a body could land in. `GET
//! /v1/record` reuses `engine_record::read_all` (the same tolerant reader
//! `drums ship`/`drums revert` use) but strips `request.body` from every
//! line before it leaves this process, even though the record on disk
//! already redacts it — belt and suspenders, because "redacted" is not the
//! same promise as "absent".

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::extract::{Path as AxPath, Query, Request, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use engine_core::{Attribution, Claim, Repair, Reproduction, ShipOutcome};
use serde::{Deserialize, Serialize};
use tower_http::cors::{Any, CorsLayer};

use crate::engine::{EngineEvent, RepairFailure, RepairMode};

// -- claim / chip views -------------------------------------------------

/// A [`Claim`] rendered for JSON: the same `text` + `[chip]` pairing the
/// terminal prints, never anything more.
#[derive(Debug, Clone, Serialize)]
pub struct ClaimView {
    pub text: String,
    pub chip: String,
}

impl From<&Claim> for ClaimView {
    fn from(c: &Claim) -> Self {
        ClaimView {
            text: c.text.clone(),
            chip: c.provenance.chip().to_string(),
        }
    }
}

/// The stages a failure moves through, one-to-one with the
/// [`EngineEvent`] variants that carry a [`engine_core::Failure`] — naming
/// stays identical to the event so "exactly as the CLI shows them" holds for
/// the dashboard too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Detected,
    Attributed,
    AttributionMissing,
    AttributionErrored,
    Reproducing,
    Reproduced,
    ReproFailed,
    Repairing,
    RepairFailed,
    RepairReady,
    Shipped,
    ShipFailed,
}

impl Stage {
    pub fn label(&self) -> &'static str {
        match self {
            Stage::Detected => "detected",
            Stage::Attributed => "attributed",
            Stage::AttributionMissing => "attribution missing",
            Stage::AttributionErrored => "attribution errored",
            Stage::Reproducing => "reproducing",
            Stage::Reproduced => "reproduction confirmed",
            Stage::ReproFailed => "reproduction failed",
            Stage::Repairing => "repairing",
            Stage::RepairFailed => "repair failed",
            Stage::RepairReady => "repair ready",
            Stage::Shipped => "shipped",
            Stage::ShipFailed => "ship failed",
        }
    }

    /// Whether this stage is a resting point — nothing further will happen
    /// to this failure without a human (`drums ship`) or another deploy.
    /// Drives the Overview's "in-flight" list: `!terminal()`.
    pub fn terminal(&self) -> bool {
        !matches!(
            self,
            Stage::Detected | Stage::Attributed | Stage::Reproducing | Stage::Repairing
        )
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct AttributionView {
    pub deploy_sha: String,
    pub description: String,
    pub author: String,
    pub minutes_after_deploy: u64,
    pub overlap_files: Vec<String>,
    pub claim: ClaimView,
}

impl From<&Attribution> for AttributionView {
    fn from(a: &Attribution) -> Self {
        AttributionView {
            deploy_sha: a.deploy.sha.clone(),
            description: a.deploy.description.clone(),
            author: a.deploy.author.clone(),
            minutes_after_deploy: a.minutes_after_deploy,
            overlap_files: a.overlap_files.clone(),
            claim: ClaimView::from(&a.claim),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ReproductionView {
    pub sha: String,
    pub reproduced: bool,
    pub parent_clean: Option<bool>,
    pub claims: Vec<ClaimView>,
}

impl From<&Reproduction> for ReproductionView {
    fn from(r: &Reproduction) -> Self {
        ReproductionView {
            sha: r.sha.clone(),
            reproduced: r.reproduced,
            parent_clean: r.parent_clean,
            claims: r.claims.iter().map(ClaimView::from).collect(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RepairView {
    pub id: String,
    pub agent: String,
    pub branch: String,
    pub sha: String,
    pub summary: String,
    pub diff_stat: String,
    pub claims: Vec<ClaimView>,
}

impl From<&Repair> for RepairView {
    fn from(r: &Repair) -> Self {
        RepairView {
            id: r.id.clone(),
            agent: r.agent.clone(),
            branch: r.branch.clone(),
            sha: r.sha.clone(),
            summary: r.summary.clone(),
            diff_stat: r.diff_stat.clone(),
            claims: r.claims.iter().map(ClaimView::from).collect(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RepairFailedView {
    pub why: String,
    pub worktree: Option<String>,
    pub branch: Option<String>,
    pub elapsed_ms: u64,
}

impl From<&RepairFailure> for RepairFailedView {
    fn from(d: &RepairFailure) -> Self {
        RepairFailedView {
            why: d.why.clone(),
            worktree: d.worktree.clone(),
            branch: d.branch.clone(),
            elapsed_ms: d.elapsed_ms,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ShipView {
    pub action: String,
    pub repair_sha: String,
    pub deploy_cmd: String,
    pub claims: Vec<ClaimView>,
}

impl From<&ShipOutcome> for ShipView {
    fn from(o: &ShipOutcome) -> Self {
        ShipView {
            action: o.action.clone(),
            repair_sha: o.repair_sha.clone(),
            deploy_cmd: o.deploy_cmd.clone(),
            claims: o.claims.iter().map(ClaimView::from).collect(),
        }
    }
}

/// One failure's whole known history — detection through ship (or whichever
/// stage it stopped at). The SAME shape backs both `GET /v1/failures` (a
/// `Vec<FailureRecord>`) and `GET /v1/failures/:id` (one `FailureRecord`):
/// the list view already carries "the full chain", so there is no separate,
/// thinner summary type to let drift between the two routes.
#[derive(Debug, Clone, Serialize)]
pub struct FailureRecord {
    pub id: String,
    pub service: String,
    pub error_name: String,
    pub top_frame_file: String,
    pub top_frame_function: Option<String>,
    /// Method + (query-string-redacted) path only — never the body.
    pub method: String,
    pub path: String,
    pub first_seen_ms: u64,
    pub event_count: usize,
    pub updated_at_ms: u64,
    pub stage: Stage,
    pub stage_label: String,
    pub terminal: bool,
    /// The provenance chip of the most recent claim actually earned —
    /// unchanged during a purely in-progress transition (`reproducing`,
    /// `repairing`) that hasn't earned a new claim yet.
    pub chip: String,
    pub detection: ClaimView,
    pub attribution: Option<AttributionView>,
    pub attribution_note: Option<String>,
    pub reproduction: Option<ReproductionView>,
    pub reproduction_error: Option<String>,
    pub repair: Option<RepairView>,
    pub repair_error: Option<RepairFailedView>,
    pub ship: Option<ShipView>,
    pub ship_error: Option<String>,
    /// URL of the change proposal opened for this repair, when one was.
    pub proposal_url: Option<String>,
    /// Why a configured proposal could not be opened. Surfaced, never hidden.
    pub proposal_error: Option<String>,
    /// `drums watch --dispatch-repairs`: what happened when this repair was
    /// handed to the control plane. `None` in local mode, where a repair never
    /// leaves the machine.
    ///
    /// Additive, and deliberately not a new [`Stage`]: the dashboard reads the
    /// stage enum and a new variant would be an unknown label to every deployed
    /// copy of it. A dispatched repair IS repairing — just somewhere else.
    pub dispatch: Option<DispatchView>,
    /// The exact next command to run, copyable — `None` once nothing more
    /// is left for a human to do (or nothing is ready yet).
    pub next_command: Option<String>,
}

/// One hosted dispatch, as the read-only API reports it.
///
/// Carries no credential and no failing material — a job id, a link a human can
/// click, and the reason it stopped. The instruction itself went to the control
/// plane and is not held here.
#[derive(Debug, Clone, Serialize)]
pub struct DispatchView {
    /// `None` when the dispatch was refused before a job existed.
    pub job_id: Option<String>,
    /// Present when the authority ladder said a person has to answer first.
    /// While this is set, NOTHING is running in anybody's CI.
    pub approval_url: Option<String>,
    pub expires_at: Option<String>,
    /// Why it did not go. Never both this and `job_id`.
    pub error: Option<String>,
}

fn now_ms() -> u64 {
    // Best-effort only — this stamps API freshness (`updated_at_ms`), not a
    // compliance record line, so a clock read failure degrading to 0 (rather
    // than refusing, the way `engine_record`/`engine_ingest` do for the
    // actual record) is an acceptable trade-off here.
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Counts {
    pub detected: u64,
    pub attributed: u64,
    pub reproduced: u64,
    pub repaired: u64,
    pub shipped: u64,
    /// Any terminal *_failed / *_errored / *_missing outcome, across every
    /// stage — the honest "how much of this needed a human" number.
    pub failed: u64,
    pub deploys: u64,
}

struct Inner {
    watching: PathBuf,
    ingest_port: u16,
    repair_mode: RepairMode,
    agent_name: Option<String>,
    record_path: PathBuf,
    started_at: Instant,
    counts: Counts,
    // Insertion order is chronological (detection order); index is by id for
    // O(1)-ish lookup without pulling in an extra ordered-map dependency —
    // dashboard-scale failure counts (tens, not millions) make the `Vec`
    // itself perfectly fine to sort-by-time on every read.
    order: Vec<String>,
    failures: HashMap<String, FailureRecord>,
}

impl Inner {
    fn upsert_new(&mut self, rec: FailureRecord) {
        if !self.failures.contains_key(&rec.id) {
            self.order.push(rec.id.clone());
        }
        self.failures.insert(rec.id.clone(), rec);
    }

    /// Applies `mutate` to the existing record for `id`, then stamps the
    /// stage/label/timestamp (and the chip, when a new claim was actually
    /// earned at this transition — `chip: None` keeps whatever chip was
    /// already showing, matching how the terminal narration has no chip of
    /// its own on a purely in-progress line). A missing `id` is silently a
    /// no-op: it means an event arrived for a failure this process never saw
    /// `FailureDetected` for, which should be unreachable given every other
    /// variant is only ever sent after that one, but a dashboard read must
    /// never panic on it.
    fn update(
        &mut self,
        id: &str,
        stage: Stage,
        chip: Option<&str>,
        mutate: impl FnOnce(&mut FailureRecord),
    ) {
        if let Some(rec) = self.failures.get_mut(id) {
            mutate(rec);
            rec.stage = stage;
            rec.stage_label = stage.label().to_string();
            rec.terminal = stage.terminal();
            if let Some(chip) = chip {
                rec.chip = chip.to_string();
            }
            rec.updated_at_ms = now_ms();
        }
    }
}

/// Everything the API needs to know that isn't in the event stream —
/// resolved once at `drums watch` startup.
pub struct ApiConfig {
    pub watching: PathBuf,
    pub ingest_port: u16,
    pub repair_mode: RepairMode,
    pub agent_name: Option<String>,
    pub record_path: PathBuf,
}

/// Cheap to clone (`Arc<Mutex<..>>`): one instance is built in `main.rs` and
/// shared between the event-applying loop and every HTTP handler.
#[derive(Clone)]
pub struct ApiState(Arc<Mutex<Inner>>);

impl ApiState {
    pub fn new(cfg: ApiConfig) -> Self {
        ApiState(Arc::new(Mutex::new(Inner {
            watching: cfg.watching,
            ingest_port: cfg.ingest_port,
            repair_mode: cfg.repair_mode,
            agent_name: cfg.agent_name,
            record_path: cfg.record_path,
            started_at: Instant::now(),
            counts: Counts::default(),
            order: Vec::new(),
            failures: HashMap::new(),
        })))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Folds one [`EngineEvent`] into the live view. Called once per event
    /// from the exact same loop in `main.rs` that hands the event to
    /// `render::render` — this is the only place `ApiState` is mutated.
    pub fn apply(&self, ev: &EngineEvent) {
        let mut inner = self.lock();
        match ev {
            EngineEvent::DeployRecorded(_) => {
                inner.counts.deploys += 1;
            }
            EngineEvent::OutcomeMeasured(_) => {
                // In the record; the live view has no outcome surface yet.
            }
            EngineEvent::RevisitMeasured { .. } => {
                // In the record, beside the outcome it revisits; the live
                // view has no outcome surface yet.
            }
            EngineEvent::BetEvaluated { .. } => {
                // In the record; the dashboard's Bets page reads the record.
            }
            EngineEvent::BetDrafted { .. } => {
                // In the record (the draft is appended as a proposed bet);
                // the dashboard's Bets page reads the record.
            }
            EngineEvent::ObservationRecorded(_) => {
                // Recorded in the record itself; the live view has no
                // observation surface yet, and inventing a counter the
                // dashboard does not draw would be state for nobody.
            }
            EngineEvent::FailureDetected(f) => {
                let rec = FailureRecord {
                    id: f.id.clone(),
                    service: f.service.clone(),
                    error_name: f.signature.error_name.clone(),
                    top_frame_file: f.signature.top_frame_file.clone(),
                    top_frame_function: f.signature.top_frame_function.clone(),
                    // Same fallback the TUI card uses (`ui/model.rs`): a
                    // trigger- or report-sourced failure has no replayable
                    // request, so the signature is shown instead of inventing
                    // a method/path the dashboard would display as if it were
                    // the failing call. `replayable_request()`, not
                    // `sample.request` — a request reconstructed from span
                    // attributes must never be presented as one we captured.
                    method: f
                        .replayable_request()
                        .map(|r| r.method.clone())
                        .unwrap_or_else(|| f.signature.error_name.clone()),
                    path: f
                        .replayable_request()
                        .map(|r| engine_record::redact_query_string(&r.path, &[]))
                        .unwrap_or_else(|| {
                            format!("in {} · no replayable request", f.signature.top_frame_file)
                        }),
                    first_seen_ms: f.first_seen_ms,
                    event_count: f.event_count,
                    updated_at_ms: now_ms(),
                    stage: Stage::Detected,
                    stage_label: Stage::Detected.label().to_string(),
                    terminal: Stage::Detected.terminal(),
                    chip: f.claim.provenance.chip().to_string(),
                    detection: ClaimView::from(&f.claim),
                    attribution: None,
                    attribution_note: None,
                    reproduction: None,
                    reproduction_error: None,
                    repair: None,
                    repair_error: None,
                    ship: None,
                    ship_error: None,
                    proposal_url: None,
                    proposal_error: None,
                    dispatch: None,
                    next_command: None,
                };
                inner.upsert_new(rec);
                inner.counts.detected += 1;
            }
            EngineEvent::Attributed(f, a) => {
                inner.update(
                    &f.id,
                    Stage::Attributed,
                    Some(a.claim.provenance.chip()),
                    |rec| {
                        rec.attribution = Some(AttributionView::from(a));
                        rec.attribution_note = None;
                    },
                );
                inner.counts.attributed += 1;
            }
            EngineEvent::AttributionMissing(f) => {
                inner.update(
                    &f.id,
                    Stage::AttributionMissing,
                    Some("unresolved"),
                    |rec| {
                        rec.attribution_note = Some("no deploy precedes this failure".to_string());
                    },
                );
                inner.counts.failed += 1;
            }
            EngineEvent::AttributionErrored(f, why) => {
                inner.update(
                    &f.id,
                    Stage::AttributionErrored,
                    Some("unresolved"),
                    |rec| {
                        rec.attribution_note = Some(why.clone());
                    },
                );
                inner.counts.failed += 1;
            }
            EngineEvent::Reproducing(f, _a) => {
                inner.update(&f.id, Stage::Reproducing, None, |_rec| {});
            }
            EngineEvent::Reproduced(f, _a, r) => {
                let chip = r
                    .claims
                    .last()
                    .map(|c| c.provenance.chip())
                    .unwrap_or("verified");
                inner.update(&f.id, Stage::Reproduced, Some(chip), |rec| {
                    rec.reproduction = Some(ReproductionView::from(r));
                    rec.reproduction_error = None;
                });
                inner.counts.reproduced += 1;
            }
            EngineEvent::ReproFailed(f, _a, why) => {
                inner.update(&f.id, Stage::ReproFailed, Some("unresolved"), |rec| {
                    rec.reproduction_error = Some(why.clone());
                });
                inner.counts.failed += 1;
            }
            EngineEvent::Repairing(f, _agent) => {
                inner.update(&f.id, Stage::Repairing, None, |rec| {
                    rec.repair_error = None;
                });
            }
            EngineEvent::RepairFailed(f, detail) => {
                inner.update(&f.id, Stage::RepairFailed, Some("unresolved"), |rec| {
                    rec.repair_error = Some(RepairFailedView::from(detail));
                });
                inner.counts.failed += 1;
            }
            EngineEvent::RepairReady(f, repair, _elapsed_ms) => {
                inner.update(&f.id, Stage::RepairReady, Some("verified"), |rec| {
                    rec.repair = Some(RepairView::from(repair));
                    rec.repair_error = None;
                    rec.next_command = Some(format!("drums ship {}", f.id));
                });
                inner.counts.repaired += 1;
            }
            EngineEvent::Shipped(f, outcome) => {
                inner.update(&f.id, Stage::Shipped, Some("verified"), |rec| {
                    rec.ship = Some(ShipView::from(outcome));
                    rec.ship_error = None;
                    rec.next_command = Some(format!("drums revert {}", f.id));
                });
                inner.counts.shipped += 1;
            }
            EngineEvent::ShipFailed(f, why) => {
                inner.update(&f.id, Stage::ShipFailed, Some("unresolved"), |rec| {
                    rec.ship_error = Some(why.clone());
                });
                inner.counts.failed += 1;
            }
            // Reproduction refused because the intake carries nothing
            // replayable. It is a terminal outcome for this failure, not a
            // crash, so it lands on ReproFailed with the honest reason
            // rather than leaving the card stuck on "Reproducing" forever.
            EngineEvent::ReproSkippedNotReplayable(f, _, claim) => {
                inner.update(
                    &f.id,
                    Stage::ReproFailed,
                    Some(claim.provenance.chip()),
                    |rec| {
                        rec.reproduction_error = Some(claim.text.clone());
                    },
                );
                inner.counts.failed += 1;
            }
            // A repair is ready but the authority gate refused to ship it
            // alone. The repair still stands — RepairReady is the accurate
            // stage — and the withheld reason is surfaced, never swallowed
            // (spec §13: a withheld ship is a miss a human must see).
            EngineEvent::ShipWithheld(f, why) => {
                inner.update(&f.id, Stage::RepairReady, Some("unresolved"), |rec| {
                    rec.ship_error = Some(why.clone());
                });
            }
            // A proposal does not advance the stage: the repair is still
            // RepairReady (or already Shipped). It adds a place to look.
            EngineEvent::Proposed(f, p) => {
                inner.update(
                    &f.id,
                    Stage::RepairReady,
                    Some(p.claim.provenance.chip()),
                    |rec| {
                        rec.proposal_url = Some(p.url.clone());
                    },
                );
            }
            EngineEvent::ProposalFailed(f, why) => {
                inner.update(&f.id, Stage::RepairReady, Some("unresolved"), |rec| {
                    rec.proposal_error = Some(why.clone());
                });
            }
            // Authority changes are about a CLASS, not about one failure, so
            // they update no failure record. They belong in the record and in
            // `drums authority`, which read the same append-only file.
            EngineEvent::Demoted(_, _) | EngineEvent::AuthorityWriteFailed(_, _) => {}
            // Reported-issue repairs are their own class with no failure
            // record to update: they never entered the failure pipeline, and
            // giving them one would put a card on the dashboard whose stages
            // can never advance. The dashboard learns about them through the
            // record, not through this in-memory view.
            EngineEvent::ReportedRepairReady(_, _, _, _)
            | EngineEvent::ReportedRepairFailed(_, _)
            | EngineEvent::ReportedCommented(_, _)
            | EngineEvent::ReportedCommentFailed(_, _) => {}
            // A human-reported issue never enters the failure pipeline (no
            // stack, no attribution, nothing replayable), so it creates no
            // failure record here — same decision the TUI makes by putting
            // it in the ticker instead of drawing it a card.
            EngineEvent::Reported(_) => {}
            // The repair is under way somewhere else, so the stage is
            // `Repairing` — the same word local mode uses, because it is the
            // same thing happening. What differs is where, and that is in
            // `dispatch`.
            //
            // An approval-held job is NOT counted as a repair attempt and gets
            // an `unresolved` chip: nothing has run, and a card that looked
            // green while a person had not answered yet would be exactly the
            // §13 miss the approval exists to make visible.
            EngineEvent::RepairDispatched(f, accepted) => {
                let waiting = accepted.approval_url.is_some();
                let chip = if waiting { Some("unresolved") } else { None };
                inner.update(&f.id, Stage::Repairing, chip, |rec| {
                    rec.dispatch = Some(DispatchView {
                        job_id: Some(accepted.job_id.clone()),
                        approval_url: accepted.approval_url.clone(),
                        expires_at: accepted.expires_at.clone(),
                        error: None,
                    });
                });
            }
            EngineEvent::RepairDispatchFailed(f, why) => {
                inner.update(&f.id, Stage::RepairFailed, Some("unresolved"), |rec| {
                    rec.dispatch = Some(DispatchView {
                        job_id: None,
                        approval_url: None,
                        expires_at: None,
                        error: Some(why.clone()),
                    });
                });
                inner.counts.failed += 1;
            }
        }
    }
}

// -- HTTP layer -------------------------------------------------

#[derive(Serialize)]
struct StatusView {
    watching: String,
    agent: Option<String>,
    repair_mode: &'static str,
    ingest_port: u16,
    uptime_secs: u64,
    counts: Counts,
}

async fn get_status(State(state): State<ApiState>) -> Json<StatusView> {
    let inner = state.lock();
    Json(StatusView {
        watching: inner.watching.display().to_string(),
        agent: inner.agent_name.clone(),
        repair_mode: match inner.repair_mode {
            RepairMode::Propose => "propose",
            RepairMode::Auto => "auto",
        },
        ingest_port: inner.ingest_port,
        uptime_secs: inner.started_at.elapsed().as_secs(),
        counts: inner.counts.clone(),
    })
}

async fn get_failures(State(state): State<ApiState>) -> Json<Vec<FailureRecord>> {
    let inner = state.lock();
    let mut list: Vec<FailureRecord> = inner
        .order
        .iter()
        .filter_map(|id| inner.failures.get(id).cloned())
        .collect();
    // Newest first.
    list.sort_by_key(|r| std::cmp::Reverse(r.first_seen_ms));
    Json(list)
}

async fn get_failure(State(state): State<ApiState>, AxPath(id): AxPath<String>) -> Response {
    let inner = state.lock();
    match inner.failures.get(&id) {
        Some(rec) => Json(rec.clone()).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("no failure with id {id}") })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct RecordQuery {
    limit: Option<usize>,
    since: Option<u64>,
}

#[derive(Serialize)]
struct RecordView {
    entries: Vec<serde_json::Value>,
    /// Torn/corrupt lines `engine_record::read_all` skipped rather than
    /// panicked on — surfaced, not hidden, per the tolerant reader's own
    /// honesty discipline.
    skipped: usize,
}

/// Removes `request.body` from a decoded record line, in place. The only
/// record kinds that ever carry a `request` object are `event` and
/// `repair_context`; every other kind is left untouched by construction
/// (`obj.get_mut("request"))` is simply `None` for them).
fn strip_captured_payload(mut value: serde_json::Value) -> serde_json::Value {
    if let Some(req) = value.get_mut("request").and_then(|r| r.as_object_mut()) {
        req.remove("body");
    }
    value
}

async fn get_record(State(state): State<ApiState>, Query(q): Query<RecordQuery>) -> Response {
    let record_path = state.lock().record_path.clone();
    let read = match engine_record::read_all(&record_path) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };
    let mut entries: Vec<serde_json::Value> = read
        .lines
        .into_iter()
        .map(|(_, v)| strip_captured_payload(v))
        .filter(|v| match q.since {
            Some(since) => v
                .get("recorded_at_ms")
                .and_then(|t| t.as_u64())
                .map(|t| t >= since)
                .unwrap_or(true),
            None => true,
        })
        .collect();
    if let Some(limit) = q.limit {
        if entries.len() > limit {
            entries = entries.split_off(entries.len() - limit);
        }
    }
    Json(RecordView {
        entries,
        skipped: read.skipped,
    })
    .into_response()
}

/// Enforced on every `/v1/*` GET route below when `--api-token` was set at
/// startup; `POST /v1/deploys` / `POST /v1/events` (mounted separately by
/// `engine_ingest::router`) are never wrapped by this and keep behaving
/// exactly as before. With no token configured, this layer isn't attached at
/// all — see [`router`].
async fn require_bearer_token(
    State(token): State<Arc<str>>,
    headers: HeaderMap,
    req: Request,
    next: Next,
) -> Response {
    let expected = format!("Bearer {token}");
    let ok = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|v| v == expected)
        .unwrap_or(false);
    if ok {
        next.run(req).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            Json(
                serde_json::json!({ "error": "missing or invalid Authorization: Bearer <token>" }),
            ),
        )
            .into_response()
    }
}

/// The read-only `/v1/*` GET router. Merge onto `engine_ingest::router(..)`
/// — never `axum::serve` a second listener for it; `drums watch` binds
/// exactly one port.
///
/// `token`: when `Some`, every route here requires `Authorization: Bearer
/// <token>` — this is the ONLY auth these routes ever get, so a non-loopback
/// bind without a token would put the whole record (claims, deploy metadata,
/// the daemon's config) on the network with no gate at all. `drums watch`
/// only ever binds `127.0.0.1` today; if that ever changes, a token must be
/// required before the bind is anything but loopback.
pub fn router(state: ApiState, token: Option<String>) -> Router {
    let mut r = Router::new()
        .route("/v1/status", get(get_status))
        .route("/v1/failures", get(get_failures))
        .route("/v1/failures/{id}", get(get_failure))
        .route("/v1/record", get(get_record))
        .with_state(state);
    if let Some(token) = token {
        let token: Arc<str> = Arc::from(token.as_str());
        r = r.layer(middleware::from_fn_with_state(token, require_bearer_token));
    }
    // The dashboard (`dashboard/`) is a separate origin (its own dev/prod
    // port) talking directly to this loopback daemon from the browser — with
    // no CORS layer, every `fetch()` it makes would be blocked before the
    // auth middleware above even runs. Applied OUTSIDE (after) the auth
    // layer so it wraps it: a CORS preflight `OPTIONS` (which never carries
    // `Authorization`) is answered here, not rejected by `require_bearer_token`.
    // `Any` origin is deliberate — this is a read-only, loopback-only API;
    // the token (when set) is still enforced on every actual GET.
    r.layer(
        CorsLayer::new()
            .allow_origin(Any)
            .allow_methods([Method::GET])
            .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE]),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use engine_core::{
        CapturedRequest, DeployRecord, ErrorEvent, ErrorSignature, Failure, Provenance,
    };
    use tower::ServiceExt;

    const SECRET_BODY_MARKER: &str = "SUPER_SECRET_BODY_MARKER_4242424242424242";

    fn cfg(dir: &std::path::Path) -> ApiConfig {
        ApiConfig {
            watching: dir.to_path_buf(),
            ingest_port: 7787,
            repair_mode: RepairMode::Propose,
            agent_name: Some("claude".to_string()),
            record_path: dir.join("record.jsonl"),
        }
    }

    fn failure_with_body(id: &str, body: &str) -> Failure {
        Failure {
            id: id.to_string(),
            service: "shop".into(),
            signature: ErrorSignature {
                error_name: "TypeError".into(),
                top_frame_file: "server.js".into(),
                top_frame_function: None,
            },
            first_seen_ms: 1_753_000_000_000,
            event_count: 3,
            intake: engine_core::Intake::Snippet,
            sample: ErrorEvent {
                intake: engine_core::Intake::Snippet,
                service: "shop".into(),
                occurred_at_ms: 1_753_000_000_000,
                error_name: "TypeError".into(),
                error_message: "boom".into(),
                stack: "TypeError: boom\n    at f (/x/server.js:1:1)".into(),
                request: Some(CapturedRequest {
                    method: "POST".into(),
                    path: "/api/checkout".into(),
                    content_type: Some("application/json".into()),
                    body: Some(body.to_string()),
                }),
            },
            claim: Claim {
                text: "3 errors matching TypeError in server.js within 60s".into(),
                provenance: Provenance::Observed,
            },
        }
    }

    #[test]
    fn status_reports_config_and_uptime() {
        let dir = tempfile::tempdir().unwrap();
        let state = ApiState::new(cfg(dir.path()));
        let inner = state.lock();
        assert_eq!(inner.ingest_port, 7787);
        assert_eq!(inner.agent_name.as_deref(), Some("claude"));
        assert!(matches!(inner.repair_mode, RepairMode::Propose));
    }

    #[tokio::test]
    async fn status_route_returns_shape() {
        let dir = tempfile::tempdir().unwrap();
        let state = ApiState::new(cfg(dir.path()));
        let app = router(state, None);
        let res = app
            .oneshot(HttpRequest::get("/v1/status").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["ingest_port"], 7787);
        assert_eq!(v["agent"], "claude");
        assert_eq!(v["repair_mode"], "propose");
        assert!(v["counts"].is_object());
    }

    #[tokio::test]
    async fn failures_route_lists_newest_first_and_carries_stage_and_chip() {
        let dir = tempfile::tempdir().unwrap();
        let state = ApiState::new(cfg(dir.path()));
        let mut older = failure_with_body("f-older", "{}");
        older.first_seen_ms = 1;
        let mut newer = failure_with_body("f-newer", "{}");
        newer.first_seen_ms = 2;
        state.apply(&EngineEvent::FailureDetected(older));
        state.apply(&EngineEvent::FailureDetected(newer));

        let app = router(state, None);
        let res = app
            .oneshot(
                HttpRequest::get("/v1/failures")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["id"], "f-newer", "newest-first ordering");
        assert_eq!(arr[0]["stage"], "detected");
        assert_eq!(arr[0]["chip"], "observed");
    }

    #[tokio::test]
    async fn failure_detail_route_404s_for_unknown_id() {
        let dir = tempfile::tempdir().unwrap();
        let state = ApiState::new(cfg(dir.path()));
        let app = router(state, None);
        let res = app
            .oneshot(
                HttpRequest::get("/v1/failures/nope")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn failure_detail_route_carries_the_full_chain() {
        let dir = tempfile::tempdir().unwrap();
        let state = ApiState::new(cfg(dir.path()));
        let failure = failure_with_body("f1", "{}");
        state.apply(&EngineEvent::FailureDetected(failure.clone()));
        let attribution = Attribution {
            deploy: DeployRecord {
                sha: "abc1234def".into(),
                description: "add promo".into(),
                author: "maya".into(),
                deployed_at_ms: 1,
            },
            overlap_files: vec!["server.js".into()],
            minutes_after_deploy: 6,
            claim: Claim {
                text: "first error 6 min after deploy abc123".into(),
                provenance: Provenance::Inferred,
            },
        };
        state.apply(&EngineEvent::Attributed(
            failure.clone(),
            attribution.clone(),
        ));
        let repair = Repair {
            id: "r1".into(),
            failure_id: "f1".into(),
            sha: "deadbeef".into(),
            branch: "drums/repair-f1".into(),
            agent: "claude".into(),
            summary: "fixed the guard".into(),
            diff_stat: "server.js | 1 +".into(),
            claims: vec![Claim {
                text: "original failing request now returns 200".into(),
                provenance: Provenance::Verified,
            }],
        };
        state.apply(&EngineEvent::RepairReady(failure.clone(), repair, 12_345));

        let app = router(state, None);
        let res = app
            .oneshot(
                HttpRequest::get("/v1/failures/f1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["stage"], "repair_ready");
        assert_eq!(v["attribution"]["deploy_sha"], "abc1234def");
        assert_eq!(v["repair"]["agent"], "claude");
        assert_eq!(v["next_command"], "drums ship f1");
    }

    #[tokio::test]
    async fn token_enforced_when_configured() {
        let dir = tempfile::tempdir().unwrap();
        let state = ApiState::new(cfg(dir.path()));
        let app = router(state, Some("s3cr3t".to_string()));

        let unauthenticated = app
            .clone()
            .oneshot(HttpRequest::get("/v1/status").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);

        let wrong = app
            .clone()
            .oneshot(
                HttpRequest::get("/v1/status")
                    .header(header::AUTHORIZATION, "Bearer nope")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);

        let right = app
            .oneshot(
                HttpRequest::get("/v1/status")
                    .header(header::AUTHORIZATION, "Bearer s3cr3t")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(right.status(), StatusCode::OK);
    }

    /// The dashboard is a separate browser origin talking straight to this
    /// loopback daemon — without `Access-Control-Allow-Origin` on the
    /// response, every one of its `fetch()` calls would be blocked before
    /// application code ever ran, regardless of how correct the JSON is.
    #[tokio::test]
    async fn cors_header_present_so_a_browser_dashboard_on_another_origin_can_read_the_response() {
        let dir = tempfile::tempdir().unwrap();
        let state = ApiState::new(cfg(dir.path()));
        let app = router(state, None);
        let res = app
            .oneshot(
                HttpRequest::get("/v1/status")
                    .header(header::ORIGIN, "http://localhost:3000")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(
            res.headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .is_some(),
            "missing CORS header: {:?}",
            res.headers()
        );
    }

    /// A CORS preflight must be answered even when a token is configured —
    /// the browser's own `OPTIONS` request never carries `Authorization`, so
    /// the auth layer must never see it (see the layering note on `router`).
    #[tokio::test]
    async fn cors_preflight_succeeds_even_when_a_token_is_configured() {
        let dir = tempfile::tempdir().unwrap();
        let state = ApiState::new(cfg(dir.path()));
        let app = router(state, Some("s3cr3t".to_string()));
        let res = app
            .oneshot(
                HttpRequest::options("/v1/status")
                    .header(header::ORIGIN, "http://localhost:3000")
                    .header(header::ACCESS_CONTROL_REQUEST_METHOD, "GET")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            res.status().is_success(),
            "preflight must not be rejected: {}",
            res.status()
        );
        assert!(res
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .is_some());
    }

    #[tokio::test]
    async fn token_not_enforced_when_not_configured() {
        let dir = tempfile::tempdir().unwrap();
        let state = ApiState::new(cfg(dir.path()));
        let app = router(state, None);
        let res = app
            .oneshot(HttpRequest::get("/v1/status").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn record_route_strips_body_but_keeps_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let record_path = dir.path().join("record.jsonl");
        // Write directly via engine_record, mirroring what engine-ingest/engine.rs
        // actually persist for an `event` line — the body here stands in for a
        // (hypothetically un-redacted) captured payload; the point of this test
        // is that the API strips it regardless of whether the record already did.
        let ev = engine_core::ErrorEvent {
            intake: engine_core::Intake::Snippet,
            service: "shop".into(),
            occurred_at_ms: 1,
            error_name: "TypeError".into(),
            error_message: "boom".into(),
            stack: "TypeError: boom\n    at f (/x/server.js:1:1)".into(),
            request: Some(CapturedRequest {
                method: "POST".into(),
                path: "/api/checkout".into(),
                content_type: Some("application/json".into()),
                body: Some(SECRET_BODY_MARKER.to_string()),
            }),
        };
        engine_record::append(&record_path, "event", &ev, 1_753_000_000_000).unwrap();

        let mut c = cfg(dir.path());
        c.record_path = record_path;
        let state = ApiState::new(c);
        let app = router(state, None);
        let res = app
            .oneshot(HttpRequest::get("/v1/record").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            !text.contains(SECRET_BODY_MARKER),
            "body must never leave this route: {text}"
        );
        assert!(
            text.contains("\"method\":\"POST\""),
            "non-payload metadata must survive: {text}"
        );
        assert!(text.contains("\"path\":\"/api/checkout\""));
    }

    #[tokio::test]
    async fn record_route_honors_since_and_limit() {
        let dir = tempfile::tempdir().unwrap();
        let record_path = dir.path().join("record.jsonl");
        for (i, sha) in ["a", "b", "c"].iter().enumerate() {
            let d = engine_core::DeployRecord {
                sha: sha.to_string(),
                description: "d".into(),
                author: "t".into(),
                deployed_at_ms: 0,
            };
            engine_record::append(&record_path, "deploy", &d, 100 + i as u64).unwrap();
        }
        let mut c = cfg(dir.path());
        c.record_path = record_path;
        let state = ApiState::new(c);
        let app = router(state, None);
        let res = app
            .oneshot(
                HttpRequest::get("/v1/record?since=101&limit=1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let entries = v["entries"].as_array().unwrap();
        assert_eq!(
            entries.len(),
            1,
            "limit=1 after since=101 filters out sha a (t=100)"
        );
        assert_eq!(
            entries[0]["sha"], "c",
            "limit keeps the most recent of the filtered set"
        );
    }

    /// The no-body-leak property, pinned across every GET route at once: a
    /// distinctive body string captured on a live failure must never surface
    /// in `/v1/status`, `/v1/failures`, `/v1/failures/:id`, or `/v1/record`,
    /// no matter how far through the pipeline the failure has progressed.
    #[tokio::test]
    async fn captured_body_never_appears_in_any_route_response() {
        let dir = tempfile::tempdir().unwrap();
        let record_path = dir.path().join("record.jsonl");
        let mut c = cfg(dir.path());
        c.record_path = record_path.clone();
        let state = ApiState::new(c);

        let failure = failure_with_body("f1", SECRET_BODY_MARKER);
        state.apply(&EngineEvent::FailureDetected(failure.clone()));
        let ev = engine_core::ErrorEvent {
            intake: engine_core::Intake::Snippet,
            service: "shop".into(),
            occurred_at_ms: 1,
            error_name: "TypeError".into(),
            error_message: "boom".into(),
            stack: "TypeError: boom\n    at f (/x/server.js:1:1)".into(),
            request: Some(CapturedRequest {
                method: "POST".into(),
                path: "/api/checkout".into(),
                content_type: Some("application/json".into()),
                body: Some(SECRET_BODY_MARKER.to_string()),
            }),
        };
        engine_record::append(&record_path, "event", &ev, 2).unwrap();

        let app = router(state, None);
        for path in [
            "/v1/status",
            "/v1/failures",
            "/v1/failures/f1",
            "/v1/record",
        ] {
            let res = app
                .clone()
                .oneshot(HttpRequest::get(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            let body = axum::body::to_bytes(res.into_body(), usize::MAX)
                .await
                .unwrap();
            let text = String::from_utf8_lossy(&body);
            assert!(
                !text.contains(SECRET_BODY_MARKER),
                "{path} leaked the captured body: {text}"
            );
        }
    }
}
