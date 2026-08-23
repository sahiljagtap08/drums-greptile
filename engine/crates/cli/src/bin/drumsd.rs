//! `drumsd` — the same detect → attribute → reproduce → repair → verify →
//! propose/auto-ship pipeline `drums watch` runs, wired for service mode
//! instead of a foreground terminal session: no TUI, no plain stdout
//! narration — structured `tracing` log lines instead (redirected to
//! `.drums/drumsd.log` by the process that spawned this one, `drums daemon
//! start`); and, on startup, its own state rebuilt from this repo's record
//! before the detector ever sees a live event, so a restart of THIS PROCESS
//! doesn't re-open a failure that already has a `repair_ready`/`shipped`
//! line (see `drums_watch::restore`).
//!
//! Deliberately thin: every pipeline primitive here (`Engine::run`,
//! `engine_ingest::{IngestState, router}`, `CliRepairAgent::detect`,
//! `LocalProcessReproducer`, `render::render`) is the exact same shared code
//! `drums watch`'s `main.rs` composes — this file is a different WIRING of
//! those pieces, not a second implementation of any of them. This binary is
//! never meant to be run directly by a human; `drums daemon start` spawns it
//! detached and manages its pidfile/logfile — see `drums_watch::daemon`.
//!
//! Known gap (documented, not silent): unlike `drums watch`, this binary
//! does not run the `--app-root` auto-derivation forwarding task
//! (`main.rs`'s `spawn_app_root_deriver`) when `--app-root` is omitted — an
//! omitted `--app-root` here means no stack-path stripping at all, same as
//! `drums watch --app-root ""` would. `drums daemon start` always passes
//! through whatever `--app-root`/config value it resolved; auto-derivation
//! for the daemon path is future work, not part of this change.

use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, ValueEnum};
use drums_watch::engine::{Engine, EngineConfig, RepairMode};
use drums_watch::signal::wait_for_stop_signal;
use drums_watch::{render, restore};
use engine_repair::CliRepairAgent;
use engine_repro::LocalProcessReproducer;

/// Mirrors `main.rs`'s own `RepairModeArg` — kept as a small, separate copy
/// rather than shared, since it's pure clap glue (not pipeline logic) and
/// living in each binary lets each `--help` describe itself independently.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum RepairModeArg {
    Propose,
    Auto,
}

/// Every value here arrives ALREADY resolved: `drums daemon start` is the
/// one place flags/config/defaults get merged (see
/// `drums_watch::config::resolve`) — deliberately, because that resolution
/// (and its honest refusals) needs to happen where a human is watching the
/// command run, not silently inside a detached child whose stdout nobody is
/// looking at. This parser exists only to receive the already-resolved
/// result on drumsd's own argv.
#[derive(Parser)]
#[command(
    name = "drumsd",
    about = "drums daemon — the loop as a detached, restart-aware service"
)]
struct Args {
    #[arg(long)]
    repo: PathBuf,
    #[arg(long)]
    ingest_port: u16,
    #[arg(long)]
    threshold: usize,
    #[arg(long)]
    window_secs: u64,
    #[arg(long)]
    app_root: Option<String>,
    #[arg(long, value_enum)]
    repair: RepairModeArg,
    #[arg(long)]
    deploy_cmd: Option<String>,
    #[arg(long)]
    check_url: Option<String>,
    /// Take reported issues (Linear/Agentation via the local ingest) and
    /// attempt propose-only repairs for them. Safe detached — the repair is
    /// local, needs no remote credential, and a restart mid-attempt loses
    /// only a worktree, exactly like `drums watch`'s own ctrl-c.
    #[arg(long)]
    repair_reported: bool,
}

