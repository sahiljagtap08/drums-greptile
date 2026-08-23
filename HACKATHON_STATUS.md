# HACKATHON STATUS

## CURRENT STATUS
The loop is built and has closed once for real: a browser user hit the fixture
bug, Drums captured it, reproduced it against HEAD in an isolated worktree,
Codex patched it (+1 -4, the minimal regex fix, no answer-key comment in the
code), Drums rebooted the candidate, replayed the same interaction, ran
guardrail tests, and printed VERIFIED. Failure-mode drills in progress.

## END TO END LOOP: WORKING
Proven by execution on 2026-08-23, incident 2026-08-23T21-20-02-o23v
(artifacts in .drums/incidents/, gitignored).

## WHAT ALREADY WORKS
- Capture: one script tag records the user's actions (with redaction), wraps
  fetch, reports 5xx/JS errors with trace + evidence to the collector.
- Reproduction: Playwright replays the trace against a `git worktree` at HEAD
  and must observe the same failure, or the run ends INCONCLUSIVE.
- Repair: `codex exec -C <worktree> -s workspace-write` with the evidence.
- Verification: candidate app rebooted on a fresh port, same replay, guardrail
  tests. verdict() enforces: VERIFIED = reproduced ∧ diff ∧ replayPassed ∧
  guardrails. Codex's own "fixed" claim has zero authority.
- Distinct outcomes: FAILED, INCONCLUSIVE, REGRESSION_FOUND (all five
  sabotage drills refused correctly).
- Friction incidents: dead/rage clicks with zero telemetry are detected from
  behavior (no request, no DOM change, no navigation) and verified by a
  measured click probe.
- Memory: every incident is remembered (.drums/memory.jsonl); new incidents
  recall related history and brief Codex with it.
- Evidence: before/after screenshots of what the user saw, plus an advisory
  vision check on the after-state (never the verdict authority).
- Verified repairs open a PR and get an independent Greptile review
  (PR #1 merged with a 5/5 review; #2 and #3 reviewed and open).

## CURRENT BLOCKER
None.

## NEXT ACTION
Onboard a stranger's repo at the venue (see ONBOARDING.md).

## EXACT DEMO COMMAND
    node heal/cli.js watch fixture
then join the waitlist at http://localhost:3000 as you+tag@gmail.com.
Insurance: node heal/simulate-user.js  (scripted user)
Replay saved evidence: node heal/cli.js repair fixture <incident.json>

## KNOWN LIMITATIONS
- Node web apps, single start command, health URL, git repo. drums.json = 5 keys.
- One incident processed at a time; extra incidents during a run are dropped.
- Redacted inputs replay as the literal string "redacted".
- Human merges; Drums stops at verdict + diff + reviewable worktree.
