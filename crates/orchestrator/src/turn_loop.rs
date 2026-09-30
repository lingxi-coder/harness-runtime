//! Inner turn-by-turn loop helpers. Private to `ConversationOrchestrator`.

use crate::conversation::{classify_api_error, ConversationOrchestrator, ModelCallPath};
use crate::error::OrchestratorError;
use lingxi_core::types::{ContentBlock, ConversationMessage, MessageId, ToolUseId};
use llm_runtime::LlmError;
mod api_recovery;
mod batch_hooks;
mod error_reporting;
mod response;
mod tool_dispatch;
mod tool_results;
pub(crate) use api_recovery::call_api_with_ptl_recovery;
pub(crate) use api_recovery::PtlCallOutcome;
pub(crate) use batch_hooks::append_tool_injected_messages;
pub(crate) use batch_hooks::apply_model_context_modifiers;
#[cfg(test)]
use batch_hooks::post_tool_batch_identity;
pub(crate) use batch_hooks::run_post_tool_batch_hooks;
pub(crate) use batch_hooks::run_post_tool_batch_hooks_after_turn_end;
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
#[cfg(test)]
pub(crate) use tool_dispatch::dispatch_tool_uses;
pub(crate) use tool_dispatch::dispatch_tool_uses_tracked;
pub(crate) use tool_dispatch::dispatch_tool_uses_tracked_deferred;
pub(crate) use tool_dispatch::normalize_lexically;
#[cfg(test)]
use tool_dispatch::rule_decision_otel_source;
#[cfg(test)]
use tool_dispatch::tool_denial_kind;
pub(crate) use tool_dispatch::DeferredToolDispatch;
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

