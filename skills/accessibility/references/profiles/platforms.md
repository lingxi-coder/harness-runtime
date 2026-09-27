# Platform accessibility profile

- iOS / VoiceOver: verify rotor-friendly labels, concise control names,
  announcements for modal and navigation changes, 44pt minimum hit comfort, and
  Dynamic Type tolerance.
- Android / TalkBack: verify descriptive labels, predictable traversal order,
  48dp targets, state announcements, and Android back behavior when dialogs or
  overlays are open.
- Desktop / keyboard + ARIA: verify visible focus, logical tab order, escape
  routes from overlays, shortcut discoverability when present, and correct ARIA
  only where native semantics are insufficient.

Across all targets, keep help, confirmation, and recovery paths available
without requiring gesture precision, motion tolerance, or color discrimination.

## Sources

Reviewed: 2026-08-27

- WCAG 2.2: https://www.w3.org/TR/WCAG22/
- Apple accessibility guidance: https://developer.apple.com/design/human-interface-guidelines/accessibility
- Android accessibility guidance: https://developer.android.com/guide/topics/ui/accessibility
- WAI keyboard interface: https://www.w3.org/WAI/ARIA/apg/practices/keyboard-interface/
