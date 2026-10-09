# Native 2.1.295 MCP `isError:true` text and hook lifecycle

This is one isolated public CLI capture against Native 2.1.295 SHA `0116ee2e0a513900b633d9951367f18747686478e2b462805b8c31609f047f70`. The MCP server ran as a local stdio process; the fixed model responses came from a loopback-only Messages stub with a dummy key. Native made exactly two initial model requests and one MCP `tools/call`, then exited successfully. Cold resume was disabled.

The JSON-RPC result had `isError:true`, a whitespace-only text block, two nonempty text blocks with leading/trailing whitespace and newlines, and an image. Native routed it to `PostToolUseFailure`; the configured `PostToolUse` hook did not run. The raw failure-hook input is in `post-tool-use-failure-hook-input.raw.json`; there is intentionally no PostToolUse input artifact because Native did not invoke it.

Observed values from the original raw slices:

- Model-facing `tool_result` has `is_error:true` and content `FIRST_LINE \n\n\nSECOND_LINE`, followed by the Native total-token reminder in the request to the next model call. The original raw slice is `model-tool-result-error_text_hook.raw.json`.
- The persisted JSONL tool-result block has the same trimmed content and `is_error:true`; the adjacent `toolUseResult` is the string `Error: FIRST_LINE \n\n\nSECOND_LINE`. The exact row and value are retained separately as `native-user-tool-result-row.raw.jsonl`, `native-user-tool-result-block.raw.json`, and `native-user-toolUseResult.raw.json`.
- The failure hook's `error` field is `FIRST_LINE \n\n\nSECOND_LINE` without the `Error: ` prefix.

The exact input text join, error presentation, and field order remain in the raw artifacts; no normalization was used. Dynamic prompt/session IDs, timestamps, and temporary paths are retained as captured. `receipt.json` records the pinned binary, runner/input hashes, two-request boundary, raw artifact hashes, and lifecycle checks. This is Native oracle evidence only; candidate behavior remains unverified until paired against the fixture.
