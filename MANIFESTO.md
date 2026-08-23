# The Drums Manifesto

*What we believe about how software gets kept working.*

Drums isn't just a product. It's a position. Here's what we refuse to compromise on.

---

### 01 — Shipping got cheap. Keeping it working stayed human.

Coding agents made writing software cheap. Keeping it working stayed expensive, and it is still done by
hand. Every tool in this space stops at the pull request: review tools read the diff, test systems check
what someone thought to write down, monitoring reports the error, agents write a fix when handed the task.
At the end of all of it a person still reads logs, decides what broke, approves a repair, ships it, and
watches to see whether it worked. That last part is the product.

### 02 — The engineer is on the loop, not in it.

Every screen answers two questions: what did it do, and can I trust it. Nothing else earns its place.
If someone is clicking through Drums regularly, we have failed — we will have built another dashboard
for the thing we promised to take away.

### 03 — Provenance instead of green checks.

A green check is a claim with its reasoning deleted. Every claim Drums makes carries how it knows it:
**verified** (we ran it and watched it pass), **observed** (telemetry says so), **inferred** (a model
concluded it), **approved** (a named person signed off), **unresolved** (we don't know, and we say so).
A repair ships on its own only when every claim supporting it is verified or observed. Attribution is
inferred — which is exactly why reproduction exists.

### 04 — Autonomy is earned per failure class, and revoked automatically.

Nobody should configure trust in a settings page. Each class of failure climbs its own ladder — observe,
shadow, propose, act alone — and Drums proposes the promotion from its own track record, with the
evidence attached. Demotion is automatic and quiet: two rollbacks in a class inside thirty days and it
drops a rung, with a notification saying why. A team should never have to remember to take authority away.

### 05 — Some things always wait for a person.

Customer data, billing, permissions, infrastructure. These do not climb the ladder, no matter how clean
the record is. An approval is a signed action by a named person against an exact revision — not a click
on a model's suggestion.

### 06 — Reversibility is stated at the moment of action.

Every completed repair prints its undo command in the same breath as the success, and stays one command
away from undone for thirty days. `drums stop` pauses everything, everywhere, in one step. If the way
back is ever more than one step away, no team gives a tool like this production access, and the whole
thesis dies at the demo.

### 07 — Design the miss harder than the hit.

A good handoff is what makes a team forgive the failures, and it converts skeptics faster than a success
does. When Drums can't fix something it does not return a stack trace and an apology — it hands over a
running reproduction, the patches it tried, and what each of them broke. Six minutes of work handed over
rather than thrown away.

### 08 — Git is the record.

Repairs land as real commits with structured trailers, and the evidence chain attaches as a git note.
`git log` becomes the audit trail: it travels with the repository, it survives Drums, and a security team
can read it with tools they already trust. No database anyone has to take on faith. Rolled-back repairs
are recorded at the same weight as the successes — hiding failures is how a tool like this loses a team
permanently.

### 09 — Bring your own agent, your own telemetry, your own deploys.

Claude Code and Codex write the repair today, OpenCode next. Telemetry comes in from OpenTelemetry,
PostHog, and Sentry; promotion goes out through the deploy platform you already run. Drums decides what
needs repairing, whether the repair worked, and whether it ships. Being agent-neutral is worth more than
owning that step, and the moment Drums requires its own deploy target the install stops being five minutes.

---

*Trust survives the first mistake, or it never existed. Nothing changes until you say so.*

**Drums — software that maintains itself.**
