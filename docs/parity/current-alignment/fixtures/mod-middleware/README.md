# Mods `session.append` middleware gate

This fixture shares `hooks/register.js` between the pinned Native plugin test,
the production worker protocol runner, and the Agent transcript integration
gate. The Native cases use literal IDs `rewrite`, `direct`, `cancel`, and
`throw`; the Rust gate uses four fixed UUIDs mapped to those same modes in the
module. Both paths keep the same message text and middleware implementation.

From `harness-runtime`, rerun the actual worker dispatch seam with:

```sh
LINGXI_MOD_BUN_EXECUTABLE=/path/to/bun node docs/parity/current-alignment/fixtures/mod-middleware/run-harness-worker.mjs
```

The runner requires an explicitly selected Bun executable, verifies it is
named as Bun, and records its `--version` and executable SHA-256. It starts the
checked-in production `crates/hooks/src/mods_worker.mjs` entry over its framed
JSON-lines protocol. The driver uses the shared
`crates/hooks/src/mods_ipc_codec.mjs` for every worker stdin/stdout line; it
decodes frames for semantics and retains the original lines for raw result
assertions. Its narrowly scoped core continuation replies
with `{ message, uuid }` from the event passed to `next`; the middleware itself
is never reimplemented in the runner. Fresh full stdout/stderr and the full
driver trace go to ignored `run-output/harness-worker/`. Full stdout contains
the required absolute module path in prepare frames. The post-codec baseline at
`harness-baseline/worker-codec-v1/` keeps selected framed worker dispatch lines,
core replies, comparisons and receipt; machine-local prepare/load frames are
omitted.

The worker seam agrees with Native for rewrite, deny, and throw. On direct
return, the worker's intermediate dispatch result is `native-direct-return`,
while Native's public `$.session.append` result is the original
`fixture-input`. This is not yet a terminal-row mismatch: Agent's
`mod_append_message_row` caller only accepts a row after its `next` core
continuation records an applied projection, so the direct answer can be
discarded at that outer boundary. `crates/agent/tests/current_alignment_mod_append.rs`
records both the actual `ModHost` worker-stage result/core inputs and the
`AgentTranscriptWriter` terminal in-memory/JSONL rows. Its Cargo command is
owned by the integration build queue:

```sh
cargo test --locked --offline -p agent --test current_alignment_mod_append -- --nocapture
```

Until that test passes on the shared candidate, MOD-1 remains pending. This
gate does not establish explicit module unload behavior or whole Mods parity.
`worker-comparison.json` also records exact extracted result-object byte
slices from the Native raw event strings and Bun worker stdout frames; there is
no parse/re-serialization in that byte assertion. Rewrite, deny, and throw
result objects match byte-for-byte; direct does not. This is only the common
result-object boundary, not a claim that the enclosing Native and worker frames
are byte-identical.

The same JS runner also exercises worker `unload → dispatch → reload → dispatch`
and writes the selected verbatim frames to
`harness-baseline/worker-codec-v1/lifecycle-frames.jsonl`.
After unload, the hook is absent and the core receives the original event;
after reload, the rewrite hook runs again. Native's public `plugin test` run
ends its process after the tests, but the fixture has no public explicit-unload
trigger; this worker lifecycle result therefore remains a Harness-only gate,
not a Native unload parity claim.
