//! Attribution: which deploy caused this failure. Time correlation + stack
//! trace ∩ changed files. The result is an INFERRED claim — reproduction
//! (engine-repro) is what upgrades it (spec §10).

use std::path::Path;
use std::process::Command;

use engine_core::{Attribution, Claim, DeployRecord, Failure, Provenance};

#[derive(Debug, thiserror::Error)]
pub enum AttributeError {
    #[error("git diff-tree failed for {sha}: {detail}")]
    Git { sha: String, detail: String },
}

pub fn changed_files(repo: &Path, sha: &str) -> Result<Vec<String>, AttributeError> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["diff-tree", "--no-commit-id", "--name-only", "-r", "--root", "-m", "--first-parent", sha])
        .output()
        .map_err(|e| AttributeError::Git { sha: sha.into(), detail: e.to_string() })?;
    if !out.status.success() {
        return Err(AttributeError::Git { sha: sha.into(), detail: String::from_utf8_lossy(&out.stderr).into() });
    }
    Ok(String::from_utf8_lossy(&out.stdout).lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect())
}

/// Whether two paths refer to the same file, allowing one to be a
/// deployment-relative or otherwise shorter path into the other. Matching is
/// component-aligned: a bare `ends_with` would let "not-total.js" match
/// "total.js" (true as raw strings, false as paths), so overlap requires
/// either exact equality or a `/`-aligned suffix boundary.
///
/// `pub`: reused by `drums why` (`crates/cli/src/why.rs`) to match a
/// requested file path against a production failure's `top_frame_file` —
/// the same discipline, one implementation, so a fix here (or a bug) can't
/// silently drift between attribution and `why`.
pub fn paths_overlap(a: &str, b: &str) -> bool {
    a == b || a.ends_with(&format!("/{b}")) || b.ends_with(&format!("/{a}"))
}

