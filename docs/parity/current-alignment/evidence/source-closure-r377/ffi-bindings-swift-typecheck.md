# r377 generated Swift bindings typecheck

## Inputs

- Full-wrapper baseline: `acceptance/source-closure-r377/ffi-bindings-base` (the existing saved wrapper generation).
- Current client generation: `acceptance/source-closure-r377/ffi-bindings-client`.
  - `client.swift`: `f0fd24a69b948359d3bdfff349b907b4271e7edab12c6fdf7efbac013fc7991f`
  - `clientFFI.h`: `3464fec33ce10eaf27fc2771a507f262eb6fd6e1017c447298ce4dc51e775bc4`
  - `clientFFI.modulemap`: `db599f2fe692e05be20062e2e047d9998c98f7b6b2392f8355a1a0402589f38f`
- Build-script source: `lingxi-app/apps/ios/native/scripts/build-xcframework.sh`, SHA-256 `b86c2002cda8ed4280dc1bb8f30eecf797a184c41b86424afe462e34ae13af86`.

## Procedure and result

Copied `ffi-bindings-base` to `ffi-bindings-current`, then replaced `client.swift`, `clientFFI.h`, and `clientFFI.modulemap` with the three files from `ffi-bindings-client`. The three copies were byte-compared before postprocessing.

Executed the exact script range 196–433 with `GEN_DIR=acceptance/source-closure-r377/ffi-bindings-current`. The range's only external script dependency is the earlier `log` helper; the run supplied its equivalent output-only helper. This range merged the per-namespace modulemaps, performed the Swift scaffold and async-future dedup passes, and inserted the two idempotent iOS callback-init calls. It completed with exit 0 and reported three Swift files.

Final postprocessed source hashes:

- `client.swift`: `322df2ab325f0d4f53e5a58e6904a3622ff3a2ca47e0577bd060b6ca52c0c2fc`
- `harness_runtime.swift`: `bb88fc0eb28c9d91efc464ebdab1277605a9b766c571c701de8ba468fd3592c4`
- `LingxiCodeBindings.swift`: `adee8462467b1a57a76901b11476649ca4203776e242b76ae49c5327c77aa662`
- merged `module.modulemap`: `4468c15f676b54a5bd2d78ce0fe5bb6657469e947ade735236676bddde00e5dc`

Typecheck command:

```sh
swiftc -typecheck \
  -I acceptance/source-closure-r377/ffi-bindings-current \
  -module-cache-path /private/tmp/r377-swift-module-cache \
  acceptance/source-closure-r377/ffi-bindings-current/*.swift
```

Swift 6.2.4 (arm64 macOS 15.0) exited 0 in 2.8 seconds with no diagnostics. The module cache contains PCM files for `clientFFI`, `harness_runtimeFFI`, and `lingxi_code_bindings_FFI`.

## Limits

This typechecks the three generated Swift binding files together after the repository's binding postprocessing. The client namespace is current; the other wrapper files come from the saved `ffi-bindings-base` generation. This is not a fresh full-wrapper bindgen run, full Lingxi app compile, Rust archive link, XCFramework/package build, or device/UI validation. No Cargo command or Native process was run.
