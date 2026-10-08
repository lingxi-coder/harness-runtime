# Native CLI in-flight SIGINT fixture

`run-native.py` starts Native 2.1.293 with a fixed session ID and prompt, an
isolated HOME/config, a dummy API key, and a loopback Anthropic Messages stub.
The stub accepts one request and deliberately sends no response. The runner
sends SIGINT only after that request is received, then captures the Native
result and the persisted session.

Run from the Harness repository root with an empty, fixed working directory
outside this workspace:

```sh
CLAUDE_NATIVE_BINARY=/path/to/claude \
CLAUDE_CANCEL_FIXTURE_CWD=/private/tmp/empty-cancel-probe \
python3 docs/parity/current-alignment/fixtures/cli-cancel/run-native.py
```

The pinned 2.1.293 run exited with process status 0 while emitting a JSON
`result` with `is_error: true`, `terminal_reason: "aborted_streaming"`, and
`subtype: "error_during_execution"`. It sent exactly one non-streaming POST,
received no stub response, and did not require forced process cleanup. The
persisted session contains the fixed prompt followed by a user row whose text
is exactly `[Request interrupted by user]`; no assistant row was persisted.
The output reports `num_turns: 2`.

The small `native-baseline/sigint-in-flight/` directory contains raw stdout,
stderr, and the exact JSON `message.content` fields extracted from the two
session user rows, plus their receipt. The complete session JSONL remains in
ignored `run-output/`; its 33 KB snapshot includes Native-generated UUIDs,
timestamps and working-directory metadata. The receipt records its hash but
does not treat that file's full bytes as a reproducible equality target. Compare
the fixed result fields, prompt/interrupt code units, row order, and references
between generated IDs. Do not strip fields and call the full session a byte
pass.

This is a process-level CLI observation with `ANTHROPIC_MAX_RETRIES=0`. It does
not prove TUI signal wiring, in-turn cancellation-token equivalence, tool side
effect rollback, default retry policy, or whole-runtime parity. For the
candidate, the bounded Rust gate should hold a production orchestrator request
behind a deterministic mock transport, trigger its production cancellation
path, and assert the interruption text/order and terminal reason. Product CLI
signal-to-cancel wiring and its JSON sink remain a separate acceptance seam.
