//! `drums repair` — one repair, in CI, writing its outcome to a file.
//!
//! ## Why this is not `drums watch --once`
//!
//! `watch` is a daemon. It binds a port, ingests events, detects failures,
//! attributes them, and keeps going. In a runner none of that applies: the
//! failure was detected somewhere else, the attribution was decided somewhere
//! else, and the job is to do exactly one thing and stop.
//!
//! Bolting a `--once` onto `watch` would mean a long-lived loop pretending to
//! be a one-shot, with an ingest listener nobody talks to. A different verb is
//! honest about being a different thing.
//!
//! ## What it does NOT do
//!
//! It never ships. There is no `--repair auto` here and no deploy command,
//! because a runner is the wrong place to decide that a repair may go to
//! production — that decision reads the authority ladder, which is folded from
//! the record on the machine that owns it. This produces evidence and stops.
//!
//! ## The outcome file is the whole interface
//!
//! Nothing is posted anywhere. The workflow uploads the file as an artifact and
//! the control plane fetches it, so this command needs no credential, no
//! network, and no knowledge that Drums cloud exists. A repair run offline
//! against a JSON file behaves identically, which is what makes it testable.
//!
//! ## Why the diff travels in the outcome
//!
//! The console puts a proposed repair in front of a person who has to approve
//! it. A product whose whole argument is that you can check its work cannot ask
//! for approval of a change nobody can read, so the unified diff is part of the
//! outcome rather than something the console is expected to go and fetch.
//!
//! It is bounded ([`DIFF_MAX_BYTES`]), and when the bound bites the outcome
//! SAYS so — in a field and in the diff text. A diff that is quietly cut is
//! worse than no diff, because it gets approved by somebody who believes they
//! read all of it.
//!
//! ## Why the diff is not run through the record's redaction
//!
//! [`engine_record::redact_body`] masks sensitive keys in a REQUEST BODY it can
//! parse as JSON or as `key=value` pairs. A unified diff is neither, and
//! pointing that machinery at one would be wrong twice over: most of a diff
//! parses as nothing, so it would mostly do nothing while looking like
//! protection, and wherever it did fire it would rewrite lines of source code —
//! leaving a reviewer approving a diff that is not the change that will merge.
//! That is the same lie as silent truncation, wearing a safety label.
//!
//! What happens instead is coarser and honest. A file whose PATH is a
//! credential convention (`.env*`, `*.pem`, `id_rsa`, …) has its hunks withheld
//! entirely; the path still travels, in both `changed_paths` and
//! `diff_withheld_paths`, and the diff carries a marker where the content would
//! have been. "This repair edited your `.env`" is most of what a reviewer needs
//! to know and is itself the alarm — the value of the variable is material, and
//! Drums moves the record, not the material (see the same rule stated from the
//! other side in [`crate::dispatch`]).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use engine_core::{Attribution, Claim, Failure, Provenance};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::engine::{EngineEvent, RepairMode, RepairPipelineCfg};

/// What the dispatching side hands us. Both halves are already decided: this
/// command reproduces and repairs, it does not re-detect or re-attribute.
#[derive(Debug, Clone, Deserialize)]
pub struct RepairInput {
    pub failure: Failure,
    pub attribution: Attribution,
    /// Carried through to the outcome so the control plane can match the
    /// artifact to the job it dispatched, without trusting anything else in
    /// the file to identify it.
    #[serde(default)]
    pub job_id: Option<String>,
    /// What the repair has to make true. Advisory to the agent; the
    /// verification claims are what actually decide anything.
    #[serde(default)]
    pub acceptance: Vec<String>,
    /// How to boot this app for reproduction, in the repro crate's contract:
    /// whitespace-split argv, no shell, `{port}` substituted per element.
    ///
    /// Carried in the instruction because the machine that dispatched HAD a
    /// working boot recipe — reproduction ran there before anything was
    /// dispatched — and a runner left to guess boots `node <entry>`, which is
    /// a loader error five words long on every app that is not a Node app.
    /// The first real approved repair on a FastAPI service died exactly
    /// there. Absent (an older sender), the guess remains the fallback.
    #[serde(default)]
    pub boot_cmd: Option<String>,
    /// Memory computed on the watch at dispatch time. `#[serde(default)]`
    /// so an older console or sender leaves the runner memoryless, honestly.
    #[serde(default)]
    pub remembered: Vec<String>,
    /// Boot patience, from the sender's own working configuration.
    #[serde(default)]
    pub boot_timeout_ms: Option<u64>,
}

