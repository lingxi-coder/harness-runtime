---
name: local-app-interact
description: Drive a running app's view with typed click/fill/select/toggle/scroll/navigate/back/reload/pointer/key actions and report what happened. Never injects JS or taps blind coordinates.
---

# Interact with a local app

Own driving a running app's own view: resolve a target (or address a raw
point on a canvas), dispatch one structured action, and report the actual
result the page gave back. This is the only skill that sends input to the
app.

## The tool

Call `act_on_ui` with `app_id`, `action`, and `target` and/or `value`
depending on the action:

- `action` is one of `click`, `fill`, `select`, `toggle`, `scroll`,
  `navigate`, `back`, `reload`, `pointer`, `key`. Any other value is
  rejected before it reaches the page — there is no arbitrary-JavaScript
  escape hatch.
- `target` addresses an element for `click`/`fill`/`select`/`toggle` (and
  optionally `scroll`). Pass either a bare element id string, or an object
  `{element_id, role, name}`. Resolution tries `element_id` first (matched
  through shadow roots, so an Ionic-style `ion-input-0` still resolves), then
  falls back to the first candidate whose role and name both match — the
  same "interactive candidate" set `$local-app-inspect-view`'s `elements`
  list enumerates. Get `element_id`/`role`/`name` from that snapshot rather
  than guessing them.
- `value` carries the action's payload where one applies: the fill/select
  text, the toggle's desired boolean/`"true"`/`"false"`, the scroll amount
  or `"up"`/`"down"`/`"top"`/`"bottom"`, the navigate URL (same-origin only),
  the pointer coordinates, or the key name.

## Canvas and WebGL surfaces: pointer and key

`click`/`fill`/`select`/`toggle` resolve a DOM element and act on it. A
canvas or WebGL surface has none — it only listens for pointer and keyboard
events at real coordinates. Use `pointer` and `key` for those surfaces (and
for anything a plain click can't reach, like a drag):

- `pointer` takes `value` `"x,y"` or `"x,y,phase"` in **viewport CSS
  pixels** — the same coordinate space `$local-app-inspect-view`'s `rect`
  and `$local-app-capture-view`'s converted image coordinates use. `phase`
  is `tap` (default), `down`, `move`, or `up`. Drive a drag as `down` at the
  start point, one or more `move`s, then `up`; the surface receives a
  `buttons`-held state on `move` so a handler gated on `if (!e.buttons)
  return` still fires. A point outside the current viewport is rejected
  with an error naming the viewport size, not silently retargeted.
- `key` takes `value` `"<key>"` or `"<key>,phase"`, where `<key>` is a DOM
  key name (`ArrowLeft`, `a`, `Space`, …) and `phase` is `press` (default),
  `down`, or `up`. The event is dispatched on whatever last received focus,
  or the page's own canvas, or the page body — never lost because nothing
  had focus after a pointer tap.

## Reading the result

A successful call returns whatever the page's own action handler reported,
always including at least `{ok: true, action: "<action>"}`. Beyond that the
shape depends on the action — e.g. `pointer` also reports the coordinates,
phase, and which element received the event; a resolved-element action
reports the element it acted on. Treat this as the action's own receipt, not
a fixed schema to depend on field-by-field beyond `ok`/`action`.

A failed call fails closed with a message naming what went wrong — a target
that resolved to nothing, a disabled target, a point outside the viewport, a
non-toggleable target, an unsupported action — rather than answering
`ok: true` for something that did not happen. `back` fails when the app has
no history to go back to; it does not silently no-op.

## Permission and timing

The first UI action against an app in a session prompts the user for
permission to control that app's visible interface; a denial fails the call
rather than queuing it. An action can also time out if the page never
answers, or report as cancelled if the WebView went away mid-action — both
are failures to report as-is, not something to retry silently in a loop.

`back` and `reload` briefly leave the page not-ready while it navigates;
give the page a moment (or re-`inspect`/`capture`) before driving the next
action rather than firing an action into a page that is still loading.

## Boundaries

- Never pass raw JavaScript, a script string, or an `eval`-style value —
  there is no action that accepts one. If a task seems to need arbitrary
  script execution, that is out of scope here; report the limitation
  instead of working around it.
- Never dispatch a `pointer` action at coordinates read from a stale
  snapshot or an old screenshot. Rects and images go stale on scroll,
  resize, or re-render exactly as `$local-app-inspect-view` and
  `$local-app-capture-view` describe; re-inspect or re-capture immediately
  before addressing a point.
- Report the action's own result. Do not additionally claim the app "looks
  right" or "worked as intended" from here — that judgment, and any
  before/after evidence to support it, belongs to `$local-app-inspect-view`,
  `$local-app-capture-view`, `$local-app-test`, or `$local-app-debug`.
