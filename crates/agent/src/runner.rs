//! Subagent state-machine loop.
//!
//! [`run_subagent`] is the future that
//! [`crate::pool::StateMachinePool::allocate`] hands to the runtime.
//!
//! The multi-turn loop requires a configured [`SubagentContext::api_client`].
//! A missing client fails before startup persistence or a model request.
//!
//! The loop calls the model, dispatches tools, and repeats until the model
//! stops or the turn limit is reached. A termination event emits `Killed`.
//! Persistent agents park between turn sets and resume on the next input.

use crate::context::SubagentContext;
use crate::mod_prompt_attachment::ChildPromptAttachments;
use crate::mod_turn_complete::{child_turn_start_text, fire_child_turn_start, ChildTurnComplete};
use futures::StreamExt;
use hooks::ExactHookText;
use lingxi_core::host::{CancellationToken, WorkflowQueryWatchdog};
use lingxi_core::types::{AgentId, ContentBlock, ConversationMessage, MessageId};
use llm_runtime::{HistoryEvent, LlmError};
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::time::Duration;
use tokio::sync::mpsc;

#[path = "runner_live_tools.rs"]
mod live_tools;

/// Events emitted by [`run_subagent`] back to the host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SubagentEvent {
    /// Periodic progress beacon while the agent is still running.
    Progress {
        /// Agent emitting the progress event.
        agent_id: AgentId,
        /// Number of tool calls processed so far.
        tool_use_count: u32,
        /// Cumulative token count consumed so far.
        token_count: u64,
    },
    /// Agent finished normally with `result`.
    Completed {
        /// Agent that completed.
        agent_id: AgentId,
        /// Final result payload (free-form JSON).
        result: serde_json::Value,
        /// Final response usage, translated by the spawner into the result's
        /// [`lingxi_core::host::SubagentUsage`] and token total.
        usage: llm_runtime::ExecutionUsage,
        /// Number of tool-use blocks executed across the run (claude
        /// `totalToolUseCount`).
        total_tool_use_count: u64,
        /// Wall-clock duration of the run in milliseconds (claude
        /// `totalDurationMs`).
        total_duration_ms: u64,
        /// Number of assistant messages produced across the run (claude
        /// `agentMessages.length`, fed into `tengu_agent_tool_completed`'s
        /// `assistant_message_count`).
        assistant_message_count: u64,
        /// The FINAL assistant turn's provider request id (claude
        /// `lastAssistantMessage.requestId`) — used to gate
        /// `tengu_cache_eviction_hint`.
        last_request_id: Option<String>,
        /// Cross-turn summed usage. Distinct from [`Self::Completed::usage`].
        #[serde(default)]
        cumulative_usage: llm_runtime::ExecutionUsage,
        /// `false` only on the CC 2.1.207 `api_error_partial` SALVAGE path
        /// (Finding [9]): a mid-stream provider error whose already-produced
        /// text is recovered as a `Completed` result instead of discarding
        /// it. On that path `usage`/`cumulative_usage` above are the STALE
        /// values from the last turn that completed successfully BEFORE the
        /// error — the failing turn's own (real, provider-billed) tokens are
        /// not included, because they were never captured. `true` on every
        /// other completed path with complete usage totals.
        #[serde(default = "usage_complete_default")]
        usage_complete: bool,
        /// Trusted final reporting disposition and persisted whole report.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        handback: Option<lingxi_core::host::handback::HandbackState>,
    },
    /// Agent terminated due to an error.
    Failed {
        /// Agent that failed.
        agent_id: AgentId,
        /// Human-readable error message.
        error: String,
        /// Cross-turn summed usage from every SUCCESSFUL turn before the one
        /// that failed (same accumulation [`Self::Completed::cumulative_usage`]
        /// carries). Finding [9]/[11]: a `Failed` termination (provider
        /// error, idle-timeout watchdog, max-turns/structured-output
        /// exhaustion) still reflects real, already-billed provider spend
        /// from any turn that succeeded before it — this lets a caller price
        /// that spend instead of settling it at $0. `ExecutionUsage::default()` on
        /// every startup failure before a real round-trip.
        #[serde(default)]
        cumulative_usage: llm_runtime::ExecutionUsage,
    },
    /// Agent was cancelled by the host.
    Killed {
        /// Agent that was killed.
        agent_id: AgentId,
    },
    /// A raw message produced by the agent (assistant or tool result).
    Message {
        /// Agent that produced the message.
        agent_id: AgentId,
        /// Free-form message payload (full schema lands in Plan 09+).
        message: serde_json::Value,
        /// Shared session-agent stream index. Hidden lifecycle rows have no
        /// public transcript index.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message_index: Option<u64>,
    },
    /// Host-only settled transcript snapshot used by the local-agent registry
    /// for Native `pae` result extraction. This is sent after the runner has
    /// applied tombstones, fallback stitching, and retained-row hooks; it is
    /// not a model/SDK message and must not be projected as one.
    #[serde(skip)]
    TranscriptSnapshot {
        /// Agent whose settled history is being replaced.
        agent_id: AgentId,
        /// The exact current runner history after terminal settlement.
        messages: Vec<lingxi_core::types::ConversationMessage>,
    },
    /// Host-only transcript deletion emitted by a server-fallback decision.
    /// `display_only` is preserved, but does not prevent UUID-based deletion.
    #[serde(skip)]
    ServerFallbackTombstone {
        /// Agent whose row was removed.
        agent_id: AgentId,
        /// Complete row facts available to the Agent host.
        message: lingxi_core::host::ServerFallbackTombstoneMessage,
        /// Native presentation flag.
        display_only: bool,
    },
    /// Host-only synthetic API-error row. This is not a provider message or
    /// `LlmError`; the nested query message is used only for Agent history and
    /// observer presentation.
    #[serde(skip)]
    ServerFallbackApiErrorRow {
        /// Agent that emitted the row.
        agent_id: AgentId,
        /// Native outer row envelope.
        row: lingxi_core::host::ServerFallbackApiErrorRow,
        /// Shared session-agent stream index assigned to the visible row.
        message_index: u64,
    },
}

/// `#[serde(default = ...)]` for [`SubagentEvent::Completed::usage_complete`]
/// / [`lingxi_core::host::subagent_spawn::SubagentResult::Completed::usage_complete`]
/// — an older wire payload with no such field must decode as `true` (a
/// normal complete usage rollup), not `bool::default()`'s `false`.
fn usage_complete_default() -> bool {
    true
}

fn completed_result_text(result: &serde_json::Value) -> Option<String> {
    if let Some(text) = result.get("text").and_then(serde_json::Value::as_str) {
        if !text.is_empty() {
            return Some(text.to_string());
        }
    }
    let content = result
        .get("content")
        .and_then(serde_json::Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|block| {
                    (block.get("type").and_then(serde_json::Value::as_str) == Some("text"))
                        .then(|| block.get("text").and_then(serde_json::Value::as_str))
                        .flatten()
                        .map(str::to_string)
                })
                .collect::<Vec<String>>()
        })?;
    if content.is_empty() {
        None
    } else {
        Some(content.join("\n"))
    }
}

