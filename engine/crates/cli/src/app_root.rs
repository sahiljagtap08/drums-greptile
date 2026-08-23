//! `--app-root` auto-derivation (carried Stage-1 seam, spec §22): when the
//! flag is omitted, derive the deployment-boundary prefix to strip from a
//! live app's stack traces per failure, straight from the failure's own
//! first error event, rather than requiring it to be typed in manually.
//!
//! `--app-root` exists because a live app's stack traces (V8 or CPython)
//! carry WHEREVER-it-was-deployed-from's absolute path (e.g. a separate
//! `<repo>/.deploy` checkout, per `demo/deploy.sh`), while reproduction
//! computes signatures relative to its own freshly created worktree — the
//! two are only comparable once both are stripped down to the same
//! repo-root-relative form. Passing `--app-root` does that stripping inside
//! `engine_core::ErrorSignature::from_error`; this module reproduces the
//! same EFFECT without touching `engine-detect` or `engine-core` at all: it
//! runs upstream of the detector, rewriting each event's raw `stack` text so
//! that, by the time the (untouched) detector parses it with an empty
//! app_root, the result is identical to what a correctly-guessed
//! `--app-root` would have produced.
//!
//! Explicit `--app-root` always wins — this module is only ever consulted
//! by `main.rs` when the flag was omitted.
//!
//! py-traceback-review.md M1: this module used to scan V8 `at ` frames
//! only, so a Python service's app_root derivation always returned `None` —
//! the raw stack text was never stripped, the detector's signature kept the
//! deployment prefix, reproduction computed the repo-relative path, and
//! `matches()` was permanently false. [`raw_top_frame_path`] now mirrors
//! CPython tracebacks too, so `--app-root` auto-derivation covers the
//! FastAPI pilot the same way it already covers Node.

use std::collections::HashMap;
use std::path::Path;

use engine_core::ErrorEvent;

/// Per-raw-prefix memoization so the same deployment path doesn't re-hit the
/// filesystem on every event — most watch sessions see exactly one distinct
/// prefix for their whole lifetime, but nothing here assumes that.
pub struct AppRootCache {
    cache: HashMap<String, Option<String>>,
}

/// Bound on distinct keys held at once.
///
/// Closing round (carried M-d): the key is a path parsed out of `ev.stack`,
/// and `ev.stack` is whatever `POST /v1/events` was handed — arbitrary,
/// unvalidated, caller-controlled text. So the key space is caller-chosen,
/// and an unbounded map made a long-lived `drums watch` grow without limit
/// on a chatty or hostile reporter. 1024 distinct deployment prefixes is
/// already far past anything real (most sessions see exactly one).
const MAX_CACHE_ENTRIES: usize = 1024;

impl AppRootCache {
    pub fn new() -> Self {
        Self {
            cache: HashMap::new(),
        }
    }

    /// How many entries are memoized (test-visible so the bound can be
    /// asserted rather than described).
    pub fn len(&self) -> usize {
        self.cache.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cache.is_empty()
    }

    /// `raw_top_frame_file` is the UNSTRIPPED path exactly as it appeared in
    /// a stack frame (see [`raw_top_frame_path`] — deliberately NOT sourced
    /// via `ErrorSignature::from_error`, whose empty-`app_root` case still
    /// trims a leading `/` via `str::trim_start_matches`, which would make
    /// "was this path absolute at all" unanswerable here).
    pub fn derive(&mut self, raw_top_frame_file: &str, repo: &Path) -> Option<String> {
        if let Some(cached) = self.cache.get(raw_top_frame_file) {
            return cached.clone();
        }
        let result = derive_app_root_prefix(raw_top_frame_file, repo);
        // Closing round (M-d): memoization is a speed optimization, so
        // dropping it under flood is always safe — the derivation is pure with
        // respect to the filesystem and simply recomputes. Clearing wholesale
        // (rather than tracking recency) keeps this a few lines with no
        // eviction bookkeeping to get wrong; a workload with a real working
        // set under the bound never reaches it, and one that blows past 1024
        // distinct prefixes has no working set to preserve anyway.
        if self.cache.len() >= MAX_CACHE_ENTRIES {
            tracing::warn!(
                entries = self.cache.len(),
                "app_root prefix cache hit its bound; clearing (a reporter is sending unusually varied stack paths)"
            );
            self.cache.clear();
        }
        self.cache
            .insert(raw_top_frame_file.to_string(), result.clone());
        result
    }
}

impl Default for AppRootCache {
    fn default() -> Self {
        Self::new()
    }
}

