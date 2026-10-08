use super::tool_results::persist_keep_recent_clears;
use crate::conversation::{
    ConversationOrchestrator, SessionCompactCore, SessionCompactCoreOutput, SessionCompactDecision,
};
use crate::error::OrchestratorError;
use hooks::attachment::HookPublicationGuard;
use lingxi_core::types::{ConversationMessage, MessageId};
use llm_runtime::{HistoryResponse, LlmError};
use std::sync::Arc;

/// Apply the native request-parameter envelope at the model-input boundary.
pub(crate) fn apply_context_hint_params(
    request: &mut llm_runtime::MessagesCreateRequest,
    params: compaction::context_hint::ContextHintRequestParams,
) {
    request.opts.context_hint_beta = !params.beta.is_empty();
    request.opts.context_hint = params
        .body
        .and_then(|body| body.get("context_hint").cloned());
}

/// The current native producer carries these controls independently: a hint
/// offer does not replace an output escalation or configured model fallback.
/// Native 2.1.287 `src_185762130.js` computes the cap at bytes 2089737..2089841,
/// builds hint params at 2092426..2092502, and combines both at 2094773..2095445.
/// Retry inputs retain fallback and overload state at 2120624..2120989 and
/// 2127356..2127797. The byte ranges are half-open.
pub(crate) fn apply_main_request_options(
    request: &mut llm_runtime::MessagesCreateRequest,
    max_tokens_override: Option<u32>,
    hint_params: Option<compaction::context_hint::ContextHintRequestParams>,
    fallback_models: Option<&str>,
) {
    request.opts.max_output_tokens = max_tokens_override;
    if let Some(params) = hint_params {
        apply_context_hint_params(request, params);
    }
    request.opts.fallback = fallback_models
        .map(llm_runtime::FallbackPolicy::from_models_csv)
        .unwrap_or(llm_runtime::FallbackPolicy::Disabled);
}

/// Outcome of [`call_api_with_ptl_recovery`]: either a successful
/// `HistoryResponse`, or a signal that the prompt-too-long reactive recovery
/// (Batch 5) was exhausted and the turn should end with the byte-exact
/// [`PROMPT_TOO_LONG_ERROR_MESSAGE`].
pub(crate) enum PtlCallOutcome {
    /// The API call (or a retry after truncation/compaction) succeeded.
    Response {
        response: Box<HistoryResponse>,
        /// The exact successful physical request's messages. Recovery can
        /// rewrite or compact them; tools must not reread later session state.
        request_history: Vec<ConversationMessage>,
    },
    /// The PTL retry budget + reactive-compact fallback were all exhausted.
    /// End the turn with terminal reason `"prompt_too_long"` (the REACTIVE
    /// exhaustion path, `query.ts:1175`).
    PromptTooLong,
    /// The PROACTIVE blocking-limit preempt fired: the prompt was already at
    /// the hard blocking limit (`token_usage >= effective_window −
    /// MANUAL_COMPACT_BUFFER_TOKENS`) BEFORE the call, so the turn ends with
    /// the DISTINCT terminal reason `"blocking_limit"` — not the reactive
    /// `"prompt_too_long"`. The binary keeps these two terminals separate
    /// (`bin/claude.exe` offset ~208021400: the proactive arm returns
    /// `{reason:"blocking_limit"}` while the reactive arm returns
    /// `{reason:"prompt_too_long"}`; the terminal-reason enum lists both).
    BlockingLimit,
    /// #54: the rapid-refill (thrashing) breaker tripped on the reactive PTL
    /// path — re-compacting cannot help, so surface the byte-exact thrashing
    /// message and end the turn with `reason:"rapid_refill_breaker"`
    /// (`bin/claude.exe` offset 202942256).
    RapidRefillBreaker,
}

