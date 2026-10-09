# Native 2.1.295 repeated MCP cursor probe

This fixture runs one isolated Native public CLI process with a local stdio MCP
server and a loopback-only Anthropic Messages stub. It uses the CLI's
`--tools default` profile and strict `--mcp-config`. The server exposes
`tools/list`, `prompts/list`, and
`resources/list`; each first response returns page 0 and `nextCursor` equal to
`same-cursor`. The next two requests with that cursor return distinct pages
while repeating the same cursor. If Native asks for that cursor a third time,
the fixture returns a fixed JSON-RPC error. This makes any cursor guard, partial
result, or error behavior observable while bounding the server interaction.

The model stub inspects the actual first request's MCP tool catalog, selects
the highest numbered page tool present, asks Native to call it, and then
returns a fixed final answer. Thus the receipt can show whether a tool from a
later page was advertised and actually callable. The prompt/resource pages
have distinct names/URIs as well. The server has no external transport or OS
side effects. The Native binary is pinned by version and SHA in `cases.json`;
each run has an isolated HOME/config, a dummy API key, an empty temporary CWD
outside the repository, and a 45-second process limit.

Run the legacy-fallback control from the Harness repository root:

```sh
CLAUDE_NATIVE_BINARY=/Users/luolingfeng/.local/share/claude/versions/2.1.295 \
python3 docs/parity/current-alignment/fixtures/mcp-pagination-295/run-native.py
```

Run the successful modern discovery comparison with the same fixture inputs:

```sh
CLAUDE_NATIVE_BINARY=/Users/luolingfeng/.local/share/claude/versions/2.1.295 \
python3 docs/parity/current-alignment/fixtures/mcp-pagination-295/run-native.py \
  --discovery-mode modern
```

Full model request bodies and Native config/session caches stay in ignored
`run-output/`. The runner preserves raw MCP JSON-RPC input/output, per-page
requests and responses, model tool-slice, model responses, Native MCP debug
log, stdout/stderr, and a receipt. The captured run and artifact hashes are
curated under `native-baseline/2.1.295/`; do not alter older baselines.

## Observed runs

Pinned Native 2.1.295 SHA-256:
`0116ee2e0a513900b633d9951367f18747686478e2b462805b8c31609f047f70`.
Both public CLI runs exited 0 in under two seconds of measured run time and
returned the fixed final result with two turns.

| Negotiation | Captured protocol | Calls per list | Repeat/error behavior | Tool result |
| --- | --- | --- | --- | --- |
| `fallback` | `server/discover` returns `-32601`; Native sends `initialize` and negotiates `2025-11-25` (`protocolEra: legacy`). | `tools/list`, `prompts/list`, and `resources/list` each make 2 requests: no cursor (page 0), then `cursor: "same-cursor"` (page 1). | Both pages return that cursor. There is no third request and no injected JSON-RPC error. Native's raw debug log says all three list methods stopped because the cursor was already sent in the walk. | The model catalog includes pages 0 and 1; Native calls `page_tool_01`. |
| `modern` | Native accepts a complete `server/discover` result advertising `2026-07-28`; no `initialize` fallback occurs. | Same 2 requests for each list, with distinct page-0/page-1 entries. | Both pages return the same cursor; the client makes no third request and sees no injected error. The raw trace proves the tool-list count; the debug log explicitly records the stop for prompts/resources. | The model catalog includes pages 0 and 1; Native calls `page_tool_01`. |

Both runs return different names/URIs on pages 0 and 1. Because Native stops
before the bounded third-cursor error response, this establishes cursor-cycle
termination and partial page retention, not how Native would present a server
error after continuing pagination.

In both runs, the actual first model request included
`mcp__fixture__page_tool_00` and `mcp__fixture__page_tool_01`. The deterministic
model stub selected the latter; Native issued `tools/call` for `page_tool_01`
with `{ "page": 1 }`, the server returned its fixed result, and the CLI
completed successfully. This proves the distinct second-page tool remained
callable after Native stopped the repeated-cursor walk.

For prompts and resources, the raw trace also shows both distinct pages were
requested and the repeated cursor then stopped the walk. This run did not call
`prompts/get` or `resources/read`, so it proves list request/result handling,
not downstream use of those partial inventories.

Both observed runs use the same fixed page contents, model prompt and model
stub, and the same `--tools default` profile. The fallback run is preserved in
`native-baseline/2.1.295/`; the successful discovery run is separately
preserved under `native-baseline/2.1.295/modern-discovery/`.

In the modern comparison, the Native `server/discover` request advertises
`2026-07-28`. The fixture replies with a complete discovery result containing
`supportedVersions`, tool/prompt/resource capabilities, instructions, server
info, and `resultType: complete`; Native accepts it, uses protocol version
`2026-07-28`, and does not send the fallback `initialize` request. Modern list
requests include `_meta` with the negotiated version and client metadata. List
responses carry the modern `resultType`, `ttlMs`, and `cacheScope` fields.
Native again sends exactly two requests per list and stops after page 1 repeats
the cursor; it makes no third request and sees no injected error. It still
advertises and successfully calls `page_tool_01`. The successful discovery
response follows the current test DTO shape in
`crates/platforms/posix/tests/mcp_protocol_negotiation_e2e_test.rs`; Native's
acceptance and subsequent method sequence are established by the raw receipt,
not by that candidate test.

This probe only establishes the observed 2.1.295 behavior for these fixed
three-list responses under default-tools + successful-modern-discovery and
default-tools + fallback negotiation. It does not establish all MCP pagination
cases, downstream prompt/resource usage, other server errors, or candidate
parity.
