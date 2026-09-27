# Extraction validation

The runtime code imported from LingXi `01dcc428ba0c12522f6ee29b6474f3a4e0891c47` is consumed at `36dd3c1295ae13bde59f6131705dbf86a244ae52`. The original standalone repository parent is `da6f2b9027f6c1e6b66c946544e10c7591be82d5`; its history is retained. CI follow-ups `0fe594a610bc46e7db11a19bdcaa9f39bee61680` and `36436caf643efcaa5b3fd58b81f37ac30beb86ba` do not change runtime code. Source file mappings and original hashes are in [source-manifest.json](source-manifest.json).

## Independence and resources

- All workspace targets compile with all features; minimal, core, desktop, mobile, mobile+uniffi and android-computer-use also compile separately.
- A fresh remote clone builds with filesystem reads to both original checkouts denied. Seven existing resume, cancellation and permission tests, the 78-crate dependency gate, resource supply-chain tests and rootfs tooling tests pass under the same restriction. Negative read probes confirm the restriction is active.
- 2,110 plugin/template/skill/vendor resource files match their source bytes. Ten previously ignored production policy files are now explicitly included, with bytes preserved.
- The downstream host resolves all 79 runtime/vendor identities through one full Git revision, with no path overrides or cache selection fallback.

## Host integration

LingXi migration commit `0102773438de56fad21d1ce7d88fab18e96f82d2` passes its complete retained workspace tests: 2,780 passed, 0 failed, 4 ignored. Swift/Kotlin public interfaces were compared, and every generated Kotlin FFI declaration matches the new JNI exports. Android play/direct both build for arm64-v8a and x86_64, including mksh and toybox.

The signed Flare macOS package passes path remapping, signing, static checks and application smoke through `npm run package:mac:flare -- --launch`. The local runtime checkout is physically absent throughout this package run, then restored. All 4,684 files in the pinned Cargo checkout retain their SHA-256 hashes. The package was built from the host commit above; ZIP SHA-256 is `63893a8dfd5934d66150a913bef5a5481a5b1c9ff9a286ac89d1933d101fdd52`.

The iOS arm64 simulator XCFramework passes generated Swift typecheck, FFI initialization and linking. Full local iOS packaging validation is recorded in the downstream delivery report.

## Actual CI

[Native validation run](https://github.com/lingxi-coder/harness-runtime/actions/runs/36291111935) passes six feature profiles, four Android feature/ABI combinations, Linux/macOS/Windows core packages, Linux/macOS desktop and both iOS targets. Windows desktop still fails on its existing unconditional POSIX dependency.

[Real Linux seccomp helper test](https://github.com/lingxi-coder/harness-runtime/actions/runs/36291747832/job/108543030207) builds and resolves the helper from a custom target directory, then verifies AF_UNIX socket/socketpair denial, AF_INET allowance, NO_NEW_PRIVS and child exit-code propagation on the Linux kernel.

## Failures retained without baseline resets

Full local upstream tests report 16,870 passed, 27 failed and 8 ignored; the agent test process also aborts with a stack overflow. Of 13 failed targets, 12 reproduce on the original source archive. The one migration-introduced DesktopConfig doctest failure was fixed and retested. A separate pause timing failure passed five isolated baseline and five migrated reruns and is not claimed fixed.

Linux CI reports 17 failed targets: completed test summaries total 16,845 passed, 48 failed and 8 ignored, plus the unfinished agent process. Not every Linux failure has been independently reproduced on the original full Linux workspace. The additional failures include scheduling-sensitive workflow tests, missing bwrap/socat, process timing, model/mock fixtures and Rust diagnostic snapshots. Bounded inspection found no additional migration-specific error. The actual plugin-directory consistency test passes.

The Linux prompt byte-lock failure reproduces identically in original and migrated tests when only uname output changes: the existing test normalizes `OS version:`, while production emits `OS Version:`. No template bytes or snapshots were changed.

Other visible failures remain: original Windows ConPTY and Clippy issues; historical branding/skill-description gates; and cargo-deny license/advisory findings with unchanged dependency locks. See [brand-baseline.md](brand-baseline.md) for the narrowly mapped branding inventory.

Android complete APK acceptance is blocked by missing existing PRoot/PTY/policy native libraries. After the existing pinned submodule initialization, both variant native rebuilds reach an inherited iSH bridge source-hash mismatch. Original and migrated pin entries match, and the actual files match the original OpenMinis Git blobs; pins were not rewritten. Mobile physical-device smoke remains unverified. A passing compile or binding test does not represent complete release/device acceptance.

## Rollback

Revert LingXi's migration commit to restore its original source/dependency layout; no user-data migration is required. The upstream snapshot and its original repository history remain available. Existing unrelated host document deletions were excluded from the migration commit.
