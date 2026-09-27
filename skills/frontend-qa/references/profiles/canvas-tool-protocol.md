# Canvas tool protocol

`LocalAppInspectUi` describes selected DOM elements. A drawn surface can return
an empty `elements` list while rendering correctly, rendering blank, or having
crashed. Use its `canvasCount`, `canvases[].rect`, viewport, and runtime errors
for routing, then use `LocalAppCaptureUi` for visual evidence.

## Coordinates and capture

- A whole-view capture maps image pixel `(ix, iy)` to CSS coordinates as
  `(ix * viewport.width / image_width, iy * viewport.height / image_height)`.
- Inspect rectangles are arrays `[left, top, width, height]`; capture crops are
  objects `{x, y, width, height}`. Transpose the shape rather than passing it
  through unchanged.
- A cropped result reports `capture_rect`. Convert a crop pixel back with
  `(capture_rect.x + ix * capture_rect.width / image_width,
  capture_rect.y + iy * capture_rect.height / image_height)`.
- Inspect rectangles are layout CSS coordinates, while capture crops use the
  native view. Reuse an inspect rect only when `viewport.scale === 1` and both
  visual viewport offsets are zero. Otherwise capture the whole view.

## Input wire format

- Drive a canvas with `LocalAppActOnUi` action `pointer`; its value is `"x,y"`
  or `"x,y,phase"`, where phase is `tap`, `down`, `move`, or `up`.
- Drive keys with action `key`; its value is `"<key>"` or `"<key>,phase"`,
  where phase is `press`, `down`, or `up`. Spell the space bar `Space`.
- A DOM `click` cannot prove a canvas interaction. Record the actual pointer
  and key actions in the result.

Capture at least one visible frame for render evidence and at least two frames
for a motion claim. A playing surface whose frames do not change fails motion
verification unless the tested state was deliberately paused.

## Sources

Reviewed: 2026-09-21

- LingXi Android WebView inspector: `clients/android/app/src/main/java/com/lingxi/code/localapps/LocalAppWebView.kt`
- LingXi iOS WebView inspector: `clients/ios/Sources/LocalApps/LocalAppWebView.swift`
- LingXi frontend QA canvas gate (what actually encodes this protocol today):
  `lingxi-code/plugins/lingxi-local-app/schemas/use-test-report.schema.json`
  (`render_check` requires `canvas_surfaces`/`frames_captured`, `motion_check`
  requires `frames_compared`), reported by the
  `lingxi-local-app:local-app-use-test` workflow. `local_app_canvas_workflow.js`
  does not exist in this repo — an earlier draft cited it, and the former
  `workflows/local-app-build.js` that also carried the gate has been retired.