/// Milliseconds elapsed since `start`, saturated into a `u64` (claude
/// `totalDurationMs`).
fn elapsed_ms(start: std::time::Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

async fn publish_local_agent_idle_fact(ctx: &SubagentContext, value: Option<bool>) {
    let (Some(registry), Some(value)) = (&ctx.task_registry, value) else {
        return;
    };
    if let Err(error) = registry
        .update_agent_list_local_fact(
            ctx.agent_id,
            lingxi_core::host::task_registry::AgentListLocalFactUpdate::IsIdle(value),
        )
        .await
    {
        tracing::debug!(
            agent_id = %ctx.agent_id,
            %error,
            "could not update the local-agent idle fact"
        );
    }
}

fn observe_local_tool_result(
    lifecycle: &std::sync::Mutex<lingxi_core::host::tool_use_lifecycle::ToolUseLifecycleTracker>,
    tool_use_id: &lingxi_core::types::ToolUseId,
) -> Option<bool> {
    lifecycle
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .observe_tool_result(tool_use_id)
}

fn invocation_context_for_tool(
    ctx: &SubagentContext,
    name: &str,
    tool_use_id: &lingxi_core::types::ToolUseId,
    assistant_message_id: MessageId,
    current_history: &[ConversationMessage],
    system_prompt: Option<&str>,
    model: &str,
    model_profile: Option<String>,
    tool_context_state: Option<lingxi_core::host::tool_invoker::ToolInvocationContextState>,
) -> lingxi_core::host::tool_invoker::SubagentInvocationContext {
    let agent_id = ctx.agent_id;
    lingxi_core::host::tool_invoker::SubagentInvocationContext {
        input_projection: None,
        cancellation_token: CancellationToken::new(),
        permission_pause_observer: ctx.task_registry.clone().map(|registry| {
            lingxi_core::host::permission_gate::PermissionPauseObserver::new(move |ms| {
                registry.add_permission_paused_ms(agent_id, ms);
            })
        }),
        parent_agent_id: Some(agent_id),
        origin_session_id: ctx.origin_session_id,
        instruction_context: Some(ctx.instruction_context.clone()),
        fork_context: (name == "Agent" || name == lingxi_core::host::handback::HANDBACK_TOOL_NAME)
            .then(|| lingxi_core::host::tool_invoker::SubagentForkContext {
                messages: current_history.to_vec(),
                system_prompt: system_prompt.map(str::to_owned),
            }),
        tool_execution_policy: if lingxi_core::host::is_fusion_panel_type(
            &ctx.agent_definition.agent_type,
        ) {
            lingxi_core::host::tool_invoker::ToolExecutionPolicy::FusionPanel
        } else {
            lingxi_core::host::tool_invoker::ToolExecutionPolicy::Ordinary
        },
        agent_name: ctx.agent_name.clone(),
        team_name: ctx.team_name.clone(),
        is_async: ctx.is_async,
        is_non_interactive_session: ctx.is_async
            || lingxi_core::host::session_flags::effective_non_interactive_session(),
        can_show_permission_prompts: ctx.can_show_permission_prompts,
        cwd: ctx.cwd.clone(),
        tool_use_id: Some(tool_use_id.as_str().to_string()),
        assistant_message_id: Some(assistant_message_id),
        depth: ctx.depth,
        observer: ctx
            .observer
            .as_ref()
            .filter(|observer| {
                observer.observe_subagents
                    && ctx.depth < crate::observer::DEFAULT_OBSERVER_FANOUT_DEPTH
            })
            .cloned(),
        parent_model: Some(model.to_owned()),
        parent_model_profile: model_profile,
        agent_spawn_provenance: ctx.agent_spawn_provenance.clone(),
        tool_context_state,
        current_history: current_history.to_vec(),
        assistant_message: None,
        same_turn_tool_uses: Vec::new(),
        mode_override: ctx.permission_mode_override.clone(),
        request_source: None,
        frozen_command_denies: ctx.frozen_command_denies.clone(),
    }
}

struct AgentStreamAttempt {
    response: Option<
        Result<
            (
                llm_runtime::HistoryResponse,
                futures::stream::BoxStream<'static, Result<HistoryEvent, LlmError>>,
            ),
            (Vec<llm_runtime::ContentBlock>, LlmError),
        >,
    >,
    assistant_rows: Vec<crate::transcript::StagedAssistantRow>,
    ordered_rows: Vec<ConversationMessage>,
    /// Native `je`: completed user/tool-result and attachment rows only.
    /// This is intentionally distinct from the full executor/result queue.
    je_rows: Vec<ConversationMessage>,
    early_tool_result_ids: std::collections::HashSet<lingxi_core::types::ToolUseId>,
    tool_calls: Vec<(
        live_tools::LiveAgentToolCall,
        Option<
            Result<
                lingxi_core::host::tool_invoker::ToolInvocationResult,
                lingxi_core::host::tool_invoker::ToolInvokerError,
            >,
        >,
    )>,
    request_messages: Vec<ConversationMessage>,
    declined_fallback: Option<String>,
    declined_api_error_row: Option<lingxi_core::host::ServerFallbackApiErrorRow>,
    partial_response: Option<llm_runtime::HistoryResponse>,
}

impl AgentStreamAttempt {
    fn failed(error: LlmError, request_messages: Vec<ConversationMessage>) -> Self {
        Self {
            response: Some(Err((Vec::new(), error))),
            assistant_rows: Vec::new(),
            ordered_rows: Vec::new(),
            je_rows: Vec::new(),
            early_tool_result_ids: std::collections::HashSet::new(),
            tool_calls: Vec::new(),
            request_messages,
            declined_fallback: None,
            declined_api_error_row: None,
            partial_response: None,
        }
    }
}

fn live_agent_tool_dispatch(ctx: &SubagentContext) -> Option<live_tools::LiveAgentToolDispatch> {
    let invoker = ctx.tool_invoker.clone()?;
    let handback = ctx.handback.clone();
    Some(std::sync::Arc::new(move |call| {
        let invoker = invoker.clone();
        let handback = handback.clone();
        Box::pin(async move {
            if call.name == lingxi_core::host::handback::HANDBACK_TOOL_NAME
                && handback.as_ref().is_some_and(|runtime| runtime.eligible)
            {
                if let Some(runtime) = handback {
                    let tool: std::sync::Arc<dyn tool_api::Tool> =
                        std::sync::Arc::new(crate::handback::SubagentHandbackTool(runtime));
                    invoker
                        .invoke_supplied_detailed(
                            &call.name,
                            call.input,
                            call.context,
                            std::sync::Arc::new(tool_api::tool_invoker_impl::SuppliedTool(tool)),
                        )
                        .await
                } else {
                    Err(lingxi_core::host::tool_invoker::ToolInvokerError::NotFound(
                        call.name,
                    ))
                }
            } else {
                invoker
                    .invoke_detailed(&call.name, call.input, call.context)
                    .await
            }
        }) as futures::future::BoxFuture<'static, _>
    }))
}

#[allow(clippy::too_many_arguments)]
async fn stage_assistant_row(
    // api_block_index identifies the append row. A complete-only response
    // uses Native's row index 0 even though each ToolCall keeps its own source
    // content ordinal for dispatch and fallback bookkeeping.
    api_block_index: u32,
    blocks: &[llm_runtime::ContentBlock],
    stop_reason: Option<&str>,
    source_tool_use_block_indices: &[u32],
    ctx: &SubagentContext,
    transcript: Option<&crate::transcript::AgentTranscriptWriter>,
    prior_history: &[ConversationMessage],
    system_prompt: Option<&str>,
    response_model: &str,
    logical_model: &str,
    logical_model_profile: Option<String>,
    tool_context_state: Option<lingxi_core::host::tool_invoker::ToolInvocationContextState>,
    allowed_tools: &[String],
    force_structured_tool: Option<&str>,
    defer_local_work: bool,
    retryable_body: &std::sync::atomic::AtomicBool,
    live_output_started: &std::sync::atomic::AtomicBool,
    tool_effects_started: &std::sync::atomic::AtomicBool,
    emitted_assistant_row_ids: &mut std::collections::HashSet<MessageId>,
    assistant_row_models: &mut std::collections::HashMap<MessageId, String>,
    executor: &mut live_tools::LiveAgentToolExecutor,
    dispatch: Option<&live_tools::LiveAgentToolDispatch>,
    prior_siblings: &mut Vec<(u32, lingxi_core::types::ContentBlock)>,
    rows: &mut Vec<crate::transcript::StagedAssistantRow>,
    ordered_rows: &mut Vec<ConversationMessage>,
    lifecycle: &std::sync::Mutex<lingxi_core::host::tool_use_lifecycle::ToolUseLifecycleTracker>,
    out_tx: &mpsc::Sender<SubagentEvent>,
) {
    let content = translate_response_blocks(blocks);
    if content.is_empty() {
        return;
    }
    let raw = ConversationMessage::Assistant {
        per_turn_effort: None,
        id: MessageId::new(),
        content,
        stop_reason: stop_reason.map(str::to_owned),
    };
    let staged = if defer_local_work {
        crate::transcript::StagedAssistantRow::from_source(api_block_index, raw.clone())
    } else if let Some(writer) = transcript {
        match writer
            .accept_assistant_block(api_block_index, raw.clone(), prior_history)
            .await
        {
            Ok(staged) => staged,
            Err(error) => {
                tracing::warn!(%error, "could not accept assistant row");
                crate::transcript::StagedAssistantRow::from_source(api_block_index, raw)
            }
        }
    } else {
        crate::transcript::StagedAssistantRow::from_source(api_block_index, raw.clone())
    };
    debug_assert_eq!(
        staged.source_tool_uses.len(),
        source_tool_use_block_indices.len(),
        "every source ToolCall needs its provider content index"
    );
    assistant_row_models.insert(staged.accepted.id(), response_model.to_owned());

    // Native's through wrapper yields the same row after session.append has
    // projected it. The append merger keeps the original ToolUse block even
    // when a hook returns only accepted Text, so query history, emitted events,
    // and eventual JSONL persistence all observe that accepted row.
    if !defer_local_work && emitted_assistant_row_ids.insert(staged.accepted.id()) {
        live_output_started.store(true, std::sync::atomic::Ordering::Relaxed);
        emit_message(out_tx, ctx.agent_id, &staged.accepted).await;
    }
    ordered_rows.push(staged.accepted.clone());
    rows.push(staged.clone());

    if staged.source_tool_uses.is_empty() {
        return;
    }

    let assistant_message_id = staged.accepted.id();
    for (source_tool_use, index) in staged
        .source_tool_uses
        .iter()
        .cloned()
        .zip(source_tool_use_block_indices.iter().copied())
    {
        let lingxi_core::types::ContentBlock::ToolUse {
            id,
            name,
            input,
            provider_id,
            ..
        } = &source_tool_use
        else {
            continue;
        };
        let id = id.clone();
        let name = name.clone();
        let input = input.clone();
        let provider_id = provider_id.clone();

        let idle_update = lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .observe_assistant_row([(id.clone(), name == "Agent")]);
        publish_local_agent_idle_fact(ctx, idle_update).await;
        executor.track_tool_use(index, id.clone());

        if force_structured_tool == Some(name.as_str())
            || (!allowed_tools.is_empty() && !allowed_tools.iter().any(|allowed| allowed == &name))
        {
            prior_siblings.push((index, source_tool_use));
            continue;
        }
        let Some(dispatch) = dispatch else {
            prior_siblings.push((index, source_tool_use));
            continue;
        };

        let mut call_context = invocation_context_for_tool(
            ctx,
            &name,
            &id,
            assistant_message_id,
            prior_history,
            system_prompt,
            logical_model,
            logical_model_profile.clone(),
            tool_context_state.clone(),
        );
        call_context.input_projection = source_tool_use
            .projected_tool_input()
            .expect("source tool input projection is validated before admission");
        // Main's W1 carrier distinguishes the query's pre-call history, the
        // current accepted row and earlier source ToolUse blocks.
        call_context.assistant_message = Some(staged.accepted.clone());
        call_context.same_turn_tool_uses = prior_siblings
            .iter()
            .map(|(_, sibling)| sibling.clone())
            .collect();

        let concurrency_safe = ctx
            .tool_invoker
            .as_ref()
            .and_then(|invoker| invoker.tool_is_concurrency_safe(&name, &input))
            .unwrap_or(false);
        let call = live_tools::LiveAgentToolCall {
            block_index: index,
            id,
            name,
            input,
            provider_id,
            concurrency_safe,
            context: call_context,
        };
        if defer_local_work {
            executor.add_call_deferred(call, true);
        } else {
            tool_effects_started.store(true, std::sync::atomic::Ordering::Relaxed);
            retryable_body.store(false, std::sync::atomic::Ordering::Relaxed);
            executor.add_call(call, true, dispatch);
        }
        prior_siblings.push((index, source_tool_use));
    }
}

fn assistant_content(message: &ConversationMessage) -> &[lingxi_core::types::ContentBlock] {
    match message {
        ConversationMessage::Assistant { content, .. } => content,
        _ => &[],
    }
}

async fn emit_unpublished_assistant_rows(
    rows: &[crate::transcript::StagedAssistantRow],
    emitted_ids: &mut std::collections::HashSet<MessageId>,
    out_tx: &mpsc::Sender<SubagentEvent>,
    agent_id: AgentId,
    live_output_started: &std::sync::atomic::AtomicBool,
) {
    for row in rows {
        if emitted_ids.insert(row.accepted.id()) {
            live_output_started.store(true, std::sync::atomic::Ordering::Relaxed);
            emit_message(out_tx, agent_id, &row.accepted).await;
        }
    }
}

async fn accept_deferred_assistant_rows(
    rows: &mut [crate::transcript::StagedAssistantRow],
    transcript: Option<&crate::transcript::AgentTranscriptWriter>,
    prior_history: &[ConversationMessage],
    stop_reason: Option<&str>,
) {
    for row in rows {
        if let Some(writer) = transcript {
            match writer
                .accept_assistant_block(row.api_block_index, row.accepted.clone(), prior_history)
                .await
            {
                Ok(accepted) => {
                    row.accepted = accepted.accepted;
                    row.source_tool_uses = accepted.source_tool_uses;
                }
                Err(error) => tracing::warn!(%error, "could not accept qualified assistant block"),
            }
        }
        if let ConversationMessage::Assistant {
            stop_reason: accepted_stop_reason,
            ..
        } = &mut row.accepted
        {
            *accepted_stop_reason = stop_reason.map(str::to_owned);
        }
    }
}

async fn persist_salvaged_assistant_rows(
    rows: &mut [crate::transcript::StagedAssistantRow],
    transcript: Option<&crate::transcript::AgentTranscriptWriter>,
    history: &mut [ConversationMessage],
) {
    for row in rows {
        if let Some(writer) = transcript {
            if let Err(error) = writer.persist_assistant_block(row, Some("api_error")).await {
                tracing::warn!(%error, "could not persist salvaged assistant block");
            }
        } else {
            if let ConversationMessage::Assistant { stop_reason, .. } = &mut row.accepted {
                *stop_reason = Some("api_error".to_string());
            }
        }
        if let Some(existing) = history
            .iter_mut()
            .find(|message| message.id() == row.accepted.id())
        {
            *existing = row.accepted.clone();
        }
    }
}

fn remove_discarded_assistant_rows(
    assistant_rows: &mut Vec<crate::transcript::StagedAssistantRow>,
    ordered_rows: &mut Vec<ConversationMessage>,
    discarded_blocks: &[usize],
    assistant_row_models: &mut std::collections::HashMap<MessageId, String>,
    target_model: Option<&str>,
) -> Vec<(crate::transcript::StagedAssistantRow, Option<String>)> {
    let removed_message_ids = assistant_rows
        .iter()
        .filter(|row| {
            discarded_blocks.contains(&(row.api_block_index as usize))
                || target_model.is_some_and(|model| {
                    assistant_row_models
                        .get(&row.accepted.id())
                        .is_some_and(|row_model| row_model == model)
                })
        })
        .map(|row| row.accepted.id())
        .collect::<std::collections::HashSet<_>>();
    let removed_rows = assistant_rows
        .iter()
        .filter(|row| removed_message_ids.contains(&row.accepted.id()))
        .map(|row| {
            (
                row.clone(),
                assistant_row_models.get(&row.accepted.id()).cloned(),
            )
        })
        .collect::<Vec<_>>();
    assistant_rows.retain(|row| !removed_message_ids.contains(&row.accepted.id()));
    assistant_row_models.retain(|row_id, _| !removed_message_ids.contains(row_id));
    ordered_rows.retain_mut(|message| !removed_message_ids.contains(&message.id()));
    removed_rows
}

fn clear_stream_je_rows(
    je_rows: &mut Vec<ConversationMessage>,
    ordered_rows: &mut Vec<ConversationMessage>,
) -> Vec<ConversationMessage> {
    let removed_ids = je_rows
        .iter()
        .map(ConversationMessage::id)
        .collect::<std::collections::HashSet<_>>();
    ordered_rows.retain(|message| !removed_ids.contains(&message.id()));
    std::mem::take(je_rows)
}

fn fallback_tombstone_for_message(
    message: &ConversationMessage,
    model: Option<&str>,
) -> lingxi_core::host::ServerFallbackTombstoneMessage {
    let (message_type, content, stop_reason) = match message {
        ConversationMessage::Assistant {
            content,
            stop_reason,
            ..
        } => ("assistant", content.clone(), stop_reason.clone()),
        ConversationMessage::User { content, .. } => ("user", content.clone(), None),
        ConversationMessage::System { content, .. } => (
            "system",
            vec![lingxi_core::types::ContentBlock::Text {
                text: content.clone(),
                citations: None,
            }],
            None,
        ),
    };
    lingxi_core::host::ServerFallbackTombstoneMessage {
        uuid: message.id(),
        message_type: message_type.into(),
        timestamp: chrono::Utc::now()
            .format("%Y-%m-%dT%H:%M:%S%.3fZ")
            .to_string(),
        request_id: None,
        request_ref: None,
        provider_message_id: None,
        model: model.map(str::to_owned),
        stop_reason,
        stop_details: None,
        usage: None,
        content,
        is_api_error_message: None,
        supersedes_uuids: None,
    }
}

async fn emit_fallback_tombstone(
    out_tx: &mpsc::Sender<SubagentEvent>,
    transcript: Option<&crate::transcript::AgentTranscriptWriter>,
    agent_id: AgentId,
    message: &ConversationMessage,
    model: Option<&str>,
    display_only: bool,
) {
    let tombstone = fallback_tombstone_for_message(message, model);
    if let Some(writer) = transcript {
        if let Err(error) = writer
            .remove_server_fallback_row(&tombstone, display_only)
            .await
        {
            tracing::warn!(%error, row_id = %tombstone.uuid, "could not remove server-fallback transcript row");
        }
    }
    let _ = out_tx
        .send(SubagentEvent::ServerFallbackTombstone {
            agent_id,
            message: tombstone,
            display_only,
        })
        .await;
}

async fn append_live_stream_rows(
    ordered_rows: Vec<ConversationMessage>,
    je_rows: &[ConversationMessage],
    assistant_rows: &mut [crate::transcript::StagedAssistantRow],
    transcript: Option<&crate::transcript::AgentTranscriptWriter>,
    history: &mut Vec<ConversationMessage>,
    transcript_written: &mut usize,
    stop_reason: Option<&str>,
) {
    // `K` and `je` are separate Native query arrays. The staged assistant
    // UUIDs identify K; the explicit JE vector contains only projected user
    // rows (including user rows carrying attachment blocks). Other executor
    // context rows must not leak into the next provider history merely because
    // their message enum is not Assistant.
    let assistant_row_ids = assistant_rows
        .iter()
        .map(|row| row.accepted.id())
        .collect::<std::collections::HashSet<_>>();
    let je_row_ids = je_rows
        .iter()
        .map(ConversationMessage::id)
        .collect::<std::collections::HashSet<_>>();
    let mut append_history = history.clone();
    let mut assistant_history = Vec::new();
    let mut je_history = Vec::new();
    for message in ordered_rows {
        let row_id = message.id();
        if assistant_row_ids.contains(&row_id) {
            match message {
                message @ ConversationMessage::Assistant { .. } => {
                    let history_message = if let Some(row) = assistant_rows
                        .iter_mut()
                        .find(|row| row.accepted.id() == row_id)
                    {
                        if let Some(writer) = transcript {
                            if let Err(error) =
                                writer.persist_assistant_block(row, stop_reason).await
                            {
                                tracing::warn!(%error, "could not persist completed assistant block");
                            }
                        } else if let Some(stop_reason) = stop_reason {
                            if let ConversationMessage::Assistant {
                                stop_reason: row_stop_reason,
                                ..
                            } = &mut row.accepted
                            {
                                *row_stop_reason = Some(stop_reason.to_string());
                            }
                        }
                        row.accepted.clone()
                    } else {
                        message
                    };
                    append_history.push(history_message.clone());
                    assistant_history.push(history_message);
                }
                _ => {}
            }
        } else {
            let mut stored = message.clone();
            if let Some(writer) = transcript {
                if let Err(error) = writer.record_retained(&mut stored, &append_history).await {
                    tracing::warn!(%error, "could not persist streamed executor message row");
                }
            }
            if je_row_ids.contains(&row_id) {
                append_history.push(message.clone());
                je_history.push(message);
            }
        }
    }
    history.extend(assistant_history);
    history.extend(je_history);
    *transcript_written = history.len();
}

async fn append_unmatched_stream_tool_results(
    assistant_rows: &[crate::transcript::StagedAssistantRow],
    error: &str,
    already_result_ids: &mut std::collections::HashSet<lingxi_core::types::ToolUseId>,
    ordered_rows: &mut Vec<ConversationMessage>,
    je_rows: &mut Vec<ConversationMessage>,
    lifecycle: &std::sync::Mutex<lingxi_core::host::tool_use_lifecycle::ToolUseLifecycleTracker>,
    ctx: &SubagentContext,
    out_tx: &mpsc::Sender<SubagentEvent>,
) {
    for row in assistant_rows {
        for block in &row.source_tool_uses {
            let lingxi_core::types::ContentBlock::ToolUse {
                id, provider_id, ..
            } = block
            else {
                continue;
            };
            if !already_result_ids.insert(id.clone()) {
                continue;
            }
            let result = lingxi_core::types::ContentBlock::ToolResult {
                content_projection: None,
                tool_use_id: id.clone(),
                content: format!(
                    "The turn ended on an error, so this tool call was cancelled. If it had already started, some of its effects may have happened. Error: {error}"
                ),
                is_error: Some(true),
                provider_tool_use_id: provider_id.clone(),
                content_blocks: None,
            };
            let message = ConversationMessage::User {
                api_message_override: None,
                id: MessageId::new(),
                content: vec![result],
                is_meta: false,
                is_compact_summary: false,
                is_visible_in_transcript_only: false,
            };
            emit_message(out_tx, ctx.agent_id, &message).await;
            ordered_rows.push(message.clone());
            je_rows.push(message);
            let idle_update = observe_local_tool_result(lifecycle, id);
            publish_local_agent_idle_fact(ctx, idle_update).await;
        }
    }
}

fn live_tool_result_block(
    name: &str,
    id: &lingxi_core::types::ToolUseId,
    provider_id: Option<&str>,
    result: &Result<
        lingxi_core::host::tool_invoker::ToolInvocationResult,
        lingxi_core::host::tool_invoker::ToolInvokerError,
    >,
) -> Option<ContentBlock> {
    match result {
        Ok(invocation) => Some(successful_live_tool_result_block(
            name,
            id,
            provider_id,
            invocation,
        )),
        Err(lingxi_core::host::tool_invoker::ToolInvokerError::Abort(_)) => None,
        Err(error) => Some(ContentBlock::ToolResult {
            content_projection: None,
            tool_use_id: id.clone(),
            content: error.model_tool_result_content(),
            is_error: Some(true),
            provider_tool_use_id: provider_id.map(str::to_owned),
            content_blocks: None,
        }),
    }
}

#[allow(clippy::too_many_arguments)]
async fn publish_live_tool_completion(
    call: Option<live_tools::LiveAgentToolCall>,
    result: &Result<
        lingxi_core::host::tool_invoker::ToolInvocationResult,
        lingxi_core::host::tool_invoker::ToolInvokerError,
    >,
    ctx: &SubagentContext,
    out_tx: &mpsc::Sender<SubagentEvent>,
    ordered_rows: &mut Vec<ConversationMessage>,
    je_rows: &mut Vec<ConversationMessage>,
    early_tool_result_ids: &mut std::collections::HashSet<lingxi_core::types::ToolUseId>,
    lifecycle: &std::sync::Mutex<lingxi_core::host::tool_use_lifecycle::ToolUseLifecycleTracker>,
) {
    let Some(call) = call else {
        return;
    };
    let live_tools::LiveAgentToolCall {
        id,
        name,
        provider_id,
        ..
    } = call;
    let Some(block) = live_tool_result_block(&name, &id, provider_id.as_deref(), result) else {
        return;
    };
    let tool_use_id = id;
    early_tool_result_ids.insert(tool_use_id.clone());
    let row = ConversationMessage::User {
        api_message_override: None,
        id: MessageId::new(),
        content: vec![block],
        is_meta: false,
        is_compact_summary: false,
        is_visible_in_transcript_only: false,
    };
    emit_message(out_tx, ctx.agent_id, &row).await;
    ordered_rows.push(row.clone());
    je_rows.push(row);
    if let Ok(invocation) = result {
        // Native first yields and journals every returned row, then `xr`
        // projects only user rows (including rows whose content contains
        // attachments) into `je` and the next model-history array.
        for message in &invocation.new_messages {
            emit_message(out_tx, ctx.agent_id, message).await;
            ordered_rows.push(message.clone());
            if matches!(message, ConversationMessage::User { .. }) {
                je_rows.push(message.clone());
            }
        }
    }
    let idle_update = observe_local_tool_result(lifecycle, &tool_use_id);
    publish_local_agent_idle_fact(ctx, idle_update).await;
}

fn successful_live_tool_result_block(
    name: &str,
    id: &lingxi_core::types::ToolUseId,
    provider_id: Option<&str>,
    invocation: &lingxi_core::host::tool_invoker::ToolInvocationResult,
) -> ContentBlock {
    let value = &invocation.data;
    let supplied_content = invocation.model_content.clone();
    let mapped_text = supplied_content.clone().unwrap_or_else(|| {
        value
            .as_str()
            .map_or_else(|| value.to_string(), str::to_owned)
    });
    let content_blocks =
        tool_api::tool_result_media::media_content_blocks_for_tool(name, value, &mapped_text);
    let content = supplied_content.unwrap_or_else(|| {
        content_blocks
            .as_ref()
            .and_then(|_| tool_api::tool_result_media::ephemeral_summary(value))
            .unwrap_or(mapped_text)
    });
    let wire_content = content_blocks
        .as_ref()
        .map(|blocks| serde_json::Value::Array(blocks.clone()))
        .unwrap_or_else(|| serde_json::Value::String(content.clone()));
    let content_projection = invocation
        .data_projection
        .as_ref()
        .filter(|p| p.value == wire_content)
        .or_else(|| {
            invocation
                .model_content_projection
                .as_ref()
                .filter(|p| p.value == wire_content)
        })
        .cloned();
    ContentBlock::ToolResult {
        content_projection,
        tool_use_id: id.clone(),
        content,
        is_error: Some(invocation.is_error),
        provider_tool_use_id: provider_id.map(str::to_owned),
        content_blocks,
    }
}

#[allow(clippy::too_many_arguments)]
async fn accumulate_agent_stream_live(
    mut stream: futures::stream::BoxStream<'static, Result<HistoryEvent, LlmError>>,
    ctx: &SubagentContext,
    transcript: Option<&crate::transcript::AgentTranscriptWriter>,
    request_messages: Vec<ConversationMessage>,
    system_prompt: Option<String>,
    initial_response_model: String,
    logical_tool_model: String,
    logical_tool_model_profile: Option<String>,
    tool_context_state: Option<lingxi_core::host::tool_invoker::ToolInvocationContextState>,
    allowed_tools: &[String],
    force_structured_tool: Option<&str>,
    defer_local_work: bool,
    retryable_body: &std::sync::atomic::AtomicBool,
    live_output_started: &std::sync::atomic::AtomicBool,
    tool_effects_started: &std::sync::atomic::AtomicBool,
    lifecycle: &std::sync::Mutex<lingxi_core::host::tool_use_lifecycle::ToolUseLifecycleTracker>,
    out_tx: &mpsc::Sender<SubagentEvent>,
) -> AgentStreamAttempt {
    use llm_runtime::stream_accumulator::{ResponseAccumulator, ResponseAccumulatorUpdate};

    let mut accumulator = ResponseAccumulator::default();
    let mut executor = live_tools::LiveAgentToolExecutor::new(
        lingxi_core::host::tool_use_lifecycle::max_tool_use_concurrency(),
    );
    let dispatch = live_agent_tool_dispatch(ctx);
    let mut assistant_rows = Vec::new();
    let mut ordered_rows = Vec::new();
    let mut je_rows = Vec::new();
    let mut emitted_assistant_row_ids = std::collections::HashSet::new();
    let mut assistant_row_models = std::collections::HashMap::new();
    let mut early_tool_result_ids = std::collections::HashSet::new();
    let mut prior_siblings = Vec::new();
    let mut response: Option<(
        llm_runtime::HistoryResponse,
        futures::stream::BoxStream<'static, Result<HistoryEvent, LlmError>>,
    )> = None;
    let mut response_error: Option<(Vec<llm_runtime::ContentBlock>, LlmError)> = None;
    let mut active_response_model = initial_response_model.clone();
    // Keep the route being served separately from ResponseObserved: Native
    // emits that event when a fallback candidate starts, before the host has
    // accepted it. Only an admitted visible hop advances this source route.
    let mut query_serving_model = initial_response_model;
    let mut declined_fallback = None;
    let mut declined_api_error_row = None;
    let mut partial_response = None;

    loop {
        enum Next {
            Event(Option<Result<HistoryEvent, LlmError>>),
            Tool(
                Option<(
                    usize,
                    Result<
                        lingxi_core::host::tool_invoker::ToolInvocationResult,
                        lingxi_core::host::tool_invoker::ToolInvokerError,
                    >,
                )>,
            ),
        }
        let next = if executor.has_inflight() {
            tokio::select! {
                biased;
                completion = executor.next_completion() => Next::Tool(completion),
                event = stream.next() => Next::Event(event),
            }
        } else {
            Next::Event(stream.next().await)
        };
        let item = match next {
            Next::Tool(Some((index, result))) => {
                if let Some(dispatch) = dispatch.as_ref() {
                    let call = executor.call(index).cloned();
                    publish_live_tool_completion(
                        call,
                        &result,
                        ctx,
                        out_tx,
                        &mut ordered_rows,
                        &mut je_rows,
                        &mut early_tool_result_ids,
                        lifecycle,
                    )
                    .await;
                    executor.record_completion(index, result, dispatch);
                }
                continue;
            }
            Next::Tool(None) => continue,
            Next::Event(item) => item,
        };
        let Some(item) = item else {
            let partial = accumulator.partial_content().to_vec();
            let error =
                accumulator
                    .take_malformed_error()
                    .unwrap_or_else(|| LlmError::StreamInterrupted {
                        message: "stream ended without message_stop or completed event".into(),
                    });
            response_error = Some((partial, error));
            break;
        };
        let event = match item {
            Ok(event) => event,
            Err(error) => {
                let partial = accumulator.partial_content().to_vec();
                response_error =
                    Some((partial, accumulator.take_malformed_error().unwrap_or(error)));
                break;
            }
        };

        if let HistoryEvent::ResponseObserved { model, .. } = &event {
            active_response_model.clone_from(model);
        }

        if let HistoryEvent::ServerFallback {
            event: fallback,
            lane,
            profile,
        } = &event
        {
            let visible = matches!(fallback.reason.as_str(), "refusal" | "sticky");
            let discarded_tool = executor.contains_discarded_tool(&fallback.discarded_blocks);
            let allowed = !ctx
                .server_fallback_model_enforcement
                .as_ref()
                .is_some_and(|policy| {
                    visible
                        && llm_runtime::model::allowlist::model_allowed_under(
                            policy,
                            &fallback.to_model,
                        ) == Some(false)
                });
            if visible && !allowed {
                // Native declines are a query-level abandon, not a decision to
                // keep reading the rejected serving model's body. The host
                // executor owns the outstanding ids, so cancel all work and
                // remove every tracked id even when the event's discarded
                // list is empty or contains no tool block.
                let removal = executor.reset_for_server_fallback(None);
                if let Some(removal) = removal {
                    let idle_update = lifecycle
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .apply_removal(&removal);
                    publish_local_agent_idle_fact(ctx, idle_update).await;
                }
                prior_siblings.clear();
                let removed_rows = remove_discarded_assistant_rows(
                    &mut assistant_rows,
                    &mut ordered_rows,
                    &fallback.discarded_blocks,
                    &mut assistant_row_models,
                    Some(&fallback.to_model),
                );
                for (row, model) in removed_rows {
                    emit_fallback_tombstone(
                        out_tx,
                        transcript,
                        ctx.agent_id,
                        &row.accepted,
                        model.as_deref(),
                        false,
                    )
                    .await;
                }
                let tombstoned_je = clear_stream_je_rows(&mut je_rows, &mut ordered_rows);
                for row in tombstoned_je {
                    emit_fallback_tombstone(out_tx, transcript, ctx.agent_id, &row, None, false)
                        .await;
                }
                let decline_error = server_fallback_decline_error(
                    fallback.reason.as_str(),
                    fallback.to_model.as_str(),
                );
                declined_fallback = Some(decline_error);
                declined_api_error_row = declined_server_fallback_api_error_row(
                    ctx.api_client.as_deref(),
                    &query_serving_model,
                    Some(profile),
                    fallback,
                );
                // Preserve the typed fallback/usage observation, but represent
                // the host decline outside the provider-error channel.
                let _ = accumulator.observe(event);
                partial_response = Some(accumulator.partial_snapshot());
                break;
            }
            if visible && allowed {
                // Accepted Native fallback always removes discarded assistant
                // rows from K by their source block identities. JE is separate:
                // it is retained unless a discarded block was a tool use.
                let removed_rows = remove_discarded_assistant_rows(
                    &mut assistant_rows,
                    &mut ordered_rows,
                    &fallback.discarded_blocks,
                    &mut assistant_row_models,
                    None,
                );
                if discarded_tool {
                    let reason = Some(
                        lingxi_core::host::tool_use_lifecycle::ToolUseRemovalReason::FallbackSweep,
                    );
                    if let Some(removal) = executor.reset_for_server_fallback(reason) {
                        let idle_update = lifecycle
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .apply_removal(&removal);
                        publish_local_agent_idle_fact(ctx, idle_update).await;
                    }
                    prior_siblings.clear();
                }
                for (row, model) in removed_rows {
                    emit_fallback_tombstone(
                        out_tx,
                        transcript,
                        ctx.agent_id,
                        &row.accepted,
                        model.as_deref(),
                        true,
                    )
                    .await;
                }
                if discarded_tool {
                    let tombstoned_je = clear_stream_je_rows(&mut je_rows, &mut ordered_rows);
                    for row in tombstoned_je {
                        emit_fallback_tombstone(out_tx, transcript, ctx.agent_id, &row, None, true)
                            .await;
                    }
                }
                query_serving_model =
                    lingxi_core::host::refusal_server_control::resolve_received_model(
                        Some(&lane.model),
                        &fallback.to_model,
                    );
                active_response_model.clone_from(&query_serving_model);
                for row_model in assistant_row_models.values_mut() {
                    row_model.clone_from(&active_response_model);
                }
            }
        }

        match accumulator.observe(event) {
            Ok(ResponseAccumulatorUpdate::Continue {
                completed_block: Some((index, block)),
            }) => {
                let source_tool_use_block_indices =
                    matches!(&block, llm_runtime::ContentBlock::ToolCall { .. })
                        .then_some(index)
                        .into_iter()
                        .collect::<Vec<_>>();
                stage_assistant_row(
                    index,
                    std::slice::from_ref(&block),
                    None,
                    &source_tool_use_block_indices,
                    ctx,
                    transcript,
                    &request_messages,
                    system_prompt.as_deref(),
                    &active_response_model,
                    &logical_tool_model,
                    logical_tool_model_profile.clone(),
                    tool_context_state.clone(),
                    allowed_tools,
                    force_structured_tool,
                    defer_local_work,
                    retryable_body,
                    live_output_started,
                    tool_effects_started,
                    &mut emitted_assistant_row_ids,
                    &mut assistant_row_models,
                    &mut executor,
                    dispatch.as_ref(),
                    &mut prior_siblings,
                    &mut assistant_rows,
                    &mut ordered_rows,
                    lifecycle,
                    out_tx,
                )
                .await;
            }
            Ok(ResponseAccumulatorUpdate::Continue { .. }) => {}
            Ok(ResponseAccumulatorUpdate::Completed(completed)) => {
                if let Some((fallback, serving_model)) = declined_fallback_in_response(
                    &completed,
                    ctx.server_fallback_model_enforcement.as_ref(),
                    &query_serving_model,
                ) {
                    let error = server_fallback_decline_error(
                        &fallback.event.reason,
                        &fallback.event.to_model,
                    );
                    declined_api_error_row = declined_server_fallback_api_error_row(
                        ctx.api_client.as_deref(),
                        &serving_model,
                        Some(&fallback.profile),
                        &fallback.event,
                    );
                    partial_response = Some(completed.clone());
                    let removal = executor.reset_for_server_fallback(None);
                    if let Some(removal) = removal {
                        let idle_update = lifecycle
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .apply_removal(&removal);
                        publish_local_agent_idle_fact(ctx, idle_update).await;
                    }
                    declined_fallback = Some(error.clone());
                    // A complete-response adapter can carry the same host
                    // observation in metadata. Do not stage its rejected body.
                    break;
                }
                // A client may provide a complete response snapshot instead of
                // block events. Keep that adapter path complete, while normal
                // streaming starts each tool at its stop boundary above.
                if assistant_rows.is_empty() {
                    // Native normalizes a complete-only response into one
                    // assistant row at apiBlockIndex 0. Keep source tool
                    // ordinals separate for the executor and same-row siblings.
                    let source_tool_use_block_indices = completed
                        .content
                        .iter()
                        .enumerate()
                        .filter_map(|(index, block)| {
                            matches!(block, llm_runtime::ContentBlock::ToolCall { .. })
                                .then(|| u32::try_from(index).ok())
                                .flatten()
                        })
                        .collect::<Vec<_>>();
                    let mut complete_row_prior_siblings = Vec::new();
                    stage_assistant_row(
                        0,
                        &completed.content,
                        completed.stop_reason.as_deref(),
                        &source_tool_use_block_indices,
                        ctx,
                        transcript,
                        &request_messages,
                        system_prompt.as_deref(),
                        &active_response_model,
                        &logical_tool_model,
                        logical_tool_model_profile.clone(),
                        tool_context_state.clone(),
                        allowed_tools,
                        force_structured_tool,
                        defer_local_work,
                        retryable_body,
                        live_output_started,
                        tool_effects_started,
                        &mut emitted_assistant_row_ids,
                        &mut assistant_row_models,
                        &mut executor,
                        dispatch.as_ref(),
                        &mut complete_row_prior_siblings,
                        &mut assistant_rows,
                        &mut ordered_rows,
                        lifecycle,
                        out_tx,
                    )
                    .await;
                }
                if defer_local_work {
                    if executor.has_pending_calls() {
                        tool_effects_started.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    accept_deferred_assistant_rows(
                        &mut assistant_rows,
                        transcript,
                        &request_messages,
                        completed.stop_reason.as_deref(),
                    )
                    .await;
                    // Queued calls captured their row before deferred Mod
                    // acceptance. Update those snapshots after the accepted
                    // content and terminal metadata are both available.
                    for row in &assistant_rows {
                        executor.refresh_assistant_message(&row.accepted);
                    }
                    emit_unpublished_assistant_rows(
                        &assistant_rows,
                        &mut emitted_assistant_row_ids,
                        out_tx,
                        ctx.agent_id,
                        live_output_started,
                    )
                    .await;
                    if let Some(dispatch) = dispatch.as_ref() {
                        executor.start_queued_calls(dispatch);
                    }
                }
                response = Some((completed, stream));
                break;
            }
            Err((partial, error)) => {
                response_error = Some((partial, error));
                break;
            }
        }
    }

    if response_error.is_some() || declined_fallback.is_some() {
        // The Native query finalizer drains completed results before it
        // discards the executor. Preserve every future already ready at this
        // boundary, cancel the rest before dropping them, and do not start
        // queued calls while handling a failed provider stream.
        let completed = executor.abort_inflight_and_collect_ready().await;
        for (index, result) in completed {
            let call = executor.call(index).cloned();
            publish_live_tool_completion(
                call,
                &result,
                ctx,
                out_tx,
                &mut ordered_rows,
                &mut je_rows,
                &mut early_tool_result_ids,
                lifecycle,
            )
            .await;
            executor.record_completion_without_scheduling(index, result);
        }
        if defer_local_work {
            // Calls held for host-side response qualification have never run.
            // Clear their pending lifecycle ids on this failed attempt without
            // manufacturing ToolResults for effects that were never started.
            if let Some(removal) = executor.reset_for_server_fallback(None) {
                let idle_update = lifecycle
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .apply_removal(&removal);
                publish_local_agent_idle_fact(ctx, idle_update).await;
            }
        }
    } else if let Some(dispatch) = dispatch.as_ref() {
        while executor.has_inflight() {
            if let Some((index, result)) = executor.next_completion().await {
                let call = executor.call(index).cloned();
                publish_live_tool_completion(
                    call,
                    &result,
                    ctx,
                    out_tx,
                    &mut ordered_rows,
                    &mut je_rows,
                    &mut early_tool_result_ids,
                    lifecycle,
                )
                .await;
                executor.record_completion(index, result, dispatch);
            }
        }
        executor.mark_completed_normally();
    }
    let tool_calls = executor.into_calls();
    let response = if declined_fallback.is_some() {
        None
    } else {
        Some(response.map(Ok).unwrap_or_else(|| {
            Err(response_error.unwrap_or_else(|| {
                (
                    accumulator.partial_content().to_vec(),
                    LlmError::StreamInterrupted {
                        message: "stream ended without message_stop or completed event".into(),
                    },
                )
            }))
        }))
    };
    if partial_response.is_none() && response.as_ref().is_some_and(Result::is_err) {
        partial_response = Some(accumulator.partial_snapshot());
    }
    AgentStreamAttempt {
        response,
        assistant_rows,
        ordered_rows,
        je_rows,
        early_tool_result_ids,
        tool_calls,
        request_messages,
        declined_fallback,
        declined_api_error_row,
        partial_response,
    }
}

const USAGE_LIMIT_NEAR_WRAP_UP_FLAG: &str = "tengu_vellum_anchor";
const NEAR_LIMIT_WRAP_UP_NOTE: &str = "[Usage limit approaching. Checkpoint now: finish the current step, then list up to 3 short bullets of the most impactful remaining work. Don't start subagents or long-running work.]";

fn usage_limit_near_wrap_up_enabled() -> bool {
    ::telemetry::flag_bool(USAGE_LIMIT_NEAR_WRAP_UP_FLAG, false)
}

async fn maybe_emit_near_limit_wrap_up(
    history: &mut Vec<ConversationMessage>,
    ctx: &SubagentContext,
    api_client: &dyn crate::api::SubagentApiClient,
    out_tx: &mpsc::Sender<SubagentEvent>,
    agent_id: AgentId,
) {
    if ctx.depth == 0 {
        return;
    }
    // Oracle evaluation order is `depth > 0 && consumePendingHint() &&
    // flag("tengu_vellum_anchor", false)`: a disabled flag still consumes and
    // drops the one-shot hint, preventing a stale emission if flags refresh.
    if !api_client.consume_pending_near_limit_wrap_up_hint() || !usage_limit_near_wrap_up_enabled()
    {
        return;
    }

    // `Le()` is the owning session's print/SDK gate. A background child of an
    // interactive session is still interactive for this purpose, so `is_async`
    // must not suppress the checkpoint. The oracle starts this fire-and-forget
    // work before yielding either the UI notice or the model-visible note.
    api_client.record_usage_limit_near_wrap_up();
    let non_interactive = ctx.session_interactive == Some(false);
    if !non_interactive {
        api_client.dispatch_near_limit_checkpoint(crate::api::NearLimitCheckpointRequest {
            session_id: ctx.hook_session_id,
            cwd: ctx.hook_cwd.clone(),
            non_interactive,
        });
    }

    let note =
        ConversationMessage::user_meta(MessageId::new(), NEAR_LIMIT_WRAP_UP_NOTE.to_string());
    history.push(note.clone());
    emit_message(out_tx, agent_id, &note).await;
}

fn workflow_watchdog_timeout_error(phase: &str, timeout: Duration) -> LlmError {
    LlmError::TransportTimeout {
        message: format!(
            "workflow model query stalled while {phase} for {}ms",
            timeout.as_millis()
        ),
    }
}

fn is_workflow_watchdog_timeout(error: &LlmError) -> bool {
    matches!(
        error,
        LlmError::TransportTimeout { message }
            if message.starts_with("workflow model query stalled while ")
    )
}

/// Apply the workflow watchdog to one model-query phase. This helper is used
/// only for stream establishment; tool execution is deliberately outside every
/// call site, so a slow tool cannot consume the model-query idle budget.
async fn await_workflow_query_phase<T, F>(
    future: F,
    watchdog: Option<WorkflowQueryWatchdog>,
    phase: &'static str,
) -> Result<T, LlmError>
where
    F: Future<Output = Result<T, LlmError>>,
{
    let Some(policy) = watchdog else {
        return future.await;
    };
    let timeout = Duration::from_millis(policy.stall_timeout_ms);
    match tokio::time::timeout(timeout, future).await {
        Ok(result) => result,
        Err(_) => Err(workflow_watchdog_timeout_error(phase, timeout)),
    }
}

/// Wrap a response stream with a per-event idle timeout. Every successful
/// `next()` starts a fresh timeout, so total stream lifetime is unbounded while
/// progress continues. The wrapper yields one typed timeout error then closes.
fn with_workflow_stream_watchdog(
    stream: futures::stream::BoxStream<'static, Result<HistoryEvent, LlmError>>,
    watchdog: Option<WorkflowQueryWatchdog>,
) -> futures::stream::BoxStream<'static, Result<HistoryEvent, LlmError>> {
    let Some(policy) = watchdog else {
        return stream;
    };
    let timeout = Duration::from_millis(policy.stall_timeout_ms);
    futures::stream::unfold(
        (stream, false),
        move |(mut stream, terminated)| async move {
            if terminated {
                return None;
            }
            match tokio::time::timeout(timeout, stream.next()).await {
                Ok(Some(event)) => Some((event, (stream, false))),
                Ok(None) => None,
                Err(_) => Some((
                    Err(workflow_watchdog_timeout_error(
                        "waiting for the next response event",
                        timeout,
                    )),
                    (stream, true),
                )),
            }
        },
    )
    .boxed()
}

/// A cancelled/dropped worker future must not leave a pending hook snapshot.
struct PromptTranscriptCancellationGuard {
    executor: Option<std::sync::Arc<hooks::HookExecutorImpl>>,
    session_id: lingxi_core::types::SessionId,
    agent_id: lingxi_core::types::AgentId,
    owner: Option<std::sync::Arc<dyn lingxi_core::host::subagent_spawn::SubagentStopHookFirer>>,
    completed: bool,
}

impl Drop for PromptTranscriptCancellationGuard {
    fn drop(&mut self) {
        if !self.completed {
            if let Some(executor) = &self.executor {
                executor.discard_agent_prompt_transcript(
                    self.session_id,
                    self.agent_id,
                    self.owner.as_ref(),
                );
            }
        }
    }
}

/// Subagent state-machine loop.
///
/// Drives the multi-turn loop and emits [`SubagentEvent`]s on `out_tx`.
/// A missing API client emits a startup failure.
pub async fn run_subagent(
    ctx: SubagentContext,
    event_rx: mpsc::Receiver<lingxi_core::Event>,
    out_tx: mpsc::Sender<SubagentEvent>,
) {
    let _restore_startup =
        crate::context::HandbackRestoreStartupGuard(ctx.handback_restore_start.clone());
    let input_cleanup = ctx
        .tool_invoker
        .clone()
        .map(|invoker| (invoker, ctx.agent_id, ctx.origin_session_id));
    let mut snapshot_cleanup = PromptTranscriptCancellationGuard {
        executor: ctx.hook_executor.clone(),
        session_id: ctx.hook_session_id,
        agent_id: ctx.agent_id,
        owner: ctx.subagent_stop_firer.clone(),
        completed: false,
    };
    let non_interactive = ctx
        .session_interactive
        .map_or(ctx.is_async, |interactive| !interactive || ctx.is_async);
    // claude-code `agentCacheTtlOverride`: the definition's
    // `experimental.cacheTtl` rides the whole run, so every round-trip this
    // agent makes sees it. Read before `ctx` moves into the inner future.
    let prompt_cache_ttl_override = ctx.agent_definition.cache_ttl.map(|ttl| match ttl {
        crate::definition::AgentCacheTtl::FiveMinutes => {
            llm_runtime::AgentPromptCacheTtlOverride::FiveMinutes
        }
        crate::definition::AgentCacheTtl::OneHour => {
            llm_runtime::AgentPromptCacheTtlOverride::OneHour
        }
    });
    let resumed_history = ctx.resumed_history.clone();
    let request_session_id = ctx
        .origin_session_id
        .or_else(lingxi_core::host::session_flags::current_request_session_id);
    let safety_observer = ctx
        .budget
        .as_ref()
        .and_then(|budget| budget.model_safety_observer())
        .or_else(lingxi_core::host::model_safety::current_model_safety_observer);
    let inner: llm_runtime::BoxFuture<'static, ()> =
        Box::pin(run_subagent_inner(ctx, event_rx, out_tx));
    let inner: llm_runtime::BoxFuture<'static, ()> = if let Some(observer) = safety_observer {
        Box::pin(lingxi_core::host::model_safety::scope_model_safety(
            observer, inner,
        ))
    } else {
        inner
    };
    let inner: llm_runtime::BoxFuture<'static, ()> = Box::pin(
        lingxi_core::host::session_flags::scope_subagent_request_session_id(
            request_session_id,
            inner,
        ),
    );
    crate::transcript::scope_message_row_indexes(
        resumed_history.as_deref(),
        llm_runtime::scope_agent_prompt_cache_ttl(
            prompt_cache_ttl_override,
            llm_runtime::thinking_scope::scope_thinking_recovery(
                llm_runtime::thinking_scope::ThinkingRecoveryScope::default(),
                lingxi_core::host::session_flags::scope_non_interactive_session(
                    non_interactive,
                    // This large state-machine future must live off Tokio's
                    // default worker stack after adding Mod turn state.
                    inner,
                ),
            ),
        ),
    )
    .await;
    if let Some((invoker, agent_id, origin_session_id)) = input_cleanup {
        if let Err(error) = invoker
            .cleanup_computer_inputs(agent_id, origin_session_id)
            .await
        {
            tracing::error!(%agent_id, %error, "Failed to release Agent computer inputs");
        }
    }
    snapshot_cleanup.completed = true;
}

