# Migration gate follow-up — 2026-09-29

This follow-up starts from merged main `a7dce30bf9fcc71617fd4a1acf3cd29e84b98f78`.
The [main CI run](https://github.com/lingxi-coder/harness-runtime/actions/runs/36527040250)
completed with failures in lint, parity fixtures, Windows desktop, supply chain,
and unit tests. Mobile Linux remains fixed at
`224f1fb1fd0f24b5e138c9095e22a6c20b375eb1`; dependency versions, Cargo.lock,
client DTOs, serialized data, and FFI namespaces are unchanged.

## Repairs

- Desktop assembly selects existing Unix or Windows filesystem, process,
  worktree, and LSP implementations through private target-specific imports.
  Unix-only filesystem, process, and credential-store modules are excluded when
  compiling `platform-posix` on Windows. Shared portable services remain shared.
- Vendored libssh2/libgit2 build scripts validate the packaged source tree
  instead of invoking `git submodule update` inside a consumer's checkout.
  Third-party C source, patches, and license bytes are unchanged.
- The live prompt test now normalizes the emitted `OS Version` field. The old
  normalizer's lowercase `version` left `Darwin 24.6.0` in its byte lock. Starting
  from the passing original 14,602-byte prompt and replacing only that field
  produces 14,601 bytes and SHA-256
  `80dba21fd57101276064ae42542b67805db7c7a8f3e42a81cd63a0dba8cdc28a`.
  Production prompt text is unchanged. A focused regression checks Darwin,
  Linux, and Windows OS values.
- Rust 1.94 Clippy findings are addressed with equivalent Option/slice/counting
  operations, derived defaults preserving their variants, cached lazy regexes,
  corrected documentation placement, and removal of an unused provider beta
  constant. Permission, cancellation, event, and scheduling logic are unchanged.

## Focused local verification

- `cargo clippy --locked --offline -p harness-runtime --features desktop --lib -- -D warnings`: passed.
- `cargo check --locked --offline -p harness-runtime --features desktop`: passed.
- `cargo clippy --locked --offline -p sandbox-runtime --lib -- -D warnings`: passed.
- Existing permission tests with `bash-ast`: 1,685 passed.
- Existing MCP enterprise policy tests: 48 passed.
- Existing attempt-pricing tests: 5 passed.
- Existing thinking and signature tests: 24 passed.
- Prompt byte-lock tests, including host normalization: 5 passed.
- Windows GNU `platform-posix` and `platform-windows` checks: passed. Existing
  platform warnings remain; this is a compile check, not a Windows runtime test.
- The repository gate runner passed all nine gates, including plugin/resource
  contracts and source identity checks. The final tree is checked again before
  submission. No local feature/ABI matrix or full workspace test suite was run.

LSP diagnostics are unavailable in this environment; Cargo, strict Clippy, and
rustfmt provide the Rust diagnostics. No dependency upgrade or advisory exception
was added.

## Acceptance still outstanding

The complete Windows GNU desktop check reaches a vendored libssh2 C compilation
failure: `arc4random_buf` is undeclared. Native Windows MSVC CI must establish
desktop compilation separately; neither result proves device/runtime behavior.

[PR #5's first CI run](https://github.com/lingxi-coder/harness-runtime/actions/runs/36651351158)
passes the Linux parity-fixture job. Windows MSVC progresses beyond the original
67 POSIX-package errors but still fails on nine Unix socket/permission errors in
`desktop/pane_teammate.rs`. Windows' existing swarm backend reports unavailable;
a new Windows pane transport is outside this behavior-preserving follow-up.
The first lint run also identified test-target documentation and iterator lints;
those receive a separate diagnostic-only follow-up in this PR.

Supply-chain CI reports advisories against the existing dependency lock. The
all-features unit job also contains a stack overflow, settings/provider fixture
failures, workflow timing failures, and Rust diagnostic snapshot differences.
These are not reported as passing or hidden by skips, snapshot resets, dependency
upgrades, or policy exemptions.

Android/iOS physical-device and final product release acceptance retain the gaps
recorded by the product's three-repository validation record. This focused
follow-up does not claim a new mobile or signed macOS release.
