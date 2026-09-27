---
name: threejs-local-app
description: Build LingXi Local Apps whose primary scene uses Three.js 0.185.1 with explicit lifecycle, disposal, input, and performance rules.
---

# Three.js Local App

Use this skill when the confirmed Local App surface is canvas and the rendering
runtime profile family is `three_3d`.

Routing is load-mode aware: bundled runtimes must use the `Bundled resource`
section below and must not read from the app workspace (`references/router.md`
or its profiles). File-backed runtimes follow the markdown link
[references/router.md](references/router.md) first, then only the profiles
needed for the current scene work.

Use only `three@0.185.1` from the shipped scaffold.

Always:

- own one renderer and one animation loop
- handle scene, camera, lights, materials, resize, pixel ratio, and teardown
  explicitly
- dispose geometries, materials, textures, listeners, and renderer resources
- define pointer and keyboard behavior for the core actions
- keep overlays and platform chrome outside the scene

This skill is only for the persisted `three_3d` runtime profile. Do not switch
to `canvas_2d`, `phaser_2d`, or `babylon_3d`, and do not ask for a second 3D
runtime.

Do not introduce React Three Fiber, drei, helper scene wrappers, external
physics stacks, postprocess dependency piles, or any extra 3D runtime layer.