pub fn attribute(
    repo: &Path,
    deploys: &[DeployRecord],
    failure: &Failure,
) -> Result<Option<Attribution>, AttributeError> {
    let Some(deploy) = deploys
        .iter()
        .filter(|d| d.deployed_at_ms <= failure.first_seen_ms)
        .max_by_key(|d| d.deployed_at_ms)
        .cloned()
    else {
        return Ok(None);
    };
    let files = changed_files(repo, &deploy.sha)?;
    let frame = failure.signature.top_frame_file.as_str();
    let overlap: Vec<String> = files
        .iter()
        .filter(|f| paths_overlap(f, frame))
        .cloned()
        .collect();
    let minutes_after_deploy = (failure.first_seen_ms.saturating_sub(deploy.deployed_at_ms)) / 60_000;
    let short = deploy.sha.get(..6).unwrap_or(&deploy.sha);
    let claim = Claim {
        text: format!(
            "first error {minutes_after_deploy} min after deploy {short}; {} of {} changed files in the stack trace",
            overlap.len(),
            files.len()
        ),
        provenance: Provenance::Inferred,
    };
    Ok(Some(Attribution { deploy, overlap_files: overlap, minutes_after_deploy, claim }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_core::*;
    use std::path::Path;
    use std::process::Command;

    fn git(repo: &Path, args: &[&str]) {
        let ok = Command::new("git").arg("-C").arg(repo).args(args).status().unwrap().success();
        assert!(ok, "git {args:?} failed");
    }

    /// two commits: c1 adds server.js+lib/cart/total.js, c2 modifies lib/cart/total.js
    fn fixture() -> (tempfile::TempDir, String, String) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        git(p, &["init", "-q"]);
        git(p, &["config", "user.email", "t@t"]);
        git(p, &["config", "user.name", "t"]);
        std::fs::create_dir_all(p.join("lib/cart")).unwrap();
        std::fs::write(p.join("server.js"), "s1").unwrap();
        std::fs::write(p.join("lib/cart/total.js"), "t1").unwrap();
        git(p, &["add", "."]);
        git(p, &["commit", "-qm", "c1"]);
        let c1 = String::from_utf8(Command::new("git").arg("-C").arg(p).args(["rev-parse", "HEAD"]).output().unwrap().stdout).unwrap().trim().into();
        std::fs::write(p.join("lib/cart/total.js"), "t2").unwrap();
        git(p, &["add", "lib/cart/total.js"]);
        git(p, &["commit", "-qm", "add promo code field"]);
        let c2 = String::from_utf8(Command::new("git").arg("-C").arg(p).args(["rev-parse", "HEAD"]).output().unwrap().stdout).unwrap().trim().into();
        (dir, c1, c2)
    }

    fn rev_parse(p: &Path, rev: &str) -> String {
        String::from_utf8(Command::new("git").arg("-C").arg(p).args(["rev-parse", rev]).output().unwrap().stdout)
            .unwrap()
            .trim()
            .into()
    }

    /// root commit adds server.js+lib/cart/total.js; a feature branch modifies
    /// lib/cart/total.js and is merged back with `--no-ff`, producing a real
    /// merge commit with two parents.
    fn merge_fixture() -> (tempfile::TempDir, String, String) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        git(p, &["init", "-q"]);
        git(p, &["config", "user.email", "t@t"]);
        git(p, &["config", "user.name", "t"]);
        std::fs::create_dir_all(p.join("lib/cart")).unwrap();
        std::fs::write(p.join("server.js"), "s1").unwrap();
        std::fs::write(p.join("lib/cart/total.js"), "t1").unwrap();
        git(p, &["add", "."]);
        git(p, &["commit", "-qm", "root"]);
        let root = rev_parse(p, "HEAD");
        git(p, &["checkout", "-qb", "feature"]);
        std::fs::write(p.join("lib/cart/total.js"), "t2").unwrap();
        git(p, &["add", "lib/cart/total.js"]);
        git(p, &["commit", "-qm", "feature change"]);
        git(p, &["checkout", "-q", "-"]);
        git(p, &["merge", "--no-ff", "-q", "-m", "merge feature", "feature"]);
        let merge = rev_parse(p, "HEAD");
        (dir, root, merge)
    }

    fn failure_at(ms: u64, file: &str) -> Failure {
        Failure {
            id: "f1".into(),
            service: "shop".into(),
            signature: ErrorSignature { error_name: "TypeError".into(), top_frame_file: file.into(), top_frame_function: None },
            first_seen_ms: ms,
            event_count: 3,
            sample: ErrorEvent { service: "shop".into(), occurred_at_ms: ms, error_name: "TypeError".into(), error_message: "m".into(), stack: String::new(), request: Some(CapturedRequest { method: "POST".into(), path: "/api/checkout".into(), content_type: None, body: None }), intake: engine_core::Intake::Snippet },
            intake: engine_core::Intake::Snippet,
            claim: Claim { text: "t".into(), provenance: Provenance::Observed },
        }
    }

    fn deploy(sha: &str, at: u64) -> DeployRecord {
        DeployRecord { sha: sha.into(), description: "d".into(), author: "maya".into(), deployed_at_ms: at }
    }

    #[test]
    fn changed_files_lists_commit_diff() {
        let (dir, _c1, c2) = fixture();
        let files = changed_files(dir.path(), &c2).unwrap();
        assert_eq!(files, vec!["lib/cart/total.js".to_string()]);
    }

    #[test]
    fn attributes_to_latest_deploy_before_failure_with_overlap() {
        let (dir, c1, c2) = fixture();
        let deploys = vec![deploy(&c1, 1_000), deploy(&c2, 100_000)];
        let attr = attribute(dir.path(), &deploys, &failure_at(460_000, "lib/cart/total.js")).unwrap().unwrap();
        assert_eq!(attr.deploy.sha, c2);
        assert_eq!(attr.overlap_files, vec!["lib/cart/total.js".to_string()]);
        assert_eq!(attr.minutes_after_deploy, 6);
        assert_eq!(attr.claim.provenance, Provenance::Inferred);
    }

    #[test]
    fn no_deploy_before_failure_yields_none() {
        let (dir, c1, _c2) = fixture();
        let deploys = vec![deploy(&c1, 500_000)];
        assert!(attribute(dir.path(), &deploys, &failure_at(1_000, "lib/cart/total.js")).unwrap().is_none());
    }

    #[test]
    fn git_failure_propagates_as_err_not_none() {
        // A nonexistent sha precedes the failure: this is a real git failure
        // (bad sha), not "no deploy precedes the failure" -- it must surface
        // as Err, never be swallowed into Ok(None).
        let (dir, _c1, _c2) = fixture();
        let deploys = vec![deploy("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef", 1_000)];
        let result = attribute(dir.path(), &deploys, &failure_at(10_000, "lib/cart/total.js"));
        assert!(result.is_err(), "expected Err on git failure, got {result:?}");
    }

    #[test]
    fn overlap_uses_suffix_match_against_deployment_paths() {
        // stack frames may carry deployment-relative paths; suffix matching bridges them
        let (dir, c1, c2) = fixture();
        let deploys = vec![deploy(&c1, 1_000), deploy(&c2, 2_000)];
        let attr = attribute(dir.path(), &deploys, &failure_at(10_000, "cart/total.js")).unwrap().unwrap();
        assert_eq!(attr.overlap_files, vec!["lib/cart/total.js".to_string()]);
    }

    #[test]
    fn paths_overlap_rejects_bare_suffix_without_path_boundary() {
        // "not-total.js" and "subtotal.js" both end with "total.js" as raw
        // strings, but neither is the same file or a path-aligned suffix of it.
        assert!(!paths_overlap("not-total.js", "total.js"));
        assert!(!paths_overlap("subtotal.js", "total.js"));
    }

    #[test]
    fn paths_overlap_matches_path_aligned_suffix() {
        assert!(paths_overlap("lib/cart/total.js", "cart/total.js"));
    }

    #[test]
    fn paths_overlap_matches_exact_equal_paths() {
        assert!(paths_overlap("server.js", "server.js"));
    }

    #[test]
    fn changed_files_on_root_commit_lists_its_files() {
        let (dir, root, _merge) = merge_fixture();
        let mut files = changed_files(dir.path(), &root).unwrap();
        files.sort();
        assert_eq!(files, vec!["lib/cart/total.js".to_string(), "server.js".to_string()]);
    }

    #[test]
    fn changed_files_on_merge_commit_lists_files_brought_in_by_merged_branch() {
        let (dir, _root, merge) = merge_fixture();
        let files = changed_files(dir.path(), &merge).unwrap();
        assert_eq!(files, vec!["lib/cart/total.js".to_string()]);
    }
}
