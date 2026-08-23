//! Reproduction: rebuild the attributed revision, replay the captured request.
//! ALL reproduction I/O lives behind `Reproducer` — this trait is the seam the
//! spec's Stage 8 (in-customer-boundary reproduction, containers) drops into.
//! Nothing here may assume Drums-hosted infrastructure.
//!
//! [`ManagedWorktree`] and [`BootedApp`] are the shared, public primitives:
//! creating a detached git worktree at a sha, and booting+replaying against
//! whatever is checked out there. `LocalProcessReproducer::reproduce` uses
//! them internally (create, boot, replay, drop — worktree always removed).
//! The repair-verification pipeline (Task 3, `engine/crates/cli/src/engine.rs`)
//! reuses the same two primitives directly against an EXISTING worktree it
//! keeps alive across a commit + verify sequence, and can ask a worktree to
//! survive past `Drop` (`keep_on_drop`) so a failed repair is left on disk
//! for a human to inspect (spec §13 "design the miss") instead of the
//! trait's always-remove behavior.
//!
//! **Real apps (pilot blocker fix).** The signature that decides whether a
//! reproduction MATCHES the original failure used to come only from the
//! booted app's HTTP response BODY (`signature_from_body`) — the demo app
//! happens to leak a structured `{"error":{name,message,stack}}` JSON body
//! on failure, but a real app (FastAPI/uvicorn returning a bare `"Internal
//! Server Error"`, an Express app with a generic error middleware, ...)
//! does not, so the body path silently starves and every real failure comes
//! back `unresolved`. `signature_from_process` tries the booted app's own
//! STDERR first instead — uvicorn logs the Python traceback there, Node's
//! default handler logs an uncaught exception there — and only falls back
//! to the body when stderr yields nothing usable. See
//! [`BootedApp::stderr_settled`] for why this is a short bounded POLL, not
//! an instant read: a framework's own error-logging commonly happens AFTER
//! the response has already gone out to the client.
//!
//! **`--boot-cmd` (real apps don't run on `node <entry>`).** [`BootedApp`]
//! used to hardcode `Command::new("node")` against an auto-discovered
//! entrypoint. [`BootedApp::boot_with_cmd`] takes an optional command
//! template instead (`None` keeps that exact original behavior, so nothing
//! regresses) — argv-split on whitespace with NO shell, substituting the
//! literal token `{port}` per element, mirroring the discipline
//! `engine-repair`/`ship::build_argv` already use for `{prompt}`/`{sha}`/
//! `{repo}`. A boot-cmd boot has no "listening" line to wait for (the port
//! is decided and substituted in BEFORE the child spawns, not announced by
//! it), so it polls [`DEFAULT_BOOT_READINESS_PATH`] instead.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use engine_core::{Attribution, CapturedRequest, Claim, ErrorSignature, Failure, Provenance, Reproduction};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{Child, Command};


