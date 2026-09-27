# Current template library upgrade

## Scope and sequence

1. Exercise the current bridge and lifecycle behavior before changing it; add regressions for mismatches with the native v2 API.
2. Publish a complete r4 template for each family using the existing Node 26.9.0 / pnpm 12.5.1 dependency lockfiles. Upgrade the bridge/provider/runtime helpers without adding dependencies.
3. Remove r1/r2/r3 templates, revision dispatch, and legacy toolchain compatibility as explicitly requested. Do not migrate or rewrite existing application workspaces or data.
4. Make Host includes, template mirrors, inventories, catalog hashes and supply-chain checks agree on the sole current revision.
5. Run JavaScript behavior tests, template builds, Rust profile/build/catalog tests and static checks. Babylon remains unavailable until its existing device-validation gate is satisfied.

Old application contracts are intentionally unsupported after this change. The
template revision is still recorded for integrity, not for fallback routing.

## Implemented changes

- The five template families now own their complete r4 library sources. The
  Host catalog and build targets support only the current revision; r1/r2/r3
  directories, dependency overlays and dispatch branches are removed.
- Bridge haptics/deep links match the native scalar API. Retired network/runtime
  aliases are removed. Provider refreshes after page restore, visibility changes
  and visual viewport scrolling; frame loops also handle DPR changes.
- Phaser and Babylon terminal cleanup cannot restart destroyed runtimes, and
  Phaser asynchronous startup is released exactly once.
- The Local App Node 24 / pnpm 11 compatibility installer and pins are removed.
  Current Node 26.9.0 / pnpm 12.5.1 packages and lockfile versions are unchanged.

## Validation

- 36 JavaScript behavior regressions pass, including real iOS Host bridge payload
  tests and simulated React/graphics lifecycle cases.
- All five family templates build with the pinned Node 26.9.0 and Vite 8.3.0 in
  isolated temporary workspaces using frozen lockfiles. Babylon build success
  does not remove its real-device availability gate.
- 144 Rust regressions pass: profiles 14, catalog 5, build 59, MCP 2,
  workflow support 60, workflow binding 3, current-toolchain dependency cache 1.
- Plugin inventories, source mirrors and supply-chain/rootfs tooling checks pass.

No device installation or new native rootfs artifact was produced. Old app
contracts intentionally fail instead of falling back to historical templates.
