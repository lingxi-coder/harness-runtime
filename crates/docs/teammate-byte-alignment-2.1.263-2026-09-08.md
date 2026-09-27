# Teammate alignment — Claude Code 2.1.263

## Execution / cleanup plan

The approved scope replaces explicit TeamCreate/TeamDelete with the session's implicit team, preserves persistent teammate spawning and mailbox delivery, and aligns Agent routing and collaboration text with the installed 2.1.263 executable. Remove old registrations, permissions, prompts, and tests without legacy aliases, version branches, data migration, or deleting user data. Preserve existing worktree changes and reuse the registry, spawner, and mailbox seams; add no dependencies.

Before implementation, run existing teammate/Agent tests. Add independently extracted oracle fixtures, then change the tool surface, runtime, and host registrations in separate owned lanes. Verify new contracts and lifecycle behavior, then run affected crate tests, checks, formatting, and Clippy. Record failures and unverified UI/backend behavior explicitly.

## Oracle provenance

- Executable: `~/.local/share/claude/versions/2.1.263`.
- SHA-256 (verified 2026-09-08): `ef5d2909c8af49f31ab6d5487e90316777bc2fac170adfe8160716caa8aaf4f9`.
- Extracted production chunks: `~/.claude/oracle-chunks/2.1.263/`.
- Fixture: `test-harness/src/parity/fixtures/teammate_2_1_263.json`.
- This oracle was inspected directly; the older TypeScript mirror was not used to generate expectations.

## Finite contract coverage

The new test driver compares Agent schema descriptions with oracle-extracted literals. The fixture also retains exact production source slices for Agent schema/dispatch, SendMessage schema, and lifecycle field definitions, with chunk SHA-256, UTF-8 byte offsets, and independently verified executable byte offsets, so implementation reviews can distinguish evidence from assumptions. These source slices are provenance, not claims that the corresponding runtime behavior has been tested.

The latest Agent schema retains `team_name` and `mode`, both explicitly ignored. Its nested-teammate error says the roster is flat, and in-process teammates reject background subagents. Latest SendMessage has optional summary defaulting to the first message line with truncation at 200; cross-session addresses are names with optional listing refs, and its schema depends on feature gates. The existing session-UUID interface is not evidence for that newer contract.

The implicit initializer (`src_183125516.js`) uses `session-${sessionId.slice(0,8)}`, with an existing-team / once-consumed internal environment override. It moves the current session task directory to the team task directory when the names differ, reserves the first color for the leader, and starts subsequent color assignment at index 1. This initializer is retained in the fixture as source evidence.

## Limits and verification

A baseline `cargo test -p test-harness --test parity_agent_task_tools --no-default-features` was started before creating the new contract; both baseline and new-driver runs waited on the shared build lock and were stopped to let the parent run consolidated verification. Neither queued run establishes test success. `rustfmt --check` passes for the new driver; all eleven recorded source excerpts were found byte-for-byte in the executable. Wire ordering, live model execution, backend behavior, shutdown races and UI rendering require separate evidence beyond this initial fixture. Later sections record those tests and their limits; source presence and passing schema tests cannot establish complete byte-level alignment.

Task-list lookup evidence: `zE` selects explicit task-list env, in-process team context, process team context, host-owned leader team, then session ID. The backing `ds()` resolves `m.of(B().host)` in `src_159610765.js`; a process-global Rust singleton does not preserve isolation when multiple independent hosts/sessions share one process. Both task tools and reminder providers must resolve the corresponding session-owned team.

## Session task isolation implementation

Replaced the process-global leader-name API with session-keyed registration and lookup in `platform-api/src/team_registry.rs`; removal accepts `None` for one session only. Task tools and reminder lookup retain their existing override precedence but consult only the calling session at the leader-name level, with no legacy global fallback. Tests now use actual session contexts and cover two independent leaders, teardown without affecting the other session, matching leader/teammate lists, and override precedence. No dependencies were added. The session-isolation unit tests and the task/reminder integration checks pass in the consolidated verification.

## Reproducible pane-worker smoke

