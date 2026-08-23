//! Full local reproduction against the seeded demo repo. Requires node >= 20.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use engine_core::*;
use engine_repro::{BootedApp, LocalProcessReproducer, Reproducer};

fn seed(work: &Path) -> (String, String) {
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../demo/seed.sh");
    let out = Command::new("bash").arg(script).arg(work).output().expect("seed.sh runs");
    assert!(out.status.success(), "seed failed: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let get = |k: &str| stdout.lines().find_map(|l| l.strip_prefix(k)).expect(k).to_string();
    (get("V1_SHA="), get("V2_SHA="))
}

fn run_git(repo: &Path, args: &[&str]) {
    let out = Command::new("git").arg("-C").arg(repo).args(args).output().expect("git runs");
    assert!(out.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
}

fn rev_parse_head(repo: &Path) -> String {
    let out = Command::new("git").arg("-C").arg(repo).args(["rev-parse", "HEAD"]).output().expect("git runs");
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn init_repo(repo: &Path) {
    std::fs::create_dir_all(repo).unwrap();
    run_git(repo, &["init", "-q"]);
    run_git(repo, &["config", "user.email", "maya@example.com"]);
    run_git(repo, &["config", "user.name", "maya"]);
}

/// A repo with exactly one commit — the broken v2 server — so `<sha>^` does
/// not resolve to anything. Exercises the root-commit / shallow-clone case
/// where the parent-side boot has no revision to build.
fn seed_root_commit_only(work: &Path) -> (PathBuf, String) {
    let repo = work.join("shop");
    init_repo(&repo);
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../demo/checkout/versions/server_v2.js");
    std::fs::copy(&src, repo.join("server.js")).unwrap();
    run_git(&repo, &["add", "server.js"]);
    run_git(&repo, &["commit", "-qm", "only commit: add promo code field"]);
    let sha = rev_parse_head(&repo);
    (repo, sha)
}

/// Two commits that both carry the same bug (v2's unguarded `body.promo.code`
/// access) — the parent-also-fails case, where the parent boots fine but 500s
/// on the same request.
fn seed_both_commits_broken(work: &Path) -> (PathBuf, String) {
    let repo = work.join("shop");
    init_repo(&repo);
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../demo/checkout/versions/server_v2.js");
    let content = std::fs::read_to_string(&src).unwrap();
    std::fs::write(repo.join("server.js"), &content).unwrap();
    run_git(&repo, &["add", "server.js"]);
    run_git(&repo, &["commit", "-qm", "first: add promo code field (broken)"]);
    std::fs::write(repo.join("server.js"), format!("// still broken\n{content}")).unwrap();
    run_git(&repo, &["add", "server.js"]);
    run_git(&repo, &["commit", "-qm", "second: comment only, still broken"]);
    let sha = rev_parse_head(&repo);
    (repo, sha)
}

fn checkout_failure() -> (Failure, CapturedRequest) {
    let req = CapturedRequest {
        method: "POST".into(),
        path: "/api/checkout".into(),
        content_type: Some("application/json".into()),
        body: Some(r#"{"items":[{"price":100,"qty":2}]}"#.into()),
    };
    let f = Failure {
        id: "f1".into(),
        service: "shop".into(),
        signature: ErrorSignature { error_name: "TypeError".into(), top_frame_file: "server.js".into(), top_frame_function: None },
        first_seen_ms: 2_000,
        event_count: 3,
        sample: ErrorEvent {
            service: "shop".into(),
            occurred_at_ms: 2_000,
            error_name: "TypeError".into(),
            error_message: "Cannot read properties of undefined (reading 'code')".into(),
            stack: "TypeError: x\n    at computeTotal (/w/shop/server.js:5:25)".into(),
            request: Some(req.clone()),
            intake: Intake::Snippet,
        },
        intake: Intake::Snippet,
        claim: Claim { text: "t".into(), provenance: Provenance::Observed },
    };
    (f, req)
}

#[tokio::test]
async fn reproduces_v2_bug_and_confirms_parent_clean() {
    let work = tempfile::tempdir().unwrap();
    let (_v1, v2) = seed(work.path());
    let repo = work.path().join("shop");
    let (failure, _req) = checkout_failure();
    let attribution = Attribution {
        deploy: DeployRecord { sha: v2.clone(), description: "add promo code field".into(), author: "maya".into(), deployed_at_ms: 1_000 },
        overlap_files: vec!["server.js".into()],
        minutes_after_deploy: 0,
        claim: Claim { text: "t".into(), provenance: Provenance::Inferred },
    };
    let r = LocalProcessReproducer { boot_timeout_ms: 10_000, boot_cmd: None }
        .reproduce(&repo, &failure, &attribution)
        .await
        .expect("reproduction runs");
    assert!(r.reproduced, "v2 must reproduce: {}", r.detail);
    assert_eq!(r.parent_clean, Some(true), "v1 (parent) must serve cleanly: {}", r.detail);
    assert!(r.claims.iter().all(|c| c.provenance == Provenance::Verified));
    // worktrees cleaned up
    let wt = Command::new("git").arg("-C").arg(&repo).args(["worktree", "list"]).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&wt.stdout).lines().count(), 1, "no leftover worktrees");
}

#[tokio::test]
async fn healthy_revision_does_not_reproduce() {
    let work = tempfile::tempdir().unwrap();
    let (v1, _v2) = seed(work.path());
    let repo = work.path().join("shop");
    let (failure, _req) = checkout_failure();
    let attribution = Attribution {
        deploy: DeployRecord { sha: v1, description: "initial".into(), author: "maya".into(), deployed_at_ms: 1_000 },
        overlap_files: vec![],
        minutes_after_deploy: 0,
        claim: Claim { text: "t".into(), provenance: Provenance::Inferred },
    };
    let r = LocalProcessReproducer { boot_timeout_ms: 10_000, boot_cmd: None }
        .reproduce(&repo, &failure, &attribution)
        .await
        .expect("runs");
    assert!(!r.reproduced, "v1 must NOT reproduce");
    assert_eq!(r.parent_clean, None, "no parent evaluation happens on non-reproduction: {}", r.detail);
    assert_eq!(r.claims[0].provenance, Provenance::Unresolved, "non-reproduction claim must be Unresolved: {:?}", r.claims);
}

/// I3: a parent-side infrastructure failure (here: no parent revision exists
/// at all, the root-commit case) must not discard a confirmed deploy-side
/// reproduction. The engine must still emit `reproduced: true` with an honest
/// `parent_clean: None` and an Unresolved claim explaining why the parent
/// could not be evaluated — never an `Err` that throws away the real
/// Verified claim.
#[tokio::test]
async fn root_commit_parent_failure_preserves_confirmed_reproduction() {
    let work = tempfile::tempdir().unwrap();
    let (repo, sha) = seed_root_commit_only(work.path());
    let (failure, _req) = checkout_failure();
    let attribution = Attribution {
        deploy: DeployRecord { sha: sha.clone(), description: "only commit".into(), author: "maya".into(), deployed_at_ms: 1_000 },
        overlap_files: vec!["server.js".into()],
        minutes_after_deploy: 0,
        claim: Claim { text: "t".into(), provenance: Provenance::Inferred },
    };
    let r = LocalProcessReproducer { boot_timeout_ms: 10_000, boot_cmd: None }
        .reproduce(&repo, &failure, &attribution)
        .await
        .expect("a parent-side failure must not error out the whole reproduction");
    assert!(r.reproduced, "deploy revision must still be confirmed reproduced: {}", r.detail);
    assert_eq!(r.parent_clean, None, "no parent exists to evaluate: {}", r.detail);
    assert_eq!(r.claims.len(), 2, "claims: {:?}", r.claims);
    let verified = r.claims.iter().filter(|c| c.provenance == Provenance::Verified).count();
    let unresolved = r.claims.iter().filter(|c| c.provenance == Provenance::Unresolved).count();
    assert_eq!(verified, 1, "exactly one Verified claim: {:?}", r.claims);
    assert_eq!(unresolved, 1, "exactly one Unresolved claim: {:?}", r.claims);
}

/// I5(a): the parent-also-fails branch is the only path that emits a mixed
/// Verified + Unresolved packet — the literal encoding of "never collapse
/// claims into one green check." Pin it directly: both commits carry the
/// same bug, so the deploy reproduces AND the parent also fails.
#[tokio::test]
async fn parent_also_fails_yields_mixed_verified_and_unresolved_claims() {
    let work = tempfile::tempdir().unwrap();
    let (repo, sha) = seed_both_commits_broken(work.path());
    let (failure, _req) = checkout_failure();
    let attribution = Attribution {
        deploy: DeployRecord { sha: sha.clone(), description: "second (still broken)".into(), author: "maya".into(), deployed_at_ms: 1_000 },
        overlap_files: vec!["server.js".into()],
        minutes_after_deploy: 0,
        claim: Claim { text: "t".into(), provenance: Provenance::Inferred },
    };
    let r = LocalProcessReproducer { boot_timeout_ms: 10_000, boot_cmd: None }
        .reproduce(&repo, &failure, &attribution)
        .await
        .expect("reproduction runs");
    assert!(r.reproduced, "second commit must reproduce: {}", r.detail);
    assert_eq!(r.parent_clean, Some(false), "parent also fails: {}", r.detail);
    assert_eq!(r.claims.len(), 2, "claims: {:?}", r.claims);
    assert_eq!(r.claims[0].provenance, Provenance::Verified);
    assert_eq!(r.claims[1].provenance, Provenance::Unresolved);
    assert!(r.claims[1].text.contains("also fails"), "claim text must say 'also fails': {}", r.claims[1].text);
}

/// I5(c): repro children must never be able to feed telemetry back, even if
/// `DRUMS_INGEST_URL` is set in the parent process's own environment. Bind a
/// real listener at that address and prove it never receives a connection —
/// the only way this named constraint is enforced by something other than
/// reading the source.
#[tokio::test]
async fn repro_children_never_leak_telemetry_to_drums_ingest_url() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    std::env::set_var("DRUMS_INGEST_URL", format!("http://{addr}"));

    let work = tempfile::tempdir().unwrap();
    let (_v1, v2) = seed(work.path());
    let repo = work.path().join("shop");
    let (failure, _req) = checkout_failure();
    let attribution = Attribution {
        deploy: DeployRecord { sha: v2, description: "add promo code field".into(), author: "maya".into(), deployed_at_ms: 1_000 },
        overlap_files: vec!["server.js".into()],
        minutes_after_deploy: 0,
        claim: Claim { text: "t".into(), provenance: Provenance::Inferred },
    };
    let r = LocalProcessReproducer { boot_timeout_ms: 10_000, boot_cmd: None }
        .reproduce(&repo, &failure, &attribution)
        .await
        .expect("reproduction runs");
    assert!(r.reproduced, "sanity: v2 must still reproduce with DRUMS_INGEST_URL set in the parent env: {}", r.detail);

    std::env::remove_var("DRUMS_INGEST_URL");

    match listener.accept() {
        Ok(_) => panic!("a repro child connected to DRUMS_INGEST_URL — telemetry leaked back"),
        Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::WouldBlock, "listener must see zero connection attempts, got: {e}"),
    }
}

