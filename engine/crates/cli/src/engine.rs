//! The Stage-1 pipeline: ingest → detect → attribute → reproduce, extended
//! (Stage 2, spec §17) into repair → verify → propose/auto-ship.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use engine_attribute::attribute;
use engine_core::authority::{ship_decision, Rung, ShipDecision};
use engine_core::{
    Attribution, CapturedRequest, Claim, DeployRecord, ErrorSignature, Failure, Provenance, Repair,
    RepairSample, ReportedIssue, Reproduction, ShipOutcome,
};
use engine_detect::Detector;
use engine_ingest::Ingested;
use engine_repair::{RepairAgent, RepairContext};
use engine_repro::{BootedApp, ManagedWorktree, Reproducer};
use tokio::sync::mpsc;

use crate::proc::{drain_into, kill_process_group, set_new_process_group, take_text, DRAIN_GRACE};

#[derive(Debug)]
pub enum EngineEvent {
    DeployRecorded(DeployRecord),
    FailureDetected(Failure),
    Attributed(Failure, Attribution),
    AttributionMissing(Failure),
    AttributionErrored(Failure, String),
    Reproducing(Failure, Attribution),
    Reproduced(Failure, Attribution, Reproduction),
    ReproFailed(Failure, Attribution, String),
    /// Reproduction was NOT attempted, because the failure's intake carries no
    /// replayable request (an OTel span, a log alert, a human report — see
    /// [`engine_core::Intake`]). Carries the `unresolved` claim that says so.
    ///
    /// Distinct from [`EngineEvent::ReproFailed`] on purpose: that means "we
    /// tried to replay the failing request and could not"; this means "there was
    /// never a request to replay". Collapsing them would let the record read as
    /// a flaky reproduction when the truth is that reproduction is impossible
    /// for this input, which is a permanent property of the intake and the
    /// reason the failure can never ship alone.
    ReproSkippedNotReplayable(Failure, Attribution, Claim),
    /// Repair kicked off: which agent is driving it.
    Repairing(Failure, String),
    /// A repair attempt (agent invocation, commit, or verification) failed.
    /// `worktree`/`branch` name where the human can inspect what was tried,
    /// when either exists — `None` only when nothing was ever created (e.g.
    /// no agent was available at all).
    RepairFailed(Failure, RepairFailure),
    /// A repair earned every verification claim and is ready to ship —
    /// `elapsed_ms` covers agent invocation through verification.
    RepairReady(Failure, Repair, u64),
    /// The observation producer read the record and found an error-rate
    /// shift correlated with a deploy — the first stage of the improvement
    /// loop, produced from data Drums already held, with zero configuration.
    ObservationRecorded(engine_core::observation::Observation),
    /// A change's window fully elapsed and its outcome was measured against
    /// the plan it shipped under — verified when the comparison was readable,
    /// shipped-but-unmeasured said plainly when it was not.
    OutcomeMeasured(engine_core::change::OutcomeRecorded),
    /// The slow loop matured a revisit: the same metric, the same frozen plan
    /// and baseline, re-read over a later window ending at ship + horizon
    /// days, and appended BESIDE the original outcome — never over it. The
    /// original verdict is never edited; this event only says what reality
    /// reads now.
    RevisitMeasured {
        change: String,
        horizon_days: u32,
        /// The revisit's direction differs from the original outcome's.
        /// Unmeasured readings (either side) are never "drifted" — absence of
        /// a reading is not a reading.
        drifted: bool,
        /// The revisit's own reading, so narration can state it without
        /// re-reading the record.
        outcome: engine_core::evaluation::Outcome,
        /// The metric revisited — the units the narration prints.
        metric: engine_core::evaluation::Metric,
        /// The direction the original window showed at close; `None` when
        /// that outcome was honestly unmeasured.
        was: Option<engine_core::evaluation::Direction>,
    },
    /// A confirmed bet's chain reached its outcome and the verdict was
    /// derived — support from the measurement, causal confidence from the
    /// rollout design, never asserted.
    BetEvaluated {
        bet: String,
        belief: String,
        verdict: engine_core::bet::Verdict,
        /// from/to/entries when the outcome was measured; `None` when the
        /// outcome was honestly unmeasured.
        measured: Option<(f64, f64, u32)>,
    },
    /// Proactive drafting (config `proactive_draft = true`): a NEW rate-shift
    /// observation landed and Drums ran the configured agent through the same
    /// pipeline as `drums draft`, producing a PROPOSED bet. `by` is the agent
    /// program's name. A draft commits nobody — confirmation stays a human
    /// act (`drums bet confirm <id>`), which is why this is narrated as
    /// inferred, never as a decision already made.
    BetDrafted {
        bet: String,
        belief: String,
        by: String,
    },
    /// `--repair auto` shipped a repair on its own.
    Shipped(Failure, ShipOutcome),
    /// `--repair auto` attempted to ship but the deploy command failed.
    ShipFailed(Failure, String),
    /// A change proposal (pull request) was opened for a ready repair,
    /// carrying its evidence. Additive: the repair still exists and the
    /// `drums ship` path is unchanged — this is where the work becomes
    /// visible to a human who was never watching a terminal.
    Proposed(Failure, engine_propose::Proposal),
    /// A proposal was configured but could not be opened. Never silence: a
    /// verified repair nobody can see is the failure mode this surface
    /// exists to prevent, so the reason is narrated.
    ProposalFailed(Failure, String),
    /// A class lost the act-alone rung because a ship it made went wrong.
    /// Automatic and immediate — and never silent.
    Demoted(String, String),
    /// The authority record could not be written. Surfaced because the ladder
    /// is rebuilt from that record: a line that never lands means a class keeps
    /// authority a rollback should have cost it.
    AuthorityWriteFailed(String, String),
    /// A reported-issue repair cleared the non-regression bar. Carries the
    /// branch and the claims — including the permanently unresolved one about
    /// whether it actually resolves the report.
    ReportedRepairReady(ReportedIssue, String, Vec<Claim>, u64),
    /// A reported-issue repair was attempted and did not land. Never silence:
    /// someone filed a ticket and is waiting on it.
    ReportedRepairFailed(ReportedIssue, String),
    /// The evidence was written back onto the issue thread.
    ReportedCommented(ReportedIssue, Claim),
    /// It could not be. The repair still exists; only the notification is lost.
    ReportedCommentFailed(ReportedIssue, String),
    /// `--repair auto` was configured, a repair is ready, and the authority gate
    /// REFUSED to ship it on its own — carries the reason
    /// ([`engine_core::authority::ProposeReason::withheld_text`]). A withheld
    /// ship is a miss the human must be able to see (spec §13), never silence.
    ShipWithheld(Failure, String),
    /// A human-reported issue came in (Agentation/Linear webhook). Recorded
    /// intake only — narration-only, never enters the failure/repair
    /// pipeline (real-world-scenarios plan, Scenario C item 1).
    Reported(ReportedIssue),
    /// `drums watch --dispatch-repairs`: the failure was reproduced HERE and
    /// the repair was handed to the control plane. Carries the job id, and the
    /// approval URL when the authority ladder said a person has to answer
    /// before anything runs.
    RepairDispatched(Failure, crate::dispatch::Accepted),
    /// The control plane could not be reached, or refused. NEVER fatal: local
    /// observation continues, because a hosted dispatch failing must not take
    /// down the loop that noticed the failure in the first place.
    RepairDispatchFailed(Failure, String),
}

/// Detail carried by [`EngineEvent::RepairFailed`]. A plain struct rather
/// than more positional `String`s on the enum variant, since three of its
/// four fields are easy to transpose by accident.
#[derive(Debug, Clone)]
pub struct RepairFailure {
    pub why: String,
    pub worktree: Option<String>,
    pub branch: Option<String>,
    pub elapsed_ms: u64,
}

/// Whether a completed repair stops for a human (`drums ship <id>`) or
/// continues straight into shipping. Propose is the default — spec §19
/// "repairs default to PROPOSE; autonomous shipping is an explicit opt-in".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepairMode {
    Propose,
    Auto,
}

/// The lower of two rungs. `Rung` deliberately has no `Ord`: ordering it
/// would invite arithmetic on authority ("rung + 1"), and the ladder's steps
/// are not interchangeable units. This is the only comparison anything needs.
pub(crate) fn min_rung(a: Rung, b: Rung) -> Rung {
    fn level(r: Rung) -> u8 {
        match r {
            Rung::Observe => 0,
            Rung::Shadow => 1,
            Rung::Propose => 2,
            Rung::ActAlone => 3,
        }
    }
    if level(a) <= level(b) {
        a
    } else {
        b
    }
}

impl RepairMode {
    /// The CEILING this mode sets on the autonomy ladder (spec §11).
    ///
    /// Not the rung itself — the highest rung the operator has consented to.
    /// The rung a class actually sits on is EARNED, and lives in
    /// `engine-authority`, folded from the append-only record. The effective
    /// authority is the lower of the two:
    ///
    /// - `--repair propose` pins the ceiling at Propose. Nothing ships, no
    ///   matter what a class has earned. An operator can always opt out.
    /// - `--repair auto` raises the ceiling to act-alone. It does NOT grant
    ///   act-alone: a class still has to have earned it. This is the whole of
    ///   "autonomy is earned, not configured" — a flag that granted it would
    ///   make the ladder decorative.
    ///
    /// The change this represents is deliberate and visible: `--repair auto`
    /// alone no longer ships. It withholds, narrates why, and accumulates a
    /// promotion proposal — which a human applies with `drums authority
    /// promote <class>`.
    pub fn ceiling(self) -> Rung {
        match self {
            RepairMode::Propose => Rung::Propose,
            RepairMode::Auto => Rung::ActAlone,
        }
    }
}

pub struct EngineConfig {
    pub repo: PathBuf,
    pub threshold: usize,
    pub window_ms: u64,
    pub app_root: String,
    /// The behavior source (PostHog), when configured — the reader for
    /// completion-rate/abandonment change measurement. `None` is a normal
    /// state; due changes needing it are skipped with the reason narrated
    /// once, not written as unmeasured outcomes.
    pub behavior: Option<Arc<dyn engine_behavior::BehaviorSource>>,
    /// Where the engine appends `repair_ready`/`shipped` record lines
    /// (`.drums/record.jsonl`) — deploys/events are appended by
    /// `engine-ingest`; this is the same file, written from here for the
    /// lines only the repair pipeline produces.
    pub record_path: PathBuf,
    /// `None` means no repair agent was available at startup
    /// (`CliRepairAgent::detect()` returned `None`) — the engine still
    /// reproduces, it just cannot attempt a repair, and stays silent about
    /// it rather than manufacturing a failure event for an attempt that
    /// never happened.
    pub repair_agent: Option<Arc<dyn RepairAgent>>,
    pub repair_mode: RepairMode,
    /// `None` means no change proposal was requested — the repair stops at a
    /// branch plus `drums ship`, exactly as before. `Some` opens a proposal
    /// carrying the evidence as soon as a repair is ready, BEFORE any ship
    /// decision, so a human sees the reasoning whether or not it ships.
    pub proposal: Option<Arc<dyn engine_propose::ChangeProposal>>,
    /// Branch a proposal targets. Ignored when `proposal` is `None`.
    pub proposal_base: String,
    /// See [`RepairPipelineCfg::repair_reported`].
    pub repair_reported: bool,
    /// Required for `RepairMode::Auto` to actually continue into shipping;
    /// `{sha}`/`{repo}` are substituted before the command is argv-split
    /// and run (no shell).
    pub deploy_cmd: Option<String>,
    pub check_url: Option<String>,
    /// Boot timeout for the repair-verification boot (original request +
    /// `/health`), independent of reproduction's own boot timeout.
    pub repair_boot_timeout_ms: u64,
    /// Signatures to mark opened before the detector ever sees a live event —
    /// the restart-idempotence seam (spec: a service must survive restarts of
    /// itself). `drums watch` always passes `vec![]` (a fresh process has
    /// nothing to restore); `drumsd` populates this on every start by
    /// replaying `.drums/record.jsonl`'s `event` lines through a throwaway
    /// detector (`drums_watch::restore::rebuild_opened_signatures`) and
    /// handing back the resulting gated set, so a restart does not re-open —
    /// and therefore does not re-attempt a repair for — a signature that
    /// already crossed the threshold (and, in particular, one that already
    /// has a `repair_ready` or `shipped` record line) in an earlier run of
    /// this same process.
    pub initial_opened: Vec<ErrorSignature>,
    /// `drums watch --boot-cmd`: the same command template handed to
    /// [`engine_repro::LocalProcessReproducer`], reused here for the
    /// repair-verification boot (`verify_repair`) — a repaired real app
    /// needs to come up the same way its reproduction did, or verification
    /// would try to boot it as `node <entry>` regardless. `None` keeps
    /// today's node-only behavior.
    pub boot_cmd: Option<String>,
    /// Slack delivery for the four-kind proactive messages (`crate::notify`),
    /// when a webhook is configured (`slack_webhook_url` in config, or
    /// `DRUMS_SLACK_WEBHOOK_URL`). `None` means notifications are off — the
    /// terminal narration and the record are identical either way, because
    /// Slack is a courtesy copy of the record, never a second source of truth.
    pub notify: Option<crate::notify::Sink>,
    /// Consent gate for proactive drafting on the observe tick (config
    /// `proactive_draft`, default false — it spends the customer's own agent
    /// tokens, so it is opt-in by name, never assumed).
    pub proactive_draft: bool,
    /// The poll half of reported intake: present when this watch is logged
    /// in, absent otherwise. `None` is a normal state — webhooks still work.
    pub tracker_poll: Option<crate::tracker_poll::TrackerPoll>,
    /// The drafting agent command template, resolved at startup via
    /// `crate::draft::agent_template` (config `agent_cmd`, then
    /// `DRUMS_AGENT_CMD`, then claude/codex on PATH) — same startup-time
    /// resolution as `repair_agent`. `None` means drafting is skipped with
    /// the reason narrated via tracing, never an error.
    pub draft_agent: Option<String>,
    /// Opt-in record sync to the hosted plane (config `sync_record = true`
    /// plus a `drums login` credential), resolved at startup like `notify`
    /// and `dispatch`. `None` — the default — means the record stays
    /// local-first and nothing about the loop changes; see [`crate::sync`]
    /// for the privacy invariant a `Some` is bound by.
    pub sync: Option<Arc<crate::sync::RecordSync>>,
    /// `drums watch --dispatch-repairs`: hand each REPAIR to the Drums control
    /// plane instead of running it here. `None` (the default) is local mode,
    /// unchanged in every respect.
    ///
    /// What this does NOT move is everything above the repair. Detection,
    /// attribution and reproduction still run on this machine, and a failure
    /// that did not reproduce is never dispatched — see
    /// [`crate::dispatch::RemoteRepairs::dispatch`], which holds that rule
    /// rather than trusting this call site to.
    pub dispatch: Option<Arc<crate::dispatch::RemoteRepairs>>,
}

pub struct Engine;

