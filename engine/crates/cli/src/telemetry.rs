//! Anonymous usage telemetry, and the disclosure that is the price of it.
//!
//! ## Why this file is written the way it is
//!
//! Drums is installed with `curl | sh` and runs on machines whose owners are
//! in safety-sensitive and regulated work. The product's whole argument is
//! that it never claims more than it can prove. **Telemetry that was
//! discovered later rather than announced up front would be exactly the
//! failure this product exists to argue against** — it would not matter how
//! little was sent, only that it was sent quietly. So four rules, each of
//! which is enforced here by code rather than promised in prose:
//!
//! 1. **The first run that would send anything says so first**, on stdout,
//!    in full: what is sent, what is never sent, and the exact command that
//!    turns it off. Shown once per machine (a marker file next to the
//!    install id), never once per run — see [`start`] and
//!    [`Telemetry::mark_disclosed`].
//! 2. **Opt-out is trivial and is honoured at the sending seam**, not merely
//!    at the disclosure. `DRUMS_TELEMETRY=off` or `telemetry = "off"` in
//!    `.drums/config.toml`; the env var wins, on the same "what the operator
//!    typed on THIS invocation beats a file written earlier" rule
//!    [`crate::config::resolve`] already applies to flags. An unrecognised
//!    value resolves to OFF, loudly — see [`Decision::OffByUnrecognised`].
//! 3. **[`Payload`] is a closed list.** Not "customer data is redacted
//!    before sending" — there is nowhere in the struct to put it. The list
//!    is guarded by a test that fails if a field is ever added carelessly
//!    (`engine/crates/cli/tests/telemetry.rs`).
//! 4. **Nothing here can slow the loop or fail it.** Sending is a detached
//!    background task with a short timeout, and [`send_once`] returns `()` —
//!    there is deliberately no error for a caller to narrate, because our
//!    analytics being down is not the operator's problem and must never be
//!    printed as though a repair went wrong.
//!
//! ## Known gap, stated rather than hidden
//!
//! Only `drums watch` is wired to this module. `drumsd` (the detached
//! daemon) sends nothing: its stdout is a logfile nobody is watching at
//! startup, so a first-run disclosure printed there would not be a
//! disclosure at all. Wiring the daemon needs a place to show the notice
//! where a human is actually looking (`drums daemon start`'s own terminal),
//! and that is not built.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::engine::EngineEvent;

/// Where a heartbeat goes when nothing overrides it. `DRUMS_TELEMETRY_URL`
/// overrides it — which is how the tests in
/// `engine/crates/cli/tests/telemetry.rs` point a real `drums` at a real
/// local server and assert on the exact bytes it received.
/// Where heartbeats go.
///
/// Deliberately a path on the main site rather than a `telemetry.` subdomain:
/// a separate hostname is a separate DNS record, a separate certificate, and a
/// separate thing to be quietly broken for months while every install fails a
/// send nobody is watching. It also reads better to the person running
/// `tcpdump` during a security review — the traffic goes to the same host they
/// already downloaded from, not to an analytics-shaped name they would have to
/// go look up.
pub const DEFAULT_TELEMETRY_URL: &str = "https://drums.sh/api/telemetry";

/// Short on purpose. This is a fire-and-forget POST on a background task; if
/// the endpoint is slow, the right answer is to give up on this heartbeat,
/// not to hold a task open behind it.
const SEND_TIMEOUT: Duration = Duration::from_secs(5);

/// How often a running `drums watch` repeats the heartbeat. The first one is
/// sent at startup — that is the "this install is active" signal — and the
/// counters it carries are this process's running totals, never a per-repo
/// or per-failure breakdown.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(6 * 3600);

// -- the payload -------------------------------------------------------------

