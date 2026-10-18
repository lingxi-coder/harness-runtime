# Final ledger r376 — client, platform, and build gate audit

Target: Native 2.1.295, SHA-256 `0116ee2e0a513900b633d9951367f18747686478e2b462805b8c31609f047f70`.
Pins: SDK `a363c5b3a237d53eea12c3abfb3b98648ef15d3e`, H `44f2eef5baf8ae16254934dbca451cbfd528d955`, Product `17593eaa66dc762d5c9c581ba348398c914bdbcd`. The original 42-contract baseline and initial pins are unchanged.

## Harness and SDK

The last complete H workspace gate is r271 at `dc6ce121` with SDK `a363c5b3`: 20,471 passed, 0 failed, 7 ignored across 521 test targets and 73 doctests. Receipt: `/private/tmp/h-full-gate-final-r271-20261017/run.json`; the HTTP layout target passed in phase 27. This is the historical full-H anchor, not a full-workspace result for H `44f2eef5`.

H `44f2eef5` is pushed to `main`. The final repair receipt binds the committed source to the r10 patch and records 20 repair phases. Final H runtime passed 1,468/0; formatting and all-targets/all-features Clippy passed. Focused permission, orchestrator, Agent, Tasks, Hooks, and cancellation gates cover affected paths; no current full-H workspace pass is claimed.

SDK `a363c5b3` is unchanged. Its default CI, Rustdoc, focused context-policy tests, and default library graph (620 passed, 0 failed) are reusable for the recorded source. No tokenizer matrix is required.

## Product, platform, and package

Product `17593eaa66dc762d5c9c581ba348398c914bdbcd` passed 2,740 tests, 0 failed, 4 ignored across 40 test targets and 9 doctest targets. Source validation, fmt, workspace check, workspace Clippy, and the 8 MiB CLI regression passed; see `final-evidence-17593/product/runner/run.json`.

All five locked/offline final-pin platform checks passed: Android arm64/x86_64 default AAR/UniFFI, both Android Computer Use variants, and iOS simulator framework. Receipt: `platform-final-17593-v2.json`. These checks do not establish physical-device, AAR-package, or XCFramework runtime behavior.

Public CLI in-flight SIGINT run04 passed all 15 checks in 4.071 seconds, exited 0 without forced cleanup, and started no Native process. Receipt: `cli-sigint-candidate-run-04-17593eaa66-20261018T091547Z-1496/receipt.json`. Raw-byte equality covers only fixed prompt/interruption `message.content` slices; terminal fields and transcript invariants were checked separately.

The fresh P17593/H44f2 Flare package passed static audit, startup smoke, independent audit, and actual packaged GUI input/Submit. Development-signed ZIP SHA-256: `64c8a4d7a441f53c5dc9990d13f3ea4930f1ead72c2df52759565102f73481de`; it is not notarized. Visible CUA typed `packaged-host-cua-value`, appended `xy`, and submitted successfully; cleanup completed. See `receipts/mac-package-final-17593.json` and `mac-package-evidence-17593/`.

## Evidence boundaries and remaining status

The H cancellation fix is guarded to the evidenced in-flight stream case. Native projection for compaction-retargeted message IDs and pre-request interruption remains unverified. Run04 does not compare whole stdout/session bytes or establish broader cancellation parity.

The alias/glob probe is `capture_invalid`: one Native process started, but the positive Edit-deny control failed, so no semantics were established. SESSION-1 has no Native durable-row oracle; TOOL-4 is a helper-only 2.1.293 result; SESSION-5 has no Native scalar result. These remain separate unverified boundaries, not established current source defects.

Finalization passed 20 repair phases and retained the eight-phase Product runner in `repairs-final-17593-v2.json`; current baseline, platform, and gate receipts are saved here. Production, CLI, package, GUI, and platform gates are complete. The 42-row matrix assembly and validator passed; universal parity, full current-H workspace acceptance, and physical-device acceptance are not claimed.
