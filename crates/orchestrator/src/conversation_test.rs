//! Unit tests for the conversation orchestrator.

use super::*;

#[path = "conversation/tests/session_memory_context_tests.rs"]
mod session_memory_context_tests;

#[path = "conversation/tests/invoked_skill_lifecycle_tests.rs"]
mod invoked_skill_lifecycle_tests;

#[path = "conversation/tests/session_memory_background_tests.rs"]
mod session_memory_background_tests;

#[cfg(test)]
#[path = "conversation/tests/prompt_snapshot_tests.rs"]
mod prompt_snapshot_tests;

#[path = "conversation/tests/bounded_post_compact_read_tests.rs"]
mod bounded_post_compact_read_tests;

#[cfg(test)]
#[path = "conversation/tests/audio_schema_projection_tests.rs"]
mod audio_schema_projection_tests;

#[path = "conversation/tests/durable_cost_handoff_tests.rs"]
mod durable_cost_handoff_tests;

impl ConversationOrchestrator {
    async fn restore_post_compact_attachments(
        &self,
    ) -> Vec<lingxi_core::types::ConversationMessage> {
        self.restore_post_compact_attachments_against(&[]).await
    }
}

#[cfg(test)]
#[path = "conversation/tests/ephemeral_tool_result_persistence_tests.rs"]
mod ephemeral_tool_result_persistence_tests;
#[cfg(test)]
#[path = "conversation/tests/generated_session_name_tests.rs"]
mod generated_session_name_tests;
// ============================================================================
// Turn-recovery behaviors (RECOV.1 / RECOV.2 / RECOV.4)
// ============================================================================
//
// In-file integration tests for the three turn-driver recovery behaviors ported
// from claude-code `query.ts`:
//   - RECOV.1 — the streaming driver's blocking-limit preempt
//     (`query.ts:592-648`): a prompt already at the hard blocking limit ends the
//     turn with the byte-exact prompt-too-long message WITHOUT opening the stream.
//   - RECOV.2 — `StopFailure` hooks fire on an api-error turn-end
//     (`query.ts:1174/1181/1263`); the normal `Stop` hooks do NOT.
//   - RECOV.4 — a Stop-hook blocking continuation resets the
//     `max_output_tokens` recovery budget (`query.ts:1291`).
#[cfg(test)]
#[path = "conversation/tests/turn_recovery_tests.rs"]
mod turn_recovery_tests;

// ============================================================================
// OUTSTYLE.3: per-turn, transient output-style reminder.
//
// Proves the byte-exact `<system-reminder>` meta user message is appended to
// EACH turn's OUTGOING model input when a non-default output style is active,
// on BOTH turn drivers (batched `run_turn` + streaming `run_turn_streaming`),
// and that it is NEVER persisted to `session.history` nor the JSONL transcript
// (transient — never accumulates). With the default style the outgoing message
// list is byte-identical (no extra message), keeping the locked parity fixtures
// green.
// ============================================================================
#[cfg(test)]
#[path = "conversation/tests/output_style_reminder_tests.rs"]
mod output_style_reminder_tests;

// ============================================================================
// R-P1c/R-P1d: the leading `additionalContext` (`# claudeMd` / `# userEmail` /
// `# currentDate`) meta message — byte-lock against claude-code `A6n`.
// ============================================================================
#[cfg(test)]
#[path = "conversation/tests/additional_context_tests.rs"]
mod additional_context_tests;

// ============================================================================
// SKILLEXEC.3 (model scope): a tool's `context_modifier` switches the session's
// main-loop model, applied POST-BATCH on BOTH turn drivers (batched `run_turn`
// + streaming `run_turn_streaming`). A tool that returns NO modifier (every
// existing tool + skills WITHOUT a `model:` frontmatter) leaves `session.model`
// untouched — byte-identical, keeping the locked turn-loop/streaming fixtures
// green.
// ============================================================================
#[cfg(test)]
#[path = "conversation/tests/skill_model_override_tests.rs"]
mod skill_model_override_tests;

