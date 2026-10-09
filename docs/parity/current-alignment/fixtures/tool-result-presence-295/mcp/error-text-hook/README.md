# Native 2.1.295 MCP error text and hook lifecycle capture

This one-case fixture reuses the existing MCP stdio + loopback Messages capture flow. The local MCP server returns `isError: true`, two nonempty text blocks with surrounding whitespace/newlines, one whitespace-only text block, and an image. The runner saves the original model-facing `tool_result`, the user JSONL row and its `toolUseResult` value, and configures separate `PostToolUse` and `PostToolUseFailure` shell hooks to capture any event input. The Native capture runs only `PostToolUseFailure` for this error result.

Run with the pinned executable:

```sh
CLAUDE_NATIVE_BINARY=/path/to/claude-2.1.295 python3 docs/parity/current-alignment/fixtures/tool-result-presence-295/mcp/error-text-hook/run-native.py
```

The runner binds only to loopback for the dummy model endpoint, starts the MCP server as a local stdio subprocess, uses isolated HOME/config and a dummy API key, and disables retries. It sends two initial model requests: the fixed tool-use response and the fixed final response. Cold resume is disabled in this case. Raw outputs and receipts go under the ignored `run-output/`; curate only the necessary raw MCP/model/JSONL/hook inputs into `native-baseline/2.1.295/mcp/error-text-hook/` after verifying the pinned binary receipt.

The case input records the exact text-block join before outer trim as a hypothesis to compare with Native artifacts; it is not itself Native evidence. The raw hook files determine whether the failure lifecycle runs and whether the hook receives the raw or presentation-trimmed detail. Candidate behavior remains unverified until paired against these captured bytes.
