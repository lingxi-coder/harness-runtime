# Native SubagentStop prompt-hook release probe

This fixture drives the public Claude Code CLI path through one actual
`Agent` tool call with `subagent_type: "general-purpose"`, a fixed child
response, the configured `SubagentStop` prompt hook, and a fixed parent
completion. It runs only against a loopback Messages stub with a dummy key;
no request is relayed upstream.

Run once per explicitly selected Native executable:

```sh
CLAUDE_NATIVE_BINARY=/path/to/claude-2.1.293 python3 docs/parity/current-alignment/fixtures/subagent-stop-prompt/run-native.py
CLAUDE_NATIVE_BINARY=/path/to/claude-2.1.294 python3 docs/parity/current-alignment/fixtures/subagent-stop-prompt/run-native.py
CLAUDE_NATIVE_BINARY=/path/to/claude-2.1.295 python3 docs/parity/current-alignment/fixtures/subagent-stop-prompt/run-native.py
python3 docs/parity/current-alignment/fixtures/subagent-stop-prompt/compare-versions.py
python3 docs/parity/current-alignment/fixtures/subagent-stop-prompt/compare-295.py
```

The runner pins and checks the 2.1.293, 2.1.294, and 2.1.295 binary hashes. It classifies requests
from fixed content markers, the advertised `Agent` tool, a matching Agent
tool-use/tool-result ID, the hook prompt marker, and the serialized
`hook_event_name`. Request sequence is stored for chronology only. A usable
fixture requires exactly one recognized child request, one request whose
message contains `hook_event_name: SubagentStop`, one matching parent
continuation, and the expected final parent response. A hook-shaped request
without the event name is counted separately and does not satisfy the gate.

For each version, the checked-in `native-baseline/` retains the original child
and SubagentStop `system`/`messages` JSON value slices, all four fixed stub SSE
responses, stdout/stderr, and a receipt with request/response hashes and
classification evidence. Complete provider request bodies are generated under
ignored `run-output/` so they can be inspected locally without placing the
large injected project/system context into the portable fixture. The settings
file and cases are fixed checked-in inputs.

All three versions produced a real SubagentStop hook request from the general-
purpose child context (`cc_is_subagent=true` in the original system field),
then completed the parent with the fixed result. The raw hook `system` field is
1,810 bytes for 2.1.293 and 2,283 bytes for both 2.1.294 and 2.1.295. The
allow/block decision rule and untrusted-event-JSON instruction appear in 2.1.294
and remain text-identical in the 2.1.295 captured system field. The raw billing
header is retained, but its suffix varies across captures of the same binary
and is not attributed solely to the release.

The hook `messages` field is 7,154 bytes for 2.1.293/2.1.294 and 7,156 bytes
for 2.1.295. It contains generated `prompt_id` and `agent_id` values plus an
agent transcript path containing that generated agent id. The field also has
runtime git-status and date context that changed between captures. Raw bytes
remain unequal; these dynamic references are recorded, not normalized. The
fixed `{"ok":true}` response verifies the public request/lifecycle boundary,
not model judgment quality. Candidate Harness comparison remains pending.
