# Assets, audio, and QA

- Every action must define both pointer or touch behavior and keyboard behavior.
- Audio should start only after a user gesture and should fail soft when audio
  initialization is blocked.
- Image assets need preload or fallback handling. If an asset is unavailable,
  use a procedural fallback such as shapes, gradients, or text instead of a
  broken scene.
- When the confirmed brief requires original raster art and ImageGen is
  available, generate it under `public/` and record its intended use. ImageGen
  is optional: unavailable generation must fall back to procedural graphics.
- Reduced motion should reduce non-essential shake, particle density, or
  parallax, not just DOM transitions around the canvas.
- Publish scene state such as score, phase, and errors through readable overlay
  text.
- Keep at least one deterministic logic path testable without a live renderer.
- Keep frame cost conservative on phone-class hardware; avoid expensive full
  redraw work when a smaller invalidation path is enough.

## Sources

Reviewed: 2026-08-27

- MDN Web Audio API: https://developer.mozilla.org/en-US/docs/Web/API/Web_Audio_API
- WCAG 2.2: https://www.w3.org/TR/WCAG22/
