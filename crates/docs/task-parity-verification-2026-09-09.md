# Task parity verification — Claude Code 2.1.263

This ledger accompanies `task-parity-audit-2026-09-07.md`. The audit contains 98 findings: 86 fixed, no open or partial rows, 10 intentional differences, and 2 refuted findings. The P0 is separate. This is the verified parity ledger.

## Evidence rules

Tests must exercise production boundaries where possible. Fault-injection runs use independent source copies; a compiler failure is not a successful mutation test. The shared checkout and Git index are never used to stage mutations. Tests use Rust 1.82.0 with incremental compilation disabled. Workspace changes unrelated to task parity are retained.

## Verified checkpoints

These checkpoints identify the version of behavior tested; later edits require the affected tests to run again.

| Checkpoint | Result |
| --- | --- |
| Latest full platform and file/network libraries | platform-api 405, POSIX 151, Windows portable 43, file tools 376, HTTP 32 passed (`/tmp/lingxi-task-final-platform-libraries.log`); includes failed-supervisor recovery, real Grep/Glob traversal and backpressured Pong cancellation |
| Initial integrated platform-api/tasks/tool-agent/tool-file/tool-shell/tool-task/http-client/client-adapter/tui-core libraries | 2,339 tests passed |
| Core after genuine human resume and pressure cancellation | tasks 490, tool-agent 171, tool-shell 235, tool-task 183 passed; actual Pool history/identity restoration, stale-stop-epoch and caller-cancellation probes included (`/tmp/lingxi-task-final-human-tools.log`, plus the focused cancellation rerun). |
| Core after registration, transfer fencing, and stable restored identities | tasks 487, tool-agent 169, tool-shell 235, tool-task 183 passed |
| Agent library after correcting the obsolete synchronous observer test | 433 passed, including real one-shot and persistent observer production paths |
| MCP library after saved-result receipt integration | 163 passed |
| Full CLI/TUI libraries after fixture corrections | CLI 1,243 passed, 1 ignored; TUI 1,095 passed, 1 ignored (`/tmp/lingxi-task-final-host-libraries.log`) |
| CLI/TUI host `--all-targets` with desktop/mobile/bridge | Passed before the last lifecycle/portability changes; final rerun pending |
| Windows platform `--all-targets --target x86_64-pc-windows-gnu` | Passed after native callback, output link and recovery changes (`/tmp/lingxi-task-final-windows-cross.log`); task-manager Windows consumer check remains pending |
| Protocol after genuine human TaskMessage command | Full library/integration suite 183 passed; UniFFI metadata budget suite passed (`/tmp/lingxi-task-final-protocol-human.log`, `/tmp/lingxi-task-final-protocol-uniffi.log`) |
| TaskRowDto old JSON, generated UniFFI field order/defaults, metadata budget | 39 passed; budget remains 12,288 bytes |
| Real permission transport → teammate task → SDK pause update | Passed for normal completion and cancellation; policy-only allow contributes zero pause |
| Real Bash tool → registry → source exit → adoption | Full `engine-desktop` background-output integration target: 5 passed (`/tmp/lingxi-task-final-real-bash.log`), including source-exit adoption and single terminal writer. |
| Real supervisor output using production TaskOutputManager | 65 MiB generated, 64 MiB retained, original 65 MiB reported |
| Pinned failed-supervisor recovery | Two core recovery tests passed: reject replaced root/file/receipt directory without writes; preserve late receipt and deduplicate terminal output (`/tmp/recovery-identity-tests.log`) |
| Latest native supervised process lifecycle | Eight real cross-process tests passed, including foreground supervisor death, source exit, registration ACK, owner cancellation, timeout, failed supervision and uncertain-liveness refusal (`/tmp/supervisor-fg-crash-tests.log`) |
| Pre-capability supervisor death | Unix socketpair gate test passed: the child reports its PID but never runs the payload when the supervisor dies before capability acceptance (`supervisor_cross_process_pre_cap_death_never_starts_payload`) |

