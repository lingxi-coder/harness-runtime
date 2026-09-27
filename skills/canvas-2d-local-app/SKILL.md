---
name: canvas-2d-local-app
description: Build LingXi Local Apps whose primary interaction surface is a Canvas 2D scene with a deterministic loop, native-feeling overlays, and bridge-safe input handling.
---

# Canvas 2D Local App

Use this skill when the confirmed Local App surface is canvas and the primary
runtime profile family is `canvas_2d`.

Routing is load-mode aware: bundled runtimes must use the `Bundled resource`
section below and must not read from the app workspace (`references/router.md`
or its profiles). File-backed runtimes follow the markdown link
[references/router.md](references/router.md) first, then only the profiles
needed for the scene you are building.

Always:

- use the profile-managed `createFrameLoop` from `lib/frame-loop.js`; start it
  inside an effect and stop it in that effect's cleanup
- do not call `requestAnimationFrame` directly or hand-write a replacement loop
- model an explicit phase machine
- define both pointer or touch and keyboard behavior for every core action
- keep per-frame simulation state out of React UI state
- support resize, DPR changes, suspend or resume, reduced motion, and bridge-safe overlays
- keep deterministic logic test seams for phase and simulation checks

This skill is only for the engine-free `canvas_2d` runtime. Do not switch to
`three_3d`, `phaser_2d`, or `babylon_3d`, and do not add a package-based scene
runtime.

Use browser and scaffold primitives only. Do not add a separate game engine or
rendering wrapper.
