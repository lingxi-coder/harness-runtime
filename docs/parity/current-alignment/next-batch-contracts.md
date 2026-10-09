# Next shared-contract batch

Target remains Claude Code **2.1.295**. This is an implementation handoff for
known open contracts, not acceptance evidence. Start after the current Harness
checkpoint and canonical Product pins are recorded. Introduce the shared fields
together so producers and consumers can be compiled as one batch.

## Finish the current JSONL carrier migration

The latest three-request Glob gate passed both complete model tool-result raw
byte comparisons, then failed because zero of three token-reminder attachment
payloads were present in JSONL. `JsonlMessage` now emits recognized source
values from `attachment_projection`, `tool_use_result_projection`, and
`mcp_meta_projection`, while several producers still put those values in
`extra`. Its serializer intentionally excludes the recognized extra keys.

Migrate every current production producer to the corresponding explicit
projection field, including reminder/hook/thinking attachments and tool-result
sidecars. Do not restore a compatibility lookup in the serializer. C is
inventorying the remaining callsites; Root owns the main transcript writer.
Any Computer Use callsite adjustment remains limited to the approved source
carrier adaptation, with execution and validation logic unchanged.

The two-request Glob error gate separately needs the Native model-facing
`<tool_use_error>` wrapper restored for invalid Mod-rewritten output. Its
OutputEvent error payload is bare, and its persisted sidecar has `Error: `;
those different surfaces must not be conflated. Current 2.1.295 counterpart
fixtures are being collected without replacing the historical 2.1.293 bytes.

## MCP error presentation

- Keep a server `isError:true` result on `Err(ToolError::McpFailure)`. It must
  retain MCP failure telemetry and the PostToolUseFailure lifecycle; changing it
  into a successful call with a boolean would change behavior.
- Preserve the exact error detail as UTF-16 source. The fixed Native text
  combination rule must be verified before changing extraction. Model content
  is the bare Native detail, while the persisted `toolUseResult` sidecar is
  `Error: ` followed by that detail.
- Carry a presentation discriminant, `NativeToolResultLayout::{Mapped,
  ErrorMessage}`, on Core, runtime history, and SDK tool-result blocks. The
  explicit MCP failure producers select ErrorMessage. Do not infer it from
  `is_error`: Bash mapped results may explicitly contain false or true.
- Preserve the failure subtype through Agent invocation with
  `ToolInvokerError::McpFailure { detail }`. No new field on every
  `ToolCallResult` is needed.
- Native mapped block order is `tool_use_id,type,content[,is_error]`; Native MCP
  error order is `type,content,is_error,tool_use_id`. The model request may then
  append cache control. Both main and Agent transcript writers must retain the
  right layout. Cold loading must restore presentation from the original raw
  object order, without adding a non-Native JSONL field or treating that order
  as trusted execution authority.
- Other providers preserve their existing semantic encoding. Existing SDK
  final-body UTF-16 normalization already handles the observed lone-surrogate
  replacement; do not add another lossy conversion.

Acceptance includes absent/false/true MCP results, complex error text, failure
hooks, the Error-prefixed sidecar, hot and cold replay, and Bash false/true
controls. Existing `current_alignment_mcp_tool_result_presence` is the first
actual production replay; its known failure must not be normalized away.

## Per-turn effort and API-system rows

- Add durable `Assistant.per_turn_effort: Option<String>`, sourced from the
  resolved effort on the admitted request that produced the successful
  assistant. Retries retain that request value. Failed requests do not stamp
  history; never read current settings after a response to reconstruct it.
- Add typed API-system content/output configuration and a model-only API
  override on the appropriate visible User row. Preserve source content and
  transcript visibility. These are message-body rows, separate from the
  top-level system prompt.
- Project cloned request history using completed-turn effort and the current
  admitted effort. Merge effort into existing API-system reminder content;
  replace the current tail-only synthetic marker instead of duplicating it.
- Carry the admitted response value through buffered/streamed history and
  completed assistant rows into JSONL `perTurnEffort`; restore it on cold
  resume. The token reminder's API-system representation must survive resume
  without converting its visible transcript row into a different user event.

Acceptance reuses the fixed 2.1.295 low-then-high two-process fixture, checking
historical low and current high markers, content/order, and persisted
`perTurnEffort` values. A current-request-only marker does not close this row.

Root decides shared interfaces; E owns their definitions and mechanical
consumer synchronization. A owns SDK encoding and request/response effort;
B owns MCP failure production and invocation consumers. Root coordinates
transcript/resume changes. Product UI work remains independently owned by D.
No compatibility branch, latest-version expansion, push, or main merge is part
of this batch.
