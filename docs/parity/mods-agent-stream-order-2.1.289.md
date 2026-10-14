# Claude Code 2.1.289 Agent stream and next-query history

This note pins the Native order from a completed assistant tool block through
the next model request. The executable source probe and its captured JSON are
[`mods_agent_stream_order_289_probe.mjs`](../../scripts/tests/mods_agent_stream_order_289_probe.mjs)
and [`mods-agent-stream-order-2.1.289.json`](mods-agent-stream-order-2.1.289.json).
Run it with:

```sh
node scripts/tests/mods_agent_stream_order_289_probe.mjs \
  /tmp/harness-claude-code-2.1.289-darwin-arm64
```

The binary SHA-256 is
`03d66745e3bb69ec727d66023696f3820bc0a00a8a5ba725eb6706d0c67cbe69`. The
probe hashes each selected byte window and executes only the pure typed
integer parser helpers in a VM. It does not execute the Native query runner or
provider path; those conclusions below are source tracing. The separate
[bounded Agent list/spawn acceptance](mods-native-agent-acceptance-2.1.289.md)
does execute the pinned CLI with a local-only stub.

## Row and append ordering

At `content_block_stop`, Native constructs and yields a completed assistant
row for that block, including a fresh outer UUID, timestamp, and API block
index. The producer is `[189368685,189370128)` with SHA-256
`aa8ce11178f40647830a76867eab1458645b378e24fbe5f6092a9f9042e4df50`.
This yield precedes later `message_delta` and `message_stop` events.

The query consumer first awaits `r.appended(M)` for assistant rows containing
tool calls. It then yields its display row. When the generator resumes, it
selects `X = eo ?? M`, pushes `X` into its local `K` history unless the row was
already inserted by the refusal-continuation stitch branch, and enqueues the
tool blocks from `X` using that row. The consumer window is
`[195466181,195469377)`, SHA-256
`380eb29894b93878ef7fc2d3b75c57ffc73b9d331a838520dff9b949e656570c`.

The session append bridge does not substitute its hook projection into the
query stream. `Ui` calls and awaits `Xq(b,[v],{fills:true})`, but `appended`
has no return value; `through` also awaits the append side effect and yields
the original event. Its source is `[195330691,195331847)`, SHA-256
`e4f8db2c16eefdd434b5a3cbf728bf54d228f83abc75806932c411df6e43e3e4`. The
`Xq` hook gate is `[191180248,191180513)`, SHA-256
`962d2adb18e375911f4fd513a0d71f3d79550fda81a6792b82f7863a1bddc732`; the
core append dispatcher does return its accepted row (`m ?? s`) at
`[212347111,212348000)`, SHA-256
`135dea13c52fc00a732fa842002f689186bfc67b265c4321cb52eeac7f6fc156`, but
the streaming query caller does not assign that return value to `M`, `X`, or
`K`.

For an ordinary tool-use response, `eo` is undefined and `X` is the Native
response row `M`. A refusal-continuation stitch is the explicit exception:
when eligible, Native creates merged row `We` and sets `eo = We`, so `X` is
that merged row. The locally yielded `Me` can also differ from `M` for
display/input normalization; the history/tool-enqueue path shown above uses
`X = eo ?? M`, not the append hook's returned projection.

## JSONL recording during the stream

The awaited `session.append` side effect is not the JSONL writer. After that
query row reaches the headless turn consumer, the assistant branch pushes it
into the current turn transcript and calls `TS.record()` immediately when
`persistSession` is enabled. That call is not awaited. The caller is
`[206326212,206326987)`, SHA-256
`134049f5b797bdfe0badd558aa74b9e8cae82ba2a69295c85010f22d3270f6f2`; the
incremental recorder is `[206296696,206298650)`, SHA-256
`531bf0855c64095b0b966bfb226989c211cbd5e4eb37d3b225a132574fbc0428`. Its
default `record()` uses the full current message length. The separate
`ANt` cutoff only excludes an assistant row with `stop_reason:null` when
`record(true)` is used. So Native starts transcript recording for the
completed block before `message_delta`; it does not defer the record call to
terminal metadata.

