# Desktop subagent lifecycle: research and implementation report

Research date: 2026-09-12. Scope: desktop subagent liveness after process restart, cancellation, progress, and forked skills. The investigation combines official release/documentation sources, locally extracted JavaScript from the installed Claude Code binary, runtime logs, and the targeted implementation diff. Byte-level source anchors establish particular upstream contracts; they do not establish byte-identical implementation or protocol identity.

## Provenance and citation convention

- Binary: `/Users/luolingfeng/.local/share/claude/versions/2.1.269`.
- SHA-256, independently recomputed: `c942e1228b93cb4d52183b3dfbc77f28264f35aa947acd9c0853d029164cf450`.
- Embedded version metadata says `2.1.269`, build time `2026-09-11T17:33:46Z`, Git SHA `d0733697ad641a564c7cfcb19a6eb1eb9d61357e` [1]. The npm `latest` endpoint independently returned `2.1.269`; the official GitHub release page also identified v2.1.269 as Latest on the research date [14, 15].
- `@N` below is the zero-based byte offset in the extracted chunk, NOT a binary file offset. Local links select the containing source line; offsets make minified source unambiguous. Helpers: `~/.claude/oracle-chunks/find.py` and `ctx.py`.

## Outcome and compatibility scope

The investigated failure was a stale liveness indicator after an engine restart, compounded by a cancellation race that could leave the last persisted subagent status as running. The implementation now separates current process observations from saved transcript data, allows cooperative cancellation to settle before deallocation, and exposes available progress rather than always showing a generic Thinking placeholder. The report covers these lifecycle changes only; unrelated existing working-tree changes are excluded.

Claude Code v2.1.269 is a particularly relevant baseline: its release notes include fixes for background-agent running-state reporting and SDK/Desktop session status in agent lists [15]. Official documentation distinguishes foreground blocking from background concurrency, delivers background results through completion notifications, and treats API failure as a failed background task while retaining partial output [16]. These documented behaviors reinforce the local byte evidence without establishing an identical Desktop interface.

LingXi retains `SessionAgentList`/`SessionAgentUpdated`, its `SessionAgentSummaryDto`, and its `cancelled` display projection. It does not replace those DTOs with Claude Code's `background_tasks_changed` JSON schema. The aligned contract is that only current process facts can establish liveness; the precise upstream bytes are evidenced separately. Additional lifecycle features, foreground/background defaults, permissions, full retry wire fields, and task-panel retention timings are outside this fix.

## Observed incident timeline

The desktop runtime log and the affected subagent transcript establish the following sequence on 2026-09-12. Times below are PDT (UTC−07:00); the log stores UTC [17, 18].

| Local time | Observation | Evidence |
| --- | --- | --- |
| 03:43:31.031 | The subagent transcript records running, followed by its initial user message. | Transcript entries 1–2; no prompt contents reproduced. |
| 03:46:41.448 | The bridge receives a `ctrl-c` shutdown signal. | Desktop log line 180. |
| 03:46:41.795 | The child exits cleanly, code 0. | Desktop log line 181. |
| 03:47:59.392 | Desktop starts again. | Desktop log line 182. |
| 03:48:04.619 | The session is resumed. | Desktop log line 200. |

The inspected transcript contains only the running system record and the initial user message, with no terminal record. The log shows a clean process exit rather than evidence of a model still computing across restart. In combination with the old list implementation's use of saved running status, this supports the stale-status diagnosis. It does not identify why the initial inference produced no output before shutdown, nor establish that a provider request itself was stuck.

## Findings and implementation consequences

1. **Live background membership is a replaceable, process-scoped level signal.** The `background_tasks_changed` schema explicitly describes replace semantics, emission on membership changes, and reset to an empty set whenever the CLI process restarts. Re-initialization of an already-running process must send its current snapshot, including empty membership. Edge ordering is unspecified, so clients must not infer the live set by pairing start and finish events [2]. A historical `running` record is not evidence of a live task after restart. This directly supports fixing stale desktop Thinking indicators. It does not require rewriting historical transcript records to pretend they received a terminal event.

2. **The live roster derives from the current task registry.** `Bp` accepts `running` or `pending`, rejects explicitly foreground tasks, and `Rl` applies an additional exclusion predicate before serializing `task_id`, `task_type`, `description`, and optional `ambient`. The publisher compares previous/current membership, description, and ambient state; `X4e` emits an explicit snapshot [3]. A receiving consumer's `Moe` implementation clears its map on a snapshot, adds qualifying starts, and deletes notifications [4]. Resume of a saved transcript and reconnection to an existing live process are different operations.