/// The first application-frame path in a stack trace, exactly as it appears
/// (no app_root stripping — see [`AppRootCache::derive`]'s doc for why this
/// can't reuse `engine_core::ErrorSignature::from_error`). Detects V8 vs
/// CPython the same way `engine_core::ErrorSignature::from_error` does
/// (branch `scenario/py-traceback`, commit c4592ac,
/// `engine/crates/core/src/lib.rs` — NOT merged into this branch; see that
/// commit for the source of truth, and py-traceback-review.md M1/M2 for
/// why): V8 wins whenever any real `at ` frame anchor is present, even if
/// the stack also contains literal `File "..."` text elsewhere (e.g. a Node
/// service re-throwing a Python worker's captured stderr); otherwise, CPython
/// is used only when an actually-anchored `File "<path>", line <N>` header
/// exists — the bare substring `File "` is not enough (M5). Neither format
/// present falls back to `None`, matching the pre-existing "no application
/// frame" outcome.
fn raw_top_frame_path(stack: &str) -> Option<String> {
    if has_v8_frame(stack) {
        return raw_top_v8_frame_path(stack);
    }
    if stack
        .lines()
        .any(|line| parse_python_frame_line(line).is_some())
    {
        return raw_top_python_frame_path(stack);
    }
    None
}

/// True when `stack` contains a real V8 frame anchor. Mirrors engine-core's
/// format-detection precedence (see [`raw_top_frame_path`]'s doc) so a
/// polyglot stack with both `at ` frames and message-embedded `File "..."`
/// text is never mis-routed to the Python parser (M2).
fn has_v8_frame(stack: &str) -> bool {
    stack
        .lines()
        .any(|line| line.trim_start().starts_with("at "))
}

/// The V8 half of [`raw_top_frame_path`] — unchanged from before this
/// module gained CPython support. Mirrors
/// `engine_core::ErrorSignature::from_v8_stack`'s frame walk (skip
/// `node:`/`node_modules/` frames, parse `"fn (path:line:col)"` or
/// `"path:line:col"`, strip a `file://` scheme) without the final app_root
/// strip step.
///
/// Fix round (round-2 N1): the non-`at ` case must `continue`, exactly as
/// `from_error` does — it used to be `?`, which returned `None` from this
/// WHOLE function on the first non-frame line. `err.stack`'s MESSAGE is
/// routinely multi-line in ordinary Node (`AssertionError [ERR_ASSERTION]`'s
/// `actual:`/`expected:` lines, `JSON.parse`'s `SyntaxError` position
/// excerpt, `AggregateError`, any thrown `Error` whose message contains
/// `\n`, or simply a blank separator line), so `skip(1)` does not reliably
/// land on the first frame — and aborting there derived no prefix at all,
/// leaving the detector's signature as `srv/app/server.js` against
/// reproduction's `server.js`: a silent `could not reproduce [unresolved]`
/// with no `--app-root` for the user to blame, i.e. verbatim the failure
/// mode this module exists to eliminate.
fn raw_top_v8_frame_path(stack: &str) -> Option<String> {
    for line in stack.lines().skip(1) {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("at ") else {
            continue;
        };
        if rest.contains("node:") || rest.contains("node_modules/") {
            continue;
        }
        let loc = match rest.split_once(" (") {
            Some((_, l)) => l.trim_end_matches(')'),
            None => rest,
        };
        let file = loc.rsplitn(3, ':').nth(2).unwrap_or(loc);
        let file = file.strip_prefix("file://").unwrap_or(file);
        return Some(file.to_string());
    }
    None
}

/// The CPython half of [`raw_top_frame_path`] (py-traceback-review.md M1).
/// Mirrors `engine_core::ErrorSignature::from_python_traceback`'s frame walk
/// (same source commit as [`raw_top_frame_path`]'s doc) minus the final
/// app_root strip step, exactly parallel to how [`raw_top_v8_frame_path`]
/// mirrors `from_v8_stack`. CPython lists frames OUTERMOST-first (the
/// reverse of V8), so the relevant application frame is the LAST surviving
/// `File "..."` line — found by collecting frames then scanning in reverse,
/// skipping library frames along the way.
fn raw_top_python_frame_path(stack: &str) -> Option<String> {
    let frames: Vec<(&str, Option<&str>)> =
        stack.lines().filter_map(parse_python_frame_line).collect();
    for (file, _func) in frames.into_iter().rev() {
        if is_python_library_frame(file) {
            continue;
        }
        return Some(file.to_string());
    }
    None
}

/// Parses a trimmed CPython traceback frame header —
/// `File "/path/to/file.py", line 42, in func_name` — into
/// `(path, Some("func_name"))`. Returns `None` for anything that isn't a
/// real anchored frame header (code-context lines, the `Traceback (...)`
/// banner, the final exception line, or a message-embedded pseudo-frame
/// that merely starts with `File "..."` with no `, line <N>` behind it —
/// M5). The `, line <N>` component is mandatory for exactly that reason;
/// `, in <func>` stays optional since module-level frames spell it
/// `in <module>` and nothing guarantees a function name follows the line
/// number.
///
/// Faithful mirror of
/// `engine_core::ErrorSignature::parse_python_frame_line` (same source
/// commit as [`raw_top_frame_path`]'s doc) — duplicated rather than shared
/// because that commit's diff must not be touched from this branch. Keep in
/// sync if/when it lands.
fn parse_python_frame_line(line: &str) -> Option<(&str, Option<&str>)> {
    let line = line.trim();
    let rest = line.strip_prefix("File \"")?;
    let (path, rest) = rest.split_once('"')?;
    let rest = rest.strip_prefix(", line ")?;
    let digit_count = rest.chars().take_while(|c| c.is_ascii_digit()).count();
    if digit_count == 0 {
        return None;
    }
    let func = rest[digit_count..].strip_prefix(", in ").map(|f| f.trim());
    Some((path, func))
}

