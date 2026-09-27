# Interaction and QA

- Every core scene action needs both pointer and keyboard behavior.
- Keep HUD, menus, pause, onboarding, and failure states in ordinary UI layers
  above the canvas.
- Reduced motion should alter camera motion, particle density, or animation
  intensity when possible.
- Keep simulation or camera choreography testable through deterministic update
  seams or seeded inputs, even if rendering stays visual.
- Verify boot, active, paused, error, and completion phases when they exist.
- Test resize, orientation change, and context-loss recovery as explicit design
  and QA invariants.

## Sources

Reviewed: 2026-08-27

- Three.js docs: https://threejs.org/docs/
- WCAG 2.2: https://www.w3.org/TR/WCAG22/
