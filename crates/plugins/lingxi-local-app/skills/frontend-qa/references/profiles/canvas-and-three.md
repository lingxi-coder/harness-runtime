# Canvas runtime checks

- Use rendered-frame evidence, not DOM emptiness, to judge whether the app is
  alive.
- Verify phase transitions such as boot -> menu -> play -> pause -> resume ->
  game over or success.
- For every interactive action, test pointer/touch and keyboard input.
- Check resize, DPR changes, pause/resume, reduced motion, and asset fallback.
- Use double-frame observation for movement, animation, or camera drift.
- For Three.js, also check camera framing, renderer resize, material/light
  balance, context-loss recovery, and resource cleanup symptoms such as growing
  memory or duplicate scenes after restarts.
- For Phaser, also check scene boot/preload/create/update ordering, scale or
  resize handling, pause or resume semantics, input-manager focus recovery, and
  texture or atlas fallback when an asset is unavailable.
- For Babylon, also check engine resize, camera controls, context-loss or
  restore behavior, and, when Havok-backed motion is in scope, that the scene
  remains playable without runaway timestep spikes after pause or resume.

## Sources

Reviewed: 2026-08-27

- MDN Canvas API: https://developer.mozilla.org/en-US/docs/Web/API/Canvas_API
- MDN Basic animations: https://developer.mozilla.org/en-US/docs/Web/API/Canvas_API/Tutorial/Basic_animations
- three.js docs: https://threejs.org/docs/
- Phaser concepts: https://docs.phaser.io/phaser/concepts
- Babylon.js docs: https://doc.babylonjs.com/
