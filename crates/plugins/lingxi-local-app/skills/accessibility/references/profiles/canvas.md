# Canvas accessibility profile

Canvas has no native landmarks, headings, or control rectangles, so provide
equivalents explicitly.

- Give the canvas a real accessible name and short text description.
- Mirror score, phase, warnings, and win/lose state into text or live-region
  output.
- Every action must work without a pointer: keyboard at minimum, plus switch or
  stepwise alternatives where dragging or timing would otherwise block access.
- Do not communicate state by color alone.
- Reduced motion must keep the app playable, not only remove decorative motion.

Do not report missing DOM landmarks or tab order inside a drawn surface. Report
only issues the app can act on: missing labels, missing live state, missing
non-pointer controls, unreadable contrast, or unbounded motion.

## Sources

Reviewed: 2026-08-27

- WCAG 2.2: https://www.w3.org/TR/WCAG22/
- MDN Canvas API: https://developer.mozilla.org/en-US/docs/Web/API/Canvas_API