// ============================================================================
// Task 7: mid-stream 529 → non-streaming fallback tests
//
// Parity: `claude.ts:2469-2594`, `withRetry.ts:141,186`
// Env gate: `LINGXI_DISABLE_NONSTREAMING_FALLBACK` (claude.ts:2470)
// Error copy: `errors.ts:166` REPEATED_529_ERROR_MESSAGE = "Repeated 529 Overloaded errors"
// ============================================================================
#[cfg(test)]
#[path = "conversation/tests/task7_midstream_fallback_tests.rs"]
mod task7_midstream_fallback_tests;

/// Task 6 (llm-runtime future-work batch 5): the terminal-429 limits-copy
/// re-map (`enrich_rate_limited_error`). The integration test
/// (`tests/rate_limit_terminal_429_test.rs`) drives the batched `ApiCall`
/// wrapper end-to-end; these cover the `Streaming` wrapper and the
/// pass-through arms directly.
#[cfg(test)]
#[path = "conversation/tests/enrich_rate_limited_error_tests.rs"]
mod enrich_rate_limited_error_tests;

// ============================================================================
// SKILLLIST.1: per-turn, transient `skill_listing` reminder.
//
// Proves the orchestrator method: returns the rendered `<system-reminder>` when
// a provider is wired AND the `Skill` tool is present this turn; returns `None`
// when no provider is wired, or when the `Skill` tool is absent (so we never
// advertise skills the model can't invoke). The byte-level formatting is covered
// in `prompt::skill_listing::tests`.
// ============================================================================
#[cfg(test)]
#[path = "conversation/tests/skill_listing_reminder_tests.rs"]
mod skill_listing_reminder_tests;

// ── `agent_listing_delta`: per-turn, transient agent catalog reminder ─────────
//
// Proves [`ConversationOrchestrator::agent_listing_reminder_message`]:
// - GATE OFF (default): always `None`, and the inline `AgentTool` prompt is
//   unchanged (asserted in `tool-agent` — here we just confirm the orchestrator
//   side stays silent).
// - GATE ON (`LINGXI_AGENT_LIST_IN_MESSAGES=1`, guarded by a process-wide
//   lock): turn-0 full listing + "Available agent types for the Agent tool:"
//   header; a later turn with no new types ⇒ `None`; a newly-added type ⇒ a
//   delta with the "New agent types are now available…" header and ONLY the new
//   line. Also gated on the `Agent` tool's presence + a wired catalog.
#[cfg(test)]
#[path = "conversation/tests/agent_listing_reminder_tests.rs"]
mod agent_listing_reminder_tests;

// ── §F: per-turn, transient `conditional_rules` reminder ──────────────────────
//
// Mirrors the `skill_listing_reminder_tests` template: a `StaticMemoryProvider`
// fixture supplies conditional (`paths:`-gated) `MemoryFile`s, the touched-file
// set is seeded directly into the shared `read_state_map`, and
// `conditional_rules_reminder_message` is asserted to inject the matching rule
// once (with sent-tracking dedup) and skip non-matching / already-sent rules.
#[cfg(test)]
#[path = "conversation/tests/new_diagnostics_reminder_tests.rs"]
mod new_diagnostics_reminder_tests;

#[cfg(test)]
#[path = "conversation/tests/conditional_rules_reminder_tests.rs"]
mod conditional_rules_reminder_tests;

// Nested memory (`k$o` @237714543 fed by `Rop` @237715260): the per-turn
// reminder that surfaces the LINGXI.md governing a TOUCHED file's directory.
#[cfg(test)]
#[path = "conversation/tests/nested_memory_reminder_tests.rs"]
mod nested_memory_reminder_tests;

