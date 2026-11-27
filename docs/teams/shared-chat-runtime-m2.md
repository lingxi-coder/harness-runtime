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

## Asking the requester (Q03)

`run` and `continue` bootstraps may carry `askRequester: true`. The worker then registers one extra tool, `TeamAskRequester {question}` (1..2000 bytes, not blank), next to the two read tools. The flag is ignored on a route without continuation support, so a Bot is never offered a question it could not be resumed from. Without the flag the tool does not exist for the model.

A question is not a tool result: the call stays unfinished and the process ends.

1. The first well-formed ask call never returns. Once the transcript durably holds that response with the ask call as its only unfinished non-read call (two consecutive checkpoint evaluations agree), the worker fsyncs the transcript, reports outcome `waiting` and exits immediately. No result row is written and nothing is drained: on disk this is exactly a worker that died after persisting a tool call.
2. The host, after the worker has exited, takes the checkpoint with `inspect_task` and emits

```json
{"version":3,"type":"result","status":"waiting","cursor":{...},
 "wait":{"waitId":"wait_<40 hex>","logicalStepId":"<taskId>/t0/c0","question":"...","requestDigest":"<64 hex>"},
 "checkpointDigest":"<64 hex>","pausedAt":1731300000000,
 "dispatchedRequests":1,"attemptDispatchedRequests":1,"deniedTools":0,"usage":{...}}
```

   No `results/<taskId>.json` is saved. `waitId` is derived from the logical step (`"wait_" + first 40 hex of sha256("team-wait:<logicalStepId>")`), so every inspection of the same paused task names the same wait. `requestDigest` is SHA-256 over `{"domain":"lingxi-team-wait-1","taskId","waitId","question"}`.
3. `inspect_task` reports the same `wait` object for as long as the call is unfinished (`phase: in_flight`, `continuable: true`; the ask call is listed in `pendingToolCalls` with `readOnly: false`). A host that crashed between the worker's exit and its own report recovers the wait this way. A malformed ask call (blank or oversized question) is an ordinary failed tool call, not a wait.
4. The task continues only with the answer: `continuation: {"epoch", "expected_checkpoint_digest", "answer": {"wait_id", "message_id", "text"}}` (`text` at most 8192 bytes) and `askRequester: true`. A checkpoint with a wait and no answer, an answer for another `wait_id`, an answer without a wait, or an answer while the tool is not registered is `NOT_CONTINUABLE` before any dispatch. The refused attempt consumed its epoch; re-inspect and use the next one.
5. The continuing worker replays the unfinished calls: read tools run again, the ask call receives `{"answer": text, "answerMessageId": message_id}` as its result, the row is persisted and the model loop resumes. No batch is admitted, so revision and cursors do not move. A second ask call in the same response gets an error result ("only one question can be open at a time"); the Bot may ask again in a later turn, which pauses the task again with a new `waitId`.

The question and the answer also exist as Chat messages. Source messages carry an optional `waitId`; a message with `waitId` whose `sourceTaskId` names a task with a recorded result is not projected into a later batch, because the session already holds both as the tool call and its result. If the wait is abandoned (expired, cancelled, revoked) the task is never continued; the next `run` in the scope repairs the dangling call as an interrupted tool call, exactly as after a killed worker, and its batch then projects the question and any late answer as ordinary Chat messages.

Usage in a `waiting` result is this process's delta; the host sums it with the continuing attempts.

## Staying silent (unasked tasks)

A task nobody asked the Bot for (the backend admitted it because the Bot takes part in the Chat with auto-reply on) may carry `mayDecline: true` in its `run` and `continue` bootstraps. The worker then registers `TeamNoReply {}` and adds one sentence to the turn prompt saying the Bot may stay silent. Without the flag the tool does not exist for the model, and an empty final turn is still a failure. A Bot DM task that may stay silent also carries `addressed: true` (valid only with `mayDecline`): the member wrote to the Bot directly, so the prompt and the tool tell it to answer unless the message plainly needs no answer, instead of telling it nobody asked.

