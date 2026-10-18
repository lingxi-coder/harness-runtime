# Final 2.1.295 evidence ledger rules

Target remains Native 2.1.295 SHA256 `0116ee2e0a513900b633d9951367f18747686478e2b462805b8c31609f047f70`. No target-version updates or bulk captures are in scope. The bounded Native alias/glob attempt remains `capture_invalid`; count its started process from its receipt, but do not treat it as semantic evidence. The immutable initial 42-contract baseline remains SDK `a363c5b3a237d53eea12c3abfb3b98648ef15d3e`, Harness `4d6d0d6c428e6624a303f5e2a5c96eb4a392f1a1`, and Product `f71087b428665c30d0bd752e16f76cb53ab7fef6`; `baseline.json` is not being rewritten.

Current implementation pins are SDK `a363c5b3a237d53eea12c3abfb3b98648ef15d3e`, Harness `44f2eef5baf8ae16254934dbca451cbfd528d955`, and Product `17593eaa66dc762d5c9c581ba348398c914bdbcd`. Harness 44f2 is pushed to `main`; its r10 formatting, runtime (1,468 passed, 0 failed), and Clippy gates passed, without claiming a full-workspace run. Its guarded streaming-cancellation projection is scoped to the evidenced path. Native parity remains unverified for compaction that retargets a preserved message ID and for pre-request interruption.

Product 17593's full runner completed 2,740 passed, 0 failed, and 4 ignored across 40 test targets and 9 doctest targets at `/private/tmp/product-full-r342-r376-20261018T084434Z-98045/runner/run.json`; all five final platform checks passed (`product-platform-final-run-17593.log`, exit 0). The public CLI SIGINT run04 receipt passed all 15 checks in 4.071 seconds, with exit code 0, no forced cleanup, and no Native launch (`cli-sigint-candidate-run-04-17593eaa66-20261018T091547Z-1496/receipt.json`). Product 8610 remains historical: its 2,739 tests passed with zero failures, but its run04 public CLI SIGINT candidate emitted the wrong terminal result; do not reuse that outcome for Product 17593.

The current P17593/H44f2 macOS Flare package passed static audit, startup smoke, independent audit, and actual packaged GUI input/Submit acceptance. The development-signed ZIP SHA-256 is `64c8a4d7a441f53c5dc9990d13f3ea4930f1ead72c2df52759565102f73481de`; notarization was not performed. The actual GUI appended `xy` to `packaged-host-cua-value` and submitted successfully. The final package receipt is `receipts/mac-package-final-17593.json`, with archived file hashes in `mac-package-evidence-17593/`. Finalization passed 20 repair phases and recorded the eight-phase Product runner in `repairs-final-17593-v2.json`; the final-pin five-target receipt is `platform-final-17593-v2.json`. The receipt inputs now pass; 42-row matrix assembly and validator passed.

The active bounded table has 42 actual contract IDs. A historical r35 count of43 included the domain row named MCP as well as MCP-1; do not manufacture a new contract or drop MCP-1.

Evidence classes are independent:
- Raw byte comparison: fixed original bytes actually compared, no normalization.
- Source differential: executed extracted Native code or source-proven branch correspondence; not a public Native capture.
- Regression: actual current production path exercised; mock/unit-only boundaries explicit.
- Constraint: observed nonfixed identity/time/order/lifecycle invariants; no whole-artifact byte claim.
- Allowed difference / out of scope: exact user decision, no parity-pass claim.
- Unverified: missing direct evidence stays visible. A stale label alone is not an actual current source gap.

Reusing a historical execution requires an identified test source, relevant production source/fixture scope and unchanged-source or explicitly covered-delta evidence. A later full Product gate does not execute dependency crate tests. Doc-only ledger commits do not alter tested production source; bind implementation SHA and final documentation SHA separately.

Only Root runs necessary Cargo checks on the shared queue. Workers audit disjoint domains and write evidence files only. No production modifications until a reproducible source/behavior gap is identified. Preserve existing Native artifacts, unrelated dirty canonical worktrees and accepted brand/multi-provider/Fusion/platform differences. No old-version compatibility layer.
