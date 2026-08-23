//! `drums open <failure-id>` — put the repair in front of a human, in the
//! editor they already use.
//!
//! ## The problem this solves
//!
//! A finished repair is a branch and a commit sha. That is the correct thing
//! for it to be, and it is a terrible thing to hand somebody at 09:00. Reading
//! a diff in a terminal is fine for three lines and useless for thirty, and
//! `git checkout drums/repair-…` in the working tree is exactly the action a
//! person should not have to take on a Monday to look at something a robot
//! wrote.
//!
//! So: a fresh worktree at the repair's commit, and the editor opened on it.
//! The main working tree is never touched — no checkout, no stash, no
//! interrupted branch. Whatever was in progress stays in progress.
//!
//! ## Which editor
//!
//! `$DRUMS_EDITOR`, then `$VISUAL`, then `$EDITOR`, then a short list of
//! editors that are actually installed. This ordering matters: `$EDITOR` on
//! many systems is `vi`, set decades ago by a distribution and never changed,
//! and opening `vi` on a directory is not what anybody meant. `$DRUMS_EDITOR`
//! exists so a person whose `$EDITOR` is wrong for this purpose can fix it for
//! Drums without changing it for `git commit`.
//!
//! If nothing is found, this REFUSES and prints the path. It never guesses a
//! GUI application, and it never silently does nothing.
//!
//! ## What it never does
//!
//! Never merges, never checks out into the working tree, never deletes a
//! worktree it did not create, and never runs a shell — the branch name is
//! derived from a failure id that arrived over HTTP.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use engine_core::Repair;

#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error("could not read the record at {0}: {1}")]
    Record(String, String),
    #[error("no repair has completed for failure {0} — nothing to open yet")]
    NoRepair(String),
    #[error("the record's sha for this repair is not a sha: {0:?}")]
    InvalidSha(String),
    #[error("git {what} failed: {detail}")]
    Git { what: String, detail: String },
    #[error("no editor found. Set $DRUMS_EDITOR (or $VISUAL / $EDITOR) to the command you want")]
    NoEditor,
    #[error("`{editor}` could not be launched: {detail}")]
    EditorFailed { editor: String, detail: String },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Editors worth trying when the environment says nothing. GUI editors first
/// and terminal editors deliberately absent: a terminal editor launched on a
/// directory from a non-interactive context is a hang, not a help.
const KNOWN_EDITORS: &[&str] = &["cursor", "code", "zed", "subl", "idea", "windsurf"];

/// The editor command to run, and where it came from — so the output can say
/// *why* it picked what it picked, which is the difference between "it opened
/// the wrong thing" and "it opened the wrong thing and I know which variable
/// to change".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Editor {
    pub argv: Vec<String>,
    pub source: &'static str,
}

/// Resolve the editor from the environment, then from what is installed.
///
/// `env` is passed in rather than read directly so the precedence is testable
/// without mutating the process environment — which is a data race in a test
/// binary that runs threads.
pub fn resolve_editor(
    env: &dyn Fn(&str) -> Option<String>,
    is_installed: &dyn Fn(&str) -> bool,
) -> Option<Editor> {
    for (var, source) in [
        ("DRUMS_EDITOR", "$DRUMS_EDITOR"),
        ("VISUAL", "$VISUAL"),
        ("EDITOR", "$EDITOR"),
    ] {
        if let Some(raw) = env(var) {
            let argv: Vec<String> = raw.split_whitespace().map(|s| s.to_string()).collect();
            if !argv.is_empty() {
                return Some(Editor { argv, source });
            }
        }
    }
    for candidate in KNOWN_EDITORS {
        if is_installed(candidate) {
            return Some(Editor {
                argv: vec![(*candidate).to_string()],
                source: "found on PATH",
            });
        }
    }
    None
}

fn on_path(program: &str) -> bool {
    Command::new(program)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Same rule `engine_repro::validate_sha` and `ship.rs` apply: a sha read back
/// out of the record is untrusted input, because `POST /v1/events` accepts
/// fields that reach it.
fn validate_sha(sha: &str) -> Result<(), OpenError> {
    if (4..=64).contains(&sha.len()) && sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(OpenError::InvalidSha(sha.to_string()))
    }
}