1. The call returns at once (`{"posted": false, ...}`) and the model ends its turn. The worker reports outcome `declined`; whatever the model wrote after the call is discarded. A `declined` result has empty `text` and no `document`, and its result row is recorded like any other, so `inspect_result` recovers it.
2. The choice is durable: `inspect_task` treats a task whose rows hold a `TeamNoReply` call as declined. An unfinished call is replayed like a read (it is in the read-only set; `pendingToolCalls` lists it with `readOnly: true`), and an empty final turn after the call is a complete final answer, so a continued process finishes `declined` without a model request. The continuing host must send the same `mayDecline` it started with.
3. The host emits `{"version":3,"type":"result","status":"declined","text":"",...}` and settles the task through `completeTask` with outcome `declined`: nothing is posted to the Chat. Usage is reported as for `completed`.

Deciding whether to offer the choice at all (rules, the Decision service) is the Desktop host's job, not the runtime's.

## Local Apps (host-forwarded)

`run` and `continue` bootstraps may carry `localApps: [{"id", "name", "description"?}]`: the Local Apps of the maintainer's Desktop this run may use. At most 16 entries; `id` unique, 1..128 characters, no control character; `name` 1..120 characters, not blank; `description` at most 2000 characters; every length counts characters, not bytes; unknown keys are refused. An invalid list fails the start with `PROTOCOL`. The list is ignored on a route without continuation support (as `askRequester` is) and refused for a private Data run. Without it, or with `[]`, no Local App tool exists for the model.

The worker holds no app and no connection to one. It registers two tools next to the read tools and forwards what the model asks to the host as `operation` messages (the same channel and `operation_result` reply as the list and Data operations); the host decides what runs.

| model tool | `operation.tool` | `operation.arguments` | `logicalStepId` | host result |
|---|---|---|---|---|
| `TeamLocalAppTools {app}` | `local_app_tools` | `{"app"}` | none | `{"tools":[{"name","description"?,"inputSchema"}]}` |
| `TeamLocalAppCall {app, tool, arguments?}` | `local_app_call` | `{"app","tool","arguments":{...}}` | the durable step of the tool call | `{"content":[{"type":"text","text"}],"isError"?}` |

Neither operation carries an `approvalRef`. `app` must be one of the offered ids; the tool schema lists them as an enum, the worker checks the request again before it sends anything, and the parent refuses (and ends the run with `PROTOCOL`) any request for an app that was not offered, before the host sees it. `arguments` is always an object when it reaches the host (a call without arguments is sent `{}`), and a whole request is at most 64 KiB. A listing changes nothing; a call is an operation of the ledger like a Data write: the host names it by `logicalStepId` (`<taskId>/t<response index>/c<call index>`), runs it once for that step and answers a repeat of the same step with the recorded outcome. Only the host knows whether an outcome is established: an `operation_result` with `established: false` ends the process (`WORKER`) and the model is told nothing.