/// Registry name of the subagent-spawning tool (`tools/agent` `AGENT_TOOL_NAME`)
/// and its legacy alias (`LEGACY_AGENT_TOOL_NAME`). A completed dispatch of this
/// tool means the spawned subagent's loop has stopped, so it is where the turn
/// loop fires the `SubagentStop` hook. Held as literals (not imported) so
/// `orchestrator` keeps no dependency on `tools/agent` — same precedent as
/// `ENTER_WORKTREE_TOOL_NAME`.
const AGENT_TOOL_NAME: &str = "Agent";
const LEGACY_AGENT_TOOL_NAME: &str = "Task";

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
) {
    let Some(seq) = seq else {
        return;
    };
    match hooks::terminal_seq::validate_terminal_sequence(seq) {
        Some(validated) => {
            // Forward the validated, BEL-normalized sequence to the host's
            // terminal-write seam (claude-code `BEo`). Default no-op off the TUI.
            orch.output.emit_terminal_sequence(&validated).await;
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
/// next `messages_create_with_opts` request.
pub(crate) const ESCALATED_MAX_TOKENS: u32 = 64_000;

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
/// Keep the port's established `subagent` alias; sanitization only collapses
/// custom-agent suffixes and does not otherwise classify query sources.
pub(crate) fn truncated_response_recovery_is_subagent(query_source: &str) -> bool {
    query_source.starts_with("agent:") || matches!(query_source, "hook_agent" | "subagent")
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
pub(crate) const THINKING_ONLY_NUDGE: &str =
    "[Your previous response had no visible output. Please continue and produce a user-visible response.]";

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
    /// call via [`crate::OrchestratorApiClient::messages_create_with_opts`], so
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
    system: Option<&str>,
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
    system: Option<&str>,
    recovery: Option<&mut RecoveryState>,
) -> Result<TurnStepOutcome, OrchestratorError> {
    // Drop the per-call output-token count (A3 callers use the `_tracked`
    // variant). Preserves the historical signature for every existing caller.
    Ok(
        execute_one_turn_with_recovery_tracked(orch, system, recovery)
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
#[allow(clippy::too_many_lines)]
pub(crate) async fn execute_one_turn_with_recovery_tracked(
    orch: &ConversationOrchestrator,
    system: Option<&str>,
    mut recovery: Option<&mut RecoveryState>,
) -> Result<(TurnStepOutcome, u64), OrchestratorError> {
    // Shared per-step preparation. `None` for the cancel token on purpose: the
    // batched path is covered by the outer `select!` in
    // `try_run_turn_cancelable`, which races this ENTIRE function — preparation
    // included. Passing a token here as well would be a second, narrower
    // cancellation seam for the same turn.
    let prepared = orch
        .prepare_turn_step(ModelCallPath::Batched, system, true, None)
        .await?;
    let history_snapshot = prepared.snapshot;
    let model = prepared.model;
    let model_profile = prepared.model_profile;
    let outgoing_history_rewriter = prepared.outgoing_history_rewriter;
    let turn_reminders = prepared.turn_reminders;
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
    let api_result = call_api_with_ptl_recovery(
        orch,
        system,
        &model,
        model_profile.as_deref(),
        history_snapshot,
        outgoing_history_rewriter,
        tools.clone(),
        max_tokens_override,
        deferred_tools_reminder,
        date_change_reminder,
        &turn_reminders,
        cost_scope.as_ref(),
    )
    .await;
    orch.persist_thinking_signature_strip_latch().await;
    let response = match api_result {
        Ok(outcome) => match outcome {
            PtlCallOutcome::Response(resp) => resp,
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
            let assistant_id = surface_model_error(orch, &content, env).await;
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

    // Transfer the known provider response into its session-owned accounting
    // supervisor before any tool/cache/progress/telemetry await below.
    let owned_cost_response = orch.model_runtime.cost_tracker.as_ref().map(|_| {
        let usage = crate::cost_wiring::llm_usage_to_cost_usage(&response.usage);
        let cache_read = response.usage.counts().cache_read_tokens;
        let cache_create = response.usage.counts().cache_write_tokens;
        let quote = response
            .cost
            .as_ref()
            .and_then(crate::cost_wiring::frozen_cost_quote);
        let model_ref = quote.as_ref().map_or_else(
            || crate::cost_wiring::model_ref_from_string(&model, model_profile.as_deref()),
            |(model_ref, _)| model_ref.clone(),
        );
        let elapsed = api_call_started.elapsed();
        let retries = orch.api.last_retry_count();
        let scope = cost_scope
            .clone()
            .expect("a wired cost tracker captured its scope before provider dispatch");
        let receipt = scope.submit_model_response_with_quote(
            cost::CostModelResponse {
                model_ref: model_ref.clone(),
                usage,
                duration: elapsed,
                retries,
                cache_read_input_tokens: cache_read,
                cache_creation_input_tokens: cache_create,
                is_batch_request: false,
                bus: orch.model_runtime.analytics_bus.clone(),
            },
            quote.map(|(_, amount)| amount),
        );
        (
            receipt,
            model_ref,
            elapsed,
            retries,
            cache_read,
            cache_create,
        )
    });

    // CLI-4: one ledger entry per provider response. Recorded from the SAME
    // token split the cost tracker just billed, so the two can never disagree
    // about what the provider reported.
    {
        let now_ms = u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(0);
        let mut ledger = orch.model_runtime.prompt_cache_ledger.lock().await;
        ledger.record(cost::prompt_cache_ledger::RequestFacts {
            at_ms: now_ms,
            input_tokens: response.usage.counts().input_tokens,
            cache_read_tokens: response.usage.counts().cache_read_tokens,
            cache_creation_tokens: response.usage.counts().cache_write_tokens,
            // The port asks for the 5m TTL; a 1h request would set this from
            // the cache-control it sent.
            ttl: cost::prompt_cache_ledger::CacheTtl::FiveMinutes,
        });
    }

    // Retain paid usage in the owned cost mutation before surfacing output failure.
    orch.check_output_accounting()?;

    // Inline tool descriptions may grow after MCP/plugin discovery. Commit an
    // append-only replacement only after a successful non-API-error response;
    // deferred entries are excluded and existing descriptions are immutable.
    orch.record_inline_prompt_tools_after_success(&tools).await;

    // A3: this call's output-token count, returned to the budget loop so it can
    // accumulate `global_turn_tokens` (TS `getTurnOutputTokens()`).
    let output_tokens = response
        .usage
        .counts()
        .output_tokens
        .saturating_sub(response.usage.counts().reasoning_tokens);

    // #55: cache this response's total input tokens (the `Xtt` last-usage
    // snapshot) so the proactive fixed-prefix overflow guard can compute the
    // immovable prefix on the next `maybe_compact_before_call`.
    orch.record_response_input_tokens(&response.usage);

    // In-Loop Compaction Batch 6: snapshot the cache-safe prompt prefix now the
    // call has succeeded, so the forked autocompact summarizer can replay this
    // turn's prefix and share Anthropic's prompt cache. `session.history` here is
    // the exact message set the model saw (post any PTL truncation / reactive
    // compaction inside `call_api_with_ptl_recovery`), BEFORE the assistant reply
    // is appended below. Strict no-op when no cache-safe slot is wired.
    orch.save_cache_safe_params(system, &model, &tools).await;
    // FORK (codex #5 follow-up): record the rendered system prompt this turn
    // handed the model, so a fork-subagent spawn dispatched below in this same
    // turn can thread the exact bytes onto its child (cache-identical prefix).
    orch.save_current_turn_system_prompt(system).await;

    // Task 8 (llm-runtime future-work batch 3): the call succeeded — forward
    // the adapter's unified rate-limit snapshot to the output stream when it
    // changed since the last emission (emit-on-change; no-op for clients
    // without a snapshot). Covers the batched AND cancelable drivers (both
    // funnel through this function).
    orch.emit_rate_limit_if_changed().await;
    // Task 2 (llm-runtime future-work batch 5): same seam, raw per-window
    // utilization snapshot (emit-on-change; empty snapshot never emitted).
    orch.emit_raw_utilization_if_changed().await;

    // 1.5 M6-06: record this response's usage into the wired CostTracker (if any).
    // #5 (main-loop parity): pass the REAL wall-clock duration of the API
    // round-trip and the REAL retry count (`last_retry_count()`, the adapter's
    // `RetryState::attempt`) instead of the previous hardcoded `Duration::ZERO`
    // / `0`. claude-code's cost recorder receives both.
    if let Some((receipt, model_ref, elapsed, retries, cache_read, cache_create)) =
        owned_cost_response
    {
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
            let dur_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
            cost::emit_api_success(
                bus,
                &cost::ApiSuccessFields {
                    model: model.clone(),
                    input_tokens: response.usage.counts().input_tokens,
                    output_tokens: response
                        .usage
                        .counts()
                        .output_tokens
                        .saturating_sub(response.usage.counts().reasoning_tokens),
                    cached_input_tokens: cache_read,
                    uncached_input_tokens: cache_create,
                    duration_ms: dur_ms,
                    duration_ms_including_retries: dur_ms,
                    attempt: retries + 1,
                    cost_nano_usd: cost_for_this_call,
                    provider: crate::cost_wiring::provider_tag(&model_ref.provider),
                    stop_reason: response.stop_reason.clone(),
                    request_id: orch.api.last_request_id(),
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
                    fast_mode: response.usage.inference.service_tier
                        == Some(llm_runtime::services::sdk::protocol::ServiceTier::Fast),
                    time_since_last_api_call_ms: orch.record_api_call_gap_ms(),
                },
            )
            .await;
        }
    }

    // 2. Translate `HistoryResponse.content` -> `ContentBlock` history entry.
    let assistant_blocks = translate_response_blocks(&response.content);

    // 3. Append the assistant message to the session. We need the
    //    `final_message_id` to return to the caller.
    let assistant_id = MessageId::new();
    let assistant_msg = ConversationMessage::Assistant {
        id: assistant_id,
        content: assistant_blocks.clone(),
        stop_reason: response.stop_reason.clone(),
    };
    {
        let mut s = orch.session.lock().await;
        s.history.push(assistant_msg.clone());
    }
    // M5-07 T13: mirror the in-memory append to the optional JSONL writer.
    // Best-effort — write failures never fail the turn.
    //
    // NON-streaming (batched) parity: claude-code's non-streaming response
    // handler (`claude.ts:2571`) emits exactly ONE merged `AssistantMessage`
    // (single top-level uuid, ALL blocks via `...result` / full `content`) — it
    // does NOT split per content block. Only the STREAMING `content_block_stop`
    // writer (`claude.ts:2171-2211`) splits one line per block. So the batched
    // path persists ONE merged assistant JSONL line; the tool_results below
    // chain off that single line's uuid (shared parent), matching the
    // non-streaming transcript shape. The per-block split lives ONLY on the
    // streaming drain (`conversation.rs::persist_assistant_per_block`).
    //
    // Persist the FULL BetaMessage envelope (real model + usage + requestId) via
    // the batched counterpart — NOT the model-less `persist_message_to_jsonl`,
    // which recorded real replies as `model:"<synthetic>"` with `usage` dropped
    // (the `--print` / `--bg` mislabel + lost-cost bug). `claude.ts:2571` builds
    // the merged non-streaming AssistantMessage with `result.model`/`usage`.
    let request_id = orch.api.last_request_id();
    orch.persist_assistant_merged(&assistant_msg, Some(&response.usage), request_id.as_deref())
        .await;

    // 4. Emit each Text block to the output stream (whole-body in M5-02;
    //    M5-04 will switch to per-delta).
    for blk in &assistant_blocks {
        if let ContentBlock::Text { text } = blk {
            orch.output.emit_text(text).await;
        }
    }

    // OTEL_LOG_ASSISTANT_RESPONSES (claude-code opt-in): default OFF, byte-no-op.
    // When enabled, log the assistant text with req-id/model/stop/usage so OTEL
    // exporters capture response bodies. The gate reads the single authoritative
    // predicate in the OTEL monitoring module (H-BIN-06), which parses the var
    // with the byte-faithful `ct` truthy semantics (1/true/yes/on, trimmed,
    // case-insensitive). Default (var unset) keeps the locked turn fixtures OFF.
    if telemetry::otel::logs::assistant_responses_enabled() {
        let text: String = assistant_blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        tracing::info!(
            event = "assistant_response",
            request_id = orch.api.last_request_id().unwrap_or_default(),
            model = %model,
            stop_reason = response.stop_reason.as_deref().unwrap_or(""),
            input_tokens = response.usage.counts().input_tokens,
            output_tokens = response.usage.counts().output_tokens.saturating_sub(response.usage.counts().reasoning_tokens),
            body = %text,
        );
        telemetry::otel::emit_assistant_response_log(
            orch.api.last_request_id().as_deref().unwrap_or_default(),
            &model,
            response.stop_reason.as_deref().unwrap_or(""),
            response.usage.counts().input_tokens,
            response
                .usage
                .counts()
                .output_tokens
                .saturating_sub(response.usage.counts().reasoning_tokens),
            &text,
        );
    }

    // 5. If there are tool_use blocks, dispatch them and feed results back.
    // `/loop` fold span: this response's calls, and the messages it adds.
    let tool_uses: Vec<(ToolUseId, String, serde_json::Value, Option<String>)> = assistant_blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolUse {
                id,
                name,
                input,
                provider_id,
            } => Some((id.clone(), name.clone(), input.clone(), provider_id.clone())),
            _ => None,
        })
        .collect();
    orch.turn_span.note_assistant_response(tool_uses.len());

    // HOOK.2: a PreToolUse hook returning `continue:false` (preventContinuation)
    // stops the agent loop AFTER this turn step's tools have run (TS
    // `query.ts:1518-1521` returns `{ reason: 'hook_stopped' }`). The tracked
    // dispatch ORs the per-tool `prevent_continuation` signal; the tool still
    // executes and its results are still appended below, exactly like TS (where
    // the tool runs and `hook_stopped_continuation` is yielded after success).
    let mut hook_prevent_continuation = false;
    let mut post_tool_batch_calls = Vec::new();
    let pre_batch_mcp_tool_count = if tool_uses.is_empty() {
        None
    } else {
        Some(orch.filtered_mcp_tool_count().await)
    };
    if !tool_uses.is_empty() {
        let dispatched =
            dispatch_tool_uses_tracked_deferred(orch, &tool_uses, None, Some(assistant_id)).await?;
        let DeferredToolDispatch {
            results: tool_results,
            prevent_continuation,
            injected_messages,
            context_modifiers,
            post_tool_batch_calls: deferred_batch_calls,
        } = dispatched;
        hook_prevent_continuation = prevent_continuation;
        post_tool_batch_calls = deferred_batch_calls;
        // Claude's tool executor yields one user message per resolved tool,
        // even for a concurrent batch. Keep that topology here (the streaming
        // driver already does): message-level `mcpMeta`, `toolEndsTurn`, and
        // `sourceToolAssistantUUID` can then belong to the exact result that
        // produced them. Combining parallel results into one user message
        // loses those fields behind the serializer's exactly-one-result guard.
        let mut remaining_injected = injected_messages;
        // `results` is flat because a denied tool may append non-result blocks
        // (for example an image supplied by an ask rejection) immediately after
        // its `tool_result`. Claude keeps those blocks in that result's user
        // message. Split only at the next `tool_result`, not at every block.
        let mut result_messages: Vec<Vec<ContentBlock>> = Vec::new();
        for block in tool_results {
            if matches!(&block, ContentBlock::ToolResult { .. }) || result_messages.is_empty() {
                result_messages.push(vec![block]);
            } else if let Some(message) = result_messages.last_mut() {
                message.push(block);
            }
        }
        for result_content in result_messages {
            let result_tool_use_id = result_content.iter().find_map(|block| match block {
                ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.clone()),
                _ => None,
            });
            let (result_injected, rest) = match &result_tool_use_id {
                Some(id) => remaining_injected
                    .into_iter()
                    .partition::<Vec<_>, _>(|(_, source_id)| source_id == id),
                None => (Vec::new(), remaining_injected),
            };
            remaining_injected = rest;

            let tool_result_msg = ConversationMessage::User {
                id: MessageId::new(),
                content: result_content,
                is_meta: false,
                is_compact_summary: false,
                is_visible_in_transcript_only: false,
            };
            {
                let mut s = orch.session.lock().await;
                s.history.push(tool_result_msg.clone());
                for (message, source_id) in &result_injected {
                    s.history.push(message.clone());
                    s.injected_message_sources
                        .insert(message.id(), source_id.clone());
                }
            }
            let parent_uuid = match &result_tool_use_id {
                Some(id) => orch.source_tool_assistant_uuid(id).await,
                None => None,
            };
            orch.persist_message_to_jsonl_with_parent(&tool_result_msg, parent_uuid)
                .await;
            if let Some(id) = &result_tool_use_id {
                orch.flush_hook_attachments(id).await;
            }
            for (message, _) in &result_injected {
                if !message.is_meta() {
                    orch.persist_message_to_jsonl(message).await;
                }
            }
        }
        // Defensive only: every injected message should name a result in this
        // dispatch. Preserve rather than drop one if a future synthetic source
        // uses a distinct id.
        append_tool_injected_messages(orch, remaining_injected).await;
        // SKILLEXEC.3 (model scope): fold this batch's `context_modifier`s and
        // switch `session.model` if a skill declared a `model:` override. Applied
        // AFTER `injected_messages` so it mirrors the streaming twin's ordering.
        // Empty for every existing tool + non-`model:` skills → strict no-op
        // (session.model untouched → byte-identical turn-loop fixtures).
        apply_model_context_modifiers(orch, context_modifiers).await;
    }
    let tool_result_turn_end = orch
        .take_pending_tool_result_turn_ends(
            &tool_uses
                .iter()
                .map(|(tool_use_id, _, _, _)| tool_use_id.clone())
                .collect::<Vec<_>>(),
        )
        .await;
    let tool_requested_end_turn = tool_result_turn_end.is_some() && !hook_prevent_continuation;
    if !hook_prevent_continuation {
        if let Some(turn_end) = tool_result_turn_end {
            // Oracle order: results first, then the single end-turn telemetry
            // event, then PostToolBatch, then forced Stop hooks.
            emit_tool_result_ended_turn_telemetry(orch, turn_end).await;
        }
        let (batch_prevent, batch_messages) = if tool_requested_end_turn {
            (
                false,
                run_post_tool_batch_hooks_after_turn_end(orch, post_tool_batch_calls).await,
            )
        } else {
            run_post_tool_batch_hooks(orch, post_tool_batch_calls).await
        };
        append_tool_injected_messages(orch, batch_messages).await;
        // A tool-requested end wins over PostToolBatch stop/block. The batch
        // hook still runs and its records are kept, but cannot re-enter the
        // model or change the terminal disposition.
        if !tool_requested_end_turn {
            hook_prevent_continuation |= batch_prevent;
            if !batch_prevent {
                if let Some(old_mcp_count) = pre_batch_mcp_tool_count {
                    emit_tools_refreshed_mid_turn_telemetry(orch, old_mcp_count).await;
                }
            }
        }
    }

    // LONE `ScheduleWakeup` ENDS THE TURN (binary
    // `if(yo.length===1 && yo[0].name===Xi && Zoe(…)) { if(kg().some(…loop…)) … }`).
    // A round whose ONLY tool call was `ScheduleWakeup`, and which actually
    // armed a wakeup, has nothing left to do: the tool's own result already
    // tells the model the harness will re-invoke it when the wakeup fires, so
    // feeding that result back just buys one more model round to say so.
    //
    // The flag is CONSUMED either way (inside the predicate) so a call that
    // armed a wakeup alongside other tools cannot leak into the next round.
    let lone_wakeup_ended_turn = !hook_prevent_continuation
        && !tool_requested_end_turn
        && take_lone_wakeup_turn_end(orch, tool_uses.iter().map(|(_, name, _, _)| name.as_str()))
            .await;
    if lone_wakeup_ended_turn {
        emit_loop_dynamic_wakeup_ends_turn_telemetry(orch).await;
    }

    // Finding #73 (batched twin): advance the per-turn todo/task reminder
    // counters for THIS assistant turn, then reset `turns_since_last_todo_write`
    // to 0 if this turn's assistant response invoked the variant's "recent use"
    // tool (TodoWrite for V1; TaskCreate/TaskUpdate for V2). Mirrors the
    // binary's per-assistant-message counting in `L4p`/`N4p` (which zero `r` at
    // the last such tool_use). Order — bump THEN reset — so a turn that calls
    // TodoWrite lands at 0 (not 1), matching the binary scan that excludes the
    // TodoWrite message itself. No-op for the locked fixtures (a single-turn
    // run never reaches the threshold).
    orch.bump_reminder_turn_counters().await;
    let invoked_tool_names: Vec<String> = tool_uses
        .iter()
        .map(|(_, name, _, _)| name.clone())
        .collect();
    orch.note_todo_reminder_tool_call(&invoked_tool_names).await;

    // #78 nudge guard `!Pt(ce)`: suppress the thinking-only nudge during a
    // StructuredOutput exchange. Computed here (a match guard cannot `.await`
    // the session lock); the scan is cheap — it stops at the first real user
    // message. The current assistant response is already in `history` (pushed at
    // the top of this fn), mirroring the binary's `se` including `se.at(-1)`.
    let prior_structured_output = {
        let session = orch.session();
        let s = session.lock().await;
        prior_assistant_used_structured_output(&s.history)
    };

    // EndConversation (2.1.206): the tool's 2nd consecutive call raised the
    // shared end-request slot during tool execution above. Consume it; if
    // raised, terminate the conversation and surface the end message to the
    // user. Default-OFF (no slot wired) → never raised → byte-identical.
    // Consumed AFTER the lone-wakeup resolution above, unlike the streaming
    // twin.  still outranks every other signal in the
    // disposition match below.
    let end_conversation_requested = orch.take_end_conversation_request().await;

    // Oer only exhausts two consecutive malformed attempts: every other
    // transition replaces `malformed_tool_use_retry` in the oracle state.
    if response.stop_reason.as_deref() != Some("tool_use") || !tool_uses.is_empty() {
        if let Some(state) = recovery.as_deref_mut() {
            state.malformed_tool_use_retried = false;
        }
    }

    // 6. Decide loop disposition.
    let outcome = if end_conversation_requested {
        // The model confirmed (2nd EndConversation call) — end the query.
        TurnStepOutcome::Ended {
            final_message_id: assistant_id,
            stop_reason: "end_conversation".to_string(),
            allow_budget_continuation: false,
            tool_requested_end: false,
        }
    } else if hook_prevent_continuation {
        // HOOK.2: honor the PreToolUse `continue:false` request — end the turn
        // step so the driver stops the loop (TS `{ reason: 'hook_stopped' }`).
        // Takes precedence over the `stop_reason`-derived disposition (a step
        // that ran tools never has `stop_reason == "end_turn"`).
        TurnStepOutcome::Ended {
            final_message_id: assistant_id,
            stop_reason: "hook_stopped".to_string(),
            allow_budget_continuation: false,
            tool_requested_end: false,
        }
    } else if tool_requested_end_turn {
        TurnStepOutcome::Ended {
            final_message_id: assistant_id,
            stop_reason: "end_turn".to_string(),
            allow_budget_continuation: false,
            tool_requested_end: true,
        }
    } else if lone_wakeup_ended_turn {
        // `tool_requested_end: false` — the tool did not ask (no `toolEndsTurn`
        // marker); the turn loop decided, as the binary's own arm does.
        TurnStepOutcome::Ended {
            final_message_id: assistant_id,
            stop_reason: "end_turn".to_string(),
            allow_budget_continuation: false,
            tool_requested_end: false,
        }
    } else {
        match response.stop_reason.as_deref() {
            // #1 needsFollowUp gate (claude-code `query.ts:554-558`, `832-835`,
            // `1062`): continuation is keyed on tool-block PRESENCE, NOT the raw
            // `stop_reason` string — the ref explicitly notes `stop_reason ==
            // "tool_use"` "is unreliable -- it's not always set correctly", so it
            // sets `needsFollowUp = true` whenever the assistant message carried
            // ANY tool_use block (regardless of stop_reason) and `if (!needsFollowUp)`
            // is the SOLE end-vs-continue gate. So a response that dispatched tools
            // but reported a non-`tool_use` stop_reason (e.g. `end_turn`,
            // `stop_sequence`, or a truncated `max_tokens` that still carried a
            // complete tool block) must run the tools AND continue, feeding the
            // tool_results back — NOT end the turn. This leading arm fires only when
            // tools were dispatched (`!tool_uses.is_empty()`); a withheld
            // `max_output_tokens` response carries NO tool_uses, so it falls through
            // to the recovery/terminal arms below unchanged. The common `tool_use`+
            // tools case (previously handled by the `_ => Continue` fallback) is
            // unaffected.
            _ if !tool_uses.is_empty() => TurnStepOutcome::Continue,
            // #77 malformed-tool-use retry (batched twin): `stop_reason ==
            // "tool_use"` but the response produced ZERO tool_use blocks. Only
            // the recovery-aware drivers participate (the per-turn guard lives on
            // `RecoveryState`); the legacy shim (`None`) keeps the historical
            // `_ => Continue` no-op (re-call with no nudge). `tool_uses` is the
            // dispatched set computed above — empty here means a malformed
            // response (`tool_use` stop with no parseable tool_use block).
            Some("tool_use") if recovery.is_some() && tool_uses.is_empty() => {
                let state = recovery.as_deref_mut().expect("recovery is Some");
                handle_malformed_tool_use(orch, assistant_id, state).await?
            }
            // #78 thinking-only nudge (batched twin): an `end_turn` /
            // `stop_sequence` (or absent → treated as `end_turn`) response with
            // no visible text gets ONE nudge before the turn ends. Recovery-aware
            // drivers only; the compact-source exclusion (`a !== "compact" &&
            // !GRe(a)`) is satisfied unconditionally (compaction runs in a
            // separate code path, never this turn step).
            Some("end_turn" | "stop_sequence") | None
                if recovery.as_deref().is_some_and(|s| !s.thinking_only_nudged)
                    && !has_visible_text(&assistant_blocks)
                    && !prior_structured_output =>
            {
                let state = recovery.as_deref_mut().expect("recovery is Some");
                handle_thinking_only(orch, assistant_id, state).await?
            }
            Some("end_turn" | "stop_sequence") | None => TurnStepOutcome::Ended {
                final_message_id: assistant_id,
                stop_reason: "end_turn".to_string(),
                allow_budget_continuation: true,
                tool_requested_end: false,
            },
            // A1: max_output_tokens recovery (TS `query.ts:1223-1255`). Only the
            // recovery-aware drivers (`Some(state)`) participate; the legacy shim
            // (`None`) falls through to Continue, unchanged.
            Some("max_tokens") if recovery.is_some() => {
                // `recovery.is_some()` guarded above — unwrap is infallible.
                let state = recovery.expect("recovery is Some");
                handle_max_output_tokens(orch, assistant_id, state).await?
            }
            // Finding #80 (batched twin, claude-code `bin/claude.exe` offset
            // ~205871579): a `refusal` response swaps to the configured
            // `refusalFallbackModel` ONCE per session, warns the user, and Continues
            // (the next step re-snapshots `session.model`, so it re-issues against
            // the fallback). When no fallback is configured (or the latch is already
            // set), the helper returns `false` and this falls through to the
            // historical `_ => Continue` bare re-call — byte-identical to before.
            Some("refusal") if orch.maybe_swap_to_refusal_fallback().await => {
                TurnStepOutcome::Continue
            }
            // Terminal error stop_reasons (batched twin of the streaming
            // `Some(other)` arm, claude.ts:2266/2279): surface the byte-locked
            // `API Error: …` assistant message and END the turn. Previously these
            // fell through to `_ => Continue` and bare-re-called the API, never
            // surfacing the error — the #24 batched-path gap. `model_context_window_exceeded`
            // and a terminal `refusal` (reached only when no `refusalFallbackModel`
            // is configured / the once-per-session latch is set, so the swap arm
            // above did not `continue`) both end here. `max_tokens` (recovery
            // exhausted) is surfaced inside `handle_max_output_tokens`.
            Some(other @ ("model_context_window_exceeded" | "refusal")) => {
                // Pass the response's refusal `stop_details` so the cyber/bio
                // variant fires (no-op for model_context_window_exceeded).
                let surfaced_id =
                    surface_terminal_api_error(orch, other, response.stop_details.as_ref()).await;
                TurnStepOutcome::Ended {
                    final_message_id: surfaced_id.unwrap_or(assistant_id),
                    stop_reason: other.to_string(),
                    allow_budget_continuation: false,
                    tool_requested_end: false,
                }
            }
            _ => TurnStepOutcome::Continue,
        }
    };
    Ok((outcome, output_tokens))
}

/// PARITY the binary's `Xi`. Spelled out rather than imported: `orchestrator`
/// does not depend on `tool-cron`, and the tool name is a model-facing wire
/// string, not an internal symbol.
const SCHEDULE_WAKEUP_TOOL_NAME: &str = "ScheduleWakeup";

/// PARITY `Zoe(family, model)` =
/// `dm(model, "fable_5_mitigations", family) || family === "claude-mythos-5"`
/// — the model gate on the lone-`ScheduleWakeup` turn end. It is a
/// model-generation mitigation, so most models never take the branch and keep
/// feeding the tool result back, exactly as before.
fn lone_wakeup_ends_turn_model(model_id: &str) -> bool {
    use lingxi_core::host::model_capabilities::{
        has_capability, normalize_model_id, ModelCapability,
    };
    has_capability(model_id, ModelCapability::Fable5Mitigations)
        || normalize_model_id(model_id) == "claude-mythos-5"
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
    let session = orch.session();
    let model = session.lock().await.model.clone();
    lone_wakeup_ends_turn_model(&model)
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
