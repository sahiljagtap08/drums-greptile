//! `<repo>/.drums/config.toml` — a service can't be started with a fresh set
//! of flags typed by a human every time; it needs a file. Every field mirrors
//! a `drums watch`/`drums daemon start` flag 1:1, so the two configuration
//! surfaces never drift into two different vocabularies for the same thing.
//! CLI flags always win over the file (`resolve` below); the file always
//! wins over this module's own hardcoded fallbacks.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One config field per `drums watch` flag that has a service-shaped
/// equivalent. Every field is optional: a config only has to say what it
/// wants to override, and an absent field falls through to whatever
/// `resolve` was given as a fallback (a CLI flag, then a hardcoded default).
///
/// `deny_unknown_fields` is deliberate: a typo'd key (`theshold`) silently
/// being ignored would leave an operator believing they configured
/// something they didn't — the same "never silently do less than what was
/// typed" discipline `main.rs`'s `--repair auto` validation already applies
/// to flags, extended to the file.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub ingest_port: Option<u16>,
    pub threshold: Option<usize>,
    pub window_secs: Option<u64>,
    pub app_root: Option<String>,
    /// The repair-agent command template (e.g. `claude -p {prompt}
    /// --permission-mode acceptEdits`). `None` means "let
    /// `CliRepairAgent::detect()` pick" — the same DRUMS_AGENT_CMD-then-
    /// claude-then-codex-then-none fallback `drums watch` already uses.
    pub agent_cmd: Option<String>,
    pub boot_timeout_ms: Option<u64>,
    /// `"propose"` or `"auto"` — validated at resolve time, not parse time
    /// (see [`resolve`]), so a bad value produces the same named, actionable
    /// refusal a bad `--repair` flag value would from clap.
    pub repair_mode: Option<String>,
    pub deploy_cmd: Option<String>,
    pub check_url: Option<String>,
    /// PostHog, for reading the customer's own product analytics: the app
    /// host (`https://us.posthog.com`, `https://eu.posthog.com`, or a
    /// self-hosted origin) and the numeric project id. The API key is NEVER
    /// configured here — a secret in a committed file is a leak with a delay —
    /// it comes from `DRUMS_POSTHOG_API_KEY` in the environment.
    pub posthog_host: Option<String>,
    pub posthog_project: Option<String>,
    /// A full Slack incoming-webhook URL for proactive notifications
    /// (Decisions, Learnings, Working, FYI — see `crate::notify`). A delivery
    /// address, not a secret of the same class as an API key — config-file
    /// placement is acceptable; rotate it in Slack if the file leaks. The
    /// env override `DRUMS_SLACK_WEBHOOK_URL` takes precedence when both are
    /// set. Absent means notifications are off — a normal state, never an
    /// error.
    pub slack_webhook_url: Option<String>,
    /// Consent gate for proactive drafting: when `true`, a NEW rate-shift
    /// observation on the watch tick lets Drums run the configured coding
    /// agent (`agent_cmd` resolution) to DRAFT a Product Bet for human
    /// confirmation. Default `false`, deliberately — drafting spends the
    /// customer's own agent tokens, so it must be asked for, never assumed.
    /// Nothing is ever committed to without `drums bet confirm`.
    pub proactive_draft: Option<bool>,
    /// Consent gate for record sync: when `true`, `drums watch`/`drumsd` push
    /// this repo's `.drums/record.jsonl` lines to the hosted plane
    /// (app.drums.sh) so the console can render the Bet Feed for the team,
    /// and `drums sync` runs one push on demand. Default `false`,
    /// deliberately — the record is local-first, and the trust terms are
    /// exactly two: your record leaves this machine only when you set this;
    /// it arrives redacted because it is stored redacted (capture-time
    /// redaction, `engine_record::redact_body` — sync sends the stored bytes
    /// verbatim, pinned by test in [`crate::sync`]). Requires `drums login`.
    /// NOT part of [`ResolvedSettings`]: like `telemetry`, it is a consent
    /// switch read where the send could happen, not a daemon setting merged
    /// with flags.
    pub sync_record: Option<bool>,
    /// `"on"` (default) or `"off"` — anonymous usage telemetry. NOT part of
    /// [`ResolvedSettings`], and deliberately so: it is not a daemon setting
    /// merged with flags, it is a consent switch read directly by
    /// [`crate::telemetry::start`] wherever a send could happen.
    /// `DRUMS_TELEMETRY` overrides it. A value this version cannot read
    /// resolves to OFF with a named warning rather than to the default —
    /// see [`crate::telemetry::Decision::OffByUnrecognised`].
    pub telemetry: Option<String>,
}