fn cleanup_agent_inputs(
    ctx: &SubagentContext,
) -> llm_runtime::BoxFuture<'static, Result<(), String>> {
    let owner = ctx
        .tool_invoker
        .clone()
        .map(|invoker| (invoker, ctx.agent_id, ctx.origin_session_id));
    Box::pin(async move {
        if let Some((invoker, agent_id, session_id)) = owner {
            for attempt in 0..3 {
                match invoker.cleanup_computer_inputs(agent_id, session_id).await {
                    Ok(()) => return Ok(()),
                    Err(error) if attempt == 2 => {
                        return Err(format!(
                            "Computer input cleanup failed for Agent {agent_id}: {error}. The desktop remains reserved until this owner's inputs are released."
                        ));
                    }
                    Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
                }
            }
        }
        Ok(())
    })
}

async fn cleanup_before_terminal(
    ctx: &SubagentContext,
    out_tx: &mpsc::Sender<SubagentEvent>,
    transcript: Option<&crate::transcript::AgentTranscriptWriter>,
    cumulative_usage: llm_runtime::ExecutionUsage,
    history: &[lingxi_core::types::ConversationMessage],
    hook_usage: &llm_runtime::ExecutionUsage,
    hook_model_selection: &hooks::HookModelSelection,
) -> bool {
    if let Err(error) = cleanup_agent_inputs(ctx).await {
        if let Some(writer) = transcript {
            let _ = writer.record_terminal("failed", Some(&error)).await;
        }
        publish_prompt_hook_transcript(ctx, history, hook_usage, hook_model_selection);
        let _ = out_tx
            .send(SubagentEvent::Failed {
                agent_id: ctx.agent_id,
                error,
                cumulative_usage,
            })
            .await;
        return false;
    }
    true
}

