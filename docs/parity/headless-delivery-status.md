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
matrix has not passed. Native-native nondeterminism, initial compatible-follower
batching, official final request checksum bytes, and the remaining
lifecycle/capability matrix still require validation.
Existing accepted product differences remain in force; unknown differences
must fail and must not become new normalization exemptions.

Source receipts, mock tests, compilation and native process comparisons are
separate evidence. Merging this checkpoint does not establish full parity.

## Continuation: native process evidence

The frozen Native version remains 2.1.293. The new runner records the Harness
executable SHA-256 and rejects a changed executable within a run. An invalid
fork identity is reported as a failed comparison without aborting later cases.
No normalization rules or accepted divergences were expanded.

The complete 28-case capture at `/tmp/headless-resume-293-matrix-02` used explicit
local source overlays, not updated committed product dependency pins. All
Native captures completed; 16 of 28 Native repetitions were stable under the
existing rules. Harness completed the expected protocol in 26 cases. Budget
and interrupt failed. The detailed failures are retained in
`scripts/tests/headless-fixtures/native-harness-resume-matrix-2026-10-08.json`.

Subsequent fixes preserve empty tools and the last conversation cache marker,
use unprefixed session UUIDs, retain Agent statistics with an empty tool catalog,
preserve the Sonnet 5.5 canonical identity, and remove the extra requesting
status. Provider identity in metadata is independent of endpoint admission;
fast-request admission still checks the endpoint and credential. The execution
owner now checks the budget after a final response settles. Model stop reason
and driver termination reason are separate owner-published facts.

Focused verification at `/tmp/headless-resume-293-focused-05` establishes:

- `print-json` and `schema-success`: stdout matches after the already registered
  dynamic-token replacements; stderr and exit artifacts match raw bytes.
- `budget-limit`: error subtype and exit code are now correct; aggregate usage
  still differs. This case does not pass byte acceptance.
- Earlier current-source print-text/stdin-text stdout matched raw bytes. The
  request cache/tools differences are fixed; the first remaining print-text
  body difference is the authorized brand text. Full HTTP equality is not claimed.

Channel receipts and executable hashes are retained in
`scripts/tests/headless-fixtures/native-harness-focused-2026-10-08.json`. They
do not establish session-file or whole-case equality.

Local verification in this continuation: 332 selected Headless runtime tests,
88 SDK Anthropic tests, 48 strict-comparator/adapter tests, 12 context-window
tests and the real request-admission regression passed. The CLI build and
minimal and mobile/uniffi runtime configurations passed. Stale test-only UTF-16 API fixtures and
a misplaced request-admission test were repaired to compile the llm-runtime
test target; no obsolete API shim was added.

Remaining concrete failures include cancellation while opening a model stream,
Native assistant block publication and identities, result usage aggregation,
queue/attachment/metadata JSONL emission, compatible follower batching and
the unimplemented advertised capabilities. The runtime's durable cost sidecar
also remains an explicit unmatched artifact; it has not been silently excluded.
Ordinary configuration/plugin/MCP captures remain distinct from bare captures.
