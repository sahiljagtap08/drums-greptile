//! `drums daemon {start,stop,status,logs}` — running the same pipeline
//! `drums watch` runs, but as a detached service (`drumsd`) instead of a
//! foreground command a human has to keep a terminal open for.
//!
//! Split into: a JSON pidfile format any of the four subcommands can read
//! and write; process liveness/signal primitives (`kill(pid, 0)`, SIGTERM,
//! SIGKILL — Unix only, this workspace's whole runtime story already is);
//! and pure argv-building so `drums daemon start`'s flag→`drumsd` translation
//! is unit-testable without ever actually spawning a process.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub fn state_dir(repo: &Path) -> PathBuf {
    repo.join(".drums")
}

pub fn pidfile_path(repo: &Path) -> PathBuf {
    state_dir(repo).join("drumsd.pid")
}

pub fn logfile_path(repo: &Path) -> PathBuf {
    state_dir(repo).join("drumsd.log")
}

/// The pidfile's whole content — deliberately more than a bare pid, so
/// `drums daemon status` can answer "what is it watching" and "how long has
/// it been up" by reading ONE file rather than re-deriving them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PidRecord {
    pub pid: u32,
    pub repo: PathBuf,
    pub ingest_port: u16,
    pub started_at_ms: u64,
    pub log_path: PathBuf,
}

pub fn write_pidfile(repo: &Path, record: &PidRecord) -> Result<(), String> {
    let dir = state_dir(repo);
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    let path = pidfile_path(repo);
    let json = serde_json::to_string_pretty(record)
        .map_err(|e| format!("could not encode pidfile: {e}"))?;
    std::fs::write(&path, json).map_err(|e| format!("could not write {}: {e}", path.display()))
}

/// `Ok(None)` when no pidfile exists at all — the normal "never started, or
/// cleanly stopped and cleaned up after itself" state, not an error.
/// `Err` only for a pidfile that exists but is unreadable or not valid JSON
/// in the expected shape (a hand-edited or truncated file) — that is
/// genuinely unreadable state a caller must be told about, not silently
/// treated as "not running", which would let `drums daemon start` spawn a
/// second daemon right on top of one that might still be alive.
pub fn read_pidfile(repo: &Path) -> Result<Option<PidRecord>, String> {
    let path = pidfile_path(repo);
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("could not read {}: {e}", path.display())),
    };
    serde_json::from_str(&content).map(Some).map_err(|e| format!("{} is unreadable ({e}) — a stale or corrupt pidfile; inspect and remove it by hand before retrying", path.display()))
}

/// Best-effort: a pidfile failing to delete must not itself be reported as
/// the operation's failure (the process it named is already gone by the time
/// this is called).
fn remove_pidfile(repo: &Path) {
    let _ = std::fs::remove_file(pidfile_path(repo));
}

// -- process liveness / signalling (Unix only — this whole runtime story,
// process groups and all, already is) --------------------------------------

#[cfg(unix)]
/// `kill(pid, 0)`: sends no signal, but IS validated by the kernel — `ESRCH`
/// means no such process (dead), `EPERM` means it exists but this process
/// lacks permission to signal it (so it's alive, just not ours to control),
/// success means alive and ours.
pub fn is_process_alive(pid: u32) -> bool {
    let ret = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if ret == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(unix)]
fn send_signal(pid: u32, sig: libc::c_int) -> Result<(), String> {
    let ret = unsafe { libc::kill(pid as libc::pid_t, sig) };
    if ret == 0 {
        return Ok(());
    }
    let e = std::io::Error::last_os_error();
    if e.raw_os_error() == Some(libc::ESRCH) {
        return Ok(()); // already gone — not this call's failure to report
    }
    Err(format!("kill({pid}, {sig}) failed: {e}"))
}

