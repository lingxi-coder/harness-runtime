# Canvas HUD and overlay profile

Use when the main experience is drawn on canvas or WebGL and Ionic/DOM is only
for HUD, menus, settings, pause, onboarding, or game-over flows.

- Separate the drawn surface from overlay chrome in `design_spec`.
- Define HUD placement, safe-area behavior, and when overlays pause or dim the
  simulation.
- Specify how score, status, and warnings stay legible over motion.
- Describe reduced-motion behavior for both the simulation and overlays.
- Every gameplay action must map to both pointer/touch and keyboard controls.

Avoid:

- treating the canvas like a normal page with stacked cards;
- burying essential game state only inside pixels;
- reusing desktop HUD density on phones.

## Sources

Reviewed: 2026-08-27

- MDN Canvas API: https://developer.mozilla.org/en-US/docs/Web/API/Canvas_API
- MDN Basic animations: https://developer.mozilla.org/en-US/docs/Web/API/Canvas_API/Tutorial/Basic_animations