/// **Every field that is ever sent, and the entire contract.**
///
/// # What must NEVER appear in this struct
///
/// Not "redacted before sending" — ABSENT, with nowhere to put it:
///
/// * repository names, organisation names, remote URLs
/// * file paths of any kind, absolute or repo-relative
/// * branch names, commit shas, commit messages, author names
/// * error names, error messages, stack traces, log lines
/// * request bodies, request paths, query strings, headers, any URL
/// * agent output, prompts, diffs, patches, test output
/// * failure-class names (`service/ErrorName`) — a service name is the
///   customer's own vocabulary and identifies their system
/// * anything computed from, hashed from, or keyed on any of the above. A
///   per-file or per-service counter is still a fact about their code; a
///   hash of a repo name is still a repo name to anyone holding the list.
///
/// The four counters below are whole-process totals with no key, which is
/// precisely what makes them safe: "17 failures detected" says nothing about
/// which failures, in what, or where.
///
/// **This list is enforced.** `the_payload_carries_exactly_these_fields`
/// in `engine/crates/cli/tests/telemetry.rs` compares the serialised key set
/// against a literal list; adding a field here without deliberately going and
/// changing that list fails the build. That is the point.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Payload {
    /// Random, generated once on this machine, stored in a plain-text file
    /// the operator can read and delete (delete it and you become a new
    /// install). Deliberately NOT derived from the hostname, the MAC
    /// address, the username, the repo path, or any other property of the
    /// machine — nothing about the machine can reproduce it, and two
    /// installs on one machine get different ids.
    pub install_id: String,
    /// This binary's own version, from `CARGO_PKG_VERSION`.
    pub drums_version: String,
    /// `std::env::consts::OS` — `"macos"`, `"linux"`, ... A compile-time
    /// constant of this binary, not an inspection of the running system.
    pub os: String,
    /// `std::env::consts::ARCH` — `"aarch64"`, `"x86_64"`, ... Also a
    /// compile-time constant.
    pub arch: String,
    pub failures_detected: u64,
    pub repairs_attempted: u64,
    pub repairs_verified: u64,
    pub repairs_shipped: u64,
}

/// The payload's field names as data, so the disclosure text and the tests
/// can both read the list from one place instead of restating it.
pub const PAYLOAD_FIELDS: &[&str] = &[
    "install_id",
    "drums_version",
    "os",
    "arch",
    "failures_detected",
    "repairs_attempted",
    "repairs_verified",
    "repairs_shipped",
];

// -- the counters ------------------------------------------------------------

/// The four coarse totals, folded from the same [`EngineEvent`] stream the
/// terminal already narrates. Atomics rather than a lock because the fold
/// happens on the hot tap task that must never be able to stall the engine.
#[derive(Debug, Default)]
pub struct Counters {
    failures_detected: AtomicU64,
    repairs_attempted: AtomicU64,
    repairs_verified: AtomicU64,
    repairs_shipped: AtomicU64,
}

/// A point-in-time read of [`Counters`]. Four numbers; nothing that could
/// identify what produced them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Totals {
    pub failures_detected: u64,
    pub repairs_attempted: u64,
    pub repairs_verified: u64,
    pub repairs_shipped: u64,
}

impl Counters {
    /// Fold one engine event into the totals.
    ///
    /// The catch-all arm is deliberate and is a safety property, not
    /// laziness: a future `EngineEvent` variant carrying customer data
    /// cannot widen what telemetry observes by accident — someone has to
    /// come here and name it.
    pub fn observe(&self, ev: &EngineEvent) {
        let counter = match ev {
            EngineEvent::FailureDetected(_) => &self.failures_detected,
            EngineEvent::Repairing(_, _) => &self.repairs_attempted,
            // `RepairReady` is the ONLY event that means verification
            // passed. `ReportedRepairReady` clears a non-regression bar and
            // is permanently `unresolved` about whether it fixed anything
            // (see its doc on `EngineEvent`), so counting it here would
            // inflate the one number that claims proof.
            EngineEvent::RepairReady(_, _, _) => &self.repairs_verified,
            EngineEvent::Shipped(_, _) => &self.repairs_shipped,
            _ => return,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub fn totals(&self) -> Totals {
        Totals {
            failures_detected: self.failures_detected.load(Ordering::Relaxed),
            repairs_attempted: self.repairs_attempted.load(Ordering::Relaxed),
            repairs_verified: self.repairs_verified.load(Ordering::Relaxed),
            repairs_shipped: self.repairs_shipped.load(Ordering::Relaxed),
        }
    }
}

// -- the opt-out -------------------------------------------------------------

/// Whether anything will be sent at all, and — when it will not — which
/// switch decided that, so the reason can be named instead of guessed at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    On,
    /// `DRUMS_TELEMETRY` said so.
    OffByEnv,
    /// `.drums/config.toml`'s `telemetry` key said so, or that file exists
    /// and could not be parsed (see [`start`] — an unreadable config is read
    /// as `off`, never as consent).
    OffByConfig,
    /// A switch was set to a value this version does not recognise. Resolved
    /// to OFF rather than to the default, and never silently: someone who
    /// typed `DRUMS_TELEMETRY=disable` meant to stop the sending, and
    /// falling back to the default there would be reading a clear intention
    /// as permission.
    OffByUnrecognised {
        source: &'static str,
        value: String,
    },
}

impl Decision {
    pub fn is_on(&self) -> bool {
        matches!(self, Decision::On)
    }

