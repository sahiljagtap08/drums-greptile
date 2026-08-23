use std::io::{IsTerminal, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};
use drums_watch::engine::{Engine, EngineConfig, RepairMode};
use drums_watch::signal::wait_for_stop_signal;
use drums_watch::{
    account, app_root, bet_cmd, change_cmd, daemon, digest, doctor, draft, hypothesize, license,
    login, notify, open, record_cmd, render, repair_cmd, ship, telemetry, ui, why,
};
use engine_repair::CliRepairAgent;
use engine_repro::LocalProcessReproducer;

/// What `drums --version` prints: the crate version, then the commit and the
/// target the binary was actually built from (see `build.rs` for why those two
/// are not decoration).
///
/// A `const` rather than a formatted string at startup because clap wants a
/// `&'static str`, and because there is nothing here to compute at runtime —
/// every part was decided when the binary was built, which is the point.
const VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("DRUMS_BUILD_SHA"),
    ", ",
    env!("DRUMS_BUILD_TARGET"),
    ")"
);

#[derive(Subcommand)]
enum HypothesisAction {
    /// Accept: a change may now cite this hypothesis.
    Accept {
        id: String,
        /// Repo to operate on (default: the current directory).
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Reject, with the reason recorded beside it.
    Reject {
        id: String,
        /// Why not — required, because the reason is the useful part.
        #[arg(long)]
        reason: String,
        /// Repo to operate on (default: the current directory).
        #[arg(long)]
        repo: Option<PathBuf>,
    },
}

#[derive(clap::Subcommand)]
// Create carries the whole pre-registration; parsed once and consumed, so the
// variant-size spread has no resident cost.
#[allow(clippy::large_enum_variant)]
enum BetAction {
    /// Draft a Product Bet: what we believe, why, for whom, and what we
    /// expect to happen — captured before the outcome is known. Writes the
    /// bet and the hypothesis under it; nothing is committed to until
    /// `drums bet confirm`.
    Create {
        /// What we believe will get better, and how.
        #[arg(long)]
        belief: String,
        /// Why — the evidence and reasoning, in your own words.
        #[arg(long)]
        because: String,
        /// Who this should affect. Optional; absent means everyone.
        #[arg(long = "for")]
        audience: Option<String>,
        /// A road not taken — repeatable. Cheap now, priceless in a year.
        #[arg(long = "alt")]
        alternatives: Vec<String>,
        /// Observation id(s) the belief cites — at least one; repeatable.
        #[arg(long = "cite", required = true)]
        cites: Vec<String>,
        /// Expectation: a human name for what is being evaluated.
        #[arg(long)]
        name: String,
        /// Expectation: the event that means someone entered.
        #[arg(long)]
        start: String,
        /// Expectation: the event that means they finished.
        #[arg(long)]
        success: String,
        /// Expectation: the metric that should move — completion_rate,
        /// time_to_complete, error_rate, step_retries, abandonment,
        /// support_contacts, or error_event_rate.
        #[arg(long)]
        metric: String,
        /// A metric that may not get worse — `error_rate` or
        /// `error_rate:0.02`. Repeatable.
        #[arg(long = "guardrail")]
        guardrails: Vec<String>,
        /// Days to watch after rollout (default 7).
        #[arg(long)]
        window_days: Option<u32>,
        /// Entries required before a comparison is reported (default 100).
        #[arg(long)]
        min_entries: Option<u32>,
        /// Smallest move worth calling a direction (default 0.02).
        #[arg(long)]
        min_effect: Option<f64>,
        /// Repo to operate on (default: the current directory).
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Confirm a proposed bet — the pre-registration moment. The belief goes
    /// on record, timestamped, before the outcome exists, and the hypothesis
    /// under it is accepted so a change can proceed.
    Confirm {
        id: String,
        /// Repo to operate on (default: the current directory).
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Decline a proposed bet, with the reason. Kept in the record — a
    /// decision not to act is a decision too.
    Decline {
        id: String,
        #[arg(long)]
        reason: String,
        /// Repo to operate on (default: the current directory).
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Record what the team learned from an evaluated (or declined) bet —
    /// the one field only a human can write.
    Learn {
        id: String,
        #[arg(long)]
        note: String,
        /// Repo to operate on (default: the current directory).
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Show one bet as its card (with chain, priors, and learnings), or all
    /// bets newest-first when no id is given.
    Show {
        id: Option<String>,
        /// Repo to operate on (default: the current directory).
        #[arg(long)]
        repo: Option<PathBuf>,
    },
}

#[derive(Parser)]
#[command(
    name = "drums",
    about = "Software that maintains itself",
    version = VERSION
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

/// `--repair propose` (default) stops a completed repair at `drums ship
/// <id>` for a human. `--repair auto` continues straight into shipping once
/// verification passes — spec §19 "repairs default to PROPOSE; autonomous
/// shipping is an explicit opt-in".
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum RepairModeArg {
    Propose,
    Auto,
}

impl From<RepairModeArg> for RepairMode {
    fn from(m: RepairModeArg) -> Self {
        match m {
            RepairModeArg::Propose => RepairMode::Propose,
            RepairModeArg::Auto => RepairMode::Auto,
        }
    }
}

/// `drums digest --to <target>` (spec §14 "the morning message"). `Stdout`
/// is the default: it works the first night, before any Slack app has been
/// installed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum DigestTargetArg {
    Stdout,
    Slack,
}

#[derive(Subcommand)]
enum Commands {
    /// Watch a repository's deployments: detect failures, attribute them,
    /// reproduce them, and — when a repair agent is available — repair and
    /// verify them. Repairs default to PROPOSE: a completed repair stops
    /// for a human at `drums ship <id>`; nothing is shipped automatically.
    ///
    /// Needs an account: run `drums login` once first, so the repairs Drums
    /// makes belong to somebody. Set `DRUMS_NO_ACCOUNT=1` to run the loop
    /// with no account at all — for CI and for air-gapped machines, which
    /// cannot sign in and must still be able to run this.
    Watch {
        /// Port the ingest server listens on (127.0.0.1 only).
        #[arg(long, default_value_t = 7787)]
        ingest_port: u16,
        /// Repo to operate on (default: the current directory).
        #[arg(long)]
        repo: Option<PathBuf>,
        /// Same-signature error events within the window before a failure opens.
        #[arg(long, default_value_t = 3)]
        threshold: usize,
        /// Detection window, in seconds.
        #[arg(long, default_value_t = 60)]
        window_secs: u64,
        /// Directory the monitored app's process actually runs from, if it
        /// differs from `--repo` (e.g. a deploy step that checks out a
        /// separate working copy rather than running the watched repo in
        /// place). Stack-trace paths reported by the live app are stripped
        /// of this prefix so they line up with the repo-root-relative paths
        /// reproduction computes from its own worktree. When omitted, the
        /// prefix is auto-derived per failure from its first error event's
        /// own stack trace (falls back to no stripping if nothing under
        /// `--repo` matches). An explicit `--app-root` always wins over
        /// auto-derivation.
        #[arg(long)]
        app_root: Option<PathBuf>,
        /// `propose` (default): a completed repair stops at `drums ship
        /// <id>` for a human. `auto`: continue straight into shipping once
        /// verification passes — requires `--deploy-cmd`.
        #[arg(long, value_enum, default_value_t = RepairModeArg::Propose)]
        repair: RepairModeArg,
        /// Command template run to deploy a repair's sha. `{sha}`/`{repo}`
        /// are substituted, then argv-split (no shell). Required for
        /// `--repair auto`; also usable by a separately-run `drums ship`.
        ///
        /// Contract: the command must be NON-INTERACTIVE. Its stdin is
        /// `/dev/null`, so anything that prompts gets EOF; a program that
        /// opens `/dev/tty` instead (`ssh` without `-n`/`BatchMode`, `sudo`
        /// without `-n`, `git` credential prompts) will stall until the
        /// 10-minute timeout. It is NOT run through a shell and NOT run with
        /// `--repo` as its working directory — it inherits `drums`' own cwd,
        /// so use absolute paths. And it is expected to EXIT once the deploy
        /// is handed off (backgrounding a long-lived server is fine); its
        /// exiting 0 means the command succeeded, never that the service is
        /// up and serving. Use `--check-url` for that evidence.
        #[arg(long)]
        deploy_cmd: Option<String>,
        /// URL checked after `--repair auto` deploys: GET expects 200, and
        /// the originally failing request is replayed against its origin.
        /// With no `--check-url`, a successful auto-ship is still honestly
        /// marked `unresolved` for "was the deployed instance actually
        /// healthy" — the deploy command running is not itself that
        /// evidence.
        #[arg(long)]
        check_url: Option<String>,
        /// Command used to boot the monitored app for reproduction and
        /// repair-verification, in place of the built-in `node
        /// <auto-discovered entry>` contract. The literal token `{port}` is
        /// substituted, then argv-split (no shell, same discipline as
        /// `--deploy-cmd`'s `{sha}`/`{repo}`) — Drums assigns the port
        /// itself and polls `/health` for readiness rather than waiting for
        /// a "listening" line, since a boot-cmd app has no reason to print
        /// one. Example: `uvicorn app.main:app --host 127.0.0.1 --port
        /// {port}`. Omit to keep the original node-only behavior.
        #[arg(long)]
        boot_cmd: Option<String>,
        /// Force the plain, line-based §7 narration even on a real terminal
        /// (the default there is the live TUI). Always in effect — TUI or
        /// not — when stdout isn't a terminal at all (a pipe, a file, CI);
        /// this flag only matters for an interactive terminal. `DRUMS_PLAIN=1`
        /// does the same thing, for scripts that can't easily pass a flag.
        #[arg(long)]
        plain: bool,
        /// Requires `Authorization: Bearer <token>` on every read-only
        /// `/v1/*` GET route (`/v1/status`, `/v1/failures`, `/v1/record`,
        /// ...) served alongside ingest on the same port. `POST /v1/deploys`
        /// / `POST /v1/events` are unaffected — this only gates reads.
        /// `drums watch` binds `127.0.0.1` only today; a token is REQUIRED
        /// before this daemon is ever bound to anything but loopback (the
        /// record, claims and daemon config these routes expose would
        /// otherwise be reachable with no gate at all).
        #[arg(long)]
        api_token: Option<String>,
        /// Open a pull request carrying the repair's evidence as soon as a
        /// repair is ready — the surface where a human actually encounters
        /// the work. Drives the team's own authenticated `gh` CLI, so Drums
        /// needs no token and stores no credential. Refuses at startup if
        /// `gh` is missing or logged out, rather than discovering it after a
        /// repair has already been produced.
        #[arg(long)]
        propose_pr: bool,
        /// Branch a proposed pull request targets. Only meaningful with
        /// `--propose-pr`.
        #[arg(long, default_value = "main")]
        pr_base: String,
        /// Also attempt a repair for HUMAN-REPORTED issues arriving on
        /// `/v1/adapters/linear` and `/v1/adapters/agentation`. The bar is
        /// non-regression (the build and the test suite are no worse than
        /// before), never resolution: Drums has no visual check, so whether
        /// the change actually fixes what someone described stays
        /// `unresolved` and a human confirms it. Always propose-only — a
        /// reported intake has nothing to replay, so the ship gate refuses it
        /// whatever `--repair` says. Off by default: an agent editing the repo
        /// because someone filed a ticket is a far bigger step than repairing
        /// a failure Drums reproduced itself.
        #[arg(long)]
        repair_reported: bool,
        /// Hand each REPAIR to the Drums control plane instead of running it
        /// on this machine: it dispatches the repair into your own GitHub
        /// Actions, where your agent credential and your code already live.
        /// Requires `drums login`, a repository connected at app.drums.sh, and
        /// `.github/workflows/drums-repair.yml` committed on your default
        /// branch.
        ///
        /// REPRODUCTION STAYS HERE. Detection, attribution and reproduction all
        /// still run locally, and a failure that did not reproduce is never
        /// dispatched — Drums only repairs failures it made happen again, and
        /// that is not a property it will delegate to a runner.
        ///
        /// The name says what actually moves. `--remote` or `--hosted` would
        /// imply the whole loop went somewhere else, which is the one thing
        /// this must not be read as.
        ///
        /// By default every dispatch asks a person first (the console opens an
        /// approval and prints the link). It stops asking for a failure class
        /// that has EARNED act-alone and only when `--repair auto` says you
        /// consent to it.
        #[arg(long)]
        dispatch_repairs: bool,
    },
    /// Ship a completed repair: run the deploy command at the repair's sha,
    /// then whatever post-deploy checks `--check-url` allows. Refuses
    /// honestly if no repair is ready for `failure-id`, or it was already
    /// shipped/reverted. Reversible: `drums revert <failure-id>`.
    Ship {
        failure_id: String,
        /// Command template run to deploy. `{sha}`/`{repo}` are
        /// substituted, then argv-split (no shell). Must be
        /// NON-INTERACTIVE: stdin is `/dev/null`, cwd is `drums`' own (use
        /// absolute paths), and exiting 0 means the command succeeded — not
        /// that the service is up. See `drums watch --help` for the full
        /// contract.
        #[arg(long)]
        deploy_cmd: String,
        /// URL checked after deploy: GET expects 200, and (when the
        /// original request was recorded) it's replayed against this URL's
        /// origin. Without it, a successful deploy is honestly recorded as
        /// `unresolved` for "is the deployed instance actually healthy".
        #[arg(long)]
        check_url: Option<String>,
        /// Repo to operate on (default: the current directory).
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Revert a shipped repair: roll back to the deploy that preceded the
    /// one this failure was attributed to — undoing both the buggy deploy
    /// and its repair. Refuses honestly if nothing was shipped for
    /// `failure-id`, it was already reverted, or no prior deploy exists.
    Revert {
        failure_id: String,
        /// Command template run to deploy the ROLLBACK sha. Same contract as
        /// `drums ship --deploy-cmd`: `{sha}`/`{repo}` substituted, argv-split
        /// (no shell), non-interactive, absolute paths.
        #[arg(long)]
        deploy_cmd: String,
        /// URL checked after the rollback deploys. Without it, the rollback
        /// is honestly recorded as `unresolved` for "is the rolled-back
        /// instance actually healthy".
        #[arg(long)]
        check_url: Option<String>,
        /// Repo to operate on (default: the current directory).
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Decide on a proposed hypothesis. Accepting is what lets a change cite
    /// it; rejecting records the reason beside it, because a rejected
    /// hypothesis with its reason is what the next proposal about the same
    /// observations should read first.
    Hypothesis {
        #[command(subcommand)]
        action: HypothesisAction,
    },
    /// Record a shipped change against an accepted hypothesis. Freezes the
    /// evaluation plan and takes the baseline reading at this moment — the
    /// plan you ship under is the plan you are measured under. When the
    /// window fully elapses, `drums watch` measures the outcome and appends
    /// it beside the change.
    Change {
        /// The accepted hypothesis this change acts on.
        #[arg(long)]
        hypothesis: String,
        /// The commit that carries the change.
        #[arg(long)]
        commit: String,
        /// Repo to operate on (default: the current directory).
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// A Product Bet: a product decision pre-registered before its outcome is
    /// known — belief, evidence, audience, expectation — evaluated once the
    /// change underneath it is measured, and closed with what the team
    /// learned. The record the rest of the loop exists to produce.
    Bet {
        #[command(subcommand)]
        action: BetAction,
    },
    /// Propose a hypothesis: an interpretation of observed facts, citing the
    /// observations it rests on, optionally carrying the evaluation plan a
    /// change would be measured against. The judgment stays in your hands —
    /// Drums records it, marks the cited observations hypothesized, and
    /// refuses citations the record does not hold.
    Hypothesize {
        /// What could be better, and why. The one free-prose judgment field
        /// in the whole record.
        #[arg(long)]
        statement: String,
        /// Observation id(s) this interprets — at least one; repeatable.
        /// `drums record` lists what has been observed.
        #[arg(long = "cite", required = true)]
        cites: Vec<String>,
        /// Evaluation plan (all four together, or none): a human name for
        /// what is being evaluated.
        #[arg(long)]
        name: Option<String>,
        /// Plan: the event that means someone entered.
        #[arg(long)]
        start: Option<String>,
        /// Plan: the event that means they finished.
        #[arg(long)]
        success: Option<String>,
        /// Plan: the metric that should move — completion_rate,
        /// time_to_complete, error_rate, step_retries, abandonment,
        /// support_contacts, or error_event_rate.
        #[arg(long)]
        metric: Option<String>,
        /// Plan: a metric that may not get worse — `error_rate` or
        /// `error_rate:0.02` (tolerance in the metric's own unit). Repeatable.
        #[arg(long = "guardrail")]
        guardrails: Vec<String>,
        /// Plan: how many days to watch after rollout (default 7).
        #[arg(long)]
        window_days: Option<u32>,
        /// Plan: entries required before a comparison is reported (default 100).
        #[arg(long)]
        min_entries: Option<u32>,
        /// Plan: smallest move worth calling a direction (default 0.02).
        #[arg(long)]
        min_effect: Option<f64>,
        /// Repo to operate on (default: the current directory).
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Drums drafts a Product Bet from the record — unclaimed observations,
    /// the memory of evaluated bets, open bets — using the coding agent you
    /// already use (config `agent_cmd`, `DRUMS_AGENT_CMD`, or claude/codex
    /// on PATH), in plain prompt mode with no edit permissions. The draft is
    /// validated like a human's `drums bet create` and recorded as proposed;
    /// nothing is committed to until you confirm. The agent may also decline
    /// to draft, and its reason is shown verbatim — a skip is a good outcome.
    /// The always-on version of this is `proactive_draft = true` in
    /// .drums/config.toml: `drums watch` then runs this same pipeline on its
    /// own whenever a new observation lands (consent-gated because it spends
    /// your agent's tokens), and confirmation stays yours either way.
    Draft {
        /// Repo to operate on (default: the current directory).
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Rebuild the semantic index: every record object as a node in a typed
    /// graph with searchable text, scoped to this product. The index is
    /// derived and disposable — the record stays the only source of truth —
    /// and embeddings (when an embedding endpoint is configured by
    /// environment) are cached by content hash so unchanged text is never
    /// paid for twice.
    Index {
        /// Repo to operate on (default: the current directory).
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Ask a question over the record. Lexical search always works; vector
    /// search joins in when DRUMS_EMBED_URL / DRUMS_EMBED_MODEL /
    /// DRUMS_EMBED_API_KEY point at an OpenAI-compatible embeddings
    /// endpoint. The answer says which one ran.
    Ask {
        /// The question, in plain words.
        query: String,
        /// How many results (default 5).
        #[arg(long, default_value_t = 5)]
        limit: usize,
        /// Repo to operate on (default: the current directory).
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// The append-only record (spec §9), rendered as readable history —
    /// newest first. Read-only: never writes, never ships, never reverts.
    Record {
        /// Repo to operate on (default: the current directory).
        #[arg(long)]
        repo: Option<PathBuf>,
        /// Show at most this many of the most recent record lines.
        #[arg(long)]
        limit: Option<usize>,
        /// Only lines recorded within this long ago, e.g. `24h`, `7d`,
        /// `30m`, `45s`.
        #[arg(long)]
        since: Option<String>,
        /// Print the raw record entries (plus the skipped-line count) as
        /// JSON, for scripting, instead of the narrated text view.
        #[arg(long)]
        json: bool,
    },
    /// Production failure history for a file (spec §17: "blame, except it
    /// knows about production"). Read-only. `<path>` or `<path>:<line>` —
    /// matching stays per-file; a given line number is noted, not tracked.
    Why {
        path: String,
        /// Repo to operate on (default: the current directory).
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Why nothing is happening. Checks every precondition the loop depends on
    /// — repository, config, coding agent, ingest, whether any report or deploy
    /// has EVER arrived, account — and names the broken one.
    ///
    /// Read-only: starts nothing, writes nothing, fixes nothing. Exits non-zero
    /// when something is broken, so CI can run it; an unknown never fails the
    /// exit code, because a fact this machine cannot establish is not a fault.
    Doctor {
        /// Repo to operate on (default: the current directory).
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Push this repo's record to the hosted plane, once — the same pass
    /// `drums watch` runs on every tick when `sync_record = true`. Opt-in
    /// twice over: refuses (naming the fix) unless .drums/config.toml sets
    /// `sync_record = true` AND this machine has run `drums login`. What is
    /// sent is the record exactly as stored — redacted at capture time, so it
    /// arrives redacted.
    Sync {
        /// Repo to operate on (default: the current directory).
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Write `.drums/config.toml` if the repo doesn't already have one — the
    /// starter config `drums daemon start` needs when nothing else has
    /// configured it. Refuses to overwrite an existing config.
    Init {
        /// Repo to operate on (default: the current directory).
        #[arg(long)]
        repo: Option<PathBuf>,
        /// Also install the reporting snippet that captures failing requests.
        /// Prints the full diff and writes NOTHING unless `--yes` is given —
        /// the plan is a thing you read, not a thing that already happened.
        #[arg(long)]
        wire: bool,
        /// Apply the plan `--wire` printed. Without it, `--wire` is a dry run.
        #[arg(long)]
        yes: bool,
        /// Service name the snippet reports under. Defaults to the directory.
        #[arg(long)]
        service: Option<String>,
        /// Prove the wiring by sending a real event to a running Drums and
        /// reading it back out of the record. `verified` here means an event
        /// was observed making the whole trip; anything else is `unresolved`.
        #[arg(long)]
        verify: bool,
        /// Where to send the verification event.
        #[arg(long, default_value = "http://127.0.0.1:7787")]
        ingest_url: String,
    },
    /// Open a completed repair in your editor: a fresh worktree at the
    /// repair's commit, so your working tree is never touched. Reuses the same
    /// worktree on repeat runs rather than accumulating checkouts.
    Open {
        /// The failure id, exactly as `drums record` and the `repair ready`
        /// line print it.
        failure_id: String,
        /// Repo to operate on (default: the current directory).
        #[arg(long)]
        repo: Option<PathBuf>,
        /// Create the worktree and print its path without launching anything —
        /// for piping into another tool.
        #[arg(long)]
        no_editor: bool,
        /// Remove the inspection worktree for this failure instead of opening
        /// it. Only ever removes one under `.drums/open/`.
        #[arg(long)]
        close: bool,
    },
    /// What Drums is allowed to do on its own, per failure class (spec §11).
    /// Read-only by default; `promote` and `demote` are the two write actions,
    /// and only a human ever runs them.
    Authority {
        #[command(subcommand)]
        action: AuthorityCommand,
    },
    /// Connect this machine to a Drums account, so repairs can run when the
    /// laptop is closed. Approval happens in a browser; no token is ever
    /// typed or pasted.
    Login {
        /// Print the URL and wait, rather than opening a browser. For remote
        /// shells, and for anyone who would rather not have a command open
        /// windows on their machine.
        #[arg(long)]
        no_browser: bool,
    },
    /// Forget this machine's credentials. Local only — revoke the token in
    /// the console if the machine itself is no longer trusted.
    Logout,
    /// Which account this machine is signed in to.
    Whoami,
    /// Run ONE repair from an instruction file and write its outcome to
    /// another. This is what GitHub Actions runs — it never ships, never
    /// opens a pull request, and needs no network: the outcome file is the
    /// whole interface.
    Repair {
        /// JSON: the failure and the attribution, both already decided.
        #[arg(long, value_name = "FILE")]
        input: PathBuf,
        /// Where to write the outcome. Always written, including when nothing
        /// was established — a silent run is indistinguishable from one that
        /// never happened.
        #[arg(long, value_name = "FILE", default_value = "drums-outcome.json")]
        outcome_file: PathBuf,
        /// The checkout to repair. Defaults to the working directory.
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Run the loop as a detached service: no terminal required, survives
    /// the terminal closing, and rebuilds its own state on restart.
    Daemon {
        #[command(subcommand)]
        action: DaemonCommand,
    },
    /// Store a license key. Only ONE thing in Drums needs it — `drums
    /// authority promote`, which grants a failure class act-alone. Everything
    /// else (detect, attribute, reproduce, repair, verify, propose) is free
    /// forever and does not read this.
    ///
    /// Verification is entirely offline: the key is checked against a public
    /// key compiled into this binary. No license server is contacted, now or
    /// later.
    Activate {
        /// The key as it was issued, e.g. `drums-license-v1.<payload>.<sig>`.
        key: String,
    },
    /// The morning message (spec §14): what broke, what Drums did, what
    /// still needs you — read-only, built purely from the record. Defaults
    /// to the last 24h, printed to stdout.
    Digest {
        /// Repo to operate on (default: the current directory).
        #[arg(long)]
        repo: Option<PathBuf>,
        /// How far back to look, e.g. `24h`, `7d`, `30m`, `45s`. Defaults to
        /// `24h`.
        #[arg(long)]
        since: Option<String>,
        /// Where to send it. `stdout` (default) needs no configuration.
        /// `slack` posts to the incoming-webhook URL in
        /// `DRUMS_SLACK_WEBHOOK_URL`.
        #[arg(long, value_enum, default_value_t = DigestTargetArg::Stdout)]
        to: DigestTargetArg,
        /// Send the digest to the configured Slack webhook as ONE
        /// notification: FYI when nothing needs anyone, Decision (with the
        /// count in the title) when something does. Uses `slack_webhook_url`
        /// from .drums/config.toml, or DRUMS_SLACK_WEBHOOK_URL (which wins).
        #[arg(long)]
        send: bool,
    },
}

#[derive(Subcommand)]
enum AuthorityCommand {
    /// Every class Drums has seen, its rung, its streak, and any promotion it
    /// has earned the right to PROPOSE. Read-only.
    List {
        /// Repo to operate on (default: the current directory).
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Grant a class the act-alone rung. This is the only way a class ever
    /// gains authority — nothing in the loop promotes itself.
    Promote {
        /// `service/ErrorName`, exactly as `drums authority list` prints it.
        class: String,
        /// Repo to operate on (default: the current directory).
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Drop a class back to proposing. Always allowed, never asks why:
    /// reducing what software may do on its own must be the easiest operation
    /// in the system.
    Demote {
        class: String,
        /// Repo to operate on (default: the current directory).
        #[arg(long)]
        repo: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum DaemonCommand {
    /// Start `drumsd` detached from this terminal and return immediately,
    /// printing where its pidfile and logfile live. A repo with
    /// `.drums/config.toml` and no flags just works; with neither, this
    /// refuses honestly rather than guessing.
    Start {
        /// Repo to operate on (default: the current directory).
        #[arg(long)]
        repo: Option<PathBuf>,
        #[arg(long)]
        ingest_port: Option<u16>,
        #[arg(long)]
        threshold: Option<usize>,
        #[arg(long)]
        window_secs: Option<u64>,
        #[arg(long)]
        app_root: Option<String>,
        #[arg(long, value_enum)]
        repair: Option<RepairModeArg>,
        #[arg(long)]
        deploy_cmd: Option<String>,
        #[arg(long)]
        check_url: Option<String>,
        /// Also take reported issues (Linear/Agentation) and attempt
        /// propose-only repairs for them, exactly like `drums watch
        /// --repair-reported`.
        #[arg(long)]
        repair_reported: bool,
    },
    /// Stop a running daemon: SIGTERM, wait, then SIGKILL after a bound —
    /// killing agent process groups and cleaning reproduction worktrees
    /// exactly as `drums watch`'s own ctrl-c/SIGTERM path does (same code,
    /// running inside `drumsd`).
    Stop {
        /// Repo to operate on (default: the current directory).
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Report whether a daemon is running (verifying the pid is actually
    /// alive, not just that a pidfile exists), its uptime, and what it's
    /// watching.
    Status {
        /// Repo to operate on (default: the current directory).
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Print the daemon's logfile. `-f`/`--follow` keeps printing new lines
    /// as they're appended, like `tail -f`, until ctrl-c/SIGTERM.
    Logs {
        /// Repo to operate on (default: the current directory).
        #[arg(long)]
        repo: Option<PathBuf>,
        #[arg(short = 'f', long)]
        follow: bool,
    },
}

/// `--repair auto` without `--deploy-cmd` is a startup error, not a silent
/// fall-back to propose — spec §7 honesty: never let a flag the user typed
/// quietly do less than what it says.
///
/// `--dispatch-repairs` changes what `--repair auto` MEANS, so it changes this
/// rule with it. In hosted mode nothing is deployed by this process at all: the
/// repair runs in the customer's CI and the ship decision lives with the
/// control plane. `--repair auto` there says only "I consent to act-alone" —
/// it raises the ceiling the authority ladder is capped by. Demanding a deploy
/// command that would never be run would be the same dishonesty from the other
/// direction: a flag the user typed that does nothing.
fn validate_watch_repair_flags(
    repair: RepairModeArg,
    deploy_cmd: &Option<String>,
    dispatch_repairs: bool,
) -> Result<(), String> {
    if repair == RepairModeArg::Auto && deploy_cmd.is_none() && !dispatch_repairs {
        return Err("--repair auto requires --deploy-cmd (refusing to start — auto would otherwise silently behave like propose)".to_string());
    }
    Ok(())
}

/// Flag combinations that `--dispatch-repairs` makes meaningless.
///
/// `Err` refuses at startup; `Ok(Some(..))` is a warning to print and carry on.
/// The line between them is whether the flag would UNDER-DELIVER on a promise
/// (refuse) or merely waste an expectation (warn) — the same distinction
/// `--repair auto` and `--check-url` already draw above.
fn validate_dispatch_flags(
    dispatch_repairs: bool,
    repair_reported: bool,
    propose_pr: bool,
) -> Result<Option<String>, String> {
    if !dispatch_repairs {
        return Ok(None);
    }
    if repair_reported {
        // A reported issue has nothing to replay, so it is never reproduced and
        // can never be dispatched. Honouring the flag would mean an agent
        // editing this repo locally while every other repair went to CI — two
        // different execution planes for two kinds of work, decided by an
        // interaction nobody asked for.
        return Err(
            "--repair-reported cannot be combined with --dispatch-repairs (a reported issue has no replayable request, so it is never reproduced and never dispatched — and repairing it here would contradict the flag)"
                .to_string(),
        );
    }
    if propose_pr {
        return Ok(Some(
            "--propose-pr does nothing with --dispatch-repairs (no repair is produced on this machine, so there is no branch here to open a pull request from — the control plane opens it)"
                .to_string(),
        ));
    }
    Ok(None)
}

/// When `--app-root` was omitted, wrap `ingest_rx` with a forwarding task
/// that auto-derives (and caches, per `app_root::AppRootCache`) the
/// deployment-boundary prefix from each error event's own stack trace and
/// strips it before the event ever reaches the engine/detector — the exact
/// same effect a correctly-guessed `--app-root` has, computed dynamically
/// instead of typed in manually (spec §22, the carried Stage-1 seam).
/// Deploys pass through unchanged. With an explicit `--app-root`, this is
/// never called — `ingest_rx` is used directly, matching prior behavior
/// exactly.
fn spawn_app_root_deriver(
    mut ingest_rx: tokio::sync::mpsc::UnboundedReceiver<engine_ingest::Ingested>,
    repo: PathBuf,
) -> tokio::sync::mpsc::UnboundedReceiver<engine_ingest::Ingested> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut cache = app_root::AppRootCache::new();
        while let Some(item) = ingest_rx.recv().await {
            let item = match item {
                engine_ingest::Ingested::Error(mut ev) => {
                    // Closing round (carried M-d): the derivation shells out
                    // to `git ls-files` (see `app_root::is_git_tracked`) —
                    // synchronous process spawn + wait. Running it inline
                    // here blocked one of the runtime's worker threads on
                    // every cache miss, and this task sits directly in the
                    // ingest path, so a slow git (cold cache, large repo,
                    // network filesystem) stalled unrelated work: the ingest
                    // server's own handlers, the engine, the render loop. The
                    // cache moves into the blocking closure and back out so
                    // it stays a plain `&mut` with no locking.
                    let stack = ev.stack.clone();
                    let repo_for_derive = repo.clone();
                    let derived = tokio::task::spawn_blocking(move || {
                        let prefix =
                            app_root::derive_for_stack(&mut cache, &stack, &repo_for_derive);
                        (cache, prefix)
                    })
                    .await;
                    match derived {
                        Ok((returned_cache, prefix)) => {
                            cache = returned_cache;
                            if let Some(prefix) = prefix {
                                ev.stack = app_root::strip_prefix_from_stack(&ev.stack, &prefix);
                            }
                        }
                        // The blocking pool panicked or was shut down. The
                        // event still has to be forwarded — unstripped, which
                        // is exactly the documented "nothing matched"
                        // fallback — because dropping it would silently lose
                        // a real production error.
                        Err(e) => {
                            tracing::warn!(error = %e, "app_root derivation task failed; forwarding the event with its stack unstripped");
                            cache = app_root::AppRootCache::new();
                        }
                    }
                    engine_ingest::Ingested::Error(ev)
                }
                other => other,
            };
            if tx.send(item).is_err() {
                break;
            }
        }
    });
    rx
}

/// The refusal `drums watch` prints when it cannot bind the ingest port —
/// most commonly because another watch or daemon already holds it. A raw
/// `Err(Os { code: 48, .. })` Debug dump names neither the port nor a next
/// step; this names both.
fn ingest_bind_refusal(port: u16, e: &std::io::Error) -> String {
    format!(
        "drums watch: could not listen on 127.0.0.1:{port} ({e}).\n  another `drums watch` or daemon may already be running — check `drums daemon status`,\n  or start this one on a different port with --ingest-port <other>."
    )
}

/// Installs the process's one `tracing` subscriber, writing to STDERR so it
/// can never interleave with the §7 narration on stdout.
///
/// Fix round (round-2 N3): every `tracing::error!` in this binary's crate —
/// notably both `append_record`s, which log a failed record-line write and
/// keep going — was a NO-OP, because nothing ever installed a subscriber
/// (`tracing-subscriber` was a declared but unused dependency). A
/// best-effort compliance-record write that fails silently is the one thing
/// this product cannot afford, so the logs have to actually go somewhere.
/// `WARN` and above only: this is a narration-first CLI, not a service log.
/// `try_init` (not `init`) so a second call — or a test harness that already
/// installed one — is a no-op rather than a panic.
fn install_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_max_level(tracing::Level::WARN)
        .with_target(false)
        .try_init();
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    install_tracing();
    let cli = Cli::parse();
    match cli.command {
        Commands::Watch {
            ingest_port,
            repo,
            threshold,
            window_secs,
            app_root: app_root_flag,
            repair,
            deploy_cmd,
            check_url,
            boot_cmd,
            plain,
            api_token,
            propose_pr,
            pr_base,
            repair_reported,
            dispatch_repairs,
        } => {
            if let Err(msg) = validate_watch_repair_flags(repair, &deploy_cmd, dispatch_repairs) {
                eprintln!("drums watch: {msg}");
                std::process::exit(2);
            }
            match validate_dispatch_flags(dispatch_repairs, repair_reported, propose_pr) {
                Err(msg) => {
                    eprintln!("drums watch: {msg}");
                    std::process::exit(2);
                }
                Ok(Some(warning)) => eprintln!("drums watch: {warning}"),
                Ok(None) => {}
            }
            // Closing round (carried minor 7): in `propose` mode nothing is
            // deployed by this process, so there is no post-deploy moment for
            // `--check-url` to check and the flag is silently inert. Silently
            // ignoring a flag the operator typed is the same honesty failure
            // as `--repair auto` quietly behaving like propose — that one is
            // a hard refusal because it would under-deliver on a promise;
            // this one only wastes an expectation, so it warns and continues.
            if repair == RepairModeArg::Propose && check_url.is_some() {
                eprintln!(
                    "drums watch: --check-url is ignored with --repair propose (nothing is deployed, so there is no post-deploy check to run); it takes effect with --repair auto, or on `drums ship --check-url`"
                );
            }

            // THE ACCOUNT GATE. First thing that does any work, and deliberately
            // before the state directory, the telemetry install id, the ingest
            // listener and the engine: a refusal must cost nothing and leave
            // nothing behind on the machine.
            //
            // After the flag criticism above, though. A mistyped flag is a usage
            // error the operator can fix without leaving the terminal, and there
            // is no reason to spend a network round trip on a run that was never
            // going to start.
            //
            // On the ordering against the telemetry disclosure a few dozen lines
            // down ("nothing leaves this machine before the notice describing it
            // has been written"): that rule is about the ANONYMOUS heartbeat,
            // which is still sent only after its notice. This request is the
            // operator's own credential going to the console they signed in to,
            // on a command that has just told them it requires one.
            //
            // `--dispatch-repairs` is unaffected either way: it needs a real
            // token to send anything and `RemoteRepairs::from_login` still
            // refuses on its own terms, so DRUMS_NO_ACCOUNT=1 cannot smuggle an
            // unauthenticated dispatch past this.
            match account::gate().await {
                account::Gate::Refused(message) => {
                    eprintln!("{message}");
                    std::process::exit(1);
                }
                account::Gate::Bypassed => {
                    // Said out loud, on every run, not just the first. An
                    // install that counts as nobody is the exact thing this gate
                    // exists to stop, so it must never become the quiet resting
                    // state of a process that runs for days.
                    eprintln!(
                        "drums watch: running with no account ({}=1) — this install is anonymous.",
                        account::NO_ACCOUNT_ENV
                    );
                }
                account::Gate::Start { account, note } => {
                    if let Some(note) = note {
                        eprintln!("drums watch: {note}");
                    }
                    println!("signed in as {account}");
                }
            }

            let repo = repo.unwrap_or(std::env::current_dir()?);
            // Resolved here, at the top, but neither PRINTED nor SENT here:
            // the notice is printed further down (just before the display
            // starts, so the TUI's alternate screen cannot swallow it), and
            // the first heartbeat is spawned on the line after that. Nothing
            // leaves this machine before the notice describing it has been
            // written to stdout.
            let telemetry_startup = telemetry::start(&repo);
            if let Some(warning) = &telemetry_startup.warning {
                eprintln!("drums watch: {warning}");
            }
            let telemetry = telemetry_startup.telemetry;
            let telemetry_counters = telemetry.counters();
            let state_dir = repo.join(".drums");
            std::fs::create_dir_all(&state_dir)?;
            let record_path = state_dir.join("record.jsonl");
            // Cloned before `record_path` moves into `EngineConfig` below —
            // the TUI's footer reads it directly off the `ViewModel`
            // (`record N events`) rather than through an event.
            let record_path_for_ui = record_path.clone();
            let (ingest_state, ingest_rx) = engine_ingest::IngestState::new(record_path.clone());
            let (ev_tx, ev_rx) = tokio::sync::mpsc::unbounded_channel();
            // `CliRepairAgent::detect()` finds `DRUMS_AGENT_CMD`, then
            // `claude`, then `codex` on PATH — `None` means genuinely
            // nothing is available, which the engine handles honestly (no
            // repair is ever attempted, nothing is faked).
            //
            // `--dispatch-repairs` pins it to `None` regardless of what is on
            // PATH. This is the mechanism, not a cosmetic: with no agent the
            // engine's `pcfg` is `None` and `run_repair_pipeline` returns
            // immediately, so there is no code path left in this process that
            // can edit the repo. "Repairs do not run here" is enforced by the
            // absence of the thing that runs them, rather than by every branch
            // remembering to check a flag.
            let repair_agent = if dispatch_repairs {
                None
            } else {
                CliRepairAgent::detect().map(|a| Arc::new(a) as Arc<dyn engine_repair::RepairAgent>)
            };
            // Captured before `repair_agent` moves into `cfg` below — the
            // TUI header banner names the agent the same way the plain
            // `--repair auto` banner already implies it via the narration.
            let agent_label = repair_agent
                .as_ref()
                .map(|a| a.name().to_string())
                .unwrap_or_else(|| "none".to_string());
            // The dashboard's `/v1/status` needs the same fact, but as an
            // Option: "no agent detected" is a real state the API must be
            // able to report, and the TUI's "none" placeholder string would
            // be indistinguishable from an agent actually named "none".
            let agent_name = repair_agent.as_ref().map(|a| a.name().to_string());

            // `--app-root` explicit always wins: only when it's omitted do
            // events get routed through the auto-deriving forwarding task,
            // and `EngineConfig.app_root` stays empty (no further stripping
            // needed — the forwarding task already did it on the raw stack
            // text before this point).
            let (engine_rx, engine_app_root) = match &app_root_flag {
                Some(p) => (ingest_rx, p.display().to_string()),
                None => (
                    spawn_app_root_deriver(ingest_rx, repo.clone()),
                    String::new(),
                ),
            };

            // Fail here, loudly, not after a repair exists. A proposal that
            // cannot be opened turns a verified repair into work nobody sees.
            let proposal: Option<Arc<dyn engine_propose::ChangeProposal>> = if propose_pr {
                if let Err(e) = engine_propose::GitHubProposal::available().await {
                    eprintln!("drums watch --propose-pr: {e}");
                    eprintln!("  run `gh auth login` first — Drums uses your own GitHub login and never stores a token.");
                    std::process::exit(1);
                }
                Some(Arc::new(engine_propose::GitHubProposal::new()))
            } else {
                None
            };

            let repair_mode: RepairMode = repair.into();

            // The hosted seam, resolved at STARTUP for the same reason
            // `--propose-pr` checks `gh` here: a missing login or an
            // unconnected repository must be discovered now, not after a
            // failure has already been detected, attributed and reproduced —
            // at which point the work is done and there is nowhere to send it.
            //
            // The ceiling handed over is the operator's consent
            // (`--repair propose` / `--repair auto`), never a rung. What a
            // class has EARNED is folded from the record on every dispatch.
            let dispatch: Option<Arc<drums_watch::dispatch::RemoteRepairs>> = if dispatch_repairs {
                match drums_watch::dispatch::RemoteRepairs::from_login(
                    &repo,
                    record_path.clone(),
                    repair_mode.ceiling(),
                ) {
                    // The boot recipe this watch reproduces with travels in
                    // every instruction, so the runner boots the way this
                    // machine did instead of guessing `node <entry>`.
                    Ok(r) => Some(Arc::new(r.with_boot(boot_cmd.clone()))),
                    Err(e) => {
                        eprintln!("drums watch --dispatch-repairs: {e}");
                        std::process::exit(1);
                    }
                }
            } else {
                None
            };
            let dispatch_banner = dispatch.as_ref().map(|d| {
                format!(
                    "repairs dispatch to {} for {} — reproduction stays on this machine.",
                    d.console_url(),
                    d.repository()
                )
            });

            let api_state = drums_watch::api::ApiState::new(drums_watch::api::ApiConfig {
                watching: repo.clone(),
                ingest_port,
                repair_mode,
                agent_name,
                record_path: record_path.clone(),
            });
            // Loaded once; the behavior source, the notification webhook and
            // the proactive-draft consent all read from it. All three are
            // config-driven on purpose — none of this needs a flag.
            let file_cfg = drums_watch::config::load(&repo)
                .ok()
                .flatten()
                .unwrap_or_default();
            let behavior = drums_watch::behavior_source::build(&file_cfg);
            if behavior.is_some() {
                eprintln!(
                    "behavior source: posthog configured — completion-rate plans are measurable"
                );
            }
            // Slack notifications (the four-kind vocabulary, `notify.rs`):
            // `slack_webhook_url` in config, DRUMS_SLACK_WEBHOOK_URL winning
            // over it. Absent = off, and nothing else changes.
            let notify_sink = notify::resolve_webhook(file_cfg.slack_webhook_url.as_deref())
                .map(notify::Sink::new);
            if notify_sink.is_some() {
                eprintln!(
                    "notifications: slack configured — decisions and learnings will be delivered"
                );
            }
            // Proactive drafting: consent-gated (`proactive_draft = true`),
            // and the drafting agent resolves the same way `drums draft`'s
            // does. Resolved at startup, like the repair agent, so a missing
            // agent is known before the first observation, not discovered by
            // a failing tick.
            // Record sync (opt-in twice over: `sync_record = true` in config
            // plus a `drums login` credential). Resolved at startup like
            // notify and dispatch; the flag without a credential is narrated
            // and sync stays off — sync never blocks or fails the loop, and
            // `drums doctor`'s `sync` check names the missing half.
            let sync = if file_cfg.sync_record.unwrap_or(false) {
                match drums_watch::sync::RecordSync::from_login(&repo, record_path.clone()) {
                    Ok(s) => {
                        eprintln!(
                            "record sync: on — the record mirrors to {} for {} (stored redacted, so it arrives redacted)",
                            s.app_base(),
                            s.repo_slug()
                        );
                        Some(Arc::new(s))
                    }
                    Err(why) => {
                        eprintln!(
                            "record sync: sync_record = true but {why} — running with sync off"
                        );
                        None
                    }
                }
            } else {
                None
            };
            let proactive_draft = file_cfg.proactive_draft.unwrap_or(false);
            let draft_agent = draft::agent_template(file_cfg.agent_cmd.as_deref());
            if proactive_draft {
                match &draft_agent {
                    Some(t) => eprintln!(
                        "proactive drafting: on — a new observation may draft a bet via `{}` for your confirmation",
                        t.split_whitespace().next().unwrap_or("agent")
                    ),
                    None => eprintln!(
                        "proactive drafting: proactive_draft = true but no drafting agent resolves — set agent_cmd in .drums/config.toml or DRUMS_AGENT_CMD, or install one of the supported agents (claude, codex, gemini, cursor-agent, amp, opencode)"
                    ),
                }
            }
            // The poll half of reported intake: only a logged-in watch has
            // a line to the console, and a credential for another console
            // is refused the same way dispatch refuses it.
            let tracker_poll = login::load().ok().flatten().and_then(|creds| {
                let console = login::console_url();
                if !creds.console_url.is_empty() && creds.console_url != console {
                    return None;
                }
                Some(drums_watch::tracker_poll::TrackerPoll::new(
                    console,
                    creds.token,
                    ingest_port,
                ))
            });
            let cfg = EngineConfig {
                notify: notify_sink,
                proactive_draft,
                tracker_poll,
                draft_agent,
                behavior,
                proposal,
                proposal_base: pr_base,
                repair_reported,
                repo: repo.clone(),
                threshold,
                window_ms: window_secs * 1000,
                app_root: engine_app_root,
                record_path,
                repair_agent,
                repair_mode,
                deploy_cmd: deploy_cmd.clone(),
                check_url: check_url.clone(),
                repair_boot_timeout_ms: 15_000,
                // `drums watch` is always a fresh process — there is no prior
                // run of THIS process to restore state from (that's
                // `drumsd`'s job; see `EngineConfig::initial_opened`'s doc).
                initial_opened: Vec::new(),
                boot_cmd: boot_cmd.clone(),
                dispatch,
                sync,
            };
            // F4 (Task-3 review round 3): built once from the exact
            // `EngineConfig` the operator configured, so the `RepairReady`
            // line's printed `drums ship` command carries the real
            // `--deploy-cmd`/`--repo` rather than a bare `drums ship <id>`
            // that exits 2 on a REQUIRED clap arg nobody supplied.
            let render_ctx = render::RenderContext {
                repo: repo.clone(),
                deploy_cmd: deploy_cmd.clone(),
            };
            let reproducer = Arc::new(LocalProcessReproducer {
                boot_timeout_ms: 15_000,
                boot_cmd,
            });
            tokio::spawn(Engine::run(cfg, engine_rx, reproducer, ev_tx));

            let listener = match tokio::net::TcpListener::bind(("127.0.0.1", ingest_port)).await {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("{}", ingest_bind_refusal(ingest_port, &e));
                    std::process::exit(1);
                }
            };
            let repair_banner = match (&dispatch_banner, repair_mode) {
                // Hosted mode wins the banner outright: what a person needs to
                // know first is that repairs are not happening here, and a
                // "repairs propose only" line would describe a pipeline this
                // process is no longer running.
                (Some(banner), _) => banner.clone(),
                // A completed repair lands as a commit on its own
                // `drums/repair-*` branch (never on the branch being
                // watched) and stops there — nothing is deployed without an
                // explicit `drums ship <id>`, run separately by a human.
                (None, RepairMode::Propose) => {
                    "repairs propose only — nothing ships without `drums ship`.".to_string()
                }
                (None, RepairMode::Auto) => format!(
                    "repairs auto-ship via `{}` — reversible with `drums revert <id>`.",
                    deploy_cmd.as_deref().unwrap_or("")
                ),
            };
            // Tap the engine's event stream so the dashboard's in-memory
            // state sees every event, WITHOUT changing `ui::run_plain` /
            // `ui::run_with_terminal`, both of which own the receiver. The
            // tap applies then forwards, so the terminal narration is
            // byte-for-byte unchanged and the API is never a second source
            // of truth — it observes the same events the terminal renders.
            // Unbounded, matching the engine's own channel: a bounded tap
            // would apply backpressure to the engine whenever the terminal
            // renderer is slow, so a full buffer could stall a repair. The
            // renderer must never be able to throttle the loop.
            let (tap_tx, tap_rx) =
                tokio::sync::mpsc::unbounded_channel::<drums_watch::engine::EngineEvent>();
            let api_state_tap = api_state.clone();
            let mut engine_rx_for_tap = ev_rx;
            tokio::spawn(async move {
                while let Some(ev) = engine_rx_for_tap.recv().await {
                    api_state_tap.apply(&ev);
                    // Four counters incremented in memory — see
                    // `telemetry::Counters::observe`, which matches on four
                    // variants and drops everything else on the floor. The
                    // event itself is never stored, copied, or serialised by
                    // telemetry; only the totals survive this line.
                    telemetry_counters.observe(&ev);
                    if tap_tx.send(ev).is_err() {
                        // The UI hung up (ctrl-c, or the TUI exited): stop
                        // forwarding rather than filling the channel.
                        break;
                    }
                }
            });
            let ev_rx = tap_rx;
            let api_router_state = api_state.clone();

            tokio::spawn(async move {
                // ONE listener, ONE router: the read-only dashboard API is
                // merged onto the ingest server rather than binding a second
                // port.
                // ONE listener, ONE router: ingest, the read-only dashboard API,
                // and the dashboard bundle itself. Serving the page from the
                // daemon is what makes it same-origin as the API it reads —
                // an HTTPS page cannot fetch http://127.0.0.1 at all.
                let _ = axum::serve(
                    listener,
                    engine_ingest::router(ingest_state)
                        .merge(drums_watch::api::router(api_router_state, api_token))
                        .merge(drums_watch::dashboard::router()),
                )
                .await;
            });
            // The BINARY owns the process's one and only signal handler(s).
            // No library in this workspace installs one: `tokio::signal`
            // registers an OS handler process-wide, once, permanently, so a
            // library that calls it silently takes a signal away from the
            // program that owns it for the rest of its life (engine-repair
            // did exactly that for one commit, and after a single repair
            // `drums watch` could only be stopped with `kill -9`). Created
            // ONCE, outside the loop: re-registering per iteration leaves a
            // window in which an arriving signal has no listener at all.
            // Both the plain loop and the TUI (`ui::run`) below are handed
            // this SAME future — neither installs a second handler.
            //
            // SIGTERM gets the same graceful path as ctrl-c (carried from
            // the Task-2 gate review): `demo/run-demo.sh`'s `trap ... EXIT`
            // uses `kill`, which sends SIGTERM, and once a repair can be
            // in-flight that must stop the watch, kill the agent's process
            // group, and clean up worktrees the same way ctrl-c does —
            // `kill -9` remains the only way to bypass this.

            // The live TUI (additive, ratatui) is used only when BOTH stdout
            // and stdin are actual terminals — a human watching redraw in
            // place also needs a working stdin for `enable_raw_mode` to set
            // up (it touches stdin's termios, not just stdout's), so stdout
            // alone is not sufficient: `drums watch < /dev/null`, `nohup
            // drums watch`, and most supervisor/launchd wrappers attach a
            // real stdout but no terminal stdin. Any of those, plus a pipe,
            // a file, or CI on stdout (or an explicit `--plain`/
            // `DRUMS_PLAIN=1`) always get the exact byte-for-byte plain
            // narration `e2e.rs` and `demo/run-demo.sh` assert on — this
            // check is the decision, made once, before either path starts
            // printing anything. It is deliberately still not the ONLY
            // gate: `try_init_terminal` below can still fail for reasons
            // this check can't see (a terminal that vanishes between the
            // check and setup, an exotic pty), and that failure degrades to
            // plain output too, rather than aborting the command.
            let use_tui = std::io::stdout().is_terminal()
                && std::io::stdin().is_terminal()
                && !plain
                && std::env::var("DRUMS_PLAIN").as_deref() != Ok("1");

            // The first-run telemetry notice, printed before anything has
            // been sent and before the display takes over the screen.
            //
            // The pause is only for the TUI path, and only on the one run
            // that shows the notice: `ui::run_with_terminal` enters an
            // alternate screen a few milliseconds from here, and a
            // conspicuous disclosure that is painted over before it can be
            // read is not a disclosure. It delays nothing but the display —
            // the ingest server is already bound and the engine is already
            // running by this point, so no failure can be missed while it
            // sits there. In plain/non-terminal mode the text simply stays
            // on stdout and there is nothing to wait for.
            if let Some(notice) = &telemetry_startup.disclosure {
                print!("\n{notice}\n");
                std::io::stdout().flush().ok();
                telemetry.mark_disclosed();
                if use_tui {
                    tokio::time::sleep(Duration::from_millis(2500)).await;
                }
            }
            // Strictly after the notice. An opted-out install spawns no task
            // at all (`spawn_heartbeat` returns immediately), which is why
            // the opt-out is a property of the sending seam and not just of
            // what gets printed.
            telemetry.spawn_heartbeat();

            let print_plain_banner = || {
                println!(
                    "watching {} · ingest :{} · {repair_banner}",
                    repo.display(),
                    ingest_port
                );
                if drums_watch::dashboard::is_bundled() {
                    println!("dashboard: http://127.0.0.1:{ingest_port}");
                }
                std::io::stdout().flush().ok();
            };

            let stopped_by_signal = if use_tui {
                match ui::try_init_terminal() {
                    Ok((terminal, guard)) => {
                        let header = ui::HeaderInfo {
                            repo_name: repo
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_else(|| repo.display().to_string()),
                            agent_label,
                            repair_label: repair_banner,
                            ingest_port,
                        };
                        ui::run_with_terminal(
                            terminal,
                            guard,
                            header,
                            ev_rx,
                            render_ctx,
                            record_path_for_ui,
                            wait_for_stop_signal(),
                        )
                        .await?
                    }
                    Err(e) => {
                        // Terminal setup failed for a reason `use_tui`'s own
                        // checks couldn't see. `ev_rx` was never touched by
                        // `try_init_terminal`, so it's still whole here —
                        // fall back to the exact same plain path the
                        // non-TUI branch below uses, rather than letting `?`
                        // propagate a raw `io::Error` out of `main` and
                        // abort the whole command watching nothing.
                        eprintln!(
                            "note: interactive display unavailable ({e}); using plain output."
                        );
                        print_plain_banner();
                        ui::run_plain(ev_rx, &render_ctx, wait_for_stop_signal()).await
                    }
                }
            } else {
                print_plain_banner();
                ui::run_plain(ev_rx, &render_ctx, wait_for_stop_signal()).await
            };

            if stopped_by_signal {
                // §7 honesty: say what stopping actually did, and don't
                // promise anything it didn't do. Breaking out of this loop
                // returns from `main`, which drops the tokio runtime, which
                // drops every spawned engine task; each in-flight
                // reproduction's and repair's child guard drops with it and
                // kills that child's whole process group. The reproduction
                // and repair worktree guards REMOVE their worktree on that
                // same drop (`engine/crates/repro/src/lib.rs`,
                // `ManagedWorktree::drop`) UNLESS a repair had already set
                // `keep_on_drop` before the signal arrived (a failed repair
                // being left for inspection) — so this line describes the
                // common case, not an absolute guarantee for every worktree.
                println!("\nstopped — in-flight reproduction and repair child processes killed; temporary worktrees removed, not preserved.");
                std::io::stdout().flush().ok();
            }
            Ok(())
        }
        Commands::Ship {
            failure_id,
            deploy_cmd,
            check_url,
            repo,
        } => {
            let repo = repo.unwrap_or(std::env::current_dir()?);
            let record_path = repo.join(".drums").join("record.jsonl");
            // F6 (Task-3 review round 4): the `reversible: drums revert <id>`
            // footer is built from THIS process's own flags, because `drums
            // revert <id>` alone exits 2 (`--deploy-cmd` is required) and
            // resolves its record path from cwd. A rollback instruction printed
            // right after a deploy landed has to be runnable verbatim.
            let render_ctx = render::RenderContext {
                repo: repo.clone(),
                deploy_cmd: Some(deploy_cmd.clone()),
            };
            match ship::ship(
                &record_path,
                &repo,
                &failure_id,
                &deploy_cmd,
                check_url.as_deref(),
            )
            .await
            {
                Ok(outcome) => {
                    print!("{}", ship::narrate_shipped(&outcome, &render_ctx));
                    std::io::stdout().flush().ok();
                    Ok(())
                }
                Err(e) => {
                    eprint!("{}", ship::narrate_error("ship", &e));
                    std::io::stderr().flush().ok();
                    std::process::exit(1);
                }
            }
        }
        Commands::Revert {
            failure_id,
            deploy_cmd,
            check_url,
            repo,
        } => {
            let repo = repo.unwrap_or(std::env::current_dir()?);
            let record_path = repo.join(".drums").join("record.jsonl");
            match ship::revert(
                &record_path,
                &repo,
                &failure_id,
                &deploy_cmd,
                check_url.as_deref(),
            )
            .await
            {
                Ok(outcome) => {
                    print!("{}", ship::narrate_reverted(&outcome));
                    std::io::stdout().flush().ok();
                    Ok(())
                }
                Err(e) => {
                    eprint!("{}", ship::narrate_error("revert", &e));
                    std::io::stderr().flush().ok();
                    std::process::exit(1);
                }
            }
        }
        Commands::Hypothesis { action } => {
            let (id, decision, repo) = match action {
                HypothesisAction::Accept { id, repo } => {
                    (id, engine_core::hypothesis::Status::Accepted, repo)
                }
                HypothesisAction::Reject { id, reason, repo } => (
                    id,
                    engine_core::hypothesis::Status::Rejected { reason },
                    repo,
                ),
            };
            let repo = repo.unwrap_or(std::env::current_dir()?);
            let record_path = repo.join(".drums").join("record.jsonl");
            let lines = engine_record::read_all(&record_path)
                .map(|r| r.lines)
                .unwrap_or_default();
            match change_cmd::decide(&lines, &id, decision.clone()) {
                Ok((kind, value)) => {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0);
                    engine_record::append(&record_path, &kind, &value, now)?;
                    match decision {
                        engine_core::hypothesis::Status::Accepted => {
                            println!("hypothesis {id} accepted — a change may now cite it: drums change --hypothesis {id} --commit <sha>");
                        }
                        engine_core::hypothesis::Status::Rejected { reason } => {
                            println!("hypothesis {id} rejected: {reason:?} — kept in the record, not deleted");
                        }
                        engine_core::hypothesis::Status::Open => {}
                    }
                }
                Err(refusal) => {
                    eprintln!("drums hypothesis: {refusal}");
                    std::process::exit(2);
                }
            }
            Ok(())
        }
        Commands::Change {
            hypothesis,
            commit,
            repo,
        } => {
            let repo = repo.unwrap_or(std::env::current_dir()?);
            // Before anything else: the sha must actually exist in the repo.
            // A change recorded against a commit git cannot resolve is a
            // record line pointing at nothing, forever.
            if let Err(refusal) = change_cmd::verify_commit_exists(&repo, &commit) {
                eprintln!("drums change: {refusal}");
                std::process::exit(2);
            }
            let record_path = repo.join(".drums").join("record.jsonl");
            let lines = engine_record::read_all(&record_path)
                .map(|r| r.lines)
                .unwrap_or_default();
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let change_id = format!("chg_{}", ulid::Ulid::new().to_string().to_lowercase());
            // When the plan's metric reads from PostHog, take the baseline now
            // over the frozen window ending at this moment — the same reading
            // the outcome will be compared against a window later.
            let behavior_baseline = {
                use engine_core::evaluation::Metric;
                let plan = engine_core::hypothesis::Hypothesis::all(lines.iter())
                    .into_iter()
                    .find(|h| h.id.0 == hypothesis)
                    .and_then(|h| h.plan);
                match plan {
                    Some(p) if matches!(p.metric, Metric::CompletionRate | Metric::Abandonment) => {
                        let file_cfg = drums_watch::config::load(&repo)
                            .ok()
                            .flatten()
                            .unwrap_or_default();
                        let Some(source) = drums_watch::behavior_source::build(&file_cfg) else {
                            eprintln!(
                                "drums change: this plan's metric is read from PostHog, and the source is not configured — missing: {}",
                                drums_watch::behavior_source::missing(&file_cfg).join(", ")
                            );
                            std::process::exit(2);
                        };
                        let span = (p.window.days as u64).saturating_mul(86_400_000);
                        match source
                            .sample_between(&p, now.saturating_sub(span), now)
                            .await
                        {
                            Ok(sample) => {
                                println!(
                                    "baseline from posthog: {:.4} over {} entrants ({}d window)",
                                    sample.value, sample.entries, p.window.days
                                );
                                Some(sample)
                            }
                            Err(e) => {
                                eprintln!("drums change: the baseline reading failed — {e}");
                                std::process::exit(2);
                            }
                        }
                    }
                    _ => None,
                }
            };
            match change_cmd::build_change(
                &lines,
                &hypothesis,
                &commit,
                &change_id,
                now,
                behavior_baseline,
            ) {
                Ok(change) => {
                    engine_record::append(
                        &record_path,
                        engine_core::change::RECORD_KIND,
                        &change,
                        now,
                    )?;
                    println!(
                        "change {} · {} acts on {}\n  baseline {:.2}/h over {}h · window {}d\n  the outcome is measured once the window fully elapses — drums watch does it and writes it beside the change",
                        change.id,
                        change.sha,
                        change.hypothesis,
                        change.baseline.value,
                        change.baseline.entries,
                        change.plan.window.days,
                    );
                }
                Err(refusal) => {
                    eprintln!("drums change: {refusal}");
                    std::process::exit(2);
                }
            }
            Ok(())
        }
        Commands::Bet { action } => {
            fn now_ms() -> u64 {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0)
            }
            fn record_of(repo: &std::path::Path) -> PathBuf {
                repo.join(".drums").join("record.jsonl")
            }
            fn lines_of(record_path: &std::path::Path) -> Vec<(String, serde_json::Value)> {
                engine_record::read_all(record_path)
                    .map(|r| r.lines)
                    .unwrap_or_default()
            }
            match action {
                BetAction::Create {
                    belief,
                    because,
                    audience,
                    alternatives,
                    cites,
                    name,
                    start,
                    success,
                    metric,
                    guardrails,
                    window_days,
                    min_entries,
                    min_effect,
                    repo,
                } => {
                    let repo = repo.unwrap_or(std::env::current_dir()?);
                    let record_path = record_of(&repo);
                    let lines = match engine_record::read_all(&record_path) {
                        Ok(read) => read.lines,
                        Err(_) => {
                            eprintln!(
                                "drums bet: no record at {} — a bet cites observations, and this repo has none yet",
                                record_path.display()
                            );
                            std::process::exit(2);
                        }
                    };
                    let args = bet_cmd::BetCreateArgs {
                        belief,
                        because,
                        audience,
                        alternatives,
                        cites,
                        plan: hypothesize::HypothesizeArgs {
                            name: Some(name),
                            start: Some(start),
                            success: Some(success),
                            metric: Some(metric),
                            guardrails,
                            window_days,
                            min_entries,
                            min_effect,
                            ..Default::default()
                        },
                    };
                    let now = now_ms();
                    let ulid = || ulid::Ulid::new().to_string().to_lowercase();
                    let bet_id = format!("bet_{}", ulid());
                    let hyp_id = format!("hyp_{}", ulid());
                    let eval_id = format!("eval_{}", ulid());
                    match bet_cmd::build(&args, &lines, &bet_id, &hyp_id, &eval_id, now) {
                        Ok((bet, hypothesis)) => {
                            let mut lines = lines;
                            for (kind, value) in bet_cmd::record_lines(&bet, &hypothesis) {
                                engine_record::append(&record_path, &kind, &value, now)?;
                                lines.push((kind, value));
                            }
                            print!("{}", bet_cmd::render_bet(&lines, &bet, true));
                            println!("  proposed — nothing is committed to yet\n  confirm it (the pre-registration moment): drums bet confirm {}", bet.id);
                        }
                        Err(refusal) => {
                            eprintln!("drums bet: {refusal}");
                            std::process::exit(2);
                        }
                    }
                    Ok(())
                }
                BetAction::Confirm { id, repo } => {
                    let repo = repo.unwrap_or(std::env::current_dir()?);
                    let record_path = record_of(&repo);
                    let lines = lines_of(&record_path);
                    match bet_cmd::confirm(&lines, &id) {
                        Ok(new_lines) => {
                            let now = now_ms();
                            let hyp = engine_core::bet::ProductBet::all(lines.iter())
                                .into_iter()
                                .find(|b| b.id.0 == id)
                                .map(|b| b.hypothesis.0.clone())
                                .unwrap_or_default();
                            for (kind, value) in new_lines {
                                engine_record::append(&record_path, &kind, &value, now)?;
                            }
                            println!("bet {id} confirmed — the belief is on record before the outcome exists");
                            println!("  ship the change against it: drums change --hypothesis {hyp} --commit <sha>");
                            println!("  when its window fully elapses, drums watch measures the outcome and evaluates the bet");
                        }
                        Err(refusal) => {
                            eprintln!("drums bet: {refusal}");
                            std::process::exit(2);
                        }
                    }
                    Ok(())
                }
                BetAction::Decline { id, reason, repo } => {
                    let repo = repo.unwrap_or(std::env::current_dir()?);
                    let record_path = record_of(&repo);
                    let lines = lines_of(&record_path);
                    match bet_cmd::decline(&lines, &id, &reason) {
                        Ok(new_lines) => {
                            let now = now_ms();
                            for (kind, value) in new_lines {
                                engine_record::append(&record_path, &kind, &value, now)?;
                            }
                            println!(
                                "bet {id} declined: {reason:?} — kept in the record, not deleted"
                            );
                        }
                        Err(refusal) => {
                            eprintln!("drums bet: {refusal}");
                            std::process::exit(2);
                        }
                    }
                    Ok(())
                }
                BetAction::Learn { id, note, repo } => {
                    let repo = repo.unwrap_or(std::env::current_dir()?);
                    let record_path = record_of(&repo);
                    let lines = lines_of(&record_path);
                    match bet_cmd::learn(&lines, &id, &note) {
                        Ok((kind, value)) => {
                            engine_record::append(&record_path, &kind, &value, now_ms())?;
                            println!("bet {id} · learned: {note}");
                        }
                        Err(refusal) => {
                            eprintln!("drums bet: {refusal}");
                            std::process::exit(2);
                        }
                    }
                    Ok(())
                }
                BetAction::Show { id, repo } => {
                    let repo = repo.unwrap_or(std::env::current_dir()?);
                    let record_path = record_of(&repo);
                    let lines = lines_of(&record_path);
                    match id {
                        Some(id) => {
                            match engine_core::bet::ProductBet::all(lines.iter())
                                .into_iter()
                                .find(|b| b.id.0 == id)
                            {
                                Some(bet) => print!("{}", bet_cmd::render_bet(&lines, &bet, true)),
                                None => {
                                    eprintln!("drums bet: bet {id:?} is not in this repo's record — `drums bet show` lists what is");
                                    std::process::exit(2);
                                }
                            }
                        }
                        None => print!("{}", bet_cmd::render_all(&lines)),
                    }
                    Ok(())
                }
            }
        }
        Commands::Hypothesize {
            statement,
            cites,
            name,
            start,
            success,
            metric,
            guardrails,
            window_days,
            min_entries,
            min_effect,
            repo,
        } => {
            let repo = repo.unwrap_or(std::env::current_dir()?);
            let record_path = repo.join(".drums").join("record.jsonl");
            let lines = match engine_record::read_all(&record_path) {
                Ok(read) => read.lines,
                Err(_) => {
                    eprintln!(
                        "drums hypothesize: no record at {} — a hypothesis cites observations, and this repo has none yet",
                        record_path.display()
                    );
                    std::process::exit(2);
                }
            };
            let args = hypothesize::HypothesizeArgs {
                statement,
                cites,
                name,
                start,
                success,
                metric,
                guardrails,
                window_days,
                min_entries,
                min_effect,
            };
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let hyp_id = format!("hyp_{}", ulid::Ulid::new().to_string().to_lowercase());
            let eval_id = format!("eval_{}", ulid::Ulid::new().to_string().to_lowercase());
            match hypothesize::build(&args, &lines, &hyp_id, &eval_id, now) {
                Ok(h) => {
                    let narration = hypothesize::record_and_narrate(&record_path, &h, now)?;
                    print!("{narration}");
                }
                Err(refusal) => {
                    eprintln!("drums hypothesize: {refusal}");
                    std::process::exit(2);
                }
            }
            Ok(())
        }
        Commands::Draft { repo } => {
            let repo = repo.unwrap_or(std::env::current_dir()?);
            let record_path = repo.join(".drums").join("record.jsonl");
            let lines = match engine_record::read_all(&record_path) {
                Ok(read) if !read.lines.is_empty() => read.lines,
                _ => {
                    eprintln!("drums draft: no record at {} — a draft cites observations, and this repo has none yet", record_path.display());
                    std::process::exit(2);
                }
            };
            let file_cfg = drums_watch::config::load(&repo)
                .ok()
                .flatten()
                .unwrap_or_default();
            let Some(template) = draft::agent_template(file_cfg.agent_cmd.as_deref()) else {
                eprintln!("drums draft: no coding agent available — set agent_cmd in .drums/config.toml, or DRUMS_AGENT_CMD, or have claude/codex on PATH");
                std::process::exit(2);
            };
            let agent_name = template
                .split_whitespace()
                .next()
                .unwrap_or("agent")
                .to_string();
            println!("drafting with {agent_name} — reading the record…");
            let prompt = draft::full_prompt(&lines);
            let drafted = match draft::run_agent(&template, &prompt, &repo).await {
                Ok(output) => match draft::parse(&output) {
                    Ok(d) => d,
                    Err(refusal) => {
                        eprintln!("drums draft: {refusal}");
                        std::process::exit(2);
                    }
                },
                Err(refusal) => {
                    eprintln!("drums draft: {refusal}");
                    std::process::exit(2);
                }
            };
            let args = match draft::to_args(drafted) {
                Ok(a) => a,
                Err(refusal) => {
                    // Includes the agent's own skip, reason verbatim.
                    println!("{refusal}");
                    return Ok(());
                }
            };
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let ulid = || ulid::Ulid::new().to_string().to_lowercase();
            let bet_id = format!("bet_{}", ulid());
            let hyp_id = format!("hyp_{}", ulid());
            let eval_id = format!("eval_{}", ulid());
            match bet_cmd::build(&args, &lines, &bet_id, &hyp_id, &eval_id, now) {
                Ok((bet, hypothesis)) => {
                    let mut lines = lines;
                    for (kind, value) in bet_cmd::record_lines(&bet, &hypothesis) {
                        engine_record::append(&record_path, &kind, &value, now)?;
                        lines.push((kind, value));
                    }
                    print!("{}", bet_cmd::render_bet(&lines, &bet, true));
                    println!("  drafted by {agent_name} · proposed — nothing is committed to yet\n  confirm (the pre-registration moment): drums bet confirm {}\n  or decline with the reason: drums bet decline {} --reason \"…\"", bet.id, bet.id);
                }
                Err(refusal) => {
                    eprintln!("drums draft: the agent's draft failed validation — {refusal}");
                    std::process::exit(2);
                }
            }
            Ok(())
        }
        Commands::Index { repo } => {
            let repo = repo.unwrap_or(std::env::current_dir()?);
            let record_path = repo.join(".drums").join("record.jsonl");
            let lines = engine_record::read_all(&record_path)
                .map(|r| r.lines)
                .unwrap_or_default();
            let product = repo
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("product")
                .to_string();
            let db_path = repo.join(".drums").join("semantic.db");
            let mut store = match engine_semantic::Store::open(&db_path) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("drums index: {e}");
                    std::process::exit(2);
                }
            };
            let graph = engine_semantic::extract(&lines, &product);
            if let Err(e) = store.rebuild(&graph, lines.len()) {
                eprintln!("drums index: {e}");
                std::process::exit(2);
            }
            // Embed what is new, when the environment names an endpoint.
            let embedded_note = match engine_semantic::embedder_from_env() {
                Some(embedder) => {
                    use engine_semantic::Embedder;
                    let due = store.unembedded(embedder.model()).unwrap_or_default();
                    if due.is_empty() {
                        format!("embeddings current for {}", embedder.model())
                    } else {
                        let texts: Vec<String> =
                            due.iter().map(|(_, body, _)| body.clone()).collect();
                        match embedder.embed(&texts).await {
                            Ok(vecs) => {
                                let mut stored = 0usize;
                                for ((id, _, hash), vec) in due.iter().zip(vecs) {
                                    if store.put_embedding(id, *hash, embedder.model(), &vec).is_ok() {
                                        stored += 1;
                                    }
                                }
                                format!("embedded {stored} new nodes with {}", embedder.model())
                            }
                            Err(e) => format!("embedding pass failed — {e}; the graph and lexical search are unaffected"),
                        }
                    }
                }
                None => engine_semantic::MISSING_EMBED_ENV.to_string(),
            };
            match store.counts() {
                Ok((nodes, edges, vecs)) => {
                    println!(
                        "indexed {} record lines → {} nodes · {} edges · {} embedded\n  {}",
                        lines.len(),
                        nodes,
                        edges,
                        vecs,
                        embedded_note
                    );
                }
                Err(e) => eprintln!("drums index: {e}"),
            }
            Ok(())
        }
        Commands::Ask { query, limit, repo } => {
            let repo = repo.unwrap_or(std::env::current_dir()?);
            let record_path = repo.join(".drums").join("record.jsonl");
            let db_path = repo.join(".drums").join("semantic.db");
            let lines = engine_record::read_all(&record_path)
                .map(|r| r.lines)
                .unwrap_or_default();
            let product = repo
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("product")
                .to_string();
            let mut store = match engine_semantic::Store::open(&db_path) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("drums ask: {e}");
                    std::process::exit(2);
                }
            };
            // Keep the index current before answering — an answer from a
            // stale index is a wrong answer with good manners.
            if let Err(e) = engine_semantic::store::refresh(&mut store, &lines, &product) {
                eprintln!("drums ask: {e}");
                std::process::exit(2);
            }
            let embedder = engine_semantic::embedder_from_env();
            let answer = match engine_semantic::ask(
                &store,
                &query,
                embedder
                    .as_ref()
                    .map(|e| e as &dyn engine_semantic::Embedder),
                limit,
            )
            .await
            {
                Ok(a) => a,
                Err(e) => {
                    eprintln!("drums ask: {e}");
                    std::process::exit(2);
                }
            };
            if answer.hits.is_empty() {
                println!("nothing in this repo's record matches {query:?}");
                if !answer.semantic {
                    println!("  {}", engine_semantic::MISSING_EMBED_ENV);
                }
                return Ok(());
            }
            println!(
                "{} — {}",
                query,
                if answer.semantic {
                    "lexical + vector"
                } else {
                    "lexical only"
                }
            );
            for hit in &answer.hits {
                println!(
                    "\n{} {} — {}",
                    hit.node.kind.label(),
                    hit.node.label,
                    snippet(&hit.node.body)
                );
                for (rel, other) in hit.context.iter().take(4) {
                    println!("    {rel} {other}");
                }
            }
            if !answer.semantic {
                println!("\n  {}", engine_semantic::MISSING_EMBED_ENV);
            }
            Ok(())
        }
        Commands::Record {
            repo,
            limit,
            since,
            json,
        } => {
            let repo = repo.unwrap_or(std::env::current_dir()?);
            let record_path = repo.join(".drums").join("record.jsonl");
            let since_ms = match since {
                Some(s) => match record_cmd::parse_duration_ms(&s) {
                    Ok(ms) => Some(ms),
                    Err(msg) => {
                        eprintln!("drums record: {msg}");
                        std::process::exit(2);
                    }
                },
                None => None,
            };
            match record_cmd::load(&record_path) {
                Ok((mut entries, skipped)) => {
                    // Captured before `--since`/`--limit` narrow `entries`
                    // down — `render_lines` needs this to tell "the record
                    // is genuinely empty" apart from "the filters matched
                    // nothing" (HIGH fix-round).
                    let total_loaded = entries.len();
                    let now = record_cmd::now_ms();
                    if let Some(since_ms) = since_ms {
                        entries = record_cmd::filter_since(entries, now, since_ms);
                    }
                    let entries = record_cmd::apply_limit(entries, limit);
                    if json {
                        println!("{}", record_cmd::render_json(&entries, skipped));
                    } else {
                        let use_color = std::io::stdout().is_terminal();
                        print!(
                            "{}",
                            record_cmd::render_lines(
                                &entries,
                                skipped,
                                total_loaded,
                                now,
                                use_color
                            )
                        );
                    }
                    std::io::stdout().flush().ok();
                    Ok(())
                }
                Err(msg) => {
                    eprintln!("drums record: {msg}");
                    std::process::exit(1);
                }
            }
        }
        Commands::Why { path, repo } => {
            let repo = repo.unwrap_or(std::env::current_dir()?);
            let record_path = repo.join(".drums").join("record.jsonl");
            // MEDIUM fix-round: an unparseable `:<line>` suffix (e.g. one
            // that overflows u32, from a `file:line:col` stack-trace-shaped
            // argument) must be refused explicitly, not silently folded
            // back into the path where it can never match anything and
            // produces a false "no production history found".
            let (target, line) = match why::parse_path_arg(&path) {
                Ok(t) => t,
                Err(msg) => {
                    eprintln!("drums why: {msg}");
                    std::process::exit(2);
                }
            };
            match record_cmd::load(&record_path) {
                Ok((entries, skipped)) => {
                    let (history, undecodable) = why::find_history(&entries, &target);
                    let use_color = std::io::stdout().is_terminal();
                    print!(
                        "{}",
                        why::render(
                            &path,
                            line,
                            &history,
                            skipped,
                            undecodable,
                            record_cmd::now_ms(),
                            use_color
                        )
                    );
                    std::io::stdout().flush().ok();
                    Ok(())
                }
                Err(msg) => {
                    eprintln!("drums why: {msg}");
                    std::process::exit(1);
                }
            }
        }
        Commands::Doctor { repo } => {
            let repo = repo.unwrap_or(std::env::current_dir()?);
            let checks = drums_watch::doctor::run(&repo, VERSION);
            let broken = drums_watch::doctor::report(&checks);
            // Non-zero when something is actually broken, so `drums doctor`
            // can gate a CI step or a setup script. Nothing here writes, so
            // there is no partial state to unwind on the way out.
            if broken > 0 {
                std::process::exit(1);
            }
            Ok(())
        }
        Commands::Sync { repo } => {
            let repo = repo.unwrap_or(std::env::current_dir()?);
            // The consent gate reads the config file honestly: a config that
            // cannot be parsed is its own named refusal, never silently
            // treated as "sync is off".
            let file_cfg = match drums_watch::config::load(&repo) {
                Ok(cfg) => cfg.unwrap_or_default(),
                Err(msg) => {
                    eprintln!("drums sync: {msg}");
                    std::process::exit(2);
                }
            };
            if !file_cfg.sync_record.unwrap_or(false) {
                eprintln!(
                    "drums sync: record sync is off for this repo — set sync_record = true in {} first.\n  \
                     Your record leaves this machine only when you set it; it arrives redacted \
                     because it is stored redacted.",
                    drums_watch::config::config_path(&repo).display()
                );
                std::process::exit(2);
            }
            let record_path = repo.join(".drums").join("record.jsonl");
            // The one auth path: `drums login`'s credential, `login::console_url`'s
            // base. A missing token refuses here, naming the command that fixes it.
            let sync = match drums_watch::sync::RecordSync::from_login(&repo, record_path) {
                Ok(s) => s,
                Err(why) => {
                    eprintln!("drums sync: {why}");
                    std::process::exit(2);
                }
            };
            match sync.pass().await {
                Ok(s) => {
                    println!(
                        "synced {} lines through seq {} for {}",
                        s.sent,
                        s.through,
                        sync.repo_slug()
                    );
                    Ok(())
                }
                // An unreachable or refusing plane is said out loud, nonzero —
                // an explicit `drums sync` that moved nothing must never
                // pretend otherwise. (The watch tick warns and retries
                // instead; only the on-demand command exits over it.)
                Err(why) => {
                    eprintln!("drums sync: {why}");
                    std::process::exit(1);
                }
            }
        }
        Commands::Init {
            repo,
            wire,
            yes,
            service,
            verify,
            ingest_url,
        } => {
            let repo = repo.unwrap_or(std::env::current_dir()?);
            // --wire / --verify remain for scripts and for re-running one part
            // of setup. Plain `drums init` now does the whole thing.
            if wire || verify {
                return init_wire(&repo, wire, yes, service, verify, &ingest_url).await;
            }
            init_everything(&repo, yes, service).await
        }
        Commands::Digest {
            repo,
            since,
            to,
            send,
        } => {
            let repo = repo.unwrap_or(std::env::current_dir()?);
            let record_path = repo.join(".drums").join("record.jsonl");
            let since_ms = match since {
                Some(s) => match record_cmd::parse_duration_ms(&s) {
                    Ok(ms) => ms,
                    Err(msg) => {
                        eprintln!("drums digest: {msg}");
                        std::process::exit(2);
                    }
                },
                None => 24 * 3_600_000,
            };
            let now = record_cmd::now_ms();
            // Read here, once, and passed in as data — `Digest::build` stays
            // a pure function of (record, window, instant, license state).
            let licensed = license::current_status(now).grants_act_alone();
            let d = digest::Digest::build(&record_path, since_ms, now, licensed);
            let ctx = render::RenderContext {
                repo: repo.clone(),
                deploy_cmd: None,
            };

            if send {
                // ONE notification in the four-kind vocabulary (notify.rs):
                // FYI when nothing needs anyone, Decision when something does.
                let file_cfg = drums_watch::config::load(&repo)
                    .ok()
                    .flatten()
                    .unwrap_or_default();
                let Some(webhook) = notify::resolve_webhook(file_cfg.slack_webhook_url.as_deref())
                else {
                    eprintln!(
                        "drums digest --send: no Slack webhook configured — set slack_webhook_url in {}, or export {} (the env var wins when both are set)",
                        drums_watch::config::config_path(&repo).display(),
                        notify::WEBHOOK_ENV
                    );
                    std::process::exit(2);
                };
                // A digest built over an unreadable record must not be sent as
                // "FYI — nothing needs you"; that would be a lie about a
                // record we could not read.
                if let Some(err) = &d.read_error {
                    eprintln!("drums digest --send: {err}");
                    std::process::exit(1);
                }
                let repo_name = repo.file_name().and_then(|n| n.to_str()).unwrap_or("repo");
                let n = digest::notification(&d, &ctx, repo_name);
                match notify::send(&webhook, &n).await {
                    Ok(()) => {
                        println!(
                            "sent to slack: [{}] {} (drums · {})",
                            n.kind.label(),
                            n.title,
                            n.repo
                        );
                        return Ok(());
                    }
                    Err(e) => {
                        eprintln!("drums digest --send: {e}");
                        std::process::exit(1);
                    }
                }
            }

            let notifier: Box<dyn digest::Notifier> = match to {
                DigestTargetArg::Stdout => Box::new(digest::StdoutNotifier {
                    ctx,
                    use_color: std::io::stdout().is_terminal(),
                }),
                DigestTargetArg::Slack => {
                    match std::env::var("DRUMS_SLACK_WEBHOOK_URL") {
                        Ok(url) if !url.is_empty() => {
                            Box::new(digest::SlackWebhookNotifier::new(url, ctx))
                        }
                        _ => {
                            eprintln!("drums digest: --to slack requires DRUMS_SLACK_WEBHOOK_URL to be set");
                            std::process::exit(2);
                        }
                    }
                }
            };

            match notifier.send(&d).await {
                Ok(()) => Ok(()),
                Err(e) => {
                    eprintln!("drums digest: could not send via {}: {e}", notifier.name());
                    std::process::exit(1);
                }
            }
        }
        Commands::Open {
            failure_id,
            repo,
            no_editor,
            close,
        } => {
            let repo = repo.unwrap_or(std::env::current_dir()?);
            let record_path = repo.join(".drums").join("record.jsonl");

            if close {
                match open::close_repair(&repo, &failure_id) {
                    Ok(Some(dir)) => {
                        println!("removed {}", dir.display());
                        Ok(())
                    }
                    Ok(None) => {
                        println!("nothing to remove — no inspection worktree for {failure_id}");
                        Ok(())
                    }
                    Err(e) => {
                        eprintln!("drums open --close: {e}");
                        std::process::exit(1);
                    }
                }
            } else {
                match open::open_repair(&repo, &record_path, &failure_id, !no_editor) {
                    Ok(o) => {
                        println!(
                            "{} {}",
                            if o.reused { "reusing" } else { "created" },
                            o.worktree.display()
                        );
                        println!("  {} at {}", o.branch, &o.sha[..o.sha.len().min(7)]);
                        println!("  {}", o.summary.lines().next().unwrap_or(""));
                        match &o.editor {
                            Some(e) => {
                                println!("  opened with `{}` ({})", e.argv.join(" "), e.source)
                            }
                            None => println!("  no editor launched"),
                        }
                        println!();
                        println!("when you are done: drums open {failure_id} --close");
                        Ok(())
                    }
                    Err(e) => {
                        eprintln!("drums open: {e}");
                        // A missing editor is the one failure with an obvious
                        // next step, so name it rather than leaving them to
                        // guess which variable Drums reads.
                        if matches!(e, open::OpenError::NoEditor) {
                            eprintln!("  try: DRUMS_EDITOR=code drums open {failure_id}");
                            eprintln!("  or:  drums open {failure_id} --no-editor");
                        }
                        std::process::exit(1);
                    }
                }
            }
        }
        Commands::Activate { key } => {
            let now = record_cmd::now_ms();
            match license::activate(&key, now) {
                Ok((l, path)) => {
                    print!("{}", license::activated_message(&l, &path, now));
                    std::io::stdout().flush().ok();
                    Ok(())
                }
                Err(why) => {
                    eprintln!("drums activate: {why}");
                    // The one thing worth saying after a failed activation:
                    // nothing was taken away. A key that will not verify
                    // leaves the machine exactly where it was, which is the
                    // whole free product.
                    eprintln!("  nothing was stored, and nothing changed — the loop up to a verified, proposed repair");
                    eprintln!("  does not need a key and is unaffected.");
                    std::process::exit(1);
                }
            }
        }
        Commands::Authority { action } => authority_command(action),
        Commands::Daemon { action } => daemon_command(action).await,
        Commands::Login { no_browser } => login::run(!no_browser)
            .await
            .map(|_| ())
            .map_err(Into::into),
        Commands::Logout => {
            if login::forget()? {
                println!("Signed out. The token is still valid until revoked in the console.");
            } else {
                println!("This machine was not signed in.");
            }
            Ok(())
        }
        Commands::Repair {
            input,
            outcome_file,
            repo,
        } => {
            let repo = repair_cmd::resolve_repo(repo);
            // A malformed instruction still produces an outcome file — the
            // runner uploads whatever is there, and "we could not read the
            // instruction" is a far more useful artifact than an empty one —
            // but it is an operator error, so the reason goes to stderr and
            // the exit code is nonzero rather than a quiet "no repair".
            let instruction = match repair_cmd::read_input(&input) {
                Ok(instruction) => instruction,
                Err(why) => {
                    let outcome = repair_cmd::RepairOutcome::nothing_established(
                        None,
                        String::new(),
                        why.clone(),
                    );
                    repair_cmd::write_outcome(&outcome_file, &outcome)?;
                    eprintln!("drums repair: {why}");
                    eprintln!(
                        "  no repair — outcome written to {}",
                        outcome_file.display()
                    );
                    std::process::exit(2);
                }
            };
            let outcome = repair_cmd::run(instruction, repo).await;
            let repaired = outcome.repaired;
            repair_cmd::write_outcome(&outcome_file, &outcome)?;
            println!(
                "{} — outcome written to {}",
                if repaired {
                    "repair ready"
                } else {
                    "no repair"
                },
                outcome_file.display()
            );
            Ok(())
        }
        Commands::Whoami => match login::load()? {
            Some(creds) => {
                // The account and the console, never the token. `whoami` is
                // the command people run inside screen shares.
                println!("{} at {}", creds.account, creds.console_url);
                Ok(())
            }
            None => {
                println!("Not signed in. Run `drums login`.");
                Ok(())
            }
        },
    }
}

/// `drums daemon <action>` dispatch, split out of `main`'s match arm purely
/// for readability — every branch here is still exactly one command's worth
/// of wiring around `drums_watch::daemon`/`config`, no different in kind
/// from the other `Commands::*` arms above.
/// What a typed answer to `Proceed? [Y/n]` means. Only consulted when stdin
/// is a terminal — a non-TTY stdin never sees the prompt at all (EOF is not
/// consent, it is nobody answering).
fn init_answer_is_consent(answer: &str) -> bool {
    let a = answer.trim().to_lowercase();
    a.is_empty() || a == "y" || a == "yes"
}

/// `drums init` — detect, plan, confirm once, write, and prove it works.
///
/// One command, one terminal. See `setup.rs` for why the verification can run
/// without a second window.
async fn init_everything(
    repo: &Path,
    yes: bool,
    service: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    use drums_watch::setup;
    use std::io::Write as _;

    let plan = setup::plan(repo, service);
    let f = &plan.findings;

    // The wizard's whole voice: green ✓ for a fact established, dim · for a
    // calm non-blocker, bold for the words a person acts on. Styled only for
    // a real terminal — DRUMS_PLAIN or a pipe gets clean text, same boundary
    // rule as render.rs, restated here because the bin cannot see the lib's
    // crate-private constants.
    let styled = std::env::var_os("DRUMS_PLAIN").is_none() && std::io::stdout().is_terminal();
    let g = |on: &str| {
        if styled {
            on.to_string()
        } else {
            String::new()
        }
    };
    let (bold, dim, green, reset) = (g("\x1b[1m"), g("\x1b[2m"), g("\x1b[32m"), g("\x1b[0m"));
    let ok = format!("{green}✓{reset}");
    let soft = format!("{dim}·{reset}");

    let name = repo
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| repo.display().to_string());
    println!("{bold}drums init{reset} · {name}");
    println!(
        "{dim}{} · drums {}{reset}",
        repo.display(),
        env!("CARGO_PKG_VERSION")
    );
    println!();

    if !f.is_git {
        eprintln!("  {} is not a git repository.", repo.display());
        eprintln!("  Drums works by rebuilding revisions, so it needs git. Run `git init` first.");
        std::process::exit(1);
    }
    println!("  {ok} git repository");

    match (&f.framework, &f.detection_claim, f.detection_provenance) {
        (Some(fr), Some(claim), Some(p)) => {
            println!("  {ok} {} {dim}— {claim} [{}]{reset}", fr.label(), p.chip());
        }
        _ => {
            // Not fatal: the loop works without the reporting snippet if
            // failures arrive some other way. Say what is lost, not just what
            // is missing — in one calm line, not a paragraph.
            println!(
                "  {soft} framework {dim}— none detected; Express and Next.js wire themselves, \
                 anything else POSTs to /v1/events{reset}"
            );
        }
    }
    if let Some(e) = &f.entrypoint {
        println!(
            "  {ok} entrypoint {dim}{}{reset}",
            e.strip_prefix(repo).unwrap_or(e).display()
        );
    }
    match &f.agent {
        Some(a) => println!("  {ok} {a} {dim}— writes the repairs{reset}"),
        None => {
            println!(
                "  {soft} agent {dim}— none on PATH; Drums still detects, attributes and \
                 reproduces — it just cannot repair until `claude` or `codex` exists{reset}"
            );
        }
    }
    println!();

    if plan.is_noop() {
        println!("Everything is already set up. Nothing to change.");
        println!();
        // `drums watch` refuses without an account, so printing it on its own
        // to somebody who has none would be printing a command that fails.
        if !matches!(login::load(), Ok(Some(_))) {
            println!("  drums login                 once, in a browser");
        }
        println!("  drums watch                 start the loop");
        return Ok(());
    }

    println!("This will:");
    for (kind, path, why) in plan.changes() {
        let shown = path
            .strip_prefix(repo)
            .unwrap_or(&path)
            .display()
            .to_string();
        let mark = if kind == "new" { "+" } else { "~" };
        println!("  {mark} {shown:<38} {dim}{why}{reset}");
    }
    println!();

    if !yes {
        if std::io::stdin().is_terminal() {
            print!("Proceed? [Y/n] ");
            std::io::stdout().flush().ok();
            let mut answer = String::new();
            std::io::stdin().read_line(&mut answer)?;
            if !init_answer_is_consent(&answer) {
                println!("Nothing was written.");
                return Ok(());
            }
        } else {
            // No terminal on stdin: a question printed to nobody, with EOF
            // read back as a "yes", says nothing about consent. Agents
            // pasting the install prompt rely on non-interactive init
            // succeeding, so proceed — and say so, in one line, so the
            // transcript states what happened.
            println!("non-interactive: proceeding");
        }
        println!();
    }

    if plan.write_config {
        match drums_watch::config::write_default(repo) {
            Ok(p) => println!(
                "  {ok} wrote {}",
                p.strip_prefix(repo).unwrap_or(&p).display()
            ),
            Err(e) => {
                eprintln!("  could not write the config: {e}");
                std::process::exit(1);
            }
        }
    }
    if let Some(wp) = &plan.wire_plan {
        match drums_watch::wire::apply(wp, &drums_watch::wire::snippet_for) {
            Ok(written) => {
                for p in &written {
                    // "wrote" on a file the team authored reads as replaced;
                    // an append is an edit and says so.
                    let edited = wp
                        .changes
                        .iter()
                        .any(|c| &c.path == p && c.kind == drums_watch::wire::ChangeKind::Edit);
                    let verb = if edited { "edited" } else { "wrote" };
                    println!(
                        "  {ok} {verb} {}",
                        p.strip_prefix(repo).unwrap_or(p).display()
                    );
                }
            }
            Err(e) => {
                eprintln!("  could not install the reporter: {e}");
                std::process::exit(1);
            }
        }
    }
    println!();

    // The proof. Same check the old two-terminal flow ran, against a listener
    // this process owns.
    println!("{dim}verifying the path from your app to the record…{reset}");
    let record_path = repo.join(".drums").join("record.jsonl");
    let (text, provenance) = match tokio::time::timeout(
        setup::VERIFY_TIMEOUT,
        setup::verify_round_trip_locally(&record_path),
    )
    .await
    {
        Ok(r) => r,
        Err(_) => (
            format!(
                "the check did not finish within {}s — nothing was established either way",
                setup::VERIFY_TIMEOUT.as_secs()
            ),
            engine_core::Provenance::Unresolved,
        ),
    };
    if provenance == engine_core::Provenance::Verified {
        let text = text.replace(&format!("{}/", repo.display()), "");
        println!("  {ok} {text} {dim}[{}]{reset}", provenance.chip());
    } else {
        println!("  {text} [{}]", provenance.chip());
    }
    println!();

    // Nothing above this line consulted an account, and nothing above it ever
    // will: `drums init` is the command somebody runs to watch Drums prove
    // itself before they have decided anything, and a sign-in wall in front of
    // it would mean the proof is only available to people who already trusted
    // us. The sign-in is named AFTER the verification, as the next step it
    // genuinely is — `drums watch` refuses without one.
    let signed_in = matches!(login::load(), Ok(Some(_)));

    if provenance == engine_core::Provenance::Verified {
        println!("{bold}Set up.{reset} Start the loop:");
        println!();
        if !signed_in {
            println!("  {bold}drums login{reset}   {dim}once, in a browser{reset}");
        }
        println!("  {bold}drums watch{reset}");
    } else {
        // Never claim setup worked when the one check that could prove it did
        // not pass.
        println!("The files are written, but the round trip did not complete, so this is");
        println!("NOT a working install yet.");
    }
    println!();

    // Earned, not demanded. It arrives after a command that asked for nothing,
    // and it stops arriving the moment it is accepted.
    if let Some(invitation) = account::invitation(signed_in) {
        for line in invitation.lines() {
            println!("{line}");
        }
        println!();
    }

    // The two things Drums cannot do for them, last so they are the thing left
    // on screen.
    println!("Your app's side — two lines, and {bold}drums doctor{reset} checks them any time:");
    println!();
    println!(
        "  1  DRUMS_INGEST_URL=http://127.0.0.1:{}   {dim}in the app's environment —",
        setup::DEFAULT_INGEST_PORT
    );
    println!("     without it the reporter is a silent no-op{reset}");
    println!("  2  {dim}tell Drums which commit shipped, from your deploy step:{reset}");
    for line in setup::deploy_hook_snippet(setup::DEFAULT_INGEST_PORT).lines() {
        println!("     {dim}{line}{reset}");
    }
    println!();
    // The consequence of skipping this used to be spelled out here AND again in
    // the `deploys` verdict below, fifteen lines apart. Said once, by the line
    // that also knows whether it has actually happened.

    // A third thing, but only for somebody who has committed the repair
    // workflow — local mode drives the agent on this machine and needs no
    // secret at all, so naming one would be inventing work.
    //
    // Deliberately NOT run for them. `claude setup-token` mints a one-year
    // bearer token against a named person's Claude account: it consumes a
    // per-person seat with a machine, and it honours `forceLoginMethod` but
    // not `forceLoginOrgUUID`, so it can land in a different organisation
    // than an enterprise admin pinned. Both are the customer's calls to make.
    // Drums names the command and gets out of the way — it never runs it,
    // never sees the token, and is never the reason a seat was spent.
    if let doctor::Status::Bad { .. } = doctor::check_agent_credentials(repo).status {
        println!("  3  {dim}for team mode, the CI runner needs an agent credential in YOUR");
        println!("     repo secrets — Drums cannot read it back:{reset}");
        println!("       claude setup-token {dim}&& {reset}gh secret set CLAUDE_CODE_OAUTH_TOKEN");
    }

    // End on a verdict, not a wall. An earlier version printed the entire
    // doctor table here, and a founder watching a prospect's first install
    // named what that does: the success story drowns in its own diagnosis.
    // The diagnostic still RUNS — the summary line is real counts, not vibes
    // — but the table lives where somebody asks for it, in `drums doctor`.
    let checks = doctor::run(repo, VERSION);
    let (mut n_ok, mut n_bad, mut n_unknown) = (0usize, 0usize, 0usize);
    for c in &checks {
        match c.status {
            doctor::Status::Ok(_) => n_ok += 1,
            doctor::Status::Bad { .. } => n_bad += 1,
            doctor::Status::Unknown(_) => n_unknown += 1,
        }
    }
    println!();
    if n_bad > 0 {
        println!(
            "{green}{n_ok} checks pass{reset} · {dim}{n_unknown} unknowable yet{reset} · \
             {n_bad} waiting on the two lines above — {bold}drums doctor{reset} {dim}names each{reset}"
        );
    } else {
        println!(
            "{green}{n_ok} checks pass{reset} · {dim}{n_unknown} unknowable yet — \
             {reset}{bold}drums doctor{reset} {dim}any time for the full picture{reset}"
        );
    }

    Ok(())
}

/// `drums init --wire` / `--verify`.
///
/// Two separate things on purpose. `--wire` changes files and can only be
/// checked by reading. `--verify` changes nothing and is the only thing that
/// can produce a `verified` claim, because it makes a real event travel the
/// real path. Running the first and believing it worked is the mistake this
/// command exists to prevent.
async fn init_wire(
    repo: &Path,
    wire: bool,
    yes: bool,
    service: Option<String>,
    verify: bool,
    ingest_url: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    use drums_watch::wire::{self, ChangeKind};

    if wire {
        let service = service.unwrap_or_else(|| {
            repo.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "app".to_string())
        });

        let plan = match wire::plan(repo, &service) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("drums init --wire: {e}");
                std::process::exit(1);
            }
        };

        println!(
            "detected {} — {} [{}]",
            plan.detected.framework.label(),
            plan.detected.claim.text,
            plan.detected.claim.provenance.chip()
        );
        println!();

        if plan.changes.is_empty() {
            println!("nothing to change — this repo already has the reporting snippet wired.");
        } else {
            println!("{} change(s):", plan.changes.len());
            println!();
            for c in &plan.changes {
                let label = match c.kind {
                    ChangeKind::NewFile => "new file",
                    ChangeKind::Edit => "EDIT to a file you wrote",
                };
                println!("  [{label}] {}", c.path.display());
                for line in c.diff.lines() {
                    println!("    {line}");
                }
                println!();
            }
        }

        if !plan.manual.is_empty() {
            println!("you still have to:");
            for m in &plan.manual {
                println!("  · {m}");
            }
            println!();
        }

        if !yes {
            // The whole point: a plan is a thing you read.
            println!("nothing was written. Re-run with --yes to apply.");
            return Ok(());
        }

        let written = wire::apply(&plan, &wire::snippet_for)?;
        if written.is_empty() {
            println!("nothing needed writing.");
        } else {
            for p in &written {
                println!("wrote {}", p.display());
            }
        }
        println!();
        println!("this is NOT proof it works. Start `drums watch`, then:");
        println!("  drums init --verify");
    }

    if verify {
        let record_path = repo.join(".drums").join("record.jsonl");
        let marker = format!("drums-wire-check-{}", ulid::Ulid::new());
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        println!("sending a test event to {ingest_url} …");
        let outcome = wire::verify_round_trip(
            ingest_url,
            &record_path,
            &marker,
            "drums init --verify",
            now,
        )
        .await;
        println!(
            "  {} [{}]",
            outcome.claim.text,
            outcome.claim.provenance.chip()
        );

        if outcome.claim.provenance != engine_core::Provenance::Verified {
            // Exit non-zero so this is usable in a setup script that should
            // stop when the wiring is not actually working.
            std::process::exit(1);
        }
        println!();
        println!("the path from your app to the record works. Failures will reach Drums.");
    }

    Ok(())
}

/// `drums authority`. Read-only except for the two explicit write actions, and
/// both of those append to the same record everything else reads — there is no
/// separate authority state that could disagree with the audit trail.
fn authority_command(action: AuthorityCommand) -> Result<(), Box<dyn std::error::Error>> {
    use engine_authority::{demote, promote, Ladder, Rung};

    fn record_path(repo: Option<PathBuf>) -> Result<PathBuf, Box<dyn std::error::Error>> {
        let repo = match repo {
            Some(r) => r,
            None => std::env::current_dir()?,
        };
        Ok(repo.join(".drums").join("record.jsonl"))
    }

    fn now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    fn rung_label(r: Rung) -> &'static str {
        match r {
            Rung::Observe => "observe",
            Rung::Shadow => "shadow",
            Rung::Propose => "propose",
            Rung::ActAlone => "act-alone",
        }
    }

    match action {
        AuthorityCommand::List { repo } => {
            let path = record_path(repo)?;
            let ladder = Ladder::load(&path)?;
            let all = ladder.all();

            if all.is_empty() {
                // Not an error, and not an empty table either. Nothing has
                // shipped yet, and saying so is more useful than column
                // headers over no rows.
                println!("no failure class has shipped a repair yet, so nothing has earned or lost authority.");
                println!("every class proposes by default — that is the product working, not a restriction.");
                return Ok(());
            }

            println!(
                "{:<28} {:<10} {:>6} {:>7} {:>5}",
                "CLASS", "RUNG", "STREAK", "SHIPPED", "BAD"
            );
            for (class, s) in &all {
                println!(
                    "{:<28} {:<10} {:>6} {:>7} {:>5}",
                    class.key(),
                    rung_label(s.rung),
                    s.streak,
                    s.total_clean,
                    s.total_bad
                );
            }

            let candidates = ladder.promotion_candidates();
            if !candidates.is_empty() {
                println!();
                println!("earned a promotion — apply with `drums authority promote <class>`:");
                for c in &candidates {
                    println!("  {}", c.class.key());
                    println!("    {}", c.because);
                }
                // Said here because this listing points at a command that
                // would otherwise refuse without warning. One line, on a
                // surface the operator asked for — not a recurring notice.
                if !license::current_status(now()).grants_act_alone() {
                    println!("    act-alone is the paid part of Drums; `drums authority promote` says what that means.");
                }
            }
            if ladder.skipped > 0 {
                println!();
                println!(
                    "({} unreadable record line(s) were skipped)",
                    ladder.skipped
                );
            }
            Ok(())
        }
        AuthorityCommand::Promote { class, repo } => {
            let path = record_path(repo)?;
            let ladder = Ladder::load(&path)?;

            // Refuse to promote a class that has not earned it. The command
            // exists to APPLY evidence, not to substitute for it — otherwise
            // it is the configurable autonomy switch this design rejects.
            let parsed = engine_authority::FailureClass::parse(&class).ok_or_else(|| {
                format!("`{class}` is not a class key — expected `service/ErrorName`")
            })?;
            let earned = ladder
                .promotion_candidates()
                .into_iter()
                .any(|c| c.class == parsed);
            if !earned {
                let s = ladder.standing(&parsed);
                eprintln!("drums authority promote: {class} has not earned act-alone yet.");
                eprintln!(
                    "  it is on {} with a streak of {} — {} consecutive clean ships are needed.",
                    rung_label(s.rung),
                    s.streak,
                    engine_authority::PROMOTION_STREAK
                );
                eprintln!("  autonomy is earned, not configured (spec §11): there is deliberately no override.");
                std::process::exit(1);
            }

            // THE PAYWALL, and the only one in the product.
            //
            // Deliberately AFTER the earned check, in that order. A class that
            // has not earned act-alone is refused on the evidence and money is
            // never mentioned — we do not ask for payment for something we
            // cannot yet show is deserved. Only a customer standing in front
            // of a promotion their own production record earned is told that
            // this last step is the paid one.
            let now_ms = now();
            let status = license::current_status(now_ms);
            if !status.grants_act_alone() {
                let s = ladder.standing(&parsed);
                eprint!(
                    "{}",
                    license::act_alone_refusal(&parsed.key(), s.streak, &status, now_ms)
                );
                std::io::stderr().flush().ok();
                std::process::exit(1);
            }

            let (c, rung) = promote(&path, &class, now_ms)?;
            println!("{} is now {}.", c.key(), rung_label(rung));
            println!(
                "one rollback or failed ship drops it straight back to propose, automatically."
            );
            Ok(())
        }
        AuthorityCommand::Demote { class, repo } => {
            let path = record_path(repo)?;
            let ladder = Ladder::load(&path)?;
            let parsed = engine_authority::FailureClass::parse(&class).ok_or_else(|| {
                format!("`{class}` is not a class key — expected `service/ErrorName`")
            })?;
            if let Some(refusal) = demote_unknown_class_refusal(&ladder, &parsed) {
                eprintln!("drums authority demote: {refusal}");
                std::process::exit(1);
            }
            let (c, rung) = demote(&path, &class, now())?;
            println!(
                "{} is now {}. It will propose instead of shipping.",
                c.key(),
                rung_label(rung)
            );
            Ok(())
        }
    }
}

/// Demoting a class the record has never seen would append a transition line
/// claiming a rung nothing ever held — a record entry about an event that
/// did not happen. Refuse before anything is written.
fn demote_unknown_class_refusal(
    ladder: &engine_authority::Ladder,
    class: &engine_authority::FailureClass,
) -> Option<String> {
    if ladder.all().iter().any(|(c, _)| c == class) {
        None
    } else {
        Some(format!(
            "class {} has no recorded rung — nothing to demote (`drums authority list` shows the classes the record holds)",
            class.key()
        ))
    }
}

async fn daemon_command(action: DaemonCommand) -> Result<(), Box<dyn std::error::Error>> {
    match action {
        DaemonCommand::Start {
            repo,
            ingest_port,
            threshold,
            window_secs,
            app_root,
            repair,
            deploy_cmd,
            check_url,
            repair_reported,
        } => {
            let repo = repo.unwrap_or(std::env::current_dir()?);
            // Canonicalize so the pidfile/argv/logfile all agree on one
            // absolute path regardless of the cwd `drumsd` (a detached
            // child) happens to inherit. Best-effort: a repo path that
            // doesn't exist yet fails loudly a few lines below anyway (state
            // dir creation), so falling back to the given path here just
            // lets that honest failure happen instead of a confusing one
            // about canonicalization.
            let repo = std::fs::canonicalize(&repo).unwrap_or(repo);

            let flags = drums_watch::config::DaemonFlags {
                ingest_port,
                threshold,
                window_secs,
                app_root,
                agent_cmd: None,
                boot_timeout_ms: None,
                repair_mode: repair.map(|r| match r {
                    RepairModeArg::Propose => "propose".to_string(),
                    RepairModeArg::Auto => "auto".to_string(),
                }),
                deploy_cmd,
                check_url,
            };
            let cfg = match drums_watch::config::load(&repo) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("drums daemon start: {e}");
                    std::process::exit(1);
                }
            };
            let settings = match drums_watch::config::resolve(&repo, &flags, &cfg) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("drums daemon start: {e}");
                    std::process::exit(2);
                }
            };

            // Idempotent: starting an already-running daemon is not an
            // error, it's a no-op that says so honestly. `Stale` (a pidfile
            // whose pid is dead) falls through and starts fresh, exactly
            // like there being no pidfile at all.
            let now = record_cmd::now_ms();
            if let Ok(daemon::DaemonStatus::Running { pid, .. }) = daemon::status(&repo, now) {
                println!(
                    "drumsd is already running (pid {pid}) for {} — not starting a second one.",
                    repo.display()
                );
                return Ok(());
            }

            let start_args = daemon::StartArgs {
                repo: repo.clone(),
                ingest_port: settings.ingest_port,
                threshold: settings.threshold,
                window_secs: settings.window_secs,
                app_root: settings.app_root.clone(),
                repair_auto: settings.repair_mode_auto,
                deploy_cmd: settings.deploy_cmd.clone(),
                check_url: settings.check_url.clone(),
                repair_reported,
            };
            let (binary, argv) = match daemon::resolve_daemon_invocation(&start_args) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("drums daemon start: {e}");
                    std::process::exit(1);
                }
            };
            let log_path = daemon::logfile_path(&repo);

            #[cfg(unix)]
            let spawned = daemon::spawn_detached(&binary, &argv, &log_path);
            #[cfg(not(unix))]
            let spawned: Result<std::process::Child, String> =
                Err("drums daemon start is only implemented on Unix today".to_string());

            match spawned {
                Ok(child) => {
                    let pid = child.id();
                    let record = daemon::PidRecord {
                        pid,
                        repo: repo.clone(),
                        ingest_port: settings.ingest_port,
                        started_at_ms: now,
                        log_path: log_path.clone(),
                    };
                    if let Err(e) = daemon::write_pidfile(&repo, &record) {
                        eprintln!("drums daemon start: drumsd started (pid {pid}) but its pidfile could not be written: {e} — it is running unmanaged; find and stop it manually.");
                        std::process::exit(1);
                    }
                    println!("drumsd started — pid {pid}");
                    println!("  pidfile: {}", daemon::pidfile_path(&repo).display());
                    println!("  log:     {}", log_path.display());
                    Ok(())
                }
                Err(e) => {
                    eprintln!("drums daemon start: could not launch the daemon: {e}");
                    eprintln!(
                        "  it would have run `{}` — if that binary is unusable here, set DRUMS_DAEMON_BIN to a drumsd binary, or run `drums watch` in a terminal instead.",
                        binary.display()
                    );
                    std::process::exit(1);
                }
            }
        }
        DaemonCommand::Stop { repo } => {
            let repo = repo.unwrap_or(std::env::current_dir()?);
            #[cfg(unix)]
            {
                match daemon::stop(&repo, Duration::from_secs(10)).await {
                    Ok(outcome) => {
                        println!("{}", daemon::render_stop_outcome(&outcome));
                        Ok(())
                    }
                    Err(e) => {
                        eprintln!("drums daemon stop: {e}");
                        std::process::exit(1);
                    }
                }
            }
            #[cfg(not(unix))]
            {
                eprintln!("drums daemon stop: only implemented on Unix today");
                std::process::exit(1);
            }
        }
        DaemonCommand::Status { repo } => {
            let repo = repo.unwrap_or(std::env::current_dir()?);
            let now = record_cmd::now_ms();
            match daemon::status(&repo, now) {
                Ok(s) => {
                    println!("{}", daemon::render_status(&s));
                    // Scripts gate on this: "is a daemon up" must be readable
                    // from the exit code, not parsed out of prose.
                    if !matches!(s, daemon::DaemonStatus::Running { .. }) {
                        std::process::exit(1);
                    }
                    Ok(())
                }
                Err(e) => {
                    eprintln!("drums daemon status: {e}");
                    std::process::exit(1);
                }
            }
        }
        DaemonCommand::Logs { repo, follow } => {
            let repo = repo.unwrap_or(std::env::current_dir()?);
            let log_path = daemon::logfile_path(&repo);
            let existing = match std::fs::read_to_string(&log_path) {
                Ok(c) => c,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // Printing nothing here is indistinguishable from an
                    // empty logfile; say which file is missing and what
                    // creates it. With --follow, keep watching for it.
                    println!(
                        "no logfile yet at {} — `drums daemon start` creates it",
                        log_path.display()
                    );
                    String::new()
                }
                Err(e) => {
                    eprintln!(
                        "drums daemon logs: could not read {}: {e}",
                        log_path.display()
                    );
                    std::process::exit(1);
                }
            };
            print!("{existing}");
            std::io::stdout().flush().ok();
            if !follow {
                return Ok(());
            }
            let mut pos = existing.len() as u64;
            let stop = wait_for_stop_signal();
            tokio::pin!(stop);
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(300)) => {
                        if let Ok(meta) = std::fs::metadata(&log_path) {
                            if meta.len() > pos {
                                if let Ok(mut f) = std::fs::File::open(&log_path) {
                                    let _ = f.seek(SeekFrom::Start(pos));
                                    let mut buf = String::new();
                                    if f.read_to_string(&mut buf).is_ok() {
                                        print!("{buf}");
                                        std::io::stdout().flush().ok();
                                    }
                                    pos = meta.len();
                                }
                            } else if meta.len() < pos {
                                // The logfile was truncated or replaced (a
                                // fresh `drums daemon start` after a stop) —
                                // resume from its new end rather than
                                // erroring on a seek past EOF.
                                pos = meta.len();
                            }
                        }
                    }
                    _ = &mut stop => { return Ok(()); }
                }
            }
        }
    }
}

