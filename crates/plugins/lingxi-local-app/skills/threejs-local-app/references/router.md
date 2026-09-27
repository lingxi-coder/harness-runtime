# Router

Read this file first.

- Renderer lifecycle, resize, budgets, disposal, and context loss ->
  `profiles/lifecycle-and-performance.md`
- Input, overlays, reduced motion, and QA seams ->
  `profiles/interaction-and-qa.md`

## Required scene shape

- one renderer per scene
- one explicit scene and camera setup path
- one owned animation loop
- one explicit teardown path

Keep the dependency pinned to `three@0.185.1`; do not add a parallel 3D stack.

## Sources

Reviewed: 2026-08-27

- Three.js docs: https://threejs.org/docs/
- Three.js disposal guide: https://threejs.org/manual/en/how-to-dispose-of-objects.html
- Local App runtime package: `lingxi-code/plugins/lingxi-local-app/assets/templates/three-3d/r4/package.json`