    pub fn label(&self) -> &'static str {
        if self.is_on() {
            "on"
        } else {
            "off"
        }
    }
}

/// `"off"`/`"on"` and the spellings people actually type. `None` means "this
/// version does not recognise it", which callers turn into OFF plus a named
/// complaint — never into a silent default.
fn parse_switch(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "off" | "0" | "false" | "no" | "none" | "disabled" | "disable" => Some(false),
        "on" | "1" | "true" | "yes" | "enabled" | "enable" => Some(true),
        _ => None,
    }
}

/// Resolve the two switches into one decision. Pure, so the precedence rule
/// is testable without touching this process's environment.
///
/// An empty string counts as unset in both positions: `DRUMS_TELEMETRY=`
/// is how a shell script clears an inherited variable, and reading that as
/// an unrecognised value would turn an "unset it" into an opt-out the
/// operator never asked for.
pub fn decide(env: Option<&str>, config: Option<&str>) -> Decision {
    for (source, value) in [
        ("DRUMS_TELEMETRY", env),
        (".drums/config.toml `telemetry`", config),
    ] {
        let Some(value) = value.map(str::trim).filter(|v| !v.is_empty()) else {
            continue;
        };
        return match parse_switch(value) {
            Some(true) => Decision::On,
            Some(false) if source == "DRUMS_TELEMETRY" => Decision::OffByEnv,
            Some(false) => Decision::OffByConfig,
            None => Decision::OffByUnrecognised {
                source,
                value: value.to_string(),
            },
        };
    }
    Decision::On
}

// -- where the install id and the disclosure marker live ---------------------

/// `~/.drums` — the per-MACHINE directory, deliberately not the repo's own
/// `.drums/`. The install id identifies an install, not a checkout, and the
/// first-run notice has to appear once per machine rather than once per
/// repository someone happens to point `drums watch` at.
///
/// `DRUMS_HOME` overrides it. `None` when neither `DRUMS_HOME` nor `HOME` is
/// set (notably Windows, where `HOME` usually is not) — and every caller
/// treats `None` as "send nothing" rather than falling back to a guess,
/// because a machine with nowhere to persist an id is a machine that would
/// otherwise get a NEW id on every run.
pub fn home_dir() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("DRUMS_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(explicit));
    }
    std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(|h| PathBuf::from(h).join(".drums"))
}

pub fn install_id_path(home: &Path) -> PathBuf {
    home.join("install-id")
}

/// The marker that says the first-run notice has already been shown on this
/// machine. Its presence is the ONLY thing that suppresses the notice, so
/// deleting it shows the notice again — which is the behaviour someone
/// auditing this would want.
pub fn disclosure_marker_path(home: &Path) -> PathBuf {
    home.join("telemetry-disclosed")
}

/// 128 random bits, lowercase hex.
///
/// `Ulid::random()` is the 80 random bits of a freshly drawn ULID; the
/// 48-bit timestamp half is thrown away, twice, so the id does not even
/// encode WHEN it was created — an install id that carried its own creation
/// time would be one more fact about the machine that nobody asked to send.
/// Two independent draws, low 64 bits of each.
fn new_install_id() -> String {
    let hi = ulid::Ulid::new().random() as u64;
    let lo = ulid::Ulid::new().random() as u64;
    format!("{hi:016x}{lo:016x}")
}

/// Read the install id, generating and persisting one on first use.
///
/// `None` on any IO failure — a machine where the id cannot be stored gets
/// no telemetry at all, rather than a fresh id per run that would count one
/// install as many.
pub fn load_or_create_install_id(home: &Path) -> Option<String> {
    let path = install_id_path(home);
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let existing = existing.trim();
        if !existing.is_empty() {
            return Some(existing.to_string());
        }
    }
    let id = new_install_id();
    std::fs::create_dir_all(home).ok()?;
    std::fs::write(&path, format!("{id}\n")).ok()?;
    Some(id)
}

// -- the disclosure ----------------------------------------------------------

