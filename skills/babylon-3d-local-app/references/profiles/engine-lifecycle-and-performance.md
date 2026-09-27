# Engine lifecycle and performance

- Use the profile-managed `createBabylonRuntime` adapter as the one explicit
  engine, scene, camera, lighting, render-loop, resize, and disposal path; app
  code supplies behavior callbacks instead of creating another engine.
- Resize from the host-owned canvas box, update camera parameters, and avoid
  accidental duplicate render loops after hot reload or resume.
- Dispose meshes, materials, textures, observers, and engine resources on
  teardown.
- Handle WebGL context loss or restoration intentionally instead of leaving the
  app on a blank frame.
- Keep geometry, texture, and shader complexity conservative on phone-class
  hardware unless the brief clearly requires more.

## Sources

Reviewed: 2026-08-27

- Babylon scene docs: https://doc.babylonjs.com/features/featuresDeepDive/scene
- Babylon optimize your scene: https://doc.babylonjs.com/features/featuresDeepDive/scene/optimize_your_scene