**Passing worker/engine test:** `scripts/teammate_worker_smoke.py /tmp/lingxi-teammate-target/debug/deps/cli-c7979e8104ee6cc5 --test-harness --output /tmp/lingxi-worker-engine-smoke-verified --timeout 120` completed successfully. The script runs the exact ignored CLI test `teammate_worker::unix::tests::offline_worker_engine_smoke` in a controlling PTY, using the existing explicit fixture-storage setting under `cfg(test)`. It exercises the same worker/engine implementation without changing production credential policy.

Verified: authenticated Hello, Ready, a real model-originated SendMessage RPC, one terminal approval for that synthetic operation, replay of the parent response into a subsequent model request, actual first idle notification, parent message wakeup, a new model request, wake-response output followed by its completed event, shutdown exit 0, and consumed launch manifest. There were three loopback model requests, 1,501 output bytes, and zero rejected external HTTP attempts. The child inherits no user credentials/configuration; HOME/config/cwd are isolated and the HTTP proxies reject external destinations.

The wake completion assertion uses the actual completed output event after the unique wake-response marker. A fast turn can finish entirely between the child's 100 ms status snapshots, so an unchanged second idle snapshot can be deduplicated. The first idle frame, parent wake, new model request and completed wake response are all mandatory; no output normalization is used. Evidence: `/tmp/lingxi-worker-engine-smoke-verified/report.json`, `frames.json`, `requests.json`, and `terminal.log`.

**Production CLI limitation:** the standalone development CLI passed terminal initialization and authenticated Hello, then stopped at native credential initialization because it lacks the signed credential broker. No production storage policy was changed and no broker was mocked. Evidence: `/tmp/lingxi-worker-smoke-integration-2/report.json` and `terminal.log`. That production-CLI scenario remains blocked before Ready/model execution; the passing harness is not a packaged production launch or a tmux/iTerm2 pane-creation test.

The smoke caught and drove fixes for unsupported Darwin `/dev/tty` kqueue registration and the missing runtime idle-status publication before the final passing run.

## Implemented behavior and affected files

- Tool surface: removed `tools/team/`, coordinator `tool_team_create.rs` / `tool_team_delete.rs`, their workspace dependency edges, permission defaults, prompts and old parity fixtures. Desktop default/coordinator and mobile registries assert both names are absent. No old-data migration, aliases for the removed tools, version dispatch or user-data deletion was introduced.
- Agent dispatch: `tools/agent/src/agent.rs`, `apps/engine-desktop/src/background_agent.rs`, `platform-api/src/subagent_spawn.rs`. Keep the published `team_name` and `mode` schema while ignoring these inputs; distinguish named normal agents from enabled implicit teammates, preserve cwd/isolation/model inheritance and upstream validation order.
- Implicit runtime: `coordinator/src/implicit_team.rs`, `team_registry.rs`, `teammate_pump.rs`, `status_sink.rs`, `tasks/src/handlers/in_process_teammate.rs`, `tasks/src/registry*.rs`, `platform-api/src/team_registry.rs`. Session-owned team/task lookup, member registration, shared task list, mailbox startup, transcript context, cancellation-safe activation and terminal-state replay use the existing registries and spawner.
- Messages: `tool-api/src/send_message_contract.rs`, `tools/ui/src/send_message.rs`, `coordinator/src/tool_send_message.rs`. Shared gate-dependent schemas and prompt, required fields, first-line summary, actual delivery message IDs, self/named-sender checks, ordered shutdown and plan approval envelopes.
- Plan approval: `platform-api/src/teammate_plan.rs`, `tasks/src/handlers/teammate_plan.rs`, `tools/plan/src/plan_mode.rs`. Correlated trusted responses control worker permissions through the existing gate. Inline plan input remains supported because it is present in 2.1.263. Pending approval is independent of idle/running lifecycle, rejects stale/forged responses and survives early linking.
- Terminal workers: `apps/engine-desktop/src/pane_teammate.rs`, `apps/cli/src/teammate_worker.rs`, `platform-api/src/teammate_worker.rs`, POSIX `swarm/`. Authenticated private Unix transport, bounded startup/buffering, model/message/control/status relay and backend ownership. Backend settings are read at spawn time; only automatic backend acquisition failures fall back, explicit/launch failures propagate. Darwin PTY input uses cancellable nonblocking reads.
- Hosts and presentation: Desktop `lib.rs`, CLI `init.rs` / `mode.rs`, bridge `boot.rs` / `server.rs`, `client-adapter/src/lowering.rs`, `client-protocol/src/listings.rs`, TUI and `tui-core`, Electron `runtimeCenterState.ts` / `RuntimeCenter.tsx`, shared protocol and snapshot tests. Leader idle wakeups, cancellation handles, worker roster events, task rows and `awaiting approval` rendering are connected.
- Incidental verification fix: CLI `commands/agents.rs` replaces a single-iteration labeled loop in an existing test with an equivalent labeled block to satisfy Clippy `never_loop`.

