# Input, overlays, and QA

- Define every core scene action in both pointer or touch and keyboard terms.
- Keep HUD, menus, pause, onboarding, and failure states in ordinary UI layers
  above the canvas.
- Reduced motion should reduce camera drift, shake, or particle intensity while
  keeping the app navigable.
- When Havok-backed motion is in scope, keep fixed-step or clamped-step updates
  predictable across pause and resume.
- Keep at least one deterministic update path testable without a live renderer.

## Sources

Reviewed: 2026-08-27

- Babylon camera inputs: https://doc.babylonjs.com/features/featuresDeepDive/cameras/camera_inputs
- Babylon Havok plugin: https://doc.babylonjs.com/features/featuresDeepDive/physics/havokPlugin
- WCAG 2.2: https://www.w3.org/TR/WCAG22/
