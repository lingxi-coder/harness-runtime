# Local App AG-UI package

This package binds the Local App browser and Desktop transports to the official
AG-UI 1.0 TypeScript SDK. The protocol surface comes from the pinned
`@ag-ui/core@1.0.3` and `@ag-ui/client@1.0.3` packages; application code should
use their `RunAgentInput`, event, message, and `ContentPart` types directly.

## Exports

- `@lingxi/local-app-agui/client` exports `LocalAppAgent` and
  `createLocalAppAgent`, a custom-transport implementation of the official
  `AbstractAgent` API.
- `@lingxi/local-app-agui` exports the client adapter, File helper, validators,
  and official SDK types and schemas.
- `@lingxi/local-app-agui/browser` exports the browser host installer.
- `@lingxi/local-app-agui/browser/global` is the deterministic browser IIFE
  built for the Harness host script.
- `@lingxi/local-app-agui/browser/source` exports that IIFE as a deterministic
  ESM string (`browserAgentSource`) for host-script injection and tests.
- `@lingxi/local-app-agui/schema` is the frozen official 1.0 JSON Schema used
  by the Rust request boundary.

The host transport is authenticated and app/run scoped. Its `start` operation
accepts a standard `RunAgentInput`; event replay returns standard AG-UI events
in a transport-only `{events, sequence}` page. The sequence cursor is not an
event field. `LocalAppAgent.run` starts once, then reads event batches until an
official `RUN_FINISHED` or `RUN_ERROR` event arrives. `connectAgent` only reads
existing history/events and never starts a model run.

Local App options live in `RunAgentInput.forwardedProps['com.lingxi.localApp']`
with `version: 1`. They are not added as top-level protocol fields. Local file
content uses the standard inline `DataSource`; images are `ImagePart`s and
other files are `DocumentPart`s. The helper stores the filename in standard
`ContentPart.metadata` under the versioned `com.lingxi.localApp` namespace.

## Reproducibility

`spec/ag-ui-1.0.schema.json` is the official schema copied byte-for-byte from
the same AG-UI repository revision as the pinned SDK release. Its source
revision, canonical URL, byte count, and SHA-256 are recorded in
`spec/provenance.json`. `package-lock.json` locks the npm dependency closure.
Run `npm ci`, `npm run build`, and `npm test` from this directory.