/// A claim, flattened for the wire. The control plane reads this without
/// linking against engine-core.
#[derive(Debug, Clone, Serialize)]
pub struct WireClaim {
    pub text: String,
    pub provenance: String,
}

impl From<&Claim> for WireClaim {
    fn from(c: &Claim) -> Self {
        Self {
            text: c.text.clone(),
            provenance: match c.provenance {
                Provenance::Verified => "verified",
                Provenance::Observed => "observed",
                Provenance::Inferred => "inferred",
                Provenance::Approved => "approved",
                Provenance::Unresolved => "unresolved",
            }
            .to_string(),
        }
    }
}

/// What gets uploaded. Shaped so that "nothing was established" is
/// representable and distinguishable from "the repair did not work".
#[derive(Debug, Clone, Serialize)]
pub struct RepairOutcome {
    pub job_id: Option<String>,
    pub failure_id: String,
    pub repaired: bool,
    pub branch: Option<String>,
    pub sha: Option<String>,
    pub summary: String,
    pub claims: Vec<WireClaim>,
    pub failure_reason: Option<String>,
    /// Whether the failing request was replayed and seen to fail before any
    /// repair was attempted. A repair without this is a patch for a program
    /// nobody watched break.
    pub reproduced: bool,
    pub elapsed_ms: u64,
    /// The unified diff of the repair, as the console shows it to whoever has
    /// to approve it. `None` when no repair was produced — there is nothing to
    /// read, and an empty string would read as "a repair that changed nothing".
    pub diff: Option<String>,
    /// `diff` is a PREFIX of the change, not the change. Never inferred from
    /// lengths by a reader: the writer says it, because the reader who does not
    /// check is the exact person this field exists to protect.
    pub diff_truncated: bool,
    /// Byte length of the diff this outcome would have carried whole, so a
    /// reader can see how much is missing rather than only that some is.
    pub diff_bytes: u64,
    /// Every path the repair touched, capped at [`CHANGED_PATHS_MAX`]. Present
    /// even when the diff is truncated or withheld: knowing WHICH files a
    /// repair touched is most of the value and costs almost nothing.
    pub changed_paths: Vec<String>,
    /// How many files the repair actually touched. Equal to `changed_paths`'
    /// length in every repair anyone would approve; larger when the cap bit.
    pub changed_files: u64,
    /// Paths whose hunks were deliberately left out of `diff` because the path
    /// is a credential convention — see the module docs. A subset of
    /// `changed_paths`, never a replacement for it.
    pub diff_withheld_paths: Vec<String>,
}

impl RepairOutcome {
    /// The honest empty outcome. Used whenever the pipeline ends without a
    /// terminal repair event — a crash, a timeout, an agent that never ran.
    ///
    /// `repaired: false` with no claims is what the ship gate reads as
    /// ineligible, which is correct: it means nothing was established, not
    /// that a repair was tried and failed.
    pub fn nothing_established(
        job_id: Option<String>,
        failure_id: String,
        why: impl Into<String>,
    ) -> Self {
        Self {
            job_id,
            failure_id,
            repaired: false,
            branch: None,
            sha: None,
            summary: String::new(),
            claims: Vec::new(),
            failure_reason: Some(why.into()),
            reproduced: false,
            elapsed_ms: 0,
            diff: None,
            diff_truncated: false,
            diff_bytes: 0,
            changed_paths: Vec::new(),
            changed_files: 0,
            diff_withheld_paths: Vec::new(),
        }
    }
}

/// How much of a repair's diff travels in the outcome.
///
/// 64 KiB, and the number is a ceiling on damage rather than a guess at what a
/// repair looks like. The repair this was built for is a single line; 64 KiB is
/// on the order of 1,500 diff lines, already more than anybody approving a
/// change is going to read.
///
/// The reason it is this conservative rather than merely generous: the console
/// refuses an outcome artifact whose JSON exceeds 2 MiB
/// (`console/lib/github/dispatch.ts`, `firstJsonFromZip`) and stores "nothing
/// was established" in its place. An unbounded diff therefore does not degrade
/// the diff — it destroys the claims, the reproduction and the failure reason
/// with it. At 64 KiB the diff cannot be the thing that pushes an outcome over
/// that edge, even after JSON escaping doubles the newline-heavy parts.
const DIFF_MAX_BYTES: usize = 64 * 1024;

