---
name: local-app-inspect-view
description: Read a local app's structured DOM/accessibility snapshot — elements, canvases, viewport, runtime errors — as evidence. Never guesses invisible semantics from a screenshot.
---

# Inspect a local app's view

Own structural evidence about a running app's own view: what elements exist,
where they are, what canvases exist, and what the page itself reported as a
runtime error. This is read-only and never executes arbitrary JavaScript.

## The tool

Call `LocalAppInspectUi` with `app_id` and an optional `selector` (an
element id) to narrow the snapshot. The result is a JSON snapshot, not an
image:

- `title`, `url`, `documentState` — the page's own identity and load state.
- `viewport` — `{width, height, offsetLeft, offsetTop, scale}` in CSS
  pixels, from the visual viewport at the moment of the call.
- `elements` — up to 200 interactive candidates (buttons, links, inputs,
  anything with a role or tabindex), each `{elementId, role, name, value,
  checked, disabled, visible, rect:[left, top, width, height]}` in CSS
  pixels. `value` is always `null` for a password or hidden input; never
  treat a non-null `value` on those as meaningful, and never repeat one back.
- `canvases` — up to 16 canvas elements, each `{rect:[left, top, width,
  height]}`; `canvasCount` is a separate, larger count (up to 64) kept for
  compatibility with callers that only need the number.
- `runtimeErrors` — up to 8 errors the page itself threw, plus
  `runtimeErrorsDropped` if more were suppressed.
- `truncated` — which of `elements`/`canvases`/`runtimeErrors` were cut down
  to fit a size budget; treat a truncated list as partial, not exhaustive.

Rects are CSS pixels captured at call time and go stale after a scroll,
resize, or re-render — re-inspect rather than reusing an old rect to target
a later action.

## Reading an empty `elements` list correctly

An empty (or near-empty) `elements` array means the same thing whether the
app is a correctly-rendering canvas/WebGL surface, a blank page, or a
crashed one — the DOM snapshot cannot tell those apart on its own. Use
`canvasCount`/`canvases` to check whether a drawn surface is actually in
play:

- `canvasCount > 0` — this is a drawn surface with nothing else to inspect;
  do not describe what it looks like from this snapshot. Hand off to
  `$local-app-capture-view` for a visual frame instead of inferring content,
  layout, or correctness from the absence of elements.
- `canvasCount == 0` and `elements` is still empty — that is real evidence
  of a blank or failed page; check `runtimeErrors` and `documentState`
  before concluding anything further.

## Boundaries

- This skill reports what the structured snapshot says, nothing else. Do not
  describe colors, layout appearance, or "how it looks" from this data —
  that is not something a DOM/accessibility snapshot can tell you; that
  belongs to `$local-app-capture-view`.
- Do not drive pointer or keyboard input from here, even to "just check" —
  that is `$local-app-interact`'s job, and this tool never executes
  JavaScript or dispatches events.
- Do not judge pass/fail or diagnose a root cause here; report the snapshot
  and let `$local-app-test` or `$local-app-debug` make that call.