No dependencies were added by this teammate change. Other concurrent worktree changes, including Electron Markdown dependencies, are outside this delivery. Existing unrelated modifications were retained. Branding, config/home and environment mapping exceptions remain intentional.

## Verification evidence (2026-09-08)

The isolated Rust 1.82 target is `/tmp/lingxi-teammate-target`; evidence logs are under `/tmp/lingxi-teammate-263`. These are execution artifacts, not portable oracle inputs.

- `final-stable-libs.log`: 1,158 tests passed across coordinator (111), tasks (387), tool-agent (161), tool-api (211), tool-plan (19), tool-task (143), tool-ui (126), including final idle/event/color/hook propagation changes.
- Final CLI worker tests: 14 passed, 1 external-driver test ignored by default; that ignored test passed through the smoke driver. The incidental CLI labeled-block regression also passed.
- Final host regression filters: 14 teammate/pane/hook/TUI tests and 9 approval/lowering/TUI tests passed (`final-host-teammate-tests.log`, `final-host-approval-tests.log`).
- Final coordinator activation integration: 6 passed (`final-activation.log`). The completed scripted turn is asserted Idle, while actual model-call and client-event checks retain the negative control against metadata-only creation.
- Earlier integration passes: desktop coordinator activation 6; pane transport 5; desktop registry 1; mobile registry 1; bridge idle wake 1; task reminder isolation 2; permission defaults 13; CLI worker 11; TUI/TUI-core teammate behavior 7.
- Final oracle/parity rerun passed: Agent/task 9, registry 4, teammate description 1, exact TeammateIdle serialization 1 (`final-parity.log`). Earlier telemetry 4 and Tengu 3 passed. Shared SendMessage wire contracts cover all four feature-gate combinations, seven shutdown cases and nine plan frames.
- POSIX backend tests: 18 detection/iTerm-command/argv cases, plus a real tmux create/pane/destroy integration on an isolated socket. A live iTerm2 GUI launch has not been exercised.
- Desktop runtime render/state tests (25), shared protocol snapshots (20), shared TypeScript check and Electron web typecheck passed.
- Desktop production-component screenshot: `target/teammate-visuals/desktop-plan-approval.png`; inspected for roster lifecycle and pending label. `.omx/state/teammate-alignment/ralph-progress.json` records `reference_missing` with no score/pass. There is no upstream screenshot comparison and no interactive TUI screenshot acceptance claim.
- Final all-target Clippy passed for coordinator, tasks, tool-agent, tool-ui, tool-plan, engine-desktop, CLI, bridge-server, platform-posix, TUI and TUI-core; warnings remain (`final-stable-clippy.log`).
- All-target `cargo check` passed for engine-desktop, engine-mobile, CLI, bridge-server, client-adapter, TUI and TUI-core (`final-check.log`).
- `git diff --check` passes. Whole-workspace `cargo fmt --check` still reports unrelated existing/concurrent differences in fusion, orchestrator/turn_loop and permission files; no teammate-owned formatting differences remain.
- Pre-change baseline was attempted but never completed because of the shared build lock. Passing post-change tests must not be described as a successful pre-change baseline.

These assertions establish the covered contracts, not universal byte identity. Source excerpts alone do not prove runtime behavior; untested backend/host branches and finite byte coverage remain explicit acceptance limits.

## Final oracle corrections and remaining differences

The final review added ordered optional `senderColor` / `targetColor` routing fields from actual member registrations, removed the unsupported `agent:<uuid>` recipient alias, and emits routable bare UUIDs for normal host subagents. Teammate `name@team` IDs are unchanged. The seven-case standalone shared-contract suite passes, including independently captured optional-color order.