/// The first-run notice, in full. Names every field that is sent, the whole
/// never-sent list, where the id lives so it can be inspected or deleted,
/// and both opt-out commands verbatim.
pub fn disclosure_text(install_id_path: &Path, repo: &Path) -> String {
    let rule = "─".repeat(72);
    format!(
        "{rule}\n\
         Anonymous usage telemetry is ON. You are seeing this once, on this machine.\n\
         \n\
         Sent when `drums watch` starts, and every 6 hours while it runs:\n\
         \x20 · a random install id — generated here, stored at\n\
         \x20   {id}\n\
         \x20   (delete that file and you become a new install)\n\
         \x20 · the drums version ({version}), and this machine's OS and CPU\n\
         \x20   architecture ({os}/{arch})\n\
         \x20 · four running totals: failures detected, repairs attempted,\n\
         \x20   repairs verified, repairs shipped\n\
         \n\
         NEVER sent — absent from the payload, not redacted out of it:\n\
         \x20 repository names · file paths · branch names · commit shas ·\n\
         \x20 error messages · stack traces · request bodies · URLs ·\n\
         \x20 agent output · failure-class names · anything derived from your code\n\
         \n\
         To send nothing at all, either:\n\
         \x20 export DRUMS_TELEMETRY=off\n\
         or add this line to {config}:\n\
         \x20 telemetry = \"off\"\n\
         {rule}\n",
        rule = rule,
        id = install_id_path.display(),
        version = env!("CARGO_PKG_VERSION"),
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
        config = crate::config::config_path(repo).display(),
    )
}

// -- resolution + the heartbeat ---------------------------------------------

/// Everything `drums watch` needs, resolved once at startup.
pub struct Telemetry {
    decision: Decision,
    counters: Arc<Counters>,
    install_id: Option<String>,
    home: Option<PathBuf>,
    url: String,
}

/// What [`start`] worked out, plus the two things the caller has to print.
/// [`start`] prints nothing itself — `drums watch` owns stdout, and a module
/// that wrote to it directly could interleave with the §7 narration.
pub struct Startup {
    pub telemetry: Telemetry,
    /// The first-run notice, printed by the caller BEFORE any heartbeat is
    /// spawned, then acknowledged with [`Telemetry::mark_disclosed`].
    /// `None` once this machine has seen it, and always `None` when nothing
    /// would be sent — there is nothing to disclose when nothing is sent,
    /// and marking it shown then would rob a later opt-in of its notice.
    pub disclosure: Option<String>,
    /// A named complaint about a switch this version could not read. Goes to
    /// stderr; never fatal.
    pub warning: Option<String>,
}

/// Resolve the opt-out, load or create the install id, and decide whether
/// the first-run notice is owed.
///
/// A `.drums/config.toml` that exists and cannot be PARSED resolves to OFF.
/// That is a deliberate fail-closed: an operator may have written
/// `telemetry = "off"` into a file whose other key has a typo, and treating
/// an unreadable file as though it had said nothing would read their opt-out
/// as consent. `DRUMS_TELEMETRY` still overrides, since that is an explicit
/// choice made on this invocation.
pub fn start(repo: &Path) -> Startup {
    let env = std::env::var("DRUMS_TELEMETRY").ok();
    let (config_value, config_unreadable) = match crate::config::load(repo) {
        Ok(Some(cfg)) => (cfg.telemetry, false),
        Ok(None) => (None, false),
        Err(_) => (Some("off".to_string()), true),
    };

    let decision = decide(env.as_deref(), config_value.as_deref());
    let warning = match &decision {
        Decision::OffByUnrecognised { source, value } => Some(format!(
            "{source} is set to {value:?}, which this version does not recognise — telemetry is OFF for this run. Use \"on\" or \"off\"."
        )),
        _ if config_unreadable => Some(format!(
            "{} could not be parsed; telemetry treats an unreadable config as \"off\" (a file that may say telemetry = \"off\" is never read as consent). DRUMS_TELEMETRY still overrides — telemetry this run: {}.",
            crate::config::config_path(repo).display(),
            decision.label()
        )),
        _ => None,
    };

    let home = home_dir();
    // No id is created for someone who opted out. Writing a persistent
    // identifier for a machine that will never send one is exactly the kind
    // of quiet thing this module exists not to do.
    let install_id = if decision.is_on() {
        home.as_deref().and_then(load_or_create_install_id)
    } else {
        None
    };

    let disclosure = match (&home, install_id.is_some()) {
        (Some(home), true) if !disclosure_marker_path(home).exists() => {
            Some(disclosure_text(&install_id_path(home), repo))
        }
        _ => None,
    };

    let url = std::env::var("DRUMS_TELEMETRY_URL")
        .ok()
        .map(|u| u.trim().to_string())
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| DEFAULT_TELEMETRY_URL.to_string());

    Startup {
        telemetry: Telemetry {
            decision,
            counters: Arc::new(Counters::default()),
            install_id,
            home,
            url,
        },
        disclosure,
        warning,
    }
}

