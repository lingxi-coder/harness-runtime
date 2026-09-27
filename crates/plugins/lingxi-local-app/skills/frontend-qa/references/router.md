# Router

Read this file first.

## Load order

- Always read `profiles/evidence-model.md`
- Always read `profiles/platform-matrix.md`
- `surface: dom` -> read `profiles/dom-and-webview.md`
- `surface: canvas` -> read `profiles/canvas-and-three.md`
- `surface: canvas` -> also read `profiles/canvas-tool-protocol.md`
- `runtime_profile.family: three_3d` -> apply the Three.js checks inside
  `profiles/canvas-and-three.md`
- `runtime_profile.family: phaser_2d` -> apply the Phaser checks inside
  `profiles/canvas-and-three.md`
- `runtime_profile.family: babylon_3d` -> apply the Babylon checks inside
  `profiles/canvas-and-three.md`

## Report contract

When the caller supplies no result schema, return a `qa_report` with:

```yaml
qa_report:
  surface: dom | canvas_2d | three_3d | phaser_2d | babylon_3d
  ok: false
  targets: []
  checked_matrix: []
  browser_available: false
  webview_checked: false
  degraded_verification: true
  evidence:
    browser: []
    native_webview: []
    console_and_logs: []
    bridge_checks: []
  data_roundtrip:
    status: passed | not_applicable | failed
    collections: []
    evidence: ""
  render_check:
    status: failed
    canvas_surfaces: 0
    frames_captured: 0
    interactions_driven: []
    evidence: ""
  motion_check:
    status: failed
    frames_compared: 0
    difference: ""
  findings:
    - id: ""
      severity: blocker | major | minor
      target: ""
      surface: dom | canvas_2d | three_3d | phaser_2d | babylon_3d
      summary: ""
      repro: []
      evidence: []
      likely_source: []
      next_check: []
  summary: ""
```

The false and failed values above are conservative initialization values, not
an expected result. Derive them from recorded evidence:

- Set `ok: true` only when every required surface check passes and `findings`
  is empty. A failed required check or missing required evidence sets it false.
- For `canvas_2d`, `three_3d`, `phaser_2d`, and `babylon_3d`, `render_check.status` and
  `motion_check.status` are `passed` or `failed`, never `not_applicable`.
  Passing requires at least one inspected frame and two time-separated frames
  for motion, with the actual differences recorded.
- For a DOM app with no canvas, render and motion may be `not_applicable`; say
  which DOM evidence replaced them.
- If Browser is unavailable, set `degraded_verification: true`. Do not infer
  success from an unexercised target or an empty evidence array.

Evidence-first means no unsupported speculation, and no "probably fine" claims
without Browser/native/WebView evidence.

## Sources

Reviewed: 2026-08-27

- LingXi local-app handoff: `docs/local-apps/HANDOFF.md`
- LingXi create-local-app skill: `skills/create-local-app/SKILL.md`
- WCAG 2.2: https://www.w3.org/TR/WCAG22/
