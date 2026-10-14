# Current Claude Code 2.1.295 alignment matrix

The fixed reference is Native **2.1.295**, SHA-256
`0116ee2e0a513900b633d9951367f18747686478e2b462805b8c31609f047f70`.
No newer release is in scope. Source agreement, meaningful regression, raw-byte
comparison and lifecycle constraints remain separate evidence classes.
Historical evidence is preserved in [the archive](archive/current-matrix-before-source-convergence-20261011.md).

## Current candidate

| Repository | Current candidate | Executed evidence and limits |
| --- | --- | --- |
| llm-client | `af45d354bfa91d736a0cb1cb6f89dcd9c0a90288`; integration and remote main verified. | This five-file SDK stage passed seven suggestion regressions, one raw error-response regression, formatting and all-features compilation under unchanged source. Historical e8d full suites remain historical evidence; they are not inherited as current runtime parity. |
| Harness | HEAD `cbb720052be8bf80370e6af70a972852d03a59bf` plus current source WIP; canonical SDK manifest and five lock entries point to af45. | Locked metadata passed for 748 packages and actual SDK checkouts match af45. Workspace all-target/all-feature compilation was interrupted at the 2 GiB disk floor with exit 130 before Harness diagnostics. No new Harness focused filters ran in that freeze. |
| Product | HEAD `280b9b764f16f3eb714fd3fc8d3cfcf63ea93904` plus WIP; includes remote main `32b5add1929293e8476b4f3924ef35d5eca02d0f`, preserving previous dirty bytes. | Bridge-client TypeScript build, 13 desktop state regressions and Electron node/web type checks passed. Final canonical pins, Rust/bridge snapshots, complete product gates and signed macOS acceptance remain open. |

The model-selection consumer batch (37 Harness / 26 Product files), Insights
binding, exact requested-model Hook payload, reset/qualification/timeout
corrections and nine-file request/Foundry repair are source-applied with guarded
pre/post hashes. These applications are not test passes. Live settings facts
for model suggestions and picker validation, plus managed → Mods → ordinary
classic model-switch dispatch, remain active source work.

Source agreement, meaningful regressions, raw-byte comparison and lifecycle
constraints are distinct evidence classes. The original 42 contract IDs below
are unchanged. No Native CLI was launched in this batch; broad historical
capture plans are not a requirement to repeat collection.

One Cargo runner owns Rust checks and Cargo-bearing frontend/package commands.
Compilation resumes only for a coherent source batch and adequate disk space.
Final acceptance uses one committed three-repository combination.

The tokenizer scope is one DeepSeek representative, as requested on
2026-10-14. Existing evidence is retained; no individual tokenizer matrix or
repeat `tokenizers-all` run is scheduled.

Current receipts in the integration acceptance root:
`sdk-model-validation-stage-r49-20261014/commit-receipt.json`,
`sdk-suggestion-error-carrier-focused-r49-02-20261014-01/receipt.json`,
`harness-sdk-af45-metadata-20261014-01/receipt.json`,
`harness-af45-workspace-all-features-check-20261014-01/freeze-end.json`,
`model-selection-v3-owned-consumers-applied-20261014/receipt.json`,
`model-selection-small-correctness-v5-applied-20261014/receipt.json`,
`model-switch-hook-source-r44-20261014/root-applied-receipt.json`,
`model-validation-foundry-r42-v4-20261014/root-applied-receipt.json`,
`insights-model-source-binding-r43-20261014/root-applied-receipt.json`,
`product-model-display-focused-r49-20261014/`, and
`product-main-privacy-refresh-20261014.json`.

Prior candidate narratives and executed totals are preserved in
[the pre-r50 archive](archive/current-matrix-before-r50-20261014.md).

## Source closure by domain