Tool results emitted by the in-loop `Tn` poll arrive as user rows. The main
query wraps `Xl` with `Ui.through` (the observer path also wraps it with
`Ui.through`), so each yielded row passes through `Ui.R`. `R` accepts rows
whose type is `user`, `assistant`, `attachment`, or `system`, and calls `Xq`
when the running conversation is hookable and `session.append` is enabled.
Thus a tool-result user row does invoke the `session.append` hook before it is
yielded to the headless consumer. The explicit `r.appended(M)` call in `Xl` is
a separate assistant-tool-use path; `Ui` uses a `WeakSet` to avoid invoking
the same row twice. The row-type gate is `[195330200,195330691)`, SHA-256
`24ba3da2d11657f5126b59c8a5362e6e5022e0230907f7dac22b7ca4754fd823`; the
main query wrapper is `[195423499,195425275)`, SHA-256
`b351439ed10addcc0c52a9a30e82e2db44fa3e092edbc9f6a080f93b5d69a975`.

`Ui.through` awaits the append side effect and then yields the original event
value; it does not substitute the hook's accepted row into the query stream.
The headless user branch then pushes that yielded row into the current
transcript and awaits `TS.record()` before it continues consuming events.
Therefore a completed tool result may be persisted while the provider stream
is still open, and the transcript writer receives the original Native
tool-result event row rather than the append-hook projection.

The record path is asynchronous. `TS.record()` calls `NE`, which awaits
session-message reads before passing rows to `insertMessageChain`. That method
spreads each source row into an outer transcript envelope and calls
`appendEntry`. The deduplicating writer enqueues the entry to a per-file FIFO;
the write-queue drain serializes the entry with `LM` and appends the resulting
chunk. The queue's scheduled flush interval is 100 ms. Relevant pins are in
the probe JSON for `ANt`, `NE`, `insertMessageChain`, `appendEntry`, queue
admission, and queue drain.

There is an important snapshot boundary: Native's headless row unwrap and
transcript envelope are shallow spreads, so the nested `message` object stays
shared with the row that the stream parser later updates. The parser mutates
all completed block rows on `message_delta`. The recorder cursor was advanced
when the earlier `record()` call selected the row, so the later terminal
`record()` call does not select that row a second time. However, JSONL queue
serialization occurs later, after asynchronous reads and queue admission. The
source proves an early record attempt, but it cannot prove whether the final
line has `stop_reason:null` or the terminal value: that depends on whether
`message_delta` mutates the shared object before `LM(entry)` serializes it.
Separate record calls also await reads before queue admission, so source alone
does not establish their global admission order when assistant recording is
unawaited and a tool-result record is awaited.

This headless recorder is not the only Native entry path. The interactive
transcript UI uses `MSe.write` and `ASe`, a separate recorder that calls the
same `NE`/session JSONL writer. `MSe.write` passes `isResponseInFlight` to
`ANt`; while the response is in flight, `ANt` stops before an assistant row
whose `stop_reason` is null, excluding that incomplete tail from the selected
slice. The class window is `[209701325,209704237)`, SHA-256
`2c7b630b663b9d7c8839931dc5d2d7f264f45e155d891e65ececec285408d7ca`; its
React recorder/subscription is `[209704237,209704933)`, SHA-256
`1c73559cad8006fad39aca770df29db1981cbb393d773212c152d092def4f2b2`. This
pins the interactive in-flight cutoff and the shared writer, not the exact
effect scheduling point after a later terminal update.

If a later Native tombstone retracts a recorded row, the headless tombstone
branch calls `TS.evict(message.uuid)`. For a row already past the recorder
cursor, that requests `removeMessageByUuid` from the transcript writer. This
explains how a discarded assistant row can be removed after its early record
attempt; it does not imply that every error path retracts its earlier rows.

## Terminal metadata and live tool settlement

The response parser keeps `wp` for completed assistant rows. It resets that
array for each stream attempt at `[189360098,189360254)`, SHA-256
`48f132ffe226519d54e1fe6d2b12daf8f6fabe157c603f5c26e4cdbf8dcae9b0`. Each
`content_block_stop` pushes its completed row into `wp` before yielding it;
the row producer window above proves the same `wd` is both stored and yielded.
When `message_delta` arrives, the parser updates the accumulated usage and
stop reason, then loops over every row in `wp`, writing `message.usage`,
`message.stop_reason`, and the applicable `stop_details`,
`context_management`, and `resumable` fields. This update is response-wide,
not limited to the last completed block. The exact row loop is
`[189371401,189371840)`, SHA-256
`03a6b386623e88e67ffa67250960e20624d4f80cd91efe36193835b2f6a299c1`.
This parser window establishes object reuse and mutation inside the response
producer. The headless JSONL path is traced separately below; it does not prove
which version of the shared nested message the asynchronous writer eventually
serializes.