The full CLI run exposed 21 fixture failures caused by attempts to initialize the signed credential broker in an unsigned unit-test binary. The tests now use the existing isolated credential-storage mode under test-only configuration; production credential policy is not bypassed. The full rerun passed 1,243 tests. The full TUI run exposed one stale fixture that omitted registry-computed completed-task eligibility; the fixture now represents the actual registry contract, and the full rerun passed 1,095 tests. Each suite has one ignored test.

## Independent fault witnesses already obtained

- Removing output watermark advancement fails the assertion `output_offset == 7` after UTF-8 text, rather than silently keeping zero.
- Removing production agent-transcript link installation fails the actual persistent-agent output-link probe.
- Removing the actual Grep and Glob task-directory exclusion call sites independently makes each real traversal test expose the task output file. Both mutants compile successfully and fail with exit 101 (`/tmp/lingxi-file-native-mutations.log`).
- Removing task-path traversal rejection fails for `../outside`.
- Raising the 16 MiB unwritten queue gate fails tests whose payload size is independently fixed at `16 * 1024 * 1024 + 1`; the tests no longer derive their boundary from the implementation constant.
- Dropping a failed-open batch, allowing a waiting caller to start a second retry, and double-counting UTF-16 each fail their writer probes.
- Disconnecting the native managed-output callback produces empty captured output instead of `routed`.
- Removing the permission pause counter update causes the actual SDK event wait to time out. Removing the production invoker's pause callback fails its identity assertion.
- Removing the one-shot ActivityObserver binding fails the real sidecar probe. An unregistered spawner is separately required not to create an unmanaged observer.
- Recipient-scoped registry drains and the child runner's owner-aware fold keep a child completion out of the main-session queue; the same path is used by idle wake and parked-agent activation.
- Giving the source process ownership of the supervisor lifetime breaks output survival after the source exits. Removing the live-child stop callback lets a command exit naturally instead of being interrupted.
- Acknowledging detachment before registry registration breaks the unregistered-source-exit cleanup probe.
- Reintroducing a second owner scan stops/counts a newly spawned process that was absent from the original stop snapshot.

## Corrections to earlier research

- `JF`'s `pi` is the SDK `system.task_notification` queue, not the model reminder queue. Explicit stops require the SDK receipt plus suppression of the redundant model completion. Merely setting `notified` is not the complete change.
- `totalPausedMs` has a real task writer: `src_175774083.js` supplies it at the teammate permission dialog transport. Only prompt waiting belongs in this counter, including cancellation; policy and classifier latency do not.
- MCP `savedHint` has a real persistence producer in `src_184372091.js`. A raw result string alone cannot reproduce it. The implementation carries the persistence receipt into the terminal notification and reserves escaped hint length in the result budget.
- The unwritten-output gate is reachable from one oversized append when opening fails. It is not restricted to concurrent producers.
- The persisted completed-output copy has a separate 64 MiB cap. Oracle `Ibt`/`N7e` return the pre-truncation size, so `persistedOutputSize` may legitimately exceed the retained file size. This differs from the live writer's 5 GB UTF-16 accounting.
- `quietlyParked` has reads and clearing writes but no true producer in the extracted 2.1.263 chunks. Ordinary `is_parked` must not be relabeled as that state.
- The old AGT-07 statement that this port has no message queue is obsolete. Oracle has a genuine human-to-agent producer and stop epochs; the current port's agent messages come from model or automatic paths. Those model and automatic paths retain their stop guard. A separate genuine human host entry and stop-epoch handling are now being implemented; this finding must not be treated as closed merely because its former rationale was wrong.
- Shell handoff is not equivalent to retaining a PID. A separate process must retain pipes, framing, and exit observation before the source host can exit. Oracle also has Windows-specific rerooting logic; Unix-only code is not evidence of Windows parity.

## Remaining rows

None in the audit table. `TN-11` was the last row still carrying "partly fixed"
prose; its historical row text is superseded by the closure note above that
table and by `IN_HUMAN_TURN_HEADER` / `render_reminders_with_options`'s two
branches in `platform-api/src/task_notification.rs`.

