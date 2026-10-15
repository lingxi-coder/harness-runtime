# Current Claude Code 2.1.295 alignment matrix

Reference: **2.1.295**, binary SHA-256 `0116ee2e0a513900b633d9951367f18747686478e2b462805b8c31609f047f70`.
Version updates are excluded. Source agreement, regression tests, byte comparisons and lifecycle constraints are separate evidence classes. Original Native captures remain immutable; no Native executable was launched in this batch.

## Current integrated candidate

| Repository | Current state | Current evidence and remaining gate |
| --- | --- | --- |
| llm-client | Main `161944a5b021f08aad83916ae7df6804718480a0`; deferred OAuth budget-seal follow-up to `fdfc922` is committed and pushed without force. Remote SHA verified; divergence `0 0`. | Latest delta: 8 integration + 2 library body-policy tests, default-member CI Clippy/fmt passed (r51-77); valid host-managed OAuth budget projection seals and tampered alternative budget is rejected. Historical baseline `8730791`: 2,707 passed/8 ignored; `fdf` AFK delta: 614 library tests plus CI Clippy/fmt. These are layered evidence, not a full-suite rerun on `161944a`. No additional tokenizer runs. |
| Harness | Private `8ed614d8` merges main `b8cd509c` after source checkpoint `8ebe9c8a`; all conflict consumers retain the current typed UTF-16 contracts. SDK canonical manifest/lock now pin published `161944a` (deferred OAuth budget-seal fix). Locked all-features metadata passed (813 packages; all 5 SDK packages canonical `161944a`). Actual host llm-runtime all-features library suite passed 1,392/1,392 (r51-78); previous `fdf` desktop compile passed r51-76 and tool-ui all-targets Clippy passed r51-77. | Actual host full llm-runtime all-features library suite: 1,392 passed/0 failed on canonical SDK `161944a` (r51-78), following clientAuth seal and Native Main focused 1/1 each. Actual streaming resume passed; LocalApp 23 tests and tool-ui Clippy passed. Full workspace/current three-repo combination not passed. |
| Product | Private `f9c9a56a` includes main `95cf701` and fixture checkpoint `0d2d295c0`; source carriers/resume and incoming mobile visualization/voice fixes are retained (four overlapping files reviewed). | Final canonical pins, generated index/bindings, all Rust/bridge/Electron gates and Flare package/runtime/GUI remain open. Current bridge typecheck/build and 85 Cargo-free tests passed; Electron typecheck/build and all four repaired interaction targets passed (r51-58/r51-61/r51-63, unchanged-input reuse). Three metadata tests remain deferred. Electron full-suite r51-58 had 1,557 pass/4 fail/3 skip; all four failures have subsequent focused green evidence, not a full-suite rerun. |

## Source closure and active work