/// What to say when a boot times out. A silent stderr is the COMMON case for
/// an app that started fine but never printed the readiness line this
/// contract waits for — and reporting that as an empty string after a colon
/// ("failed to boot within 15000ms: ") tells an operator nothing and reads
/// like a bug in Drums. Name the actual contract instead, so the fix is
/// obvious from the message alone.
fn boot_detail(stderr: &str) -> String {
    let trimmed = stderr.trim();
    if trimmed.is_empty() {
        "the app printed nothing on stderr and never printed a `listening <port>` line on stdout. \
         Drums waits for that exact line to learn the port. If your app announces itself \
         differently, pass `--boot-cmd` and Drums will assign the port and poll /health instead."
            .to_string()
    } else {
        trimmed.to_string()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ReproError {
    #[error("git worktree failed: {0}")]
    Worktree(String),
    #[error("app failed to boot within {0}ms: {1}")]
    BootTimeout(u64, String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("replay request failed: {0}")]
    Replay(String),
    /// The failure carries no replayable request, so there is nothing to replay
    /// against the rebuilt revision (spec §9). Defense in depth: the engine
    /// SKIPS reproduction for a trigger/reported intake rather than calling in
    /// here at all (see `attribute_and_reproduce` in
    /// `engine/crates/cli/src/engine.rs`), but any future caller that forgets
    /// gets a typed refusal instead of a synthesized request — the one thing
    /// that must never happen, since a synthesized replay could earn
    /// `verified` for a request nobody ever made.
    /// `intake_source` rather than `source`: thiserror reserves a field named
    /// `source` for the error-cause chain.
    #[error("no replayable request captured for this {intake_source} failure — reproduction not attempted")]
    NotReplayable { intake_source: String },
    #[error("invalid boot command: {0}")]
    BootCmd(String),
}

#[async_trait::async_trait]
pub trait Reproducer: Send + Sync {
    async fn reproduce(&self, repo: &Path, failure: &Failure, attribution: &Attribution) -> Result<Reproduction, ReproError>;
}

pub struct LocalProcessReproducer {
    pub boot_timeout_ms: u64,
    /// Command template used to boot the app instead of the original,
    /// hardcoded `node <auto-discovered-entry>`. `None` (the default)
    /// preserves that exact behavior — nothing regresses for the demo app
    /// or any existing Node deployment. `Some(template)` is argv-split on
    /// whitespace (NO shell) with the literal token `{port}` substituted
    /// per element; the port is one Drums assigns itself, not one the app
    /// announces. Example: `uvicorn app.main:app --host 127.0.0.1 --port
    /// {port}`. See [`BootedApp::boot_with_cmd`].
    pub boot_cmd: Option<String>,
}

/// Result of booting one revision and replaying the captured request against it.
struct BootReplay {
    status: u16,
    body: String,
    /// Canonicalized worktree root — strips down absolute stack-trace paths
    /// (node resolves through realpath) to project-relative ones.
    app_root: String,
    /// The booted app's own stderr, captured only when `status` indicated a
    /// server error (see [`LocalProcessReproducer::boot_and_replay`]) —
    /// empty otherwise. This is what lets [`signature_from_process`] read a
    /// real traceback instead of the (often-absent, on a real app) response
    /// body.
    stderr: String,
}

/// A git worktree checked out (detached) at a given sha, under a fresh,
/// caller-uncontrolled temp directory. Removes itself (`git worktree
/// remove --force` + `rm -rf`) on drop, UNLESS `keep_on_drop` is set —
/// the repair pipeline sets it when a repair/verify step fails, so the
/// directory (and any commit/branch made inside it) is left on disk for a
/// human to inspect rather than being erased along with the evidence of
/// what happened (spec §13 "design the miss").
pub struct ManagedWorktree {
    repo: PathBuf,
    pub dir: PathBuf,
    pub keep_on_drop: bool,
}

impl ManagedWorktree {
    /// Create a detached worktree at `sha` (which may carry a trailing `^`
    /// for the parent form) under `repo`. `sha` is validated the same way
    /// as reproduction's own worktree creation always has been: ASCII hex
    /// only, so it can never be mistaken for a flag or a path-traversal
    /// component.
    pub fn create(repo: &Path, sha: &str) -> Result<ManagedWorktree, ReproError> {
        let sha_for_git = sha.trim_end_matches('^');
        validate_sha(sha_for_git)?;

        // Unique per invocation, with no caller-controlled input (failure
        // id, sha) in the path — see the historical note on why a
        // deterministic name is unsafe, preserved from the pre-refactor
        // version of this crate: a fresh ULID per call has no collision
        // surface across crashed or concurrent runs.
        let dir = std::env::temp_dir().join(format!("drums-repro-{}", ulid::Ulid::new()));
        let st = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["worktree", "add", "--detach"])
            .arg(&dir)
            .arg(sha)
            .output()?;
        if !st.status.success() {
            return Err(ReproError::Worktree(String::from_utf8_lossy(&st.stderr).into()));
        }
        Ok(ManagedWorktree { repo: repo.to_path_buf(), dir, keep_on_drop: false })
    }

    /// Canonicalized path of this worktree — strips down absolute
    /// stack-trace paths the app reports (node resolves through realpath)
    /// to project-relative ones.
    pub fn app_root(&self) -> String {
        std::fs::canonicalize(&self.dir).unwrap_or_else(|_| self.dir.clone()).to_string_lossy().into_owned()
    }
}

impl Drop for ManagedWorktree {
    fn drop(&mut self) {
        if self.keep_on_drop {
            return;
        }
        let _ = std::process::Command::new("git")
            .arg("-C")
            .arg(&self.repo)
            .args(["worktree", "remove", "--force"])
            .arg(&self.dir)
            .status();
        // Belt-and-suspenders: on Unix, deleting a directory doesn't require
        // the process using it as cwd to have exited first, so this is safe
        // even if a booted child hasn't been reaped yet. Guards against
        // `worktree remove` refusing (e.g. it raced a not-yet-dead process)
        // leaving a stale directory the next run's `add` would collide with.
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Makes a child the leader of a brand-new process group (pgid == its own
/// pid), so [`kill_process_group`] can reach subprocesses it spawns — the
/// same guarantee `engine-repair`'s `ChildGuard` gives an agent process
/// tree, extended here to the app process a reproduction or a repair
/// verification boots (a booted app that itself spawns a worker/child
/// process must not be able to survive `BootedApp` being dropped).
#[cfg(unix)]
fn set_new_process_group(cmd: &mut Command) {
    cmd.process_group(0);
}

#[cfg(not(unix))]
fn set_new_process_group(_cmd: &mut Command) {}

#[cfg(unix)]
fn kill_process_group(pid: u32) {
    // Safety: `kill` with a negative pid signals the process group rather
    // than a single process; no memory is touched, only a syscall is made.
    unsafe {
        libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn kill_process_group(_pid: u32) {}

/// Caps what a drained stderr RETAINS (oldest bytes dropped first) —
/// mirrors `drums-watch::proc`'s `MAX_DRAIN_BYTES`/`drain_into` discipline,
/// duplicated here per that module's own note that cross-crate copies of
/// this shape are unavoidable: output is kept only to explain a boot
/// failure or derive a signature, and a looping/chatty app must not be able
/// to turn that into unbounded memory in this process.
const MAX_STDERR_BYTES: usize = 256 * 1024;

/// The path a boot-cmd boot polls for readiness (no "listening" line exists
/// to wait for — the port is decided and substituted in before the child
/// spawns, not announced by it). Also the convention `verify_repair`'s own
/// `/health` check already uses, so this is the same contract, not a new
/// one.
pub const DEFAULT_BOOT_READINESS_PATH: &str = "/health";

/// How often a boot-cmd boot polls [`DEFAULT_BOOT_READINESS_PATH`] while
/// waiting for the app to come up.
const READINESS_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Bounds [`BootedApp::stderr_settled`]'s wait for a just-logged traceback
/// to actually land in the drained buffer. A framework's error-logging
/// commonly happens AFTER the response has already gone out (e.g.
/// Starlette's `ServerErrorMiddleware` sends the response, then re-raises
/// for the ASGI server to log) — this is a small, bounded allowance for
/// that race, not a hope that the process is done talking.
const STDERR_SETTLE_MAX_WAIT: Duration = Duration::from_millis(1_500);
const STDERR_SETTLE_POLL: Duration = Duration::from_millis(50);

/// A drained stderr buffer, capped at [`MAX_STDERR_BYTES`] with the OLDEST
/// bytes evicted first — but `dropped` keeps a running count of everything
/// ever evicted, so a byte offset taken before an eviction ([`Self::mark`])
/// stays meaningful afterward ([`Self::since`]) instead of being silently
/// reinterpreted against a buffer whose start moved out from under it.
/// Before this, a mark was just `bytes.len()`: once the cap was hit,
/// `len()` pinned at [`MAX_STDERR_BYTES`] forever, so a mark taken at the
/// cap was indistinguishable from "end of everything that will ever
/// exist" and every later `since` computed an empty slice — starving
/// reproduction of evidence that was, in fact, still retained (round-2
/// review Major).
struct StderrBuf {
    bytes: Vec<u8>,
    /// Total bytes ever evicted from the front by the [`MAX_STDERR_BYTES`]
    /// cap. `dropped + bytes.len()` is therefore the total byte count ever
    /// written, which is what makes offsets comparable across an eviction.
    dropped: usize,
}

impl StderrBuf {
    fn push(&mut self, chunk: &[u8]) {
        self.bytes.extend_from_slice(chunk);
        if self.bytes.len() > MAX_STDERR_BYTES {
            let excess = self.bytes.len() - MAX_STDERR_BYTES;
            self.bytes.drain(..excess);
            self.dropped += excess;
        }
    }

    /// Lossy UTF-8 snapshot of everything currently retained.
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }

    /// A cumulative byte offset — stable across eviction — meant to be
    /// handed back to [`Self::since`].
    fn mark(&self) -> usize {
        self.dropped + self.bytes.len()
    }

    /// Everything retained strictly after cumulative offset `since`. If
    /// `since` falls inside (or before) the range this buffer has already
    /// evicted, the returned text starts at whatever oldest byte is still
    /// retained — never empty merely because eviction moved the start, only
    /// when `since` truly is at-or-past everything written so far.
    fn since(&self, since: usize) -> String {
        let start = since.saturating_sub(self.dropped).min(self.bytes.len());
        String::from_utf8_lossy(&self.bytes[start..]).into_owned()
    }
}

/// Drain a child's stderr continuously into a shared, bounded buffer,
/// started the instant the child is spawned (so a chatty/crashing process
/// can never block on a full pipe) and kept alive for the whole life of the
/// [`BootedApp`] (not just until boot succeeds) — a traceback printed AFTER
/// the "listening" line, or after a later replay, must still be captured.
fn spawn_stderr_drain(mut stderr: tokio::process::ChildStderr) -> Arc<Mutex<StderrBuf>> {
    let buf = Arc::new(Mutex::new(StderrBuf { bytes: Vec::new(), dropped: 0 }));
    let buf_task = buf.clone();
    tokio::spawn(async move {
        let mut chunk = [0u8; 4096];
        loop {
            match stderr.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    let mut g = buf_task.lock().unwrap_or_else(|p| p.into_inner());
                    g.push(&chunk[..n]);
                }
            }
        }
    });
    buf
}

/// Lossy UTF-8 snapshot of a stderr drain's current contents.
fn stderr_text(buf: &Arc<Mutex<StderrBuf>>) -> String {
    buf.lock().unwrap_or_else(|p| p.into_inner()).text()
}

/// Drain a child's stdout without inspecting it — used by a boot-cmd boot,
/// which has no "listening" line to look for but still must not let stdout
/// fill and block the child.
fn spawn_stdout_discard(stdout: tokio::process::ChildStdout) {
    let mut lines = BufReader::new(stdout).lines();
    tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
}

/// A booted app process. Group-killed on drop (reaches any subprocess the
/// app itself spawned, not only the direct child) — reproduction always
/// used a bare `start_kill()`, which only reaches the direct child; this
/// closes that gap for both reproduction and repair verification, which
/// now share this type.
pub struct BootedApp {
    child: Child,
    pgid: Option<u32>,
    pub port: u16,
    pub app_root: String,
    /// Continuously-updated, bounded stderr the app has printed so far —
    /// read via [`BootedApp::stderr`]/[`BootedApp::stderr_settled`].
    stderr_buf: Arc<Mutex<StderrBuf>>,
}

impl Drop for BootedApp {
    fn drop(&mut self) {
        if let Some(pgid) = self.pgid {
            kill_process_group(pgid);
        }
        let _ = self.child.start_kill();
    }
}

impl BootedApp {
    /// Boot the app already checked out at `dir` (a [`ManagedWorktree`]'s
    /// `dir`, or any other directory), waiting up to `boot_timeout_ms` for
    /// it to print `listening <port>` on stdout. Equivalent to
    /// `boot_with_cmd(dir, boot_timeout_ms, None)` — kept as its own name
    /// since most call sites never need a boot-cmd override.
    pub async fn boot(dir: &Path, boot_timeout_ms: u64) -> Result<BootedApp, ReproError> {
        Self::boot_with_cmd(dir, boot_timeout_ms, None).await
    }

    /// Boot the app already checked out at `dir` per `boot_cmd`: `None`
    /// keeps the original, hardcoded contract (`node <auto-discovered
    /// entry>`, `PORT=0`, wait for `listening <port>` on stdout — see
    /// [`Self::boot_announce`]). `Some(template)` boots via that command
    /// instead — a real app that doesn't speak the node/PORT=0/"listening"
    /// contract (FastAPI+uvicorn, ...) — argv-split with NO shell,
    /// substituting the literal token `{port}` per element (see
    /// [`Self::boot_assigned`]).
    pub async fn boot_with_cmd(dir: &Path, boot_timeout_ms: u64, boot_cmd: Option<&str>) -> Result<BootedApp, ReproError> {
        match boot_cmd {
            None => Self::boot_announce(dir, boot_timeout_ms).await,
            Some(template) => Self::boot_assigned(dir, boot_timeout_ms, template).await,
        }
    }

    /// The original boot contract: `node <auto-discovered entry>`,
    /// `PORT=0`, wait up to `boot_timeout_ms` for `listening <port>` on
    /// stdout.
    async fn boot_announce(dir: &Path, boot_timeout_ms: u64) -> Result<BootedApp, ReproError> {
        let entry = discover_entry(dir);
        let mut cmd = Command::new("node");
        cmd.arg(&entry)
            .current_dir(dir)
            .env("PORT", "0")
            .env_remove("DRUMS_INGEST_URL") // never feed telemetry back to itself
            // Closing round (M-l): stdin is `/dev/null`, not inherited, for
            // the same reason `drums-watch`'s `run_deploy_cmd` does it
            // (round-3 R1) — this child is in its own background process
            // group, so a read of an inherited terminal stdin earns
            // `SIGTTIN` and STOPS it, and the boot then "times out" having
            // never begun. The app under repair is untrusted code from the
            // operator's repo; it must never be able to consume the
            // operator's keystrokes either.
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        set_new_process_group(&mut cmd);
        let mut child = cmd.spawn()?;
        let pgid = child.id();
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");

        // Drain stderr concurrently (into a buffer that survives past boot
        // — see `spawn_stderr_drain`'s doc) so a crashing/chatty process
        // can't block on a full pipe while we wait on stdout for the
        // "listening" line; its contents are folded into the boot-timeout
        // error for diagnosis, and stay readable afterward for signature
        // derivation.
        let stderr_buf = spawn_stderr_drain(stderr);

        let mut lines = BufReader::new(stdout).lines();
        let port = tokio::time::timeout(Duration::from_millis(boot_timeout_ms), async {
            while let Ok(Some(line)) = lines.next_line().await {
                if let Some(p) = line.trim().strip_prefix("listening ") {
                    if let Ok(port) = p.trim().parse::<u16>() {
                        return Some(port);
                    }
                }
            }
            None
        })
        .await
        .ok()
        .flatten();

        let Some(port) = port else {
            let stderr_out = stderr_text(&stderr_buf);
            let _ = child.start_kill();
            if let Some(pgid) = pgid {
                kill_process_group(pgid);
            }
            return Err(ReproError::BootTimeout(boot_timeout_ms, boot_detail(&stderr_out)));
        };

        // Drums must keep reading the app's stdout for the whole life of
        // the `BootedApp`, not just until the "listening" line is seen.
        // `lines` (a `BufReader<ChildStdout>`) was a local of the timeout
        // future above; dropping it here would close the READ end of the
        // app's stdout pipe. Any app with default per-request stdout
        // logging then gets `EPIPE` on its next write and dies — and the
        // induced crash's own stack (naming the app's own file) then became
        // the reproduction signature (C2). `boot_assigned` already avoids
        // this via `spawn_stdout_discard`; do the equivalent here by handing
        // the same reader — already positioned past the "listening" line —
        // to a background task instead of letting it drop.
        tokio::spawn(async move {
            while let Ok(Some(_)) = lines.next_line().await {}
        });

        let app_root = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf()).to_string_lossy().into_owned();
        Ok(BootedApp { child, pgid, port, app_root, stderr_buf })
    }

    /// Boot via an operator-supplied `template` (`drums watch --boot-cmd`):
    /// Drums assigns a free ephemeral port itself (there is no "listening"
    /// line to wait for — the port is decided and substituted in BEFORE the
    /// child spawns, not announced by it), argv-splits `template` on
    /// whitespace with NO shell substituting the literal token `{port}` per
    /// element (see [`build_boot_argv`] — same discipline as
    /// `engine-repair`/`ship::build_argv`), spawns it, then polls
    /// [`DEFAULT_BOOT_READINESS_PATH`] until it answers 2xx or
    /// `boot_timeout_ms` elapses.
    async fn boot_assigned(dir: &Path, boot_timeout_ms: u64, template: &str) -> Result<BootedApp, ReproError> {
        // Bind an ephemeral port, note it, drop the listener before the
        // child binds it. A small TOCTOU window exists between the drop and
        // the child's own bind — unavoidable without a socket-passing
        // protocol the target frameworks don't support — but narrow enough
        // in practice for a local reproduction boot.
        let port = {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            listener.local_addr()?.port()
        };

        let argv = build_boot_argv(template, port);
        let Some((prog, args)) = argv.split_first() else {
            return Err(ReproError::BootCmd("--boot-cmd template is empty".to_string()));
        };

        let mut cmd = Command::new(prog);
        cmd.args(args)
            .current_dir(dir)
            .env_remove("DRUMS_INGEST_URL") // never feed telemetry back to itself
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        set_new_process_group(&mut cmd);
        let mut child = cmd.spawn()?;
        let pgid = child.id();
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");

        // Neither stream carries information this strategy needs (there's
        // no "listening" line to parse), but both must still be drained so
        // the child can't block writing to a full pipe while readiness is
        // polled; stderr stays readable afterward (boot-timeout diagnosis,
        // and — the whole point of this branch existing — signature
        // derivation from a real traceback).
        spawn_stdout_discard(stdout);
        let stderr_buf = spawn_stderr_drain(stderr);

        let url = format!("http://127.0.0.1:{port}{DEFAULT_BOOT_READINESS_PATH}");
        let client = reqwest::Client::new();
        let ready = tokio::time::timeout(Duration::from_millis(boot_timeout_ms), async {
            loop {
                if let Ok(resp) = client.get(&url).send().await {
                    if resp.status().is_success() {
                        return true;
                    }
                }
                tokio::time::sleep(READINESS_POLL_INTERVAL).await;
            }
        })
        .await
        .unwrap_or(false);

        if !ready {
            let stderr_out = stderr_text(&stderr_buf);
            let _ = child.start_kill();
            if let Some(pgid) = pgid {
                kill_process_group(pgid);
            }
            return Err(ReproError::BootTimeout(boot_timeout_ms, boot_detail(&stderr_out)));
        }

        let app_root = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf()).to_string_lossy().into_owned();
        Ok(BootedApp { child, pgid, port, app_root, stderr_buf })
    }

    /// Snapshot of the app's stderr accumulated so far.
    pub fn stderr(&self) -> String {
        stderr_text(&self.stderr_buf)
    }

    /// A CUMULATIVE byte offset into everything the app has ever written to
    /// stderr, meant to be handed back to [`Self::stderr_since`] or
    /// [`Self::stderr_settled`] — lets a caller correlate stderr with a
    /// SPECIFIC replay instead of treating boot-time/startup output and the
    /// replayed request's output as equally eligible evidence, which is
    /// otherwise indistinguishable (C2b). Call this immediately before
    /// [`Self::replay`].
    ///
    /// Stable across [`MAX_STDERR_BYTES`] eviction: this is
    /// [`StderrBuf::mark`], not the retained buffer's length, so a mark
    /// taken once the cap has been hit is still comparable to a later
    /// [`Self::stderr_since`] rather than being pinned at the cap forever
    /// (round-2 review Major).
    pub fn stderr_mark(&self) -> usize {
        self.stderr_buf.lock().unwrap_or_else(|p| p.into_inner()).mark()
    }

    /// The stderr text written strictly after `since` (a mark from
    /// [`Self::stderr_mark`]). If the buffer has since been evicted below
    /// `since` by [`MAX_STDERR_BYTES`] truncation (an extremely chatty
    /// app), falls back to everything currently retained — all of it
    /// postdates the mark in that case, since the older bytes were
    /// dropped. This holds even once the buffer is AT the cap: `since` is
    /// a cumulative offset ([`StderrBuf::mark`]), not a length, so evidence
    /// written after the mark but still inside the retained window is never
    /// mistaken for having been evicted (round-2 review Major).
    pub fn stderr_since(&self, since: usize) -> String {
        self.stderr_buf.lock().unwrap_or_else(|p| p.into_inner()).since(since)
    }

    /// Wait for a USABLE stderr signature to appear in the text written
    /// since `since` (a mark from [`Self::stderr_mark`]) — not merely for
    /// the buffer to go quiet. A framework's error-logging (uvicorn logging
    /// an ASGI exception; Node's default uncaught-exception handler)
    /// commonly happens AFTER the response has already been sent back to
    /// the client (e.g. Starlette's `ServerErrorMiddleware` sends the
    /// response, then re-raises the exception for the ASGI server to log
    /// it) — and the buffer is normally quiet in the instant right after a
    /// response goes out, which is exactly the state a "stop when quiet"
    /// loop returns on immediately, before the framework has logged
    /// anything (round-2 review C3). Polls every `poll` for a candidate
    /// [`signature_from_stderr`] would accept (a non-empty `top_frame_file`)
    /// against `app_root`, or until `max_wait` elapses; on finding one,
    /// waits one more `poll` so an in-progress multi-line write can finish
    /// landing before the final snapshot. Always returns whatever has
    /// accumulated since `since` (never errors — a caller that gets nothing
    /// usable falls back to the response body).
    pub async fn stderr_settled(&self, since: usize, app_root: &str, max_wait: Duration, poll: Duration) -> String {
        let deadline = tokio::time::Instant::now() + max_wait;
        loop {
            let tail = self.stderr_since(since);
            if signature_from_stderr(&tail, app_root).is_some() {
                tokio::time::sleep(poll).await;
                return self.stderr_since(since);
            }
            if tokio::time::Instant::now() >= deadline {
                return tail;
            }
            tokio::time::sleep(poll).await;
        }
    }

    /// Replay one captured request against this booted app. Callers may
    /// call this more than once against the same running instance (e.g.
    /// the repair pipeline replays the original failing request, then
    /// checks `/health`, against one boot).
    pub async fn replay(&self, req: &CapturedRequest) -> Result<(u16, String), ReproError> {
        let client = reqwest::Client::new();
        let url = format!("http://127.0.0.1:{}{}", self.port, req.path);
        let method = parse_method(&req.method)?;
        let mut builder = client.request(method, &url);
        if let Some(ct) = &req.content_type {
            builder = builder.header("content-type", ct.clone());
        }
        if let Some(body) = &req.body {
            builder = builder.body(body.clone());
        }
        let resp = builder.timeout(Duration::from_secs(10)).send().await.map_err(|e| ReproError::Replay(e.to_string()))?;
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        Ok((status, body))
    }
}

impl LocalProcessReproducer {
    /// Boot `sha` in a fresh worktree, replay the captured request. `sha`
    /// may carry a trailing `^` (parent revision). Built entirely on
    /// [`ManagedWorktree`] and [`BootedApp`] — the worktree always drops
    /// with `keep_on_drop: false` (removed), matching the trait's
    /// long-standing behavior; only the repair pipeline ever keeps one.
    async fn boot_and_replay(&self, repo: &Path, sha: &str, failure: &Failure) -> Result<BootReplay, ReproError> {
        // `replayable_request()`, not `sample.request` — a request being present
        // is not on its own permission to replay it (an OTel adapter can
        // reconstruct a method and path from span attributes, and that is not
        // the request that failed).
        let request = failure
            .replayable_request()
            .ok_or_else(|| ReproError::NotReplayable { intake_source: failure.intake.source().to_string() })?
            .clone();
        let worktree = ManagedWorktree::create(repo, sha)?;
        let app = BootedApp::boot_with_cmd(&worktree.dir, self.boot_timeout_ms, self.boot_cmd.as_deref()).await?;
        // Marked immediately before the replay so the signature is derived
        // only from stderr written from here on — boot-time/startup output
        // must never be eligible evidence for a specific replayed request
        // (C2b).
        let mark = app.stderr_mark();
        let (status, body) = app.replay(&request).await?;
        let app_root = app.app_root.clone();
        // Only a server error needs a signature at all (a 2xx/redirect
        // response is never checked against `failure.signature`), and only
        // then is the stderr-settle wait worth its bounded latency.
        let stderr = if status >= 500 { app.stderr_settled(mark, &app_root, STDERR_SETTLE_MAX_WAIT, STDERR_SETTLE_POLL).await } else { String::new() };
        Ok(BootReplay { status, body, app_root, stderr })
        // `app` and `worktree` drop here, in reverse declaration order:
        // the process group is killed first, then the worktree is removed —
        // before we return to the caller.
    }
}

/// Split a `--boot-cmd` `template` on whitespace into argv, substituting
/// the literal token `{port}` PER ELEMENT — never into the joined template
/// string first — so a substituted value can never be reinterpreted as
/// extra argv elements or shell syntax. Mirrors `engine_repair::build_argv`
/// / `ship::build_argv`'s discipline for `{prompt}`/`{sha}`/`{repo}`. A port
/// number is always plain digits, so this is belt-and-suspenders rather
/// than a plausible attack surface — but it is the SAME discipline every
/// other templated command in this workspace uses, and drifting from it
/// here would be exactly the kind of copy that quietly regresses later.
fn build_boot_argv(template: &str, port: u16) -> Vec<String> {
    let port = port.to_string();
    template.split_whitespace().map(|tok| tok.replace("{port}", &port)).collect()
}

/// Validate a git revision before it touches a filesystem path or a git
/// argv: ASCII hex only, 4..=64 chars. Rejects flags (`--force`), empty
/// strings, and path-traversal payloads (`../..`) by construction — none of
/// those are valid hex. Callers pass the *base* sha (before any trailing `^`
/// they append themselves for the parent form).
fn validate_sha(sha: &str) -> Result<(), ReproError> {
    let len_ok = (4..=64).contains(&sha.len());
    let hex_ok = !sha.is_empty() && sha.bytes().all(|b| b.is_ascii_hexdigit());
    if !len_ok || !hex_ok {
        return Err(ReproError::Worktree(format!("invalid sha: {sha:?}")));
    }
    Ok(())
}

/// First 6 characters of a sha for narration, char-boundary-safe.
///
/// m2 (trust-hardening review): this used to be a byte-range slice
/// (`sha[..sha.len().min(6)]`), which panics if byte index 6 lands inside a
/// multi-byte UTF-8 character — the exact panic class a prior closing commit
/// set out to kill everywhere (`engine/crates/cli/src/ship.rs`'s
/// `render::short`/`short_sha`), but this one call site survived that pass.
/// Unreachable today only because `attribute()` runs `git diff-tree <sha>`
/// before `reproduce()` is ever entered, and a non-ASCII sha cannot resolve
/// there — this is defense-in-depth, and the shape `engine-attribute`'s own
/// `attribute/src/lib.rs` already uses (`sha.get(..6).unwrap_or(&sha)`).
fn short_sha(sha: &str) -> String {
    sha.get(..6).unwrap_or(sha).to_string()
}

/// Parse the captured request's HTTP method for replay. Unlike a `_ => GET`
/// fallback, an unrecognized or malformed method is a hard error rather than
/// a silent substitution — the "replayed the captured request" claim must
/// never describe a request that used a different method than the one that
/// was actually captured.
fn parse_method(method: &str) -> Result<reqwest::Method, ReproError> {
    reqwest::Method::from_bytes(method.to_ascii_uppercase().as_bytes())
        .map_err(|_| ReproError::Replay(format!("unsupported method {method}")))
}

fn discover_entry(dir: &Path) -> String {
    if let Ok(pkg) = std::fs::read_to_string(dir.join("package.json")) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&pkg) {
            if let Some(start) = v.pointer("/scripts/start").and_then(|s| s.as_str()) {
                // "node server.js" → entry after "node "
                if let Some(entry) = start.strip_prefix("node ") {
                    return entry.trim().to_string();
                }
            }
        }
    }
    "server.js".to_string()
}