impl Telemetry {
    /// Build one directly, bypassing env/config resolution. This is the seam
    /// the tests use: a process's environment is global and `cargo test` runs
    /// tests in parallel threads, so a test that had to `set_var` to point at
    /// its own local server would be racing every other test.
    pub fn new(decision: Decision, install_id: Option<String>, url: impl Into<String>) -> Self {
        Telemetry {
            decision,
            counters: Arc::new(Counters::default()),
            install_id,
            home: None,
            url: url.into(),
        }
    }

    pub fn decision(&self) -> &Decision {
        &self.decision
    }

    /// The handle the caller folds engine events into. Shared with the
    /// heartbeat task, which only ever reads it.
    pub fn counters(&self) -> Arc<Counters> {
        Arc::clone(&self.counters)
    }

    /// Record that the first-run notice has now actually been printed.
    /// Called by the caller AFTER printing, not by [`start`] before it — a
    /// process that dies between the two must show the notice again rather
    /// than count an unread one as shown. Best-effort: a marker that cannot
    /// be written means the notice repeats, which is the harmless direction.
    pub fn mark_disclosed(&self) {
        let Some(home) = &self.home else { return };
        if std::fs::create_dir_all(home).is_ok() {
            let _ = std::fs::write(disclosure_marker_path(home), "shown\n");
        }
    }

    /// The payload as it stands right now, or `None` when nothing would be
    /// sent. Public so a caller (and the tests) can look at exactly what
    /// would go over the wire.
    pub fn payload(&self) -> Option<Payload> {
        if !self.decision.is_on() {
            return None;
        }
        let install_id = self.install_id.clone()?;
        Some(payload_for(&install_id, &self.counters.totals()))
    }

    /// Start the background heartbeat: one send now, one every
    /// [`HEARTBEAT_INTERVAL`] after that.
    ///
    /// Returns immediately, always. Nothing here is awaited by the caller,
    /// no failure here reaches the caller, and an opted-out install spawns
    /// no task at all — the opt-out is enforced HERE, at the sending seam,
    /// not only at the disclosure.
    pub fn spawn_heartbeat(&self) {
        if !self.decision.is_on() {
            return;
        }
        let Some(install_id) = self.install_id.clone() else {
            return;
        };
        let counters = Arc::clone(&self.counters);
        let url = self.url.clone();
        tokio::spawn(async move {
            loop {
                send_once(&url, &payload_for(&install_id, &counters.totals())).await;
                tokio::time::sleep(HEARTBEAT_INTERVAL).await;
            }
        });
    }
}

/// The one place a [`Payload`] is ever built. Every string in it comes from
/// a constant of this binary or from the install id; there is no parameter
/// through which anything of the customer's could arrive.
fn payload_for(install_id: &str, totals: &Totals) -> Payload {
    Payload {
        install_id: install_id.to_string(),
        drums_version: env!("CARGO_PKG_VERSION").to_string(),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        failures_detected: totals.failures_detected,
        repairs_attempted: totals.repairs_attempted,
        repairs_verified: totals.repairs_verified,
        repairs_shipped: totals.repairs_shipped,
    }
}

