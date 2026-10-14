# Mods `$.tool.call()` and Native 2.1.289

This note pins the Native Promise wrapper, its Projects consent predicate, and
the current Harness hand-off boundary for `$.tool.call()`. It does not claim an
end-to-end Native tool execution or an installed-Mod runtime comparison. The
executable oracle runs the extracted Native `eU` wrapper against a fake `W1`
host so its Promise shaping, result-frame extraction, Projects guard, and
caller-signal cancellation branch are directly exercised.

Run the oracle with the pinned macOS arm64 executable:

```sh
node scripts/tests/mods_tool_call_289_oracle.mjs \
  /private/tmp/harness-claude-code-2.1.289-darwin-arm64 --check
```

It fails closed on an executable, source-chunk, source-range hash or generated
result-fixture mismatch. The recorded probe output is
[mods-tool-call-2.1.289.json](mods-tool-call-2.1.289.json). No Native executable
or extracted source is checked in.

## Native identity and byte ranges

The pinned executable SHA-256 is
`03d66745e3bb69ec727d66023696f3820bc0a00a8a5ba725eb6706d0c67cbe69`.
The separately extracted Mods type declarations have SHA-256
`8ae1244d19d4b393261605fe72d517b66be0d7e1c214c378cdec5107e8b0b46c`.

Keep the two offset coordinate systems distinct:

* In the executable, the relevant extracted source chunk is the absolute
  half-open byte range `[187181803,191691521)`. The final byte is NUL. The
  extracted file `relevant-src/src_187181803.js` omits that NUL and contains
  4,509,717 bytes; its full SHA-256 is
  `47b62b9ac561f0cc5cd75ab6e7b9e14c1b027cccebfb900b56f07685c69fa825`.
* The `eU` function is at extracted-JS-relative
  `[1671409,1673850)`, which maps to executable-absolute
  `[188853212,188855653)`. Its exact range SHA-256 is
  `a8488d8e2775de457b6ff288249a54f1367ef3116b6f9b64b78863bedf22dd66`.
* The helper chunk `relevant-src/src_182637043.js` is extracted from executable
  bytes `[182637043,182864930)` with a trailing NUL omitted. The extracted
  file contains 227,886 bytes and its full SHA-256 is
  `b8b83549f7b4273ed3f6b4669d666e535f9abfd831ea3952e9387ac73500e85c`.

The oracle records and checks every full 64-character range digest. Selected
Native spans are:

| Native source | Executable absolute byte range | Extracted-JS relative byte range | SHA-256 |
| --- | ---: | ---: | --- |
| `eU` Promise wrapper | `188853212..188855653` | `1671409..1673850` | `a8488d8e2775de457b6ff288249a54f1367ef3116b6f9b64b78863bedf22dd66` |
| `hfo` synthetic tool-use builder | `188508394..188508499` | `1326591..1326696` | `4e933392955334171fd9efc7dd0a1ab917ecebf507c0037e30e6c010318ee6f7` |
| `bfo` user result-frame extractor | `188508846..188508932` | `1327043..1327129` | `fe7a3d4a251aa07165ee0a7aa300e4ec6aea2efd2d05349a8a24044c217b4858` |
| `Aq` result clone/parser | `187732214..187732252` | `550411..550449` | `825dbf7480279df62d806647d028466d4c531bb1f139dae21a93b68f78057aff` |
| `FAt` error result helper | `188508310..188508356` | `1326507..1326553` | `045f105e8a6e8de36b56189cfb4a4a40534f75ce98a68423376f9fd1777cf51f` |
| `_se` reserved input and `$shadowed` merge | `182734762..182734864` | `97719..97821` | `5bd63444d844fa9131e1eaf820ddf5d57212fcf472794ec2f1a245b89adf8702` |
| `r5r` result text extraction | `182734921..182734977` | `97878..97934` | `3c1cf0c0fced0ed217d10afd8a44a8d74874a7c95176225e695cb6569ccd2894` |
| `W1` tool execution generator, source-only | `188814536..188819240` | `1632733..1637437` | `2dac343f7459b2386e673fc7726ae690d0b2bb0304ae2e8727fda593fcbaa3d4` |
| `DVt` middleware outcome to messages, source-only | `189564393..189566538` | `2382590..2384735` | `2d239227114d8a8a6f9c14bce7f44192a2758fd8c3e8ef4226498d90f9eaf52a` |
| Projects `Uoe` predicate | `183805687..183805858` | Not in the selected extracted chunk | `7507090d046e83385390644695e89b695b66ec7b919048cc1dda2e69e98fa3cb` |
| `WebFetch` alias used by `Uoe` | `183190392..183190431` | Not in the selected extracted chunk | `a54a825ea4a9f70c0e0a26180866f0076cc8d8b3dcb14f4cf1e22eaf7ae9fcde` |
| `WebSearch` alias used by `Uoe` | `183304712..183304751` | Not in the selected extracted chunk | `dd6beace88fabb5ea7984a88a74082229eeaf84b73195d7e7be1de6802f400f9` |

