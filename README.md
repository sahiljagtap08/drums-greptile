# Drums — the product that improves itself

**Greptile Fast Hackathon build.** A user hits a real failure in a web app.
Nobody files a bug. Drums notices, converts what the user did into an
executable reproduction, proves the failure reproduces against HEAD, hands the
repair to **OpenAI Codex**, then independently boots the changed app, replays
the exact same user interaction, and only says **VERIFIED** when the thing
that failed for the user no longer fails.

```
user uses product
  → product fails
    → Drums captures the interaction + failure evidence
      → Drums reproduces it against HEAD in an isolated worktree
        → Codex writes the smallest fix
          → Drums reboots the changed app and replays the SAME evidence
            → VERIFIED only if the original failure is gone and guardrails pass
```

## The one idea

**The system generating the fix is not the system deciding whether the fix
worked.** Codex generates. Drums verifies. Codex saying "fixed" has zero
authority; tests being green is not sufficient; the original user failure has
to be gone. The invariant is enforced in code
([heal/pipeline.js](heal/pipeline.js), `verdict()`): `VERIFIED` requires

1. the original failure reproduced against HEAD,
2. a non-empty code change,
3. the same replay passing against the changed, rebooted app,
4. guardrail tests passing.

Anything else ends as `FAILED`, `INCONCLUSIVE`, or `REGRESSION_FOUND`.

## Run it

```bash
cd heal && npm install && npx playwright install chromium && cd ..
node heal/cli.js watch fixture
```

Then be the user: open http://localhost:3000, join the waitlist as
`you+tag@gmail.com`, and watch the terminal. (Or script the user:
`node heal/simulate-user.js`.) Replay a saved incident with
`node heal/cli.js repair fixture .drums/incidents/<id>/incident.json`.

## Onboard any Node web app in under 5 minutes

1. Add one script tag to your page (the capture snippet is served by Drums):
   `<script src="http://localhost:4600/snippet.js"></script>` — plus set
   `window.__DRUMS_COLLECTOR__ = "http://localhost:4600"` before it.
2. Drop a `drums.json` in the repo:
   ```json
   { "install": "npm install", "start": "npm start",
     "health": "/api/health", "app": "/", "test": "npm test" }
   ```
3. `node heal/cli.js watch <your-repo>`

The repo must be a git repo (isolation is a `git worktree` at HEAD).

## What's in here

- `heal/` — the hackathon loop: capture snippet, incident collector,
  Playwright replayer, and the pipeline state machine
  (`OBSERVED → REPRODUCING → REPRODUCED → REPAIRING → CANDIDATE_READY →
  VERIFYING → VERIFIED | FAILED | INCONCLUSIVE | REGRESSION_FOUND`).
- `fixture/` — a tiny zero-dependency waitlist app with one real bug: its
  email regex has no `+` in the local part, so a plus-addressed signup makes
  `match()` return null and the endpoint 500s.
- `engine/` — a snapshot of the production Drums engine (Rust). The hackathon
  loop is a narrow, browser-native implementation of the same doctrine the
  engine encodes: agent-neutral repair in caller-prepared worktrees
  (`engine/crates/repair`), and local-only Verified authority — a remote
  plane asserting "verified" in a payload has no authority
  (`engine/crates/core/src/authority.rs`).

## What Drums never does

It never merges or deploys. The loop ends with a verdict, a diff, and a
worktree for a human to review. A repair the system cannot prove is a repair
it refuses to claim.