| Domain | Current conclusion | Remaining |
| --- | --- | --- |
| Native configuration | Actual schema-backed getter trace proves B6/HUn use `M.bool`: only 1/true/yes/on are true. Corrected v5 is applied; original captures unchanged. Unknown strings are false, not raw nonempty truthy. | Same-candidate runtime tests. |
| Admission/UPE | Account epoch captured before credentials await. Blocked admission no longer publishes NAr. Accepted durable writer binding and task-local UPE IDs are current. | Accepted NAr request projection repair is applied without early live publication; warm/cold tests await current dependency gate. |
| Thinking | SDK typed rejected type (Host retries the opposite), single request-context resume intent, and exact Lsn floor provenance are applied. Provenance matches the resolved encoded output ceiling and permits only the 1024 floor exception. | Historical r51-71 had 28 targeted passes including thinking/prefix/B6/HUn/AFK; its physical-attempt cap expectations were superseded by Source review. Native C_e special onError retries deduplicate and decrement the ordinary retry index; optional Jrn apiAttemptsLeft is separate. The false max_retries+1 physical cap is removed. Current r51-75 physical-dispatch pair, zero-budget stream/nonstream thinking repair and retry-scope regression all passed (5 tests); independent review and full gate remain open. The JS trim FEFF/NEL boundary fix passed all 3 new regressions (r51-73). Actual max-output resume v5 is integrated: SDK facts plus explicit observation presence prevent stale false after terminal Unknown; route/model/refusal producers and incoming frozen-budget retry are preserved. Production compile passed; actual streaming resume integration passed after test lifecycle/observation fixtures were corrected (r51-73). Current host llm-runtime all-features library suite passed 1,392/1,392 on canonical SDK `161944a` (r51-78). |
| Explicit API attempt limit | Source confirms optional `Jrn.apiAttemptsLeft` is separate from ordinary retries: absence is unlimited; idle compaction explicitly supplies `{count:1}`. Harness has no equivalent operation-scoped total-attempt carrier yet; admission and first-dispatch observer do not implement it. | Confirmed remaining Source gap. Implement the explicit producer/consumer chain and test stream/nonstream plus shared compaction operation before claiming full alignment. |
| Betas | Cloud CLI additions now reach all four exact Native route header helpers. Bedrock model/body beta subset remains independent. AFK-only error text in the regression can correctly cause HQ/TBC retry; no Kie/AFK retry is implied. | Source-bound AFK nonstream test is applied. Streaming AFK-before-context/TBC overlap repair is applied with marker-correct regression fixtures. SDK fdf fixes Jw classifier. Execute actual runtime gate. |
| Models/Insights | Source model/carrier consumers, managed/Mods/classic model-switch dispatch and Insights are preserved in the checkpoint. | Main merge interface closure and current regression gates. |
| Commands/Skills/MCP/Mods | Existing source repairs and historical evidence retained; Bun fixed to 1.4.3 revision c6da4a4d3010e5553438c60f6bd76d981976867c. | Full current workspace/client/UniFFI gates and real package lifecycle checks. |
| Local App bug fixes | No Native parity counterpart. Current Tool API, nested validation/permissions, pre-approval target binding, bounded reads, UTF16 prompt and mobile runtime accessor are fixed. | 19 tool + 2 actual host integration tests passed (r51-73); 2 unchanged prompt tests reused (r51-70). Independent final source review passed. Final Product package/GUI remains open. |
| Delivery | SDK stage is validated and pushed; Harness/Product are not yet delivered. | SDK commit → Harness canonical pin/commit → Product canonical pins/locks/index, full checks, signed macOS package and GUI, then merge/push main without force. |

Evidence: shared `acceptance/turn-progress-r51-20261014/checkpoint.json`, `r51-48-root-session-context-integration`, `r51-48-v5-typed-config-integration`, `r51-49-root-effective-budget-integration`, `r51-50-harness-main-integration`, and task cache logs `r51-48` / `r51-49`. Detailed prior results are archived in [the preceding matrix](archive/current-matrix-before-r51-50-20261014.md).

## Scope and gate rules

- LLM communication remains in llm-client; current APIs only, no old compatibility branches.
- Preserve brand, multi-provider, Fusion and LingXi UI. Remote service and Local Apps Native parity are excluded; incoming main features are preserved. The user explicitly included Local App tool bug fixes and current Tool API integration. No new Computer Use implementation changes.
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
| CLIENT-1 | CLI, TUI, desktop and mobile callers preserve prompt, session, command, cancellation and transcript identity under current client protocol 25.0.0 (formal contract blessing passed; generated bindings pending); Local Apps Native parity is excluded; Local App tool bug fixes and current API integration remain in scope. |
| RUNTIME-1 | Bun worker startup/restart, filesystem/process/sandbox, env inheritance, cancellation/deadline, stdout/stderr and cleanup. |
| BUILD-1 | Three repositories resolve one candidate; dependency-complete checks, macOS production app and package, and required cross-platform checks. |
| BASH-INPUT-295 | Normalize Bash `tool_input.command_description` and `timeout_ms` aliases before `PreToolUse`, validation, and execution; preserve Native field priority and source units. |
