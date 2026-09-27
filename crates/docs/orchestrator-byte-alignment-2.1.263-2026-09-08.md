# Orchestrator re-audit against Claude Code 2.1.263

Date: 2026-09-08. Baseline commit: `1e4bf3d87`.

## Reference and scope

Installed binary, npm latest, and the official changelog all resolve to
2.1.263. Binary SHA-256:
`ef5d2909c8af49f31ab6d5487e90316777bc2fac170adfe8160716caa8aaf4f9`.
Extracted source is `~/.claude/oracle-chunks/2.1.263/` (1651 chunks).
Inventory: 122 Rust source files under orchestrator/src; 81 production
candidates (60,507 lines, including inline tests). Inventory is not a claim
that every path was executed or independently proved equivalent.

Independent reviews covered tool assembly/dispatch, turn/stream termination
and retry, prompts/reminders, resume/thinking/history, persistence, and hooks.
Fixes cross into shared state/providers only where required by those paths.
Existing unrelated working-tree changes were preserved.

LingXi branding, multi-provider routing, unsupported Cowork/remote surfaces,
and the repository's existing opt-in feature policies remain product differences.
This audit does not advance the repository-wide Claude version pin.

## Corrections backed by the executable/source oracle

| Surface | Oracle evidence | Correction |
|---|---|---|
| Coordinator pool | `src_177951083.js` `Idr`/`M6e`; `src_160256736.js` qbt | Base qbt includes StructuredOutput and Skill; add PR suffixes, MCP comms, brief tools, configured extras matched by canonical/V1/family names. |
| Outer tool assembly | terminal `src_180597926.js` computeToolPool; SDK `src_178069082.js` li | No plan/user-question post-append. SDK can append StructuredOutput. X2 WebFetch append is unreachable while coordinator mode is active. |
| Worker redirect | main `Ldt` ~3891812; `dt("external")` | Separate pool enablement from simple-mode suffix suppression. Require enabled external-worker membership, current Agent, and tool-wide permission eligibility; preserve Brief fallback. |
| Malformed/thinking-only retry | main Oer ~4259812–4261200; `src_158021603.js` ZZe | Exact clean retry nudge; remove rejected attempt from next model request; only consecutive malformed failures exhaust retry. |
| Terminal states | Oer and streaming terminal arms | Batched stop_sequence/missing stop reason terminate; streaming error transcript entries carry API-error envelopes. |
| Truncated recovery sources | main tZo/ji | Interactive main sessions stop after the partial notice; headless main and subagent sessions recover. Source classification and nudge selection share the same predicate, including hook_agent. The local subagent alias remains a compatibility extension. |
| Resume payloads | main JDo/Eht/nNo | Reject malformed hook arrays wholesale; skip malformed text blocks without losing valid siblings. |
| Thinking strip | main mce/uhr/wys/CCt ~5378770 | Store rejected identities and thinking indexes; respect marker order and partial ranges; do not strip newly generated thinking. |
| Durable retry exclusion | `hu.evict → Y5e → performRemoveByUuid` | Physically delete rejected JSONL rows, preserve unrelated raw bytes, repair parent links and append cursor; emit identity/retraction events through desktop, TUI and mobile. |
| Goal timing | main aJn/iJn ~4096160 | Detect new task batches using base interval, then apply/reset backoff. |
| Goal task state | n3t/r3t | Preserve authoritative teammate idle events internally; idle teammates no longer defer goal evaluation; active ones do. |
| Goal impossible | main Stop consumer ~4137648 | Propagate internal impossible result and end goal as failed, not achieved. |
| Silent reminders | Ffs/jfr/WG; `src_157893357.js` model catalog | ExitPlanMode/SendUserFile count as speaking; Fable/Mythos5.1 model defaults and explicit overrides match. |
| StructuredOutput | `src_160256736.js` | Exact description and prompt; passthrough-object native input stage and dynamic AJV-style validation inside the call, before capture. |
| Persisted results | `src_160701526.js` tG/_7e; PIn/OIn | UTF-16 sizing/preview, safe existing symlink replacement, ancestor checks, reject nonregular/hardlinked collisions. |
| Prompt hooks | main ppr/Wps and Stop evaluator | Event-specific exact prompts, bounded transcript, JSON output schema, disabled thinking, overflow retry, timeout. |
| Schema errors | main Qbn/zue; bundled Zod 4.4.3 | Executed 28 fixtures for diagnostics, ordering and JS numeric formatting; separate advertised/runtime schemas and wire native refinement issues into Agent, AskUserQuestion and Workflow. MCP uses its actual native passthrough-object runtime schema. |