The streaming executor starts a tool from the assistant row handler, and
`Xl` polls completed results after processing each stream event. `Tn` reads
only `getCompletedResults()` and yields each available result; it does not
wait for unfinished calls or apply their `newContext` value. If a result is
complete at an in-loop poll before the stream iterator exits, it can be emitted
and added to this round's result rows while the query is still consuming the
provider stream; the source permits this but does not guarantee every tool
finishes before the final poll. The `Tn` source is `[195439370,195439671)`,
SHA-256 `7d3b57630b7ff7561262bcd9672e9e34d101a9011058b9454bc97b75d2b9d1d8`;
the per-event query consumer call is included in
`[195466181,195469377)`.

Tool context layers first update the executor's own `toolUseContext` when a
non-concurrency-safe tool completes, which can affect later tools in that
executor. The relevant `executeTool` source is
`[195354117,195355928)`, SHA-256
`ab6baabfd849f979f60a49d4ff78957f58d819da25513c83ed9d8ca6c8089e80`. The
query-local `he` is updated from result `newContext` only in the subsequent
`getRemainingResults()` drain, which runs after the API stream loop. That
drain completes before the `PostToolBatch` hook gate; source
`[195485961,195489200)` has SHA-256
`7fcac4553bbb01791b1e0da0e5005fdc6c061a051391aa00de6bc79d42995a47`.

This separates three timings: tool execution starts on block stop; completed
tool-result rows can be surfaced during in-loop polls; query-local context
from `newContext` is merged after the stream's final drain and before
`PostToolBatch`.

## Next model request

After tool execution and the remaining turn work, the next-turn branch builds
`H.messages = [...w, ...K, ...je]` (`w`: prior request history, `K`: Native
assistant rows from this query round, `je`: user/tool results). The source
window is `[195494144,195495020)`, SHA-256
`e9c6e49c60a6ec63600f75d76c4d1740b96535a0661e6d0cefa91d1e39bf38a8`.
At the next query-round entry, Native derives working history with
`w = [..._i(at)]` from `H.messages`, then merges newly arrived rows. That
window is `[195431190,195432057)`, SHA-256
`d2e569c74e02132f10467367df29052a00d78dbcc16b2bd1d14311612902c359`.
The provider request uses the prepared `Zt.messages` value, except for the
Native `bare` prompt transformation; the call site is
`[195449754,195450504)`, SHA-256
`5761508c90937c49e30ba530b8c23bfd090ea17e7f917b10359dd6704cb71ba4`.

Therefore, an append-hook projection being persisted or returned by the
append dispatcher does not by itself change the next request's history. The
query's own `K` rows are carried into `H.messages`. They still pass through
Native's ordinary next-round history and prompt preparation, so this finding
does not claim byte-for-byte identity at the eventual provider wire boundary.

The Agent child route calls `N5` with `MC` as its stream factory at
`[194849961,194850600)`, SHA-256
`953e0221437e2f90475e88e4f2157ee1440938fa6637df73a13062fbbbf5eff6`; `N5`
consumes the yielded rows in `[194760296,194770594)`, SHA-256
`c3fbc7491ba06e1c1b4297004f310679fcf8df1eecb622159bcff42160783021`.
The W1 dispatch window is recorded in the probe JSON. Together these establish
the Agent stream consumer path without treating session storage projection
as model-history replacement.

## Final provider-stream error after tools have started

For a final thrown stream error with no separate fallback-chain target,
`Xl` rethrows the error to its model-error finalizer. The attempt-disposition
branch is `[195470508,195472449)`, SHA-256
`8be9c1b0f8a16e81007a48a86a89d5ac1e9717083bf17461f0b4bc51fbb66bce`;
when an actual fallback-chain failure is present, that is a distinct
`abandonAttempt` path.

The ordinary model-error finalizer first drains completed results via
`Tn(false)`, then `pn` takes any results that completed in the meantime,
discards the executor and rebuilds it. These completed non-deferred results
are emitted and recorded as user/tool-result rows. The executor discard
aborts executing tools and does not start queued tools. It does not wait for
in-flight work to settle; tool effects that already began cannot be rolled
back. For still-unpaired tool uses, `Wl` yields a synthetic error tool result
with `is_error: true` and text saying the call was cancelled while warning
that already-started effects may have happened. The query then yields a model
error assistant row and returns `model_error`.

