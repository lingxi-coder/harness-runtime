# Native 2.1.295 refusal stop-details baseline

This fixture supplies the frozen Native 2.1.295 baseline for
`crates/orchestrator/tests/current_alignment_refusal.rs`. The two fixed SSE
inputs and their UTF-16 explanation units are copied unchanged from the
2.1.293 source cases in `../stop-details/cases.json`; the acceptance capture
README documents the isolated 2.1.295 replay and its pinned binary SHA.

`native-baseline/<case>/` contains only the captured response bytes, stdout,
stderr, extracted stop-details rows, and receipt needed to replay and audit
these cases. The temporary HOME, complete session JSONL, and other run state
are intentionally not included. `SHA256SUMS` pins every checked-in case and
capture artifact to the captured bytes.

The test parses the original stdout bytes through `Utf16JsonProjection` and
compares the exact `/result` UTF-16 units. It does not reserialize or compare
the entire stdout JSON, whose unrelated fields can vary. It separately checks
the fixed response/session identity, model/stream request shape, terminal
refusal marker, and persisted explanation units. Native 2.1.295 captured three
stop-details rows per case; the Harness assertion checks the explanation and
category semantics on its own transcript rows.

The 2.1.295 acceptance comparison records the Native result and response bytes
equal to the 2.1.293 case, with the same three-row explanation persistence.
This is a Native baseline only and does not itself establish Harness parity.