/// Extract the signature from the app's 500-body `{"error":{name,message,stack}}`.
/// Only the demo app's error middleware happens to leak this — a real app
/// (FastAPI's default handler, an Express app with a generic error
/// middleware, ...) returns an opaque body, which is exactly why
/// [`signature_from_process`] tries the process's own stderr FIRST and
/// falls back to this only when that yields nothing.
fn signature_from_body(body: &str, app_root: &str) -> Option<ErrorSignature> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let e = v.get("error")?;
    Some(ErrorSignature::from_error(
        e.get("name")?.as_str()?,
        e.get("message").and_then(|m| m.as_str()).unwrap_or(""),
        e.get("stack").and_then(|s| s.as_str()).unwrap_or(""),
        app_root,
    ))
}

/// Derive the reproduced error's signature, stderr FIRST: a real app's HTTP
/// 500 response body carries none of the structured evidence
/// `signature_from_body` needs (spec pilot blocker), but its process stderr
/// almost always does — uvicorn logs the Python traceback there, Node's
/// default uncaught-exception handler logs there. Falls back to the body
/// only when stderr yields nothing usable (an app that genuinely never
/// logged anything, or crashed before it could).
fn signature_from_process(stderr: &str, body: &str, app_root: &str) -> Option<ErrorSignature> {
    signature_from_stderr(stderr, app_root).or_else(|| signature_from_body(body, app_root))
}

