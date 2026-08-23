//! Full Stage-1 loop against the seeded demo app. Requires node >= 20 and git.
//! seed → watch (in-process) → deploy v1 → clean traffic → deploy v2 →
//! failing traffic → detect → attribute to v2 → reproduce → parent clean.

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

fn script(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../demo")
        .join(name)
}

/// RAII cleanup for the two fixed-port processes this test spins up: the
/// `drums watch` child and the deployed demo app (tracked by its pidfile).
/// Constructing this immediately after spawning `watch` means `Drop` runs
/// on ANY exit from the test -- including a panic unwind from an assertion
/// that fires before the normal end-of-test cleanup would otherwise run --
/// so an early panic can't orphan either process on ports 7797/7098 and
/// poison the next run of the suite on this machine.
struct WatchGuard {
    watch: std::process::Child,
    /// Recorded at construction time, before `deploy.sh` has written the
    /// file. `Drop` re-reads the path at drop time, so whatever is on disk
    /// then (or nothing, if we never got that far) is honored.
    pidfile: PathBuf,
}

impl Drop for WatchGuard {
    fn drop(&mut self) {
        let _ = self.watch.kill();
        let _ = self.watch.wait(); // reap: clippy::zombie_processes
        if let Ok(pid) = std::fs::read_to_string(&self.pidfile) {
            let _ = Command::new("kill").arg(pid.trim()).status();
        }
    }
}