The relevant source anchors are recorded in the probe JSON:

| Source | Byte range | SHA-256 | Meaning |
|---|---:|---|---|
| `Xl` thrown-request disposition | `[195470508,195472449)` | `8be9c1b0f8a16e81007a48a86a89d5ac1e9717083bf17461f0b4bc51fbb66bce` | Distinguishes fallback-chain retry from rethrow to the model-error finalizer. |
| `Tn` and model-error `wr` | `[195439370,195440609)` | `d082ae78e6737bc5c45d5adda2d515c8406231b5f3d8e63d23f0ac0041ce3ce7` | Drains completed results, finalizes the executor, emits missing tool results, then surfaces the model error. |
| `pn` cleanup | `[195414914,195415236)` | `6e60a0aec6abb094ef5f3f236b8e28ff285147929b7077e68a44a8a9d112f1b3` | Takes finished rows, invokes cancellation cleanup, and rebuilds the executor. |
| `nn` tool cancellation | `[195348540,195349196)` | `93a6cbb46376c2e709da66fc58341cdbde7c78638338d46ddbe1d340d8033edb` | Calls `discardAndAbortInFlight` and returns IDs for in-progress removal. |
| executor lifecycle | `[195349196,195357760)` | `6459257ae75dd02d6b6f06df7f14bf7ce0468ebea7d3ce7909e239bdd527cba8` | `discardAndAbortInFlight` marks the executor discarded, aborts executing calls, and counts queued calls as never started. |
| `Wl` unmatched tool uses | `[195420477,195420769)` | `a57cc6636bc80c34121c148bc729cfafdb058e66a1a6238def0a59a77b82d410` | Emits synthetic error `tool_result` rows for tool uses without a paired result. |
| model-error tool-cancel text | `[195414572,195414734)` | `73da64a566740e4e4897ce3f5cca1457602bc630f7800a74306482dd22751658` | Explicitly warns that effects may have happened if a tool call had already started. |

This is a source trace of the final thrown-error path, not an execution of
Native's provider failure handling. Any retries performed inside the
provider client before the error reaches `Xl` are outside this source window.

## Server-fallback cleanup and decline rows

The server-fallback event has a separate cleanup path from `abandonAttempt`.
The branch is `[195453912,195458354)`, SHA-256
`cbeb6affbd77bc8220676eef8842abe4b17551e562f5058519458d88b30a62e3`. It
enters only for a user-visible fallback projection. If the target fails
`uA(toModel) ?? (Ur(toModel) || ay(toModel))`, Native declines it.

The query's `je` local aliases `ue.toolResults` in
`[195439205,195439671)`, SHA-256
`acbac16529445942a6c430201d13f36f9f1401f1ed384d0ddebfa31164c77e5d`. On a
normal per-event result poll, `Tn` yields the completed result row and then
passes that same row through `xr`; `xr` appends user rows (and attachment rows)
to `je` at `[195429764,195430089)`, SHA-256
`0f2b2a6e52dbbc3f5932f2f60e43d6912d64070d27545cbaa103f2dbe6411e23`. `xr`
runs after the consumer resumes the yielded result. Thus decline sees rows
already handed off by prior polls; this does not claim to include a result
that completed asynchronously after the prior poll but has not yet been
polled and resumed into `je`.

On decline, Native first makes `We = je.slice()`, then calls
`ue.reset({clearAssistantMessages:false})`, which empties the original `je`
array. It selectively removes from `K` only the discarded assistant UUIDs and
rows whose `message.model` equals the refused target. It then tombstones those
removed `K` rows followed by every row in `We`, with no `displayOnly` field,
removes in-progress tool-use IDs, and only then builds the synthetic decline
row. `break` exits the query event loop; it does not run that iteration's
common `yield*Tn()` poll.

The synthetic row depends on the fallback reason:

- For `reason === "refusal"`, Native calls
  `$dt("refusal", {type:"refusal", category:apiRefusalCategory??null,
  explanation:null, fallback_credit_token:null,
  fallback_has_prefill_claim:null, recommended_model:null}, requestId,
  servingModel)`. The helper is `[188298052,188299837)`, SHA-256
  `7a19dc31283968e16b1583ddc200d3b9ad0097f094e9de501f61733083242455`. It
  returns an assistant API-error row (not an SDK `LlmError`): the text is
  produced by Native's refusal-message helper; the row carries
  `error:"invalid_request"`, the event request ID, `message.stop_reason` set
  to `"refusal"`, and `message.stop_details` set to the supplied refusal
  metadata. This call omits explicit clock/UUID arguments, so the helper uses
  its defaults.