`test-harness/tests/parity_teammate_idle_2_1_263.rs` compares serialized payload bytes with outputs produced by executing upstream `Sa` + `E_n`, with binary/chunk SHA-256 and byte offsets. TeammateIdle now serializes base fields before event/name fields, uses the owning session's bare UUID, carries the live permission mode and omits tool-context-only agent ID/effort. Repeated idle transitions no longer manufacture new session identities.

Known differences / unverified branches:

- Normal host subagents still use UUID identities rather than upstream `a...` lexical IDs. This is not a claim of byte parity for normal Agent IDs.
- Direct `uds:` / `bridge:` cross-session transports are unavailable in these host implementations; local teammate routing is the verified scope.
- TeammateIdle optional `prompt_id`, scratchpad path and ambient agent type are serializer-tested but not supplied by the current firing scope. Full optional-context propagation is not established.
- The unsigned production CLI cannot complete credential initialization without its signed native broker. The isolated-storage ignored test harness validates the worker engine separately and must not be presented as a signed production launch.
- Live iTerm2 GUI execution, an interactive TUI screenshot and an upstream visual reference comparison remain unverified. Desktop uses existing components, not an emulation of terminal layout.

No global version bump or legacy-data migration was performed. Consequently this delivery must not be described as complete end-to-end byte identity across every Claude Code host/feature combination.

## Code review follow-up

The ten review findings were addressed together with cross-boundary regressions:

1. Accepted shutdown owns an independent task lifetime, so cancellation of the responding teammate cannot interrupt task unassignment and the departure notification.
2. Coordinator SendMessage resolves ordinary agent names and task aliases through the shared mailbox. Reserved session/UDS/bridge address syntax is validated before alias lookup.
3. A registered teammate gets a call-local permission and file-containment allowance for its exact plan file. Explicit ask/deny rules, frozen denies, other files/agents, links, and permission-input rewrites remain checked.
4. The raw UUID returned by ordinary Agent resolves to its actual task for output and stop operations.
5. Initial launch status cannot overwrite a handler-observed Idle state.
6. Desktop maps coordinator Working to its existing Running state before rendering and transcript polling.
7. Detailed tool invocation preserves structured result data and independent model-facing prose through the registry, deferred/lease/plan wrappers and agent runner.
8. Parent IPC lock acquisition and writes are bounded; stop proceeds to backend termination and private-directory cleanup after cooperative notification fails. Status polling cannot be starved by continuous output.
9. A newly split tmux pane is rolled back if subsequent styling or border initialization fails.
10. Transcript list refreshes preserve live coordinator rows, retain approval state, and cannot revive terminal workers.

Commit scope is verified using a temporary checkout assembled from the selected index. Unrelated permission, cron, fusion and formatting hunks remain in the original worktree; they are not prerequisites for the selected teammate changes.

Follow-up validation in the selected checkout:

- 2,103 core library tests passed: platform-api 301, tasks 388, tool-api 213, tool-file 374, tool-plan 20, agent 404, coordinator 116, tool-agent 161 and tool-ui 126.
- Seven pane tests passed, including blocked writer and socket backpressure; tmux post-split rollback and deferred model-content/lease forwarding regressions passed.
- Runtime Center state/render tests: 26 passed; Electron node/web typechecking passed.
- All-target Clippy passed for the affected tool, runtime and host crates, with existing warnings. The whole-tree format check still reports inherited formatting differences deliberately excluded from this commit.
- All-target `cargo check` passed for Desktop, Mobile, CLI, Bridge, client-adapter, TUI and TUI-core in the selected checkout.
- The selected CLI binary passed 14 worker tests and the strict external smoke driver: authenticated startup, message RPC/permission/result replay, idle/wake/completion, shutdown exit 0 and consumed manifest. Evidence: `/tmp/lingxi-worker-engine-smoke-candidate-final/` (three local model requests, no external HTTP attempts).
- Evidence logs use the `review-fix-` prefix under `/tmp/lingxi-teammate-263/`.

## Second review follow-up

The second review fixes cover eight runtime and presentation failures:

