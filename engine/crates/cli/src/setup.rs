//! `drums init` — the whole setup, one command, one terminal.
//!
//! ## What this replaces
//!
//! Getting to a working install used to be seven steps across two terminals:
//! install, `init`, `init --wire --yes`, `watch` in one window, `init --verify`
//! in another, then set an environment variable and add a deploy hook by hand.
//! Every one of those was defensible on its own and the sum was indefensible —
//! a person following along in a demo gets lost at "now open a second
//! terminal".
//!
//! ## The one interesting problem
//!
//! Verification needs something listening. The old flow solved that by making
//! you run `drums watch` in another window, which is where the second terminal
//! came from. Instead this starts its OWN ingest listener on an ephemeral port,
//! fires a test event through the real path, reads the record back, and shuts
//! it down.
//!
//! That is not a shortcut around the check — it is the same check. The event
//! travels real HTTP into the real `engine-ingest` router and lands in the real
//! record file. What it removes is the second window, not the evidence.
//!
//! ## What it still refuses to do silently
//!
//! It prints the plan and waits, exactly as `--wire` did. One prompt instead of
//! four, and `--yes` skips it for scripts. The two things it CANNOT do — set an
//! environment variable in someone's deployed app, and add a hook to their
//! deploy pipeline — are printed as the last thing on screen rather than
//! buried, because an install that looks finished but reports nothing is worse
//! than one that says what is missing.

use std::path::{Path, PathBuf};
use std::time::Duration;

use engine_core::Provenance;
use engine_repair::RepairAgent;

use crate::wire::{self, ChangeKind, Framework};

/// What `init` found before it changed anything, with how it knows.
pub struct Findings {
    pub repo: PathBuf,
    pub is_git: bool,
    pub framework: Option<Framework>,
    pub detection_claim: Option<String>,
    pub detection_provenance: Option<Provenance>,
    pub entrypoint: Option<PathBuf>,
    pub agent: Option<String>,
    pub config_exists: bool,
}

pub fn inspect(repo: &Path) -> Findings {
    let detected = wire::detect(repo).ok();
    Findings {
        repo: repo.to_path_buf(),
        is_git: repo.join(".git").exists(),
        framework: detected.as_ref().map(|d| d.framework),
        detection_claim: detected.as_ref().map(|d| d.claim.text.clone()),
        detection_provenance: detected.as_ref().map(|d| d.claim.provenance),
        entrypoint: detected.as_ref().and_then(|d| d.entrypoint.clone()),
        agent: engine_repair::CliRepairAgent::detect().map(|a| a.name().to_string()),
        config_exists: crate::config::config_path(repo).exists(),
    }
}

/// What to call this service when nobody said.
///
/// `package.json`'s `name` first, because that is the name the team already
/// chose and the one that already appears in their logs. The directory name is
/// a fallback and is often wrong — a checkout service worked on in
/// `/tmp/tmp.zD1DjDq0Sj` would otherwise be called that.
pub fn default_service_name(repo: &Path) -> String {
    if let Ok(text) = std::fs::read_to_string(repo.join("package.json")) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(name) = v.get("name").and_then(|n| n.as_str()) {
                // Scoped packages read better without the scope: a service
                // called `@acme/payments` is ugly in every log line.
                let name = name.trim().rsplit('/').next().unwrap_or("").trim();
                if !name.is_empty() {
                    return name.to_string();
                }
            }
        }
    }
    repo.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "app".to_string())
}

/// The port the generated config and the printed instructions agree on.
pub const DEFAULT_INGEST_PORT: u16 = 7787;

/// Run the round trip against a listener this function owns.
///
/// Binds port 0 so it cannot collide with a `drums watch` the person already
/// has running, or with anything else on the machine — the old `--verify` flow
/// assumed 7787 was free and theirs.
pub async fn verify_round_trip_locally(record_path: &Path) -> (String, Provenance) {
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", 0)).await {
        Ok(l) => l,
        Err(e) => {
            return (
                format!("could not start a listener to verify against: {e}"),
                Provenance::Unresolved,
            )
        }
    };
    let port = match listener.local_addr() {
        Ok(a) => a.port(),
        Err(e) => {
            return (
                format!("could not read the listener's port: {e}"),
                Provenance::Unresolved,
            )
        }
    };

    // The REAL ingest router, writing the REAL record. The only thing
    // synthetic here is that the event came from us rather than from the app.
    let (state, mut rx) = engine_ingest::IngestState::new(record_path.to_path_buf());
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, engine_ingest::router(state)).await;
    });
    // Drain, or the channel fills and ingest blocks on a full buffer.
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });

    let marker = format!("drums-init-verify-{}", ulid::Ulid::new());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    let outcome = wire::verify_round_trip(
        &format!("http://127.0.0.1:{port}"),
        record_path,
        &marker,
        // The command actually run. `drums init --verify` goes through
        // `main.rs::init_wire` and names itself there.
        "drums init",
        now,
    )
    .await;

    server.abort();
    drain.abort();
    (outcome.claim.text, outcome.claim.provenance)
}