fn run_git(repo: &Path, args: &[&str]) -> Result<String, OpenError> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .output()?;
    if !out.status.success() {
        return Err(OpenError::Git {
            what: args.first().copied().unwrap_or("?").to_string(),
            detail: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The newest completed repair for `failure_id`.
fn latest_repair(record_path: &Path, failure_id: &str) -> Result<Repair, OpenError> {
    let read = engine_record::read_all(record_path)
        .map_err(|e| OpenError::Record(record_path.display().to_string(), e.to_string()))?;
    read.lines
        .iter()
        .rev()
        .filter(|(kind, _)| kind == "repair_ready")
        .filter(|(_, v)| v.get("failure_id").and_then(|f| f.as_str()) == Some(failure_id))
        .find_map(|(_, v)| serde_json::from_value::<Repair>(v.clone()).ok())
        .ok_or_else(|| OpenError::NoRepair(failure_id.to_string()))
}

/// Where a repair's inspection worktree lives. Deterministic, so running
/// `drums open` twice reuses one rather than accumulating checkouts of the
/// same commit.
pub fn worktree_path(repo: &Path, failure_id: &str) -> PathBuf {
    // The failure id reaches the filesystem, so keep only characters that
    // cannot traverse or escape. A ULID is alphanumeric; anything else is
    // something we did not write.
    let safe: String = failure_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(64)
        .collect();
    repo.join(".drums").join("open").join(safe)
}

pub struct Opened {
    pub worktree: PathBuf,
    pub branch: String,
    pub sha: String,
    pub summary: String,
    pub reused: bool,
    pub editor: Option<Editor>,
}

/// Materialise a worktree at the repair's commit and launch the editor on it.
///
/// `launch` is false for a dry run: the worktree is still created and its path
/// printed, which is what somebody piping this into another tool wants.
pub fn open_repair(
    repo: &Path,
    record_path: &Path,
    failure_id: &str,
    launch: bool,
) -> Result<Opened, OpenError> {
    let repair = latest_repair(record_path, failure_id)?;
    validate_sha(&repair.sha)?;

    let dir = worktree_path(repo, failure_id);
    let reused = dir.join(".git").exists();

    if !reused {
        if let Some(parent) = dir.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Detached at the sha, not checked out on the branch: two worktrees
        // cannot hold the same branch, and a detached inspection checkout can
        // never be mistaken for the branch itself or accidentally committed to.
        run_git(
            repo,
            &[
                "worktree",
                "add",
                "--detach",
                &dir.to_string_lossy(),
                &repair.sha,
            ],
        )?;
    }

    let editor = if launch {
        let e = resolve_editor(&|k| std::env::var(k).ok(), &on_path).ok_or(OpenError::NoEditor)?;
        let (program, rest) = e
            .argv
            .split_first()
            .expect("resolve_editor rejects empty argv");
        // Spawned, never waited on: a GUI editor returns immediately and a
        // long-lived one would otherwise block the terminal forever.
        Command::new(program)
            .args(rest)
            .arg(&dir)
            .stdin(Stdio::null())
            .spawn()
            .map_err(|err| OpenError::EditorFailed {
                editor: program.clone(),
                detail: err.to_string(),
            })?;
        Some(e)
    } else {
        None
    };

    Ok(Opened {
        worktree: dir,
        branch: repair.branch,
        sha: repair.sha,
        summary: repair.summary,
        reused,
        editor,
    })
}

/// Remove an inspection worktree. Only ever removes one under
/// `.drums/open/`, so a mistyped id cannot delete somebody's checkout.
pub fn close_repair(repo: &Path, failure_id: &str) -> Result<Option<PathBuf>, OpenError> {
    let dir = worktree_path(repo, failure_id);
    if !dir.exists() {
        return Ok(None);
    }
    run_git(
        repo,
        &["worktree", "remove", "--force", &dir.to_string_lossy()],
    )?;
    Ok(Some(dir))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of<'a>(
        pairs: &'a [(&'static str, &'static str)],
    ) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| (*v).to_string())
        }
    }

    #[test]
    fn drums_editor_beats_visual_and_editor() {
        let env = env_of(&[
            ("DRUMS_EDITOR", "zed"),
            ("VISUAL", "code"),
            ("EDITOR", "vi"),
        ]);
        let e = resolve_editor(&env, &|_| true).unwrap();
        assert_eq!(e.argv, vec!["zed"]);
        assert_eq!(e.source, "$DRUMS_EDITOR");
    }

    #[test]
    fn visual_beats_editor() {
        let env = env_of(&[("VISUAL", "code"), ("EDITOR", "vi")]);
        assert_eq!(resolve_editor(&env, &|_| true).unwrap().argv, vec!["code"]);
    }

    /// `$EDITOR` is honoured because a person set it — even when it is `vi`,
    /// which is probably not what they want here. That is what `$DRUMS_EDITOR`
    /// is for, and silently overriding an explicit setting would be worse.
    #[test]
    fn editor_is_honoured_even_when_it_is_a_terminal_editor() {
        let env = env_of(&[("EDITOR", "vi")]);
        let e = resolve_editor(&env, &|_| true).unwrap();
        assert_eq!(e.argv, vec!["vi"]);
        assert_eq!(e.source, "$EDITOR");
    }

    #[test]
    fn an_editor_with_arguments_survives_as_argv() {
        let env = env_of(&[("DRUMS_EDITOR", "code --new-window")]);
        let e = resolve_editor(&env, &|_| true).unwrap();
        assert_eq!(e.argv, vec!["code", "--new-window"]);
    }

    #[test]
    fn an_empty_variable_is_ignored_rather_than_launching_nothing() {
        let env = env_of(&[("EDITOR", "   ")]);
        assert!(resolve_editor(&env, &|_| false).is_none());
    }

    #[test]
    fn falls_back_to_what_is_installed_in_a_stable_order() {
        let env = env_of(&[]);
        // Only `code` and `zed` installed: `cursor` is checked first and
        // missing, so `code` wins by list order, not by chance.
        let e = resolve_editor(&env, &|p| p == "code" || p == "zed").unwrap();
        assert_eq!(e.argv, vec!["code"]);
        assert_eq!(e.source, "found on PATH");
    }

    #[test]
    fn nothing_installed_and_nothing_configured_refuses() {
        assert!(resolve_editor(&env_of(&[]), &|_| false).is_none());
    }

    #[test]
    fn a_failure_id_can_never_escape_the_open_directory() {
        let repo = Path::new("/srv/shop");
        for hostile in [
            "../../etc/passwd",
            "..",
            "/absolute",
            "a/b/c",
            "id;rm -rf /",
            "id$(whoami)",
        ] {
            let p = worktree_path(repo, hostile);
            assert!(
                p.starts_with(repo.join(".drums").join("open")),
                "{hostile:?} escaped to {p:?}"
            );
            assert!(!p.to_string_lossy().contains(".."), "{hostile:?} -> {p:?}");
        }
    }

    #[test]
    fn a_recorded_sha_that_is_not_a_sha_is_refused() {
        assert!(validate_sha("abc1234").is_ok());
        for bad in ["--upload-pack=touch /tmp/pwned", "", "zzz", "abc; rm -rf /"] {
            assert!(validate_sha(bad).is_err(), "{bad:?} should be refused");
        }
    }
}