1. TUI model-turn start events and cancellation-token registration now occur after acquiring the shared host turn gate. Local slash commands execute without that gate and preserve the active model turn and its cancellation token.
2. External pane tasks route public TaskStop through their owning backend. Failed or timed-out termination preserves task ownership and a retryable status; successful termination marks the teammate killed. Failed startup cleanup also exposes a stoppable task instead of losing the pane.
3. An incomplete parent JSONL write poisons the connection, including cancellation during a write. Subsequent frames cannot reuse a partially written stream; teardown remains bounded and retryable.
4. The team registry captures the owning runtime's configuration home. Shutdown task unassignment and team-member removal use that same root instead of re-reading ambient environment variables.
5. Team-file read/modify/write operations share the existing cross-process lock and publish through atomic rename. Member and metadata updates preserve concurrent changes and unknown JSON fields.
6. Pane workers resolve project settings, MCP, providers and plan paths from the manifest working directory before constructing the engine. Project-scoped boot settings do not change the process working directory.
7. Child output and RPC writes have bounded, cancellation-aware lock acquisition and writes. The RPC deadline includes writing, partial frames poison the connection, and dropped requests release their pending reply entries.
8. Desktop reconnect and matching session-resume events clear stale coordinator overlays before rebuilding the live roster, while retaining transcript tabs.

No dependencies or legacy compatibility paths were added. The earlier byte-parity and live-backend verification limits still apply.

Files changed for this follow-up (other pending worktree changes are outside this fix):

- `clients/electron/src/renderer/bridge/runtimeCenterState.ts`, `clients/electron/test/runtime-center-state.test.ts`.
- `lingxi-code/apps/cli/src/init.rs`, `mode.rs`, `teammate_worker.rs`.
- `lingxi-code/apps/engine-desktop/src/lib.rs`, `pane_teammate.rs`, `lingxi-code/apps/engine-desktop/tests/coordinator_activation.rs`.
- `lingxi-code/coordinator/src/implicit_team.rs`, `team_file.rs`, `team_registry.rs`, `tool_send_message.rs`.
- `lingxi-code/tasks/src/registry.rs`, `registry_test.rs`, `lingxi-code/tui/src/chat_widget.rs`, and this report.

Second follow-up validation used the current worktree, including preserved unrelated edits; it is not a selected-commit checkout:

- Coordinator: 119 passed; tasks: 389 passed after correcting the teammate killed-state transition.
- CLI worker: 19 passed, one external-driver test ignored by default; explicit project-directory and TUI host-turn regressions: three passed.
- Pane backend/transport: nine passed, including failed startup cleanup and public TaskStop retry. Coordinator activation integration: six passed.
- TUI chat widget: 152 passed. Desktop Runtime Center state/render: 28 passed, including reconnect/resume reset tests; Electron web TypeScript check passed.
- The strict worker-engine smoke driver passed authenticated startup, message RPC, idle/wake/completion and shutdown exit 0. It made three local model requests and no external HTTP attempts; evidence is `/tmp/lingxi-worker-engine-smoke-second-fix/`.
- Rust evidence logs use the `second-fix-` prefix under `/tmp/lingxi-teammate-263/`. The initial tasks run found one failed regression assertion; the subsequent complete 389-test run passed. The Desktop commands were captured in the agent tool transcript, not those log files.
- All-target `cargo check` passed for coordinator, tasks, Desktop, Mobile, CLI, Bridge and TUI. All-target Clippy passed for coordinator, tasks, Desktop, CLI and TUI; warnings remain. Scoped Rust formatting and `git diff --check` passed.

## Third review follow-up

- Shutdown approval no longer treats a failed backend stop as confirmed departure. The tool reports the stop error while preserving team membership and shared-task ownership; a successful retry performs departure cleanup. The approval frame still records acceptance independently of termination.
- Pane task/output allocation occurs before external pane creation. An allocation error therefore leaves no pane to recover; later launch failures reuse the reserved task for retryable cleanup.
- A child's terminal report is published only after confirmed pane cleanup. Failed cleanup leaves a running task accepted by the public TaskStop guard. Status publication uses the registry's authoritative terminal result when completion races with a user stop.
- Pending slash dispatch has its own cancellation and completion ownership, separate from an active teammate turn. Follow-up prompts remain ordered behind expansion, and session switching cancels pending work.
- Normal submissions and slash prompts share a synchronous submission sequence. Delayed enqueue publication cannot hide an earlier input, and a queued turn inherits its owner's cancellation. Both between-turn draining and the actual mid-turn input consumer apply the same ordering rules and retire consumed sequence metadata; local control commands remain immediately responsive.