The `Idr` function was executed locally from the extracted chunk against
ordinary/brief/extra-tool matrices, rather than inferred from a prompt.
Qbn/zue were also executed against diagnostic fixtures. Rust regression
expectations were checked against those outputs.

The prior claim that qbt was unrelated to coordinator assembly was wrong.
Its use as the *entire* pool was incomplete; removing it from pool assembly
was also wrong. The assembler, not the coordinator prose, is authoritative.

The repository-only doctor telemetry check also had an obsolete 416-entry
count: all 19 current registry blocks sum to 410, matching the existing
fixture. The diagnostic and duplicate count/order tests now agree. This is
not a claim that the complete telemetry catalog matches Claude.

## Initial audit verification

All commands completed successfully against the modified workspace:

| Check | Result |
|---|---|
| `cargo test -p orchestrator -p llm-runtime -p session -p hooks -p tasks --lib -- --test-threads=1` | 3,080 passed: hooks 474, llm-runtime 855, orchestrator 1,095, session 264, tasks 392 |
| `cargo test -p orchestrator --tests --no-fail-fast -- --test-threads=1` | 1,428 passed across 81 suites: includes the same 1,095 library tests plus 333 integration tests |
| telemetry event-name completeness and settings-schema tests | 21 passed |
| Clippy for the five affected crates, library + tests, `--no-deps` | No errors; warnings remain |
| Final orchestrator Clippy, library + partial-finalize regression target | No errors; warnings remain |
| `cargo check -p engine-desktop --lib` | Passed |
| `rustfmt --check --edition 2021 --config skip_children=true` | Passed for 46 scoped Rust files |
| `git diff --check` | Passed |

That is **3,434 distinct passing tests**, excluding the repeated library run.
Tests ran serially because existing test fixtures use process-global flags.
No live authenticated Claude API calls or signed desktop packaging were run.
Compilation used an isolated target directory to avoid another task's build
lock; low concurrency and disabled incremental caching resolved disk-space
failures. Temporary verification caches were removed after completion.

The added partial-finalize regression distinguishes interactive stop-after-
notice from headless recovery. The old fixture used headless defaults while
asserting interactive behavior; production recovery was not disabled to make
that assertion pass.

## Changed implementation areas

- `orchestrator/src/conversation/{wiring,tooling,hooks,model,reminders,transcript}.rs`
  and `conversation/drivers/mod.rs`: pool policy, scoped recovery, hook and
  reminder integration.
- `orchestrator/src/{turn_loop,streaming_executor,resume,hook_prompt_runner,
  provider_adapter,tool_result_persistence,structured_output,schema_validation,
  stop_hook_snapshot,diagnostics}.rs`, prompt modules and regression tests.
- Shared seams: `core/src/session.rs`, `llm-runtime/src/{protocol,service}.rs`
  and thinking normalization; `session/src/jsonl/{reader,loader,writer}.rs`;
  hook request/response definitions and prompt executor; platform task/goal
  metadata and model capabilities; task idle state propagation; tool metadata.
- Telemetry count/order tests and this report plus the corrected prior audit.

No dependencies were added. The main simplifications remove the permanent
thinking-strip boolean as the active policy, remove rejected retry context,
and make reminder/dispatch decisions reuse tool eligibility instead of
independent registry-presence guesses.

## Non-remote follow-up

The requested follow-up closes the previously listed non-remote gaps:

- **UTF-16:** preserve exact sliced code units through result persistence,
  JSONL, cold resume and Anthropic request/count-token encoding, including a
  lone surrogate at the preview boundary. Reuse the existing exact-string wire
  override; the internal sidecar never appears in provider requests. Other
  provider codecs consume the display fallback. Size labels use JS rounding.
