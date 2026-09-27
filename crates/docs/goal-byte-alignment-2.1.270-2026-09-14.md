# Goal alignment audit — Claude Code 2.1.270

Baseline verified 2026-09-14 using `npm view @anthropic-ai/claude-code version dist.integrity`: latest is **2.1.270**. Authoritative executable: `~/.local/share/claude/versions/2.1.270`, SHA-256 `a506b6d970a4cf44f6abdb53a81ddcd5d3b0ce042a95c502fe9d1f946bdb8807`. npm integrity: `sha512-0zMkfIWQu7/SG56VP8r780HZWvrNShzK28AbAnhKRK0ns+ToGXPT0W8UqyZmZCUKAkJDd5//TrwSOhk1+hysiw==`.

Evidence below comes from extracted executable chunks in `~/.claude/oracle-chunks/2.1.270/`, not reconstructed source. Offsets are character offsets within the named extracted file.

| Surface | Exact executable evidence | Audit finding |
| --- | --- | --- |
| Goal registration | `src_167833692.js`, `tPe`: `add(r,"Stop","",{type:"prompt",prompt:e})` | Register the raw condition; the former extra “Evaluate whether…” wrapper changes model input. |
| Evaluator user prompt | `src_169588164.js`, `PFr`, offset 5481693 | `Based on the conversation transcript above, has the following stopping condition been satisfied? Answer based on transcript evidence only.\n\nCondition: ${e.prompt}`. |
| Evaluator system prompt | Same chunk, `PFr` | Uses “in Claude Code”; replacing that with “in LingXi” prevents byte identity. The impossible-condition guidance otherwise matches. |
| Continuation feedback | `PFr` offset 5486099, `Het` offset 5557441; `gLt` in `src_166496417.js` | `Stop hook feedback:\n[${condition}]: ${reason}`; stripping the condition is appropriate for stored `lastReason`, but not model feedback. |
| Error and timeout | `src_169588164.js`, `$Qn`, before `tengu_goal_evaluated` offset 3586589 | Evaluator errors set `goalPass="error"`; timed-out evaluations set `"cancelled"`. Neither increments goal iterations nor produces a NotMet goal status. |
| Evaluator duration | Same `$Qn` | `let D=Date.now()` at Stop-handler entry, then `durationMs:Date.now()-D`; includes Stop-handler work, not whole API turn duration. |
| Interrupted goals | Same chunk, `Kps` / `Yps` / `WQn` | Error retries; timeout pauses; unmet at block cap pauses. Retry delays are 60/300/900 seconds plus 0–20% positive jitter, maximum three attempts. Displayed minutes use the unjittered base (1/5/15). |
| Check-in deferral | Same chunk, `K7n` / `Y7n` / `Jfs` | New work batch uses minimum task startTime strictly after lastDeferralPassAt; task identity alone is not equivalent. Default 30/60/120-minute schedule, three idle deliveries. |
| Command boundary | `src_193633469.js`, local handler, offset 4299 | `e.length` measures UTF-16 units, `trim()` uses ECMAScript whitespace; status tests lastReason truthiness before trimming. Narrow command fixes and regression tests cover these cases. |

Implemented in this change: raw evaluator condition registration and evaluator fixes; command UTF-16 length, ECMAScript whitespace and reason truthiness; hooks-policy precedence; check-in text sanitization and exact bracket/slash mapping. Prompt fixture evidence is checked into `hooks/tests/fixtures/goal_oracle_2_1_270.json`. The check-in sanitizer preserves ordinary letters and combining accents: upstream `Vue` does not perform general homoglyph folding or NFC normalization.

The implementation now excludes deferred evaluators before dispatch, preserves the condition in blocking feedback and success evidence, leaves iteration counts unchanged on timeout/error, shares the Stop block cap, and measures Stop-handler duration. Goal length metrics use UTF-16 units. Idle check-ins enter the host queue instead of appending a dormant history message; the redundant idle-only persistence path was removed. Finished background work queues the final check-in too. Start timestamps are joined from one internal task snapshot without changing the serialized hook payload.

Changed code is grouped by responsibility:

