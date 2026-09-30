# Remaining acceptance follow-up — 2026-09-29

This follow-up starts from main `e2074703d612e6481b339d2cd6ffbbbe0e859387`.
Historical validation files remain records of their original revisions.

## Repairs

- The pinned llm-client main revision `0c6a907d897a54e656700c00cf335d91b10dca0b`
  is published; its 276 all-features library tests pass. CI checkout now succeeds.
- Unix pane transport is compiled only on Unix. Windows auto swarm selection
  uses its existing in-process backend; explicit Unix pane selections report
  unavailable. Vendored Ed25519 uses libssh2's platform RNG. Full Windows GNU
  desktop compilation and the final native MSVC CI job pass.
- CI checks the current `engine` feature, installs real Linux sandbox test
  tools, and provides a 16 MiB stack for debug libtest async state machines.
- Updated usage/model fixtures use SDK cumulative usage and fixed pricing.
  Audio advertisement follows available host capabilities. Workflow tests wait
  for terminal state with a bounded deadline. Rust 1.94 diagnostic text changes
  preserve their error messages and locations.
- QuickJS 0.14 hooks bound recursion and retain their synchronous global
  surface. All 498 hook tests pass. Foreground cancellation completes recovery
  when a dead supervisor's child is already gone.
- Dependency upgrades remove the known advisory set. RSA certificate generation
  keeps RSA2048 through AWS-LC. Modern rustls reads PEM without rustls-pemfile.
  OpenTelemetry/Prometheus, XML consumers, document parsing and UniFFI move to
  maintained dependency versions. No advisory ignore remains.
- Syntect's maintained local cache codec uses CBOR. Bundled grammar caches and
  private lazy contexts are converted from checksum-verified upstream assets.
  Exact upstream theme data and highlighting spans for every built-in grammar
  pass regression comparison. See the vendored HARNESS-PATCHES.md.
- UniFFI runtime and metadata are pinned to 0.32.2. Its actual hard buffer is
  32768 bytes; the original conservative 12288-byte per-type budget remains.
  All six metadata-budget tests pass. Native generators must match 0.32.2 and
  bindings must be regenerated before product/device acceptance.

## Verified locally

- All-feature, all-target workspace compilation: passed.
- Standalone SDK smoke uses the current `scripts/checks` resource-contract entrypoint;
  authorization, evidence and supply-chain gates pass with the pinned SDK.
- Nine repository gates: passed.
- Windows GNU complete desktop compilation: passed (existing platform warnings).
- Strict cargo audit: zero vulnerabilities, zero warnings.
- cargo deny: zero advisory errors/warnings and zero license/source errors;
  configured duplicate/unused-license/source warnings remain visible.
- Strict workspace/all-target Clippy (`-D warnings`): passed.
- Full workspace/all-feature tests: 17301 passed, 0 failed, 7 existing ignored
  tests across 536 summaries, including nested supervisor helper invocations.
  Subsequent Linux CI corrections pass 7 MCP reload, 8 Mobile Linux sandbox
  and 1 process-tree focused tests.
- [Final CI f155e54](https://github.com/lingxi-coder/harness-runtime/actions/runs/36676810833):
  26 of 27 jobs pass, including strict lint, complete Linux unit tests, native
  Windows MSVC/Linux/macOS desktop, both iOS targets, four Android combinations,
  feature profiles, parity, repository gates and Linux seccomp.
  Vulnerability/license checks pass. Only cargo-vet source review fails.

## Evidence still required

cargo-vet reports 608 dependencies without safe-to-deploy source review.
The audit store has no exemptions or fabricated reviews. This remains a hard
source-review failure; the clean vulnerability scan does not close it.

Product follow-up is committed on main as `68a5153b990b0dde04a2eb1cbb40689e5d578ecd`.
See its [acceptance record](https://github.com/lingxi-coder/lingxi-app/blob/68a5153b990b0dde04a2eb1cbb40689e5d578ecd/docs/architecture/three-repo-structure-validation.md):
2756 Rust tests, 1284 Electron tests, Android Direct 998/Play 985 unit tests,
all product gates, complete Android APK/native-byte checks, three iOS framework
architectures, iOS device/simulator application compilation and three focused
native simulator roundtrips pass. The signed macOS package passes static checks;
its smoke guard refuses to launch while the user's existing app is running.
Closing that app requires the pending approval. Physical Android is not connected;
iPhone/Android install-and-launch authorization is also pending. No physical-device
execution is claimed for this dependency and FFI upgrade.


## Windows SDK public API follow-up

The SDK's existing handle-based directory enumeration and file-ID reopen functions
were private to their implementation module. Publish the missing exports through
scoped SDK commit `13ffbec5665cd586e1d6a97928cb9987393645c2`, based on the previous
`224f1fb1` integration revision and merged into SDK main. The patch adds public API
documentation and an external consumer test that enumerates, reopens and deletes
an actual Windows file by its identity. CLI's `forbid(unsafe_code)` remains intact.

All five manifest SDK entries use this one canonical Git revision. Cargo.lock
changes only SDK source identities; existing registry choices are retained.
SDK Windows GNU consumer compilation and strict Clippy pass. Full-feature runtime
compilation and all nine repository gates pass after the pin update. Final downstream
native evidence above belongs to its explicitly recorded earlier pin; the new
locked product graph is being validated separately.
