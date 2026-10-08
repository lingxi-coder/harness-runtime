//! Inner turn-by-turn loop helpers. Private to `ConversationOrchestrator`.

use crate::conversation::{classify_api_error, ConversationOrchestrator, ModelCallPath};
use crate::error::OrchestratorError;
use hooks::attachment::HookPublicationGuard;
use lingxi_core::types::{ContentBlock, ConversationMessage, MessageId, ToolUseId};
use llm_runtime::LlmError;

/// Construct large phase futures outside the caller's poll stack frame.
/// Passing the factory also avoids a large temporary before heap allocation.
pub(crate) fn boxed_turn_future<'a, T, F>(
    create: impl FnOnce() -> F,
) -> futures::future::BoxFuture<'a, T>
where
    F: std::future::Future<Output = T> + Send + 'a,
{
    Box::pin(create())
}
pub(crate) mod api_recovery;
mod batch_hooks;
mod error_reporting;
mod mod_batched_step;
mod mod_fs_ancestors;
mod mod_session_messages;
mod response;
pub(crate) mod tool_dispatch;
mod tool_results;
mod visible_response;
pub(crate) use api_recovery::call_api_with_ptl_recovery;
pub(crate) use api_recovery::PtlCallOutcome;
pub(crate) use batch_hooks::append_tool_injected_messages;
pub(crate) use batch_hooks::apply_model_context_modifiers;
pub(crate) use batch_hooks::apply_model_context_state;
#[cfg(test)]
use batch_hooks::post_tool_batch_identity;
pub(crate) use batch_hooks::resolve_model_context_modifier;
pub(crate) use batch_hooks::run_post_tool_batch_hooks;
pub(crate) use batch_hooks::run_post_tool_batch_hooks_after_turn_end;
pub(crate) use batch_hooks::{PostToolBatchDispatch, PostToolBatchOutcome};
pub(crate) use error_reporting::clear_goal_after_unrecoverable_error;
#[cfg(test)]
use error_reporting::goal_clear_bucket;
#[cfg(test)]
use error_reporting::goal_cleared_after_error_message;
pub(crate) use error_reporting::is_carveout_propagated;
use error_reporting::maybe_checkpoint_on_rate_limit;
#[cfg(test)]
use error_reporting::refusal_explanation_clause;
pub(crate) use error_reporting::surface_api_error_notice;
pub(crate) use error_reporting::surface_model_error;
pub(crate) use error_reporting::surface_prompt_too_long;
pub(crate) use error_reporting::surface_rapid_refill_thrashing;
pub(crate) use error_reporting::surface_terminal_api_error;
pub(crate) use error_reporting::terminal_api_error_text;
#[cfg(test)]
use error_reporting::GoalClearBucket;
pub(crate) use error_reporting::GoalClearReason;
#[cfg(test)]
use error_reporting::GOAL_CLEAR_CONDITION_WIDTH;
use response::handle_malformed_tool_use;
use response::handle_max_output_tokens;
use response::handle_thinking_only;
use response::has_visible_text;
pub(crate) use response::prior_assistant_used_structured_output;
pub(crate) use response::translate_response_blocks;
#[cfg(test)]
use tool_dispatch::accumulate_code_change;
pub(crate) use tool_dispatch::dispatch_streaming_tool_use;
#[cfg(test)]
pub(crate) use tool_dispatch::dispatch_tool_uses;
pub(crate) use tool_dispatch::dispatch_tool_uses_tracked;
pub(crate) use tool_dispatch::dispatch_tool_uses_tracked_deferred;
pub(crate) use tool_dispatch::generation_bound_mod_session_context;
pub(crate) use tool_dispatch::normalize_lexically;
#[cfg(test)]
use tool_dispatch::rule_decision_otel_source;
#[cfg(test)]
use tool_dispatch::tool_denial_kind;
pub(crate) use tool_dispatch::ToolUseDispatchFacts;
pub(crate) use tool_dispatch::{dispatch_streaming_tool_use_owned, streaming_tool_context_base};
pub(crate) use tool_dispatch::{
    DeferredToolDispatch, ToolResultFramePublication, ToolResultPublication,
};
#[cfg(test)]
use tool_results::apply_tool_result_persistence;
#[cfg(test)]
use tool_results::apply_tool_result_persistence_with_process_output;
#[cfg(test)]
use tool_results::bash_image_tool_result_blocks;
#[cfg(test)]
use tool_results::image_tool_result_blocks;
#[cfg(test)]
use tool_results::process_output_file_from_data;
#[cfg(test)]
use tool_results::tool_result_size;
#[cfg(test)]
use tool_results::tool_result_to_model_text;

/// Registry name of the worktree-creation tool (`tool_worktree::ENTER_TOOL_NAME`).
/// A successful invocation of this tool is the port's sole worktree-creation
/// path, so it is where the turn loop fires the `WorktreeCreate` hook. Held as a
/// literal (not imported) so `orchestrator` keeps no dependency on `tool-worktree`.
const ENTER_WORKTREE_TOOL_NAME: &str = "EnterWorktree";

/// Registry name of the subagent-spawning tool (`tools/agent` `AGENT_TOOL_NAME`).
/// A completed dispatch of this
/// tool means the spawned subagent's loop has stopped, so it is where the turn
/// loop fires the `SubagentStop` hook. Held as literals (not imported) so
/// `orchestrator` keeps no dependency on `tools/agent` — same precedent as
/// `ENTER_WORKTREE_TOOL_NAME`.
const AGENT_TOOL_NAME: &str = "Agent";

/// #40: apply a hook's folded `terminalSequence` (claude-code `szn`, BIN off
/// 205755390) via the allowlist validator
/// ([`hooks::terminal_seq::validate_terminal_sequence`], the `NEo` port):
/// - REJECT → warn (claude-code's byte-faithful message; the observable half).
/// - ACCEPT → forward the validated string through the
///   [`OutputStream::emit_terminal_sequence`] seam (`BEo`, #6 main-loop parity).
///   The orchestrator holds no TTY (the TUI owns the terminal in a separate
///   process), so non-interactive hosts (print/CLI/tests) keep the no-op.
///
/// Strict no-op when `seq` is `None`.
async fn apply_terminal_sequence(
    orch: &ConversationOrchestrator,
    hook_name: &str,
    seq: Option<&str>,
    publication_fence: Option<std::sync::Arc<dyn HookPublicationGuard>>,
) {
    let Some(seq) = seq else {
        return;
    };
    match hooks::terminal_seq::validate_terminal_sequence(seq) {
        Some(validated) => {
            // Forward the validated, BEL-normalized sequence to the host's
            // terminal-write seam (claude-code `BEo`). Default no-op off the TUI.
            if let Some(fence) = publication_fence {
                fence
                    .publish_if_current(Box::pin(orch.output.emit_terminal_sequence(&validated)))
                    .await;
            } else {
                orch.output.emit_terminal_sequence(&validated).await;
            }
        }
        None => {
            tracing::warn!(
                "Hook {hook_name} returned a terminalSequence that was rejected by the allowlist (only OSC 0/1/2/9/99/777 and BEL are permitted, and OSC 9 bodies may not begin with a digit unless in the 9;4 progress form)"
            );
        }
    }
}

// Tests for this module live in the sibling `turn_loop_test.rs`.
#[cfg(test)]
#[path = "turn_loop_test.rs"]
mod turn_loop_test;