pub fn config_path(repo: &Path) -> PathBuf {
    repo.join(".drums").join("config.toml")
}

/// Loads `<repo>/.drums/config.toml`. `Ok(None)` — not `Err` — when the file
/// simply doesn't exist: a missing config is a normal, nameable state
/// (`drums init` writes one; see [`write_default`]), not an IO failure.
pub fn load(repo: &Path) -> Result<Option<Config>, String> {
    let path = config_path(repo);
    match std::fs::read_to_string(&path) {
        Ok(s) => toml::from_str(&s)
            .map(Some)
            .map_err(|e| format!("could not parse {}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("could not read {}: {e}", path.display())),
    }
}

/// The refusal `drums daemon start` (or any future config-driven command)
/// prints when nothing configured it at all: no config file, no flags
/// either. Named here, not inlined at each call site, so the message and the
/// one command that resolves it stay in exactly one place.
pub fn missing_config_error(repo: &Path) -> String {
    let path = config_path(repo);
    format!(
        "no config at {} and no flags were given — run `drums init --repo {}` to write a starter config, or pass flags directly",
        path.display(),
        repo.display()
    )
}

/// The default config `drums init` (and, until it exists, this module's own
/// [`write_default`]) writes: propose mode, the same threshold/window
/// defaults `drums watch`'s flags already default to, and nothing
/// agent/deploy-specific assumed — those need a deliberate, informed choice
/// (an agent to run, a place to deploy to), not a guessed default.
pub fn default_toml() -> &'static str {
    r#"# drums config — see `drums daemon start --help` for what each key does.
# Command-line flags always override a value set here.

ingest_port = 7787
threshold = 3
window_secs = 60
repair_mode = "propose"

# Anonymous usage telemetry. What is sent: a random install id (generated on
# this machine, stored at ~/.drums/install-id, delete it and you become a new
# install), the drums version, this machine's OS and CPU architecture, and
# four running totals — failures detected, repairs attempted, repairs
# verified, repairs shipped. What is NEVER sent: repository names, file paths,
# branch names, commit shas, error messages, stack traces, request bodies,
# URLs, agent output, or anything else derived from your code. Set to "off"
# to send nothing (or export DRUMS_TELEMETRY=off, which overrides this).
telemetry = "on"

# Uncomment and set to enable a specific repair agent / auto-shipping:
# agent_cmd = "claude -p {prompt} --permission-mode acceptEdits"
# deploy_cmd = "bash deploy.sh {sha}"
# check_url = "http://localhost:3000/health"

# Slack notifications: a full incoming-webhook URL. This is a delivery
# address, not a secret of the same class as an API key, so it may live here;
# rotate it in Slack if this file ever leaks. The env override
# DRUMS_SLACK_WEBHOOK_URL takes precedence when both are set.
# slack_webhook_url = "https://hooks.slack.com/services/T000/B000/XXXXXXXX"

# Proactive drafting: when true, a new rate-shift observation lets Drums run
# your own coding agent (agent_cmd resolution) to DRAFT a Product Bet for
# your confirmation. Off by default because it spends your agent's tokens —
# turning it on is the consent. Nothing is committed to without
# `drums bet confirm`.
# proactive_draft = true

# Record sync: when true, the daemon pushes this repo's record lines to the
# hosted plane (app.drums.sh) so the console can show the Bet Feed to your
# team, and `drums sync` pushes on demand. The trust terms: your record
# leaves this machine only when you set this, and it
# arrives redacted because it is stored redacted — redaction happens at
# capture time, and sync sends the stored lines byte-for-byte, adding
# nothing. Requires `drums login`.
# sync_record = true
"#
}

/// Writes the default config at `<repo>/.drums/config.toml`. Refuses to
/// clobber a config that already exists — this is a scaffold for a repo that
/// has none, not an overwrite of one an operator may have already edited.
///
/// Overlap note (spec coordination): `drums init` is the documented command
/// that's supposed to own writing this file. As of this change `drums init`
/// does not exist yet, so this function is also wired up as a minimal `drums
/// init` (see `main.rs`) — when a real `drums init` lands with a broader
/// scaffold, this function is exactly the piece it should call for the
/// config half, not a second implementation to keep in sync.
pub fn write_default(repo: &Path) -> Result<PathBuf, String> {
    let dir = repo.join(".drums");
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    let path = config_path(repo);
    if path.exists() {
        return Err(format!(
            "{} already exists — not overwriting",
            path.display()
        ));
    }
    std::fs::write(&path, default_toml())
        .map_err(|e| format!("could not write {}: {e}", path.display()))?;
    Ok(path)
}

/// One field's resolution: whatever the flag says, else whatever the config
/// says, else `default`. Flags always win — a human who typed a flag on this
/// exact invocation is making an explicit, in-the-moment choice that a file
/// written earlier must never silently override.
fn pick<T: Clone>(flag: &Option<T>, cfg: &Option<T>, default: T) -> T {
    flag.clone().or_else(|| cfg.clone()).unwrap_or(default)
}

fn pick_opt<T: Clone>(flag: &Option<T>, cfg: &Option<T>) -> Option<T> {
    flag.clone().or_else(|| cfg.clone())
}

/// The flags `drums daemon start` accepts — every field optional (unlike
/// `drums watch`'s clap struct, which bakes defaults straight into the
/// flags) precisely so [`resolve`] can tell "the operator typed this" apart
/// from "nothing was typed, fall through to the config/default".
#[derive(Debug, Clone, Default)]
pub struct DaemonFlags {
    pub ingest_port: Option<u16>,
    pub threshold: Option<usize>,
    pub window_secs: Option<u64>,
    pub app_root: Option<String>,
    pub agent_cmd: Option<String>,
    pub boot_timeout_ms: Option<u64>,
    pub repair_mode: Option<String>,
    pub deploy_cmd: Option<String>,
    pub check_url: Option<String>,
}

impl DaemonFlags {
    /// Whether the operator typed ANY of these flags — the other half of the
    /// "missing config" refusal condition in [`resolve`]: a config-less repo
    /// is only an error when NOTHING configured it, flags included.
    pub fn any_set(&self) -> bool {
        self.ingest_port.is_some()
            || self.threshold.is_some()
            || self.window_secs.is_some()
            || self.app_root.is_some()
            || self.agent_cmd.is_some()
            || self.boot_timeout_ms.is_some()
            || self.repair_mode.is_some()
            || self.deploy_cmd.is_some()
            || self.check_url.is_some()
    }
}

/// Fully resolved daemon settings — the merged, defaulted output of `flags`
/// (highest priority), `config` (middle), and this module's hardcoded
/// fallbacks (lowest). Every field here is concrete: by the time this
/// struct exists, "was it configured" is a settled question.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedSettings {
    pub ingest_port: u16,
    pub threshold: usize,
    pub window_secs: u64,
    pub app_root: Option<String>,
    pub agent_cmd: Option<String>,
    pub boot_timeout_ms: u64,
    pub repair_mode_auto: bool,
    pub deploy_cmd: Option<String>,
    pub check_url: Option<String>,
}

