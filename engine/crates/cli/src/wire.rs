//! `drums init --wire` — install the reporting snippet, showing the diff
//! first, and prove it works by making a real event travel the real path.
//!
//! ## The correction this implements
//!
//! An earlier plan had an agent inject middleware into someone's app and
//! report success. `docs/CONTRACTS.md` rejected it, in these words: "silently
//! inject and report success" is precisely the behaviour this product exists
//! to argue against. So:
//!
//! 1. **Detect, then say what it found and how it knows.** A guess is labelled
//!    a guess.
//! 2. **Show the diff before touching anything.** Every edit to a file the
//!    team wrote is printed in full first. Without `--yes`, nothing is
//!    written at all and the diff is the entire output — a plan they can read
//!    and apply themselves.
//! 3. **Prove it with a round trip.** `--verify` posts a synthetic error to
//!    the ingest URL the snippet is configured with, then reads the record
//!    back and looks for it. A `verified` claim here means an event was
//!    observed arriving. Anything else is `unresolved`, never "probably fine".
//!
//! The snippet FILE is always additive — a new file that overwrites nothing.
//! The only edit to existing code is two lines in the entrypoint, and that is
//! the edit the diff is for.

use std::path::{Path, PathBuf};

use engine_core::{Claim, Provenance};

#[derive(Debug, thiserror::Error)]
pub enum WireError {
    #[error("could not tell what framework this is. Drums wires Express and Next.js today; for anything else, copy a snippet from integrations/ by hand")]
    UnknownFramework,
    #[error("could not find an entrypoint to mount the reporter in ({0})")]
    NoEntrypoint(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// What Drums thinks this repo is, and — as with every other claim in the
/// product — how it knows.
#[derive(Debug, Clone)]
pub struct Detected {
    pub framework: Framework,
    /// Provenance of the detection itself. `Observed` when a dependency was
    /// read out of a manifest; `Inferred` when it came from file layout alone.
    pub claim: Claim,
    pub entrypoint: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framework {
    Express,
    NextJs,
}

impl Framework {
    pub fn label(self) -> &'static str {
        match self {
            Framework::Express => "express",
            Framework::NextJs => "next.js",
        }
    }

    /// The snippet filename this framework gets.
    pub fn snippet_name(self) -> &'static str {
        match self {
            Framework::Express => "drums-report.js",
            Framework::NextJs => "drums-report.ts",
        }
    }
}

fn read_json(path: &Path) -> Option<serde_json::Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// Look for an Express or Next.js app, and say which signal decided it.
///
/// Reading a dependency out of `package.json` is `Observed` — the manifest is
/// a statement the team wrote. Guessing from file layout is `Inferred`, and
/// labelled that way, because a `pages/` directory is evidence and not proof.
pub fn detect(repo: &Path) -> Result<Detected, WireError> {
    let pkg = read_json(&repo.join("package.json"));

    let dep = |name: &str| -> bool {
        pkg.as_ref()
            .map(|p| {
                ["dependencies", "devDependencies"]
                    .iter()
                    .any(|section| p.get(section).and_then(|d| d.get(name)).is_some())
            })
            .unwrap_or(false)
    };

    if dep("next") {
        return Ok(Detected {
            framework: Framework::NextJs,
            claim: Claim {
                text: "package.json declares `next` as a dependency".into(),
                provenance: Provenance::Observed,
            },
            entrypoint: None, // Next.js wires per-route, not at one entrypoint.
        });
    }
    if dep("express") {
        let entrypoint = find_express_entrypoint(repo, pkg.as_ref());
        return Ok(Detected {
            framework: Framework::Express,
            claim: Claim {
                text: "package.json declares `express` as a dependency".into(),
                provenance: Provenance::Observed,
            },
            entrypoint,
        });
    }

    // Layout-only guesses, labelled as guesses.
    if repo.join("next.config.js").exists() || repo.join("next.config.mjs").exists() {
        return Ok(Detected {
            framework: Framework::NextJs,
            claim: Claim {
                text: "a next.config file is present, but package.json does not declare `next`"
                    .into(),
                provenance: Provenance::Inferred,
            },
            entrypoint: None,
        });
    }

    Err(WireError::UnknownFramework)
}

/// `scripts.start`'s `node <entry>` first, then the conventional names. The
/// manifest is preferred because it is what the team actually runs.
fn find_express_entrypoint(repo: &Path, pkg: Option<&serde_json::Value>) -> Option<PathBuf> {
    if let Some(start) = pkg
        .and_then(|p| p.pointer("/scripts/start"))
        .and_then(|s| s.as_str())
        .and_then(|s| s.strip_prefix("node "))
    {
        let candidate = repo.join(start.trim());
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    for name in [
        "server.js",
        "app.js",
        "index.js",
        "src/server.js",
        "src/app.js",
        "src/index.js",
    ] {
        let candidate = repo.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// One file change, rendered so a human can read it before it happens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub path: PathBuf,
    pub kind: ChangeKind,
    /// Unified-diff-ish text. For DISPLAY only — never parsed to decide what
    /// to write. An earlier version rebuilt the edit by taking every line of
    /// this string that began with `+`, which silently swallowed the `+++`
    /// header and appended `++ /path/to/server.js` into the customer's
    /// entrypoint. Code must never be derived from a string whose job is to
    /// be readable.
    pub diff: String,
    /// The exact bytes to write. For `ChangeKind::Edit` this is what gets
    /// appended; `diff` only describes it. For `ChangeKind::NewFile` it is
    /// the whole file body when the change carries its own (the CI workflow
    /// does); absent, `apply` falls back to the framework snippet.
    pub content: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    /// A file that does not exist yet. Additive, overwrites nothing.
    NewFile,
    /// An edit to a file the team wrote. This is the one that needs consent.
    Edit,
}

/// The whole plan, computed without writing anything.
#[derive(Debug, Clone)]
pub struct Plan {
    pub detected: Detected,
    pub changes: Vec<Change>,
    /// Things the operator still has to do that Drums cannot do for them.
    pub manual: Vec<String>,
}

const EXPRESS_MOUNT: &str =
    "const { drumsReport } = require(\"./drums-report\");\napp.use(drumsReport({ service: \"SERVICE\" }));";

/// Build the plan. Pure: reads the repo, writes nothing.
pub fn plan(repo: &Path, service: &str) -> Result<Plan, WireError> {
    let detected = detect(repo)?;
    let mut changes = Vec::new();
    let mut manual = Vec::new();

    match detected.framework {
        Framework::Express => {
            let entry = detected
                .entrypoint
                .clone()
                .ok_or_else(|| WireError::NoEntrypoint(repo.display().to_string()))?;
            let snippet = entry
                .parent()
                .unwrap_or(repo)
                .join(Framework::Express.snippet_name());

            if !snippet.exists() {
                changes.push(Change {
                    path: snippet.clone(),
                    kind: ChangeKind::NewFile,
                    content: None,
                    diff: format!(
                        "+ {} (new file, ~120 lines)\n  the reporting middleware. It never throws: any failure while\n  building or sending a report is swallowed and next(err) is always\n  called, so your error handling runs unaffected.",
                        snippet.display()
                    ),
                });
            }

            let body = std::fs::read_to_string(&entry).unwrap_or_default();
            if body.contains("drums-report") {
                manual.push(format!(
                    "{} already references drums-report — leaving it alone",
                    entry.display()
                ));
            } else {
                let mount = EXPRESS_MOUNT.replace("SERVICE", service);
                changes.push(Change {
                    path: entry.clone(),
                    kind: ChangeKind::Edit,
                    diff: format!(
                        "--- {}\n+++ {}\n@@ appended at the end of the file @@\n{}",
                        entry.display(),
                        entry.display(),
                        mount
                            .lines()
                            .map(|l| format!("+{l}"))
                            .collect::<Vec<_>>()
                            .join("\n")
                    ),
                    content: Some(mount),
                });
                manual.push(
                    "Express error middleware must be mounted AFTER your routes. Drums appends \
                     to the end of the file, which is usually right — check that it lands after \
                     your route definitions and before app.listen()."
                        .to_string(),
                );
            }
        }
        Framework::NextJs => {
            let snippet = repo.join("lib").join(Framework::NextJs.snippet_name());
            if !snippet.exists() {
                changes.push(Change {
                    path: snippet.clone(),
                    kind: ChangeKind::NewFile,
                    content: None,
                    diff: format!(
                        "+ {} (new file)\n  a route wrapper for App Router handlers.",
                        snippet.display()
                    ),
                });
            }
            // Deliberately no automatic edit: Next.js has no single entrypoint
            // to mount error handling in, and guessing which of a dozen route
            // files to rewrite is exactly the silent injection CONTRACTS.md
            // rejects.
            manual.push(
                "Next.js has no single entrypoint, so Drums will not guess which routes to \
                 wrap. Import drumsReport in a route and wrap its handler — see the header of \
                 the generated file for the two-line example."
                    .to_string(),
            );
        }
    }

    // The CI runner, staged the same way as everything else: shown as a diff,
    // written only on apply, never over a file that exists. It is inert until
    // `--dispatch-repairs` is used — pure YAML that does nothing on its own —
    // and shipping it here is what makes team mode reachable without hunting
    // for the file: the Drums App cannot write workflows on purpose, so the
    // one place this file can come from is a tool the person is already
    // watching print diffs.
    let workflow = repo
        .join(".github")
        .join("workflows")
        .join("drums-repair.yml");
    if !workflow.exists() {
        changes.push(Change {
            path: workflow.clone(),
            kind: ChangeKind::NewFile,
            content: Some(REPAIR_WORKFLOW.to_string()),
            diff: format!(
                "+ {} (new file)\n  the CI runner for team mode. Inert until `drums watch \
                 --dispatch-repairs`;\n  read its header — it explains every permission it asks for.",
                workflow.display()
            ),
        });
        manual.push(
            "Commit .github/workflows/drums-repair.yml on your default branch when you want \
             team mode. The Drums App deliberately cannot write workflows, so this file is \
             always something a person committed."
                .to_string(),
        );
    }

    manual.push(
        "Set DRUMS_INGEST_URL in the app's environment (e.g. http://127.0.0.1:7787). Without \
         it the reporter is a silent no-op and sends nothing — which is the safe default, and \
         also means an unset variable looks exactly like a working install until you verify."
            .to_string(),
    );

    Ok(Plan {
        detected,
        changes,
        manual,
    })
}

/// Apply the plan. Only ever called after the diff has been printed.
pub fn apply(
    plan: &Plan,
    snippet_source: &dyn Fn(Framework) -> String,
) -> Result<Vec<PathBuf>, WireError> {
    let mut written = Vec::new();
    for change in &plan.changes {
        match change.kind {
            ChangeKind::NewFile => {
                if let Some(parent) = change.path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                // Never overwrite. A file that already exists may have been
                // edited by the team, and clobbering it would destroy work to
                // install something they already have.
                if !change.path.exists() {
                    let body = change
                        .content
                        .clone()
                        .unwrap_or_else(|| snippet_source(plan.detected.framework));
                    std::fs::write(&change.path, body)?;
                    written.push(change.path.clone());
                }
            }
            ChangeKind::Edit => {
                let mut body = std::fs::read_to_string(&change.path)?;
                if body.contains("drums-report") {
                    continue;
                }
                if !body.ends_with('\n') {
                    body.push('\n');
                }
                body.push('\n');
                body.push_str("// Added by `drums init --wire`. Reports failures to Drums; a\n");
                body.push_str("// no-op unless DRUMS_INGEST_URL is set. Safe to remove.\n");
                // `content`, never `diff`. See the field's doc comment.
                let Some(mount) = change.content.as_ref() else {
                    continue;
                };
                body.push_str(mount);
                body.push('\n');
                std::fs::write(&change.path, body)?;
                written.push(change.path.clone());
            }
        }
    }
    Ok(written)
}

/// The CI runner workflow, compiled in for the same reason as the snippets:
/// an installed binary has no repo next to it. This is the file the docs also
/// serve at drums.sh/drums-repair.yml — one template, two doors.
const REPAIR_WORKFLOW: &str = include_str!("../templates/drums-repair.yml");

/// The snippet bodies, compiled in.
///
/// `include_str!` rather than reading `integrations/` at run time: an
/// installed binary has no repo next to it, and a snippet that only exists in
/// a git checkout would work on the machine that built it and nowhere else.
pub fn snippet_for(framework: Framework) -> String {
    match framework {
        Framework::Express => {
            include_str!("../../../../integrations/node-express/drums-report.js").to_string()
        }
        Framework::NextJs => {
            include_str!("../../../../integrations/nextjs/drums-report.ts").to_string()
        }
    }
}

// -- the round trip ----------------------------------------------------------

/// What `--verify` established. There is no "probably working" state.
#[derive(Debug, Clone)]
pub struct VerifyOutcome {
    pub claim: Claim,
    /// The synthetic event's marker, so a human can find it in the record.
    pub marker: String,
}

/// Post a synthetic error to `ingest_url` and look for it in the record.
///
/// This is the difference between an install that reports success and an
/// install that IS successful: a `verified` claim here means an event was
/// observed making the entire trip — over the wire, through ingest, onto disk.
/// Every other outcome is `unresolved`, and says which leg failed.
///
/// `intake_source` is the command actually run — `"drums init"` or
/// `"drums init --verify"` — because the record must never claim a flag
/// nobody typed.
pub async fn verify_round_trip(
    ingest_url: &str,
    record_path: &Path,
    marker: &str,
    intake_source: &str,
    now_ms: u64,
) -> VerifyOutcome {
    let url = format!("{}/v1/events", ingest_url.trim_end_matches('/'));
    let payload = serde_json::json!({
        // The service name every reading surface (doctor, digest, the
        // rate/shift readers in engine-detect) excludes as a self-test.
        "service": engine_detect::observe::SELF_TEST_SERVICE,
        "occurred_at_ms": now_ms,
        "error_name": "DrumsWireCheck",
        // The marker travels in the message so it can be found again without
        // relying on timing, which is unreliable on a busy record.
        "error_message": marker,
        "stack": format!("DrumsWireCheck: {marker}\n    at drumsInitVerify (drums-report:1:1)"),
        "intake": { "kind": "trigger", "source": intake_source },
    });

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return VerifyOutcome {
                claim: Claim {
                    text: format!("could not build an HTTP client: {e}"),
                    provenance: Provenance::Unresolved,
                },
                marker: marker.to_string(),
            }
        }
    };

    match client.post(&url).json(&payload).send().await {
        Ok(res) if res.status().is_success() => {}
        Ok(res) => {
            return VerifyOutcome {
                claim: Claim {
                    text: format!("{url} answered {} — the event was not accepted", res.status()),
                    provenance: Provenance::Unresolved,
                },
                marker: marker.to_string(),
            }
        }
        Err(e) => {
            return VerifyOutcome {
                claim: Claim {
                    text: format!(
                        "nothing is listening at {url} ({e}). Start `drums watch` (or the daemon) first"
                    ),
                    provenance: Provenance::Unresolved,
                },
                marker: marker.to_string(),
            }
        }
    }

    // Accepted over the wire is not the same as landed on disk — ingest
    // appends asynchronously, and a write that fails must not read as success.
    for _ in 0..20 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if record_contains(record_path, marker) {
            return VerifyOutcome {
                claim: Claim {
                    text: format!(
                        "a test event travelled {url} → the record at {}",
                        record_path.display()
                    ),
                    provenance: Provenance::Verified,
                },
                marker: marker.to_string(),
            };
        }
    }