/// Parse the most recent exception logged to a booted app's stderr. Tries
/// the CPython traceback shape first (uvicorn/FastAPI's own uncaught-
/// exception logging: `Traceback (most recent call last):` … ending in an
/// unindented `ExceptionName: message` summary line), then Node's default
/// uncaught-exception dump (an unindented `ErrorName: message` line
/// immediately followed by `    at ...` V8 frames). Returns `None` when
/// neither shape is present — a chatty app whose stderr happens to contain
/// neither marker must not manufacture a signature out of unrelated log
/// noise.
fn signature_from_stderr(stderr: &str, app_root: &str) -> Option<ErrorSignature> {
    // An empty `top_frame_file` means the parser matched a shape but found
    // no application frame beneath it (e.g. a stack that is all `node:`
    // internals, or all Python library frames) -- that is "no evidence",
    // not "evidence of nothing". Returning it as `Some` here would let it
    // permanently shadow a real signature the response body could still
    // supply, since `signature_from_process`'s `.or_else` only falls back
    // on `None` (C4). Discard it and keep trying the remaining shapes.
    if let Some((name, message, stack)) = extract_python_exception(stderr) {
        let sig = ErrorSignature::from_error(&name, &message, &stack, app_root);
        if !sig.top_frame_file.is_empty() {
            return Some(sig);
        }
    }
    if let Some((name, message, stack)) = extract_node_exception(stderr) {
        let sig = ErrorSignature::from_error(&name, &message, &stack, app_root);
        if !sig.top_frame_file.is_empty() {
            return Some(sig);
        }
    }
    None
}