| Domain | Current source work | Remaining delivery |
| --- | --- | --- |
| SDK/request | Raw model Source request/attempt/codec pipeline and error-response/suggestion facade are committed at af45; provider routing stays separate. Harness request/Foundry repair is source-applied. | Focused af45 SDK checks passed. Live per-catalog post-overrides facts and current Harness behavior gates remain open; historical e8d full suites do not establish same-candidate acceptance. |
| Turn/placement | Staged first attachment batch uses a single selector/append and phase-aware cancellation. | The strict unknown-slash replay is not green: r46 reports `session_context=2`, `total_tokens=6`, `fallback=1`, `instructions=8`, `date=10`, `prompt=11`; only `total_tokens_before_fallback` failed. The r47 request-projection repair is source-applied and a fresh replay remains required; do not relax fixture/order assertions. |
| Tools/permissions | MCP drop permissions and Auto/Plan transitions are current. ToolSearch PFR/GCe and AQo changes have source-bound applied receipts. | Focused Tool API checks for filtered complete-wire membership and absent-name reannouncement each pass 1/1 in the r43 evidence set; Root reports focused PFR/GCe/AQo checks green. The r43 Runtime model-selection filter did not complete, and no full ToolSearch/permission gate is established. The dynamically imported `/model` handler’s argument validation remains separate and unverified. |
| Prompt/settings | Raw settings layers, UTF-16 language, forced-plugin output-style priority and source-generated Sandbox goldens wired. | Settings/Sandbox cold-path regressions pending. Skills accepted local-file and explicit CLI tiers are connected; Local MDM and source-specific parent/host policy producers are integrated; current Runtime compilation and execution acceptance remain pending. Remote service alignment is excluded by the user on 2026-10-12; the project will implement its own service separately. |
| Sessions/tasks | Raw absent/null envelope fields, explicit human provenance, live/fork/cold task delivery implemented. The ordinary/small-file X5e parser trim boundary is now source-bound and applied: FEFF is trimmed and NEL retained. | In r46, `route_lines_uses_native_javascript_trim_for_each_jsonl_row` and `typed_row_routing_matches_a_full_route_with_escaped_types` each passed 1/1; the current-consumer Agent pool filter passed 19/19. These are focused regressions, not complete session/Agent lifecycle acceptance. Background classifier and broader JSONL/cold-resume gates remain pending. |
| MCP | Failure/sanitizer/drop/retry scheduler and actual env proxy selection source-bound. | Shared proxyAuthHelper cache consumer, transport gates and current independent source/byte fixtures. |
| Commands/Skills | The Headless fixture uses a real user Skill input and strict catalog/order assertions. Native `ikn` fallback source is now byte-bound to the fixed catalog: 13 trigger rows and 12 target IDs; this is source evidence, not runtime parity. | Historical Headless replay 04/06 and Insights 22/22 remain the cited focused receipts. The new model-suggestion source map shows the SDK facade now has source-applied `fallback_3p` fields and typed suggestion facts, while Harness still needs the live per-catalog effective `hb()` config producer; cross-route model-suggestion integration remains open. The Native final response frame and Mods `skill.prompt` consumers remain focused evidence only; full dependency/runtime gates remain open. |
| Hooks/Mods | Raw durable versus request-only projection retained; Bun worker/UI source changes integrated. | Focused UPE tests pass 6/6; the nonempty `hook_success` durable/hot/cold lifecycle has focused batch 03 feedback and awaits a frozen-candidate rerun. Final Hooks/worker/runtime suites and signed package Mod UI/tool/command/restart acceptance remain open. |
| CLI/client/Product | Current provenance and Mod UI consumers are present; no old protocol compatibility branch. | Product pure TypeScript build and 82 focused bridge-client tests passed; two Bun revision guards passed. The broader npm test's snapshot import hit stale PinnedRuntimeSource and a denied Cargo fetch, so snapshot assertions and the final post-pin snapshot remain open. Full Rust/bridge/UniFFI integration, canonical final pins/index and CLI machine output fixtures remain open. |
| Platforms/integration | SDK af45 stage is committed and pushed; Harness locked metadata resolves the exact canonical checkout; Product includes the observed latest main. | Harness compilation stopped at the disk floor before source diagnostics. Same-candidate Harness/Product gates, final pins, FFI/platform targets and signed macOS static/runtime checks remain open. |

Allowed differences remain brand, multi-provider, Fusion and LingXi interface.
Local Apps behavior and remote service alignment are excluded; no Electron runtime migration. Remote endpoint, authentication, gateway/WIF, remote cache/consent and signature behavior are not final gates for this task. This exclusion does not remove local managed-file or macOS MDM policy coverage. CU implementation
belongs to the other session and is included in integration gates without removal
or suppression. No source-only or unexecuted item is recorded as byte/constraint passed.

## R47 bounded evidence corrections (existing IDs preserved)

These corrections are scoped evidence overlays for the original 42 contract IDs; they do not add or remove contracts. `source` means fixed-source binding, `focused` means only the named regression filter, `byte` means an actual raw-byte comparison, `constraint` records a known scope boundary, and `not-run` means no passing execution receipt.