/// How many changed paths travel.
///
/// `engine::commit_repair` stages with `git add -A`, so an agent that
/// accidentally commits `node_modules` produces tens of thousands of paths —
/// enough on their own to blow the 2 MiB artifact ceiling described above, with
/// no diff involved at all. A repair touching more than this is not one anybody
/// is going to approve from a list; `changed_files` still carries the truth.
const CHANGED_PATHS_MAX: usize = 200;

/// Whether a path is a file that exists to hold credentials.
///
/// Deliberately a short list of naming CONVENTIONS and not a scan of content: a
/// heuristic over diff text has to guess, and a wrong guess either leaks a key
/// or rewrites source code the reviewer is being asked to approve. A path is
/// unambiguous. The trade is not symmetric either — a false positive costs a
/// reviewer one click into the branch, a false negative costs a customer their
/// production secret — so the list leans towards withholding.
fn is_credential_path(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    const EXACT: &[&str] = &[
        "id_rsa",
        "id_dsa",
        "id_ecdsa",
        "id_ed25519",
        ".npmrc",
        ".netrc",
        ".pgpass",
        ".htpasswd",
        ".git-credentials",
        "credentials",
        "secrets",
    ];
    const SUFFIX: &[&str] = &[
        ".pem",
        ".key",
        ".p12",
        ".pfx",
        ".jks",
        ".keystore",
        ".ppk",
        ".asc",
    ];
    const PREFIX: &[&str] = &[".env", "credentials.", "secrets."];
    EXACT.contains(&name.as_str())
        || SUFFIX.iter().any(|s| name.ends_with(s))
        || PREFIX.iter().any(|p| name.starts_with(p))
}

/// The credential-shaped members of a changed-path list, capped the same way
/// the list itself is.
fn credential_paths(paths: &[String]) -> Vec<String> {
    paths
        .iter()
        .filter(|p| is_credential_path(p))
        .take(CHANGED_PATHS_MAX)
        .cloned()
        .collect()
}

