# Bridge and data

- Read host capabilities only through `window.lingxi.v2` and the checked-in
  bridge helpers.
- Read device, safe-area, color-scheme, reduced-motion, and input-mode changes
  through the checked-in provider or adapter boundary rather than ad hoc window
  probes.
- Normalize and validate `deviceContext` and bridge payloads before UI code
  consumes them.
- Treat `viewport`, `safeArea`, `colorScheme`, `reducedMotion`, and
  `inputMode` as live values that can change during a session.
- Reflect rejected writes, permission refusals, and network or device failures
  in UI state instead of swallowing them.
- Use the bridge's structured query and mutation contracts; do not invent a
  parallel client-side persistence layer.
- Do not make `localStorage`, IndexedDB, or a store the authority for declared
  host collections.

## Sources

Reviewed: 2026-08-27

- LingXi runtime contract: `docs/local-apps/RUNTIME-OS-V2.md`
- LingXi bridge helpers: `lingxi-code/plugins/lingxi-local-app/assets/templates/react-dom/r4/lib/lingxi-bridge.js`
- Platform adapter: `lingxi-code/plugins/lingxi-local-app/assets/templates/react-dom/r4/lib/platform-adapter.js`
