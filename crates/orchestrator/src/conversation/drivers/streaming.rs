use std::sync::Arc;

use llm_runtime::{LlmError, LlmEvent};
use protocol::{ContentBlock, ConversationMessage, MessageId};
use tokio_util::sync::CancellationToken;

use super::{
    is_env_truthy, llm_response_to_pumped_turn, loop_state, prepare, token_aborted,
    QueuedPromptInput, StepExit, StreamingIterationDisposition, TurnLoopState,
};
use crate::conversation::{
    assistant_usage_value, classify_api_error, output_accounting_impl, ApiErrorEnvelope,
    ConversationOrchestrator, ConversationOutcome, ModelCallPath, OutgoingHistoryRewriter,
    INTERRUPT_MESSAGE, INTERRUPT_MESSAGE_FOR_TOOL_USE,
};
use crate::error::OrchestratorError;
use crate::streaming_loop::ExecutorPump;
use crate::turn_loop::{
    call_api_with_ptl_recovery, surface_prompt_too_long, surface_rapid_refill_thrashing,
    PtlCallOutcome, MAX_OUTPUT_TOKENS_RECOVERY_LIMIT,
};

pub(super) struct StreamingTurnDriver<'a> {
    pub(super) orch: &'a ConversationOrchestrator,
    pub(super) prompt: &'a str,
    pub(super) images: Vec<protocol::ImageSource>,
    pub(super) user_cancel: Option<CancellationToken>,
    pub(super) message_id: Option<MessageId>,
    pub(super) transient_rewake: bool,
    pub(super) in_human_turn: bool,
    pub(super) queued_inputs: Option<Vec<QueuedPromptInput>>,
}

struct PreparedStreamingIteration {
    step: prepare::PreparedTurnStep,
    assistant_id: MessageId,
    partial_finalize: Option<crate::streaming_loop::PartialFinalizeCause>,
    partial_finalize_notice_id: Option<MessageId>,
    turn_id: String,
    display_hook_active: bool,
    cost_scope: Option<cost::CostSessionScope>,
}

enum PrepareStreamingOutcome {
    Ready(PreparedStreamingIteration),
    Complete(MessageId),
}

enum OpenedModelStream {
    Stream(futures::stream::BoxStream<'static, Result<LlmEvent, LlmError>>),
    Recovered(crate::streaming_loop::PumpedTurn),
}

struct OpenedStreamingIteration<'a> {
    opened: OpenedModelStream,
    exec: crate::streaming_executor::StreamingToolExecutor<'a>,
    model: String,
    model_profile: Option<String>,
    outgoing_history_rewriter: Option<Arc<dyn OutgoingHistoryRewriter>>,
    turn_reminders: Vec<ConversationMessage>,
    wire_tools: Vec<serde_json::Value>,
    deferred_reminder: Option<ConversationMessage>,
    date_change_reminder: Option<ConversationMessage>,
    assistant_id: MessageId,
    partial_finalize: Option<crate::streaming_loop::PartialFinalizeCause>,
    partial_finalize_notice_id: Option<MessageId>,
    turn_id: String,
    display_hook_active: bool,
    api_call_started: std::time::Instant,
    api_success_message_count: u32,
    api_success_message_tokens: u64,
    did_fall_back_to_non_streaming: bool,
    cost_scope: Option<cost::CostSessionScope>,
    cost_receipt: Option<cost::CostResponseReceipt>,
}

enum OpenStreamingOutcome<'a> {
    Opened(OpenedStreamingIteration<'a>),
    Complete(MessageId),
}

struct PumpedStreamingIteration<'a> {
    pumped: crate::streaming_loop::PumpedTurn,
    exec: crate::streaming_executor::StreamingToolExecutor<'a>,
    model: String,
    model_profile: Option<String>,
    wire_tools: Vec<serde_json::Value>,
    assistant_id: MessageId,
    partial_finalize: Option<crate::streaming_loop::PartialFinalizeCause>,
    partial_finalize_notice_id: Option<MessageId>,
    turn_id: String,
    display_hook_active: bool,
    api_call_started: std::time::Instant,
    api_success_message_count: u32,
    api_success_message_tokens: u64,
    did_fall_back_to_non_streaming: bool,
    pre_batch_mcp_tool_count: usize,
    cost_receipt: Option<cost::CostResponseReceipt>,
}

enum PumpStreamingOutcome<'a> {
    Pumped(PumpedStreamingIteration<'a>),
    Complete(MessageId),
}

struct FinalizedStreamingIteration {
    pumped: crate::streaming_loop::PumpedTurn,
    assistant_id: MessageId,
    tool_prevent_continuation: bool,
    post_tool_batch_calls: Vec<hooks::events::PostToolBatchCall>,
    pre_batch_mcp_tool_count: usize,
    partial_finalize: Option<crate::streaming_loop::PartialFinalizeCause>,
    partial_finalize_notice_id: Option<MessageId>,
    aborted_during_stream: bool,
}