This follow-up changes `coordinator/src/tool_send_message.rs`, `apps/engine-desktop/src/pane_teammate.rs`, the CLI/TUI slash dispatch path, their regression tests and this report. No dependencies or legacy behavior branches were added. Existing unrelated changes remain in the worktree.

Third follow-up validation used the current worktree:

- 370 targeted tests passed: coordinator 120, pane lifecycle/transport 11, TUI widget 156, CLI mode 57, worker/config 20, and coordinator activation integration six.
- The shutdown regression was first run against the broken implementation and failed because the tool returned success after failed termination; the final coordinator suite passes.
- Independent review caught mixed-input ordering and a second mid-turn queue consumer. Both have production-path regression tests in the final CLI suite; the final bounded recheck found no further confirmed blocker in those paths.
- Strict worker-engine smoke passed authentication, Ready, message RPC, idle/wake/completion and shutdown exit 0, with three local model requests and no external HTTP attempts. Evidence: `/tmp/lingxi-worker-engine-smoke-third-fix/`.
- Scoped Rust formatting and `git diff --check` passed. Evidence logs use the `third-fix-` prefix under `/tmp/lingxi-teammate-263/`; an initial test-mock compile error was corrected before the final CLI suite.
- All-target `cargo check` passed for coordinator, Desktop, CLI, TUI, Mobile and Bridge. All-target Clippy passed for coordinator, Desktop, CLI and TUI; warnings remain.

These tests do not add live iTerm2, signed production CLI, or upstream visual-reference coverage; the earlier stated verification limits remain.

## Fourth review follow-up

- CLI queued-input ownership is now immutable for the host session. Cancellation cleanup and both consumers retire ordering metadata without deleting the cancellation token, so an in-flight snapshot or dequeue cannot reinterpret canceled user input as a fresh unowned teammate message. These records are released with the session queue state.
- Approved departure data is recorded after approval delivery and before attempting termination. A later confirmed pane stop completes the stored team-file removal, task unassignment and termination notification. The original response path and retry paths share an owned, once-only completion operation; completed entries remain in the session registry for idempotency. No callback or registry reference cycle was introduced.
- Changes are in `apps/cli/src/mode.rs`, `coordinator/src/team_registry.rs`, `coordinator/src/tool_send_message.rs`, `apps/engine-desktop/src/pane_teammate.rs`, their tests and this report. No dependency or legacy compatibility path was added.

Fourth follow-up verification uses the current worktree:

- 218 targeted tests passed: coordinator 121, CLI mode 59, pane lifecycle/transport 12, worker/config 20, and coordinator activation six.
- Two deterministic queue tests suspend the real mid-turn consumer or dequeue while cancellation cleanup runs. They verify that canceled input cannot return and genuinely unowned teammate input still works.
- The pane integration test delivers a real socket Shutdown frame, disconnects the simulated child, observes failed teardown without file/task mutation, then retries through the public task-registry handle. It verifies successful departure cleanup and no duplicate notification after concurrent/repeated completion.
- Previously verified CLI/coordinator binaries also passed their 57/120-test baseline suites; these are prior-build baselines rather than freshly compiled pre-edit worktree tests.
- Strict worker-engine smoke passed startup, RPC, idle/wake/completion, manifest consumption and shutdown exit 0, with three local model requests and no external HTTP attempts. Evidence: `/tmp/lingxi-worker-engine-smoke-fourth-fix/`.
- Logs use the `fourth-fix-` prefix under `/tmp/lingxi-teammate-263/`. The existing live-backend, signed-production and upstream-visual verification limits remain.
- All-target `cargo check` passed for coordinator, Desktop, CLI, TUI, Mobile and Bridge. All-target Clippy passed for coordinator, Desktop and CLI; warnings remain. Scoped Rust formatting and `git diff --check` passed.

## Final retry correction and commit verification

Departure completion now retains per-step progress instead of caching the first attempt in `OnceCell<()>`. Team-file, task-list, lock, read, write and notification errors propagate. Successful task updates survive retries in an accumulated result; a single accurate termination notification is sent only after the required steps succeed.