async fn run_subagent_inner(
    ctx: SubagentContext,
    event_rx: mpsc::Receiver<lingxi_core::Event>,
    out_tx: mpsc::Sender<SubagentEvent>,
) {
    // Keep cleanup outside the loop so every terminal return closes it.
    let diagnostics_cleanup = ctx.new_diagnostics_source.clone();
    let mut live_hook_model_selection = hooks::registry::HookModelSelection {
        model: resolve_model(&ctx),
        model_profile: ctx.model_profile.clone(),
    };
    if ctx.api_client.is_none() {
        let mut transcript_written = 0;
        emit_failed(
            &ctx,
            &out_tx,
            None,
            &mut [],
            &mut transcript_written,
            ctx.agent_id,
            "Subagent model API is not configured".into(),
            llm_runtime::ExecutionUsage::default(),
            &llm_runtime::ExecutionUsage::default(),
            &live_hook_model_selection,
        )
        .await;
        if let Some(source) = diagnostics_cleanup {
            source.close().await;
        }
        return;
    }

    // G4 (frontmatter hooks): register the agent definition's frontmatter hooks
    // scoped to this child `agent_id` BEFORE the run and clear them AFTER —
    // claude `registerFrontmatterHooks(…, isAgent=true)` (runAgent.ts:557-575)
    // then `clearSessionHooks(agentId)` in the `runAgent` finally. `isAgent=true`
    // retargets each `Stop` subscription to `SubagentStop` (a subagent's loop end
    // fires `SubagentStop`). Wrapped here at the dispatcher so the clear runs
    // regardless of how the body returns (the loop has many early returns), and
    // so an un-wired `hook_executor` (tests / minimal builds) is a strict no-op.
    //
    // (cc 2.1.218 `mvo`) ORIGIN TRUST gate: registering these hooks installs
    // COMMANDS, so a definition whose folder has never been trusted must not get
    // them — the `--add-dir <untrusted-repo>` case, where the repo ships
    // `<dot>/agents/*.md` with a `hooks:` block. 2.1.217 registered
    // unconditionally; 2.1.218 skips + logs + counts instead.
    let frontmatter_cleanup = match &ctx.hook_executor {
        Some(_)
            if !ctx.agent_definition.frontmatter_hooks.is_empty()
                && ctx.strict_plugin_only_hooks
                && !crate::mcp_servers::plugin_trusted_source(ctx.agent_definition.source) =>
        {
            tracing::warn!(
                agent = %ctx.agent_definition.agent_type,
                "Skipping agent frontmatter hooks: strictPluginOnlyCustomization locks hooks to plugin-only sources"
            );
            None
        }
        Some(he)
            if !ctx.agent_definition.frontmatter_hooks.is_empty()
                && (!ctx.strict_plugin_only_hooks
                    || crate::mcp_servers::plugin_trusted_source(ctx.agent_definition.source)) =>
        {
            let cwd = ctx
                .cwd
                .clone()
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
            if crate::hooks_trust::agent_hooks_origin_trusted(&ctx.agent_definition, &cwd) {
                he.register_agent_hooks(
                    ctx.agent_id,
                    &ctx.agent_definition.frontmatter_hooks,
                    true,
                )
                .await;
                Some((he.clone(), ctx.agent_id))
            } else {
                crate::hooks_trust::report_untrusted_hooks(
                    &ctx.agent_definition,
                    &cwd,
                    crate::hooks_trust::HooksTrustSurface::Subagent,
                    false,
                );
                None
            }
        }
        _ => None,
    };

    // #9 (dead SubagentStop): when this agent registered frontmatter hooks, the
    // `Stop`→`SubagentStop` retargeted ones (registerFrontmatterHooks isAgent=true)
    // must actually FIRE at the loop's natural end — claude fires the subagent's
    // stop hooks INSIDE the child (`stopHooks.ts` via `query.ts`, keyed on
    // `toolUseContext.agentId`). The LingXi orchestrator-side `SubagentStop`
    // chokepoint runs AFTER this dispatcher returns (i.e. after `clear_agent_hooks`
    // below), so those retargeted frontmatter hooks would be dead code without an
    // in-child fire. We fire SubagentStop here, AGENT-SCOPED to this child's own
    // bucket (see `execute_agent_scoped`), so session / plugin `SubagentStop`
    // hooks are NOT double-fired — the chokepoint already covers those.
    //
    // To carry a faithful run status we proxy `out_tx`: forward every
    // `SubagentEvent` to the real channel while remembering the terminal one, so
    // the status maps to claude's outcome (`completed` / `failed` — `Killed` does
    // not fire SubagentStop, matching claude where an aborted child's loop does
    // not reach `stopHooks`). When there are NO frontmatter hooks (the common
    // case) we skip the proxy entirely and pass `out_tx` straight through, so the
    // hot path is byte-identical to legacy.
    let agent_scoped_stop = frontmatter_cleanup.as_ref().map(|(he, agent_id)| {
        // FIX 2: the agent-scoped SubagentStop carries `agent_transcript_path`
        // (claude-code `getAgentTranscriptPath(subagentId)`, coreSchemas.ts:556 /
        // utils/hooks.ts:3676). TS builds it as
        // `…/subagents[/subdir]/agent-${agentId}.jsonl`; LingXi mirrors the
        // `agent-<id>.jsonl` leaf under this child's `transcript_subdir`.
        // FIX C: the production spawn path (handle.rs) now seeds `transcript_subdir`
        // to the REAL session-scoped dir
        // `<lingxi_home>/projects/<sanitize(cwd)>/<session>/subagents` (threaded
        // from the composition root via `with_hook_context`), so this is the true
        // `getAgentTranscriptPath` location — not the former `/tmp` placeholder.
        // Tests / minimal builds that wire no subagents dir keep the `/tmp` default.
        let agent_transcript_path = ctx
            .transcript_subdir
            .join(format!("agent-{agent_id}.jsonl"));
        (
            he.clone(),
            *agent_id,
            ctx.agent_definition.agent_type.clone(),
            ctx.hook_session_id,
            ctx.hook_cwd.clone(),
            agent_transcript_path,
        )
    });

    let mut live_hook_transcript = hooks::PromptHookTranscript::default();
    let hook_inherit = subagent_hook_inheritance(&ctx);
    let hook_depth = ctx.depth;
    let hook_permission_mode = ctx.permission_mode_override.clone();
    let terminal_status = if agent_scoped_stop.is_some() {
        // Proxy: forward events, capture the terminal disposition.
        let (proxy_tx, mut proxy_rx) = mpsc::channel::<SubagentEvent>(16);
        let forwarder = {
            let real = out_tx.clone();
            tokio::spawn(async move {
                let mut terminal: Option<(&'static str, Option<String>)> = None;
                while let Some(ev) = proxy_rx.recv().await {
                    terminal = match &ev {
                        SubagentEvent::Completed { result, .. } => {
                            Some(("completed", completed_result_text(result)))
                        }
                        SubagentEvent::Failed { .. } => Some(("failed", None)),
                        // Killed does not fire SubagentStop (claude: an aborted
                        // child throws before reaching its stop hooks).
                        SubagentEvent::Killed { .. } => None,
                        // Non-terminal: keep whatever terminal we last saw.
                        _ => terminal,
                    };
                    // Best-effort forward; a closed receiver drops the rest.
                    if real.send(ev).await.is_err() {
                        break;
                    }
                }
                terminal
            })
        };
        // Run the body against the proxy, then drop our proxy sender so the
        // forwarder's `recv()` loop ends and we can read the captured status.
        Box::pin(run_subagent_loop(
            ctx,
            event_rx,
            proxy_tx,
            &mut live_hook_transcript,
            &mut live_hook_model_selection,
        ))
        .await;
        forwarder.await.unwrap_or(None)
    } else {
        // No frontmatter hooks: straight passthrough, no proxy overhead.
        Box::pin(run_subagent_loop(
            ctx,
            event_rx,
            out_tx,
            &mut live_hook_transcript,
            &mut live_hook_model_selection,
        ))
        .await;
        None
    };

    // Fire the agent-scoped SubagentStop BEFORE clearing the frontmatter hooks
    // (otherwise the retargeted Stop→SubagentStop hooks are already gone). Only
    // when the child reached a terminal that fires SubagentStop in claude
    // (`completed` / `failed`).
    if let (
        Some((he, agent_id, agent_type, session_id, cwd, agent_transcript_path)),
        Some((status, last_assistant_message)),
    ) = (agent_scoped_stop, terminal_status)
    {
        let stop_ctx = hooks::registry::HookContext {
            model_selection: Some(live_hook_model_selection),
            agent_depth: Some(hook_depth),
            inherit: hook_inherit,
            permission_mode: hook_permission_mode,
            prompt_transcript: Some(live_hook_transcript),
            session_id,
            agent_id: Some(agent_id),
            cwd,
            agent_type: Some(agent_type.clone()),
            last_assistant_message,
            // FIX 2: SubagentStop carries the agent's own transcript path
            // (claude-code `agent_transcript_path`). See the tuple build above.
            agent_transcript_path: Some(agent_transcript_path),
            ..Default::default()
        };
        he.execute_agent_scoped(
            hooks::events::HookEvent::SubagentStop {
                agent_id,
                status: status.to_string(),
                // claude keys SubagentStop matchers on the subagent's type.
                agent_type,
            },
            stop_ctx,
            agent_id,
        )
        .await;
    }

    if let Some((he, agent_id)) = frontmatter_cleanup {
        he.clear_agent_hooks(agent_id).await;
    }
    if let Some(source) = diagnostics_cleanup {
        source.close().await;
    }
}

/// Resolve the wire model string from the agent definition.
///
/// This forwards the definition's model string verbatim. For the production
/// spawn path the model is ALREADY resolved to a concrete wire id at spawn time
/// by [`crate::model_resolution::resolve_agent_model`] (`Inherit` → parent /
/// main-loop model, bare family alias → concrete `claude-*` id), so the
/// definition here carries an `Explicit(...)` id and `resolve_model` simply
/// forwards it. Only when the spawner has no `default_model` wired (legacy /
/// tests) does this forward a raw `"inherit"` / bare alias — which then resolves
/// solely via any configured `routing.aliases`.
/// Validate a captured `StructuredOutput` input against the workflow
/// `agent({schema})` JSON Schema (claude-code's Ajv `validateSchema`/`compile`
/// inside the StructuredOutput tool `call`). Returns a concise leaf-error string
/// on mismatch, `Ok(())` on a valid input.
///
/// BEHAVIORAL parity only: PASS/FAIL matches claude-code (full JSON-Schema
/// validation), but the error-detail bytes differ from Ajv's
/// `${instancePath}: ${message}` form (boon's native messages are unportable —
/// the same intentional divergence as `orchestrator::schema_validation`). The
/// byte-exact part is the `Output does not match required schema: ` WRAPPER the
/// caller prepends. A schema that fails to COMPILE is treated as PASS (a LingXi
/// schema bug must not block the model), matching the binary's stance.
pub fn validate_structured_output(
    schema_json: Option<&str>,
    input: &serde_json::Value,
) -> Result<(), String> {
    let Some(schema_str) = schema_json else {
        return Ok(());
    };
    let schema: serde_json::Value = match serde_json::from_str(schema_str) {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    const URL: &str = "mem://structured-output-schema";
    let mut schemas = boon::Schemas::new();
    let mut compiler = boon::Compiler::new();
    if compiler.add_resource(URL, schema).is_err() {
        return Ok(());
    }
    let sch = match compiler.compile(URL, &mut schemas) {
        Ok(s) => s,
        Err(_) => return Ok(()),
    };
    match schemas.validate(input, sch) {
        Ok(()) => Ok(()),
        Err(err) => {
            let mut out = Vec::new();
            flatten_schema_error(&err, &mut out);
            Err(out.join(", "))
        }
    }
}

/// Flatten a boon validation error into concise `at '<loc>': <kind>` leaves
/// (mirrors `orchestrator::schema_validation::flatten`).
fn flatten_schema_error(err: &boon::ValidationError, out: &mut Vec<String>) {
    if err.causes.is_empty() {
        let loc = err.instance_location.to_string();
        let loc = if loc.is_empty() {
            "(root)".to_string()
        } else {
            loc
        };
        out.push(format!("at '{loc}': {}", err.kind));
    } else {
        for cause in &err.causes {
            flatten_schema_error(cause, out);
        }
    }
}

/// The workflow `agent({schema})` StructuredOutput retry cap — claude-code
/// `Fe.MAX_STRUCTURED_OUTPUT_RETRIES ?? OBp`, where `OBp = 5`. The env override
/// matches the binary's `parseInt(process.env.MAX_STRUCTURED_OUTPUT_RETRIES||"5")`.
fn structured_output_retry_cap() -> u32 {
    std::env::var("MAX_STRUCTURED_OUTPUT_RETRIES")
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(5)
}

pub(crate) fn resolve_model(ctx: &SubagentContext) -> String {
    match &ctx.agent_definition.model {
        crate::definition::AgentModel::Inherit => "inherit".to_string(),
        crate::definition::AgentModel::Alias(n) | crate::definition::AgentModel::Explicit(n) => {
            n.clone()
        }
    }
}

/// Consume only host-authored, admitted server-fallback observations. The
/// provider's `HistoryResponse.model` is not sufficient authority to change a
/// child query route: the typed event carries both the declared lane and the
/// actual received model, and the managed policy applies to the latter.
fn admitted_server_fallback_route(
    response: &llm_runtime::HistoryResponse,
    enforcement: Option<&llm_runtime::model::allowlist::ModelEnforcement>,
) -> Result<Option<(String, String)>, String> {
    let mut route = None;
    for fallback in response.server_fallback_events() {
        let event = &fallback.event;
        if !matches!(event.reason.as_str(), "refusal" | "sticky") {
            continue;
        }
        if enforcement.is_some_and(|policy| {
            llm_runtime::model::allowlist::model_allowed_under(policy, &event.to_model)
                == Some(false)
        }) {
            return Err(server_fallback_decline_error(
                &event.reason,
                &event.to_model,
            ));
        }
        route = Some((
            lingxi_core::host::refusal_server_control::resolve_received_model(
                Some(&fallback.lane.model),
                &event.to_model,
            ),
            fallback.profile,
        ));
    }
    Ok(route)
}

fn declined_fallback_in_response(
    response: &llm_runtime::HistoryResponse,
    enforcement: Option<&llm_runtime::model::allowlist::ModelEnforcement>,
    initial_serving_model: &str,
) -> Option<(llm_runtime::history::HistoryServerFallback, String)> {
    let mut serving_model = initial_serving_model.to_owned();
    for fallback in response.server_fallback_events() {
        if !matches!(fallback.event.reason.as_str(), "refusal" | "sticky") {
            continue;
        }
        if enforcement.is_some_and(|policy| {
            llm_runtime::model::allowlist::model_allowed_under(policy, &fallback.event.to_model)
                == Some(false)
        }) {
            return Some((fallback, serving_model));
        }
        serving_model = lingxi_core::host::refusal_server_control::resolve_received_model(
            Some(&fallback.lane.model),
            &fallback.event.to_model,
        );
    }
    None
}

fn declined_server_fallback_api_error_row(
    api_client: Option<&dyn crate::api::SubagentApiClient>,
    source_model: &str,
    profile: Option<&str>,
    event: &llm_runtime::services::sdk::providers::anthropic::fallback_response::ServerFallbackEvent,
) -> Option<lingxi_core::host::ServerFallbackApiErrorRow> {
    let timestamp = chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string();
    if event.reason == "sticky" {
        return Some(lingxi_core::host::ServerFallbackApiErrorRow::new(
            lingxi_core::host::refusal_server_control::SERVER_FALLBACK_ALLOWLIST_ERROR,
            timestamp,
        ));
    }
    if event.reason != "refusal" {
        return None;
    }

    let Some(api_client) = api_client else {
        tracing::warn!(
            source_model,
            "refusal fallback was declined without a provider API facts source"
        );
        return None;
    };
    let snapshot = match api_client.refusal_api_text_snapshot(source_model, profile) {
        Ok(Some(snapshot)) => snapshot,
        Ok(None) => {
            tracing::warn!(
                source_model,
                profile = profile.unwrap_or("<none>"),
                "refusal fallback API-error text facts were not supplied"
            );
            return None;
        }
        Err(error) => {
            tracing::warn!(
                source_model,
                profile = profile.unwrap_or("<none>"),
                %error,
                "could not resolve refusal fallback API-error text facts"
            );
            return None;
        }
    };
    let text = match snapshot.format(
        event.api_refusal_category.as_deref(),
        event.request_id.as_deref(),
    ) {
        Ok(text) => text,
        Err(error) => {
            tracing::warn!(
                source_model,
                profile = profile.unwrap_or("<none>"),
                %error,
                "resolved refusal fallback facts could not render native API-error text"
            );
            return None;
        }
    };
    let mut row = lingxi_core::host::ServerFallbackApiErrorRow::new(&text, timestamp);
    row.set_refusal(
        event.request_id.clone(),
        serde_json::json!({
            "type": "refusal",
            "category": event.api_refusal_category,
            "explanation": null,
            "fallback_credit_token": null,
            "fallback_has_prefill_claim": null,
            "recommended_model": null,
        }),
    );
    Some(row)
}

fn server_fallback_decline_error(reason: &str, model: &str) -> String {
    if reason == "sticky" {
        lingxi_core::host::refusal_server_control::SERVER_FALLBACK_ALLOWLIST_ERROR.to_string()
    } else {
        format!(
            "Server refusal-fallback target \"{}{}",
            model,
            llm_runtime::model::allowlist::warnings::NOT_IN_ALLOWLIST_SWAP_DECLINE
        )
    }
}

/// Extract the text blocks the final agent response surfaces, with claude's
/// backward-scan fallback.
///
/// Port of claude-code `finalizeAgentTool` (agentToolUtils.ts:304-317): take the
/// text blocks from the LAST assistant message; if it carried none (the loop
/// exited mid-turn on a pure `tool_use` turn), fall back to the most recent
/// assistant message in `history` that DOES have text blocks. Returns the raw
/// text strings (one per surviving text block) in source order — the caller maps
/// them into claude's `content: [{type:'text', text}]` array.
fn final_text_blocks(
    history: &[lingxi_core::types::ConversationMessage],
    final_assistant_blocks: &[lingxi_core::types::ContentBlock],
) -> Vec<String> {
    let texts_of = |blocks: &[lingxi_core::types::ContentBlock]| -> Vec<String> {
        blocks
            .iter()
            .filter_map(|block| block.visible_text().map(str::to_owned))
            .collect()
    };
    // 1. Text from the final assistant message.
    let primary = texts_of(final_assistant_blocks);
    if !primary.is_empty() {
        return primary;
    }
    // 2. Backward scan: most recent assistant message WITH text.
    for msg in history.iter().rev() {
        if let lingxi_core::types::ConversationMessage::Assistant { content, .. } = msg {
            let t = texts_of(content);
            if !t.is_empty() {
                return t;
            }
        }
    }
    Vec::new()
}

/// Build the terminal `Completed.result` JSON for a clean stop.
///
/// Carries claude's `content` array (`[{type:'text', text}]`, agentToolUtils.ts
/// `finalizeAgentTool` return) computed via the backward-scan
/// [`final_text_blocks`], plus the legacy `text`/`stop_reason` keys existing
/// consumers (and the runner's own tests) read. `AgentTool` reads `content` to
/// build claude's structured result + model-facing trailer; the joined `text`
/// stays for back-compat (`tasks::handlers::local_agent` / `dream`).
fn build_completed_result(
    history: &[lingxi_core::types::ConversationMessage],
    final_assistant_blocks: &[lingxi_core::types::ContentBlock],
    stop_reason: Option<&str>,
    serving_model: &str,
) -> serde_json::Value {
    // `ICe`: retracted messages come out before the answer is picked, so a
    // superseded hop's output cannot become the report.
    let live = drop_retracted(history);
    let mut blocks = final_text_blocks(&live, final_assistant_blocks);
    // The `⚠ {notice}` harness note. Upstream unshifts it in `iht` AFTER the
    // turn-limit note; here the turn-limit note is inserted at index 0 by the
    // agent tool's finalizer, so prepending here lands the pair in upstream's
    // order — turn-limit, then this, then the report.
    if let Some(notice) = local_refusal_notice(&live, serving_model) {
        blocks.insert(0, format!("\u{26A0} {notice}\n"));
    }
    let content: Vec<serde_json::Value> = blocks
        .iter()
        .map(|t| serde_json::json!({ "type": "text", "text": t }))
        .collect();
    serde_json::json!({
        "content": content,
        "text": blocks.join("\n"),
        "stop_reason": stop_reason,
    })
}

/// CC 2.1.207 subagent api-error classification (`CTy` / `zho`
/// `AgentApiErrorTerminationError`). Maps an [`llm_runtime::LlmError`] surfaced by
/// a mid-stream round-trip to `(errorKind, api_error_text)` when it is an API
/// TERMINATION whose kind is in `CTy = {rate_limit, overloaded, server_error}` —
/// the only kinds CC recovers as `api_error_partial` (every other kind rethrows
/// → `Failed`). The `api_error_text` is the model-visible `API Error: …` string
/// the query-loop finalize embeds into `zho.message`
/// (`yield tu({content:…,error:"server_error"})`); the finalize always tags the
/// synthesized message `server_error`, so the connection-close / stall variants
/// still qualify. The exact request-level rate-limit copy is NOT reproduced
/// here (it lives in the orchestrator's `errors.ts` port, which the agent crate
/// cannot depend on); a mid-stream 429 is surfaced with the server-error text,
/// which is what the finalize path yields.
fn classify_api_termination(e: &llm_runtime::LlmError) -> Option<(&'static str, &'static str)> {
    use llm_runtime::LlmError;
    match e {
        LlmError::Overloaded { .. } => Some((
            "overloaded",
            "API Error: Server error mid-response. The response above may be incomplete.",
        )),
        LlmError::RateLimited { .. } => Some((
            "rate_limit",
            "API Error: Server error mid-response. The response above may be incomplete.",
        )),
        LlmError::ProviderInternal => Some((
            "server_error",
            "API Error: Server error mid-response. The response above may be incomplete.",
        )),
        LlmError::Transport { .. } => Some((
            "server_error",
            "API Error: Connection lost mid-response. The response above may be incomplete.",
        )),
        // A stall is NOT in `CTy`. Claude Code 2.1.238 classifies it through
        // `xtt`:
        //
        //     e.message.startsWith("Stream idle timeout") ||
        //     e.name === "StreamIdleTimeoutError"   ->  "api_timeout"
        //
        // and `api_timeout` is absent from
        // `CTy = new Set(["rate_limit","overloaded","server_error"])`, so the
        // oracle RETHROWS a stalled stream instead of recovering it as
        // `api_error_partial`. The suspend variant says the same thing in its
        // own message -- "aborting to retry on a fresh connection" -- which is
        // a retry on a fresh connection, not a partial answer.
        //
        // The difference is one bit and it decides whether a caller learns
        // anything. Recovered-as-partial hands a workflow stage a `completed`
        // result whose content is "I was cut off and did nothing"; the stage
        // records it as output and moves on. Observed: three consecutive
        // generate attempts stalled, each "succeeded" with an empty recovery,
        // and the failure only surfaced at the verify stage as "the app is
        // still the unmodified template".
        //
        // Every OTHER `StreamInterrupted` -- a protocol violation, a stream
        // that ended before `message_stop` -- keeps the recovering
        // classification it was ported with. There is no oracle evidence to
        // move those, and moving them on the strength of this one would be
        // guessing.
        LlmError::StreamInterrupted { message }
            if message
                .starts_with(llm_runtime::model::stream_watchdog::STREAM_IDLE_TIMEOUT_PREFIX)
                || message
                    .starts_with(llm_runtime::model::stream_watchdog::STREAM_SUSPENDED_PREFIX) =>
        {
            None
        }
        LlmError::StreamInterrupted { .. } => Some((
            "server_error",
            "API Error: The response stopped arriving. The response above may be incomplete.",
        )),
        // Auth / permission / invalid-request / quota / context / TLS / cost /
        // unsupported-capability / model-unavailable are terminal — CC rethrows
        // (errorKind not in CTy), so they surface as `Failed`.
        _ => None,
    }
}

/// Build the CC 2.1.207 subagent `api_error_partial` result: the normal
/// completed result (final text blocks via the backward scan) with `cutoff_note`
/// prepended as the FIRST text block — claude's sync-agent recovery
/// (`On.content=[{type:"text",text:Dn},...On.content]`, status `"completed"`).
fn build_recovered_result(
    history: &[lingxi_core::types::ConversationMessage],
    final_assistant_blocks: &[lingxi_core::types::ContentBlock],
    cutoff_note: &str,
) -> serde_json::Value {
    let mut blocks = final_text_blocks(history, final_assistant_blocks);
    blocks.insert(0, cutoff_note.to_string());
    let content: Vec<serde_json::Value> = blocks
        .iter()
        .map(|t| serde_json::json!({ "type": "text", "text": t }))
        .collect();
    serde_json::json!({
        "content": content,
        "text": blocks.join("\n"),
        "stop_reason": serde_json::Value::Null,
    })
}

/// Assemble the CC 2.1.207 `cutoffNote` (`wTy`): the
/// `AgentApiErrorTerminationError` message (`Agent terminated early due to an
/// API error: {api_error_text}`) followed by the byte-locked incomplete-output
/// notice, joined by a blank line (two newlines) — the binary builds
/// `cutoffNote:` + "${e.message}\n\n" + "Everything below…".
fn build_cutoff_note(api_error_text: &str) -> String {
    format!(
        "Agent terminated early due to an API error: {api_error_text}\n\n\
Everything below is PARTIAL output recovered from the agent before it was cut off. The agent did NOT finish its task \u{2014} treat these results as incomplete."
    )
}

/// Byte-locked `formatSkillLoadingMetadata(skillName)` port
/// (claude `processSlashCommand.tsx:786`): the leading text block of a preloaded
/// skill's meta user message. claude ignores the `progressMessage` arg
/// (`_progressMessage` is unused), so this renders only the (resolved) name:
/// `<command-message>{name}</command-message>\n<command-name>{name}</command-name>\n<skill-format>true</skill-format>`.
fn format_skill_loading_metadata(skill_name: &str) -> String {
    format!(
        "<command-message>{skill_name}</command-message>\n\
<command-name>{skill_name}</command-name>\n\
<skill-format>true</skill-format>"
    )
}

fn subagent_hook_inheritance(
    ctx: &SubagentContext,
) -> Option<lingxi_core::host::SubagentInheritance> {
    Some(lingxi_core::host::SubagentInheritance {
        tool_invoker: ctx.tool_invoker.clone()?,
        budget: ctx.budget.clone()?,
    })
}

/// Build the G4 (SubagentStart additionalContext) + G5 (skills) messages claude
/// `runAgent` prepends to a child's INITIAL messages before the query loop, in
/// claude's order: additionalContext (runAgent.ts:530-555) → skills
/// (runAgent.ts:577-646). (Frontmatter-hook registration — runAgent.ts:557-575,
/// ordered between them — is handled at the [`run_subagent`] dispatcher so the
/// clear is guaranteed; its registration is side-effecting, not message-producing,
/// so its position relative to these two message-producing steps is unobservable
/// in the child history.)
///
/// Returns the extra [`ConversationMessage`]s to append after the prompt seed.
/// A `None` `hook_executor` / `skill_loader` (tests / minimal builds) makes the
/// respective step a strict no-op, so the child history stays byte-identical to
/// legacy.
async fn build_preload_messages(
    ctx: &SubagentContext,
) -> Result<Vec<lingxi_core::types::ConversationMessage>, String> {
    use lingxi_core::types::{ContentBlock, ConversationMessage, MessageId};

    let agent_type = ctx.agent_definition.agent_type.clone();
    let mut out: Vec<ConversationMessage> = Vec::new();

    // --- G4: SubagentStart hooks → additionalContext injection --------------
    // claude fires `executeSubagentStartHooks(agentId, agentType, signal)`,
    // collects every hook's `additionalContexts` into ONE `string[]`, and pushes
    // a SINGLE `hook_additional_context` user message into `initialMessages`
    // (runAgent.ts:530-555). That attachment renders (messages.ts:4117-4128 via
    // `wrapInSystemReminder`) as ONE `<system-reminder>` message:
    //   `<system-reminder>\nSubagentStart hook additional context: ` +
    //   contexts.join("\n") + `\n</system-reminder>`
    // An empty collection produces NO message (messages.ts:4118 early return).
    // We match those bytes exactly: one message, the `SubagentStart hook
    // additional context: ` prefix, the `\n`-join of all contexts.
    // G008: a Fusion panel is not an ordinary `Agent` tool spawn — it is one of
    // N concurrent provider round-trips the orchestrator's own subagent-hook
    // chokepoint (turn_loop.rs) already accounts for as a SINGLE `fusion` node
    // (see `fusion_tool_result`'s `subagentHooksFired` marker below). Firing a
    // real per-panel `SubagentStart` here — with no matching `SubagentStop`,
    // since a panel definition carries no frontmatter `Stop` hook — would leave
    // N starts and zero stops for every Fusion run. Skip it for this
    // `agent_type` on EVERY entrypoint (Agent tool, `/fusion`, workflow), so a
    // Fusion run's hook activity is exactly the chokepoint's one pair.
    if let Some(hooks) = &ctx.hook_executor {
        if !lingxi_core::host::is_fusion_panel_type(&agent_type) {
            let hook_ctx = hooks::registry::HookContext {
                model_selection: Some(hooks::registry::HookModelSelection {
                    model: resolve_model(ctx),
                    model_profile: ctx.model_profile.clone(),
                }),
                agent_depth: Some(ctx.depth),
                inherit: subagent_hook_inheritance(ctx),
                permission_mode: ctx.permission_mode_override.clone(),
                session_id: ctx.hook_session_id,
                agent_id: Some(ctx.agent_id),
                cwd: ctx.hook_cwd.clone(),
                agent_type: Some(agent_type.clone()),
                ..Default::default()
            };
            let agg = hooks
                .execute(
                    hooks::events::HookEvent::SubagentStart {
                        agent_id: ctx.agent_id,
                        agent_type: agent_type.clone(),
                        parent_agent_id: ctx.parent_agent_id,
                    },
                    hook_ctx,
                )
                .await;
            if !agg.additional_contexts.is_empty() {
                let joined = hooks::ExactHookText::join(&agg.additional_contexts, "\n");
                out.push(
                    hooks::ExactHookText::wrapped(
                        "<system-reminder>\nSubagentStart hook additional context: ",
                        &joined,
                        "\n</system-reminder>",
                    )
                    .to_conversation_message(MessageId::new(), false),
                );
            }
        }
    }

    // --- G5: skills preload -------------------------------------------------
    // claude resolves+loads each frontmatter skill and pushes a `isMeta` user
    // message whose first block is `formatSkillLoadingMetadata(skillName,
    // skill.progressMessage)` followed by the loaded content blocks
    // (runAgent.ts:577-646). A missing / non-prompt skill logs claude's exact
    // warn and is skipped.
    if let Some(loader) = &ctx.skill_loader {
        for skill_name in &ctx.agent_definition.skills {
            match loader
                .resolve_and_load(
                    skill_name,
                    &agent_type,
                    ctx.cwd.as_deref(),
                    Some(resolve_model(ctx).as_str()),
                )
                .await?
            {
                None => {
                    // claude runAgent.ts:600 — exact warn string.
                    tracing::warn!(
                        "[Agent: {agent_type}] Warning: Skill '{skill_name}' specified in frontmatter was not found"
                    );
                }
                Some(load) => {
                    tracing::debug!("[Agent: {agent_type}] Preloaded skill '{skill_name}'");
                    // Leading metadata text block + the loaded content blocks
                    // (claude `createUserMessage({ content: [metadata, ...content],
                    // isMeta: true })`). LingXi's `ConversationMessage::User` has
                    // marks the message meta so last-user-query selection and UI
                    // rendering never mistake a preload for user intent.
                    let mut blocks: Vec<ContentBlock> = Vec::with_capacity(1 + load.content.len());
                    blocks.push(ContentBlock::Text {
                        text: format_skill_loading_metadata(&load.display_name),
                        citations: None,
                    });
                    blocks.extend(load.content);
                    out.push(ConversationMessage::User {
                        api_message_override: None,
                        id: MessageId::new(),
                        content: blocks,
                        is_meta: true,
                        is_compact_summary: false,
                        is_visible_in_transcript_only: false,
                    });
                }
            }
        }
    }

    Ok(out)
}

/// Translate llm-runtime content blocks into protocol content blocks.
///
/// Mirrors the orchestrator's `translate_response_blocks`: `Text` /
/// `ToolCall` / `Reasoning` map through; server-side and other variants
/// are dropped.
fn translate_response_blocks(
    content: &[llm_runtime::ContentBlock],
) -> Vec<lingxi_core::types::ContentBlock> {
    content
        .iter()
        .filter_map(|b| match b {
            llm_runtime::ContentBlock::ProviderContent { protocol, value } => Some(lingxi_core::types::ContentBlock::ProviderContent { protocol: protocol.clone(), value: value.clone() }),

            llm_runtime::ContentBlock::Text { text, citations, .. } => {
                Some(lingxi_core::types::ContentBlock::Text {
                    text: text.clone(),
                    citations: citations.clone(),
                })
            }
            llm_runtime::ContentBlock::TextJsUtf16 {
                text,
                utf16_code_units,
                citations,
                ..
            } => Some(lingxi_core::types::ContentBlock::TextJsUtf16 {
                text: text.clone(),
                utf16_code_units: utf16_code_units.clone(),
                citations: citations.clone(),
            }),
            llm_runtime::ContentBlock::ToolCall { id, name, input, input_projection } => {
                // (cc 2.1.218 `jYd`) Same literal-`\uXXXX` repair the orchestrator
                // applies — a subagent's tool inputs must be normalized too.
                let (input, _stats) =
                    llm_runtime::unicode_repair::repair_tool_input(name, input);
                let mut input_projection = input_projection.clone();
                if let Some(projection) = &mut input_projection {
                    projection.rebase_display_value(input.clone()).expect("valid tool projection remains valid after repair");
                }
                Some(lingxi_core::types::ContentBlock::ToolUse {
                    input_projection,
                    // The provider-issued id IS the canonical ToolUseId (byte
                    // parity with claude-code); the provider_id sidecar stays None.
                    id: lingxi_core::types::ToolUseId::from(id.clone()),
                    name: name.clone(),
                    input,
                    provider_id: None,
                })
            }
            llm_runtime::ContentBlock::Reasoning { text, signature } => {
                Some(lingxi_core::types::ContentBlock::Thinking {
                    thinking: text.clone(),
                    signature: signature.clone(),
                })
            }
            // Low-frequency server-side blocks: PRESERVED verbatim for resume/replay
            // byte parity (matches orchestrator::turn_loop::translate_response_blocks).
            llm_runtime::ContentBlock::RedactedThinking { data } => {
                Some(lingxi_core::types::ContentBlock::RedactedThinking { data: data.clone() })
            }
            llm_runtime::ContentBlock::ServerToolUse { id, name, input } => {
                Some(lingxi_core::types::ContentBlock::ServerToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                })
            }
            llm_runtime::ContentBlock::ConnectorText {
                connector_text,
                signature,
            } => Some(lingxi_core::types::ContentBlock::ConnectorText {
                connector_text: connector_text.clone(),
                signature: signature.clone(),
            }),
            llm_runtime::ContentBlock::AdvisorToolResult {
                tool_use_id,
                content,
                is_error,
            } => Some(lingxi_core::types::ContentBlock::AdvisorToolResult {
                tool_use_id: tool_use_id.clone(),
                content: content.clone(),
                is_error: *is_error,
            }),
            // Input-only / non-output variants remain dropped on the response path.
            llm_runtime::ContentBlock::Image { .. }
            | llm_runtime::ContentBlock::ImageUrl { .. }
            | llm_runtime::ContentBlock::Document { .. }
            | llm_runtime::ContentBlock::ToolResult { .. }
            // cache_edits is a request-only directive — never in a response.
            | llm_runtime::ContentBlock::CacheEdits { .. } => None,
        })
        .collect()
}

/// Emit `msg` as a [`SubagentEvent::Message`] on `out_tx`.
async fn emit_message(
    out_tx: &mpsc::Sender<SubagentEvent>,
    agent_id: AgentId,
    msg: &lingxi_core::types::ConversationMessage,
) {
    let message_index = crate::transcript::message_row_index(msg);
    if message_index.is_some() {
        if let Err(error) = crate::transcript::persist_current_message_index_high_water().await {
            tracing::warn!(%error, "could not persist session-agent message index high-water");
        }
    }
    let _ = out_tx
        .send(SubagentEvent::Message {
            agent_id,
            message: serde_json::to_value(msg).unwrap_or(serde_json::Value::Null),
            message_index,
        })
        .await;
}

/// Publish a rest transition without ending the foreground event pump. A
/// Completed event would deallocate its runner, which still owns background
/// notifications. This marker stays out of model history and the transcript.
async fn emit_parked(
    ctx: &SubagentContext,
    out_tx: &mpsc::Sender<SubagentEvent>,
    agent_id: AgentId,
    history: &[lingxi_core::types::ConversationMessage],
    hook_usage: &llm_runtime::ExecutionUsage,
    hook_model_selection: &hooks::HookModelSelection,
) -> bool {
    if !cleanup_before_terminal(
        ctx,
        out_tx,
        None,
        llm_runtime::ExecutionUsage::default(),
        history,
        hook_usage,
        hook_model_selection,
    )
    .await
    {
        return false;
    }
    emit_message(
        out_tx,
        agent_id,
        &lingxi_core::types::ConversationMessage::System {
            api_system: None,
            id: lingxi_core::types::MessageId::new(),
            content: "idle".to_string(),
            subtype: Some("agent_idle".to_string()),
            compact_metadata: None,
            model_fallback: None,
            refusal_fallback: None,
        },
    )
    .await;
    true
}

async fn emit_progress(
    out_tx: &mpsc::Sender<SubagentEvent>,
    agent_id: AgentId,
    tool_use_count: u64,
    token_count: u64,
) {
    let _ = out_tx
        .send(SubagentEvent::Progress {
            agent_id,
            tool_use_count: u32::try_from(tool_use_count).unwrap_or(u32::MAX),
            token_count,
        })
        .await;
}

async fn flush_transcript(
    transcript: Option<&crate::transcript::AgentTranscriptWriter>,
    history: &mut [lingxi_core::types::ConversationMessage],
    written: &mut usize,
) {
    let Some(writer) = transcript else {
        return;
    };
    // The watermark is normally <= history.len(). Be defensive around a
    // malformed restored history so transcript persistence can never panic and
    // mask the actual agent terminal event.
    let start = (*written).min(history.len());
    for index in start..history.len() {
        let (prior, remaining) = history.split_at_mut(index);
        let message = &mut remaining[0];
        if writer.record_retained(message, prior).await.is_err() {
            break;
        }
        *written += 1;
    }
}

fn publish_prompt_hook_transcript(
    ctx: &SubagentContext,
    history: &[lingxi_core::types::ConversationMessage],
    usage: &llm_runtime::ExecutionUsage,
    model_selection: &hooks::HookModelSelection,
) {
    if ctx.stop_hook_scope == lingxi_core::host::subagent_spawn::SubagentStopScope::AgentScoped
        || ctx
            .subagent_stop_firer
            .as_ref()
            .is_some_and(|owner| !owner.is_current())
    {
        return;
    }
    if let Some(executor) = &ctx.hook_executor {
        executor.publish_agent_prompt_transcript(
            ctx.hook_session_id,
            ctx.agent_id,
            model_selection.clone(),
            hooks::PromptHookTranscript {
                messages: history.to_vec(),
                last_usage_tokens: usize::try_from(
                    usage
                        .counts()
                        .input_tokens
                        .saturating_add(
                            usage
                                .counts()
                                .output_tokens
                                .saturating_sub(usage.counts().reasoning_tokens),
                        )
                        .saturating_add(usage.counts().cache_read_tokens)
                        .saturating_add(usage.counts().cache_write_tokens),
                )
                .unwrap_or(usize::MAX),
                ..Default::default()
            },
            hooks::AgentStopMetadata {
                agent_transcript_path: ctx
                    .transcript_subdir
                    .join(format!("agent-{}.jsonl", ctx.agent_id)),
                cwd: ctx.cwd.clone().unwrap_or_else(|| ctx.hook_cwd.clone()),
                last_assistant_message: history
                    .iter()
                    .rev()
                    .find(|message| {
                        matches!(
                            message,
                            lingxi_core::types::ConversationMessage::Assistant { .. }
                        )
                    })
                    .and_then(|message| {
                        let text = message.text_content();
                        (!text.trim().is_empty()).then_some(text)
                    }),
                owner: ctx.subagent_stop_firer.clone(),
                depth: Some(ctx.depth),
            },
        );
    }
}

async fn emit_failed(
    ctx: &SubagentContext,
    out_tx: &mpsc::Sender<SubagentEvent>,
    transcript: Option<&crate::transcript::AgentTranscriptWriter>,
    history: &mut [lingxi_core::types::ConversationMessage],
    written: &mut usize,
    agent_id: AgentId,
    error: String,
    cumulative_usage: llm_runtime::ExecutionUsage,
    hook_usage: &llm_runtime::ExecutionUsage,
    hook_model_selection: &hooks::HookModelSelection,
) {
    flush_transcript(transcript, history, written).await;
    emit_transcript_snapshot(out_tx, agent_id, history).await;
    let error = match cleanup_agent_inputs(ctx).await {
        Ok(()) => error,
        Err(cleanup_error) => format!("{error}; {cleanup_error}"),
    };
    if let Some(writer) = transcript {
        let _ = writer.record_terminal("failed", Some(&error)).await;
    }
    // Publish after retained rows are accepted and before the terminal event.
    // Every failed exit, including pre-query failures, shares this boundary.
    // Initial instruction loading precedes the writer and history seeding. A
    // cold resume still owns its already-retained transcript at that boundary.
    let hook_history = if transcript.is_none() && history.is_empty() {
        ctx.resumed_history.as_deref().unwrap_or(history)
    } else {
        history
    };
    publish_prompt_hook_transcript(ctx, hook_history, hook_usage, hook_model_selection);
    let _ = out_tx
        .send(SubagentEvent::Failed {
            agent_id,
            error,
            cumulative_usage,
        })
        .await;
}

async fn emit_killed(
    ctx: &SubagentContext,
    out_tx: &mpsc::Sender<SubagentEvent>,
    transcript: Option<&crate::transcript::AgentTranscriptWriter>,
    history: &[lingxi_core::types::ConversationMessage],
    written: &mut usize,
    agent_id: AgentId,
    hook_usage: &llm_runtime::ExecutionUsage,
    hook_model_selection: &hooks::HookModelSelection,
) {
    // Cancellation can arrive while the in-flight API future still borrows
    // model history immutably. The run is terminal, so persist a private copy;
    // the serialized transcript still receives accepted edits without trying
    // to mutably borrow history before that future is dropped.
    let mut terminal_history = history.to_vec();
    flush_transcript(transcript, &mut terminal_history, written).await;
    emit_transcript_snapshot(out_tx, agent_id, &terminal_history).await;
    if !cleanup_before_terminal(
        ctx,
        out_tx,
        transcript,
        llm_runtime::ExecutionUsage::default(),
        &terminal_history,
        hook_usage,
        hook_model_selection,
    )
    .await
    {
        return;
    }
    if let Some(writer) = transcript {
        let _ = writer.record_terminal("cancelled", None).await;
    }
    let _ = out_tx.send(SubagentEvent::Killed { agent_id }).await;
}

async fn emit_transcript_snapshot(
    out_tx: &mpsc::Sender<SubagentEvent>,
    agent_id: AgentId,
    messages: &[lingxi_core::types::ConversationMessage],
) {
    if messages.is_empty() {
        return;
    }
    let _ = out_tx
        .send(SubagentEvent::TranscriptSnapshot {
            agent_id,
            messages: messages.to_vec(),
        })
        .await;
}

/// Returns the companion note suffix (`yyo` in the binary, `nke` set) appended
/// to the allow-list-refusal error when a subagent tries to call a tool from
/// the "external companion" set that has been stripped from its pool.
///
/// Binary anchor: `function yyo(e,t,n,r)` at 203453056 in v2.1.186.
/// Trigger branch: `if (n && o && nke.has(o.name)) return …` where `n` = inside
/// a subagent, `o` = the resolved tool, `nke = HDd("external")`.
///
/// `HDd("external")` at 198067186:
/// ```text
/// new Set([lW, iO, qz, Qp, brt, tke, ...(e!=="ant"?[SI]:[]), Mh])
/// ```
/// Resolved (confirmed from binary):
/// - `lW`  = `"TaskOutput"`
/// - `iO`  = `"ExitPlanMode"`
/// - `qz`  = `"EnterPlanMode"`
/// - `Qp`  = `"AskUserQuestion"`
/// - `brt` = `"ConnectGitHub"`
/// - `tke` = `"WaitForMcpServers"`
/// - `Mh`  = `"ScheduleWakeup"`
///
/// All subagent runners are inside a subagent by definition (`n` = true).
/// Workflow availability follows the resolved tool catalog and permissions.
///
/// Returns `Some(note_suffix)` when the tool is in the `nke` set, `None`
/// otherwise. The note starts with `. ` to append to an in-progress sentence.
fn companion_note_for_disallowed_tool(tool_name: &str) -> Option<String> {
    // Tools unavailable in every subagent, regardless of product identity:
    const NKE_BASE: &[&str] = &[
        "TaskOutput",
        "ExitPlanMode",
        "EnterPlanMode",
        "AskUserQuestion",
        "ConnectGitHub",
        "WaitForMcpServers",
        "ScheduleWakeup",
    ];
    let in_nke = NKE_BASE.contains(&tool_name);
    if in_nke {
        // Binary §7 verbatim (leading `. ` — appended to an in-progress sentence):
        // `. ${toolName} is not available inside subagents. Complete the task with
        //  the tools provided and return findings to the orchestrator.`
        Some(format!(
            ". {tool_name} is not available inside subagents. Complete the task with the tools provided and return findings to the orchestrator."
        ))
    } else {
        None
    }
}

struct PreparedNestedToolContext {
    state: lingxi_core::host::tool_invoker::ToolInvocationContextState,
    model: String,
    model_profile: Option<String>,
    model_selected: bool,
}

/// Prepare one batch's modifiers in tool-use order, resolving each model
/// against the preceding route before publishing any state or transcript row.
fn apply_nested_tool_context_modifiers(
    state: &Option<lingxi_core::host::tool_invoker::ToolInvocationContextState>,
    observed_states: Vec<lingxi_core::host::tool_invoker::ToolInvocationContextState>,
    modifiers: Vec<lingxi_core::host::tool_invoker::ToolInvocationContextModifier>,
    current_model: &str,
    current_profile: Option<&str>,
    provider: Option<&dyn crate::model_resolution::ModelResolutionContextProvider>,
) -> Result<PreparedNestedToolContext, String> {
    let base = observed_states
        .first()
        .cloned()
        .or_else(|| state.clone())
        .ok_or_else(|| {
            "Nested tool returned a context modifier without a concrete ToolUseContext snapshot"
                .to_string()
        })?;
    let mut context = base
        .downcast_arc::<tool_api::context::ToolUseContext>()
        .map_err(|error| format!("Nested tool context state is invalid: {error}"))?
        .as_ref()
        .clone();
    let mut selected_model = current_model.to_string();
    let mut selected_profile = current_profile.map(str::to_string);
    let mut model_selected = false;
    for modifier in modifiers {
        context = modifier
            .apply::<tool_api::context::ToolUseContext>(context)
            .map_err(|error| {
                format!("Nested tool context modifier could not be applied: {error}")
            })?;
        if context.options.main_loop_model != selected_model
            || context.options.model_profile != selected_profile
        {
            model_selected = true;
            let provider = provider.ok_or_else(|| {
                "Nested tool model change requires a configured model route resolver".to_string()
            })?;
            let parent = provider
                .context_for_route(&selected_model, selected_profile.as_deref())
                .map_err(|error| error.to_string())?;
            let selection = crate::model_resolution::resolve_skill_model_selection(
                &context.options.main_loop_model,
                context.options.model_profile.as_deref(),
                &parent,
                provider,
            )
            .map_err(|error| error.to_string())?;
            context.options.main_loop_model = selection.model;
            context.options.model_profile = selection.model_profile;
            selected_model.clone_from(&context.options.main_loop_model);
            selected_profile.clone_from(&context.options.model_profile);
        }
    }
    Ok(PreparedNestedToolContext {
        model: context.options.main_loop_model.clone(),
        model_profile: context.options.model_profile.clone(),
        model_selected,
        state: lingxi_core::host::tool_invoker::ToolInvocationContextState::new(
            std::sync::Arc::new(context),
        ),
    })
}

/// Real multi-turn agentic loop.
///
/// Imperative — mirrors `orchestrator::turn_loop::execute_one_turn`: call the
/// model, append the assistant turn, dispatch `tool_use` blocks through the
/// inherited [`lingxi_core::host::ToolInvoker`], feed results back as a user message,
/// and repeat. Each model round-trip goes over the streaming seam
/// ([`crate::api::SubagentApiClient::stream`] drained through
/// `llm_runtime::stream_accumulator::accumulate_stream_salvaging`) and races a `UserExit` /
/// `UserInterrupt` on `event_rx` via [`tokio::select!`]; a termination event
/// aborts the loop and surfaces [`SubagentEvent::Killed`].
#[allow(
    clippy::too_many_lines,
    reason = "imperative multi-turn agentic loop — splitting the turn body hurts readability"
)]
async fn run_subagent_loop(
    mut ctx: SubagentContext,
    mut event_rx: mpsc::Receiver<lingxi_core::Event>,
    out_tx: mpsc::Sender<SubagentEvent>,
    live_hook_transcript: &mut hooks::PromptHookTranscript,
    live_hook_model_selection: &mut hooks::registry::HookModelSelection,
) {
    use lingxi_core::types::{ContentBlock, ConversationMessage, MessageId};

    let agent_id = ctx.agent_id;
    let api_client = ctx
        .api_client
        .clone()
        .expect("run_subagent_loop requires an api_client");
    // Inherited budget enforcer (cloned Option<Arc> — cheap refcount bump).
    // `Some` consults the parent's cumulative cost once per turn; `None`
    // disables enforcement (legacy/test contexts).
    let budget = ctx.budget.clone();
    let mut model = resolve_model(&ctx);
    let mut model_profile = ctx.model_profile.clone();
    // Provider routing may move to an admitted refusal-serving model. Child
    // tools keep the user's logical model/profile unless a tool context
    // modifier explicitly changes them.
    let mut logical_tool_model = model.clone();
    let mut logical_tool_model_profile = model_profile.clone();
    // Nested ToolUseContext is owned by this agent run. Each batch passes the
    // same snapshot into every tool invocation; selected one-shot modifiers
    // fold over the concrete snapshot only after the whole tool-result batch.
    let mut tool_context_state: Option<
        lingxi_core::host::tool_invoker::ToolInvocationContextState,
    > = None;
    // Per-run refusal cascade. claude-code's subagents share the main thread's
    // because they share its query generator; here the loops are separate, so
    // each run walks its own chain (handed down on the context).
    let mut refusal_cascade = lingxi_core::host::refusal_driver::RefusalCascadeState::default();
    let system: Option<String> = ctx
        .rendered_system_prompt
        .as_ref()
        .map(std::string::ToString::to_string);
    // Wire tool definitions advertised to the model on every round-trip (empty
    // when the spawner wired none). Cloned per round-trip below.
    //
    // Structured output (claude-code workflow `agent({schema})`): when a schema
    // was requested, inject a synthetic `StructuredOutput` tool whose
    // `input_schema` IS the schema; its tool input is captured below as the
    // run's result. `force_structured_tool` drives validation/capture/retry-cap
    // (unchanged) — see `force_tool_choice_for_api` below for what actually
    // gets sent on the wire as `tool_choice`.
    let mut tool_schemas = ctx.tool_schemas.clone();
    let force_structured_tool: Option<&'static str> = if let Some(schema_str) = &ctx.schema {
        let input_schema: serde_json::Value = serde_json::from_str(schema_str)
            .unwrap_or_else(|_| serde_json::json!({ "type": "object" }));
        // The normal registry exposes a permissive StructuredOutput tool. A
        // workflow schema run must replace it with this invocation's schema,
        // not append a second declaration with the same provider-facing name.
        tool_schemas.retain(|tool| {
            tool.get("name").and_then(serde_json::Value::as_str) != Some("StructuredOutput")
        });
        tool_schemas.push(lingxi_core::types::utf16_json::Utf16JsonProjection::plain(serde_json::json!({
            "name": "StructuredOutput",
            "description":
                "Return the final result as a single structured object matching the required schema.",
            "input_schema": input_schema,
        })));
        Some("StructuredOutput")
    } else {
        None
    };
    // P0-1 (2026-09-02): forcing `tool_choice` to StructuredOutput on EVERY
    // round-trip made a schema subagent unable to call any other advertised
    // tool for its whole run — verified against the claude-code 2.1.258
    // oracle binary (`~/.local/share/claude/versions/2.1.258`), whose shared
    // subagent turn loop hardcodes `toolChoice: void 0` on its single
    // `callModel` call site (offset ~164713374) regardless of
    // `requiresStructuredOutput`, and instead enforces StructuredOutput
    // purely by injecting an in-conversation nudge message once per turn the
    // model ends without a valid captured output (offset ~164629006, sentinel
    // `"[structured-output-enforce]"` at 161464131) — never by restricting
    // the tool_choice wire param. LingXi mirrors that: only pin `tool_choice`
    // when StructuredOutput is the ONLY tool being advertised (there is
    // nothing else the model could usefully call, so forcing changes
    // nothing observable and just removes one avoidable no-tool-called
    // round-trip); whenever other tools are present the model chooses
    // freely every round, and the existing SubagentStop nudge path below
    // (`:1949` as of this change) is the sole enforcement mechanism, exactly
    // as the oracle does. See `<scratchpad>/WP3-oracle.txt` for the full
    // extracted evidence.
    //
    // 🚨 CORRECTING THE RECORD: commit `1c9cbd695`'s message says `tool_choice`
    // is "forced after the nudge". That is WRONG and was never what shipped.
    // `force_tool_choice_for_api` is computed ONCE, HERE, before the turn loop
    // is entered, from the advertised-tool count alone; the nudge path below
    // deliberately does NOT re-arm it, and there is no other assignment to this
    // binding anywhere in the loop (it is a `let`, not a `let mut`). Under
    // `StructuredOutputMode::Forced` (the byte-parity default this comment was
    // originally written about), a run that advertises tools besides
    // StructuredOutput sends `tool_choice = None` on every round-trip, before
    // AND after the nudge — this binding alone is the whole story there.
    //
    // Round-3 review items 3/7/9/20 fix: under `StructuredOutputMode::WhenDone`
    // (Fusion panels), this binding is NOT the whole story any more — the wire
    // call below additionally pins on the run's designated last-chance turn
    // (final turn, or two consecutive idle turns) even when `tool_schemas.len()
    // != 1`, because that turn is the one place WhenDone's "forced only on the
    // last turn" contract (`platform-api/src/subagent_spawn.rs`) cannot be
    // honored by this binding's single-tool gate alone — see the wire call's
    // own comment below for the mode-aware condition.
    let force_tool_choice_for_api: Option<&'static str> =
        if force_structured_tool.is_some() && tool_schemas.len() == 1 {
            force_structured_tool
        } else {
            None
        };
    // Captured when the model calls the synthetic `StructuredOutput` tool with an
    // input that VALIDATES against the schema — that input becomes the run's
    // result, and the loop terminates.
    let mut structured_result: Option<serde_json::Value> = None;
    // claude-code `agent({schema})` run-scoped counters: `kn` (failed
    // StructuredOutput validations) and `ft` (in-conversation nudges injected
    // when the model ends a turn without calling StructuredOutput). `Yr` is the
    // retry cap (`MAX_STRUCTURED_OUTPUT_RETRIES ?? 5`).
    let mut structured_failed_count: u32 = 0;
    let mut structured_nudge_count: u32 = 0;
    let mut structured_truncation_retries: u32 = 0;
    // Separate run-scoped cap: parsing failures never reach schema validation.
    let mut structured_parse_retries: u32 = 0;
    let structured_parse_retry_cap = ctx.structured_output_parse_retries.min(2);
    let structured_retry_cap = structured_output_retry_cap();
    // `StructuredOutputMode::WhenDone` (Fusion panels — WP2a item 1): counts
    // CONSECUTIVE turns that produced no tool_use block at all (not even a
    // `StructuredOutput` call). Reset to 0 the moment any tool is called.
    // Reaching 2 forces `StructuredOutput` on the very next turn, same as
    // being on the run's last turn — see `force_this_turn` below.
    let mut whendone_idle_turns: u32 = 0;
    let force_every_turn = matches!(
        ctx.structured_output_mode,
        lingxi_core::host::subagent_spawn::StructuredOutputMode::Forced
    );
    // Per-agent tool allow-list enforced at dispatch (see below). Empty = no
    // restriction (the resolver has not filtered, e.g. `AgentToolPolicy::All`).
    // This is the dispatch-time guard the advertised set relies on: the
    // inherited `RegistryToolInvoker` itself does NOT check policy.
    let mut allowed_tools = ctx.allowed_tools.clone();

    // Recovered rows already belong to the child, even if initial instruction
    // loading fails. Seed the live hook view before startup without publishing
    // transcript rows or duplicating the later successful resume prefix.
    if let Some(resumed) = &ctx.resumed_history {
        live_hook_transcript
            .messages
            .extend(resumed.iter().cloned());
    }

    ctx.instruction_context = match crate::instructions::resolve(&ctx).await {
        Ok(context) => context,
        Err(error) => {
            // Native Lv's initial Gv Promise rejects before startup history,
            // transcript writes or a reporting run exists. The dispatcher
            // still performs its usual hook, diagnostic and restore cleanup.
            let mut transcript_written = 0;
            emit_failed(
                &ctx,
                &out_tx,
                None,
                &mut [],
                &mut transcript_written,
                agent_id,
                error,
                llm_runtime::ExecutionUsage::default(),
                &llm_runtime::ExecutionUsage::default(),
                live_hook_model_selection,
            )
            .await;
            return;
        }
    };

    // Seed history. A RESTORED agent is seeded from its persisted transcript
    // and nothing else: fork context, prompt and preload are all already inside
    // that history (they were persisted on the original run), so re-adding them
    // would duplicate context the agent has seen and re-fire `SubagentStart`
    // for a run that began in another process.
    let history = &mut live_hook_transcript.messages;
    if ctx.resumed_history.is_some() {
        if let Some(scope) = llm_runtime::thinking_scope::current() {
            crate::transcript::restore_thinking_recovery(history, &scope);
        }
    } else {
        // Keep the engine-owned mobile snapshot at the fixed first-message
        // position, before the variable task/fork prompt. That preserves the
        // provider-cacheable prefix while leaving custom/fork SYSTEM bytes
        // untouched. A resumed transcript already contains this message.
        if let Some(reminder) = &ctx.mobile_runtime_environment_reminder {
            history.push(ConversationMessage::user_meta(
                MessageId::new(),
                reminder.to_string(),
            ));
        }
        if let Some(reminder) = &ctx.mobile_runtime_workspace_reminder {
            history.push(ConversationMessage::user_meta(
                MessageId::new(),
                reminder.to_string(),
            ));
        }

        // Fork-context prefix (if any) followed by the prompt.
        if let Some(fork) = &ctx.fork_context_messages {
            history.extend(fork.iter().cloned());
        }
        history.extend(ctx.prompt_messages.iter().cloned());

        // G4 + G5 (claude runAgent.ts:530-646): SubagentStart-hook
        // additionalContext injection then frontmatter skills preload, appended
        // to the child's INITIAL messages before the first turn. Strict no-op
        // (no extra messages) when neither `hook_executor` nor `skill_loader` is
        // wired, so legacy / test builds keep a byte-identical history.
        // (Frontmatter-hook registration is done at the `run_subagent`
        // dispatcher for guaranteed cleanup.)
        //
        // AG-6, verified at the 2.1.270 oracle 2026-09-14 — NOT a gap here, and
        // this line is why. 2.1.267 shipped "Fixed agent teammates and resumed
        // subagents moving SubagentStart hook context and preloaded skills out
        // of the prompt prefix on later turns, which broke prompt-cache reuse",
        // with a sibling for in-process teammates re-sending their first-turn
        // tool and skill announcements on the second turn. Both describe
        // content that belongs in the STABLE PREFIX being re-emitted into the
        // varying tail. Neither can happen here: the preload is seeded ONCE,
        // into the initial history, inside the branch a resume does not take —
        // `SubagentContext::resumed_history` REPLACES prompt + fork context +
        // preload rather than prefixing them, because all three are already in
        // the recovered history. The 2.1.265 half ("record the system prompt
        // and tool definitions once instead of re-rendering them") is
        // `SubagentContext::rendered_system_prompt`, and the resumed tool list
        // is pinned by the same SHA-256 definition snapshot AG-5 turns on.
        // Re-open this only with a measured prefix diff across two turns.
        match build_preload_messages(&ctx).await {
            Ok(messages) => history.extend(messages),
            Err(error) => {
                let mut transcript_written = 0;
                emit_failed(
                    &ctx,
                    &out_tx,
                    None,
                    history,
                    &mut transcript_written,
                    agent_id,
                    format!("Could not preload agent skills: {error}"),
                    llm_runtime::ExecutionUsage::default(),
                    &llm_runtime::ExecutionUsage::default(),
                    live_hook_model_selection,
                )
                .await;
                return;
            }
        }
    }

    let mut child_attachments = ChildPromptAttachments::default();
    if ctx.resumed_history.is_some() {
        child_attachments.restore_from_history(history);
    }

    // Per-agent transcript. The watermark makes each retained row pass through
    // `session.append` once, immediately before its next provider snapshot (and
    // at the terminal boundary for rows that cannot affect another request).
    // This keeps accepted edits in model history and JSONL without adding
    // persistence calls at every history.push site.
    let transcript_mod_cwd = ctx.cwd.clone().unwrap_or_else(|| ctx.hook_cwd.clone());
    let transcript_path = ctx
        .transcript_subdir
        .join(format!("agent-{agent_id}.jsonl"));
    let transcript = match ctx.transcript_fs.clone() {
        Some(fs) => Some(crate::transcript::AgentTranscriptWriter::new(
            transcript_path.clone(),
            agent_id,
            fs,
        )),
        None if ctx.hook_executor.is_some() => Some(
            crate::transcript::AgentTranscriptWriter::without_persistence(
                transcript_path,
                agent_id,
            ),
        ),
        None => None,
    }
    .map(|writer| {
        writer
            .with_metadata(
                ctx.agent_name.clone(),
                Some(ctx.agent_definition.agent_type.clone()),
                Some(resolve_model(&ctx)),
                ctx.model_profile.clone(),
            )
            .with_correlation_id(ctx.correlation_id.clone())
            .with_mod_append(
                ctx.hook_executor.clone(),
                transcript_mod_cwd,
                Some(resolve_model(&ctx)),
            )
    });
    crate::transcript::set_message_row_index_writer(
        ctx.transcript_fs.as_ref().and(transcript.as_ref()),
    );
    if ctx.transcript_fs.is_some() {
        if let Some(writer) = transcript.as_ref() {
            match writer.read_next_message_index().await {
                Ok(next_message_index) => {
                    crate::transcript::advance_message_row_index(next_message_index);
                }
                Err(error) => tracing::warn!(
                    %error,
                    "could not restore session-agent message index high-water"
                ),
            }
        }
    }
    if let (Some(writer), Some(scope)) =
        (transcript.as_ref(), llm_runtime::thinking_scope::current())
    {
        let writer = writer.clone();
        scope.set_recorder(std::sync::Arc::new(move |messages| {
            let writer = writer.clone();
            Box::pin(async move {
                if let Err(error) = writer.record_thinking_recovery(messages).await {
                    tracing::warn!(%error, "could not persist worker thinking recovery");
                }
            })
        }));
    }
    // Mark a child as live before its first round-trip. A persistent child may
    // later transition to `idle` without terminating; the lifecycle records
    // make that distinction observable to mobile clients tailing the file.
    if let Some(writer) = transcript.as_ref() {
        let _ = writer.record_terminal("running", None).await;
    }
    // A RESTORED run starts with its transcript already on disk, so its
    // watermark starts past the recovered messages — otherwise the first flush
    // would append the whole conversation a second time. A fresh run starts at
    // 0: its seeded prompt and preload are new and must be persisted.
    let mut transcript_written: usize = if ctx.resumed_history.is_some() {
        history.len()
    } else {
        0
    };

    // A fresh child's task is already real input before the provider answers.
    // Persist that seed now, rather than at the first turn boundary, so a
    // stalled first request still has an inspectable transcript. Publish the
    // caller-supplied prompt through the same typed message stream as later
    // turns; mobile clients can then render it immediately even when a
    // transcript load races this first append. Restored agents skip both paths
    // because their seed is already durable and replayable.
    if ctx.resumed_history.is_none() {
        flush_transcript(transcript.as_ref(), history, &mut transcript_written).await;
        for message in &ctx.prompt_messages {
            emit_message(&out_tx, agent_id, message).await;
        }
    }

    // A restored human-owned turn drains its typed inbox before the first API
    // request. The persisted-history watermark above excludes these new inputs.
    if ctx.resumed_history.is_some() && fold_task_notifications(&ctx, history).await {
        flush_transcript(transcript.as_ref(), history, &mut transcript_written).await;
    }

    let max_turns = ctx.agent_definition.max_turns;

    // Once the cancellation channel closes, no UserExit / UserInterrupt can
    // ever arrive, so we stop racing it and await the API future directly
    // (racing a perpetually-ready `recv() -> None` arm would busy-loop).
    let mut event_channel_open = true;

    // Result-level rollups carried onto the terminal `Completed` event so the
    // spawner can populate claude's `totalDurationMs` / `totalToolUseCount`
    // without re-deriving them. `run_start` spans the whole run (every turn-set
    // in persistent mode); `total_tool_use_count` accumulates `tool_uses.len()`
    // across turns. `last_usage` keeps the FINAL response usage (claude
    // `getTokenCountFromUsage` reads the LAST assistant usage, not a sum), so it
    // is overwritten — never accumulated — each turn.
    let run_start = std::time::Instant::now();
    let mut total_tool_use_count: u64 = 0;
    let local_agent_tool_uses = std::sync::Arc::new(std::sync::Mutex::new(
        lingxi_core::host::tool_use_lifecycle::ToolUseLifecycleTracker::default(),
    ));
    let mut last_usage = llm_runtime::ExecutionUsage::default();
    let mut cumulative_usage = llm_runtime::ExecutionUsage::default();
    let mut usage_complete = true;
    // claude `agentMessages.length` — assistant turns produced across the run
    // (one per round-trip) — and the FINAL turn's provider request id (claude
    // `lastAssistantMessage.requestId`), both surfaced on the terminal
    // `Completed` event so the spawner can emit `tengu_agent_tool_completed` /
    // `tengu_cache_eviction_hint`.
    let mut assistant_message_count: u64 = 0;
    let mut last_request_id: Option<String> = None;
    let mut notification_changes = ctx
        .task_registry
        .as_ref()
        .and_then(|registry| registry.subscribe_task_notifications());
    let mut pending_peer_messages = Vec::new();
    if let Some(participant) = ctx.handback_restore_start.take() {
        participant.arrive_and_wait().await;
    }
    let mod_executor = ctx.hook_executor.clone();
    let mod_cwd = ctx.cwd.clone().unwrap_or_else(|| ctx.hook_cwd.clone());

    // Outer loop: one iteration per turn-set. In non-persistent mode the
    // turn-set runs exactly once (we `return` after it). In persistent mode the
    // runner parks at the bottom awaiting the next inbound `UserMessage` and
    // loops back here to run the next turn-set, retaining `history` across
    // turn-sets (matching the TS teammate's accumulated transcript). `max_turns`
    // is per-turn-set: the `_turn` counter re-zeroes each outer iteration, so
    // every injected message gets a fresh budget.
    loop {
        if let Some(handback) = &ctx.handback {
            let parent_mode = handback.trusted_parent_permission_mode.or_else(|| {
                ctx.tool_invoker
                    .as_ref()
                    .and_then(|invoker| invoker.permission_mode())
                    .as_deref()
                    .and_then(crate::permission_mode::parse_wire_mode)
            });
            let child_override = parent_mode
                .and_then(|parent| {
                    crate::permission_mode::effective_child_mode(
                        None,
                        parent,
                        ctx.agent_definition.permission_mode,
                        handback.spawn_bypass_gates,
                        &mut |message| tracing::warn!("{message}"),
                    )
                })
                .or(handback.trusted_parent_permission_mode);
            ctx.permission_mode_override =
                child_override.map(|mode| crate::permission_mode::wire_mode_str(mode).to_string());
            let child_mode = child_override.or(parent_mode);
            let active = handback
                .begin(
                    handback.eligible
                        && parent_mode == Some(permission::PermissionMode::Auto)
                        && child_mode == Some(permission::PermissionMode::Auto),
                )
                .await;
            configure_handback_run(
                &ctx,
                history,
                &mut tool_schemas,
                &mut allowed_tools,
                active,
                &out_tx,
            )
            .await;
        } else if lingxi_core::host::handback::latest_handback_instruction(history)
            == Some(lingxi_core::host::handback::HandbackInstruction::Reminder)
        {
            configure_handback_run(
                &ctx,
                history,
                &mut tool_schemas,
                &mut allowed_tools,
                false,
                &out_tx,
            )
            .await;
        }
        let mod_host = match mod_executor.as_ref() {
            Some(executor) => executor.mod_host().await,
            None => None,
        };
        let mod_turn_id = mod_host.as_ref().map(|_| uuid::Uuid::new_v4().to_string());
        let mut mod_turn: Option<Box<ChildTurnComplete>> = None;
        let mut mod_started = false;
        // Set to `true` when the inner turn loop hits a clean terminal stop (it has
        // already emitted its `Completed`). Stays `false` if the loop instead falls
        // through by exhausting `max_turns`, which needs the max-turns `Completed`.
        let mut terminated_cleanly = false;
        let mut foreground_parked = false;
        'model_turns: for turn_idx in 0..max_turns {
            loop {
                drain_peer_messages(
                    &ctx,
                    &mut pending_peer_messages,
                    history,
                    transcript.as_ref(),
                    &mut transcript_written,
                    &out_tx,
                )
                .await;
                if pending_peer_messages.is_empty() {
                    break;
                }
                // An admitted peer report must enter its typed transcript row
                // before the next API query can consume it. Keep storage retry
                // outside the turn budget and remain interruptible.
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {}
                    event = event_rx.recv(), if event_channel_open => match event {
                        Some(lingxi_core::Event::UserExit | lingxi_core::Event::UserInterrupt) => {
                            emit_killed(&ctx, &out_tx, transcript.as_ref(), history, &mut transcript_written, agent_id,
                                &last_usage,
                                live_hook_model_selection,
                            ).await;
                            return;
                        }
                        Some(lingxi_core::Event::PeerMessage { envelope }) => pending_peer_messages.push(envelope),
                        Some(lingxi_core::Event::UserMessage { content, .. }) => history.push(ConversationMessage::user(MessageId::new(), content)),
                        Some(lingxi_core::Event::UserMessageJsUtf16 { content, utf16_code_units, .. }) => {
                            history.push(ExactHookText { display: content, utf16_code_units }
                                .to_conversation_message(MessageId::new(), false));
                        }
                        Some(_) => {}
                        None => event_channel_open = false,
                    }
                }
            }
            fold_task_notifications(&ctx, history).await;
            announce_instruction_context(
                &mut ctx,
                history,
                transcript.as_ref(),
                &mut transcript_written,
                &out_tx,
            )
            .await;
            // Retained child rows pass through `session.append` before the next
            // provider snapshot is built, matching the native loop's keep point.
            flush_transcript(transcript.as_ref(), history, &mut transcript_written).await;
            // Per-turn budget gate. This is the achievable analog of
            // `QueryEngine.ts`'s `error_max_budget_usd` loop-terminator, built on
            // the same frozen seam `AgentTool`'s pre-spawn gate uses
            // (tools/agent/src/agent.rs:321): charge 0 to ask "is cumulative cost
            // already over the configured limit?" without pricing tokens (the
            // agent crate can't depend on lingxi-cost; the enforcer tracks cost
            // globally, exactly like TS reading `getTotalCost()`).
            //
            // Placement remains at the TOP of the turn (stop before spending)
            // rather than TS's post-message check, so an already-over-budget
            // child makes zero additional round-trips. The denial string uses
            // the same current/maximum bytes as Claude Code 2.1.217's background
            // task budget halt when the configured ceiling is available.
            if let Some(b) = &budget {
                if let Err(lingxi_core::host::budget::BudgetError::Exceeded { current_nano_usd }) =
                    b.check_and_charge(0).await
                {
                    if let Some(token) = &ctx.agent_spawn_token {
                        token.killed(lingxi_core::host::agent_statistics::AgentKillReason::System);
                    }
                    // Stop with the 2.1.217 background-agent budget string.
                    #[allow(clippy::cast_precision_loss)]
                    let dollars = current_nano_usd as f64 / 1_000_000_000.0;
                    let error = b.max_session_nano_usd().map_or_else(
                        || format!("Budget exceeded (${dollars:.2}); stopped."),
                        |limit_nano_usd| {
                            let whole = limit_nano_usd / 1_000_000_000;
                            let fractional = limit_nano_usd % 1_000_000_000;
                            let maximum = if fractional == 0 {
                                whole.to_string()
                            } else {
                                let fraction = format!("{fractional:09}");
                                format!("{whole}.{}", fraction.trim_end_matches('0'))
                            };
                            format!(
                                "Budget limit reached (${dollars:.2} of ${maximum}); stopping background agents."
                            )
                        },
                    );
                    emit_failed(
                        &ctx,
                        &out_tx,
                        transcript.as_ref(),
                        history,
                        &mut transcript_written,
                        agent_id,
                        error,
                        cumulative_usage.clone(),
                        &last_usage,
                        live_hook_model_selection,
                    )
                    .await;
                    return;
                }
                // `Ok` and `BudgetError::Internal` fall through to the round-trip:
                // TS has no analog branch that errors the loop on an internal
                // budget condition, so an internal failure is non-fatal here.
            }

            if !mod_started {
                if let (Some(host), Some(turn_id)) = (mod_host.as_ref(), mod_turn_id.as_ref()) {
                    if host.has_event("turn.complete") {
                        mod_turn = Some(Box::new(ChildTurnComplete::new(
                            Some(host.clone()),
                            mod_cwd.clone(),
                            agent_id,
                            turn_id.clone(),
                        )));
                    }
                    if host.has_event("turn.start") {
                        let text = child_turn_start_text(history);
                        fire_child_turn_start(host, &mod_cwd, &text, turn_id).await;
                    }
                }
                mod_started = true;
            }

            // Race the model round-trip against a user-termination event. A
            // UserExit / UserInterrupt on event_rx aborts the loop -> Killed.
            // Any other inbound event is ignored (the loop is self-driving) and
            // we re-issue the round-trip on the next iteration.
            //
            // The round-trip goes over the STREAMING seam: open the SSE stream and
            // drain its decoded history events through `accumulate_stream`.
            // Dropping this future on the termination arm cancels the stream.
            // Per-request thinking-effort (claude-code `me.effort`): the subagent's
            // resolved effort (its definition's, possibly overridden by a workflow
            // `agent({effort})` opt at spawn) → `output_config.effort`.
            let effort_wire = ctx
                .agent_definition
                .effort
                .as_ref()
                .map(crate::definition::AgentEffort::to_wire);
            // `StructuredOutputMode::WhenDone` (Fusion panels, WP2a item 1): let
            // the model use its other tools with normal (auto) `tool_choice`
            // while turns remain; force `StructuredOutput` only on the run's
            // LAST turn, or once it has produced two consecutive turns with no
            // tool call at all (`whendone_idle_turns`, updated after the
            // round-trip below). `StructuredOutputMode::Forced` (the default,
            // byte-parity with pre-WP2a behavior) forces every turn
            // unconditionally. Computed once per turn (stable across any
            // watchdog retry of the SAME turn below).
            let is_last_turn = turn_idx + 1 == max_turns;
            let force_this_turn = force_structured_tool.is_some()
                && (force_every_turn || is_last_turn || whendone_idle_turns >= 2);
            // (M9 cc2.1.198 wake-on-message) Captured by the wake arm in the
            // select below and appended to `history` HERE, before the next
            // `api_call` is built, because the in-flight future immutably
            // borrows `history` inside the select.
            let mut wake_message: Option<ExactHookText> = None;
            let watchdog = api_client.workflow_query_watchdog();
            let mut watchdog_retry_count = 0_u32;
            let mut retry_scope = llm_runtime::model::retry_scope::ModelCallRetryScope::default();
            let model_attempt = match ctx
                .model_attempt
                .as_ref()
                .map(|context| context.fresh_call())
                .transpose()
            {
                Ok(context) => context,
                Err(error) => {
                    emit_failed(
                        &ctx,
                        &out_tx,
                        transcript.as_ref(),
                        history,
                        &mut transcript_written,
                        agent_id,
                        error.to_string(),
                        cumulative_usage.clone(),
                        &last_usage,
                        live_hook_model_selection,
                    )
                    .await;
                    return;
                }
            };
            let retry_response_body = watchdog.is_some_and(|policy| policy.retry_response_body);
            let defer_local_work = retry_response_body
                || (force_structured_tool.is_some() && structured_parse_retry_cap > 0);
            let mod_step_active = mod_host
                .as_ref()
                .is_some_and(|host| host.has_event("turn.step"));
            let physical_responses = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            if mod_step_active {
                if let Some(turn) = mod_turn.as_mut() {
                    turn.attach_physical(physical_responses.clone());
                }
            }
            let mut mod_physical_usage = llm_runtime::ExecutionUsage::default();
            let (response, local_only_response) = 'response_attempts: loop {
                // Only the opened body is retried here; ApiService already owns
                // connect-phase retries. Any server-side content revokes replay
                // even when its block never closes and cannot be salvaged.
                let stream_opened = std::sync::atomic::AtomicBool::new(false);
                let retryable_body = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
                let live_output_started =
                    std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let tool_effects_started =
                    std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                // (M9) A wake message injected below rides into the next
                // round-trip as a user turn. The runner is already active here.
                if let Some(content) = wake_message.take() {
                    history.push(content.to_conversation_message(MessageId::new(), false));
                    // A new human message starts a new logical request. Only
                    // retries of the unchanged request retain the old budget.
                    retry_scope = llm_runtime::model::retry_scope::ModelCallRetryScope::default();
                    flush_transcript(transcript.as_ref(), history, &mut transcript_written).await;
                }
                maybe_emit_near_limit_wrap_up(
                    history,
                    &ctx,
                    api_client.as_ref(),
                    &out_tx,
                    agent_id,
                )
                .await;
                flush_transcript(transcript.as_ref(), history, &mut transcript_written).await;
                let api_call = async {
                    let current_model = model.clone();
                    tracing::debug!(
                        agent_id = %agent_id,
                        model = %current_model,
                        event = "query_started"
                    );
                    // Provider routing (dual-LLM dual-PROVIDER): thread the
                    // per-spawn `model_profile` as the api client's `profile` so the
                    // round-trip targets the candidate's resolved provider. `None`
                    // ⇒ default/unscoped resolution (legacy). The `_in` variants
                    // default to the profile-less methods, so a client that only
                    // implements the legacy seam is unaffected.
                    //
                    // The error carries the partial content blocks completed
                    // before the failure so the arm below can SALVAGE them (CC
                    // 2.1.207 `api_error_partial`). A connect-phase error yields no
                    // partial (empty vec); a mid-stream error yields whatever
                    // blocks were finalized.
                    let profile = model_profile.as_deref();
                    let mut model_snapshot = history.to_vec();
                    child_attachments
                        .screen(&mut model_snapshot, mod_host.as_ref(), &mod_cwd, agent_id)
                        .await;
                    let messages_for_api = match instruction_request_messages(
                        &model_snapshot,
                        &ctx.instruction_context,
                        ctx.max_input_bytes_per_turn,
                    ) {
                        Ok(messages) => messages,
                        Err(message) => {
                            return AgentStreamAttempt::failed(
                                LlmError::InvalidRequest { message },
                                Vec::new(),
                            );
                        }
                    };
                    let request_messages = messages_for_api.clone();
                    let call_opts = crate::api::SubagentApiCallOpts {
                        model_attempt: model_attempt.clone(),
                        max_output_tokens: ctx.max_output_tokens_per_turn,
                        query_source_label: ctx.query_source_label.clone(),
                    };
                    let forced_tool = (force_this_turn
                        && (force_tool_choice_for_api.is_some() || !force_every_turn))
                        .then(|| force_structured_tool.map(str::to_owned))
                        .flatten();
                    let request = crate::mod_turn_step::ChildStepRequest {
                        api: api_client.clone(),
                        model: current_model.clone(),
                        profile: profile.map(str::to_owned),
                        system: system.clone(),
                        messages: messages_for_api,
                        tools: tool_schemas.clone(),
                        effort: effort_wire.clone(),
                        forced_tool,
                        call_opts,
                        agent_spawn_provenance: ctx.agent_spawn_provenance.clone(),
                        fallback_target: lingxi_core::host::refusal_driver::FallbackTargetContext {
                            user_model: logical_tool_model.clone(),
                            turn_override: refusal_cascade.target_model().map(str::to_owned),
                            ..Default::default()
                        },
                    };
                    let open_stream = async {
                        if let Some(host) =
                            mod_host.as_ref().filter(|host| host.has_event("turn.step"))
                        {
                            let input = crate::mod_turn_step::input(
                                mod_turn_id.as_deref().unwrap_or_default(),
                                turn_idx,
                                &agent_id.to_string(),
                                &current_model,
                                effort_wire.as_ref(),
                                request.messages.len(),
                            );
                            Ok(crate::mod_turn_step::stream(
                                host.clone(),
                                input,
                                request.clone(),
                                mod_cwd.clone(),
                                physical_responses.clone(),
                            ))
                        } else {
                            request.open(&current_model, effort_wire.clone()).await
                        }
                    };
                    let stream = match await_workflow_query_phase(
                        lingxi_core::host::refusal_driver::scope_fallback_target(
                            lingxi_core::host::refusal_driver::FallbackTargetContext {
                                user_model: logical_tool_model.clone(),
                                turn_override: refusal_cascade.target_model().map(str::to_owned),
                                ..Default::default()
                            },
                            retry_scope.run(open_stream),
                        ),
                        watchdog,
                        "opening the response stream",
                    )
                    .await
                    {
                        Ok(stream) => stream,
                        Err(error) => {
                            return AgentStreamAttempt::failed(error, request_messages);
                        }
                    };
                    stream_opened.store(true, std::sync::atomic::Ordering::Relaxed);
                    tracing::debug!(
                        agent_id = %agent_id,
                        model = %current_model,
                        event = "stream_opened"
                    );
                    let stream = with_workflow_stream_watchdog(stream, watchdog);
                    let mut first_event_seen = false;
                    let observed_model = current_model.clone();
                    let stream = stream.inspect({
                        let out_tx = out_tx.clone();
                        let current_agent_id = agent_id;
                        let retryable_body = retryable_body.clone();
                        move |event| {
                            if retry_response_body {
                                let local_content = |block: &llm_runtime::ContentBlock| {
                                    matches!(
                                        block,
                                        llm_runtime::ContentBlock::Text { .. }
                                            | llm_runtime::ContentBlock::Reasoning { .. }
                                            | llm_runtime::ContentBlock::RedactedThinking { .. }
                                            | llm_runtime::ContentBlock::ToolCall { .. }
                                    )
                                };
                                let has_server_content = match event {
                                    Ok(HistoryEvent::ContentBlockStart {
                                        content_block, ..
                                    }) => !local_content(content_block),
                                    Ok(
                                        HistoryEvent::MessageStart { response }
                                        | HistoryEvent::Completed { response },
                                    ) => response.content.iter().any(|block| !local_content(block)),
                                    _ => false,
                                };
                                if has_server_content {
                                    retryable_body
                                        .store(false, std::sync::atomic::Ordering::Relaxed);
                                }
                            }
                            if first_event_seen || event.is_err() {
                                return;
                            }
                            first_event_seen = true;
                            tracing::debug!(
                                agent_id = %current_agent_id,
                                model = %observed_model,
                                event = "first_event"
                            );
                            // Preserve stream order: a detached send can arrive
                            // after the response's terminal event. This beacon is
                            // best-effort, so a synchronous try_send is sufficient.
                            let _ = out_tx.try_send(SubagentEvent::Progress {
                                agent_id: current_agent_id,
                                tool_use_count: u32::try_from(total_tool_use_count)
                                    .unwrap_or(u32::MAX),
                                token_count: 0,
                            });
                        }
                    });
                    accumulate_agent_stream_live(
                        Box::pin(stream),
                        &ctx,
                        transcript.as_ref(),
                        request_messages,
                        system.clone(),
                        current_model.clone(),
                        logical_tool_model.clone(),
                        logical_tool_model_profile.clone(),
                        tool_context_state.clone(),
                        &allowed_tools,
                        force_structured_tool.as_deref(),
                        defer_local_work,
                        &retryable_body,
                        &live_output_started,
                        &tool_effects_started,
                        local_agent_tool_uses.as_ref(),
                        &out_tx,
                    )
                    .await
                };
                let attempt_result = if !event_channel_open {
                    api_call.await
                } else if model_attempt.is_some() {
                    // Registered panel calls cannot silently abandon one wire
                    // owner and issue another for an unrelated event. Keep the
                    // same future until response or explicit user cancellation.
                    tokio::pin!(api_call);
                    loop {
                        tokio::select! {
                            biased;
                            ev = async {
                                loop {
                                    match event_rx.recv().await {
                                        Some(lingxi_core::Event::PeerMessage { envelope }) => pending_peer_messages.push(envelope),
                                        other => break other,
                                    }
                                }
                            }, if event_channel_open => {
                                match ev {
                                    Some(lingxi_core::Event::UserExit | lingxi_core::Event::UserInterrupt) => {
                                        if let Some(turn) = mod_turn.as_mut() { turn.aborted(); }
                                        emit_killed(&ctx, &out_tx, transcript.as_ref(), history,
                                            &mut transcript_written, agent_id,
                                &last_usage,
                                live_hook_model_selection,
                            ).await;
                                        return;
                                    }
                                    Some(lingxi_core::Event::UserMessage { content, .. })
                                        if ctx.persistent || ctx.task_registry.is_some() =>
                                    {
                                        wake_message = Some(ExactHookText::from_text(content));
                                        if !tool_effects_started
                                            .load(std::sync::atomic::Ordering::Relaxed)
                                        {
                                            continue 'response_attempts;
                                        }
                                    }
                                    Some(lingxi_core::Event::UserMessageJsUtf16 { content, utf16_code_units, .. })
                                        if ctx.persistent || ctx.task_registry.is_some() =>
                                    {
                                        wake_message = Some(ExactHookText { display: content, utf16_code_units });
                                        if !tool_effects_started
                                            .load(std::sync::atomic::Ordering::Relaxed)
                                        {
                                            continue 'response_attempts;
                                        }
                                    }
                                    None => event_channel_open = false,
                                    Some(_) => {}
                                }
                            }
                            response = &mut api_call => break response,
                        }
                    }
                } else {
                    tokio::pin!(api_call);
                    loop {
                        tokio::select! {
                            biased;
                            ev = async {
                                loop {
                                    match event_rx.recv().await {
                                        Some(lingxi_core::Event::PeerMessage { envelope }) => pending_peer_messages.push(envelope),
                                        other => break other,
                                    }
                                }
                            }, if event_channel_open => {
                                match ev {
                                    Some(lingxi_core::Event::UserExit | lingxi_core::Event::UserInterrupt) => {
                                        if let Some(turn) = mod_turn.as_mut() { turn.aborted(); }
                                        if let Some(executor) = &ctx.hook_executor {
                                            executor.discard_agent_prompt_transcript(ctx.hook_session_id, ctx.agent_id, ctx.subagent_stop_firer.as_ref());
                                        }
                                        emit_killed(&ctx,
                                            &out_tx,
                                            transcript.as_ref(),
                                            history,
                                            &mut transcript_written,
                                            agent_id,
                                &last_usage,
                                live_hook_model_selection,
                            ).await;
                                        return;
                                    }
                                    // A launcher message is task direction, not
                                    // cancellation of an already-running tool or
                                    // its pending permission decision. Keep this
                                    // query future alive once tool effects have started;
                                    // otherwise retry the pending model call with direction.
                                    Some(lingxi_core::Event::UserMessage { content, .. })
                                        if ctx.persistent || ctx.task_registry.is_some() =>
                                    {
                                        wake_message = Some(ExactHookText::from_text(content));
                                        if !tool_effects_started.load(std::sync::atomic::Ordering::Relaxed) {
                                            continue 'response_attempts;
                                        }
                                    }
                                    Some(lingxi_core::Event::UserMessageJsUtf16 { content, utf16_code_units, .. })
                                        if ctx.persistent || ctx.task_registry.is_some() =>
                                    {
                                        wake_message = Some(ExactHookText { display: content, utf16_code_units });
                                        if !tool_effects_started.load(std::sync::atomic::Ordering::Relaxed) {
                                            continue 'response_attempts;
                                        }
                                    }
                                    Some(_) => {}
                                    None => event_channel_open = false,
                                }
                            }
                            resp = &mut api_call => break resp,
                        }
                    }
                };

                if mod_step_active {
                    let completed = if let Some(turn) = mod_turn.as_mut() {
                        turn.drain_physical()
                    } else {
                        std::mem::take(
                            &mut *physical_responses
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner),
                        )
                    };
                    for physical in completed {
                        accumulate_usage(&mut mod_physical_usage, &physical.usage);
                        accumulate_usage(&mut cumulative_usage, &physical.usage);
                    }
                }

                if let Some(Err((_partial_blocks, error))) = &attempt_result.response {
                    let interrupted_body = retry_response_body
                        && matches!(error, LlmError::Transport { .. })
                        && stream_opened.load(std::sync::atomic::Ordering::Relaxed);
                    if attempt_result.declined_fallback.is_none()
                        && !live_output_started.load(std::sync::atomic::Ordering::Relaxed)
                        && (is_workflow_watchdog_timeout(error) || interrupted_body)
                        && (!retry_response_body
                            || retryable_body.load(std::sync::atomic::Ordering::Relaxed))
                        && model_attempt.is_none()
                        && watchdog.is_some_and(|policy| watchdog_retry_count < policy.max_retries)
                        && retry_scope.take_stream_retry()
                    {
                        // The retry is safe only while the live response yielded
                        // no host-visible row and started no local effects.
                        // Mark unavailable usage for the discarded provider body.
                        if retry_response_body {
                            usage_complete = false;
                        }
                        watchdog_retry_count = watchdog_retry_count.saturating_add(1);
                        let model_attempt = watchdog_retry_count.saturating_add(1);
                        let reason = error.to_string();
                        tracing::warn!(
                            agent_id = %agent_id,
                            attempt = model_attempt,
                            reason = %reason,
                            event = "query_retry"
                        );
                        api_client
                            .observe_workflow_query_retry(agent_id, model_attempt, reason)
                            .await;
                        continue;
                    }
                }
                break (
                    attempt_result,
                    retryable_body.load(std::sync::atomic::Ordering::Relaxed),
                );
            };

            let mut pending_attempt = response;
            let mut first_visible_response = true;
            'visible_responses: loop {
                if !first_visible_response && mod_step_active {
                    let completed = if let Some(turn) = mod_turn.as_mut() {
                        turn.drain_physical()
                    } else {
                        std::mem::take(
                            &mut *physical_responses
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner),
                        )
                    };
                    for physical in completed {
                        accumulate_usage(&mut mod_physical_usage, &physical.usage);
                        accumulate_usage(&mut cumulative_usage, &physical.usage);
                    }
                }
                first_visible_response = false;
                if let Some(error) = pending_attempt.declined_fallback.take() {
                    let mut declined_assistant_rows =
                        std::mem::take(&mut pending_attempt.assistant_rows);
                    let declined_je_rows = std::mem::take(&mut pending_attempt.je_rows);
                    append_live_stream_rows(
                        std::mem::take(&mut pending_attempt.ordered_rows),
                        &declined_je_rows,
                        &mut declined_assistant_rows,
                        transcript.as_ref(),
                        history,
                        &mut transcript_written,
                        None,
                    )
                    .await;
                    if let Some(mut partial) = pending_attempt.partial_response.take() {
                        if partial.usage.cost_estimate.is_none() {
                            partial.usage.cost_estimate.clone_from(&partial.cost);
                        }
                        partial.usage.provider_metadata = partial.provider_metadata.clone();
                        last_usage = partial.usage.clone();
                        if let Some(turn) = mod_turn.as_mut() {
                            turn.observe(&partial, !mod_step_active);
                        }
                        live_hook_transcript.last_usage_tokens = usize::try_from(
                            last_usage
                                .counts()
                                .input_tokens
                                .saturating_add(
                                    last_usage
                                        .counts()
                                        .output_tokens
                                        .saturating_sub(last_usage.counts().reasoning_tokens),
                                )
                                .saturating_add(last_usage.counts().cache_read_tokens)
                                .saturating_add(last_usage.counts().cache_write_tokens),
                        )
                        .unwrap_or(usize::MAX);
                        if !mod_step_active {
                            accumulate_usage(&mut cumulative_usage, &last_usage);
                        }
                        cumulative_usage.cost_estimate = last_usage.cost_estimate.clone();
                        cumulative_usage.provider_metadata = last_usage.provider_metadata.clone();
                    }
                    if let Some(mut row) = pending_attempt.declined_api_error_row.take() {
                        if let Some(writer) = transcript.as_ref() {
                            if let Err(append_error) = writer
                                .accept_server_fallback_api_error_row(&mut row, history)
                                .await
                            {
                                tracing::warn!(
                                    %append_error,
                                    "declined server-fallback API-error append failed; preserving the original row"
                                );
                            }
                        }
                        let message = row.query_message();
                        let message_index = crate::transcript::message_row_index(&message)
                            .expect("synthetic API-error row is session-visible");
                        if let Err(index_error) =
                            crate::transcript::persist_current_message_index_high_water().await
                        {
                            tracing::warn!(%index_error, "could not persist declined fallback row index");
                        }
                        if let Some(writer) = transcript.as_ref() {
                            if let Err(write_error) =
                                writer.record_server_fallback_api_error_row(&row).await
                            {
                                tracing::warn!(%write_error, "could not persist declined fallback API-error row");
                            }
                        }
                        history.push(message);
                        transcript_written = history.len();
                        let _ = out_tx
                            .send(SubagentEvent::ServerFallbackApiErrorRow {
                                agent_id,
                                row,
                                message_index,
                            })
                            .await;
                    }
                    emit_failed(
                        &ctx,
                        &out_tx,
                        transcript.as_ref(),
                        history,
                        &mut transcript_written,
                        agent_id,
                        error,
                        cumulative_usage.clone(),
                        &last_usage,
                        live_hook_model_selection,
                    )
                    .await;
                    return;
                }
                let mut live_tool_calls = std::mem::take(&mut pending_attempt.tool_calls);
                let mut assistant_rows = std::mem::take(&mut pending_attempt.assistant_rows);
                let mut ordered_rows = std::mem::take(&mut pending_attempt.ordered_rows);
                let mut je_rows = std::mem::take(&mut pending_attempt.je_rows);
                let mut early_tool_result_ids =
                    std::mem::take(&mut pending_attempt.early_tool_result_ids);
                let attempt_request_messages =
                    std::mem::take(&mut pending_attempt.request_messages);
                let (response, mut remainder) = match pending_attempt.response {
                    Some(Ok(r)) => r,
                    Some(Err((partial_blocks, e))) => {
                        let had_streamed_assistant_rows = !assistant_rows.is_empty();
                        let stream_error_text = e
                            .provider_message()
                            .map(str::to_owned)
                            .unwrap_or_else(|| e.to_string());
                        if let LlmError::MalformedToolInput {
                            tool_name,
                            has_other_tool_calls,
                            ..
                        } = &e
                        {
                            // The Host enables this policy only for a host-managed app creation run.
                            // Ordinary agents keep the existing StructuredOutput-only
                            // recovery policy, even when parse retries are configured.
                            let create_local_tool = retry_response_body
                                && ctx.tool_invoker.is_some()
                                && (allowed_tools.is_empty() || allowed_tools.contains(tool_name))
                                && tool_schemas.iter().any(|tool| {
                                    tool.get("name").and_then(serde_json::Value::as_str)
                                        == Some(tool_name.as_str())
                                        && tool.get("input_schema").is_some()
                                        && tool.get("type").is_none()
                                });
                            let can_recover = (force_structured_tool == Some(tool_name.as_str())
                                || create_local_tool)
                                && (!retry_response_body || local_only_response)
                                && !has_other_tool_calls
                                && structured_parse_retry_cap > 0
                                && model_attempt.is_none();
                            if can_recover
                                && structured_parse_retries < structured_parse_retry_cap
                                && turn_idx + 1 < max_turns
                            {
                                structured_parse_retries += 1;
                                // The parse-error result has no authoritative usage;
                                // a later success must not imply all calls were counted.
                                usage_complete = false;
                                let reason = format!(
                                    "{tool_name} JSON correction {structured_parse_retries}/{structured_parse_retry_cap}: {e}"
                                );
                                api_client
                                    .observe_workflow_query_retry(
                                        agent_id,
                                        structured_parse_retries + 1,
                                        reason.clone(),
                                    )
                                    .await;
                                // Keep completed turns and their tool results. Nothing
                                // from this malformed response is dispatched or replayed.
                                let correction = ConversationMessage::user_meta(
                                    MessageId::new(),
                                    format!(
                                        "{reason}. The malformed response was discarded. Reuse the completed tool results above. Call {tool_name} again with one complete, concise JSON object matching its schema; omit unrelated fields and do not repeat completed tools."
                                    ),
                                );
                                history.push(correction.clone());
                                emit_message(&out_tx, agent_id, &correction).await;
                                flush_transcript(
                                    transcript.as_ref(),
                                    history,
                                    &mut transcript_written,
                                )
                                .await;
                                // Re-enter the normal turn boundary: budgets, cancellation,
                                // model call ownership and max_turns still apply.
                                continue 'model_turns;
                            }
                            let error = if can_recover && structured_parse_retries > 0 {
                                format!(
                                    "{tool_name} JSON recovery stopped after {structured_parse_retries} retries (limit {structured_parse_retry_cap}, turn {}/{max_turns}): {e}",
                                    turn_idx + 1
                                )
                            } else {
                                format!("subagent output error: {e}")
                            };
                            if !defer_local_work {
                                append_unmatched_stream_tool_results(
                                    &assistant_rows,
                                    &stream_error_text,
                                    &mut early_tool_result_ids,
                                    &mut ordered_rows,
                                    &mut je_rows,
                                    local_agent_tool_uses.as_ref(),
                                    &ctx,
                                    &out_tx,
                                )
                                .await;
                            }
                            append_live_stream_rows(
                                std::mem::take(&mut ordered_rows),
                                &je_rows,
                                &mut assistant_rows,
                                transcript.as_ref(),
                                history,
                                &mut transcript_written,
                                None,
                            )
                            .await;
                            emit_failed(
                                &ctx,
                                &out_tx,
                                transcript.as_ref(),
                                history,
                                &mut transcript_written,
                                agent_id,
                                error,
                                cumulative_usage.clone(),
                                &last_usage,
                                live_hook_model_selection,
                            )
                            .await;
                            return;
                        }
                        if !defer_local_work {
                            append_unmatched_stream_tool_results(
                                &assistant_rows,
                                &stream_error_text,
                                &mut early_tool_result_ids,
                                &mut ordered_rows,
                                &mut je_rows,
                                local_agent_tool_uses.as_ref(),
                                &ctx,
                                &out_tx,
                            )
                            .await;
                        }
                        append_live_stream_rows(
                            std::mem::take(&mut ordered_rows),
                            &je_rows,
                            &mut assistant_rows,
                            transcript.as_ref(),
                            history,
                            &mut transcript_written,
                            None,
                        )
                        .await;
                        if is_workflow_watchdog_timeout(&e) {
                            emit_failed(&ctx,
                            &out_tx,
                            transcript.as_ref(),
                            history,
                            &mut transcript_written,
                            agent_id,
                            format!(
                                "{} {e}",
                                lingxi_core::host::subagent_spawn::SUBAGENT_QUERY_TIMEOUT_REASON_PREFIX
                            ),
                            cumulative_usage.clone(),
                    &last_usage,
                    live_hook_model_selection,
                ).await;
                            return;
                        }
                        // CC 2.1.207 subagent api_error_partial recovery. Only
                        // transient provider failures can return already-produced
                        // prose; schema-bound runs must keep their structured contract.
                        let salvaged = translate_response_blocks(&partial_blocks);
                        match classify_api_termination(&e) {
                            Some((_error_kind, api_error_text))
                                if ctx.schema.is_none()
                                    && !final_text_blocks(history, &salvaged).is_empty() =>
                            {
                                if had_streamed_assistant_rows {
                                    if defer_local_work {
                                        accept_deferred_assistant_rows(
                                            &mut assistant_rows,
                                            transcript.as_ref(),
                                            &attempt_request_messages,
                                            None,
                                        )
                                        .await;
                                        for row in &assistant_rows {
                                            emit_message(&out_tx, agent_id, &row.accepted).await;
                                        }
                                    }
                                    persist_salvaged_assistant_rows(
                                        &mut assistant_rows,
                                        transcript.as_ref(),
                                        history,
                                    )
                                    .await;
                                }
                                let cutoff_note = build_cutoff_note(api_error_text);
                                let mut result =
                                    build_recovered_result(history, &salvaged, &cutoff_note);
                                let handback = finalize_handback_result(&ctx, &mut result).await;
                                if !salvaged.is_empty() {
                                    if !had_streamed_assistant_rows {
                                        let partial_message = ConversationMessage::Assistant {
                                            per_turn_effort: None,
                                            id: MessageId::new(),
                                            content: salvaged,
                                            stop_reason: Some("api_error".to_string()),
                                        };
                                        history.push(partial_message.clone());
                                        emit_message(&out_tx, agent_id, &partial_message).await;
                                    }
                                    assistant_message_count =
                                        assistant_message_count.saturating_add(1);
                                }
                                flush_transcript(
                                    transcript.as_ref(),
                                    history,
                                    &mut transcript_written,
                                )
                                .await;
                                emit_transcript_snapshot(&out_tx, agent_id, history).await;
                                if !cleanup_before_terminal(
                                    &ctx,
                                    &out_tx,
                                    transcript.as_ref(),
                                    cumulative_usage.clone(),
                                    history,
                                    &last_usage,
                                    live_hook_model_selection,
                                )
                                .await
                                {
                                    return;
                                }
                                if let Some(writer) = transcript.as_ref() {
                                    let _ = writer.record_terminal("completed", None).await;
                                }
                                publish_prompt_hook_transcript(
                                    &ctx,
                                    history,
                                    &last_usage,
                                    live_hook_model_selection,
                                );
                                if let Some(registry) = &ctx.task_registry {
                                    reconcile_agent_child_keepalives(&ctx, registry.as_ref()).await;
                                }
                                let _ = out_tx
                                    .send(SubagentEvent::Completed {
                                        agent_id,
                                        result,
                                        usage: last_usage.clone(),
                                        total_tool_use_count,
                                        total_duration_ms: elapsed_ms(run_start),
                                        assistant_message_count,
                                        last_request_id: last_request_id.clone(),
                                        cumulative_usage: cumulative_usage.clone(),
                                        usage_complete: false,
                                        handback,
                                    })
                                    .await;
                                return;
                            }
                            _ => {
                                emit_failed(
                                    &ctx,
                                    &out_tx,
                                    transcript.as_ref(),
                                    history,
                                    &mut transcript_written,
                                    agent_id,
                                    format!("subagent api error: {e}"),
                                    cumulative_usage.clone(),
                                    &last_usage,
                                    live_hook_model_selection,
                                )
                                .await;
                                return;
                            }
                        }
                    }
                    None => {
                        unreachable!("declined server fallback is handled before response parsing")
                    }
                };

                // Keep the FINAL response usage for the terminal `Completed` rollup
                // (claude `getTokenCountFromUsage` reads the LAST assistant usage — so
                // overwrite, never accumulate, to stay byte-faithful).
                //
                // WP2a item 2 (F002 sub-claim 3): `max_output_tokens_per_turn` is a
                // WIRE ceiling forwarded to the provider via
                // `SubagentApiCallOpts::max_output_tokens` (below) — it is NOT a
                // clamp on the REPORTED usage. Reporting min(real, ceiling) here
                // hid a provider overrun from `cumulative_usage`/settlement instead
                // of surfacing it; report the provider's real usage verbatim.
                last_usage = if mod_step_active {
                    mod_physical_usage.clone()
                } else {
                    response.usage.clone()
                };
                if let Some(turn) = mod_turn.as_mut() {
                    turn.observe(&response, !mod_step_active);
                }
                live_hook_transcript.last_usage_tokens = usize::try_from(
                    last_usage
                        .counts()
                        .input_tokens
                        .saturating_add(
                            last_usage
                                .counts()
                                .output_tokens
                                .saturating_sub(last_usage.counts().reasoning_tokens),
                        )
                        .saturating_add(last_usage.counts().cache_read_tokens)
                        .saturating_add(last_usage.counts().cache_write_tokens),
                )
                .unwrap_or(usize::MAX);
                if !mod_step_active {
                    accumulate_usage(&mut cumulative_usage, &last_usage);
                }
                emit_progress(
                    &out_tx,
                    agent_id,
                    total_tool_use_count,
                    cumulative_usage
                        .counts()
                        .input_tokens
                        .saturating_add(cumulative_usage.counts().cache_write_tokens)
                        .saturating_add(cumulative_usage.counts().cache_read_tokens)
                        .saturating_add(
                            cumulative_usage
                                .counts()
                                .output_tokens
                                .saturating_sub(cumulative_usage.counts().reasoning_tokens),
                        ),
                )
                .await;
                match admitted_server_fallback_route(
                    &response,
                    ctx.server_fallback_model_enforcement.as_ref(),
                ) {
                    Ok(Some((fallback_model, fallback_profile))) => {
                        model = fallback_model;
                        model_profile = Some(fallback_profile);
                    }
                    Ok(None) => {}
                    Err(error) => {
                        emit_failed(
                            &ctx,
                            &out_tx,
                            transcript.as_ref(),
                            history,
                            &mut transcript_written,
                            agent_id,
                            error,
                            cumulative_usage.clone(),
                            &last_usage,
                            live_hook_model_selection,
                        )
                        .await;
                        return;
                    }
                }
                // Track the assistant-message count (claude `agentMessages.length`) and
                // the FINAL turn's provider request id (claude
                // `lastAssistantMessage.requestId`). One assistant turn per round-trip;
                // `response.id` is the provider response id (the `requestId` analog).
                assistant_message_count = assistant_message_count.saturating_add(1);
                last_request_id = if response.id.is_empty() {
                    None
                } else {
                    Some(response.id.clone())
                };

                let assistant_blocks = translate_response_blocks(&response.content);
                let stop_reason = response.stop_reason.clone();
                append_live_stream_rows(
                    std::mem::take(&mut ordered_rows),
                    &je_rows,
                    &mut assistant_rows,
                    transcript.as_ref(),
                    history,
                    &mut transcript_written,
                    stop_reason.as_deref(),
                )
                .await;
                if let Some(content) = wake_message.take() {
                    let direction = content.to_conversation_message(MessageId::new(), false);
                    history.push(direction.clone());
                    flush_transcript(transcript.as_ref(), history, &mut transcript_written).await;
                    emit_message(&out_tx, agent_id, &direction).await;
                }
                // These streamed rows have already passed the retained-row
                // append boundary. Even when terminal metadata is absent, keep
                // them in raw query history while preventing the ordinary flush
                // path from writing an incomplete JSONL assistant row.
                let tool_uses: Vec<(
                    lingxi_core::types::ToolUseId,
                    String,
                    serde_json::Value,
                    Option<String>,
                )> = assistant_blocks
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::ToolUse {
                            id,
                            name,
                            input,
                            provider_id,
                            ..
                        } => Some((id.clone(), name.clone(), input.clone(), provider_id.clone())),
                        _ => None,
                    })
                    .collect();
                total_tool_use_count = total_tool_use_count.saturating_add(tool_uses.len() as u64);
                let recoverable_structured_truncation = retry_response_body
                    && force_structured_tool.is_some()
                    && stop_reason.as_deref() == Some("max_tokens");
                if tool_uses.is_empty() && !recoverable_structured_truncation {
                    whendone_idle_turns = whendone_idle_turns.saturating_add(1);
                } else if !tool_uses.is_empty() {
                    whendone_idle_turns = 0;
                }

                // Dispatch any tool_use blocks FIRST, then decide loop disposition by
                // stop_reason — mirroring the orchestrator references. Every emitted
                // tool is dispatched even when the model used an unusual stop reason.
                let mut tool_turn_end = None;
                if !tool_uses.is_empty() {
                    let mut context_state_candidates = Vec::new();
                    let mut context_modifiers = Vec::new();
                    let mut injected_tool_messages = Vec::new();
                    let file_write_requested = tool_uses
                        .iter()
                        .any(|(_, name, _, _)| matches!(name.as_str(), "Write" | "Edit"));
                    if ctx.tool_invoker.is_none() {
                        emit_failed(
                            &ctx,
                            &out_tx,
                            transcript.as_ref(),
                            history,
                            &mut transcript_written,
                            agent_id,
                            "subagent requested a tool but no tool_invoker was inherited"
                                .to_string(),
                            cumulative_usage.clone(),
                            &last_usage,
                            live_hook_model_selection,
                        )
                        .await;
                        return;
                    }
                    let mut instruction_reminders: Vec<(
                        hooks::ExactHookText,
                        Option<lingxi_core::types::utf16_json::Utf16JsonProjection>,
                    )> = Vec::new();
                    let mut tool_results: Vec<ContentBlock> = Vec::with_capacity(tool_uses.len());
                    let mut tool_context: Vec<(
                        String,
                        lingxi_core::types::utf16_json::Utf16JsonProjection,
                    )> = Vec::new();
                    for (tool_use_id, name, input, provider_id) in &tool_uses {
                        // Structured output: the synthetic `StructuredOutput` tool is not
                        // dispatched — its input IS the run's result, but ONLY when it
                        // VALIDATES against the schema (claude-code Ajv validation inside
                        // the tool `call`). A valid input is captured + a benign result fed
                        // back (the loop terminates below); an INVALID input feeds back an
                        // `is_error` ToolResult (`Output does not match required schema: …`)
                        // and increments the failed-validation count `kn` — the model sees
                        // the error and retries on the next turn (the `tool_use` stop keeps
                        // the loop going), up to the retry cap checked after dispatch.
                        if force_structured_tool == Some(name.as_str()) {
                            match validate_structured_output(ctx.schema.as_deref(), input) {
                                Ok(()) => {
                                    structured_result = Some(input.clone());
                                    tool_results.push(ContentBlock::ToolResult {
                                        content_projection: None,
                                        tool_use_id: tool_use_id.clone(),
                                        // claude's StructuredOutput tool returns
                                        // `data: "Structured output provided successfully"`
                                        // as the tool-result content (binary 2.1.195
                                        // strings :311431 / :438281).
                                        content: "Structured output provided successfully"
                                            .to_string(),
                                        is_error: Some(false),
                                        provider_tool_use_id: provider_id.clone(),
                                        content_blocks: None,
                                    });
                                }
                                Err(detail) => {
                                    structured_failed_count =
                                        structured_failed_count.saturating_add(1);
                                    tool_results.push(ContentBlock::ToolResult {
                                        content_projection: None,
                                        tool_use_id: tool_use_id.clone(),
                                        content: format!(
                                            "Output does not match required schema: {detail}"
                                        ),
                                        is_error: Some(true),
                                        provider_tool_use_id: provider_id.clone(),
                                        content_blocks: None,
                                    });
                                }
                            }
                            let idle_update =
                                observe_local_tool_result(&local_agent_tool_uses, tool_use_id);
                            publish_local_agent_idle_fact(&ctx, idle_update).await;
                            continue;
                        }
                        // Allow-list guard: when `allowed_tools` is non-empty, a model
                        // request for a tool outside it is refused WITHOUT dispatching
                        // (the inherited `RegistryToolInvoker` would otherwise run any
                        // registered tool by name). Surfaced as an `is_error` ToolResult
                        // so the model sees the refusal and can recover, mirroring how a
                        // tool error is fed back. Empty `allowed_tools` skips the guard.
                        if !allowed_tools.is_empty() && !allowed_tools.iter().any(|t| t == name) {
                            // yyo companion note (binary v2.1.186 §7): when the blocked tool
                            // is in the `nke` external companion set, append the byte-exact
                            // guidance suffix so the model knows the tool is a subagent
                            // boundary, not a typo.
                            let note = companion_note_for_disallowed_tool(name).unwrap_or_default();
                            tool_results.push(ContentBlock::ToolResult {
                                content_projection: None,
                                tool_use_id: tool_use_id.clone(),
                                content: format!(
                                    "tool {name:?} is not in this agent's allowed tools{note}"
                                ),
                                is_error: Some(true),
                                provider_tool_use_id: provider_id.clone(),
                                content_blocks: None,
                            });
                            let idle_update =
                                observe_local_tool_result(&local_agent_tool_uses, tool_use_id);
                            publish_local_agent_idle_fact(&ctx, idle_update).await;
                            continue;
                        }
                        let invocation = live_tool_calls
                            .iter_mut()
                            .find(|(call, _)| &call.id == tool_use_id)
                            .and_then(|(_, result)| result.take());
                        let Some(invocation) = invocation else {
                            // A fallback sweep removed the outstanding host
                            // ToolUseId. Do not invent a ToolResult for it.
                            continue;
                        };
                        match invocation {
                            Ok(invocation) => {
                                let result_block = (!early_tool_result_ids.contains(tool_use_id))
                                    .then(|| {
                                        successful_live_tool_result_block(
                                            name,
                                            tool_use_id,
                                            provider_id.as_deref(),
                                            &invocation,
                                        )
                                    });
                                if let Some(state) = invocation.context_state.clone() {
                                    context_state_candidates.push(state);
                                }
                                if !early_tool_result_ids.contains(tool_use_id) {
                                    injected_tool_messages.extend(invocation.new_messages);
                                }
                                if let Some(modifier) = invocation.context_modifier {
                                    context_modifiers.push(modifier);
                                }
                                if invocation
                                    .context
                                    .value
                                    .as_array()
                                    .is_some_and(|context| !context.is_empty())
                                {
                                    tool_context.push((
                                        tool_use_id.as_str().to_owned(),
                                        invocation.context,
                                    ));
                                }
                                if !invocation.is_error {
                                    if let Some(turn_end) = invocation.turn_end {
                                        tool_turn_end = Some(turn_end);
                                    }
                                }
                                if name == "Read" && !invocation.is_error {
                                    if let (Some(provider), Some(path)) = (
                                        ctx.instruction_provider.as_ref(),
                                        input.get("file_path").and_then(serde_json::Value::as_str),
                                    ) {
                                        let cwd = ctx.cwd.as_deref().unwrap_or(&ctx.hook_cwd);
                                        let path = std::path::Path::new(path);
                                        let path = if path.is_absolute() || path.starts_with("~") {
                                            path.to_path_buf()
                                        } else {
                                            cwd.join(path)
                                        };
                                        let partial = input.get("offset").is_some()
                                            || input.get("limit").is_some();
                                        let read_context = provider
                                            .after_read(
                                                cwd,
                                                &path,
                                                partial,
                                                &mut ctx.instruction_context,
                                            )
                                            .await;
                                        instruction_reminders.extend(
                                            read_context.legacy_reminders.into_iter().map(|body| {
                                                (hooks::ExactHookText::from_text(body), None)
                                            }),
                                        );
                                        if !read_context.agents_context.is_empty() {
                                            let exact_context: Vec<_> = read_context
                                                .agents_context
                                                .iter()
                                                .cloned()
                                                .map(|text| hooks::ExactHookText::from_text(text))
                                                .collect();
                                            let attachment = hooks::additional_context_attachment(
                                                "tool.call",
                                                &format!("{}-context", tool_use_id.as_str()),
                                                "PostToolUse",
                                                &exact_context,
                                            );
                                            let body =
                                                hooks::ExactHookText::join(&exact_context, "\n");
                                            let rendered = hooks::ExactHookText::wrapped(
                                                "<system-reminder>\ntool.call hook additional context: ",
                                                &body,
                                                "\n</system-reminder>",
                                            );
                                            instruction_reminders
                                                .push((rendered, Some(attachment)));
                                        }
                                    }
                                }
                                if let Some(result_block) = result_block {
                                    tool_results.push(result_block);
                                }
                                if !early_tool_result_ids.contains(tool_use_id) {
                                    let idle_update = observe_local_tool_result(
                                        &local_agent_tool_uses,
                                        tool_use_id,
                                    );
                                    publish_local_agent_idle_fact(&ctx, idle_update).await;
                                }
                            }
                            Err(lingxi_core::host::tool_invoker::ToolInvokerError::Abort(
                                error,
                            )) => {
                                emit_failed(
                                    &ctx,
                                    &out_tx,
                                    transcript.as_ref(),
                                    history,
                                    &mut transcript_written,
                                    agent_id,
                                    error,
                                    cumulative_usage.clone(),
                                    &last_usage,
                                    live_hook_model_selection,
                                )
                                .await;
                                return;
                            }
                            Err(error) => {
                                if !early_tool_result_ids.contains(tool_use_id) {
                                    tool_results.push(ContentBlock::ToolResult {
                                        content_projection: None,
                                        tool_use_id: tool_use_id.clone(),
                                        content: error.model_tool_result_content(),
                                        is_error: Some(true),
                                        provider_tool_use_id: provider_id.clone(),
                                        content_blocks: None,
                                    });
                                    let idle_update = observe_local_tool_result(
                                        &local_agent_tool_uses,
                                        tool_use_id,
                                    );
                                    publish_local_agent_idle_fact(&ctx, idle_update).await;
                                }
                            }
                        }
                    }
                    if !tool_results.is_empty() {
                        let tool_results_msg = ConversationMessage::User {
                            api_message_override: None,
                            id: MessageId::new(),
                            content: tool_results,
                            is_meta: false,
                            is_compact_summary: false,
                            is_visible_in_transcript_only: false,
                        };
                        history.push(tool_results_msg.clone());
                        flush_transcript(transcript.as_ref(), history, &mut transcript_written)
                            .await;
                        let tool_results_msg = history
                            .last()
                            .cloned()
                            .expect("tool results message was just appended");
                        emit_message(&out_tx, agent_id, &tool_results_msg).await;
                    }

                    // ToolCallResult.new_messages are ordered after the tool
                    // result batch, as in the main loop. They are real history
                    // rows: emit and flush each exactly once before the next
                    // provider snapshot.
                    for message in injected_tool_messages {
                        history.push(message);
                        flush_transcript(transcript.as_ref(), history, &mut transcript_written)
                            .await;
                        let message = history
                            .last()
                            .cloned()
                            .expect("a tool-injected message was just appended");
                        emit_message(&out_tx, agent_id, &message).await;
                    }

                    for (reminder, attachment) in instruction_reminders {
                        let message = reminder.to_conversation_message(MessageId::new(), true);
                        if let (Some(writer), Some(attachment)) = (transcript.as_ref(), attachment)
                        {
                            flush_transcript(transcript.as_ref(), history, &mut transcript_written)
                                .await;
                            if transcript_written == history.len() {
                                match writer.record_attachment_message(&message, attachment).await {
                                    Ok(()) => {
                                        // The attachment writer already persisted this row.
                                        transcript_written += 1;
                                    }
                                    Err(error) => tracing::warn!(
                                        %error,
                                        "could not persist worker instruction attachment"
                                    ),
                                }
                            }
                        }
                        history.push(message.clone());
                        emit_message(&out_tx, agent_id, &message).await;
                    }

                    for (tool_use_id, context) in tool_context {
                        let context_projection = context;
                        let Some(context) = context_projection.value.as_array() else {
                            continue;
                        };
                        let mut lines = Vec::new();
                        for (index, value) in context.iter().enumerate() {
                            let Some(text) = value.as_str() else {
                                continue;
                            };
                            if text.is_empty() {
                                continue;
                            }
                            let units = context_projection
                                .string_units(&format!("/{index}"))
                                .unwrap_or_else(|| text.encode_utf16().collect());
                            lines.push((text.to_owned(), units));
                        }
                        if lines.is_empty() {
                            continue;
                        }
                        let mut reminder_units =
                            "<system-reminder>\ntool.call hook additional context: "
                                .encode_utf16()
                                .collect::<Vec<_>>();
                        for (index, (_, units)) in lines.iter().enumerate() {
                            if index > 0 {
                                reminder_units.push(b'\n' as u16);
                            }
                            reminder_units.extend(units.iter().copied());
                        }
                        reminder_units.extend("\n</system-reminder>".encode_utf16());
                        let reminder_text = String::from_utf16_lossy(&reminder_units);
                        let reminder = match String::from_utf16(&reminder_units) {
                            Ok(text) => ConversationMessage::user_meta(MessageId::new(), text),
                            Err(_) => ConversationMessage::user_meta_js_utf16(
                                MessageId::new(),
                                reminder_text,
                                reminder_units,
                            ),
                        };
                        let attachment_value = serde_json::json!({
                            "type":"hook_additional_context",
                            "content":lines.iter().map(|(text, _)| text).collect::<Vec<_>>(),
                            "hookName":"tool.call",
                            "toolUseID":format!("{tool_use_id}-context"),
                            "hookEvent":"PostToolUse",
                        });
                        let mut source_attachment =
                            lingxi_core::types::utf16_json::Utf16JsonProjection::plain(
                                attachment_value,
                            );
                        source_attachment.strings.extend(
                            lines
                                .iter()
                                .enumerate()
                                .filter(|(_, (_, units))| String::from_utf16(units).is_err())
                                .map(|(index, (_, units))| {
                                    lingxi_core::types::utf16_json::Utf16JsonString {
                                        pointer: format!("/content/{index}"),
                                        code_units: units.clone(),
                                    }
                                }),
                        );
                        if let Some(writer) = transcript.as_ref() {
                            writer.register_source_attachment(&reminder, source_attachment);
                        }
                        child_attachments.register(
                            &reminder,
                            "hook_additional_context",
                            serde_json::json!({"kind":"plugin","event":"tool.call"}),
                        );
                        history.push(reminder.clone());
                        flush_transcript(transcript.as_ref(), history, &mut transcript_written)
                            .await;
                        let reminder = history
                            .last()
                            .cloned()
                            .expect("attachment message was just appended");
                        emit_message(&out_tx, agent_id, &reminder).await;
                    }

                    if file_write_requested {
                        if let Some(block) = match &ctx.new_diagnostics_source {
                            Some(source) => source.take_new_diagnostics_block().await,
                            None => None,
                        } {
                            let diagnostics_message = ConversationMessage::user_meta(
                                MessageId::new(),
                                format!("<system-reminder>\n{block}\n</system-reminder>"),
                            );
                            history.push(diagnostics_message.clone());
                            flush_transcript(transcript.as_ref(), history, &mut transcript_written)
                                .await;
                            let diagnostics_message = history
                                .last()
                                .cloned()
                                .expect("diagnostics message was just appended");
                            emit_message(&out_tx, agent_id, &diagnostics_message).await;
                        }
                    }
                    flush_transcript(transcript.as_ref(), history, &mut transcript_written).await;
                    if !context_modifiers.is_empty() {
                        let prepared = apply_nested_tool_context_modifiers(
                            &tool_context_state,
                            context_state_candidates,
                            context_modifiers,
                            &logical_tool_model,
                            logical_tool_model_profile.as_deref(),
                            ctx.model_resolution_context_provider.as_deref(),
                        );
                        let prepared = match prepared {
                            Ok(prepared) => {
                                let changed = prepared.model_selected
                                    || prepared.model != logical_tool_model
                                    || prepared.model_profile != logical_tool_model_profile;
                                let persisted = match transcript.as_ref().filter(|_| changed) {
                                    Some(writer) => writer
                                        .record_model_selection(
                                            &prepared.model,
                                            prepared.model_profile.as_deref(),
                                        )
                                        .await
                                        .map_err(|error| format!("could not persist nested tool model selection: {error}")),
                                    None => Ok(()),
                                };
                                persisted.map(|()| prepared)
                            }
                            Err(error) => Err(error),
                        };
                        match prepared {
                            Ok(prepared) => {
                                if prepared.model_selected
                                    || prepared.model != logical_tool_model
                                    || prepared.model_profile != logical_tool_model_profile
                                {
                                    // An accepted tool selection is a user route
                                    // change, so its next query starts outside
                                    // the previously admitted refusal cascade.
                                    refusal_cascade.clear_target();
                                    model.clone_from(&prepared.model);
                                    model_profile.clone_from(&prepared.model_profile);
                                }
                                logical_tool_model.clone_from(&prepared.model);
                                logical_tool_model_profile.clone_from(&prepared.model_profile);
                                live_hook_model_selection
                                    .model
                                    .clone_from(&logical_tool_model);
                                live_hook_model_selection
                                    .model_profile
                                    .clone_from(&logical_tool_model_profile);
                                tool_context_state = Some(prepared.state);
                            }
                            Err(error) => {
                                emit_failed(
                                    &ctx,
                                    &out_tx,
                                    transcript.as_ref(),
                                    history,
                                    &mut transcript_written,
                                    agent_id,
                                    error,
                                    cumulative_usage.clone(),
                                    &last_usage,
                                    live_hook_model_selection,
                                )
                                .await;
                                return;
                            }
                        }
                    } else if let Some(state) = context_state_candidates.pop() {
                        // A tool can return a concrete state even when it has
                        // no modifier. Keep the latest observed snapshot, while
                        // the adapter refreshes history and per-call facts on
                        // every later invocation.
                        tool_context_state = Some(state);
                    }
                    if let Some(turn_end) = tool_turn_end {
                        flush_transcript(transcript.as_ref(), history, &mut transcript_written)
                            .await;
                        tracing::info!(
                            event = "tengu_mcp_tool_result_ended_turn",
                            source = turn_end.source.as_str(),
                        );
                    }

                    // A failed StructuredOutput validation is terminal once its retry
                    // budget is exhausted.
                    if force_structured_tool.is_some()
                        && structured_result.is_none()
                        && structured_failed_count > 0
                        && structured_failed_count >= structured_retry_cap
                    {
                        let calls = if structured_failed_count == 1 {
                            "call"
                        } else {
                            "calls"
                        };
                        emit_failed(&ctx,
                    &out_tx,
                    transcript.as_ref(),
                    history,
                    &mut transcript_written,
                    agent_id,
                    format!(
                        "agent({{schema}}): StructuredOutput retry cap ({structured_retry_cap}) exceeded — {structured_failed_count} failed {calls} with no valid output"
                    ),
                    cumulative_usage.clone(),
                    &last_usage,
                    live_hook_model_selection,
                ).await;
                        return;
                    }
                }

                // A Mods turn.step may yield further assistant records. Consume them
                // before advancing the child turn, while still honoring cancellation.
                // This belongs to the visible-response loop, not the tool-dispatch
                // branch: a text-only synthetic response can precede another physical
                // provider response just as a tool response can.
                let next_visible_response = if mod_step_active
                    && structured_result.is_none()
                    && !matches!(
                        stop_reason.as_deref(),
                        Some("refusal" | "model_context_window_exceeded")
                    ) {
                    let next_ctx = ctx.clone();
                    let next_transcript = transcript.clone();
                    let next_request_messages = attempt_request_messages.clone();
                    let next_system = system.clone();
                    let next_logical_tool_model = logical_tool_model.clone();
                    let next_logical_tool_profile = logical_tool_model_profile.clone();
                    let next_tool_context_state = tool_context_state.clone();
                    let next_allowed_tools = allowed_tools.clone();
                    let next_defer_local_work = retry_response_body
                        || (force_structured_tool.is_some() && structured_parse_retry_cap > 0);
                    let next_lifecycle = local_agent_tool_uses.clone();
                    let next_out_tx = out_tx.clone();
                    let next_response = async move {
                        let first = remainder.next().await?;
                        let stream = futures::stream::once(async move { first })
                            .chain(remainder)
                            .boxed();
                        let next_retryable_body = std::sync::atomic::AtomicBool::new(true);
                        let next_live_output_started = std::sync::atomic::AtomicBool::new(false);
                        let next_tool_effects_started = std::sync::atomic::AtomicBool::new(false);
                        Some(
                            accumulate_agent_stream_live(
                                stream,
                                &next_ctx,
                                next_transcript.as_ref(),
                                next_request_messages,
                                next_system,
                                next_logical_tool_model.clone(),
                                next_logical_tool_model,
                                next_logical_tool_profile,
                                next_tool_context_state,
                                &next_allowed_tools,
                                force_structured_tool.as_deref(),
                                next_defer_local_work,
                                &next_retryable_body,
                                &next_live_output_started,
                                &next_tool_effects_started,
                                next_lifecycle.as_ref(),
                                &next_out_tx,
                            )
                            .await,
                        )
                    };
                    if !event_channel_open {
                        next_response.await
                    } else {
                        tokio::pin!(next_response);
                        loop {
                            tokio::select! {
                                biased;
                                event = event_rx.recv(), if event_channel_open => {
                                    match event {
                                        Some(lingxi_core::Event::UserExit | lingxi_core::Event::UserInterrupt) => {
                                            if let Some(turn) = mod_turn.as_mut() { turn.aborted(); }
                                            emit_killed(&ctx, &out_tx, transcript.as_ref(), history,
                                                &mut transcript_written, agent_id,
                                &last_usage,
                                live_hook_model_selection,
                            ).await;
                                            return;
                                        }
                                        None => event_channel_open = false,
                                        Some(_) => {}
                                    }
                                }
                                response = &mut next_response => break response,
                            }
                        }
                    }
                } else {
                    None
                };
                if let Some(next) = next_visible_response {
                    pending_attempt = next;
                    continue 'visible_responses;
                }

                // A refusal may continue through the subagent's local cascade unless a
                // trusted terminal tool marker already ended this response.
                if tool_turn_end.is_none() && stop_reason.as_deref() == Some("refusal") {
                    let frame_id = MessageId::new();
                    let notice_uuid = frame_id.as_uuid().to_string();
                    if let Some(hop) =
                        refusal_cascade.next_hop(&ctx.refusal_fallback_chain, &model, notice_uuid)
                    {
                        for report in &hop.declines {
                            tracing::info!(
                                event = "tengu_refusal_fallback_route_declined",
                                reason = report.as_str(),
                            );
                        }
                        model = hop.fallback_model.clone();
                        for emitted in hop.notices {
                            history.push(refusal_fallback_frame(frame_id, &emitted.banner));
                        }
                        continue 'model_turns;
                    }
                }
                let should_continue = tool_turn_end.is_none()
                    && stop_reason.as_deref() == Some("tool_use")
                    && !tool_uses.is_empty()
                    && structured_result.is_none();
                if !should_continue {
                    if tool_turn_end.is_none()
                        && !pending_peer_messages.is_empty()
                        && turn_idx + 1 < max_turns
                    {
                        drain_peer_messages(
                            &ctx,
                            &mut pending_peer_messages,
                            history,
                            transcript.as_ref(),
                            &mut transcript_written,
                            &out_tx,
                        )
                        .await;
                        continue 'model_turns;
                    }
                    if tool_turn_end.is_none() && turn_idx + 1 < max_turns {
                        if let Some(handback_runtime) = &ctx.handback {
                            let auxiliary =
                                ctx.query_source_label.as_deref().is_some_and(|source| {
                                    source != "agent" && source != "agent_resume"
                                });
                            let waiting = handback_runtime
                                .registry
                                .agent_waiting_on_owned_work(agent_id)
                                .await;
                            if !auxiliary
                                && !waiting
                                && handback_runtime.next_bounce().await.is_some()
                            {
                                let reminder = ConversationMessage::user_meta(
                                    MessageId::new(),
                                    format!(
                                        "{}\n{}",
                                        lingxi_core::host::handback::HANDBACK_ENFORCEMENT_PREFIX,
                                        lingxi_core::host::handback::HANDBACK_REMINDER
                                    ),
                                );
                                history.push(reminder.clone());
                                emit_message(&out_tx, agent_id, &reminder).await;
                                continue 'model_turns;
                            }
                        }
                    }

                    // Structured-output calls may continue only while the run and turn
                    // budgets allow it. Already-dispatched tools are never replayed.
                    if tool_turn_end.is_none()
                        && recoverable_structured_truncation
                        && structured_result.is_none()
                    {
                        if structured_truncation_retries < 2 && turn_idx + 1 < max_turns {
                            structured_truncation_retries += 1;
                            let continuation = ConversationMessage::user(
                            MessageId::new(),
                            "Your response reached the output token limit before completing. Continue the unfinished work with the available tools, keeping reasoning concise. Preserve completed work and do not repeat successful tool calls. Call StructuredOutput only when the requested work is complete.".to_string(),
                        );
                            history.push(continuation.clone());
                            emit_message(&out_tx, agent_id, &continuation).await;
                            continue 'model_turns;
                        }
                        emit_failed(&ctx,
                        &out_tx,
                        transcript.as_ref(),
                        history,
                        &mut transcript_written,
                        agent_id,
                        "agent({schema}): output token limit reached before completion; continuation budget exhausted".to_string(),
                        cumulative_usage.clone(),
                    &last_usage,
                    live_hook_model_selection,
                ).await;
                        return;
                    }

                    if force_structured_tool.is_some() && structured_result.is_none() {
                        if tool_turn_end.is_some() {
                            emit_failed(&ctx,
                            &out_tx,
                            transcript.as_ref(),
                            history,
                            &mut transcript_written,
                            agent_id,
                            "agent({schema}): subagent completed without calling StructuredOutput (after in-conversation nudge)".to_string(),
                            cumulative_usage.clone(),
                    &last_usage,
                    live_hook_model_selection,
                ).await;
                            return;
                        }
                        if !force_this_turn {
                            let wrap_up = ConversationMessage::user(
                            MessageId::new(),
                            "Continue working, or call StructuredOutput now if you have your answer."
                                .to_string(),
                        );
                            history.push(wrap_up.clone());
                            emit_message(&out_tx, agent_id, &wrap_up).await;
                            continue 'model_turns;
                        }
                        if structured_nudge_count < 2 {
                            structured_nudge_count = structured_nudge_count.saturating_add(1);
                            let nudge = ConversationMessage::user(
                            MessageId::new(),
                            "You did not call StructuredOutput. You MUST call StructuredOutput to return your answer — the tool input IS your answer. Call it now.".to_string(),
                        );
                            history.push(nudge.clone());
                            emit_message(&out_tx, agent_id, &nudge).await;
                            continue 'model_turns;
                        }
                        emit_failed(&ctx,
                        &out_tx,
                        transcript.as_ref(),
                        history,
                        &mut transcript_written,
                        agent_id,
                        "agent({schema}): subagent completed without calling StructuredOutput (after in-conversation nudge)".to_string(),
                        cumulative_usage.clone(),
                    &last_usage,
                    live_hook_model_selection,
                ).await;
                        return;
                    }

                    if tool_turn_end.is_none()
                        && turn_idx + 1 < max_turns
                        && fold_task_notifications(&ctx, history).await
                    {
                        continue 'model_turns;
                    }
                    let mut result = match structured_result.take() {
                        Some(structured) => structured,
                        None => build_completed_result(
                            history,
                            &assistant_blocks,
                            stop_reason.as_deref(),
                            &model,
                        ),
                    };
                    let handback = finalize_handback_result(&ctx, &mut result).await;
                    flush_transcript(transcript.as_ref(), history, &mut transcript_written).await;
                    emit_transcript_snapshot(&out_tx, agent_id, history).await;
                    if let Some(turn) = mod_turn.as_mut() {
                        turn.answered();
                    }
                    if !cleanup_before_terminal(
                        &ctx,
                        &out_tx,
                        transcript.as_ref(),
                        cumulative_usage.clone(),
                        history,
                        &last_usage,
                        live_hook_model_selection,
                    )
                    .await
                    {
                        return;
                    }
                    foreground_parked = park_foreground_owner(
                        &ctx,
                        &result,
                        &last_usage,
                        total_tool_use_count,
                        elapsed_ms(run_start),
                    )
                    .await;
                    if foreground_parked {
                        ctx.is_async = true;
                        if let Some(writer) = transcript.as_ref() {
                            let _ = writer.record_terminal("idle", None).await;
                        }
                        if !emit_parked(
                            &ctx,
                            &out_tx,
                            agent_id,
                            history,
                            &last_usage,
                            live_hook_model_selection,
                        )
                        .await
                        {
                            return;
                        }
                        terminated_cleanly = true;
                        break 'model_turns;
                    }
                    if let Some(writer) = transcript.as_ref() {
                        let status = if ctx.persistent { "idle" } else { "completed" };
                        let _ = writer.record_terminal(status, None).await;
                    }
                    publish_prompt_hook_transcript(
                        &ctx,
                        history,
                        &last_usage,
                        live_hook_model_selection,
                    );
                    if let Some(registry) = &ctx.task_registry {
                        reconcile_agent_child_keepalives(&ctx, registry.as_ref()).await;
                    }
                    let _ = out_tx
                        .send(SubagentEvent::Completed {
                            agent_id,
                            result,
                            usage: last_usage.clone(),
                            total_tool_use_count,
                            total_duration_ms: elapsed_ms(run_start),
                            assistant_message_count,
                            last_request_id: last_request_id.clone(),
                            cumulative_usage: cumulative_usage.clone(),
                            usage_complete,
                            handback,
                        })
                        .await;
                    terminated_cleanly = true;
                    break 'model_turns;
                }

                // The current visible record's tools have been handled; if the Mods
                // stream has no next visible assistant record, proceed to the next
                // request turn instead of replaying this record.
                break 'visible_responses;
            }
        }

        if !terminated_cleanly {
            // A schema run has no "whatever work was produced" to hand back: the
            // caller asked for an object matching its schema, and
            // `{"reason":"max_turns_exhausted"}` is not one. claude-code checks for
            // a captured structured result AFTER the whole subagent attempt has
            // ended — for ANY exit reason, not just the nudge path — and throws the
            // same terminal error when there is none. Oracle 2.1.258
            // (`~/.local/share/claude/versions/2.1.258`) @172635430, in the workflow
            // `agent()` wrapper `mn`, after its `for await` over the turn loop:
            //   `let U = we && E.structured !== void 0 ? tVn(E.structured, …) : void 0;`
            //   … `if(we){ if(U===void 0) throw Error("agent({schema}): subagent`
            //   `completed without calling StructuredOutput (after in-conversation`
            //   `nudge)"); … }`
            // (`we` = schema-presence flag, `U` = the captured output). Until this
            // change's `tool_choice` relaxation the model was forced to call
            // StructuredOutput on every round, so a schema run terminated within one
            // valid call or `structured_retry_cap` invalid ones and could not reach
            // `max_turns` at all; now that it chooses freely, it can — so this exit
            // has to carry the schema contract too.
            if force_structured_tool.is_some() && structured_result.is_none() {
                emit_failed(&ctx,
                    &out_tx,
                    transcript.as_ref(),
                    history,
                    &mut transcript_written,
                    agent_id,
                    "agent({schema}): subagent completed without calling StructuredOutput (after in-conversation nudge)".to_string(),
                    cumulative_usage.clone(),
                    &last_usage,
                    live_hook_model_selection,
                ).await;
                return;
            }
            // The inner loop fell through: `max_turns` exhausted without a terminal
            // stop. claude-code surfaces this as a completion carrying a max-turns
            // reason rather than a hard failure, so the parent can still consume
            // whatever work was produced.
            flush_transcript(transcript.as_ref(), history, &mut transcript_written).await;
            if let Some(writer) = transcript.as_ref() {
                let status = if ctx.persistent { "idle" } else { "completed" };
                let _ = writer.record_terminal(status, None).await;
            }
            publish_prompt_hook_transcript(&ctx, history, &last_usage, live_hook_model_selection);
            // The oracle's `bft` (src_162329786.js @3532630) finalizes a
            // max-turns exit through the SAME path as any other completion: the
            // last assistant message's text blocks are the result content, and a
            // `max_turns_reached` attachment only adds a harness NOTE in front of
            // them. Dropping the blocks here made every turn-limited subagent
            // return `(Subagent completed but returned no output.)` — the caller
            // lost whatever partial work the agent had reported. Build the normal
            // result and carry the reason alongside it, so the reason readers
            // (`tasks::handlers::local_agent::max_turns_reached_from`,
            // `fusion::panel::max_turns_exhausted_detail`) still see it.
            let mut result = build_completed_result(history, &[], None, &model);
            if let Some(obj) = result.as_object_mut() {
                obj.insert(
                    "reason".to_string(),
                    serde_json::Value::String("max_turns_exhausted".to_string()),
                );
                obj.insert("max_turns".to_string(), serde_json::json!(max_turns));
            }
            let handback = finalize_handback_result(&ctx, &mut result).await;
            if !cleanup_before_terminal(
                &ctx,
                &out_tx,
                transcript.as_ref(),
                cumulative_usage.clone(),
                history,
                &last_usage,
                live_hook_model_selection,
            )
            .await
            {
                return;
            }
            foreground_parked = park_foreground_owner(
                &ctx,
                &result,
                &last_usage,
                total_tool_use_count,
                elapsed_ms(run_start),
            )
            .await;
            if let Some(turn) = mod_turn.as_mut() {
                turn.answered();
            }
            if foreground_parked {
                ctx.is_async = true;
                if let Some(writer) = transcript.as_ref() {
                    let _ = writer.record_terminal("idle", None).await;
                }
                if !emit_parked(
                    &ctx,
                    &out_tx,
                    agent_id,
                    history,
                    &last_usage,
                    live_hook_model_selection,
                )
                .await
                {
                    return;
                }
            } else {
                emit_transcript_snapshot(&out_tx, agent_id, history).await;
                if let Some(registry) = &ctx.task_registry {
                    reconcile_agent_child_keepalives(&ctx, registry.as_ref()).await;
                }
                let _ = out_tx
                    .send(SubagentEvent::Completed {
                        agent_id,
                        result,
                        usage: last_usage.clone(),
                        total_tool_use_count,
                        total_duration_ms: elapsed_ms(run_start),
                        assistant_message_count,
                        last_request_id: last_request_id.clone(),
                        cumulative_usage: cumulative_usage.clone(),
                        usage_complete,
                        handback,
                    })
                    .await;
            }
        }

        if let Some(turn) = mod_turn.as_mut() {
            turn.dispatch();
        }

        // Flush the turn-set's messages to the per-agent transcript. Runs for
        // BOTH dispositions below — a one-shot subagent's transcript is just as
        // much a record as a persistent one's, and the `SubagentStop` hook
        // reports its path either way. Best-effort: a transcript write failure
        // must never mask the agent's result.
        flush_transcript(transcript.as_ref(), history, &mut transcript_written).await;

        // ----- Persist decision ------------------------------------------------
        // Non-persistent (batch-8) behavior: end after one turn-set. This preserves
        // today's exact semantics — every existing call site sets `persistent`
        // false, so they `return` here as before.
        if !ctx.persistent && !foreground_parked {
            return;
        }

        // Persistent teammate: park awaiting the next inbound `UserMessage`. If the
        // event channel has already closed, no message can ever arrive again, so we
        // terminate gracefully.
        if !event_channel_open {
            if let Some(writer) = transcript.as_ref() {
                let _ = writer.record_terminal("completed", None).await;
            }
            return;
        }
        loop {
            // Subscribe was established before the turn; checking after registering
            // the revision prevents a notification between check and park being lost.
            // Completed is consumed asynchronously by the handler. Wait for
            // its rest acknowledgement before starting a notification turn,
            // otherwise the old Completed can park a newly running owner.
            let handler_rested = match &ctx.task_registry {
                Some(registry) => {
                    registry
                        .can_wake_agent_for_task_notification(agent_id)
                        .await
                }
                None => true,
            };
            let before_notifications = history.len();
            if handler_rested {
                drain_peer_messages(
                    &ctx,
                    &mut pending_peer_messages,
                    history,
                    transcript.as_ref(),
                    &mut transcript_written,
                    &out_tx,
                )
                .await;
                if history.len() != before_notifications {
                    if let Some(registry) = &ctx.task_registry {
                        registry
                            .activate_agent_for_task_notification(agent_id)
                            .await;
                    }
                    break;
                }
            }
            if handler_rested && fold_task_notifications(&ctx, history).await {
                for message in &history[before_notifications..] {
                    emit_message(&out_tx, agent_id, message).await;
                }
                break;
            }
            let event = tokio::select! {
                event = event_rx.recv() => event,
                _ = tokio::time::sleep(Duration::from_millis(100)), if !pending_peer_messages.is_empty() => { continue; }
                changed = async {
                    match notification_changes.as_mut() {
                        Some(receiver) => receiver.changed().await,
                        None => std::future::pending().await,
                    }
                } => {
                    if changed.is_err() { notification_changes = None; }
                    continue;
                }
            };
            match event {
                Some(lingxi_core::Event::PeerMessage { envelope }) => {
                    pending_peer_messages.push(envelope);
                    // Consume only after the preceding Completed has rested;
                    // the next loop iteration performs the durable projection.
                    continue;
                }
                Some(lingxi_core::Event::UserMessage { content, .. }) => {
                    if let Some(writer) = transcript.as_ref() {
                        let _ = writer.record_terminal("running", None).await;
                    }
                    // Append the injected message to history (minting our own
                    // MessageId, consistent with the assistant-id minting above —
                    // the event's message_id / request_id are the host's bookkeeping)
                    // and resume the inner turn loop with a fresh `max_turns` budget.
                    let message = ConversationMessage::user(MessageId::new(), content);
                    history.push(message.clone());
                    // Publish the wake before the next provider call, which may
                    // stall: observers use this user message to leave idle.
                    emit_message(&out_tx, agent_id, &message).await;
                    break;
                }
                Some(lingxi_core::Event::UserMessageJsUtf16 {
                    content,
                    utf16_code_units,
                    ..
                }) => {
                    if let Some(writer) = transcript.as_ref() {
                        let _ = writer.record_terminal("running", None).await;
                    }
                    let message = ExactHookText {
                        display: content,
                        utf16_code_units,
                    }
                    .to_conversation_message(MessageId::new(), false);
                    history.push(message.clone());
                    emit_message(&out_tx, agent_id, &message).await;
                    break;
                }
                Some(lingxi_core::Event::UserExit | lingxi_core::Event::UserInterrupt) => {
                    if let Some(executor) = &ctx.hook_executor {
                        executor.discard_agent_prompt_transcript(
                            ctx.hook_session_id,
                            ctx.agent_id,
                            ctx.subagent_stop_firer.as_ref(),
                        );
                    }
                    emit_killed(
                        &ctx,
                        &out_tx,
                        transcript.as_ref(),
                        history,
                        &mut transcript_written,
                        agent_id,
                        &last_usage,
                        live_hook_model_selection,
                    )
                    .await;
                    return;
                }
                // Ignore any other event while idle and keep parking.
                Some(_) => {}
                // Channel closed -> graceful terminal shutdown. A persistent
                // child is idle only while this channel remains open.
                None => {
                    if let Some(writer) = transcript.as_ref() {
                        let _ = writer.record_terminal("completed", None).await;
                    }
                    return;
                }
            }
        }
    }
}

