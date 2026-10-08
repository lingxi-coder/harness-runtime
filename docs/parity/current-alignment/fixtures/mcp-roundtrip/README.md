# Native MCP structured-content round trip

This fixture exercises one complete public Native 2.1.293 CLI chain:

1. Native starts the configured local stdio MCP server, performs `initialize`
   and `tools/list`, and advertises `mcp__fixture__emit_payload` to the model.
2. A loopback-only Anthropic stub returns one fixed `tool_use` response.
3. The stdio server returns two text `content` blocks plus a
   `structuredContent` object whose `label` contains an isolated U+D800.
4. Native sends the resulting `tool_result` in its second model request and
   persists the relevant assistant/user JSONL rows before returning the fixed
   final text.

The server is launched with a generated `--mcp-config`; its executable is the
Python interpreter running the fixture, and it has no network transport. The
Native process uses a dummy API key, loopback base URL, isolated HOME/config,
project-only settings, strict MCP configuration, a fixed session ID, and an
empty working directory outside this workspace. Full Anthropic request bodies
and the full session JSONL stay in ignored `run-output/`; they contain Native
system/context and dynamic metadata. The checked-in baseline keeps raw stdio
frames, the exact tool-call response, the first request's tools slice, the
selected model `tool_result` block, the relevant raw JSONL rows, and Native
stdout/stderr.

Re-run from the Harness repository root:

```sh
CLAUDE_NATIVE_BINARY=/path/to/claude \
CLAUDE_MCP_FIXTURE_CWD=/private/tmp/empty-mcp-roundtrip \
python3 docs/parity/current-alignment/fixtures/mcp-roundtrip/run-native.py
```

For Native 2.1.293 (`4e21122a227857da1178aca3299700c1fd7f2b77c93f12e73c2c76db796a105e`),
the raw `tools/call` JSON-RPC response contains both text blocks and
`structuredContent.label` with U+D800. The model-facing `tool_result.content`
is a JSON-text rendering of `structuredContent`; it does not contain either
original `content[]` text block. Its inner JSON string spells the lone unit as
the six ASCII characters `\ud800`; parsing that inner JSON recovers U+D800.
Native appends a `<system-reminder>` with a run-dependent token counter to the
tool-result text. The receipt keeps those exact one-run bytes and records that
counter as dynamic; do not normalize it away or call the entire model body a
byte match.

Native also sends `server/discover` before standard MCP `initialize`; this
fixture returns JSON-RPC `-32601` for that optional probe. Native then performs
`initialize`, `tools/list`, and `tools/call` successfully. That captured
fallback is fixture-specific and does not establish the semantics of every
Native discovery extension.

The paired Harness candidate test is
`crates/tools/mcp/tests/current_alignment_native_mcp_result.rs`. It replays the
checked-in raw Native `tools/call` response through the production `McpClient`
and `MCPTool::call`, then compares the fixed model-content UTF-16 units and
structured-content source units. E has not run it yet; it is a static test
asset, not a pass receipt or a full Orchestrator conversation replay. Its
queued command is:

```sh
cargo test --locked -p tool-mcp --test current_alignment_native_mcp_result
```