// P0.1: `relevant_memory_reminder_messages` SURFACING tests.
//
// A `MemoryPrefetch::with_fixed_result` (seeded surfaced set) is wired via
// `with_memory_prefetch`; `start_memory_prefetch` arms the per-turn handle and
// the reminder is asserted to render the `relevant_memories` shape, dedup against
// both `surfaced_memory_paths` (across turns) and `read_state_map` (the SHARED
// P3.2 nested-channel guard), and stay a strict no-op when no prefetch is wired.
#[cfg(test)]
#[path = "conversation/tests/relevant_memory_reminder_tests.rs"]
mod relevant_memory_reminder_tests;

// ── EXPERIMENTAL_SKILL_SEARCH skill-discovery surfacing (default OFF) ─────────
#[cfg(test)]
#[path = "conversation/tests/skill_discovery_reminder_tests.rs"]
mod skill_discovery_reminder_tests;

// ── Finding #80: refusal → fallback-model swap (maybe_swap_to_refusal_fallback) ──
#[cfg(test)]
#[path = "conversation/tests/refusal_fallback_tests.rs"]
mod refusal_fallback_tests;

// ── `persist_message_to_jsonl_with_parent`: explicit parentUuid override ──────
//
// Proves that the streaming executor can parent each tool-result user message to
// the assistant message that REQUESTED the tool (TS `sourceToolAssistantUUID`),
// rather than the linear `last_jsonl_uuid` chain, by calling
// `persist_message_to_jsonl_with_parent(msg, Some(assistant_uuid))`.
//
// Also proves the `None` path (default chain) is byte-identical to the old
// `persist_message_to_jsonl` behaviour.
#[cfg(test)]
#[path = "conversation/tests/transcript_persistence_warning_tests.rs"]
mod transcript_persistence_warning_tests;

#[cfg(test)]
#[path = "conversation/tests/hook_attachment_persistence_tests.rs"]
mod hook_attachment_persistence_tests;

#[cfg(test)]
#[path = "conversation/tests/persist_with_parent_tests.rs"]
mod persist_with_parent_tests;

#[cfg(test)]
#[path = "conversation/tests/prefix_overflow_block_count_tests.rs"]
mod prefix_overflow_block_count_tests;

// #78: unit coverage for the streaming-path "visible output" predicate. The
// streaming driver's thinking-only nudge (`conversation.rs` `Some("end_turn")`
// / `Some("stop_sequence")` / `None` arms) gates on this exact function; the
// batched twin's branch transitions are covered in
// `turn_loop::malformed_and_thinking_only_tests`.
#[cfg(test)]
#[path = "conversation/tests/pumped_visible_text_tests.rs"]
mod pumped_visible_text_tests;

// ============================================================================
// Finding #73: per-turn `todo_reminder` (V1) / `task_reminder` (V2).
//
// Proves [`ConversationOrchestrator::todo_reminder_message`] +
// [`ConversationOrchestrator::bump_reminder_turn_counters`] +
// [`ConversationOrchestrator::note_todo_reminder_tool_call`]:
// - counters increment once per turn and reset on the relevant tool call;
// - the reminder fires only when BOTH counters reach the thresholds, the
//   relevant tool is present, the Brief tool is absent, history is non-empty,
//   and the killswitch is not "off";
// - the body is byte-exact (V1 with/without items; V2 with items) and emitted
//   inside a `<system-reminder>` envelope as a META user message (oracle
//   `Zy([kn({content:o,isMeta:!0})])`, 2.1.238 @296690005).
// The byte-level renderer is additionally covered in `tool_task::reminder::tests`.
// ============================================================================
#[cfg(test)]
#[path = "conversation/tests/todo_reminder_tests.rs"]
mod todo_reminder_tests;

// ── P2-12: post-compact FILE restoration re-reads from disk ───────────────────
//
// `restore_post_compact_attachments` must RE-READ each selected file from disk
// (the binary's `eRg`/`XQn` behaviour) rather than reusing the stale
// `readFileState` snapshot content: a file that changed after its last read is
// restored with FRESH content, a deleted/unreadable file is dropped, and each
// re-read attempt fires the `tengu_post_compact_file_restore_{success,error}`
// telemetry (empty payload).
#[cfg(test)]
#[path = "conversation/tests/post_compact_file_restore_tests.rs"]
mod post_compact_file_restore_tests;