async fn drain_peer_messages(
    ctx: &SubagentContext,
    pending: &mut Vec<lingxi_core::host::handback::HandbackEnvelope>,
    history: &mut Vec<ConversationMessage>,
    transcript: Option<&crate::transcript::AgentTranscriptWriter>,
    written: &mut usize,
    out_tx: &mpsc::Sender<SubagentEvent>,
) {
    let registered = match &ctx.task_registry {
        Some(registry) => registry.pending_handback_reports_for(ctx.agent_id).await,
        None => Vec::new(),
    };
    for envelope in &registered {
        if let Some(queued) = pending
            .iter_mut()
            .find(|queued| queued.receipt == envelope.receipt)
        {
            // The recipient-owned durable claim is authoritative even when
            // an untrusted wake event supplied the same receipt first.
            *queued = envelope.clone();
        } else {
            pending.push(envelope.clone());
        }
    }
    let mut batch = std::mem::take(pending).into_iter();
    while let Some(envelope) = batch.next() {
        if !envelope.validate()
            || !matches!(envelope.receipt.recipient, lingxi_core::host::handback::HandbackRecipient::Agent { agent_id, .. } if agent_id == ctx.agent_id)
        {
            continue;
        }
        let Some(registry) = &ctx.task_registry else {
            continue;
        };
        if registry
            .handback_scope()
            .await
            .map(|scope| scope.session_id)
            != Some(envelope.origin.scope.session_id)
            || !registered.iter().any(|committed| committed == &envelope)
        {
            continue;
        }
        let message = envelope.model_message();
        if let Some(existing) = history
            .iter()
            .find(|message| message.id() == envelope.receipt.message_id)
        {
            if existing != &message {
                pending.push(envelope);
                pending.extend(batch);
                break;
            }
            if let Some(transcript) = transcript {
                if transcript
                    .record_durable_attachment_once(
                        &message,
                        serde_json::json!({"type":"subagent_handback","envelope":envelope}),
                    )
                    .await
                    .is_err()
                {
                    pending.push(envelope);
                    pending.extend(batch);
                    break;
                }
            }
            if let Some(registry) = &ctx.task_registry {
                registry
                    .acknowledge_handback_consumption(ctx.agent_id, &envelope.receipt)
                    .await;
            }
            continue;
        }
        // Flush preceding ordinary rows before the typed peer row. A failed
        // append never advances history or its watermark, and cannot be retried
        // through generic user-message serialization.
        flush_transcript(transcript, history, written).await;
        if transcript.is_some() && *written != history.len() {
            pending.push(envelope);
            pending.extend(batch);
            break;
        }
        if let Some(transcript) = transcript {
            if transcript
                .record_durable_attachment_once(
                    &message,
                    serde_json::json!({"type":"subagent_handback","envelope":envelope}),
                )
                .await
                .is_err()
            {
                pending.push(envelope);
                pending.extend(batch);
                break;
            }
        }
        history.push(message.clone());
        if transcript.is_some() {
            *written = history.len();
        }
        if let Some(registry) = &ctx.task_registry {
            registry
                .acknowledge_handback_consumption(ctx.agent_id, &envelope.receipt)
                .await;
        }
        emit_message(out_tx, ctx.agent_id, &message).await;
    }
}