    VerifyOutcome {
        claim: Claim {
            text: format!(
                "{url} accepted the event but it did not reach {} within 2s — accepted is not the same as recorded",
                record_path.display()
            ),
            provenance: Provenance::Unresolved,
        },
        marker: marker.to_string(),
    }
}

fn record_contains(record_path: &Path, marker: &str) -> bool {
    engine_record::read_all(record_path)
        .map(|r| {
            r.lines.iter().any(|(kind, v)| {
                kind == "event" && v.get("error_message").and_then(|m| m.as_str()) == Some(marker)
            })
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_with(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
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
    fn a_declared_dependency_is_observed_not_inferred() {
        let d = repo_with(&[
            ("package.json", r#"{"dependencies":{"express":"^4"}}"#),
            ("server.js", "const app = require('express')();\n"),
        ]);
        let det = detect(d.path()).unwrap();
        assert_eq!(det.framework, Framework::Express);
        assert_eq!(
            det.claim.provenance,
            Provenance::Observed,
            "a manifest is a statement the team wrote, not a guess"
        );
    }

    #[test]
    fn a_layout_only_guess_is_labelled_inferred() {
        let d = repo_with(&[("next.config.mjs", "export default {};\n")]);
        let det = detect(d.path()).unwrap();
        assert_eq!(det.framework, Framework::NextJs);
        assert_eq!(
            det.claim.provenance,
            Provenance::Inferred,
            "a config file is evidence, not proof"
        );
    }

    #[test]
    fn an_unrecognised_repo_refuses_instead_of_guessing() {
        let d = repo_with(&[("package.json", r#"{"dependencies":{"koa":"^2"}}"#)]);
        assert!(matches!(detect(d.path()), Err(WireError::UnknownFramework)));
    }

    #[test]
    fn the_start_script_beats_conventional_filenames() {
        let d = repo_with(&[
            (
                "package.json",
                r#"{"dependencies":{"express":"^4"},"scripts":{"start":"node src/boot.js"}}"#,
            ),
            ("src/boot.js", "x"),
            ("server.js", "x"),
        ]);
        let det = detect(d.path()).unwrap();
        assert_eq!(det.entrypoint.unwrap(), d.path().join("src/boot.js"));
    }

    #[test]
    fn the_plan_writes_nothing() {
        let d = repo_with(&[
            ("package.json", r#"{"dependencies":{"express":"^4"}}"#),
            ("server.js", "const app = require('express')();\n"),
        ]);
        let before = std::fs::read_to_string(d.path().join("server.js")).unwrap();
        let p = plan(d.path(), "shop").unwrap();
        assert!(!p.changes.is_empty());
        assert_eq!(
            std::fs::read_to_string(d.path().join("server.js")).unwrap(),
            before,
            "computing a plan must not touch a single byte"
        );
        assert!(!d.path().join("drums-report.js").exists());
        assert!(!d.path().join(".github/workflows/drums-repair.yml").exists());
    }

    #[test]
    fn the_workflow_is_planned_when_absent_and_written_by_apply() {
        let d = repo_with(&[
            ("package.json", r#"{"dependencies":{"express":"^4"}}"#),
            ("server.js", "const app = require('express')();\n"),
        ]);
        let p = plan(d.path(), "shop").unwrap();
        assert!(
            p.changes
                .iter()
                .any(|c| c.path.ends_with("drums-repair.yml")),
            "{:?}",
            p.changes.iter().map(|c| &c.path).collect::<Vec<_>>()
        );
        apply(&p, &|_f| "// snippet\n".to_string()).unwrap();
        let body =
            std::fs::read_to_string(d.path().join(".github/workflows/drums-repair.yml")).unwrap();
        // The compiled-in template, not a stub: the push step and the OIDC
        // fetch are the parts the hosted path invokes by name.
        assert!(
            body.contains("Push the verified branch"),
            "stale template compiled in"
        );
        assert!(body.contains("drums repair"), "{body:.100}");
    }

    #[test]
    fn an_existing_workflow_is_never_overwritten() {
        let d = repo_with(&[
            ("package.json", r#"{"dependencies":{"express":"^4"}}"#),
            ("server.js", "const app = require('express')();\n"),
            (
                ".github/workflows/drums-repair.yml",
                "# MY PINNED VERSION\n",
            ),
        ]);
        let p = plan(d.path(), "shop").unwrap();
        assert!(
            !p.changes
                .iter()
                .any(|c| c.path.ends_with("drums-repair.yml")),
            "an existing workflow must not even be planned over"
        );
        apply(&p, &|_f| "// snippet\n".to_string()).unwrap();
        assert_eq!(
            std::fs::read_to_string(d.path().join(".github/workflows/drums-repair.yml")).unwrap(),
            "# MY PINNED VERSION\n",
            "a workflow the team may have edited must never be clobbered"
        );
    }

    #[test]
    fn the_service_name_reaches_the_diff_so_it_can_be_checked_before_it_is_applied() {
        let d = repo_with(&[
            ("package.json", r#"{"dependencies":{"express":"^4"}}"#),
            ("server.js", "const app = require('express')();\n"),
        ]);
        let p = plan(d.path(), "checkout").unwrap();
        let edit = p
            .changes
            .iter()
            .find(|c| c.kind == ChangeKind::Edit)
            .unwrap();
        assert!(edit.diff.contains("service: \"checkout\""), "{}", edit.diff);
    }

    #[test]
    fn applying_twice_changes_nothing_the_second_time() {
        let d = repo_with(&[
            ("package.json", r#"{"dependencies":{"express":"^4"}}"#),
            ("server.js", "const app = require('express')();\n"),
        ]);
        let src = |_f: Framework| "// snippet\n".to_string();

        let p = plan(d.path(), "shop").unwrap();
        let first = apply(&p, &src).unwrap();
        assert_eq!(first.len(), 3, "snippet + entrypoint edit + CI workflow");
        let after_first = std::fs::read_to_string(d.path().join("server.js")).unwrap();

        // Re-planning sees the already-wired entrypoint.
        let p2 = plan(d.path(), "shop").unwrap();
        let second = apply(&p2, &src).unwrap();
        assert!(
            second.is_empty(),
            "a second run must be a no-op: {second:?}"
        );
        assert_eq!(
            std::fs::read_to_string(d.path().join("server.js")).unwrap(),
            after_first,
            "the entrypoint must not accumulate mounts"
        );
    }

    /// The regression: `apply` used to rebuild the edit by taking every line
    /// of the DIFF beginning with `+`, which swallowed the `+++` header and
    /// appended `++ /path/to/server.js` into the customer's entrypoint. Caught
    /// by reading the file after a real run, not by a test.
    #[test]
    fn the_applied_edit_contains_only_real_code() {
        let d = repo_with(&[
            ("package.json", r#"{"dependencies":{"express":"^4"}}"#),
            ("server.js", "const app = require('express')();\n"),
        ]);
        let p = plan(d.path(), "shop").unwrap();
        apply(&p, &|_f| "// snippet\n".to_string()).unwrap();

        let body = std::fs::read_to_string(d.path().join("server.js")).unwrap();
        assert!(
            !body.contains("+++"),
            "a diff header reached the file:\n{body}"
        );
        assert!(
            !body.contains("++ "),
            "a diff header reached the file:\n{body}"
        );
        assert!(
            body.contains(r#"const { drumsReport } = require("./drums-report");"#),
            "{body}"
        );
        assert!(
            body.contains(r#"app.use(drumsReport({ service: "shop" }));"#),
            "{body}"
        );
        for line in body.lines() {
            assert!(
                !line.starts_with('+') && !line.starts_with("--- "),
                "diff syntax in the entrypoint: {line:?}"
            );
        }
    }

    #[test]
    fn an_existing_snippet_is_never_overwritten() {
        let d = repo_with(&[
            ("package.json", r#"{"dependencies":{"express":"^4"}}"#),
            ("server.js", "const app = require('express')();\n"),
            ("drums-report.js", "// MY EDITED VERSION\n"),
        ]);
        let p = plan(d.path(), "shop").unwrap();
        apply(&p, &|_f| "// fresh\n".to_string()).unwrap();
        assert_eq!(
            std::fs::read_to_string(d.path().join("drums-report.js")).unwrap(),
            "// MY EDITED VERSION\n",
            "a snippet the team may have edited must never be clobbered"
        );
    }

    #[test]
    fn nextjs_is_never_auto_edited() {
        let d = repo_with(&[("package.json", r#"{"dependencies":{"next":"^15"}}"#)]);
        let p = plan(d.path(), "web").unwrap();
        assert!(
            p.changes.iter().all(|c| c.kind == ChangeKind::NewFile),
            "Next.js has no single entrypoint; guessing which route to rewrite is the silent \
             injection CONTRACTS.md rejects: {:?}",
            p.changes
        );
        assert!(p.manual.iter().any(|m| m.contains("will not guess")));
    }

    #[test]
    fn the_plan_always_warns_that_an_unset_url_looks_identical_to_success() {
        let d = repo_with(&[
            ("package.json", r#"{"dependencies":{"express":"^4"}}"#),
            ("server.js", "x\n"),
        ]);
        let p = plan(d.path(), "shop").unwrap();
        assert!(
            p.manual
                .iter()
                .any(|m| m.contains("looks exactly like a working install")),
            "{:?}",
            p.manual
        );
    }
}
