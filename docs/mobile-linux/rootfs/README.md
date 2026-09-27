# Alpine 3.24.2 Android rootfs pipeline

This directory defines the reproducible evidence contract for the Android
MobileLinux rootfs. Release archives come from the official Alpine mirrors and
must match `docs/mobile-linux/mobile-linux-pins.json`.

Pinned distribution baseline:

- Alpine release: `3.24.2`
- Repository branch: `v3.24`
- Supported targets:
  - `android-proot` / `android` / `arm64`
  - `android-proot` / `android` / `x86_64`

Fixed primary package set:

- `apk-tools`
- `busybox`
- `git`
- `nodejs`
- `npm`
- `openssh-client`
- `python3`
- `ca-certificates`

Local-app runtime pins:

- Official Node `26.9.0` source built for musl; source hash, compiler inputs,
  builder image and binary provenance are verified.
- `git 2.54.0-r0`
- `npm 12.0.2` (including `npx`) remains available for user terminals;
  the host-owned dependency job uses the pinned `pnpm 12.5.1` CLI installed
  from integrity-pinned npm and native platform tarballs. `corepack` and `yarn` remain excluded.
- Native TypeScript `7.0.2` is installed from the integrity-pinned official
  per-architecture package at `/opt/lingxi/toolchains/typescript/7.0.2/tsc`.
  Each rootfs build must pass both `--version` and an LSP `initialize` exchange.
- The app template is text-only: it contains the committed package manifest
  and lockfile but no `node_modules`. Each app runs a host-owned, locked
  `pnpm install --frozen-lockfile --ignore-scripts --no-runtime --prefer-offline`
  inside the isolated runtime, writing dependencies to its own workspace and
  using a host-owned cache mount only for that install. Generated code cannot
  invoke package managers.
- iOS development/full bundles carry a verified dependency seed keyed by the
  r4 lockfile digest; drifted dependency requests are installed per workspace.

`docs/mobile-linux/local-app-runtime-pins.json` records the exact APK, npm, pnpm,
and native TypeScript
pins. The structural verifier accepts an explicitly recorded upstream gap, but
the `--release` gate remains closed until x86_64's fully hashed closure has also
passed an offline install on a native x86_64 or qemu-backed host. This prevents
development checks from treating artifact hashes alone as executable evidence.

`docs/mobile-linux/local-app-runtime-policy.json` is the executable contract:
the host invokes the workspace-local Vite CLI through `/usr/bin/node`, exposes
only one writable build-project mount, binds production servers to loopback,
and enforces build/start timeouts. Build process trees receive 2048/3072/4096 MiB
for devices with `<6`/`6–<8`/`>=8` GiB of physical memory, with Node old-space
fixed to 75% of that budget; Full runtime process trees remain limited to 800
MiB. The host-owned dependency job is the only product path allowed to invoke
pnpm; generated jobs and MCP source-edit operations never invoke package
managers or npx.
User interactive terminals remain separately permission-gated.

Policy decisions:

- The interactive, user-opened terminal retains `apk` and networking so the
  user can explicitly install packages.
- Plain PRoot/rootfs isolation is not a security boundary. Agent-initiated
  package installation and external-mount writes remain controlled by the host
  permission gate; local-app builds enter through the seccomp policy launcher
  and are accepted only when its enforcement receipt proves `Disabled`.
- Android builds use an inherited seccomp filter for `Disabled`. Direct/Full
  starts PRoot's pinned sockaddr-aware extension for `LoopbackOnly`; it permits
  only AF_UNIX, `127.0.0.0/8`, and `::1`. Both paths publish policy-specific
  receipts, and launch fails with `network_policy_unavailable` if the expected
  receipt is absent or invalid.
- iOS builds apply and then restore a pinned iSH syscall patch during archive
  construction. The patch enforces the same `Disabled`/`LoopbackOnly` socket
  contract per inherited execution context; a 250 ms watchdog enforces the
  selected build tier or the Full runtime's 800 MiB process-tree limit.
- The source archive format is Alpine's official `tar.gz`; its digest is a
  source pin. The deterministic package-augmented release archive has a
  separate manifest digest and Gradle stores it without recompression.
- The rootfs manifest allowlist must hash every shipped ELF executable,
  interpreter, and shared library that remains in the release archive.
- The release evidence set must include:
  - `rootfs-manifest.json`
    - schema v2 includes a complete immutable regular-file/symlink inventory;
      writable roots are excluded and verified separately
  - `rootfs-build.lock.json`
  - `rootfs.spdx.json`
  - `executable-allowlist.json`

Recommended build flow:

1. Download both official Alpine 3.24.2 minirootfs archives named in the pin
   manifest and verify SHA-256 before extraction or modification.
2. Produce the complete package/license inventory and immutable content digest.
3. Generate `rootfs-manifest.json` and `rootfs.spdx.json` for each ABI.
4. Stage only through `clients/android/scripts/stage-mobile-linux-assets.sh`.
5. Publish the archives, evidence, and corresponding source together.

Schema v1 manifests are intentionally rejected because they lack the immutable
file inventory required to detect runtime tampering.

Tooling:

- `scripts/mobile-linux/rootfs_tool.py`
  - validate extracted trees
  - validate tar archives
  - generate rootfs manifests
  - generate SPDX SBOMs
  - generate release lock files
  - snapshot executable allowlists
- `scripts/mobile-linux/package-rootfs-release.sh`
  - high-level wrapper for packaging a reviewed extracted rootfs into release
    evidence
- `scripts/mobile-linux/test-rootfs-tooling.sh`
  - positive/negative tests for the packaging and verification helpers
- `clients/android/scripts/verify-local-app-supply-chain.sh`
- `clients/ios/scripts/verify-local-app-supply-chain.sh`
  - validate the shared Node/Next/Git pins, template lockfile, source policy,
    and SPDX inventory