3. **Cancellation publishes terminal state while aborting execution; it is not just removal of a UI record.** Local-agent `EM` calls the abort controller, stamps `killed`, `killedBy`, and `endTime`, clears keepalive reasons, abort-controller and selected-agent references, handles dependent work/notifications, and emits a settled signal [5]. The async runner's cancellation catch also performs cancelled lifecycle handling and enqueues a killed notification; cleanup runs in `finally` [6]. This supports allowing a cancelled runner to settle its persistence work before freeing it. No inspected bytes establish a specific grace-period duration, so a LingXi timeout is an engineering choice, not an upstream constant.

4. **Terminal notification is guarded per stop cycle, not forever per task id.** `N5` claims notification ownership and skips already-notified/missing tasks. Its payload explains that a task can notify again after the user resumes it with another message [7]. Completion clears execution references and records a result/end time, but may retain keepalive reasons for live child work [8]. Tests should cover cancellation/completion races without permanently suppressing later resumed-turn notifications.

5. **Progress and retry are distinct from liveness.** `task_progress` carries usage (`total_tokens`, `tool_uses`, `duration_ms`), optional last tool and summary; the schema tells clients to render a supplied summary regardless of task type. Live progress changes are guarded by `status === "running"`; transcript progress persists token/tool counts separately [9]. A progress event alone should not resurrect a task missing from the authoritative live registry.

6. **Regular-agent retry has an exact wire contract.** Internal `agent_api_retry` becomes `tool_progress` with optional `subagent_retry` containing `agent_id`, `attempt`, `max_retries`, `retry_delay_ms`, `error_status` (null fallback), and `error_category`. Recovery emits the same tool-progress envelope without `subagent_retry`; the internal retry latch clears on the next non-API-error event [10]. This is stronger evidence than interpreting all silence as model thinking.

7. **The previous claim that all ignored retry events diverge from Claude Code was too broad.** The synchronous forked-skill path forwards skill progress from assistant/user messages, while explicitly skipping several internal types and then filtering out non-assistant/user messages. It does not contain the regular-agent retry-forwarding branch [11]. Forwarding extra useful progress in LingXi may improve UX, but must not be advertised as exact upstream forked-skill behavior without further evidence.

8. **Saved subagent transcripts remain readable independently of live membership.** `m7t` loads agent transcripts and returns a map of agent ids to messages; it does not create running task registry entries [12]. Keep historical output available even when active membership is empty.

## Short byte contracts

These small exact source excerpts are sufficient to anchor compatibility assertions without copying the implementation:

```text
"background_tasks_changed"
"Every live background task after the change. REPLACE semantics: swap your set for this payload."
if(e.status!=="running"&&e.status!=="pending")return!1
if("isBackgrounded"in e&&e.isBackgrounded===!1)return!1
"task_updated"
["pending","running","completed","failed","killed","paused"]
```

The updated-task patch permits status, description, end time, total paused duration, error and backgrounding, and excludes execution objects/messages/result [13]. LingXi may retain its own public protocol shape; equivalence should be asserted against these lifecycle semantics and tested traces, not claimed as byte-for-byte protocol identity where field names differ.

## Regression matrix derived from the evidence

- Restart with historical start but no terminal record: no live roster entry or Thinking spinner; transcript remains readable.
- Reconnect to the same still-running process: its live task remains running after snapshot reconciliation.
- Snapshot replaces an older nonempty set with empty membership even if completion edge was lost.
- Stop returns terminal state and permits cancellation persistence to settle; a racing completion cannot restore running.
- Progress/retry arriving after terminalization does not restore liveness.
- A resumed agent can run and produce another completion notification under the same id.
- Retry start and recovery update display without affecting registry membership.

## Targeted implementation

**Process-owned roster and nonmutating history projection.** `DesktopSessionAgentObserver` now keeps current-process observations keyed by their owning session. The bridge reads these observations and the task registry after loading historical transcripts. Current observations, including persistent idle, take precedence over an earlier task snapshot or disk record. This prevents a delayed running task row from reviving a completed worker, and prevents a delayed parked row from hiding a genuine resumed run. A historical running/pending entry with no current execution is presented as cancelled/interrupted when the registry was successfully queried; if verification fails it becomes unknown instead. Listing leaves the JSONL bytes unchanged. A newly allocated child appears even before its first transcript append [19].

**Allocation and session-switch ordering.** A synchronous allocation receipt records ownership before asynchronous event binding. Cancellation can terminalize this receipt before binding; a late allocation callback checks the receipt and cannot recreate a running row. Updating the observer's default session moves future allocations while existing receipts retain their original owner. Late background events therefore remain attached to the session that created them [20].