#[cfg(unix)]
/// The session id `pid` belongs to. Used only by the "survives the terminal
/// closing" integration test (`tests/daemon_detach.rs`) to prove `drumsd`
/// runs in a session of its own (via `setsid` at spawn — see
/// [`spawn_detached`]) rather than the invoking shell's — the actual
/// property that makes a terminal hangup unable to reach it.
pub fn session_id(pid: u32) -> Result<i32, String> {
    let sid = unsafe { libc::getsid(pid as libc::pid_t) };
    if sid == -1 {
        return Err(format!(
            "getsid({pid}) failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(sid)
}

// -- status -------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum DaemonStatus {
    /// No pidfile at all.
    NotRunning { pidfile: PathBuf },
    /// A pidfile exists, but the pid it names is not alive — a crash, a
    /// `kill -9` from outside `drums daemon stop`, or a machine reboot that
    /// never cleaned up. Reported honestly, never as "running".
    Stale { pidfile: PathBuf, recorded_pid: u32 },
    Running {
        pid: u32,
        repo: PathBuf,
        ingest_port: u16,
        uptime_ms: u64,
        log_path: PathBuf,
    },
}

/// Reads the pidfile and VERIFIES the process is actually alive before ever
/// reporting "running" — the whole point of this function over a bare
/// pidfile read (task requirement: "a stale pidfile must report honestly,
/// not lie").
pub fn status(repo: &Path, now_ms: u64) -> Result<DaemonStatus, String> {
    let pidfile = pidfile_path(repo);
    match read_pidfile(repo)? {
        None => Ok(DaemonStatus::NotRunning { pidfile }),
        Some(rec) => {
            if is_process_alive(rec.pid) {
                Ok(DaemonStatus::Running {
                    pid: rec.pid,
                    repo: rec.repo,
                    ingest_port: rec.ingest_port,
                    uptime_ms: now_ms.saturating_sub(rec.started_at_ms),
                    log_path: rec.log_path,
                })
            } else {
                Ok(DaemonStatus::Stale {
                    pidfile,
                    recorded_pid: rec.pid,
                })
            }
        }
    }
}

fn format_uptime(ms: u64) -> String {
    let secs = ms / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3_600 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{}h{}m", secs / 3_600, (secs % 3_600) / 60)
    }
}

pub fn render_status(s: &DaemonStatus) -> String {
    match s {
        DaemonStatus::NotRunning { pidfile } => {
            format!("not running (no pidfile at {})", pidfile.display())
        }
        DaemonStatus::Stale {
            pidfile,
            recorded_pid,
        } => {
            format!("not running — stale pidfile at {} names pid {recorded_pid}, which is not alive (the daemon likely crashed or was killed outside `drums daemon stop`)", pidfile.display())
        }
        DaemonStatus::Running {
            pid,
            repo,
            ingest_port,
            uptime_ms,
            log_path,
        } => format!(
            "running (pid {pid}, up {}) — watching {} · ingest :{ingest_port} · log: {}",
            format_uptime(*uptime_ms),
            repo.display(),
            log_path.display()
        ),
    }
}

// -- start: argv-building (pure) + spawning (impure, Unix-only) --------

/// Everything `drums daemon start` resolved (flags merged over config over
/// defaults — see `crate::config::resolve`) that `drumsd` needs on its own
/// command line. `drumsd` takes concrete, already-resolved values: the
/// merge/refuse-if-unconfigured decision happens here, in the command the
/// operator is actually looking at, not silently inside a detached child
/// whose stdout nobody is watching.
#[derive(Debug, Clone, PartialEq)]
pub struct StartArgs {
    pub repo: PathBuf,
    pub ingest_port: u16,
    pub threshold: usize,
    pub window_secs: u64,
    pub app_root: Option<String>,
    pub repair_auto: bool,
    pub deploy_cmd: Option<String>,
    pub check_url: Option<String>,
    pub repair_reported: bool,
}

/// Pure translation of [`StartArgs`] into `drumsd`'s own argv — no process
/// spawned, so every flag-shape decision here is unit-testable directly.
pub fn build_drumsd_argv(args: &StartArgs) -> Vec<String> {
    let mut argv = vec![
        "--repo".to_string(),
        args.repo.display().to_string(),
        "--ingest-port".to_string(),
        args.ingest_port.to_string(),
        "--threshold".to_string(),
        args.threshold.to_string(),
        "--window-secs".to_string(),
        args.window_secs.to_string(),
        "--repair".to_string(),
        if args.repair_auto { "auto" } else { "propose" }.to_string(),
    ];
    if let Some(app_root) = &args.app_root {
        argv.push("--app-root".to_string());
        argv.push(app_root.clone());
    }
    if let Some(deploy_cmd) = &args.deploy_cmd {
        argv.push("--deploy-cmd".to_string());
        argv.push(deploy_cmd.clone());
    }
    if let Some(check_url) = &args.check_url {
        argv.push("--check-url".to_string());
        argv.push(check_url.clone());
    }
    if args.repair_reported {
        argv.push("--repair-reported".to_string());
    }
    argv
}

/// How `drums daemon start` will actually run the daemon: which binary, with
/// which argv.
///
/// In order: `DRUMS_DAEMON_BIN` (an explicit override, refused by name if it
/// points at nothing); a `drumsd` sibling of the running `drums` executable
/// (a source build — both `[[bin]]`s land in the same `target/<profile>/`
/// directory); and finally the running `drums` executable itself, re-exec'd
/// as `drums watch`. The public release ships a single `drums` binary with
/// no `drumsd` beside it, and a daemon that only starts from a source
/// checkout is a daemon that never starts for a customer. The fallback is
/// not an approximation: `drumsd`'s argv is exactly `drums watch`'s flags
/// minus the subcommand (compare [`build_drumsd_argv`] with `bin/drumsd.rs`),
/// so the same executable runs the identical pipeline.
pub fn resolve_daemon_invocation(args: &StartArgs) -> Result<(PathBuf, Vec<String>), String> {
    let drumsd_argv = build_drumsd_argv(args);
    if let Ok(overridden) = std::env::var("DRUMS_DAEMON_BIN") {
        let p = PathBuf::from(overridden);
        if !p.is_file() {
            return Err(format!(
                "DRUMS_DAEMON_BIN names {}, which does not exist — unset it to let `drums` run the daemon itself, or point it at a real drumsd binary",
                p.display()
            ));
        }
        return Ok((p, drumsd_argv));
    }
    let exe = std::env::current_exe().map_err(|e| {
        format!("could not resolve the running `drums` binary's own path: {e} — set DRUMS_DAEMON_BIN to name the daemon binary directly")
    })?;
    let name = if cfg!(windows) {
        "drumsd.exe"
    } else {
        "drumsd"
    };
    let sibling = exe.with_file_name(name);
    if sibling.is_file() {
        return Ok((sibling, drumsd_argv));
    }
    let mut watch_argv = Vec::with_capacity(drumsd_argv.len() + 1);
    watch_argv.push("watch".to_string());
    watch_argv.extend(drumsd_argv);
    Ok((exe, watch_argv))
}

#[cfg(unix)]
/// Spawns `binary argv…` fully detached: stdin `/dev/null`, stdout+stderr
/// appended to `log_path` (never the caller's own terminal), and — via
/// `pre_exec` — `setsid()` in the child before it execs, making it the
/// leader of a brand-new session with no controlling terminal at all. That
/// last part is what "survives the terminal closing" actually rests on: a
/// terminal hangup delivers SIGHUP to processes IN THAT SESSION; a process
/// with no controlling terminal is never a member of one, so it's never a
/// target no matter what happens to the shell that ran `drums daemon
/// start`.
///
/// Returns the spawned `Child` deliberately never waited on: the caller
/// (`drums daemon start`) records its pid to the pidfile and returns
/// immediately, exactly as documented. Rust does not kill a child when its
/// `Child` handle drops, so letting this value go out of scope leaves the
/// process running, orphaned to be reaped by init/launchd whenever it
/// eventually exits — the standard single-fork Unix daemonizing shape.
pub fn spawn_detached(
    binary: &Path,
    argv: &[String],
    log_path: &Path,
) -> Result<std::process::Child, String> {
    use std::os::unix::process::CommandExt;

    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
    }
    let log_out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .map_err(|e| format!("could not open log file {}: {e}", log_path.display()))?;
    let log_err = log_out
        .try_clone()
        .map_err(|e| format!("could not duplicate the log file handle: {e}"))?;

    let mut cmd = std::process::Command::new(binary);
    cmd.args(argv)
        .stdin(std::process::Stdio::null())
        .stdout(log_out)
        .stderr(log_err);
    // SAFETY: the closure only calls async-signal-safe functions
    // (`setsid(2)`) and touches no Rust heap state shared with the parent —
    // the one contract `pre_exec` requires of its closure.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.spawn()
        .map_err(|e| format!("could not spawn {}: {e}", binary.display()))
}

