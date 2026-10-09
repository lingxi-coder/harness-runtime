# Bash input repair, fixed Native 2.1.295

The five public CLI cases in `run-native.py` each completed with exit 0,
two loopback model requests, a real `PreToolUse` command hook and the expected
`probe.ok` bytes from the executed Bash command. No real provider is used.
Each case has a new isolated HOME/config/cwd, a dummy API key and an explicitly
allowed Bash tool whose sole supplied command writes a fixed local marker.
Reruns create a fresh temporary run and its `curated/2.1.295/` directory,
leaving checked-in captures unchanged. The manifest retains the hash of the
original capture runner, before this output-directory isolation was added.

```sh
CLAUDE_NATIVE_BINARY=/Users/luolingfeng/.local/share/claude/versions/2.1.295 \
  python3 docs/parity/current-alignment/fixtures/bash-input-295/run-native.py
```

| Input | Actual hook input after input repair and schema parsing |
| --- | --- |
| String `command_description` | Moved to `description`; alias removed |
| Alias plus existing `description` | Existing description retained; alias removed |
| Null alias | Removed without adding description; command still executes |
| String alias containing U+D800 | Exact UTF-16 unit retained in description |
| `timeout_ms: "5000"` plus string alias | Numeric `timeout: 5000`, followed by description; both aliases removed |

`hook-input.raw.json` is the original stdin delivered to the hook.
`tool-input.raw.json` is an exact source slice of its `tool_input` member.
The receipts, original stdout/stderr and offered SSE responses are retained
per case with a SHA manifest. Full captured requests remain at the receipt's
run directory; neither the requests nor output slices were reserialized.

The Native receipts above do not by themselves establish candidate parity.
The candidate `BashTool::coerce_input` now implements the alias repair, and
`test-harness/tests/bash_input_validation.rs` adds a five-case gate through
the real Orchestrator, Bash schema, permission consumer and process boundary.
It compares those permission inputs against the captured raw Hook input
slices. Its process runner and model responses are mocked: it does not claim
the complete provider wire or OS execution matches. This gate is statically
ready and awaiting E's Cargo queue.

Static supporting evidence: the frozen Native binary, SHA
`0116ee2e0a513900b633d9951367f18747686478e2b462805b8c31609f047f70`,
contains the input-repair function near byte 198683060. It records shape
classes `command_description` or `command_description_dropped`, removes the
alias in both cases, and combines them after `timeout_ms` when both apply.
That source evidence also shows invalid `timeout_ms` remains present when
another repair succeeds; this combination is not one of the five runtime
cases and must not be reported as Native-verified.