/// Maximum number of consecutive `max_tokens` recovery nudges before the
/// turn loop gives up and surfaces the `max_tokens` `stop_reason`. 1:1 with TS
/// `query.ts:164` `MAX_OUTPUT_TOKENS_RECOVERY_LIMIT = 3`.
pub(crate) const MAX_OUTPUT_TOKENS_RECOVERY_LIMIT: u32 = 3;

/// Escalated output-token cap for the single-shot 8k→64k retry. 1:1 with TS
/// `utils/context.ts:25` `ESCALATED_MAX_TOKENS = 64_000`.
/// [`RecoveryState::max_output_tokens_override`] carries this value into the
/// next `messages_create` request.
pub(crate) const ESCALATED_MAX_TOKENS: u32 = 64_000;

fn select_visible_response_request_history<'a>(
    visible_response_id: &str,
    physical_request_histories: impl Iterator<Item = (&'a str, &'a [ConversationMessage])>,
    final_response: bool,
    final_request_history: &'a [ConversationMessage],
    final_history_source: Option<mod_batched_step::RequestHistorySource>,
    turn_step_input_history: &'a [ConversationMessage],
) -> (
    Vec<ConversationMessage>,
    mod_batched_step::RequestHistorySource,
) {
    let mut matching = physical_request_histories.filter(|(response_id, _)| {
        !visible_response_id.is_empty() && *response_id == visible_response_id
    });
    if let Some((_, history)) = matching.next() {
        if matching.next().is_none() {
            return (
                history.to_vec(),
                mod_batched_step::RequestHistorySource::PhysicalRequest,
            );
        }
    }

    if final_response {
        return (
            final_request_history.to_vec(),
            final_history_source.unwrap_or(
                mod_batched_step::RequestHistorySource::TurnStepInputForSyntheticResponse,
            ),
        );
    }

    (
        turn_step_input_history.to_vec(),
        mod_batched_step::RequestHistorySource::TurnStepInputForSyntheticResponse,
    )
}

/// The byte-exact meta "resume directly" nudge injected as a user message on a
/// `max_tokens` `stop_reason`. 1:1 with TS `query.ts:1226-1227` (note the U+2014
/// em-dash in "directly —"). Concatenating these two string literals — exactly
/// as TS does — yields one contiguous line with NO separator between them.
pub(crate) const MAX_OUTPUT_TOKENS_RECOVERY_NUDGE: &str = concat!(
    "Output token limit hit. Resume directly — no apology, no recap of what you were doing. ",
    "Pick up mid-thought if that is where the cut happened. Break remaining work into smaller pieces.",
);

/// Non-interactive main (`-p`) truncated-after-output recovery nudge
/// (cc 2.1.263 `tZo` / `query_truncated_response_recovery`).
pub(crate) const TRUNCATED_RESPONSE_RECOVERY_NUDGE_MAIN: &str = concat!(
    "Your response above was cut off mid-stream. Resume directly from where it stops — no apology, no recap. ",
    "If none of it survived, answer the request from the start.",
);

/// Subagent truncated-after-output recovery nudge.
pub(crate) const TRUNCATED_RESPONSE_RECOVERY_NUDGE_SUBAGENT: &str = concat!(
    "Your response above was cut off mid-stream and only your next message is delivered. ",
    "Write the complete response again from the start — no apology, no mention of the cut-off.",
);

/// `tZo`: recover a truncated-after-output api-error for subagents and for
/// non-interactive (`-p`) main. Interactive main ends (the notice is enough).
#[must_use]
pub(crate) fn truncated_response_recovery_eligible(query_source: &str, interactive: bool) -> bool {
    let src = crate::config::sanitize_query_source(query_source);
    truncated_response_recovery_is_subagent(src)
        || (!interactive && (src.starts_with("repl_main_thread") || src == "sdk"))
}

/// cc 2.1.263 `ji`: `agent:*` and `hook_agent` are subagent queries.
/// Current native source classification has no `subagent` compatibility alias.
pub(crate) fn truncated_response_recovery_is_subagent(query_source: &str) -> bool {
    query_source.starts_with("agent:") || query_source == "hook_agent"
}

/// Byte-exact `isMeta` retry message pushed when a `PermissionDenied` hook
/// returns `{retry: true}` on the gated auto-mode classifier-deny path. 1:1 with
/// claude-code `toolExecution.ts:1096`. DORMANT in the external build — the
/// retry path is double-gated (see [`PERMISSION_DENIED_RETRY_MESSAGE`]'s only
/// emit site in the deny arm), so this string is never produced on the normal
/// deny path. When it does fire it is built as a META user message
/// ([`ConversationMessage::user_meta`]), matching CC's `isMeta:!0`.
pub(crate) const PERMISSION_DENIED_RETRY_MESSAGE: &str =
    "The PermissionDenied hook indicated you may retry this tool call.";

/// Clean retry nudge from cc 2.1.263 `ZZe` (src_158021603.js).
/// `Oer` unconditionally drops the malformed attempt before appending it.
pub(crate) const MALFORMED_TOOL_USE_RETRY_NUDGE: &str =
    "The previous response failed to produce a valid tool call. Please retry the tool call now.";

/// Byte-exact NON-meta message emitted on the SECOND malformed-tool-use failure
/// (the retry also produced no `tool_use` block): the turn terminates as
/// completed. 1:1 with claude-code v2.1.183 (`bin/claude.exe` offset
/// ~202946360, the `tc({content:...})` terminal branch).
pub(crate) const MALFORMED_TOOL_USE_RETRY_FAILED: &str =
    "The model's tool call could not be parsed (retry also failed).";

/// Byte-exact meta nudge injected when the model returns an `end_turn` /
/// `stop_sequence` response with NO visible text (thinking-only output) and it
/// has not yet been nudged this turn. 1:1 with claude-code v2.1.183
/// (`bin/claude.exe` offset ~202947000). Injected as a META user message
/// ([`ConversationMessage::user_meta`]), matching CC's `isMeta:!0` — it persists
/// with top-level `isMeta:true` and is skipped by title/first-prompt extraction.
pub(crate) const THINKING_ONLY_NUDGE: &str = "[Your previous response had no visible output. Please continue and produce a user-visible response.]";

/// Byte-exact bare content returned as `is_error:true` `tool_result` when the
/// user-interrupt signal fires BEFORE a tool executes — the pre-cancellation
/// guard in `dispatch_tool_uses_tracked`. 1:1 with claude-code
/// `toolExecution.ts:413-453` `CANCEL_MESSAGE` (utils/messages.ts:210).
const CANCEL_MESSAGE: &str = "The user doesn't want to take this action right now. STOP what you are doing and wait for the user to tell you how to proceed.";