impl Engine {
    pub async fn run(
        cfg: EngineConfig,
        mut rx: mpsc::UnboundedReceiver<Ingested>,
        reproducer: Arc<dyn Reproducer>,
        tx: mpsc::UnboundedSender<EngineEvent>,
    ) {
        let mut detector = Detector::new(cfg.threshold, cfg.window_ms, cfg.app_root.clone());
        // Restart-idempotence seam: `drumsd` hands back the gated set it
        // reconstructed by replaying this repo's own record before this
        // detector ever sees a live event, so a signature that already
        // crossed the threshold (and, in particular, one that already has a
        // `repair_ready`/`shipped` line) in an earlier run of this process
        // stays gated across the restart. `drums watch` always passes an
        // empty vec — a fresh process trivially has nothing to restore.
        for sig in cfg.initial_opened.iter().cloned() {
            detector.mark_opened(sig);
        }
        let mut deploys: Vec<DeployRecord> = Vec::new();
        // The detector lives on THIS loop thread; a `--repair auto` ship
        // completing on a spawned task cannot call `detector.reopen(..)`
        // directly. It sends the signature back over this channel instead —
        // the loop applies it inline, so `Detector::observe` and
        // `Detector::reopen` are never called from two places at once. The
        // sender is cloned into every spawned repair task; the original stays
        // alive here so `reopen_rx.recv()` blocks rather than resolving to
        // `None` while the loop itself is still running.
        let (reopen_tx, mut reopen_rx) = mpsc::unbounded_channel::<ErrorSignature>();

        // The observation producer's clock. Ten minutes, first tick immediate,
        // so a freshly booted watch reads its record once right away — which is
        // also what makes the first real observations appear on a repo that has
        // been quietly accumulating events and deploys all along.
        // Changes that were due but could not be measured (source missing or a
        // read failing) get their reason narrated once, not every ten minutes.
        let mut measure_skips_narrated: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        // The drafted-once guard: observation ids that have already had their
        // one proactive-draft consideration (attempted, or skipped with the
        // reason narrated). Same shape and same reason as
        // `measure_skips_narrated` — say it once, never retry, never loop.
        let mut draft_attempted: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        let mut observe_tick = tokio::time::interval(std::time::Duration::from_secs(600));
        observe_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        // The context line of every notification ("drums · <repo>"): the
        // repo's display name, never its path — a webhook message gets pasted
        // into threads, and a filesystem path is nobody's business there.
        let repo_name = cfg
            .repo
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("repo")
            .to_string();

        let pcfg = cfg.repair_agent.map(|agent| RepairPipelineCfg {
            repair_agent: agent,
            record_path: cfg.record_path.clone(),
            repair_mode: cfg.repair_mode,
            deploy_cmd: cfg.deploy_cmd.clone(),
            check_url: cfg.check_url.clone(),
            boot_timeout_ms: cfg.repair_boot_timeout_ms,
            boot_cmd: cfg.boot_cmd.clone(),
            proposal: cfg.proposal.clone(),
            proposal_base: cfg.proposal_base.clone(),
            repair_reported: cfg.repair_reported,
            // The watch reads its own record at repair time; the wire path
            // fills this instead. Empty = read your own.
            remembered: Vec::new(),
            reopen_tx: reopen_tx.clone(),
            notify: cfg.notify.clone(),
            repo_name: repo_name.clone(),
        });

        loop {
            tokio::select! {
                item = rx.recv() => {
                    let Some(item) = item else { break };
                    match item {
                        Ingested::Deploy(d) => {
                            deploys.push(d.clone());
                            let _ = tx.send(EngineEvent::DeployRecorded(d));
                        }
                        Ingested::Error(e) => {
                            let Some(failure) = detector.observe(e) else { continue };
                            let _ = tx.send(EngineEvent::FailureDetected(failure.clone()));
                            // The attribute→reproduce→repair chain runs off the
                            // recv loop: it does a worktree checkout + process
                            // boot + HTTP replay (plausibly seconds to low
                            // minutes) at least once, and again for repair
                            // verification — running it inline here would
                            // stall ingestion of every subsequent
                            // deploy/error until it finishes.
                            let repo = cfg.repo.clone();
                            let deploys_snapshot = deploys.clone();
                            let repro = ReproSetup { reproducer: reproducer.clone(), boot_cmd: cfg.boot_cmd.clone() };
                            let tx = tx.clone();
                            let pcfg = pcfg.clone();
                            let dispatch = cfg.dispatch.clone();
                            tokio::spawn(attribute_and_reproduce(repo, deploys_snapshot, failure, repro, tx, pcfg, dispatch));
                        }
                        Ingested::Reported(issue) => {
                            // engine-ingest already appended the `reported`
                            // record line and redacted the copy that went
                            // there. Narration always happens; a repair only
                            // when the operator asked for one.
                            let _ = tx.send(EngineEvent::Reported(issue.clone()));

                            // Scenario C, deliberately its OWN path rather
                            // than `attribute_and_reproduce`: a reported issue
                            // has no stack to attribute and nothing to replay,
                            // so every stage that pipeline runs would have to
                            // be faked. It gets a non-regression bar instead,
                            // and the ship gate refuses it structurally
                            // (`Intake::Reported.is_replayable()` is false).
                            if let Some(pcfg) = pcfg.clone() {
                                if pcfg.repair_reported {
                                    tokio::spawn(repair_reported(cfg.repo.clone(), issue, tx.clone(), pcfg));
                                }
                            }
                        }
                    }
                }
                Some(sig) = reopen_rx.recv() => {
                    detector.reopen(&sig);
                }
                _ = observe_tick.tick() => {
                    // Observation, not detection: rate shifts are facts for the
                    // improvement loop, computed from the record Drums already
                    // keeps. A missing or empty record is the normal first-boot
                    // state and produces honest silence, not an error.
                    if let Ok(read) = engine_record::read_all(&cfg.record_path) {
                        if let Some(now) = now_ms() {
                            let mut found = engine_detect::observe::rate_shifts(
                                &read.lines,
                                &cfg.app_root,
                                now,
                                &engine_detect::observe::RateShiftParams::default(),
                            );
                            // Frustration is a pull from the behavior source,
                            // where the click telemetry already lives. No
                            // source, or a source that cannot answer this
                            // question, is a normal state and stays quiet.
                            if let Some(behavior) = cfg.behavior.as_ref() {
                                match engine_behavior::frustration::observe_frustration(
                                    behavior.as_ref(),
                                    &read.lines,
                                    now,
                                    &engine_behavior::frustration::FrustrationParams::default(),
                                )
                                .await
                                {
                                    Ok(mut more) => found.append(&mut more),
                                    Err(engine_behavior::BehaviorError::UnsupportedMetric {
                                        ..
                                    }) => {}
                                    Err(e) if e.is_transient() => {
                                        tracing::debug!("frustration read skipped: {e}");
                                    }
                                    Err(e) => tracing::warn!("frustration read failed: {e}"),
                                }
                            }
                            // The tracker poll rides the same tick: fresh
                            // issues go through the daemon's own ingest door,
                            // so both intake paths produce identical lines.
                            if let Some(tp) = cfg.tracker_poll.as_ref() {
                                match tp.tick(&read.lines, now).await {
                                    Ok(0) => {}
                                    Ok(n) => {
                                        tracing::info!("tracker poll took {n} reported issue(s)");
                                    }
                                    Err(e) => tracing::debug!("tracker poll skipped: {e}"),
                                }
                            }
                            // Which observations THIS tick appended — the
                            // proactive-draft trigger below fires only on a
                            // tick that produced something new.
                            let mut fresh_observations: Vec<String> = Vec::new();
                            for o in found {
                                append_record(
                                    &cfg.record_path,
                                    engine_core::observation::RECORD_KIND,
                                    &o,
                                );
                                fresh_observations.push(o.id.0.clone());
                                let ev = EngineEvent::ObservationRecorded(o);
                                notify_event(&cfg.notify, &repo_name, &ev);
                                let _ = tx.send(ev);
                            }
                            // The fourth field of the record: any change whose
                            // window has fully elapsed gets its outcome taken
                            // now and written beside it — once, ever.
                            let (due, skips) = crate::change_cmd::measure_due_changes(
                                &read.lines,
                                now,
                                cfg.behavior.as_deref(),
                            )
                            .await;
                            for (chg, why) in skips {
                                if measure_skips_narrated.insert(chg.clone()) {
                                    tracing::warn!(change = %chg, %why, "change due but not measurable yet");
                                }
                            }
                            for outcome in due {
                                append_record(
                                    &cfg.record_path,
                                    engine_core::change::OUTCOME_KIND,
                                    &outcome,
                                );
                                let ev = EngineEvent::OutcomeMeasured(outcome);
                                notify_event(&cfg.notify, &repo_name, &ev);
                                let _ = tx.send(ev);
                            }
                            // Keep the semantic index current. Derived and
                            // disposable: a failure here is logged, never
                            // fatal, and never blocks measurement.
                            if let Ok(reread) = engine_record::read_all(&cfg.record_path) {
                                let repo_dir = cfg
                                    .record_path
                                    .parent()
                                    .and_then(|p| p.parent())
                                    .map(|p| p.to_path_buf())
                                    .unwrap_or_default();
                                let product = repo_dir
                                    .file_name()
                                    .and_then(|n| n.to_str())
                                    .unwrap_or("product")
                                    .to_string();
                                let db_path = cfg
                                    .record_path
                                    .parent()
                                    .map(|p| p.join("semantic.db"))
                                    .unwrap_or_else(|| std::path::PathBuf::from("semantic.db"));
                                match engine_semantic::Store::open(&db_path) {
                                    Ok(mut store) => {
                                        if let Err(e) = engine_semantic::store::refresh(
                                            &mut store,
                                            &reread.lines,
                                            &product,
                                        ) {
                                            tracing::warn!(%e, "semantic index refresh failed");
                                        }
                                    }
                                    Err(e) => tracing::warn!(%e, "semantic store unavailable"),
                                }
                            }
                            // Bets whose chain just closed: derive the verdict
                            // and write it beside the bet. Re-read so the
                            // outcomes appended above are visible. Idempotent:
                            // an evaluated bet is never due again.
                            if let Ok(reread) = engine_record::read_all(&cfg.record_path) {
                                for due_bet in crate::bet_cmd::evaluate_due(&reread.lines) {
                                    let status = engine_core::bet::BetStatusChanged {
                                        bet: due_bet.bet.id.clone(),
                                        status: engine_core::bet::BetStatus::Evaluated {
                                            verdict: due_bet.verdict.clone(),
                                        },
                                    };
                                    append_record(
                                        &cfg.record_path,
                                        engine_core::bet::STATUS_KIND,
                                        &status,
                                    );
                                    let ev = EngineEvent::BetEvaluated {
                                        bet: due_bet.bet.id.0.clone(),
                                        belief: due_bet.bet.belief.clone(),
                                        verdict: due_bet.verdict,
                                        measured: due_bet.measured,
                                    };
                                    notify_event(&cfg.notify, &repo_name, &ev);
                                    let _ = tx.send(ev);
                                }
                            }
                            // The slow loop: revisit matured outcomes at
                            // 7/30/90 days after ship — the same metric, the
                            // same frozen plan and baseline, a later window —
                            // and append what reality reads now BESIDE the
                            // original outcome, never over it. Idempotent:
                            // the revisit line's presence in the record is
                            // the (change, horizon) guard, so a restart never
                            // re-measures. Unreadable sources are named skips
                            // retried next tick, never invented readings.
                            if let Ok(reread) = engine_record::read_all(&cfg.record_path) {
                                let (revisits, skips) = crate::change_cmd::revisit_due_changes(
                                    &reread.lines,
                                    now,
                                    cfg.behavior.as_deref(),
                                )
                                .await;
                                for (key, why) in skips {
                                    if measure_skips_narrated.insert(key.clone()) {
                                        tracing::warn!(revisit = %key, %why, "revisit due but not measurable yet");
                                    }
                                }
                                for revisit in revisits {
                                    let original = engine_core::change::OutcomeRecorded::for_change(
                                        reread.lines.iter(),
                                        &revisit.change,
                                    );
                                    let drifted = original
                                        .as_ref()
                                        .map(|o| revisit.drifted_from(o))
                                        .unwrap_or(false);
                                    let was = original.and_then(|o| match o.outcome {
                                        engine_core::evaluation::Outcome::Measured { direction, .. } => Some(direction),
                                        engine_core::evaluation::Outcome::Unmeasured(_) => None,
                                    });
                                    let metric = engine_core::change::Change::all(reread.lines.iter())
                                        .into_iter()
                                        .find(|c| c.id == revisit.change)
                                        .map(|c| c.plan.metric)
                                        .unwrap_or(engine_core::evaluation::Metric::ErrorEventRate);
                                    append_record(
                                        &cfg.record_path,
                                        engine_core::change::REVISIT_KIND,
                                        &revisit,
                                    );
                                    let ev = EngineEvent::RevisitMeasured {
                                        change: revisit.change.0.clone(),
                                        horizon_days: revisit.horizon_days,
                                        drifted,
                                        outcome: revisit.outcome,
                                        metric,
                                        was,
                                    };
                                    notify_event(&cfg.notify, &repo_name, &ev);
                                    let _ = tx.send(ev);
                                }
                            }
                            // Record sync (consent-gated; config
                            // `sync_record` plus a `drums login` credential,
                            // both resolved at startup into `cfg.sync`). One
                            // pass per tick, after every line this tick
                            // appended, so the hosted mirror trails the
                            // local record by at most one tick. Awaited here
                            // exactly as `measure_due_changes` is; a failed
                            // pass is a warn and a fresh, server-anchored
                            // attempt NEXT tick — never a retry within this
                            // one — because the record is the source of
                            // truth, the hosted copy is a courtesy mirror,
                            // and sync must never block or fail the loop it
                            // rides on.
                            if let Some(sync) = cfg.sync.as_ref() {
                                match sync.pass().await {
                                    Ok(s) if s.sent > 0 => tracing::info!(
                                        lines = s.sent,
                                        through = s.through,
                                        repo = %sync.repo_slug(),
                                        "record sync"
                                    ),
                                    Ok(_) => {}
                                    Err(why) => tracing::warn!(
                                        %why,
                                        "record sync pass failed — the local record is untouched; retrying next tick"
                                    ),
                                }
                            }
                            // Proactive drafting (consent-gated; config
                            // `proactive_draft`). Fires only on a tick that
                            // just appended a NEW observation, at most once
                            // per observation id ever (`draft_attempted`) —
                            // a skip or a failure is narrated via tracing and
                            // never retried. The agent invocation is awaited
                            // here on the tick, exactly as
                            // `measure_due_changes` is (the select loop's
                            // shutdown responsiveness accepts the same
                            // bound), and `draft::run_agent`'s own timeout
                            // caps how long that can be.
                            let fresh: Vec<String> = fresh_observations
                                .into_iter()
                                .filter(|id| draft_attempted.insert(id.clone()))
                                .collect();
                            if !fresh.is_empty() {
                                if let Ok(reread) = engine_record::read_all(&cfg.record_path) {
                                    if let Some(reason) = should_draft(
                                        &reread.lines,
                                        cfg.proactive_draft,
                                        cfg.draft_agent.is_some(),
                                    ) {
                                        tracing::info!(
                                            observations = fresh.len(),
                                            reason,
                                            "proactive draft skipped"
                                        );
                                    } else if let Some(template) = cfg.draft_agent.as_deref() {
                                        match run_proactive_draft(
                                            &cfg.repo,
                                            &cfg.record_path,
                                            &reread.lines,
                                            template,
                                        )
                                        .await
                                        {
                                            Ok((bet, belief, by)) => {
                                                let ev = EngineEvent::BetDrafted { bet, belief, by };
                                                notify_event(&cfg.notify, &repo_name, &ev);
                                                let _ = tx.send(ev);
                                            }
                                            // A declined draft ("skip") and a
                                            // failed one land here alike:
                                            // narrated, never an error, never
                                            // retried for these observations.
                                            Err(why) => tracing::info!(
                                                %why,
                                                "proactive draft did not produce a bet"
                                            ),
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The reproducer plus the boot command it was built with (`--boot-cmd`).
/// Bundled because the `Reproducer` trait does not expose its boot command,
/// and the spawn-failure narration needs the program name (see
/// [`describe_repro_error`]) — same no-parameter-growth discipline as
/// [`RepairPipelineCfg`] below.
#[derive(Clone)]
pub(crate) struct ReproSetup {
    pub(crate) reproducer: Arc<dyn Reproducer>,
    pub(crate) boot_cmd: Option<String>,
}

/// Everything a spawned repair attempt needs beyond `repo`/`failure`/
/// `attribution` — bundled so `attribute_and_reproduce`'s signature doesn't
/// grow a parameter per repair concern. Cheap to clone: the agent and the
/// channel sender are both already `Arc`/mpsc-internal-`Arc`.
#[derive(Clone)]
pub(crate) struct RepairPipelineCfg {
    pub(crate) repair_agent: Arc<dyn RepairAgent>,
    pub(crate) record_path: PathBuf,
    pub(crate) repair_mode: RepairMode,
    pub(crate) deploy_cmd: Option<String>,
    pub(crate) check_url: Option<String>,
    pub(crate) boot_timeout_ms: u64,
    pub(crate) boot_cmd: Option<String>,
    pub(crate) proposal: Option<Arc<dyn engine_propose::ChangeProposal>>,
    pub(crate) proposal_base: String,
    /// `drums watch --repair-reported`. OFF by default: an agent editing the
    /// repo because someone filed a ticket is a far bigger step than repairing
    /// a failure Drums reproduced itself, and it must be asked for.
    pub(crate) repair_reported: bool,
    /// Memory that arrived over the wire (a dispatched runner's instruction).
    /// Empty means "read your own record", the watch's normal state.
    pub(crate) remembered: Vec<String>,
    pub(crate) reopen_tx: mpsc::UnboundedSender<ErrorSignature>,
    /// Same handle as [`EngineConfig::notify`], carried here because
    /// `RepairReady` — the one Decision the pipeline produces — is emitted
    /// from a spawned repair task, off the loop that holds `cfg`. Clones
    /// share the de-duplication set, so an event id notifies once wherever
    /// it is emitted from.
    pub(crate) notify: Option<crate::notify::Sink>,
    pub(crate) repo_name: String,
}

/// attribute() → Reproducing → reproduce() → (if reproduced) repair →
/// verify → propose/auto-ship, for one already-detected failure. Spawned off
/// the recv loop so a slow reproduction or repair never blocks ingestion of
/// later deploys/errors (see the comment at the `tokio::spawn` call site in
/// `Engine::run`).
async fn attribute_and_reproduce(
    repo: PathBuf,
    deploys: Vec<DeployRecord>,
    failure: Failure,
    repro: ReproSetup,
    tx: mpsc::UnboundedSender<EngineEvent>,
    pcfg: Option<RepairPipelineCfg>,
    dispatch: Option<Arc<crate::dispatch::RemoteRepairs>>,
) {
    // attribute() returns Result<Option<Attribution>, AttributeError>:
    // Ok(None) = genuinely no preceding deploy; Err = the machinery
    // failed (bad sha, git error) — render those differently or the
    // engine lies about what it knows.
    let attr_result = {
        let repo = repo.clone();
        let failure = failure.clone();
        tokio::task::spawn_blocking(move || attribute(&repo, &deploys, &failure)).await
    };
    let attr = match attr_result {
        Ok(Ok(Some(attr))) => attr,
        Ok(Ok(None)) => {
            let _ = tx.send(EngineEvent::AttributionMissing(failure));
            return;
        }
        Ok(Err(e)) => {
            let _ = tx.send(EngineEvent::AttributionErrored(failure, e.to_string()));
            return;
        }
        Err(join_err) => {
            let _ = tx.send(EngineEvent::AttributionErrored(
                failure,
                join_err.to_string(),
            ));
            return;
        }
    };
    let _ = tx.send(EngineEvent::Attributed(failure.clone(), attr.clone()));

    reproduce_and_repair(repo, failure, attr, repro, tx, pcfg, dispatch).await;
}

/// Reproduce a failure at an ALREADY-KNOWN attribution, then repair it.
///
/// Split out of `attribute_and_reproduce` because CI needs exactly this half
/// and none of the other. A repair dispatched into GitHub Actions arrives with
/// the attribution already decided — the machine that detected the failure did
/// that work, and redoing it in a runner that has no deploy history would
/// produce a different, worse answer.
///
/// The two callers must not drift: local `drums watch` and `drums repair` in a
/// runner have to make the same claims about the same failure, or the record
/// means different things depending on where the work happened.
pub(crate) async fn reproduce_and_repair(
    repo: PathBuf,
    failure: Failure,
    attr: Attribution,
    repro: ReproSetup,
    tx: mpsc::UnboundedSender<EngineEvent>,
    pcfg: Option<RepairPipelineCfg>,
    dispatch: Option<Arc<crate::dispatch::RemoteRepairs>>,
) {
    // THE INTAKE FORK. Reproduction replays the ACTUAL failing request against
    // the rebuilt revision — that replay is the only thing that earns
    // `verified`. A trigger/reported failure has no such request, so
    // reproduction is SKIPPED and an `unresolved` claim says exactly that.
    // The alternatives are both lies: synthesizing a body would manufacture a
    // `verified` for a request nobody ever made, and staying silent would leave
    // the record reading as though the step simply hadn't run yet.
    //
    // Repair still proceeds if an agent is configured — a proposal built from
    // the stack trace alone is genuinely useful — but with its verification
    // claims limited to what was actually executed (build/boot/tests can still
    // be `verified`; the original-request replay cannot exist), and it can never
    // ship on its own (`ship_decision` in `run_repair_pipeline`).
    if !failure.intake.is_replayable() {
        let claim = failure.intake.no_replay_claim();
        let _ = tx.send(EngineEvent::ReproSkippedNotReplayable(
            failure.clone(),
            attr.clone(),
            claim,
        ));
        // Deliberately NOT dispatched, in either mode. This branch is reached
        // precisely because there was never a request to replay, so no
        // reproduction can exist for it — and a repair dispatched into
        // somebody's CI on that basis would be a repair for a failure Drums
        // never made happen. Locally a propose-only repair is still worth
        // producing from the stack trace; remotely it is not ours to spend
        // their runners on.
        run_repair_pipeline(repo, failure, attr, None, pcfg, tx).await;
        return;
    }

    let _ = tx.send(EngineEvent::Reproducing(failure.clone(), attr.clone()));
    match repro.reproducer.reproduce(&repo, &failure, &attr).await {
        Ok(r) => {
            let reproduced = r.reproduced;
            let _ = tx.send(EngineEvent::Reproduced(
                failure.clone(),
                attr.clone(),
                r.clone(),
            ));
            // THE ONLY GATE INTO A REPAIR, local or hosted. `reproduced` false
            // means the failure did not happen again at the attributed
            // revision — the attribution is wrong, and anything built on it is
            // a change made for the wrong reason. Both arms below sit inside
            // it, so hosted mode cannot become the lenient one by accident;
            // `RemoteRepairs::dispatch` re-checks it anyway, because a rule
            // this important should not depend on one `if` staying where it is.
            if reproduced {
                // Hosted mode: the repair leaves this machine. Everything up to
                // here — detect, attribute, reproduce — already happened here,
                // which is the point: the control plane is told about a failure
                // Drums watched fail again, not about one it was told about.
                if let Some(remote) = dispatch {
                    dispatch_repair(&repo, failure, attr, r, remote, tx).await;
                    return;
                }
                run_repair_pipeline(repo, failure, attr, Some(r), pcfg, tx).await;
            }
        }
        Err(e) => {
            let _ = tx.send(EngineEvent::ReproFailed(
                failure,
                attr,
                describe_repro_error(&e, repro.boot_cmd.as_deref()),
            ));
        }
    }
}

/// R7: a spawn failure surfaced as a bare "io: No such file or directory
/// (os error 2)" names no subject. Say what was being executed — the boot
/// command's program, or the built-in `node` contract when none was given —
/// so the narration tells the operator what to install.
fn describe_repro_error(e: &engine_repro::ReproError, boot_cmd: Option<&str>) -> String {
    match e {
        engine_repro::ReproError::Io(io) if io.kind() == std::io::ErrorKind::NotFound => {
            let program = boot_cmd
                .and_then(|c| c.split_whitespace().next())
                .unwrap_or("node");
            format!("could not run `{program}`: {io} — is it installed and on PATH?")
        }
        other => other.to_string(),
    }
}

/// Hand a reproduced failure to the control plane (`drums watch
/// --dispatch-repairs`).
///
/// Nothing here is fatal. A console that is down, a revoked token, a repository
/// nobody connected — each produces a narrated
/// [`EngineEvent::RepairDispatchFailed`] and the watch carries on. The local
/// loop's value does not depend on the hosted half being up, and taking down
/// observation because a dispatch failed would be the worst possible trade.
async fn dispatch_repair(
    repo: &Path,
    failure: Failure,
    attribution: Attribution,
    reproduction: Reproduction,
    remote: Arc<crate::dispatch::RemoteRepairs>,
    tx: mpsc::UnboundedSender<EngineEvent>,
) {
    // The same criteria the local pipeline hands its agent, built from the same
    // function so the two modes ask for the same thing. The test script is read
    // from the working tree rather than a worktree at the attributed sha: no
    // worktree is created in this mode, and the criteria are advisory to the
    // agent (the verification claims are what decide anything), so an
    // approximately-right name for the suite is worth more than none.
    let acceptance = build_acceptance(
        &failure,
        failure.replayable_request(),
        &read_test_script(repo),
    );

    match remote
        .dispatch(&failure, &attribution, &reproduction, &acceptance)
        .await
    {
        Ok(accepted) => {
            let _ = tx.send(EngineEvent::RepairDispatched(failure, accepted));
        }
        Err(why) => {
            let _ = tx.send(EngineEvent::RepairDispatchFailed(failure, why));
        }
    }
}

fn elapsed_ms(start: Instant) -> u64 {
    start.elapsed().as_millis() as u64
}

fn now_ms() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as u64)
}

/// Best-effort record append: a clock-read or write failure here must not
/// take down the repair pipeline (the user-facing narration is the primary
/// signal; the record is the compliance artifact behind it). Logged, not
/// propagated.
fn append_record(record_path: &Path, kind: &'static str, item: &impl serde::Serialize) {
    let Some(ms) = now_ms() else {
        tracing::error!(
            kind,
            "refusing to append record line: system clock unreadable"
        );
        return;
    };
    if let Err(e) = engine_record::append(record_path, kind, item, ms) {
        tracing::error!(kind, error = %e, "failed to append record line");
    }
}

/// The four-kind mapping (see `crate::notify`): which engine events become a
/// proactive Slack message, and as what. Pure — `(event id, notification)`
/// out, nothing sent — so tests pin the copy and the epistemics without a
/// network. Five events map (a matured revisit only when it DRIFTED);
/// everything else is `None`, deliberately: the terminal narrates the whole
/// pipeline, but a Slack channel gets AT MOST one message per event and only
/// for the moments the four-kind vocabulary names.
///
/// Copy discipline holds here structurally: bodies restate the record's own
/// claims (verdicts stay supported/not supported/inconclusive with their
/// causal-confidence line verbatim) and never the words worked/proved/caused.
/// Nothing request-derived is included at all — service names, shas, rates,
/// ids and the repair's own summary, never a path or a body — which is how
/// "no raw secrets" is enforced rather than remembered. The one free-text
/// field that passes through (the repair summary) is control-stripped via
/// [`crate::render::sanitize`], same as every narrated string.
pub(crate) fn notification_for(
    event: &EngineEvent,
    repo: &str,
) -> Option<(String, crate::notify::Notification)> {
    use crate::notify::{Kind, Notification};
    use crate::render::sanitize;
    match event {
        // The work is done and a human decision is the only remaining step.
        EngineEvent::RepairReady(f, repair, _elapsed_ms) => Some((
            repair.id.clone(),
            Notification {
                kind: Kind::Decision,
                title: format!(
                    "repair ready for {} — review and ship",
                    sanitize(&f.service)
                ),
                body: format!(
                    "{}\napprove and ship: drums ship {}",
                    sanitize(&repair.summary),
                    sanitize(&f.id)
                ),
                repo: repo.to_string(),
            },
        )),
        // A prior bet matured: belief, verdict, and the causal-confidence
        // line verbatim — the same words the record holds, never rounded up.
        EngineEvent::BetEvaluated {
            bet,
            belief,
            verdict,
            measured,
        } => {
            let m = match measured {
                Some((from, to, entries)) => format!("{from:.2} → {to:.2} over {entries}"),
                None => "outcome unmeasured".to_string(),
            };
            Some((
                bet.clone(),
                Notification {
                    kind: Kind::Learning,
                    title: format!(
                        "\"{}\" — {}",
                        sanitize(belief),
                        crate::bet_cmd::support_word(verdict.support)
                    ),
                    body: format!(
                        "{m}\ncausal confidence {}: {}",
                        crate::bet_cmd::level_word(verdict.causal_confidence.level),
                        verdict.causal_confidence.basis
                    ),
                    repo: repo.to_string(),
                },
            ))
        }
        // Something worth investigating, and Drums is on it. Correlation is
        // said as correlation ("after deploy"), exactly like the terminal's
        // own `observed` line.
        EngineEvent::ObservationRecorded(o) => {
            use engine_core::observation::Kind as OKind;
            // "N sessions" only when the source counted them — `None` is
            // unknown, not zero, and the sentence must not flatten that.
            let sessions = o
                .affected
                .sessions
                .map(|s| format!(" across {s} sessions"))
                .unwrap_or_default();
            let (title, body) = match &o.kind {
                OKind::RateShift {
                    previous,
                    since_deploy,
                } => {
                    let title = match since_deploy {
                        Some(sha) => format!(
                            "watching an error-rate shift after deploy {}",
                            crate::render::short_sha(&sanitize(sha))
                        ),
                        None => "watching an error-rate shift".to_string(),
                    };
                    let now_rate = o.measure.map(|m| m.sample.value).unwrap_or(0.0);
                    let n = o.measure.map(|m| m.sample.entries).unwrap_or(0);
                    (
                        title,
                        format!(
                            "error rate {previous:.2}/h → {now_rate:.2}/h over {n} events · {}",
                            sanitize(&o.id.0)
                        ),
                    )
                }
                OKind::RageClick { path, clicks } => (
                    format!(
                        "watching rage clicks on {}",
                        crate::render::frustration_page(path)
                    ),
                    format!("{clicks} rage clicks{sessions} · {}", sanitize(&o.id.0)),
                ),
                OKind::DeadClick { path, clicks } => (
                    format!(
                        "watching dead clicks on {}",
                        crate::render::frustration_page(path)
                    ),
                    format!(
                        "{clicks} clicks that did nothing{sessions} · {}",
                        sanitize(&o.id.0)
                    ),
                ),
                _ => return None,
            };
            Some((
                o.id.0.clone(),
                Notification {
                    kind: Kind::Working,
                    title,
                    body,
                    repo: repo.to_string(),
                },
            ))
        }
        // Only the UNMEASURED outcome is an FYI: it was investigated and
        // nothing needs anyone — the honest sentence says what would change
        // that. A measured outcome is not notified here at all; its story
        // reaches Slack when the bet above it is evaluated.
        EngineEvent::OutcomeMeasured(out) => match &out.outcome {
            engine_core::evaluation::Outcome::Unmeasured(u) => Some((
                out.change.0.clone(),
                Notification {
                    kind: Kind::Fyi,
                    title: format!("nothing needs you — {}", sanitize(&out.change.0)),
                    body: u.sentence(),
                    repo: repo.to_string(),
                },
            )),
            engine_core::evaluation::Outcome::Measured { .. } => None,
        },
        // A matured revisit that DRIFTED is a Learning: a prior bet's metric
        // no longer reads the way the declared window read it. Both readings
        // are stated; the original verdict is never re-labeled. An un-drifted
        // revisit sends nothing — "still what we said" is FYI noise.
        EngineEvent::RevisitMeasured {
            change,
            horizon_days,
            drifted,
            outcome,
            metric,
            was,
        } => {
            if !drifted {
                return None;
            }
            // drifted implies both sides were measured (see
            // `Revisit::drifted_from`), so the readings exist to state.
            let engine_core::evaluation::Outcome::Measured {
                from, to, entries, ..
            } = outcome
            else {
                return None;
            };
            let was_word = was
                .map(crate::render::direction_word)
                .unwrap_or("unmeasured");
            Some((
                format!("revisit:{}:{}d", change, horizon_days),
                Notification {
                    kind: Kind::Learning,
                    title: format!(
                        "a prior bet matured: at {horizon_days} days the metric no longer shows the move the window showed"
                    ),
                    body: format!(
                        "{} · {} {} (was: {} at close)",
                        sanitize(change),
                        metric.label(),
                        crate::render::metric_reading(*metric, *from, *to, *entries),
                        was_word,
                    ),
                    repo: repo.to_string(),
                },
            ))
        }
        // Drums drafted; the human decides. Working, not Decision — the work
        // (the measurement window) has not completed, it is being set up.
        EngineEvent::BetDrafted { bet, belief, .. } => Some((
            bet.clone(),
            Notification {
                kind: Kind::Working,
                title: format!("drafted a bet for your confirmation: {}", sanitize(belief)),
                body: format!(
                    "nothing is committed to until you confirm — drums bet confirm {}",
                    sanitize(bet)
                ),
                repo: repo.to_string(),
            },
        )),
        _ => None,
    }
}

/// The one call site shape for firing a notification beside a `tx.send`:
/// maps, de-duplicates (inside [`crate::notify::Sink::deliver`]) and
/// fire-and-forgets. Doing nothing when no webhook is configured — or when
/// the event is not one of the four — costs one match.
fn notify_event(sink: &Option<crate::notify::Sink>, repo: &str, event: &EngineEvent) {
    let Some(sink) = sink else { return };
    if let Some((id, n)) = notification_for(event, repo) {
        sink.deliver(&id, n);
    }
}

/// The proactive-draft gates, in the order a skip should be reported:
/// consent first (`proactive_draft` in config), then capability (an agent
/// template resolved at startup), then state (no bet already sitting in
/// `proposed` — a second draft on top of an unanswered one is a nag, and the
/// human decides at their own pace). `Some(reason)` is "do not draft, and
/// this is what to log"; `None` means go ahead. Pure, so every gate is
/// testable without an agent, a tick, or a record file.
pub(crate) fn should_draft(
    lines: &[(String, serde_json::Value)],
    proactive_draft: bool,
    agent_available: bool,
) -> Option<&'static str> {
    if !proactive_draft {
        return Some(
            "proactive_draft is off — set proactive_draft = true in .drums/config.toml to let Drums draft bets on its own (it spends your agent's tokens)",
        );
    }
    if !agent_available {
        return Some(
            "no drafting agent — set agent_cmd in .drums/config.toml or DRUMS_AGENT_CMD, or install one of the supported agents (claude, codex, gemini, cursor-agent, amp, opencode)",
        );
    }
    let a_proposed_bet_is_open = engine_core::bet::ProductBet::all(lines.iter())
        .into_iter()
        .any(|b| {
            matches!(
                engine_core::bet::ProductBet::current_status(lines.iter(), &b.id),
                Some(engine_core::bet::BetStatus::Proposed)
            )
        });
    if a_proposed_bet_is_open {
        return Some(
            "a drafted bet is already awaiting confirmation — confirm or decline it (drums bet confirm/decline) before Drums drafts another",
        );
    }
    None
}

/// The same pipeline as `drums draft` (see `main.rs`'s `Commands::Draft`),
/// run against the fresh record: full prompt → the customer's own agent →
/// parse → the SAME `bet_cmd::build` validation gate a human's `drums bet
/// create` goes through → append. Returns `(bet id, belief, agent name)`;
/// every failure — including the agent's own honest "skip" — comes back as
/// the reason string the tick logs at info, never an error.
async fn run_proactive_draft(
    repo: &Path,
    record_path: &Path,
    lines: &[(String, serde_json::Value)],
    template: &str,
) -> Result<(String, String, String), String> {
    let by = template
        .split_whitespace()
        .next()
        .unwrap_or("agent")
        .to_string();
    let prompt = crate::draft::full_prompt(lines);
    let output = crate::draft::run_agent(template, &prompt, repo)
        .await
        .map_err(|r| r.0)?;
    let drafted = crate::draft::parse(&output).map_err(|r| r.0)?;
    let args = crate::draft::to_args(drafted).map_err(|r| r.0)?;
    let now = now_ms().ok_or_else(|| "system clock unreadable".to_string())?;
    let ulid = || ulid::Ulid::new().to_string().to_lowercase();
    let bet_id = format!("bet_{}", ulid());
    let hyp_id = format!("hyp_{}", ulid());
    let eval_id = format!("eval_{}", ulid());
    let (bet, hypothesis) =
        crate::bet_cmd::build(&args, lines, &bet_id, &hyp_id, &eval_id, now).map_err(|r| r.0)?;
    for (kind, value) in crate::bet_cmd::record_lines(&bet, &hypothesis) {
        engine_record::append(record_path, &kind, &value, now)
            .map_err(|e| format!("could not append the drafted bet to the record: {e}"))?;
    }
    Ok((bet.id.0.clone(), bet.belief.clone(), by))
}

/// Repair(if configured) → verify → propose (default) or auto-ship. Runs
/// entirely off the engine loop thread (see the spawn site in `Engine::run`);
/// the only thing it hands back to the loop is an optional `reopen` signal, sent
/// over `pcfg.reopen_tx` rather than mutated in place.
///
/// Entered from two places, and `reproduction` is which one:
/// - `Some(r)` — a replayable (snippet) failure whose reproduction came back
///   `reproduced: true`. The full evidence chain is available.
/// - `None` — a trigger/reported failure whose intake carries no replayable
///   request, so reproduction was SKIPPED rather than attempted. A repair may
///   still be proposed; it just cannot earn a replay claim, and
///   [`ship_decision`] will never let it ship on its own.
async fn run_repair_pipeline(
    repo: PathBuf,
    failure: Failure,
    attribution: Attribution,
    reproduction: Option<Reproduction>,
    pcfg: Option<RepairPipelineCfg>,
    tx: mpsc::UnboundedSender<EngineEvent>,
) {
    let Some(pcfg) = pcfg else {
        // No agent configured at startup (`CliRepairAgent::detect()` found
        // nothing): reproduction alone is still a complete, honest outcome —
        // stay silent rather than emitting a failure for a repair that was
        // never attempted. The renderer's `Reproduced` line is the last word.
        return;
    };

    let start = Instant::now();
    let _ = tx.send(EngineEvent::Repairing(
        failure.clone(),
        pcfg.repair_agent.name().to_string(),
    ));

    // `Failure::replayable_request()` is the only sanctioned way to reach for
    // the captured request: a request being present is not on its own
    // permission to replay it (an OTel adapter can reconstruct a method and a
    // path from span attributes, and that is not the request that failed).
    //
    // `None` is a legitimate, expected state here, not an error — a
    // trigger/reported failure reaches this function deliberately (see the
    // intake fork in `attribute_and_reproduce`) so a repair can still be
    // PROPOSED from the stack trace alone. What changes is the evidence it can
    // earn: `verify_repair` skips the original-request replay and records an
    // `unresolved` claim in its place, while the build/boot, `/health`, and
    // test-suite checks still run and can still be `verified`. Refusing the
    // repair outright would throw away a useful proposal; claiming a replay
    // happened would be the false-`verified` path.
    let original_request = failure.replayable_request().cloned();

    // Step 1: a fresh worktree at the attributed sha — reused for the
    // agent's edits, the repair commit, and verification, so all three see
    // the same checkout.
    let worktree = match ManagedWorktree::create(&repo, &attribution.deploy.sha) {
        Ok(w) => w,
        Err(e) => {
            let _ = tx.send(EngineEvent::RepairFailed(
                failure,
                RepairFailure {
                    why: format!("could not prepare a worktree for the repair: {e}"),
                    worktree: None,
                    branch: None,
                    elapsed_ms: elapsed_ms(start),
                },
            ));
            return;
        }
    };
    let mut worktree = worktree;

    // I1: what the app's own test suite looked like BEFORE the agent touched
    // anything. Read here, while the worktree is still exactly the attributed
    // sha, because `verify_repair`'s test-script check otherwise reads a
    // `package.json` the agent itself may have edited — and an agent that
    // deletes `"test"` from `scripts` would silently turn the strongest gate
    // we have into no gate at all (see `TestScript`).
    let test_script_before = read_test_script(&worktree.dir);

    // n25 (trust-hardening review): the I1 gate above compares `scripts.test`'s
    // TEXT before/after, which an agent can satisfy while hollowing out
    // whatever that unchanged command actually RUNS — a reviewer proved this
    // live, producing an all-verified `RepairReady` with `0-line test script
    // passed [verified]` for a suite that had been emptied. Capturing what the
    // pre-repair run of the SAME script actually did — how many tests it
    // reported, and a fingerprint of the tracked files that look like test
    // files — gives the post-repair check something honest to measure against
    // instead of the command's text alone. Captured here, at the same point as
    // `test_script_before` and for the same reason: after this, the only
    // checkout on disk is the one the agent may have edited.
    let test_baseline = capture_test_baseline(&worktree.dir, &test_script_before).await;

    // Step 2: invoke the agent.
    let acceptance = build_acceptance(&failure, original_request.as_ref(), &test_script_before);
    // What the record remembers: a dispatched runner carries its memory in
    // the instruction (its local record is a throwaway); a watch reads its
    // own record here. Failure to read is honest emptiness, not an error —
    // recall is advice, and a repair must not die for want of it.
    let remembered = if pcfg.remembered.is_empty() {
        engine_record::read_all(&pcfg.record_path)
            .map(|read| {
                engine_recall::for_failure(
                    &read.lines,
                    &failure.signature.error_name,
                    &failure.signature.top_frame_file,
                    now_ms().unwrap_or(0),
                )
            })
            .unwrap_or_default()
    } else {
        pcfg.remembered.clone()
    };
    let ctx = RepairContext {
        failure: failure.clone(),
        attribution: attribution.clone(),
        acceptance,
        remembered,
    };
    let attempt = match pcfg.repair_agent.repair(&worktree.dir, &ctx).await {
        Ok(a) => a,
        Err(e) => {
            worktree.keep_on_drop = true;
            let _ = tx.send(EngineEvent::RepairFailed(
                failure,
                RepairFailure {
                    why: format!("agent could not produce a fix: {e}"),
                    worktree: Some(worktree.dir.display().to_string()),
                    branch: None,
                    elapsed_ms: elapsed_ms(start),
                },
            ));
            return;
        }
    };

    // Step 3: commit the agent's edits on their own branch, off the
    // detached HEAD the worktree was created at; attach a git note with the
    // evidence gathered so far (spec §17 "git is the record").
    // Closing round (F1's audit): `failure.id` is a ULID this process minted
    // (`engine-detect`), so it is ASCII by construction and the old
    // `&failure.id[..8]` byte-slice could not actually panic today. It is
    // shortened through the shared char-safe helper anyway, because the ONE
    // property keeping it safe lives in another crate, and this string is
    // interpolated into a git branch name — a silent panic here would abort a
    // repair that has already been committed.
    let short_id = crate::render::short(&failure.id, 8);
    let branch = format!("drums/repair-{short_id}");
    let commit_sha = match commit_repair(&worktree.dir, &branch, &attempt.summary) {
        Ok(sha) => sha,
        Err(e) => {
            worktree.keep_on_drop = true;
            let _ = tx.send(EngineEvent::RepairFailed(
                failure,
                RepairFailure {
                    why: format!("could not commit the repair: {e}"),
                    worktree: Some(worktree.dir.display().to_string()),
                    branch: None,
                    elapsed_ms: elapsed_ms(start),
                },
            ));
            return;
        }
    };
    let evidence = build_evidence(&failure, &attribution, reproduction.as_ref(), &attempt);
    // Best-effort: a note failing to attach is not itself a repair failure.
    let _ = attach_note(&worktree.dir, &evidence);

    // Step 4: VERIFY — this is where `verified` is earned. Any check failing
    // is the whole verify failing, named specifically; never a partial
    // verified set that hides which check didn't hold.
    let claims = match verify_repair(
        &worktree.dir,
        pcfg.boot_timeout_ms,
        pcfg.boot_cmd.as_deref(),
        original_request.as_ref(),
        &test_script_before,
        test_baseline.as_ref(),
    )
    .await
    {
        Ok(claims) => claims,
        Err(why) => {
            worktree.keep_on_drop = true;
            let _ = tx.send(EngineEvent::RepairFailed(
                failure,
                RepairFailure {
                    why,
                    worktree: Some(worktree.dir.display().to_string()),
                    branch: Some(branch),
                    elapsed_ms: elapsed_ms(start),
                },
            ));
            return;
        }
    };

    let repair = Repair {
        id: ulid::Ulid::new().to_string(),
        failure_id: failure.id.clone(),
        sha: commit_sha,
        branch: branch.clone(),
        agent: pcfg.repair_agent.name().to_string(),
        summary: attempt.summary.clone(),
        diff_stat: attempt.diff_stat.clone(),
        claims,
    };
    append_record(&pcfg.record_path, "repair_ready", &repair);
    // Persisted separately from `repair_ready` (own record kind,
    // `repair_context`) so a standalone `drums ship <id>` — a genuinely
    // separate process reading only `.drums/record.jsonl` — can locate the
    // exact request that was originally failing and replay it against the
    // deployed instance for its own post-deploy verification, without this
    // in-memory pipeline having to hand it anything directly.
    //
    // Fix round (C1, CRITICAL): `failure.sample.request` is the RAW
    // in-memory request by construction (`engine-ingest` redacts only a
    // *copy* for the `event` record line, per its own doc comment, and
    // `engine-detect` stores the raw event as `Failure.sample`) — appending
    // it here verbatim would persist exactly what the plan's redact-at-capture
    // posture, and the invariant `engine-ingest`'s own tests assert
    // (`record must never contain the raw card number`), forbid. This is a
    // SECOND writer of request content into the same compliance record, so
    // it gets the exact same redaction discipline as the first.
    //
    // Written ONLY when a replayable request exists. For a trigger/reported
    // failure the line is simply absent, which `ship::find_repair_sample`
    // already handles honestly (`None` → the post-deploy replay claim is
    // `unresolved`, never guessed). An empty or placeholder `repair_context`
    // would be worse than no line: `drums ship` would replay a request nobody
    // made and record the result as post-deploy verification.
    if let Some(original_request) = &original_request {
        append_record(
            &pcfg.record_path,
            "repair_context",
            &RepairSample {
                failure_id: failure.id.clone(),
                request: redact_for_record(original_request),
            },
        );
    }

    let elapsed = elapsed_ms(start);
    let ready = EngineEvent::RepairReady(failure.clone(), repair.clone(), elapsed);
    notify_event(&pcfg.notify, &pcfg.repo_name, &ready);
    let _ = tx.send(ready);

    // Open the proposal BEFORE the ship decision, deliberately. A repair that
    // ships still deserves a reviewable record of why, and a repair that is
    // withheld needs one even more. Ordering it after the ship would mean the
    // most autonomous path produced the least evidence.
    if let Some(proposer) = &pcfg.proposal {
        let req = engine_propose::ProposalRequest {
            failure: failure.clone(),
            attribution: Some(attribution.clone()),
            reproduction: reproduction.clone(),
            repair: repair.clone(),
            base: pcfg.proposal_base.clone(),
            revert_hint: Some(format!("drums revert {}", failure.id)),
        };
        match proposer.propose(&repo, &req).await {
            Ok(proposal) => {
                let _ = tx.send(EngineEvent::Proposed(failure.clone(), proposal));
            }
            Err(e) => {
                let _ = tx.send(EngineEvent::ProposalFailed(failure.clone(), e.to_string()));
            }
        }
    }

    // Step 5/6: default is propose (stop here). `--repair auto` continues
    // into shipping when a deploy command is configured. Delegates to
    // `crate::ship::ship` (fix round, I4: this used to be a second,
    // near-verbatim copy of the deploy-run/post-deploy-check/parse_method
    // logic, so every fix to that logic — C2, I2, I3 — had to be made
    // twice, and the two copies could silently drift on the exact claim
    // wording that lands in the compliance record). This works because the
    // `repair_ready`/`repair_context` lines this same function just
    // appended above are exactly what `ship::ship` reads to reconstruct the
    // repair and the (redacted) original request — the record-driven path
    // and the in-process auto-ship path are now provably the same code.
    //
    // THE SHIP DECISION POINT. Every path to an unattended deploy goes through
    // `ship_decision` — see `engine/crates/core/src/authority.rs`, which is a
    // SEAM: it moves verbatim to `engine/crates/authority/src/lib.rs` when the
    // `engine-authority` crate lands, and this stays its one call site. The gate
    // it enforces is absolute: a failure whose intake carries no replayable
    // request can never ship on its own, whatever rung its class has earned,
    // because nothing in its chain was ever verified against the request that
    // was actually failing.
    // Effective authority is the LOWER of what the operator consented to and
    // what this class has earned. A flag cannot grant autonomy and a streak
    // cannot bypass consent.
    let class =
        engine_authority::FailureClass::new(&failure.service, &failure.signature.error_name);
    let earned = match engine_authority::Ladder::load(&pcfg.record_path) {
        Ok(l) => l.rung(&class),
        // A record we cannot read is not permission to act alone. Fall back to
        // the safe rung rather than to the operator's ceiling.
        Err(_) => Rung::Propose,
    };
    let effective = min_rung(pcfg.repair_mode.ceiling(), earned);

    // The evidence the gate weighs. Today this pipeline runs the repair
    // IN-PROCESS, so every claim is firsthand and `LocalEvidence` says so —
    // but it goes through the same `Evidence` trait a remote plane will, which
    // is what makes the remote case impossible to add without passing the same
    // check. There is no `ship_decision` signature that omits this argument.
    let evidence = LocalEvidence {
        claims: &repair.claims,
    };

    match ship_decision(effective, &failure.intake, &evidence) {
        ShipDecision::MayShip => {
            if let Some(deploy_cmd) = &pcfg.deploy_cmd {
                match crate::ship::ship(
                    &pcfg.record_path,
                    &repo,
                    &failure.id,
                    deploy_cmd,
                    pcfg.check_url.as_deref(),
                )
                .await
                {
                    Ok(outcome) => {
                        // The ladder only learns from ships, because autonomy
                        // is only ever exercised at the ship. A clean one
                        // builds the streak; anything else demotes on the spot.
                        record_ship_outcome(
                            &pcfg.record_path,
                            &class,
                            engine_authority::Outcome::ShippedClean,
                            &failure.id,
                            &tx,
                        );
                        let _ = tx.send(EngineEvent::Shipped(failure.clone(), outcome));
                        // A bad repair must be re-detectable (spec §22): clear
                        // the detector's opened state for this signature now
                        // that a fix has actually shipped.
                        let _ = pcfg.reopen_tx.send(failure.signature.clone());
                    }
                    Err(e) => {
                        record_ship_outcome(
                            &pcfg.record_path,
                            &class,
                            engine_authority::Outcome::ShipFailed,
                            &failure.id,
                            &tx,
                        );
                        let _ = tx.send(EngineEvent::ShipFailed(failure, e.to_string()));
                    }
                }
            }
        }
        // Stop for a human. `RungBelowActAlone` is the DEFAULT and needs no
        // narration — `RepairReady` (already sent above, with the `drums ship`
        // command) is the last word, byte-identical to what the propose path has
        // always printed. But when the operator asked for act-alone and the
        // intake gate is what refused, say so out loud: a withheld ship is a
        // miss the human must be able to see (spec §13), not silence that looks
        // like a ship that simply hasn't happened yet.
        ShipDecision::Propose(reason) => {
            if pcfg.repair_mode == RepairMode::Auto {
                let _ = tx.send(EngineEvent::ShipWithheld(failure, reason.withheld_text()));
            }
        }
    }
    // `worktree` drops here: removed (keep_on_drop stayed false on every
    // success path — the branch and commit made inside it already persist
    // in the shared repo, so removing the checkout loses nothing).
}

/// Human-readable acceptance criteria (spec §17) the agent must clear, built
/// from the failure itself and from what the app's own `package.json` declared
/// before the agent ran.
///
/// I1: the test-suite line is not decoration. `verify_repair` runs the app's
/// own `scripts.test` and fails the whole verify if it stops being the same
/// runnable script — so the criterion that will actually be enforced has to be
/// stated to the agent, rather than enforced silently against a rule it was
/// never told.
///
/// n25 (trust-hardening review): the prior wording — "do not change, weaken,
/// or remove `scripts.test` in `package.json`" — named the loophole out loud:
/// it told a competent agent exactly which ONE file is watched, and nothing
/// about what that script runs. A reviewer proved live that leaving
/// `scripts.test`'s text untouched and hollowing out what it invokes (a test
/// file, a locally-installed binary) earned an all-`verified` `RepairReady`.
/// The wording now says what `run_package_test_script`'s baseline comparison
/// (`TestBaseline`) actually enforces: the suite itself, not just the command
/// string that launches it.
///
/// `req` is `None` for a trigger/reported failure. The replay criterion is then
/// omitted rather than restated against a request that does not exist — an
/// acceptance criterion nothing will check is worse than no criterion, because
/// the agent optimizes for it and `verify_repair` cannot confirm it. What is
/// stated instead is the honest situation: no captured request, so the fix must
/// come from the stack trace, and the checks that WILL run are the ones listed.
fn build_acceptance(
    failure: &Failure,
    req: Option<&CapturedRequest>,
    test_script_before: &TestScript,
) -> Vec<String> {
    let mut lines = match req {
        Some(req) => vec![format!("{} {} with the captured body returns 2xx (it currently returns a server error)", req.method, req.path)],
        None => vec![format!(
            "there is no captured request for this failure (it was opened by {}), so fix the fault the stack trace points at — no request replay will be run, and none may be assumed",
            failure.intake.label()
        )],
    };
    lines.push("GET /health returns 200".to_string());
    if let TestScript::Declared(script) = test_script_before {
        lines.push(format!(
            "the app's own test suite still passes and still runs at least as many tests as it did before this repair (`scripts.test` = `{script}`) — do not change, weaken, hollow out, or delete `scripts.test` or the test files/binaries it invokes"
        ));
    }
    lines.push("keep the diff minimal; do not reformat unrelated code".to_string());
    lines
}

/// Evidence text for the git note attached to the repair commit: what was
/// known before the repair, plus what the repair claims to have done.
///
/// `reproduction` is `None` for a trigger/reported failure. The note then
/// records the `unresolved` no-replay claim in the reproduction slot instead of
/// leaving the slot out: git is the record (spec §17), and a note that simply
/// omits reproduction reads as "this evidence chain was truncated", not as
/// "reproduction was impossible for this input and here is why".
fn build_evidence(
    failure: &Failure,
    attribution: &Attribution,
    reproduction: Option<&Reproduction>,
    attempt: &engine_repair::RepairAttempt,
) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "failure: {} [{}] (intake: {})\n",
        failure.claim.text,
        failure.claim.provenance.chip(),
        failure.intake.label()
    ));
    s.push_str(&format!(
        "attribution: {} [{}]\n",
        attribution.claim.text,
        attribution.claim.provenance.chip()
    ));
    let repro_claims: Vec<Claim> = match reproduction {
        Some(r) => r.claims.clone(),
        None => vec![failure.intake.no_replay_claim()],
    };
    for c in &repro_claims {
        s.push_str(&format!(
            "reproduction: {} [{}]\n",
            c.text,
            c.provenance.chip()
        ));
    }
    s.push_str(&format!("repair: {}\n", attempt.summary));
    s.push_str(&format!("diff: {}\n", attempt.diff_stat.trim()));
    s
}

fn run_git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Create `branch` from the worktree's current (detached) HEAD, stage
/// everything the agent touched (idempotent whether or not the agent already
/// staged its own changes), and commit. Returns the new commit's sha.
///
/// The identity is stated per-invocation with `-c` rather than assumed from
/// the machine: a laptop always has a global git config, and a CI runner
/// never does. The first repair that ever reproduced in CI died at exactly
/// this line with `Author identity unknown` — after the boot, the replay,
/// the diagnosis and the fix had all succeeded. The name says what the
/// commit is; the address is the product's own domain, and a PR reviewer
/// sees the fix attributed to drums rather than to whoever's leftover CI
/// config happened to be lying around.
fn commit_repair(worktree: &Path, branch: &str, summary: &str) -> Result<String, String> {
    run_git(worktree, &["switch", "-c", branch])?;
    run_git(worktree, &["add", "-A"])?;
    run_git(
        worktree,
        &[
            "-c",
            "user.name=drums",
            "-c",
            "user.email=repairs@drums.sh",
            "commit",
            "-m",
            &format!("repair: {summary}"),
        ],
    )?;
    Ok(run_git(worktree, &["rev-parse", "HEAD"])?
        .trim()
        .to_string())
}

/// Attach a git note (`refs/notes/drums`) to the repair commit — spec §17
/// "git is the record": the evidence travels with the commit itself, not
/// only in the append-only record file.
fn attach_note(worktree: &Path, evidence: &str) -> Result<(), String> {
    // Notes are commit objects too; same identity rule as commit_repair.
    run_git(
        worktree,
        &[
            "-c",
            "user.name=drums",
            "-c",
            "user.email=repairs@drums.sh",
            "notes",
            "--ref=drums",
            "add",
            "-m",
            evidence,
        ],
    )
    .map(|_| ())
}

/// Boot the repair worktree once and replay two requests against the same
/// running instance: the original failing request (must now be non-5xx),
/// then `/health` (must be 200). If `package.json` declares a `test`
/// script, run it too. Any check failing returns the specific reason —
/// never a partial `Ok` that hides which check didn't hold.
///
/// `original_request` is `None` for a trigger/reported failure (spec §9, and
/// [`engine_core::Intake`]): there is no failing request to replay, so that ONE
/// check is replaced by an `unresolved` claim naming what was not done, while
/// every other check still runs and can still earn `verified`. This is the whole
/// shape of "verification claims limited to what was actually executed" — the
/// claim list gets shorter and weaker, never quieter, and no request is ever
/// synthesized to keep the list looking full.
///
/// `test_script_before` is what the SAME checkout's `package.json` declared at
/// the pre-repair sha, read before the agent was invoked — see
/// [`run_package_test_script`] (I1). `test_baseline` is what that same
/// pre-repair run of the script actually DID — see [`TestBaseline`] (n25).
/// Both are parameters rather than something re-derived here on purpose: by
/// the time this function runs, the only checkout on disk is the one the
/// agent may have edited.
async fn verify_repair(
    dir: &Path,
    boot_timeout_ms: u64,
    boot_cmd: Option<&str>,
    original_request: Option<&CapturedRequest>,
    test_script_before: &TestScript,
    test_baseline: Option<&TestBaseline>,
) -> Result<Vec<Claim>, String> {
    let app = BootedApp::boot_with_cmd(dir, boot_timeout_ms, boot_cmd)
        .await
        .map_err(|e| format!("the repaired worktree failed to boot: {e}"))?;

    let mut claims = match original_request {
        Some(original_request) => {
            let (status, _body) =
                app.replay(original_request).await.map_err(|e| format!("could not replay the original failing request: {e}"))?;
            // The acceptance criterion handed to the agent (`build_acceptance`) says
            // 2xx, not merely "not a 5xx" — accepting any non-5xx here let the
            // cheapest possible "fix" (deleting or short-circuiting the route, which
            // typically 404s) earn a `Verified` claim and an all-verified
            // `RepairReady` for an app whose endpoint no longer exists. A fix that
            // legitimately turns a 500 into a 400/422 (e.g. proper input validation)
            // also fails this check and is routed to a human via `RepairFailed`
            // (worktree + branch kept) rather than silently trusted — the same
            // whole-verify-fails-named-specifically discipline every other check in
            // this function already uses; never a partial verified set.
            if !(200..300).contains(&status) {
                return Err(format!("the original failing request still returns {status} (2xx required, not just non-5xx)"));
            }
            vec![Claim { text: format!("original failing request now returns {status}"), provenance: Provenance::Verified }]
        }
        // No replayable request: the strongest check in this function did not
        // run and cannot run, and the claim list says so instead of skipping
        // straight to `/health` and leaving a chain that reads as though the
        // replay had passed. This is an `unresolved` claim, never a `verified`
        // one against a synthesized request — the cardinal sin.
        None => vec![Claim {
            text: "no replayable request captured for this failure — the original request was not replayed against the repair".to_string(),
            provenance: Provenance::Unresolved,
        }],
    };

    let health_req = CapturedRequest {
        method: "GET".to_string(),
        path: "/health".to_string(),
        content_type: None,
        body: None,
    };
    let (health_status, _) = app
        .replay(&health_req)
        .await
        .map_err(|e| format!("could not check /health: {e}"))?;
    if health_status != 200 {
        return Err(format!("/health returned {health_status}, expected 200"));
    }
    claims.push(Claim {
        text: "GET /health returns 200".to_string(),
        provenance: Provenance::Verified,
    });

    // Stop the app before (optionally) running the test script: a test
    // suite that also wants a port should not have to contend with a
    // process still holding one open for the two checks above.
    drop(app);

    if let Some(claim) = run_package_test_script(dir, test_script_before, test_baseline).await? {
        claims.push(claim);
    }
    Ok(claims)
}

/// Bound on `run_package_test_script`'s child. Kept as a named constant (and
/// not inlined into the `tokio::time::timeout` call) so
/// `run_test_script_with_timeout` — the function that actually owns the
/// timeout/process-group logic — can be driven with a short timeout directly
/// from tests, without waiting out the real 120s.
const TEST_SCRIPT_TIMEOUT: Duration = Duration::from_secs(120);

/// What a checkout's `package.json` says about a runnable test suite.
///
/// I1 (review round 5): this exists because the state has to be read at TWO
/// points — once at the pre-repair sha, *before* the agent is invoked, and
/// once from the post-repair worktree — and the difference between them is the
/// only thing that can tell "this app never had tests" apart from "the agent
/// removed the tests it was supposed to pass". Read post-repair only (the
/// prior implementation), an agent that deletes `"test"` from `scripts` or
/// leaves `package.json` unparseable switches off the strongest gate in
/// `verify_repair` and earns the same all-verified `RepairReady` as an app
/// that legitimately has no suite — with no status code left over to give it
/// away, and in `--repair auto` that ships.
#[derive(Debug, Clone, PartialEq)]
enum TestScript {
    /// No `package.json` in the checkout at all: no npm test suite, and never
    /// one. The ONLY state that legitimately produces no claim — see
    /// `run_package_test_script`.
    NoPackageJson,
    /// `package.json` is present but could not be read or parsed, so what it
    /// declares is unknown. Carries the reason, phrased for a claim.
    Unusable(String),
    /// `package.json` parsed but declares no runnable `scripts.test` (absent,
    /// empty, or npm's `no test specified` placeholder).
    NotDeclared(String),
    /// `package.json` declares this (trimmed) test script.
    Declared(String),
}

impl TestScript {
    /// Why no suite ran, phrased for a claim or a failure message. `None` for
    /// [`TestScript::Declared`] — there a suite exists and gets executed.
    fn no_suite_reason(&self) -> Option<&str> {
        match self {
            TestScript::Declared(_) => None,
            TestScript::NoPackageJson => Some("there is no `package.json`"),
            TestScript::Unusable(why) | TestScript::NotDeclared(why) => Some(why),
        }
    }
}

/// Classify `dir`'s `package.json`. Deliberately total: every way this can go
/// wrong becomes a *named* state rather than an early `None`, because the
/// caller has to narrate the degraded cases (the binding "degraded paths are
/// `unresolved`" rule) and compare pre- against post-repair.
fn read_test_script(dir: &Path) -> TestScript {
    let content = match std::fs::read_to_string(dir.join("package.json")) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return TestScript::NoPackageJson,
        Err(e) => {
            return TestScript::Unusable(format!(
                "`package.json` exists but could not be read ({e})"
            ))
        }
    };
    let v = match serde_json::from_str::<serde_json::Value>(&content) {
        Ok(v) => v,
        Err(e) => return TestScript::Unusable(format!("`package.json` could not be parsed ({e})")),
    };
    let Some(script) = v.pointer("/scripts/test").and_then(|s| s.as_str()) else {
        return TestScript::NotDeclared("`package.json` declares no `scripts.test`".to_string());
    };
    let script = script.trim();
    if script.is_empty() {
        return TestScript::NotDeclared("`package.json`'s `scripts.test` is empty".to_string());
    }
    if script.contains("no test specified") {
        return TestScript::NotDeclared(
            "`package.json`'s `scripts.test` is npm's `no test specified` placeholder".to_string(),
        );
    }
    TestScript::Declared(script.to_string())
}

// -- n25: a baseline of what the pre-repair test run actually DID ----------
//
// I1 (above) closes the cheapest bypass — deleting or rewriting
// `scripts.test`'s TEXT — but a reviewer proved live that the bypass simply
// moves one file over: leave the command untouched and hollow out whatever
// it invokes (a `test/*.test.js` file, a locally-installed binary), and the
// pipeline printed `0-line test script passed [verified]` for a suite that
// had actually been emptied. Comparing TEXT can never catch that; only
// comparing what the script's own pre-repair run actually reported, and
// whether its tracked test files still have content, can.

/// A snapshot of the app's own declared test suite taken BEFORE the repair
/// agent is invoked, in the same worktree it's about to edit — what the
/// post-repair run is judged against instead of `scripts.test`'s text alone.
/// `None` (via [`capture_test_baseline`]) when `TestScript::Declared` wasn't
/// true before the agent ran: an app that never had a runnable test script,
/// or where the agent adds one where none existed, has no pre-repair
/// suite-strength to protect — see [`run_package_test_script`]'s only caller.
#[derive(Debug, Clone, Default)]
struct TestBaseline {
    /// `(tests_passed, tests_total)` parsed from the pre-repair run's own
    /// combined stdout+stderr, when [`parse_test_counts`] recognized the
    /// runner's summary-line shape. `None` for an unrecognized runner, OR
    /// when the pre-repair run itself didn't exit 0 (nothing reliable to
    /// compare a post-repair count against, and that pre-existing state is
    /// not this repair's doing) — either way the post-repair count check
    /// degrades to `unresolved` rather than guessing.
    counts: Option<(usize, usize)>,
    /// Tracked files that look like test files (see [`looks_like_test_file`]),
    /// mapped to a content fingerprint taken before the agent ran (see
    /// [`content_digest`]) — enough to tell "still has the content it had
    /// before" apart from "deleted, or hollowed out to nothing", independent
    /// of whether the count above could be parsed at all.
    test_files: BTreeMap<String, u64>,
}

/// Runs `before`'s declared script once, in `dir`, before the repair agent is
/// invoked, and records what actually happened — see [`TestBaseline`]. `None`
/// when `before` is not `TestScript::Declared`: there is nothing to baseline.
///
/// The baseline run's own exit status is deliberately NOT what gates
/// anything here — a pre-existing failing/flaky suite is not this repair's
/// doing, so a baseline run that itself fails just means `counts` stays
/// `None` (an honest "nothing to compare against"), never a reason to refuse
/// the repair before the agent has even started.
async fn capture_test_baseline(dir: &Path, before: &TestScript) -> Option<TestBaseline> {
    let TestScript::Declared(script) = before else {
        return None;
    };
    let test_files = tracked_test_file_digests(dir);
    let counts = match run_test_script_capturing_output(dir, script, TEST_SCRIPT_TIMEOUT).await {
        Ok(output) => parse_test_counts(&format!("{}\n{}", output.stdout, output.stderr)),
        Err(_) => None,
    };
    Some(TestBaseline { counts, test_files })
}

/// Best-effort extraction of `(tests_passed, tests_total)` from a test
/// runner's own combined stdout+stderr summary line. Deliberately narrow:
/// only the shapes emitted by jest, vitest, mocha, and tap/`node --test` are
/// recognized; anything else returns `None` rather than guessing — a guessed
/// count that reads as unchanged when the suite actually shrank would be
/// exactly the false-`verified` this exists to prevent. The caller degrades
/// to an honest `unresolved` claim when this returns `None` on either side of
/// the comparison, never a silent `verified`.
fn parse_test_counts(output: &str) -> Option<(usize, usize)> {
    // jest / vitest summary line, e.g.
    //   "Tests:       3 passed, 3 total"
    //   "Tests:       1 failed, 2 passed, 3 total"
    //   "Tests  3 passed (3)"
    //   "Tests  3 passed | 1 skipped (4)"
    for line in output.lines() {
        let line = line.trim();
        if !(line.starts_with("Tests:") || line.starts_with("Tests ")) {
            continue;
        }
        let mut passed = None;
        let mut total = None;
        let tokens: Vec<&str> = line
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|s| !s.is_empty())
            .collect();
        for w in tokens.windows(2) {
            if let Ok(n) = w[0].parse::<usize>() {
                match w[1] {
                    "passed" => passed = Some(n),
                    "total" => total = Some(n),
                    _ => {}
                }
            }
        }
        // vitest's trailing "(N)" form carries the total when no bare
        // "N total" token is present.
        if total.is_none() {
            if let Some(open) = line.rfind('(') {
                if let Some(close_rel) = line[open + 1..].find(')') {
                    if let Ok(n) = line[open + 1..open + 1 + close_rel].parse::<usize>() {
                        total = Some(n);
                    }
                }
            }
        }
        if let (Some(p), Some(t)) = (passed, total) {
            return Some((p, t));
        }
    }

    // mocha: "  3 passing (12ms)" plus an optional "  1 failing" line — mocha
    // never prints a separate "total", so total = passing + failing.
    let mut passing = None;
    let mut failing = 0usize;
    for line in output.lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        if words.len() >= 2 {
            if let Ok(n) = words[0].parse::<usize>() {
                match words[1] {
                    "passing" => passing = Some(n),
                    "failing" => failing = n,
                    _ => {}
                }
            }
        }
    }
    if let Some(p) = passing {
        return Some((p, p + failing));
    }

    // tap / `node --test`: "# pass 3" and "# tests 3" on their own lines.
    let mut pass = None;
    let mut total = None;
    for line in output.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("# pass ") {
            pass = rest.trim().parse::<usize>().ok();
        } else if let Some(rest) = line.strip_prefix("# tests ") {
            total = rest.trim().parse::<usize>().ok();
        }
    }
    if let (Some(p), Some(t)) = (pass, total) {
        return Some((p, t));
    }

    None
}

/// A deterministic (not cryptographic — collisions are not a threat model
/// here, only "did this change") fingerprint of `bytes`, trimmed of leading
/// and trailing ASCII whitespace first so a trailing-newline-only edit isn't
/// mistaken for content loss, and so a byte-for-byte-empty file and a
/// whitespace-only one collapse to the SAME digest — both are "emptied" for
/// [`find_deleted_or_emptied_test_file`]'s purposes.
fn content_digest(bytes: &[u8]) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let trimmed = {
        let start = bytes
            .iter()
            .position(|b| !b.is_ascii_whitespace())
            .unwrap_or(bytes.len());
        let end = bytes
            .iter()
            .rposition(|b| !b.is_ascii_whitespace())
            .map(|i| i + 1)
            .unwrap_or(0);
        if start < end {
            &bytes[start..end]
        } else {
            &[][..]
        }
    };
    let mut h = DefaultHasher::new();
    trimmed.hash(&mut h);
    h.finish()
}