| Existing contract ID(s) | Evidence correction | Class and boundary |
| --- | --- | --- |
| SESSION-4 | X5e’s ordinary/small-file parser now matches ECMAScript line trimming at the FEFF/NEL boundary; its two current focused Session filters passed. | `source + focused`; not a full JSONL/session lifecycle pass. Native executable was not launched for the r46 gate. |
| AGENT-1 | The current-consumer `agent::pool::tests` filter passed 19/19. | `focused`; does not establish full Agent, Orchestrator or Runtime lifecycle acceptance. Supersedes the stale earlier 17/19 pool result only for this exact filter. |
| TOOL-1, TOOL-2, MOD-2 | Filtered complete-wire ToolSearch membership and absent-name reannouncement each passed 1/1; PFR/GCe/AQo source patches are applied, and Root reports their focused checks green. | `source + focused`; full ToolSearch/permission gate remains `not-run`. The PFR/GCe/AQo apply receipts are source receipts, not by themselves a full gate. |
| TURN-1 | The strict unknown-slash replay still fails `total_tokens_before_fallback`; the fixture and assertion remain intact. | `byte fixture + failed focused replay`; current accepted result is failure, not a parity pass. A producer re-anchor fix and fresh rerun are pending. |
| PROMPT-2, SDK-3 | Model-source v2 / Native `ikn` facts are source-bound; the current source map records the per-route fallback/config chain and its dynamic override boundaries. | `source`; effective model suggestion and source propagation across all current providers remain unaccepted. A single family default is insufficient. |
| SDK-ERR-1, HOST-ERR-1, CLI-ERR-1 | af45 includes raw SDK error facts; the focused source-retention regression passed. Host and CLI production replay remain open. | `focused SDK`; not Host/CLI byte acceptance. |

Primary receipts: `r46-sessionx-native-replay-20261014-01/freeze-end.json`; `agent-pool-post-current-consumers-20261014-01/receipt.json`; `r43-focused-model-validation-diagnostic-20261014-01/{tool-api-view-filter.raw.log,tool-api-absence-filter.raw.log,freeze-end.json}`; `session-x5e-line-trim-r43-20261014/root-applied-receipt.json`; `tool-search-pfr-gce-cache-r32-v3-20261014/root-applied-receipt.json`; `source-alignment-295-r31/toolsearch-aqo-repair-v1-20261014/root-apply-receipt.json`; `source-alignment-295-r31/model-source-harness-audit-r37-20261014/root-applied-v2-receipt.json`; `source-alignment-295-r31/ikn-model-suggestion-fallback-catalog-20261014.json`; `native-replay-order-diagnostic-r46-20261014/root-applied-receipt.json`; and `source-alignment-295-r31/model-validation-error-carrier-delivery-20261014/`.

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
| CLIENT-1 | CLI, TUI, desktop and mobile callers preserve prompt, session, command, cancellation and transcript identity under current client protocol 25.0.0 (formal blessing and generated bindings pending); Local Apps dedicated behavior is excluded. |
| RUNTIME-1 | Bun worker startup/restart, filesystem/process/sandbox, env inheritance, cancellation/deadline, stdout/stderr and cleanup. |
| BUILD-1 | Three repositories resolve one candidate; dependency-complete checks, macOS production app and package, and required cross-platform checks. |
| BASH-INPUT-295 | Normalize Bash `tool_input.command_description` and `timeout_ms` aliases before `PreToolUse`, validation, and execution; preserve Native field priority and source units. |

## Comparison and scope rules

- Preserve original bytes where inputs are fixed: field absence/order, numeric
  and Unicode spelling. Never delete fields, reorder arrays or reserialize
  captured output to create an apparent match.
- Validate random IDs, time and process state by explicit format, references,
  ordering and lifecycle constraints; label them separately from byte passes.
- Preserve branding, multi-provider support, Fusion and LingXi presentation.
  Local Apps behavior is excluded. Do not clone Claude's TUI layout/ANSI/keys.
- LLM communication belongs to llm-client; current APIs only. Computer Use
  implementation belongs to the other session; no new CU edits are included.
- Historical Mods worker checks used Bun 1.3.11. Current policy-semantic worker
  evidence and Product staging pin Bun 1.4.3 revision
  `c6da4a4d3010e5553438c60f6bd76d981976867c`; matching production consumers and
  final package execution remain open. Electron runtime migration is excluded.
  Optional host capabilities are not inferred from installed tools.
- macOS requires the Flare development-channel wrapper and actual launch.
  Cross-compilation is not device or Linux/Windows runtime acceptance.
- Deliver committed SDK → Harness → Product pins after each stage passes. The user authorized main integration and main pushes on 2026-10-12. Refresh advanced main without force-pushing or discarding other work. No final completion claim while a known semantic or integration failure remains.

The compiled-in local `/cron` management handler is retained as an existing project extension. It is excluded from the Native prompt catalog only for its own Builtin/Bundled metadata; custom same-name skills remain visible. Its execution is not claimed to match a Native 2.1.295 `/cron` command. Native catalog prompt names match 13 entries in preserved earlier feedback. The separate 45-versus-41 result (`harness-catalog-count-diagnostic-02`) and missing-`skill_listing` Headless failure (`graphify-headless-replay-r31-07`) are historical pre-current-candidate receipts; current replay 04 passes catalog count 41 and exact prompt names at 64 MiB; replay 06 also passes in the original CI environment with explicit test-only thread allocation. Raw Native captures and strict assertions are unchanged.
