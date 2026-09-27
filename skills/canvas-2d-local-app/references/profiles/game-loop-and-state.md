# Game loop and state

- Base the scene on the profile-managed `createFrameLoop` helper in
  `lib/frame-loop.js`; start it inside an effect and stop it in that
  effect's cleanup. It provides clamped `dt` and resize awareness. Do not call
  `requestAnimationFrame` directly or hand-write a replacement loop.
- Follow the template pattern: scene size in CSS pixels, backing store scaled
  by DPR, and clock reset when visibility resumes.
- Keep coarse UI state in React or a store, and keep per-frame state in refs or
  pure simulation modules.
- Define an explicit phase machine such as menu, playing, paused, error, and
  over or complete.
- Keep collision logic simple and readable unless the brief genuinely demands
  more.
- Put simulation updates behind pure functions where possible; any randomness
  should be injectable or seedable for tests and replay.
- Handle pause and resume without replaying an unbounded catch-up frame.

## Sources

Reviewed: 2026-08-27

- MDN Canvas optimization: https://developer.mozilla.org/en-US/docs/Web/API/Canvas_API/Tutorial/Optimizing_canvas
- LingXi frame loop template: `lingxi-code/plugins/lingxi-local-app/assets/templates/canvas-2d/r4/lib/frame-loop.js`
- LingXi game store template: `lingxi-code/plugins/lingxi-local-app/assets/templates/canvas-2d/r4/src/stores/game-store.js`