/// Whether `rel` (a path as `git ls-files` prints it, `/`-separated,
/// repo-root-relative) looks like a test file — a conventional test
/// directory, or a `*.test.*`/`*.spec.*`/`*_test.*`/`*_spec.*` filename.
/// Heuristic and deliberately narrow: this drives WHAT gets fingerprinted for
/// the "no tracked test file was deleted/emptied" check, so a false negative
/// (missing a real test file) only means that file isn't covered by the
/// digest check — the count-based check above still applies independently.
fn looks_like_test_file(rel: &str) -> bool {
    let lower = rel.to_ascii_lowercase();
    let in_test_dir = ["test/", "tests/", "__tests__/", "spec/"]
        .iter()
        .any(|d| lower.starts_with(d) || lower.contains(&format!("/{d}")));
    let test_named_file = [
        "test.js", "test.mjs", "test.cjs", "test.ts", "tests.js", "tests.ts",
    ]
    .iter()
    .any(|f| lower == *f || lower.ends_with(&format!("/{f}")));
    let test_suffixed = [
        ".test.js",
        ".test.mjs",
        ".test.cjs",
        ".test.ts",
        ".test.jsx",
        ".test.tsx",
        ".spec.js",
        ".spec.ts",
        "_test.js",
        "_test.ts",
        "_spec.js",
        "_spec.ts",
    ]
    .iter()
    .any(|suf| lower.ends_with(suf));
    in_test_dir || test_named_file || test_suffixed
}

/// Tracked (`git ls-files`) test files under `dir`, mapped to
/// [`content_digest`] of their current bytes. Empty (never an error) when
/// `dir` isn't a git checkout, `git` isn't available, or nothing matches —
/// callers treat "nothing to compare" as "nothing to enforce", the same
/// fail-open-on-absence posture [`read_test_script`]'s `NoPackageJson` uses.
fn tracked_test_file_digests(dir: &Path) -> BTreeMap<String, u64> {
    let mut map = BTreeMap::new();
    let Ok(out) = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["ls-files"])
        .output()
    else {
        return map;
    };
    if !out.status.success() {
        return map;
    }
    for rel in String::from_utf8_lossy(&out.stdout).lines() {
        if !looks_like_test_file(rel) {
            continue;
        }
        if let Ok(content) = std::fs::read(dir.join(rel)) {
            map.insert(rel.to_string(), content_digest(&content));
        }
    }
    map
}

/// The path of the first tracked test file from `baseline_files` that is now
/// either gone or emptied (its trimmed content hashes the same as empty),
/// `None` if every one of them still has the content it had before the agent
/// ran. A file the agent legitimately ADDED, or changed WITHOUT hollowing it
/// out, is not reported — this is a floor, not a diff.
fn find_deleted_or_emptied_test_file(
    dir: &Path,
    baseline_files: &BTreeMap<String, u64>,
) -> Option<String> {
    if baseline_files.is_empty() {
        return None;
    }
    let empty = content_digest(b"");
    let current = tracked_test_file_digests(dir);
    for (path, &before_digest) in baseline_files {
        match current.get(path) {
            None => return Some(path.clone()),
            Some(&after_digest) if after_digest == empty && before_digest != empty => {
                return Some(path.clone())
            }
            _ => {}
        }
    }
    None
}

/// Turns a completed, exit-0 test-script run into the claim `verify_repair`
/// carries, given the pre-repair baseline (n25) — or the `Err` that fails the
/// WHOLE verify when the run's own evidence says the suite was reduced.
///
/// `baseline` is `None` only when `before` (in [`run_package_test_script`])
/// wasn't `TestScript::Declared` — nothing to protect, so the pre-existing
/// `{n}-line test script passed` claim stands unchanged (this is also the
/// shape a script the agent newly ADDED takes; see n26 in the review, a
/// separate, accepted, narration-only gap this task does not close).
fn claim_from_test_run(
    dir: &Path,
    script: &str,
    output: &TestRunOutput,
    baseline: Option<&TestBaseline>,
) -> Result<Claim, String> {
    let n = output.stdout.lines().count();
    let Some(baseline) = baseline else {
        return Ok(Claim {
            text: format!("{n}-line test script passed"),
            provenance: Provenance::Verified,
        });
    };

    if let Some(hollowed) = find_deleted_or_emptied_test_file(dir, &baseline.test_files) {
        return Err(format!(
            "the repair emptied or deleted the tracked test file `{hollowed}` — the test gate cannot be cleared by hollowing out the tests, even with `scripts.test` unchanged"
        ));
    }

    let combined = format!("{}\n{}", output.stdout, output.stderr);
    match (baseline.counts, parse_test_counts(&combined)) {
        (Some((_, before_total)), Some((after_passed, after_total))) => {
            if after_total < before_total {
                return Err(format!(
                    "the repair reduced the app's own test suite from {before_total} tests to {after_total} — the test gate cannot be cleared by deleting tests, even with `scripts.test` unchanged"
                ));
            }
            Ok(Claim {
                text: format!("the app's own test script passed (`{script}`) — {after_passed}/{after_total} tests, no fewer than the {before_total} it ran before the repair"),
                provenance: Provenance::Verified,
            })
        }
        _ => Ok(Claim {
            text: format!(
                "the app's own test script (`{script}`) exited 0, but its output could not be parsed to count tests — could not confirm the repair did not reduce test coverage"
            ),
            provenance: Provenance::Unresolved,
        }),
    }
}

/// Run the app's own `scripts.test` in the post-repair worktree with a 120s
/// timeout, given what the SAME checkout declared before the agent ran
/// (`before`, read by [`read_test_script`] at the pre-repair sha).
///
/// Three outcomes, and which one you get is the whole point (I1):
/// - the suite ran → a `verified` claim, or the error that fails the whole
///   verify when it exited non-zero;
/// - `before` declared a runnable script and the worktree no longer runs the
///   same one → `Err`, failing the WHOLE verify and naming both, exactly as
///   every other check in `verify_repair` does. Deleting the test, breaking
///   the JSON around it, and rewriting it to `exit 0` are all equally cheap
///   ways to clear a gate, so all three fail closed: the fix is routed to a
///   human with the worktree and branch kept, never trusted silently. (An
///   agent that legitimately needs to change the test command is told not to
///   — `build_acceptance` says so out loud.)
/// - no suite exists to run → `Ok(None)` ONLY when the app has no
///   `package.json` at all; every other "we looked and found nothing runnable"
///   case is an `unresolved` claim that says so, because a check that was
///   expected and did not run is a miss the human must be able to see (§13),
///   not silence indistinguishable from a green one.
///
/// `baseline` (n25) is what the SAME script's pre-repair run actually
/// reported — see [`TestBaseline`] — checked in [`claim_from_test_run`] on
/// top of the above: passing is necessary but no longer sufficient.
async fn run_package_test_script(
    dir: &Path,
    before: &TestScript,
    baseline: Option<&TestBaseline>,
) -> Result<Option<Claim>, String> {
    let after = read_test_script(dir);

    if let TestScript::Declared(before_script) = before {
        match &after {
            TestScript::Declared(after_script) if after_script == before_script => {}
            TestScript::Declared(after_script) => {
                return Err(format!(
                    "the repair rewrote the app's own test script: `package.json` declared `scripts.test` = `{before_script}` at the pre-repair revision and now declares `{after_script}` — the test gate cannot be cleared by changing the test"
                ));
            }
            other => {
                let reason = other
                    .no_suite_reason()
                    .unwrap_or("no runnable test script remains");
                return Err(format!(
                    "the repair removed the app's own test suite: `package.json` declared `scripts.test` = `{before_script}` at the pre-repair revision, and now {reason} — the test gate cannot be cleared by deleting the test"
                ));
            }
        }
    }

    match after {
        TestScript::Declared(script) => {
            let output =
                run_test_script_capturing_output(dir, &script, TEST_SCRIPT_TIMEOUT).await?;
            claim_from_test_run(dir, &script, &output, baseline).map(Some)
        }
        TestScript::NoPackageJson => Ok(None),
        TestScript::Unusable(why) | TestScript::NotDeclared(why) => Ok(Some(Claim {
            text: format!("{why} — no test suite was executed"),
            provenance: Provenance::Unresolved,
        })),
    }
}

/// Runs `test_script` through a shell in `dir` and turns its exit status into
/// a claim (or into the error that fails the WHOLE verify).
///
/// F5 (review round 3): matches the process-group discipline every other
/// child-spawn site in the workspace carries (`repro/src/lib.rs`,
/// `repair/src/lib.rs`, `ship.rs`'s `run_deploy_cmd`) — `sh` becomes the leader
/// of its own new process group and EVERY exit arm `killpg`s that whole group,
/// not just the direct `sh` pid `kill_on_drop` alone would reach. Without it, a
/// test script that backgrounds a worker or boots the service under test (jest
/// workers, `vitest` without `--run`, `node test/boot.js`) leaves that
/// grandchild running past this function returning — and past `drums watch`
/// itself if the whole process is torn down — holding a port or CPU with no
/// record, and a held fixed port then fails every LATER repair for a reason the
/// narration blames on the agent rather than on an orphaned process from an
/// earlier one.
///
/// F7 (review round 4): the wait is on `child.wait()` alone, with the pipe
/// drains started BEFORE it and bounded — see the comments inside. The helpers
/// for both disciplines live in [`crate::proc`], shared with `ship.rs` (n15):
/// two copies of a `libc::kill` `unsafe` block in one crate is the shape that
/// drifts when only one of them is fixed.
///
/// n25: this is the thin claim-building wrapper `run_package_test_script`
/// used before this task, kept unchanged (and still exercised directly by
/// this file's low-level process-mechanics tests below — timeout,
/// process-group kill, drain bounding) so none of that pinned behavior moved.
/// `run_package_test_script`'s own Declared-script arm now calls
/// [`run_test_script_capturing_output`] directly instead, so it can compare
/// the run's stdout/stderr against a [`TestBaseline`] before deciding what
/// claim (if any) to produce — which makes this wrapper test-only in
/// production code, hence `#[cfg(test)]`.
#[cfg(test)]
async fn run_test_script_with_timeout(
    dir: &Path,
    test_script: &str,
    timeout: Duration,
) -> Result<Option<Claim>, String> {
    let output = run_test_script_capturing_output(dir, test_script, timeout).await?;
    let n = output.stdout.lines().count();
    Ok(Some(Claim {
        text: format!("{n}-line test script passed"),
        provenance: Provenance::Verified,
    }))
}

/// A completed test-script run's captured output — `Err` (never returned)
/// covers spawn failure, timeout, and a non-zero exit; those are baked into
/// the `Result` at the call site the same way they always were.
struct TestRunOutput {
    stdout: String,
    stderr: String,
}

