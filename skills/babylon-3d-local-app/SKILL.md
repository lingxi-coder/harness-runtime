---
name: babylon-3d-local-app
description: Build LingXi Local Apps whose primary scene uses the persisted Babylon 3D runtime profile with explicit engine lifecycle, input parity, and disposal rules.
---

# Babylon 3D Local App

Use this skill when the confirmed Local App surface is canvas and
`runtime_profile.family=babylon_3d`.

Routing is load-mode aware: bundled runtimes must use the `Bundled resource`
section below and must not read from the app workspace (`references/router.md`
or its profiles). File-backed runtimes follow the markdown link
[references/router.md](references/router.md) first, then only the profiles
needed for the current scene work.

Use only the persisted host-managed Babylon runtime already shipped with the
app: `@babylonjs/core@9.22.1`, `@babylonjs/loaders@9.22.1`, and
`@babylonjs/havok@1.3.14` only when that physics package is already part of the
confirmed runtime contract. If the host/runtime contract did not already
confirm `babylon_3d`, do not assume it exists and do not request package
installs from this skill.

Always:

- use the profile-managed `createBabylonRuntime` from
  `lib/babylon-runtime.js`; it is the single engine, scene, render-loop,
  resize, Havok initialization, and teardown owner, so app code supplies
  business callbacks and never creates a second engine or loop
- keep one Babylon scene lifecycle through that adapter
- define both pointer or touch and keyboard behavior for every core action
- handle resize, DPR changes, context loss, disposal, and pause or resume
  explicitly
- keep bridge, data, and overlay UI outside engine globals and surface native
  failures clearly
- keep deterministic scene-update seams for QA and logic checks

Do not add npm installs, alternate engines, React 3D wrappers, or external
physics or asset-service dependencies.