/// Per-conversation recovery bookkeeping carried by the turn drivers in
/// `conversation.rs` and threaded `&mut` into [`execute_one_turn_with_recovery`].
///
/// Mirrors the TS recovery sub-state on `query.ts`'s loop `State`
/// (`maxOutputTokensRecoveryCount`, `maxOutputTokensOverride`). One instance
/// lives per `try_run_turn` / `try_run_turn_streaming` invocation; it persists
/// the nudge count ACROSS turn-steps so the 3-retry limit is consecutive.
#[derive(Debug, Default)]
// The `max_output_tokens_*` prefix is the parity-faithful name for all three
// fields (TS `maxOutputTokens*`); the shared prefix is intentional.
#[allow(clippy::struct_field_names)]
pub(crate) struct RecoveryState {
    /// How many consecutive `max_tokens` nudges have been injected this
    /// conversation. Capped at [`MAX_OUTPUT_TOKENS_RECOVERY_LIMIT`]; once it
    /// reaches the limit the next `max_tokens` ends the turn.
    pub(crate) max_output_tokens_recovery_count: u32,
    /// When `Some(n)`, the NEXT API call uses `n` as its output-token cap
    /// (REC.A1 escalated retry). The turn loop TAKEs it (one-shot) before each
    /// call via [`crate::OrchestratorApiClient::messages_create`], so
    /// it never leaks past the single escalated retry.
    pub(crate) max_output_tokens_override: Option<u32>,
    /// Whether the 8k→64k escalation has already fired this recovery episode
    /// (TS gates the single-shot retry on the override being unset; we use a
    /// separate flag because the override is TAKEN per call). Reset alongside
    /// [`Self::max_output_tokens_recovery_count`].
    pub(crate) max_output_tokens_escalated: bool,
    /// #77: whether a malformed-tool-use retry (`stop_reason == "tool_use"` with
    /// zero `tool_use` blocks) has already fired this turn. Mirrors claude-code's
    /// `transition.reason === "malformed_tool_use_retry"` guard so the SECOND
    /// such failure terminates instead of looping. NOT reset by
    /// [`Self::reset_max_output_tokens_recovery`] — it is a per-turn one-shot
    /// independent of the max-output-tokens escalation episode.
    #[allow(clippy::struct_field_names)]
    pub(crate) malformed_tool_use_retried: bool,
    /// #78: whether the thinking-only nudge (an `end_turn`/`stop_sequence`
    /// response with no visible text) has already fired this turn. Mirrors
    /// claude-code's `thinkingOnlyNudged` loop-state flag.
    #[allow(clippy::struct_field_names)]
    pub(crate) thinking_only_nudged: bool,
}

impl RecoveryState {
    /// Reset the `max_output_tokens` recovery bookkeeping to begin a fresh
    /// escalation episode: zero the consecutive nudge count, drop any armed
    /// escalation override, and re-arm the 8k→64k single-shot. 1:1 with the TS
    /// loop-state resets that set `maxOutputTokensRecoveryCount: 0` +
    /// `maxOutputTokensOverride: undefined` on a continuation — the token-budget
    /// continuation (`query.ts:1332`) AND the Stop-hook blocking continuation
    /// (RECOV.4, `query.ts:1291`).
    pub(crate) fn reset_max_output_tokens_recovery(&mut self) {
        self.max_output_tokens_recovery_count = 0;
        self.max_output_tokens_override = None;
        self.max_output_tokens_escalated = false;
    }
}

/// What one turn step decided.
pub(crate) enum TurnStepOutcome {
    /// Continue the loop (e.g. model returned `tool_use`).
    Continue,
    /// Loop should terminate.
    Ended {
        final_message_id: MessageId,
        stop_reason: String,
        allow_budget_continuation: bool,
        /// True only when a successful tool result requested this end. Stop
        /// hooks still run, but their block/prevent dispositions are advisory
        /// and cannot re-enter the model loop.
        tool_requested_end: bool,
    },
}

/// Execute one `messages_create_non_stream` round-trip + tool dispatches.
///
/// `system` is the assembled system prompt for the conversation (built
/// once by `ConversationOrchestrator::try_run_turn`). It is passed
/// through to every API round-trip in the conversation, NOT re-built
/// per turn-step — the prompt is stable across the conversation lifetime
/// (see M5-03 plan "Out of scope" note: SSE M5-04 will not re-assemble
/// per turn-step either).
// Retained as a test-only convenience: the legacy no-recovery shim. As of #2
// (main-loop parity) the cancelable REPL driver no longer uses it — it now
// calls the recovery-aware [`execute_one_turn_with_recovery_tracked`] like the
// main batched [`ConversationOrchestrator::run_turn`] loop. The in-file
// `#[cfg(test)]` suites still drive this clean-signature wrapper, so it is kept
// (not `#[cfg(test)]`-gated, to preserve the intra-doc links from the live
// `_tracked` function).
#[allow(dead_code)]
pub(crate) async fn execute_one_turn(
    orch: &ConversationOrchestrator,
    system: Option<&lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput>,
) -> Result<TurnStepOutcome, OrchestratorError> {
    // Backward-compatible shim: no recovery state → legacy disposition
    // (any non-`end_turn` stop_reason Continues). Used by the in-file tests.
    // The recovery-aware drivers call [`execute_one_turn_with_recovery`] with a
    // live `RecoveryState`.
    execute_one_turn_with_recovery(orch, system, None).await
}

/// Recovery-aware twin of [`execute_one_turn`].
///
/// When `recovery` is `Some`, a `max_tokens` `stop_reason` triggers the A1
/// multi-turn nudge: while the consecutive recovery count is below
/// [`MAX_OUTPUT_TOKENS_RECOVERY_LIMIT`], a byte-exact "resume directly" meta
/// user message ([`MAX_OUTPUT_TOKENS_RECOVERY_NUDGE`]) is appended to history,
/// the counter is incremented, and the step returns
/// [`TurnStepOutcome::Continue`] (1:1 with TS `query.ts:1223-1252`). When the
/// count has reached the limit, the turn ends with `stop_reason = "max_tokens"`
/// (TS `query.ts:1254-1255` surfaces the withheld error). When `recovery` is
/// `None`, the `max_tokens` path falls through to the legacy disposition
/// (Continue), preserving the legacy disposition.
///
/// Test-only as of #2: the production drivers all call the `_tracked` variant
/// directly. Retained (not `#[cfg(test)]`) so intra-doc links resolve in the
/// normal doc build.
#[allow(dead_code)]
pub(crate) async fn execute_one_turn_with_recovery(
    orch: &ConversationOrchestrator,
    system: Option<&lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput>,
    recovery: Option<&mut RecoveryState>,
) -> Result<TurnStepOutcome, OrchestratorError> {
    // Drop the per-call output-token count (A3 callers use the `_tracked`
    // variant). Preserves the historical signature for every existing caller.
    Ok(
        execute_one_turn_with_recovery_tracked(orch, system, recovery, None)
            .await?
            .0,
    )
}

/// A3 twin of [`execute_one_turn_with_recovery`] that ALSO returns this turn
/// step's output-token count (`response.usage.output_tokens`).
///
/// The token-budget continuation loop (`conversation.rs`) accumulates these
/// into `global_turn_tokens` and feeds the running total to
/// [`crate::token_budget::check_token_budget`] — mirroring TS
/// `getTurnOutputTokens()`. The plain
/// [`execute_one_turn_with_recovery`] wrapper drops the count so existing
/// callers (the cancelable REPL driver + in-file tests) are unchanged.
pub(crate) fn execute_one_turn_with_recovery_tracked<'a>(
    orch: &'a ConversationOrchestrator,
    system: Option<&'a lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput>,
    recovery: Option<&'a mut RecoveryState>,
    mod_step: Option<(&'a str, u32)>,
) -> futures::future::BoxFuture<'a, Result<(TurnStepOutcome, u64), OrchestratorError>> {
    // A shared heap/type boundary keeps the turn state out of caller futures
    // and stops Send checks from expanding through every streaming wrapper.
    Box::pin(execute_one_turn_with_recovery_tracked_impl(
        orch, system, recovery, mod_step,
    ))
}

