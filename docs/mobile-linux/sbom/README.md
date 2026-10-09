# SBOM and license evidence contract

When `mobile-linux` is enabled, CI must have reproducible evidence for the shipped rootfs and runtime packaging.

Required outputs for each release candidate:

- SPDX or CycloneDX SBOM for the packaged rootfs contents
- package inventory covering BusyBox, Git, Node, native TypeScript/LSP,
  OpenSSH client, Python 3, standard library, and CA certificates
- executable allowlist snapshot aligned with `rootfs-manifest.json`
- license inventory for all shipped runtime components
- corresponding-source pins from the Git-pinned SDK `docs/toolchains/runtime-pins.json`
- the SDK-owned license texts and `docs/mobile-linux/LICENSES/NOTICE.md`
- the Local App runtime seed's `local-app-runtime.spdx.json`, generated from its pnpm lockfile
  and pinned by its SHA-256 digest; it is owned and checked by the `local-app-builder`
  repository (`docs/runtime/sbom`)

Required external release evidence layout (`MOBILE_LINUX_EVIDENCE_DIR`), alongside
the actual `MOBILE_LINUX_ROOTFS_ARCHIVE`:

- `rootfs-build.lock.json` and `rootfs-manifest.json`
- `rootfs.spdx.json`
- `licenses.json` (product approval inventory)
- `executable-allowlist.json`

Evidence shape enforced by CI:

- `rootfs.spdx.json` is an SPDX 2.x document whose `packages[]` covers every
  package in the release rootfs manifest.
- `licenses.json` has `schema_version: 1`, `status: "approved"`, and
  `components[]` entries with non-empty `id` and `license` values for at least
  `openminis`, `proot`, `talloc`, `alpine-rootfs`, and `typescript-native`.
- `executable-allowlist.json` has `schema_version: 1` and an `entries[]` array
  byte-for-field equivalent to the rootfs manifest executable allowlist.

An Android release must fail closed when any of these artifacts is missing or
does not match the staged native/rootfs bytes.

The engine-free local-app seed evidence is regenerated and verified from the `local-app-builder`
repository (`scripts/runtime/generate-local-app-sbom.py`, `scripts/tests/test-local-app-supply-chain.sh`).

Historical aggregate source records are preserved byte-for-byte in the SDK
`docs/migration/legacy`; they are not active release evidence. The SDK archive
verifier compares every immutable tar payload entry against the real manifest.