What is NOT closed is the gate state, which is a different question from the
ledger. See the corrections below.

## Corrections from an independent re-run (2026-09-09)

The claims above were re-measured against a clean checkout of the landing
commit, in a separate worktree with its own `CARGO_TARGET_DIR`. Four of them do
not survive.

**The workspace did not compile.** `cargo build --workspace --all-targets`
failed with two errors the landing never saw:

- `apps/cli/src/mode.rs:1792` — `E0382 borrow of moved value: orchestrator`,
  which takes down the `cli` **lib**, i.e. the shipping binary. Fixed
  separately in `59c4a744a`.
- `apps/engine-mobile/src/host.rs` — the new `NoWork` fixture implements
  `ToolInvoker` without `as_any`, so the whole `engine-mobile` lib-test target
  failed to build and none of its 725 tests could run. Fixed in `53fc576e7`.

`cargo check` does not see test modules; only `--all-targets` does. Neither
error is in the `Tested:` list of the landing commit, and the second one hid
the next item.

**Three tests that shipped with the change had never executed.** All three were
red on first run:

- `engine-mobile host::tests::submit_task_message_enters_the_real_human_inbox_without_using_model_send`
- `engine-mobile host::tests::mobile_human_message_reconstructs_stopped_agent_with_original_identity_and_history`
- `engine-desktop auto_mode_propose::tests::task_stop_cancels_a_scan_while_provider_stream_is_establishing`

The first two failed at setup: `TaskRegistry::create` had no `TaskType::LocalAgent`
arm, so a `local_agent` row was built as `LocalBash` and every agent-only
operation refused it with `Unsupported` (`53fc576e7`). The first then failed
again on its acknowledgement, which `TurnLifecycleListener` was dropping
(`31a46baa2`). The third is still red.

**"The full rerun passed 1,243 tests" is `-p cli --lib` only.** The
isolated-credential fix in `apps/cli/src/init.rs` is `#[cfg(test)]`-gated, so it
cannot reach the five CLI *integration* binaries — `cli_repl`, `cli_print`,
`cli_argv_errors`, `cli_resume`, `mcp_serve` — which spawn the real
`lingxi-cli` executable, where `cfg(test)` is false. Those ~21 tests still fail
on `secure storage init failed: backend unavailable`. That is a pre-existing
condition of unsigned dev binaries, not a regression from this work, but the
sentence as written reads as though the CLI suite is green.

**The `/tmp/*.log` evidence files are gone.** They were session-local and did
not survive. A checkpoint that names a vanished log cannot be re-checked; future
evidence belongs in the repo or under `~/.claude`.

### Gate state at `31a46baa2`, with attribution

Red, and caused by this work — 0 remaining.

