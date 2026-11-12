# Shared Bot M2 / Q01: in-flight task continuation

Private transport stays at major 3. A Team task whose worker process died mid-run can be continued by a new isolated worker in the same Bot×Chat session. Continuation never re-admits the input batch, never repeats a persisted tool result, always presents a provider-legal message sequence, and never falls back to re-running the task from the start. Identity and authorization are unchanged from `run`: no owner or App identity is inherited and every dispatch/read still goes through the host broker.

## What is persisted (no new persistence engine)

- The canonical Bot×Chat session journal: admitted batches and both cursors.
- The canonical session transcript, through the existing durable append-once writer: the task prompt (pinned message id derived from the task id), per-block assistant rows including tool calls, tool-result rows, one `team_bot_dispatch` receipt per physical provider dispatch (written under the writer lease immediately before the dispatch is allowed, so it over-counts rather than under-counts), and the existing terminal `team_bot_result` receipt.
- Host-side attempt marks: `home/team-tasks/<sha256(taskId)>/process-started-<epoch>`, create-new, fsynced before a worker is spawned. They carry only task id, batch id and epoch, never progress. They replace the M1 per-invocation `process-started` tombstone, which is deleted.

The assistant response with its tool calls is durable before any tool result is, and every tool result is durable before the next request is built (existing streaming settlement writes rows in event order under the writer). Dispatch receipts are the only Team-specific addition. A read-only Team tool may begin during response streaming, before that response's rows are durable; if the worker dies in that window the response is discarded and re-requested and the read-only tool may run again (authorization is rechecked on every read).

## `inspect_task` (host bootstrap, read-only)

Taken under the session writer lease; mounts nothing, appends nothing and makes no model call. It returns the checkpoint:

```json
{"version":3,"type":"result","status":"task_checkpoint","taskId":"...","phase":"in_flight",
 "cursor":{"sessionRevision":1,"lastMessageSeq":2,"lastChangeSeq":2},"attempts":1,"dispatchesObserved":1,
 "pendingToolCalls":[{"logicalStepId":"<taskId>/t0/c0","toolName":"TeamChatHistory","readOnly":true}],
 "continuable":true,"checkpointDigest":"<64 hex>"}
```

`phase`: `absent` (batch not admitted), `admitted` (batch committed, no model activity), `in_flight`, `result_recorded`. `logicalStepId` is `<taskId>/t<response index>/c<call index>`, stable across processes and independent of the provider's call id. `checkpointDigest` is SHA-256 over scope, task id, batch id, session revision, both cursors, the bootstrap cursor and the identity (row count, byte length, SHA-256) of the whole committed transcript, so any journal or transcript append changes it. Epoch tombstones are deliberately not covered. A failed or aborted continuation attempt still mounts the session and may append rows: always re-inspect before launching the next epoch.

`continuable` and `reason`: `route_unsupported`, `not_admitted`, `transcript_inconsistent` (orphaned or duplicate results, an older response with unanswered calls, a tool-use response whose call rows were never written, a cut-off answer, an API-error row, a foreign batch after this one), `pending_write_tool` (any unfinished call that is not `TeamChatHistory` or `TeamKnowledge`). `result_recorded` is continuable (the result is returned unchanged). `inspect_result` is kept unchanged; the recorded result is also what `continue` returns.

## `continue` (run type)

Same bootstrap as `run` plus `continuation: {"epoch": <u32 >= 2>, "expected_checkpoint_digest": "<hex>"}`.

1. If a terminal result row exists it is returned with no epoch consumed, no worker spawned and zero model calls.
2. The route must be on the allow-list, otherwise `ROUTE_UNSUPPORTED` before anything is written.
3. The epoch must exceed every recorded epoch and be unused (`process-started-<epoch>`, create-new); otherwise `TASK_INTERRUPTED` (`AlreadyAttempted`). Epoch 1 is the first `run`. A refused launch that got as far as the tombstone still consumed its epoch.
4. The worker authorizes through the broker, mounts the session, then recomputes the checkpoint under the lease from the captured pre-mount transcript and the hydrated journal. Digest mismatch is `CHECKPOINT_MISMATCH`, an unsupported or not continuable state is `NOT_CONTINUABLE`/`ROUTE_UNSUPPORTED`, all before any provider dispatch and before projection repair.
5. No batch admission: revision and cursors are untouched. Persisted tool results are in the restored history and are reused. Each unfinished read-only Team tool call is re-executed once under current authorization through the ordinary tool dispatch path and its result row is persisted before the next request. Anything else makes the task not continuable; nothing is auto-resent.
6. The model loop then resumes on the durable history without a new prompt (`phase: admitted` with no persisted prompt admits the prompt exactly once through the pinned id). A partial response that was never durably persisted is discarded and its request is issued again; that one repeat is a counted dispatch. If the newest durable response already is the complete final answer, no model call is made and only the result row is written.
7. Limits (`max_turns`, `physical_dispatches`, deadline) come from the bootstrap, as for `run`; the host passes what remains.

`dispatchedRequests` in the result is cumulative for the task (receipts of all epochs); `attemptDispatchedRequests` is this process only and is what the host checks against the bootstrap's `physical_dispatches`. Usage is this process's cost-ledger delta and is never `complete` for a continued task, because earlier processes' usage is not attributable here.

New failure codes (host `type:"error"`): `CHECKPOINT_MISMATCH`, `NOT_CONTINUABLE`, `ROUTE_UNSUPPORTED`. `TASK_INTERRUPTED` also covers a reused epoch.

## Route allow-list

Continuation is enabled only for `ProtocolFamily::OpenAiChat` routes (DeepSeek and other OpenAI-chat-compatible providers). It is covered by loopback fixtures in `crates/runtime/tests/desktop/team_continuation_process.rs`: kill after the tool call is persisted, kill after the result is persisted, kill mid-response, epoch reuse, stale digest, recorded result, unsupported route, exactly-once batch and legal provider sequence. Anthropic, OpenAI Responses, Gemini, Azure and Foundry routes report `route_unsupported` until a continuation test proves them. Provider hidden reasoning state is neither saved nor relied on; a real-provider acceptance run (for example a DeepSeek reasoning model with tool calls) has not been done and is required before enabling the route in a product build.

## Not covered

Write tools, waiting/approval, member answers and Team permits (Q02/Q03); cumulative usage across processes; a read-only tool result produced before its assistant row is durable.