/// Defense in depth for the intake taxonomy: the engine SKIPS reproduction for
/// a trigger/reported failure and never calls in here (see
/// `attribute_and_reproduce` in `engine/crates/cli/src/engine.rs`). If a future
/// caller forgets, the reproducer must refuse with a typed error rather than
/// invent a request — a synthesized replay could earn `verified` for a request
/// nobody ever made, which is the cardinal sin of this product.
///
/// The refusal happens BEFORE any worktree is created, so the repo path below
/// is deliberately nonexistent: reaching git at all would mean the guard ran
/// too late.
#[tokio::test]
async fn reproduce_refuses_a_trigger_intake_failure_instead_of_synthesizing_a_request() {
    let (mut failure, _req) = checkout_failure();
    failure.intake = Intake::Trigger { source: "hyperdx".into() };
    failure.sample.request = None;
    failure.sample.intake = failure.intake.clone();
    let attribution = Attribution {
        deploy: DeployRecord { sha: "0123456789abcdef0123456789abcdef01234567".into(), description: "d".into(), author: "a".into(), deployed_at_ms: 1_000 },
        overlap_files: vec!["server.js".into()],
        minutes_after_deploy: 0,
        claim: Claim { text: "t".into(), provenance: Provenance::Inferred },
    };
    let err = LocalProcessReproducer { boot_timeout_ms: 10_000, boot_cmd: None }
        .reproduce(std::path::Path::new("/nonexistent-repo-drums-intake-test"), &failure, &attribution)
        .await
        .expect_err("reproduction must refuse a failure with no replayable request");
    assert!(
        matches!(err, engine_repro::ReproError::NotReplayable { .. }),
        "expected a typed NotReplayable refusal, got: {err}"
    );
    assert!(err.to_string().contains("hyperdx"), "the refusal must name the intake source: {err}");
}

