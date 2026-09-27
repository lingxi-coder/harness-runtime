# Apple Design in Local App creation

## Decision

Use the bundled `lingxi-local-app:apple-design` skill by default for confirmed
iOS/iPadOS targets, while preserving explicit user branding and references.
The target OS chooses design guidance; the device launching creation does not.
This changes generated Local Apps, not LingXi's own native interface.

Apple Design supplies the Apple presentation rules. `frontend-design` retains
the target routing and structured-output contract. Ionic remains the renderer
guide for DOM apps; Apple Design is not a second renderer or navigation stack.
No dependency, template, public API, authoring schema, or data migration is
needed.

## Implementation sequence

1. Preserve existing workflow/schema regression coverage and add a small
   routing regression before changing the workflow.
2. Bundle a fixed upstream Apple Design revision with MIT attribution, product
   adaptations, exact root-skill mirrors, and the existing inventory checks.
3. Load the skill on demand before the coordinator's UI proposal and in the
   relevant designer/builder roles. Do not add it to static role preloads.
4. Replace duplicated iOS design rules with the routing/Host adaptation and
   add compact, contract-bound QA guidance.

## Behavioral boundaries

- Create: Apple guidance applies in all quality modes; fast still skips the
  designer, so the coordinator and builder must apply it.
- Update: only a Host-confirmed UI-impacting change activates new design
  guidance, within the requested scope. Existing branding and contract stay
  authoritative; code-only updates do not restyle an app.
- Repair: preserve the effective contract and repair only Host-localized
  source findings. Never introduce a redesign or new acceptance requirements.
- Verify: read the existing contract and evidence; do not load authoring
  guidance or mutate the App.
- Mixed targets: apply Apple rules only to Apple presentations. Use separate
  iPhone/iPad layouts where both are requested.
- Canvas: apply Apple guidance to menu/HUD/form overlays, not the scene or
  runtime engine.

Designer output remains exactly `{ "design": ... }`, with string token values.
Collect acceptance requirements before confirming the AuthoringSpec; neither
designer nor QA may silently add new requirements afterward. Preserve the Host
approval/scaffold boundary, single renderer guide, fixed dependencies,
device-context provider, and bottom-leading 80-by-80 CSS-pixel control reserve.

## Verification scope for this delivery

The user requested fast development and only a few necessary tests. Run the
plugin inventory/mirror check, focused workflow routing/fast regressions and
runtime validators, plus a focused skill-loader test when the build environment
allows it. Retain existing preload budgets and first-pass agent counts.
Do not run the full repository or device matrix for this delivery.

Automated DOM actions/captures can demonstrate operation and final state, not
physical gesture smoothness. `motion_required` remains Canvas-only. Runtime QA
must preserve Host in-scope/unverified targets and use actual evidence. Physical
gesture continuity, keyboard/sheet interaction, and the full iPhone/iPad visual
matrix require a subsequent device validation; do not report those as tested.

## Work allocation

Luna implements the bounded skill-bundle, workflow, and guidance changes.
Astra coordinates, reviews, integrates, and takes over repeatedly failing
fixes. Preserve unrelated work already present in this shared checkout.

## Delivery evidence

- Plugin checker passed with 28 skills and an exact build inventory; mirrors
  and whitespace checks passed.
- Six focused Rust checks passed: Apple target routing, fast UI update,
  workflow runtime validators, existing first-pass agent counts, namespaced
  skill loading with a short-name collision, and static preload byte budgets.
- Actual preload sizes remained within the existing limits: builder 14,701
  bytes and designer 29,038 bytes. The on-demand Apple skill loaded separately.
- Astra review found no remaining blocking issue after corrections. Skill
  invocation/failure handling remains agent-instruction based, consistent with
  renderer-guide loading; this change adds no execution receipt protocol.
- Full-suite, generated-app visual, and real-device gesture validation were
  intentionally not run under the requested fast-development scope.
