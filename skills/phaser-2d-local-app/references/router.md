# Router

Read this file first.

- Scene ownership, lifecycle, and input mapping ->
  `profiles/scene-lifecycle-and-input.md`
- Assets, performance, reduced motion, and QA seams ->
  `profiles/assets-performance-and-qa.md`

## Required scene shape

- one profile-managed Phaser `Game` owner in `lib/phaser-runtime.js`
- one scene lifecycle with controlled preload/create/update responsibilities
- one overlay path for menus, pause, onboarding, and errors
- one keyboard path alongside pointer or touch input for each core action

Keep the persisted `phaser_2d` runtime profile intact. Do not switch to
`canvas_2d`, `three_3d`, or `babylon_3d`, and do not add package-manager or
runtime-migration steps from this skill. Do not instantiate `Phaser.Game`
outside the profile-managed adapter or start a parallel animation loop.

## Sources

Reviewed: 2026-08-27

- Phaser concepts: https://docs.phaser.io/phaser/concepts
- Phaser API docs: https://docs.phaser.io/api-documentation
- LingXi local-app handoff: `docs/local-apps/HANDOFF.md`
