# Native 2.1.295 MCP `isError` presence baseline

One public Native 2.1.295 CLI run calls three local MCP tools sequentially. The local MCP server returns the same JSON-RPC result shape and text content with `isError` omitted, `false`, and `true` respectively; there is no `structuredContent`. The CLI uses a loopback-only model stub, a dummy key, strict isolated MCP config, and a fresh HOME. No upstream requests are made.

The actual model-facing raw `tool_result` slices are byte-preserved: omitted and explicit false both omit `is_error`; true includes `is_error:true`. Key order for omitted/false is `tool_use_id,type,content,cache_control`; true is `type,content,is_error,tool_use_id,cache_control`. Each raw JSON-RPC request/response, Anthropic body/SSE, session JSONL, stdout/stderr, fixture, and capture receipt is in `manifest.json`. The isolated `.claude.json` is not included.