- **Windows collisions:** open the existing path without following reparse
  points, and inspect regular-file status and link count on the same handle.
  Reuse the existing platform API handle metadata implementation.
- **Native validation:** production refinement callbacks retain native issues;
  runtime schemas are distinct from advertised schemas. Zod fixtures come from
  executing the bundled 4.4.3 implementation, including numeric key ordering,
  UTF-16 lengths and JS number rendering. Agent names reuse the existing NFKC
  normalization through the production spawner. MCP foreign schemas remain
  advertisements, matching H4 rather than enforcing extra local validation.
- **Rejected attempts:** physical JSONL removal replaces the local tombstone
  record. Exact message identities support transient retraction in desktop,
  terminal scrollback, Android and iOS without removing unrelated messages.
- **Hooks:** live main/worker snapshots replace persisted-history reads;
  parent SubagentStop consumes the correct child snapshot. Grouping retains
  virtual and resumed-thinking metadata. Native 1M models use the existing
  model capability calculation.
- **Thinking recovery:** request-owned query scopes isolate workers sharing a
  service and inherited parent IDs. Sidequeries fork state. Durable markers
  are recorded before retry; cancellation followed by cold resume retains the
  rejected ranges without duplicate markers. Scope wrappers box large futures
  before awaiting, preserving the default test-thread stack.

Key follow-up files include `protocol/src/js_utf16.rs`,
`llm-runtime/src/thinking_scope.rs`, `orchestrator/src/{structured_output,
schema_validation,tool_result_persistence,turn_loop,provider_adapter}.rs`,
`orchestrator/testdata/schema-validation-2.1.263.json`,
`session/src/jsonl/writer.rs`, `hooks/src/{executor,prompt_executor}.rs`,
`agent/src/{runner,transcript}.rs`, `platform-api/src/rooted_fs.rs`,
`client-{adapter,protocol}` event handling and the desktop/TUI/mobile reducers.

No dependencies were added. Existing wire-string, rooted-filesystem, model
capability, tool and transcript interfaces were extended instead of adding
parallel implementations.

### Follow-up verification

| Check | Result |
|---|---|
| Cross-layer `--tests`: agent, llm-runtime, hooks, session, protocol, client-protocol, client-adapter, tool-agent, tool-ui, tool-workflow, tool-mcp | 3,414 tests; initial two missing additive contract baselines corrected and both suites rerun successfully |
| Orchestrator library and integration tests | 1,447 tests across 81 suites; six event-order assertions in five suites updated for additive identity metadata and rerun |
| Desktop and mobile engine `cargo check --lib` | Passed |
| Clippy for the 12 cross-layer crates, library and tests, `--no-deps` | Passed with warnings; no errors |
| Scoped Rust format check and `git diff --check` | Passed |
| TUI / tui-core / client-adapter libraries | 1,575 passed, one pre-existing ignored test |
| Desktop conversation reducer | 48 passed |
| Android mapper/reducer | 111 passed |
| iOS simulator | 54 passed |
| Shared SDK build and Electron node/web typecheck | Passed |
| Windows platform-api and orchestrator library cross-compilation | Passed, `x86_64-pc-windows-gnu` |

Contract regeneration adds the two message event goldens and their four
index rows. It also captures the already-present additive
`TaskRowDto.awaiting_plan_approval` field; no existing contract entry changed.
The broader and UI rows overlap in client-adapter tests and must not be added
as a distinct-test total.

## Remaining scope and evidence limits

Remote-agent execution and its long-running goal-deferral branch remain
excluded at the user's request. This follow-up does not change that surface.

Windows code is cross-compiled for `x86_64-pc-windows-gnu`; no Windows runtime
was available. Tests and executable-oracle fixtures establish the covered
contracts, not a formal proof for every possible schema, provider and host.
StructuredOutput call-stage diagnostics are covered; arbitrary invalid schema
construction/strict-mode error wording is not claimed byte-identical.
No live authenticated Claude API calls or signed desktop packaging were run.
