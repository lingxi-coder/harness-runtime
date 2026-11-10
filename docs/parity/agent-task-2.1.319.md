# Agent and task contracts: Claude Code 2.1.319

On November 9, 2026, the npm `latest` metadata resolved to **2.1.319**:
<https://registry.npmjs.org/@anthropic-ai/claude-code/latest>.
The installed official Darwin arm64 executable has SHA-256
`1f53587e92dee8e089fa349188a4cda64fa34ace6e7b0ca2ee278ddaa3cc2ebd`.
All four new oracles reject a different artifact and record the exact native
source ranges and their digests. They execute bounded native JavaScript in
isolated VMs; their imported host facts are explicit. No application bootstrap,
credentials, or model provider is used by these source oracles.

## Changes and independent expectations

| Surface | Native evidence | Production change |
| --- | --- | --- |
| Completed and background Agent results | Native result mapper: 108 completed, 18 background, two raw UTF-16 scenarios | Continuation hints follow the caller's admitted SendMessage capability. Empty output paths and worktree branches use native truthiness. Output validation retains the continuation flag and task-call records; async `isAsync` accepts only `true`. |
| Default report provenance | Native `LAr`/`KFn`/`Nyo`/`gQe`: 25 UTF-16/hash scenarios | Enable the native default report frame, indent every report line, verify head/tail attribution against the native section hash, and keep trusted notes distinct from the report. Hashing preserves native UTF-16 lengths and Node's lossy UTF-8 projection of isolated surrogates. |
| Background decisions | `Gr`: 384 local truth-table cases | Headless nested workers suppress implicit background execution. Background workers carry the owning session's interactivity separately from permission-prompt restrictions, so interactive-session descendants keep the native background default. Fork background capability can force asynchronous execution, with the inherited conversation, system sources and prompt policy preserved. Explicit fork worktree isolation retains the native worktree notice. |
| Effort | `ko`/`yr` metadata and `Pdr` coercion | Advertise the five absolute levels, omit effort under a valid global override, and drop relative values when the native relative-effort feature is disabled. Ordinary launches pass the chosen effort to the host; forks use the trusted parent effort snapshot. Main, nested and reused tool contexts carry that snapshot explicitly. |
| Turn-limit notes | 36 native producer cases | Suppress continuation guidance for callers without messaging tools; a zero turn limit does not manufacture an exhausted-limit note. |
| Names and descriptions | Native name refinements and JS whitespace rules | Reserve `user` and `system` as well as the existing reserved recipients. Description normalization preserves U+0085 and isolated UTF-16 units while trimming ECMAScript whitespace. |
| TaskCreate/Get/List/Update | 29 coercion inputs, 19 calls and six edited result projections using the native tool objects | Separate structured results from model-facing text; retain the created task description. TaskCreate/Get/List share pure UTF-16 renderers between normal calls and PostToolUse replacements. First-line subject derivation uses ECMAScript trim and UTF-16 word-cut positions. An empty owner does not send an assignment notification. |
| TaskStop | Three native result projections | A live-loop stop explanation becomes the result's `message`; it is not returned as a separate `note`. |
| Agent notifications | `U9t`, `Gi`, `Yt`: 17 scalar and one raw UTF-16 scenario | Preserve non-user/non-parent stop errors, apply native empty-value behavior, and XML-escape worktree branches. |

The Agent fixture is
`crates/tools/agent/tests/fixtures/agent_contract_319.json`.
Task and notification fixtures are under `scripts/tests/fixtures/`.
Fixture scenario counts are separate from Rust test counts. Metadata adapters
record native schema-builder declarations; they do not establish native schema
parser equivalence. Production tests additionally exercise the actual Rust
renderers, output parser, input coercers, state changes and effort carriers.

Existing description and child/reminder checks remain in
[tool-prompts-2.1.319.md](tool-prompts-2.1.319.md) and
[agent-reminders-2.1.319.md](agent-reminders-2.1.319.md).

## Scope and host differences

- LingXi branding, configured provider/model profiles, Fusion and the legacy
  implemented TaskOutput tool remain explicit product extensions. Deprecated
  native `team_name`/`mode` Agent inputs are not reintroduced.
- The relative-effort feature-service branch is retained in the oracle; this
  implementation uses the native disabled baseline. Remote/CCR launch is
  outside the local result comparisons.
- The headless check covers ordinary tool invocations. LingXi has no native
  `innerCall` carrier; this work does not invent one from model input.