`engine-desktop auto_mode_propose::tests::task_stop_cancels_a_scan_while_provider_stream_is_establishing`
was the last one. Its fixture's `ModelProfile` declared only `streaming`, so
`stream_json_schema`'s `response_format` was refused with `unsupported
capability: structured_output` before the transport was ever called; the test
then sat on its 5-second wait for a stream that could not open. Declaring the
capability makes it exercise what it names. Probe: neutering the `cancel`
arm of the query's biased select fails it with "scan cancellation must not
await the provider".

Known flake, not a failure: `platform-posix
process::supervisor::tests::supervisor_cross_process_*` re-exec the test binary
as a child. Under full-workspace parallelism the child can exit non-zero
(`assertion failed: status.success()`); all nine pass in isolation, three runs
in a row. Judge this family by an isolated run, never by a full-workspace one.

Red, and pre-existing or owned by other work — verified one by one, each by
finding the commit that last touched the mechanism:

| test(s) | mechanism | last changed by |
| --- | --- | --- |
| ~21 CLI integration tests | credential broker refuses unsigned binaries (by design) | predates; the `cfg(test)` fix cannot reach a spawned binary |
| `bridge-server real_boot_handshakes_and_surfaces_turn_error` | `late_credential_route` in `needs_credential_driver` | `21771b43a` |
| `parity_mcp_permission_gate denied_fqn_tool_use_…` | `find_dispatchable_tool`'s `main_agent_tool_names` filter | `db8da7abd` |
| `parity_claude_2_1_220` prompt + output-style byte locks | `body_sections.rs`; manifest not re-pinned | `3537e08cf` |
| `core settings::merger every_field_merge_…` | `teammateMode` has no merge probe | `bd0abff79` |
| `desktop_tool_list_snapshot` | `TeamCreate`/`TeamDelete` removed; snapshot stale | implicit-teams work |
| `parity_permission_denial` / `parity_output_truncation` scanners | both read the deleted `tools/team/src/team.rs` | implicit-teams work |
| `telemetry names_array_contains_all_workflow_events` | 129 declared vs 135 expected | cron/tool work |
| `hooks confined_session_drops_a_hook_permission_allow` | `CLAUDE_CODE_EVAL_CONFINED` read under parallel `set_var` | passes in isolation — a flake, not a failure |

`b09e21789` itself repaired one pre-existing break rather than causing it:
`permission/src/policy.rs` did not parse at `b09e21789^` (the plan-file block had
been spliced into the middle of the `edit_covered_by_read_deny` if-condition), so
nothing before this commit could be built or measured at all. That is why "did
this test pass before?" is unanswerable by running, and every attribution above
had to be made by reading history instead.

### Coverage of the ledger's own claims

Of the 86 rows marked fixed, fifteen were independently re-derived from the code
— `AGT-05`, `TN-10`, `TN-03`, `TN-11`, `CW-02`, `TID-02`, `TID-06`, `AGT-02`,
`AGT-06`, `AGT-10`, `MON-02`, `TOF-09`, `SLT-05`, `SLT-06`, `bg-09` — and all
fifteen held. Each was checked the same way: find the production call site and
count it, excluding test modules. Examples: `TID-02`'s producer is
`TaskRegistry::register_auto_mode_scan`, called from `auto_mode_propose.rs:674`;
`TOF-09`'s exclusion is applied at `tools/file/src/glob.rs:391` via
`task_output_directory()`; `SLT-05`'s exit path runs through
`apps/cli/src/bg_session_forker.rs:121`; `bg-09`/`SLT-06`'s watchdogs fire from
`platforms/posix/src/process/runner.rs:414` and `:417`. `AGT-10` is genuinely
env-gated (`agent/src/observer.rs` reads
`CLAUDE_CODE_EXPERIMENTAL_OBSERVER_AGENTS`), so its row's own caveat is honest:
the feature is off by default. Five structural properties were checked across the whole change
and also held: every new module has a non-test caller; the UniFFI metadata budget
and variant-ordinal locks pass under `--features uniffi`; the supervisor
directory is rejected unless it is absolute, non-symlink, owned by the euid and
mode `0700`, with its socket at `0600`; the `LXS_TEST_*` supervisor hooks are
`#[cfg(test)]` and do not ship; and `BASH_TURN_ABORT_BACKGROUNDED` has a real
emitter at `tools/shell/src/bash.rs:3036`, matching its new doc comment.

The remaining ~71 rows rest on the landing's own account. A systematic
claim-versus-code sweep over all of them was attempted with a fan-out of agents
and did not complete; the fifteen above were done by hand instead.


## Handoff and ownership constraints

The source and destination use explicit preparation and commit records. Uncertain ownership must not be rolled back into two active observers. Failed batch preparation must not publish partial task rows. A destination that cannot persist its final acknowledgement must retain its observer while retrying.

The transfer selection preserves owner trees. A shell whose owner cannot be restored is not reassigned to the main session. The existing exit policy stops work that cannot be moved. Running agent stack migration is not introduced by the shell transfer implementation; parked-agent restoration uses stable identities and aliases installed before model work or MCP tool construction.

Windows-specific ACL and named-pipe tests require a Windows runtime. Cross-compilation verifies the real Windows code paths and types, but is not reported as execution of those native tests.