#[allow(clippy::too_many_lines)]
async fn execute_one_turn_with_recovery_tracked_impl(
    orch: &ConversationOrchestrator,
    system: Option<&lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput>,
    mut recovery: Option<&mut RecoveryState>,
    mod_step: Option<(&str, u32)>,
) -> Result<(TurnStepOutcome, u64), OrchestratorError> {
    let cancel = crate::native_computer::current_turn_cancel();
    crate::native_computer::cancel_before_effects(crate::native_computer::recover_before_request(
        orch,
    ))
    .await?;
    let is_regular_user_prompt = orch.regular_user_prompt_for_model_step().await;
    let prepared = crate::native_computer::cancel_before_effects(boxed_turn_future(|| {
        orch.prepare_turn_step(
            ModelCallPath::Batched,
            system,
            true,
            is_regular_user_prompt,
            cancel.as_ref(),
        )
    }))
    .await?;
    let skip_global_cache_for_system_prompt = prepared.skip_global_cache_for_system_prompt;
    let mut history_snapshot = prepared.snapshot;
    let mut model = prepared.model;
    let mut model_profile = prepared.model_profile;
    // The unmodified `w` supplied to a turn.step is the correct request-history
    // fallback only for a fully synthetic Mod response. Successful physical
    // requests carry their own post-PTL/compact message snapshot below.
    let outgoing_history_rewriter = prepared.outgoing_history_rewriter;
    let mut turn_reminders = prepared.turn_reminders;
    let mut guarded_async_hook_reminders = prepared.guarded_async_hook_reminders;
    let context_announcements = prepared.context_announcements;
    let tools = prepared.wire_tools;
    let deferred_tools_reminder = prepared.deferred_reminder;
    let date_change_reminder = prepared.date_change_reminder;

    // REC.A1: consume the one-shot escalated `max_tokens` override (armed by a
    // prior `max_tokens` recovery via `handle_max_output_tokens`). TAKE it so it
    // applies to EXACTLY this call and never leaks to the next turn.
    let max_tokens_override = recovery
        .as_deref_mut()
        .and_then(|r| r.max_output_tokens_override.take());
    // #5: wall-clock the API round-trip (incl. any in-adapter retries + the PTL
    // reactive-recovery tail) so the CostTracker records a REAL duration instead
    // of `Duration::ZERO`. Paired with `orch.api.last_retry_count()` below.
    let mut cost_scope = orch
        .model_runtime
        .cost_scope
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    if cost_scope.is_none() {
        if let Some(tracker) = orch.model_runtime.cost_tracker.as_ref() {
            let session_id = orch.session.lock().await.session_id;
            cost_scope = Some(tracker.session_scope(session_id));
        }
    }
    if let Some(scope) = cost_scope.as_ref() {
        scope.preflight().await.map_err(|error| {
            OrchestratorError::Internal(format!("cost durability preflight failed: {error}"))
        })?;
    }
    let api_call_started = std::time::Instant::now();
    // tengu_api_success `messageCount:n` / `messageTokens:r`: capture from the
    // input snapshot BEFORE it is moved into `call_api_with_ptl_recovery`.
    let api_success_message_count = u32::try_from(history_snapshot.len()).unwrap_or(u32::MAX);
    let api_success_message_tokens =
        compaction::grouping::estimate_tokens_for_range(&history_snapshot);
    let mut requested_model = true;
    let mut physical_responses = Vec::new();
    let mut preceding_responses = Vec::new();
    let mut mod_response_request_history_source = None;
    let mod_host = if mod_step.is_some() {
        if let Some(registry) = &orch.lifecycle_runtime.hook_registry {
            registry
                .read()
                .await
                .mod_host()
                .filter(|host| host.has_event("turn.step"))
        } else {
            None
        }
    } else {
        None
    };
    crate::prompt::async_hook_response::retain_current_async_hook_reminders(
        &mut history_snapshot,
        &mut turn_reminders,
        &mut guarded_async_hook_reminders,
    );
    let turn_step_input_history = history_snapshot.clone();
    let computer_projection =
        crate::native_computer::prepare_projection(orch, &model, model_profile.as_deref(), &tools)
            .await?;
    let api_result = llm_runtime::computer::scope_computer_request(
        computer_projection,
        crate::native_computer::cancel_before_effects(boxed_turn_future(|| async {
            if let (Some(host), Some((turn_id, index))) = (mod_host, mod_step) {
                let effort = orch
                    .model_runtime
                    .current_effort
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                let answer = mod_batched_step::dispatch(
                    orch,
                    host,
                    mod_batched_step::Request {
                        system: system.cloned(),
                        skip_global_cache_for_system_prompt,
                        model: model.clone(),
                        profile: model_profile.clone(),
                        history: history_snapshot,
                        rewriter: outgoing_history_rewriter,
                        tools: tools.clone(),
                        max_tokens: max_tokens_override,
                        deferred: deferred_tools_reminder,
                        date_change: date_change_reminder,
                        reminders: turn_reminders.clone(),
                        guarded_async_hook_reminders: guarded_async_hook_reminders.clone(),
                        context_announcements: context_announcements.clone(),
                        cost_scope: cost_scope.clone(),
                        turn_id: turn_id.to_owned(),
                        index,
                        effort,
                        api_success_message_count,
                        api_success_message_tokens,
                    },
                )
                .await;
                match answer {
                    Ok(answer) => {
                        model = answer.model;
                        model_profile = answer.profile;
                        requested_model = answer.requested_model;
                        physical_responses = answer.physical_responses;
                        preceding_responses = answer.preceding_responses;
                        mod_response_request_history_source =
                            answer.response_request_history_source;
                        Ok(answer.outcome)
                    }
                    Err(error) => Err(error),
                }
            } else {
                let fallback_context =
                    match lingxi_core::host::refusal_driver::current_fallback_target() {
                        Some(context) => context,
                        None => {
                            crate::query_model::fallback_target_context(orch, &model, None).await
                        }
                    };
                crate::server_fallback::record_request_route(crate::query_model::ModelRoute {
                    model: model.clone(),
                    profile: model_profile.clone(),
                });
                crate::prompt::async_hook_response::retain_current_async_hook_reminders(
                    &mut history_snapshot,
                    &mut turn_reminders,
                    &mut guarded_async_hook_reminders,
                );
                lingxi_core::host::refusal_driver::scope_fallback_target(
                    fallback_context,
                    call_api_with_ptl_recovery(
                        orch,
                        system,
                        skip_global_cache_for_system_prompt,
                        &model,
                        model_profile.as_deref(),
                        history_snapshot,
                        outgoing_history_rewriter,
                        tools.clone(),
                        max_tokens_override,
                        deferred_tools_reminder,
                        date_change_reminder,
                        turn_reminders,
                        &mut guarded_async_hook_reminders,
                        &context_announcements,
                        cost_scope.as_ref(),
                    ),
                )
                .await
            }
        })),
    )
    .await;
    orch.persist_thinking_signature_strip_latch().await;
    let (response, final_request_history) = match api_result {
        Ok(outcome) => match outcome {
            PtlCallOutcome::Response {
                response,
                request_history,
            } => (response, request_history),
            PtlCallOutcome::PromptTooLong => {
                let assistant_id = surface_prompt_too_long(orch).await;
                // SLASH-04: `w4v` maps `prompt_too_long` to `context_limit`.
                clear_goal_after_unrecoverable_error(orch, GoalClearReason::ContextLimit).await;
                return Ok((
                    TurnStepOutcome::Ended {
                        final_message_id: assistant_id,
                        stop_reason: "prompt_too_long".to_string(),
                        allow_budget_continuation: false,
                        tool_requested_end: false,
                    },
                    0,
                ));
            }
            PtlCallOutcome::BlockingLimit => {
                // PROACTIVE blocking-limit preempt: surface the prompt-too-long
                // message (its api-error field is `invalid_request`, like the
                // binary's `Ol({...,error:"invalid_request"})`) but end the turn with
                // the DISTINCT terminal reason `"blocking_limit"` — the binary's
                // `{reason:"blocking_limit"}` (offset ~208021400), kept separate from
                // the reactive-exhausted `prompt_too_long` so SDK/stream-json
                // consumers categorize the two preempt origins distinctly.
                let assistant_id = surface_prompt_too_long(orch).await;
                // SLASH-04: `w4v` maps `blocking_limit` to `context_limit`.
                clear_goal_after_unrecoverable_error(orch, GoalClearReason::ContextLimit).await;
                return Ok((
                    TurnStepOutcome::Ended {
                        final_message_id: assistant_id,
                        stop_reason: "blocking_limit".to_string(),
                        allow_budget_continuation: false,
                        tool_requested_end: false,
                    },
                    0,
                ));
            }
            PtlCallOutcome::RapidRefillBreaker => {
                // #54 reactive trip: surface the thrashing message (api-error field
                // `invalid_request`, matching the binary `Ol({...,error:"invalid_request"})`)
                // but end the turn with the terminal reason `"rapid_refill_breaker"`
                // — the binary's loop returns `{reason:"rapid_refill_breaker"}` even
                // though the assistant MESSAGE carries `error:"invalid_request"`
                // (`bin/claude.exe` offset ~208016504; terminal-reason enum lists
                // `rapid_refill_breaker`, never `invalid_request`).
                let assistant_id = surface_rapid_refill_thrashing(orch).await;
                // SLASH-04: `w4v` maps `rapid_refill_breaker` to `context_limit`.
                clear_goal_after_unrecoverable_error(orch, GoalClearReason::ContextLimit).await;
                return Ok((
                    TurnStepOutcome::Ended {
                        final_message_id: assistant_id,
                        stop_reason: "rapid_refill_breaker".to_string(),
                        allow_budget_continuation: false,
                        tool_requested_end: false,
                    },
                    0,
                ));
            }
        },
        // #10: a model/runtime error that escaped the API layer is NOT a hard
        // failure (faithful port of `query.ts:955-997` catch → `model_error`).
        // PROPAGATE the carve-outs that have dedicated downstream handling
        // (RateLimited → wrapper rate-limit enrichment; Overloaded /
        // RepeatedOverloaded → the "Repeated 529" surface); surface EVERYTHING
        // else gracefully as an `isApiErrorMessage` assistant message + end the
        // turn with `reason:"model_error"` (no Stop/StopFailure hooks — the catch
        // path runs neither). 0 output tokens.
        Err(e) if is_carveout_propagated(&e) => {
            // SC-02: the `rate_limited` half of the rate-limit resume
            // checkpoint. This is the port's twin of the oracle's REPL trigger
            // (@306528240: the `vut` rate-limit callback fires
            // `performRateLimitCheckpoint({todos, trigger:"rate_limited"})`
            // fire-and-forget) — the point at which a rate-limited response
            // ends the turn is where the user's in-progress files are worth
            // snapshotting.
            crate::server_fallback::flush_pending_notice(orch).await;
            maybe_checkpoint_on_rate_limit(orch, &e).await;
            return Err(e);
        }
        Err(e) => {
            // Classify the TYPED error into the api-error envelope (`Flp`/`KNn`)
            // BEFORE consuming it for the verbatim error text. The rendered
            // message stays `e.to_string()` (`createAssistantAPIErrorMessage`
            // renders content verbatim); the envelope adds `error`/`apiErrorStatus`.
            let env = classify_api_error(&e);
            // Content is rendered verbatim (`e.to_string()`) EXCEPT a 413
            // `request_too_large` (accumulated images/attachments), which the
            // 2.1.212 handler renders with the byte-exact `$Vi()` notice.
            let content = match &e {
                OrchestratorError::ApiCall(LlmError::RequestTooLarge)
                | OrchestratorError::Streaming(LlmError::RequestTooLarge) => {
                    crate::conversation::request_too_large_notice(orch.prompt_is_interactive())
                }
                _ => e.to_string(),
            };
            let error_kind = env.error;
            crate::server_fallback::flush_pending_notice(orch).await;
            let assistant_id = surface_model_error(orch, &content, env).await;
            orch.mark_mod_turn_error();
            // SLASH-04: this arm is the oracle's `api_error` reason (see the
            // NAMING NOTE on `GoalClearBucket`), so it is classified by
            // errorKind, not by the port's `model_error` spelling.
            // `mal(Wr)`'s `apiErrorIsTransient` field has no port analogue; the
            // `overloaded`/`server_error` half of the predicate is subsumed by
            // those categories' own no-clear arms.
            clear_goal_after_unrecoverable_error(
                orch,
                GoalClearReason::ApiError {
                    error_kind,
                    is_transient: false,
                },
            )
            .await;
            return Ok((
                TurnStepOutcome::Ended {
                    final_message_id: assistant_id,
                    stop_reason: "model_error".to_string(),
                    allow_budget_continuation: false,
                    tool_requested_end: false,
                },
                0,
            ));
        }
    };

    let mut visible_responses = preceding_responses;
    visible_responses.push(*response);
    let mut declined_server_fallback = None;
    for visible in &mut visible_responses {
        let server_fallback_events = visible.server_fallback_events();
        for info in &server_fallback_events {
            if !matches!(info.event.reason.as_str(), "refusal" | "sticky") {
                continue;
            }
            match crate::server_fallback::handle(orch, info, false).await? {
                crate::server_fallback::ServerFallbackAdmission::Applied => {
                    if matches!(info.event.reason.as_str(), "refusal" | "sticky") {
                        visible.model =
                            lingxi_core::host::refusal_server_control::resolve_received_model(
                                Some(&info.lane.model),
                                &info.event.to_model,
                            );
                    }
                }
                crate::server_fallback::ServerFallbackAdmission::Declined => {
                    declined_server_fallback = Some(info.clone());
                    break;
                }
            }
        }
        if declined_server_fallback.is_some() {
            break;
        }
    }
    let response = visible_responses
        .last()
        .expect("a successful turn step contains a visible assistant response");

    // A Mod may call `next` more than once and replace the visible answer.
    // Keep each paid provider response separate from the outer history record.
    struct MeteredCall<'a> {
        response: &'a llm_runtime::HistoryResponse,
        model: &'a str,
        profile: Option<&'a str>,
        duration: std::time::Duration,
        retries: u32,
        request_id: Option<&'a str>,
    }
    let metered_calls: Vec<_> = if physical_responses.is_empty() {
        requested_model
            .then(|| MeteredCall {
                response,
                model: &model,
                profile: model_profile.as_deref(),
                duration: api_call_started.elapsed(),
                retries: orch.api.last_retry_count(),
                request_id: None,
            })
            .into_iter()
            .collect()
    } else {
        physical_responses
            .iter()
            .map(|call| MeteredCall {
                response: &call.response,
                model: &call.model,
                profile: call.profile.as_deref(),
                duration: call.duration,
                retries: call.retries,
                request_id: call.request_id.as_deref(),
            })
            .collect()
    };
    let metered_response = metered_calls.last().map_or(response, |call| call.response);

    // Transfer each directly dispatched provider response into its session-owned
    // accounting supervisor. Mod source calls settle at the physical-response
    // boundary in `mod_batched_step::PhysicalStepResponse::capture`.
    let owned_cost_responses: Vec<_> = metered_calls
        .iter()
        .filter_map(|call| {
            if !physical_responses.is_empty() {
                return None;
            }
            orch.model_runtime.cost_tracker.as_ref().map(|_| {
                let usage = crate::cost_wiring::llm_usage_to_cost_usage(&call.response.usage);
                let cache_read = call.response.usage.counts().cache_read_tokens;
                let cache_create = call.response.usage.counts().cache_write_tokens;
                let native_quote =
                    crate::cost_wiring::has_native_fallback_quote(&call.response.provider_metadata);
                let (quoted_model, pricing) =
                    crate::cost_wiring::response_pricing(call.response.cost.as_ref(), native_quote);
                let billing_model = if native_quote {
                    crate::cost_wiring::native_fallback_cost_model(&call.response.provider_metadata)
                        .unwrap_or(call.model)
                } else {
                    call.model
                };
                let model_ref = quoted_model.unwrap_or_else(|| {
                    crate::cost_wiring::model_ref_from_string(billing_model, call.profile)
                });
                let scope = cost_scope
                    .clone()
                    .expect("a wired cost tracker captured its scope before provider dispatch");
                let measurement = if call.response.usage.report.usage.is_some() {
                    cost::CostResponseMeasurement::Observed
                } else {
                    cost::CostResponseMeasurement::Missing
                };
                let receipt = scope.submit_model_response_with_pricing(
                    cost::CostModelResponse {
                        model_ref: model_ref.clone(),
                        usage,
                        duration: call.duration,
                        retries: call.retries,
                        cache_read_input_tokens: cache_read,
                        cache_creation_input_tokens: cache_create,
                        is_batch_request: false,
                        bus: orch.model_runtime.analytics_bus.clone(),
                    },
                    pricing,
                    measurement,
                );
                (call, receipt, model_ref, cache_read, cache_create)
            })
        })
        .collect();

    if requested_model && physical_responses.is_empty() {
        orch.record_prompt_cache_usage(&response.usage).await;
    }

    // Retain paid usage in the owned cost mutation before surfacing output failure.
    orch.check_output_accounting()?;

    // Inline tool descriptions may grow after MCP/plugin discovery. Commit an
    // append-only replacement only after a successful non-API-error response;
    // deferred entries are excluded and existing descriptions are immutable.
    if requested_model && physical_responses.is_empty() {
        orch.record_inline_prompt_tools_after_success(&tools).await;
    }

    // A3: this call's output-token count, returned to the budget loop so it can
    // accumulate `global_turn_tokens` (TS `getTurnOutputTokens()`).
    let output_tokens = metered_calls.iter().fold(0u64, |total, call| {
        total.saturating_add(
            call.response
                .usage
                .counts()
                .output_tokens
                .saturating_sub(call.response.usage.counts().reasoning_tokens),
        )
    });

    // #55: cache this response's total input tokens (the `Xtt` last-usage
    // snapshot) so the proactive fixed-prefix overflow guard can compute the
    // immovable prefix on the next `maybe_compact_before_call`.
    if requested_model && physical_responses.is_empty() {
        orch.record_response_input_tokens(&metered_response.usage);
    }

    // In-Loop Compaction Batch 6: snapshot the cache-safe prompt prefix now the
    // call has succeeded, so the forked autocompact summarizer can replay this
    // turn's prefix and share Anthropic's prompt cache. `session.history` here is
    // the exact message set the model saw (post any PTL truncation / reactive
    // compaction inside `call_api_with_ptl_recovery`), BEFORE the assistant reply
    // is appended below. Strict no-op when no cache-safe slot is wired.
    if requested_model && physical_responses.is_empty() {
        let display_system = system.map(
            lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput::display_text,
        );
        orch.save_cache_safe_params(display_system.as_deref(), &model, &tools)
            .await;
    }
    // FORK (codex #5 follow-up): record the rendered system prompt this turn
    // handed the model, so a fork-subagent spawn dispatched below in this same
    // turn can thread the exact bytes onto its child (cache-identical prefix).
    let display_system = system.map(
        lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput::display_text,
    );
    orch.save_current_turn_system_prompt(display_system.as_deref())
        .await;

    // Task 8 (llm-runtime future-work batch 3): the call succeeded — forward
    // the adapter's unified rate-limit snapshot to the output stream when it
    // changed since the last emission (emit-on-change; no-op for clients
    // without a snapshot). Covers the batched AND cancelable drivers (both
    // funnel through this function).
    if requested_model && physical_responses.is_empty() {
        orch.emit_rate_limit_if_changed().await;
    }
    // Task 2 (llm-runtime future-work batch 5): same seam, raw per-window
    // utilization snapshot (emit-on-change; empty snapshot never emitted).
    if requested_model && physical_responses.is_empty() {
        orch.emit_raw_utilization_if_changed().await;
    }

    // 1.5 M6-06: record this response's usage into the wired CostTracker (if any).
    // #5 (main-loop parity): pass the REAL wall-clock duration of the API
    // round-trip and the REAL retry count (`last_retry_count()`, the adapter's
    // `RetryState::attempt`) instead of the previous hardcoded `Duration::ZERO`
    // / `0`. claude-code's cost recorder receives both.
    for (call, receipt, model_ref, cache_read, cache_create) in owned_cost_responses {
        let settlement = receipt.settle().await;
        let cost_for_this_call = settlement.observed_nano_usd();
        orch.model_runtime
            .api_calls_recorded
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if let Err(error) = settlement.persistence_result() {
            orch.note_cost_settlement_failure(error).await;
        }
        // strict-parity (2.1.195): fire `tengu_api_success` on the per-request
        // success path (claude `j("tengu_api_success", {...})`). The port-only
        // `tengu_cost_recorded` event was dropped. request id / stop reason /
        // provider live on the orchestrator, so we emit directly here.
        if let Some(bus) = orch.model_runtime.analytics_bus.as_ref() {
            #[allow(clippy::cast_possible_truncation)]
            let dur_ms = u64::try_from(call.duration.as_millis()).unwrap_or(u64::MAX);
            cost::emit_api_success(
                bus,
                &cost::ApiSuccessFields {
                    model: call.model.to_owned(),
                    input_tokens: call.response.usage.counts().input_tokens,
                    output_tokens: call
                        .response
                        .usage
                        .counts()
                        .output_tokens
                        .saturating_sub(call.response.usage.counts().reasoning_tokens),
                    cached_input_tokens: cache_read,
                    uncached_input_tokens: cache_create,
                    duration_ms: dur_ms,
                    duration_ms_including_retries: dur_ms,
                    attempt: call.retries + 1,
                    cost_nano_usd: cost_for_this_call,
                    provider: crate::cost_wiring::provider_tag(&model_ref.provider),
                    stop_reason: call.response.stop_reason.clone(),
                    request_id: if physical_responses.is_empty() {
                        orch.api.last_request_id()
                    } else {
                        call.request_id.map(str::to_owned)
                    },
                    message_count: api_success_message_count,
                    message_tokens: api_success_message_tokens,
                    did_fall_back_to_non_streaming: false,
                    is_non_interactive_session: !orch.prompt_is_interactive(),
                    print: orch.config.print,
                    is_tty: orch.config.is_tty,
                    query_source: crate::config::sanitize_query_source(&orch.config.query_source)
                        .to_string(),
                    permission_mode: if orch.session.lock().await.plan_mode {
                        "plan"
                    } else {
                        "default"
                    }
                    .to_string(),
                    ttft_ms: None,
                    fast_mode: call.response.usage.inference.service_tier
                        == Some(llm_runtime::services::sdk::protocol::ServiceTier::Fast),
                    time_since_last_api_call_ms: orch.record_api_call_gap_ms(),
                },
            )
            .await;
        }
    }

    if let Some(info) = declined_server_fallback {
        crate::server_fallback::flush_pending_notice(orch).await;
        let (assistant_id, stop_reason) =
            crate::server_fallback::surface_declined(orch, &info).await;
        return Ok((
            TurnStepOutcome::Ended {
                final_message_id: assistant_id,
                stop_reason: stop_reason.to_owned(),
                allow_budget_continuation: false,
                tool_requested_end: false,
            },
            output_tokens,
        ));
    }

    let final_index = visible_responses.len() - 1;
    let mut outcome = TurnStepOutcome::Continue;
    let mut same_turn_tool_uses = Vec::new();
    for (index, visible) in visible_responses.iter().enumerate() {
        let final_response = index == final_index;
        let (request_history, request_history_source) = select_visible_response_request_history(
            &visible.id,
            physical_responses
                .iter()
                .map(|call| (call.response.id.as_str(), call.request_history.as_slice())),
            final_response,
            &final_request_history,
            mod_response_request_history_source,
            &turn_step_input_history,
        );
        outcome = visible_response::process_visible_response(
            orch,
            visible,
            if final_response {
                recovery.as_deref_mut()
            } else {
                None
            },
            &visible.model,
            final_response && requested_model,
            physical_responses.is_empty(),
            !visible.server_fallback_events().is_empty(),
            request_history,
            same_turn_tool_uses.clone(),
            request_history_source,
        )
        .await?;
        same_turn_tool_uses.extend(
            translate_response_blocks(&visible.content)
                .into_iter()
                .filter(|block| matches!(block, ContentBlock::ToolUse { .. })),
        );
        if matches!(
            outcome,
            TurnStepOutcome::Ended {
                allow_budget_continuation: false,
                ..
            }
        ) {
            break;
        }
    }
    Ok((outcome, output_tokens))
}

