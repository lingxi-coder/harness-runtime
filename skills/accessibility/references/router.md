# Router

Read this file first.

- `surface: dom` -> read `profiles/dom.md`
- `surface: canvas` -> read `profiles/canvas.md`
- Any target set -> read `profiles/platforms.md`

## Required output

Return or verify an `a11y_contract` with:

```yaml
a11y_contract:
  surface: dom | canvas
  assistive_tech:
    ios_voiceover: []
    android_talkback: []
    desktop_keyboard_aria: []
  interaction:
    pointer: []
    keyboard: []
    switch_or_non_pointer: []
  motion_and_visibility:
    focus: []
    contrast: []
    reduced_motion: []
  open_findings: []
```

## Sources

Reviewed: 2026-08-27

- WCAG 2.2: https://www.w3.org/TR/WCAG22/
- MDN `:focus-visible`: https://developer.mozilla.org/en-US/docs/Web/CSS/:focus-visible
