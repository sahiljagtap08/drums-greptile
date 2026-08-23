# HACKATHON STATUS

## CURRENT STATUS
Clean snapshot repo created (engine + vision docs only, secret-scanned, no customer
data, no git history). Private at github.com/sahiljagtap08/drums-greptile.
Building the capture → reproduce → Codex → replay → VERIFIED loop as `heal/`.

## END TO END LOOP: BROKEN (being built)
The hackathon loop (real user failure → executable reproduction → agent fix →
replay same evidence → VERIFIED) does not exist yet in runnable form.

## WHAT ALREADY WORKS (verified by reading real code, not docs)
- engine/crates/repair: agent-neutral repair in a caller-prepared worktree.
  Explicitly does NOT boot or verify the app (its own header says so).
- engine/crates/check: runs repo checks (tests/clippy) and emits Verified
  provenance claims — but "verified" there means "checks ran", not "the user's
  failure is gone".
- engine/crates/ingest: real failure ingestion adapters (Sentry-shaped, Linear).
- engine/crates/core/authority.rs: local-authority rule — a remote plane
  asserting Verified has no authority. We keep this principle.

## WHAT IS MISSING (the hackathon build)
- Browser interaction capture (no Playwright/puppeteer anywhere in engine).
- Executable reproduction from user evidence.
- Booting the target app from an isolated worktree, before AND after the patch.
- Replaying the same evidence against the candidate and gating VERIFIED on it.
- Codex as the repair agent inside the loop.

## CURRENT BLOCKER
None. Playwright installing in background; codex CLI authed (API key).

## NEXT ACTION
Build heal/: collector + browser snippet (capture), replayer (Playwright),
pipeline (worktree → boot → reproduce → codex exec → reboot → replay → gate),
plus fixture/ app with one real user-triggered bug.

## EXACT DEMO COMMAND
(will be) `node heal/cli.js watch fixture/` — then a user hits the bug in the
browser and the loop runs to VERIFIED on its own.

## KNOWN LIMITATIONS
- Narrow target: Node web apps started by a single command with a health URL.
- Onboarding config: drums.json {install, start, health, app}.
- Human merge/deploy approval is out of scope; loop stops at a diff + verdict.
