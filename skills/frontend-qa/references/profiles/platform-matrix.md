# Platform matrix

Run the same scenario across targets only when the Host verification scope
authorizes those targets on the current device. Keep the complete declared
target matrix in the report, but record `unverified_target_ids` and
`unverified_scenario_ids` for platforms not matched by the current Host
device; do not attempt them through guessed device metadata or call them pass.

- Browser preview: first paint, fatal console errors, primary interaction,
  navigation, back, loading and error handling.
- iPhone: safe area, 44 CSS-pixel controls, iOS navigation/back, text scaling, reduced
  motion.
- Android phone: 48dp controls, Android back, Material feedback, permission and
  recovery flows.
- iPad portrait and landscape: resize, split/sidebar suitability, pointer,
  keyboard, and orientation changes.
- Android tablet portrait and landscape: rail/drawer or list-detail behavior,
  resize, pointer, keyboard, and system back.
- Desktop: resize, hover/focus, keyboard-first use, and long-width behavior.

If a target was not requested, say so instead of silently skipping it.
Record bridge-dependent checks, reduced-motion behavior, and at least one
double-frame motion observation for animated canvas or Three.js scenes.
For iPhone/iPad checks, use the active contract's Apple requirements and the
actual Host `verification_scope`; include system-font/Ionic navigation and the
reserved bottom-leading 80-by-80 CSS-pixel host control corner where in scope.
Do not infer an out-of-scope device from the launching device or report a
partial current-device run as a full matrix. Screenshots and synthetic pointer
events do not prove physical smoothness.

## Sources

Reviewed: 2026-09-06

- Apple Human Interface Guidelines: https://developer.apple.com/design/human-interface-guidelines
- Android adaptive apps: https://developer.android.com/develop/adaptive-apps/guides/get-started-with-adaptive-apps
- WCAG 2.2: https://www.w3.org/TR/WCAG22/
