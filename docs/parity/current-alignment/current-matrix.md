# Current Claude Code 2.1.295 alignment matrix

Reference: **2.1.295**, binary SHA-256 `0116ee2e0a513900b633d9951367f18747686478e2b462805b8c31609f047f70`.
Version updates are excluded. Source agreement, regression tests, byte comparisons and lifecycle constraints are separate evidence classes. Original Native captures remain immutable; no Native executable was launched in this batch.

## Current integrated candidate

| Repository | Current state | Current evidence and remaining gate |
| --- | --- | --- |
| llm-client | Main `8730791aba6a355d48e299ce51ae662a9476bfd9`; merged origin main `179d336`, committed and pushed without force. Remote SHA verified, integration/origin-main divergence `0 0`. | r56 full plus r57 scoped repairs: 2,707 passed, 0 failed, 8 ignored; exact fingerprints and formatting/equivalent-syntax review support reuse. CI Clippy/fmt, Rustdoc and downstream API/277 doctests passed. No additional tokenizer runs. |
| Harness | Private `56e22dd7` merges main `b6b4f92`; current prefix/overlay source work follows this merge. SDK canonical manifest/lock now pin private `fdfc922` (AFK marker fix; 614 library tests and CI Clippy/fmt passed). Previous `8730791` metadata verified; `fdfc922` locked metadata is running. New Harness main `b8cd509c` merge awaits live writer release. | r51-48 focused 18 instances: 12 passed, 6 failed. UPE 3/3 and account-generation 1/1 passed. Four staging and one Skill failure shared missing accepted NAr request projection (now repaired, awaiting rerun); the new context test now expects the exact Native email instruction. Full workspace/current combination not passed. |
| Product | Private `0d2d295c0` includes merge of main `49ed46eb3`; source model display and new mobile effort controls both retained. | Final canonical pins, generated index/bindings, all Rust/bridge/Electron gates and Flare package/runtime/GUI remain open. Current bridge typecheck/build and 85 Cargo-free tests passed; Electron typecheck/build and all four repaired interaction targets passed (r51-58/r51-61/r51-63, unchanged-input reuse). Three metadata tests remain deferred. |

## Source closure and active work

| Domain | Current conclusion | Remaining |
| --- | --- | --- |
| Native configuration | Actual schema-backed getter trace proves B6/HUn use `M.bool`: only 1/true/yes/on are true. Corrected v5 is applied; original captures unchanged. Unknown strings are false, not raw nonempty truthy. | Same-candidate runtime tests. |
| Admission/UPE | Account epoch captured before credentials await. Blocked admission no longer publishes NAr. Accepted durable writer binding and task-local UPE IDs are current. | Accepted NAr request projection repair is applied without early live publication; warm/cold tests await current dependency gate. |
| Thinking | SDK typed rejection target, single request-context resume intent, and exact Lsn floor provenance are applied. Provenance matches the resolved encoded output ceiling and permits only the 1024 floor exception. | Host overlay/type latch and prefix recovery are integrated, awaiting current Cargo feedback. Actual max-output resume v4 must carry SDK context facts and close local model/refusal producers. Preserve incoming output-budget/frozen-retry behavior. |
| Betas | Cloud CLI additions now reach all four exact Native route header helpers. Bedrock model/body beta subset remains independent. AFK-only error text in the regression can correctly cause HQ/TBC retry; no Kie/AFK retry is implied. | Source-bound AFK nonstream test is applied. Streaming AFK-before-context/TBC overlap repair is applied with marker-correct regression fixtures. SDK fdf fixes Jw classifier. Execute actual runtime gate. |
| Models/Insights | Source model/carrier consumers, managed/Mods/classic model-switch dispatch and Insights are preserved in the checkpoint. | Main merge interface closure and current regression gates. |
| Commands/Skills/MCP/Mods | Existing source repairs and historical evidence retained; Bun fixed to 1.4.3 revision c6da4a4d3010e5553438c60f6bd76d981976867c. | Full current workspace/client/UniFFI gates and real package lifecycle checks. |
| Delivery | SDK stage is validated and pushed; Harness/Product are not yet delivered. | SDK commit → Harness canonical pin/commit → Product canonical pins/locks/index, full checks, signed macOS package and GUI, then merge/push main without force. |

