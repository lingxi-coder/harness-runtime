---
name: local-app-use
description: Recognize what the user wants to do with an existing local app and route to the skill that runs, inspects, captures, interacts, tests, debugs, queries data for, or backgrounds it.
---

# Use a local app

This skill is a router. It classifies the user's intent inside an existing
app's own conversation and hands off to one specialist skill; it does not
operate the Host itself and does not write or edit source.

## Scope

Use this skill only inside an app's own app-scoped conversation, the one
whose `workspace/LINGXI.md` already names the app id — that file is the one
channel guaranteed to reach the model on every turn in that session, so its
id is authoritative. Do not call a discovery tool to rediscover or confirm
the current app from inside that session.

If the user is asking to build a new app, or to change what an existing app
IS (shape, screens, data model, source), that is not a `local-app-use`
request — it belongs to `$create-local-app`. This skill only covers
operating an app that already exists and already has a shape: running it,
looking at it, poking it, checking it, diagnosing it, reading/writing its
declared data, and its background jobs.

## Routing

Classify the request into exactly one of these categories, then hand off:

- **Runtime lifecycle** — start, stop, restart, open, "is it up", "why won't
  it launch" → `$local-app-run`.
- **Structural / accessibility evidence** — "what's on screen", "find the
  button labeled X", "read the DOM", "is there an error banner" →
  `$local-app-inspect-view`.
- **Visual evidence** — "show me", "screenshot it", "what does the canvas
  look like", "is it animating" → `$local-app-capture-view`.
- **Driving the UI** — tap, click, type, scroll, navigate, press a key, drive
  a canvas with pointer/key input → `$local-app-interact`.
- **Verification** — "does X work", "check the acceptance criteria", "run
  through the happy path" → `$local-app-test`.
- **Diagnosis** — "why is it broken", "read the console/logs", "trace this
  failure" → `$local-app-debug`.
- **Declared data** — read or write the app's own collections/records →
  `$local-app-data`.
- **Background jobs** — schedule, list, check status, cancel, retry a
  background flow → `$local-app-background`.

"Structural vs. visual evidence" is the judgment call unique to this router:
a routed/DOM app is usually answerable from `$local-app-inspect-view` alone;
a canvas/WebGL app or a purely visual question ("does this look right") needs
`$local-app-capture-view` instead, because a drawn surface has no elements
for the DOM snapshot to describe. When unsure which kind of surface the app
is, or when the request needs both ("is the button there AND does it look
right"), route to both in sequence rather than guessing from one.

A single user turn may span more than one category — "start it and show me"
is lifecycle then visual evidence; "tap submit and tell me if it worked" is
driving the UI then evidence. Route through each specialist in the order the
request implies instead of picking only the first category that matches.

## Boundaries

- Never call the Host's runtime, inspect, capture, act, data, log, or
  background tools directly from this skill — that is what the specialist
  skills above are for. This skill's own job ends at choosing which one.
- Never write or edit app source, never call a build or dependency tool, and
  never propose a manifest/capability change from here — none of that is
  "use," and drifting into it is exactly what `$create-local-app` and its
  build workflow own instead.
- When the request is genuinely ambiguous between two categories, ask one
  short clarifying question rather than guessing a specialist and running it.