**Cooperative persistent stop.** Persistent stop previously sent `UserExit`, performed cleanup, and deallocated without waiting for runner completion. It now gives the runner the existing bounded cancellation grace to write its cancelled terminal record and emit `Killed`, then falls back to hard cancellation and cleanup. The spawn gate stays closed throughout. The timeout reuses LingXi's existing grace constant; it is an adapted Rust scheduling mechanism, not a recovered Claude Code timeout [21].

**Observable background progress.** Persistent-spawn progress now reaches both the task consumer and the lifecycle observer. Desktop turns tool/token counters and retry reasons into activity text. These updates are accepted only while the observed agent is running, so delayed progress cannot bring a terminated child back to life. Retry text remains an adapted Desktop representation rather than a claim to emit all upstream `subagent_retry` fields [20, 21].

**Resume visibility before provider response.** Explicit input to a parked worker is published once through the existing typed message stream before its next provider query. A task-notification wake similarly publishes newly appended hidden inputs; Desktop exposes only the running transition for those messages, never their private content. This gives the current-process observer a real resume edge even when the next provider call stalls. Tests cover the stalled second request, notification wake ordering and duplicate suppression.

**Reliable lifecycle under observer saturation.** The observer handoff now uses one ordered, nonblocking FIFO. A semaphore retains the existing 100 queued-event limit for ordinary progress, assistant messages and tool-result messages. Allocation, terminal transitions and genuine user/hidden notification inputs enter the same FIFO reliably. Detached terminal-enqueue tasks were removed, eliminating a route by which a delayed completion could overtake a newer resume. The pressure regression fills the ordinary queue, verifies excess progress and bulky tool results are omitted, and verifies exact lifecycle ordering after the consumer resumes. This queue mechanism is a Rust implementation choice supporting the upstream lifecycle contract [23].

**Thinking fallback ownership.** The transcript row builder now accepts whether explicit pending activity exists. When such activity is present it suppresses only the synthetic fallback Thinking row. Actual reasoning rows and running tool/compaction rows retain their prior behavior. This avoids presenting available progress alongside a misleading generic wait indicator [22].

## State reconciliation cases

| Durable transcript | Current process observation | Earlier task snapshot | Desktop result |
|---|---|---|---|
| running | absent after restart | no matching worker | cancelled, with an interruption explanation |
| running | running | absent for foreground worker | running |
| absent before first append | allocation receipt | absent | running and inspectable |
| running | killed | running | killed |
| idle or running | idle after completion | running | idle |
| idle | running after resumed input | completed/parked | running |
| running | absent | registry lookup failed | unknown, without claiming the agent was cancelled |

Historical transcript bytes are unchanged in every listing case. LingXi keeps old rows visible for inspection; Claude Code's live background set excludes them. This is a presentation adapter distinction, not a copied wire format. A new process cannot infer that a historical agent is executing merely from its last persisted lifecycle word.

## Integration compatibility

The concurrent cron schema introduced an optional automation configuration while this fix was being verified. The legacy CronCreate initializer now supplies `automation: None`, preserving its previous serialized output because absent automation is skipped by serialization. The subagent fix adds no third-party dependency. Integration also retained the provider adapter handle needed by the concurrent migration path and classified the new scheduling messages as host-private, and placed scheduling event variants in `ClientEvent` rather than `AudioOpDto`; these compatibility changes are separate from the subagent lifecycle fix.

## Changed files

| Files | Purpose |
|---|---|
| `lingxi-code/agent/src/api.rs` | Reliable ordered lifecycle delivery, bounded ordinary telemetry, saturation regression. |
| `lingxi-code/agent/src/handle.rs` | Cooperative stop grace, persistent progress forwarding, real-runner tests. |
| `lingxi-code/agent/src/runner.rs`, `runner_test.rs` | Publish explicit and notification resumes before the provider call; preserve exactly-once input display. |
| `lingxi-code/apps/engine-desktop/src/session_agents.rs` | Process-owned observations, allocation/session fencing, atomic activity guards and observer regressions. |
| `lingxi-code/apps/bridge-server/src/boot.rs`, `router.rs` | Share the observer with the roster and reconcile historical rows with current facts. |
| `lingxi-code/apps/bridge-server/tests/router_test.rs`, `router_test/session_agent_liveness.rs` | Restart, reconnect, no-first-write, foreign-session, terminal and idle/resume regression cases. |
| `clients/electron/src/renderer/components/RuntimeCenter.tsx`, `Stage.tsx`, `transcriptRows.ts` | Display explicit activity without hiding true reasoning or active tools. |
| `clients/electron/test/runtime-center-render.test.ts` | Feedback, interrupted state and live-reasoning rendering regressions. |
| `lingxi-code/tools/cron/src/schedule_cron.rs`, `apps/engine-desktop/src/lib.rs`, `clients/electron/src/shared/clientCommands.ts`, `lingxi-code/client-protocol/src/events.rs` | Compatibility with concurrently introduced scheduling fields and retained provider ownership. |