/// Runs `test_script` through a shell in `dir` with a timeout, and returns
/// its captured stdout/stderr on a clean exit-0 — see
/// [`run_test_script_with_timeout`] (unchanged wrapper) for why this must go
/// through a shell, and the comments below for the timeout/process-group/
/// drain disciplines this carries forward unchanged from before n25.
async fn run_test_script_capturing_output(
    dir: &Path,
    test_script: &str,
    timeout: Duration,
) -> Result<TestRunOutput, String> {
    // Run it the way `npm test` does: through a shell, with the worktree's
    // own `node_modules/.bin` prepended to PATH. `split_whitespace` +
    // `Command::new(prog)` (the prior implementation) cannot run a real
    // script — `jest --coverage`, `eslint . && jest`, `NODE_ENV=test vitest
    // run` all either ENOENT (locally-installed binaries aren't on PATH
    // without this) or pass shell operators like `&&` through as literal
    // argv elements. This is not the same category as the deploy-command
    // template or the repair-agent invocation (both FORBID a shell because a
    // developer-authored template gets untrusted values substituted into
    // it): `test_script` is not a template plus a substitution, it is
    // exactly the opaque script text `npm test` itself would already run
    // through a shell, in a worktree the pipeline already executes code in
    // (the boot step above already ran the app's own entry point from this
    // checkout).
    let bin_dir = dir.join("node_modules").join(".bin");
    let existing_path = std::env::var_os("PATH").unwrap_or_default();
    let mut search_path = vec![bin_dir];
    search_path.extend(std::env::split_paths(&existing_path));
    let new_path = std::env::join_paths(search_path)
        .map_err(|e| format!("could not build PATH for the test script: {e}"))?;

    let mut cmd = tokio::process::Command::new("sh");
    // stdin is `/dev/null` for the same reason the deploy child's is (round-3
    // R1, `ship.rs`'s `run_deploy_cmd`): a test script that reads stdin —
    // `vitest` without `--run`, a watch-mode runner, anything prompting —
    // would otherwise block on the CLI's inherited stdin and burn the full
    // 120s timeout while showing the operator nothing.
    cmd.arg("-c")
        .arg(test_script)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // Minimal env, mirroring `engine-repair`'s discipline for the agent
    // child: PATH (+ this worktree's node_modules/.bin) and HOME only. The
    // package.json being read is the one the agent just edited, so this
    // executes an agent-authored command — it must never be able to feed
    // telemetry back into the running ingest (e.g. a test script that boots
    // the app under test), which would contaminate the append-only
    // compliance record with events Drums itself caused.
    cmd.env_clear();
    cmd.env("PATH", new_path);
    if let Some(home) = std::env::var_os("HOME") {
        cmd.env("HOME", home);
    }
    cmd.env_remove("DRUMS_INGEST_URL");
    // F5: `sh` becomes the leader of its own new process group — the
    // precondition for `kill_process_group` below to be able to reach any
    // grandchild it backgrounds (jest workers, `vitest` without `--run`,
    // `node test/boot.js`), which plain `kill_on_drop`/`Child::start_kill`
    // (direct-child-only) cannot.
    set_new_process_group(&mut cmd);

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("could not run the test script: {e}"))?;
    let pgid = child.id();
    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");

    // F7 (review round 4): this used to be `child.wait_with_output()`, which
    // returns only when BOTH pipes reach EOF — not when the child exits. A
    // test script that backgrounds anything (`node server.js & jest`, `npm
    // start & npm run test:api`, a runner that daemonizes a helper) hands its
    // inherited stdout/stderr to a process that outlives it, so the wait
    // blocked for the full timeout and this returned "test script timed out
    // after 120s" for a script that had exited 0 in milliseconds — a
    // `RepairFailed` for a good repair, narrated as the agent's fault. Same
    // defect `ship.rs`'s C2 fix removed from `run_deploy_cmd` and
    // `engine-repair` fixed before that; the shape below is `run_deploy_cmd`'s,
    // and it also bounds retention (the drains cap at 256KiB/stream), so a
    // looping agent-authored script can no longer grow this process without
    // limit for up to 120s.
    //
    // Order is load-bearing: the drains start BEFORE anything waits.
    let stdout_buf = Arc::new(std::sync::Mutex::new(Vec::new()));
    let stderr_buf = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut stdout_task = tokio::spawn(drain_into(stdout, stdout_buf.clone()));
    let mut stderr_task = tokio::spawn(drain_into(stderr, stderr_buf.clone()));

    // ONLY the wait carries the timeout — never the drains.
    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(e)) => {
            // The child's own wait failed — its process tree's fate is
            // unknown, so abandon it the same way the timeout arm below does
            // (matches `ship.rs`'s `run_deploy_cmd` wait-error arm).
            if let Some(pgid) = pgid {
                kill_process_group(pgid);
            }
            stdout_task.abort();
            stderr_task.abort();
            return Err(format!("test script failed to run: {e}"));
        }
        Err(_) => {
            if let Some(pgid) = pgid {
                kill_process_group(pgid);
            }
            let _ = child.start_kill();
            stdout_task.abort();
            stderr_task.abort();
            // Name the most likely cause. A test that hangs is almost never
            // slow — it is usually a process that will not EXIT, because
            // something is still holding the event loop open. The commonest
            // shape by far: a test requires the app's entry file, and that
            // file calls `listen()` at module level rather than behind a
            // `require.main === module` guard, so the server starts and never
            // stops. Hit while building a demo fixture; a person seeing only
            // "timed out" has no reason to suspect their own test harness.
            return Err(format!(
                "test script timed out after {}s. A hanging test is usually one that never \
                 EXITS rather than one that is slow — check for a server, interval, or open \
                 handle keeping the process alive (a common cause is the test importing a \
                 file that calls listen() at module level). Run it yourself and see whether \
                 it returns to the prompt.",
                timeout.as_secs_f64()
            ));
        }
    };

    // n14 (round 4): the exit status is known, and UNLIKE a deploy command —
    // which is supposed to leave the service it just started running
    // (`ship.rs`'s policy) — nothing a test script backgrounds should outlive
    // verification. A jest worker or a `node test/boot.js` service left holding
    // a fixed port makes every LATER repair's verification fail for a reason
    // the narration blames on the agent. So the group is killed on the success
    // path too, immediately after the wait (no await in between, so the pgid
    // cannot have been recycled), which also releases any inherited pipe and
    // lets the drains below finish at once instead of waiting out their grace.
    if let Some(pgid) = pgid {
        kill_process_group(pgid);
    }
    let joined = tokio::time::timeout(DRAIN_GRACE, async {
        tokio::join!(&mut stdout_task, &mut stderr_task)
    })
    .await;
    if joined.is_err() {
        stdout_task.abort();
        stderr_task.abort();
    }

    if !status.success() {
        let stderr = take_text(&stderr_buf).trim().to_string();
        let detail = if stderr.is_empty() {
            let stdout = take_text(&stdout_buf).trim().to_string();
            if stdout.is_empty() {
                "it printed nothing on stdout or stderr".to_string()
            } else {
                format!("nothing on stderr; stdout said: {stdout}")
            }
        } else {
            stderr
        };
        return Err(format!("test script `{test_script}` failed: {detail}"));
    }
    Ok(TestRunOutput {
        stdout: take_text(&stdout_buf),
        stderr: take_text(&stderr_buf),
    })
}

/// The redacted `CapturedRequest` persisted in the `repair_context` record
/// line — fix round, C1 (CRITICAL). Applies the exact same masking
/// `engine-ingest`'s `post_event` applies to the `event` record line
/// (`engine_record::redact_body` on the body, `engine_record::redact_query_string`
/// on the path's query string) so this second writer of request content into
/// `.drums/record.jsonl` can never reintroduce the leak that discipline
/// exists to prevent. The RAW request stays available for everything that
/// legitimately needs it in-memory — `verify_repair`'s own replay above, and
/// the repair agent's prompt (`engine-repair::build_prompt`) — only what
/// gets written to the append-only record is masked.
fn redact_for_record(req: &CapturedRequest) -> CapturedRequest {
    CapturedRequest {
        method: req.method.clone(),
        path: engine_record::redact_query_string(&req.path, &[]),
        content_type: req.content_type.clone(),
        body: req
            .body
            .as_deref()
            .map(|b| engine_record::redact_body(req.content_type.as_deref(), b, &[])),
    }
}

/// Record a ship outcome and narrate any demotion it caused.
///
/// A demotion is never silent: a class quietly losing the authority to act
/// alone is as bad as quietly gaining it, and an operator who is not told will
/// keep believing repairs are shipping while they queue up as proposals.
///
/// A failure to WRITE the outcome is also narrated rather than swallowed. The
/// ladder is rebuilt from the record, so a line that never lands means a class
/// keeps authority a rollback should have cost it.
fn record_ship_outcome(
    record_path: &Path,
    class: &engine_authority::FailureClass,
    outcome: engine_authority::Outcome,
    failure_id: &str,
    tx: &mpsc::UnboundedSender<EngineEvent>,
) {
    // A clock that cannot be read is not a reason to skip recording the
    // outcome — but it IS a reason not to invent a timestamp. Fall back to 0,
    // which reads as "unknown" rather than as a plausible time.
    let ts = now_ms().unwrap_or(0);
    match engine_authority::record_outcome(record_path, class, outcome, failure_id, ts) {
        Ok(Some(demotion)) => {
            let _ = tx.send(EngineEvent::Demoted(
                demotion.class.key(),
                demotion.because.clone(),
            ));
        }
        Ok(None) => {}
        Err(e) => {
            let _ = tx.send(EngineEvent::AuthorityWriteFailed(
                class.key(),
                e.to_string(),
            ));
        }
    }
}

/// Evidence for a repair this process ran itself.
///
/// Every claim here was observed in-process: `verify_repair` executed the
/// checks and watched the results. So the eligibility question reduces to
/// provenance — is anything actually `Verified`?
///
/// It still implements the same trait a remote plane's evidence does. That is
/// the point: the gate has one shape, and a future execution plane cannot
/// reach act-alone through a path that skips it.
pub(crate) struct LocalEvidence<'a> {
    pub(crate) claims: &'a [Claim],
}

impl engine_core::authority::Evidence for LocalEvidence<'_> {
    fn has_actionable_verified_claim(&self) -> bool {
        // Provenance is the whole check here. A repair whose claims are all
        // `unresolved` — a timed-out agent, a check that could not run — has
        // established nothing, and nothing is not grounds for an unattended
        // deploy.
        self.claims
            .iter()
            .any(|c| c.provenance == Provenance::Verified)
    }

    fn ineligibility_detail(&self) -> String {
        if self.claims.is_empty() {
            return "the repair produced no claims at all".to_string();
        }
        let unresolved = self
            .claims
            .iter()
            .filter(|c| c.provenance == Provenance::Unresolved)
            .count();
        if unresolved == self.claims.len() {
            return format!(
                "all {unresolved} of the repair's claims are unresolved — nothing was established"
            );
        }
        "nothing in the repair's evidence reached `verified`".to_string()
    }
}

/// Scenario C end to end: repair a human-reported issue, propose it, and
/// answer on the thread the person actually started.
///
/// Every failure here is narrated rather than swallowed, and none is fatal to
/// the others: a repair that cannot be proposed still exists on its branch,
/// and a proposal that cannot be commented on is still a proposal. The
/// ordering reflects that — the cheapest thing to lose goes last.
async fn repair_reported(
    repo: PathBuf,
    issue: ReportedIssue,
    tx: mpsc::UnboundedSender<EngineEvent>,
    pcfg: RepairPipelineCfg,
) {
    let start = std::time::Instant::now();
    let Some(rev) = current_head(&repo) else {
        let _ = tx.send(EngineEvent::ReportedRepairFailed(
            issue,
            "could not read HEAD of the repository".to_string(),
        ));
        return;
    };

    let task = engine_check::IssueTask {
        id: issue.id.clone(),
        source: issue.source.clone(),
        title: issue.title.clone(),
        body: issue.body_excerpt.clone(),
        url: issue.url.clone(),
    };

    let remembered = engine_record::read_all(&pcfg.record_path)
        .map(|read| {
            engine_recall::for_reported(&read.lines, &task.title, &task.body, now_ms().unwrap_or(0))
        })
        .unwrap_or_default();
    let outcome = match engine_check::repair_reported_issue(
        &repo,
        &rev,
        &task,
        pcfg.boot_timeout_ms.max(60_000),
        pcfg.repair_agent.as_ref(),
        remembered,
    )
    .await
    {
        Ok(o) => o,
        Err(e) => {
            let _ = tx.send(EngineEvent::ReportedRepairFailed(issue, e.to_string()));
            return;
        }
    };

    if !outcome.repaired {
        let why = outcome
            .repair_failure
            .unwrap_or_else(|| "the repair did not clear the non-regression bar".to_string());
        let _ = tx.send(EngineEvent::ReportedRepairFailed(issue, why));
        return;
    }

    let claims = outcome
        .verify
        .as_ref()
        .map(|v| v.claims.clone())
        .unwrap_or_default();
    let branch = outcome.branch.clone().unwrap_or_default();
    let _ = tx.send(EngineEvent::ReportedRepairReady(
        issue.clone(),
        branch.clone(),
        claims.clone(),
        elapsed_ms(start),
    ));

    // A proposal, if one is configured. Never required: the branch is the work.
    let mut proposal_url: Option<String> = None;
    if let Some(proposer) = &pcfg.proposal {
        let synthetic = engine_check::synthetic_failure_for_issue(&task);
        let req = engine_propose::ProposalRequest {
            failure: synthetic.clone(),
            // Neither exists for a reported issue, and `None` renders as an
            // absent section rather than an empty one that implies a check ran.
            attribution: None,
            reproduction: None,
            repair: Repair {
                id: format!("issue-{}", issue.id),
                failure_id: synthetic.id.clone(),
                sha: outcome.commit_sha.clone().unwrap_or_default(),
                branch: branch.clone(),
                agent: pcfg.repair_agent.name().to_string(),
                summary: issue.title.clone(),
                diff_stat: String::new(),
                claims: claims.clone(),
            },
            base: pcfg.proposal_base.clone(),
            // No `drums revert` hint: nothing shipped, so there is nothing to
            // revert, and offering the command would imply a deploy happened.
            revert_hint: None,
        };
        match proposer.propose(&repo, &req).await {
            Ok(p) => {
                proposal_url = Some(p.url.clone());
                let _ = tx.send(EngineEvent::Proposed(synthetic, p));
            }
            Err(e) => {
                let _ = tx.send(EngineEvent::ProposalFailed(synthetic, e.to_string()));
            }
        }
    }

    // Answer on the thread. Last, because it is the cheapest thing to lose:
    // failing here costs a notification, not the work.
    // Addressed to the TRACKER'S identifier, never our ULID: `IssueRef::id`
    // is what Linear's commentCreate resolves, and a comment addressed to a
    // ULID posts to nothing. `external_id` exists precisely for this line —
    // its doc-comment names this exact failure. Intake that carried no
    // external id has nothing to write back to, and the record narration is
    // the honest whole of the answer.
    let external = issue.external_id.clone();
    if let (Some(tracker), Some(external_id)) = (engine_track::for_source(&issue.source), external)
    {
        let body = engine_track::render_comment(&claims, proposal_url.as_deref(), true);
        let issue_ref = engine_track::IssueRef {
            id: external_id,
            source: issue.source.clone(),
        };
        match tracker.comment(&issue_ref, &body).await {
            Ok(c) => {
                let _ = tx.send(EngineEvent::ReportedCommented(issue, c.claim));
            }
            Err(e) => {
                let _ = tx.send(EngineEvent::ReportedCommentFailed(issue, e.to_string()));
            }
        }
    }
}