/// A trigger failure that happens to carry a RECONSTRUCTED request (an OTel
/// adapter can often recover a method and a path from span attributes) is still
/// refused — that is not the request that failed, and replaying it would prove
/// nothing while looking exactly like proof.
#[tokio::test]
async fn reproduce_refuses_a_trigger_failure_even_when_a_reconstructed_request_is_attached() {
    let (mut failure, _req) = checkout_failure();
    failure.intake = Intake::Trigger { source: "otel".into() };
    let attribution = Attribution {
        deploy: DeployRecord { sha: "0123456789abcdef0123456789abcdef01234567".into(), description: "d".into(), author: "a".into(), deployed_at_ms: 1_000 },
        overlap_files: vec!["server.js".into()],
        minutes_after_deploy: 0,
        claim: Claim { text: "t".into(), provenance: Provenance::Inferred },
    };
    let err = LocalProcessReproducer { boot_timeout_ms: 10_000, boot_cmd: None }
        .reproduce(std::path::Path::new("/nonexistent-repo-drums-intake-test"), &failure, &attribution)
        .await
        .expect_err("an attached request must not buy replayability for a trigger intake");
    assert!(matches!(err, engine_repro::ReproError::NotReplayable { .. }), "got: {err}");
}

// -- real apps: signature from process stderr + `--boot-cmd` -------------
//
// The pilot blocker this branch exists to fix: the demo app above leaks a
// structured `{"error":{...}}` JSON body on failure, which is NOT what a
// real app does. FastAPI (no custom exception handler, `debug=False`)
// returns a bare `"Internal Server Error"` and logs the traceback to
// stderr only. Before this fix, `signature_from_body` found nothing in
// that bare body, `ErrorSignature::matches` correctly refused to match two
// empty signatures, and the failure came back `unresolved` — reproduction,
// the product's core claim, was unreachable on a real app.