/// `true` for text that plausibly names a Python exception class: a
/// (possibly dotted) identifier and nothing else. Guards
/// [`extract_python_exception`] against treating an unindented CONTINUATION
/// line of a multi-line exception message (which CPython does not indent)
/// as the summary line itself.
fn looks_like_python_exception_name(candidate: &str) -> bool {
    !candidate.is_empty()
        && candidate.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && candidate.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
}

/// Splits `s` into `(start, end, line)` triples on `\n`, `start`/`end` being
/// byte offsets into `s` (the delimiter itself excluded from both the line
/// and the range). [`str::lines`] alone doesn't expose these — needed here
/// because [`extract_python_exception`] must truncate `stack` at an exact
/// byte offset (the end of the summary line), not merely iterate lines.
fn lines_with_offsets(s: &str) -> Vec<(usize, usize, &str)> {
    let mut out = Vec::new();
    let mut pos = 0;
    loop {
        let end = s[pos..].find('\n').map(|i| pos + i).unwrap_or(s.len());
        out.push((pos, end, &s[pos..end]));
        if end == s.len() {
            break;
        }
        pos = end + 1;
    }
    out
}

/// Find the LAST CPython traceback in `stderr` (`rfind` on the banner line
/// means a chatty app that logged more than one exception yields the most
/// recent) and pull `(error_name, message, stack)` out of it.
///
/// CPython's own grammar is: banner → one-or-more INDENTED frame headers
/// (`File "...", line N[, in func]`, each optionally followed by an
/// indented source-context line) → exactly one UNINDENTED summary line
/// (`ExceptionName: message`), optionally followed by unindented
/// CONTINUATION lines of a multi-line message. The summary is therefore the
/// FIRST unindented, name-shaped line found after AT LEAST ONE frame header
/// has been seen scanning FORWARD from the banner — never the last such
/// line in the whole tail-of-buffer slice (round-1 review C1), and not the
/// first such line after the LAST frame header either (round-2 review C1
/// residual): anchoring on the last header is itself movable by anything
/// that emits a frame-header-shaped line after the real summary with no
/// summary of its own — `traceback.print_stack()`, a `faulthandler` dump,
/// or two threads' tracebacks interleaving on the same fd — which would
/// then hand `error_name` to whatever `Word: text` line comes next while
/// `top_frame_file` still comes from the genuine traceback. Scanning
/// forward and stopping at the first summary after the first header has no
/// such door: CPython never emits a second banner-less run of frame headers
/// followed by nothing, so the first summary found IS the one that ends the
/// traceback `rfind` anchored on. `stack` is truncated to end exactly at
/// the summary line, so trailing log lines never reach
/// `ErrorSignature::from_error` either.
fn extract_python_exception(stderr: &str) -> Option<(String, String, String)> {
    let start = stderr.rfind("Traceback (most recent call last):")?;
    let tail = &stderr[start..];
    let lines = lines_with_offsets(tail);

    let mut seen_header = false;
    for (_, end, line) in &lines {
        if line.trim_start().starts_with("File \"") {
            seen_header = true;
            continue;
        }
        if !seen_header || line.trim_start() != *line || line.is_empty() {
            continue; // no header seen yet, or an indented (frame/context) or blank line
        }
        let (name, message) = line.split_once(": ").unwrap_or((line, ""));
        if looks_like_python_exception_name(name) {
            return Some((name.to_string(), message.to_string(), tail[..*end].to_string()));
        }
    }
    None
}

/// Find the LAST Node uncaught-exception dump in `stderr` — an unindented
/// header line (`ErrorName: message`) immediately followed by a `    at
/// ...` V8 frame line. Scanning from the end means a chatty process that
/// logged more than one exception yields the most recent. Returns
/// `(error_name, message, stack)` with `stack` reconstructed as `header`
/// followed by its consecutive `at ...` frames — the exact shape
/// `ErrorSignature::from_error`'s V8 parser expects (it skips line 0,
/// expecting it to be this header, then reads `at ` lines beneath it).
fn extract_node_exception(stderr: &str) -> Option<(String, String, String)> {
    let lines: Vec<&str> = stderr.lines().collect();
    for i in (0..lines.len()).rev() {
        let header = lines[i].trim_end();
        if header.is_empty() || header.trim_start().starts_with("at ") {
            continue;
        }
        let Some(next) = lines.get(i + 1) else { continue };
        if !next.trim_start().starts_with("at ") {
            continue;
        }
        let Some((name, message)) = header.split_once(": ") else { continue };
        let mut stack_lines = vec![header.to_string()];
        for l in &lines[i + 1..] {
            if l.trim_start().starts_with("at ") {
                stack_lines.push(l.to_string());
            } else {
                break;
            }
        }
        return Some((name.trim().to_string(), message.trim().to_string(), stack_lines.join("\n")));
    }
    None
}