/// PARITY the binary's `Xi`. Spelled out rather than imported: `orchestrator`
/// does not depend on `tool-cron`, and the tool name is a model-facing wire
/// string, not an internal symbol.
const SCHEDULE_WAKEUP_TOOL_NAME: &str = "ScheduleWakeup";

/// Current native AM/ny gate, shared with the loop prompt builder. Explicit
/// capability denial precedes the baked mitigation and Mythos fallback.
fn lone_wakeup_ends_turn_model(model_id: &str) -> bool {
    let model = lingxi_core::host::model_capabilities::normalize_model_id(model_id);
    lingxi_core::host::model_capabilities::wakeup_ends_turn(
        &model,
        std::env::var(branding::MODEL_CAPABILITIES_ENV)
            .ok()
            .as_deref(),
        false,
    )
}

/// Consume the wakeup-armed flag and report whether this round was exactly one
/// `ScheduleWakeup` that armed a wakeup, on a model the binary's gate covers.
///
/// PARITY `if(yo.length===1 && yo[0].name===Xi && Zoe(...)) { if(kg().some(…loop…)) … }`.
///
/// Shared by BOTH turn loops — the batched one in this module and the streaming
/// twin in `conversation::drivers`. One definition matters more than usual here:
/// the first cut of this arm lived only in the batched loop, and the streaming
/// loop is the one the desktop bridge takes, so `/loop` never reached it.
///
/// The flag is consumed on every call (`swap`), including the early returns, so
/// a `ScheduleWakeup` that armed a wakeup alongside other tools cannot leak into
/// the next round.
pub(crate) async fn take_lone_wakeup_turn_end<'a>(
    orch: &ConversationOrchestrator,
    tool_names: impl Iterator<Item = &'a str>,
) -> bool {
    let armed = orch
        .loop_wakeup_armed_slot
        .as_ref()
        .is_some_and(|slot| slot.swap(false, std::sync::atomic::Ordering::SeqCst));
    if !armed {
        return false;
    }
    let mut names = tool_names;
    if !matches!(
        (names.next(), names.next()),
        (Some(only), None) if only == SCHEDULE_WAKEUP_TOOL_NAME
    ) {
        return false;
    }
    match orch.bundled_prompt_model().await {
        Ok(model) => lone_wakeup_ends_turn_model(&model),
        Err(error) => {
            tracing::warn!(%error, "could not resolve wakeup model context");
            false
        }
    }
}

