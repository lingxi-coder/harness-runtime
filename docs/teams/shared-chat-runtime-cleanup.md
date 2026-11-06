# Runtime M1 cleanup receipt

Skill: `/Users/luolingfeng/.codex/skills/ai-slop-cleaner/SKILL.md` (standard scoped pass).

Behavior lock before cleanup: actual worker fixtures 12/12 passed in 211.66s; canonical journal 6/6 (including >4 MiB replay and copied cross-language vectors); provider account isolation 4/4; native durable writer 27/27, including ordinary compact-boundary retry. Synthetic HTTP/model fixtures are not production-model/device acceptance.

Plan before cleanup:

1. Dead code: remove synthetic-only stderr diagnostics, the obsolete bounded-string input error and unused profile test helper; remove unused old `home/transcripts` creation/check. Retired guardian module/branches were already removed with root approval.
2. Duplication: retain the single canonical journal, existing captured resume and append-once writer; do not introduce cache, resume, tokenizer, quota or transport abstraction. The temporary verified projection index avoids repeated fsyncs, with one final append-once durability acknowledgement for an existing prefix.
3. Naming/readability: clarify the changed compact-boundary predicate and format only newly added blocks/schema literals. Preserve unrelated create-bot/skill edits.
4. Test reinforcement: preserve all behavioral fixtures; include provider error/overflow refusal, measured partial usage and exact terminal recovery. Run ordinary writer and compaction regressions plus final default build.

Scope: `crates/runtime/src/desktop/team_process/`, `team_invocation.rs`, `session_state/team_chat.rs`, narrow session_state/assembly/headless/branch-recovery hooks, `bin/team_worker.rs`, owned Team tests/fixture; the new orchestrator input projection/compaction fence, existing llm-runtime quota authorization hook, the narrow session durable-writer predicate/test, and retired posix guardian module/branches. Connector host belongs to the Desktop host lane and is excluded from cleanup edits.

Cleanup passes completed:

- Removed all temporary stderr forwarding and diagnostic prints from the worker path. Child stderr is again null; failures expose fixed categories only.
- Removed unused input-limit error and obsolete transcript directory checks. No fallback decoder, second journal or second resume implementation was introduced.
- Renamed the ordinary compact-boundary predicate to `resets_parent_chain`; formatted only added code blocks and owned new files.
- Retained verified per-row identity and one existing append-once fsync acknowledgement for a captured prefix. This is an ephemeral projection index, not a Bot history store.
- Dead-code pass `cargo check -p harness-runtime --features desktop --lib` passed in 3m29s (`/tmp/teams-runtime-cleanup-deadcode-check.log`). Existing repository warnings remain.

Final static verification: `git diff --check` passed. Runtime journal 7/7, account/model/result units 12/12 and per-dispatch quota 4/4 passed. Final actual-worker acceptance passed 15/15 in 303.22s (`/tmp/teams-runtime-process-final.log`). Profile7, reference/history4 and major3 host3 also passed. The final default binaries passed the actual Desktop JS pipeline with stable before/after hashes; see `shared-chat-runtime-m1.md` for timing and receipt. No temporary stderr diagnostics, obsolete guardian callers, old identity decoders or parallel persistence layer remain in this scope.

## Existing native 2.95 oracle discrepancy

The focused ordinary orchestrator compaction suite has 7 passes and one failure: `conversation::compaction_impl::session_compact_mod_tests::native_295_compact_summary_reaches_output_and_jsonl_with_exact_source` (`/tmp/teams-orchestrator-compaction-regressions.log`). This is recorded, not changed or skipped in the acceptance count.

Baseline source inspection with `git show HEAD:crates/orchestrator/src/conversation/compaction.rs` shows `CapturedSummaryClient::query` returning the raw captured response, whose UTF-16 units are explicitly asserted as `[65, 0xd800, 66]` (A, lone surrogate, B), without a `<summary>` block. Baseline `git show HEAD:crates/compaction/src/autocompact.rs` lines 696–701 already rejects a non-preserved-tail bounded forced summary lacking that block.

The call path is `force_compact_with_instructions_and_cancel` → `CompactionOrchestrator::process_forced(...Some(new_compaction_api_budget()))` → the existing summary validation. It returns `ActionFailed("Error during compaction: Failed to generate conversation summary - the model's reply has no <summary> block")`; the fixture fails at `compaction.rs:5135` before the writer/parent-chain change or new external-input protection branch. This is baseline source evidence, not a separately built clean-baseline execution. The oracle and strict summary acceptance rule remain unchanged. Current compaction crate positive and negative tests pass 301/301 (`/tmp/teams-compaction-final.log`), including missing/empty summary refusal, bounded retries, preservation of recent history and byte-faithful summaries.