- For any other visible reason (including sticky refusal), Native calls
  `cs({content:"The server routed this response to a model that is not in your
  organization’s availableModels allowlist; the response was discarded.",
  error:"invalid_request", now:B.now, uuid:B.uuid})`. The assistant API-error
  row constructor is `[191309034,191310574)`, SHA-256
  `194c892f3602f2fe05b6bc63e35ffc97bd0b5c795f96667fc8f4eddce4bfa521`. It
  sets `isApiErrorMessage:true`; the generic message has one text block,
  Native model `xa`, `stop_reason:"stop_sequence"`, empty
  `stop_sequence`, default usage, an outer timestamp from `B.now()`, and
  separate generated outer `uuid` and nested `message.id`.

If the helper returns a row, Native awaits `r.appended(Ro)`, pushes the same
`Ro` object into `K`, and yields it. The generic user-row constructor used for
the existing `je` rows is `[191312309,191313825)`, SHA-256
`643c2975cfe603827a217f3ebc358cdd155416c704bd0d6b37bcd0a25de1e33f`; it
preserves the existing message content and has outer UUID/timestamp fields.
The fallback tombstones pass those original row objects rather than creating
new identities or timestamps.

On accepted fallback, Native always prunes `K` by
`M.discardedMessages[].uuid`. It does not clear `je` unless
`tombstonedToolUse` is true. In that case it snapshots all of `je` into `to`,
resets `je`, discards/rebuilds the executor, then tombstones every entry in
`to` with `displayOnly:true`; this is whole-current-result/attachment-list
cleanup, not a filter by the discarded tool-use IDs. When the flag is false,
`je` remains.
The accepted branch also `continue`s before the common per-event `Tn` poll.

Native's consumers do not use `displayOnly` to protect persisted rows. The
generic event dispatcher at `[191363275,191363406)`, SHA-256
`f0ef9d2cbe5620fbf0146acd1280e3bb386cd744fdafa69cd7d65ba61b809d07`, calls
`onTombstone(e.message)` and drops the `displayOnly` field. The headless turn
consumer at `[206327451,206328033)`, SHA-256
`11b53a15cebf3070dadf5e34ab6259080d0acca6ecb6db3cc921df8511022fcb`, calls
`xr.evict(message.uuid)` unconditionally. The interactive transcript callback
at `[209676451,209676591)`, SHA-256
`eb408c6c149e1977195ef9c08578327e7d0e5e6dc79e35e373a50881d8fd4d55`, removes
the in-memory row by UUID and calls `xNt`; `xNt` at
`[191536950,191537035)`, SHA-256
`4644d4eab64b58b9e950f547eb0ca17685e89f03e587781565d54a5ceab435f8`, invokes
`removeMessageByUuid`. `MSe/ASe` record transcript snapshots; they do not add
a `displayOnly` check. Therefore accepted fallback's `displayOnly:true` on
both discarded assistant rows and the entire `to` list still results in
persisted-row removal in the Native consumers.

The reset helper is `[195408929,195409771)`, SHA-256
`82fa20588a3b3404c18113d3de3839f31742cb6ca181803c7fbcaab5d71fb6ea`.
Its `abandonAttempt` separately tombstones every current assistant and result
row before resetting with `clearAssistantMessages:true`; that broader cleanup
must not be attributed to server fallback. After the query event loop, the
normal post-stream `getRemainingResults()` drain is
`[195485961,195489200)`, SHA-256
`7fcac4553bbb01791b1e0da0e5005fdc6c061a051391aa00de6bc79d42995a47`. A
fallback-triggered break/continue bypasses the current event's `Tn` poll. The
`sa` provider-stream wrapper at `[195357760,195358236)`, SHA-256
`6f4e4413fb4254a799ea9f686bea69ec26fec13f73796d3b4d90d28c50ad7192`, does
call the underlying iterator's `return()` in `finally`, but ignores its return
value and yields no event from that cleanup. This establishes that the wrapper
does not forward rows from `return()`; any underlying cleanup side effects are
outside this claim.
