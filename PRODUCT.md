# Product

Canonical strategy lives in the company operating memo (August 2026). This file
is the product-facing statement of the same direction — what Drums is, who it is
for, and the laws the interface obeys. It describes where the product is going.
How much of it works today is a separate document: `docs/GAP.md`.

## Register

product

## Platform

web

## Users

A founder, CTO, or product engineer at a small software company who personally
owns one revenue-critical product workflow, already writes a meaningful share of
their code with Claude Code or Codex, and already has analytics or observability
running. They ship fast, they have live user traffic, and nobody on the team is
continuously turning that traffic into engineering decisions. The workflow is
something a customer moves through and a business gets paid for: signup to
activation, file upload to quote, credentialing to approval, booking to
completion, checkout to payment, invite to collaboration.

The buyer is the person who currently does the translation by hand — reads the
replay or the error, decides what matters, writes the prompt or the ticket, has
an agent implement it, ships it, and then usually does not go back to check
whether it worked. That last part is not laziness. It is that the checking has no
owner and no artifact.

Two things qualify a team, and they are different from each other. The first is
whether the product has **real user traffic and enough of it that a change's
effect can eventually be detected** — without that, Drums can observe and make
changes but cannot honestly call any of them improvements. The success event and
target metric do not need to exist on day one; they are defined when an
intervention is proposed, against whatever the hypothesis is trying to move. The
second is whether the team will let Drums see production behavior and open
branches against their repo. Teams that pass the second and fail the first are
still workable, but the product owes them plainer language about what it proved.

The disqualifier that matters most is not size, it is ownership: a solo founder
who wrote the whole system and personally watches every user does not have this
problem. Also out: pre-product teams, anyone with no live traffic, and teams
whose only ask is code review or agent orchestration — both are crowded
commodity layers and neither is what this product is.

## Product Purpose

Drums is self-improving software. It watches how people use a live product,
finds problems and opportunities that can be addressed in code, makes the
change, measures whether the product actually got better, and keeps what it
learned.

The loop is: **observe → understand → hypothesis → change → verify → roll out →
measure → learn.** Drums watches how people actually use the product and looks
for repeated behavior that costs something: users retrying a step, abandoning
halfway, detouring through a workaround, bouncing between screens, contacting
support about the same thing twice, taking unusually long to finish, hitting a
silent failure, regressing after a deploy, or throwing an actual exception. It
connects that behavior to product state, to a release, and to the code
responsible. It states a hypothesis — what should change, and why that is
expected to help. **At that point, not before**, it defines the evaluation: the
outcome that should move, the baseline, the smallest effect worth claiming, and
the guardrail metrics that are not allowed to regress. The team's own coding
agent implements the change in an isolated worktree. Drums verifies the change
without asking the agent whether it worked. A human approves. It rolls out,
measures the target and the guardrails over the window it declared, and writes
the result down.

The durable record is **Observation → Hypothesis → Change → Outcome**, and the
observation is the root. Nobody has to configure a workflow, a funnel, or a KPI
before Drums is allowed to notice something — evidence comes first and structure
is attached to it, because a product that can only see what a customer already
defined can never discover anything. A team that already knows what matters can
declare it up front and Drums will watch it from day one; that is a shortcut,
never a gate. The four durable objects are: the **product model** (what Drums
has learned about the software — journeys, surfaces, cohorts, all inferred),
**observations** (what objectively happened), **hypotheses** (what Drums thinks
could be better, citing observations), and **changes with their outcomes** (what
was tried, and what the numbers did afterward). The last field is what makes the
record worth keeping: an intervention is not finished when the pull request
merges; it is finished when the post-ship number is in.

Verification never rests on the agent that wrote the change. There are two
admissible sources and they cover different territory. **Reproduction** is the
strong one: for failures where an exact request was captured, Drums checks out
the causing revision into a detached worktree, replays the captured request
until the failure reproduces with a matching signature, then replays it against
the repair. This is objective and it is the highest evidence the system can
produce — but it only reaches failures that can be replayed, which is a small
share of what actually degrades a workflow. **Measured outcome** is the general
one: the target metric moved, the guardrails held, over a window stated before
the rollout rather than chosen after it. Everything reproduction cannot reach —
confusion, abandonment, friction, latency people tolerate but hate — is verified
this way or not at all.

Neither source is permission to overstate. An intervention moves through
**observed → hypothesized → shipped → outcome unmeasured**, and only then, once
enough evidence exists, to **outcome verified positive, neutral, or negative**.
A change that shipped into a workflow without the volume to detect the effect it
was aiming at stops at *shipped, outcome unmeasured* — never verified. That
state is not a failure mode to be designed around; it is the honest majority
case early on, and saying it plainly is the same discipline as saying "could not
reproduce." Neutral is a real result too: a change that did nothing is a finding,
and a product with no minimum effect size reports every wobble as a win.

Success at the company level is **verified product improvements per active
customer per week**, where a verified improvement means Drums found a
code-addressable issue or opportunity without a human-filed ticket, shipped a
change to production, moved the target metric, and did not regress a guardrail
past an agreed threshold. Sessions analyzed, pull requests opened, bugs
detected, and agent runs are operating metrics; all of them can rise while the
product creates nothing.

## Positioning

The category converged fast. Coding agents run longer and absorb more context
every month, product analytics platforms now market themselves as self-driving,
and session-intelligence startups already go from replay to diagnosis to a pull
request. Anything Drums can assert in a headline, a better-funded incumbent can
assert by Tuesday. The differentiation has to be a loop that is materially
deeper, not a claim that is louder:

