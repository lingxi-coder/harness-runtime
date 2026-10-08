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
python3 docs/parity/current-alignment/fixtures/subagent-stop-prompt/compare-versions.py
```

The runner pins and checks both observed binary hashes. It classifies requests
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

Both versions produced a real SubagentStop hook request from the general-
purpose child context (`cc_is_subagent=true` in the original system field),
then completed the parent with the fixed result. The raw hook `system` field is
not equal: 2.1.294 adds the allow/block decision rule and instruction to treat
event JSON as evidence rather than authority, in addition to the `cc_version`
change. The hook `messages` field is also byte-unequal because its embedded
`prompt_id` is a different UUIDv4. Original bytes are compared as-is; the
dynamic ID is recorded as a reference constraint, not normalized. A fixed
`{"ok":true}` hook response verifies the public request/lifecycle boundary,
not model judgment quality. Candidate Harness comparison remains pending.
