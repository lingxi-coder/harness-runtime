# `session.append` Native and Harness evidence

The Native public plugin-test case for a system message with an isolated UTF-16
unit is in `native-plugin/append.test.ts`. It calls `$.session.append` with
`message.type = "system"`, `door = "notice"`, `origin.kind = "engine"`, and a
fixed content string containing U+D800. The registered middleware rewrites the
content through `next`; the test asserts the returned system content retains
the exact lone unit.

Re-run only against the pinned Native executable:

```sh
CLAUDE_NATIVE_BINARY=/path/to/claude python3 native-plugin/run-native.py
```

The runner verifies Native 2.1.293's SHA, uses an isolated HOME/config and
temporary working directory, and writes the exact plugin-test stdout/stderr,
the raw system-case result event, and a provenance receipt under
`native-plugin/native-baseline/`. The result event is captured as emitted; it is
not parsed and re-serialized. The test itself makes no model request. Plugin
test process exit is only the case boundary; the public API exposes no explicit
module unload trigger here, so unload behavior remains unverified.

The existing `harness-byte-gate/` remains a separate scoped user-message
`session.append` result-byte comparison. The system case also has a direct
production-worker runner that does not require Cargo:

```sh
CLAUDE_NATIVE_BINARY=/path/to/claude \
LINGXI_MOD_BUN_EXECUTABLE=/path/to/bun \
node run-system-bun-worker.mjs
```

The checked-in `harness-baseline/system-bun-worker-ipc-v1/` receipt and raw
result/`next` frames are the current post-codec replay of Native 2.1.293's fixed
system event under Bun 1.3.11. The runner uses the shared
`crates/hooks/src/mods_ipc_codec.mjs` for every worker stdin/stdout line and
retains the original encoded worker frames. The earlier
`harness-baseline/system-bun-worker/` capture remains available separately.
The worker result and core continuation both preserve the Native result's
U+D800 code unit at the same index. This starts the actual `mods_worker.mjs`
entry and does not reimplement the middleware.

`crates/agent/tests/current_alignment_mod_append.rs` now adds that system case
to the actual Bun `ModHost` and `AgentTranscriptWriter` acceptance path. It
loads the same checked-in module, derives the exact input/result units from the
raw Native event, verifies the worker continuation result, then persists a
`ConversationMessage::System` and checks its raw JSONL code units. The writer
case shares the fixed UUID with Native; Host-only fields still differ. The Rust
integration test awaits E's Cargo queue. These worker/retained-row gates do not
cover the full Agent turn/model request.

## Native cold-resume projection

`native-resume/run-native.py` probes the public Native CLI twice with a fixed
session ID and an isolated loopback model stub. The first process's public
`session.append` hook writes a fixed assistant string containing U+D800, Omega,
and U+DC00. The second process uses `--resume`; its `turn.complete` hook reads
both public `$.session.messages()` and `$.session.messages({as: 'api'})`.

For Native 2.1.293 (`4e21122a227857da1178aca3299700c1fd7f2b77c93f12e73c2c76db796a105e`),
the persisted assistant JSONL row retains the two lone units. The resumed
provider request and both public message views contain U+FFFD at those two
positions. Raw selected row, raw request text member, exact debug-hook lines,
stdout captures, and the scoped receipt are in
`native-resume/native-baseline/cold-resume-projection/`. Full model request
bodies remain in ignored `run-output/` because they contain Native context.

Re-run from the repository root with an empty fixed CWD outside this workspace:

```sh
CLAUDE_NATIVE_BINARY=/path/to/claude \
CLAUDE_RESUME_FIXTURE_CWD=/private/tmp/empty-resume-probe \
python3 docs/parity/current-alignment/fixtures/session-append/native-resume/run-native.py
```

This is Native-only evidence. It means cold-resumed `session.messages()` must
project the lone units to U+FFFD, while the raw persisted JSONL row retains the
source units. It does not establish candidate parity until the production
Harness resume/API path is checked by E's integration gate.