The strict task-store departure API acquires each file lock before rechecking ownership and completion. It returns committed partial progress on failure and atomically replaces task files, so failed writes cannot truncate the remaining ownership record. Existing general task CRUD behavior is outside this change. Pane teardown retains its public TaskStop route and running status through departure I/O failures; a confirmed backend termination is not repeated on retry.

The selected commit contains 19 teammate-related files from these review follow-ups. It excludes concurrent cron, permission, coordinator-mode dispatch, unrelated orchestrator and formatting changes. Verification ran in `/tmp/lingxi-teammate-final-check`, assembled from the selected Git index rather than the full working tree:

- 825 tests passed: coordinator 123, task-store 32, task registry/handlers 389, CLI mode 59, worker/config 20, pane transport/lifecycle 12, coordinator activation six, TUI widget 156 and Desktop state/render 28.
- Regression cases include team-file lock recovery, partial task-unassignment recovery with accurate accumulated notification, ownership changes while waiting for a task lock, atomic-write failure, and public TaskStop retry after the backend is already terminated but departure I/O failed.
- Electron web TypeScript checking passed. Strict worker-engine smoke passed startup, RPC, idle/wake/completion, manifest consumption and shutdown exit 0, with three local model requests and no external HTTP attempts; evidence: `/tmp/lingxi-worker-engine-smoke-commit-final/`.
- Selected-source formatting checks report only three inherited `tasks/src/registry_test.rs` differences deliberately excluded from this commit. `git diff --check` passes. Initial test-fixture and test-only import issues were corrected before the final selected-source suites passed.
- Evidence logs use the `commit-candidate-` prefix under `/tmp/lingxi-teammate-263/`. Live iTerm2, signed production CLI and upstream visual-reference coverage remain unverified as described above.
- All-target `cargo check` passed for coordinator, task-store, tasks, Desktop, CLI, TUI, Mobile and Bridge. All-target Clippy passed for coordinator, task-store, tasks, Desktop, CLI and TUI, with warnings. Its first attempt exhausted disk space; clearing only this task's temporary build cache and rerunning without incremental compilation completed successfully.

## In-process departure retry follow-up

Approved departure completion is now connected to the generic task-registry stop path through a weak, session-owned cleanup interface. Backing execution is stopped before cleanup is attempted. A failed cleanup retains its existing progress and can be retried even after the in-process handler has removed its worker and the task is Killed.

TaskStop allows a terminal task only when it is an `in_process_teammate` with a registered, unfinished approved departure. After cleanup succeeds, the normal terminal-task rejection applies again. This follow-up preserves the actual Killed status and adds no TaskRecord fields, general terminal-task stopping exception or dependency.

Handler-spawned in-process stops are serialized per task. The backing stop and its owned lock run independently of the caller, so cancelling a Stop cannot let a second Stop release shared work before execution has exited. Normal completion retains the lock until routing teardown is committed. External pane controllers remain outside this lock because their fallback can re-enter the registry.

Changes are in the platform task/spawn interfaces, task-registry implementation and adapter, coordinator cleanup interface implementation, Desktop composition root, TaskStop and their tests. This follow-up is separate from commit `50b9b11ad` and preserves unrelated working-tree edits.

The final current-worktree test runs passed 979 tests: platform-api 301, tasks 392, coordinator 123, tool-task 144, coordinator activation 7 and pane teammate 12. The new integration uses the real in-process handler, pool, runtime and scripted model: it stops a worker, forces departure I/O failure, repairs the file, invokes the actual TaskStop tool, checks membership/task cleanup and one notification, then confirms another terminal Stop is rejected. A deterministic blocking-handler regression covers concurrent stops and cancellation of the first caller before exit. Independent review found no new confirmed defect in the changed retry and owned-stop paths. Logs use the `inprocess-retry-final-` prefix under `/tmp/lingxi-teammate-263/`.

All-target `cargo check` passed for platform-api, tasks, coordinator, tool-task, engine-desktop, CLI, engine-mobile and bridge-server. All-target Clippy passed for platform-api, tasks, coordinator, tool-task and engine-desktop, with repository warnings. Focused rustfmt checks and `git diff --check` passed. The separate Desktop root-file rustfmt check reports an unrelated existing `with_coordinator_mode` formatting difference, which this follow-up leaves intact. An earlier Clippy attempt hit an unrelated provider-adapter call to a test-only accumulator; that call was repaired in the concurrent working tree before the final checks.
