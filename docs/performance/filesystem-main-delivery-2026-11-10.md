# Filesystem main delivery — 2026-11-10

This publication extracts the filesystem alignment, optimization, SDK compatibility and gap fixes onto current main (`cf79bbd542f36b84f9d9b760ca264119f1d7ff1d`). It preserves main's newer compaction, MCP authorization, opened-file identity, refusal and EndConversation behavior. The private dirty-baseline snapshots are not publication ancestors.

The SDK is the published main revision `54cf6b7b96e36c16593e43e170d80767099fbe7d`: current main's UTF-16 streaming optimization plus selective scalar user-content projection. Runtime and consumer lockfiles use immutable Git sources, without local source overrides. Scalar provenance now travels through main requests, stream retries, scheduled requests, transcript persistence, cold resume and background forks.

The delivered baseline repairs include Darwin's zombie-only process-group denial handling. Only positive evidence of no live members can complete a denied stop; live or uninspectable groups retain an error. This is required by supervised system-ripgrep execution and foreground cancellation.

## Validation on the integrated source

The final scoped matrix passed all 14 steps: 773 successful test executions, zero failures and two intentional ignores. The published SDK also passed 38 request-policy tests.

| Check | Passed | Failed |
|---|---:|---:|
| filesystem-all-features | 548 | 0 |
| kill-tree-regression | 1 | 0 |
| posix-cleanup | 95 | 0 |
| instruction-announcements | 11 | 0 |
| read-count | 8 | 0 |
| scalar-wire | 3 | 0 |
| provider-count | 20 | 0 |
| host-read-room | 4 | 0 |
| scalar-replay | 4 | 0 |
| file-replay | 29 | 0 |
| compaction-restore | 30 | 0 |
| bounded-compaction | 4 | 0 |
| read-fences | 3 | 0 |
| teammate-cwd | 13 | 0 |

Rust and Cargo inputs stayed stable during the final run. Four already passing steps were carried after corrections limited to SDK test imports and empty test-caller provenance arguments; their original receipts and source hashes are retained.

The broad nine-package attempt ran 4,128 passing tests, 15 failing tests and five ignored tests. Eight failures exposed the omitted Darwin repair; the repaired process suite, grandchild regression and all-feature filesystem suite are rerun separately. Seven MCP failures remain in unchanged current-main paths: three in `mcp_input_required_http_test` and four in `mcp_protocol_negotiation_e2e_test`. The three HTTP failures reproduce with one test thread. All 105 inspected MCP implementation and fixture paths match the main baseline. This delivery does not claim a green broad suite.

The [frozen validation report](filesystem-gaps-validation-2026-11-10.md) remains historical evidence for its explicitly identified snapshot and SDK revision. The publication checks use the clean integrated source and the published SDK above. Final consumer Git pins, build identity, CLI flows, main parity and shared-checkout preservation are recorded under `/Users/luolingfeng/lingxi/artifacts/filesystem-delivery-20261110`.

## Preservation and acceptance

Publication uses isolated clean worktrees. Shared checkout files, staged state, branches and HEADs are excluded from mutation; concurrent edits are retained and recorded separately. No private snapshot, broad working-tree commit, stash, reset, cleanup or force push is used.

The acceptance boundary remains macOS source tests and disposable loopback CLI fixtures. Signed Desktop/native keychain, packaged UI, Windows runtime behavior, physical devices, external provider tokenizers and production soak are not established by these checks.
