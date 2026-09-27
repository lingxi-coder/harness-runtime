# Router

Read this file first.

- Loop, resize, phase, and simulation ownership ->
  `profiles/game-loop-and-state.md`
- Assets, audio, reduced motion, accessibility, and QA seams ->
  `profiles/assets-audio-and-qa.md`

## Required scene shape

- one owned `<canvas>`
- one frame loop started through the profile-managed `createFrameLoop` helper
  in `lib/frame-loop.js` and stopped in effect cleanup
- one explicit phase machine
- one overlay layer for menus, pause, onboarding, and failure states
- one keyboard path alongside pointer or touch input for each core action

Do not call `requestAnimationFrame` directly or hand-write a replacement loop.
Do not add a separate game engine, dependency-install steps, or external
asset-service requirements.

## Sources

Reviewed: 2026-08-27

- MDN Canvas optimization: https://developer.mozilla.org/en-US/docs/Web/API/Canvas_API/Tutorial/Optimizing_canvas
- MDN Web Audio API: https://developer.mozilla.org/en-US/docs/Web/API/Web_Audio_API
- LingXi frame loop template: `lingxi-code/plugins/lingxi-local-app/assets/templates/canvas-2d/r4/lib/frame-loop.js`
- LingXi canvas screen template: `lingxi-code/plugins/lingxi-local-app/assets/templates/canvas-2d/r4/app/screens/game-screen.jsx`