/// One POST, then forget it.
///
/// Returns `()` on purpose. There is no error to hand back: the only thing a
/// caller could do with one is narrate it, and a line about telemetry
/// failing, printed in the middle of the §7 narration, would read as the
/// product failing. Failures go to `tracing::debug!`, which this binary's
/// WARN-level subscriber never prints — visible to anyone who turns the
/// level up, invisible in normal operation.
pub async fn send_once(url: &str, payload: &Payload) {
    let client = match reqwest::Client::builder().timeout(SEND_TIMEOUT).build() {
        Ok(client) => client,
        Err(e) => {
            tracing::debug!(error = %e, "telemetry: no http client; skipping");
            return;
        }
    };
    match client.post(url).json(payload).send().await {
        Ok(_) => {}
        Err(e) => tracing::debug!(error = %e, "telemetry: heartbeat not delivered"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telemetry_is_on_when_nothing_says_otherwise() {
        assert_eq!(decide(None, None), Decision::On);
    }

    #[test]
    fn the_env_var_turns_it_off() {
        assert_eq!(decide(Some("off"), None), Decision::OffByEnv);
        assert_eq!(decide(Some("OFF"), None), Decision::OffByEnv);
        assert_eq!(decide(Some(" 0 "), None), Decision::OffByEnv);
        assert_eq!(decide(Some("false"), None), Decision::OffByEnv);
    }

    #[test]
    fn the_config_turns_it_off() {
        assert_eq!(decide(None, Some("off")), Decision::OffByConfig);
    }

    #[test]
    fn the_env_var_wins_over_the_config_in_both_directions() {
        assert_eq!(
            decide(Some("off"), Some("on")),
            Decision::OffByEnv,
            "a variable set on THIS invocation must beat a file written earlier"
        );
        assert_eq!(decide(Some("on"), Some("off")), Decision::On);
    }

    #[test]
    fn an_unrecognised_value_resolves_to_off_and_names_itself() {
        let d = decide(Some("maybe"), None);
        assert!(!d.is_on(), "an unreadable switch must fail closed: {d:?}");
        match d {
            Decision::OffByUnrecognised { source, value } => {
                assert_eq!(source, "DRUMS_TELEMETRY");
                assert_eq!(value, "maybe");
            }
            other => panic!("expected OffByUnrecognised, got {other:?}"),
        }
    }

    #[test]
    fn an_empty_env_var_counts_as_unset_not_as_a_bad_value() {
        // `DRUMS_TELEMETRY=` is how a shell clears an inherited variable.
        assert_eq!(decide(Some(""), None), Decision::On);
        assert_eq!(decide(Some("  "), Some("off")), Decision::OffByConfig);
    }

    #[test]
    fn an_install_id_is_generated_once_and_then_reused() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let first = load_or_create_install_id(&home).expect("should create");
        let second = load_or_create_install_id(&home).expect("should reuse");
        assert_eq!(
            first, second,
            "a stable install must not be counted as a new one every run"
        );
        assert_eq!(first.len(), 32, "128 bits of hex: {first}");
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()), "{first}");
    }

    #[test]
    fn two_installs_never_share_an_id() {
        let dir = tempfile::tempdir().unwrap();
        let a = load_or_create_install_id(&dir.path().join("a")).unwrap();
        let b = load_or_create_install_id(&dir.path().join("b")).unwrap();
        assert_ne!(
            a, b,
            "the id must be random, never derived from the machine"
        );
    }

    #[test]
    fn the_disclosure_names_both_opt_outs_and_the_never_sent_list() {
        let text = disclosure_text(Path::new("/home/x/.drums/install-id"), Path::new("/repo"));
        assert!(text.contains("DRUMS_TELEMETRY=off"), "{text}");
        assert!(text.contains("telemetry = \"off\""), "{text}");
        assert!(text.contains("/repo/.drums/config.toml"), "{text}");
        assert!(text.contains("/home/x/.drums/install-id"), "{text}");
        for never in [
            "repository names",
            "file paths",
            "branch names",
            "stack traces",
            "request bodies",
            "URLs",
            "agent output",
        ] {
            assert!(
                text.contains(never),
                "the notice must name {never:?}:\n{text}"
            );
        }
    }

    #[test]
    fn the_disclosure_marker_suppresses_the_notice_and_nothing_else_does() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!disclosure_marker_path(dir.path()).exists());
        std::fs::write(disclosure_marker_path(dir.path()), "shown\n").unwrap();
        assert!(disclosure_marker_path(dir.path()).exists());
    }

    #[test]
    fn an_opted_out_install_has_no_payload_to_send() {
        let t = Telemetry::new(
            Decision::OffByEnv,
            Some("id".to_string()),
            "http://127.0.0.1:1/",
        );
        assert!(
            t.payload().is_none(),
            "opt-out is enforced where the payload is built, not only where it is printed"
        );
    }

    #[test]
    fn an_install_with_nowhere_to_store_an_id_sends_nothing() {
        let t = Telemetry::new(Decision::On, None, "http://127.0.0.1:1/");
        assert!(t.payload().is_none());
    }
}