The absolute ranges above are executable coordinates. Relative coordinates are
indexes into the named extracted JS file; the script verifies that adding the
captured chunk start maps those bytes back to the same executable slice.

## Native Promise behavior

`eU` builds a fresh `toolu_plugin_<uuid>` tool-use through `hfo`; caller-provided
`tool_use_id` and `agentId` do not become the new inner ID. `_se` removes the
reserved fields and merges `$shadowed` arguments. It resolves the tool from the
session catalog, checks Projects consent before constructing its execution
context, creates an inner `AbortController`, composes the caller signal, and
adds consent as a human-origin message. Consent is context for the permission
decision; it is not itself an allow.

`eU` consumes `W1` frames through `bfo`. For the matching `tool_result`, it
collects the Native text via `r5r`, `is_error`, `toolDenialKind`, and the first
`toolUseResult` via `Aq`. Its ordinary final Promise shapes are:

```js
{ result: toolUseResult, text: toolResultText }
{ result: toolUseResult, text: toolResultText, isError: true }
{ deny: toolResultTextWithoutToolUseErrorTags }
```

`FAt` is the exact helper for the ordinary error shape. A denial is selected by
`toolDenialKind`, separately from the `is_error` flag. No matching result causes
an error; when the composed signal is aborted, the error says the result was
not produced “(aborted)”. `eU` also has special Agent launch-only and settled
branches. Those branches are pinned inside the `eU` span but are not exercised
by this fake-host oracle.

The public Mods declarations describe optional `context` and `ref` on core
`tool.call` middleware results. `ref` identifies core messages that remain
host-side when `next(e)` is passed through; `context` is model-only reminder
text from the middleware layer, not a tool `context_modifier`. The direct `eU`
ordinary Promise branch itself returns only result/text/error or deny. Do not
invent JSON fields for host-side effects merely because the tool dispatcher
produced them.

The oracle executes the literal extracted `eU`, `_se`, `r5r`, `bfo`, `Aq`,
`FAt`, `hfo`, and `Uoe` functions. Its fake `W1` yields Native-shaped frames for
raw structured success, logical tool error, policy denial, no result, and
caller cancellation. The tests therefore verify wrapper behavior and helper
semantics. `W1` itself requires the real tool catalog, permission subsystem,
hooks, process/session state, and tool implementations; it is pinned but
source-only here. This is not a Native runtime launch or a permission-gate
integration test.

## Projects consent predicate and Harness inputs

The pinned `Uoe` expression is:

```js
function Uoe(e){
  if (e !== _r && e !== Pm ||
      !(Is(el).value || a.CLAUDE_CODE_PROJECTS_SESSION || Sbt(SS()))) return false;
  let n = io(Tf, false);
  return !(n.value === true && n.source === "payload" && !Use());
}
```

The pinned aliases are `_r = "WebFetch"` and `Pm = "WebSearch"`. Thus the
consent rejection is tool-specific: other tools do not fail this guard. The
Projects state inputs are the dynamic `Is(el).value`, the process setting
`CLAUDE_CODE_PROJECTS_SESSION`, and `Sbt(SS())`; the predicate also honors the
source-aware `io(Tf, false)` payload exception. This is not the managed
`sec-default` permission rule and must not be implemented as a broad deny for
all tools or as an inferred cwd/project-root test.

In this checkout, `ModSessionContext::tool_call(plugin, input)` in
`crates/hooks/src/mods.rs` has no cancellation parameter and no trusted
Projects-session input. `OrchestratorConfig` has query/source and fork facts but
no Projects-session field; a repository search found no use of
`CLAUDE_CODE_PROJECTS_SESSION`. The trait exposes cwd/root, but those are not a
valid substitute for Native Projects session state. A future implementation
needs an explicit trusted session fact wired from the actual runtime
composition/session owner; until that source exists, the Projects gate remains
unimplemented rather than defaulting to false.