/// Finds a `python3` on `PATH` with `fastapi` and `uvicorn` importable.
/// `None` (rather than a hard test failure) when either is missing, so this
/// suite degrades gracefully on a machine that hasn't `pip install`-ed
/// them — an environment gap unrelated to this crate's own correctness.
fn python3_with_fastapi() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let python = std::env::split_paths(&path).map(|dir| dir.join("python3")).find(|p| p.is_file())?;
    let ok = Command::new(&python).args(["-c", "import fastapi, uvicorn"]).output().ok()?.status.success();
    ok.then_some(python)
}

/// A FastAPI app shaped exactly like the pilot's real failure: `/health`
/// returns 200, `POST /api/checkout` divides by zero. FastAPI's default
/// handling of that uncaught exception is a bare `"Internal Server Error"`
/// body — the traceback goes to stderr, never to the client.
fn write_fastapi_fixture(repo: &Path) {
    std::fs::create_dir_all(repo.join("app")).unwrap();
    std::fs::write(
        repo.join("app/main.py"),
        "from fastapi import FastAPI\n\napp = FastAPI()\n\n\n@app.get(\"/health\")\ndef health():\n    return {\"ok\": True}\n\n\n@app.post(\"/api/checkout\")\ndef checkout():\n    total = 10\n    count = 0\n    return {\"total\": total / count}\n",
    )
    .unwrap();
}