// -- stop -----------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum StopOutcome {
    /// No pidfile, or a stale one for a pid that was already dead —
    /// `drums daemon stop` on an already-stopped daemon is a harmless no-op,
    /// not an error, but it must say so honestly (never "stopped it" for a
    /// daemon that was never running).
    AlreadyStopped,
    StoppedGracefully {
        pid: u32,
    },
    /// SIGTERM did not make it exit within the bound; SIGKILL did.
    ForceKilled {
        pid: u32,
    },
}

pub fn render_stop_outcome(o: &StopOutcome) -> String {
    match o {
        StopOutcome::AlreadyStopped => "not running — nothing to stop.".to_string(),
        StopOutcome::StoppedGracefully { pid } => format!("stopped drumsd (pid {pid}) — in-flight reproduction/repair processes killed, temporary worktrees removed, same as `drums watch`'s own ctrl-c/SIGTERM path."),
        StopOutcome::ForceKilled { pid } => format!("drumsd (pid {pid}) did not exit after SIGTERM within the grace period — sent SIGKILL. Any in-flight worktree it hadn't already cleaned up may be left for inspection."),
    }
}

/// SIGTERM, wait up to `term_wait` (polling every 100ms) for the process to
/// actually exit — not merely for the signal to have been SENT — then
/// SIGKILL if it's still alive. Draining/killing the agent's process group
/// and reproduction worktrees happens INSIDE `drumsd` itself on receiving
/// SIGTERM (it shares `drums watch`'s own `wait_for_stop_signal` +
/// drop-the-runtime shutdown path — see `bin/drumsd.rs`), so this function's
/// only job is making sure that shutdown actually completed before this
/// command exits, and escalating if it didn't.
#[cfg(unix)]
pub async fn stop(repo: &Path, term_wait: Duration) -> Result<StopOutcome, String> {
    let Some(rec) = read_pidfile(repo)? else {
        return Ok(StopOutcome::AlreadyStopped);
    };
    if !is_process_alive(rec.pid) {
        remove_pidfile(repo);
        return Ok(StopOutcome::AlreadyStopped);
    }
    send_signal(rec.pid, libc::SIGTERM)?;

    let deadline = tokio::time::Instant::now() + term_wait;
    while tokio::time::Instant::now() < deadline {
        if !is_process_alive(rec.pid) {
            remove_pidfile(repo);
            return Ok(StopOutcome::StoppedGracefully { pid: rec.pid });
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    send_signal(rec.pid, libc::SIGKILL)?;
    // Give SIGKILL a brief, bounded moment to actually take effect before
    // reporting — the kernel delivers it essentially immediately, but this
    // avoids a race where the pidfile is removed a hair before the process
    // table has caught up.
    let kill_deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < kill_deadline && is_process_alive(rec.pid) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    remove_pidfile(repo);
    Ok(StopOutcome::ForceKilled { pid: rec.pid })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(pid: u32, repo: &Path) -> PidRecord {
        PidRecord {
            pid,
            repo: repo.to_path_buf(),
            ingest_port: 7787,
            started_at_ms: 1_000,
            log_path: logfile_path(repo),
        }
    }

    #[test]
    fn pidfile_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let r = rec(12345, dir.path());
        write_pidfile(dir.path(), &r).unwrap();
        let read = read_pidfile(dir.path()).unwrap().expect("just wrote it");
        assert_eq!(read, r);
    }

    #[test]
    fn read_pidfile_on_a_repo_that_was_never_started_is_ok_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_pidfile(dir.path()).unwrap(), None);
    }

    #[test]
    fn read_pidfile_on_corrupt_content_is_a_named_refusal() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(state_dir(dir.path())).unwrap();
        std::fs::write(pidfile_path(dir.path()), "not json").unwrap();
        let err =
            read_pidfile(dir.path()).expect_err("must refuse, not silently report not-running");
        assert!(err.contains("drumsd.pid"), "{err}");
    }

    /// The task's own words: "a stale pidfile must report honestly, not
    /// lie". Uses a pid guaranteed dead — a real child this test spawns and
    /// waits out — rather than a guessed unlikely-to-exist number.
    fn a_definitely_dead_pid() -> u32 {
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn `true`");
        let pid = child.id();
        child.wait().expect("wait for it to exit");
        pid
    }

    #[cfg(unix)]
    #[test]
    fn status_reports_stale_honestly_for_a_pidfile_naming_a_dead_process() {
        let dir = tempfile::tempdir().unwrap();
        let dead = a_definitely_dead_pid();
        write_pidfile(dir.path(), &rec(dead, dir.path())).unwrap();
        let s = status(dir.path(), 2_000).unwrap();
        match s {
            DaemonStatus::Stale { recorded_pid, .. } => assert_eq!(recorded_pid, dead),
            other => panic!("expected Stale, got {other:?}"),
        }
        assert!(render_status(&s).contains("not running"));
    }

    #[cfg(unix)]
    #[test]
    fn status_reports_running_for_this_very_test_processs_own_pid() {
        let dir = tempfile::tempdir().unwrap();
        let me = std::process::id();
        write_pidfile(dir.path(), &rec(me, dir.path())).unwrap();
        let s = status(dir.path(), 61_000).unwrap();
        match s {
            DaemonStatus::Running { pid, uptime_ms, .. } => {
                assert_eq!(pid, me);
                assert_eq!(uptime_ms, 60_000);
            }
            other => panic!("expected Running, got {other:?}"),
        }
        assert!(render_status(&s).contains("running"));
    }

    #[test]
    fn status_on_a_repo_with_no_pidfile_is_not_running() {
        let dir = tempfile::tempdir().unwrap();
        let s = status(dir.path(), 0).unwrap();
        assert!(matches!(s, DaemonStatus::NotRunning { .. }));
        assert!(render_status(&s).contains("not running"));
    }

    #[cfg(unix)]
    #[test]
    fn is_process_alive_is_true_for_self_and_false_for_a_dead_pid() {
        assert!(is_process_alive(std::process::id()));
        assert!(!is_process_alive(a_definitely_dead_pid()));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stop_on_a_never_started_repo_is_already_stopped_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = stop(dir.path(), Duration::from_millis(50)).await.unwrap();
        assert_eq!(outcome, StopOutcome::AlreadyStopped);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stop_on_a_stale_pidfile_reports_already_stopped_and_cleans_it_up() {
        let dir = tempfile::tempdir().unwrap();
        write_pidfile(dir.path(), &rec(a_definitely_dead_pid(), dir.path())).unwrap();
        let outcome = stop(dir.path(), Duration::from_millis(50)).await.unwrap();
        assert_eq!(outcome, StopOutcome::AlreadyStopped);
        assert_eq!(
            read_pidfile(dir.path()).unwrap(),
            None,
            "the stale pidfile must be cleaned up"
        );
    }

    /// A real, currently-running child that ignores SIGTERM: proves the
    /// SIGTERM→wait→SIGKILL escalation actually happens rather than hanging
    /// forever or giving up early.
    #[cfg(unix)]
    #[tokio::test]
    async fn stop_escalates_to_sigkill_when_the_process_ignores_sigterm() {
        let dir = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new("sh")
            .args(["-c", "trap '' TERM; sleep 30"])
            .spawn()
            .expect("spawn");
        let pid = child.id();
        // Give the trap a moment to install before we ever signal it.
        tokio::time::sleep(Duration::from_millis(200)).await;
        write_pidfile(dir.path(), &rec(pid, dir.path())).unwrap();

        let outcome = stop(dir.path(), Duration::from_millis(300)).await.unwrap();
        assert_eq!(outcome, StopOutcome::ForceKilled { pid });
        assert_eq!(read_pidfile(dir.path()).unwrap(), None);
        let _ = child.wait();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stop_exits_gracefully_when_the_process_honors_sigterm() {
        let dir = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn");
        let pid = child.id();
        write_pidfile(dir.path(), &rec(pid, dir.path())).unwrap();

        // Reap the child the instant it exits. In real operation `drumsd`
        // is an orphan by the time `drums daemon stop` runs (its own
        // spawning `drums daemon start` process already exited), so
        // init/launchd reaps it automatically the moment it dies — a
        // freshly-dead pid is simply gone, never a zombie `kill(pid, 0)`
        // would still see. THIS test still holds a live `Child` handle
        // (Rust never auto-reaps), so without a thread standing in for
        // init here, the process would sit as a zombie — still visible to
        // `kill(pid, 0)` — until something waits on it, which would make
        // `is_process_alive` unable to ever observe it as dead and turn
        // this into a false SIGKILL escalation.
        let reaper = std::thread::spawn(move || {
            let _ = child.wait();
        });

        let outcome = stop(dir.path(), Duration::from_secs(5)).await.unwrap();
        assert_eq!(outcome, StopOutcome::StoppedGracefully { pid });
        assert_eq!(read_pidfile(dir.path()).unwrap(), None);
        reaper.join().unwrap();
    }

    // -- argv building (pure) -------------------------------------------

    fn min_args(repo: &Path) -> StartArgs {
        StartArgs {
            repo: repo.to_path_buf(),
            ingest_port: 7787,
            threshold: 3,
            window_secs: 60,
            app_root: None,
            repair_auto: false,
            deploy_cmd: None,
            check_url: None,
            repair_reported: false,
        }
    }

    #[test]
    fn build_drumsd_argv_carries_every_required_flag() {
        let dir = tempfile::tempdir().unwrap();
        let argv = build_drumsd_argv(&min_args(dir.path()));
        assert!(argv
            .windows(2)
            .any(|w| w[0] == "--repo" && w[1] == dir.path().display().to_string()));
        assert!(argv
            .windows(2)
            .any(|w| w[0] == "--ingest-port" && w[1] == "7787"));
        assert!(argv
            .windows(2)
            .any(|w| w[0] == "--threshold" && w[1] == "3"));
        assert!(argv
            .windows(2)
            .any(|w| w[0] == "--window-secs" && w[1] == "60"));
        assert!(argv
            .windows(2)
            .any(|w| w[0] == "--repair" && w[1] == "propose"));
        assert!(!argv.contains(&"--app-root".to_string()));
        assert!(!argv.contains(&"--deploy-cmd".to_string()));
        assert!(!argv.contains(&"--check-url".to_string()));
    }

    #[test]
    fn build_drumsd_argv_includes_optional_flags_only_when_set() {
        let dir = tempfile::tempdir().unwrap();
        let mut args = min_args(dir.path());
        args.repair_auto = true;
        args.app_root = Some("/srv/app".to_string());
        args.deploy_cmd = Some("bash deploy.sh {sha}".to_string());
        args.check_url = Some("http://localhost/health".to_string());
        let argv = build_drumsd_argv(&args);
        assert!(argv
            .windows(2)
            .any(|w| w[0] == "--repair" && w[1] == "auto"));
        assert!(argv
            .windows(2)
            .any(|w| w[0] == "--app-root" && w[1] == "/srv/app"));
        assert!(argv
            .windows(2)
            .any(|w| w[0] == "--deploy-cmd" && w[1] == "bash deploy.sh {sha}"));
        assert!(argv
            .windows(2)
            .any(|w| w[0] == "--check-url" && w[1] == "http://localhost/health"));
    }

    /// Serializes the tests that mutate `DRUMS_DAEMON_BIN` — cargo runs
    /// tests on parallel threads, and the process env is shared.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn resolve_honors_an_env_override_that_names_a_real_file() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("drumsd");
        std::fs::write(&fake, "").unwrap();
        // SAFETY: this test's own process env, serialized by ENV_LOCK against
        // the other DRUMS_DAEMON_BIN tests.
        unsafe { std::env::set_var("DRUMS_DAEMON_BIN", &fake) };
        let resolved = resolve_daemon_invocation(&min_args(dir.path()));
        unsafe { std::env::remove_var("DRUMS_DAEMON_BIN") };
        let (bin, argv) = resolved.unwrap();
        assert_eq!(bin, fake);
        assert_eq!(
            argv,
            build_drumsd_argv(&min_args(dir.path())),
            "an explicit drumsd gets drumsd's own argv, no `watch` prefix"
        );
    }

    #[test]
    fn resolve_refuses_an_env_override_that_names_nothing() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: as above — serialized by ENV_LOCK.
        unsafe { std::env::set_var("DRUMS_DAEMON_BIN", "/tmp/wherever/drumsd") };
        let resolved = resolve_daemon_invocation(&min_args(dir.path()));
        unsafe { std::env::remove_var("DRUMS_DAEMON_BIN") };
        let err = resolved
            .expect_err("an override pointing at nothing must refuse, not spawn-fail later");
        assert!(err.contains("DRUMS_DAEMON_BIN"), "{err}");
        assert!(err.contains("/tmp/wherever/drumsd"), "{err}");
        assert!(
            err.contains("unset it"),
            "the refusal must name the fix: {err}"
        );
    }

    /// B1: the released layout — a single `drums` binary, no `drumsd`
    /// sibling (a test binary's own `deps/` directory has none either, so
    /// this exercises exactly that layout). The fallback must re-exec the
    /// current executable as `drums watch` with drumsd's own flags.
    #[test]
    fn with_no_drumsd_shipped_the_daemon_reexecs_drums_watch() {
        let _guard = ENV_LOCK.lock().unwrap();
        // SAFETY: as above — serialized by ENV_LOCK.
        unsafe { std::env::remove_var("DRUMS_DAEMON_BIN") };
        let dir = tempfile::tempdir().unwrap();
        let (bin, argv) = resolve_daemon_invocation(&min_args(dir.path())).unwrap();
        assert_eq!(bin, std::env::current_exe().unwrap());
        assert_eq!(argv.first().map(String::as_str), Some("watch"));
        assert_eq!(
            argv[1..],
            build_drumsd_argv(&min_args(dir.path()))[..],
            "everything after `watch` is drumsd's own argv, unchanged"
        );
    }
}