- Notification fixtures use no pending messages or retained monitor/workflow/
  REPL work and default continuation capability. Native late-message/interim
  notifications and the notification-level continuation-disabled branch need
  additional typed lifecycle facts; they are not covered by these byte locks.
- These checks establish the enumerated local contract bytes and controlled
  state behavior. They do not establish complete Claude Code lifecycle parity,
  model quality, packaged Desktop/Mobile acceptance, or a full repository pass.

## Reproduction

```sh
node scripts/tests/agent_contract_319_oracle.mjs /path/to/official/2.1.319 --check
node scripts/tests/task_contract_319_oracle.mjs /path/to/official/2.1.319 --check
node scripts/tests/agent_task_notifications_319_oracle.mjs /path/to/official/2.1.319 --check
node scripts/tests/agent_result_framing_319_oracle.mjs /path/to/official/2.1.319 --check
python3 scripts/tests/agent_reminders_319_byte_check.py
python3 scripts/tests/tool_prompts_319_check.py
cargo test --locked -p tool-agent -p tool-task --lib -- --test-threads=1
cargo test --locked -p core --lib agent_notifications_match_native_319_artifact
```

Run the `tool-api` library tests and the `native_319` tests in `agent` and
`orchestrator` for parent-effort, session-mode propagation and PostToolUse
replacement coverage. The accompanying task report records the checks actually
completed in this checkout, including unrelated build blockers.

## Actual native CLI corroboration

An isolated native CLI session used a fresh home/config/workspace, a fake API
key and a loopback Anthropic server. It exited successfully after eight local
API requests. TaskCreate/Get/Update/List structured data and model text matched
source replay exactly; invalid TaskStop and a foreground Agent result also
matched. The Agent comparison used captured IDs and usage values without
normalizing them and executed the native default provenance frame. With no
messaging tool, the result correctly recorded `canContinueAgent: false`.

The original outgoing requests, CLI JSONL, source ranges and comparison receipt
are under `artifacts/agent-task-319-20261109/work/native-smoke/` in the parent
workspace. This corroborates native tool behavior; it does not execute a built
LingXi application or a real provider.

## Verification in this checkout

- Agent tool: **236 tests passed**; task tools: **205 tests passed**, using the
  final locked, offline combined Cargo run. The Agent test binary used reduced
  debug information to limit build disk use; execution semantics were unchanged.
- Fresh support test binaries passed **66** notification tests, all **310**
  tool-api tests, and **21** focused native-contract tests in the agent runner
  and orchestrator, including main/nested context propagation and PostToolUse
  replacements.
- All four pinned-artifact oracles, existing agent/reminder and tool-template
  byte checks, and the isolated native CLI smoke passed.
- Locked, offline runtime checks passed independently for minimal,
  Desktop and Mobile/UniFFI feature configurations. All three final checks used
  `CARGO_INCREMENTAL=0`; existing runtime/vendor warnings remain.
- Scoped Rust formatting and `git diff --check` passed. The workspace-wide
  formatting command also found existing unrelated formatting differences.
- A concurrent fallback-routing signature change required its call site to
  pass the owning conversation. The subsequent runtime checks passed.
- Earlier attempts waited for shared build locks or hit disk exhaustion. The
  only cleanup was this task's failed MCP incremental working directories.

## Review corrections

TaskCreate/Get/List now implement `Tool::map_result_text`, so echoed or edited
PostToolUse `updatedToolOutput` uses the native model text instead of JSON.
The same pure renderer handles normal calls and replacements; native fixtures
cover edits to IDs, subjects, descriptions, owners and dependency IDs, including
isolated UTF-16 surrogates, empty lists and missing tasks. A real orchestrator
dispatch test verifies three echoed results and six edited results against
these native bytes.

TaskList reads borrowed JSON views and UTF-16 slices, avoiding projection
bookkeeping sized to the full task arena for every row. Rendering stays linear
in the task count and rendered content. A standalone benchmark of the exact
before/after renderer functions with the cached dev projection library averaged
1,981 ms before and 37 ms after for 10,000 rows, with identical output across
100, 1,000, 5,000 and 10,000 rows. The native fixture probe also retained all
15 TaskCreate/Get/List result mappings exactly.

`ToolUseOptions` and `SubagentInvocationContext` now carry the trusted owning
session's interactivity separately from the effective permission-prompt flag.
Nested dispatch refreshes this fact when reusing tool context state. Main
dispatch uses `prompt_is_interactive()`, covering Bridge/SDK sessions that can
resolve permission prompts while retaining headless semantics. Background
children retain non-interactive tool approval behavior.