| Area | Files |
| --- | --- |
| Command and oracle fixtures | `commands/core/src/goal.rs`, `commands/core/tests/fixtures/goal_2_1_270.json`, `hooks/tests/fixtures/goal_oracle_2_1_270.json` |
| Evaluator dispatch and exact system prompt | `hooks/src/executor.rs`, `hooks/src/prompt_executor.rs` |
| Goal evaluation / deferral / idle delivery | `orchestrator/src/conversation/hooks.rs`, `conversation.rs`, `conversation/model.rs`, `conversation/transcript.rs`, `stop_hook_snapshot.rs`, `prompt/goal_checkin.rs` |
| Retry state and admission | `orchestrator/src/conversation/goal_retry.rs`, `conversation/runtime.rs`, `conversation/drivers/mod.rs`, `handle_impl.rs`, `turn_loop.rs`, `prompt/goal_interruption.rs`, `prompt/mid_turn_input.rs` |
| Existing host queue adapters | `msgqueue/src/queue.rs`, `apps/bridge-server/src/{driver,server}.rs`, `apps/cli/src/{mode,repl,loop_wakeup}.rs`, `apps/engine-mobile/src/host.rs`, `apps/engine-desktop/src/lib.rs` |
| Regression coverage | `orchestrator/tests/stop_hooks_test.rs`, goal tests under `orchestrator/src/conversation/tests/`, inline retry/checkin/command/queue tests, queued-prompt fixture initializers |

## Retry transport and remaining differences

The interruption timer now enqueues an opaque, cancellable goal retry through the existing Bridge, terminal TUI/REPL, and mobile host queues. Normal host admission owns the running-turn token, permission lifecycle, UI events and error finalization; the timer does not call the model directly. Admission revalidates the opaque retry identity at the orchestrator turn gate, including a queued retry batched with fresh human input. Goal replacement, clear, session reset, human input, cancellation and runtime drop invalidate pending work. A separate queued check-in identity uses the same transport without consuming interruption retry attempts. The retry notice uses base delays of 1/5/15 minutes while its timer uses positive jitter.

Remaining differences after this wiring:

- The native `tengu_goal_interruption` analytics event is not yet emitted; absence of this telemetry is separate from user-facing notice and model-prompt byte alignment.
- The API interruption classifier currently lacks upstream `quotaLimits` account-exhaustion metadata and host reset-wait intent. It therefore cannot distinguish the two usage-limit pause messages from generic rate limiting.
- The retry timer conservatively defers while **any** main-thread queue item is pending, whereas upstream tests its passive-notification predicate. This can delay retry delivery more than upstream but gives queued human work priority.
- Real packaged-app interaction has not been exercised. Unit/integration tests exercise queue delivery into a mocked model turn, stale identity rejection, human-input invalidation, retry cap/deduplication, check-in independence, unsupported-host gates and cancellation cleanup; they do not establish complete product equivalence.

## Verification results

- `cargo test -p hooks --lib`: **496 passed**, including the extracted Stop system-prompt byte fixture.
- `cargo test -p orchestrator --lib goal -- --test-threads=1`: **74 passed**, including idle host-queue wake-up for running/finished background work, turn-end check-in continuation, stale admission and timeout/cancellation state.
- `cargo test -p orchestrator --test stop_hooks_test`: **16 passed**, including successful goal evidence in the terminal attachment, exact feedback, skipped deferred evaluator, timeout/error behavior and shared block cap.
- `cargo test -p orchestrator --test queued_prompt_batch_test`: **1 passed**.
- `cargo test -p command-core --lib goal::tests`: **13 passed**, including full extracted directive byte equality.
- `cargo test -p msgqueue --lib`: **17 passed**.
- `cargo check -p bridge-server -p engine-desktop -p engine-mobile -p cli`: passed.
- `cargo clippy -p orchestrator -p hooks -p command-core -p msgqueue --lib --no-deps`: passed with warnings; this is not a warning-free workspace baseline.
- Scoped rustfmt checks and `git diff --check -- lingxi-code`: passed.

Total: **617 tests** across the listed suites. Compilation used an isolated target directory to preserve the concurrent workspace build. No new dependencies, commits, or signed app package were produced. The remaining differences above mean this report deliberately does **not** certify universal byte-for-byte equivalence with Claude Code.
