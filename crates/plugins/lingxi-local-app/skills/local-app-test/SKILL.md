---
name: local-app-test
description: Turn an app's acceptance checks into bounded scenarios and run them through the use-test workflow for a structured pass/fail report. Never edits source or self-judges a fix.
---

# Test a local app

Own turning "does this work" into evidence: read the app's acceptance
checks, shape them into a bounded set of scenarios, and hand them to the
use-test workflow, which drives the running app and reports what actually
happened. This skill does not itself drive the UI, read logs, or judge
whether a fix landed — it hands the app to the workflow and relays the
workflow's own verdict.

## Where acceptance checks come from

An app's design carries its own acceptance checks — the criteria the user
(or the app's own design spec) named as what "working" means for this app.
Read those from wherever this conversation already has them (the app's
LINGXI.md context, a prior design turn, or an explicit user ask like "check
that submitting the form saves an item") rather than inventing generic
checks the app never declared. A check with no obvious way to observe it
from the running app (nothing to click, nothing to read back) is not
testable here; say so rather than silently dropping or rewriting it.

## Bounding a scenario

Turn each acceptance check into a scenario the workflow can actually run: a
starting state, a short ordered sequence of user-observable actions, and
what a pass looks like. Keep scenarios bounded — few, concrete steps tied to
one check — rather than one sprawling script that tries to cover the whole
app at once; a bounded scenario is what lets a failure point back at the one
check it broke.

## Running it

Invoke the use-test workflow through the `Workflow` tool —
`{"name": "lingxi-local-app:local-app-use-test", "args": {"app_id": "<id>", ...}}`.
Testing is the only remaining host workflow for a Local App: `$create-local-app`
plans with the user, prepares the workspace from the approved plan, implements it
and stops at a successful build, so an app is tested here on request rather than
by a build-time stage. The workflow (per the plugin's frozen design, §11.3) takes
the app id, a scope, and a scenario/quality policy; the host injects the
app's own build and runtime-profile identity rather than trusting a
model-supplied one. Internally it drives the running app for
runtime/interaction/render evidence and then evaluates that evidence against
the acceptance checks and profile policy — the same two-step split as the
`operator`/`tester` agents this plugin defines, so nothing here reimplements
either.

Do not pass `fast` for a canvas/WebGL app's quality level; the canvas family
rejects it, because a drawn surface still needs a real design and motion check
even at the lowest tier.

## Reading the report

The workflow returns a structured report, not prose: which in-scope scenario passed
or failed and why, render/motion evidence when the surface is a canvas,
logs/console/errors it captured along the way, and whether a bridge/data
round-trip behaved. Preserve the Host verification scope and explicitly relay
declared but unverified targets/scenarios; a current-device pass is not a
full-matrix pass. Relay findings by what the report actually says —
quote the failing scenario and the evidence field that failed it, not a
paraphrase — rather than summarizing a run as "looks good" when the report
named a specific failure.

## Boundaries

- Never write or edit app source from here, and never call a build tool.
  A failing scenario is a finding to report, not something to patch
  in-place; repairing source in response to a finding belongs to the
  workflow's own repair round (bounded, budgeted, and driven by the
  workflow itself) or to `$create-local-app`'s update path — not to this
  skill.
- Never declare a check fixed, passing, or resolved on your own reading of
  the app. This skill's job ends at relaying what the workflow's report
  says; it does not re-adjudicate the report or override a failure with
  your own judgment that the app "seems fine."
- Do not drive the app directly with `$local-app-interact` as a substitute
  for running the workflow — ad hoc pokes are not a scenario result and
  don't produce the structured report this skill exists to produce. Use
  `$local-app-interact`/`$local-app-inspect-view`/`$local-app-capture-view`
  only to gather what you need to shape a scenario before invoking the
  workflow, not to stand in for it.
