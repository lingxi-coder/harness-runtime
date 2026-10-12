# Current Claude Code 2.1.295 alignment matrix

The fixed reference is Native **2.1.295**, SHA-256
`0116ee2e0a513900b633d9951367f18747686478e2b462805b8c31609f047f70`.
No newer release is in scope. Source agreement, meaningful regression, raw-byte
comparison and lifecycle constraints remain separate evidence classes.
Historical evidence is preserved in [the archive](archive/current-matrix-before-source-convergence-20261011.md).

## Current candidate

R31 is active under the user's resumed instruction. Six GPT-6 Luna max lanes
share the fixed sibling worktrees on `codex/claude-alignment-integration`.
Main was integrated at the initial fixed baseline. On 2026-10-12 the user authorized merging each completed stage into main and pushing main. Newly advanced main commits must be preserved and reconciled before a stage ships; they do not change the fixed 2.1.295 comparison target. No Native capture is part of this batch. Root serializes Cargo gates.

| Repository | Current committed baseline | Current state |
| --- | --- | --- |
| llm-client | `1da15cee0ffd21a083296969b5752165aa559889` | SDK stage merged and pushed to main; remote SHA confirmed. Current default 2601, tokenizers-all 2610 and all-features 2630 passed with 8 ignores each; full Clippy, format, deny-warning docs and five extracted production package checks passed. Harness/Product consumption still pending. |
| Harness | `664651414a905738739b289dab631a9b4866d220` | R30/R31 source changes present; manifest and all five SDK lock entries use canonical a0232b5. Current workspace checks passed before the newest interfaces; fresh SDK pin and full gates remain pending. Final source freeze, full tests/Clippy and commit pending. |
| Product | `fd688f71f2eafc1161dede8535fd7bfd6038b5d9` | Source WIP and current consumers; baseline Rust pins remain. Electron 1542 pass/2 skip and TypeScript checks are frontend-only evidence before final pins. |

Receipts and source binding hashes are in the sibling integration
`acceptance/source-alignment-295-r31/`; they are not test runtime dependencies.
The independent audit preserves all 42 contract IDs below. R31 focused results
are repair feedback until the same committed dependency combination passes.

- SDK default: 2587 passed/8 ignored; tokenizers-all: 2595/8; all-features: 2617/8. Accidental vendor native-TLS workspace profile failure is retained separately from the selected rustls production/CI profile.
- Core settings: 151 passed; permission: 1773 passed; MCP: 688 passed. Later source changes invalidate these as final-candidate receipts.
- Orchestrator02: 1914 passed/7 failed. Exact renderer, fixture/identity and fallback interleave repairs are being validated; no failures are waived.
- Graphify07: actual Headless replay fails because its replacement orchestrator uses an empty tool registry, suppressing `skill_listing`. The fixture's pre-existing user Skill provenance is established, 21 raw files are repository-local, and strict catalog/order assertions remain.
- SDK check04: all targets/all features passed on unchanged source, before the final real CONNECT regression batch.
- Electron typecheck04: passed on unchanged source; full tests require localhost-capable execution.
- Flare signing preflight passed. No final signed artifact or packaged runtime acceptance yet.

## Source closure by domain

| Domain | Current source work | Remaining delivery |
| --- | --- | --- |
| SDK/request | `Ta` is an exact raw-model-key scoped memo, not a host override. Provider route gate and defaults preserved. | Source-confirmed proxyAuthHelper setting, host callback/cache and SDK typed407 retry/header chain; final SDK gates/commit. |
| Turn/placement | Staged first attachment batch uses a single selector/append and phase-aware cancellation. | Production registry now retained in unknown-command replay; exact replay and lifecycle gates pending after the SDK pin. |
| Tools/permissions | MCP drop permissions and Auto/Plan transitions are current. | ToolSearch Native declared pool/latches and AQo retracted/restored/surfaced/absent fields; final permission gates. |
| Prompt/settings | Raw settings layers, UTF-16 language, forced-plugin output-style priority and source-generated Sandbox goldens wired. | Settings/Sandbox cold-path regressions pending. Skills accepted local-file and explicit CLI tiers are connected; Local MDM and source-specific parent/host policy producers remain under implementation and must not be counted as complete policySettings parity. Remote service alignment is excluded by the user on 2026-10-12; the project will implement its own service separately. |
| Sessions/tasks | Raw absent/null envelope fields, explicit human provenance, live/fork/cold task delivery implemented. | Native internal opt-in/external level belongs to CCR worker metadata; local JSONL/fork begin opted out. Full current session/compaction/agent gates pending. |
| MCP | Failure/sanitizer/drop/retry scheduler and actual env proxy selection source-bound. | Shared proxyAuthHelper cache consumer, transport gates and current independent source/byte fixtures. |
| Commands/Skills | Graphify is an isolated real Skill input; production catalog is not fabricated. | Strict actual Headless fallback replay pending. Source-confirmed Skills listing budget/override/usage/display-name chain is being repaired; current consumers now use the `skill` implementation crate without a legacy alias. |
| Hooks/Mods | Raw durable versus request-only projection retained; Bun worker/UI source changes integrated. | Final Hooks/worker/runtime suites and signed package Mod UI/tool/command/restart acceptance. |
| CLI/client/Product | Current provenance and Mod UI consumers present; no old protocol compatibility branch. | Complete current Rust/bridge/UniFFI consumers, canonical pins/index, frontend build/test and CLI machine output fixtures. |
| Platforms/integration | Fixed local three-repository baseline; Flare team preflight passed. | Same committed triple full gates, available FFI/platform targets, signed macOS static/run checks. Untested platforms must remain explicit. |

Allowed differences remain brand, multi-provider, Fusion and LingXi interface.
Local Apps behavior and remote service alignment are excluded; no Electron runtime migration. Remote endpoint, authentication, gateway/WIF, remote cache/consent and signature behavior are not final gates for this task. This exclusion does not remove local managed-file or macOS MDM policy coverage. CU implementation
belongs to the other session and is included in integration gates without removal
or suppression. No source-only or unexecuted item is recorded as byte/constraint passed.

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
| CLIENT-1 | CLI, TUI, desktop and mobile callers preserve prompt, session, command, cancellation and transcript identity under the main-integrated client protocol 24.0.0; Local Apps dedicated behavior is excluded. |
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
- Mods use Bun 1.3.11 in the current worker checks. Electron runtime migration
  is excluded. Optional host capabilities are not inferred from installed tools.
- macOS requires the Flare development-channel wrapper and actual launch.
  Cross-compilation is not device or Linux/Windows runtime acceptance.
- Deliver committed SDK → Harness → Product pins after final gates. No push,
  no merge back to main, and no final completion claim while a known semantic
  or integration failure remains.
