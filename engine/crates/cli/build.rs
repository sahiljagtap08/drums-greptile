//! Bakes the commit a binary was built from into the binary.
//!
//! # Why this exists
//!
//! On 2026-08-09 the published `drums-darwin-arm64` turned out to be built from
//! 2026-07-31 and the published `drums-linux-x86_64` from 2026-08-01 — two
//! different revisions, shipped as one release, one of them missing
//! `--dispatch-repairs` entirely. So `drums watch --dispatch-repairs`, the
//! entire hosted path, answered `error: unexpected argument`.
//!
//! None of that was detectable. `drums --version` did not exist, the binaries
//! carried no provenance, and the release script's verification step ran
//! `drums watch --help` — which passes perfectly well on a binary missing the
//! flag the product depends on.
//!
//! A build that cannot say where it came from cannot be debugged from a bug
//! report, and a support conversation that opens with "which version are you
//! on" and has no answer is a conversation about nothing.
//!
//! # What it records, and what each field is worth
//!
//! `DRUMS_BUILD_SHA` is the only load-bearing one: it identifies the source
//! exactly. `DRUMS_BUILD_DIRTY` marks a build made from a modified tree, which
//! makes the sha a lie by omission unless it is said out loud. `DRUMS_BUILD_TARGET`
//! is what catches the failure above — two artifacts in one release whose shas
//! disagree.
//!
//! # Why the fallbacks are honest strings, not empty
//!
//! A source tarball has no `.git`, and a release runner may build from an
//! export. In that case the sha is genuinely unknown, and `unknown` is the
//! correct thing to print — never a fabricated value and never a silently
//! blank field, which would read as "no commit" rather than "not recorded".

use std::process::Command;

fn main() {
    // Re-run when HEAD moves. Without this, an incremental build keeps the sha
    // from whenever the crate last compiled, which is worse than no sha at
    // all: it is a confident wrong answer.
    println!("cargo:rerun-if-changed=../../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../../.git/refs");

    let sha = git(&["rev-parse", "--short=12", "HEAD"]).unwrap_or_else(|| "unknown".to_string());

    // `--porcelain` is empty exactly when the tracked tree is clean. Untracked
    // files are deliberately not counted: a scratch file beside the source did
    // not go into the binary.
    //
    // Neither did the previously-published binaries sitting in
    // website/public/releases. Excluding them is not a loosening of the check —
    // it is the same claim, stated correctly. "Dirty" has to mean "the SOURCE
    // differs from the commit this binary reports", and a release necessarily
    // stages the linux artifact before building the darwin ones, so counting
    // that as source made every release binary report `-dirty` and the release
    // script refuse itself. The one release that gets built is exactly the one
    // that cannot be built.
    // `:(top)` is load-bearing and the obvious spelling is wrong. build.rs runs
    // with the CRATE as its working directory, so a bare `.` pathspec scopes the
    // check to engine/crates/cli — meaning an edit to engine/crates/core would
    // not register as dirty and the binary would report a clean sha it does not
    // have. `top` anchors both halves at the repository root instead. Verified
    // by editing core and confirming this form sees it and `.` does not.
    let dirty = match git(&[
        "status",
        "--porcelain",
        "--untracked-files=no",
        "--",
        ":(top)",
        ":(exclude,top)website/public/releases",
    ]) {
        Some(out) if !out.trim().is_empty() => "-dirty",
        Some(_) => "",
        // Not a git tree at all — say nothing rather than assert cleanliness
        // that was never established.
        None => "",
    };

    println!("cargo:rustc-env=DRUMS_BUILD_SHA={sha}{dirty}");
    println!(
        "cargo:rustc-env=DRUMS_BUILD_TARGET={}",
        std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_string())
    );
}

/// Runs a git command, returning `None` for anything that is not a clean
/// success — git missing, not a repository, a non-zero exit. Every one of those
/// means the same thing to the caller: this fact could not be established.
fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8(out.stdout).ok()?.trim().to_string())
}
