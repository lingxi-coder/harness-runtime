# Headless integration checkpoint — 2026-10-08

This commit exposes the shared desktop Headless service and moves CLI protocol
ownership into runtime. It includes the exact JSON, control, queue persistence,
credential and provider dependencies needed by that service.

Validation at this checkpoint:

- Headless runtime tests: 335 passed.
- Strict raw-byte comparator tests: 36 passed.
- SDK library tests against its exact staged tree: 406 passed.
- Native baseline remains Claude Code 2.1.293, darwin-arm64; its distribution
  hash and evidence are recorded in `headless-native-baseline.json`.

This is **not completed Native↔Harness byte-level acceptance**. The complete
matrix has not passed. The last recorded Harness print-text comparison predates
the tool-selection/schema fixes and timed out. Native-native nondeterminism,
initial compatible-follower batching, official final request checksum bytes,
and the remaining lifecycle/capability matrix still require validation.
Existing accepted product differences remain in force; unknown differences
must fail and must not become new normalization exemptions.

Source receipts, mock tests, compilation and native process comparisons are
separate evidence. Merging this checkpoint does not establish full parity.