The file references in sources 19–23 link to the principal implementation and regression files. Existing unrelated changes in these shared files were preserved.

## Validation status

The final targeted test runs passed 616 tests with no test failures:

| Verification | Result | Command / scope |
|---|---|---|
| Agent crate, including FIFO saturation, real stop persistence, progress and resume | 485 passed | `cargo test -p agent --lib` |
| Desktop observer, including hidden input and process/session ownership | 10 passed | `cargo test -p engine-desktop --lib session_agents::tests` |
| Bridge router integration, including stale task versus idle/resume observations | 69 passed | `cargo test -p bridge-server --test router_test` |
| Desktop renderer, runtime state/polling, command coverage and Electron startup interaction | 52 passed | Seven targeted Electron test files: `runtime-center-render`, `runtime-center-state`, `runtime-center-poll`, `transcript-agents`, `transcript-rows`, `welcome-startup-interaction`, and `client-commands` |
| Shared TypeScript declarations | Passed | `npm run build` in `clients/shared` |
| Desktop Node and renderer type checking | Passed | `npm run typecheck` in `clients/electron` |
| Electron production build | Passed | `npm run build` in `clients/electron` |
| Reference binary and bounded source excerpts | Passed | Binary SHA-256 plus all 18 offset/length/SHA-256 anchors |

The stop, restart and stalled-provider resume regressions failed before their corresponding fixes, then passed. The saturation regression verifies both nonblocking production and retained lifecycle order; no pre-fix execution result is claimed for that test. Runtime tests use controlled providers and temporary stores. Production transcripts were read for diagnosis and were not rewritten.

Static analysis passed with warnings: `cargo clippy -p agent -p engine-desktop -p bridge-server --lib --no-deps` completed successfully. Formatting checks on the targeted Rust files and `git diff --check` passed. The repository still reports documentation and style warnings; these were not suppressed or represented as a warning-free result. No live Anthropic request or signed packaged-app smoke test was run, and the currently running Desktop installation was not replaced. The scope is source-level lifecycle repair and verification, not a packaged-app release or a claim that all Claude Code wire bytes have been reproduced.

## Mechanically verifiable source anchors

The companion `subagent-oracle-2.1.269-anchors.json` contains 18 bounded byte anchors. Each includes its chunk filename, zero-based offset, byte length, SHA-256, and a short identifying needle. The binary SHA is included separately. No full extracted chunk or proprietary implementation is embedded in the artifact. Revalidation consists of checking the binary SHA, reading each indicated chunk slice, and comparing its hash and starting needle. These checks verify source identity; behavioral tests remain necessary to validate LingXi's adaptation.

## Limitations

The independently inspected logs establish process shutdown/restart and the absence of a terminal subagent record. They do not establish the cause of the original provider silence before shutdown. No live Anthropic API invocation was performed. Exact visual Thinking text, full task-management feature parity, and every skill/background path were not exhaustively verified. Byte-level anchors below support the stated behavior, not blanket byte-identical implementation claims.

## Primary local sources