/// PARITY the turn-loop branch that ends a turn on a lone `ScheduleWakeup`:
/// `i("tengu_loop_dynamic_wakeup_ends_turn", {queryChainId, queryDepth})`.
pub(crate) async fn emit_loop_dynamic_wakeup_ends_turn_telemetry(orch: &ConversationOrchestrator) {
    telemetry::emit_loop_dynamic_wakeup_ends_turn(&orch.query_chain_id, 0);
    let Some(bus) = orch.model_runtime.analytics_bus.as_ref() else {
        return;
    };
    let mut metadata = telemetry::LogEventMetadata::new();
    metadata.insert(
        "queryChainId".into(),
        telemetry::AnalyticsValue::String(orch.query_chain_id.clone()),
    );
    metadata.insert("queryDepth".into(), telemetry::AnalyticsValue::Int(0));
    bus.log_event(
        telemetry::tengu::kairos::LOOP_DYNAMIC_WAKEUP_ENDS_TURN,
        metadata,
    )
    .await;
}

pub(crate) async fn emit_tool_result_ended_turn_telemetry(
    orch: &ConversationOrchestrator,
    turn_end: tool_api::tool_trait::ToolResultTurnEnd,
) {
    let Some(bus) = orch.model_runtime.analytics_bus.as_ref() else {
        return;
    };
    let payload = telemetry::tengu::mcp::ToolResultEndedTurnPayload {
        query_chain_id: telemetry::Verified::assert_safe(orch.query_chain_id.clone()),
        query_depth: 0,
        source: telemetry::Verified::assert_safe(turn_end.source.as_str().to_string()),
    };
    let mut metadata = telemetry::LogEventMetadata::new();
    metadata.insert(
        "queryChainId".into(),
        telemetry::AnalyticsValue::String(payload.query_chain_id.as_str().to_string()),
    );
    metadata.insert(
        "queryDepth".into(),
        telemetry::AnalyticsValue::Int(i64::from(payload.query_depth)),
    );
    metadata.insert(
        "source".into(),
        telemetry::AnalyticsValue::String(payload.source.as_str().to_string()),
    );
    bus.log_event(telemetry::tengu::mcp::TOOL_RESULT_ENDED_TURN, metadata)
        .await;
}