## Optimization audit follow-up

The eighteen follow-up findings are addressed without changing the native Agent
and Task result contracts. The durable inbox still reads legacy snapshots; new
receipts use an indexed, sequenced journal and amortized snapshots.

| Finding | Change |
| --- | --- |
| OPT-01 | Close the unused nested progress receiver before executing a tool, eliminating the eight-event stall. |
| OPT-02 | Validate and scan native spools in bounded chunks, keeping the requested window or capped tail and exact full-file line count. |
| OPT-03 | Retain repeatable worker-completion watches across canceled and timed-out Workflow/Fusion drains. |
| OPT-04 | Use independent linked-transcript locks, release the global map before I/O, and forward native byte reads through GuestFs. |
| OPT-05 | Poll the completion barrier without reading the spool; refresh output at completion/timeout and keep waiting if the agent resumes. |
| OPT-06 | Select the exact ordered Agent row without cloning all tasks; reuse its rendered transcript until its revision changes. |
| OPT-07 | Snapshot only touched SDK-visible rows and emit in insertion order; transcript and local-fact updates skip SDK projections. |
| OPT-08 | Skip owned query fields that nested dispatch immediately replaces with trusted current facts. |
| OPT-09 | Merge metadata with exact UTF-16 key indexes and one destination arena, retaining deletion and insertion semantics. |
| OPT-10 | Borrow legacy TodoWrite rows and construct the result once, preserving validation order and sparse-array errors. |
| OPT-11 | Borrow Mod-generated Agent arrays and import accepted fields once, preserving source units and array/property order. |
| OPT-12 | Scan separator-prefix candidates once while retaining the native anchored suffix rules and match spans. |
| OPT-13 | Stream workflow progress without retaining history; bound queued events and bytes with ordered, cancellation-aware backpressure. |
| OPT-14 | Index receipts and append durable journal records; pin root/file identity, roll back the written descriptor, recover raw torn tails, and suppress uncertain fallback delivery. |
| OPT-15 | Avoid listing the store when an ordinary claim has no dependencies; retain the explicit busy-agent scan. |
| OPT-16 | Gate assistant serialization at the producer using the live sink capability and decode private envelopes once; preserve activity, tombstones and legacy custom sinks. |
| OPT-17 | Own shared shutdown attempts, start independent stops concurrently, retain per-task exclusion/transfer fences, and keep failed cancellation owners for retry. |
| OPT-18 | Own persistent Stop across caller cancellation and release pool capacity before sequential MCP disconnection. |

The sanitizer's cached development probe at 4,000 separators improved from
1,517 ms to 29 ms. Native string/UTF-16 fixtures, generated differential cases,
exact match/insertion offsets and the public sanitizer path verify that the
scanner retains the existing rules. These are development measurements, not
release or packaged-application benchmarks.


The follow-up validation passed **2,325** source tests: Agent 753, Tasks 595,
Agent tool 240, Task tool 211, JSON projection 37, task store 34, tool API 312,
workflow 71, rooted filesystem 45, sanitizer 10, and platform filesystem adapters
17. The three independent runtime checks passed with no default features,
Desktop, and Mobile/UniFFI. Formatting checks passed for 56 Rust files, and
`git diff --check` passed.

Two Agent fixtures now supply explicit current policy/provider/model facts;
their original prefix, mode, tool-pool and environment assertions remain.
The pool-allocation rollback fixture waits for the terminal acknowledgment
under a deadline instead of a fixed count of scheduler yields. Mobile also
uses the current skill-name helper, matching Desktop's adapter.

These checks exercise source behavior on macOS. Packaged applications, native
Windows execution, physical devices and live providers were not exercised.
The four LingXi application output-sink opt-outs were formatted and inspected;
the LingXi application crates were not built in this pass.


## Scoped main delivery

The shared-checkout validation above includes independent in-progress prompt
work. Publication was prepared separately from main `0872be75`, excluding that
prompt migration and unrelated decision-service edits. The Agent fork gate
required by the native background contract is included. Native fork results
retain the exact parent system prompt on asynchronous as well as synchronous
launches.

The isolated candidate passed 2,096 tests across Agent (741), Tasks (593),
Agent tool (239), Task tool (211), and tool API (312), plus all three independent
runtime feature checks. A missing fork carrier is explicit in the nested-effort
refresh regression, and headless availability expectations use the Native
2.1.319 fork contract. Development source and excluded work remain in the
original checkouts; publication uses an explicit path manifest.
