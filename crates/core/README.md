# Internal shared core

`core` is the internal dependency shared by the runtime, domain crates and
platform implementations. Its Rust import name is `lingxi_core`.

- `types`: shared IDs, messages, effects and other data that crosses crate boundaries.
- `host`: host capability contracts and shared cross-platform primitives.
- `settings`: settings schema, precedence, provenance and file loading.
- `events`, `state_machine`, `reducer`, `session`, `prompt`, `token`: conversation state and rules.

Put a type or interface in its owning domain crate when that dependency does
not create a cycle. Keep concrete platform selection, product assembly,
background services and SDK lifecycle in the owning platform/domain crate or
`harness-runtime`. The runtime re-exports the types its consumers need; a host
should not depend directly on this internal package.

The old `protocol` and `platform-api` packages are now the `types` and `host`
modules. Settings loading reports diagnostics through `SettingsLoadObserver`;
the telemetry crate owns analytics event formatting and PII tagging.