#[async_trait::async_trait]
impl Reproducer for LocalProcessReproducer {
    async fn reproduce(&self, repo: &Path, failure: &Failure, attribution: &Attribution) -> Result<Reproduction, ReproError> {
        let sha = attribution.deploy.sha.clone();
        let short = short_sha(&sha);

        let deploy_run = self.boot_and_replay(repo, &sha, failure).await?;
        let sig = signature_from_process(&deploy_run.stderr, &deploy_run.body, &deploy_run.app_root);
        let reproduced = deploy_run.status >= 500 && sig.as_ref().map(|s| s.matches(&failure.signature)).unwrap_or(false);
        if !reproduced {
            return Ok(Reproduction {
                sha,
                reproduced: false,
                parent_clean: None,
                detail: format!("replay at {short} returned {}; signature did not match", deploy_run.status),
                claims: vec![Claim {
                    text: format!("could not reproduce at {short} (status {})", deploy_run.status),
                    provenance: Provenance::Unresolved,
                }],
            });
        }

        let verified_claim = Claim {
            text: format!("replayed the captured request at {short}: same {}", failure.signature.error_name),
            provenance: Provenance::Verified,
        };

        // A parent-side infrastructure failure (root commit with no parent,
        // a shallow clone missing the parent object, a parent revision that
        // predates the current entry point and never boots) must not erase
        // the deploy revision's confirmed reproduction. `?`-propagating here
        // would throw away a real `Verified` claim and emit no decision
        // packet at all — the honest output is to say what we do know
        // (deploy reproduces) and mark what we couldn't check as unresolved.
        let parent = format!("{sha}^");
        match self.boot_and_replay(repo, &parent, failure).await {
            Ok(parent_run) => {
                let parent_clean = (200..300).contains(&parent_run.status);
                let parent_claim = if parent_clean {
                    Claim { text: format!("parent of {short} serves the same request cleanly"), provenance: Provenance::Verified }
                } else {
                    Claim {
                        text: format!("parent of {short} also fails (status {}) — attribution uncertain", parent_run.status),
                        provenance: Provenance::Unresolved,
                    }
                };
                Ok(Reproduction {
                    sha,
                    reproduced: true,
                    parent_clean: Some(parent_clean),
                    detail: format!("replay {} at deploy, {} at parent", deploy_run.status, parent_run.status),
                    claims: vec![verified_claim, parent_claim],
                })
            }
            Err(e) => Ok(Reproduction {
                sha,
                reproduced: true,
                parent_clean: None,
                detail: format!("replay {} at deploy; parent of {short} could not be rebuilt: {e}", deploy_run.status),
                claims: vec![
                    verified_claim,
                    Claim { text: format!("parent of {short} could not be rebuilt: {e}"), provenance: Provenance::Unresolved },
                ],
            }),
        }
    }
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    // -- signature-from-process: the pilot blocker fix -----------------

    /// A real uvicorn/FastAPI stderr capture (verified against a live
    /// `uvicorn app.main:app` boot — see `tests/python_boot.rs` for the
    /// end-to-end proof) for a route that divides by zero. The response
    /// body for this exact failure is the bare `"Internal Server Error"`
    /// FastAPI's default handler returns — no structured error ever reaches
    /// the client, so this stderr text is the ONLY evidence a signature can
    /// come from.
    ///
    /// Deliberately ends with trailing `INFO:` lines the way a real uvicorn
    /// boot does (the next request's access log, more startup chatter) —
    /// this is the one arrangement in which C1 (the summary line being
    /// hijacked by whatever unindented `Word: text` line comes LAST in the
    /// buffer, rather than the one that actually ends the traceback) is
    /// visible at the unit level. Before the fix, this constant alone made
    /// `signature_from_process_prefers_stderr_over_an_opaque_body` fail with
    /// `error_name == "INFO"`.
    const FASTAPI_STDERR: &str = "INFO:     Started server process [1]\nINFO:     Waiting for application startup.\nINFO:     Application startup complete.\nINFO:     Uvicorn running on http://127.0.0.1:8931\nINFO:     127.0.0.1:1 - \"POST /api/checkout HTTP/1.1\" 500 Internal Server Error\nERROR:    Exception in ASGI application\nTraceback (most recent call last):\n  File \"/usr/local/lib/python3.11/site-packages/uvicorn/protocols/http/h11_impl.py\", line 406, in run_asgi\n    result = await app(\n  File \"/usr/local/lib/python3.11/site-packages/fastapi/applications.py\", line 1054, in __call__\n    await super().__call__(scope, receive, send)\n  File \"/app/api/app/main.py\", line 13, in checkout\n    return {\"total\": total / count}\nZeroDivisionError: division by zero\nINFO:     127.0.0.1:2 - \"GET /health HTTP/1.1\" 200 OK\nINFO:     127.0.0.1:3 - \"GET /health HTTP/1.1\" 200 OK\n";

    #[test]
    fn signature_from_process_prefers_stderr_over_an_opaque_body() {
        let sig = signature_from_process(FASTAPI_STDERR, "Internal Server Error", "/app/api").expect("a signature must be derived from stderr");
        assert_eq!(sig.error_name, "ZeroDivisionError");
        assert_eq!(sig.top_frame_file, "app/main.py");
    }

    #[test]
    fn signature_from_process_falls_back_to_body_when_stderr_has_nothing() {
        let body = r#"{"error":{"name":"TypeError","message":"boom","stack":"TypeError: boom\n    at computeTotal (/w/shop/server.js:5:25)"}}"#;
        let sig = signature_from_process("", body, "/w/shop").expect("must fall back to the body");
        assert_eq!(sig.error_name, "TypeError");
        assert_eq!(sig.top_frame_file, "server.js");
    }

    #[test]
    fn signature_from_process_yields_none_when_neither_source_has_evidence() {
        assert!(signature_from_process("just some ordinary log output\nnothing exceptional here", "Internal Server Error", "/app/api").is_none());
    }

    #[test]
    fn extract_python_exception_takes_the_last_traceback_when_more_than_one_was_logged() {
        let stderr = format!("{FASTAPI_STDERR}\nINFO:     another request came in\n{FASTAPI_STDERR}");
        let (name, _message, stack) = extract_python_exception(&stderr).expect("must find a traceback");
        assert_eq!(name, "ZeroDivisionError");
        // The slice must start at the LAST banner, not the first — proven
        // by counting how many banners survive in the returned stack.
        assert_eq!(stack.matches("Traceback (most recent call last):").count(), 1);
    }

    #[test]
    fn extract_python_exception_is_not_hijacked_by_an_unindented_message_continuation_line() {
        // CPython does not indent continuation lines of a multi-line
        // exception message — `looks_like_python_exception_name` is what
        // stops the second physical line here from being mistaken for the
        // real summary.
        let stderr = "Traceback (most recent call last):\n  File \"/app/api/app/routes/quote.py\", line 20, in create_quote\n    raise RuntimeError('build failed:\\nsee /tmp/build/worker.py for detail')\nRuntimeError: build failed:\nsee /tmp/build/worker.py for detail\n";
        let (name, _message, _stack) = extract_python_exception(stderr).expect("must find the real summary line");
        assert_eq!(name, "RuntimeError");
    }

    #[test]
    fn extract_python_exception_returns_none_without_a_traceback_banner() {
        assert!(extract_python_exception("INFO: nothing went wrong here\n").is_none());
    }

    #[test]
    fn extract_python_exception_is_not_hijacked_by_ordinary_logging_after_the_traceback() {
        // C1 (Critical): a heartbeat thread — or uvicorn's own startup INFO
        // lines — writing an ordinary `Word: text`-shaped line AFTER the
        // real traceback must never overwrite `error_name`. The summary
        // must be bound to the traceback block (first unindented
        // name-shaped line after the LAST frame header), not to whatever
        // unindented name-shaped line happens to be last in the buffer.
        let stderr = format!("{FASTAPI_STDERR}ConnectionError: redis heartbeat failed, retrying\n");
        let (name, _message, _stack) = extract_python_exception(&stderr).expect("must find the real traceback");
        assert_eq!(name, "ZeroDivisionError", "a trailing ConnectionError log line must not overwrite the real exception name");
    }

    #[test]
    fn extract_python_exception_is_not_hijacked_by_uvicorns_own_startup_info_lines() {
        // The exact mechanism named in review: "INFO:     Started server
        // process [57992]" splits at the first `": "` to name = "INFO",
        // and the old last-line-wins scan took it as the summary.
        let stderr = "Traceback (most recent call last):\n  File \"/app/main.py\", line 8, in <module>\n    _ = 10 / 0\nZeroDivisionError: division by zero\nINFO:     Started server process [57992]\nINFO:     Waiting for application startup.\n";
        let (name, _message, _stack) = extract_python_exception(stderr).expect("must find the real traceback");
        assert_eq!(name, "ZeroDivisionError", "uvicorn's own trailing startup INFO lines must not overwrite the real exception name");
    }