/// Correctness bar (task spec): a FastAPI-shaped app that returns a bare
/// 500 while printing a traceback to stderr must produce a signature that
/// MATCHES the one the detector derives from the live app's OWN reported
/// traceback. `live_sig` below stands in for that detector-side signature —
/// computed the same way `engine_detect::Detector::observe` computes it
/// (`ErrorSignature::from_error` on the app's self-reported name/message/
/// stack), at a DIFFERENT app_root than the one reproduction's worktree
/// will use, to prove the match survives that difference the way it must
/// in production (the live deploy and a fresh worktree checkout are never
/// at the same absolute path).
#[tokio::test]
async fn fastapi_bare_500_reproduces_via_stderr_signature_and_boot_cmd() {
    let Some(python) = python3_with_fastapi() else {
        eprintln!(
            "skipping fastapi_bare_500_reproduces_via_stderr_signature_and_boot_cmd: no python3 with fastapi+uvicorn importable on PATH"
        );
        return;
    };

    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("pilot");
    init_repo(&repo);
    write_fastapi_fixture(&repo);
    run_git(&repo, &["add", "."]);
    run_git(&repo, &["commit", "-qm", "checkout: divide by zero on empty cart"]);
    let sha = rev_parse_head(&repo);

    let live_stack = "Traceback (most recent call last):\n  File \"/srv/pilot/app/main.py\", line 13, in checkout\n    return {\"total\": total / count}\nZeroDivisionError: division by zero\n";
    let live_sig = ErrorSignature::from_error("ZeroDivisionError", "division by zero", live_stack, "/srv/pilot");

    let failure = Failure {
        id: "f1".into(),
        service: "pilot".into(),
        signature: live_sig,
        first_seen_ms: 2_000,
        event_count: 1,
        sample: ErrorEvent {
            service: "pilot".into(),
            occurred_at_ms: 2_000,
            error_name: "ZeroDivisionError".into(),
            error_message: "division by zero".into(),
            stack: live_stack.into(),
            request: Some(CapturedRequest { method: "POST".into(), path: "/api/checkout".into(), content_type: Some("application/json".into()), body: Some("{}".into()) }),
            intake: Intake::Snippet,
        },
        intake: Intake::Snippet,
        claim: Claim { text: "t".into(), provenance: Provenance::Observed },
    };
    let attribution = Attribution {
        deploy: DeployRecord { sha: sha.clone(), description: "checkout: divide by zero on empty cart".into(), author: "pilot".into(), deployed_at_ms: 1_000 },
        overlap_files: vec!["app/main.py".into()],
        minutes_after_deploy: 0,
        claim: Claim { text: "t".into(), provenance: Provenance::Inferred },
    };

    let boot_cmd = format!("{} -m uvicorn app.main:app --host 127.0.0.1 --port {{port}}", python.display());
    let r = LocalProcessReproducer { boot_timeout_ms: 20_000, boot_cmd: Some(boot_cmd) }
        .reproduce(&repo, &failure, &attribution)
        .await
        .expect("reproduction runs against a real uvicorn boot");

    assert!(r.reproduced, "a FastAPI app returning a bare 500 while logging a traceback to stderr must still reproduce: {}", r.detail);
    assert!(
        r.claims.iter().any(|c| c.provenance == Provenance::Verified && c.text.contains("ZeroDivisionError")),
        "must earn a Verified claim naming the real exception (from stderr, since the body is opaque), not silently fall to unresolved: {:?}",
        r.claims
    );
}

/// Writes a fake "boot" script that (1) logs its own received argv, one
/// element per line, to `log_path` — the same technique `ship.rs`'s
/// `write_fake_deploy_script` uses to observe EXACTLY what a real
/// (non-shell) `Command` invocation produced — and (2) answers 2xx on any
/// path so `boot_assigned`'s readiness poll can succeed without a real
/// framework.
fn write_fake_boot_script(dir: &Path, log_path: &Path) -> PathBuf {
    let script_path = dir.join("fake-boot.js");
    let content = format!(
        "const fs = require('fs');\nconst http = require('http');\nconst args = process.argv.slice(2);\nfs.writeFileSync('{log}', args.join('\\n') + '\\n');\nconst port = Number(args[0]);\nhttp.createServer((req, res) => {{ res.writeHead(200); res.end('ok'); }}).listen(port);\n",
        log = log_path.display(),
    );
    std::fs::write(&script_path, content).unwrap();
    script_path
}