/// First ~140 chars of a node body, cut on a word. Search results quote the
/// record; they do not reprint it.
fn snippet(body: &str) -> String {
    if body.len() <= 140 {
        return body.to_string();
    }
    let cut = body[..140].rfind(' ').unwrap_or(140);
    format!("{}…", &body[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_without_deploy_cmd_is_refused() {
        let err = validate_watch_repair_flags(RepairModeArg::Auto, &None, false)
            .expect_err("must refuse");
        assert!(err.contains("--deploy-cmd"), "{err}");
    }

    #[test]
    fn auto_with_deploy_cmd_is_accepted() {
        assert!(validate_watch_repair_flags(
            RepairModeArg::Auto,
            &Some("bash deploy.sh {sha}".to_string()),
            false
        )
        .is_ok());
    }

    #[test]
    fn propose_without_deploy_cmd_is_accepted() {
        assert!(validate_watch_repair_flags(RepairModeArg::Propose, &None, false).is_ok());
    }

    /// In hosted mode nothing is deployed by this process — the repair runs in
    /// the customer's CI. `--repair auto` there raises the authority ceiling
    /// and nothing else, so demanding a deploy command that would never run
    /// would be a flag the operator typed that does nothing.
    #[test]
    fn auto_without_deploy_cmd_is_accepted_when_repairs_are_dispatched() {
        assert!(validate_watch_repair_flags(RepairModeArg::Auto, &None, true).is_ok());
    }

    /// A reported issue has no replayable request, so it is never reproduced
    /// and can never be dispatched. Honouring `--repair-reported` alongside
    /// `--dispatch-repairs` would run an agent against this repo locally while
    /// every other repair went to CI — two execution planes chosen by an
    /// interaction nobody asked for.
    #[test]
    fn repair_reported_cannot_be_combined_with_dispatching() {
        let err = validate_dispatch_flags(true, true, false).expect_err("must refuse");
        assert!(err.contains("--repair-reported"), "{err}");
        assert!(
            err.contains("never reproduced"),
            "the refusal must say why: {err}"
        );
        // Neither flag alone is affected.
        assert_eq!(validate_dispatch_flags(false, true, false), Ok(None));
        assert_eq!(validate_dispatch_flags(true, false, false), Ok(None));
    }

    /// Warned, not refused: `--propose-pr` only wastes an expectation, it does
    /// not under-deliver on a promise. Same line the `--check-url` warning
    /// already draws.
    #[test]
    fn propose_pr_with_dispatching_warns_rather_than_refusing() {
        let warning = validate_dispatch_flags(true, false, true)
            .expect("a warning is not a refusal")
            .expect("it must say something");
        assert!(warning.contains("--propose-pr"), "{warning}");
        assert_eq!(validate_dispatch_flags(false, false, true), Ok(None));
    }

    /// Closing round (carried M-e): `app_root.rs` was thoroughly tested as a
    /// pure module, but the WIRING — this binary's forwarding task, the one
    /// thing that decides whether any of that derivation reaches the engine —
    /// had no test at all. A task that forwarded events without stripping, or
    /// dropped them, would have passed every existing test in the workspace.
    #[tokio::test]
    async fn the_deriver_task_strips_the_derived_prefix_and_passes_other_items_through() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join("lib")).unwrap();
        std::fs::write(repo.path().join("lib/total.js"), "x").unwrap();
        for args in [&["init", "-q"][..], &["add", "-A"][..]] {
            assert!(std::process::Command::new("git")
                .arg("-C")
                .arg(repo.path())
                .args(args)
                .status()
                .unwrap()
                .success());
        }
        for kv in [("user.email", "t@t"), ("user.name", "t")] {
            std::process::Command::new("git")
                .arg("-C")
                .arg(repo.path())
                .args(["config", kv.0, kv.1])
                .status()
                .unwrap();
        }
        assert!(std::process::Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(["commit", "-qm", "c"])
            .status()
            .unwrap()
            .success());

        let (in_tx, in_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut out_rx = spawn_app_root_deriver(in_rx, repo.path().to_path_buf());

        // A live app deployed at /srv/app reports absolute paths; the repo
        // knows the same file as `lib/total.js`.
        let ev = engine_core::ErrorEvent {
            service: "shop".into(),
            occurred_at_ms: 1,
            error_name: "TypeError".into(),
            error_message: "boom".into(),
            stack: "TypeError: boom\n    at computeTotal (/srv/app/lib/total.js:14:31)".into(),
            request: Some(engine_core::CapturedRequest {
                method: "POST".into(),
                path: "/api/checkout".into(),
                content_type: None,
                body: None,
            }),
            intake: engine_core::Intake::Snippet,
        };
        in_tx.send(engine_ingest::Ingested::Error(ev)).unwrap();

        let got = out_rx
            .recv()
            .await
            .expect("the event must be forwarded, never dropped");
        let engine_ingest::Ingested::Error(got) = got else {
            panic!("expected an Error item, got {got:?}")
        };
        assert!(
            got.stack.contains("at computeTotal (lib/total.js:14:31)"),
            "the deployment prefix must be stripped so the detector's signature matches what reproduction computes from the repo: {:?}",
            got.stack
        );
        assert!(!got.stack.contains("/srv/app"), "{:?}", got.stack);

        // A non-Error item must pass through untouched.
        let deploy = engine_core::DeployRecord {
            sha: "abc123".into(),
            description: "d".into(),
            author: "a".into(),
            deployed_at_ms: 2,
        };
        in_tx
            .send(engine_ingest::Ingested::Deploy(deploy.clone()))
            .unwrap();
        let got = out_rx.recv().await.expect("deploys must be forwarded too");
        let engine_ingest::Ingested::Deploy(got) = got else {
            panic!("expected a Deploy item, got {got:?}")
        };
        assert_eq!(got.sha, deploy.sha);
    }

    /// M-d: the memo key is caller-controlled (`ev.stack` off
    /// `POST /v1/events`), so the map must be bounded.
    #[test]
    fn the_app_root_cache_is_bounded_against_caller_controlled_keys() {
        let repo = tempfile::tempdir().unwrap();
        let mut cache = app_root::AppRootCache::new();
        for i in 0..3_000 {
            // Each distinct, each deriving nothing — the flood shape.
            cache.derive(&format!("/srv/app-{i}/x.js"), repo.path());
        }
        assert!(
            cache.len() <= 1024,
            "the cache must not grow without bound: {} entries",
            cache.len()
        );
    }

    /// Round-2 N3: the point of `install_tracing` is that `tracing::error!`
    /// stops being a no-op. Calling it twice must be harmless (`try_init`,
    /// not `init`) — a panic here would mean the very code path added to
    /// surface a record-write failure could itself take the CLI down.
    #[test]
    fn install_tracing_is_idempotent() {
        install_tracing();
        install_tracing();
    }

    #[test]
    fn propose_with_deploy_cmd_is_still_accepted() {
        // `--deploy-cmd` is also usable by a separately-run `drums ship`;
        // propose mode must not reject it just because auto isn't in use.
        assert!(validate_watch_repair_flags(
            RepairModeArg::Propose,
            &Some("bash deploy.sh {sha}".to_string()),
            false
        )
        .is_ok());
    }

    /// R5: an in-use ingest port used to print a raw anyhow/io Debug dump.
    /// The refusal must name the port and both next steps, and carry no Rust
    /// debug formatting.
    #[test]
    fn the_ingest_bind_refusal_names_the_port_and_the_next_steps() {
        let e = std::io::Error::new(
            std::io::ErrorKind::AddrInUse,
            "Address already in use (os error 48)",
        );
        let msg = ingest_bind_refusal(7787, &e);
        assert!(msg.contains("7787"), "{msg}");
        assert!(msg.contains("drums daemon status"), "{msg}");
        assert!(msg.contains("--ingest-port"), "{msg}");
        assert!(
            !msg.contains("Os {") && !msg.contains("kind:"),
            "no Rust debug formatting: {msg}"
        );
    }

    /// R13: only consulted when stdin is a terminal; an EOF'd read never
    /// reaches this because the prompt is never printed.
    #[test]
    fn a_typed_answer_consents_on_empty_y_and_yes_and_nothing_else() {
        for yes in ["", "\n", "y\n", "Y\n", "yes\n", "YES\n"] {
            assert!(init_answer_is_consent(yes), "{yes:?}");
        }
        for no in ["n\n", "no\n", "q\n", "anything else\n"] {
            assert!(!init_answer_is_consent(no), "{no:?}");
        }
    }

    /// C2: `drums authority demote` on a class the record has never seen must
    /// refuse — appending a transition would claim a rung nothing ever held.
    #[test]
    fn demoting_an_unseen_class_is_refused_and_a_seen_one_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("record.jsonl");
        let unseen = engine_authority::FailureClass::new("shop", "TypeError");

        let empty = engine_authority::Ladder::load(&record).unwrap();
        let refusal = demote_unknown_class_refusal(&empty, &unseen).expect("must refuse");
        assert!(refusal.contains("shop/TypeError"), "{refusal}");
        assert!(refusal.contains("no recorded rung"), "{refusal}");
        assert!(refusal.contains("nothing to demote"), "{refusal}");

        engine_authority::record_outcome(
            &record,
            &unseen,
            engine_authority::Outcome::ShippedClean,
            "f1",
            1_000,
        )
        .unwrap();
        let ladder = engine_authority::Ladder::load(&record).unwrap();
        assert!(
            demote_unknown_class_refusal(&ladder, &unseen).is_none(),
            "a class the record holds may always be demoted"
        );
    }
}