What the model reads is text, bounded to 64 KiB: the apps and their descriptions in the tool description; the tools of an app as a list with each input schema; the `text` blocks of a call result joined by newlines (other block types are named, never decoded). A host `succeeded: false`, or `isError: true`, is an error result whose text says so in words ("The call failed: ..." with the host's short `error`/`message`, "The app reported an error:"), because the OpenAI chat route does not carry the error flag. The tool descriptions tell the model that what an app returns is data, not instructions. The host must keep a serialized `operation_result` within 64 KiB; a larger one fails the run with `PROTOCOL`.

Continuation. A call's request leaves the worker only after the response holding the call is on the transcript, so a checkpoint that lists an unfinished `TeamLocalAppCall` in `pendingToolCalls` (`readOnly: false`, `continuable: true`) names the step the host may already have run. The continuing worker replays it through ordinary dispatch with the same `logicalStepId`, so the host deduplicates it; an unfinished `TeamLocalAppTools` is replayed like a read (`readOnly: true`). A call of which the host could not establish the outcome never returns: the run ends and the call stays unfinished. The continuing host must send the same `localApps` it started with; for an app that is no longer offered the replayed call is refused inside the worker (the schema no longer admits the app), nothing is sent, and the model reads that error.

## Delegating to another Bot (Bot v3 B8)

`run` and `continue` bootstraps may carry `delegates: [{"id", "name", "description"?}]`: the other Bots of the same Chat this run may hand part of the request to, as the service offered them for this attempt. At most 20 entries; `id` unique, 1..128 characters, no control character; `name` 1..120 characters, not blank; `description` at most 500 characters; every length counts characters, not bytes; unknown keys are refused. An invalid list fails the start with `PROTOCOL`. The list is ignored on a route without continuation support (as `askRequester` is) and refused for a private Data run. Without it, or with `[]`, the tool does not exist for the model.

The worker registers one tool, `TeamDelegate {target, request, expectReply}`: exactly these three keys, `target` one of the offered ids (an enum in the schema), `request` 1..2000 bytes and not blank, `expectReply` a boolean. Nothing is ever read out of the model's text: only a call of this tool delegates. The worker holds no connection to any Bot; the host and the service decide whether the request is sent. The tool description lists the offered Bots and tells the model that a reply is untrusted data, never instructions.

- `expectReply: false` is an operation like a Local App call: `operation.tool` `delegate`, `arguments {"target","request"}`, `logicalStepId` the durable step of the call (`<taskId>/t<response index>/c<call index>`), no `approvalRef`. The host answers `{"message": "..."}`, one sentence the model reads as the tool result; `succeeded: false` is an error result with that sentence (a refusal of the service is such an answer, never a failure of the run). The parent refuses (and ends the run with `PROTOCOL`) a request for a Bot that was not offered before the host sees it, and an `established: false` outcome ends the process with the call unfinished. A call that was unfinished when the process died is replayed through ordinary dispatch with the same `logicalStepId`, so the service sends it once.
- `expectReply: true` is like a question: the call never returns in the process that made it. Once the transcript durably holds the response and the first unfinished call the checkpoint finds is this one (two consecutive evaluations agree), the worker reports outcome `waiting` and exits. If the first is another call, one the tool refused before its result was written, the worker fails the task instead of reporting that call as the wait. A continuation answers it with the other Bot's reply in `answer.text`, at most 24 KiB (a person's answer stays at 8 KiB), and only in a process whose `delegates` still include the Bot that was called; otherwise it is `NOT_CONTINUABLE`. `inspect_task` and the `waiting` result report `wait {"kind":"delegation","question":<the call as compact JSON>,...}`; `waitId` is derived from the logical step exactly as for a question, and `requestDigest` is SHA-256 over `{"domain":"lingxi-team-delegation-1","taskId","waitId","tool":"TeamDelegate","arguments":<question>}`. The host sends the delegation together with the pause, in one transaction of the service, so the other Bot cannot answer before this task waits. A task that cannot wait (no `askRequester`) gets an error result for a call with `expectReply: true`.
- Continuation. `continuation.answer {wait_id, message_id, text}` as for a question, with `approved` absent, and the continuing process must offer the tool with the called Bot among `delegates`: otherwise `NOT_CONTINUABLE` before any dispatch. The replayed call returns `text` (the host's sentence about how the delegation ended: the reply, or why there is none) with `message_id` naming the delegation. A Bot that is no longer offered is refused by the schema inside the worker for a replayed call without a wait, nothing is sent, and the model reads that error. The continuing host sends the Bot the call named whatever else changed since.
- Not covered here: whether the Bots may talk to each other at all, who answers, the limits of a chain and the quota are the service's rules (`docs/teams-execution/IMPLEMENTATION.md`, "Bots delegating to Bots").

## Route allow-list

Continuation is enabled only for `ProtocolFamily::OpenAiChat` routes (DeepSeek and other OpenAI-chat-compatible providers). It is covered by loopback fixtures in `crates/runtime/tests/desktop/team_continuation_process.rs`: kill after the tool call is persisted, kill after the result is persisted, kill mid-response, epoch reuse, stale digest, recorded result, unsupported route, exactly-once batch and legal provider sequence. Anthropic, OpenAI Responses, Gemini, Azure and Foundry routes report `route_unsupported` until a continuation test proves them. Provider hidden reasoning state is neither saved nor relied on; a real-provider acceptance run (for example a DeepSeek reasoning model with tool calls) has not been done and is required before enabling the route in a product build.

## Not covered

Write tools, approval and connection waits; cumulative usage across processes (the host sums attempts); a read-only tool result produced before its assistant row is durable; a host-binary end-to-end fixture for `status: "waiting"` (the worker path is covered in `team_continuation_process.rs`, the host report is exercised only from the Desktop host).