1. [Embedded version metadata](/Users/luolingfeng/.claude/oracle-chunks/2.1.269/src_160105197.js:11), `VERSION:"2.1.269"` @833.
2. [Background task schema](/Users/luolingfeng/.claude/oracle-chunks/2.1.269/src_162223558.js:49), `Kde=f(` @572315; complete schema continues until `jde=f(`.
3. [Live predicate](/Users/luolingfeng/.claude/oracle-chunks/2.1.269/src_165238852.js:2381), `function Bp(e)` @3526568; [snapshot derivation/publishing](/Users/luolingfeng/.claude/oracle-chunks/2.1.269/src_174646004.js:15), `if(w.tasks!==P.tasks)` @84717 and `function Rl(w)` @89078.
4. [Snapshot consumer](/Users/luolingfeng/.claude/oracle-chunks/2.1.269/src_185748759.js:67), `function Moe(` @337420.
5. [Local-agent kill](/Users/luolingfeng/.claude/oracle-chunks/2.1.269/src_165238852.js:4320), `function EM(e,n` @5151724.
6. [Async cancellation handling](/Users/luolingfeng/.claude/oracle-chunks/2.1.269/src_165238852.js:2935), `fn("cancelled")` @4152235 and killed notification @4152800. The byte anchor is authoritative if extraction line layout differs.
7. [Notification claim/dispatch](/Users/luolingfeng/.claude/oracle-chunks/2.1.269/src_165238852.js:4315), `function N5(` @5149055.
8. [Completion transition](/Users/luolingfeng/.claude/oracle-chunks/2.1.269/src_165238852.js:4321), `function Hpr(e` @5154955.
9. [Progress schema](/Users/luolingfeng/.claude/oracle-chunks/2.1.269/src_162223558.js:49), `Jde=f(` @576702; [progress writer](/Users/luolingfeng/.claude/oracle-chunks/2.1.269/src_165238852.js:2875), `function yZe(` @4093534; [running guard/transcript counters](/Users/luolingfeng/.claude/oracle-chunks/2.1.269/src_165238852.js:4321), `function aEt(` @5153710.
10. [Retry wire conversion](/Users/luolingfeng/.claude/oracle-chunks/2.1.269/src_165238852.js:2841), `else if(e.data.type==="agent_api_retry")` @3988787; [retry resolution](/Users/luolingfeng/.claude/oracle-chunks/2.1.269/src_165238852.js:3100), `if(Up!==void 0` @4202157.
11. [Synchronous forked-skill forwarding](/Users/luolingfeng/.claude/oracle-chunks/2.1.269/src_165238852.js:3132), `Sn=s.options.forwardSubagentText` @4241415.
12. [Saved transcript loading](/Users/luolingfeng/.claude/oracle-chunks/2.1.269/src_165238852.js:5554), `async function m7t(` @5934135.
13. [Task patch schema](/Users/luolingfeng/.claude/oracle-chunks/2.1.269/src_162223558.js:49), `Vde=f(` @571832.

14. [Official npm latest package metadata](https://registry.npmjs.org/@anthropic-ai/claude-code/latest), accessed 2026-09-12; `_id` and `version` identify `2.1.269`.
15. [Anthropic Claude Code v2.1.269 release](https://github.com/anthropics/claude-code/releases/tag/v2.1.269), accessed 2026-09-12; release marked Latest, dated September 11.
16. [Official subagent documentation](https://code.claude.com/docs/en/sub-agents), sections “Run subagents in foreground or background” and “API errors in subagents,” accessed 2026-09-12.
17. [Desktop runtime log](</Users/luolingfeng/Library/Application Support/lingxi-code-desktop/logs/desktop.jsonl:180>), lines 180, 181, 182, and 200; UTC timestamps mapped to PDT above.
18. [Affected subagent transcript](/Users/luolingfeng/.lingxi/projects/-Users-luolingfeng-Projects-AGAI/e236a6cc-8c98-4a7d-a9de-7cf76d310d3c/subagents/agent-agent:bf19481a-52e0-4985-a31f-d989b2100ab7.jsonl:1), entries 1–2, inspected read-only; prompt contents intentionally omitted.
19. [Bridge roster reconciliation](/Users/luolingfeng/Projects/LingXi-Next/lingxi-code/apps/bridge-server/src/router.rs), `read_session_agent_summary` and session-agent listing; [roster regressions](/Users/luolingfeng/Projects/LingXi-Next/lingxi-code/apps/bridge-server/tests/router_test/session_agent_liveness.rs).
20. [Desktop observer](/Users/luolingfeng/Projects/LingXi-Next/lingxi-code/apps/engine-desktop/src/session_agents.rs), allocation receipt, snapshot, session owner, terminal, progress and retry handlers.
21. [Persistent spawn and stop](/Users/luolingfeng/Projects/LingXi-Next/lingxi-code/agent/src/handle.rs), `PoolSubagentSpawner::stop`, persistent progress forwarding, and corresponding regression tests.
22. [Thinking fallback rows](/Users/luolingfeng/Projects/LingXi-Next/clients/electron/src/renderer/components/transcriptRows.ts) and [Stage activity input](/Users/luolingfeng/Projects/LingXi-Next/clients/electron/src/renderer/components/Stage.tsx).

23. [Observer handoff and saturation regression](/Users/luolingfeng/Projects/LingXi-Next/lingxi-code/agent/src/api.rs), `ObserverEventSink` and `observer_saturation_preserves_lifecycle_fifo_without_blocking_producer`; [resume and notification input publication](/Users/luolingfeng/Projects/LingXi-Next/lingxi-code/agent/src/runner.rs).
