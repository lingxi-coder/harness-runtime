# Router

Read this file first.

- Engine, scene, camera, resize, and disposal work ->
  `profiles/engine-lifecycle-and-performance.md`
- Input, overlays, reduced motion, and QA seams ->
  `profiles/input-overlay-and-qa.md`

## Required scene shape

- one profile-managed Babylon engine owner in `lib/babylon-runtime.js`
- one explicit app callback path for scene behavior and camera actions
- no second render loop beside the adapter
- one adapter-owned teardown path invoked from effect cleanup

Keep the persisted `babylon_3d` runtime profile intact. Do not switch to
`canvas_2d`, `three_3d`, or `phaser_2d`, and do not add package-manager or
runtime-migration steps from this skill. Do not instantiate a second Babylon
`Engine` or bypass the profile-managed adapter.

## Sources

Reviewed: 2026-08-27

- Babylon.js docs: https://doc.babylonjs.com/
- Babylon engine docs: https://doc.babylonjs.com/features/featuresDeepDive/scene/engine
- LingXi local-app handoff: `docs/local-apps/HANDOFF.md`
