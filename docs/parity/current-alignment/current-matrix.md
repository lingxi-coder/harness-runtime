# Current Claude Code 2.1.295 alignment matrix

The fixed reference is Native **2.1.295**, SHA-256
`0116ee2e0a513900b633d9951367f18747686478e2b462805b8c31609f047f70`.
No newer release is in scope. Source agreement, meaningful regression, raw-byte
comparison and lifecycle constraints remain separate evidence classes.
Historical evidence is preserved in [the archive](archive/current-matrix-before-source-convergence-20261011.md).

## Current candidate

R31 remains fixed to Native 2.1.295. Main was merged into all three integration
branches before the current repairs. The published SDK `404fa7f...` is historical
stage evidence; the current SDK is `a9216bb901847831a58c488ac0a8ef72f1eee5c9`
and its independent gate queue is running. Harness and Product will be frozen
with canonical pins after the current worker repair. Full acceptance and
publication of this refreshed combination are pending. No Native capture is
part of this status.

| Repository | Current stage | Current state |
| --- | --- | --- |
| llm-client | `a9216bb901847831a58c488ac0a8ef72f1eee5c9` | Main integrated; Codex source carrier regression 5/5. The new 19-gate SDK queue must pass on this SDK source before publication; previous SDK-stage results do not establish this revision. |
| Harness | Integration branch `codex/claude-alignment-integration` | Main integrated. Current Codex SDK consumer regression 1/1. Skill prompt getter ordering, forwarded argument cloning, and core text-only result are repaired with real Bun worker feedback. Final freeze and full workspace/consumer/platform checks pending. |
| Product | `98f82e109509a0b0309d0684e5e09150e0125265`, before final pins | Main integrated; current bridge-client typecheck and 98/98 tests pass. Canonical pins must be updated to the final SDK/Harness commits and the index regenerated before full Product and signed-package gates. |

The exact final commits and gate fingerprints are recorded in sibling acceptance
receipts after committing, avoiding a self-referential documentation commit.

Receipts and source binding hashes are in the sibling integration
`acceptance/source-alignment-295-r31/`; they are not test runtime dependencies.
The independent audit preserves all 42 contract IDs below. R31 focused results
are repair feedback until the same committed dependency combination passes.

- Historical full-workspace feedback (not current-candidate acceptance): 19,491 passed, 43 failed, 7 ignored. Subsequent repair feedback is incomplete; no failures are waived and no full same-candidate workspace result is recorded here.
- Historical focused receipts (not the refreshed candidate): `harness-insights-sdk404-tests-02.json` passed 22/22 Insights tests and `harness-final-upe-tests-02.json` passed 6/6 UPE tests. Command-api skill dispatch passed 771/771 but not on the same whole candidate; Skill-batch 03 passed 5/5 in a focused run whose candidate changed during parallel edits; Runtime `mod_skill_prompt_tests` 04 passed 5/5 on an unchanged candidate. The nonempty `hook_success` durable/hot/cold path is covered by batch 03 focused feedback; it awaits a frozen-candidate workspace rerun.
- `headless-current-candidate-20261012-04` passes the unchanged strict replay through the actual production API normalization: four fixture cases, Graphify skill listing, exact ordered prompt catalog and count 41. Native captures and assertions are unchanged. Receipts 01/03 overflow at the existing 16 MiB CI stack; receipt 02 at 64 MiB exposed an obsolete pre-SDK/wire comparison, repaired before 04. Replay 06 passes in the original CI environment after explicit test-only thread allocation. These are focused semantic receipts; complete final gates remain pending. Newer replay 07 failed with missing skill listing on a drifting candidate; it is preserved as failed feedback. The frozen current candidate must be replayed before closure.
- The SDK 2.1.295 stage is now published at `404fa7...` after 19 gate receipts. Harness and Product final pins and full acceptance remain open.
- Flare signing preflight passed. No final signed artifact or packaged runtime acceptance is recorded.
- Frozen Harness queue 01 completed 20 gates with unchanged input: 14 passed; six failed (one test sentinel brand leak, workspace lib-test helper scope, Core Clippy and three mobile feature checks sharing a missing futures dependency). All reported causes have been repaired. Clippy repair feedback 09 passes every workspace target with warnings denied. The next full candidate gate batch is still required; no workspace tests were executed by queue 01 because compilation failed.
- Source review additionally closed inherited/class-getter `skill.prompt` text loss at the final JSON IPC boundary. All own JSON fields remain; the real Bun IPC regression passes three tests, including prototype text, getter text and own-extra preservation. This is focused evidence pending the full frozen candidate suite.

## Source closure by domain

