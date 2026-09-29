use super::tool_results::persist_keep_recent_clears;
use crate::conversation::ConversationOrchestrator;
use crate::error::OrchestratorError;
use llm_runtime::{LlmError, LlmResponse};
use protocol::ConversationMessage;
use std::sync::Arc;

/// Outcome of [`call_api_with_ptl_recovery`]: either a successful
/// `LlmResponse`, or a signal that the prompt-too-long reactive recovery
/// (Batch 5) was exhausted and the turn should end with the byte-exact
/// [`PROMPT_TOO_LONG_ERROR_MESSAGE`].
pub(crate) enum PtlCallOutcome {
    /// The API call (or a retry after truncation/compaction) succeeded.
    Response(Box<LlmResponse>),
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
    system: Option<&str>,
    model: &str,
    profile: Option<&str>,
    history_snapshot: Vec<ConversationMessage>,
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
    turn_reminders: &[ConversationMessage],
    // Exact originating-session accounting authority captured before the
    // first provider dispatch. Explicit recovery calls reuse it rather than
    // resolving whichever session happens to be active later.
    cost_scope: Option<&cost::CostSessionScope>,
) -> Result<PtlCallOutcome, OrchestratorError> {
    orch.sync_thinking_signature_strip_flag_to_api().await;
    // A first request after resume may overflow before any successful call
    // has populated the summary fork's cache-safe slot.
    orch.save_cache_safe_params(system, model, &tools).await;
    // SC-04: the compaction-failure detail is per-CALL state (the oracle reads
    // it off THIS iteration's `precomputeOutcome`), so clear any leftover before
    // the preempt — a failure recorded for an earlier call must never colour
    // this call's prompt-too-long surface.
    orch.compaction_runtime
        .compaction_tracking
        .lock()
        .await
        .last_compact_failure_detail = None;

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
    .map(|b| platform_api::ContextPressureBanner {
        text: b.text,
        level: match b.color {
            compaction::TokenWarningColor::Dim => platform_api::ContextPressureLevel::Dim,
            compaction::TokenWarningColor::Warning => platform_api::ContextPressureLevel::Warning,
            compaction::TokenWarningColor::Error => platform_api::ContextPressureLevel::Error,
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

    // (2) Initial call. When an Opus-fallback model is configured, route the
    // primary request through the fallback-aware seam. In Task 6, `LlmError`
    // has no `FallbackTriggered` variant — fallback becomes adapter-internal.
    // The `messages_create_with_fallback` seam still passes the fallback hint to
    // `ProviderApiAdapter`, which handles the 529-triggered switch internally.
    // With NO fallback configured the plain `messages_create` seam is taken,
    // byte-identical to before — locked turn-loop fixtures are unaffected.
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
    let hint_params = hint_controller
        .as_mut()
        .and_then(|c| c.build_request_params(&history_snapshot));

    if let Some(scope) = cost_scope {
        scope.preflight().await.map_err(|error| {
            OrchestratorError::Internal(format!("cost durability preflight failed: {error}"))
        })?;
    }
    let mut output_observation = orch.capture_main_output().await?;
    let first = if let Some(params) = hint_params {
        // The controller is live: take the hint-carrying seam. `params.body` is
        // `None` when the estimated savings are under the floor — the oracle
        // still sends the beta in that case and omits only the body.
        orch.api
            .messages_create_with_context_hint(
                model,
                profile,
                system,
                history_snapshot,
                tools.clone(),
                params.body,
            )
            .await
    } else if let Some(max_tokens) = max_tokens_override {
        // REC.A1 escalated single-shot (TS `query.ts:1199-1221`): re-issue with
        // the override `max_tokens` (8k→64k). The escalation is orthogonal to the
        // Opus-fallback gate, so it takes the plain `_with_opts` seam regardless
        // of `fallback_model`. The no-override branches below are byte-identical
        // to before, so the locked turn-loop fixtures (which never arm an
        // override) are unaffected.
        orch.api
            .messages_create_with_opts(
                model,
                profile,
                system,
                history_snapshot,
                tools.clone(),
                max_tokens,
            )
            .await
    } else if orch.config.fallback_model.is_some() {
        orch.api
            .messages_create_with_fallback(
                model,
                profile,
                system,
                history_snapshot,
                tools.clone(),
                orch.config.fallback_model.as_deref(),
                orch.config.is_subscriber,
                orch.config.is_enterprise,
            )
            .await
    } else {
        orch.api
            .messages_create(model, profile, system, history_snapshot, tools.clone())
            .await
    };
    // NOTE: `ApiError::FallbackTriggered` interception is REMOVED — `LlmError`
    // has no `FallbackTriggered` variant. The model-fallback logic moves into
    // `ProviderApiAdapter` in Task 6 (the adapter handles the 529 switch
    // internally and falls back silently without emitting a separate warning).

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
            return Ok(PtlCallOutcome::Response(Box::new(resp)));
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
                    let mut retry = orch
                        .rewrite_outgoing_history(retry_raw, outgoing_history_rewriter.as_ref())
                        .await?;
                    orch.reattach_outgoing_context(
                        &mut retry,
                        deferred_tools_reminder.as_ref(),
                        date_change_reminder.as_ref(),
                        turn_reminders,
                    )
                    .await;
                    if let Some(scope) = cost_scope {
                        scope.preflight().await.map_err(|error| {
                            OrchestratorError::Internal(format!(
                                "cost durability preflight failed: {error}"
                            ))
                        })?;
                    }
                    return match orch
                        .api
                        .messages_create(model, profile, system, retry, tools.clone())
                        .await
                    {
                        Ok(resp) => {
                            if let Some(observation) = &mut output_observation {
                                observation.observe(&resp.usage);
                                let _ = observation.finish();
                            }
                            Ok(PtlCallOutcome::Response(Box::new(resp)))
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
                    turn_reminders,
                )
                .await;
                if let Some(scope) = cost_scope {
                    scope.preflight().await.map_err(|error| {
                        OrchestratorError::Internal(format!(
                            "cost durability preflight failed: {error}"
                        ))
                    })?;
                }
                match orch
                    .api
                    .messages_create(model, profile, system, retry, tools.clone())
                    .await
                {
                    Ok(resp) => {
                        if let Some(observation) = &mut output_observation {
                            observation.observe(&resp.usage);
                            let _ = observation.finish();
                        }
                        return Ok(PtlCallOutcome::Response(Box::new(resp)));
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
        let messages_before = u32::try_from(snapshot.len()).unwrap_or(u32::MAX);
        let bytes_before: u64 = snapshot.iter().map(protocol::text_byte_size).sum();
        // Capture the boundary's preTokens before the summary consumes the snapshot.
        let pre_tokens_estimate = compaction::grouping::estimate_tokens_for_range(&snapshot);
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
        // API duration = the summarizer pass only; `compact_started` (above,
        // pre-hooks) is the boundary durationMs clock. Folding hook wall-time
        // into `record_compaction_usage` would inflate /cost's API duration.
        orch.output.emit_compaction_phase("summarizing").await;
        let reactive_cost_scope = orch.compaction_cost_scope().await.map_err(|error| {
            OrchestratorError::Internal(format!(
                "reactive compaction cost preflight failed: {error}"
            ))
        })?;
        let api_started = std::time::Instant::now();
        let compact_result = {
            let mut tracking = orch.compaction_runtime.compaction_tracking.lock().await;
            compactor
                .process_reactive_tracked(
                    snapshot,
                    &mut tracking,
                    pre_compact.additional_instructions.as_deref(),
                    (token_gap > 0).then_some(token_gap),
                )
                .await
        };
        let compact_duration = api_started.elapsed();
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
                    turn_reminders,
                )
                .await;
                if let Some(scope) = cost_scope {
                    scope.preflight().await.map_err(|error| {
                        OrchestratorError::Internal(format!(
                            "cost durability preflight failed: {error}"
                        ))
                    })?;
                }
                match orch
                    .api
                    .messages_create(model, profile, system, history, tools)
                    .await
                {
                    Ok(resp) => {
                        if let Some(observation) = &mut output_observation {
                            observation.observe(&resp.usage);
                            let _ = observation.finish();
                        }
                        return Ok(PtlCallOutcome::Response(Box::new(resp)));
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

/// Port of the Opus-fallback re-issue (claude-code `query.ts:894-948`'s
/// `catch (FallbackTriggeredError)` arm). Only reachable when
/// `config.fallback_model.is_some()` (see [`call_api_with_ptl_recovery`]):
///
/// 1. (i) switch `session.model` to `fallback_model` (TS `currentModel =
///    fallbackModel`); the conversation continues on it.
/// 2. (ii) clear in-flight accumulators — STRUCTURAL no-op: history is appended
///    only after success (see [`execute_one_turn_with_recovery_tracked`]).
/// 3. (iii) surface a `warning` on the output stream (TS `createSystemMessage`,
///    same channel as [`surface_prompt_too_long`]) — not pushed to history, as
///    a `role:"system"` entry is rejected by the API.
/// 4. (iv) emit `tengu_model_fallback_triggered` via `tracing` (INLINE name, not
///    a locked const, so the event-name fixture lock holds).
/// 5. (v) re-issue ONE round-trip with `fallback_model = None` (non-Opus → 529
///    gate closed → cannot recurse; TS `continue` re-enters once).
///
/// Bounded divergences: TS also sets `mainLoopModel`, but `main_loop_model`
/// derives from immutable `config.model`; TS's `ant`-gated `stripSignatureBlocks`
/// is unported (no protected-thinking replay).
///
/// NOTE: Task 5 dead code — `FallbackTriggered` interception was removed; this is
/// business logic until then.
#[allow(dead_code)]
pub(super) async fn reissue_after_model_fallback(
    orch: &ConversationOrchestrator,
    system: Option<&str>,
    original_model: &str,
    fallback_model: String,
    tools: Vec<serde_json::Value>,
) -> Result<LlmResponse, LlmError> {
    // (i) Switch the working/session model to the fallback.
    {
        let mut s = orch.session.lock().await;
        s.model.clone_from(&fallback_model);
    }

    // (ii) Clear in-flight accumulators — structural no-op here (see doc above).

    // (iii) Surface the user-visible warning (byte-shaped on the TS intent;
    // includes both model names).
    let warning = format!("Switched to {fallback_model} due to high demand for {original_model}");
    orch.output.emit_text(&warning).await;

    // (iv) Success-path analytics — inline event name (NOT a locked const).
    tracing::info!(
        event = "tengu_model_fallback_triggered",
        original_model = %original_model,
        fallback_model = %fallback_model,
        entrypoint = "cli",
    );

    // (v) Re-issue ONE round-trip against the fallback model. Re-snapshot the
    // current history (unchanged by steps i–iv). `fallback_model = None` keeps
    // the 529 gate closed → no recursion.
    let history = {
        let s = orch.session.lock().await;
        s.model_context_history()
    };
    orch.api
        .messages_create_with_fallback(
            &fallback_model,
            None, // fallback model has no associated profile
            system,
            history,
            tools,
            None,
            orch.config.is_subscriber,
            orch.config.is_enterprise,
        )
        .await
}