Cancellation has a real but incomplete path: `background_api_loop` in
`crates/hooks/src/mods.rs` associates an API call ID with an `AbortHandle`, and
`api.cancel` aborts that host future. `dispatch_host_api` therefore drops the
ongoing API future, but the orchestrator base method still calls
`dispatch_tool_uses_tracked_deferred_core(..., None, None, 1, consent)`.
The actual `ToolUseContext.cancel` is consequently absent. A future bridge must
carry the caller/API cancellation into the permissioned dispatcher token so
permission waits and cancel-sensitive tools observe it; merely seeing a
cancelled API Promise does not prove the underlying tool stopped.

The Promise wrapper contains no model-usage field. `ToolInvocationResult` also
has no usage member. Tool-triggered LLM work (for example an Agent tool) must
continue to use its existing request/usage accounting; `$.tool.call()` itself
must not fabricate or re-charge provider usage.

## Current Harness boundary and complete mini-call finalizer plan

The current direct base method is
`ConversationOrchestrator::tool_call` in
`crates/orchestrator/src/turn_loop/tool_dispatch.rs`. It correctly resolves a
session-visible tool, strips reserved fields, mints one internal plugin tool
ID, adds consent as a temporary user message, and calls the permissioned core
directly so the same `tool.call` Mod layer is not recursively re-entered. It
then takes only the first `ToolResult` and returns the staged raw result/text/
error or denial. The returned `DeferredToolDispatch` also holds
`prevent_continuation`, injected messages, context modifiers, and
`post_tool_batch_calls`; the direct API method drops them. Its isolated
`ModResultStage` is not committed, so buffered frame/MCP/turn-end/denial state
does not reach the host settlement path.

The normal event path in `dispatch_tool_uses_tracked_deferred` keeps each
`ModCoreRun` and its `ModResultStage`, gives the Mod layer an internal `ref`,
then commits the selected result after middleware settles. The visible-response
path appends injected messages, applies context modifiers, and invokes
PostToolBatch after result handling. The API path needs an equivalent
single-call transaction, not a second outer event dispatch:

1. **Create one API-call transaction.** At the `ModHost` API dispatch boundary,
   mint an opaque host-owned call/transaction identity and carry the actual
   caller cancellation plus trusted Projects-session snapshot. The worker's
   JSON input must not select either value.
2. **Run the core exactly once per `next(e)` call.** Keep using the direct
   permissioned core below the already-running `tool.call` middleware. Capture
   the complete `DeferredToolDispatch` and `ModResultStage`, not just its first
   result block. Each core execution has its own opaque `ref`; if middleware
   invokes `next` multiple times, each run remains distinct and only the run
   selected by the final pass-through/replacement semantics is settled.
3. **Resolve the Native Promise shape separately from host effects.** Raw
   `toolUseResult` becomes `result`; `tool_result.content` becomes `text`;
   `is_error` becomes `isError: true`; `toolDenialKind` becomes `{deny}`. Keep
   missing result as an error and denial distinct from ordinary tool error.
   Do not expose `DeferredToolDispatch` or invent extra Promise properties.
4. **Commit session effects once at the API call boundary.** Use the result
   stage to retain raw result/MCP/denial/turn-end metadata; append each actual
   injected message once with its source tool-use ID; apply context modifiers
   once at this mini-batch boundary; run one PostToolBatch call with the final
   tool name/input/response; and carry `prevent_continuation` to the enclosing
   turn disposition rather than dropping it. If the final middleware result
   replaces/denies a core result, settle only the side effects Native permits
   for that final answer; do not publish stale staged metadata.
5. **Do not synthesize duplicate visible transcript rows.** The Native `eU`
   tool-use is virtual and its result frame is consumed to form the Promise.
   Preserve host-visible injected messages and required call effects, but do
   not persist a second outer assistant tool-use/result pair solely to reuse
   `visible_response`. Also do not call `commit_mod_result_stage` blindly if
   that would emit a user-visible SDK `tool_result` event for the internal
   virtual call. The mini-call finalizer should own the frame's host-side
   settlement and drain all side tables exactly once.