| Domain | Current source work | Remaining delivery |
| --- | --- | --- |
| SDK/request | `Ta` is an exact raw-model-key scoped memo, not a host override. Provider route gate and defaults preserved. | SDK `404fa7f` is published after 19 stage gates; five process readers use Native nonempty-string truthiness. Focused Insights tests pass with the published SDK. Final same-candidate Harness consumer and Product pin/package gates remain open. |
| Turn/placement | Staged first attachment batch uses a single selector/append and phase-aware cancellation. | Production registry now retained in unknown-command replay; exact replay and lifecycle gates pending after the SDK pin. |
| Tools/permissions | MCP drop permissions and Auto/Plan transitions are current. | ToolSearch Native declared pool/latches and AQo retracted/restored/surfaced/absent fields; final permission gates. |
| Prompt/settings | Raw settings layers, UTF-16 language, forced-plugin output-style priority and source-generated Sandbox goldens wired. | Settings/Sandbox cold-path regressions pending. Skills accepted local-file and explicit CLI tiers are connected; Local MDM and source-specific parent/host policy producers are integrated; current Runtime compilation and execution acceptance remain pending. Remote service alignment is excluded by the user on 2026-10-12; the project will implement its own service separately. |
| Sessions/tasks | Raw absent/null envelope fields, explicit human provenance, live/fork/cold task delivery implemented. | JSONL source overflow preservation repaired; session 370 and compaction 280 passed in current focused feedback. Background classifier carrier decode repair awaits execution. Final committed candidate gates remain pending. |
| MCP | Failure/sanitizer/drop/retry scheduler and actual env proxy selection source-bound. | Shared proxyAuthHelper cache consumer, transport gates and current independent source/byte fixtures. |
| Commands/Skills | The Headless fixture uses a real user Skill input and strict catalog/order assertions. | Historical Headless replay 04 passes all four fixture cases, Graphify skill listing, ordered names and count 41 at 64 MiB; Replay 06 also passes in the original 16 MiB CI environment with explicit test-only thread allocation; final full gates remain pending. Insights focused regression passes 22/22. Source audit hashes 139 assets; the Bun 1.3.11 source-bound comparison matches 15 actions / 92,138 bytes, including two `render_report` outputs. The separate four-case renderer oracle is a different evidence class. The Native final response frame is implemented and consumer-wired through all-Prompt Qe header/meta/companion, pre-producer UPE and actual Runtime Mods `skill.prompt`; Runtime Mods focused regression passes 5/5. The q4 host boundary is the user-accepted remote/custom-host exclusion. Current consumers use `skill` with no legacy alias; Product current `skill` key is closed; final dependency SHA closure remains pending. |
| Hooks/Mods | Raw durable versus request-only projection retained; Bun worker/UI source changes integrated. | Focused UPE tests pass 6/6; the nonempty `hook_success` durable/hot/cold lifecycle has focused batch 03 feedback and awaits a frozen-candidate rerun. Final Hooks/worker/runtime suites and signed package Mod UI/tool/command/restart acceptance remain open. |
| CLI/client/Product | Current provenance and Mod UI consumers present; no old protocol compatibility branch. | Complete current Rust/bridge/UniFFI consumers, canonical pins/index, frontend build/test and CLI machine output fixtures. |
| Platforms/integration | SDK `404fa7f` is a published historical stage; the refreshed three-repo combination remains in the integration branches. Flare team preflight passed before this refresh. | Canonical pins, same-candidate three-repository full gates/package checks, available FFI/platform targets, and signed macOS static/run checks remain open. Untested platforms must remain explicit. |

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
- Deliver committed SDK → Harness → Product pins after each stage passes. The user authorized main integration and main pushes on 2026-10-12. Refresh advanced main without force-pushing or discarding other work. No final completion claim while a known semantic or integration failure remains.

The compiled-in local `/cron` management handler is retained as an existing project extension. It is excluded from the Native prompt catalog only for its own Builtin/Bundled metadata; custom same-name skills remain visible. Its execution is not claimed to match a Native 2.1.295 `/cron` command. Native catalog prompt names match 13 entries in preserved earlier feedback. The separate 45-versus-41 result (`harness-catalog-count-diagnostic-02`) and missing-`skill_listing` Headless failure (`graphify-headless-replay-r31-07`) are historical pre-current-candidate receipts; current replay 04 passes catalog count 41 and exact prompt names at 64 MiB; replay 06 also passes in the original CI environment with explicit test-only thread allocation. Raw Native captures and strict assertions are unchanged.

### Concurrent main refresh before final gates (2026-10-12)

SDK integration now includes main `e9b76f32261f2aee1f7c968d5bb099d915822e90`, retaining the published `404fa7f` fixes and adding Codex standalone search. Harness main `74507a79` and Product remote main `93eea0be` are being integrated before freezing final pins. Historical SDK404/queue03 receipts remain evidence for their recorded candidates; they do not establish that the refreshed combination passes.

Queue03 workspace tests stopped at linker/objcopy ENOSPC before executing tests. Seven recovery04 consumer/Runtime feature checks passed on unchanged pre-refresh source. Full workspace tests, final Product pins and signed packaging remain open. The sole diff whitespace report is faithfully retained Native source whitespace in `src_186578912.mjs:182`, not an implementation formatting error.

The refreshed SDK candidate is now `1092ee1bd357a64a391f4b11beb7dc9d18cf027f`, containing main `e9b76f3` plus formatting of three merged files. Harness contains main `74507a79`, preserves both source-alignment and Codex consumer changes, and resolves every SDK package to canonical revision `1092ee1`. Product contains remote main `93eea0be`; two conflicted test files were merged retaining both sides, with 59 focused tests passing. These refreshed candidates require fresh complete gates before main promotion; no earlier receipt is relabeled as a new-candidate pass.