Evidence: shared `acceptance/turn-progress-r51-20261014/checkpoint.json`, `r51-48-root-session-context-integration`, `r51-48-v5-typed-config-integration`, `r51-49-root-effective-budget-integration`, `r51-50-harness-main-integration`, and task cache logs `r51-48` / `r51-49`. Detailed prior results are archived in [the preceding matrix](archive/current-matrix-before-r51-50-20261014.md).

## Scope and gate rules

- LLM communication remains in llm-client; current APIs only, no old compatibility branches.
- Preserve brand, multi-provider, Fusion and LingXi UI. Remote service and Local Apps behavior are excluded; incoming main features are preserved during merges. No new Computer Use implementation changes.
- One Cargo runner and one shared cache; do not rerun unchanged failures. One DeepSeek representative only; no per-tokenizer matrix or repeat tokenizer run.
- SDK uses the actual CI default-member test/Clippy graph. An extra r51-49 `--workspace` invocation reached vendor-only native TLS and failed at mixed registry/vendored hyper types; it is recorded separately and is not a default production graph pass.
- Original raw outputs are compared without reserialization/deleting fields/reordering arrays. Random IDs/times use explicit constraints. Unexecuted/source-only contracts are not marked passed.
- macOS acceptance requires `npm run package:mac:flare`, signing checks and actual launch/interactions. Cross compilation is not device/runtime acceptance.

## Bounded contracts retained for final acceptance

The table preserves the finite original contract inventory. Current source and
execution evidence must be attached before a row closes; old capture plans are
not an obligation to repeat bulk collection. Historical release comparisons
are reused only where the frozen 2.1.295 producer is established unchanged.