> Coding agents are the execution layer. Observability tools are inputs. Drums
> owns the loop between what users experience, what should change in the
> product, the code change, and whether the outcome actually improved.

What that requires Drums to own, concretely: an explicit **evaluation plan for
every intervention** — the outcome it should move, its baseline, its guardrails,
its window — rather than a generic instruction to fix bugs; **cross-source
evidence** that
combines behavior, business state, support, errors, releases, and code;
**intervention design** that states why a change should move a metric and what
could regress; **independent verification** that reproduces or measures rather
than trusting the author; **controlled rollout and evaluation** as part of the
task instead of a dashboard someone might check later; **longitudinal memory**
of what was tried, what worked, what regressed, and what the team trusts Drums to
do unattended; and **cost-efficient candidate selection**, because a loop that
spends frontier-model reasoning on every session has no margin and therefore no
business.

There is no moat today. There is early product, production access, design
partners, and a direction. The path to one is accumulated: a customer-specific
record linking behavior to hypotheses to code to measured outcomes, plus the
evaluation infrastructure and policy that make the loop safe to keep running.
That is copyable in principle. The defense is time, integration depth, and
repeated real use — and the only convincing proof is a customer who already pays
for the incumbent stack and still pays for this.

## Brand Personality

Drums reads as a control surface an engineer would build for themselves, not a
startup dashboard performing confidence it has not earned. The governing
instinct is radical honesty rendered directly in the interface: a change whose
outcome could not be measured says so instead of borrowing the language of one
that was; a failure that never reproduced is labeled "could not reproduce" and
offered no ship button; a claim Drums inferred is never drawn like one it
verified; a page with nothing to report says so in plain language rather than
filling itself with facts that already have their own page. A control that does
nothing does not ship.

That honesty is inseparable from the visual register — near-monochrome, quiet,
text-first, machine facts in mono and human judgment in sans — because a company
whose entire pitch is "we will not claim it improved unless we measured it"
cannot present itself with the glossy confidence it refuses to let the product
claim. Tone is engineer-to-engineer: plain-spoken, precise, unhyped, comfortable
saying "not measured" and "unresolved" out loud instead of smoothing them over.
In customer-facing writing the working test is whether a sentence would embarrass
you in a year; "it made the bug happen again" beats "it reproduced the failure"
for everybody who does not already work here.

## Anti-references

Not "PostHog plus Claude plus a pull request" — that is the shape this has to be
materially deeper than, and any version of the product that reduces to it is a
feature waiting to be absorbed. Not a better session-replay summarizer, not a
code-review agent, not a generic self-healing incident bot, not a multi-repo
orchestration UI, and never "we have more context" offered without an outcome
system behind it. Not "the independent verifier for AI-written code," "we close
the whole loop," or "AI SRE" — postures a competitor can assert in a headline,
and reaching for them is what made the differentiation question land in the
first place.

Never a single confidence score, safety percentage, or lone green check standing
in for judgment; blending distinct kinds of evidence into one number is exactly
what makes a claim dishonest. Never a metric move presented without its
measurement window and its guardrails. Not a passive report generator or a wall
of AI commentary bolted onto a pull request — the product is decision-shaped.
Not a generic AI-generated SaaS dashboard: no centered glyph-card empty states,
glowing dashed reticles, gradient-filled primary buttons, decorative purple
washed across chrome, bubbly ≥12px radii, faked drop shadows between panels, or
invented affordances the product does not actually have. And not a
workspace-for-coding-agents shell — that framing was retired deliberately, and
the desktop app that carried it is parked.

## Design Principles

The observation is the frame for everything on screen: a finding is presented as
what actually happened — who, how many, over what window — never as a diagnosis
the evidence has not earned, and a change is presented against the outcome its
evaluation plan declared. Outcome is a first-class field, not a follow-up — a
change that has shipped and not yet been measured is visibly unfinished, and the
interface never lets it read as complete. Structure Drums inferred (a journey, a
grouping, a suspected cause) is drawn as inferred, distinct from what was
observed, and never as something the customer declared.

Trust is assembled, not asserted: the interface presents evidence pulled from
systems the user already trusts rather than asking them to trust Drums'
judgment. Semantic honesty is a hard interface law, not a copy nicety: a state,
claim, or control renders only what is true right now, never what the roadmap
will make true later, and an unearned status never borrows a more finished
status's language. Every claim carries its own kind — verified, observed,
inferred, approved, unresolved — surfaced distinctly rather than averaged. The
machine recommends and a named human decides; advisory output is labeled
advisory and attributed. Authority is earned on-screen the same way it is earned
architecturally: the product shows only the capability level it currently has,
never a preview of authority it has not reached, and reducing what Drums may do
on its own is always the easiest operation in the system.

## Accessibility & Inclusion

Keyboard-first operability throughout the console: every menu, dialog, and
filter bar supports focus trapping, arrow-key operation, Escape to close, and
focus restoration on close; custom controls carry full ARIA semantics over hit
targets of at least 8px rather than a visual affordance alone. No bare-key global
shortcuts. State is never carried by color alone — every provenance chip and
status dot pairs with plain-text state language, so approvals stay legible
without color perception, which matters here because the difference between
"verified" and "inferred" is the difference between shipping and not. Diffs are
readable without relying on red/green: added and removed lines are marked with a
sign and a label, not a hue. All motion respects `prefers-reduced-motion`. The
console is expected to stay usable down to 1280×720 and at 200% zoom, falling
into a compact single-column mode rather than breaking, and every approval is
answerable from Slack on a phone for the person who is not at a desk when the
message arrives.