async fn configure_handback_run(
    ctx: &SubagentContext,
    history: &mut Vec<ConversationMessage>,
    schemas: &mut Vec<lingxi_core::types::utf16_json::Utf16JsonProjection>,
    allowed: &mut Vec<String>,
    active: bool,
    out_tx: &mpsc::Sender<SubagentEvent>,
) {
    use lingxi_core::host::handback::*;
    if ctx
        .handback
        .as_ref()
        .is_some_and(|runtime| runtime.eligible)
    {
        schemas.retain(|tool| {
            tool.get("name").and_then(serde_json::Value::as_str) != Some(HANDBACK_TOOL_NAME)
        });
        allowed.retain(|name| name != HANDBACK_TOOL_NAME);
        if active {
            let tool =
                crate::handback::SubagentHandbackTool(ctx.handback.as_ref().unwrap().clone());
            schemas.push(lingxi_core::types::utf16_json::Utf16JsonProjection::plain(serde_json::json!({"name":HANDBACK_TOOL_NAME,"description":HANDBACK_PROMPT,"input_schema":tool_api::Tool::input_schema(&tool)})));
            allowed.push(HANDBACK_TOOL_NAME.into());
        }
    }
    let latest = latest_handback_instruction(history);
    let reminder = if active && latest != Some(HandbackInstruction::Reminder) {
        Some(HANDBACK_REMINDER)
    } else if !active && latest == Some(HandbackInstruction::Reminder) {
        Some(HANDBACK_COUNTERMAND)
    } else {
        None
    };
    if let Some(reminder) = reminder {
        let message = ConversationMessage::user_meta(
            MessageId::new(),
            format!("<system-reminder>\n{reminder}\n</system-reminder>"),
        );
        history.push(message.clone());
        emit_message(out_tx, ctx.agent_id, &message).await;
    }
}

