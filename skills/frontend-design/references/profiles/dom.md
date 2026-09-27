# DOM profile

Use for routed screens, forms, lists, and detail views.

- Specify screen hierarchy, route depth, and back behavior.
- Define empty/loading/error/permission/success states per flow, not only once
  globally.
- Describe content width, pane behavior, and density per target.
- Call out where Ionic components are enough versus where custom layout is
  required.
- Require readable, concrete tokens instead of vague style adjectives.

Avoid:

- one-card-grid defaults;
- layout decisions expressed only as breakpoints;
- mixing tablet and desktop behavior into one generic large-screen answer.

## Sources

Reviewed: 2026-08-27

- Ionic app structure: https://ionicframework.com/docs/layout/structure
- Ionic React navigation: https://ionicframework.com/docs/react/navigation
