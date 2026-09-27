# Android phone and tablet profile

Use Android and Material-shaped presentation, not iOS chrome with different
colors.

- Phone: 48dp targets, top app bar patterns, Android back behavior, state-layer
  feedback, and FAB only when the primary action truly warrants it.
- Tablet: prefer navigation rail or drawer, adaptive list-detail or supporting
  panes, larger browsing surfaces, and explicit portrait/landscape behavior.
- If both phone and tablet are targeted, write different `presentation` values
  and navigation models.

Avoid:

- copying iOS tab/back conventions literally;
- stretching a phone layout across a tablet;
- assuming width alone determines navigation choice.

## Sources

Reviewed: 2026-08-27

- Adaptive apps overview: https://developer.android.com/develop/adaptive-apps/guides/get-started-with-adaptive-apps
- Adaptive navigation: https://developer.android.com/develop/adaptive-apps/guides/build-adaptive-navigation
- Adaptive do's and don'ts: https://developer.android.com/develop/adaptive-apps/guides/adaptive-dos-and-donts
- Navigation rail: https://developer.android.com/develop/ui/compose/components/navigation-rail