6. **Keep batching and cascade boundaries explicit.** The API call is one
   mini-batch and emits its own PostToolBatch once after its result/effects.
   Do not also add that API call to the parent model response's batch list, and
   do not run the full `dispatch_tool_uses_tracked_deferred` wrapper from
   `ModSessionContext::tool_call` (that would re-enter the same `tool.call`
   middleware). In the ordinary assistant-tool path, leave the existing
   outer batch finalizer unchanged. Tool side effects already executed are
   never represented as rolled back if a later Mod replaces text or cancels.

`ToolInvocationResult` in `crates/core/src/host/tool_invoker.rs` has
`is_error`, `data`, `model_content`, `turn_end`, and `context`. The desktop
subagent Mod wrapper supplies `context`, and `crates/agent/src/runner.rs`
consumes it as model-only context associated with the tool-use ID. The normal
registry adapter in `crates/tool-api/src/tool_invoker_impl.rs` builds this
record from `ToolCallResult` but currently drops `new_messages`,
`context_modifier`, and `mcp_meta`; a lossless nested-Agent path may need a
separate typed effects carrier. Keep `context` distinct from
`context_modifier`, and keep `turn_end` host-side rather than turning it into a
Promise field.

Likely implementation ownership once production work is authorized:

| Path | Responsibility |
| --- | --- |
| `crates/hooks/src/mods.rs` | Pass a trusted API-call context through `ModSessionContext`; connect `api.cancel` to cancellation; run one completion/abort callback after middleware settles. |
| `crates/orchestrator/src/turn_loop/tool_dispatch.rs` | Capture the whole deferred result/stage, implement the single-call finalizer, preserve permission and Native result mapping, and prevent duplicate middleware/PostToolBatch dispatch. |
| `crates/orchestrator/src/conversation/transcript.rs` | Commit or discard staged frame/MCP/denial/turn-end metadata exactly once without leaking a virtual tool result as a visible row. |
| `crates/orchestrator/src/turn_loop/batch_hooks.rs` and `visible_response.rs` | Reuse/factor message-injection, context-modifier, and PostToolBatch behavior at the mini-call boundary while keeping ordinary batches unchanged. |
| `crates/core/src/host/tool_invoker.rs`, `crates/tool-api/src/tool_invoker_impl.rs`, and child Mod invoker/runner paths | Preserve any effects that cross the nested `ToolInvoker` boundary; keep child `ToolInvocationResult.context` semantics intact. |
| Runtime session composition | Supply the real Projects-session predicate from its trusted owner. No current `cwd`-based fallback is valid. |

The above is a plan, not an implementation claim. `llm-client` remains the
owner of provider wire behavior and is outside this host-effects work.

## Focused acceptance tests for the authorized implementation

Use the actual Rust `ModHost` and `ConversationOrchestrator`, a real registered
tool, and the real permission gate—not only a mock `ModSessionContext`:

* One successful tool returns raw `data` distinct from `model_content`, an
  injected message, a context modifier, MCP metadata, and a turn-end marker.
  Assert Promise result/text, session history/persistence/source IDs, changed
  route before the next request, PostToolBatch input/response and ordering, and
  every staged metadata effect exactly once.
* One logical-error result has `is_error=true`; assert `isError:true` and raw
  result/text remain distinct, with normal effects ordered once. Separately,
  policy denial returns `{deny}` and never invokes the tool; consent text does
  not bypass the gate.
* Use a trusted Projects-session fixture input and the Native WebFetch/WebSearch
  predicate. Assert those tools reject consent before the dispatcher runs,
  ordinary tools do not hit that predicate, and the payload-source exception
  matches `Uoe`. Do not substitute a cwd-only fixture.
* Abort after the real tool signals it has started. Assert cancellation reaches
  `ToolUseContext.cancel` and a cancel-sensitive external tool is stopped; a
  rejected API Promise alone is insufficient.
* Have one Mod handler call `next` and return the answer unchanged; assert its
  opaque ref selects one core transaction and effects/PostToolBatch commit once.
  Add a replacement/deny case that proves no stale frame or duplicate batch is
  committed. Keep this separate from the normal model-issued `tool.call` event
  test to catch recursion/cascade regressions.

The source oracle intentionally does not claim those Harness integration gates
pass; it only executes Native `eU` with a fake `W1` frame producer and pins the
rest of the source boundaries above.