    #[test]
    fn extract_python_exception_requires_a_frame_header_between_banner_and_summary() {
        // A banner with nothing beneath it is not a traceback CPython could
        // have produced — must not manufacture a signature from it.
        assert!(extract_python_exception("Traceback (most recent call last):\nZeroDivisionError: division by zero\n").is_none());
    }

    #[test]
    fn extract_python_exception_is_not_hijacked_by_indented_file_lines_after_the_summary() {
        // Round-2 review C1 residual: anchoring on the LAST `File "..."`
        // header is itself movable by anything that emits a
        // frame-header-shaped line AFTER the real summary with no summary
        // of its own — stdlib `traceback.print_stack()` in a heartbeat
        // thread, a `faulthandler` dump, or two threads' tracebacks
        // interleaving on the same fd. Here a heartbeat thread's
        // `traceback.print_stack()` call logs one more `File "..."` frame
        // AFTER the real ZeroDivisionError summary, immediately followed by
        // an ordinary `ConnectionError: ...` log line — the old
        // last-header anchor took THAT as the summary; the real one is
        // `ZeroDivisionError`.
        let stderr = "Traceback (most recent call last):\n  File \"/app/main.py\", line 8, in checkout\n    _ = 10 / 0\nZeroDivisionError: division by zero\n  File \"/app/heartbeat.py\", line 20, in heartbeat\n    traceback.print_stack()\nConnectionError: redis heartbeat failed, retrying\n";
        let (name, _message, stack) = extract_python_exception(stderr).expect("must find the real traceback");
        assert_eq!(name, "ZeroDivisionError", "a trailing print_stack() frame header must not let the next log line hijack error_name");
        assert!(stack.contains("app/main.py"));
        assert!(!stack.contains("ConnectionError"), "stack must be truncated at the real summary, not extended past it");
    }

    // -- StderrBuf: marks must survive MAX_STDERR_BYTES eviction ---------

    #[test]
    fn stderr_buf_mark_and_since_agree_before_any_eviction() {
        // Baseline: with no eviction, cumulative offsets behave exactly
        // like plain buffer offsets always did.
        let mut buf = StderrBuf { bytes: Vec::new(), dropped: 0 };
        buf.push(b"hello ");
        let mark = buf.mark();
        buf.push(b"world");
        assert_eq!(mark, 6);
        assert_eq!(buf.since(mark), "world");
        assert_eq!(buf.since(0), "hello world");
    }

    #[test]
    fn stderr_buf_mark_taken_at_the_cap_still_finds_evidence_written_after_it() {
        // Round-2 review Major, reproduced deterministically at the unit
        // level (probe D was a live ~440 KiB-before-`listening` node app).
        // Fill straight past MAX_STDERR_BYTES so the buffer evicts from the
        // front and `dropped` becomes nonzero, take a mark AT that point
        // (the old bug: a mark taken here was `bytes.len() ==
        // MAX_STDERR_BYTES`, indistinguishable from "everything that will
        // ever exist"), then push the traceback and confirm `since(mark)`
        // returns exactly it rather than an empty slice.
        let mut buf = StderrBuf { bytes: Vec::new(), dropped: 0 };
        let filler = vec![b'x'; MAX_STDERR_BYTES + 10_000];
        buf.push(&filler);
        assert!(buf.dropped > 0, "the fill must have forced an eviction for this test to prove anything");
        assert_eq!(buf.bytes.len(), MAX_STDERR_BYTES);

        let mark = buf.mark();
        assert_eq!(mark, buf.dropped + MAX_STDERR_BYTES, "mark must be cumulative, not the retained length");

        buf.push(b"Traceback (most recent call last):\n  File \"/app/main.py\", line 8\nZeroDivisionError: boom\n");
        let tail = buf.since(mark);
        assert!(tail.contains("ZeroDivisionError"), "evidence written after the mark must be reachable even though the buffer is at the eviction cap; got {tail:?}");
        assert!(!tail.contains('x'), "the tail must not include any of the pre-mark filler");
    }

    #[test]
    fn stderr_buf_since_saturates_when_the_mark_itself_has_been_evicted() {
        // An extremely chatty app can evict PAST a mark that was already
        // taken (not just up to it) — `since` must fall back to whatever
        // oldest byte is still retained rather than panicking on a
        // negative offset or silently going out of bounds.
        let mut buf = StderrBuf { bytes: Vec::new(), dropped: 0 };
        buf.push(b"early evidence that will be evicted");
        let mark = buf.mark();
        let filler = vec![b'y'; MAX_STDERR_BYTES + 50_000];
        buf.push(&filler);
        assert!(buf.dropped > mark, "the fill must evict past the mark itself for this test to prove anything");
        let tail = buf.since(mark);
        assert_eq!(tail.len(), buf.bytes.len(), "since() must fall back to everything currently retained, not panic or misindex");
    }

    #[test]
    fn extract_node_exception_reads_the_uncaught_exception_dump() {
        let stderr = "some startup noise\nError: boom\n    at Object.<anonymous> (/srv/app/server.js:5:9)\n    at Module._compile (node:internal/modules/cjs/loader:1105:14)\n";
        let (name, message, stack) = extract_node_exception(stderr).expect("must find the uncaught exception");
        assert_eq!(name, "Error");
        assert_eq!(message, "boom");
        assert!(stack.starts_with("Error: boom"));
        assert!(stack.contains("at Object.<anonymous>"));
    }

    #[test]
    fn extract_node_exception_takes_the_last_dump_when_more_than_one_was_logged() {
        let stderr = "Error: first\n    at a (/srv/app/one.js:1:1)\nsome log line in between\nTypeError: second\n    at b (/srv/app/two.js:2:2)\n";
        let (name, _message, _stack) = extract_node_exception(stderr).expect("must find an exception");
        assert_eq!(name, "TypeError");
    }

    #[test]
    fn extract_node_exception_ignores_an_error_line_with_no_frames_beneath_it() {
        // A line that merely CONTAINS "Error:" (an ordinary log message, not
        // an uncaught-exception dump) must not be mistaken for one — the
        // required signal is a `    at ...` frame immediately beneath it.
        assert!(extract_node_exception("Error: this is just a log line, nothing threw\n").is_none());
    }

    #[test]
    fn signature_from_stderr_discards_an_empty_frame_candidate_instead_of_shadowing_the_body() {
        // C4 (Major): `extract_node_exception`'s accept shape is loose
        // enough to match a benign boot-time warning whose frames are all
        // `node:` internals — an empty `top_frame_file` means "no
        // evidence", not "evidence of nothing", so it must not be returned
        // as `Some`.
        let stderr = "DeprecationWarning: something is deprecated\n    at emitWarning (node:internal/process/warning:120:9)\n    at node:internal/modules/cjs/loader:1105:14\n";
        assert!(signature_from_stderr(stderr, "/w/shop").is_none(), "an all-node_modules/node: stack must be treated as no evidence, not a false signature");
    }

    #[test]
    fn signature_from_process_falls_back_to_body_when_stderr_only_has_an_empty_frame_candidate() {
        // Proven on the previously-working demo shape: a node app leaking
        // the structured body plus one benign boot-time warning must still
        // reproduce off the body, not get shadowed by the warning.
        let stderr = "DeprecationWarning: something is deprecated\n    at emitWarning (node:internal/process/warning:120:9)\n    at node:internal/modules/cjs/loader:1105:14\n";
        let body = r#"{"error":{"name":"TypeError","message":"boom","stack":"TypeError: boom\n    at computeTotal (/w/shop/server.js:5:25)"}}"#;
        let sig = signature_from_process(stderr, body, "/w/shop").expect("must fall back to the body, not stay shadowed by the empty-frame stderr candidate");
        assert_eq!(sig.error_name, "TypeError");
        assert_eq!(sig.top_frame_file, "server.js");
    }