| Contract | Behavior to accept |
| --- | --- |
| M2-HTTP-1 | Compare the first physical Native Claude Messages request in headless print-text mode: HTTP/1 request line and query, raw header spelling/order/CRLF/framing, request-body bytes and offered response. |
| SDK-ERR-1 | Anthropic HTTP 400 with nested error.message containing one isolated UTF-16 unit, nested message null, and empty response body. Preserve response bytes and Native CLI result text without reserializing captured output. |
| HOST-ERR-1 | Replay captured HTTP error bodies through production `ApiService → ProviderApiAdapter → ConversationOrchestrator`; compare Native result text at `OutputStream` and persisted assistant rows. |
| HOST-ERR-STATUS-1 | Status-specific HTTP presentation for 403, 404, 413, plus separately scoped 401/500 responses with explicit `x-should-retry:false`. |
| HOST-REFUSAL-1 | Final Native terminal refusal presentation from stop reason/category/explanation, model display label, and interactive mode; compare exact JSON CLI result and persisted source fields. |
| CLI-ERR-1 | Replay the same bodies through the exact Native mode `--print --output-format json` and the pinned Product CLI entrypoint; compare raw stdout schema, error/status flags, final result text units, stderr and exit status. |
| SDK-ERR-2 | Statusless Anthropic SSE `event:error`, `data.type=error`, nested `timeout_error` with U+D800 message; compare no-start and partial-text-before-error cases plus a generic `api_error` control. |
| SDK-1 | Claude Messages/Foundry request method, URL, headers, ordered body fields, exact-string sidecars, final body string normalization, signing inputs, response and SSE frame bytes. |
| SDK-2 | Non-Anthropic providers retain their own URL, headers, body/signing, streaming, error and fallback contracts. |
| SDK-3 | Credential snapshot, account change, retry latch, body/stream fallback, cache and model selection agree at each physical attempt. |
| TURN-1 | `/` and unknown slash command fall back to ordinary turn with unchanged prompt; ParseFailure hint/refusal is checked per execution mode. |
| TURN-2 | Retry, cancellation, truncation, refusal and terminal result preserve event order and tool/transcript identity. |
| STOPDETAILS-1 | Anthropic refusal `stop_details.explanation` containing U+D800; compare raw response, public CLI result, and durable assistant row against fixed Native source units. |
| TURN-3 | Public CLI SIGINT while one provider request is in flight: terminal result envelope, interruption row, and persisted event order. |
| TOOL-1 | Tool registration with `isDeferred` absent, `false`, `true`; first physical Messages request's `tools` array and schemas. |
| TOOL-2 | Tool check/call, permission allow/deny/rewrite, result/error, pre-cancel and post-cancel behavior, process side effects and transcript mutation. |
| TOOL-3 | Oversized registered-tool string result: source preview UTF-16 units, Claude Messages request serialization, session JSONL row and persisted UTF-8 file bytes. |
| TOOL-4 | Cold resume of object-valued `tool_result.content`: JSON-stringification, 50,000 UTF-16-unit cut, code-point boundary and omitted-character count. |
| CMD-1 | Skill names with supplementary Unicode and punctuation resolve to the local skill; unpaired UTF-16 command input retains the Native persistence/final-wire boundary. |
| PROMPT-1 | Main, hook, child, fork and resume prompt assembly; CLAUDE.md/AGENTS, skill discovery/read, order and attachments. |
| PROMPT-2 | Context compaction, cache attribution, token limits, aliases and request counting use current model/protocol policy. |
| SESSION-1 | `session.append` middleware rewrite and raw returned event for user and system messages; preserve source UTF-16 units through Native result and candidate transcript path. |
| SESSION-3 | Cold `--resume` distinguishes exact persisted source units from Native's resumed public/API projection. |
| EFFORT-1 | A per-turn effort override survives a cold resume and is projected into the resumed model request without replacing the historical turn's system metadata. |
| PER-TURN-CONTROL-295 | The default main request includes the `per-turn-control-2026-07-01` beta and a per-message effort row only when the experimental-beta setting permits it, across first-party/Foundry and Sonnet/Fable routes. |
| SESSION-2 | Direct return, denial, thrown error, subscription removal and plugin/process shutdown. |
| SESSION-4 | JSONL persistence, parallel tail recovery, attachments, crash write, compact boundaries and rollback. |
| SESSION-5 | Generated session name and prompt-suggestion public scalar APIs when the model's final text contains an unpaired UTF-16 unit. |
| AGENT-1 | Agent/task spawn, offer, receipt, wait, stop, report admission, handback and nested/cold recovery. |
| MCP-1 | JSON-RPC initialize/discovery, tools/resources/prompts, input-required, auth, result/error/null presence and transport frames. |
| HOOK-1 | RegExp matcher and event source, registration/order, match/nonmatch and raw event payload. |
| HOOK-2 | Hook rewrite/refusal, commands/settings merge, env overlays, managed policy, built-in protected events and refresh. |
| HOOK-RELEASE-1 | Compare the Native public Stop `type:prompt` hook request across historical 2.1.293 and 2.1.294 without mutating either baseline. |
| AGENT-RELEASE-1 | Compare the Native public `Agent` → general-purpose child → `SubagentStop` prompt hook → parent continuation using preserved 2.1.293/2.1.294 evidence and the frozen 2.1.295 target. |
| FINAL-TARGET-1 | Freeze one Native version for the final alignment pass and close impacted acceptance gates against that exact binary. |
| MOD-1 | Public Mods middleware event ordering, `next(e)` rewrite/direct answer/error/deny, module load and unload boundary. |
| MOD-2 | `$.tool.register`, `$.tool.list/call/check`, command and prompt APIs; provider-facing schema/catalog order. |
| MOD-3 | ModUI subscriptions, events, resolution/render tree, update/disposal, restart and visible terminal/desktop surfaces. |
| CLIENT-1 | CLI, TUI, desktop and mobile callers preserve prompt, session, command, cancellation and transcript identity under current client protocol 25.0.0 (formal contract blessing passed; generated bindings pending); Local Apps dedicated behavior is excluded. |
| RUNTIME-1 | Bun worker startup/restart, filesystem/process/sandbox, env inheritance, cancellation/deadline, stdout/stderr and cleanup. |
| BUILD-1 | Three repositories resolve one candidate; dependency-complete checks, macOS production app and package, and required cross-platform checks. |
| BASH-INPUT-295 | Normalize Bash `tool_input.command_description` and `timeout_ms` aliases before `PreToolUse`, validation, and execution; preserve Native field priority and source units. |
