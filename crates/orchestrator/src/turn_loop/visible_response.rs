//! Process each assistant record yielded by a Mod-wrapped model step.

use super::*;
use hooks::attachment::HookPublicationGuard;
use std::sync::Arc;

#[allow(clippy::too_many_lines)]
pub(super) fn process_visible_response<'a>(
    orch: &'a ConversationOrchestrator,
    response: &'a llm_runtime::HistoryResponse,
    mut recovery: Option<&'a mut RecoveryState>,
    model: &'a str,
    requested_model: bool,
    no_physical_responses: bool,
    has_server_fallback_event: bool,
    request_history: Vec<ConversationMessage>,
    same_turn_tool_uses: Vec<ContentBlock>,
    request_history_source: super::mod_batched_step::RequestHistorySource,
) -> futures::future::BoxFuture<'a, Result<TurnStepOutcome, OrchestratorError>> {
    Box::pin(async move {
        // 2. Translate `HistoryResponse.content` -> `ContentBlock` history entry.
        let mut assistant_blocks = crate::native_computer::bind_response(
            orch,
            response,
            translate_response_blocks(&response.content),
        )
        .await?;

        // 3. Append the assistant message to the session. We need the
        //    `final_message_id` to return to the caller.
        let assistant_id = MessageId::new();
        let mut assistant_msg = ConversationMessage::Assistant {
            id: assistant_id,
            content: assistant_blocks.clone(),
            stop_reason: response.stop_reason.clone(),
        };
        let (generation_root, publication_lock) = orch
            .lifecycle_runtime
            .session_tool_hook_generation
            .current();
        let publication_fence = crate::autonomous_tool_scheduler::ToolDispatchPublicationFence::new(
            generation_root,
            publication_lock,
        );
        if !orch
            .append_streamed_assistant_to_history(
                &assistant_msg,
                Some(Arc::new(publication_fence.clone())),
                false,
            )
            .await
        {
            return Ok(TurnStepOutcome::Ended {
                final_message_id: assistant_id,
                stop_reason: "end_turn".into(),
                allow_budget_continuation: false,
                tool_requested_end: false,
            });
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
        let request_id = requested_model
            .then(|| orch.api.last_request_id())
            .flatten();
        assistant_msg = if crate::native_computer::has_native_binding(&assistant_msg)
            || crate::native_computer::has_pending_receipts(orch).await
        {
            boxed_turn_future(|| {
                crate::native_computer::persist_native_assistant(
                    orch,
                    &assistant_msg,
                    response,
                    request_id.as_deref(),
                    publication_fence.clone(),
                )
            })
            .await?
        } else {
            boxed_turn_future(|| {
                orch.persist_assistant_merged(
                    &assistant_msg,
                    Some(&response.usage),
                    request_id.as_deref(),
                    Some(publication_fence.clone()),
                )
            })
            .await
        };
        if !publication_fence.is_current() {
            return Ok(TurnStepOutcome::Ended {
                final_message_id: assistant_id,
                stop_reason: "end_turn".into(),
                allow_budget_continuation: false,
                tool_requested_end: false,
            });
        }
        if let ConversationMessage::Assistant { content, .. } = &assistant_msg {
            // `session.append` mutates the yielded row object before Native's query
            // consumer selects K and dispatches its original ToolUse blocks.
            assistant_blocks.clone_from(content);
        }
        orch.record_mod_turn_response(
            &assistant_msg,
            (requested_model && no_physical_responses).then_some(&response.usage),
            &response.model,
            response.stop_reason.as_deref(),
            response.stop_details.as_ref(),
        );

        // 4. Emit each Text block to the output stream (whole-body in M5-02;
        //    M5-04 will switch to per-delta).
        for blk in &assistant_blocks {
            if let Some(text) = blk.visible_text() {
                orch.output.emit_text(text).await;
            }
        }
        crate::server_fallback::flush_pending_notice(orch).await;

        // OTEL_LOG_ASSISTANT_RESPONSES (claude-code opt-in): default OFF, byte-no-op.
        // When enabled, log the assistant text with req-id/model/stop/usage so OTEL
        // exporters capture response bodies. The gate reads the single authoritative
        // predicate in the OTEL monitoring module (H-BIN-06), which parses the var
        // with the byte-faithful `ct` truthy semantics (1/true/yes/on, trimmed,
        // case-insensitive). Default (var unset) keeps the locked turn fixtures OFF.
        if telemetry::otel::logs::assistant_responses_enabled() {
            let text: String = assistant_blocks
                .iter()
                .filter_map(ContentBlock::visible_text)
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
        let tool_uses: Vec<(ToolUseId, String, serde_json::Value, Option<String>)> =
            assistant_blocks
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
            tracing::trace!(
                request_history_source = ?request_history_source,
                history_message_count = request_history.len(),
                prior_same_turn_tool_use_count = same_turn_tool_uses.len(),
                "dispatching tools with captured model-request history"
            );
            let mut dispatched = boxed_turn_future(|| {
                crate::native_computer::dispatch_tools(
                    orch,
                    &tool_uses,
                    assistant_id,
                    super::tool_dispatch::ToolUseDispatchFacts {
                        query_history: request_history,
                        assistant_message: assistant_msg.clone(),
                        same_turn_tool_uses,
                    },
                    publication_fence.clone(),
                )
            })
            .await?;
            if !dispatched.publish_results(orch, &publication_fence).await {
                return Ok(TurnStepOutcome::Ended {
                    final_message_id: assistant_id,
                    stop_reason: "end_turn".into(),
                    allow_budget_continuation: false,
                    tool_requested_end: false,
                });
            }
            let DeferredToolDispatch {
                results: tool_results,
                publications: _,
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

                let mut tool_result_msg = ConversationMessage::User {
                    id: MessageId::new(),
                    content: result_content,
                    is_meta: false,
                    is_compact_summary: false,
                    is_visible_in_transcript_only: false,
                };
                if crate::native_computer::is_native_result(orch, &tool_result_msg).await {
                    tool_result_msg = orch
                        .mod_session_append_row(
                            &tool_result_msg,
                            None,
                            false,
                            Some(Arc::new(publication_fence.clone())),
                        )
                        .await;
                }
                let publication_guard: Arc<dyn hooks::attachment::HookPublicationGuard> =
                    Arc::new(publication_fence.clone());
                let injected_ids = result_injected
                    .iter()
                    .map(|(message, _)| message.id())
                    .collect::<Vec<_>>();
                let append_rows = async {
                    let mut s = orch.session.lock().await;
                    s.history.push(tool_result_msg.clone());
                    for (message, source_id) in &result_injected {
                        s.history.push(message.clone());
                        s.injected_message_sources
                            .insert(message.id(), source_id.clone());
                    }
                    drop(s);
                    orch.prompt_runtime
                        .remember_guarded_prompt_message(
                            tool_result_msg.id(),
                            Arc::clone(&publication_guard),
                        )
                        .await;
                    for message_id in injected_ids {
                        orch.prompt_runtime
                            .remember_guarded_prompt_message(
                                message_id,
                                Arc::clone(&publication_guard),
                            )
                            .await;
                    }
                };
                if !publication_fence
                    .commit_if_current(Box::pin(append_rows))
                    .await
                {
                    return Ok(TurnStepOutcome::Ended {
                        final_message_id: assistant_id,
                        stop_reason: "end_turn".into(),
                        allow_budget_continuation: false,
                        tool_requested_end: false,
                    });
                }
                let parent_uuid = match &result_tool_use_id {
                    Some(id) => orch.source_tool_assistant_uuid(id).await,
                    None => None,
                };
                if crate::native_computer::is_native_result(orch, &tool_result_msg).await {
                    if let ConversationMessage::User { content, .. } = &tool_result_msg {
                        crate::native_computer::final_model_result(orch, content).await?;
                    }
                }
                if !crate::native_computer::publish_native_result(orch, &tool_result_msg).await? {
                    boxed_turn_future(|| {
                        orch.persist_guarded_message_to_jsonl_with_parent(
                            &tool_result_msg,
                            parent_uuid,
                            Arc::new(publication_fence.clone()),
                        )
                    })
                    .await;
                }
                if !crate::native_computer::is_native_result(orch, &tool_result_msg).await {
                    let accepted = orch
                        .session
                        .lock()
                        .await
                        .history
                        .iter()
                        .find(|message| message.id() == tool_result_msg.id())
                        .cloned();
                    if let Some(ConversationMessage::User { content, .. }) = accepted {
                        crate::native_computer::final_model_result(orch, &content).await?;
                    }
                }
                if !publication_fence.is_current() {
                    return Ok(TurnStepOutcome::Ended {
                        final_message_id: assistant_id,
                        stop_reason: "end_turn".into(),
                        allow_budget_continuation: false,
                        tool_requested_end: false,
                    });
                }
                if let Some(id) = &result_tool_use_id {
                    // Each queued entry commits with its own originating fence.
                    // Wrapping the whole flush would re-enter the same lease.
                    orch.flush_hook_attachments(id).await;
                }
                for (message, _) in &result_injected {
                    if !message.is_meta() {
                        boxed_turn_future(|| {
                            orch.persist_guarded_message_to_jsonl_with_parent(
                                message,
                                None,
                                Arc::new(publication_fence.clone()),
                            )
                        })
                        .await;
                    }
                }
            }
            // Defensive only: every injected message should name a result in this
            // dispatch. Preserve rather than drop one if a future synthetic source
            // uses a distinct id.
            boxed_turn_future(|| {
                append_tool_injected_messages(
                    orch,
                    remaining_injected,
                    Some(Arc::new(publication_fence.clone())),
                )
            })
            .await;
            // SKILLEXEC.3 (model scope): fold this batch's `context_modifier`s and
            // switch `session.model` if a skill declared a `model:` override. Applied
            // AFTER `injected_messages` so it mirrors the streaming twin's ordering.
            // Empty for every existing tool + non-`model:` skills → strict no-op
            // (session.model untouched → byte-identical turn-loop fixtures).
            let mut model_result = Ok(());
            publication_fence
                .commit_if_current(Box::pin(async {
                    model_result = apply_model_context_modifiers(orch, context_modifiers).await;
                }))
                .await;
            model_result?;
        }
        crate::native_computer::prepare_receipts(
            orch,
            &tool_uses
                .iter()
                .map(|(id, ..)| id.clone())
                .collect::<Vec<_>>(),
        )
        .await?;
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
            let batch_outcome = if tool_requested_end_turn {
                run_post_tool_batch_hooks_after_turn_end(
                    orch,
                    super::PostToolBatchDispatch {
                        tool_calls: post_tool_batch_calls,
                        publication_guard: Some(Arc::new(publication_fence.clone())),
                    },
                )
                .await
            } else {
                run_post_tool_batch_hooks(
                    orch,
                    super::PostToolBatchDispatch {
                        tool_calls: post_tool_batch_calls,
                        publication_guard: Some(Arc::new(publication_fence.clone())),
                    },
                )
                .await
            };
            let batch_prevent = batch_outcome.prevent_continuation;
            append_tool_injected_messages(
                orch,
                batch_outcome.injected_messages,
                batch_outcome.publication_guard,
            )
            .await;
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
        if !publication_fence.is_current() {
            return Ok(TurnStepOutcome::Ended {
                final_message_id: assistant_id,
                stop_reason: "end_turn".into(),
                allow_budget_continuation: false,
                tool_requested_end: false,
            });
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
            && take_lone_wakeup_turn_end(
                orch,
                tool_uses.iter().map(|(_, name, _, _)| name.as_str()),
            )
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
                Some("refusal")
                    if !has_server_fallback_event
                        && orch.maybe_swap_to_refusal_fallback().await =>
                {
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
                        surface_terminal_api_error(orch, other, response.stop_details.as_ref())
                            .await;
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
        Ok(outcome)
    })
}