/// `--boot-cmd`'s injection test (task spec): a `;` embedded in the
/// template must arrive at the child as an inert argv element, never as a
/// shell command separator — there is no shell. Mirrors `ship.rs`'s
/// `ship_deploy_cmd_argv_is_never_shell_interpreted` exactly: a shell that
/// (wrongly) interpreted the template would run `rm -rf <marker_dir>` as a
/// SEPARATE command, deleting it.
#[tokio::test]
async fn boot_cmd_argv_is_never_shell_interpreted() {
    let scripts_dir = tempfile::tempdir().unwrap();
    let log_path = scripts_dir.path().join("boot.log");
    let script = write_fake_boot_script(scripts_dir.path(), &log_path);
    let marker_dir = scripts_dir.path().join("must-not-be-deleted");
    std::fs::create_dir_all(&marker_dir).unwrap();
    std::fs::write(marker_dir.join("canary.txt"), "still here").unwrap();

    let template = format!("node {} {{port}} ; rm -rf {}", script.display(), marker_dir.display());
    let dir = tempfile::tempdir().unwrap();
    let app = BootedApp::boot_with_cmd(dir.path(), 10_000, Some(&template)).await.expect("boot must succeed (the fake script always answers 2xx)");

    assert!(marker_dir.exists(), "a shell-interpreted `; rm -rf` would have deleted this");
    assert!(marker_dir.join("canary.txt").exists());

    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(lines.first().copied(), Some(app.port.to_string().as_str()), "the FIRST argv element must be the substituted port, not the literal \"{{port}}\": {log}");
    assert!(lines.contains(&";"), "the literal \";\" must arrive as its own inert argv element: {log}");
    assert!(lines.contains(&"rm"));
    assert!(lines.contains(&"-rf"));

    drop(app);
}

// -- C1/C2/C3 round-2 review regressions ----------------------------------

/// A FastAPI app shaped exactly like [`write_fastapi_fixture`] (dividing by
/// zero on checkout), but with a background thread that writes an ordinary
/// `Word: text`-shaped line to stderr AFTER the traceback — the exact
/// mechanism review C1 proved hijacks `error_name`.
fn write_fastapi_fixture_with_heartbeat_noise(repo: &Path) {
    std::fs::create_dir_all(repo.join("app")).unwrap();
    std::fs::write(
        repo.join("app/main.py"),
        "import sys\nimport threading\nimport time\n\nfrom fastapi import FastAPI\n\napp = FastAPI()\n\n\ndef heartbeat():\n    while True:\n        time.sleep(0.08)\n        sys.stderr.write(\"ConnectionError: redis heartbeat failed, retrying\\n\")\n        sys.stderr.flush()\n\n\nthreading.Thread(target=heartbeat, daemon=True).start()\n\n\n@app.get(\"/health\")\ndef health():\n    return {\"ok\": True}\n\n\n@app.post(\"/api/checkout\")\ndef checkout():\n    total = 10\n    count = 0\n    return {\"total\": total / count}\n",
    )
    .unwrap();
}

/// C1 (Critical), proven live: a real FastAPI app raising `ZeroDivisionError`
/// alongside a background thread that keeps writing an ordinary
/// `ConnectionError: ...` line to stderr AFTER the traceback must still
/// reproduce as `ZeroDivisionError` — never as the heartbeat's line, which
/// review proved got picked up as a `Verified` claim naming an exception
/// the replay never raised.
#[tokio::test]
async fn fastapi_traceback_is_not_hijacked_by_a_heartbeat_threads_logging() {
    let Some(python) = python3_with_fastapi() else {
        eprintln!("skipping fastapi_traceback_is_not_hijacked_by_a_heartbeat_threads_logging: no python3 with fastapi+uvicorn importable on PATH");
        return;
    };

    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("pilot");
    init_repo(&repo);
    write_fastapi_fixture_with_heartbeat_noise(&repo);
    run_git(&repo, &["add", "."]);
    run_git(&repo, &["commit", "-qm", "checkout: divide by zero, plus a noisy heartbeat"]);
    let sha = rev_parse_head(&repo);

    let live_stack = "Traceback (most recent call last):\n  File \"/srv/pilot/app/main.py\", line 20, in checkout\n    return {\"total\": total / count}\nZeroDivisionError: division by zero\n";
    let live_sig = ErrorSignature::from_error("ZeroDivisionError", "division by zero", live_stack, "/srv/pilot");

    let failure = Failure {
        id: "f1".into(),
        service: "pilot".into(),
        signature: live_sig,
        first_seen_ms: 2_000,
        event_count: 1,
        sample: ErrorEvent {
            service: "pilot".into(),
            occurred_at_ms: 2_000,
            error_name: "ZeroDivisionError".into(),
            error_message: "division by zero".into(),
            stack: live_stack.into(),
            request: Some(CapturedRequest { method: "POST".into(), path: "/api/checkout".into(), content_type: Some("application/json".into()), body: Some("{}".into()) }),
            intake: Intake::Snippet,
        },
        intake: Intake::Snippet,
        claim: Claim { text: "t".into(), provenance: Provenance::Observed },
    };
    let attribution = Attribution {
        deploy: DeployRecord { sha: sha.clone(), description: "checkout: divide by zero, plus a noisy heartbeat".into(), author: "pilot".into(), deployed_at_ms: 1_000 },
        overlap_files: vec!["app/main.py".into()],
        minutes_after_deploy: 0,
        claim: Claim { text: "t".into(), provenance: Provenance::Inferred },
    };

    let boot_cmd = format!("{} -m uvicorn app.main:app --host 127.0.0.1 --port {{port}}", python.display());
    let r = LocalProcessReproducer { boot_timeout_ms: 20_000, boot_cmd: Some(boot_cmd) }
        .reproduce(&repo, &failure, &attribution)
        .await
        .expect("reproduction runs against a real uvicorn boot");

    assert!(r.reproduced, "the real ZeroDivisionError must still reproduce despite the heartbeat noise: {}", r.detail);
    assert!(
        r.claims.iter().any(|c| c.provenance == Provenance::Verified && c.text.contains("ZeroDivisionError")),
        "the Verified claim must name the real exception, never the heartbeat's ConnectionError: {:?}",
        r.claims
    );
    assert!(
        !r.claims.iter().any(|c| c.text.contains("ConnectionError")),
        "a heartbeat thread's ordinary logging must never hijack the reproduction's claimed exception name: {:?}",
        r.claims
    );
}

