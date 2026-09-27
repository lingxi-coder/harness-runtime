# /loop audit against Claude Code 2.1.270

## Reference and meaning of alignment

On 2026-09-12 `npm view @anthropic-ai/claude-code version` returned **2.1.270**. The installed binary was 2.1.269, so this audit downloaded the official `@anthropic-ai/claude-code-darwin-arm64@2.1.270` package rather than treating the installation or older repository fixtures as latest.

Native binary SHA-256: `a506b6d970a4cf44f6abdb53a81ddcd5d3b0ce042a95c502fe9d1f946bdb8807`.
Package integrity: `sha512-cfSXE9L21yhJilj8+PCpoV3jYNcS3/lnTOZDKoS27LAh+Un4HqyGecQWDXddDwL4TKPR/2Q9CyCjFqIFnSXSXg==`.

Embedded JavaScript was extracted at `/tmp/lingxi-loop-oracle-2.1.270/chunks`. Public documentation is supporting context, not the byte oracle: [scheduled tasks](https://code.claude.com/docs/en/scheduled-tasks). Different published documentation versions describe different expiration periods; this audit uses the shipped binary's seven-day default.

“Byte equal” below refers to the named deterministic strings/fixtures, not a claim that the Rust executable, UI, native FFI, storage envelopes, or account-specific behavior are identical to Claude Code.

## Findings and changes

| Surface | Confirmed correction / coverage |
| --- | --- |
| `/loop` prompt selection | Seven-day recurrence copy; bounded Monitor instructions; explicit-input activation; preload must not activate; JavaScript whitespace, ASCII interval digits and raw environment truthiness. |
| Autonomous / loop.md ticks | Exact prompt variants; JavaScript UTF-16 truncation rather than UTF-8 byte truncation; latest Monitor instructions. |
| ScheduleWakeup | Numeric-string grammar and ECMAScript trim; actual `Tool::coerce_input` before outer schema validation; missing required output fields; minute/cache boundaries; stop, supersede, restart, expiry, keepalive and abort. |
| Fixed scheduling | Seven-day default; zero max-age disabling and permanent-job exemption; JavaScript boolean coercion; leap-day calendar search beyond a year; local DST setters; creator session/PID/start-token preservation, resume refresh and authoritative ownership checks; tool-created jobs are excluded from task-center migration so restart does not change their lifetime. |
| Monitor | Bounded feature-gated schema; 30-minute normal / 10-minute single-shot-print cap; actual CLI single-shot flag wiring; flush and delivered count before expiry; exact notice strings; terminal Killed status; strictly greater-than 30-second noise threshold. |
| Bridge delivery | Later priority, meta scheduled input, slash parsing bypass; wait for active turn; preserve raw sentinel for keepalive and resolve consumed prompts at execution; mixed user/scheduled batch veto. |
| Cancellation | Pending entries remain cancellable across persistence/publication; logical token handles runtime abort limitations; registration handshake handles zero-delay timers; cancelled queued commands are removed. |
| No-op folding | Fold latest available fire-to-tail slice and union previous IDs, rather than requiring N locally available boundaries; full assistant/tool/footer slice; clear settled streak on intervening user work. |
| CLI/local mobile entry points | Retain and bind scheduler cells; TUI and stdio queues; local mobile idle draining; session cwd; shutdown, idle cancel, hot-session transition fences, consistent disable gates and keepalive. Previously visible dynamic loops had no scheduler. |
| Live clients | Electron regression repair; Android/iOS session-level wakeup events accepted before turn start; original history retained. |
| Resume | Structured persisted scheduled-fire metadata, excluded from model context; additive optional DTO metadata and all-client restored folding. |

The independent host review found the persistence cancellation race and intervening-user stale streak; both received production fixes and regression tests. The reviewer then found no additional definite blocker in the reviewed host changes.

## Reproducible byte evidence

- `commands/core/tests/fixtures/loop_2_1_270/generate.mjs` and `provenance.json`: **60 command + 20 tick** variants evaluated from upstream functions; exact UTF-8 byte comparisons, **no normalization**. See the adjacent README for invocation and disabled cloud-offer branch.
- `tools/cron/tests/oracle/schedule-wakeup-2.1.270.mjs`: native and source SHA checks; **114 timing vectors**, **14 coercion vectors**, **3 prompt variants**, **5 result strings**, custom missing-field errors and schema metadata. The follow-up additionally executes the native Zod core for 208 inputs (131 rejects) and compares 48 full result strings across six timezones with no result normalization. Monitor adds 258 input vectors, giving 466 inputs and 319 rejects across the combined malformed-input corpus. Actual localhost provider-wire capture from the official binary also verifies the five model-visible tool schemas, including `$schema` metadata.
- `tools/cron/tests/oracle/verify_latest.py`: CronCreate prompt and calendar/DST differential evidence. Four tool-contract variants and 23 human-cadence outputs use no normalization. Raw CronCreate prompt SHA-256 `0ad661417250fc337888f6492b0b20d977f4fde8b5e8310c37efa6dc477cadab`.
- Folding source: `src_197155721.js` `P` takes the latest fire slice; `src_190098428.js` `oke` unions folded UUIDs. Scheduled delivery uses `priority:"later", isMeta:true, skipSlashCommands:true`.

## Follow-up scope: all local behavior, no old-version compatibility

The follow-up explicitly excludes remote account configuration and requests all remaining local differences be removed. Old LingXi loop paths, environment aliases and fire-record fallback formats are not retained.

| Previously open difference | Local implementation now in place |
| --- | --- |
| Outer permissions | Native local rule precedence and default allow behavior for Cron tools; Monitor's own rules followed by Bash safety evaluation. Auto mode now has a real asynchronous model-classifier path, rather than deterministic Pass followed by a prompt. |
| Malformed input | One shared native Zod-style validator for main dispatch and nested tool invocation; enriched model error and raw persisted Zod error remain distinct; strict-object proto handling matches the oracle. |
| Result formatting | Full ScheduleWakeup result strings compared without timezone normalization, across six IANA zones and DST boundaries. |
| Storage and locks | Session cron uses `.claude/scheduled_tasks.json` and the upstream leader lease record. Independent version-two task-center automation keeps its own store. No old loop-store migration or fallback. Both task-center migration entry points exclude all four loop sentinel prompts, so legacy loop records cannot be reactivated as v2 automation. |
| Fixed recurring execution | Due session tasks enter the main conversation's Later/meta queue. They no longer launch Dream subagents. Teammate-owned tasks are delivered to their owner; orphaned tasks are removed. |
| Session lifecycle | Unbound/busy schedulers retain pending work; session transitions stop the old scheduler, release its lease and discard queued old-session fires before rebinding. |
| Fire persistence | Native scheduled-task-fire fields and separate meta user turn companion; exact serializer fixtures and actual producer-envelope comparisons. Fixed fires omit loop-only fields. Actual fire UUIDs link persisted records to queued turns. |
| Model input origin | Mixed batches retain separate message identities and per-entry meta flags in one model turn. Scheduled input remains meta in persisted history and classifier context; automatic instructions are not treated as human authorization. Restored internal tick text is hidden while tool-result pairing is retained. |
| loop.md loading | Stable project-root `.claude/loop.md`, then current-directory `loop.md`; Node-compatible UTF-8 decoding/error skip set; checked errors propagated through main prompts, Skill tools and agent preload. |
| Local environment | Exact `CLAUDE_CODE_DISABLE_CRON`, `CLAUDE_CODE_LOOP_KEEPALIVE` and `CLAUDE_CODE_LOOP_PERSISTENT` names; no old aliases. |

Remote account/cohort configuration is excluded by request. Deterministic byte comparisons pin time, UUIDs and host paths when needed; random identifiers and independently sampled model responses cannot themselves be identical between two live runs. Native clients use their own UI and FFI; alignment concerns the loop contracts and execution semantics, not executable-file identity.

## Verification ledger

Initial audit verification (before the follow-up local-parity pass):

| Verification | Result |
| --- | --- |
| Upstream command/tick, wakeup and cron oracle generators | Passed; byte/normalization boundaries listed above |
| Cron / tool-cron | 124 / 77 tests passed |
| Session-owned autonomous delivery | 27 tests passed |
| Command prompts / preload | 15 prompt tests and both targeted preload tests passed |
| Monitor handler / tool | 28 / 13 tests passed |
| Bridge / message queue / desktop cron | 114 / 16 / 20 tests passed |
| Persistence / lowering / protocol | 1 / 1 / 13 tests passed |
| CLI / TUI / common timers | 5 / 1 / 2 tests passed; CLI cargo check passed |
| Mobile dynamic host | 3 tests passed before final session-owned resolver substitution; final production source also passed Clippy/native builds |
| Electron | 122 tests and both typechecks passed |
| Android restored conversation | 115 tests passed (37 mapper, 78 reducer), rerun with final generated Kotlin binding |
| Rust static analysis | Clippy passed for bridge, cron, tool-cron, mobile and CLI production packages; existing warnings remain |
| Diff hygiene | git diff --check passed |
| Android native | Official direct build script passed for arm64 + x86_64 and matching Kotlin bindings; staged under /tmp/lingxi-loop-bindings-270 |
| iOS semantic checks | Full app testable Swift module and ConversationExecutionReducerTests typecheck passed against staged bindings |

ScheduleWakeup's 19 targeted tests and the common timer tests overlap broader suite counts; counts should not be added as unique coverage.

Final local-parity verification:

| Verification | Result |
| --- | --- |
| Cron library / tool-cron | 143 / 82 passed |
| Permissions with bash-ast / standalone loop matrix | 1,631 / 8 passed |
| Native schema dispatch / classifier history / Monitor approval | 22 / 2 / 1 passed |
| Mixed queued-origin integration / scheduled turns / JSONL resume | 1 / 11 / 1 passed |
| Session / bridge libraries | 306 / 116 passed |
| Mobile dynamic / fixed / preload / legacy migration | 3 / 1 / 1 / 1 passed |
| Desktop cron / preload | 27 / 2 passed |
| CLI loop | 5 passed |
| Restored message lowering | 7 passed |
| Monitor tool / execution handlers | 15 / 31 passed |
| Command loop prompts / loop.md reader / checked producers | 17 / 30 / 4 passed |
| Client protocol snapshots and version guards | 17 passed |
| Electron updated conversation/event tests | 61 passed; shared build and both Electron typechecks passed |
| Rust static analysis | Final Clippy passed for bridge, CLI, mobile, cron and loop tools; existing warnings remain |
| Android native/client validation | Official direct/play builds passed for arm64 + x86_64; each flavor passed 117 JVM tests with forced rerun; generated Kotlin byte-equal; stable-source/hash checks passed; both native libraries and shared binding promoted with backup |
| iOS native/client validation | Official three-slice build passed; 97 XCTest passed (conversation + updated task-center tests), zero failures; stable-source/hash/architecture checks passed; matching Swift and XCFramework promoted with backup |

Counts overlap across targeted and full suites and are not a unique-test total. The final source includes both old-loop migration exclusions and the restored meta-text filter. Earlier `/tmp` logs and native staging were no longer present when the task resumed; completed outcomes above were observed before that, and no unfinished run is counted as passed. Remaining tests and Clippy were resumed successfully with evidence saved under `.omx/state/loop-parity-270/verification`. A concurrent task subsequently changed shared cron sources; additional verification passed: cron/tool-cron 143/82, desktop cron 27, mobile cron 18, dynamic/fixed loop 3/1, and Clippy. The seven affected Rust source hashes match the post-verification snapshot. Native baselines built during those edits were not promoted; final builds use the stable verified Rust source. Two subsequent Swift task-center edits were captured before XCTest and covered by the expanded 97-test run.

Final native evidence is saved under `.omx/state/loop-parity-270/native` (ignored local artifacts). Oracle fixtures and this report are retained in the repository.

### Source areas changed

- Prompt/contract: `command-api/src/model.rs`, `commands/core/src/bundled/loop_skill.rs`, its oracle fixtures; `tools/cron/src/{lib,wakeup,runtime_wakeup,schedule_cron,cron_list,cron_delete}.rs` and oracle tests.
- Scheduling: `cron/src/{lib,autonomous_loop,schedule,scheduler,tasks_file}.rs`; Monitor tool/handler, `platform-api/src/session_flags.rs`.
- Hosts: `apps/bridge-server/src/{boot,driver,server,router}.rs`, `msgqueue/src/queue.rs`, `apps/engine-desktop/src/{lib,cron_management,agent_skill_loader}.rs`, `apps/engine-mobile/src/{lib,host}.rs`, CLI loop adapter/init/mode/REPL and TUI idle-interrupt callback.
- Restore protocol: `client-protocol/src/{message,events}.rs` and snapshots/guard tests; `client-adapter/src/{lowering,output_stream,turn}.rs`; `orchestrator/src/{resume,conversation/transcript,conversation/model}.rs` and persistence regression.
- Clients: Electron conversation reducer/tests; Android conversation models/projections/reducer and tests; iOS models/conversation source and tests. Prior agent placement changes remain preserved.

The follow-up also changes shared validation, permission classification, session serialization and checked Skill producer interfaces; exact paths are visible in the corresponding regression fixtures. No external dependencies were added. Existing unrelated working-tree changes were retained; this document describes this audit's scope, not the entire dirty checkout.

## Completion

All planned local verification is complete. Final native promotion manifests: `.omx/state/loop-parity-270/native/{android,ios}/promotion.json`; each records tests, backups and file hashes. `git diff --check` passed after completion. No new external dependencies, old loop environment aliases, old JSONL loop fallback, or old-loop migration support were introduced. Remote account/cohort configuration remains excluded by request. Deterministic fixture equality does not imply identical stochastic model responses or byte-identical executables. No commit or push was performed.