/// Mirrors `engine_core::ErrorSignature::is_python_library_frame` (same
/// source commit as [`raw_top_frame_path`]'s doc): any pseudo-file path
/// (`<string>`, `<stdin>`, `<frozen ...>`, and anything else CPython
/// synthesizes for exec()/eval()/frozen imports/the REPL — M3),
/// site-packages/dist-packages, the boundary-anchored standard library, and
/// importlib's bootstrap frames are never the application frame.
fn is_python_library_frame(path: &str) -> bool {
    if path.starts_with('<') {
        return true;
    }
    path.contains("/site-packages/")
        || path.contains("/dist-packages/")
        || is_stdlib_python_path(path)
        || is_importlib_frame(path)
}

/// Matches the interpreter's own standard-library layout —
/// `/lib/python<version>/...` or `/lib64/python<version>/...` — anchored on
/// a digit immediately after `python` so a real application directory that
/// merely contains the literal path segment `lib/python/` (a legitimate
/// polyglot layout, e.g. `<repo>/lib/python/dispatch.py`) is not mistaken
/// for the stdlib (M4).
fn is_stdlib_python_path(path: &str) -> bool {
    ["/lib/python", "/lib64/python"].into_iter().any(|marker| {
        path.match_indices(marker)
            .any(|(idx, _)| path[idx + marker.len()..].starts_with(|c: char| c.is_ascii_digit()))
    })
}

/// `importlib` only counts as bootstrap machinery when it is a real path
/// component (e.g. `.../importlib/_bootstrap_external.py`) — a bare
/// substring match also fires on an application file merely *named* with
/// that word, like `api/importlib_compat.py` (M4). `<frozen importlib...>`
/// frames are already caught by the pseudo-file check above, since they
/// start with `<`.
fn is_importlib_frame(path: &str) -> bool {
    path.split('/').any(|segment| segment == "importlib")
}

/// Whether `suffix` (a path relative to `repo`) is tracked by git in `repo`.
/// This is the real discriminator between a suffix that merely happens to
/// exist on disk (e.g. `<repo>/.deploy/server.js`, a live nested deploy
/// checkout — present on disk but never committed) and the actual
/// repo-relative source path (`server.js`, tracked). Fix round, I5: "deepest
/// strip wins" alone is not sound — see [`derive_app_root_prefix`]'s doc.
fn is_git_tracked(repo: &Path, suffix: &str) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["ls-files", "--error-unmatch", "--"])
        .arg(suffix)
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// Strip leading path components of `raw_top_frame_file` one at a time and
/// collect every suffix that (a) exists as a file under `repo` AND (b) is
/// git-tracked there. Only absolute paths carry a deployment-boundary prefix
/// worth stripping; a relative path returns `None` immediately (falls back
/// to no stripping, matching prior — no `--app-root` — behavior exactly).
///
/// Fix round, I5: the prior rule ("deepest strip — shortest matching suffix
/// — wins, checked by mere file existence") fixed the case it was written
/// for (`demo/deploy.sh`'s `<repo>/.deploy/server.js`, a live NESTED checkout
/// of the same repo: the shortest suffix `server.js` happens to also match
/// the tracked file at the repo root, so the search stopped there
/// immediately and got it right) but introduces a SYMMETRIC wrong answer
/// whenever a stack file's basename genuinely exists at more than one real,
/// tracked, repo-relative path: with both `index.js` (repo root) and
/// `src/index.js` tracked, a live frame `/srv/app/src/index.js` would derive
/// prefix `/srv/app/src` (matching the WRONG file, root `index.js`, purely
/// because it's shorter) and a signature of `index.js` — reproduction
/// (stripping its own worktree path) computes `src/index.js`, the two
/// signatures never match, and the result is a silent `could not reproduce
/// [unresolved]` with no `--app-root` for the user to blame.
///
/// The real discriminator is git-tracked-ness, not depth (`<repo>/.deploy/server.js`
/// is untracked; the real `server.js` is tracked) — restricting candidates to
/// tracked suffixes makes the nested-checkout case correct for the right
/// reason instead of by iteration-order coincidence, AND makes the
/// `index.js`-at-two-depths case detectable: if MORE THAN ONE tracked
/// suffix matches, the choice is genuinely ambiguous from the stack path
/// alone, and this returns `None` — the same honest, already-documented
/// "nothing derived, falls back to no stripping" outcome as no match at
/// all — rather than confidently guessing wrong.
fn derive_app_root_prefix(raw_top_frame_file: &str, repo: &Path) -> Option<String> {
    if !raw_top_frame_file.starts_with('/') {
        return None;
    }
    let components: Vec<&str> = raw_top_frame_file
        .trim_start_matches('/')
        .split('/')
        .collect();
    let mut matches: Vec<String> = Vec::new();
    for i in (0..components.len()).rev() {
        let suffix = components[i..].join("/");
        if suffix.is_empty() {
            continue;
        }
        if repo.join(&suffix).is_file() && is_git_tracked(repo, &suffix) {
            matches.push(format!("/{}", components[..i].join("/")));
        }
    }
    match matches.len() {
        1 => matches.pop(),
        _ => None,
    }
}

