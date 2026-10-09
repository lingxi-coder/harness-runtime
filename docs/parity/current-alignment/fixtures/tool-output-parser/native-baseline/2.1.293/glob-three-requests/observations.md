# Native 2.1.293 two-Glob-call request trace

This is a separate three-request case; the original two-request Glob baselines are unchanged. It uses a fixed dummy key, isolated HOME/config, `ANTHROPIC_MAX_RETRIES=0`, a loopback-only Messages stub, and a 45-second process limit. No upstream service is contacted. Native binary SHA-256 is `4e21122a227857da1178aca3299700c1fd7f2b77c93f12e73c2c76db796a105e`.

The stub returns `Glob` tool use on requests 01 and 02 and a fixed final text on request 03. Fixed usage is 5/1, 7/2, and 11/3 input/output tokens. The raw request hashes and response hashes are in `native-receipt.json`; all three original request bodies and SSE responses are retained here byte-for-byte. The session UUID and tool-use IDs are fixture inputs. Native-generated row UUIDs, prompt IDs, and timestamps remain observed dynamic values; they are not normalized into byte-equivalence claims.

Request 03 has three `<total_tokens>` occurrences, located by source byte offsets in the raw request (also recorded in the receipt):

- `messages[0].content[2].text`: initial context reminder, `15000000`, bytes 1919–1968.
- `messages[2].content[0].content`: first/previous tool result reminder, `14999994`, bytes 14048–14097.
- `messages[4].content[0].content`: second/current tool result reminder, `14999991`, bytes 14415–14464.

Thus the first tool result's reminder remains inline in request 03 alongside the second tool result's current reminder. The JSONL rows preserve both forms: `message.content[0]` is a typed `tool_result` attachment with its `tool_use_id`, while the top-level `toolUseResult` is a bare result object with no `tool_use_id`. The two row files and exact bare-value slices are retained separately; the full original JSONL rows are also present.

Re-run with:

```sh
CLAUDE_NATIVE_BINARY=/path/to/claude \
python3 docs/parity/current-alignment/fixtures/tool-output-parser/run-native-three-request.py
```

The runner writes a fresh disposable trace to ignored `run-output/glob-three-requests/`. It verifies the pinned binary SHA/version, checks for exactly three accepted loopback requests, two durable tool-result rows, both typed tool IDs in request 03, and token-reminder observations. This is Native-only evidence for this fixed flow, not a Harness acceptance claim.


## Durable token-reminder attachment rows

The complete isolated Native session transcript contains three distinct top-level JSONL rows whose `type` is `attachment` and whose nested `attachment.type` is `total_tokens_reminder`. Their raw rows are retained in `token-reminder-attachments.raw.jsonl`. The nested attachment keys are exactly `type,text` for all three values (`15000000`, `14999994`, `14999991`), and none contains `tool_use_id`. These rows are separate from the user-message `type:tool_result` blocks: those carry the tool-use reference, while the corresponding top-level bare `toolUseResult` result object does not.
