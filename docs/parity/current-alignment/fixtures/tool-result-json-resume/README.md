# Native 2.1.293 cold-resume object tool-result projection

This bounded fixture checks the 50,000-UTF-16-unit JSON-string truncation used
when Native cold-resumes an object-valued `tool_result`. Run it with the pinned
Native binary explicitly selected:

```sh
CLAUDE_NATIVE_BINARY=/path/to/claude \
  python3 /path/to/harness-runtime/docs/parity/current-alignment/fixtures/tool-result-json-resume/run-native.py
```

The runner starts a public Native CLI session with an isolated config and a
loopback-only Anthropic stub. It then inserts a fixed assistant `tool_use` row
and matching user `tool_result` row into that session's JSONL, with
`tool_result.content` stored as an object, and starts a second public process
using `--resume`. The inserted row is controlled fixture input; this does not
claim that a Native tool execution emitted an object-valued result. No real
account, credentials, or upstream service are used. Full requests, which
contain Native system context, remain under ignored `run-output/`.

The JSON-string value is exactly 50,007 UTF-16 units. Its first U+1F600 pair
begins at unit 49,999, crossing the 50,000-unit cut, and another pair is in the
omitted tail. Native's resumed provider request turns the object into a string,
keeps the prefix before the pair, and appends
`[6 more characters of this tool result not shown]`. The pair is not split;
the six omitted Unicode code points include both emoji pairs, the two fixed
letters and the JSON closing quote/brace. The post-resume JSONL row still stores
the original value as an object. Raw input and post-resume session rows, the
exact final `tool_result.content` JSON value, stdout/stderr, and receipt are in
`native-baseline/cold-resume-object/`.

This is a Native cold-resume oracle only. Candidate
`tool_result_text::normalized_tool_result_projection` has a focused unit test
for the same code-point-safe rule, but the Cargo test has not run in this
acceptance batch. The 2,000-unit persisted-output preview contract in the
neighboring fixture is a separate path.