/// Remove every occurrence of `prefix` immediately followed by `/` from
/// `stack` — the same effect passing `--app-root <prefix>` has on a single
/// parsed frame inside `ErrorSignature::from_error`, applied here to the
/// raw stack TEXT before the event ever reaches the detector. A no-op when
/// `prefix` is empty (nothing was derived).
pub fn strip_prefix_from_stack(stack: &str, prefix: &str) -> String {
    if prefix.is_empty() {
        return stack.to_string();
    }
    let with_trailing_slash = format!("{prefix}/");
    stack.replace(&with_trailing_slash, "")
}

/// Derive (and cache) the app_root prefix for one event's stack, or `None`
/// if nothing under `repo` matched any suffix of its top application frame.
pub fn derive_for_event(cache: &mut AppRootCache, ev: &ErrorEvent, repo: &Path) -> Option<String> {
    derive_for_stack(cache, &ev.stack, repo)
}

/// The stack-text-only form of [`derive_for_event`]. Exists so the caller can
/// hand just the `String` it needs to a `spawn_blocking` closure (closing
/// round, M-d) instead of moving a whole `ErrorEvent` in and back out: this
/// derivation shells out to `git ls-files`, which must not run on an async
/// runtime worker thread.
pub fn derive_for_stack(cache: &mut AppRootCache, stack: &str, repo: &Path) -> Option<String> {
    let raw = raw_top_frame_path(stack)?;
    cache.derive(&raw, repo)
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_core::CapturedRequest;

    /// `git init`s `dir` and commits (`git add -A`) whatever files are
    /// already on disk under it — used so [`is_git_tracked`] has a real git
    /// repo to check against (fix round, I5: tracked-ness is now the
    /// discriminator `derive_app_root_prefix` uses).
    fn commit_all(dir: &Path) {
        let run = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed in {}", dir.display());
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "c1"]);
    }

    fn write_fixture_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("lib/cart")).unwrap();
        std::fs::write(dir.path().join("lib/cart/total.js"), "// total").unwrap();
        std::fs::write(dir.path().join("server.js"), "// server").unwrap();
        commit_all(dir.path());
        dir
    }

    // -- raw_top_frame_path ---------------------------------------------------

    #[test]
    fn raw_top_frame_path_keeps_the_leading_slash_unlike_empty_app_root_stripping() {
        let stack =
            "TypeError: boom\n    at computeTotal (/tmp/xyz/.deploy/lib/cart/total.js:14:31)";
        assert_eq!(
            raw_top_frame_path(stack).as_deref(),
            Some("/tmp/xyz/.deploy/lib/cart/total.js")
        );
    }

    #[test]
    fn raw_top_frame_path_skips_node_internal_and_node_modules_frames() {
        let stack = "TypeError: boom\n    at node:internal/x:1:1\n    at wrap (/x/node_modules/express/lib/router.js:5:1)\n    at handler (/x/server.js:22:5)";
        assert_eq!(raw_top_frame_path(stack).as_deref(), Some("/x/server.js"));
    }

    #[test]
    fn raw_top_frame_path_strips_file_scheme_but_not_the_leading_slash() {
        let stack = "TypeError: boom\n    at handler (file:///srv/shop/server.js:22:5)";
        assert_eq!(
            raw_top_frame_path(stack).as_deref(),
            Some("/srv/shop/server.js")
        );
    }

    #[test]
    fn raw_top_frame_path_none_when_no_application_frame_exists() {
        let stack = "TypeError: boom\n    at node:internal/x:1:1";
        assert_eq!(raw_top_frame_path(stack), None);
    }

    /// N2/round-2 N1 (CONFIRMED by the reviewer's side-by-side probe): a
    /// multi-line `err.stack` MESSAGE (ordinary Node — `ERR_ASSERTION`'s
    /// `actual:`/`expected:` lines, `JSON.parse`'s `SyntaxError` position
    /// excerpt, `AggregateError`, any thrown `Error` whose message contains
    /// `\n`) puts non-`at ` lines BETWEEN the first line and the first real
    /// frame. `?` on `strip_prefix("at ")` aborted the whole walk there and
    /// derived nothing, while `engine_core::ErrorSignature::from_error` —
    /// the function this one's doc claims to mirror — `continue`s past them
    /// and finds the frame.
    #[test]
    fn raw_top_frame_path_walks_past_a_multi_line_error_message() {
        let stack = "AssertionError [ERR_ASSERTION]: boom\n  actual: 1\n  expected: 2\n    at handler (/srv/app/server.js:10:5)";
        assert_eq!(
            raw_top_frame_path(stack).as_deref(),
            Some("/srv/app/server.js")
        );
    }

    #[test]
    fn raw_top_frame_path_walks_past_a_blank_separator_line() {
        let stack = "TypeError: boom\n\n    at handler (/srv/app/server.js:10:5)";
        assert_eq!(
            raw_top_frame_path(stack).as_deref(),
            Some("/srv/app/server.js")
        );
    }

    /// The doc-comment equivalence this module asserts ("mirrors
    /// `ErrorSignature::from_error`'s frame walk"), pinned as a real
    /// side-by-side assertion on the exact input class that used to break it:
    /// stripping the derived prefix from the raw stack must leave the
    /// detector computing the SAME `top_frame_file` a correctly-typed
    /// `--app-root` would have produced.
    #[test]
    fn derivation_and_core_agree_on_a_multi_line_message_stack() {
        let stack = "AssertionError [ERR_ASSERTION]: boom\n  actual: 1\n  expected: 2\n    at handler (/srv/app/server.js:10:5)";
        let raw =
            raw_top_frame_path(stack).expect("must find the frame past the multi-line message");
        assert_eq!(raw, "/srv/app/server.js");
        // What an explicitly-typed `--app-root /srv/app` yields:
        let explicit =
            engine_core::ErrorSignature::from_error("AssertionError", "boom", stack, "/srv/app");
        assert_eq!(explicit.top_frame_file, "server.js");
        // What the derived-and-stripped stack yields with an EMPTY app_root:
        let stripped = strip_prefix_from_stack(stack, "/srv/app");
        let derived =
            engine_core::ErrorSignature::from_error("AssertionError", "boom", &stripped, "");
        assert_eq!(derived.top_frame_file, explicit.top_frame_file);
    }

    // -- derive_app_root_prefix -----------------------------------------------

    #[test]
    fn derives_the_deploy_prefix_when_the_relative_suffix_exists_under_repo() {
        let repo = write_fixture_repo();
        let raw = "/tmp/somewhere/.deploy/lib/cart/total.js";
        let prefix = derive_app_root_prefix(raw, repo.path()).expect("must derive a prefix");
        assert_eq!(prefix, "/tmp/somewhere/.deploy");
    }

    #[test]
    fn derives_the_deepest_stripped_prefix_that_matches() {
        // server.js exists at repo root — the only real match here, at the
        // deepest possible strip (down to the bare filename).
        let repo = write_fixture_repo();
        let raw = "/opt/app/current/server.js";
        let prefix = derive_app_root_prefix(raw, repo.path()).expect("must derive a prefix");
        assert_eq!(prefix, "/opt/app/current");
    }

    #[test]
    fn falls_back_to_none_when_nothing_matches_under_repo() {
        let repo = write_fixture_repo();
        let raw = "/tmp/somewhere/unrelated/nope.js";
        assert_eq!(derive_app_root_prefix(raw, repo.path()), None);
    }

    #[test]
    fn falls_back_to_none_for_a_relative_path() {
        let repo = write_fixture_repo();
        assert_eq!(
            derive_app_root_prefix("lib/cart/total.js", repo.path()),
            None
        );
    }

    /// Real-world shape from `demo/deploy.sh`: the "deploy" checkout lives
    /// INSIDE the watched repo itself (`<repo>/.deploy`), so the live app's
    /// absolute stack path is `<repo>/.deploy/server.js` — and `<repo>` ALSO
    /// has its own git-tracked `server.js` at its root, while the nested
    /// `.deploy/server.js` copy is a live checkout that was never committed
    /// (untracked). Fix round, I5: the discriminator is tracked-ness, not
    /// depth — `.deploy/server.js` exists on disk but is filtered out for
    /// not being git-tracked, so the only remaining (and correct) candidate
    /// is the real, tracked `server.js` at the repo root, giving a prefix
    /// stripped all the way through `.deploy`.
    #[test]
    fn prefers_the_tracked_match_over_a_coincidental_untracked_nested_checkout_match() {
        let repo = write_fixture_repo();
        // `.deploy` is a nested checkout of the SAME repo, living inside it —
        // exactly what `demo/deploy.sh` does — created AFTER the initial
        // commit and never `git add`ed, so it stays untracked.
        std::fs::create_dir_all(repo.path().join(".deploy")).unwrap();
        std::fs::write(repo.path().join(".deploy/server.js"), "// nested live copy").unwrap();

        let raw = format!("{}/.deploy/server.js", repo.path().display());
        let prefix = derive_app_root_prefix(&raw, repo.path()).expect("must derive a prefix");
        assert_eq!(prefix, format!("{}/.deploy", repo.path().display()), "must strip all the way through .deploy to the real TRACKED relative path, not stop at the untracked nested coincidental match");
    }

    /// I5 (CONFIRMED by the reviewer as the symmetric failure "deepest strip
    /// wins" introduces): a stack file's basename can genuinely exist,
    /// TRACKED, at more than one repo-relative depth — here both `index.js`
    /// (repo root) and `src/index.js`. A live frame of
    /// `/srv/app/src/index.js` is ambiguous from the path alone: it could be
    /// the app deployed with `--app-root /srv/app/src` (signature
    /// `index.js`) or with `--app-root /srv/app` (signature `src/index.js`).
    /// Guessing either one wrong silently breaks reproduction's signature
    /// match later with no flag for the user to blame — the honest answer
    /// is to derive nothing, matching the "no match at all" fallback.
    #[test]
    fn derives_nothing_when_more_than_one_tracked_suffix_matches_rather_than_guess() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("index.js"), "// root entry").unwrap();
        std::fs::write(dir.path().join("src/index.js"), "// src entry").unwrap();
        commit_all(dir.path());

        let raw = "/srv/app/src/index.js";
        assert_eq!(
            derive_app_root_prefix(raw, dir.path()),
            None,
            "an ambiguous stack path (both index.js and src/index.js are tracked) must derive nothing, not guess"
        );
    }

    // -- strip_prefix_from_stack -----------------------------------------------

    #[test]
    fn strip_prefix_from_stack_removes_the_prefix_from_every_frame_sharing_it() {
        let stack = "TypeError: boom\n    at computeTotal (/tmp/x/.deploy/lib/cart/total.js:14:31)\n    at Server.handle (/tmp/x/.deploy/server.js:40:9)";
        let out = strip_prefix_from_stack(stack, "/tmp/x/.deploy");
        assert_eq!(out, "TypeError: boom\n    at computeTotal (lib/cart/total.js:14:31)\n    at Server.handle (server.js:40:9)");
    }

    #[test]
    fn strip_prefix_from_stack_is_a_no_op_for_an_empty_prefix() {
        let stack = "TypeError: boom\n    at f (/x/server.js:1:1)";
        assert_eq!(strip_prefix_from_stack(stack, ""), stack);
    }

    // -- AppRootCache: caching + end-to-end derive_for_event --------------------

    #[test]
    fn cache_memoizes_so_a_second_lookup_for_the_same_raw_path_skips_the_filesystem_check() {
        let mut cache = AppRootCache::new();
        // Pre-populate as if a prior filesystem check had already run —
        // proves the cached value is returned rather than recomputed
        // against a `repo` path that (being nonexistent) would otherwise
        // always yield `None`.
        cache.cache.insert(
            "/tmp/x/.deploy/server.js".to_string(),
            Some("/tmp/x/.deploy".to_string()),
        );
        let bogus_repo = Path::new("/definitely/does/not/exist");
        assert_eq!(
            cache
                .derive("/tmp/x/.deploy/server.js", bogus_repo)
                .as_deref(),
            Some("/tmp/x/.deploy")
        );
    }

    fn error_event_with_stack(stack: &str) -> ErrorEvent {
        ErrorEvent {
            service: "shop".into(),
            occurred_at_ms: 1,
            error_name: "TypeError".into(),
            error_message: "boom".into(),
            stack: stack.into(),
            request: Some(CapturedRequest {
                method: "POST".into(),
                path: "/api/checkout".into(),
                content_type: None,
                body: None,
            }),
            intake: engine_core::Intake::Snippet,
        }
    }

    #[test]
    fn derive_for_event_end_to_end_with_a_deploy_shaped_absolute_stack() {
        let repo = write_fixture_repo();
        let mut cache = AppRootCache::new();
        let ev = error_event_with_stack(
            "TypeError: boom\n    at computeTotal (/var/deploy/shop/.deploy/lib/cart/total.js:14:31)",
        );
        let prefix = derive_for_event(&mut cache, &ev, repo.path()).expect("must derive a prefix");
        assert_eq!(prefix, "/var/deploy/shop/.deploy");
        let rewritten = strip_prefix_from_stack(&ev.stack, &prefix);
        assert_eq!(
            rewritten,
            "TypeError: boom\n    at computeTotal (lib/cart/total.js:14:31)"
        );
    }

    /// Round-2 N1, end to end: an app deployed at `<repo>/.deploy` that
    /// throws an assertion error (multi-line message) must still get its
    /// deployment prefix derived — otherwise the stack reaches the detector
    /// unstripped, its signature is `…/.deploy/server.js` while reproduction
    /// computes `server.js`, and the loop dies at `could not reproduce
    /// [unresolved]` with no `--app-root` for the user to blame.
    #[test]
    fn derive_for_event_end_to_end_with_a_multi_line_message_stack() {
        let repo = write_fixture_repo();
        let mut cache = AppRootCache::new();
        let ev = error_event_with_stack(
            "AssertionError [ERR_ASSERTION]: Expected values to be strictly equal\n  actual: 1\n  expected: 2\n    at computeTotal (/var/deploy/shop/.deploy/lib/cart/total.js:14:31)",
        );
        let prefix = derive_for_event(&mut cache, &ev, repo.path())
            .expect("must derive a prefix past the multi-line message");
        assert_eq!(prefix, "/var/deploy/shop/.deploy");
    }

    #[test]
    fn derive_for_event_none_when_no_application_frame() {
        let repo = write_fixture_repo();
        let mut cache = AppRootCache::new();
        let ev = error_event_with_stack("TypeError: boom\n    at node:internal/x:1:1");
        assert_eq!(derive_for_event(&mut cache, &ev, repo.path()), None);
    }

    // -- Python (CPython) traceback support (py-traceback-review.md M1) -------
    //
    // M1: `raw_top_frame_path` scanned `at ` V8 frames only, so a Python
    // service's app_root derivation silently returned `None` for every
    // event, the raw deploy-absolute stack text was never stripped, and the
    // detector's signature kept the deployment prefix while reproduction
    // (its own worktree) computed the repo-relative path -- `matches()` is
    // permanently false, so every Python failure died as a silent,
    // permanent `unresolved`. These tests are red against the pre-fix
    // V8-only walk: `raw_top_frame_path` returned `None` for every stack
    // below, so every `.expect(...)` here panicked and
    // `raw_top_frame_path_finds_the_python_application_frame_past_the_uvicorn_starlette_fastapi_sandwich`
    // failed its `assert_eq!` outright (`None != Some(..)`).
    //
    // The frame walk mirrors `engine_core::ErrorSignature`'s CPython support
    // on branch `scenario/py-traceback` (commit c4592ac,
    // `engine/crates/core/src/lib.rs`) -- NOT merged into this branch. That
    // commit is the source of truth for this behavior; this is a faithful
    // local duplicate (not a shared call) because engine-core must not be
    // touched here in a way that could conflict with that unmerged diff.
    // Keep this in sync if/when it lands.

    fn fastapi_stack(app_frame_path: &str) -> String {
        format!(
            "Traceback (most recent call last):\n  File \"/usr/local/lib/python3.11/site-packages/uvicorn/protocols/http/httptools_impl.py\", line 411, in run_asgi\n    result = await app(\n  File \"/usr/local/lib/python3.11/site-packages/starlette/applications.py\", line 113, in __call__\n    await self.middleware_stack(scope, receive, send)\n  File \"/usr/local/lib/python3.11/site-packages/starlette/routing.py\", line 718, in __call__\n    await route.handle(scope, receive, send)\n  File \"/usr/local/lib/python3.11/site-packages/fastapi/routing.py\", line 274, in app\n    raw_response = await run_endpoint_function(\n  File \"{app_frame_path}\", line 42, in create_quote\n    return quote.total / quote.count\nZeroDivisionError: division by zero"
        )
    }

    fn write_fastapi_fixture_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("app/routes")).unwrap();
        std::fs::write(dir.path().join("app/routes/quotes.py"), "# quotes route").unwrap();
        commit_all(dir.path());
        dir
    }

    #[test]
    fn raw_top_frame_path_finds_the_python_application_frame_past_the_uvicorn_starlette_fastapi_sandwich(
    ) {
        let stack = fastapi_stack("/srv/deploy/shop/.deploy/app/routes/quotes.py");
        assert_eq!(
            raw_top_frame_path(&stack).as_deref(),
            Some("/srv/deploy/shop/.deploy/app/routes/quotes.py")
        );
    }

    /// Primary red-first case (py-traceback-review.md M1): a realistic
    /// FastAPI/uvicorn/starlette traceback wrapping ONE application frame at
    /// an absolute deploy path, against a fixture repo that has the
    /// relative file git-tracked. `derive_for_event` must find the deploy
    /// prefix exactly as it already does for V8 stacks.
    #[test]
    fn derive_for_event_end_to_end_with_a_python_fastapi_deploy_shaped_stack() {
        let repo = write_fastapi_fixture_repo();
        let mut cache = AppRootCache::new();
        let ev = error_event_with_stack(&fastapi_stack(
            "/srv/deploy/shop/.deploy/app/routes/quotes.py",
        ));
        let prefix = derive_for_event(&mut cache, &ev, repo.path())
            .expect("must derive a prefix for a python/FastAPI stack, not just V8");
        assert_eq!(prefix, "/srv/deploy/shop/.deploy");
        let rewritten = strip_prefix_from_stack(&ev.stack, &prefix);
        assert!(
            rewritten.contains("File \"app/routes/quotes.py\", line 42, in create_quote"),
            "stripped stack should carry the worktree-relative path: {rewritten}"
        );
    }

    /// M1, stated in the review's own vocabulary: once app_root is derived
    /// and stripped, the detector's raw top-frame path for a Python event
    /// must be IDENTICAL to what reproduction independently computes from
    /// its own worktree-relative stack. Verified here at the level this
    /// module owns -- the raw path string, upstream of
    /// `engine_core::ErrorSignature` -- since that crate's Python support
    /// (branch `scenario/py-traceback`) isn't merged into this branch yet;
    /// once it is, `ErrorSignature::matches()` compares exactly this
    /// string, so this is what makes that comparison true instead of a
    /// silent permanent `unresolved`.
    #[test]
    fn python_app_root_derivation_makes_detector_and_repro_raw_paths_match() {
        let repo = write_fastapi_fixture_repo();
        let mut cache = AppRootCache::new();
        let detector_stack = fastapi_stack("/srv/deploy/shop/.deploy/app/routes/quotes.py");
        let repro_stack = fastapi_stack("app/routes/quotes.py");

        let prefix = derive_for_stack(&mut cache, &detector_stack, repo.path())
            .expect("must derive a prefix");
        let stripped_detector_stack = strip_prefix_from_stack(&detector_stack, &prefix);

        let detector_path = raw_top_frame_path(&stripped_detector_stack)
            .expect("detector must still find the app frame after stripping");
        let repro_path = raw_top_frame_path(&repro_stack).expect("repro must find the app frame");

        assert_eq!(
            detector_path, repro_path,
            "detector and repro must sign the same failure with the same top frame path"
        );
        assert_eq!(detector_path, "app/routes/quotes.py");
    }

    // -- Hardened-parser fidelity (py-traceback-review.md M2/M3/M4) -----------
    // These pin the local mirror to the SAME fixes commit c4592ac made to
    // engine-core, so this module doesn't quietly resurrect the bugs that
    // commit closed once the branches converge.

    #[test]
    fn raw_top_frame_path_prefers_v8_over_embedded_python_style_text_m2() {
        // A Node service re-throwing a Python worker's captured stderr: real
        // `at ` V8 frames are present, but the message also contains
        // literal `File "..."` text. V8 must win.
        let stack = "Error: worker exited\n    at spawnWorker (/srv/app/lib/worker.js:12:9)\nPython stderr was:\n  File \"/opt/worker/run.py\", line 5, in main\n    raise RuntimeError(\"boom\")";
        assert_eq!(
            raw_top_frame_path(stack).as_deref(),
            Some("/srv/app/lib/worker.js")
        );
    }

    #[test]
    fn raw_top_frame_path_ignores_message_embedded_pseudo_frame_without_a_line_number_m5() {
        // A `File "<path>"` reference with no `, line <N>` behind it is not
        // a real frame header -- it must not out-scan the genuine one.
        let stack = "Traceback (most recent call last):\n  File \"/app/api/app/routes/quote.py\", line 20, in create_quote\n    raise RuntimeError(f'build failed:\\nFile \"{worker_path}\" failed to build')\nRuntimeError: build failed:\nFile \"/tmp/build/worker.py\" failed to build";
        assert_eq!(
            raw_top_frame_path(stack).as_deref(),
            Some("/app/api/app/routes/quote.py")
        );
    }

    #[test]
    fn raw_top_frame_path_treats_any_angle_bracket_pseudo_file_as_a_library_frame_m3() {
        // `File "<string>", line 4, in __setattr__` (a frozen dataclass
        // assignment, verified against real CPython 3.12) must never be
        // mistaken for the application frame -- it isn't real source.
        let stack = "Traceback (most recent call last):\n  File \"/app/api/app/routes/quote.py\", line 15, in create_quote\n    quote.total = compute_total(quote)\n  File \"<string>\", line 4, in __setattr__\ndataclasses.FrozenInstanceError: cannot assign to field 'total'";
        assert_eq!(
            raw_top_frame_path(stack).as_deref(),
            Some("/app/api/app/routes/quote.py")
        );
    }

    #[test]
    fn raw_top_frame_path_does_not_filter_an_app_dir_that_merely_contains_lib_python_m4() {
        // `<repo>/lib/python/dispatch.py` is a real polyglot application
        // path, not the interpreter's stdlib -- the filter must anchor on a
        // digit immediately after `python` (the stdlib's actual shape:
        // `/lib/python3.11/...`).
        let stack = "Traceback (most recent call last):\n  File \"/app/api/api/handler.py\", line 10, in handle\n    dispatch(event)\n  File \"/app/api/lib/python/dispatch.py\", line 30, in dispatch\n    raise ValueError(\"bad event\")\nValueError: bad event";
        assert_eq!(
            raw_top_frame_path(stack).as_deref(),
            Some("/app/api/lib/python/dispatch.py")
        );
    }

    #[test]
    fn raw_top_frame_path_does_not_filter_an_app_file_merely_named_importlib_m4() {
        // `api/importlib_compat.py` is an application file whose name merely
        // contains the substring `importlib` -- the rule must require it as
        // a real path component.
        let stack = "Traceback (most recent call last):\n  File \"/app/api/api/handler.py\", line 10, in handle\n    load_plugin(name)\n  File \"/app/api/api/importlib_compat.py\", line 8, in load_plugin\n    raise ImportError(\"missing plugin\")\nImportError: missing plugin";
        assert_eq!(
            raw_top_frame_path(stack).as_deref(),
            Some("/app/api/api/importlib_compat.py")
        );
    }
}