/// Everything `init` will do, computed before it does any of it.
pub struct Plan {
    pub findings: Findings,
    pub wire_plan: Option<wire::Plan>,
    pub write_config: bool,
    pub service: String,
}

pub fn plan(repo: &Path, service: Option<String>) -> Plan {
    let findings = inspect(repo);
    let service = service.unwrap_or_else(|| default_service_name(repo));
    let wire_plan = wire::plan(repo, &service).ok();
    Plan {
        write_config: !findings.config_exists,
        findings,
        wire_plan,
        service,
    }
}

impl Plan {
    /// True when there is genuinely nothing left to do.
    pub fn is_noop(&self) -> bool {
        !self.write_config
            && self
                .wire_plan
                .as_ref()
                .map(|p| p.changes.is_empty())
                .unwrap_or(true)
    }

    /// Files this will create or edit, for the confirmation prompt.
    pub fn changes(&self) -> Vec<(&'static str, PathBuf, &'static str)> {
        let mut out = Vec::new();
        if self.write_config {
            out.push((
                "new",
                crate::config::config_path(&self.findings.repo),
                "what `drums daemon start` reads",
            ));
        }
        for c in self.wire_plan.iter().flat_map(|p| p.changes.iter()) {
            match c.kind {
                ChangeKind::NewFile => {
                    // Two different new files with two different jobs; calling
                    // the CI workflow "the reporting middleware" was a lie of
                    // laziness a founder spotted on a live install.
                    let why = if c.path.ends_with("drums-repair.yml") {
                        "the CI runner — inert until team mode"
                    } else {
                        "the reporting middleware"
                    };
                    out.push(("new", c.path.clone(), why))
                }
                ChangeKind::Edit => out.push((
                    "edit",
                    c.path.clone(),
                    "2 lines appended — mounts the reporter",
                )),
            }
        }
        out
    }
}

/// The deploy hook a person has to add themselves.
///
/// Printed rather than written: a deploy pipeline is the one place where a
/// wrong edit is expensive and where Drums genuinely cannot know the shape —
/// it might be a Dockerfile, a GitHub Action, a Railway build command, or a
/// shell script nobody has opened in a year.
pub fn deploy_hook_snippet(port: u16) -> String {
    format!(
        r#"curl -fsS -X POST http://127.0.0.1:{port}/v1/deploys \
  -H 'content-type: application/json' \
  -d "{{\"sha\":\"$(git rev-parse HEAD)\",\"description\":\"$(git log -1 --pretty=%s)\",\"author\":\"$(git log -1 --pretty=%an)\",\"deployed_at_ms\":$(date +%s)000}}" || true"#
    )
}