pub(crate) async fn emit_tools_refreshed_mid_turn_telemetry(
    orch: &ConversationOrchestrator,
    old_mcp_count: usize,
) {
    let Some(bus) = orch.model_runtime.analytics_bus.as_ref() else {
        return;
    };
    let new_mcp_count = orch.filtered_mcp_tool_count().await;
    if new_mcp_count == old_mcp_count {
        return;
    }
    let payload = telemetry::tengu::mcp::ToolsRefreshedMidTurnPayload {
        old_mcp_count: u32::try_from(old_mcp_count).unwrap_or(u32::MAX),
        new_mcp_count: u32::try_from(new_mcp_count).unwrap_or(u32::MAX),
        recovered: old_mcp_count == 0 && new_mcp_count > 0,
    };
    let mut metadata = telemetry::LogEventMetadata::new();
    metadata.insert(
        "oldMcpCount".into(),
        telemetry::AnalyticsValue::Int(i64::from(payload.old_mcp_count)),
    );
    metadata.insert(
        "newMcpCount".into(),
        telemetry::AnalyticsValue::Int(i64::from(payload.new_mcp_count)),
    );
    metadata.insert(
        "recovered".into(),
        telemetry::AnalyticsValue::Bool(payload.recovered),
    );
    bus.log_event(telemetry::tengu::mcp::TOOLS_REFRESHED_MID_TURN, metadata)
        .await;
}