#[tokio::test]
async fn stage1_full_loop() {
    let work = tempfile::tempdir().unwrap();
    let out = Command::new("bash")
        .arg(script("seed.sh"))
        .arg(work.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let get = |k: &str| {
        stdout
            .lines()
            .find_map(|l| l.strip_prefix(k))
            .unwrap()
            .to_string()
    };
    let (v1, v2) = (get("V1_SHA="), get("V2_SHA="));
    let repo = work.path().join("shop");

    // deploy.sh always runs the live app from a fixed `<repo>/.deploy`
    // clone (a separate working copy from the watched repo, so the deploy
    // step never mutates history `drums watch` is reading). That means the
    // live app's V8 stack traces carry that directory's *canonicalized*
    // absolute path — reproduction, by contrast, always boots from its own
    // freshly created worktree at a distinct path. Passing `--app-root`
    // tells the detector which prefix to strip from live traces so its
    // signature normalizes to the same repo-root-relative form
    // reproduction computes from its own worktree (e.g. "server.js"),
    // making the two signatures comparable. Canonicalize `repo` (which
    // exists) rather than `repo/.deploy` (which deploy.sh hasn't created
    // yet at watch-startup) to resolve any symlinked tmpdir prefix (macOS
    // `/var` -> `/private/var`) the same way `std::fs::canonicalize` does
    // inside the reproducer.
    let app_root = std::fs::canonicalize(&repo).unwrap().join(".deploy");

    // spawn `drums watch` as a real process (binary built by cargo)
    let bin = env!("CARGO_BIN_EXE_drums");
    let mut watch = Command::new(bin)
        .args([
            "watch",
            "--ingest-port",
            "7797",
            "--threshold",
            "3",
            "--repo",
        ])
        .arg(&repo)
        .arg("--app-root")
        .arg(&app_root)
        // `drums watch` requires an account (`crates/cli/src/account.rs`). This
        // test has no browser and no console to sign in to, which is precisely
        // the CI case the escape hatch exists for — the loop under test is the
        // local one and needs no account to run.
        .env("DRUMS_NO_ACCOUNT", "1")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // Take stdout before `watch` moves into the guard below -- the reader
    // thread needs its own handle on the pipe regardless of when cleanup runs.
    let mut stdout = watch.stdout.take().unwrap();
    let guard = WatchGuard {
        watch,
        pidfile: repo.join(".deploy.pid"),
    };

    // Read watch's stdout on a dedicated thread from the moment it's spawned
    // -- deploy.sh's final step POSTs to the ingest port, and we must not
    // race the listener actually accepting connections. A fixed sleep here
    // (however long) is inherently a guess about startup cost (binary size,
    // page cache warmth, machine load); instead wait for the banner line
    // `watch` itself prints only once its ingest listener is already bound
    // (main.rs: `println!("watching {repo} ...")` runs after `axum::serve`
    // is spawned on the already-bound listener). Lines seen before the
    // banner are still forwarded into `collected` so the later `contains(..)`
    // assertions see the full transcript regardless of what arrived first.
    let (line_tx, line_rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        use std::io::{BufRead, BufReader};
        for line in BufReader::new(&mut stdout).lines().map_while(Result::ok) {
            if line_tx.send(line).is_err() {
                break;
            }
        }
    });

    let mut collected = String::new();
    let startup_deadline = std::time::Instant::now() + Duration::from_secs(20);
    let mut ready = false;
    while std::time::Instant::now() < startup_deadline && !ready {
        match line_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(line) => {
                ready = line.starts_with("watching ");
                collected.push_str(&line);
                collected.push('\n');
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(_) => break,
        }
    }
    assert!(ready, "watch never printed its startup banner: {collected}");

    let deploy = |sha: &str| {
        let ok = Command::new("bash")
            .arg(script("deploy.sh"))
            .arg(&repo)
            .args([sha, "7098", "7797"])
            .status()
            .unwrap()
            .success();
        assert!(ok, "deploy {sha} failed");
    };
    let client = reqwest::Client::new();
    let checkout = |body: &'static str| {
        let client = client.clone();
        async move {
            client
                .post("http://127.0.0.1:7098/api/checkout")
                .header("content-type", "application/json")
                .body(body)
                .timeout(Duration::from_secs(5))
                .send()
                .await
                .unwrap()
                .status()
                .as_u16()
        }
    };

    deploy(&v1);
    assert_eq!(
        checkout(r#"{"items":[{"price":100,"qty":2}]}"#).await,
        200,
        "v1 healthy"
    );

    deploy(&v2);
    assert_eq!(
        checkout(r#"{"items":[{"price":100,"qty":2}],"promo":{"code":"TEN"}}"#).await,
        200,
        "v2 with promo still fine"
    );
    for _ in 0..3 {
        assert_eq!(
            checkout(r#"{"items":[{"price":100,"qty":2}]}"#).await,
            500,
            "v2 without promo fails"
        );
    }

    // The engine now detects, attributes, reproduces. Keep draining the same
    // reader thread/channel started right after spawn (above) into the same
    // `collected` buffer -- no second reader, no second take() of stdout.
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while std::time::Instant::now() < deadline && !collected.contains("reproduction confirmed") {
        match line_rx.recv_timeout(Duration::from_millis(500)) {
            Ok(line) => {
                collected.push_str(&line);
                collected.push('\n');
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(_) => break,
        }
    }
    // Explicit cleanup here (same point in the flow as before this fix):
    // kills `watch` and the deployed app via its pidfile. If any assertion
    // below panics, `guard` was never consumed, so `Drop` still runs during
    // unwind and cleanup still happens.
    drop(guard);

    assert!(collected.contains("shop failing"), "detected: {collected}");
    assert!(
        collected.contains("[observed]"),
        "observed chip: {collected}"
    );
    assert!(
        collected.contains(&v2[..6]),
        "attributed to v2: {collected}"
    );
    assert!(
        collected.contains("[inferred]"),
        "inferred chip: {collected}"
    );
    assert!(
        collected.contains("[verified]"),
        "verified chips: {collected}"
    );
    assert!(
        collected.contains("parent of"),
        "parent-clean claim: {collected}"
    );
    assert!(
        collected.contains("reproduction confirmed"),
        "full loop: {collected}"
    );
    // the record survives on disk
    let record = std::fs::read_to_string(repo.join(".drums/record.jsonl")).unwrap();
    assert!(record.lines().count() >= 5, "deploys + events recorded");
}