impl StreamingTurnDriver<'_> {
    fn begin_stream_cost_response(
        orch: &ConversationOrchestrator,
        cost_scope: Option<&cost::CostSessionScope>,
        model: &str,
        model_profile: Option<&str>,
        pumped: &crate::streaming_loop::PumpedTurn,
        duration: std::time::Duration,
    ) -> Option<cost::CostResponseReceipt> {
        let usage = pumped.usage.as_ref()?;
        orch.model_runtime.cost_tracker.as_ref()?;
        let scope =
            cost_scope.expect("a wired cost tracker captured its scope before stream dispatch");
        let cost_usage = crate::cost_wiring::llm_usage_to_cost_usage(usage);
        let quote = pumped
            .cost_quote
            .as_ref()
            .and_then(crate::cost_wiring::frozen_cost_quote);
        let model_ref = quote.as_ref().map_or_else(
            || crate::cost_wiring::model_ref_from_string(model, model_profile),
            |(model_ref, _)| model_ref.clone(),
        );
        Some(scope.submit_model_response_with_quote(
            cost::CostModelResponse {
                model_ref,
                usage: cost_usage,
                duration,
                retries: orch.streaming_api.last_retry_count(),
                cache_read_input_tokens: usage.billable_tokens.cache_read,
                cache_creation_input_tokens: usage.billable_tokens.cache_write,
                is_batch_request: false,
                bus: orch.model_runtime.analytics_bus.clone(),
            },
            quote.map(|(_, amount)| amount),
        ))
    }

    async fn prepare_iteration(
        orch: &ConversationOrchestrator,
        system_prompt: &Option<String>,
        user_cancel: &Option<CancellationToken>,
        loop_state: &mut TurnLoopState,
        in_human_turn: bool,
    ) -> Result<PrepareStreamingOutcome, OrchestratorError> {
        // Shared per-step preparation. The token goes IN rather than racing
        // this call from outside: streaming must not be dropped mid-flight by a
        // `select!`, because that discards in-flight tool results (see the
        // DEFERRED-3 note on `run_turn_streaming_inputs_locked`).
        let prepared = orch
            .prepare_turn_step(
                ModelCallPath::Streaming,
                system_prompt.as_deref(),
                in_human_turn,
                user_cancel.as_ref(),
            )
            .await?;

        // Capture the originating cost scope so a later session switch cannot
        // redirect observed usage to the active session. This used to be read
        // between the peer-inbox drain and the snapshot; the slot is only ever
        // written by `execute_clear_session` / `execute_resume_session`, never
        // during a turn, so reading it here is the same value.
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

        // RECOV.1: blocking-limit preempt — the streaming twin of the batched
        // `call_api_with_ptl_recovery` step (1) (TS `query.ts:592-648`). If the
        // pre-call prompt is already at the hard blocking limit
        // (`token_usage >= effective_window − MANUAL_COMPACT_BUFFER_TOKENS`),
        // surface the byte-exact `PROMPT_TOO_LONG_ERROR_MESSAGE` and END the
        // turn WITHOUT opening the stream — mirroring the batched path (which
        // returns `PtlCallOutcome::PromptTooLong` ⇒ ends with stop_reason
        // `"prompt_too_long"`). Use the exact same custom beta set as the
        // provider request so context-1m sessions are not preempted at the
        // default 200k boundary.
        let active_betas = orch.api.active_betas();
        let warning = compaction::calculate_token_warning_state(
            compaction::grouping::estimate_tokens_for_range(&prepared.snapshot),
            &prepared.model,
            &active_betas,
            true,
        );
        if warning.is_at_blocking_limit {
            tracing::warn!(
                model = %prepared.model,
                "prompt at blocking limit — preempting before stream"
            );
            let id = surface_prompt_too_long(orch).await;
            // PROACTIVE preempt ⇒ terminal reason `"blocking_limit"` (distinct
            // from the reactive-exhausted `prompt_too_long`), mirroring the
            // batched path's `PtlCallOutcome::BlockingLimit`. RECOV.2 chokepoint:
            // `handle_stop_at_end` treats `"blocking_limit"` as an api-error end
            // (fires `StopFailure`, skips the normal `Stop` hooks, returns
            // `FallThrough`), so the directive is discarded and the normal
            // end-of-turn tail runs — exactly mirroring the batched path.
            let _ = orch
                .handle_stop_at_end(
                    "blocking_limit",
                    &mut loop_state.stop_hook_active,
                    &mut loop_state.stop_hook_blocking_count,
                    loop_state.turn_count,
                    id,
                    token_aborted(user_cancel),
                )
                .await;
            let cost = orch.snapshot_cost_real().await;
            orch.output.emit_end_turn("blocking_limit", &cost).await;
            return Ok(PrepareStreamingOutcome::Complete(id));
        }
        // Past the preempt: this step's snapshot WILL be sent, so the
        // `date_change` reminder it carries counts as delivered.
        orch.commit_date_change_reminder();

        // Mid-stream tool dispatch (claude-code `query.ts:562` + `837-844`):
        // create the executor + pre-allocate this turn's assistant id BEFORE
        // opening the stream, so each `tool_use` block can be `add_tool`'d and
        // dispatched the moment it streams in (instead of collecting all
        // tool_uses and only starting them after the stream ends). This is
        // BYTE-EQUIVALENT: only WHEN tools start changes. The per-block
        // assistant JSONL persistence + per-result drain/persist still run
        // post-stream below (see `pump_stream_with_executor`'s contract).
        //
        // DEFERRED-3: hand the executor the turn's user-interrupt token (if
        // any) so it can reject in-flight/queued Cancel-behavior tools with the
        // REJECT_MESSAGE; `None` → identical to before.
        let assistant_id = MessageId::new();

        // P1-04 (cc 2.1.199 partial-stream finalize): set by the pump error
        // arm when a completed partial is finalized in place. Drives the
        // "API Error: … may be incomplete." notice (surfaced after the partial
        // is persisted) and the terminal turn-end below. Reset per turn.
        let partial_finalize: Option<crate::streaming_loop::PartialFinalizeCause> = None;
        let partial_finalize_notice_id: Option<MessageId> = None;

        // hooks #39: MessageDisplay fires at the BEGIN of this assistant
        // message's stream (claude-code `begin(d)`, BIN off 208862320),
        // mirroring its `o={apiMessageId:d, messageId:randomUUID(),
        // turnId:r, index:0, …}` initialization. The `turn_id` is a fresh
        // per-turn UUID (`newTurn(){…; r=randomUUID()}`). Best-effort +
        // no-op when unregistered, so existing flows are byte-identical.
        let turn_id = uuid::Uuid::new_v4().to_string();
        orch.fire_message_display(&turn_id, assistant_id).await;

        // P2-04 (MessageDisplay `displayContent`): a registered
        // `MessageDisplay` hook makes the live pump SUPPRESS per-token text
        // deltas so the completed-message pass below renders the full
        // (possibly hook-substituted) text once — faithful to claude-code
        // `Qff` (BIN off 229876575), whose live render flows through the
        // display flush, not raw deltas. `false` (no hook) ⇒ byte-identical
        // live streaming. Cheap subscription gate (declared-event only).
        let display_hook_active = orch
            .hooks
            .has_hooks_for(&hooks::events::HookEventType::MessageDisplay)
            .await;

        Ok(PrepareStreamingOutcome::Ready(PreparedStreamingIteration {
            step: prepared,
            assistant_id,
            partial_finalize,
            partial_finalize_notice_id,
            turn_id,
            display_hook_active,
            cost_scope,
        }))
    }

    async fn open_iteration<'a>(
        orch: &'a ConversationOrchestrator,
        prepared: PreparedStreamingIteration,
        system_prompt: &Option<String>,
        user_cancel: &Option<CancellationToken>,
        loop_state: &mut TurnLoopState,
    ) -> Result<OpenStreamingOutcome<'a>, OrchestratorError> {
        let PreparedStreamingIteration {
            step,
            assistant_id,
            partial_finalize,
            partial_finalize_notice_id,
            turn_id,
            display_hook_active,
            cost_scope,
        } = prepared;

        let prepare::PreparedTurnStep {
            snapshot,
            model,
            model_profile,
            outgoing_history_rewriter,
            turn_reminders,
            wire_tools,
            deferred_reminder,
            date_change_reminder,
        } = step;

        let mut exec = match &user_cancel {
            Some(token) => crate::streaming_executor::StreamingToolExecutor::new_with_user_cancel(
                orch,
                token.clone(),
            ),
            None => crate::streaming_executor::StreamingToolExecutor::new(orch),
        };
        // Hold `tool_result` frames until the collection point below can
        // release them in RECEIVED order, with a cancelled tool's synthetic
        // already substituted. Off again after the drive, so any later
        // emission on this orchestrator goes straight out.
        orch.set_tool_frame_buffering(true).await;

        // #5: wall-clock from stream-open through pump completion (incl. any
        // 529→non-stream fallback) so the CostTracker records a REAL duration
        // instead of `Duration::ZERO`. Paired with
        // `orch.streaming_api.last_retry_count()` at the billing site below.
        let api_call_started = std::time::Instant::now();

        // tengu_api_success `messageCount:n` / `messageTokens:r`: capture from
        // the OUTGOING snapshot BEFORE it is moved into `.stream(...)`.
        let api_success_message_count = u32::try_from(snapshot.len()).unwrap_or(u32::MAX);
        let api_success_message_tokens = compaction::grouping::estimate_tokens_for_range(&snapshot);
        let did_fall_back_to_non_streaming = false;

        orch.sync_thinking_signature_strip_flag_to_api().await;
        if let Some(scope) = cost_scope.as_ref() {
            scope.preflight().await.map_err(|error| {
                OrchestratorError::Internal(format!("cost durability preflight failed: {error}"))
            })?;
        }
        let mut cost_receipt = None;
        let output_observation = orch.capture_main_output().await?;
        // Either an open stream to pump, or a turn already RECOVERED from a
        // connect-phase prompt-too-long (#1, see the ContextOverflow arm).
        let stream_result = orch
            .streaming_api
            .stream(
                &model,
                model_profile.as_deref(),
                system_prompt.as_deref(),
                snapshot,
                wire_tools.clone(),
            )
            .await;
        orch.persist_thinking_signature_strip_latch().await;
        let opened = match stream_result {
            Ok(s) => OpenedModelStream::Stream(output_accounting_impl::account_stream(
                s,
                output_observation,
            )),
            // #1 (main-loop parity): a connect-phase 413 / prompt-too-long
            // surfaces HERE as `LlmError::ContextOverflow` — the adapter's
            // `drive_stream` returns `Err` on connect status >= 400, so it
            // never reaches the pump. The proactive blocking-limit preempt
            // above undershot the server's own limit, so recover REACTIVELY
            // via the SAME helper the batched path uses
            // (`call_api_with_ptl_recovery`): truncate-head xN -> one full
            // compact -> retry. On success we replay the recovered
            // non-streaming response exactly like the 529 fallback below; on
            // exhaustion we end the turn with the byte-exact prompt_too_long /
            // rapid_refill copy — identical to the proactive preempt and the
            // batched path. NOT gated on
            // `LINGXI_DISABLE_NONSTREAMING_FALLBACK` (that flag governs
            // the 529 overload fallback; PTL recovery is the always-on batched
            // behavior). Previously a streaming 413 bubbled as a hard
            // `OrchestratorError::Streaming` error (documented divergence).
            Err(LlmError::ContextOverflow { .. }) => {
                // Re-snapshot history — the per-turn `snapshot` was MOVED into
                // the failed `stream()` call. Prepend the additional-context
                // meta message, like every `callModel` (claude-code `A6n`).
                let (recov_snapshot_raw, recov_model, recov_profile) = {
                    let s = orch.session.lock().await;
                    (
                        s.model_context_history(),
                        s.model.clone(),
                        s.model_profile.clone(),
                    )
                };
                let mut recov_snapshot = orch
                    .rewrite_outgoing_history(
                        recov_snapshot_raw,
                        outgoing_history_rewriter.as_ref(),
                    )
                    .await?;
                orch.reattach_outgoing_context(
                    &mut recov_snapshot,
                    deferred_reminder.as_ref(),
                    date_change_reminder.as_ref(),
                    &turn_reminders,
                )
                .await;
                match call_api_with_ptl_recovery(
                    orch,
                    system_prompt.as_deref(),
                    &recov_model,
                    recov_profile.as_deref(),
                    recov_snapshot,
                    outgoing_history_rewriter.clone(),
                    wire_tools.clone(),
                    None,
                    deferred_reminder.clone(),
                    date_change_reminder.clone(),
                    &turn_reminders,
                    cost_scope.as_ref(),
                )
                .await?
                {
                    PtlCallOutcome::Response(resp) => {
                        // Replay the recovered non-streaming response exactly
                        // like the 529 fallback below: emit text live, rebuild
                        // a fresh executor, register its tool_uses, and flow on
                        // as the turn's `pumped` result.
                        let pumped_from_recovery = llm_response_to_pumped_turn(&resp);
                        cost_receipt = Self::begin_stream_cost_response(
                            orch,
                            cost_scope.as_ref(),
                            &model,
                            model_profile.as_deref(),
                            &pumped_from_recovery,
                            api_call_started.elapsed(),
                        );
                        orch.check_output_accounting()?;

                        // P2-04: when a `MessageDisplay` hook is active the
                        // completed-message pass below is the single on-screen
                        // render (with `displayContent` substitution) — skip the
                        // direct whole-body emit here to avoid double display.
                        if !display_hook_active {
                            for blk in &pumped_from_recovery.assistant_blocks {
                                if let ContentBlock::Text { text } = blk {
                                    orch.output.emit_text(text).await;
                                }
                            }
                        }
                        exec = match &user_cancel {
                            Some(token) => {
                                crate::streaming_executor::StreamingToolExecutor::new_with_user_cancel(
                                    orch,
                                    token.clone(),
                                )
                            }
                            None => crate::streaming_executor::StreamingToolExecutor::new(orch),
                        };
                        for tu in &pumped_from_recovery.tool_uses {
                            exec.add_tool(
                                tu.id.clone(),
                                tu.name.clone(),
                                tu.input.clone(),
                                tu.provider_id.clone(),
                                assistant_id,
                            );
                        }
                        OpenedModelStream::Recovered(pumped_from_recovery)
                    }
                    PtlCallOutcome::PromptTooLong => {
                        // Reactive recovery exhausted — end with the reactive
                        // terminal reason `prompt_too_long` (`query.ts:1175`).
                        let id = surface_prompt_too_long(orch).await;
                        let _ = orch
                            .handle_stop_at_end(
                                "prompt_too_long",
                                &mut loop_state.stop_hook_active,
                                &mut loop_state.stop_hook_blocking_count,
                                loop_state.turn_count,
                                id,
                                token_aborted(user_cancel),
                            )
                            .await;
                        let cost = orch.snapshot_cost_real().await;
                        orch.output.emit_end_turn("prompt_too_long", &cost).await;
                        return Ok(OpenStreamingOutcome::Complete(id));
                    }
                    PtlCallOutcome::BlockingLimit => {
                        // The recovery's own PROACTIVE step-1 preempt fired on
                        // the re-snapshotted history ⇒ terminal `"blocking_limit"`
                        // (distinct from reactive-exhausted `prompt_too_long`),
                        // mirroring the batched path's `BlockingLimit` arm.
                        let id = surface_prompt_too_long(orch).await;
                        let _ = orch
                            .handle_stop_at_end(
                                "blocking_limit",
                                &mut loop_state.stop_hook_active,
                                &mut loop_state.stop_hook_blocking_count,
                                loop_state.turn_count,
                                id,
                                token_aborted(user_cancel),
                            )
                            .await;
                        let cost = orch.snapshot_cost_real().await;
                        orch.output.emit_end_turn("blocking_limit", &cost).await;
                        return Ok(OpenStreamingOutcome::Complete(id));
                    }
                    PtlCallOutcome::RapidRefillBreaker => {
                        // #54 reactive trip — surface the thrashing message and
                        // end with the terminal reason `rapid_refill_breaker`
                        // (the MESSAGE still carries api-error `invalid_request`),
                        // mirroring the batched path.
                        let id = surface_rapid_refill_thrashing(orch).await;
                        let _ = orch
                            .handle_stop_at_end(
                                "rapid_refill_breaker",
                                &mut loop_state.stop_hook_active,
                                &mut loop_state.stop_hook_blocking_count,
                                loop_state.turn_count,
                                id,
                                token_aborted(user_cancel),
                            )
                            .await;
                        let cost = orch.snapshot_cost_real().await;
                        orch.output
                            .emit_end_turn("rapid_refill_breaker", &cost)
                            .await;
                        return Ok(OpenStreamingOutcome::Complete(id));
                    }
                }
            }
            // #10: a connect-phase RateLimited/Overloaded keeps its dedicated
            // downstream handling — propagate as a hard error.
            Err(e @ (LlmError::RateLimited { .. } | LlmError::Overloaded { .. })) => {
                return Err(OrchestratorError::Streaming(e));
            }
            // #10: any other connect-phase model/runtime error ends the turn
            // GRACEFULLY as `model_error` (faithful port of the `query.ts`
            // catch) — surface the raw error text as an api-error assistant
            // message instead of bubbling a hard error. No assistant message
            // was persisted this turn, so no orphaned tool_use to repair.
            Err(other) => {
                // Classify the typed connect-phase error (`Flp`/`KNn`) before
                // consuming it for the verbatim message text. Wrap into the
                // `Streaming` variant — this is the connect-phase streaming
                // surface — so the classifier sees the inner `LlmError`.
                let env = classify_api_error(&OrchestratorError::Streaming(other.clone()));
                let id = crate::turn_loop::surface_model_error(
                    orch,
                    &orch.model_error_text(&other).await,
                    env,
                )
                .await;
                let cost = orch.snapshot_cost_real().await;
                orch.output.emit_end_turn("model_error", &cost).await;
                return Ok(OpenStreamingOutcome::Complete(id));
            }
        };

        Ok(OpenStreamingOutcome::Opened(OpenedStreamingIteration {
            opened,
            exec,
            model,
            model_profile,
            outgoing_history_rewriter,
            turn_reminders,
            wire_tools,
            deferred_reminder,
            date_change_reminder,
            assistant_id,
            partial_finalize,
            partial_finalize_notice_id,
            turn_id,
            display_hook_active,
            api_call_started,
            api_success_message_count,
            api_success_message_tokens,
            did_fall_back_to_non_streaming,
            cost_scope,
            cost_receipt,
        }))
    }

    #[allow(clippy::too_many_arguments)]
    async fn fallback_after_stream_error<'a>(
        orch: &'a ConversationOrchestrator,
        error: OrchestratorError,
        outgoing_history_rewriter: Option<&Arc<dyn OutgoingHistoryRewriter>>,
        deferred_reminder: Option<&ConversationMessage>,
        date_change_reminder: Option<&ConversationMessage>,
        turn_reminders: &[ConversationMessage],
        wire_tools: &[serde_json::Value],
        system_prompt: &Option<String>,
        user_cancel: &Option<CancellationToken>,
        display_hook_active: bool,
        assistant_id: MessageId,
        cost_scope: Option<&cost::CostSessionScope>,
        api_call_started: std::time::Instant,
    ) -> Result<
        (
            crate::streaming_loop::PumpedTurn,
            crate::streaming_executor::StreamingToolExecutor<'a>,
            Option<cost::CostResponseReceipt>,
        ),
        OrchestratorError,
    > {
        // Seed: a streaming overload counts as 1 toward the consecutive
        // 529 budget (LlmError::Overloaded = 529).  Other in-band errors
        // (e.g. ProviderInternal) seed 0 — matching TS
        // `is529Error(streamingError) ? 1 : 0` (claude.ts:2559).
        let seed: u8 = u8::from(matches!(
            error,
            OrchestratorError::Streaming(LlmError::Overloaded { .. })
        ));

        // Re-snapshot history for the non-streaming call (the partial
        // stream never touched session.history, so it is still the same
        // snapshot we used for the stream — no reset needed).
        let (non_stream_snapshot_raw, non_stream_model, non_stream_profile) = {
            let s = orch.session.lock().await;
            (
                s.model_context_history(),
                s.model.clone(),
                s.model_profile.clone(),
            )
        };
        let mut non_stream_snapshot = orch
            .rewrite_outgoing_history(non_stream_snapshot_raw, outgoing_history_rewriter)
            .await?;
        // R-P1c/R-P1d: claude-code's `A6n` prepends the additional-
        // context meta message on EVERY `callModel`, including this
        // non-streaming fallback. Prepend it to the re-snapshot too.
        orch.reattach_outgoing_context(
            &mut non_stream_snapshot,
            deferred_reminder,
            date_change_reminder,
            turn_reminders,
        )
        .await;
        let tools_for_fallback = wire_tools.to_vec();

        if let Some(scope) = cost_scope {
            scope.preflight().await.map_err(|error| {
                OrchestratorError::Internal(format!("cost durability preflight failed: {error}"))
            })?;
        }

        let mut output_observation = orch.capture_main_output().await?;
        let resp = orch
            .api
            .messages_create_seeded(
                &non_stream_model,
                non_stream_profile.as_deref(),
                system_prompt.as_deref(),
                non_stream_snapshot,
                tools_for_fallback,
                seed,
            )
            .await
            .map_err(OrchestratorError::ApiCall)?;

        if let Some(observation) = &mut output_observation {
            observation.observe(&resp.usage);
            let _ = observation.finish();
        }

        // Convert LlmResponse → PumpedTurn so the rest of the streaming
        // turn loop can proceed identically.
        let pumped_from_fallback = llm_response_to_pumped_turn(&resp);
        let cost_receipt = Self::begin_stream_cost_response(
            orch,
            cost_scope,
            &non_stream_model,
            non_stream_profile.as_deref(),
            &pumped_from_fallback,
            api_call_started.elapsed(),
        );

        orch.check_output_accounting()?;

        // Emit text blocks from the non-streaming response to the output
        // stream, mirroring the batched path (turn_loop.rs step 4:
        // `orch.output.emit_text(text).await`).  In the normal streaming
        // path `pump_stream` calls `dispatch_event` → `emit_text` for each
        // `TextDelta`; the non-streaming path has no SSE events, so we
        // replicate the whole-body emit here.
        // P2-04: suppressed when a `MessageDisplay` hook is active — the
        // completed-message pass renders the (possibly substituted) text
        // once, so skip the direct emit to avoid double display.
        for blk in &pumped_from_fallback.assistant_blocks {
            match blk {
                ContentBlock::Text { text } if !display_hook_active => {
                    orch.output.emit_text(text).await;
                }
                ContentBlock::Thinking {
                    thinking,
                    signature,
                } => {
                    orch.output
                        .emit_thinking(thinking, signature.as_deref())
                        .await;
                }
                ContentBlock::RedactedThinking { data } => {
                    orch.output.emit_redacted_thinking(data).await;
                }
                _ => {}
            }
        }

        // claude-code `query.ts:733-740`: discard the partial
        // streaming attempt's executor (its tool_uses have stale ids
        // and would orphan against the fallback response) and replace
        // it with a fresh one. Dropping the old executor cancels any
        // in-flight tool futures it had started mid-stream. The fresh
        // executor's tools are registered from the FALLBACK response's
        // tool_uses by the post-stream drive loop below (this is the
        // ONLY path that still `add_tool`s after the stream — the
        // normal path registers mid-stream).
        let mut exec = match user_cancel {
            Some(token) => crate::streaming_executor::StreamingToolExecutor::new_with_user_cancel(
                orch,
                token.clone(),
            ),
            None => crate::streaming_executor::StreamingToolExecutor::new(orch),
        };
        for tu in &pumped_from_fallback.tool_uses {
            exec.add_tool(
                tu.id.clone(),
                tu.name.clone(),
                tu.input.clone(),
                tu.provider_id.clone(),
                assistant_id,
            );
        }

        Ok((pumped_from_fallback, exec, cost_receipt))
    }

    async fn pump_iteration<'a>(
        orch: &'a ConversationOrchestrator,
        prepared: PreparedStreamingIteration,
        system_prompt: &Option<String>,
        user_cancel: &Option<CancellationToken>,
        loop_state: &mut TurnLoopState,
    ) -> Result<PumpStreamingOutcome<'a>, OrchestratorError> {
        let OpenedStreamingIteration {
            opened,
            mut exec,
            model,
            model_profile,
            outgoing_history_rewriter,
            turn_reminders,
            wire_tools,
            deferred_reminder,
            date_change_reminder,
            assistant_id,
            mut partial_finalize,
            partial_finalize_notice_id,
            turn_id,
            display_hook_active,
            api_call_started,
            api_success_message_count,
            api_success_message_tokens,
            mut did_fall_back_to_non_streaming,
            cost_scope,
            mut cost_receipt,
        } = match Self::open_iteration(orch, prepared, system_prompt, user_cancel, loop_state)
            .await?
        {
            OpenStreamingOutcome::Opened(iteration) => iteration,
            OpenStreamingOutcome::Complete(message_id) => {
                return Ok(PumpStreamingOutcome::Complete(message_id));
            }
        };
        // Polling the stream can start tool execution immediately. Snapshot
        // the live MCP count before that point so a tool-triggered registry
        // refresh can be compared after PostToolBatch, as in the oracle.
        let pre_batch_mcp_tool_count = orch.filtered_mcp_tool_count().await;

        // 3. Pump the stream (with mid-stream 529 → non-streaming fallback OR
        //    the cc 2.1.199 partial-finalize, whichever applies).
        //
        // Task 7 / claude.ts parity: if the stream errors with `LlmError::Overloaded`
        // after the first event — AND `LINGXI_DISABLE_NONSTREAMING_FALLBACK` is not
        // set — AND no content block completed yet — issue a fresh non-streaming call
        // seeded with `initial_consecutive_overloaded = 1`.  This mirrors
        // `claude.ts:2469-2594` + `withRetry.ts:186` (`initialConsecutive529Errors`).
        //
        // The env gate name is locked byte-for-byte to TS:
        //   `process.env.LINGXI_DISABLE_NONSTREAMING_FALLBACK` (claude.ts:2470)
        // Truthiness follows `isEnvTruthy` (non-empty, non-"false", non-"0").
        //
        // P1-04 (cc 2.1.199, binary-verified): once a REAL content block has
        // completed — or a transport close leaves visible text whose stop
        // frame was lost — the partial is no longer discarded. The
        // `partial_has_output` arm in the `match pump_outcome` below finalizes
        // it in place (synthesized stop_reason + usage + `tengu_streaming_partial_finalized`),
        // persists the streamed blocks, and appends the "API Error: … may be
        // incomplete." notice — instead of the pre-2.1.199 discard-and-refetch.
        // The non-streaming fallback therefore fires only when the stream
        // produced no recoverable output. Provider errors before the first
        // completed block retain cc's `_r`-length behavior.
        // In both cases partial deltas already reached callers LIVE via
        // `event_router.rs` → `output.emit_text` at each `TextDelta`
        // (claude.ts:2210 `yield m`).
        // #1: a connect-phase prompt-too-long already recovered above (its
        // recovered non-streaming response was replayed) skips the pump; an
        // open stream is pumped as before.
        let pumped = match opened {
            OpenedModelStream::Recovered(pumped_from_recovery) => pumped_from_recovery,
            OpenedModelStream::Stream(first_stream) => {
                // cc 2.1.198 mid-response transient retry (`query.ts` stream
                // loop @219649648): on a transient network drop (ECONNRESET /
                // connection closed / reset) OR a watchdog idle-timeout, re-open
                // and re-pump the SAME streaming request with backoff — but ONLY
                // while `!real_content_started` (binary `!Hr`). Because a
                // `tool_use` block STARTING flips `real_content_started`, this
                // guard also guarantees NO tool has been dispatched, so a
                // non-idempotent tool is never re-run. The failed pump left
                // `session.history` untouched and (by the guard) the executor
                // clean, so the retry reuses `exec` and re-snapshots history.
                let mut cur_stream = first_stream;
                let mut mid_stream_retries: u32 = 0;
                let pump_outcome: Result<
                    crate::streaming_loop::PumpedTurn,
                    crate::streaming_loop::PumpFailure,
                > = loop {
                    let observed_pump = crate::streaming_loop::pump_stream_with_executor_tracked(
                        cur_stream,
                        &orch.output,
                        ExecutorPump {
                            executor: &mut exec,
                            assistant_id,
                            user_cancel: user_cancel.as_ref(),
                            suppress_live_text: display_hook_active,
                        },
                    )
                    .await;
                    if let Err(error) = orch.check_output_accounting() {
                        let retained = match &observed_pump {
                            Ok(pumped) => pumped,
                            Err(failure) => &failure.partial,
                        };
                        // The error must not discard known paid usage. This
                        // synchronous handoff survives dropping its waiter.
                        let _receipt = Self::begin_stream_cost_response(
                            orch,
                            cost_scope.as_ref(),
                            &model,
                            model_profile.as_deref(),
                            retained,
                            api_call_started.elapsed(),
                        );
                        return Err(error);
                    }
                    match observed_pump {
                        Ok(p) => break Ok(p),
                        Err(f)
                            if crate::streaming_loop::is_transient_mid_stream(&f.error)
                                && !f.real_content_started
                                && mid_stream_retries
                                    < crate::streaming_loop::mid_stream_retry_cap(&f.error) =>
                        {
                            mid_stream_retries += 1;
                            // Exponential backoff + jitter (binary `sle`).
                            let base = llm_runtime::model::retry::scaled_base_delay_ms(
                                mid_stream_retries - 1,
                                None,
                            );
                            tokio::time::sleep(llm_runtime::model::retry::jittered_delay(base))
                                .await;
                            tracing::warn!(
                                attempt = mid_stream_retries,
                                "mid-response transient stream error — retrying streaming request"
                            );
                            // Re-snapshot history (+ additional context) for
                            // the retry — same pattern as the 529 fallback.
                            let (re_snapshot_raw, re_model, re_profile) = {
                                let s = orch.session.lock().await;
                                (
                                    s.model_context_history(),
                                    s.model.clone(),
                                    s.model_profile.clone(),
                                )
                            };
                            let mut re_snapshot = orch
                                .rewrite_outgoing_history(
                                    re_snapshot_raw,
                                    outgoing_history_rewriter.as_ref(),
                                )
                                .await?;
                            orch.reattach_outgoing_context(
                                &mut re_snapshot,
                                deferred_reminder.as_ref(),
                                date_change_reminder.as_ref(),
                                &turn_reminders,
                            )
                            .await;
                            if let Some(scope) = cost_scope.as_ref() {
                                scope.preflight().await.map_err(|error| {
                                    OrchestratorError::Internal(format!(
                                        "cost durability preflight failed: {error}"
                                    ))
                                })?;
                            }
                            let output_observation = orch.capture_main_output().await?;
                            let retry_stream = orch
                                .streaming_api
                                .stream(
                                    &re_model,
                                    re_profile.as_deref(),
                                    system_prompt.as_deref(),
                                    re_snapshot,
                                    wire_tools.clone(),
                                )
                                .await;
                            orch.persist_thinking_signature_strip_latch().await;
                            match retry_stream {
                                Ok(s) => {
                                    cur_stream = output_accounting_impl::account_stream(
                                        s,
                                        output_observation,
                                    );
                                    continue;
                                }
                                // Re-open failed: surface as the terminal
                                // pump error for the arms below. No partial to
                                // finalize here — the retry only fires while
                                // `!real_content_started`, so nothing real was
                                // yielded on the attempt we are abandoning.
                                Err(e) => {
                                    break Err(crate::streaming_loop::PumpFailure {
                                        error: OrchestratorError::Streaming(e),
                                        real_content_started: false,
                                        partial: crate::streaming_loop::PumpedTurn::default(),
                                    });
                                }
                            }
                        }
                        Err(f) => break Err(f),
                    }
                };
                match pump_outcome {
                    Ok(p) => p,
                    // P1-04 (cc 2.1.199 partial-stream finalize, binary-verified): a
                    // finalize-class mid-stream error (server/overloaded/api error,
                    // watchdog stall, or connection close) that landed after useful
                    // output is not discarded. The already-streamed
                    // partial is finalized in place — persisted with a synthesized
                    // `stop_reason` (`tool_use` if any tool_use else `end_turn`) +
                    // usage — `tengu_streaming_partial_finalized` fires, and a byte-exact
                    // "API Error: … The response above may be incomplete." notice is
                    // surfaced after it (see the notice + terminal sites below, gated on
                    // `partial_finalize`). This runs BEFORE the 529 non-streaming
                    // fallback so a completed-partial 529 keeps its streamed output
                    // instead of re-fetching; a 529 that erred before any block
                    // completed (no output) falls through to the fallback as before.
                    Err(f)
                        if crate::streaming_loop::partial_has_output(&f.partial)
                            && crate::streaming_loop::partial_finalize_cause(&f.error)
                                .is_some() =>
                    {
                        let cause = crate::streaming_loop::partial_finalize_cause(&f.error)
                            .expect("finalize cause present (guarded above)");
                        let mut partial = f.partial;
                        // cc `gm=vd?"tool_use":"end_turn"`: a dispatched tool_use makes
                        // this a tool turn, else a natural end.
                        let synthesized_stop_reason = if partial.tool_uses.is_empty() {
                            "end_turn"
                        } else {
                            "tool_use"
                        };
                        partial.stop_reason = Some(synthesized_stop_reason.to_string());
                        cost_receipt = Self::begin_stream_cost_response(
                            orch,
                            cost_scope.as_ref(),
                            &model,
                            model_profile.as_deref(),
                            &partial,
                            api_call_started.elapsed(),
                        );
                        // cc `_r.length`: one yielded message per completed content block.
                        let blocks_yielded =
                            partial.assistant_blocks.len() + partial.tool_uses.len();
                        if let Some(bus) = orch.model_runtime.analytics_bus.as_ref() {
                            let mut md = telemetry::LogEventMetadata::new();
                            md.insert(
                                "model".into(),
                                telemetry::AnalyticsValue::String(model.clone()),
                            );
                            md.insert(
                                "blocks_yielded".into(),
                                telemetry::AnalyticsValue::Int(
                                    i64::try_from(blocks_yielded).unwrap_or(i64::MAX),
                                ),
                            );
                            // has_output is always true on this arm (partial_has_output).
                            md.insert("has_output".into(), telemetry::AnalyticsValue::Bool(true));
                            md.insert(
                                "synthesized_stop_reason".into(),
                                telemetry::AnalyticsValue::String(
                                    synthesized_stop_reason.to_string(),
                                ),
                            );
                            md.insert(
                                "cause".into(),
                                telemetry::AnalyticsValue::String(cause.as_str().to_string()),
                            );
                            if let Some(rid) = orch.api.last_request_id() {
                                md.insert(
                                    "request_id".into(),
                                    telemetry::AnalyticsValue::String(rid),
                                );
                            }
                            bus.log_event("tengu_streaming_partial_finalized", md).await;
                        }
                        // Arm the notice + terminal-end sites below; the partial flows
                        // through the normal billing/persist/tool-drive path first.
                        partial_finalize = Some(cause);
                        partial
                    }
                    Err(f)
                        if matches!(
                            f.error,
                            OrchestratorError::Streaming(
                                LlmError::Overloaded { .. } | LlmError::ProviderInternal
                            )
                        ) && !is_env_truthy(
                            std::env::var("LINGXI_DISABLE_NONSTREAMING_FALLBACK")
                                .as_deref()
                                .ok(),
                        ) =>
                    {
                        did_fall_back_to_non_streaming = true;
                        let (pumped_from_fallback, replacement_exec, fallback_cost_receipt) =
                            Self::fallback_after_stream_error(
                                orch,
                                f.error,
                                outgoing_history_rewriter.as_ref(),
                                deferred_reminder.as_ref(),
                                date_change_reminder.as_ref(),
                                &turn_reminders,
                                &wire_tools,
                                system_prompt,
                                user_cancel,
                                display_hook_active,
                                assistant_id,
                                cost_scope.as_ref(),
                                api_call_started,
                            )
                            .await?;
                        exec = replacement_exec;
                        cost_receipt = fallback_cost_receipt;
                        pumped_from_fallback
                    }
                    // #10: RateLimited/Overloaded/RepeatedOverloaded keep dedicated
                    // downstream handling — propagate.
                    Err(f) if crate::turn_loop::is_carveout_propagated(&f.error) => {
                        return Err(f.error);
                    }
                    // #10: any other mid-stream model/runtime error (e.g. Transport)
                    // ends the turn GRACEFULLY as `model_error` (faithful port of the
                    // `query.ts` catch) rather than bubbling a hard error / phantom
                    // interrupt. Reached when the partial finalize above did NOT apply —
                    // either no recoverable output existed before the error (for
                    // example, an incomplete tool block) or the error is not a
                    // finalize class. The assistant message for a partial-with-real-
                    // -output turn is persisted by the finalize arm above; here nothing
                    // was persisted (TS `yieldMissingToolResultBlocks` no-op).
                    Err(f) => {
                        let other = f.error;
                        // Classify the typed mid-stream error (`Flp`/`KNn`) into the
                        // api-error envelope; the message text stays verbatim.
                        let env = classify_api_error(&other);
                        let id =
                            crate::turn_loop::surface_model_error(orch, &other.to_string(), env)
                                .await;
                        let cost = orch.snapshot_cost_real().await;
                        orch.output.emit_end_turn("model_error", &cost).await;
                        return Ok(PumpStreamingOutcome::Complete(id));
                    }
                }
            }
        };
        if cost_receipt.is_none() {
            cost_receipt = Self::begin_stream_cost_response(
                orch,
                cost_scope.as_ref(),
                &model,
                model_profile.as_deref(),
                &pumped,
                api_call_started.elapsed(),
            );
        }
        Ok(PumpStreamingOutcome::Pumped(PumpedStreamingIteration {
            pumped,
            exec,
            model,
            model_profile,
            wire_tools,
            assistant_id,
            partial_finalize,
            partial_finalize_notice_id,
            turn_id,
            display_hook_active,
            api_call_started,
            api_success_message_count,
            api_success_message_tokens,
            did_fall_back_to_non_streaming,
            pre_batch_mcp_tool_count,
            cost_receipt,
        }))
    }

    async fn finalize_iteration(
        orch: &ConversationOrchestrator,
        pumped_iteration: PumpedStreamingIteration<'_>,
        system_prompt: &Option<String>,
        user_cancel: &Option<CancellationToken>,
        loop_state: &mut TurnLoopState,
    ) -> Result<FinalizedStreamingIteration, OrchestratorError> {
        let PumpedStreamingIteration {
            pumped,
            mut exec,
            model,
            model_profile,
            wire_tools,
            assistant_id,
            partial_finalize,
            mut partial_finalize_notice_id,
            turn_id,
            display_hook_active,
            api_call_started,
            api_success_message_count,
            api_success_message_tokens,
            did_fall_back_to_non_streaming,
            pre_batch_mcp_tool_count,
            cost_receipt,
        } = pumped_iteration;

        // Inline tool descriptions are committed only after a complete,
        // non-API-error response. Partial finalization is an error surface and
        // must not advance the persisted snapshot.
        if partial_finalize.is_none() {
            orch.record_inline_prompt_tools_after_success(&wire_tools)
                .await;
        }

        // BILLING: record streaming-turn usage into CostTracker — mirrors the
        // non-streaming path in `turn_loop.rs`. #5 (main-loop parity): pass
        // the REAL wall-clock duration (stream-open → pump completion) and
        // the REAL connect-phase retry count (`last_retry_count()`) instead
        // of the previous hardcoded `Duration::ZERO` / `0`.
        if let Some(ref usage) = pumped.usage {
            // #55: cache this response's total input tokens (the `Xtt`
            // last-usage snapshot) for the fixed-prefix overflow guard.
            orch.record_response_input_tokens(usage);
        }
        if let (Some(usage), Some(cost_receipt)) = (pumped.usage.as_ref(), cost_receipt) {
            let cache_read = usage.billable_tokens.cache_read;
            let cache_create = usage.billable_tokens.cache_write;
            let model_ref =
                crate::cost_wiring::model_ref_from_string(&model, model_profile.as_deref());
            let elapsed = api_call_started.elapsed();
            let retries = orch.streaming_api.last_retry_count();
            let settlement = cost_receipt.settle().await;
            let cost_for_this_call = settlement.observed_nano_usd();
            orch.model_runtime
                .api_calls_recorded
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if let Err(error) = settlement.persistence_result() {
                orch.note_cost_settlement_failure(error).await;
            }
            // strict-parity (2.1.195): fire `tengu_api_success` on the
            // streaming per-request success path (claude
            // `j("tengu_api_success", {...})`). `tengu_cost_recorded`
            // was port-only and dropped.
            //
            // P1-04: a partial-stream finalize is NOT a per-request success —
            // cc records cost (`Ae+=zhe`, kept above) but does NOT emit
            // `tengu_api_success` (it already fired `tengu_streaming_partial_finalized`).
            // Skip the success emit when this turn was finalized from a partial.
            if let Some(bus) = orch
                .model_runtime
                .analytics_bus
                .as_ref()
                .filter(|_| partial_finalize.is_none())
            {
                #[allow(clippy::cast_possible_truncation)]
                let dur_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
                cost::emit_api_success(
                    bus,
                    &cost::ApiSuccessFields {
                        model: model.clone(),
                        input_tokens: usage.billable_tokens.input,
                        output_tokens: usage.billable_tokens.output,
                        cached_input_tokens: cache_read,
                        uncached_input_tokens: cache_create,
                        duration_ms: dur_ms,
                        duration_ms_including_retries: dur_ms,
                        attempt: retries + 1,
                        cost_nano_usd: cost_for_this_call,
                        provider: crate::cost_wiring::provider_tag(&model_ref.provider),
                        stop_reason: pumped.stop_reason.clone(),
                        request_id: orch.api.last_request_id(),
                        message_count: api_success_message_count,
                        message_tokens: api_success_message_tokens,
                        did_fall_back_to_non_streaming,
                        is_non_interactive_session: !orch.prompt_is_interactive(),
                        print: orch.config.print,
                        is_tty: orch.config.is_tty,
                        query_source: crate::config::sanitize_query_source(
                            &orch.config.query_source,
                        )
                        .to_string(),
                        permission_mode: if orch.session.lock().await.plan_mode {
                            "plan"
                        } else {
                            "default"
                        }
                        .to_string(),
                        ttft_ms: None,
                        fast_mode: usage.speed.as_deref() == Some("fast"),
                        time_since_last_api_call_ms: orch.record_api_call_gap_ms(),
                    },
                )
                .await;
            }
        }

        // In-Loop Compaction Batch 6: snapshot the cache-safe prompt prefix
        // after a successful stream (streaming twin of the batched save).
        // `session.history` here equals the streamed snapshot — the streaming
        // path does not mutate history mid-call — taken before the assistant
        // reply is appended below. Strict no-op when no slot is wired.
        orch.save_cache_safe_params(system_prompt.as_deref(), &model, &wire_tools)
            .await;

        // Task 8 (llm-runtime future-work batch 3): the streamed call (or
        // its non-streaming 529 fallback) completed — forward the
        // adapter's unified rate-limit snapshot when it changed since the
        // last emission. `orch.api` is the same `ProviderApiAdapter` as
        // `orch.streaming_api` in production; the adapter records headers
        // on the `drive_stream` connect-success path too.
        orch.emit_rate_limit_if_changed().await;
        // Task 2 (batch 5): same seam, raw per-window snapshot.
        orch.emit_raw_utilization_if_changed().await;

        // 4. Assemble + append the assistant message.
        // `assistant_id` was pre-allocated before the stream (mid-stream
        // dispatch hands it to the executor as each tool registers).
        let mut blocks: Vec<ContentBlock> = pumped.assistant_blocks.clone();
        for t in &pumped.tool_uses {
            blocks.push(ContentBlock::ToolUse {
                id: t.id.clone(),
                name: t.name.clone(),
                input: t.input.clone(),
                provider_id: t.provider_id.clone(),
            });
        }
        let assistant_msg = ConversationMessage::Assistant {
            id: assistant_id,
            content: blocks,
            stop_reason: pumped.stop_reason.clone(),
        };
        {
            let mut s = orch.session.lock().await;
            s.history.push(assistant_msg.clone());
        }

        // P2-04 (MessageDisplay `displayContent`): completed-message pass.
        // Once the assistant text blocks are finalized, fire `MessageDisplay`
        // with `final:true` + the joined visible text and render the result
        // ON SCREEN — substituting the hook's `displayContent` when present,
        // else the original text. The stored `assistant_msg` / JSONL keep the
        // ORIGINAL content (claude-code stores the override on a separate
        // `displayedMessageContent`, never `message.content`). Gated on a
        // registered `MessageDisplay` hook — when active the live per-token
        // deltas were suppressed in the pump, so this is the single on-screen
        // render; when inactive this whole block is skipped (byte-identical).
        if display_hook_active {
            let joined: String = pumped
                .assistant_blocks
                .iter()
                .map(|b| match b {
                    ContentBlock::Text { text } => text.as_str(),
                    _ => "",
                })
                .collect();
            // claude-code `Qff`: skip firing entirely when the joined text is
            // empty (`if(s==="")return i`).
            if !joined.is_empty() {
                let on_screen = orch
                    .fire_message_display_completed(&turn_id, &joined)
                    .await
                    .unwrap_or(joined);
                orch.output.emit_text(&on_screen).await;
            }
        }

        // Finding #73 (streaming twin): advance the per-turn todo/task
        // reminder counters for THIS assistant turn, then reset
        // `turns_since_last_todo_write` if this turn invoked the variant's
        // "recent use" tool (TodoWrite / TaskCreate / TaskUpdate). Bump THEN
        // reset so a TodoWrite turn lands at 0 (matching the binary scan that
        // excludes the TodoWrite message itself). Mirrors the batched twin.
        orch.bump_reminder_turn_counters().await;
        let invoked_tool_names: Vec<String> =
            pumped.tool_uses.iter().map(|t| t.name.clone()).collect();
        orch.note_todo_reminder_tool_call(&invoked_tool_names).await;

        // DEFERRED-3: advance the interrupt-guard's reported final id to this
        // turn's assistant message.
        loop_state.last_message_id = assistant_id;
        // WRITE-side per-block split (claude.ts:2171-2211): persist the turn
        // as one single-block assistant JSONL line per content block, sharing
        // the turn's inner `message.id` with distinct top-level uuids, and
        // capture each `tool_use`'s line uuid so its `tool_result` parents to
        // ITS line (TS `sourceToolAssistantUUID`) — NOT one shared per-turn
        // parent. The in-memory `s.history` above stays the single merged
        // assistant message (the Anthropic request needs all blocks in one
        // assistant turn).
        // Raw Anthropic `usage` object for the persisted BetaMessage envelope
        // (the streaming codec retains it on `Usage::provider_metadata`).
        let assistant_usage = pumped.usage.as_ref().map(assistant_usage_value);
        // The Anthropic `request-id` response header for THIS turn → the
        // top-level `requestId` on each persisted assistant line. The
        // adapter records it from the stream connect-success / non-stream
        // headers (the same `record_rate_limit_from_headers` pass), so it is
        // the just-completed call's id here. `orch.api` is the same adapter
        // as `orch.streaming_api` in production; the fallback non-stream call
        // records it on `orch.api` too.
        let request_id = orch.api.last_request_id();
        // stream-json P1: signal the message boundary to the output sink so
        // `StreamJsonStream` can flush its accumulated assistant frame.
        orch.output
            .emit_assistant_message_identity(&assistant_id)
            .await;
        orch.output
            .emit_message_boundary(pumped.stop_reason.as_deref(), request_id.as_deref())
            .await;
        let tool_use_parent_uuids = orch
            .persist_assistant_per_block(
                &assistant_msg,
                assistant_usage.as_ref(),
                request_id.as_deref(),
            )
            .await;
        // Fallback parent (the LAST persisted block's uuid) for any
        // tool_result whose tool_use id is missing from the map (defensive).
        let assistant_uuid = orch.transcript.last_jsonl_uuid.lock().await.clone();

        // P1-04 (cc 2.1.199): after the finalized partial assistant is
        // persisted, yield the byte-exact incomplete-response notice as its own
        // api-error assistant message — cc yields `tu({content:…, error:"server_error"})`
        // RIGHT AFTER the patched partial and BEFORE any tool_results run. The
        // notice's api-error category is hardcoded `server_error` regardless of
        // the underlying finalize cause (cc `error:"server_error"`). Persisted
        // without the `tengu_query_error` telemetry (that fires only from the
        // top-level `model_error` catch, not this finalize path).
        if let Some(cause) = partial_finalize {
            let env = ApiErrorEnvelope {
                error: Some("server_error"),
                api_error_status: None,
                inner_stop_reason: None,
                truncated_after_output: true,
            };
            partial_finalize_notice_id = Some(
                crate::turn_loop::surface_api_error_notice(orch, cause.incomplete_notice(), env)
                    .await,
            );
        }

        // #5 aborted_streaming vs aborted_tools disambiguation (faithful port
        // of claude-code's TWO distinct abort checkpoints): query.ts:1015 runs
        // RIGHT AFTER `callModel`, BEFORE the tool-completion drive — an abort
        // observed there is `aborted_streaming` + `createUserInterruptionMessage({toolUse:false})`
        // (`[Request interrupted by user]`). query.ts:1485 runs AFTER the tool
        // drive — an abort observed only there is `aborted_tools` +
        // `createUserInterruptionMessage({toolUse:true})`
        // (`[Request interrupted by user for tool use]`). Capture the
        // checkpoint-1015 state HERE (before the drive); the single post-drive
        // abort check below picks the reason/message from it. The common
        // "interrupt during thinking/streaming" case (incl. no-tool responses)
        // lands as `aborted_streaming`, not the previous mislabel
        // `aborted_tools`. `None` token (plain `run_turn_streaming`) → always
        // false → byte-identical to before. The drive loop below still flushes
        // synthetic REJECT_MESSAGE tool_results (the executor was built with the
        // cancel token, so pending/queued tools reject rather than execute) —
        // matching ref's `getRemainingResults()` at the 1015 path.
        let aborted_during_stream = user_cancel
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled);

        let (tool_prevent_continuation, post_tool_batch_calls) = orch
            .drive_streaming_tools(&mut exec, &pumped, &tool_use_parent_uuids, &assistant_uuid)
            .await;

        Ok(FinalizedStreamingIteration {
            pumped,
            assistant_id,
            tool_prevent_continuation,
            post_tool_batch_calls,
            pre_batch_mcp_tool_count,
            partial_finalize,
            partial_finalize_notice_id,
            aborted_during_stream,
        })
    }

    /// One iteration of the streaming turn loop: prepare → pump → finalize →
    /// abort checkpoint → disposition.
    ///
    /// Everything the loop does AFTER its top-of-loop guards lives here, so the
    /// loop itself is the guard sequence plus a three-way branch on what this
    /// returned — the shape §5.4 wants both turn loops to share. The guards stay
    /// outside on purpose: §3.5 gives each entry a different order, and the
    /// streaming cancel guard runs between them and this call.
    async fn run_round(
        orch: &ConversationOrchestrator,
        system_prompt: &Option<String>,
        user_cancel: &Option<CancellationToken>,
        loop_state: &mut TurnLoopState,
        in_human_turn: bool,
    ) -> Result<StepExit, OrchestratorError> {
        let prepared = match Self::prepare_iteration(
            orch,
            system_prompt,
            user_cancel,
            loop_state,
            in_human_turn,
        )
        .await?
        {
            PrepareStreamingOutcome::Ready(iteration) => iteration,
            PrepareStreamingOutcome::Complete(message_id) => {
                return Ok(StepExit::FinishThroughEpilogue(message_id));
            }
        };

        let pumped =
            Self::pump_iteration(orch, prepared, system_prompt, user_cancel, loop_state).await;
        let pumped_iteration = match pumped {
            Err(error) => {
                // `open_iteration` enables SDK frame buffering before the
                // stream is opened. Every error path must release that
                // session-scoped buffer or later tool frames disappear.
                orch.set_tool_frame_buffering(false).await;
                return Err(error);
            }
            Ok(PumpStreamingOutcome::Pumped(iteration)) => iteration,
            Ok(PumpStreamingOutcome::Complete(message_id)) => {
                orch.set_tool_frame_buffering(false).await;
                return Ok(StepExit::FinishThroughEpilogue(message_id));
            }
        };

        let finalized = match Self::finalize_iteration(
            orch,
            pumped_iteration,
            system_prompt,
            user_cancel,
            loop_state,
        )
        .await
        {
            Ok(finalized) => finalized,
            Err(error) => {
                orch.set_tool_frame_buffering(false).await;
                return Err(error);
            }
        };
        let FinalizedStreamingIteration {
            pumped,
            assistant_id,
            tool_prevent_continuation,
            post_tool_batch_calls,
            pre_batch_mcp_tool_count,
            partial_finalize,
            partial_finalize_notice_id,
            aborted_during_stream,
        } = finalized;

        // A3: accumulate this step's output tokens (TS
        // `getTurnOutputTokens()`), ONCE, after the model step has returned
        // and before the disposition reads the running total.
        //
        // §3.4 wants a single accumulation boundary per step. This used to
        // sit inside `finalize_iteration`, where the two batched entries
        // accumulate in their loop bodies instead — three sites, one of them
        // a step deeper than the others. Moving it here puts all three on
        // the same boundary. Safe because nothing reads `global_turn_tokens`
        // between the old site and the disposition that consumes it, and
        // `streaming_budget_on_continues_then_stops_at_threshold` fails if
        // the accumulation lands after the budget check rather than before.
        loop_state.global_turn_tokens = loop_state
            .global_turn_tokens
            .saturating_add(pumped.output_tokens);

        // DEFERRED-3 / esc-interrupt FIX: "we were aborted" — the single
        // post-drive abort checkpoint. Once the user-interrupt token has fired,
        // the executor above already drained the bare REJECT_MESSAGE
        // `tool_result`s into history (model-visible). The turn MUST now STOP —
        // claude-code returns with NO further `callModel`, honoring
        // REJECT_MESSAGE's "STOP what you are doing and wait for the user".
        // Looping into the `Some("tool_use") => continue` arm below would (1)
        // issue a wasted extra round-trip after every ESC and (2) let a
        // Block-behavior tool emitted on that continuation actually EXECUTE
        // (`abort_reason_for` returns `None` for Block tools) despite the
        // interrupt — both of which claude-code structurally prevents by
        // returning here first. `None` token (plain `run_turn_streaming`) →
        // never fires → identical to before.
        //
        // #5: the terminal reason + interrupt message depend on WHICH ref
        // checkpoint observed the abort (captured in `aborted_during_stream`
        // before the drive): an abort already set when the stream ended is
        // `aborted_streaming` / `[Request interrupted by user]` (query.ts:1015,
        // `toolUse:false`); an abort that fired only DURING the tool drive is
        // `aborted_tools` / `[Request interrupted by user for tool use]`
        // (query.ts:1485, `toolUse:true`).
        if user_cancel
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
        {
            // Abort wins over a result-level end request. Results that were
            // allowed to finish (notably Block-behavior tools) may have
            // recorded one before this checkpoint, so drain the side table
            // even though the end-turn path below is intentionally skipped.
            let _ = orch
                .take_pending_tool_result_turn_ends(
                    &pumped
                        .tool_uses
                        .iter()
                        .map(|tool_use| tool_use.id.clone())
                        .collect::<Vec<_>>(),
                )
                .await;
            let cost = orch.snapshot_cost_real().await;
            let (abort_reason, interrupt_message) = if aborted_during_stream {
                ("aborted_streaming", INTERRUPT_MESSAGE)
            } else {
                ("aborted_tools", INTERRUPT_MESSAGE_FOR_TOOL_USE)
            };
            orch.output.emit_end_turn(abort_reason, &cost).await;
            // NOW-ABORT disambiguation: a `Now`-driven cancellation means the
            // urgent queued command will run next via the between-turn drain —
            // DON'T inject the user-interrupt message. For a plain user
            // interrupt (the default with no reason flag wired) inject as
            // before. claude-code `query.ts:1046-1050`/`1501-1505`:
            // `createUserInterruptionMessage`.
            if orch.cancel_reason_now()
                != crate::prompt::mid_turn_input::CancelReason::QueueNowCommand
            {
                orch.inject_user_message(interrupt_message).await;
            }
            return Ok(StepExit::FinishThroughEpilogue(assistant_id));
        }

        // P1-04 (cc 2.1.199): a finalized partial ends the turn once its
        // dispatched tools have drained (above) and the incomplete-response
        // notice has been surfaced — cc `break e`s out of the stream loop after
        // yielding the notice; it does NOT re-enter the continuation logic. We
        // terminate here (reason `model_error`, matching the api-error catch)
        // rather than looping on the synthesized `tool_use`/`end_turn`, so the
        // user sees the partial + notice and can retry. The synthesized
        // stop_reason still rides on the persisted partial assistant line
        // (patched above) for resume fidelity.
        if partial_finalize.is_some() {
            if crate::turn_loop::truncated_response_recovery_eligible(
                &orch.config.query_source,
                orch.prompt_is_interactive(),
            ) && loop_state.recovery.max_output_tokens_recovery_count
                < MAX_OUTPUT_TOKENS_RECOVERY_LIMIT
            {
                let is_subagent = crate::turn_loop::truncated_response_recovery_is_subagent(
                    &orch.config.query_source,
                );
                let nudge = if is_subagent {
                    crate::turn_loop::TRUNCATED_RESPONSE_RECOVERY_NUDGE_SUBAGENT
                } else {
                    crate::turn_loop::TRUNCATED_RESPONSE_RECOVERY_NUDGE_MAIN
                };
                orch.inject_meta_user_message(nudge).await;
                loop_state.recovery.max_output_tokens_recovery_count = loop_state
                    .recovery
                    .max_output_tokens_recovery_count
                    .saturating_add(1);
                loop_state.recovery.max_output_tokens_override = None;
                return Ok(StepExit::Continue);
            }
            // A finalized partial is terminal before normal tool-result
            // disposition. Do not leave an end marker from a completed
            // result live in the session-scoped side table.
            let _ = orch
                .take_pending_tool_result_turn_ends(
                    &pumped
                        .tool_uses
                        .iter()
                        .map(|tool_use| tool_use.id.clone())
                        .collect::<Vec<_>>(),
                )
                .await;
            let cost = orch.snapshot_cost_real().await;
            orch.output.emit_end_turn("model_error", &cost).await;
            return Ok(StepExit::FinishThroughEpilogue(
                partial_finalize_notice_id.unwrap_or(assistant_id),
            ));
        }

        match orch
            .decide_streaming_disposition(
                loop_state,
                &pumped,
                assistant_id,
                tool_prevent_continuation,
                post_tool_batch_calls,
                pre_batch_mcp_tool_count,
            )
            .await?
        {
            StreamingIterationDisposition::Continue => Ok(StepExit::Continue),
            StreamingIterationDisposition::Complete(message_id) => {
                // Pending input can arrive while the final response is
                // streaming, after the top-of-loop drain has already run.
                // Give it one final drain before committing the natural
                // end-turn so it continues this same turn.
                if orch.drain_mid_turn_input().await {
                    return Ok(StepExit::Continue);
                }
                Ok(StepExit::FinishThroughEpilogue(message_id))
            }
            StreamingIterationDisposition::ForcedComplete(message_id) => {
                Ok(StepExit::FinishThroughEpilogue(message_id))
            }
            // §3.6: the ONE disposition that skips the epilogue.
            StreamingIterationDisposition::Return(outcome) => Ok(StepExit::ReturnDirect(outcome)),
        }
    }

    pub(super) async fn run(self) -> Result<ConversationOutcome, OrchestratorError> {
        let Self {
            orch,
            prompt,
            images,
            user_cancel,
            message_id,
            transient_rewake,
            in_human_turn,
            queued_inputs,
        } = self;

        // Startup Responses WebSocket prewarm is strictly opportunistic. A real
        // user turn must never wait for an in-flight `generate=false` request to
        // finish before it can open its own stream.
        orch.abort_startup_responses_websocket_prewarm();

        // 0. Build the system prompt for THIS turn.
        // claude-code `nre` precedence: `--system-prompt` (override) wins; else
        // the `--agent`-adopted main-thread agent's prompt; else the default.
        let system_prompt: Option<String> = Some(orch.effective_system_prompt().await);

        // A side query left unfinished when the previous user turn ended was
        // keyed to that previous prompt. Never surface it against new intent.
        orch.discard_stale_prefetches().await;

        // 1. Append the user prompt (+ any pasted images) to session history.
        // `images` arrives already decoded (path-based callers ran `load_images`
        // first; the bridge converts inline `ImageRefDto`s straight to sources).
        let submissions: Vec<(QueuedPromptInput, ConversationMessage)> = match queued_inputs {
            Some(inputs) => inputs
                .into_iter()
                .map(|input| {
                    let mut message = ConversationMessage::user(
                        input.message_id.unwrap_or_default(),
                        input.text.clone(),
                    );
                    if let ConversationMessage::User { is_meta, .. } = &mut message {
                        *is_meta = input.is_meta;
                    }
                    (input, message)
                })
                .collect(),
            None => {
                let mut message = ConversationMessage::user_with_images(
                    message_id.unwrap_or_default(),
                    prompt.to_string(),
                    images,
                );
                if let ConversationMessage::User { is_meta, .. } = &mut message {
                    *is_meta = !in_human_turn;
                }
                vec![(
                    QueuedPromptInput {
                        goal_retry_id: None,
                        text: prompt.to_string(),
                        is_meta: !in_human_turn,
                        message_id,
                        ..Default::default()
                    },
                    message,
                )]
            }
        };
        // A human entry owns turn-level UI/hook identity, but never rewrites a
        // neighboring scheduled entry's per-message metadata.
        let primary = submissions
            .iter()
            .position(|(input, _)| !input.is_meta)
            .unwrap_or(0);
        let user_msg = &submissions[primary].1;
        let prior_message_id = {
            let mut s = orch.session.lock().await;
            let prior = s.history.last().map(ConversationMessage::id);
            if !transient_rewake {
                s.history
                    .extend(submissions.iter().map(|(_, message)| message.clone()));
            }
            prior
        };
        if !transient_rewake {
            for (input, message) in &submissions {
                orch.persist_queued_message_to_jsonl(message, input).await;
            }
        }
        // (/rewind) Snapshot the pre-turn file state IN MEMORY, keyed by this
        // user message, so `track_edit` (fired by Edit/Write/NotebookEdit during
        // the turn) records each file's pre-edit backup into it. The POPULATED
        // record is persisted to the transcript at TURN END (see below) — NOT
        // here: at turn start the backup map is empty (no edits yet), and
        // persisting it now would leave disk-based restore (`rewind_from_disk`,
        // which runs after the TUI unwinds and rebuilds the index from these
        // lines) with nothing to restore.
        let file_history_msg_id = (!transient_rewake).then(|| user_msg.id().as_uuid());
        if let (Some(fh), Some(message_id)) = (&orch.file_history, file_history_msg_id) {
            fh.make_snapshot(message_id).await;
        }

        // hooks B4: UserPromptSubmit (streaming twin). A Block aborts before the
        // first stream is opened. No-op when unregistered.
        if !transient_rewake {
            for (input, message) in &submissions {
                // Synthetic entries (cron fires, `/loop` wakeups) are not user
                // prompts: firing the hook for them double-counts telemetry and
                // re-runs `append_ultracode_attachments` per batch entry.
                if input.is_meta {
                    continue;
                }
                if orch
                    .fire_user_prompt_submit(&input.text, message.id())
                    .await
                {
                    return Ok(ConversationOutcome::StopHookPrevented {
                        turn_count: 0,
                        final_message_id: message.id(),
                    });
                }
            }
        }

        orch.begin_output_turn(user_msg.id()).await?;
        let mut loop_state = TurnLoopState::new(
            orch,
            prior_message_id.unwrap_or_else(|| user_msg.id()),
            user_cancel.clone(),
        );
        let final_message_id;
        loop {
            // MID-TURN DRAIN (claude-code query.ts ~1570-1580): drain BEFORE
            // every terminal top-of-loop guard, including `max_turns`. A message
            // can arrive while the previous model/tool step is running; returning
            // for the cap before this consume-once source is polled would discard
            // it. Claude Code 2.1.205 preserves that message when `--max-turns`
            // ends the turn, so inject it into the persisted history first.
            // With no source wired this remains a strict no-op.
            // The cancel guard is NOT part of this: on this path it runs AFTER
            // the increment (below), where the two batched entries have it first
            // or not at all.
            match orch
                .run_turn_loop_guards(loop_state::LoopGuardOrder::Streaming, &mut loop_state)
                .await
            {
                loop_state::GuardVerdict::Proceed => {}
                loop_state::GuardVerdict::MaxTurns => {
                    return Err(OrchestratorError::MaxTurnsReached {
                        max_turns: orch.config.max_turns,
                    })
                }
                loop_state::GuardVerdict::OverBudget => {
                    return Err(OrchestratorError::MaxBudgetReached {
                        budget_nano_usd: orch.config.max_budget_nano_usd.unwrap_or(0),
                    })
                }
            }

            // DEFERRED-3 / esc-interrupt FIX: top-of-loop user-interrupt guard
            // (faithful port of claude-code `query.ts:1015` — the `aborted_streaming`
            // return). If the user-interrupt token is already set when we reach the
            // top of an iteration — a pre-cancel, or an abort that fired during the
            // previous iteration's streaming BEFORE any tool ran — we must STOP
            // BEFORE issuing the next `callModel`. claude-code consumes any
            // remaining streaming results then returns `aborted_streaming` with no
            // further sampling; here the previous iteration already drained its
            // results into history (the post-tools guard) or there were none, so we
            // simply break. This is the structural barrier that prevents a
            // Block-behavior tool on a post-interrupt continuation from ever
            // executing. `None` token → never fires → identical to before.
            if user_cancel
                .as_ref()
                .is_some_and(CancellationToken::is_cancelled)
            {
                let cost = orch.snapshot_cost_real().await;
                orch.output.emit_end_turn("aborted_streaming", &cost).await;
                // NOW-ABORT disambiguation: when the cancellation was driven by a
                // `Now`-priority enqueue (not a user Ctrl+C/ESC), the urgent
                // command IS the "interruption" and will be run next by the
                // between-turn drain — so DON'T inject the user-interrupt message
                // (which would mislabel the abort and pollute context). For a
                // plain user interrupt (the default when no reason flag is wired)
                // behavior is byte-identical to before: inject the message.
                // claude-code `query.ts:1046-1050`: `createUserInterruptionMessage`.
                if orch.cancel_reason_now()
                    != crate::prompt::mid_turn_input::CancelReason::QueueNowCommand
                {
                    orch.inject_user_message(INTERRUPT_MESSAGE).await;
                }
                final_message_id = loop_state.last_message_id;
                break;
            }

            match Self::run_round(
                orch,
                &system_prompt,
                &user_cancel,
                &mut loop_state,
                in_human_turn,
            )
            .await?
            {
                StepExit::Continue => continue,
                StepExit::FinishThroughEpilogue(message_id) => {
                    final_message_id = message_id;
                    break;
                }
                // §3.6: skips the file-history epilogue below, which is exactly
                // what `Return` has always done.
                StepExit::ReturnDirect(outcome) => return Ok(outcome),
            }
        }

        // (/rewind) Persist THIS turn's now-populated file-history snapshot to the
        // transcript — `track_edit` filled its backup map (pre-edit content of
        // every file Edit/Write/NotebookEdit touched) during the turn above.
        // Restore (`rewind_from_disk`) and `--resume` rebuild the index from
        // these lines, so the record must carry the backups, not the empty map
        // it had at turn start. Persisted unconditionally (an edit-free turn
        // still records a restore point for conversation-only rewind).
        if let (Some(fh), Some(file_history_msg_id)) = (&orch.file_history, file_history_msg_id) {
            if let (Some(record), Some(writer)) = (
                fh.snapshot_record(file_history_msg_id),
                &orch.transcript.jsonl_writer,
            ) {
                let session_id = orch.session.lock().await.session_id;
                let session_uuid = session_id.as_uuid().to_string();
                let line = session::file_history::snapshot_line_json(&session_uuid, &record);
                if let Err(error) = writer.append_file_history_snapshot(&line).await {
                    orch.record_transcript_append_failure(
                        &session_id.to_string(),
                        "file_history_snapshot",
                        &error,
                    )
                    .await;
                }
            }
        }

        Ok(ConversationOutcome::EndTurn {
            turn_count: loop_state.turn_count,
            final_message_id,
        })
    }
}