/// Installs the process's one `tracing` subscriber: structured lines with
/// timestamps, no ANSI (task 5 — "a human can read [the log] later", and the
/// file must stay plain text even though a terminal COULD render color),
/// writing to stdout — which `drums daemon start`'s `spawn_detached` already
/// redirected to `.drums/drumsd.log` before this process ever execs, so
/// nothing in this file touches the log path directly.
fn install_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stdout)
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .with_target(false)
        .try_init();
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    install_tracing();
    let args = Args::parse();

    if args.repair == RepairModeArg::Auto && args.deploy_cmd.is_none() {
        // Mirrors `main.rs`'s `validate_watch_repair_flags`: `drums daemon
        // start` already enforces this via `config::resolve` before ever
        // spawning this process, so reaching this arm means something
        // spawned `drumsd` directly with a malformed argv — refuse loudly
        // rather than silently downgrading to propose.
        tracing::error!("--repair auto requires --deploy-cmd; refusing to start");
        std::process::exit(2);
    }

    let repo = args.repo;
    let state_dir = repo.join(".drums");
    std::fs::create_dir_all(&state_dir)?;
    let record_path = state_dir.join("record.jsonl");

    tracing::info!(repo = %repo.display(), ingest_port = args.ingest_port, threshold = args.threshold, window_secs = args.window_secs, "drumsd starting");

    // Restart-idempotence (the difference between a script and a service):
    // rebuild which signatures this repo's OWN record already shows crossed
    // the threshold — and, in particular, any that already earned a
    // `repair_ready`/`shipped` line — before the detector this process is
    // about to run ever observes a live event.
    let window_ms = args.window_secs * 1_000;
    let app_root = args.app_root.clone().unwrap_or_default();
    let initial_opened =
        restore::rebuild_opened_signatures(&record_path, args.threshold, window_ms, &app_root);
    tracing::info!(
        restored_signatures = initial_opened.len(),
        "rebuilt detector state from the existing record"
    );

    let (ingest_state, ingest_rx) = engine_ingest::IngestState::new(record_path.clone());
    let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();

    // Same agent resolution `drums watch` uses: `DRUMS_AGENT_CMD`, then
    // `claude`, then `codex` on PATH; `None` means honestly no repair is
    // ever attempted.
    let repair_agent =
        CliRepairAgent::detect().map(|a| Arc::new(a) as Arc<dyn engine_repair::RepairAgent>);
    let agent_label = repair_agent
        .as_ref()
        .map(|a| a.name().to_string())
        .unwrap_or_else(|| "none".to_string());
    tracing::info!(agent = %agent_label, "repair agent resolved");

    let repair_mode = match args.repair {
        RepairModeArg::Propose => RepairMode::Propose,
        RepairModeArg::Auto => RepairMode::Auto,
    };
    let repair_banner = match repair_mode {
        RepairMode::Propose => {
            "repairs propose only — nothing ships without `drums ship`".to_string()
        }
        RepairMode::Auto => format!(
            "repairs auto-ship via `{}`",
            args.deploy_cmd.as_deref().unwrap_or("")
        ),
    };
    tracing::info!(mode = %repair_banner, "repair mode");

    // Loaded once; the behavior source, the notification webhook and the
    // proactive-draft consent are all config-file-driven (no drumsd argv for
    // any of them — `drums daemon start` resolves flags, but these three
    // deliberately have none), so the daemon reads the same keys `drums
    // watch` does and behaves identically.
    let file_cfg = drums_watch::config::load(&repo)
        .ok()
        .flatten()
        .unwrap_or_default();
    let behavior = drums_watch::behavior_source::build(&file_cfg);
    let notify = drums_watch::notify::resolve_webhook(file_cfg.slack_webhook_url.as_deref())
        .map(drums_watch::notify::Sink::new);
    if notify.is_some() {
        tracing::info!(
            "notifications: slack configured — decisions and learnings will be delivered"
        );
    }
    let proactive_draft = file_cfg.proactive_draft.unwrap_or(false);
    let draft_agent = drums_watch::draft::agent_template(file_cfg.agent_cmd.as_deref());
    // Record sync: the same config key and the same resolution `drums watch`
    // applies — opt-in via `sync_record = true` plus a `drums login`
    // credential; the flag without the credential is narrated and sync stays
    // off, never a startup failure.
    let sync = if file_cfg.sync_record.unwrap_or(false) {
        match drums_watch::sync::RecordSync::from_login(&repo, record_path.clone()) {
            Ok(s) => {
                tracing::info!(plane = %s.app_base(), repo = %s.repo_slug(), "record sync: on (stored redacted, so it arrives redacted)");
                Some(Arc::new(s))
            }
            Err(why) => {
                tracing::warn!(%why, "record sync: sync_record = true but the credential did not resolve — running with sync off");
                None
            }
        }
    } else {
        None
    };
    let tracker_poll = drums_watch::login::load().ok().flatten().and_then(|creds| {
        let console = drums_watch::login::console_url();
        if !creds.console_url.is_empty() && creds.console_url != console {
            return None;
        }
        Some(drums_watch::tracker_poll::TrackerPoll::new(
            console,
            creds.token,
            args.ingest_port,
        ))
    });
    let cfg = EngineConfig {
        behavior,
        notify,
        proactive_draft,
        tracker_poll,
        draft_agent,
        // drumsd does not surface --propose-pr yet: a detached daemon that
        // opens pull requests needs its `gh` auth verified at start and
        // re-verified after a credential rotation, and neither is built.
        proposal: None,
        proposal_base: "main".to_string(),
        // Surfaced since the proactive-modes round: the holds that keep
        // `proposal` and `dispatch` out of a detached daemon (gh auth and CI
        // credential re-verification after rotation, restart semantics for
        // in-flight remote jobs) do not apply to a LOCAL, propose-only
        // repair of a reported issue.
        repair_reported: args.repair_reported,
        // drumsd does not surface --boot-cmd yet: config.rs has no such field.
        // None keeps the historical `node <entry>` boot; wiring it is the daemon
        // lane's follow-up, and a FastAPI app cannot be verified by drumsd until then.
        boot_cmd: None,
        repo: repo.clone(),
        threshold: args.threshold,
        window_ms,
        app_root,
        record_path,
        repair_agent,
        repair_mode,
        deploy_cmd: args.deploy_cmd.clone(),
        check_url: args.check_url.clone(),
        repair_boot_timeout_ms: 15_000,
        initial_opened,
        // Same reason as `proposal`: drumsd does not surface
        // `--dispatch-repairs` yet. A detached daemon dispatching repairs into
        // somebody's CI needs its credential re-checked after a rotation and a
        // story for what a restart does to a job already in flight
        // (`docs/RUNNER.md` §9a item 5), and neither is built. `drums watch`
        // is where the seam is exercised first.
        dispatch: None,
        sync,
    };
    let render_ctx = render::RenderContext {
        repo: repo.clone(),
        deploy_cmd: args.deploy_cmd.clone(),
    };
    let reproducer = Arc::new(LocalProcessReproducer {
        boot_timeout_ms: 15_000,
        boot_cmd: None,
    });
    tokio::spawn(Engine::run(cfg, ingest_rx, reproducer, ev_tx));

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", args.ingest_port)).await?;
    let bound_port = listener.local_addr()?.port();
    tracing::info!(port = bound_port, "ingest server listening");
    tokio::spawn(async move {
        let _ = axum::serve(listener, engine_ingest::router(ingest_state)).await;
    });

    // Narrate every engine event with the SAME text `drums watch --plain`
    // prints (task 5: "the same narration content the TUI shows") — but as
    // log lines, not stdout prose. `render::render` never emits ANSI in its
    // plain form (only the TUI's own view layer does), so this needs no
    // extra stripping to satisfy "no ANSI in the file".
    let narrate = async {
        while let Some(ev) = ev_rx.recv().await {
            for line in render::render(&ev, &render_ctx).lines() {
                if !line.is_empty() {
                    tracing::info!("{line}");
                }
            }
        }
    };

    // The SAME stop-signal future `drums watch` waits on (ctrl-c or, on
    // Unix, SIGTERM) — `drums daemon stop` sends SIGTERM and this is what
    // receives it. Returning from `main` after this drops the tokio
    // runtime, which drops the spawned `Engine::run` task and the ingest
    // server task; any in-flight reproduction's or repair's child process
    // group guard and worktree guard drop with them — identical teardown to
    // `drums watch`'s own ctrl-c/SIGTERM path, because it's the identical
    // code doing it.
    tokio::select! {
        _ = narrate => {
            tracing::warn!("engine event channel closed — the engine task ended on its own");
        }
        _ = wait_for_stop_signal() => {
            tracing::info!("stop signal received — shutting down (in-flight reproduction/repair processes killed, worktrees removed unless a failed repair asked to keep one for inspection)");
        }
    }
    Ok(())
}