/// `ingest_port`/`threshold`/`window_secs` mirror `drums watch`'s own flag
/// defaults exactly (7787/3/60s) — the two entry points must agree on "what
/// happens if you say nothing at all".
const DEFAULT_INGEST_PORT: u16 = 7787;
const DEFAULT_THRESHOLD: usize = 3;
const DEFAULT_WINDOW_SECS: u64 = 60;
const DEFAULT_BOOT_TIMEOUT_MS: u64 = 15_000;

/// Merge `flags` over `config` over hardcoded defaults into one concrete
/// [`ResolvedSettings`], honestly refusing when NEITHER configured anything
/// at all (see [`missing_config_error`]) and when `repair_mode` resolves to
/// something other than `propose`/`auto`, or to `auto` without a
/// `deploy_cmd` — the same "auto without deploy-cmd is refused, not silently
/// downgraded" rule `main.rs`'s `validate_watch_repair_flags` already
/// enforces for `drums watch`, restated here because config-driven `auto`
/// can arrive from the file instead of a flag.
pub fn resolve(
    repo: &Path,
    flags: &DaemonFlags,
    config: &Option<Config>,
) -> Result<ResolvedSettings, String> {
    if config.is_none() && !flags.any_set() {
        return Err(missing_config_error(repo));
    }
    let empty = Config::default();
    let cfg = config.as_ref().unwrap_or(&empty);

    let ingest_port = pick(&flags.ingest_port, &cfg.ingest_port, DEFAULT_INGEST_PORT);
    let threshold = pick(&flags.threshold, &cfg.threshold, DEFAULT_THRESHOLD);
    let window_secs = pick(&flags.window_secs, &cfg.window_secs, DEFAULT_WINDOW_SECS);
    let boot_timeout_ms = pick(
        &flags.boot_timeout_ms,
        &cfg.boot_timeout_ms,
        DEFAULT_BOOT_TIMEOUT_MS,
    );
    let app_root = pick_opt(&flags.app_root, &cfg.app_root);
    let agent_cmd = pick_opt(&flags.agent_cmd, &cfg.agent_cmd);
    let deploy_cmd = pick_opt(&flags.deploy_cmd, &cfg.deploy_cmd);
    let check_url = pick_opt(&flags.check_url, &cfg.check_url);
    let repair_mode_str = pick(&flags.repair_mode, &cfg.repair_mode, "propose".to_string());

    let repair_mode_auto = match repair_mode_str.as_str() {
        "propose" => false,
        "auto" => true,
        other => {
            return Err(format!(
                "invalid repair_mode {other:?}: expected \"propose\" or \"auto\""
            ))
        }
    };
    if repair_mode_auto && deploy_cmd.is_none() {
        return Err("repair_mode \"auto\" requires deploy_cmd (set it in config.toml or pass --deploy-cmd — refusing to start rather than silently behaving like propose)".to_string());
    }

    Ok(ResolvedSettings {
        ingest_port,
        threshold,
        window_secs,
        app_root,
        agent_cmd,
        boot_timeout_ms,
        repair_mode_auto,
        deploy_cmd,
        check_url,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_on_missing_file_is_ok_none_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path()).unwrap(), None);
    }

    #[test]
    fn write_default_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_default(dir.path()).unwrap();
        assert!(path.ends_with(".drums/config.toml"));
        let cfg = load(dir.path()).unwrap().expect("just wrote it");
        assert_eq!(cfg.ingest_port, Some(7787));
        assert_eq!(cfg.threshold, Some(3));
        assert_eq!(cfg.repair_mode.as_deref(), Some("propose"));
        assert_eq!(
            cfg.telemetry.as_deref(),
            Some("on"),
            "the starter config states the telemetry switch out loud rather than leaving it implicit — the file itself is a disclosure surface"
        );
    }

    /// The opt-out has to survive `deny_unknown_fields`: someone who writes
    /// `telemetry = "off"` and gets a parse error would reasonably conclude
    /// there is no opt-out at all.
    #[test]
    fn telemetry_off_parses_from_a_hand_written_config() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".drums")).unwrap();
        std::fs::write(
            dir.path().join(".drums/config.toml"),
            "telemetry = \"off\"\n",
        )
        .unwrap();
        let cfg = load(dir.path()).unwrap().expect("just wrote it");
        assert_eq!(cfg.telemetry.as_deref(), Some("off"));
    }

    /// Same rule as `telemetry = "off"` above: the notification and drafting
    /// keys have to survive `deny_unknown_fields`, or an operator who sets
    /// them gets a parse refusal and reasonably concludes they don't exist.
    #[test]
    fn slack_webhook_url_and_proactive_draft_parse_from_a_hand_written_config() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".drums")).unwrap();
        std::fs::write(
            dir.path().join(".drums/config.toml"),
            "slack_webhook_url = \"https://hooks.slack.com/services/T0/B0/x\"\nproactive_draft = true\n",
        )
        .unwrap();
        let cfg = load(dir.path()).unwrap().expect("just wrote it");
        assert_eq!(
            cfg.slack_webhook_url.as_deref(),
            Some("https://hooks.slack.com/services/T0/B0/x")
        );
        assert_eq!(cfg.proactive_draft, Some(true));
    }

    /// The same `deny_unknown_fields` rule again for the sync consent key: an
    /// operator who writes `sync_record = true` and gets a parse refusal
    /// would reasonably conclude record sync does not exist.
    #[test]
    fn sync_record_parses_from_a_hand_written_config() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".drums")).unwrap();
        std::fs::write(
            dir.path().join(".drums/config.toml"),
            "sync_record = true\n",
        )
        .unwrap();
        let cfg = load(dir.path()).unwrap().expect("just wrote it");
        assert_eq!(cfg.sync_record, Some(true));
    }

    /// The starter config is the consent surface for record sync, so it has
    /// to state the trust terms where the operator will actually read them:
    /// the record leaves the machine only on an explicit `true`, and what
    /// leaves is the redacted record because that is what is stored. The key
    /// stays commented out — local-first is the default, never a guess.
    #[test]
    fn the_default_config_documents_sync_record_with_the_trust_terms() {
        let toml = default_toml();
        assert!(toml.contains("sync_record"), "{toml}");
        assert!(
            toml.contains("leaves this machine only when you set this"),
            "the first trust term must be stated in the file itself: {toml}"
        );
        assert!(
            toml.contains("arrives redacted because it is stored redacted"),
            "the second trust term must be stated in the file itself: {toml}"
        );
        assert!(
            toml.contains("drums login"),
            "the credential prerequisite is named: {toml}"
        );
        let cfg = load_default(toml);
        assert_eq!(
            cfg.sync_record, None,
            "sync stays off unless somebody says otherwise"
        );
    }

    /// The starter config is where these keys get discovered — commented out
    /// (both need a deliberate choice), but named, with the env override and
    /// the consent framing stated where the operator will actually read them.
    #[test]
    fn the_default_config_documents_the_notification_and_drafting_keys() {
        let toml = default_toml();
        assert!(toml.contains("slack_webhook_url"), "{toml}");
        assert!(
            toml.contains("DRUMS_SLACK_WEBHOOK_URL"),
            "the env override must be disclosed: {toml}"
        );
        assert!(toml.contains("proactive_draft"), "{toml}");
        assert!(
            toml.contains("agent's tokens"),
            "the cost is the reason for the consent gate, and it is said: {toml}"
        );
        // Both stay commented out: neither has a sane guessed default.
        let cfg = load_default(toml);
        assert_eq!(cfg.slack_webhook_url, None);
        assert_eq!(cfg.proactive_draft, None);
    }

    fn load_default(toml_text: &str) -> Config {
        toml::from_str(toml_text).expect("the default config must always parse")
    }

    #[test]
    fn write_default_refuses_to_clobber_an_existing_config() {
        let dir = tempfile::tempdir().unwrap();
        write_default(dir.path()).unwrap();
        let err = write_default(dir.path()).expect_err("must refuse the second write");
        assert!(err.contains("already exists"), "{err}");
    }

    #[test]
    fn load_names_the_file_and_the_parse_error_on_malformed_toml() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".drums")).unwrap();
        std::fs::write(dir.path().join(".drums/config.toml"), "not = [valid toml").unwrap();
        let err = load(dir.path()).expect_err("malformed toml must be a named refusal");
        assert!(err.contains("config.toml"), "{err}");
    }

    #[test]
    fn load_rejects_an_unknown_key_rather_than_silently_ignoring_a_typo() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".drums")).unwrap();
        std::fs::write(dir.path().join(".drums/config.toml"), "theshold = 3\n").unwrap();
        let err = load(dir.path()).expect_err("a typo'd key must be refused, not silently dropped");
        assert!(err.contains("config.toml"), "{err}");
    }

    #[test]
    fn resolve_refuses_honestly_when_neither_config_nor_flags_configured_anything() {
        let dir = tempfile::tempdir().unwrap();
        let err = resolve(dir.path(), &DaemonFlags::default(), &None).expect_err("must refuse");
        assert!(err.contains("config.toml"), "{err}");
        assert!(
            err.contains("drums init"),
            "the refusal must name the one command that fixes it: {err}"
        );
    }

    #[test]
    fn resolve_works_from_config_alone_with_no_flags() {
        let dir = tempfile::tempdir().unwrap();
        write_default(dir.path()).unwrap();
        let cfg = load(dir.path()).unwrap();
        let settings = resolve(dir.path(), &DaemonFlags::default(), &cfg).unwrap();
        assert_eq!(settings.ingest_port, 7787);
        assert_eq!(settings.threshold, 3);
        assert_eq!(settings.window_secs, 60);
        assert!(!settings.repair_mode_auto);
    }

    #[test]
    fn resolve_works_from_flags_alone_with_no_config() {
        let dir = tempfile::tempdir().unwrap();
        let flags = DaemonFlags {
            threshold: Some(5),
            ..Default::default()
        };
        let settings = resolve(dir.path(), &flags, &None).unwrap();
        assert_eq!(settings.threshold, 5);
        assert_eq!(
            settings.ingest_port, DEFAULT_INGEST_PORT,
            "unconfigured fields still fall back to the hardcoded default"
        );
    }

    #[test]
    fn a_flag_overrides_the_same_field_set_in_config() {
        let dir = tempfile::tempdir().unwrap();
        write_default(dir.path()).unwrap(); // threshold = 3
        let cfg = load(dir.path()).unwrap();
        let flags = DaemonFlags {
            threshold: Some(9),
            ..Default::default()
        };
        let settings = resolve(dir.path(), &flags, &cfg).unwrap();
        assert_eq!(
            settings.threshold, 9,
            "the flag typed on this invocation must win over the file"
        );
    }

    #[test]
    fn resolve_refuses_auto_without_a_deploy_cmd_from_either_source() {
        let dir = tempfile::tempdir().unwrap();
        let flags = DaemonFlags {
            repair_mode: Some("auto".into()),
            ..Default::default()
        };
        let err = resolve(dir.path(), &flags, &None).expect_err("must refuse");
        assert!(
            err.contains("deploy_cmd") || err.contains("deploy-cmd"),
            "{err}"
        );
    }

    #[test]
    fn resolve_accepts_auto_when_deploy_cmd_comes_from_config_and_mode_from_a_flag() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Some(Config {
            deploy_cmd: Some("bash deploy.sh {sha}".into()),
            ..Default::default()
        });
        let flags = DaemonFlags {
            repair_mode: Some("auto".into()),
            ..Default::default()
        };
        let settings = resolve(dir.path(), &flags, &cfg).unwrap();
        assert!(settings.repair_mode_auto);
        assert_eq!(settings.deploy_cmd.as_deref(), Some("bash deploy.sh {sha}"));
    }

    #[test]
    fn resolve_rejects_an_invalid_repair_mode_value() {
        let dir = tempfile::tempdir().unwrap();
        let flags = DaemonFlags {
            repair_mode: Some("yolo".into()),
            ..Default::default()
        };
        let err =
            resolve(dir.path(), &flags, &None).expect_err("must refuse an unknown repair_mode");
        assert!(err.contains("yolo"), "{err}");
    }
}