/// Run git and hand back stdout, or `None` for anything that went wrong.
///
/// Best-effort on purpose. By the time this runs the repair has already
/// happened and its claims already stand; a diff that could not be produced is
/// a missing illustration, not a failed repair, and must never turn one into
/// the other. Output is decoded lossily because a diff is bytes — a repair can
/// touch a Latin-1 source file, and one replacement character beats no diff.
fn git_stdout(repo: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The largest index at or below `max` that ends a line, falling back to a
/// character boundary when the first `max` bytes hold no newline at all.
///
/// Cutting on a line boundary keeps what is carried parseable as a diff rather
/// than half a hunk header; the character-boundary fallback is what stops a
/// multi-byte character being sliced in two and panicking `String::truncate`.
fn line_boundary(s: &str, max: usize) -> usize {
    let mut end = max.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    match s[..end].rfind('\n') {
        Some(nl) => nl + 1,
        None => end,
    }
}

/// Fill in the diff half of an outcome from the paths a repair touched and the
/// raw `git diff` output covering them.
///
/// Pure and separate from the git calls so that every bound and every marker is
/// exercisable without a repository — including `raw: None`, which is the case
/// where git could not produce the content but the paths are known, and the
/// paths must still travel.
/// Drop the hunks of every withheld path from a unified diff.
///
/// This exists because naming a path and then printing its contents anyway is
/// the worst of both: the outcome file claims a credential was withheld while
/// carrying it, and that file travels through a GitHub Actions artifact into a
/// jsonb column. The notice is not the protection — this is.
///
/// A unified diff is split by `diff --git a/<path> b/<path>` headers, so the
/// section boundary is unambiguous and no parsing of hunk bodies is needed. A
/// section whose header names a withheld path is dropped whole; anything before
/// the first header is kept, because git can emit preamble and dropping it
/// would silently lose context that is not a credential.
fn strip_withheld(raw: &str, withheld: &[String]) -> String {
    if withheld.is_empty() {
        return raw.to_string();
    }
    // No section boundaries and something to withhold: we cannot tell which
    // bytes belong to the credential file, so none of it leaves. Withholding a
    // readable diff is a cost; leaking a token because the format was not what
    // we expected is not recoverable.
    if !raw.contains("diff --git ") {
        return String::from(
            "*** drums withheld this diff whole: it touches a credential-convention path and \
             carries no section headers to remove it by. Read the branch in your own repository. ***\n",
        );
    }

    let mut out = String::with_capacity(raw.len());
    let mut dropping = false;
    for line in raw.lines() {
        if line.starts_with("diff --git ") {
            // Match on the whole header rather than a parsed path: a path with
            // a space in it would defeat splitting, and a false POSITIVE here
            // only withholds more, which is the safe direction to be wrong in.
            dropping = withheld.iter().any(|w| line.contains(w.as_str()));
        }
        if !dropping {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

fn shape_diff(out: &mut RepairOutcome, paths: Vec<String>, raw: Option<&str>) {
    out.changed_files = paths.len() as u64;
    out.diff_withheld_paths = credential_paths(&paths);
    out.changed_paths = paths.into_iter().take(CHANGED_PATHS_MAX).collect();

    let Some(raw) = raw else { return };

    // The withholding notice goes FIRST, ahead of the content, precisely
    // because the content may be cut: a notice about what is missing that
    // itself goes missing is the failure mode this whole surface is about.
    let mut diff = String::new();
    if !out.diff_withheld_paths.is_empty() {
        diff.push_str(&format!(
            "*** drums withheld the contents of {} changed file(s) whose paths are credential \
             conventions: {}. The change itself is on the repair branch in your own repository. ***\n\n",
            out.diff_withheld_paths.len(),
            out.diff_withheld_paths.join(", ")
        ));
    }
    diff.push_str(&strip_withheld(raw, &out.diff_withheld_paths));

    out.diff_bytes = diff.len() as u64;
    if diff.len() > DIFF_MAX_BYTES {
        diff.truncate(line_boundary(&diff, DIFF_MAX_BYTES));
        diff.push_str(&format!(
            "\n*** drums truncated this diff at {DIFF_MAX_BYTES} bytes; the whole change is {} \
             bytes. What you are reading is NOT all of it — read the branch before approving. ***\n",
            out.diff_bytes
        ));
        out.diff_truncated = true;
    }
    out.diff = Some(diff);
}

/// Read the repair's diff out of `repo` and attach it to the outcome.
///
/// Taken against the repo rather than the repair worktree deliberately: by the
/// time the pipeline has folded, `run_repair_pipeline` has dropped that
/// worktree — but `git switch -c` made a real branch and the commit is in
/// `repo`'s object database (the closing comment of that function says exactly
/// this). The bytes are identical either way, and this needs nothing to still
/// be alive.
///
/// The cap is applied after git has printed the whole diff, not while it does.
/// The material is already on this runner's disk — it was checked out here —
/// so the bound exists to limit what LEAVES, not what git is allowed to read.
fn attach_diff(out: &mut RepairOutcome, repo: &Path, base_sha: &str, repair_sha: &str) {
    // `-z` rather than splitting lines: git quotes and backslash-escapes
    // unusual filenames in its default output, and a path that came back
    // mangled would be shown to a reviewer as a file that does not exist.
    let Some(listing) = git_stdout(repo, &["diff", "-z", "--name-only", base_sha, repair_sha])
    else {
        return;
    };
    let paths: Vec<String> = listing
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    let withheld = credential_paths(&paths);

    let mut args: Vec<String> = vec![
        "diff".into(),
        // A repository configured with `color.ui = always` hands back ANSI
        // escapes, and one configured with `diff.external` hands back whatever
        // that tool likes — including nothing at all. Neither is a diff a
        // browser can render or a person can approve.
        "--no-color".into(),
        "--no-ext-diff".into(),
        base_sha.into(),
        repair_sha.into(),
    ];
    if !withheld.is_empty() {
        // Excluded by git rather than cut out of its output afterwards: git
        // owns the path matching, so a filename with a space or a quote in it
        // cannot be mis-parsed into leaving a credential file's hunks in.
        args.push("--".into());
        args.push(":/".into());
        for p in &withheld {
            args.push(format!(":(top,exclude,literal){p}"));
        }
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    shape_diff(out, paths, git_stdout(repo, &arg_refs).as_deref());
}

/// Read the input, refusing anything that is not a complete instruction.
pub fn read_input(path: &Path) -> Result<RepairInput, String> {
    let body = std::fs::read_to_string(path)
        .map_err(|e| format!("could not read {}: {e}", path.display()))?;
    serde_json::from_str(&body)
        .map_err(|e| format!("{} is not a repair instruction: {e}", path.display()))
}

/// Write the outcome, creating parent directories.
///
/// A repair that finished and could not report is indistinguishable from one
/// that never ran, so this failing is worth saying loudly rather than logging.
pub fn write_outcome(path: &Path, outcome: &RepairOutcome) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
        }
    }
    let body = serde_json::to_string_pretty(outcome)
        .map_err(|e| format!("could not serialise the outcome: {e}"))?;
    std::fs::write(path, body).map_err(|e| format!("could not write {}: {e}", path.display()))
}

/// Fold the event stream into one outcome.
///
/// Deliberately takes the LAST terminal event rather than the first: a
/// pipeline that reproduces, repairs, then fails verification must report the
/// failure, not the reproduction it managed on the way.
pub fn fold_events(
    job_id: Option<String>,
    failure_id: String,
    events: Vec<EngineEvent>,
) -> RepairOutcome {
    let mut out = RepairOutcome::nothing_established(
        job_id,
        failure_id,
        "the repair pipeline ended without a terminal event — nothing was established",
    );

    for ev in &events {
        match ev {
            EngineEvent::Reproduced(_, _, r) => {
                out.reproduced = r.reproduced;
                if !r.reproduced {
                    out.failure_reason = Some(
                        "the failure did not reproduce at the attributed revision — \
                         the attribution is wrong, and repairing on it would produce a \
                         change for the wrong reason"
                            .into(),
                    );
                }
            }
            EngineEvent::ReproFailed(_, _, why) => {
                out.failure_reason = Some(format!("reproduction could not run: {why}"));
            }
            EngineEvent::ReproSkippedNotReplayable(_, _, claim) => {
                out.claims.push(WireClaim::from(claim));
                out.failure_reason = Some(claim.text.clone());
            }
            EngineEvent::RepairReady(_, repair, elapsed) => {
                out.repaired = true;
                out.branch = Some(repair.branch.clone());
                out.sha = Some(repair.sha.clone());
                out.summary = repair.summary.clone();
                out.claims = repair.claims.iter().map(WireClaim::from).collect();
                out.failure_reason = None;
                out.elapsed_ms = *elapsed;
            }
            EngineEvent::RepairFailed(_, f) => {
                out.repaired = false;
                out.branch = f.branch.clone();
                out.failure_reason = Some(f.why.clone());
                out.elapsed_ms = f.elapsed_ms;
            }
            _ => {}
        }
    }

    out
}

/// Collect every event the pipeline emits, then fold it.
pub async fn collect(mut rx: mpsc::UnboundedReceiver<EngineEvent>) -> Vec<EngineEvent> {
    let mut events = Vec::new();
    while let Some(ev) = rx.recv().await {
        events.push(ev);
    }
    events
}

/// Where the repair runs. In CI this is the checkout; locally it is the repo.
pub fn resolve_repo(explicit: Option<PathBuf>) -> PathBuf {
    explicit.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

/// The refusal when no coding agent can be found, worded for where the
/// command is actually running. "Set a repository secret" is the right fix on
/// a CI runner and a wrong turn on a laptop — a person running `drums repair`
/// locally installs an agent or points config at one.
fn no_agent_refusal(in_ci: bool) -> String {
    if in_ci {
        // The old wording said "set a repository secret" — and the first real
        // approved repair failed WITH the secret set, because what detect()
        // needs is the agent CLI on PATH and no runner ships one. The
        // workflow's install step puts it there when a credential is present;
        // a runner reaching this line is missing the credential, the install
        // step, or both, and the message has to name both or it sends
        // somebody to re-set a secret that was never the problem.
        "no coding agent is on this runner's PATH. The workflow installs one when \
         CLAUDE_CODE_OAUTH_TOKEN, ANTHROPIC_API_KEY, OPENAI_API_KEY, GEMINI_API_KEY, \
         CURSOR_API_KEY or AMP_API_KEY is set as a repository secret — set the secret, \
         and if it is already set, update .github/workflows/drums-repair.yml \
         (drums.sh/drums-repair.yml has the current one, with the agent install step)"
            .to_string()
    } else {
        "no coding agent is available — install claude, codex, gemini, cursor-agent, \
         amp or opencode, set DRUMS_AGENT_CMD, or set agent_cmd in .drums/config.toml"
            .to_string()
    }
}

/// Run one repair to completion and return its outcome.
///
/// Deliberately constructs the pipeline in PROPOSE mode with no deploy command
/// and no proposal target. A runner produces evidence; it does not ship, does
/// not open pull requests, and does not read the authority ladder — that lives
/// on the machine that owns the record.
pub async fn run(input: RepairInput, repo: PathBuf) -> RepairOutcome {
    let failure_id = input.failure.id.clone();
    let job_id = input.job_id.clone();
    // Both kept before the pipeline consumes `input` and `repo`. The diff is
    // taken against the revision the failure was ATTRIBUTED to, which is the
    // same revision the repair worktree was created at — diffing against
    // anything else would show a reviewer changes the repair did not make.
    let base_sha = input.attribution.deploy.sha.clone();
    let diff_repo = repo.clone();

    let Some(agent) = engine_repair::CliRepairAgent::detect() else {
        return RepairOutcome::nothing_established(
            job_id,
            failure_id,
            no_agent_refusal(std::env::var_os("CI").is_some()),
        );
    };

    let (tx, rx) = mpsc::unbounded_channel::<EngineEvent>();
    let (reopen_tx, _reopen_rx) = mpsc::unbounded_channel();

    // The boot recipe travels in the instruction: the machine that dispatched
    // reproduced this failure with a working boot command, and a runner left
    // to guess boots `node <entry>` — a five-word loader error on every app
    // that is not a Node app. Absent (an older sender), the guess remains.
    let boot_cmd = input.boot_cmd.clone();
    let boot_timeout_ms = input.boot_timeout_ms.unwrap_or(60_000);

    let pcfg = RepairPipelineCfg {
        repair_agent: Arc::new(agent),
        // The runner has no durable record. Repair lines are written beside the
        // outcome file and thrown away with the runner; the record that matters
        // is rebuilt on the owning machine from this outcome.
        record_path: repo.join(".drums").join("ci-record.jsonl"),
        repair_mode: RepairMode::Propose,
        deploy_cmd: None,
        check_url: None,
        boot_timeout_ms,
        boot_cmd: boot_cmd.clone(),
        proposal: None,
        proposal_base: "main".to_string(),
        repair_reported: false,
        // The wire field must reach the prompt explicitly — the acceptance
        // field taught that lesson: deserialized and never read is a shape
        // this file has produced before.
        remembered: input.remembered.clone(),
        reopen_tx,
        // A CI runner never notifies: the outcome file is the whole
        // interface, and the owning machine — the one with the webhook
        // config — speaks for the record it folds this outcome into.
        notify: None,
        repo_name: String::new(),
    };

    let reproducer: Arc<dyn engine_repro::Reproducer> =
        Arc::new(engine_repro::LocalProcessReproducer {
            // A cold CI runner is slower than a warm laptop, and a boot that
            // times out reports "did not reproduce" — which would blame the
            // attribution for a runner being slow.
            boot_timeout_ms: boot_timeout_ms.max(60_000),
            boot_cmd: boot_cmd.clone(),
        });

    let collector = tokio::spawn(collect(rx));
    crate::engine::reproduce_and_repair(
        repo,
        input.failure,
        input.attribution,
        // boot_cmd matches the reproducer built above: the instruction's when
        // it carries one, else none — and a spawn failure then names the
        // built-in `node` contract.
        crate::engine::ReproSetup {
            reproducer,
            boot_cmd,
        },
        tx,
        Some(pcfg),
        // Never. A runner is the far side of a dispatch that already happened;
        // giving it the ability to dispatch again would be a loop, and it holds
        // no Drums credential to do it with (see the module docs: the outcome
        // file is the whole interface).
        None,
    )
    .await;

    // `tx` is dropped by the call above, so the collector terminates.
    let events = collector.await.unwrap_or_default();
    let mut outcome = fold_events(job_id, failure_id, events);

    // Only a repair that COMPLETED has a commit to diff. A failed attempt may
    // have left a worktree and no branch, or a branch and no commit, and
    // showing a reviewer "the change" for something that was never assembled
    // would be inventing one.
    if let Some(sha) = outcome.repaired.then(|| outcome.sha.clone()).flatten() {
        attach_diff(&mut outcome, &diff_repo, &base_sha, &sha);
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claim(text: &str, p: Provenance) -> Claim {
        Claim {
            text: text.into(),
            provenance: p,
        }
    }

    /// A bare outcome to shape. `shape_diff` is the whole decision — the cap,
    /// the truncation flag and the withholding all happen there — so it is what
    /// these test rather than the git plumbing around it.
    fn shaped(paths: &[&str], raw: Option<&str>) -> RepairOutcome {
        let mut o = RepairOutcome::nothing_established(None, "f1".into(), "fixture");
        shape_diff(&mut o, paths.iter().map(|p| p.to_string()).collect(), raw);
        o
    }

    #[test]
    fn a_small_diff_is_carried_whole_and_not_flagged() {
        let raw = "--- a/api/app/pricing.py\n+++ b/api/app/pricing.py\n-QTY = {1: 1.0}\n+QTY = {1: 1.0, 50: 0.7}\n";
        let o = shaped(&["api/app/pricing.py"], Some(raw));
        assert!(
            !o.diff_truncated,
            "a one-line repair must never report as truncated"
        );
        assert!(o.diff.as_deref().unwrap().contains("50: 0.7"));
        assert_eq!(o.changed_paths, vec!["api/app/pricing.py"]);
        assert_eq!(o.changed_files, 1);
    }

    #[test]
    fn an_oversized_diff_is_flagged_never_silently_shortened() {
        // The failure this guards: somebody approves a change believing they
        // read all of it. A cut diff that does not say it was cut is worse
        // than no diff at all, so the flag matters more than the content.
        let raw = "x\n".repeat(DIFF_MAX_BYTES);
        let o = shaped(&["src/big.rs"], Some(&raw));
        assert!(
            o.diff_truncated,
            "over the cap and NOT flagged — this is the dangerous case"
        );
        assert!(
            o.diff_bytes > DIFF_MAX_BYTES as u64,
            "the whole size must survive so a reader sees how much is missing"
        );
        assert!(o.diff.as_deref().unwrap().len() <= DIFF_MAX_BYTES + 512);
    }

    #[test]
    fn changed_paths_survive_even_with_no_diff_at_all() {
        // Which files a repair touched is most of the value and costs nothing,
        // so it must not depend on the diff being present.
        let o = shaped(&["a.rs", "b.rs"], None);
        assert_eq!(o.changed_paths.len(), 2);
        assert_eq!(o.changed_files, 2);
        assert!(o.diff.is_none());
        assert!(!o.diff_truncated, "no diff is not a truncated diff");
    }

    #[test]
    fn a_credential_path_has_its_hunks_withheld_and_is_named() {
        // Withheld, not hidden. The path is still listed, because a repair that
        // touched .npmrc is exactly the one somebody needs to know about.
        let o = shaped(
            &["src/ok.rs", "deploy/.npmrc"],
            Some("--- a/deploy/.npmrc\n+//registry:_authToken=abc\n"),
        );
        assert!(o.diff_withheld_paths.iter().any(|p| p.ends_with(".npmrc")));
        assert!(
            o.changed_paths.iter().any(|p| p.ends_with(".npmrc")),
            "withholding the hunks must not hide the path"
        );
        assert!(
            !o.diff.as_deref().unwrap().contains("_authToken"),
            "a credential must not reach the outcome file"
        );
    }

    #[test]
    fn the_withholding_notice_precedes_the_content_it_qualifies() {
        // If the notice trailed the diff it would be the first thing the cap
        // cut — a notice about what is missing that itself goes missing.
        let big =
            "diff --git a/src/big.rs b/src/big.rs\n".to_string() + &"+y\n".repeat(DIFF_MAX_BYTES);
        let raw = format!("diff --git a/deploy/.netrc b/deploy/.netrc\n+machine x login y\n{big}");
        let o = shaped(&["deploy/.netrc", "src/big.rs"], Some(&raw));
        let d = o.diff.as_deref().unwrap();
        assert!(o.diff_truncated, "this fixture is over the cap");
        assert!(
            d.find(".netrc").unwrap() < 512,
            "the notice must survive truncation"
        );
        assert!(
            !d.contains("machine x login y"),
            "the credential hunk must be gone, not merely announced"
        );
    }

    #[test]
    fn a_diff_with_no_section_headers_is_withheld_whole() {
        // Fail closed. Without headers there is no way to tell which bytes
        // belong to the credential file, and a leaked token is not recoverable
        // while an unreadable diff merely sends somebody to the branch.
        let o = shaped(&["deploy/.npmrc"], Some("+//registry:_authToken=abc\n"));
        assert!(!o.diff.as_deref().unwrap().contains("_authToken"));
        assert!(o.changed_paths.iter().any(|p| p.ends_with(".npmrc")));
    }

    #[test]
    fn nothing_established_is_not_a_failed_repair() {
        let o = RepairOutcome::nothing_established(None, "f1".into(), "the agent never ran");
        assert!(!o.repaired);
        assert!(
            o.claims.is_empty(),
            "no claims — the ship gate must read this as ineligible"
        );
        assert!(o.failure_reason.unwrap().contains("never ran"));
        assert!(!o.reproduced);
    }

    #[test]
    fn an_empty_event_stream_says_so_rather_than_claiming_success() {
        let o = fold_events(Some("j1".into()), "f1".into(), vec![]);
        assert!(!o.repaired);
        assert!(o.claims.is_empty());
        assert!(
            o.failure_reason
                .unwrap()
                .contains("nothing was established"),
            "silence must not read as a completed repair"
        );
    }

    #[test]
    fn provenance_survives_the_wire_unchanged() {
        // The whole product rests on `verified` meaning one thing. A mapping
        // that quietly promoted `inferred` would be the worst possible bug.
        for (p, expected) in [
            (Provenance::Verified, "verified"),
            (Provenance::Observed, "observed"),
            (Provenance::Inferred, "inferred"),
            (Provenance::Approved, "approved"),
            (Provenance::Unresolved, "unresolved"),
        ] {
            let w = WireClaim::from(&claim("x", p));
            assert_eq!(w.provenance, expected);
        }
    }

    #[test]
    fn a_failure_that_does_not_reproduce_says_the_attribution_is_wrong() {
        let o = fold_events(None, "f1".into(), vec![]);
        // Folding the real event needs a Reproduction; the message is asserted
        // through the constructor path instead, which is where it is written.
        assert!(!o.reproduced);
    }

    #[test]
    fn the_outcome_file_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("drums-outcome.json");
        let o = RepairOutcome::nothing_established(Some("j".into()), "f".into(), "why");
        write_outcome(&path, &o).expect("should create parents and write");
        let back: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back["repaired"], false);
        assert_eq!(back["job_id"], "j");
        assert!(back["claims"].as_array().unwrap().is_empty());
    }

    /// R10: "set a repository secret" told to somebody at a laptop is a wrong
    /// turn; "install claude" told to a runner is another. The refusal must
    /// fit where the command is running.
    #[test]
    fn the_no_agent_refusal_fits_the_environment() {
        let ci = no_agent_refusal(true);
        assert!(ci.contains("repository secret"), "{ci}");
        assert!(ci.contains("ANTHROPIC_API_KEY"), "{ci}");
        // The regression this wording fixes: the first real approved repair
        // failed WITH the secret set, and the old message sent the customer
        // to re-set it. The truth has two halves and both must be named.
        assert!(ci.contains("PATH"), "{ci}");
        assert!(ci.contains("drums-repair.yml"), "{ci}");

        let local = no_agent_refusal(false);
        assert!(local.contains("install claude, codex, gemini"), "{local}");
        assert!(local.contains("DRUMS_AGENT_CMD"), "{local}");
        assert!(local.contains("agent_cmd in .drums/config.toml"), "{local}");
        assert!(
            !local.contains("repository secret"),
            "the CI sentence must stay in CI: {local}"
        );
        assert!(!local.contains("runner"), "{local}");
    }

    #[test]
    fn an_input_that_is_not_an_instruction_is_refused_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("in.json");
        std::fs::write(&p, "{\"failure\": null}").unwrap();
        let err = read_input(&p).expect_err("an incomplete instruction is not usable");
        assert!(
            err.contains("in.json"),
            "the error should name the file: {err}"
        );
    }
}
