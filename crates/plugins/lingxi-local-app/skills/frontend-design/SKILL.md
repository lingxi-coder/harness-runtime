---
name: frontend-design
description: Produce a platform-specific local-app design_spec for LingXi apps, separating iOS/iPadOS, Android, desktop, and DOM versus canvas overlay presentation.
---

# Frontend design

Turn the confirmed product brief into one compact `design_spec` that another
agent can implement without guessing.

Act as a lightweight platform/output router. The coordinator owns the
conversation, target inference, conditional skill loading, acceptance checks,
and final AuthoringSpec; this skill translates that confirmed contract into
target-specific output and does not open another questionnaire or confirmation.
For a design run, normalize each `targets[].os` with
`String(target.os ?? "").trim().toLowerCase()` before routing. An iOS/iPadOS
output has an explicit `lingxi-local-app:apple-design` skill dependency. Each
designing role must load that namespaced skill once in its own execution before
producing iOS/iPadOS output; a coordinator's earlier load is not inherited by a
child role. Reuse it only if already loaded in this same role execution.
Do not load it for verify-only or
code-only work. Adapt the guidance to the Host/Ionic shell, keep an explicit
brand or reference authoritative, and apply Apple guidance only to affected UI
targets. Do not add APIs, dependencies, or nested token/state fields.

Always:

- start from the confirmed product goal, target matrix, and surface choice, then
  map those decisions into one explicit `design_spec`;
- keep business logic shared but describe presentation per target;
- output distinct presentations for every requested target in `targets[]`;
- separate DOM screen design from canvas HUD/overlay design;
- refuse a single "responsive" answer when multiple platforms are in scope.

Honor a machine-readable schema supplied by the caller; map these decisions
into that schema instead of wrapping or renaming its top-level fields. When the
caller supplies no schema, return a `design_spec` with at least:

```yaml
design_spec:
  surface: dom | canvas
  targets:
    - os: ios | android | desktop
      form_factor: iphone | ipad | phone | tablet | desktop
      presentation: ""
      navigation: ""
  information_architecture:
    screens: []
    states:
      loading: ""
      empty: ""
      error: ""
      success: ""
      permission: ""
  visual_system:
    design_direction: ""
    tokens:
      color: []
      typography: []
      spacing: []
      radius: []
      elevation: []
      motion: []
      safe_area: []
  interaction_model:
    pointer_touch: []
    keyboard_mouse: []
    back: []
    reduced_motion: []
  adapter_boundary:
    shared_logic: []
    platform_specific_shell: []
  acceptance_checks: []
  summary: ""
```

Routing is load-mode aware: bundled runtimes must use the `Bundled resource`
section below and must not read from the app workspace (`references/router.md`
or its profiles). File-backed runtimes follow the markdown link
[references/router.md](references/router.md), then only the renderer and
platform profiles that match the confirmed scope. Do not edit source files,
install packages, or invent unsupported platform behavior inside this skill.
Return material new design decisions to the orchestrator for confirmation
before implementation; this skill does not write the implementation itself.