/// C2 (Critical) regression: `boot_announce` used to drop its
/// `BufReader<ChildStdout>` the instant boot returned, closing the read end
/// of the app's own stdout pipe. Any app with default per-request stdout
/// logging then died mid-replay with `Error: write EPIPE` — review measured
/// a fixture doing nothing but `console.log` per request dying on the
/// THIRD replay. Ten replays against such a fixture must all succeed.
#[tokio::test]
async fn booted_app_survives_many_requests_with_default_stdout_access_logging() {
    const SERVER_JS: &str = r#"
const http = require("http");
const server = http.createServer((req, res) => {
  console.log("access " + req.url);
  res.writeHead(200);
  res.end("ok");
});
server.listen(process.env.PORT || 0, () => {
  console.log("listening " + server.address().port);
});
"#;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("server.js"), SERVER_JS).unwrap();

    let app = BootedApp::boot(dir.path(), 10_000).await.expect("app must boot");
    let req = CapturedRequest { method: "GET".into(), path: "/".into(), content_type: None, body: None };
    for i in 0..10 {
        let (status, _) = app.replay(&req).await.unwrap_or_else(|e| panic!("replay {i} must not fail with a Drums-induced EPIPE crash: {e}"));
        assert_eq!(status, 200, "replay {i}");
    }
}

/// C3 (Major) regression, with the exact production constants: a response
/// can go out well before the framework finishes logging (Starlette's
/// `ServerErrorMiddleware` sends the response, then re-raises for the ASGI
/// server to log). `stderr_settled` must wait for USABLE evidence — not
/// merely for one quiet 50ms poll interval, which is the normal state right
/// after a response and returns long before a traceback logged 400ms later
/// (well inside the 1.5s bound) has landed.
#[tokio::test]
async fn stderr_settled_waits_for_a_traceback_logged_after_the_response_goes_out() {
    const SERVER_JS: &str = r#"
const http = require("http");
const server = http.createServer((req, res) => {
  res.writeHead(500, { "content-type": "text/plain" });
  res.end("Internal Server Error");
  setTimeout(() => {
    console.error("Error: cache miss\n    at cacheGet (" + __dirname + "/server.js:99:1)");
  }, 400);
});
server.listen(process.env.PORT || 0, () => {
  console.log("listening " + server.address().port);
});
"#;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("server.js"), SERVER_JS).unwrap();

    let app = BootedApp::boot(dir.path(), 10_000).await.expect("app must boot");
    let mark = app.stderr_mark();
    let req = CapturedRequest { method: "GET".into(), path: "/".into(), content_type: None, body: None };
    let (status, _body) = app.replay(&req).await.expect("replay must succeed");
    assert_eq!(status, 500);

    let stderr = app.stderr_settled(mark, &app.app_root.clone(), Duration::from_millis(1_500), Duration::from_millis(50)).await;
    assert!(stderr.contains("Error: cache miss"), "must wait past the quiet period right after the response for a traceback logged 400ms later, well within the 1.5s bound: got {stderr:?}");
}