async fn finalize_handback_result(
    ctx: &SubagentContext,
    result: &mut serde_json::Value,
) -> Option<lingxi_core::host::handback::HandbackState> {
    let runtime = ctx.handback.as_ref()?;
    let waiting = runtime
        .registry
        .agent_waiting_on_owned_work(ctx.agent_id)
        .await;
    let (state, text) = runtime.finalize(waiting, ctx.persistent || waiting).await?;
    if let (Some(result), Some(text)) = (result.as_object_mut(), text) {
        result.insert("text".into(), serde_json::Value::String(text.clone()));
        result.insert(
            "content".into(),
            serde_json::json!([{"type":"text","text":text}]),
        );
    }
    Some(state)
}

async fn park_foreground_owner(
    ctx: &SubagentContext,
    result: &serde_json::Value,
    usage: &llm_runtime::ExecutionUsage,
    tool_uses: u64,
    duration_ms: u64,
) -> bool {
    if ctx.persistent {
        return false;
    }
    let Some(registry) = &ctx.task_registry else {
        return false;
    };
    registry
        .park_foreground_agent(
            ctx.agent_id,
            lingxi_core::host::task_registry::AgentTerminalOutcome {
                handback: match &ctx.handback {
                    Some(runtime) => runtime.state().await,
                    None => None,
                },
                result: result
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
                usage: Some(lingxi_core::host::task_registry::AgentRunUsage {
                    subagent_tokens: crate::handle::subagent_usage_from_llm_usage(usage)
                        .total_tokens,
                    tool_uses,
                    duration_ms,
                }),
                max_turns_reached: result.get("max_turns").and_then(serde_json::Value::as_u64),
                ..Default::default()
            },
        )
        .await
}

