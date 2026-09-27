# Lifecycle and performance

- Create scene, camera, renderer, and lights in one owned initialization path.
- Keep default materials and lighting readable on phone-class displays before
  adding heavier post effects or complex shader work.
- Drive `renderer.render(scene, camera)` from the profile-managed
  `createFrameLoop` in `lib/frame-loop.js`; do not start
  `renderer.setAnimationLoop` or a second
  `requestAnimationFrame` owner alongside it.
- On resize, call `renderer.setSize(width, height, false)` and update camera
  aspect or projection parameters and any dependent picking math. The third
  argument is required: `createFrameLoop` owns the drawing buffer and the CSS
  box, and `setSize`'s default `updateStyle: true` writes inline
  `style.width`/`style.height` onto the canvas, pinning the box the helper's
  ResizeObserver watches — after which a rotation or Split View change resizes
  the container while the canvas keeps its old size and the observer never
  fires again. Use the `width`/`height` the helper reports rather than reading
  the element, and do not call `setPixelRatio` above the helper's clamp.
- Cap effective pixel ratio when necessary for thermals and frame stability.
- On teardown, dispose geometries, materials, textures, render targets, and
  event listeners, then release renderer resources.
- Handle WebGL context loss and restoration intentionally instead of leaving the
  app in a blank frame.
- Keep default scenes inside mobile-friendly geometry, texture, and draw-call
  budgets, with simple materials and lighting unless the brief clearly warrants more.
- Avoid per-frame object allocation in hot paths.

## Sources

Reviewed: 2026-08-27

- WebGLRenderer docs: https://threejs.org/docs/#api/en/renderers/WebGLRenderer
- Disposal guide: https://threejs.org/manual/en/how-to-dispose-of-objects.html
- MDN WebGL context-lost event: https://developer.mozilla.org/en-US/docs/Web/API/HTMLCanvasElement/webglcontextlost_event