/// `StructuredOutput` tool name (claude-code `bp`). It is the only tool that
/// sets the `endsTurn`/`toolEndsTurn` flag, so detecting its `tool_use` by name
/// is equivalent to the binary's `name===bp` check.
pub(crate) const STRUCTURED_OUTPUT_TOOL_NAME: &str = "StructuredOutput";

#[cfg(test)]
#[path = "turn_loop/tests/image_tool_result_tests.rs"]
mod image_tool_result_tests;

#[cfg(test)]
#[path = "turn_loop/tests/code_change_accumulation_tests.rs"]
mod code_change_accumulation_tests;

// ORCH-1: the `ZX_` rule-scope → OTEL decision-source mapping, kept out of the
// concurrently-edited turn_loop_test.rs.
#[cfg(test)]
#[path = "turn_loop/tests/decision_otel_source_tests.rs"]
mod decision_otel_source_tests;

// The `toolDenialKind` classifier, kept out of the concurrently-edited
// turn_loop_test.rs for the same reason as the module above.
#[cfg(test)]
#[path = "turn_loop/tests/tool_denial_kind_tests.rs"]
mod tool_denial_kind_tests;

// End-to-end pin for the denial-provenance WIRING (as opposed to the
// `tool_denial_kind` unit tests above, which only cover the pure classifier).
// Kept in its own module rather than in the concurrently-edited
// turn_loop_test.rs, per the convention the modules above already follow.
//
// This seam is exactly where the first version of this feature was wrong:
// `MockOutputStream` inherits the DEFAULTED `emit_tool_result_denied` unless it
// overrides it, so before that override existed every deny-path test passed no
// matter what kind the turn loop computed.
#[cfg(test)]
#[path = "turn_loop/tests/denial_kind_wiring_tests.rs"]
mod denial_kind_wiring_tests;

/// O4-A: the `interrupted` denial stamp.
///
/// claude-code stamps `toolDenialKind` for an ABORTED tool inside the per-tool
/// execution catch (`oQ_`, 2.1.220 @235424972):
/// ```js
/// toolDenialKind: YDd(ce, n.abortController.signal)
/// ```
/// with (`YDd` @235394375)
/// ```js
/// function YDd(e,t){
///   let r = e instanceof hW && e.interrupted;              // ShellError.interrupted
///   if(!(e instanceof tl || r || $7e(e)&&t.aborted)) return; // tl = AbortError
///   return t.aborted && H_(t.reason)==="background" ? "cancelled" : "interrupted";
/// }
/// ```
/// The LingXi analog of `tl` is [`tool_api::ToolError::Aborted`], and the
/// analog of `oQ_`'s catch is the `Err(err)` arm of
/// [`dispatch_tool_uses_tracked`] — so the stamp attaches at exactly the site
/// claude-code stamps at, with no emission-point change.
///
/// Real-transcript ground truth (2.1.220, `~/.claude/projects/**/*.jsonl`):
/// `toolDenialKind` census is `user-rejected` ×13 and `interrupted` ×1; the
/// `interrupted` line carries `"toolUseResult": "Error: [Request interrupted by
/// user for tool use]"`.
#[cfg(test)]
#[path = "turn_loop/tests/interrupted_denial_stamp_tests.rs"]
mod interrupted_denial_stamp_tests;

/// O3: hook `additionalContext` / `hook_error_during_execution` become
/// TRANSCRIPT ATTACHMENTS, and the model-facing rendering is EPHEMERAL.
///
/// Oracle — the attachment→model renderer table (2.1.220 BIN off 238107100):
/// ```text
/// hook_additional_context: (e) => { if (e.content.length === 0) return [];
///     return [ zr({ content: Ww(`${e.hookName} hook additional context: ${e.content.join("\n")}`), isMeta:!0 }) ] },
/// hook_error_during_execution: () => [],
/// ```
/// `Ww` (BIN off 238046823) is the `<system-reminder>` wrapper. The `zr(…)`
/// message is built at API-normalization time from the attachment and is never
/// written to the transcript — census of real 2.1.220 sessions finds 145
/// `hook_additional_context` attachment lines and ZERO persisted `user` lines
/// carrying the rendered text. `hook_error_during_execution` renders to `[]`,
/// so the MODEL NEVER SEES IT.
///
/// claude also never folds a PostToolUse `additionalContext` into the
/// tool_result string — the success arm (BIN off 235420375) assembles
/// `[formattedResult, acceptFeedback?, ...contentBlocks?]` with no hook
/// context, and the PostToolUse consumer (BIN off 234726655) only yields the
/// attachment.
#[cfg(test)]
#[path = "turn_loop/tests/hook_context_attachment_tests.rs"]
mod hook_context_attachment_tests;

/// A1 — the `<persisted-output>` substitution wired into the SUCCESS-path
/// `tool_result` push (claude-code 2.1.220 `F0u`, BIN off **230270568**).
#[cfg(test)]
#[path = "turn_loop/tests/tool_result_persistence_wiring_tests.rs"]
mod tool_result_persistence_wiring_tests;

// ===========================================================================
// BASH-10 / BASH-18 — the two trait seams this file now WIRES.
//
// Both hooks existed on `Tool` (or, for `coerce_input`, did not exist at all)
// with ZERO production call sites, which is why the findings that needed them
// were previously refused. These tests pin the CALL SITES, not the hooks: each
// one runs a real `dispatch_tool_uses_tracked` and would still pass if the
// hook were only DEFINED — so every case is paired with its A/B twin (the same
// dispatch with the hook returning the neutral value), which fails if the
// dispatcher stops consulting it.
//
// Kept in its own module rather than in the concurrently-edited
// turn_loop_test.rs, per the convention the modules above already follow.
// ===========================================================================
#[cfg(test)]
#[path = "turn_loop/tests/observer_pairings_reach_tools_tests.rs"]
mod observer_pairings_reach_tools_tests;

#[cfg(test)]
#[path = "turn_loop/tests/tool_hook_wiring_tests.rs"]
mod tool_hook_wiring_tests;