    #[test]
    fn signature_from_stderr_prefers_python_shape_when_both_markers_are_present() {
        // Mirrors core's own M2 test (a Node service re-throwing a captured
        // Python worker's stderr): a real V8 frame anchor beats a
        // message-embedded `Traceback (most recent call last):`-looking
        // string, since `signature_from_stderr` only takes the Python
        // branch off a REAL traceback banner, and that banner here really
        // is the outer process's own.
        let stderr = "Traceback (most recent call last):\n  File \"/app/worker.py\", line 3, in run\n    raise RuntimeError('boom')\nRuntimeError: boom\n";
        let sig = signature_from_stderr(stderr, "/app").expect("python path must win");
        assert_eq!(sig.error_name, "RuntimeError");
        assert_eq!(sig.top_frame_file, "worker.py");
    }

    // -- build_boot_argv: `--boot-cmd` argv discipline -------------------

    #[test]
    fn build_boot_argv_substitutes_port_as_a_whole_token() {
        let argv = build_boot_argv("uvicorn app.main:app --host 127.0.0.1 --port {port}", 8931);
        assert_eq!(argv, vec!["uvicorn", "app.main:app", "--host", "127.0.0.1", "--port", "8931"]);
    }

    #[test]
    fn build_boot_argv_does_not_reinterpret_shell_metacharacters() {
        // Mirrors `engine_repair::build_argv`'s own
        // `does_not_reinterpret_shell_metacharacters_in_the_prompt` test:
        // a `;` embedded in the template must arrive as its own inert argv
        // element, never as a shell command separator (there is no shell).
        let argv = build_boot_argv("node boot.js --port {port} ; rm -rf /", 3000);
        assert_eq!(argv, vec!["node", "boot.js", "--port", "3000", ";", "rm", "-rf", "/"]);
    }

    #[test]
    fn parse_method_accepts_uncommon_but_valid_methods() {
        assert_eq!(parse_method("PATCH").unwrap(), reqwest::Method::PATCH);
    }

    #[test]
    fn parse_method_uppercases_before_parsing() {
        assert_eq!(parse_method("get").unwrap(), reqwest::Method::GET);
    }

    #[test]
    fn parse_method_rejects_garbage() {
        assert!(parse_method("SP ACE").is_err(), "a space is not a valid HTTP method token");
    }

    #[test]
    fn validate_sha_accepts_hex_shas() {
        assert!(validate_sha("abc123").is_ok());
        assert!(validate_sha("0123456789abcdef0123456789abcdef01234567").is_ok());
    }

    #[test]
    fn validate_sha_rejects_flag_like_input() {
        assert!(validate_sha("--force").is_err());
    }

    #[test]
    fn validate_sha_rejects_empty() {
        assert!(validate_sha("").is_err());
    }

    #[test]
    fn validate_sha_rejects_traversal_strings() {
        assert!(validate_sha("../../../etc/passwd").is_err());
        assert!(validate_sha("../../etc/passwd-abc12345").is_err());
    }

    #[test]
    fn validate_sha_rejects_too_short_and_too_long() {
        assert!(validate_sha("abc").is_err(), "3 chars is below the 4-char floor");
        let too_long = "a".repeat(65);
        assert!(validate_sha(&too_long).is_err(), "65 chars is above the 64-char ceiling");
    }

    // -- m2: short_sha must never byte-slice a multi-byte boundary ----------

    #[test]
    fn short_sha_takes_the_first_six_ascii_chars() {
        assert_eq!(short_sha("abcdef0123456789"), "abcdef");
    }

    #[test]
    fn short_sha_never_panics_on_a_multibyte_char_at_the_cut_point() {
        // RED FIRST against the old `sha[..sha.len().min(6)]`: byte index 6
        // lands inside 'é' (a 2-byte UTF-8 char starting at byte 5), which
        // panics with "byte index 6 is not a char boundary". `short_sha`
        // must survive this input instead.
        let sha = "abcdeé012345";
        let short = short_sha(sha);
        assert!(!short.is_empty());
    }

    #[test]
    fn short_sha_never_panics_on_a_string_shorter_than_six_chars() {
        assert_eq!(short_sha("abc"), "abc");
    }

    // -- ManagedWorktree / BootedApp: the shared primitives the repair
    // pipeline (Task 3) reuses against an existing worktree it keeps alive
    // across a commit + verify sequence. -------------------------------------

    fn run_git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git").arg("-C").arg(dir).args(args).status().expect("git spawn");
        assert!(status.success(), "git {args:?} failed in {}", dir.display());
    }

    /// A tempdir git repo with one commit carrying a minimal real Node HTTP
    /// server: `/health` always 200, `/api/checkout` always 500 (the shape
    /// a repair-verification boot needs — a real listening server, not a
    /// stub). Returns (tempdir, sha of the commit).
    fn init_bootable_fixture_repo() -> (tempfile::TempDir, String) {
        const SERVER_JS: &str = r#"
const http = require("http");
const server = http.createServer((req, res) => {
  if (req.url === "/health") {
    res.writeHead(200);
    res.end("ok");
    return;
  }
  if (req.url === "/api/checkout") {
    res.writeHead(500, { "content-type": "application/json" });
    res.end(JSON.stringify({ error: { name: "TypeError", message: "boom", stack: "TypeError: boom\n    at computeTotal (server.js:4:2)" } }));
    return;
  }
  res.writeHead(404);
  res.end();
});
server.listen(process.env.PORT || 0, () => {
  console.log("listening " + server.address().port);
});
"#;
        let dir = tempfile::tempdir().unwrap();
        run_git(dir.path(), &["init", "-q"]);
        run_git(dir.path(), &["config", "user.email", "t@t"]);
        run_git(dir.path(), &["config", "user.name", "t"]);
        std::fs::write(dir.path().join("server.js"), SERVER_JS).unwrap();
        run_git(dir.path(), &["add", "server.js"]);
        run_git(dir.path(), &["commit", "-q", "-m", "c1"]);
        let sha = String::from_utf8(std::process::Command::new("git").arg("-C").arg(dir.path()).args(["rev-parse", "HEAD"]).output().unwrap().stdout)
            .unwrap()
            .trim()
            .to_string();
        (dir, sha)
    }

    #[test]
    fn managed_worktree_removes_itself_on_drop_by_default() {
        let (repo, sha) = init_bootable_fixture_repo();
        let worktree = ManagedWorktree::create(repo.path(), &sha).expect("worktree must be created");
        let dir = worktree.dir.clone();
        assert!(dir.exists(), "worktree directory must exist right after creation");
        drop(worktree);
        assert!(!dir.exists(), "worktree directory must be removed on drop by default");
    }

    #[test]
    fn managed_worktree_keep_on_drop_leaves_the_directory_for_inspection() {
        let (repo, sha) = init_bootable_fixture_repo();
        let mut worktree = ManagedWorktree::create(repo.path(), &sha).expect("worktree must be created");
        worktree.keep_on_drop = true;
        let dir = worktree.dir.clone();
        drop(worktree);
        assert!(dir.exists(), "keep_on_drop must leave the worktree directory on disk for a human to inspect");
        // best-effort cleanup so the test doesn't litter the real tmp dir
        let _ = std::process::Command::new("git").arg("-C").arg(repo.path()).args(["worktree", "remove", "--force"]).arg(&dir).status();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn booted_app_boots_an_existing_worktree_and_replays_more_than_one_request() {
        // Proves the reuse this refactor exists for: boot ONCE against a
        // directory the caller already checked out (not one BootedApp
        // creates itself), then replay two different requests against the
        // same running instance — the shape the repair-verification path
        // needs (original request, then /health).
        let (repo, sha) = init_bootable_fixture_repo();
        let worktree = ManagedWorktree::create(repo.path(), &sha).expect("worktree must be created");

        let app = BootedApp::boot(&worktree.dir, 15_000).await.expect("app must boot");
        assert!(app.port > 0);
        assert!(!app.app_root.is_empty());

        let checkout_req = CapturedRequest { method: "POST".into(), path: "/api/checkout".into(), content_type: Some("application/json".into()), body: Some("{}".into()) };
        let (status, body) = app.replay(&checkout_req).await.expect("replay must succeed");
        assert_eq!(status, 500, "the fixture server always 500s on /api/checkout");
        assert!(body.contains("TypeError"));

        let health_req = CapturedRequest { method: "GET".into(), path: "/health".into(), content_type: None, body: None };
        let (health_status, _) = app.replay(&health_req).await.expect("health replay must succeed");
        assert_eq!(health_status, 200, "the same booted instance must also answer /health");
    }
}
