# Local App WebMCP resource

This package vendors the unmodified w3c-oss/webmcp-polyfill source at the
commit recorded in provenance.json. It builds one browser IIFE for the Desktop
Local App scheme and the run-scoped Mobile Local App gateway.

The upstream installer exposes the standard document.modelContext API and
returns without replacing a native implementation. It has no runtime JavaScript
dependencies. It does not add an MCP transport or a navigator compatibility
alias.

Build with the repository's existing esbuild 0.28.2 installation:

    node ./scripts/build.mjs

The build verifies every pinned upstream source hash before generating
dist/webmcp-polyfill.iife.js, its source-string export, and the artifact
manifest. upstream/LICENSE is the upstream Apache-2.0 license.
