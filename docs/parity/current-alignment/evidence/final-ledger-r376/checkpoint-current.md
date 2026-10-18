# r376 source-repair checkpoint — source, CLI, package, and GUI gates passed; 42-row matrix and validation passed

Target is pinned to Native 2.1.295 SHA-256
`0116ee2e0a513900b633d9951367f18747686478e2b462805b8c31609f047f70`.
The immutable `baseline.json` remains the original 42-contract baseline at SDK
`a363c5b3`, Harness `4d6d0d6`, and Product `f71087b4`. None of those historical
receipts certifies the current repair candidate.

Current implementation pins are SDK
`a363c5b3a237d53eea12c3abfb3b98648ef15d3e`, Harness
`44f2eef5baf8ae16254934dbca451cbfd528d955`, and Product
`17593eaa66dc762d5c9c581ba348398c914bdbcd`. Harness 44f2 is verified and
pushed to `main`; its r10 formatting, runtime (1,468 passed, 0 failed), and
Clippy gates passed. These are affected-source gates, not a current full-H
workspace run. Product 17593 is the current pin and includes the public CLI
SIGINT regression, four self-contained fixtures, and the committed ledger
updates. Its full Product runner completed 2,740 passed, 0 failed, and 4
ignored; all five final platform checks passed. The public CLI SIGINT run04
receipt passed all 15 checks in 4.071 seconds, exited 0 without forced cleanup,
and did not start Native. The exact runner is
`/private/tmp/product-full-r342-r376-20261018T084434Z-98045/runner/run.json`,
the platform log is `product-platform-final-run-17593.log`, and the run04
receipt is `cli-sigint-candidate-run-04-17593eaa66-20261018T091547Z-1496/receipt.json`.
The final P17593/H44f2 Flare package receipt passed static audit, startup smoke,
independent audit, and actual packaged GUI input/Submit acceptance. The signed
development package ZIP SHA-256 is
`64c8a4d7a441f53c5dc9990d13f3ea4930f1ead72c2df52759565102f73481de`; it is not
notarized. The CUA run typed `packaged-host-cua-value`, appended `xy`, and
submitted the resulting value successfully; its app and provider credential
cleanup completed. See `receipts/mac-package-final-17593.json` and
`mac-package-evidence-17593/`. Finalization passed all 20 repair phases and
retained the eight-phase Product runner in `repairs-final-17593-v2.json`, with
current-pin platform and gate audits in `platform-final-17593-v2.json` and
`gates-audit-final-17593-v2.json`. The 42-row matrix assembly and validator passed. The
baseline file and its original pins remain unchanged.

## Current source-repair delta

The live stream-json `initialize.toolAliases` carrier repair remains in the
current Harness tree. The exact H paths are:

- `crates/orchestrator/src/conversation/model.rs`
- `crates/runtime/src/headless/run/control.rs` (focused initialize alias
  regression in this file's existing test module)
- `crates/permission/src/policy_gate_test.rs` (permission-gate regressions)
- `crates/core/src/host/permission_gate.rs`
- `crates/runtime/src/desktop/assembly.rs`

The permission/context and Agent/Tasks source paths are tracked exactly in
`validate-ledger.py`. The live initialize regression covers replacement,
omitted-field retention, invalid-map rejection, and exact UTF-16 projection;
the rule-free default setter validates valid and invalid maps without creating
policy state. The current Harness r10 delta also contains a narrowly guarded
stream-cancellation result projection in `crates/runtime/src/headless/run.rs`.
The H r10 runtime, formatting, and Clippy gates passed. This does not establish
Native projection parity when session compaction retargets a preserved message
ID or for interruption before the provider stream is active; both combinations
remain unverified. No blanket current H full-workspace pass is claimed.

Product 8610 is historical: its 2,739 tests passed with zero failures, but its
run04 public CLI SIGINT candidate emitted the wrong terminal result. Preserve
that failure as history; it does not certify Product 17593. Product 17593's
test, platform, fixed-fixture public SIGINT, fresh package, and packaged GUI
gates have passed. The 42-row matrix assembly and validator passed.
The documentation-only ledger update does not change the tested production
pins or require repinning/rebuilding production inputs.

## Native probe boundary

The first isolated alias/glob probe failed while binding its local endpoint and
recorded `native_process_count: 0`; it did not start Native. The approved
endpoint-only retry started one pinned process and sent one local provider
request, but its receipt is `capture_invalid`: the positive Edit-deny control
failed, and all six alias canonical tools remained in the provider catalog.
The valid conclusion is only that this attempt did not establish alias/glob
semantics. `new_native_launches` is read from the two probe receipts and counts
the single started process, including this invalid attempt. Do not call this a
Native behavior pass or a byte comparison.

## Remaining bounded acceptance

- `TURN-3`: the guarded H streaming-cancellation source fix passed the r10
  runtime, fmt, and Clippy gates. Product 17593 run04 passed all 15 public CLI
  checks against the fixed Native 2.1.295 fixture: 4.071 seconds, exit code 0,
  no forced cleanup, and no Native launch. The receipt verifies the accepted
  POST, terminal JSON fields, prompt/interruption raw-byte slices, ordering, and
  absence of an assistant row. The Product 8610 terminal mismatch remains
  historical. Native projection parity when compaction retargets a preserved
  message ID, and for pre-request interruption, remains unverified.
- `SESSION-3` / `CMD-1`: Product's one-process exact unpaired-surrogate Skill
  case is covered by r375, and the public cold `--resume` selector passed on
  Product 8610. These remain evidence for their recorded pins; they do not
  establish a Native cold-resume oracle or complete the final matrix.
- `SESSION-1`: Native 2.1.295 plugin-test captured the raw append event but no
  JSONL rows. There is no Native durable-row oracle; do not infer a persistence
  mismatch or claim durable byte parity.
- `TOOL-4`: the preserved 2.1.293 object-valued tool-result fixture has a
  helper-level 1/1 receipt. It is not a full Host/SDK/CLI cold-resume replay,
  and the old helper receipt is not source-hash evidence for later SDK changes.
  No concrete current source defect is established by the missing broader
  replay alone.
- `SESSION-5`: candidate name/suggestion APIs reject ill-formed UTF-16 through
  `String::from_utf16`, but there is no matching Native public scalar result.
  Keep the behavior explicitly unverified; absence of a Native capture alone
  does not establish a source mismatch.

All items above retain source, regression, Native capture, and byte-comparison
boundaries separately. The single Native alias/glob attempt remains
`capture_invalid`, so it establishes no Native semantic result. Physical-device
execution was not performed; that is an explicit unverified boundary and not a
new mandatory task. Current H/P implementation SHAs are recorded above. Product
17593's full tests, five platform checks, bounded public SIGINT run04, signed
package, and packaged GUI acceptance passed. Final matrix assembly remains
outstanding. Do not claim universal parity or a current full-workspace H pass.

Product `17593eaa66dc762d5c9c581ba348398c914bdbcd` was fast-forward pushed to remote main after final package and GUI acceptance. The pending Harness delivery is documentation only; the tested implementation remains H44.