/// How long to wait for the verification before giving up and saying so.
pub const VERIFY_TIMEOUT: Duration = Duration::from_secs(15);

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_with(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        for (name, body) in files {
            let p = dir.path().join(name);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(p, body).unwrap();
        }
        dir
    }

    #[test]
    fn a_plan_writes_nothing() {
        let d = repo_with(&[
            ("package.json", r#"{"dependencies":{"express":"^4"}}"#),
            ("server.js", "const app = require('express')();\n"),
        ]);
        let before = std::fs::read_to_string(d.path().join("server.js")).unwrap();
        let p = plan(d.path(), None);
        assert!(!p.is_noop());
        assert!(!p.changes().is_empty());
        assert_eq!(
            std::fs::read_to_string(d.path().join("server.js")).unwrap(),
            before,
            "computing a plan must not touch a byte"
        );
        assert!(!crate::config::config_path(d.path()).exists());
    }

    #[test]
    fn an_already_set_up_repo_is_a_noop() {
        let d = repo_with(&[
            ("package.json", r#"{"dependencies":{"express":"^4"}}"#),
            ("server.js", "require('./drums-report');\n"),
            ("drums-report.js", "// already here\n"),
            (".github/workflows/drums-repair.yml", "# committed\n"),
            (".drums/config.toml", "threshold = 3\n"),
        ]);
        assert!(
            plan(d.path(), None).is_noop(),
            "a second run must find nothing to do"
        );
    }

    /// A repo set up before the CI workflow existed is not fully a no-op: the
    /// one thing offered on re-run is the workflow, as a diff like everything
    /// else. That is the upgrade path — an old install learns team mode by
    /// running init again, not by hunting the docs for a file.
    #[test]
    fn an_older_install_is_offered_exactly_the_workflow() {
        let d = repo_with(&[
            ("package.json", r#"{"dependencies":{"express":"^4"}}"#),
            ("server.js", "require('./drums-report');\n"),
            ("drums-report.js", "// already here\n"),
            (".drums/config.toml", "threshold = 3\n"),
        ]);
        let p = plan(d.path(), None);
        assert!(!p.is_noop());
        let wire_changes = p
            .wire_plan
            .as_ref()
            .map(|w| w.changes.as_slice())
            .unwrap_or(&[]);
        assert_eq!(wire_changes.len(), 1, "{wire_changes:?}");
        assert!(wire_changes[0].path.ends_with("drums-repair.yml"));
    }

    #[test]
    fn the_service_defaults_to_the_directory_name() {
        let d = repo_with(&[
            ("package.json", r#"{"dependencies":{"express":"^4"}}"#),
            ("server.js", "x"),
        ]);
        let p = plan(d.path(), None);
        assert_eq!(
            p.service,
            d.path().file_name().unwrap().to_string_lossy(),
            "a person should not have to name their service to get started"
        );
    }

    #[test]
    fn inspect_reports_what_it_found_and_how_it_knows() {
        let d = repo_with(&[
            ("package.json", r#"{"dependencies":{"express":"^4"}}"#),
            ("server.js", "x"),
        ]);
        let f = inspect(d.path());
        assert!(f.is_git);
        assert_eq!(f.framework, Some(Framework::Express));
        assert_eq!(
            f.detection_provenance,
            Some(Provenance::Observed),
            "a manifest is a statement the team wrote, not a guess"
        );
        assert!(f.entrypoint.is_some());
    }

    /// The verification must use the real ingest path and the real record.
    #[tokio::test]
    async fn the_round_trip_verifies_against_a_listener_it_owns() {
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("record.jsonl");
        let (text, provenance) = verify_round_trip_locally(&record).await;
        assert_eq!(
            provenance,
            Provenance::Verified,
            "an event should travel the real path and land in the real record: {text}"
        );
        let body = std::fs::read_to_string(&record).expect("the record must exist");
        assert!(
            body.contains("drums-init-verify-"),
            "the marker must be in the record"
        );
    }

    /// The recorded self-test must say which command sent it. Plain
    /// `drums init` writing an intake source of "drums init --verify" was a
    /// record line claiming a flag nobody typed.
    #[tokio::test]
    async fn the_self_test_event_names_the_invocation_that_sent_it() {
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("record.jsonl");
        let (text, provenance) = verify_round_trip_locally(&record).await;
        assert_eq!(provenance, Provenance::Verified, "{text}");

        let read = engine_record::read_all(&record).unwrap();
        let (_, ev) = read
            .lines
            .iter()
            .find(|(kind, _)| kind == "event")
            .expect("the self-test event must be in the record");
        assert_eq!(
            ev.pointer("/intake/source").and_then(|s| s.as_str()),
            Some("drums init"),
            "the intake source must be the command actually run, not `--verify`: {ev}"
        );
    }

    /// Binding port 0 rather than 7787 means setup cannot collide with a
    /// `drums watch` the person already has running.
    #[tokio::test]
    async fn verification_does_not_need_a_specific_port() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = tokio::net::TcpListener::bind(("127.0.0.1", DEFAULT_INGEST_PORT)).await;
        let (_, provenance) = verify_round_trip_locally(&dir.path().join("r.jsonl")).await;
        drop(blocker);
        assert_eq!(
            provenance,
            Provenance::Verified,
            "7787 being busy must not matter"
        );
    }

    #[test]
    fn the_deploy_hook_never_fails_a_deploy() {
        let s = deploy_hook_snippet(7787);
        assert!(
            s.trim_end().ends_with("|| true"),
            "a deploy must not break because Drums was unreachable: {s}"
        );
        assert!(s.contains("git rev-parse HEAD"), "{s}");
    }
}