#[cfg(test)]
#[path = "conversation/tests/seed_read_state_from_host_tests.rs"]
mod seed_read_state_from_host_tests;

/// P2-02 (cc2.1.207): `--agent` adopts a main-thread agent — its system prompt
/// becomes the main-loop system prompt (claude-code `nre`, `--system-prompt`
/// still winning) and its `agentType` rides every main-thread lifecycle hook
/// payload (`bde`/`MB()`, base builder `wf` `?? MB()`).
#[cfg(test)]
#[path = "conversation/tests/main_thread_agent_tests.rs"]
mod main_thread_agent_tests;

/// `plansDirectory` (206 `iT`) resolution + within-root containment.
#[cfg(test)]
#[path = "conversation/tests/plans_dir_tests.rs"]
mod plans_dir_tests;

// ── REM-05: the per-turn `edited_text_file` (changed-files) reminders ─────────
//
// Proves [`ConversationOrchestrator::changed_files_reminder_messages`], the
// port of the oracle producer `Izm` @296537358:
// - a tracked file whose on-disk mtime moved and whose bytes differ ⇒ ONE
//   wrapped meta reminder carrying the 2.1.238 copy and the numbered diff;
// - the reminder does NOT repeat next turn (the re-read refreshes the entry);
// - partial reads (`offset`/`limit`) and seeded/partial-view entries never fire;
// - an mtime bump with identical bytes fires nothing (`vNe`);
// - a vanished file drops its read-state entry.
#[cfg(test)]
#[path = "conversation/tests/changed_files_reminder_tests.rs"]
mod changed_files_reminder_tests;

// ── REM-14: the per-turn `memory_update` reminders ───────────────────────────
//
// Proves the two halves of the port of `jzm` @296554545:
// - ENQUEUE: a terminal `dream` task notification queues a pending update
//   (`ConversationOrchestrator::enqueue_memory_updates_from`, reached from
//   `task_notification_reminder_messages_in_turn`, which BOTH turn drivers
//   call through the shared collector in `conversation/drivers/prepare.rs`);
// - DRAIN: `memory_update_reminder_messages` renders it once, with the memdir
//   files that moved and the subset the model is still holding.
#[cfg(test)]
#[path = "conversation/tests/memory_update_reminder_tests.rs"]
mod memory_update_reminder_tests;

// ── REM-09: the goal check-in deferral pass ──────────────────────────────────
//
// Proves [`ConversationOrchestrator::goal_checkin_pass`], reached from
// `fire_stop_hooks` (which computes it from the same `background_tasks`
// snapshot the Stop payload carries) and gating whether the goal's Stop hook
// disposition is consulted at all this turn.
#[cfg(test)]
#[path = "conversation/tests/goal_checkin_wiring_tests.rs"]
mod goal_checkin_wiring_tests;
#[path = "conversation/tests/goal_cleared_reason_tests.rs"]
mod goal_cleared_reason_tests;
#[path = "conversation/tests/goal_evaluated_analytics_tests.rs"]
mod goal_evaluated_analytics_tests;

// ── REM-10: the periodic `tool_search_usage_reminder` ────────────────────────
//
// Proves [`ConversationOrchestrator::tool_search_usage_reminder_message`], the
// port of `Uzm` @296553134. The reminder is INERT by default (its upstream gate
// is a GrowthBook payload absent from a stock install), so these tests drive the
// port's env stand-in under a lock.
#[cfg(test)]
#[path = "conversation/tests/tool_search_usage_reminder_tests.rs"]
mod tool_search_usage_reminder_tests;

#[path = "conversation/tests/scheduled_turn_tests.rs"]
mod scheduled_turn_tests;

#[path = "conversation/tests/turn_admission_cancel_tests.rs"]
mod turn_admission_cancel_tests;
