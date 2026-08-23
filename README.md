<div align="center">
  <img src="media/drums-logo.png" width="120" alt="Drums" />

  <h1>Drums</h1>

  <p><strong>Drums — software that maintains itself.</strong></p>

  <p>Teams use coding agents to write and ship changes faster than engineers can review and maintain them.<br/>Drums keeps the context from what a change was supposed to do through deployment, failure, repair, and recovery.</p>

  <sub>Design partners · 2026 · <a href="https://drums.sh">drums.sh</a> · <a href="https://drums.sh/docs">Docs</a> · <a href="https://drums.sh/playbooks">Routines</a> · <a href="https://x.com/drumslabs">X</a> · <a href="https://www.linkedin.com/company/drums-sh">LinkedIn</a></sub>
</div>

---

> **Private & proprietary.** This repository is confidential. See [LICENSE](LICENSE).

## What Drums is

Every tool in this space stops at the pull request. Review tools read the diff, test systems check what
someone thought to write down, monitoring reports the error, and coding agents write a fix when handed
the task. At the end of all of it, a person still reads logs, decides what broke, approves a repair,
ships it, and watches to see whether it worked.

Drums does that last part. It works with the codebase, tests, deployment system, monitoring tools, and
coding agents a team already uses.

**The loop.** Detect a failure from telemetry → link it to the deploy and the code change → recreate it
in an isolated environment → write and test a repair with the team's own coding agent → deploy to a small
share of traffic → verify the original failure is gone and nothing else broke → complete the rollout or
roll back. For safe, reversible failure classes this completes without waiting for an engineer.

- **The engineer is on the loop, not in it.** Every screen answers *what did it do* and *can I trust it*.
  If someone is clicking through Drums regularly, the product has failed.
- **Provenance instead of green checks.** Every claim carries how it is known — verified, observed,
  inferred, approved, or unresolved. A repair ships alone only when every supporting claim is verified
  or observed.
- **Autonomy is earned per failure class.** Each class climbs Observe → Shadow → Propose → Act alone on
  its own track record, and drops a rung automatically after rollbacks.
- **Repairs involving customer data, billing, permissions, or infrastructure require human approval.**
  That is a hard boundary, not a default.
- **Reversibility is stated at the moment of action.** `drums stop` pauses everything everywhere;
  `drums undo --since 1h` reverses recent changes. Repairs stay reversible with one command for 30 days.
- **Git is the record.** Repairs land as real commits and the evidence chain attaches as a git note, so
  `git log` is the audit trail — it travels with the repository and survives Drums.
- **Agent-neutral.** Claude Code and Codex write the repair today, OpenCode next. Telemetry comes in from
  OpenTelemetry, PostHog, and Sentry; deploys go out through the platform you already run.

Drums starts in observe-only: nothing will be changed until you say so.

See [MANIFESTO.md](MANIFESTO.md) for the full position, and `drums_product_spec.html` for the product
specification.

## Repository layout

```
drums/
├── drums/      # Rust core — daemon, CLI, and native shell (control plane)
├── website/    # drums.sh — landing page, docs, routines
├── media/      # Brand assets — logo, favicon set, hero imagery
├── MANIFESTO.md
├── LICENSE     # Proprietary — all rights reserved
└── README.md
```

## Status

In active development with design partners, targeting 2026. The reproduce loop — detect, attribute,
rebuild, replay — ships first; repair and release follow, to design partners before anyone else. Not yet
accepting external contributions.

---

<div align="center"><sub>© 2026 Drums Labs, Inc. Built for the teams being underserved.</sub></div>
