# Native 2.1.295 MCP resource-blob MIME probe

This isolated public-CLI fixture returns 16 `type: "resource"` blobs from one
local MCP `tools/call`. Every URI ends in `.not-the-extension`, so the saved
suffix can be attributed to the returned MIME type rather than URI parsing.
The only model endpoint is a loopback stub with a dummy API key; the MCP server
is a local stdio child. The run has no real credentials or upstream traffic.

Run from the Harness repository root with the frozen binary:

```sh
CLAUDE_NATIVE_BINARY=/Users/luolingfeng/.local/share/claude/versions/2.1.295 \
python3 -I docs/parity/current-alignment/fixtures/mcp-mime-295/run-native.py
```

The runner verifies version `2.1.295 (Claude Code)` and SHA-256
`0116ee2e0a513900b633d9951367f18747686478e2b462805b8c31609f047f70`, then
creates an isolated HOME/config/CWD and limits the process to 45 seconds.
Full temporary session/config state remains under ignored `run-output/`; the
curated baseline contains only the raw JSON-RPC/model/tool-result evidence,
the actual saved blobs, and the final CLI output.

## Observed behavior

Native discovered the fixture tool, requested it once, and transformed all 16
embedded resource blobs into saved-file messages in the model-facing
`tool_result`. The final CLI result was successful (`num_turns: 6`). All 16
actual files existed and matched the exact fixture bytes. Their basename
contains a generated timestamp and random tag; these are runtime values, while
the suffix, size, and bytes below are the observed contract.

| MIME type | Native suffix | Actual filename | Bytes |
| --- | --- | --- | ---: |
| `image/x-icon` | `.ico` | `mcp-fixture-blob-1791508973359-mb3b9l.ico` | 67 |
| `image/vnd.microsoft.icon` | `.ico` | `mcp-fixture-blob-1791508973359-ldi3bw.ico` | 67 |
| `text/css` | `.css` | `mcp-fixture-blob-1791508973359-rtz7m4.css` | 48 |
| `text/javascript` | `.js` | `mcp-fixture-blob-1791508973359-jy7dou.js` | 52 |
| `application/javascript` | `.js` | `mcp-fixture-blob-1791508973359-tfgwej.js` | 30 |
| `application/xml` | `.xml` | `mcp-fixture-blob-1791508973359-wr11mw.xml` | 36 |
| `text/xml` | `.xml` | `mcp-fixture-blob-1791508973359-sey9rp.xml` | 36 |
| `application/manifest+json` | `.webmanifest` | `mcp-fixture-blob-1791508973359-ysgkv8.webmanifest` | 51 |
| `font/woff2` | `.woff2` | `mcp-fixture-blob-1791508973359-q3e6cq.woff2` | 16 |
| `application/font-woff2` | `.woff2` | `mcp-fixture-blob-1791508973359-54so1d.woff2` | 16 |
| `font/woff` | `.woff` | `mcp-fixture-blob-1791508973359-hn9345.woff` | 16 |
| `application/font-woff` | `.woff` | `mcp-fixture-blob-1791508973359-roqodj.woff` | 16 |
| `font/ttf` | `.ttf` | `mcp-fixture-blob-1791508973359-z5xikr.ttf` | 16 |
| `font/otf` | `.otf` | `mcp-fixture-blob-1791508973359-4qyht9.otf` | 16 |
| `application/wasm` | `.wasm` | `mcp-fixture-blob-1791508973359-axh7vi.wasm` | 8 |
| `application/x-native-fixture` (unknown control) | `.bin` | `mcp-fixture-blob-1791508973359-pbeowd.bin` | 21 |

After the first tool result, the fixed model stub made real Native `Read` calls
for the CSS, JavaScript, XML, and webmanifest files, using the absolute paths
Native supplied in the tool result. All four Read tool results contained their
unique fixture markers and rendered file text. The preserved Read tool schema and raw
tool-use responses show that the CLI advertised and executed `Read` through its
normal tool path. Read behavior for the binary font/icon/WASM/unknown files was
not probed.

This is a Native 2.1.295 observation for one embedded-resource `tools/call`
shape. It does not cover `resources/read`, all possible MIME values, other
platforms, or Harness parity. The candidate runtime still needs its own
production-chain gate.

## Evidence

`native-baseline/2.1.295/` preserves:

- the complete local JSON-RPC input/output traces, including the exact
  base64-bearing `tools/call` response;
- the Native model-facing `tool_result` as an exact raw JSON source slice, plus
  the exact Read tool definition, fixture tool definition, and four Read result
  slices;
- all 16 files copied byte-for-byte from the actual Native tool-results
  directory, the six fixed model responses, CLI stdout/stderr, and MCP debug
  trace;
- the runner receipt and SHA-256 manifest. The raw-slice offsets are derived
  from the original UTF-8 request bytes, not a reserialized JSON value.

The original isolated execution, including the full request bodies and session
JSONL needed for local reinspection, remains under the ignored `run-output/`
directory named by `native-receipt.json`.