/// Wrap the batched `messages_create` with the 413 / prompt-too-long reactive
/// recovery loop (In-Loop Compaction Batch 5, BATCHED path only).
///
/// TS refs: `query.ts:628-648` (blocking-limit preempt),
/// Claude Code 2.1.261 retries context-collapse projections first, then makes
/// one reactive compaction attempt against the unchanged conversation. The
/// summary call moves complete trailing API rounds out of its request on PTL;
/// only a successful summary commits a replacement history. An unwired or
/// failed compactor surfaces `PromptTooLong` without deleting prior messages.
pub(crate) async fn call_api_with_ptl_recovery(
    orch: &ConversationOrchestrator,
    system: Option<&lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput>,
    skip_global_cache_for_system_prompt: bool,
    model: &str,
    profile: Option<&str>,
    mut history_snapshot: Vec<ConversationMessage>,
    outgoing_history_rewriter: Option<Arc<dyn crate::conversation::OutgoingHistoryRewriter>>,
    tools: Vec<serde_json::Value>,
    max_tokens_override: Option<u32>,
    // This step's transient deferred-tool delta. Like the date-change reminder,
    // it must be reattached when a retry rebuilds from raw session history.
    deferred_tools_reminder: Option<ConversationMessage>,
    // This step's transient `date_change` reminder (first on desktop, directly
    // after the fixed runtime snapshot on mobile). Retry/fallback paths rebuild
    // from raw `session.history`, so it is reattached there too.
    date_change_reminder: Option<ConversationMessage>,
    // This step's per-turn transient reminders (skill listing, conditional
    // rules, nested memory, diagnostics, …), already appended to
    // `history_snapshot`. Computing them ADVANCES session state — sent-sets,
    // delta trackers, consume-once drains — so they can never be recomputed for
    // a retry; recomputing returns `None` and the reminder is lost for the rest
    // of the session. Re-appended below wherever the request is rebuilt from
    // raw `session.history`.
    mut turn_reminders: Vec<ConversationMessage>,
    guarded_async_hook_reminders: &mut Vec<(MessageId, Arc<dyn HookPublicationGuard>)>,
    context_announcements: &crate::conversation::PreparedContextAnnouncements,
    // Exact originating-session accounting authority captured before the
    // first provider dispatch. Explicit recovery calls reuse it rather than
    // resolving whichever session happens to be active later.
    cost_scope: Option<&cost::CostSessionScope>,
) -> Result<PtlCallOutcome, OrchestratorError> {
    crate::prompt::async_hook_response::retain_current_async_hook_reminders(
        &mut history_snapshot,
        &mut turn_reminders,
        guarded_async_hook_reminders,
    );
    orch.sync_thinking_signature_strip_flag_to_api().await;
    // A first request after resume may overflow before any successful call
    // has populated the summary fork's cache-safe slot.
    let display_system = system.map(
        lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput::display_text,
    );
    orch.save_cache_safe_params(display_system.as_deref(), model, &tools)
        .await;
    // SC-04: the compaction-failure detail is per-CALL state (the oracle reads
    // it off THIS iteration's `precomputeOutcome`), so clear any leftover before
    // the preempt — a failure recorded for an earlier call must never colour
    // this call's prompt-too-long surface.
    orch.compaction_runtime
        .compaction_tracking
        .lock()
        .await
        .last_compact_failure_detail = None;

    crate::prompt::async_hook_response::retain_current_async_hook_reminders(
        &mut history_snapshot,
        &mut turn_reminders,
        guarded_async_hook_reminders,
    );

    // (1) Blocking-limit preempt. Context collapse bypasses this proactive
    // guard so a real overflow can first drain its staged summaries. When the
    // feature is off, `is_at_blocking_limit` is
    // `token_usage >= effective_window − MANUAL_COMPACT_BUFFER_TOKENS`
    // (`autoCompact.ts` `calculateTokenWarningState`). `auto_compact_enabled`
    // is `true` to mirror the always-on default of this port (no GrowthBook).
    let estimate = compaction::grouping::estimate_tokens_for_range(&history_snapshot);
    let active_betas = orch.api.active_betas();
    let warning = compaction::calculate_token_warning_state(estimate, model, &active_betas, true);

    // SC-06: the one-shot unknown-model auto-compact notice (`Pk0`
    // @306646044). Upstream emits it from the REPL launcher (@306693668) as
    // `cz(<notice>)` when interactive, or `T("[autocompact] <notice>",
    // {level:"warn"})` when the output is json/stream-json or the session kind
    // is `bg`. The port emits it HERE, on the first window resolution of the
    // session, because LingXi's model registry is populated at catalog-assembly
    // time — i.e. AFTER the launcher — so at the oracle's emit point every
    // third-party model still looks unrecognized. `_once` latches it, so this
    // costs one relaxed atomic load per turn afterwards.
    //
    // Landed on the warn log, which is the oracle's own non-interactive branch
    // verbatim; the port has no `cz`-equivalent console-notice channel for the
    // interactive branch.
    if let Some(notice) = compaction::thresholds::unknown_model_window_notice_once(
        model,
        &active_betas,
        None,
        compaction::thresholds::is_auto_compact_enabled(true),
    ) {
        tracing::warn!("[autocompact] {notice}");
    }

    // Push the live context-pressure banner to the UI — the orchestrator-side
    // twin of claude-code's `<TokenWarning>` render
    // (`PromptInput/Notifications.tsx:321`), which recomputes
    // `calculateTokenWarningState` as `tokenUsage` grows. We reuse the SAME
    // `estimate` the auto-compact gate uses (claude-code's `tokenUsage`), so the
    // banner's thresholds match the gate exactly. `None` clears a previously
    // shown banner once the context drops back below the warning threshold
    // (e.g. after a compaction). Default no-op for non-interactive sinks.
    let banner = compaction::token_warning_banner(
        &warning,
        compaction::thresholds::is_auto_compact_enabled(true),
        compaction::is_compact_warning_suppressed(),
        None,
    )
    .map(|b| lingxi_core::host::ContextPressureBanner {
        text: b.text,
        level: match b.color {
            compaction::TokenWarningColor::Dim => lingxi_core::host::ContextPressureLevel::Dim,
            compaction::TokenWarningColor::Warning => {
                lingxi_core::host::ContextPressureLevel::Warning
            }
            compaction::TokenWarningColor::Error => lingxi_core::host::ContextPressureLevel::Error,
        },
    });
    // Context usage as a 0-1 fraction of the model's effective context window
    // (claude-code `calculateContextPercentages(currentUsage, contextWindowSize)`),
    // emitted every turn — even when no warning banner shows — so the custom
    // statusline's `context_window.used_percentage` is always live. Reuses the
    // SAME `estimate` and active request betas as the banner/auto-compact gate.
    let context_window =
        compaction::thresholds::effective_context_window_size(model, &active_betas);
    let used_fraction = if context_window == 0 {
        0.0
    } else {
        (estimate as f64 / context_window as f64) as f32
    };
    orch.output
        .emit_context_pressure(banner, used_fraction, estimate, context_window)
        .await;

    if warning.is_at_blocking_limit && !compaction::is_context_collapse_enabled() {
        tracing::warn!(
            estimate,
            model,
            "prompt at blocking limit — preempting before API call"
        );
        // PROACTIVE preempt ⇒ terminal reason `"blocking_limit"` (distinct from
        // the reactive-exhausted `PromptTooLong` returned at the tail). No
        // request is issued, so the `date_change` reminder stays UNCOMMITTED and
        // the next step re-emits it.
        return Ok(PtlCallOutcome::BlockingLimit);
    }
    // Past the preempt: the snapshot WILL be sent, so this step's `date_change`
    // reminder counts as delivered.
    orch.commit_date_change_reminder();

    // Context-hint negotiation (oracle `e1y`): offer the server a compact we
    // could perform, and act on a 422/424 asking us to. `None` unless BOTH the
    // route allows first-party betas and the controller's own env gate is on —
    // and the latter is off by default because the oracle's server-delivered
    // `tengu_hazel_osprey` is false. So this is inert on every ordinary turn.
    //
    // `repl_main_thread` is this driver by definition: `call_api_with_ptl_recovery`
    // is the MAIN turn's API seam. Subagents and side queries run their own
    // paths and never reach here, which is what the oracle's querySource prefix
    // check expresses.
    let mut hint_controller = compaction::context_hint::create_context_hint_controller(
        orch.config.include_first_party_betas,
        "repl_main_thread",
    );

    if let Some(scope) = cost_scope {
        scope.preflight().await.map_err(|error| {
            OrchestratorError::Internal(format!("cost durability preflight failed: {error}"))
        })?;
    }
    let mut output_observation = orch.capture_main_output().await?;
    crate::prompt::async_hook_response::retain_current_async_hook_reminders(
        &mut history_snapshot,
        &mut turn_reminders,
        guarded_async_hook_reminders,
    );
    let hint_params = hint_controller
        .as_mut()
        .and_then(|controller| controller.build_request_params(&history_snapshot));
    let mut request = llm_runtime::MessagesCreateRequest::new(
        model,
        profile,
        system.cloned(),
        history_snapshot,
        tools.clone(),
    );
    request.opts.skip_global_cache_for_system_prompt = skip_global_cache_for_system_prompt;
    request.opts.query_source =
        Some(crate::config::sanitize_query_source(&orch.config.query_source).to_string());
    apply_main_request_options(
        &mut request,
        max_tokens_override,
        hint_params,
        orch.config.fallback_model.as_deref(),
    );
    crate::prompt::async_hook_response::retain_current_async_hook_reminders(
        &mut request.messages,
        &mut turn_reminders,
        guarded_async_hook_reminders,
    );
    request.opts.request_dispatch_admission =
        crate::prompt::async_hook_response::request_dispatch_admission(
            &request.messages,
            guarded_async_hook_reminders,
        );
    let request_history = request.messages.clone();
    let first = orch
        .model_runtime
        .prompt_cache_capture
        .scope(
            crate::native_computer::call_model(orch, request),
        )
        .await;

    // Map `LlmError::ContextOverflow` to the PTL recovery path.
    // The `token_gap` field carries the actual-minus-limit count parsed from the
    // provider error message by `llm_runtime`; the PTL truncator treats `0` as
    // "unknown" and falls back to its 20% heuristic.
    let token_gap: u64 = match first {
        Ok(resp) => {
            if let Some(observation) = &mut output_observation {
                observation.observe(&resp.usage);
                let _ = observation.finish();
            }
            return Ok(PtlCallOutcome::Response {
                response: Box::new(resp),
                request_history,
            });
        }
        Err(LlmError::ContextOverflow { token_gap }) => token_gap,
        Err(other) => {
            // Context-hint error half (oracle `onRequestError`). A 422/424 is
            // the server asking for the compact we offered: apply the edits and
            // re-issue ONCE. Every other outcome (beta unsupported, 409, 529)
            // falls through to the normal error return, exactly as the oracle
            // does — those branches edit nothing.
            //
            // The status is recoverable because the decoder writes it into the
            // message (`providers::api_error_message`); before that, a 422 and a
            // 400 were the same `LlmError`.
            if let Some(c) = hint_controller.as_mut() {
                let facts = compaction::context_hint::HttpErrorFacts::from_error(&other);
                // Re-snapshot rather than clone the history up front: the
                // snapshot was moved into the call, and every other recovery
                // path here rebuilds the same way.
                let raw_history = {
                    let s = orch.session.lock().await;
                    s.model_context_history()
                };
                // CMP-2 / TL-6: write the about-to-be-cleared tool results to
                // the session's `tool-results/` directory FIRST, so the clear
                // leaves the model a file it can `Read` instead of only
                // "[Old tool result content cleared]". Upstream's `Sir` awaits
                // `persist` per candidate and hands `lCt` the resulting map;
                // this is that map, built ahead of the (sync) controller call.
                //
                // Gated on `is_hint_reject`, because every OTHER error outcome
                // clears nothing — persisting there would write files for
                // results that stay in the conversation.
                let persisted_clears = if compaction::context_hint::is_hint_reject(&facts) {
                    persist_keep_recent_clears(orch, &raw_history).await
                } else {
                    std::collections::HashMap::new()
                };
                if let compaction::context_hint::HintErrorOutcome::Reject(edits, _event) =
                    c.on_request_error_with_persisted(&facts, raw_history, &persisted_clears)
                {
                    let retry_raw = edits.messages.clone();
                    {
                        let mut s = orch.session.lock().await;
                        s.replace_model_context_history(edits.messages.clone());
                    }
                    orch.expect_prompt_cache_rebuild().await;
                    let mut retry = orch
                        .rewrite_outgoing_history(retry_raw, outgoing_history_rewriter.as_ref())
                        .await?;
                    orch.reattach_outgoing_context(
                        &mut retry,
                        deferred_tools_reminder.as_ref(),
                        date_change_reminder.as_ref(),
                        &mut turn_reminders,
                        guarded_async_hook_reminders,
                        context_announcements,
                        false,
                    )
                    .await;
                    if let Some(scope) = cost_scope {
                        scope.preflight().await.map_err(|error| {
                            OrchestratorError::Internal(format!(
                                "cost durability preflight failed: {error}"
                            ))
                        })?;
                    }
                    crate::prompt::async_hook_response::retain_current_async_hook_reminders(
                        &mut retry,
                        &mut turn_reminders,
                        guarded_async_hook_reminders,
                    );
                    let retry_hint_params = c.build_request_params(&retry);
                    let mut request = llm_runtime::MessagesCreateRequest::new(
                        model,
                        profile,
                        system.cloned(),
                        retry,
                        tools.clone(),
                    );
                    request.opts.skip_global_cache_for_system_prompt =
                        skip_global_cache_for_system_prompt;
                    request.opts.query_source = Some(
                        crate::config::sanitize_query_source(&orch.config.query_source).to_string(),
                    );
                    apply_main_request_options(
                        &mut request,
                        max_tokens_override,
                        retry_hint_params,
                        orch.config.fallback_model.as_deref(),
                    );
                    crate::prompt::async_hook_response::retain_current_async_hook_reminders(
                        &mut request.messages,
                        &mut turn_reminders,
                        guarded_async_hook_reminders,
                    );
                    request.opts.request_dispatch_admission =
                        crate::prompt::async_hook_response::request_dispatch_admission(
                            &request.messages,
                            guarded_async_hook_reminders,
                        );
                    let request_history = request.messages.clone();
                    return match orch
                        .model_runtime
                        .prompt_cache_capture
                        .scope(
                            crate::native_computer::call_model(orch, request),
                        )
                        .await
                    {
                        Ok(resp) => {
                            if let Some(observation) = &mut output_observation {
                                observation.observe(&resp.usage);
                                let _ = observation.finish();
                            }
                            Ok(PtlCallOutcome::Response {
                                response: Box::new(resp),
                                request_history,
                            })
                        }
                        Err(e) => Err(e.into()),
                    };
                }
            }
            return Err(other.into());
        }
    };

    // Context-collapse overflow recovery: drain every already-summarized staged
    // span, persist the resulting append-only commits + last-wins snapshot, and
    // retry once with the read-time projection before reactive compaction.
    // A second overflow falls through to
    // the established recovery chain; the staged queue is now empty, so the
    // drain is naturally one-shot.
    if compaction::is_context_collapse_enabled() {
        if let Some(compactor) = orch.compaction_runtime.compaction.as_ref() {
            let raw_history = {
                let session = orch.session.lock().await;
                session.model_context_history()
            };
            let drained = compactor
                .context_collapse
                .recover_from_overflow(raw_history.clone());
            if !drained.commits.is_empty() {
                orch.persist_context_collapse_drain(&drained).await;
                let mut retry = orch
                    .rewrite_outgoing_history(raw_history, outgoing_history_rewriter.as_ref())
                    .await?;
                orch.reattach_outgoing_context(
                    &mut retry,
                    deferred_tools_reminder.as_ref(),
                    date_change_reminder.as_ref(),
                    &mut turn_reminders,
                    guarded_async_hook_reminders,
                    context_announcements,
                    false,
                )
                .await;
                if let Some(scope) = cost_scope {
                    scope.preflight().await.map_err(|error| {
                        OrchestratorError::Internal(format!(
                            "cost durability preflight failed: {error}"
                        ))
                    })?;
                }
                crate::prompt::async_hook_response::retain_current_async_hook_reminders(
                    &mut retry,
                    &mut turn_reminders,
                    guarded_async_hook_reminders,
                );
                let retry_hint_params = hint_controller
                    .as_mut()
                    .and_then(|controller| controller.build_request_params(&retry));
                let mut request = llm_runtime::MessagesCreateRequest::new(
                    model,
                    profile,
                    system.cloned(),
                    retry,
                    tools.clone(),
                );
                request.opts.query_source = Some(
                    crate::config::sanitize_query_source(&orch.config.query_source).to_string(),
                );
                apply_main_request_options(
                    &mut request,
                    max_tokens_override,
                    retry_hint_params,
                    orch.config.fallback_model.as_deref(),
                );
                crate::prompt::async_hook_response::retain_current_async_hook_reminders(
                    &mut request.messages,
                    &mut turn_reminders,
                    guarded_async_hook_reminders,
                );
                request.opts.request_dispatch_admission =
                    crate::prompt::async_hook_response::request_dispatch_admission(
                        &request.messages,
                        guarded_async_hook_reminders,
                    );
                let request_history = request.messages.clone();
                let retry_result = orch
                    .model_runtime
                    .prompt_cache_capture
                    .scope(
                        crate::native_computer::call_model(orch, request),
                    )
                    .await;
                match retry_result {
                    Ok(resp) => {
                        if let Some(observation) = &mut output_observation {
                            observation.observe(&resp.usage);
                            let _ = observation.finish();
                        }
                        return Ok(PtlCallOutcome::Response {
                            response: Box::new(resp),
                            request_history,
                        });
                    }
                    Err(LlmError::ContextOverflow { .. }) => {}
                    Err(other) => return Err(other.into()),
                }
            }
        }
    }

    // A provider overflow is recovered by summarizing a prefix and preserving
    // its recent rounds. Never delete live history before that summary succeeds.
    // (4) Reactive-compact fallback: one full compact, then retry once more.
    if let Some(compactor) = orch.compaction_runtime.compaction.clone() {
        let snapshot = orch.session.lock().await.model_context_history();
        let original_snapshot = snapshot.clone();
        // hooks compaction lifecycle: PreCompact fires before the reactive
        // summary pass. The reactive 413/PTL fallback is part of the automatic
        // recovery pipeline, so the trigger is `auto` (TS treats reactive
        // overflow recovery as a non-manual compact). TS reactive arm: a
        // blocking PreCompact hook logs `Reactive compact blocked by PreCompact
        // hook: <blockedBy>` and aborts recovery — with no compaction the prompt
        // is still over the limit, so we surface the prompt-too-long outcome
        // (the same value this fn falls through to).
        let compact_started = std::time::Instant::now();
        orch.output.emit_compaction_started().await;
        let pre_compact = orch.fire_pre_compact("auto", None).await;
        if let Some(detail) = pre_compact.blocked_by {
            tracing::warn!("Reactive compact blocked by PreCompact hook: {detail}");
            orch.output
                .emit_compaction_finished(Some(&format!(
                    "Compaction blocked by PreCompact hook: {detail}"
                )))
                .await;
            return Ok(PtlCallOutcome::PromptTooLong);
        }
        // Cost preflight precedes any core call. The Mod may skip without
        // calling next(e); only the callback below starts the summarizer.
        let reactive_cost_scope = orch.compaction_cost_scope().await.map_err(|error| {
            OrchestratorError::Internal(format!(
                "reactive compaction cost preflight failed: {error}"
            ))
        })?;
        let tracking = Arc::new(tokio::sync::Mutex::new(
            orch.compaction_runtime
                .compaction_tracking
                .lock()
                .await
                .clone(),
        ));
        let core_error = Arc::new(tokio::sync::Mutex::new(None));
        let compact_for_core = compactor.clone();
        let tracking_for_core = tracking.clone();
        let error_for_core = core_error.clone();
        let phase_output = orch.output.clone();
        let core: SessionCompactCore = Arc::new(move |source_messages, instructions| {
            let compactor = compact_for_core.clone();
            let tracking = tracking_for_core.clone();
            let core_error = error_for_core.clone();
            let output = phase_output.clone();
            Box::pin(async move {
                output.emit_compaction_phase("summarizing").await;
                let api_started = std::time::Instant::now();
                let result = {
                    let mut tracking = tracking.lock().await;
                    compactor
                        .process_reactive_tracked(
                            source_messages.clone(),
                            &mut tracking,
                            instructions.as_deref(),
                            (token_gap > 0).then_some(token_gap),
                        )
                        .await
                };
                match result {
                    Ok(result) => Ok(SessionCompactCoreOutput::new(
                        result,
                        source_messages,
                        api_started.elapsed(),
                    )),
                    Err(error) => {
                        *core_error.lock().await = Some(error.clone());
                        Err(hooks::mods::ModError::Native(error.to_string()))
                    }
                }
            })
        });
        let compacted = orch
            .dispatch_session_compact_mods(
                "auto",
                snapshot,
                pre_compact.additional_instructions.as_deref(),
                core,
            )
            .await;
        *orch.compaction_runtime.compaction_tracking.lock().await = tracking.lock().await.clone();
        let (compact_result, compact_duration, source_messages) = match compacted {
            Ok(SessionCompactDecision::Continue {
                result,
                source_messages,
                api_duration,
                ..
            }) => (Ok(result), api_duration, source_messages),
            Ok(SessionCompactDecision::Skip { reason, .. }) => {
                tracing::warn!(%reason, "Reactive compact skipped by session.compact Mod");
                orch.output.emit_compaction_finished(Some(&reason)).await;
                return Ok(PtlCallOutcome::PromptTooLong);
            }
            Err(error) => {
                let error =
                    core_error.lock().await.take().unwrap_or_else(|| {
                        compaction::CompactionError::Internal(error.to_string())
                    });
                (Err(error), std::time::Duration::ZERO, original_snapshot)
            }
        };
        // A Mod may rewrite the source rows before next(e). Boundary facts
        // describe the rows that the real compactor received.
        let messages_before = u32::try_from(source_messages.len()).unwrap_or(u32::MAX);
        let bytes_before: u64 = source_messages
            .iter()
            .map(lingxi_core::types::text_byte_size)
            .sum();
        let pre_tokens_estimate = compaction::grouping::estimate_tokens_for_range(&source_messages);
        // A successful summarizer response is owned before the failure-detail,
        // telemetry, or output awaits below.
        let compact_cost_receipt = compact_result.as_ref().ok().and_then(|result| {
            orch.begin_compaction_usage(reactive_cost_scope.as_ref(), result, compact_duration)
        });
        // SC-04 (oracle `Fol`, cc-238.js @228433532): a FAILED rescue compact is
        // what upgrades the bare `Prompt is too long` into
        // `Prompt is too long · automatic compaction failed: <detail>`. Stash the
        // detail so `surface_prompt_too_long` can render the composed copy the
        // way the oracle's `ep({content:Fol(qn)??_V,…})` does; it is consumed
        // once, and cleared on entry to this fn, so it can never leak into a
        // later turn's preempt.
        if let Err(err) = &compact_result {
            orch.compaction_runtime
                .compaction_tracking
                .lock()
                .await
                .last_compact_failure_detail = Some(err.to_string());
            orch.output
                .emit_compaction_finished(Some(&err.to_string()))
                .await;
        }
        if let Ok(result) = compact_result {
            orch.settle_compaction_usage(compact_cost_receipt)
                .await
                .map_err(|error| {
                    OrchestratorError::Internal(format!(
                        "reactive compaction cost settlement failed: {error}"
                    ))
                })?;
            // #54 reactive rapid-refill (thrashing) breaker: if the reactive
            // compact tripped the breaker, re-compacting cannot help (a single
            // file/tool output is too large). Emit telemetry + surface the
            // byte-exact thrashing message and end the turn — mirroring the
            // binary's reactive arm (`bin/claude.exe` offset 202942256).
            if result.rapid_refill_breaker_tripped {
                let turns_since = {
                    let tracking = orch.compaction_runtime.compaction_tracking.lock().await;
                    i64::from(tracking.turn_counter)
                };
                orch.fire_rapid_refill_breaker_telemetry_reactive(
                    result.consecutive_rapid_refills,
                    turns_since,
                )
                .await;
                orch.output
                    .emit_compaction_finished(Some(compaction::RAPID_REFILL_THRASHING_MESSAGE))
                    .await;
                return Ok(PtlCallOutcome::RapidRefillBreaker);
            }
            if !result.was_compacted {
                // Close progress-aware clients even when the reactive pass was
                // skipped. This callback leaves the legacy SDK sequence intact.
                orch.output.emit_compaction_phase("skipped").await;
            }
            if result.was_compacted {
                // Apply the post-compact transition (history swap + boundary
                // marker + CompactionCompleted) via the shared helper.
                // `cancel: None` — the reactive PTL fallback has no
                // user-cancellable surface, so the apply is infallible.
                orch.apply_post_compact(
                    result,
                    compaction::CompactTrigger::Auto,
                    pre_tokens_estimate,
                    messages_before,
                    bytes_before,
                    compact_started,
                    None,
                )
                .await;
                let history_raw = {
                    let s = orch.session.lock().await;
                    s.model_context_history()
                };
                let mut history = orch
                    .rewrite_outgoing_history(history_raw, outgoing_history_rewriter.as_ref())
                    .await?;
                orch.reattach_outgoing_context(
                    &mut history,
                    deferred_tools_reminder.as_ref(),
                    date_change_reminder.as_ref(),
                    &mut turn_reminders,
                    guarded_async_hook_reminders,
                    context_announcements,
                    true,
                )
                .await;
                if let Some(scope) = cost_scope {
                    scope.preflight().await.map_err(|error| {
                        OrchestratorError::Internal(format!(
                            "cost durability preflight failed: {error}"
                        ))
                    })?;
                }
                crate::prompt::async_hook_response::retain_current_async_hook_reminders(
                    &mut history,
                    &mut turn_reminders,
                    guarded_async_hook_reminders,
                );
                let retry_hint_params = hint_controller
                    .as_mut()
                    .and_then(|controller| controller.build_request_params(&history));
                let mut request = llm_runtime::MessagesCreateRequest::new(
                    model,
                    profile,
                    system.cloned(),
                    history,
                    tools,
                );
                request.opts.query_source = Some(
                    crate::config::sanitize_query_source(&orch.config.query_source).to_string(),
                );
                apply_main_request_options(
                    &mut request,
                    max_tokens_override,
                    retry_hint_params,
                    orch.config.fallback_model.as_deref(),
                );
                crate::prompt::async_hook_response::retain_current_async_hook_reminders(
                    &mut request.messages,
                    &mut turn_reminders,
                    guarded_async_hook_reminders,
                );
                request.opts.request_dispatch_admission =
                    crate::prompt::async_hook_response::request_dispatch_admission(
                        &request.messages,
                        guarded_async_hook_reminders,
                    );
                let request_history = request.messages.clone();
                match orch
                    .model_runtime
                    .prompt_cache_capture
                    .scope(
                        crate::native_computer::call_model(orch, request),
                    )
                    .await
                {
                    Ok(resp) => {
                        if let Some(observation) = &mut output_observation {
                            observation.observe(&resp.usage);
                            let _ = observation.finish();
                        }
                        return Ok(PtlCallOutcome::Response {
                            response: Box::new(resp),
                            request_history,
                        });
                    }
                    Err(LlmError::ContextOverflow { .. }) => {}
                    Err(other) => return Err(other.into()),
                }
            }
        }
    }

    // Still over the limit after truncation + one reactive compact: surface the
    // byte-exact prompt-too-long message and end the turn (no hard error).
    Ok(PtlCallOutcome::PromptTooLong)
}
