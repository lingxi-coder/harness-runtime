---
name: local-app-run
description: Start, stop, or restart a local app, wait for it to actually finish starting, and report its runtime state and URL. Never builds, installs dependencies, or repairs an app.
---

# Run a local app

Own the app's runtime lifecycle: start it, stop it, restart it, and report
what is actually true about it afterward. Nothing here writes source,
builds, installs dependencies, or repairs a broken scaffold — a runtime
failure that traces back to a missing or incompatible build is reported
back, not fixed from here.

## The runtime tool

Drive the app's dev server through `LocalAppRuntime` with one `action`:
`start`, `stop`, `restart`, `open`, `resume`, or `suspend`. `start`/`open`/
`resume` start the runtime; `stop`/`suspend` stop it; `restart` is stop then
start — there is no separate atomic restart call, so a restart can surface
either half's failure. The call blocks until the runtime is actually
serving (or has failed to), not just until the request was accepted; you do
not need to poll separately.

A concurrent `stop` issued while another start is still in flight is
rejected with "runtime is still starting; retry stop shortly" — that is not
a bug, retry it after a moment rather than treating it as a runtime failure.

## Runtime states

The runtime is always in one of: `stopped`, `starting`, `running`,
`stopping`, `failed`. A successful start/restart returns `{app_id, state,
url}`; a successful stop returns `{app_id, state:"stopped"}`. `url` is
`http://127.0.0.1:<port>` — the host assigns this port once per app and
never reassigns it, because that origin anchors the app's own browser
storage, so never construct or guess a different port or origin.

## Before starting

An app that has never been built, or whose last build failed, cannot serve
anything — starting it will fail with a message naming the real problem
rather than quietly doing nothing. Two failures you will see verbatim and
should report as-is, not attempt to work around:

- `runtime_api_incompatible: app manifest targets runtime API v<N>; ...` —
  the app was scaffolded/built against an older runtime contract and needs
  regenerating or rebuilding.
- a message that the build output is missing `index.html` and the app needs
  to be generated first.

Either one means: hand this back rather than retrying start, since fixing it
means writing source or rebuilding, both out of scope here.

## On failure

A `failed` runtime state carries a `lastError` explaining what went wrong.
Report it verbatim rather than paraphrasing it away or guessing a cause it
didn't state.

## After a successful start

This skill's job ends at "it's running at `<url>`, here is its state." For
anything about what the app actually shows or does once it is running, hand
off to `$local-app-inspect-view`, `$local-app-capture-view`, or
`$local-app-interact` rather than trying to characterize it here.