/// `git rev-parse HEAD`, argv-only.
fn current_head(repo: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if sha.is_empty() {
        None
    } else {
        Some(sha)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_core::*;
    use engine_repair::{RepairAttempt, RepairError};
    use engine_repro::{ReproError, Reproducer};
    use std::path::Path;

    struct FakeRepro;
    #[async_trait::async_trait]
    impl Reproducer for FakeRepro {
        async fn reproduce(
            &self,
            _r: &Path,
            _f: &Failure,
            a: &Attribution,
        ) -> Result<Reproduction, ReproError> {
            Ok(Reproduction {
                sha: a.deploy.sha.clone(),
                reproduced: true,
                parent_clean: Some(true),
                detail: "fake".into(),
                claims: vec![Claim {
                    text: "replayed".into(),
                    provenance: Provenance::Verified,
                }],
            })
        }
    }

    /// R7: "io: No such file or directory (os error 2)" narrates a failure
    /// with no subject. The narration must name the program that could not
    /// run — derived from the boot command, never hardcoded.
    #[test]
    fn a_spawn_failure_names_the_program_that_could_not_run() {
        let not_found = ReproError::Io(std::io::Error::from(std::io::ErrorKind::NotFound));

        let with_boot_cmd =
            describe_repro_error(&not_found, Some("uvicorn app.main:app --port {port}"));
        assert!(
            with_boot_cmd.starts_with("could not run `uvicorn`:"),
            "{with_boot_cmd}"
        );
        assert!(
            with_boot_cmd.contains("is it installed and on PATH?"),
            "{with_boot_cmd}"
        );

        let default_contract = describe_repro_error(&not_found, None);
        assert!(
            default_contract.starts_with("could not run `node`:"),
            "the built-in contract boots node: {default_contract}"
        );

        // Every other error keeps its own words — only the subjectless
        // spawn failure is rephrased.
        let boot = ReproError::Io(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
        assert!(
            !describe_repro_error(&boot, None).contains("could not run"),
            "{}",
            describe_repro_error(&boot, None)
        );
    }

    /// A `Reproducer` that signals `entered` the instant it's called, then
    /// parks on `gate` until the test releases it. Lets a test observe "the
    /// engine is mid-reproduction" and control exactly when it completes,
    /// without any wall-clock sleeps.
    struct GatedRepro {
        entered: Arc<tokio::sync::Notify>,
        gate: Arc<tokio::sync::Notify>,
    }
    #[async_trait::async_trait]
    impl Reproducer for GatedRepro {
        async fn reproduce(
            &self,
            _r: &Path,
            _f: &Failure,
            a: &Attribution,
        ) -> Result<Reproduction, ReproError> {
            self.entered.notify_one();
            self.gate.notified().await;
            Ok(Reproduction {
                sha: a.deploy.sha.clone(),
                reproduced: true,
                parent_clean: Some(true),
                detail: "fake".into(),
                claims: vec![Claim {
                    text: "replayed".into(),
                    provenance: Provenance::Verified,
                }],
            })
        }
    }

    fn event(at: u64) -> ErrorEvent {
        ErrorEvent {
            service: "shop".into(),
            occurred_at_ms: at,
            error_name: "TypeError".into(),
            error_message: "m".into(),
            stack: "TypeError: m\n    at computeTotal (/w/shop/server.js:4:2)".into(),
            request: Some(CapturedRequest {
                method: "POST".into(),
                path: "/api/checkout".into(),
                content_type: None,
                body: Some("{}".into()),
            }),
            intake: engine_core::Intake::Snippet,
        }
    }

    /// Same signature as [`event`], arriving from a TRIGGER adapter: no
    /// replayable request at all. This is what an OTel span or a HyperDX log
    /// alert produces.
    fn trigger_event(at: u64, source: &str) -> ErrorEvent {
        ErrorEvent {
            request: None,
            intake: Intake::Trigger {
                source: source.to_string(),
            },
            ..event(at)
        }
    }

    /// A detected snippet failure, for the pure-helper tests.
    fn snippet_failure() -> Failure {
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
            sample: event(1),
            intake: Intake::Snippet,
            claim: Claim {
                text: "3 errors".into(),
                provenance: Provenance::Observed,
            },
        }
    }

    /// The same failure as [`snippet_failure`], opened by a trigger adapter with
    /// no replayable request.
    fn trigger_failure(source: &str) -> Failure {
        let sample = trigger_event(1, source);
        Failure {
            intake: sample.intake.clone(),
            sample,
            ..snippet_failure()
        }
    }

    /// Same shape as [`event`] but with a request carrying a card number and
    /// a token in the body plus a secret in the query string — the exact
    /// shape the C1 review reproduced live against the seeded demo app.
    fn event_with_sensitive_request(at: u64) -> ErrorEvent {
        ErrorEvent {
            service: "shop".into(),
            occurred_at_ms: at,
            error_name: "TypeError".into(),
            error_message: "m".into(),
            stack: "TypeError: m\n    at computeTotal (/w/shop/server.js:4:2)".into(),
            // No query string here: the fixture server's route match is an
            // exact string compare on `req.url` (see `init_repairable_fixture_repo`),
            // so a query string would 404 the in-memory replay `verify_repair`
            // does against the RAW request — a fixture-matching concern
            // unrelated to what this test actually pins (query-string
            // redaction is covered directly by `redact_for_record_masks_the_body_and_the_query_string`).
            request: Some(CapturedRequest {
                method: "POST".into(),
                path: "/api/checkout".into(),
                content_type: Some("application/json".into()),
                body: Some(
                    r#"{"items":[{"sku":"a"}],"card":"4242424242424242","token":"SECRET123"}"#
                        .into(),
                ),
            }),
            intake: engine_core::Intake::Snippet,
        }
    }

    fn init_fixture_repo() -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "t@t"],
            vec!["config", "user.name", "t"],
        ] {
            assert!(std::process::Command::new("git")
                .arg("-C")
                .arg(p)
                .args(&args)
                .status()
                .unwrap()
                .success());
        }
        std::fs::write(p.join("server.js"), "x").unwrap();
        for args in [vec!["add", "."], vec!["commit", "-qm", "c1"]] {
            assert!(std::process::Command::new("git")
                .arg("-C")
                .arg(p)
                .args(&args)
                .status()
                .unwrap()
                .success());
        }
        let sha = String::from_utf8(
            std::process::Command::new("git")
                .arg("-C")
                .arg(p)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        (dir, sha)
    }

    /// A tempdir git repo with one commit carrying a REAL minimal Node
    /// server that always 500s on `/api/checkout` (the failure the repair
    /// pipeline is trying to fix) and always 200s on `/health`. Needed
    /// because the repair pipeline actually boots the worktree it creates
    /// at this sha to verify a fix — unlike `init_fixture_repo`'s "x"
    /// placeholder, which is only ever used with a `Reproducer` fake that
    /// never boots anything.
    fn init_repairable_fixture_repo() -> (tempfile::TempDir, String) {
        const SERVER_JS: &str = r#"
const http = require("http");
const server = http.createServer((req, res) => {
  if (req.url === "/health") { res.writeHead(200); res.end("ok"); return; }
  if (req.url === "/api/checkout") {
    res.writeHead(500, { "content-type": "application/json" });
    res.end(JSON.stringify({ error: { name: "TypeError", message: "boom", stack: "TypeError: boom\n    at computeTotal (server.js:4:2)" } }));
    return;
  }
  res.writeHead(404); res.end();
});
server.listen(process.env.PORT || 0, () => { console.log("listening " + server.address().port); });
"#;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "t@t"],
            vec!["config", "user.name", "t"],
        ] {
            assert!(std::process::Command::new("git")
                .arg("-C")
                .arg(p)
                .args(&args)
                .status()
                .unwrap()
                .success());
        }
        std::fs::write(p.join("server.js"), SERVER_JS).unwrap();
        for args in [vec!["add", "."], vec!["commit", "-qm", "c1"]] {
            assert!(std::process::Command::new("git")
                .arg("-C")
                .arg(p)
                .args(&args)
                .status()
                .unwrap()
                .success());
        }
        let sha = String::from_utf8(
            std::process::Command::new("git")
                .arg("-C")
                .arg(p)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        (dir, sha)
    }

    fn deploy_at(sha: &str, ms: u64) -> DeployRecord {
        DeployRecord {
            sha: sha.to_string(),
            description: "c1".into(),
            author: "t".into(),
            deployed_at_ms: ms,
        }
    }

    fn base_cfg(repo: &Path) -> EngineConfig {
        EngineConfig {
            behavior: None,
            // Test default: no proposal seam and no reported-issue repair.
            // Both paths are covered by their own crates' tests, which need
            // no network and no agent.
            proposal: None,
            proposal_base: "main".to_string(),
            repair_reported: false,
            repo: repo.to_path_buf(),
            threshold: 3,
            window_ms: 60_000,
            app_root: String::new(),
            record_path: repo.join("record.jsonl"),
            repair_agent: None,
            repair_mode: RepairMode::Propose,
            deploy_cmd: None,
            check_url: None,
            repair_boot_timeout_ms: 15_000,
            initial_opened: Vec::new(),
            boot_cmd: None,
            // Test default: no webhook, no proactive drafting — both are
            // opt-in, and their pure halves (`notification_for`,
            // `should_draft`) are covered directly without a network or an
            // agent.
            notify: None,
            proactive_draft: false,
            tracker_poll: None,
            draft_agent: None,
            // Local mode. The hosted seam has its own tests in
            // `crate::dispatch`, which need no engine and no network.
            dispatch: None,
            // Off, the flag's own default. `None` is structurally a no-op —
            // the tick's sync arm never runs — which is exactly the
            // flag-off behaviour; `crate::sync`'s tests cover a `Some`.
            sync: None,
        }
    }

    fn kind_name(e: &EngineEvent) -> &'static str {
        match e {
            EngineEvent::DeployRecorded(_) => "DeployRecorded",
            EngineEvent::FailureDetected(_) => "FailureDetected",
            EngineEvent::Attributed(_, _) => "Attributed",
            EngineEvent::AttributionMissing(_) => "AttributionMissing",
            EngineEvent::AttributionErrored(_, _) => "AttributionErrored",
            EngineEvent::Reproducing(_, _) => "Reproducing",
            EngineEvent::Reproduced(_, _, _) => "Reproduced",
            EngineEvent::ReproFailed(_, _, _) => "ReproFailed",
            EngineEvent::ReproSkippedNotReplayable(_, _, _) => "ReproSkippedNotReplayable",
            EngineEvent::Repairing(_, _) => "Repairing",
            EngineEvent::RepairFailed(_, _) => "RepairFailed",
            EngineEvent::RepairReady(_, _, _) => "RepairReady",
            EngineEvent::Shipped(_, _) => "Shipped",
            EngineEvent::ShipFailed(_, _) => "ShipFailed",
            EngineEvent::Proposed(_, _) => "Proposed",
            EngineEvent::ProposalFailed(_, _) => "ProposalFailed",
            EngineEvent::ShipWithheld(_, _) => "ShipWithheld",
            EngineEvent::Reported(_) => "Reported",
            EngineEvent::Demoted(_, _) => "Demoted",
            EngineEvent::AuthorityWriteFailed(_, _) => "AuthorityWriteFailed",
            EngineEvent::ReportedRepairReady(_, _, _, _) => "ReportedRepairReady",
            EngineEvent::ReportedRepairFailed(_, _) => "ReportedRepairFailed",
            EngineEvent::ReportedCommented(_, _) => "ReportedCommented",
            EngineEvent::ReportedCommentFailed(_, _) => "ReportedCommentFailed",
            EngineEvent::RepairDispatched(_, _) => "RepairDispatched",
            EngineEvent::RepairDispatchFailed(_, _) => "RepairDispatchFailed",
            EngineEvent::ObservationRecorded(_) => "ObservationRecorded",
            EngineEvent::OutcomeMeasured(_) => "OutcomeMeasured",
            EngineEvent::RevisitMeasured { .. } => "RevisitMeasured",
            EngineEvent::BetEvaluated { .. } => "BetEvaluated",
            EngineEvent::BetDrafted { .. } => "BetDrafted",
        }
    }

    /// Restart-idempotence (the difference between a script and a service —
    /// see `EngineConfig::initial_opened`'s doc and `drums_watch::restore`).
    /// Simulates a PRIOR run that already crossed the threshold and shipped
    /// a `repair_ready` line for that signature, then a restart that
    /// rebuilds the detector's gated state from that same record BEFORE the
    /// engine ever observes a live event — the exact seam `drumsd` uses on
    /// every start. Feeding the engine the SAME failing signature again
    /// afterward must NOT start a second repair.
    #[tokio::test]
    async fn restart_does_not_reopen_a_signature_that_already_has_a_repair_ready() {
        let (dir, sha) = init_fixture_repo();
        let p = dir.path();
        let record_path = p.join("record.jsonl");

        fn ev(at: u64) -> ErrorEvent {
            ErrorEvent {
                service: "shop".into(),
                occurred_at_ms: at,
                error_name: "TypeError".into(),
                error_message: "m".into(),
                stack: "TypeError: m\n    at computeTotal (/w/shop/server.js:4:2)".into(),
                request: Some(CapturedRequest {
                    method: "POST".into(),
                    path: "/api/checkout".into(),
                    content_type: None,
                    body: Some("{}".into()),
                }),
                intake: engine_core::Intake::Snippet,
            }
        }

        // The PRIOR run: 3 events crossed the threshold, and a repair_ready
        // line was written for the resulting failure.
        for at in [1_000, 2_000, 3_000] {
            engine_record::append(&record_path, "event", &ev(at), at).unwrap();
        }
        engine_record::append(
            &record_path,
            "repair_ready",
            &Repair {
                id: "r1".into(),
                failure_id: "f1".into(),
                sha: sha.clone(),
                branch: "drums/repair-f1".into(),
                agent: "claude".into(),
                summary: "fixed it".into(),
                diff_stat: String::new(),
                claims: vec![],
            },
            3_500,
        )
        .unwrap();

        // Restart: rebuild the opened-signature set from the record exactly
        // as `drumsd` does on startup, and seed a fresh engine with it.
        let initial_opened = crate::restore::rebuild_opened_signatures(&record_path, 3, 60_000, "");
        assert!(
            !initial_opened.is_empty(),
            "the prior threshold crossing must have been reconstructed from the record"
        );

        let (in_tx, in_rx) = tokio::sync::mpsc::unbounded_channel();
        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut cfg = base_cfg(p);
        cfg.record_path = record_path.clone();
        cfg.initial_opened = initial_opened;
        tokio::spawn(Engine::run(
            cfg,
            in_rx,
            std::sync::Arc::new(FakeRepro),
            ev_tx,
        ));

        in_tx
            .send(engine_ingest::Ingested::Deploy(deploy_at(&sha, 1_000)))
            .unwrap();
        // Feed the SAME failing signature again, post-restart.
        for at in [10_000, 11_000, 12_000] {
            in_tx.send(engine_ingest::Ingested::Error(ev(at))).unwrap();
        }

        let first = tokio::time::timeout(Duration::from_secs(5), ev_rx.recv())
            .await
            .expect("must receive at least the deploy event")
            .expect("channel must stay open for the deploy");
        assert!(
            matches!(first, EngineEvent::DeployRecorded(_)),
            "expected DeployRecorded first, got {first:?}"
        );

        // Give the already-gated events a beat to prove silence rather than
        // racing a `FailureDetected` that must never arrive.
        let second = tokio::time::timeout(Duration::from_millis(500), ev_rx.recv()).await;
        assert!(second.is_err(), "the already-opened signature must not start a second repair after a restart, got {second:?}");
    }

    /// deploy → 3 errors → detect → attribute → (fake) reproduce, in order.
    #[tokio::test]
    async fn pipeline_emits_ordered_events() {
        let (dir, sha) = init_fixture_repo();
        let p = dir.path();

        let (in_tx, in_rx) = tokio::sync::mpsc::unbounded_channel();
        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
        let cfg = base_cfg(p);
        tokio::spawn(Engine::run(
            cfg,
            in_rx,
            std::sync::Arc::new(FakeRepro),
            ev_tx,
        ));

        in_tx
            .send(engine_ingest::Ingested::Deploy(deploy_at(&sha, 1_000)))
            .unwrap();
        for at in [2_000, 3_000, 4_000] {
            in_tx
                .send(engine_ingest::Ingested::Error(event(at)))
                .unwrap();
        }

        let mut kinds = Vec::new();
        for _ in 0..5 {
            match tokio::time::timeout(Duration::from_secs(5), ev_rx.recv()).await {
                Ok(Some(e)) => kinds.push(kind_name(&e)),
                _ => break,
            }
        }
        assert_eq!(
            kinds,
            vec![
                "DeployRecorded",
                "FailureDetected",
                "Attributed",
                "Reproducing",
                "Reproduced"
            ],
            "pipeline must emit these events in exactly this order"
        );
    }

    /// With no repair agent configured, reproduction alone is the honest,
    /// terminal outcome — no `Repairing`/`RepairFailed` noise for a repair
    /// that was never attempted.
    #[tokio::test]
    async fn no_repair_agent_configured_stops_cleanly_after_reproduction() {
        let (dir, sha) = init_fixture_repo();
        let p = dir.path();

        let (in_tx, in_rx) = tokio::sync::mpsc::unbounded_channel();
        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
        let cfg = base_cfg(p); // repair_agent: None
        tokio::spawn(Engine::run(
            cfg,
            in_rx,
            std::sync::Arc::new(FakeRepro),
            ev_tx,
        ));

        in_tx
            .send(engine_ingest::Ingested::Deploy(deploy_at(&sha, 1_000)))
            .unwrap();
        for at in [2_000, 3_000, 4_000] {
            in_tx
                .send(engine_ingest::Ingested::Error(event(at)))
                .unwrap();
        }

        let mut saw_reproduced = false;
        loop {
            match tokio::time::timeout(Duration::from_secs(5), ev_rx.recv()).await {
                Ok(Some(EngineEvent::Reproduced(..))) => {
                    saw_reproduced = true;
                    break;
                }
                Ok(Some(_)) => continue,
                _ => break,
            }
        }
        assert!(saw_reproduced);
        // Nothing further arrives within a short grace window.
        let extra = tokio::time::timeout(Duration::from_millis(300), ev_rx.recv()).await;
        assert!(extra.is_err(), "no repair-related event should follow Reproduced when no agent is configured, got {extra:?}");
    }

    // -- the hosted seam (`drums watch --dispatch-repairs`) -------------------

    /// A `Reproducer` that completes and reports the failure did NOT happen
    /// again at the attributed revision. The pipeline's most important negative.
    struct DidNotReproduce;
    #[async_trait::async_trait]
    impl Reproducer for DidNotReproduce {
        async fn reproduce(
            &self,
            _r: &Path,
            _f: &Failure,
            a: &Attribution,
        ) -> Result<Reproduction, ReproError> {
            Ok(Reproduction {
                sha: a.deploy.sha.clone(),
                reproduced: false,
                parent_clean: Some(true),
                detail: "the replay returned 200".into(),
                claims: vec![Claim {
                    text: "could not make the failure happen again".into(),
                    provenance: Provenance::Unresolved,
                }],
            })
        }
    }

    /// A dispatcher pointed at a port nothing listens on. Loopback rather than
    /// a blackhole address so a request that DOES leave this process is refused
    /// immediately — the tests below assert on which events arrived, and a
    /// 20-second connect timeout would make "nothing arrived" indistinguishable
    /// from "it is still trying".
    fn unreachable_dispatch(record_path: PathBuf) -> Arc<crate::dispatch::RemoteRepairs> {
        Arc::new(
            crate::dispatch::RemoteRepairs::new(
                "http://127.0.0.1:1",
                "drums_pat_never_sent",
                "acme/api",
                "main",
                record_path,
                Rung::ActAlone,
            )
            .expect("the client must build"),
        )
    }

    /// THE product property, at the engine's own fork rather than inside the
    /// dispatcher: a failure that did not reproduce produces NO dispatch at
    /// all — not a refused one, not a failed one. Nothing is asked of the
    /// control plane, because there is nothing to ask about.
    ///
    /// A `RepairDispatchFailed` here would still be a bug: it would mean the
    /// engine tried, and the only thing stopping the repair was the network.
    #[tokio::test]
    async fn a_failure_that_did_not_reproduce_is_never_dispatched_by_the_engine() {
        let (dir, sha) = init_fixture_repo();
        let p = dir.path();

        let (in_tx, in_rx) = tokio::sync::mpsc::unbounded_channel();
        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut cfg = base_cfg(p);
        cfg.dispatch = Some(unreachable_dispatch(p.join("record.jsonl")));
        tokio::spawn(Engine::run(cfg, in_rx, Arc::new(DidNotReproduce), ev_tx));

        in_tx
            .send(engine_ingest::Ingested::Deploy(deploy_at(&sha, 1_000)))
            .unwrap();
        for at in [2_000, 3_000, 4_000] {
            in_tx
                .send(engine_ingest::Ingested::Error(event(at)))
                .unwrap();
        }

        let mut kinds = Vec::new();
        while let Ok(Some(e)) = tokio::time::timeout(Duration::from_millis(800), ev_rx.recv()).await
        {
            kinds.push(kind_name(&e));
        }
        assert_eq!(
            kinds,
            vec![
                "DeployRecorded",
                "FailureDetected",
                "Attributed",
                "Reproducing",
                "Reproduced"
            ],
            "the pipeline must stop at Reproduced — a repair is only ever attempted against a \
             failure Drums made happen again"
        );
    }

    /// A trigger intake never had a request to replay, so reproduction is
    /// SKIPPED rather than attempted — and a skipped reproduction is not a
    /// reproduction. Nothing is dispatched, and (with no local agent) nothing
    /// is repaired here either.
    #[tokio::test]
    async fn a_trigger_intake_is_never_dispatched() {
        let (dir, sha) = init_fixture_repo();
        let p = dir.path();

        let (in_tx, in_rx) = tokio::sync::mpsc::unbounded_channel();
        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut cfg = base_cfg(p);
        cfg.dispatch = Some(unreachable_dispatch(p.join("record.jsonl")));
        tokio::spawn(Engine::run(cfg, in_rx, Arc::new(FakeRepro), ev_tx));

        in_tx
            .send(engine_ingest::Ingested::Deploy(deploy_at(&sha, 1_000)))
            .unwrap();
        for at in [2_000, 3_000, 4_000] {
            in_tx
                .send(engine_ingest::Ingested::Error(trigger_event(at, "hyperdx")))
                .unwrap();
        }

        let mut kinds = Vec::new();
        while let Ok(Some(e)) = tokio::time::timeout(Duration::from_millis(800), ev_rx.recv()).await
        {
            kinds.push(kind_name(&e));
        }
        assert!(
            !kinds.iter().any(|k| k.starts_with("RepairDispatch")),
            "a failure with nothing to replay must never reach the control plane: {kinds:?}"
        );
        assert!(kinds.contains(&"ReproSkippedNotReplayable"), "{kinds:?}");
    }

    /// A console that cannot be reached is narrated and survived. This is the
    /// property that makes hosted mode safe to turn on: the local loop's value
    /// does not depend on the hosted half being up, and a dispatch failing must
    /// never take down observation.
    #[tokio::test]
    async fn an_unreachable_console_is_narrated_and_watching_continues() {
        let (dir, sha) = init_fixture_repo();
        let p = dir.path();

        let (in_tx, in_rx) = tokio::sync::mpsc::unbounded_channel();
        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut cfg = base_cfg(p);
        cfg.dispatch = Some(unreachable_dispatch(p.join("record.jsonl")));
        tokio::spawn(Engine::run(cfg, in_rx, Arc::new(FakeRepro), ev_tx));

        in_tx
            .send(engine_ingest::Ingested::Deploy(deploy_at(&sha, 1_000)))
            .unwrap();
        for at in [2_000, 3_000, 4_000] {
            in_tx
                .send(engine_ingest::Ingested::Error(event(at)))
                .unwrap();
        }

        let mut why = None;
        loop {
            match tokio::time::timeout(Duration::from_secs(40), ev_rx.recv()).await {
                Ok(Some(EngineEvent::RepairDispatchFailed(_, w))) => {
                    why = Some(w);
                    break;
                }
                Ok(Some(_)) => continue,
                _ => break,
            }
        }
        let why = why.expect("an unreachable console must be narrated, never swallowed");
        assert!(
            why.contains("127.0.0.1:1"),
            "the reason must name what could not be reached: {why}"
        );
        assert!(
            !why.contains("drums_pat_"),
            "no narration may ever carry the credential: {why}"
        );

        // And the loop is still running: a later deploy is still observed.
        let later = deploy_at("cafebabe", 9_000);
        in_tx
            .send(engine_ingest::Ingested::Deploy(later.clone()))
            .unwrap();
        loop {
            match tokio::time::timeout(Duration::from_secs(5), ev_rx.recv()).await {
                Ok(Some(EngineEvent::DeployRecorded(d))) if d.sha == later.sha => break,
                Ok(Some(_)) => continue,
                other => panic!("watching must continue after a failed dispatch, got {other:?}"),
            }
        }
    }

    /// A reproduction that hasn't returned yet must not stall the recv loop:
    /// a deploy arriving while an earlier failure's reproduction is still
    /// parked must still be observed (as `DeployRecorded`) before that
    /// reproduction completes.
    #[tokio::test]
    async fn slow_reproduction_does_not_block_ingestion() {
        let (dir, sha_a) = init_fixture_repo();
        let p = dir.path();

        let entered = Arc::new(tokio::sync::Notify::new());
        let gate = Arc::new(tokio::sync::Notify::new());
        let repro = Arc::new(GatedRepro {
            entered: entered.clone(),
            gate: gate.clone(),
        });

        let (in_tx, in_rx) = tokio::sync::mpsc::unbounded_channel();
        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
        let cfg = base_cfg(p);
        tokio::spawn(Engine::run(cfg, in_rx, repro, ev_tx));

        // Deploy A, then 3 errors: opens a failure whose reproduction parks
        // on `gate` (via GatedRepro).
        in_tx
            .send(engine_ingest::Ingested::Deploy(deploy_at(&sha_a, 1_000)))
            .unwrap();
        for at in [2_000, 3_000, 4_000] {
            in_tx
                .send(engine_ingest::Ingested::Error(event(at)))
                .unwrap();
        }

        // Deterministically wait until reproduction has actually started
        // (and is parked on the gate) before doing anything else.
        tokio::time::timeout(Duration::from_secs(5), entered.notified())
            .await
            .expect("reproduction should have started (entered GatedRepro::reproduce)");

        // Deploy B arrives while reproduction A is still parked on the gate.
        let deploy_b = deploy_at("deadbeef", 5_000);
        in_tx
            .send(engine_ingest::Ingested::Deploy(deploy_b.clone()))
            .unwrap();

        // Drain events until we see DeployRecorded(B). If ingestion were
        // blocked on the parked reproduction, this recv would time out
        // (the loop would never advance past the parked failure) rather
        // than ever observing a Reproduced event out of turn.
        let mut saw_deploy_b = false;
        loop {
            match tokio::time::timeout(Duration::from_secs(5), ev_rx.recv()).await {
                Ok(Some(EngineEvent::DeployRecorded(d))) if d.sha == deploy_b.sha => {
                    saw_deploy_b = true;
                    break;
                }
                Ok(Some(EngineEvent::Reproduced(..))) => {
                    panic!("Reproduced arrived before DeployRecorded(B) — reproduction should still be parked on the gate");
                }
                Ok(Some(_)) => continue,
                _ => break,
            }
        }
        assert!(
            saw_deploy_b,
            "DeployRecorded(B) must be observed while reproduction A is still parked"
        );

        // Release the gate; the parked reproduction now completes.
        gate.notify_one();
        let mut saw_reproduced = false;
        loop {
            match tokio::time::timeout(Duration::from_secs(5), ev_rx.recv()).await {
                Ok(Some(EngineEvent::Reproduced(..))) => {
                    saw_reproduced = true;
                    break;
                }
                Ok(Some(_)) => continue,
                _ => break,
            }
        }
        assert!(
            saw_reproduced,
            "Reproduced must arrive after the gate is released"
        );
    }

    fn sample_reported_issue() -> ReportedIssue {
        ReportedIssue {
            id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
            source: "agentation".into(),
            external_id: Some("agentation-42".into()),
            external_identifier: None,
            title: "button misaligned on checkout".into(),
            body_excerpt: "overlaps the price on mobile".into(),
            url: Some("https://agentation.example/i/42".into()),
            payload: serde_json::json!({"element": "#submit-btn", "page": "/checkout"}),
            claim: Claim {
                text: "reported via agentation webhook".into(),
                provenance: Provenance::Observed,
            },
        }
    }

    /// A `Reported` intake item must produce exactly one `EngineEvent::Reported`
    /// and nothing else — no attribution, reproduction, or repair pipeline
    /// entry (real-world-scenarios plan, Scenario C item 1: intake only).
    #[tokio::test]
    async fn reported_item_yields_only_a_reported_event_never_enters_the_pipeline() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        std::process::Command::new("git")
            .arg("-C")
            .arg(p)
            .args(["init", "-q"])
            .status()
            .unwrap();

        let (in_tx, in_rx) = tokio::sync::mpsc::unbounded_channel();
        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
        let cfg = base_cfg(p);
        tokio::spawn(Engine::run(
            cfg,
            in_rx,
            std::sync::Arc::new(FakeRepro),
            ev_tx,
        ));

        let issue = sample_reported_issue();
        in_tx.send(Ingested::Reported(issue.clone())).unwrap();

        let got = tokio::time::timeout(Duration::from_secs(5), ev_rx.recv())
            .await
            .expect("must not time out")
            .expect("channel must not close");
        match got {
            EngineEvent::Reported(r) => {
                assert_eq!(r.id, issue.id);
                assert_eq!(r.source, "agentation");
            }
            other => panic!("expected EngineEvent::Reported, got {other:?}"),
        }

        // Nothing further arrives within a short grace window — no
        // FailureDetected/Attributed/Repairing/etc. was ever spawned for it.
        let extra = tokio::time::timeout(Duration::from_millis(300), ev_rx.recv()).await;
        assert!(
            extra.is_err(),
            "a Reported item must never lead to any further pipeline event, got {extra:?}"
        );
    }

    // -- Repair pipeline: fake RepairAgent implementations ----------------

    /// Applies a real, working fix to `server.js` in the worktree (removes
    /// the 500 branch on `/api/checkout`) and stages nothing itself —
    /// `run_repair_pipeline`'s own `git add -A` in `commit_repair` must
    /// pick it up regardless.
    struct FixingAgent;
    #[async_trait::async_trait]
    impl RepairAgent for FixingAgent {
        async fn repair(
            &self,
            worktree: &Path,
            _ctx: &RepairContext,
        ) -> Result<RepairAttempt, RepairError> {
            const FIXED_SERVER_JS: &str = r#"
const http = require("http");
const server = http.createServer((req, res) => {
  if (req.url === "/health") { res.writeHead(200); res.end("ok"); return; }
  if (req.url === "/api/checkout") { res.writeHead(200, { "content-type": "application/json" }); res.end("{}"); return; }
  res.writeHead(404); res.end();
});
server.listen(process.env.PORT || 0, () => { console.log("listening " + server.address().port); });
"#;
            std::fs::write(worktree.join("server.js"), FIXED_SERVER_JS).map_err(RepairError::Io)?;
            Ok(RepairAttempt {
                summary: "fixed the checkout 500".to_string(),
                diff_stat: "server.js | 4 +---".to_string(),
            })
        }
        fn name(&self) -> &str {
            "fake-fixing-agent"
        }
    }

    /// Edits an UNRELATED file (never touches the actual 500 bug) so the
    /// diff is real (non-empty `git status`) but verification must still
    /// fail — the shape of "the agent tried, the fix didn't work".
    struct WrongFixAgent;
    #[async_trait::async_trait]
    impl RepairAgent for WrongFixAgent {
        async fn repair(
            &self,
            worktree: &Path,
            _ctx: &RepairContext,
        ) -> Result<RepairAttempt, RepairError> {
            std::fs::write(worktree.join("NOTES.md"), "attempted a fix\n")
                .map_err(RepairError::Io)?;
            Ok(RepairAttempt {
                summary: "left a note".to_string(),
                diff_stat: "NOTES.md | 1 +".to_string(),
            })
        }
        fn name(&self) -> &str {
            "fake-wrong-fix-agent"
        }
    }

    struct FailingAgent;
    #[async_trait::async_trait]
    impl RepairAgent for FailingAgent {
        async fn repair(
            &self,
            _worktree: &Path,
            _ctx: &RepairContext,
        ) -> Result<RepairAttempt, RepairError> {
            Err(RepairError::NoChanges)
        }
        fn name(&self) -> &str {
            "fake-failing-agent"
        }
    }

    /// Grant `shop/TypeError` the act-alone rung, the way a human would with
    /// `drums authority promote`.
    ///
    /// Needed by every auto-ship test now that the ladder is real: `--repair
    /// auto` is CONSENT, not a grant, so a class that has earned nothing still
    /// proposes. These tests are about ship MECHANICS (argv handling, sha
    /// substitution, the intake gate), so they promote first and then assert
    /// on the ship — rather than asserting a ship that the product would
    /// correctly refuse.
    fn promote_test_class(repo: &Path) {
        // Must match `base_cfg`'s record_path exactly — the ladder is folded
        // from the SAME file the engine writes, and promoting into a different
        // one would silently do nothing.
        let record = repo.join("record.jsonl");
        engine_authority::promote(&record, "shop/TypeError", 1).expect("promote");
    }

    fn cfg_with_agent(repo: &Path, agent: Arc<dyn RepairAgent>) -> EngineConfig {
        EngineConfig {
            repair_agent: Some(agent),
            ..base_cfg(repo)
        }
    }

    /// Full pipeline: real fix, real boot+replay verification, all-verified
    /// claims, `RepairReady` — the primary required test.
    #[tokio::test]
    async fn full_pipeline_repairs_and_verifies_to_repair_ready() {
        let (dir, sha) = init_repairable_fixture_repo();
        let p = dir.path();

        let (in_tx, in_rx) = tokio::sync::mpsc::unbounded_channel();
        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
        let cfg = cfg_with_agent(p, Arc::new(FixingAgent));
        tokio::spawn(Engine::run(
            cfg,
            in_rx,
            std::sync::Arc::new(FakeRepro),
            ev_tx,
        ));

        in_tx
            .send(Ingested::Deploy(deploy_at(&sha, 1_000)))
            .unwrap();
        for at in [2_000, 3_000, 4_000] {
            in_tx.send(Ingested::Error(event(at))).unwrap();
        }

        let mut repairing_seen = false;
        let mut ready: Option<(Failure, Repair, u64)> = None;
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_secs(30), ev_rx.recv()).await {
                Ok(Some(EngineEvent::Repairing(_, agent))) => {
                    assert_eq!(agent, "fake-fixing-agent");
                    repairing_seen = true;
                }
                Ok(Some(EngineEvent::RepairReady(f, r, ms))) => {
                    ready = Some((f, r, ms));
                    break;
                }
                Ok(Some(EngineEvent::RepairFailed(_, detail))) => {
                    panic!("expected RepairReady, got RepairFailed: {}", detail.why)
                }
                Ok(Some(_)) => continue,
                Ok(None) | Err(_) => break,
            }
        }

        assert!(
            repairing_seen,
            "Repairing must be emitted before the outcome"
        );
        let (failure, repair, _elapsed_ms) = ready.expect("expected RepairReady");
        assert_eq!(repair.failure_id, failure.id);
        assert_eq!(repair.agent, "fake-fixing-agent");
        assert!(repair.branch.starts_with("drums/repair-"));
        assert!(!repair.sha.is_empty());
        assert!(
            repair.claims.len() >= 2,
            "must carry at least the two mandatory verify claims: {:?}",
            repair.claims
        );
        assert!(
            repair
                .claims
                .iter()
                .all(|c| c.provenance == Provenance::Verified),
            "every claim on a RepairReady repair must be verified: {:?}",
            repair.claims
        );
        assert!(repair.claims.iter().any(|c| c.text.contains("now returns")));
        assert!(repair.claims.iter().any(|c| c.text.contains("/health")));

        // The branch and commit exist in the ORIGINAL repo (not just the
        // now-removed worktree) — spec §17 "git is the record".
        let branches = std::process::Command::new("git")
            .arg("-C")
            .arg(p)
            .args(["branch", "--list", &repair.branch])
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&branches.stdout).contains(&repair.branch),
            "the repair branch must exist in the origin repo"
        );
        let notes = std::process::Command::new("git")
            .arg("-C")
            .arg(p)
            .args(["notes", "--ref=drums", "show", &repair.sha])
            .output()
            .unwrap();
        assert!(
            notes.status.success(),
            "a git note must be attached to the repair commit"
        );
        assert!(
            String::from_utf8_lossy(&notes.stdout).contains("failure:"),
            "the note must carry evidence"
        );

        // record.jsonl carries the repair_ready line.
        let record = std::fs::read_to_string(p.join("record.jsonl")).unwrap_or_default();
        assert!(
            record.contains("\"kind\":\"repair_ready\""),
            "record.jsonl must carry a repair_ready line: {record}"
        );
        assert!(record.contains(&repair.branch));
        // ...and a repair_context line carrying the original captured
        // request, keyed by failure_id — this is what lets a standalone
        // `drums ship <id>` (Task 4), which only has the record to work
        // from, replay the exact request that was originally failing.
        assert!(
            record.contains("\"kind\":\"repair_context\""),
            "record.jsonl must carry a repair_context line: {record}"
        );
        assert!(
            record.contains(&failure.id),
            "the repair_context line must be keyed by the failure id: {record}"
        );
        assert!(
            record.contains("/api/checkout"),
            "the repair_context line must carry the original request path: {record}"
        );
    }

    /// **The free path.** Everything up to and including a verified, proposed
    /// repair is free forever; the only paid capability is act-alone, and the
    /// only gate is `drums authority promote`.
    ///
    /// This test exists because that promise is the easy one to break by
    /// accident. It asserts three things, in order of how badly each would
    /// hurt if it stopped being true:
    ///
    /// 1. With NO license anywhere — the state of essentially every machine
    ///    that runs Drums — `drums watch`'s propose path completes: detect,
    ///    attribute, reproduce, repair, verify, propose, all-verified claims,
    ///    a `repair_ready` line in the record and a branch to ship.
    /// 2. License state genuinely varies here (a real minted key, a real
    ///    expired one, none at all) and NONE of it changes a byte of what
    ///    that path produced. The test is not asserting against a constant:
    ///    the statuses are built by the real verifier and checked to differ
    ///    before anything is compared.
    /// 3. Nothing on the free path mentions money. A user who never pays us
    ///    must be able to run this loop forever without being sold to by the
    ///    terminal.
    #[tokio::test]
    async fn the_propose_path_is_free_and_identical_whatever_the_license_says() {
        use crate::license::{self, License, LicenseStatus};

        // -- a real, verified repair, produced with no license installed ----
        let (dir, sha) = init_repairable_fixture_repo();
        let p = dir.path();

        let (in_tx, in_rx) = tokio::sync::mpsc::unbounded_channel();
        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
        // Propose mode: exactly what `drums watch` does by default.
        let cfg = cfg_with_agent(p, Arc::new(FixingAgent));
        assert_eq!(cfg.repair_mode, RepairMode::Propose);
        tokio::spawn(Engine::run(
            cfg,
            in_rx,
            std::sync::Arc::new(FakeRepro),
            ev_tx,
        ));

        in_tx
            .send(Ingested::Deploy(deploy_at(&sha, 1_000)))
            .unwrap();
        for at in [2_000, 3_000, 4_000] {
            in_tx.send(Ingested::Error(event(at))).unwrap();
        }

        let render_ctx = crate::render::RenderContext {
            repo: p.to_path_buf(),
            deploy_cmd: None,
        };
        let mut narration = String::new();
        let mut ready: Option<Repair> = None;
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_secs(30), ev_rx.recv()).await {
                Ok(Some(ev)) => {
                    narration.push_str(&crate::render::render(&ev, &render_ctx));
                    if let EngineEvent::RepairReady(_, r, _) = ev {
                        ready = Some(r);
                        break;
                    }
                    if let EngineEvent::RepairFailed(_, d) = ev {
                        panic!("the free propose path must complete: {}", d.why);
                    }
                }
                Ok(None) | Err(_) => break,
            }
        }

        let repair =
            ready.expect("the propose path must reach RepairReady with no license installed");
        assert!(
            repair
                .claims
                .iter()
                .all(|c| c.provenance == Provenance::Verified),
            "a free user still gets a fully verified repair: {:?}",
            repair.claims
        );
        let record_path = p.join("record.jsonl");
        let record = std::fs::read_to_string(&record_path).unwrap_or_default();
        assert!(record.contains("\"kind\":\"repair_ready\""), "{record}");

        // -- three genuinely different license states -----------------------
        let now = 100 * 3_600_000u64;
        let (private_hex, public_hex) = license::generate_issuing_keypair().unwrap();
        let anchor = license::issuing_key_from_hex(&public_hex).unwrap();
        let base = License {
            v: 1,
            customer: "Caelon Systems".to_string(),
            tier: "team".to_string(),
            issued_ms: now - 86_400_000,
            expires_ms: now + 86_400_000,
        };
        let active = license::verify(&license::mint(&private_hex, &base).unwrap(), &anchor, now);
        let expired = license::verify(
            &license::mint(
                &private_hex,
                &License {
                    expires_ms: now - 1,
                    ..base.clone()
                },
            )
            .unwrap(),
            &anchor,
            now,
        );
        let absent = license::status_from(None, now);

        // The premise, checked rather than assumed — otherwise everything
        // below is comparing a value to itself.
        assert!(active.grants_act_alone(), "{active:?}");
        assert!(
            !expired.grants_act_alone(),
            "an expired key must fail closed: {expired:?}"
        );
        assert_eq!(absent, LicenseStatus::Absent);

        // -- and none of it touches the propose path ------------------------
        let with = crate::digest::render_stdout(
            &crate::digest::Digest::build(&record_path, 24 * 3_600_000, now, true),
            &render_ctx,
            false,
        );
        let without = crate::digest::render_stdout(
            &crate::digest::Digest::build(&record_path, 24 * 3_600_000, now, false),
            &render_ctx,
            false,
        );
        assert_eq!(
            with, without,
            "a proposed repair reads identically licensed or not — this is the free product"
        );
        assert!(
            without.contains(&repair.summary),
            "the proposal must be in the morning message: {without}"
        );
        assert!(without.contains("still awaiting a decision"), "{without}");

        // The ship gate, for the class this failure belongs to, is unchanged
        // and takes no license input at all: propose, because nothing has
        // been promoted — the same answer a paying customer gets here.
        assert!(matches!(
            engine_authority::ship_decision(
                engine_authority::Ladder::load(&record_path)
                    .unwrap()
                    .rung(&engine_authority::FailureClass::new("shop", "TypeError")),
                &Intake::Snippet,
                // Eligible on purpose: this test is about the RUNG, so the
                // evidence must not be what stops it.
                &engine_core::authority::testing::evidence(true),
            ),
            engine_authority::ShipDecision::Propose(_)
        ));

        // -- nobody on the free path is sold to -----------------------------
        for (surface, text) in [
            ("the terminal narration", &narration),
            ("the record", &record),
            ("the morning message", &without),
        ] {
            let lower = text.to_lowercase();
            for word in [
                "license", "paid", "pricing", "upgrade", "activate", "trial", "drums.sh", "$",
            ] {
                assert!(
                    !lower.contains(word),
                    "{surface} must never mention {word:?} on the free propose path:\n{text}"
                );
            }
        }
    }

    /// C1 (CRITICAL, CONFIRMED live by the reviewer against the seeded demo
    /// app): `failure.sample.request` is the RAW in-memory request — the
    /// `repair_context` line must never carry it verbatim. Every `event`
    /// line in the same record is correctly redacted by `engine-ingest`; the
    /// `repair_context` line, appended by a second writer, must be too.
    #[tokio::test]
    async fn repair_context_record_line_redacts_the_body_and_query_string_never_the_raw_card_token_or_query_secret(
    ) {
        let (dir, sha) = init_repairable_fixture_repo();
        let p = dir.path();

        let (in_tx, in_rx) = tokio::sync::mpsc::unbounded_channel();
        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
        let cfg = cfg_with_agent(p, Arc::new(FixingAgent));
        tokio::spawn(Engine::run(
            cfg,
            in_rx,
            std::sync::Arc::new(FakeRepro),
            ev_tx,
        ));

        in_tx
            .send(Ingested::Deploy(deploy_at(&sha, 1_000)))
            .unwrap();
        for at in [2_000, 3_000, 4_000] {
            in_tx
                .send(Ingested::Error(event_with_sensitive_request(at)))
                .unwrap();
        }

        let mut ready = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_secs(30), ev_rx.recv()).await {
                Ok(Some(EngineEvent::RepairReady(..))) => {
                    ready = true;
                    break;
                }
                Ok(Some(EngineEvent::RepairFailed(_, detail))) => {
                    panic!("expected RepairReady, got RepairFailed: {}", detail.why)
                }
                Ok(Some(_)) => continue,
                Ok(None) | Err(_) => break,
            }
        }
        assert!(ready, "expected RepairReady");

        let record = std::fs::read_to_string(p.join("record.jsonl")).unwrap_or_default();
        let repair_context_line = record
            .lines()
            .find(|l| l.contains("\"kind\":\"repair_context\""))
            .unwrap_or_else(|| panic!("record.jsonl must carry a repair_context line: {record}"));
        assert!(
            !repair_context_line.contains("4242424242424242"),
            "repair_context line must never carry the raw card number: {repair_context_line}"
        );
        assert!(
            !repair_context_line.contains("SECRET123"),
            "repair_context line must never carry the raw token: {repair_context_line}"
        );
        assert!(
            repair_context_line.contains("[redacted]"),
            "repair_context line must carry the redaction marker: {repair_context_line}"
        );
        assert!(
            repair_context_line.contains("/api/checkout"),
            "the (redacted) path must still be present: {repair_context_line}"
        );
    }

    /// Direct coverage of `redact_for_record` itself (the query-string half
    /// of C1, independent of the full pipeline's fixture route matching —
    /// see the comment on `event_with_sensitive_request`).
    #[test]
    fn redact_for_record_masks_the_body_and_the_query_string() {
        let req = CapturedRequest {
            method: "POST".into(),
            path: "/api/checkout?api_key=SECRET456".into(),
            content_type: Some("application/json".into()),
            body: Some(r#"{"card":"4242424242424242","token":"SECRET123","item":"widget"}"#.into()),
        };
        let out = redact_for_record(&req);
        assert!(
            !out.path.contains("SECRET456"),
            "query-string secret must be redacted: {}",
            out.path
        );
        assert!(out.path.contains("[redacted]"), "{}", out.path);
        let body = out.body.as_deref().unwrap();
        assert!(
            !body.contains("4242424242424242"),
            "card number must be redacted: {body}"
        );
        assert!(
            !body.contains("SECRET123"),
            "token must be redacted: {body}"
        );
        assert!(
            body.contains("widget"),
            "non-sensitive fields must survive: {body}"
        );
    }

    /// Agent produces no changes: `RepairFailed`, worktree kept, and the
    /// detector's own health (a later signature can still be observed) is
    /// unaffected.
    #[tokio::test]
    async fn agent_failure_yields_repair_failed_worktree_kept_detector_still_healthy() {
        let (dir, sha) = init_repairable_fixture_repo();
        let p = dir.path();

        let (in_tx, in_rx) = tokio::sync::mpsc::unbounded_channel();
        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
        let cfg = cfg_with_agent(p, Arc::new(FailingAgent));
        tokio::spawn(Engine::run(
            cfg,
            in_rx,
            std::sync::Arc::new(FakeRepro),
            ev_tx,
        ));

        in_tx
            .send(Ingested::Deploy(deploy_at(&sha, 1_000)))
            .unwrap();
        for at in [2_000, 3_000, 4_000] {
            in_tx.send(Ingested::Error(event(at))).unwrap();
        }

        let mut failed: Option<RepairFailure> = None;
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_secs(20), ev_rx.recv()).await {
                Ok(Some(EngineEvent::RepairFailed(_, detail))) => {
                    failed = Some(detail);
                    break;
                }
                Ok(Some(EngineEvent::RepairReady(..))) => {
                    panic!("expected RepairFailed for a failing agent")
                }
                Ok(Some(_)) => continue,
                Ok(None) | Err(_) => break,
            }
        }
        let detail = failed.expect("expected RepairFailed");
        assert!(
            detail
                .why
                .to_lowercase()
                .contains("could not produce a fix")
                || detail.why.to_lowercase().contains("no changes"),
            "why: {}",
            detail.why
        );
        assert!(
            detail.branch.is_none(),
            "no branch was ever created when the agent itself failed"
        );
        let worktree_path = detail
            .worktree
            .clone()
            .expect("worktree path must be reported so a human can inspect it");
        assert!(
            std::path::Path::new(&worktree_path).exists(),
            "the worktree must be left on disk on RepairFailed"
        );
        // best-effort cleanup so the test doesn't litter the real tmp dir
        let _ = std::process::Command::new("git")
            .arg("-C")
            .arg(p)
            .args(["worktree", "remove", "--force"])
            .arg(&worktree_path)
            .status();
        let _ = std::fs::remove_dir_all(&worktree_path);

        // detector still healthy: a fresh unrelated signature can still be
        // detected normally after this failure.
        for at in [10_000, 11_000, 12_000] {
            in_tx
                .send(Ingested::Error(ErrorEvent {
                    service: "shop".into(),
                    occurred_at_ms: at,
                    error_name: "RangeError".into(),
                    error_message: "m".into(),
                    stack: "RangeError: m\n    at other (/w/shop/other.js:1:1)".into(),
                    request: Some(CapturedRequest {
                        method: "GET".into(),
                        path: "/x".into(),
                        content_type: None,
                        body: None,
                    }),
                    intake: engine_core::Intake::Snippet,
                }))
                .unwrap();
        }
        let mut saw_new_failure = false;
        loop {
            match tokio::time::timeout(Duration::from_secs(5), ev_rx.recv()).await {
                Ok(Some(EngineEvent::FailureDetected(f)))
                    if f.signature.error_name == "RangeError" =>
                {
                    saw_new_failure = true;
                    break;
                }
                Ok(Some(_)) => continue,
                _ => break,
            }
        }
        assert!(saw_new_failure, "the detector must still be able to open a new, unrelated failure after a repair failure");
    }

    /// Agent applies a fix that does NOT actually resolve the failure —
    /// verification must fail, naming the specific check.
    #[tokio::test]
    async fn wrong_fix_fails_verification_naming_the_check() {
        let (dir, sha) = init_repairable_fixture_repo();
        let p = dir.path();

        let (in_tx, in_rx) = tokio::sync::mpsc::unbounded_channel();
        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
        let cfg = cfg_with_agent(p, Arc::new(WrongFixAgent));
        tokio::spawn(Engine::run(
            cfg,
            in_rx,
            std::sync::Arc::new(FakeRepro),
            ev_tx,
        ));

        in_tx
            .send(Ingested::Deploy(deploy_at(&sha, 1_000)))
            .unwrap();
        for at in [2_000, 3_000, 4_000] {
            in_tx.send(Ingested::Error(event(at))).unwrap();
        }

        let mut failed: Option<RepairFailure> = None;
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_secs(20), ev_rx.recv()).await {
                Ok(Some(EngineEvent::RepairFailed(_, detail))) => {
                    failed = Some(detail);
                    break;
                }
                Ok(Some(EngineEvent::RepairReady(..))) => {
                    panic!("expected RepairFailed for a fix that still 500s")
                }
                Ok(Some(_)) => continue,
                Ok(None) | Err(_) => break,
            }
        }
        let detail = failed.expect("expected RepairFailed");
        assert!(
            detail.why.contains("500"),
            "the failure reason must name the specific check that failed: {}",
            detail.why
        );
        assert!(
            detail.branch.is_some(),
            "a commit WAS made (real diff) before verification failed, so a branch must exist"
        );
        let worktree_path = detail
            .worktree
            .clone()
            .expect("worktree must be kept for inspection");
        assert!(std::path::Path::new(&worktree_path).exists());
        // The commit and branch persist in the origin repo even though the
        // worktree checkout is kept separately.
        let branch = detail.branch.unwrap();
        let branches = std::process::Command::new("git")
            .arg("-C")
            .arg(p)
            .args(["branch", "--list", &branch])
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&branches.stdout).contains(&branch));
        // best-effort cleanup
        let _ = std::process::Command::new("git")
            .arg("-C")
            .arg(p)
            .args(["worktree", "remove", "--force"])
            .arg(&worktree_path)
            .status();
        let _ = std::fs::remove_dir_all(&worktree_path);
    }

    /// Applies a fix shaped exactly like the cheapest way to stop a 500:
    /// delete/short-circuit the failing route entirely. `/health` still
    /// passes; the original request now 404s instead of 500ing.
    struct RouteDeletingAgent;
    #[async_trait::async_trait]
    impl RepairAgent for RouteDeletingAgent {
        async fn repair(
            &self,
            worktree: &Path,
            _ctx: &RepairContext,
        ) -> Result<RepairAttempt, RepairError> {
            const SERVER_JS_NO_ROUTE: &str = r#"
const http = require("http");
const server = http.createServer((req, res) => {
  if (req.url === "/health") { res.writeHead(200); res.end("ok"); return; }
  res.writeHead(404); res.end();
});
server.listen(process.env.PORT || 0, () => { console.log("listening " + server.address().port); });
"#;
            std::fs::write(worktree.join("server.js"), SERVER_JS_NO_ROUTE)
                .map_err(RepairError::Io)?;
            Ok(RepairAttempt {
                summary: "removed the failing route entirely".to_string(),
                diff_stat: "server.js | 6 +---".to_string(),
            })
        }
        fn name(&self) -> &str {
            "fake-route-deleting-agent"
        }
    }

    /// F3: verification must not accept ANY non-5xx as `verified` — the
    /// acceptance criterion the pipeline hands the agent says 2xx
    /// (`build_acceptance`). A 404 (the cheapest way to make a 500 go away —
    /// delete the route) must fail verification, never earn `RepairReady`.
    #[tokio::test]
    async fn route_deletion_shaped_fix_returns_404_and_fails_verification_naming_the_status() {
        let (dir, sha) = init_repairable_fixture_repo();
        let p = dir.path();

        let (in_tx, in_rx) = tokio::sync::mpsc::unbounded_channel();
        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
        let cfg = cfg_with_agent(p, Arc::new(RouteDeletingAgent));
        tokio::spawn(Engine::run(
            cfg,
            in_rx,
            std::sync::Arc::new(FakeRepro),
            ev_tx,
        ));

        in_tx
            .send(Ingested::Deploy(deploy_at(&sha, 1_000)))
            .unwrap();
        for at in [2_000, 3_000, 4_000] {
            in_tx.send(Ingested::Error(event(at))).unwrap();
        }

        let mut failed: Option<RepairFailure> = None;
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_secs(20), ev_rx.recv()).await {
                Ok(Some(EngineEvent::RepairFailed(_, detail))) => {
                    failed = Some(detail);
                    break;
                }
                Ok(Some(EngineEvent::RepairReady(..))) => {
                    panic!("a 404 route-deletion must never earn RepairReady — the cheapest way to stop a 500 is to delete the route")
                }
                Ok(Some(_)) => continue,
                Ok(None) | Err(_) => break,
            }
        }
        let detail = failed.expect("expected RepairFailed");
        assert!(
            detail.why.contains("404"),
            "the failure must name the actual status the endpoint now returns: {}",
            detail.why
        );
        assert!(!detail.why.contains("still returns 500"), "the message must describe what the endpoint NOW returns (404), not the original failure: {}", detail.why);
        if let Some(worktree_path) = &detail.worktree {
            let _ = std::process::Command::new("git")
                .arg("-C")
                .arg(p)
                .args(["worktree", "remove", "--force"])
                .arg(worktree_path)
                .status();
            let _ = std::fs::remove_dir_all(worktree_path);
        }
    }

    // -- F2: package.json `scripts.test` -------------------------------------

    /// A tempdir git repo carrying the same bootable server as
    /// `init_repairable_fixture_repo`, plus a `package.json` declaring the
    /// given `scripts.test`, and a fake local binary at
    /// `node_modules/.bin/fake-jest` (committed with the executable bit) —
    /// standing in for `jest`/`vitest`/`eslint`: a real local dev-dependency
    /// binary that is only resolvable if `node_modules/.bin` is on PATH.
    /// This shape (per the review) had never been exercised by any fixture:
    /// no repo in this suite, and not the demo app, had a `package.json` at
    /// all. `fake-jest` prints two "test" lines plus a jest-shaped
    /// `Tests: N passed, N total` summary (n25: real enough for
    /// `parse_test_counts` to recognize, so the baseline/post-repair count
    /// comparison exercises the SAME code path a real jest run would) and
    /// exits 0, unless invoked with `fail`, in which case it prints to
    /// stderr and exits 1.
    fn init_repairable_fixture_repo_with_test_script(
        test_script: &str,
    ) -> (tempfile::TempDir, String) {
        const SERVER_JS: &str = r#"
const http = require("http");
const server = http.createServer((req, res) => {
  if (req.url === "/health") { res.writeHead(200); res.end("ok"); return; }
  if (req.url === "/api/checkout") {
    res.writeHead(500, { "content-type": "application/json" });
    res.end(JSON.stringify({ error: { name: "TypeError", message: "boom", stack: "TypeError: boom\n    at computeTotal (server.js:4:2)" } }));
    return;
  }
  res.writeHead(404); res.end();
});
server.listen(process.env.PORT || 0, () => { console.log("listening " + server.address().port); });
"#;
        const FAKE_JEST: &str =
            "#!/bin/sh\nif [ \"$1\" = \"fail\" ]; then echo failing-suite 1>&2; exit 1; fi\necho t1\necho t2\necho 'Tests:       2 passed, 2 total'\n";
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "t@t"],
            vec!["config", "user.name", "t"],
        ] {
            assert!(std::process::Command::new("git")
                .arg("-C")
                .arg(p)
                .args(&args)
                .status()
                .unwrap()
                .success());
        }
        std::fs::write(p.join("server.js"), SERVER_JS).unwrap();
        let bin_dir = p.join("node_modules").join(".bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let bin_path = bin_dir.join("fake-jest");
        std::fs::write(&bin_path, FAKE_JEST).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&bin_path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&bin_path, perms).unwrap();
        }
        let package_json = serde_json::json!({ "scripts": { "test": test_script } });
        std::fs::write(
            p.join("package.json"),
            serde_json::to_string_pretty(&package_json).unwrap(),
        )
        .unwrap();
        for args in [vec!["add", "-f", "."], vec!["commit", "-qm", "c1"]] {
            assert!(std::process::Command::new("git")
                .arg("-C")
                .arg(p)
                .args(&args)
                .status()
                .unwrap()
                .success());
        }
        let sha = String::from_utf8(
            std::process::Command::new("git")
                .arg("-C")
                .arg(p)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        (dir, sha)
    }

    /// End-to-end: an app WITH a `package.json` test script that invokes a
    /// locally-installed binary (`node_modules/.bin/fake-jest`, standing in
    /// for `jest`/`vitest`/`eslint`) goes through the real pipeline and the
    /// script's claim lands on `RepairReady`. The prior `split_whitespace` +
    /// bare `Command::new(prog)` implementation ENOENTs here (`fake-jest`
    /// resolves only via the `node_modules/.bin` PATH prepend), which is
    /// exactly the failure mode the review describes for real npm scripts.
    #[tokio::test]
    async fn full_pipeline_runs_the_apps_test_script_and_includes_the_claim_on_repair_ready() {
        let (dir, sha) = init_repairable_fixture_repo_with_test_script("fake-jest");
        let p = dir.path();

        let (in_tx, in_rx) = tokio::sync::mpsc::unbounded_channel();
        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
        let cfg = cfg_with_agent(p, Arc::new(FixingAgent));
        tokio::spawn(Engine::run(
            cfg,
            in_rx,
            std::sync::Arc::new(FakeRepro),
            ev_tx,
        ));

        in_tx
            .send(Ingested::Deploy(deploy_at(&sha, 1_000)))
            .unwrap();
        for at in [2_000, 3_000, 4_000] {
            in_tx.send(Ingested::Error(event(at))).unwrap();
        }

        let mut ready: Option<Repair> = None;
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_secs(30), ev_rx.recv()).await {
                Ok(Some(EngineEvent::RepairReady(_, r, _))) => {
                    ready = Some(r);
                    break;
                }
                Ok(Some(EngineEvent::RepairFailed(_, detail))) => {
                    panic!("expected RepairReady, got RepairFailed: {}", detail.why)
                }
                Ok(Some(_)) => continue,
                Ok(None) | Err(_) => break,
            }
        }
        let repair = ready.expect("expected RepairReady");
        assert!(
            repair
                .claims
                .iter()
                .all(|c| c.provenance == Provenance::Verified),
            "every claim on a RepairReady repair must be verified: {:?}",
            repair.claims
        );
        assert!(
            repair
                .claims
                .iter()
                .any(|c| c.text.contains("test script passed")),
            "the test-script claim must be present: {:?}",
            repair.claims
        );
    }

    /// A good fix to the actual bug must still whole-fail verification, naming
    /// the test script, when the app's own test script fails — never a
    /// partial verified set (and never an ENOENT from a script that was
    /// never actually run).
    #[tokio::test]
    async fn full_pipeline_fails_whole_verify_naming_the_script_when_it_fails() {
        let (dir, sha) =
            init_repairable_fixture_repo_with_test_script("echo failing-suite 1>&2; exit 1");
        let p = dir.path();

        let (in_tx, in_rx) = tokio::sync::mpsc::unbounded_channel();
        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
        let cfg = cfg_with_agent(p, Arc::new(FixingAgent));
        tokio::spawn(Engine::run(
            cfg,
            in_rx,
            std::sync::Arc::new(FakeRepro),
            ev_tx,
        ));

        in_tx
            .send(Ingested::Deploy(deploy_at(&sha, 1_000)))
            .unwrap();
        for at in [2_000, 3_000, 4_000] {
            in_tx.send(Ingested::Error(event(at))).unwrap();
        }

        let mut failed: Option<RepairFailure> = None;
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_secs(30), ev_rx.recv()).await {
                Ok(Some(EngineEvent::RepairFailed(_, detail))) => {
                    failed = Some(detail);
                    break;
                }
                Ok(Some(EngineEvent::RepairReady(..))) => panic!(
                    "expected RepairFailed: the route fix is good but the app's test script fails"
                ),
                Ok(Some(_)) => continue,
                Ok(None) | Err(_) => break,
            }
        }
        let detail = failed.expect("expected RepairFailed");
        assert!(
            detail.why.contains("test script"),
            "the failure must name the check that failed: {}",
            detail.why
        );
        assert!(
            detail.why.contains("failing-suite") || detail.why.contains("exit 1"),
            "the failure must name the actual script content, not a generic message: {}",
            detail.why
        );
        if let Some(worktree_path) = &detail.worktree {
            let _ = std::process::Command::new("git")
                .arg("-C")
                .arg(p)
                .args(["worktree", "remove", "--force"])
                .arg(worktree_path)
                .status();
            let _ = std::fs::remove_dir_all(worktree_path);
        }
    }

    /// Direct coverage of `run_package_test_script` itself — proves the
    /// shell-execution, PATH, and env-minimization fixes independently of
    /// the full pipeline's runtime cost.
    #[tokio::test]
    async fn run_package_test_script_runs_shell_style_scripts_and_returns_the_claim() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"scripts":{"test":"echo one && echo two"}}"#,
        )
        .unwrap();
        let claim = run_package_test_script(dir.path(), &read_test_script(dir.path()), None)
            .await
            .expect("script must run")
            .expect("package.json declares a test script");
        assert_eq!(claim.provenance, Provenance::Verified);
        assert!(
            claim.text.starts_with("2-line"),
            "expected the shell to actually run `&&` (two echoes): {}",
            claim.text
        );
    }

    #[tokio::test]
    async fn run_package_test_script_names_the_script_when_it_fails() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"scripts":{"test":"echo boom 1>&2; exit 1"}}"#,
        )
        .unwrap();
        let err = run_package_test_script(dir.path(), &read_test_script(dir.path()), None)
            .await
            .expect_err("a failing test script must fail the whole verify");
        assert!(
            err.contains("echo boom"),
            "the failure must name the actual script that failed: {err}"
        );
    }

    #[tokio::test]
    async fn run_package_test_script_resolves_locally_installed_binaries_via_node_modules_bin() {
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("node_modules").join(".bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let bin_path = bin_dir.join("fake-test-runner");
        std::fs::write(&bin_path, "#!/bin/sh\necho ran\necho fake-test-runner\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&bin_path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&bin_path, perms).unwrap();
        }
        // Bare command name — resolvable only if `node_modules/.bin` is on
        // PATH, exactly like real npm scripts invoking `jest`/`vitest`/`eslint`.
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"scripts":{"test":"fake-test-runner"}}"#,
        )
        .unwrap();

        let claim = run_package_test_script(dir.path(), &read_test_script(dir.path()), None)
            .await
            .expect("script must run")
            .expect("must produce a claim");
        assert!(
            claim.text.starts_with("2-line"),
            "expected fake-test-runner's two lines of output: {}",
            claim.text
        );
    }

    /// F5 (Task-3 review round): `sh -c <agent-authored script>` was spawned
    /// with only `kill_on_drop(true)`, which SIGKILLs the direct `sh`
    /// process only. A script that backgrounds a grandchild (`sleep 1 &` —
    /// standing in for a `jest`/`vitest` worker, or a `node test/boot.js`
    /// that boots the service) keeps that grandchild alive past the 120s
    /// timeout, holding ports/CPU with no record — the exact same failure
    /// class `crates/repair/tests/repair.rs`'s
    /// `timeout_kills_the_whole_process_tree_not_just_the_direct_child` pins
    /// for the repair-agent child. Drives the timeout arm directly (not the
    /// real 120s constant) via `run_test_script_with_timeout`.
    #[tokio::test]
    async fn timeout_kills_the_whole_process_tree_not_just_the_direct_child() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("grandchild-marker.txt");
        let started = dir.path().join("script-started.txt");
        // The script backgrounds a grandchild that sleeps 2s then writes
        // `marker`, records that it got past that fork, and then sleeps far
        // past the 1s timeout so the timeout arm is guaranteed to fire first.
        //
        // n17 (review round 4): the timeout used to be 200ms, which RACED `sh`
        // forking its background subshell — if the timeout won, no marker would
        // have been written even without the process-group kill, so the
        // assertion proved nothing on a loaded machine. The 1s timeout plus the
        // `started` precondition below make the test non-vacuous: it fails if
        // the fork never happened, instead of passing for the wrong reason.
        let script = format!(
            "(sleep 2; touch {}) &\ntouch {}\nsleep 30\n",
            marker.display(),
            started.display()
        );

        let result =
            run_test_script_with_timeout(dir.path(), &script, Duration::from_secs(1)).await;
        assert!(
            matches!(&result, Err(e) if e.contains("timed out")),
            "expected a timeout error, got {result:?}"
        );
        assert!(started.exists(), "precondition: the script must have run past its background fork, or this test proves nothing");

        // Wait well past the 2s the grandchild would need to write its
        // marker if it survived the timeout kill.
        tokio::time::sleep(Duration::from_secs(4)).await;
        assert!(
            !marker.exists(),
            "a grandchild of the timed-out test script wrote after Drums declared it timed out — the process tree was not fully killed"
        );
    }

    /// F7 (Task-3 review round 4, reproduced live by the reviewer):
    /// `wait_with_output()` does not return when the child EXITS — it returns
    /// when both piped streams reach EOF. A test script that backgrounds
    /// anything (`node server.js & jest`, `npm start & npm run test:api`, a
    /// runner that daemonizes a helper) hands its INHERITED stdout/stderr to a
    /// grandchild that outlives it, so the wait blocked for the whole
    /// `TEST_SCRIPT_TIMEOUT` and returned `Err("test script timed out …")` for
    /// a script that had exited 0 in milliseconds — a `RepairFailed` for a
    /// good repair, with the narration blaming the agent's fix. Same bug
    /// `ship.rs`'s C2 fix removed from `run_deploy_cmd` (`ship.rs`'s doc names
    /// it at length) and `engine-repair` fixed before that.
    #[tokio::test]
    async fn a_test_script_that_backgrounds_a_process_holding_the_pipe_passes_instead_of_timing_out(
    ) {
        let dir = tempfile::tempdir().unwrap();
        // `(sleep 30) &` is the minimal shape of `node server.js & jest`: the
        // grandchild inherits the script's stdout/stderr and holds them long
        // after `sh` itself has exited 0.
        let script = "(sleep 30) &\necho ok\n";

        let started = Instant::now();
        let result = run_test_script_with_timeout(dir.path(), script, Duration::from_secs(5)).await;
        let elapsed = started.elapsed();

        let claim = result
            .expect("an exit-0 test script must pass, whatever it backgrounded")
            .expect("a claim");
        assert_eq!(claim.provenance, Provenance::Verified);
        assert!(
            elapsed < Duration::from_secs(3),
            "verification must return when the test script EXITS, not when its pipes close — took {elapsed:?} for a script that exited immediately"
        );
    }

    /// n14 (same round): nothing a test script backgrounds should outlive
    /// verification. Unlike a deploy command — which is SUPPOSED to leave the
    /// service it just started running (`ship.rs`'s `kill_process_group` doc)
    /// — a test runner's workers/daemons hold ports and CPU with no record,
    /// and a held fixed port then fails every LATER repair for a reason the
    /// narration blames on the agent.
    #[tokio::test]
    async fn a_grandchild_of_a_passing_test_script_does_not_outlive_verification() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("late-marker.txt");
        let script = format!("(sleep 1; touch {}) &\necho ok\n", marker.display());

        let claim = run_test_script_with_timeout(dir.path(), &script, Duration::from_secs(10))
            .await
            .expect("the script itself exits 0 immediately")
            .expect("a claim");
        assert_eq!(claim.provenance, Provenance::Verified);

        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(
            !marker.exists(),
            "a grandchild of a PASSING test script was still running after verification returned — it holds ports/CPU with no record and will fail a later repair"
        );
    }

    /// F7's secondary defect (and the carried Task-2 drain item): the failure
    /// message was built from a `Vec` that accumulated the child's ENTIRE
    /// stdout+stderr, so a chatty or looping agent-authored script could grow
    /// the `drums watch` process without bound for up to 120s.
    /// `engine-repair` solved this with `BoundedBuf`; `ship.rs` with
    /// `MAX_DRAIN_BYTES`.
    #[tokio::test]
    async fn a_chatty_failing_test_script_cannot_grow_the_failure_message_without_bound() {
        let dir = tempfile::tempdir().unwrap();
        // 1 MiB on stderr — four times the 256KiB retention cap.
        let script = "head -c 1048576 /dev/zero | tr '\\0' 'x' 1>&2\nexit 1\n";
        let err = run_test_script_with_timeout(dir.path(), script, Duration::from_secs(60))
            .await
            .expect_err("exit 1 must fail the verify");
        assert!(
            err.len() < 400_000,
            "the failure message must be bounded by the drain cap, not by how much the agent-authored script chose to print — got {} bytes",
            err.len()
        );
        assert!(
            err.contains("head -c"),
            "the bounded message must still name the script that failed: {}",
            &err[..err.len().min(200)]
        );
    }

    /// The record-contamination risk from the review: a test script must
    /// never see `DRUMS_INGEST_URL`, even though the watch process itself
    /// has it set (it's how the watch's own ingest is configured elsewhere).
    #[tokio::test]
    async fn run_package_test_script_strips_drums_ingest_url_even_when_the_watch_process_has_it_set(
    ) {
        let prior = std::env::var("DRUMS_INGEST_URL").ok();
        std::env::set_var("DRUMS_INGEST_URL", "http://127.0.0.1:1/must-not-leak");

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"scripts":{"test":"test -z \"$DRUMS_INGEST_URL\""}}"#,
        )
        .unwrap();

        let result = run_package_test_script(dir.path(), &read_test_script(dir.path()), None).await;

        match prior {
            Some(v) => std::env::set_var("DRUMS_INGEST_URL", v),
            None => std::env::remove_var("DRUMS_INGEST_URL"),
        }

        // `test -z` exits 0 only if the variable was empty/unset in the
        // child — if `env_clear()` were missing (or the var not stripped),
        // this would exit 1 and the whole call would be `Err`.
        result
            .expect("test script must run")
            .expect("package.json declares a test script");
    }

    // -- I1: the test gate must not be switchable-off by the agent -----------

    /// Applies the real, working fix to `server.js` **and** deletes
    /// `scripts.test` from `package.json` — the cheapest way for a coding
    /// agent to stop the app's own test suite from being a gate, and the
    /// shape I1 is about: before this fix it produced a byte-identical
    /// all-verified `RepairReady` to an app that legitimately has no tests.
    struct TestScriptDeletingAgent;
    #[async_trait::async_trait]
    impl RepairAgent for TestScriptDeletingAgent {
        async fn repair(
            &self,
            worktree: &Path,
            ctx: &RepairContext,
        ) -> Result<RepairAttempt, RepairError> {
            let attempt = FixingAgent.repair(worktree, ctx).await?;
            std::fs::write(
                worktree.join("package.json"),
                r#"{"name":"shop","scripts":{"build":"tsc"}}"#,
            )
            .map_err(RepairError::Io)?;
            Ok(attempt)
        }
        fn name(&self) -> &str {
            "fake-test-script-deleting-agent"
        }
    }

    /// Same, but keeps a `scripts.test` key and rewrites it to a no-op —
    /// deleting the key is not the only one-character way to switch the gate
    /// off.
    struct TestScriptNeuteringAgent;
    #[async_trait::async_trait]
    impl RepairAgent for TestScriptNeuteringAgent {
        async fn repair(
            &self,
            worktree: &Path,
            ctx: &RepairContext,
        ) -> Result<RepairAttempt, RepairError> {
            let attempt = FixingAgent.repair(worktree, ctx).await?;
            std::fs::write(
                worktree.join("package.json"),
                r#"{"name":"shop","scripts":{"test":"exit 0"}}"#,
            )
            .map_err(RepairError::Io)?;
            Ok(attempt)
        }
        fn name(&self) -> &str {
            "fake-test-script-neutering-agent"
        }
    }

    /// I1: an agent that deletes the test script must NOT earn a
    /// `RepairReady`. The route fix itself is perfect here (`FixingAgent`'s
    /// `server.js`), so the only thing that can fail the verify is the
    /// vanished test suite — and it must fail the WHOLE verify, named, like
    /// every other check in `verify_repair`.
    #[tokio::test]
    async fn full_pipeline_fails_the_verify_when_the_agent_deletes_the_apps_test_script() {
        let (dir, sha) = init_repairable_fixture_repo_with_test_script("fake-jest");
        let p = dir.path();

        let (in_tx, in_rx) = tokio::sync::mpsc::unbounded_channel();
        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
        let cfg = cfg_with_agent(p, Arc::new(TestScriptDeletingAgent));
        tokio::spawn(Engine::run(
            cfg,
            in_rx,
            std::sync::Arc::new(FakeRepro),
            ev_tx,
        ));

        in_tx
            .send(Ingested::Deploy(deploy_at(&sha, 1_000)))
            .unwrap();
        for at in [2_000, 3_000, 4_000] {
            in_tx.send(Ingested::Error(event(at))).unwrap();
        }

        let mut failed: Option<RepairFailure> = None;
        while let Ok(Some(ev)) = tokio::time::timeout(Duration::from_secs(30), ev_rx.recv()).await {
            match ev {
                EngineEvent::RepairFailed(_, detail) => {
                    failed = Some(detail);
                    break;
                }
                EngineEvent::RepairReady(_, r, _) => panic!(
                    "the agent deleted the app's test script and still earned a RepairReady — the strongest verification gate is switchable-off by the thing it polices: {:?}",
                    r.claims
                ),
                _ => continue,
            }
        }
        let detail = failed.expect("expected RepairFailed");
        assert!(
            detail.why.contains("test"),
            "the failure must name the check that failed: {}",
            detail.why
        );
        assert!(
            detail.why.contains("fake-jest"),
            "the failure must name the test script that was there before the agent ran: {}",
            detail.why
        );
        assert!(
            detail.worktree.is_some(),
            "design-the-miss: the worktree must be kept for the human"
        );
        if let Some(worktree_path) = &detail.worktree {
            let _ = std::process::Command::new("git")
                .arg("-C")
                .arg(p)
                .args(["worktree", "remove", "--force"])
                .arg(worktree_path)
                .status();
            let _ = std::fs::remove_dir_all(worktree_path);
        }
    }

    /// I1, second shape: rewriting `scripts.test` to a no-op is as cheap as
    /// deleting it, and passes a post-repair-only read trivially.
    #[tokio::test]
    async fn full_pipeline_fails_the_verify_when_the_agent_rewrites_the_apps_test_script() {
        let (dir, sha) = init_repairable_fixture_repo_with_test_script("fake-jest");
        let p = dir.path();

        let (in_tx, in_rx) = tokio::sync::mpsc::unbounded_channel();
        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
        let cfg = cfg_with_agent(p, Arc::new(TestScriptNeuteringAgent));
        tokio::spawn(Engine::run(
            cfg,
            in_rx,
            std::sync::Arc::new(FakeRepro),
            ev_tx,
        ));

        in_tx
            .send(Ingested::Deploy(deploy_at(&sha, 1_000)))
            .unwrap();
        for at in [2_000, 3_000, 4_000] {
            in_tx.send(Ingested::Error(event(at))).unwrap();
        }

        let mut failed: Option<RepairFailure> = None;
        while let Ok(Some(ev)) = tokio::time::timeout(Duration::from_secs(30), ev_rx.recv()).await {
            match ev {
                EngineEvent::RepairFailed(_, detail) => {
                    failed = Some(detail);
                    break;
                }
                EngineEvent::RepairReady(_, r, _) => {
                    panic!("the agent replaced the app's test script with `exit 0` and still earned a RepairReady: {:?}", r.claims)
                }
                _ => continue,
            }
        }
        let detail = failed.expect("expected RepairFailed");
        assert!(
            detail.why.contains("fake-jest") && detail.why.contains("exit 0"),
            "the failure must name both the script that was there and what it became: {}",
            detail.why
        );
        if let Some(worktree_path) = &detail.worktree {
            let _ = std::process::Command::new("git")
                .arg("-C")
                .arg(p)
                .args(["worktree", "remove", "--force"])
                .arg(worktree_path)
                .status();
            let _ = std::fs::remove_dir_all(worktree_path);
        }
    }

    /// I1 / the binding "degraded paths are `unresolved`" rule: a
    /// `package.json` that declares no runnable test script is a check that
    /// was expected and did not run. It must be SAID, not returned as
    /// silence.
    #[tokio::test]
    async fn a_package_json_with_no_test_script_yields_an_unresolved_claim_not_silence() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"name":"shop","scripts":{"build":"tsc"}}"#,
        )
        .unwrap();

        let claim = run_package_test_script(dir.path(), &TestScript::NoPackageJson, None)
            .await
            .expect("no runnable test script is not itself a verification failure")
            .expect("a package.json that declares no test script must produce an unresolved claim, not no claim at all");
        assert_eq!(
            claim.provenance,
            Provenance::Unresolved,
            "no suite ran, so nothing here is verified: {claim:?}"
        );
        assert!(
            claim.text.contains("no test suite was executed"),
            "the claim must say a suite did not run: {}",
            claim.text
        );
        assert!(
            claim.text.contains("scripts.test"),
            "the claim must name WHY no suite ran: {}",
            claim.text
        );
    }

    #[tokio::test]
    async fn an_unparseable_package_json_yields_an_unresolved_claim_not_silence() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("package.json"), "{ this is not json").unwrap();

        let claim = run_package_test_script(dir.path(), &TestScript::NoPackageJson, None)
            .await
            .expect("an unparseable package.json is not by itself a verification failure when the app never had tests")
            .expect("an unparseable package.json must produce an unresolved claim");
        assert_eq!(claim.provenance, Provenance::Unresolved);
        assert!(claim.text.contains("could not be parsed"), "{}", claim.text);
        assert!(
            claim.text.contains("no test suite was executed"),
            "{}",
            claim.text
        );
    }

    /// npm's own `no test specified` placeholder: still a check that did not
    /// run, so still `unresolved` — the reader should not have to guess
    /// whether Drums looked.
    #[tokio::test]
    async fn npms_no_test_specified_placeholder_yields_an_unresolved_claim() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"scripts":{"test":"echo \"Error: no test specified\" && exit 1"}}"#,
        )
        .unwrap();

        let claim = run_package_test_script(dir.path(), &TestScript::NoPackageJson, None)
            .await
            .expect("the placeholder must not fail the verify")
            .expect("a claim");
        assert_eq!(claim.provenance, Provenance::Unresolved);
        assert!(
            claim.text.contains("no test suite was executed"),
            "{}",
            claim.text
        );
    }

    /// `Ok(None)` — no claim at all — must mean exactly one thing: this app
    /// never had an npm test suite.
    #[tokio::test]
    async fn only_an_app_without_a_package_json_produces_no_claim_at_all() {
        let dir = tempfile::tempdir().unwrap();
        let result = run_package_test_script(dir.path(), &TestScript::NoPackageJson, None)
            .await
            .expect("no package.json is not a verification failure");
        assert!(
            result.is_none(),
            "an app with no package.json is the ONLY silent case: {result:?}"
        );
    }

    /// Direct coverage of the pre/post comparison, independent of the
    /// pipeline's runtime cost: the script existed at the pre-repair sha and
    /// does not now → the WHOLE verify fails, naming it.
    #[tokio::test]
    async fn a_test_script_that_existed_before_the_agent_ran_and_is_gone_now_fails_the_whole_verify(
    ) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"scripts":{"build":"tsc"}}"#,
        )
        .unwrap();

        let before = TestScript::Declared("jest --ci".to_string());
        let err = run_package_test_script(dir.path(), &before, None)
            .await
            .expect_err(
                "a test script that disappeared under the agent's hands must fail the whole verify",
            );
        assert!(
            err.contains("jest --ci"),
            "the failure must name the test script that was there before: {err}"
        );
        assert!(
            err.contains("test suite"),
            "the failure must say what was lost: {err}"
        );
    }

    /// And the same when `package.json` itself became unreadable/unparseable
    /// under the agent's hands: the pre-repair suite is no longer runnable,
    /// which is a whole-verify failure, not an unresolved footnote.
    #[tokio::test]
    async fn a_package_json_the_agent_broke_fails_the_whole_verify_when_it_declared_tests_before() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("package.json"), "{ broken").unwrap();

        let before = TestScript::Declared("jest --ci".to_string());
        let err = run_package_test_script(dir.path(), &before, None)
            .await
            .expect_err(
                "a broken package.json that used to declare tests must fail the whole verify",
            );
        assert!(err.contains("jest --ci"), "{err}");
        assert!(
            err.contains("could not be parsed"),
            "the failure must name what is wrong now: {err}"
        );
    }

    // -- n25 (trust-hardening review): the I1 gate above compares
    // `scripts.test`'s TEXT, so an agent that leaves the text identical and
    // hollows out what it RUNS was proven live to earn an all-verified
    // `RepairReady` (`0-line test script passed [verified]`). These tests
    // pin the `TestBaseline` mechanism that closes it: a count-based check on
    // the script's own output, and a digest-based check on the tracked test
    // files it invokes — both independent of `scripts.test`'s text. --------

    #[test]
    fn parse_test_counts_recognizes_jest_and_vitest_summary_lines() {
        assert_eq!(
            parse_test_counts("Tests:       3 passed, 3 total"),
            Some((3, 3))
        );
        assert_eq!(
            parse_test_counts(
                "Suites: 1 passed, 1 total\nTests:       1 failed, 2 passed, 3 total"
            ),
            Some((2, 3))
        );
        assert_eq!(parse_test_counts("Tests  3 passed (3)"), Some((3, 3)));
        assert_eq!(
            parse_test_counts(" Test Files  1 passed (1)\n Tests  3 passed | 1 skipped (4)"),
            Some((3, 4))
        );
    }

    #[test]
    fn parse_test_counts_recognizes_mocha_and_tap_summary_lines() {
        assert_eq!(parse_test_counts("  3 passing (12ms)"), Some((3, 3)));
        assert_eq!(
            parse_test_counts("  2 passing (5ms)\n  1 failing"),
            Some((2, 3))
        );
        assert_eq!(
            parse_test_counts("# pass 4\n# fail 0\n# tests 4"),
            Some((4, 4))
        );
    }

    #[test]
    fn parse_test_counts_returns_none_for_output_it_does_not_recognize() {
        // Exactly the shape the reviewer's live probe produced: a script that
        // exits 0 having printed nothing that looks like a summary at all.
        assert_eq!(parse_test_counts(""), None);
        assert_eq!(parse_test_counts("t1\nt2\n"), None);
        assert_eq!(parse_test_counts("ok\n"), None);
    }

    #[test]
    fn content_digest_treats_empty_and_whitespace_only_as_the_same_emptied_content() {
        assert_eq!(content_digest(b""), content_digest(b"   \n\t\n"));
        assert_ne!(content_digest(b"assert(1 === 1);"), content_digest(b""));
    }

    /// RED FIRST (the reviewer's exact live repro, condensed to a fast unit
    /// test): the script's TEXT never changes and it still exits 0 having
    /// printed nothing parseable — before this fix, `run_package_test_script`
    /// had no baseline to compare against and always produced a `Verified`
    /// claim here (`"0-line test script passed"`). It must now degrade to
    /// `unresolved`, NEVER stay silently `verified`.
    #[tokio::test]
    async fn a_test_script_producing_unparseable_output_never_earns_a_verified_claim_when_a_baseline_exists(
    ) {
        let dir = tempfile::tempdir().unwrap();
        // Genuinely unparseable both before and after: the reviewer's exact
        // repro shape (a script that exits 0 having said nothing a known
        // runner format would say).
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"scripts":{"test":"true"}}"#,
        )
        .unwrap();
        let before = TestScript::Declared("true".to_string());
        let baseline = TestBaseline {
            counts: None,
            test_files: BTreeMap::new(),
        };

        let claim = run_package_test_script(dir.path(), &before, Some(&baseline))
            .await
            .expect("an exit-0 script must not fail the whole verify")
            .expect("a claim");
        assert_eq!(claim.provenance, Provenance::Unresolved, "unparseable output must never be silently verified, even though the script passed: {claim:?}");
        assert!(claim.text.contains("could not"), "{}", claim.text);
    }

    /// A script that still exits 0 but whose OWN output says it ran fewer
    /// tests than the pre-repair baseline must fail the whole verify — this
    /// is the direct, count-based half of n25's fix, independent of the
    /// tracked-file digest check.
    #[tokio::test]
    async fn a_test_script_reporting_fewer_tests_than_the_baseline_fails_the_whole_verify_even_though_it_exits_0(
    ) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"scripts":{"test":"echo 'Tests: 1 passed, 1 total'"}}"#,
        )
        .unwrap();
        let before = TestScript::Declared("echo 'Tests: 1 passed, 1 total'".to_string());
        let baseline = TestBaseline {
            counts: Some((3, 3)),
            test_files: BTreeMap::new(),
        };

        let err = run_package_test_script(dir.path(), &before, Some(&baseline))
            .await
            .expect_err("fewer reported tests than the baseline must fail the whole verify, even though the script exits 0 and `scripts.test` never changed");
        assert!(
            err.contains('3') && err.contains('1'),
            "the failure must name both counts: {err}"
        );
    }

    /// A script that reports the SAME OR MORE tests than the baseline still
    /// earns `verified` — the gate is "did not decrease", not "must be
    /// byte-identical".
    #[tokio::test]
    async fn a_test_script_reporting_the_same_or_more_tests_than_the_baseline_is_verified() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"scripts":{"test":"echo 'Tests: 4 passed, 4 total'"}}"#,
        )
        .unwrap();
        let before = TestScript::Declared("echo 'Tests: 4 passed, 4 total'".to_string());
        let baseline = TestBaseline {
            counts: Some((3, 3)),
            test_files: BTreeMap::new(),
        };

        let claim = run_package_test_script(dir.path(), &before, Some(&baseline))
            .await
            .expect("must run")
            .expect("a claim");
        assert_eq!(claim.provenance, Provenance::Verified);
        assert!(claim.text.contains("4/4"), "{}", claim.text);
    }

    /// The digest-based half: a tracked test file the baseline saw with real
    /// content is now empty. `scripts.test` never changed, and the script
    /// itself still exits 0 (it has nothing left to fail on) — only the
    /// digest comparison catches this, and it must fail the WHOLE verify.
    #[tokio::test]
    async fn a_repair_that_empties_a_tracked_test_file_fails_the_whole_verify_even_when_the_script_still_exits_0(
    ) {
        let dir = tempfile::tempdir().unwrap();
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "t@t"],
            vec!["config", "user.name", "t"],
        ] {
            assert!(std::process::Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(&args)
                .status()
                .unwrap()
                .success());
        }
        std::fs::create_dir_all(dir.path().join("test")).unwrap();
        std::fs::write(
            dir.path().join("test").join("checkout.test.js"),
            "assert(1 === 1);\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"scripts":{"test":"true"}}"#,
        )
        .unwrap();
        for args in [vec!["add", "."], vec!["commit", "-qm", "c1"]] {
            assert!(std::process::Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(&args)
                .status()
                .unwrap()
                .success());
        }

        let before = TestScript::Declared("true".to_string());
        let test_files = tracked_test_file_digests(dir.path());
        assert!(
            !test_files.is_empty(),
            "precondition: the baseline scan must pick up the tracked test file"
        );
        let baseline = TestBaseline {
            counts: None,
            test_files,
        };

        // The repair: `scripts.test` is untouched, but the test file it
        // covers is emptied — exactly the shape the reviewer proved live.
        std::fs::write(dir.path().join("test").join("checkout.test.js"), "").unwrap();

        let err = run_package_test_script(dir.path(), &before, Some(&baseline))
            .await
            .expect_err("emptying a tracked test file must fail the whole verify even though `scripts.test` never changed and the script exits 0");
        assert!(
            err.contains("checkout.test.js"),
            "the failure must name the emptied file: {err}"
        );
    }

    /// A tracked test file the repair DELETES entirely (not merely emptied)
    /// must fail the same way.
    #[tokio::test]
    async fn a_repair_that_deletes_a_tracked_test_file_fails_the_whole_verify() {
        let dir = tempfile::tempdir().unwrap();
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "t@t"],
            vec!["config", "user.name", "t"],
        ] {
            assert!(std::process::Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(&args)
                .status()
                .unwrap()
                .success());
        }
        std::fs::create_dir_all(dir.path().join("test")).unwrap();
        std::fs::write(
            dir.path().join("test").join("checkout.test.js"),
            "assert(1 === 1);\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"scripts":{"test":"true"}}"#,
        )
        .unwrap();
        for args in [vec!["add", "."], vec!["commit", "-qm", "c1"]] {
            assert!(std::process::Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(&args)
                .status()
                .unwrap()
                .success());
        }

        let before = TestScript::Declared("true".to_string());
        let baseline = TestBaseline {
            counts: None,
            test_files: tracked_test_file_digests(dir.path()),
        };

        std::fs::remove_file(dir.path().join("test").join("checkout.test.js")).unwrap();

        let err = run_package_test_script(dir.path(), &before, Some(&baseline))
            .await
            .expect_err("deleting a tracked test file must fail the whole verify");
        assert!(err.contains("checkout.test.js"), "{err}");
    }

    /// A repair that legitimately touches an UNRELATED tracked test file
    /// without hollowing it out (still non-empty, still has real content)
    /// must not be penalized — the digest check is a floor, not a diff.
    #[tokio::test]
    async fn a_repair_that_edits_a_tracked_test_file_without_emptying_it_is_not_penalized() {
        let dir = tempfile::tempdir().unwrap();
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "t@t"],
            vec!["config", "user.name", "t"],
        ] {
            assert!(std::process::Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(&args)
                .status()
                .unwrap()
                .success());
        }
        std::fs::create_dir_all(dir.path().join("test")).unwrap();
        std::fs::write(
            dir.path().join("test").join("checkout.test.js"),
            "assert(1 === 1);\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"scripts":{"test":"true"}}"#,
        )
        .unwrap();
        for args in [vec!["add", "."], vec!["commit", "-qm", "c1"]] {
            assert!(std::process::Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(&args)
                .status()
                .unwrap()
                .success());
        }

        let before = TestScript::Declared("true".to_string());
        let baseline = TestBaseline {
            counts: None,
            test_files: tracked_test_file_digests(dir.path()),
        };

        // A legitimate edit: still real, non-empty content.
        std::fs::write(
            dir.path().join("test").join("checkout.test.js"),
            "assert(1 === 1);\nassert(2 === 2);\n",
        )
        .unwrap();

        let claim = run_package_test_script(dir.path(), &before, Some(&baseline))
            .await
            .expect("a non-hollowing edit must not fail the whole verify")
            .expect("a claim");
        assert_eq!(claim.provenance, Provenance::Unresolved, "the count still can't be parsed from `true`'s output, so this degrades honestly rather than guessing verified");
    }

    /// Full pipeline, end to end: `scripts.test` text is byte-identical
    /// before and after (so the pre-existing I1 gate does NOT fire), and the
    /// route fix itself is perfect (`FixingAgent`) — the only thing that can
    /// fail this repair is the emptied tracked test file the script invokes.
    /// This is the reviewer's exact live-probe shape, driven through the
    /// real pipeline rather than the unit-level function above.
    struct TestFileHollowingAgent;
    #[async_trait::async_trait]
    impl RepairAgent for TestFileHollowingAgent {
        async fn repair(
            &self,
            worktree: &Path,
            ctx: &RepairContext,
        ) -> Result<RepairAttempt, RepairError> {
            let attempt = FixingAgent.repair(worktree, ctx).await?;
            // `scripts.test` (`node test/checkout.test.js`) is NEVER touched —
            // only the file it runs, hollowed to genuinely empty content. A
            // shell invoking an empty (or missing) file as `node` exits 0
            // having asserted nothing, which is exactly the reviewer's live
            // repro shape ("0-line test script passed [verified]").
            std::fs::write(worktree.join("test").join("checkout.test.js"), "")
                .map_err(RepairError::Io)?;
            Ok(attempt)
        }
        fn name(&self) -> &str {
            "fake-test-file-hollowing-agent"
        }
    }

    fn init_repairable_fixture_repo_with_real_test_file() -> (tempfile::TempDir, String) {
        const SERVER_JS: &str = r#"
const http = require("http");
const server = http.createServer((req, res) => {
  if (req.url === "/health") { res.writeHead(200); res.end("ok"); return; }
  if (req.url === "/api/checkout") {
    res.writeHead(500, { "content-type": "application/json" });
    res.end(JSON.stringify({ error: { name: "TypeError", message: "boom", stack: "TypeError: boom\n    at computeTotal (server.js:4:2)" } }));
    return;
  }
  res.writeHead(404); res.end();
});
server.listen(process.env.PORT || 0, () => { console.log("listening " + server.address().port); });
"#;
        // A real (if trivial) `console.assert`-based suite, invoked directly
        // by `node` — no `node_modules/.bin` indirection needed for this
        // fixture, since what's under test here is the TRACKED FILE the
        // script runs, not PATH resolution (that's F2/the fake-jest fixture).
        const TEST_JS: &str = "console.log('Tests: 2 passed, 2 total');\nprocess.exit(0);\n";
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "t@t"],
            vec!["config", "user.name", "t"],
        ] {
            assert!(std::process::Command::new("git")
                .arg("-C")
                .arg(p)
                .args(&args)
                .status()
                .unwrap()
                .success());
        }
        std::fs::write(p.join("server.js"), SERVER_JS).unwrap();
        std::fs::create_dir_all(p.join("test")).unwrap();
        std::fs::write(p.join("test").join("checkout.test.js"), TEST_JS).unwrap();
        let package_json =
            serde_json::json!({ "scripts": { "test": "node test/checkout.test.js" } });
        std::fs::write(
            p.join("package.json"),
            serde_json::to_string_pretty(&package_json).unwrap(),
        )
        .unwrap();
        for args in [vec!["add", "."], vec!["commit", "-qm", "c1"]] {
            assert!(std::process::Command::new("git")
                .arg("-C")
                .arg(p)
                .args(&args)
                .status()
                .unwrap()
                .success());
        }
        let sha = String::from_utf8(
            std::process::Command::new("git")
                .arg("-C")
                .arg(p)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        (dir, sha)
    }

    #[tokio::test]
    async fn full_pipeline_fails_the_verify_when_the_agent_hollows_out_a_tracked_test_file_with_scripts_test_unchanged(
    ) {
        let (dir, sha) = init_repairable_fixture_repo_with_real_test_file();
        let p = dir.path();

        let (in_tx, in_rx) = tokio::sync::mpsc::unbounded_channel();
        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
        let cfg = cfg_with_agent(p, Arc::new(TestFileHollowingAgent));
        tokio::spawn(Engine::run(
            cfg,
            in_rx,
            std::sync::Arc::new(FakeRepro),
            ev_tx,
        ));

        in_tx
            .send(Ingested::Deploy(deploy_at(&sha, 1_000)))
            .unwrap();
        for at in [2_000, 3_000, 4_000] {
            in_tx.send(Ingested::Error(event(at))).unwrap();
        }

        let mut failed: Option<RepairFailure> = None;
        while let Ok(Some(ev)) = tokio::time::timeout(Duration::from_secs(30), ev_rx.recv()).await {
            match ev {
                EngineEvent::RepairFailed(_, detail) => {
                    failed = Some(detail);
                    break;
                }
                EngineEvent::RepairReady(_, r, _) => panic!(
                    "the agent hollowed out the tracked test file the (unchanged) `scripts.test` invokes and still earned a RepairReady — n25's exact bypass: {:?}",
                    r.claims
                ),
                _ => continue,
            }
        }
        let detail = failed.expect("expected RepairFailed");
        assert!(
            detail.why.contains("checkout.test.js"),
            "the failure must name the emptied/deleted test file: {}",
            detail.why
        );
        assert!(
            detail.worktree.is_some(),
            "design-the-miss: the worktree must be kept for the human"
        );
        if let Some(worktree_path) = &detail.worktree {
            let _ = std::process::Command::new("git")
                .arg("-C")
                .arg(p)
                .args(["worktree", "remove", "--force"])
                .arg(worktree_path)
                .status();
            let _ = std::fs::remove_dir_all(worktree_path);
        }
    }

    /// The pre-repair read must happen against the checkout the agent has not
    /// touched yet, and must classify the four states the post-repair read
    /// is compared against.
    #[test]
    fn read_test_script_classifies_the_four_states() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_test_script(dir.path()), TestScript::NoPackageJson);

        std::fs::write(dir.path().join("package.json"), "{ nope").unwrap();
        assert!(matches!(
            read_test_script(dir.path()),
            TestScript::Unusable(_)
        ));

        std::fs::write(
            dir.path().join("package.json"),
            r#"{"scripts":{"build":"tsc"}}"#,
        )
        .unwrap();
        assert!(matches!(
            read_test_script(dir.path()),
            TestScript::NotDeclared(_)
        ));

        std::fs::write(
            dir.path().join("package.json"),
            r#"{"scripts":{"test":"  jest --ci  "}}"#,
        )
        .unwrap();
        assert_eq!(
            read_test_script(dir.path()),
            TestScript::Declared("jest --ci".to_string())
        );
    }

    // -- F1: `run_auto_ship` --------------------------------------------------

    /// Writes an executable fake deploy script that appends each of its
    /// argv elements, one per line, to `log_path` — lets a test observe
    /// exactly what argv the (real, non-shell) `Command` invocation
    /// produced, without any shell re-parsing it on the way in.
    fn write_fake_deploy_script(dir: &Path, log_path: &Path) -> PathBuf {
        let script_path = dir.join("fake-deploy.sh");
        let content = format!(
            "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\" >> \"{}\"; done\n",
            log_path.display()
        );
        std::fs::write(&script_path, content).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script_path, perms).unwrap();
        }
        script_path
    }

    /// `{sha}`/`{repo}` actually substitute, a `Shipped` event is actually
    /// emitted, `shipped` lands in the record, AND the reopen channel
    /// actually reaches `Detector::reopen` — none of which had any test
    /// before this round.
    #[tokio::test]
    async fn auto_ship_substitutes_sha_and_repo_emits_shipped_and_reopens_the_detector() {
        let (dir, sha) = init_repairable_fixture_repo();
        let p = dir.path();
        let scripts_dir = tempfile::tempdir().unwrap();
        let log_path = scripts_dir.path().join("deploy.log");
        let script = write_fake_deploy_script(scripts_dir.path(), &log_path);

        let (in_tx, in_rx) = tokio::sync::mpsc::unbounded_channel();
        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut cfg = cfg_with_agent(p, Arc::new(FixingAgent));
        cfg.repair_mode = RepairMode::Auto;
        promote_test_class(p);
        cfg.deploy_cmd = Some(format!("{} {{sha}} {{repo}}", script.display()));
        tokio::spawn(Engine::run(
            cfg,
            in_rx,
            std::sync::Arc::new(FakeRepro),
            ev_tx,
        ));

        in_tx
            .send(Ingested::Deploy(deploy_at(&sha, 1_000)))
            .unwrap();
        for at in [2_000, 3_000, 4_000] {
            in_tx.send(Ingested::Error(event(at))).unwrap();
        }

        let mut shipped: Option<ShipOutcome> = None;
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_secs(30), ev_rx.recv()).await {
                Ok(Some(EngineEvent::Shipped(_, outcome))) => {
                    shipped = Some(outcome);
                    break;
                }
                Ok(Some(EngineEvent::ShipFailed(_, why))) => {
                    panic!("expected Shipped, got ShipFailed: {why}")
                }
                Ok(Some(_)) => continue,
                Ok(None) | Err(_) => break,
            }
        }
        let outcome = shipped
            .expect("expected a Shipped event — run_auto_ship must actually run and succeed");
        assert_eq!(outcome.action, "shipped");

        let log = std::fs::read_to_string(&log_path).unwrap_or_default();
        assert!(
            log.contains(&outcome.repair_sha),
            "the deploy command must receive the repair's sha, not the literal \"{{sha}}\": {log}"
        );
        assert!(
            log.contains(&p.display().to_string()),
            "the deploy command must receive the repo path, not the literal \"{{repo}}\": {log}"
        );

        let record = std::fs::read_to_string(p.join("record.jsonl")).unwrap_or_default();
        assert!(
            record.contains("\"kind\":\"shipped\""),
            "record.jsonl must carry a shipped line: {record}"
        );

        // Reopen: resend the SAME signature and expect a fresh
        // FailureDetected — only possible if the reopen channel actually
        // reached `detector.reopen`, clearing the gate `run_repair_pipeline`
        // itself set when the failure first opened.
        for at in [100_000, 101_000, 102_000] {
            in_tx.send(Ingested::Error(event(at))).unwrap();
        }
        let mut redetected = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_secs(10), ev_rx.recv()).await {
                Ok(Some(EngineEvent::FailureDetected(f)))
                    if f.signature.error_name == "TypeError" =>
                {
                    redetected = true;
                    break;
                }
                Ok(Some(_)) => continue,
                Ok(None) | Err(_) => break,
            }
        }
        assert!(redetected, "the same signature must be re-detectable after a successful auto-ship — a bad repair must be re-detectable (spec §22)");
    }

    // -- THE ABSOLUTE GATE ----------------------------------------------------

    /// The load-bearing test of the intake taxonomy, end to end through the real
    /// engine loop.
    ///
    /// Setup is the STRONGEST case for shipping that exists today: `--repair
    /// auto` (the act-alone rung as the engine expresses it, see
    /// `RepairMode::rung`), a working deploy command, a repair agent that
    /// actually fixes the app, and a fixture whose `/health` and test checks all
    /// pass. The ONLY thing different from
    /// `auto_ship_substitutes_sha_and_repo_emits_shipped_and_reopens_the_detector`
    /// — which ships — is that the failure arrived from a trigger adapter with no
    /// replayable request.
    ///
    /// So it must propose and never ship: reproduction is skipped with an
    /// `unresolved` claim, the repair is still produced and offered, the deploy
    /// script is never executed, no `shipped` line is written, and the operator
    /// is told why in as many words.
    #[tokio::test]
    async fn an_act_alone_class_never_ships_a_trigger_intake_failure_it_proposes() {
        let (dir, sha) = init_repairable_fixture_repo();
        let p = dir.path();
        let scripts_dir = tempfile::tempdir().unwrap();
        let log_path = scripts_dir.path().join("deploy.log");
        let script = write_fake_deploy_script(scripts_dir.path(), &log_path);

        let (in_tx, in_rx) = tokio::sync::mpsc::unbounded_channel();
        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut cfg = cfg_with_agent(p, Arc::new(FixingAgent));
        cfg.repair_mode = RepairMode::Auto;
        promote_test_class(p); // the act-alone rung
        cfg.deploy_cmd = Some(format!("{} {{sha}} {{repo}}", script.display()));
        tokio::spawn(Engine::run(
            cfg,
            in_rx,
            std::sync::Arc::new(FakeRepro),
            ev_tx,
        ));

        in_tx
            .send(Ingested::Deploy(deploy_at(&sha, 1_000)))
            .unwrap();
        for at in [2_000, 3_000, 4_000] {
            in_tx
                .send(Ingested::Error(trigger_event(at, "hyperdx")))
                .unwrap();
        }

        // Collect until the pipeline reaches a terminal state for this failure.
        let mut kinds: Vec<&'static str> = Vec::new();
        let mut skip_claim: Option<Claim> = None;
        let mut ready: Option<Repair> = None;
        let mut withheld: Option<String> = None;
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_secs(60), ev_rx.recv()).await {
                Ok(Some(ev)) => {
                    kinds.push(kind_name(&ev));
                    match ev {
                        EngineEvent::ReproSkippedNotReplayable(_, _, claim) => skip_claim = Some(claim),
                        EngineEvent::RepairReady(_, repair, _) => ready = Some(repair),
                        EngineEvent::ShipWithheld(_, why) => {
                            withheld = Some(why);
                            break;
                        }
                        EngineEvent::Shipped(_, _) => panic!("SHIPPED a trigger-intake failure — the absolute gate is not holding: {kinds:?}"),
                        EngineEvent::RepairFailed(_, d) => panic!("repair failed, so this test proves nothing about the gate: {}", d.why),
                        _ => {}
                    }
                }
                Ok(None) | Err(_) => break,
            }
        }

        assert!(
            !kinds.contains(&"Shipped"),
            "a trigger-intake failure must never ship: {kinds:?}"
        );
        assert!(!kinds.contains(&"Reproducing"), "reproduction must not even be STARTED for a failure with no replayable request: {kinds:?}");
        assert!(
            !kinds.contains(&"Reproduced"),
            "there is nothing to reproduce: {kinds:?}"
        );

        // Reproduction skipped, and said so as an `unresolved` claim.
        let claim = skip_claim.expect("expected a ReproSkippedNotReplayable event");
        assert_eq!(claim.provenance, Provenance::Unresolved);
        assert!(
            claim.text.contains("hyperdx"),
            "the claim must name the intake source: {}",
            claim.text
        );
        assert!(
            claim.text.contains("reproduction not attempted"),
            "{}",
            claim.text
        );

        // The repair still happened and is offered as a proposal, with its
        // verification claims limited to what actually executed.
        let repair = ready.expect(
            "a trigger-intake failure must still get a repair PROPOSED when an agent is configured",
        );
        let replay_claim = repair
            .claims
            .iter()
            .find(|c| c.text.contains("original request"))
            .expect(
                "the claim list must name the replay that did not happen, not silently omit it",
            );
        assert_eq!(
            replay_claim.provenance,
            Provenance::Unresolved,
            "a replay that never ran must never be verified: {replay_claim:?}"
        );
        assert!(
            repair
                .claims
                .iter()
                .any(|c| c.provenance == Provenance::Verified),
            "checks that DID execute (boot, /health) must still be able to earn verified: {:?}",
            repair.claims
        );
        assert!(
            !repair
                .claims
                .iter()
                .any(|c| c.provenance == Provenance::Verified
                    && c.text.contains("original failing request")),
            "nothing may claim the original request was replayed: {:?}",
            repair.claims
        );

        // The refusal is narrated, naming the source and the missing replay.
        let why = withheld.expect(
            "the operator asked for an unattended ship and must be told why it did not happen",
        );
        assert!(why.contains("hyperdx"), "{why}");
        assert!(why.contains("no replayable request"), "{why}");

        // And nothing was deployed: no deploy script run, no `shipped` line.
        assert!(
            !log_path.exists()
                || std::fs::read_to_string(&log_path)
                    .unwrap_or_default()
                    .is_empty(),
            "the deploy command must never have been executed: {:?}",
            std::fs::read_to_string(&log_path)
        );
        let record = std::fs::read_to_string(p.join("record.jsonl")).unwrap_or_default();
        assert!(
            !record.contains("\"kind\":\"shipped\""),
            "no shipped line may be appended: {record}"
        );
        assert!(
            record.contains("\"kind\":\"repair_ready\""),
            "the proposal itself must still be recorded: {record}"
        );
        // No `repair_context` line either: there is no request to persist, and a
        // placeholder one would let a later `drums ship` replay something nobody
        // ever sent and record the result as post-deploy verification.
        assert!(
            !record.contains("\"kind\":\"repair_context\""),
            "no repair_context may be written without a real request: {record}"
        );
    }

    /// The same failure through the same act-alone config, arriving from a
    /// `reported` source (a human filing a Linear/Agentation issue) rather than
    /// telemetry. Same verdict — the gate is about replayability, not about which
    /// of the two trigger-only kinds it is.
    #[tokio::test]
    async fn an_act_alone_class_never_ships_a_reported_intake_failure_either() {
        let (dir, sha) = init_repairable_fixture_repo();
        let p = dir.path();
        let scripts_dir = tempfile::tempdir().unwrap();
        let log_path = scripts_dir.path().join("deploy.log");
        let script = write_fake_deploy_script(scripts_dir.path(), &log_path);

        let (in_tx, in_rx) = tokio::sync::mpsc::unbounded_channel();
        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut cfg = cfg_with_agent(p, Arc::new(FixingAgent));
        cfg.repair_mode = RepairMode::Auto;
        promote_test_class(p);
        cfg.deploy_cmd = Some(format!("{} {{sha}} {{repo}}", script.display()));
        tokio::spawn(Engine::run(
            cfg,
            in_rx,
            std::sync::Arc::new(FakeRepro),
            ev_tx,
        ));

        in_tx
            .send(Ingested::Deploy(deploy_at(&sha, 1_000)))
            .unwrap();
        for at in [2_000, 3_000, 4_000] {
            let ev = ErrorEvent {
                request: None,
                intake: Intake::Reported {
                    source: "linear".into(),
                },
                ..event(at)
            };
            in_tx.send(Ingested::Error(ev)).unwrap();
        }

        let mut withheld = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_secs(60), ev_rx.recv()).await {
                Ok(Some(EngineEvent::Shipped(_, _))) => {
                    panic!("SHIPPED a reported-intake failure — the absolute gate is not holding")
                }
                Ok(Some(EngineEvent::ShipWithheld(_, why))) => {
                    assert!(why.contains("linear"), "{why}");
                    withheld = true;
                    break;
                }
                Ok(Some(_)) => continue,
                Ok(None) | Err(_) => break,
            }
        }
        assert!(
            withheld,
            "a reported-intake failure must be withheld from shipping, with a reason"
        );
        assert!(
            !log_path.exists()
                || std::fs::read_to_string(&log_path)
                    .unwrap_or_default()
                    .is_empty(),
            "the deploy command must never have been executed"
        );
    }

    /// The control for the two tests above: with everything else identical, a
    /// SNIPPET failure on the act-alone rung still ships. Without this, the gate
    /// could be passing by blocking all shipping.
    #[tokio::test]
    async fn the_intake_gate_does_not_block_a_replayable_snippet_failure() {
        let (dir, sha) = init_repairable_fixture_repo();
        let p = dir.path();
        let scripts_dir = tempfile::tempdir().unwrap();
        let log_path = scripts_dir.path().join("deploy.log");
        let script = write_fake_deploy_script(scripts_dir.path(), &log_path);

        let (in_tx, in_rx) = tokio::sync::mpsc::unbounded_channel();
        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut cfg = cfg_with_agent(p, Arc::new(FixingAgent));
        cfg.repair_mode = RepairMode::Auto;
        promote_test_class(p);
        cfg.deploy_cmd = Some(format!("{} {{sha}} {{repo}}", script.display()));
        tokio::spawn(Engine::run(
            cfg,
            in_rx,
            std::sync::Arc::new(FakeRepro),
            ev_tx,
        ));

        in_tx
            .send(Ingested::Deploy(deploy_at(&sha, 1_000)))
            .unwrap();
        for at in [2_000, 3_000, 4_000] {
            in_tx.send(Ingested::Error(event(at))).unwrap();
        }

        let mut shipped = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_secs(60), ev_rx.recv()).await {
                Ok(Some(EngineEvent::Shipped(_, _))) => {
                    shipped = true;
                    break;
                }
                Ok(Some(EngineEvent::ShipWithheld(_, why))) => {
                    panic!("a replayable snippet failure must NOT be withheld: {why}")
                }
                Ok(Some(EngineEvent::ShipFailed(_, why))) => {
                    panic!("expected Shipped, got ShipFailed: {why}")
                }
                Ok(Some(_)) => continue,
                Ok(None) | Err(_) => break,
            }
        }
        assert!(
            shipped,
            "the snippet path must be entirely unchanged: an act-alone snippet failure still ships"
        );
    }

    // -- THE ABSOLUTE INTAKE GATE ------------------------------------------------

    /// An `ActAlone` class (`--repair auto`, with a deploy command configured and
    /// proven to work by
    /// `auto_ship_substitutes_sha_and_repo_emits_shipped_and_reopens_the_detector`
    /// above, which uses this same fixture and agent) plus a `Trigger`-intake
    /// failure: PROPOSES, never ships.
    ///
    /// This is the cardinal-sin test. Everything about this run says "ship it" —
    /// the top rung is granted, the deploy command is real, the agent produces a
    /// working fix, `/health` comes back 200 — and it must still refuse, because
    /// no claim in the chain was ever verified against the request that was
    /// actually failing. There wasn't one.
    ///
    /// It asserts on all four halves of the behavior, because any one of them
    /// passing alone would be compatible with a broken gate:
    /// 1. reproduction is SKIPPED, with an `unresolved` claim, not attempted and
    ///    not silently absent;
    /// 2. a repair is still PROPOSED (`RepairReady`) — the proposal is the
    ///    product's value here, so the gate must not be a blanket refusal;
    /// 3. `ShipWithheld` says why, out loud;
    /// 4. no `Shipped` event, and the deploy script never ran at all — checked
    ///    against the script's own log file, not just the event stream, since a
    ///    deploy that ran and then failed to record would also produce no
    ///    `Shipped`.
    #[tokio::test]
    async fn act_alone_never_ships_a_trigger_intake_failure_but_still_proposes() {
        let (dir, sha) = init_repairable_fixture_repo();
        let p = dir.path();
        let scripts_dir = tempfile::tempdir().unwrap();
        let log_path = scripts_dir.path().join("deploy.log");
        let script = write_fake_deploy_script(scripts_dir.path(), &log_path);

        let (in_tx, in_rx) = tokio::sync::mpsc::unbounded_channel();
        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut cfg = cfg_with_agent(p, Arc::new(FixingAgent));
        cfg.repair_mode = RepairMode::Auto;
        promote_test_class(p); // the act-alone rung, granted
        cfg.deploy_cmd = Some(format!("{} {{sha}} {{repo}}", script.display()));
        tokio::spawn(Engine::run(
            cfg,
            in_rx,
            std::sync::Arc::new(FakeRepro),
            ev_tx,
        ));

        in_tx
            .send(Ingested::Deploy(deploy_at(&sha, 1_000)))
            .unwrap();
        // A HyperDX log alert: same signature, but NO replayable request.
        for at in [2_000, 3_000, 4_000] {
            in_tx
                .send(Ingested::Error(ErrorEvent {
                    request: None,
                    intake: engine_core::Intake::Trigger {
                        source: "hyperdx".into(),
                    },
                    ..event(at)
                }))
                .unwrap();
        }

        let mut kinds: Vec<&'static str> = Vec::new();
        let mut skip_claim: Option<Claim> = None;
        let mut withheld: Option<String> = None;
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_secs(60), ev_rx.recv()).await {
                Ok(Some(e)) => {
                    kinds.push(kind_name(&e));
                    match e {
                        EngineEvent::Shipped(_, outcome) => {
                            panic!("a trigger-intake failure SHIPPED on its own — the absolute gate is broken: {outcome:?}")
                        }
                        EngineEvent::ReproSkippedNotReplayable(_, _, claim) => {
                            skip_claim = Some(claim)
                        }
                        EngineEvent::ShipWithheld(_, why) => {
                            withheld = Some(why);
                            break;
                        }
                        _ => continue,
                    }
                }
                Ok(None) | Err(_) => break,
            }
        }

        // 1. Reproduction skipped, said so, and `unresolved`.
        let claim = skip_claim.expect(
            "expected ReproSkippedNotReplayable — reproduction must be skipped, not attempted",
        );
        assert_eq!(claim.provenance, Provenance::Unresolved);
        assert_eq!(
            claim.text,
            "no replayable request captured for this hyperdx failure — reproduction not attempted"
        );
        assert!(
            !kinds.contains(&"Reproducing") && !kinds.contains(&"Reproduced"),
            "reproduction must never even be started for a non-replayable intake, got: {kinds:?}"
        );

        // 2. A repair was still proposed.
        assert!(
            kinds.contains(&"RepairReady"),
            "repair-as-proposal must still run for a trigger failure, got: {kinds:?}"
        );

        // 3. The refusal is narrated, naming the source and the missing replay.
        let why = withheld
            .expect("expected ShipWithheld — a withheld ship must be visible, never silent");
        assert!(why.contains("hyperdx"), "{why}");
        assert!(why.contains("no replayable request"), "{why}");

        // 4. Nothing shipped, and the deploy command never even ran.
        assert!(!kinds.contains(&"Shipped"), "got: {kinds:?}");
        assert!(
            !log_path.exists(),
            "the deploy command must never be invoked for a non-replayable failure; log: {:?}",
            std::fs::read_to_string(&log_path)
        );
        // No `repair_context` line either: `drums ship` must not be handed a
        // request to replay post-deploy that nobody ever made.
        let record = std::fs::read_to_string(p.join("record.jsonl")).unwrap_or_default();
        assert!(
            record.contains(r#""kind":"repair_ready""#),
            "the proposal must still be recorded: {record}"
        );
        assert!(
            !record.contains(r#""kind":"repair_context""#),
            "no repair_context line may exist without a replayable request: {record}"
        );
        assert!(!record.contains(r#""kind":"shipped""#), "{record}");
    }

    /// The counterpart to the test above, so the gate is proven to be about the
    /// INTAKE and not about `--repair auto` having quietly stopped working: same
    /// config, same agent, same deploy script — a `Reported` intake is refused
    /// too, and for its own named source.
    #[tokio::test]
    async fn act_alone_never_ships_a_reported_intake_failure() {
        let (dir, sha) = init_repairable_fixture_repo();
        let p = dir.path();
        let scripts_dir = tempfile::tempdir().unwrap();
        let log_path = scripts_dir.path().join("deploy.log");
        let script = write_fake_deploy_script(scripts_dir.path(), &log_path);

        let (in_tx, in_rx) = tokio::sync::mpsc::unbounded_channel();
        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut cfg = cfg_with_agent(p, Arc::new(FixingAgent));
        cfg.repair_mode = RepairMode::Auto;
        promote_test_class(p);
        cfg.deploy_cmd = Some(format!("{} {{sha}} {{repo}}", script.display()));
        tokio::spawn(Engine::run(
            cfg,
            in_rx,
            std::sync::Arc::new(FakeRepro),
            ev_tx,
        ));

        in_tx
            .send(Ingested::Deploy(deploy_at(&sha, 1_000)))
            .unwrap();
        for at in [2_000, 3_000, 4_000] {
            in_tx
                .send(Ingested::Error(ErrorEvent {
                    request: None,
                    intake: engine_core::Intake::Reported {
                        source: "linear".into(),
                    },
                    ..event(at)
                }))
                .unwrap();
        }

        let mut withheld: Option<String> = None;
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_secs(60), ev_rx.recv()).await {
                Ok(Some(EngineEvent::Shipped(_, o))) => {
                    panic!("a human-reported failure SHIPPED on its own: {o:?}")
                }
                Ok(Some(EngineEvent::ShipWithheld(_, why))) => {
                    withheld = Some(why);
                    break;
                }
                Ok(Some(_)) => continue,
                Ok(None) | Err(_) => break,
            }
        }
        let why = withheld.expect("expected ShipWithheld for a reported-intake failure");
        assert!(why.contains("linear"), "{why}");
        assert!(
            !log_path.exists(),
            "the deploy command must never be invoked"
        );
    }

    /// `; rm -rf <marker>` embedded in a deploy-cmd template must never be
    /// shell-interpreted: the deploy command is argv-split and run via
    /// `Command::new(prog).args(args)` directly, never `sh -c`.
    #[tokio::test]
    async fn auto_ship_deploy_cmd_argv_is_never_shell_interpreted() {
        let (dir, sha) = init_repairable_fixture_repo();
        let p = dir.path();
        let scripts_dir = tempfile::tempdir().unwrap();
        let log_path = scripts_dir.path().join("deploy.log");
        let script = write_fake_deploy_script(scripts_dir.path(), &log_path);
        let marker_dir = scripts_dir.path().join("must-not-be-deleted");
        std::fs::create_dir_all(&marker_dir).unwrap();
        std::fs::write(marker_dir.join("canary.txt"), "still here").unwrap();

        let (in_tx, in_rx) = tokio::sync::mpsc::unbounded_channel();
        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut cfg = cfg_with_agent(p, Arc::new(FixingAgent));
        cfg.repair_mode = RepairMode::Auto;
        promote_test_class(p);
        cfg.deploy_cmd = Some(format!(
            "{} {{sha}} ; rm -rf {}",
            script.display(),
            marker_dir.display()
        ));
        tokio::spawn(Engine::run(
            cfg,
            in_rx,
            std::sync::Arc::new(FakeRepro),
            ev_tx,
        ));

        in_tx
            .send(Ingested::Deploy(deploy_at(&sha, 1_000)))
            .unwrap();
        for at in [2_000, 3_000, 4_000] {
            in_tx.send(Ingested::Error(event(at))).unwrap();
        }

        let mut shipped = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_secs(30), ev_rx.recv()).await {
                Ok(Some(EngineEvent::Shipped(..))) => {
                    shipped = true;
                    break;
                }
                Ok(Some(EngineEvent::ShipFailed(_, why))) => {
                    panic!("expected Shipped, got ShipFailed: {why}")
                }
                Ok(Some(_)) => continue,
                Ok(None) | Err(_) => break,
            }
        }
        assert!(
            shipped,
            "expected the deploy to succeed (the fake script always exits 0)"
        );

        assert!(marker_dir.exists(), "the marker directory must survive — a shell-interpreted `; rm -rf` would have deleted it");
        assert!(marker_dir.join("canary.txt").exists());

        let log = std::fs::read_to_string(&log_path).unwrap_or_default();
        let lines: Vec<&str> = log.lines().collect();
        assert!(lines.contains(&";"), "the literal \";\" must arrive as its own inert argv element, not be shell-interpreted: {log}");
        assert!(
            lines.contains(&"rm"),
            "\"rm\" must arrive as an inert literal argv element: {log}"
        );
        assert!(
            lines.contains(&"-rf"),
            "\"-rf\" must arrive as an inert literal argv element: {log}"
        );
    }

    // -- pure helper tests --------------------------------------------------

    #[test]
    fn build_acceptance_includes_the_captured_request_and_health_and_diff_discipline() {
        let req = CapturedRequest {
            method: "POST".into(),
            path: "/api/checkout".into(),
            content_type: None,
            body: None,
        };
        let acceptance =
            build_acceptance(&snippet_failure(), Some(&req), &TestScript::NoPackageJson);
        assert!(acceptance.iter().any(|l| l.contains("POST /api/checkout")));
        assert!(acceptance.iter().any(|l| l.contains("/health")));
        assert!(acceptance.iter().any(|l| l.contains("minimal")));
        // An app with no package.json gets no test criterion — never a
        // criterion about a suite that does not exist.
        assert!(
            !acceptance.iter().any(|l| l.contains("test script")),
            "{acceptance:?}"
        );
    }

    /// I1: `verify_repair` FAILS the whole repair when the app's own test
    /// script stops being the same runnable script, so the agent has to be
    /// told that before it edits anything — a criterion enforced but never
    /// stated is the kind of trap this product exists to remove.
    #[test]
    fn build_acceptance_names_the_apps_test_script_and_forbids_weakening_it() {
        let req = CapturedRequest {
            method: "POST".into(),
            path: "/api/checkout".into(),
            content_type: None,
            body: None,
        };
        let acceptance = build_acceptance(
            &snippet_failure(),
            Some(&req),
            &TestScript::Declared("jest --ci".to_string()),
        );
        let line = acceptance
            .iter()
            .find(|l| l.contains("jest --ci"))
            .unwrap_or_else(|| {
                panic!(
                    "the acceptance criteria must name the app's own test script: {acceptance:?}"
                )
            });
        assert!(
            line.contains("scripts.test"),
            "and must say the script itself is not the thing to change: {line}"
        );
    }

    /// With no captured request, the replay criterion must be OMITTED rather
    /// than restated against a request that does not exist — an acceptance
    /// criterion nothing will check is worse than none, because the agent
    /// optimizes for it and `verify_repair` cannot confirm it.
    #[test]
    fn build_acceptance_states_the_missing_request_instead_of_a_replay_criterion() {
        let f = trigger_failure("hyperdx");
        let acceptance = build_acceptance(&f, None, &TestScript::NoPackageJson);
        assert!(
            acceptance.iter().any(|l| l.contains("no captured request")),
            "{acceptance:?}"
        );
        assert!(
            acceptance.iter().any(|l| l.contains("trigger: hyperdx")),
            "{acceptance:?}"
        );
        assert!(
            !acceptance.iter().any(|l| l.contains("returns 2xx")),
            "no replay criterion may be stated: {acceptance:?}"
        );
        // The checks that WILL run are still stated.
        assert!(
            acceptance.iter().any(|l| l.contains("/health")),
            "{acceptance:?}"
        );
        assert!(
            acceptance.iter().any(|l| l.contains("minimal")),
            "{acceptance:?}"
        );
    }

    /// git is the record (spec §17): the note must carry the `unresolved`
    /// no-replay claim in the reproduction slot, not omit the slot — an omission
    /// reads as a truncated chain rather than as "reproduction was impossible".
    #[test]
    fn build_evidence_records_the_no_replay_claim_when_there_was_no_reproduction() {
        let f = trigger_failure("hyperdx");
        let attribution = Attribution {
            deploy: DeployRecord {
                sha: "deadbeef00".into(),
                description: "d".into(),
                author: "a".into(),
                deployed_at_ms: 1,
            },
            overlap_files: vec!["server.js".into()],
            minutes_after_deploy: 2,
            claim: Claim {
                text: "1 file overlaps".into(),
                provenance: Provenance::Inferred,
            },
        };
        let attempt = engine_repair::RepairAttempt {
            summary: "guard the null".into(),
            diff_stat: "server.js | 2 +-".into(),
        };
        let evidence = build_evidence(&f, &attribution, None, &attempt);
        assert!(evidence.contains("intake: trigger: hyperdx"), "{evidence}");
        assert!(
            evidence.contains("reproduction: no replayable request captured"),
            "{evidence}"
        );
        assert!(evidence.contains("[unresolved]"), "{evidence}");
        assert!(
            !evidence.contains("[verified]"),
            "nothing in this chain was verified against the original request: {evidence}"
        );
    }

    #[test]
    fn build_evidence_names_the_intake_on_the_snippet_path_too() {
        let f = snippet_failure();
        let attribution = Attribution {
            deploy: DeployRecord {
                sha: "deadbeef00".into(),
                description: "d".into(),
                author: "a".into(),
                deployed_at_ms: 1,
            },
            overlap_files: vec!["server.js".into()],
            minutes_after_deploy: 2,
            claim: Claim {
                text: "1 file overlaps".into(),
                provenance: Provenance::Inferred,
            },
        };
        let reproduction = Reproduction {
            sha: "deadbeef00".into(),
            reproduced: true,
            parent_clean: Some(true),
            detail: "replayed".into(),
            claims: vec![Claim {
                text: "replayed the captured request".into(),
                provenance: Provenance::Verified,
            }],
        };
        let attempt = engine_repair::RepairAttempt {
            summary: "s".into(),
            diff_stat: "d".into(),
        };
        let evidence = build_evidence(&f, &attribution, Some(&reproduction), &attempt);
        assert!(evidence.contains("intake: snippet"), "{evidence}");
        assert!(
            evidence.contains("reproduction: replayed the captured request [verified]"),
            "{evidence}"
        );
    }

    /// The per-class ladder has landed, so this is now a CEILING rather than a
    /// rung: `--repair auto` says how much authority the operator consents to,
    /// not how much a class has earned.
    #[test]
    fn repair_mode_sets_a_ceiling_on_the_autonomy_ladder() {
        assert_eq!(RepairMode::Auto.ceiling(), Rung::ActAlone);
        assert_eq!(RepairMode::Propose.ceiling(), Rung::Propose);
    }

    /// The composition that makes "earned, not configured" true: a flag cannot
    /// grant autonomy, and a streak cannot bypass consent.
    #[test]
    fn effective_authority_is_the_lower_of_consent_and_earned_rung() {
        // The operator allowed act-alone; the class has not earned it.
        assert_eq!(min_rung(Rung::ActAlone, Rung::Propose), Rung::Propose);
        // The class earned act-alone; the operator did not consent.
        assert_eq!(min_rung(Rung::Propose, Rung::ActAlone), Rung::Propose);
        // Both agree.
        assert_eq!(min_rung(Rung::ActAlone, Rung::ActAlone), Rung::ActAlone);
        // A lower earned rung wins over a lower ceiling in either order.
        assert_eq!(min_rung(Rung::Observe, Rung::ActAlone), Rung::Observe);
        assert_eq!(min_rung(Rung::ActAlone, Rung::Observe), Rung::Observe);
    }

    // -------------------------------------------------------------------------
    // The four-kind notification mapping (`notification_for`) — pure, so the
    // copy and the epistemics are pinned without a webhook or a network.
    // -------------------------------------------------------------------------

    fn notif_failure() -> Failure {
        Failure {
            intake: engine_core::Intake::Snippet,
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
                // Deliberately poisoned: the redaction assertion below leans
                // on this request existing on the failure that gets notified.
                request: Some(CapturedRequest {
                    method: "POST".into(),
                    path: "/api/checkout?api_key=SECRET123".into(),
                    content_type: Some("application/json".into()),
                    body: Some(
                        r#"{"card":"4242424242424242","secret_token":"sk_live_abc123"}"#.into(),
                    ),
                }),
                intake: engine_core::Intake::Snippet,
            },
            claim: Claim {
                text: "3 errors".into(),
                provenance: Provenance::Observed,
            },
        }
    }

    fn notif_repair() -> Repair {
        Repair {
            id: "r-notify-1".into(),
            failure_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
            sha: "deadbeef00".into(),
            branch: "drums/repair-01ARZ3ND".into(),
            agent: "claude".into(),
            summary: "widened the retry policy after an S3 latency spike".into(),
            claims: vec![Claim {
                text: "GET /health returns 200".into(),
                provenance: Provenance::Verified,
            }],
            diff_stat: "server.js | 1 +".into(),
        }
    }

    #[test]
    fn a_ready_repair_notifies_a_decision_named_for_the_service() {
        let ev = EngineEvent::RepairReady(notif_failure(), notif_repair(), 12_345);
        let (id, n) = notification_for(&ev, "shop").expect("RepairReady is one of the four");
        assert_eq!(id, "r-notify-1");
        assert_eq!(n.kind, crate::notify::Kind::Decision);
        assert_eq!(n.title, "repair ready for shop — review and ship");
        assert!(n.body.contains("widened the retry policy"), "{}", n.body);
        assert!(
            n.body.contains("drums ship 01ARZ3NDEKTSV4RRFFQ69G5FAV"),
            "the next step is named: {}",
            n.body
        );
        assert_eq!(n.repo, "shop");
    }

    /// The record's own redaction discipline holds in the courtesy copy: the
    /// failure CARRIES a captured request full of secrets, and none of it may
    /// reach a Slack channel.
    #[test]
    fn a_captured_request_never_reaches_a_notification() {
        let ev = EngineEvent::RepairReady(notif_failure(), notif_repair(), 12_345);
        let (_, n) = notification_for(&ev, "shop").unwrap();
        for text in [&n.title, &n.body] {
            assert!(
                !text.contains("4242424242424242"),
                "card number must never appear: {text}"
            );
            assert!(
                !text.contains("sk_live_abc123"),
                "secret token must never appear: {text}"
            );
            assert!(
                !text.contains("SECRET123"),
                "query-string secret must never appear: {text}"
            );
            assert!(
                !text.contains("/api/checkout"),
                "not even the path is included: {text}"
            );
        }
    }

    #[test]
    fn an_evaluated_bet_notifies_a_learning_with_belief_verdict_and_the_confidence_line_verbatim() {
        let outcome = engine_core::change::OutcomeRecorded {
            change: engine_core::change::ChangeId("chg_1".into()),
            outcome: engine_core::evaluation::Outcome::Measured {
                direction: engine_core::evaluation::Direction::Positive,
                from: 0.61,
                to: 0.72,
                entries: 240,
                guardrails: engine_core::evaluation::Guardrails::Held,
            },
            unread_guardrails: vec![],
            measured_at_ms: 1,
        };
        let verdict = engine_core::bet::Verdict::from_outcome(
            &outcome,
            engine_core::bet::RolloutDesign::Full,
        );
        let ev = EngineEvent::BetEvaluated {
            bet: "bet_1".into(),
            belief: "batching uploads will cut checkout errors".into(),
            verdict: verdict.clone(),
            measured: Some((0.61, 0.72, 240)),
        };
        let (id, n) = notification_for(&ev, "shop").unwrap();
        assert_eq!(id, "bet_1");
        assert_eq!(n.kind, crate::notify::Kind::Learning);
        assert!(
            n.title
                .contains("batching uploads will cut checkout errors"),
            "{}",
            n.title
        );
        assert!(
            n.title.contains("supported"),
            "the verdict keeps its own vocabulary: {}",
            n.title
        );
        assert!(n.body.contains("0.61 → 0.72 over 240"), "{}", n.body);
        assert!(
            n.body.contains(&format!(
                "causal confidence low: {}",
                verdict.causal_confidence.basis
            )),
            "the confidence line arrives verbatim, basis and all: {}",
            n.body
        );
    }

    #[test]
    fn a_rate_shift_observation_notifies_a_working_watching_line_with_the_deploy_sha() {
        use engine_core::evaluation::{Metric, Sample};
        use engine_core::observation::{Kind as OKind, Measure, Observation, Source, Window};
        let o = Observation::fact(
            "obs_1",
            Source::Runtime,
            OKind::RateShift {
                previous: 0.10,
                since_deploy: Some("abc1234def".into()),
            },
            Window::new(0, 3_600_000).unwrap(),
            3_600_000,
        )
        .with_measure(Measure {
            metric: Metric::ErrorEventRate,
            sample: Sample {
                value: 0.42,
                entries: 12,
            },
        });
        let ev = EngineEvent::ObservationRecorded(o);
        let (id, n) = notification_for(&ev, "shop").unwrap();
        assert_eq!(id, "obs_1");
        assert_eq!(n.kind, crate::notify::Kind::Working);
        assert_eq!(n.title, "watching an error-rate shift after deploy abc123");
        assert!(
            n.body.contains("0.10/h → 0.42/h over 12 events"),
            "{}",
            n.body
        );
    }

    #[test]
    fn an_unmeasured_outcome_notifies_an_fyi_carrying_the_honest_sentence() {
        let u = engine_core::evaluation::Unmeasured::NotEnoughTraffic {
            entries: 12,
            needed: 100,
        };
        let ev = EngineEvent::OutcomeMeasured(engine_core::change::OutcomeRecorded {
            change: engine_core::change::ChangeId("chg_1".into()),
            outcome: engine_core::evaluation::Outcome::Unmeasured(u.clone()),
            unread_guardrails: vec![],
            measured_at_ms: 1,
        });
        let (id, n) = notification_for(&ev, "shop").unwrap();
        assert_eq!(id, "chg_1");
        assert_eq!(n.kind, crate::notify::Kind::Fyi);
        assert!(n.title.contains("nothing needs you"), "{}", n.title);
        assert_eq!(
            n.body,
            u.sentence(),
            "the honest unmeasured sentence, not a paraphrase"
        );
    }

    /// A MEASURED outcome is not one of the four moments — its story reaches
    /// Slack when the bet above it is evaluated (Learning), never twice.
    #[test]
    fn a_measured_outcome_and_the_rest_of_the_pipeline_do_not_notify() {
        let measured = EngineEvent::OutcomeMeasured(engine_core::change::OutcomeRecorded {
            change: engine_core::change::ChangeId("chg_1".into()),
            outcome: engine_core::evaluation::Outcome::Measured {
                direction: engine_core::evaluation::Direction::Positive,
                from: 0.1,
                to: 0.2,
                entries: 200,
                guardrails: engine_core::evaluation::Guardrails::Held,
            },
            unread_guardrails: vec![],
            measured_at_ms: 1,
        });
        assert!(notification_for(&measured, "shop").is_none());
        assert!(notification_for(&EngineEvent::FailureDetected(notif_failure()), "shop").is_none());
        assert!(
            notification_for(
                &EngineEvent::Repairing(notif_failure(), "claude".into()),
                "shop"
            )
            .is_none(),
            "at most one notification per pipeline — the Decision at the end, not a play-by-play"
        );
    }

    #[test]
    fn a_drafted_bet_notifies_a_working_asking_for_confirmation() {
        let ev = EngineEvent::BetDrafted {
            bet: "bet_9".into(),
            belief: "surfacing the retry button will cut abandonment".into(),
            by: "claude".into(),
        };
        let (id, n) = notification_for(&ev, "shop").unwrap();
        assert_eq!(id, "bet_9");
        assert_eq!(n.kind, crate::notify::Kind::Working);
        assert_eq!(
            n.title,
            "drafted a bet for your confirmation: surfacing the retry button will cut abandonment"
        );
        assert!(n.body.contains("drums bet confirm bet_9"), "{}", n.body);
    }

    /// The slow loop's one Slack moment: a matured revisit that DRIFTED is a
    /// Learning carrying both readings; an un-drifted revisit is FYI noise
    /// and builds nothing at all.
    #[test]
    fn a_drifted_revisit_notifies_a_learning_and_an_undrifted_one_stays_silent() {
        use engine_core::evaluation::{Direction, Guardrails, Metric, Outcome};
        let revisit = |drifted: bool| EngineEvent::RevisitMeasured {
            change: "chg_1".into(),
            horizon_days: 30,
            drifted,
            outcome: Outcome::Measured {
                direction: Direction::Neutral,
                from: 0.11,
                to: 0.12,
                entries: 168,
                guardrails: Guardrails::Held,
            },
            metric: Metric::ErrorEventRate,
            was: Some(Direction::Positive),
        };

        let (id, n) =
            notification_for(&revisit(true), "shop").expect("a drifted revisit is a Learning");
        assert_eq!(id, "revisit:chg_1:30d");
        assert_eq!(n.kind, crate::notify::Kind::Learning);
        assert_eq!(
            n.title,
            "a prior bet matured: at 30 days the metric no longer shows the move the window showed"
        );
        assert!(n.body.contains("chg_1"), "{}", n.body);
        assert!(
            n.body.contains("0.11/h → 0.12/h over 168h"),
            "the numbers arrive: {}",
            n.body
        );
        assert!(
            n.body.contains("(was: improved at close)"),
            "both readings, never a re-labeled verdict: {}",
            n.body
        );
        for text in [&n.title, &n.body] {
            assert_eq!(
                crate::notify::contains_banned_word(text),
                None,
                "banned word in {text:?}"
            );
        }

        assert!(
            notification_for(&revisit(false), "shop").is_none(),
            "an un-drifted revisit is FYI noise and sends nothing"
        );
    }

    /// The vocabulary ban ("worked"/"proved"/"caused") holds across every
    /// notification this module constructs — verdict language stays
    /// supported/not supported/inconclusive with its causal-confidence line.
    #[test]
    fn no_constructed_notification_uses_a_banned_word() {
        let unmeasured = EngineEvent::OutcomeMeasured(engine_core::change::OutcomeRecorded {
            change: engine_core::change::ChangeId("chg_1".into()),
            outcome: engine_core::evaluation::Outcome::Unmeasured(
                engine_core::evaluation::Unmeasured::NoBaseline {
                    entries: 3,
                    needed: 100,
                },
            ),
            unread_guardrails: vec![],
            measured_at_ms: 1,
        });
        let evaluated = EngineEvent::BetEvaluated {
            bet: "bet_1".into(),
            belief: "batching uploads will cut checkout errors".into(),
            verdict: engine_core::bet::Verdict {
                support: engine_core::bet::Support::NotSupported,
                causal_confidence: engine_core::bet::CausalConfidence::from_design(
                    engine_core::bet::RolloutDesign::Full,
                ),
                unread_guardrails: vec![],
            },
            measured: Some((0.61, 0.55, 240)),
        };
        let drafted = EngineEvent::BetDrafted {
            bet: "bet_9".into(),
            belief: "b".into(),
            by: "claude".into(),
        };
        let ready = EngineEvent::RepairReady(notif_failure(), notif_repair(), 1);
        for ev in [&unmeasured, &evaluated, &drafted, &ready] {
            let (_, n) = notification_for(ev, "shop").expect("all four must map");
            for text in [&n.title, &n.body] {
                assert_eq!(
                    crate::notify::contains_banned_word(text),
                    None,
                    "banned word in {:?} for {}",
                    text,
                    kind_name(ev)
                );
            }
        }
    }

    // -------------------------------------------------------------------------
    // The proactive-draft gates (`should_draft`) — pure, so consent,
    // capability and no-duplication are each pinned without an agent or a
    // tick.
    // -------------------------------------------------------------------------

    fn lines_with_a_bet(
        status: Option<engine_core::bet::BetStatus>,
    ) -> Vec<(String, serde_json::Value)> {
        let bet = engine_core::bet::ProductBet::new(
            "bet_1",
            "belief",
            "because",
            engine_core::hypothesis::HypothesisId("hyp_1".into()),
            1,
        )
        .unwrap();
        let mut lines = vec![(
            engine_core::bet::RECORD_KIND.to_string(),
            serde_json::to_value(&bet).unwrap(),
        )];
        if let Some(status) = status {
            let changed = engine_core::bet::BetStatusChanged {
                bet: bet.id.clone(),
                status,
            };
            lines.push((
                engine_core::bet::STATUS_KIND.to_string(),
                serde_json::to_value(&changed).unwrap(),
            ));
        }
        lines
    }

    #[test]
    fn should_draft_refuses_when_the_consent_flag_is_off_and_names_the_key() {
        let reason = should_draft(&[], false, true).expect("consent is the first gate");
        assert!(reason.contains("proactive_draft"), "{reason}");
        assert!(reason.contains("config.toml"), "the fix is named: {reason}");
        assert!(
            reason.contains("tokens"),
            "the cost — the reason for the gate — is named: {reason}"
        );
    }

    #[test]
    fn should_draft_refuses_without_an_agent_and_names_every_way_to_get_one() {
        let reason = should_draft(&[], true, false).expect("no agent, no draft");
        assert!(reason.contains("agent_cmd"), "{reason}");
        assert!(reason.contains("DRUMS_AGENT_CMD"), "{reason}");
    }

    /// One open draft at a time: a bet sitting in `proposed` means the human
    /// has not answered yet, and drafting another on top is a nag, not help.
    #[test]
    fn should_draft_refuses_while_a_proposed_bet_awaits_confirmation() {
        let lines = lines_with_a_bet(None); // a fresh bet row folds to Proposed
        let reason = should_draft(&lines, true, true).expect("must not duplicate");
        assert!(reason.contains("awaiting confirmation"), "{reason}");
        assert!(
            reason.contains("drums bet confirm"),
            "the way to unblock is named: {reason}"
        );
    }

    /// A confirmed or declined bet is an ANSWERED one — it must not block the
    /// next draft, or one decision would switch proactive drafting off
    /// forever.
    #[test]
    fn should_draft_allows_drafting_once_every_bet_is_answered() {
        assert_eq!(
            should_draft(&[], true, true),
            None,
            "an empty record has nothing proposed"
        );
        let confirmed = lines_with_a_bet(Some(engine_core::bet::BetStatus::Confirmed));
        assert_eq!(should_draft(&confirmed, true, true), None);
        let declined = lines_with_a_bet(Some(engine_core::bet::BetStatus::Declined {
            reason: "not now".into(),
        }));
        assert_eq!(should_draft(&declined, true, true), None);
    }
}