/// Fold only at model boundaries or while parked, never racing the provider future.
async fn fold_task_notifications(
    ctx: &SubagentContext,
    history: &mut Vec<ConversationMessage>,
) -> bool {
    let Some(registry) = &ctx.task_registry else {
        return false;
    };
    let notifications = registry
        .take_pending_task_notifications_for(Some(ctx.agent_id))
        .await
        .unwrap_or_default();
    reconcile_agent_child_keepalives(ctx, registry.as_ref()).await;
    let reminders = lingxi_core::host::task_notification::render_reminders_with_options(
        &notifications,
        false,
        telemetry::push_notifications_enabled(),
    );
    let human = registry.take_human_task_messages_for(ctx.agent_id).await;
    let any = !reminders.is_empty() || !human.is_empty();
    if any {
        registry
            .activate_agent_for_task_notification(ctx.agent_id)
            .await;
    }
    for reminder in reminders {
        history.push(ConversationMessage::user_meta(MessageId::new(), reminder));
    }
    for message in human {
        history.push(ConversationMessage::user_meta(MessageId::new(), message));
    }
    any
}

/// Native `Yq` reconciles `agent:<child>` reasons against the current local
/// Agent rows. A missing row, a row already notified, or a row whose creator
/// changed no longer keeps this parent alive. Other reason families are owned
/// by their own task producers and are deliberately left untouched.
async fn reconcile_agent_child_keepalives(
    ctx: &SubagentContext,
    registry: &dyn lingxi_core::host::task_registry::TaskRegistryHandle,
) {
    let Ok(rows) = registry
        .list(lingxi_core::host::task_registry::TaskListFilter::default())
        .await
    else {
        return;
    };
    let parent_id = ctx.agent_id.to_string();
    let parent_stable_id = ctx.agent_id.as_uuid().to_string();
    let Some(parent) = rows.iter().find(|row| {
        row.task_type == "local_agent"
            && row.agent_facts.as_ref().is_some_and(|facts| {
                facts.stable_agent_id
                    == lingxi_core::host::task_registry::FieldPresence::Value(
                        parent_stable_id.clone(),
                    )
            })
    }) else {
        return;
    };
    let Some(lingxi_core::host::task_registry::FieldPresence::Value(reasons)) = parent
        .agent_facts
        .as_ref()
        .map(|facts| &facts.keepalive_reasons)
    else {
        return;
    };
    for reason in reasons {
        let Some(child_id) = reason.strip_prefix("agent:") else {
            continue;
        };
        let child_keeps_parent = rows.iter().any(|child| {
            child.task_type == "local_agent"
                && child.agent_facts.as_ref().is_some_and(|facts| {
                    facts.stable_agent_id
                        == lingxi_core::host::task_registry::FieldPresence::Value(
                            child_id.to_string(),
                        )
                        && facts.parent_id
                            == lingxi_core::host::task_registry::FieldPresence::Value(
                                serde_json::Value::String(parent_id.clone()),
                            )
                })
                && !child.notified
        });
        if child_keeps_parent {
            continue;
        }
        if let Err(error) = registry
            .update_agent_list_local_fact(
                ctx.agent_id,
                lingxi_core::host::task_registry::AgentListLocalFactUpdate::KeepaliveReason {
                    reason: reason.clone(),
                    active: false,
                },
            )
            .await
        {
            tracing::debug!(
                parent_agent_id = %ctx.agent_id,
                child_agent_id = child_id,
                %error,
                "could not reconcile a child-agent keepalive reason"
            );
        }
    }
}

fn accumulate_usage(acc: &mut llm_runtime::ExecutionUsage, turn: &llm_runtime::ExecutionUsage) {
    let current = acc.counts();
    let next = turn.counts();
    let counts = acc.counts_mut();
    counts.input_tokens = current.input_tokens.saturating_add(next.input_tokens);
    counts.output_tokens = current.output_tokens.saturating_add(next.output_tokens);
    counts.cache_write_tokens = current
        .cache_write_tokens
        .saturating_add(next.cache_write_tokens);
    counts.cache_read_tokens = current
        .cache_read_tokens
        .saturating_add(next.cache_read_tokens);
    counts.cache_write_1h_tokens = current
        .cache_write_1h_tokens
        .saturating_add(next.cache_write_1h_tokens);
    counts.reasoning_tokens = current
        .reasoning_tokens
        .saturating_add(next.reasoning_tokens);
}

/// Group `messages` into atomic trim units: an assistant message whose
/// content is entirely (or partly) `ToolUse` blocks, immediately followed by
/// a user message whose content is entirely `ToolResult` blocks, is one unit
/// — every other message is its own unit. Every provider rejects a
/// `tool_result` with no matching `tool_use` in the same request (and vice
/// versa), so [`cap_input_bytes`] must never keep one half of such a pair.
fn tool_pair_units(
    messages: &[lingxi_core::types::ConversationMessage],
) -> Vec<&[lingxi_core::types::ConversationMessage]> {
    let mut units = Vec::new();
    let mut i = 0;
    while i < messages.len() {
        let is_tool_use_turn = matches!(
            &messages[i],
            lingxi_core::types::ConversationMessage::Assistant { content, .. }
                if content.iter().any(|b| matches!(b, lingxi_core::types::ContentBlock::ToolUse { .. }))
        );
        if is_tool_use_turn && i + 1 < messages.len() {
            let next_is_all_tool_result = matches!(
                &messages[i + 1],
                lingxi_core::types::ConversationMessage::User { content, .. }
                    if !content.is_empty()
                        && content.iter().all(|b| matches!(b, lingxi_core::types::ContentBlock::ToolResult { .. }))
            );
            if next_is_all_tool_result {
                units.push(&messages[i..=i + 1]);
                i += 2;
                continue;
            }
        }
        units.push(&messages[i..=i]);
        i += 1;
    }
    units
}

/// Cached measurements for the exact compact JSON representation of one
/// borrowed trimming unit. `payload_bytes` excludes the unit's `[` and `]`;
/// compact serde_json sequences can then be joined with one comma without
/// cloning or serializing a growing candidate on every fit check.
struct SerializedInputUnit {
    serialized_bytes: u64,
    payload_bytes: Option<u64>,
}

#[derive(Default)]
struct SerializedByteCounter {
    bytes: u64,
}

impl std::io::Write for SerializedByteCounter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let bytes = u64::try_from(buf.len())
            .map_err(|_| std::io::Error::other("serialized input length overflow"))?;
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .ok_or_else(|| std::io::Error::other("serialized input length overflow"))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
std::thread_local! {
    /// Test-only observation point for unit serializations and bytes visited.
    /// Thread-local state keeps parallel module tests independent and compiles
    /// entirely out of production builds.
    static CAP_INPUT_MEASUREMENTS: std::cell::Cell<(usize, u64)> = const {
        std::cell::Cell::new((0, 0))
    };
}

#[cfg(test)]
fn reset_cap_input_measurements() {
    CAP_INPUT_MEASUREMENTS.set((0, 0));
}

#[cfg(test)]
fn cap_input_measurements() -> (usize, u64) {
    CAP_INPUT_MEASUREMENTS.get()
}

fn measure_serialized_input_unit(
    messages: &[lingxi_core::types::ConversationMessage],
) -> SerializedInputUnit {
    // A slice serializes as the same compact JSON array as the old flattened
    // Vec. Count the borrowed serialization directly instead of allocating a
    // temporary byte buffer. Keep the u64::MAX sentinel used by the old helper
    // for either a serde failure or a length overflow.
    let mut counter = SerializedByteCounter::default();
    let serialized_bytes = serde_json::to_writer(&mut counter, messages)
        .ok()
        .map(|()| counter.bytes);
    #[cfg(test)]
    CAP_INPUT_MEASUREMENTS.set({
        let (units, bytes) = CAP_INPUT_MEASUREMENTS.get();
        (
            units.saturating_add(1),
            bytes.saturating_add(serialized_bytes.unwrap_or_default()),
        )
    });
    let payload_bytes = serialized_bytes.and_then(|bytes| bytes.checked_sub(2));
    SerializedInputUnit {
        serialized_bytes: serialized_bytes.unwrap_or(u64::MAX),
        payload_bytes,
    }
}

/// Append one already-measured non-empty JSON array fragment to another
/// compact JSON array. The result is exactly the byte count of the flattened
/// candidate: the fragment contributes its contents and one inter-unit comma.
fn append_serialized_input_unit(current_bytes: u64, unit: &SerializedInputUnit) -> u64 {
    let Some(payload_bytes) = unit.payload_bytes else {
        return u64::MAX;
    };
    current_bytes
        .checked_add(payload_bytes)
        .and_then(|bytes| bytes.checked_add(1))
        .unwrap_or(u64::MAX)
}

fn cap_input_bytes(
    messages: &[lingxi_core::types::ConversationMessage],
    max_bytes: Option<u64>,
) -> Result<Vec<lingxi_core::types::ConversationMessage>, String> {
    let Some(max) = max_bytes else {
        return Ok(messages.to_vec());
    };
    let units = tool_pair_units(messages);
    if units.is_empty() {
        return Ok(Vec::new());
    };

    // The first seeded task/fork unit is mandatory: Fusion puts its entire
    // task text here and never re-injects it on later turns. Sending it whole
    // when over-cap violated the cap; dropping it silently violated task
    // semantics. Measure it first and reject without serializing any optional
    // history when it cannot fit.
    let head = units[0];
    let head_measurement = measure_serialized_input_unit(head);
    let head_bytes = head_measurement.serialized_bytes;
    if head_bytes > max {
        return Err(format!(
            "mandatory initial prompt exceeds max_input_bytes_per_turn ({head_bytes} > {max})"
        ));
    }

    // The newest unit is the current turn's continuation (usually a tool
    // result pair) and is also mandatory. Reject when preserving it together
    // with the seed would exceed the cap rather than silently sending stale,
    // incomplete history. Older units may be dropped as whole units.
    let mut selected_tail: Vec<&[lingxi_core::types::ConversationMessage]> = Vec::new();
    let mut selected_bytes = head_bytes;
    if let Some(newest) = units.get(1..).and_then(|tail| tail.last()).copied() {
        let newest_measurement = measure_serialized_input_unit(newest);
        let head_and_newest_bytes =
            append_serialized_input_unit(selected_bytes, &newest_measurement);
        if head_and_newest_bytes > max {
            let newest_bytes = newest_measurement.serialized_bytes;
            return Err(format!(
                "mandatory latest tool/message unit exceeds max_input_bytes_per_turn when combined with the initial prompt ({head_bytes} + {newest_bytes} > {max})"
            ));
        }
        selected_tail.push(newest);
        selected_bytes = head_and_newest_bytes;
    }

    // Fill from the newest older unit backwards, keeping each tool_use /
    // tool_result pair atomic. A unit that does not fit is dropped and the
    // search continues; no over-cap fallback is permitted. Each optional unit
    // is measured only when reached, once, in newest-to-oldest order.
    if units.len() > 2 {
        for unit in units[1..units.len() - 1].iter().rev().copied() {
            let measurement = measure_serialized_input_unit(unit);
            let candidate_bytes = append_serialized_input_unit(selected_bytes, &measurement);
            if candidate_bytes <= max {
                selected_tail.push(unit);
                selected_bytes = candidate_bytes;
            }
        }
    }

    selected_tail.reverse();
    let mut out =
        Vec::with_capacity(head.len() + selected_tail.iter().map(|unit| unit.len()).sum::<usize>());
    out.extend(head.iter().cloned());
    for unit in selected_tail {
        out.extend(unit.iter().cloned());
    }
    debug_assert!(
        selected_bytes <= max,
        "cap_input_bytes must never return an over-cap request"
    );
    Ok(out)
}

/// Inline runQuery receives userContext separately from durable messages.
/// Its prefix belongs to each outgoing request, never the child JSONL.
fn instruction_request_messages(
    history: &[ConversationMessage],
    context: &lingxi_core::host::instructions::InstructionContext,
    max_bytes: Option<u64>,
) -> Result<Vec<ConversationMessage>, String> {
    if context.rendering != lingxi_core::host::instructions::InstructionRendering::Inline {
        return cap_input_bytes(history, max_bytes);
    }
    let Some(reminder) = context.reminder() else {
        return cap_input_bytes(history, max_bytes);
    };
    let prefix = ConversationMessage::user_meta(MessageId::new(), reminder);
    if history.is_empty() {
        return cap_input_bytes(&[prefix], max_bytes);
    }
    // Reserve the prefix bytes without making it the trimming algorithm's
    // mandatory task unit; the actual initial task must remain mandatory too.
    let history_cap = max_bytes
        .map(|max| {
            let prefix_bytes = measure_serialized_input_unit(std::slice::from_ref(&prefix))
                .serialized_bytes
                .saturating_sub(1);
            max.checked_sub(prefix_bytes)
                .ok_or_else(|| "instruction context exceeds max_input_bytes_per_turn".to_string())
        })
        .transpose()?;
    let history = cap_input_bytes(history, history_cap)?;
    let mut messages = Vec::with_capacity(history.len() + 1);
    messages.push(prefix);
    messages.extend(history);
    Ok(messages)
}

async fn announce_instruction_context(
    ctx: &mut SubagentContext,
    history: &mut Vec<ConversationMessage>,
    transcript: Option<&crate::transcript::AgentTranscriptWriter>,
    written: &mut usize,
    out_tx: &mpsc::Sender<SubagentEvent>,
) {
    use lingxi_core::host::instruction_announcements::{
        context_attachments, render_instruction_attachment,
    };
    if ctx.instruction_context.rendering
        == lingxi_core::host::instructions::InstructionRendering::Inline
    {
        return;
    }
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    for attachment in context_attachments(&ctx.instruction_context, &date, "session_start") {
        let id = MessageId::new();
        let message = render_instruction_attachment(&attachment)
            .map(|text| ConversationMessage::user_meta(id, text));
        flush_transcript(transcript, history, written).await;
        if let Some(writer) = transcript {
            if *written == history.len()
                && writer
                    .record_context_attachment(id, message.as_ref(), attachment.clone())
                    .await
                    .is_ok()
            {
                *written += usize::from(message.is_some());
            }
        }
        if let Some(message) = message {
            history.push(message);
        }
        ctx.instruction_context
            .announcement_history
            .push(attachment.clone());
        let _ = out_tx
            .send(SubagentEvent::Message {
                agent_id: ctx.agent_id,
                message: serde_json::json!({"type":"attachment","uuid":id,"attachment":attachment}),
                message_index: None,
            })
            .await;
    }
}

#[cfg(test)]
#[path = "runner_test.rs"]
mod runner_test;

/// The typed `model_refusal_fallback` system message for a subagent hop.
///
/// `convert_messages` drops every `System` before the wire, so this rides in
/// the run's history and its transcript without becoming model context — which
/// is exactly where claude-code's `ICe` looks for it when the agent finalizes.
///
/// `scope` is `"local"`, not the main thread's `"session"`: a subagent's swap
/// lasts for this run only and does not touch the session model.
fn refusal_fallback_frame(
    id: MessageId,
    banner: &lingxi_core::host::refusal_notice::RefusalNotice,
) -> ConversationMessage {
    ConversationMessage::System {
        api_system: None,
        id,
        content: format!(
            "This model's safeguards flagged this message. Switched to {}.",
            banner.serving_model
        ),
        subtype: Some("model_refusal_fallback".to_string()),
        compact_metadata: None,
        model_fallback: None,
        refusal_fallback: Some(lingxi_core::types::RefusalFallbackMetadata {
            trigger: "refusal".to_string(),
            direction: "retry".to_string(),
            scope: Some("local".to_string()),
            original_model: banner.origin_model.clone(),
            fallback_model: banner.serving_model.clone(),
            request_id: banner.request_id.clone(),
            api_refusal_category: banner.api_refusal_category.clone(),
            retracted_message_uuids: banner.retracted_message_uuids.clone(),
            refused_user_message_uuid: banner.refused_user_message_uuid.clone(),
            ..Default::default()
        }),
    }
}

/// The uuid prefix length `PZo` compares on (claude `D4n = 24`).
const RETRACTED_UUID_PREFIX: usize = 24;

/// `PZo` — drop the messages a refusal notice retracted.
///
/// A cascade that supersedes an earlier hop names the messages that hop
/// produced; replaying them would show the user work the session has already
/// moved past. System messages always survive: the notices themselves are how
/// the retraction is expressed.
fn drop_retracted(history: &[ConversationMessage]) -> Vec<ConversationMessage> {
    let retracted: std::collections::HashSet<String> = history
        .iter()
        .filter_map(|m| match m {
            ConversationMessage::System {
                subtype: Some(subtype),
                refusal_fallback: Some(meta),
                ..
            } if subtype == "model_refusal_fallback" => Some(&meta.retracted_message_uuids),
            _ => None,
        })
        .flatten()
        .map(|u| u.chars().take(RETRACTED_UUID_PREFIX).collect())
        .collect();
    if retracted.is_empty() {
        return history.to_vec();
    }
    let live: Vec<ConversationMessage> = history
        .iter()
        .filter(|m| {
            matches!(m, ConversationMessage::System { .. })
                || !retracted.contains(
                    &m.id()
                        .as_uuid()
                        .to_string()
                        .chars()
                        .take(RETRACTED_UUID_PREFIX)
                        .collect::<String>(),
                )
        })
        .cloned()
        .collect();
    if live.len() != history.len() {
        tracing::info!(
            event = "tengu_resume_retracted_dropped",
            dropped = history.len() - live.len(),
            chain_length = history.len(),
        );
    }
    live
}

/// `ICe`'s notice half — the `scope: "local"` refusal frame that explains the
/// model which actually produced this run's answer.
///
/// Upstream finds the last non-error assistant message with real text, reads
/// its `model`, and matches a frame whose `fallbackModel` equals it. This
/// port's `Assistant` carries no model, but the runner knows the serving model
/// outright — and that IS the model that produced the answer, because every hop
/// retries the turn. So the match is on the same value, not an approximation.
fn local_refusal_notice(live: &[ConversationMessage], serving_model: &str) -> Option<String> {
    live.iter().rev().find_map(|m| match m {
        ConversationMessage::System {
            subtype: Some(subtype),
            refusal_fallback: Some(meta),
            content,
            ..
        } if subtype == "model_refusal_fallback"
            && meta.scope.as_deref() == Some("local")
            && meta.fallback_model == serving_model =>
        {
            Some(content.clone())
        }
        _ => None,
    })
}
