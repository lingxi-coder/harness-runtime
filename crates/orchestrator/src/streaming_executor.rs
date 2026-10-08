//! Faithful port of claude-code StreamingToolExecutor.
//! Schedules streamed tool_use blocks under concurrency control and buffers
//! results in received order. One actor owns dispatch tasks, queue promotion,
//! and generation cancellation in production and tests.

use crate::autonomous_tool_scheduler::{
    CompletedDispatch, OwnedToolCall, ReadyToolMeta, SchedulerStopped, Status as SchedulerStatus,
    ToolDispatch, ToolDispatchPayload, ToolOutcome, ToolScheduler,
};
use crate::conversation::ConversationOrchestrator;
use lingxi_core::host::tool_invoker::ToolInvocationContextModifier;
use lingxi_core::host::tool_use_lifecycle::{
    ToolUseLifecycleTracker, ToolUseRemoval, ToolUseRemovalReason, max_tool_use_concurrency,
};
use lingxi_core::types::{ContentBlock, ConversationMessage, MessageId, ToolUseId};
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tool_api::ContextModifier;
use tool_api::context::ToolUseContext;
use tool_api::tool_trait::ToolStaticContext;

/// claude-code `REJECT_MESSAGE` (utils/messages.ts:212). The user-interrupted
/// synthetic result is this BARE text (NOT `<tool_use_error>`-wrapped, unlike
/// sibling_error/streaming_fallback). The optional memoryCorrectionHint is gated
/// off by default in claude-code, so it is not appended.
const REJECT_MESSAGE: &str = "The user doesn't want to proceed with this tool use. The tool use was rejected (eg. if it was a file edit, the new_string was NOT written to the file). STOP what you are doing and wait for the user to tell you how to proceed.";

/// Claude Code 2.1.246 `J6`: MCP calls aborted before a response must not be
/// normalized into the generic empty-result sentinel.
const MCP_INTERRUPTED_MESSAGE: &str = "The tool call was interrupted before a result was received. It may or may not have completed on the server — verify before assuming it succeeded, and retry if needed.";

/// Result of one `dispatch_tool_uses_tracked` call routed through the executor:
/// the single result block + the tool's injected messages + context modifiers.
type DispatchOutcome = Result<
    (
        ContentBlock,
        bool,
        Vec<(ConversationMessage, ToolUseId)>,
        Vec<ContextModifier>,
        Vec<hooks::events::PostToolBatchCall>,
        Vec<crate::turn_loop::ToolResultPublication>,
    ),
    crate::error::OrchestratorError,
>;

/// Why a tracked tool is being cancelled (TS `getAbortReason`). v2.1.183's
/// `getAbortReason` returns exactly `"streaming_fallback"` (discarded) or
/// `"user_interrupted"` — there is NO sibling/parallel-error abort reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AbortReason {
    /// Emitted when the user interrupts (ESC / new message) an in-flight or
    /// queued tool whose `interrupt_behavior()==Cancel` (TS `getAbortReason`
    /// 'user_interrupted'). The synthetic result is the bare `REJECT_MESSAGE`.
    UserInterrupted,
    StreamingFallback,
}

/// The `toolUseResult` claude persists alongside a synthetic abort block.
///
/// These do NOT mirror the block's model-facing content — claude uses two
/// SHORTER literals on the transcript line:
/// * `user_interrupted` → `vld` (2.1.220 BIN off **238097321**), used at the
///   synthetic site BIN off **232972524** — `"User rejected tool use"`, while
///   the model block carries the long `<tool_use_error>`-wrapped rejection.
/// * `streaming_fallback` → BIN off **232973166** —
///   `"Streaming fallback - tool execution discarded"`, i.e. the model block's
///   text with the `<tool_use_error>Error: ` wrapper stripped.
pub(crate) fn synthetic_tool_use_result(reason: AbortReason) -> serde_json::Value {
    serde_json::Value::String(
        match reason {
            AbortReason::UserInterrupted => "User rejected tool use",
            AbortReason::StreamingFallback => "Streaming fallback - tool execution discarded",
        }
        .to_string(),
    )
}

pub(crate) fn synthetic_tool_use_result_for_tool(
    reason: AbortReason,
    is_mcp: bool,
) -> serde_json::Value {
    if reason == AbortReason::UserInterrupted && is_mcp {
        return serde_json::Value::String(format!("Error: {MCP_INTERRUPTED_MESSAGE}"));
    }
    synthetic_tool_use_result(reason)
}

/// Build the synthetic `tool_result` for a cancelled tool (TS
/// `createSyntheticErrorMessage`). `provider_tool_use_id` is left `None` —
/// the caller copies the tracked tool's `provider_id` in before persisting.
#[cfg(test)]
pub(crate) fn synthetic_error_block(tool_use_id: ToolUseId, reason: AbortReason) -> ContentBlock {
    synthetic_error_block_for_tool(tool_use_id, reason, false)
}

fn synthetic_error_block_for_tool(
    tool_use_id: ToolUseId,
    reason: AbortReason,
    is_mcp: bool,
) -> ContentBlock {
    let content = match reason {
        AbortReason::StreamingFallback => {
            "<tool_use_error>Error: Streaming fallback - tool execution discarded</tool_use_error>"
                .to_string()
        }
        // claude-code (StreamingToolExecutor.ts:160-172) uses the BARE REJECT_MESSAGE
        // here — NOT `<tool_use_error>`-wrapped — with is_error: true. This is the
        // faithful text; the `UserInterrupted` reason itself is only produced once
        // the user-ESC / per-tool cancellation path is wired in a later sub-task.
        AbortReason::UserInterrupted if is_mcp => format!("Error: {MCP_INTERRUPTED_MESSAGE}"),
        AbortReason::UserInterrupted => REJECT_MESSAGE.to_string(),
    };
    ContentBlock::ToolResult { content_projection: None,
        tool_use_id,
        content,
        is_error: Some(true),
        provider_tool_use_id: None,
        content_blocks: None,
    }
}

/// Lifecycle of one tracked tool, mirroring TS `ToolStatus`.
#[allow(dead_code)] // variants wired in Tasks 5-11
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolStatus {
    Queued,
    Executing,
    Completed,
    Yielded,
}

/// One `tool_use` block under management. `assistant_id` is the id of the
/// assistant message that requested this call — it becomes the JSONL
/// `parentUuid` of the result (TS `sourceToolAssistantUUID`).
#[allow(dead_code)] // fields wired in Tasks 5-11
pub(crate) struct TrackedTool {
    pub(crate) id: ToolUseId,
    pub(crate) name: String,
    pub(crate) input: serde_json::Value,
    pub(crate) provider_id: Option<String>,
    pub(crate) assistant_id: MessageId,
    /// Admission-time handle reused by abort policy and autonomous W1 dispatch.
    pub(crate) resolved_tool: Option<Arc<dyn tool_api::tool_trait::Tool>>,
    pub(crate) status: ToolStatus,
    pub(crate) is_concurrency_safe: bool,
    /// The result block once `Completed` (the unknown-tool case fills it
    /// synchronously at `add_tool` time).
    pub(crate) result: Option<ContentBlock>,
    /// A per-tool Pre/PostToolUse hook requested that the loop stop.
    pub(crate) prevent_continuation: bool,
    /// Tool-injected follow-up messages (SKILLEXEC.3) + context modifiers,
    /// threaded through unchanged from `dispatch_tool_uses_tracked`.
    pub(crate) injected: Vec<(ConversationMessage, ToolUseId)>,
    pub(crate) modifiers: Vec<ContextModifier>,
    /// This tool's resolved-call entry, deferred so the streaming driver can
    /// fire one `PostToolBatch` for the complete model-response batch.
    pub(crate) post_tool_batch_calls: Vec<hooks::events::PostToolBatchCall>,
    pub(crate) publications: Vec<crate::turn_loop::ToolResultPublication>,
    /// Immutable request/row facts captured when this live streamed row
    /// completed. Queued tools must not reread a later session snapshot.
    pub(crate) dispatch_facts: Option<crate::turn_loop::ToolUseDispatchFacts>,
}

// ============================================================================
// Task 7: concurrency admission predicate
// ============================================================================

/// TS `canExecuteTool` (line 129-135): a tool may start if nothing is
/// executing, OR if both the candidate and every executing tool are
/// concurrency-safe.
///
/// `executing_safe_flags` is a slice of the `is_concurrency_safe` flags for
/// every tool currently in the `Executing` state.
pub(crate) fn can_execute(executing_safe_flags: &[bool], candidate_safe: bool) -> bool {
    executing_safe_flags.is_empty() || (candidate_safe && executing_safe_flags.iter().all(|&s| s))
}

fn scheduler_stopped(operation: &str) -> crate::error::OrchestratorError {
    crate::error::OrchestratorError::StreamingProtocol(format!(
        "autonomous streaming scheduler stopped while {operation}"
    ))
}

fn scheduler_error(error: SchedulerStopped, operation: &str) -> crate::error::OrchestratorError {
    match error {
        SchedulerStopped::Unavailable => scheduler_stopped(operation),
        SchedulerStopped::ModelResolution(reason) => {
            crate::error::OrchestratorError::StreamingProtocol(format!(
                "tool model preference could not be resolved: {reason}"
            ))
        }
    }
}

// ============================================================================
// StreamingToolExecutor — Task 6: struct + new() + add_tool()
// ============================================================================

/// Manages the lifecycle of all `tool_use` blocks from one streaming assistant
/// turn. Mirrors TS `StreamingToolExecutor` (`StreamingToolExecutor.ts:76-124`
/// for `addTool`).
///
/// All fields are now read by the Task 8 dispatch/abort logic.
pub(crate) struct StreamingToolExecutor<'a> {
    orch: &'a ConversationOrchestrator,
    pub(crate) tools: Vec<TrackedTool>,
    /// The actor owns dispatch tasks and advances the eligible queue on every
    /// admission/completion, independently of provider events and Tn polling.
    scheduler: ToolScheduler,
    /// Current actor generation for accepted rows and PostToolBatch output.
    publication_fence: crate::autonomous_tool_scheduler::ToolDispatchPublicationFence,
    /// Local indices returned by the actor's Tn-ready scan, already in Native
    /// registration/barrier order. The actor, not this facade, decides which
    /// completed records cross the event boundary.
    actor_ready_indices: VecDeque<usize>,
    /// Completed-only suffix yielded by the terminal Tn(false) drain.
    actor_terminal_ready_indices: VecDeque<usize>,
    scheduler_failed: AtomicBool,
    /// User-interrupt token. The actor cancels only Cancel-behavior calls and
    /// synthesizes queued Cancel results; Block calls keep running.
    user_cancel: Option<tokio_util::sync::CancellationToken>,
    /// Outstanding ids survive completion until the result is handed to the
    /// driver, and are removed explicitly when the host sweeps this executor.
    tool_use_lifecycle: ToolUseLifecycleTracker,
    #[cfg(test)]
    next_dispatch_finished_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

impl<'a> StreamingToolExecutor<'a> {
    /// Build the actor-backed scheduler. Runtime construction fails closed unless the finalized
    /// composition root bound this exact orchestrator Arc.
    pub(crate) async fn try_new(
        orch: &'a ConversationOrchestrator,
        query_history: Vec<ConversationMessage>,
    ) -> Result<Self, crate::error::OrchestratorError> {
        Self::try_new_inner(orch, query_history, None).await
    }

    pub(crate) async fn try_new_with_user_cancel(
        orch: &'a ConversationOrchestrator,
        query_history: Vec<ConversationMessage>,
        user_cancel: tokio_util::sync::CancellationToken,
    ) -> Result<Self, crate::error::OrchestratorError> {
        Self::try_new_inner(orch, query_history, Some(user_cancel)).await
    }

    async fn try_new_inner(
        orch: &'a ConversationOrchestrator,
        query_history: Vec<ConversationMessage>,
        user_cancel: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<Self, crate::error::OrchestratorError> {
        let owner = orch
            .upgrade_streaming_tool_dispatch_owner()
            .ok_or_else(|| {
                crate::error::OrchestratorError::StreamingProtocol(
                    "streaming tool scheduler has no finalized orchestrator owner".into(),
                )
            })?;
        if !std::ptr::eq(owner.as_ref(), orch) {
            return Err(crate::error::OrchestratorError::StreamingProtocol(
                "streaming tool scheduler owner does not match the turn orchestrator".into(),
            ));
        }
        let dispatch_owner = Arc::clone(&owner);
        let dispatch: ToolDispatch = Arc::new(move |mut call| {
            let orch = Arc::clone(&dispatch_owner);
            Box::pin(async move {
                let tool_use = (
                    call.id.clone(),
                    call.presented_name.clone(),
                    call.input.clone(),
                    call.provider_id.clone(),
                );
                let dispatch_started = call.dispatch_started_tx.take();
                let publication_fence = call
                    .publication_fence
                    .take()
                    .expect("actor attaches the generation publication fence before dispatch");
                let inherited_context = call
                    .tool_context_state
                    .as_ref()
                    .and_then(|state| state.downcast_arc::<ToolUseContext>().ok())
                    .map(|context| context.as_ref().clone());
                let mut dispatched = crate::turn_loop::dispatch_streaming_tool_use_owned(
                    &orch,
                    &tool_use,
                    Some(call.cancellation.clone()),
                    call.assistant_id,
                    call.facts,
                    Arc::clone(&call.resolved_tool),
                    inherited_context,
                    dispatch_started,
                    publication_fence,
                )
                .await?;
                // ToolApi's FnOnce is moved into Core's opaque one-shot wrapper
                // exactly once. The old DeferredToolDispatch field is emptied,
                // so the final model fold cannot apply this closure a second time.
                let context_modifiers: Vec<ToolInvocationContextModifier> =
                    std::mem::take(&mut dispatched.context_modifiers)
                        .into_iter()
                        .map(ToolInvocationContextModifier::new::<ToolUseContext, _>)
                        .collect();
                Ok(ToolDispatchPayload {
                    dispatch: dispatched,
                    tool_context_state: None,
                    context_modifiers,
                })
            })
        });
        let shared_context =
            crate::turn_loop::streaming_tool_context_base(orch, query_history).await;
        let (session_root, publication_commit_lock) = orch
            .lifecycle_runtime
            .session_tool_hook_generation
            .current();
        let model_provider = orch.model_resolution_context_provider.clone();
        let model_context_resolver: crate::autonomous_tool_scheduler::ModelContextResolver =
            Arc::new(move |previous, updated| {
                crate::turn_loop::resolve_model_context_modifier(
                    previous,
                    updated,
                    model_provider.as_deref(),
                )
            });
        let scheduler = ToolScheduler::spawn(
            dispatch,
            max_tool_use_concurrency(),
            user_cancel.clone(),
            Some(
                lingxi_core::host::tool_invoker::ToolInvocationContextState::new(Arc::new(
                    shared_context,
                )),
            ),
            session_root,
            publication_commit_lock,
            Some(model_context_resolver),
        );
        let publication_fence = scheduler.current_publication_fence();
        Ok(Self {
            orch,
            tools: Vec::new(),
            scheduler,
            publication_fence,
            actor_ready_indices: VecDeque::new(),
            actor_terminal_ready_indices: VecDeque::new(),
            scheduler_failed: AtomicBool::new(false),
            user_cancel,
            tool_use_lifecycle: ToolUseLifecycleTracker::default(),
            #[cfg(test)]
            next_dispatch_finished_tx: None,
        })
    }

    /// Admit a completed streamed ToolUse into the owned scheduler. Known
    /// handles are resolved exactly once here and carried into every core/Mod
    /// dispatch path; unknown tools enter the same actor registration order as
    /// immediately-ready results.
    pub(crate) async fn add_tool_with_context_owned(
        &mut self,
        id: ToolUseId,
        name: String,
        input: serde_json::Value,
        provider_id: Option<String>,
        assistant_id: MessageId,
        dispatch_facts: crate::turn_loop::ToolUseDispatchFacts,
    ) -> Result<(), crate::error::OrchestratorError> {
        let scheduler = &mut self.scheduler;
        let tool = self.orch.find_tool_for_dispatch(&name);
        let is_agent = tool.as_ref().is_some_and(|tool| tool.name() == "Agent");
        self.tool_use_lifecycle
            .observe_assistant_row([(id.clone(), is_agent)]);
        match tool {
            None => {
                let suffix = unknown_tool_suffix_for(&name, self.orch);
                let block = synthetic_unknown_tool(id.clone(), &name, provider_id.clone(), &suffix);
                let meta = ReadyToolMeta {
                    id: id.clone(),
                    presented_name: name.clone(),
                    canonical_name: None,
                    input: input.clone(),
                    provider_id: provider_id.clone(),
                    assistant_id,
                    facts: Some(dispatch_facts.clone()),
                    resolved_tool: None,
                    concurrency_safe: true,
                    is_agent: false,
                };
                let post_tool_batch_calls =
                    vec![post_tool_batch_call_for_result(&id, &name, &input, &block)];
                self.tools.push(TrackedTool {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                    provider_id: provider_id.clone(),
                    assistant_id,
                    resolved_tool: None,
                    status: ToolStatus::Completed,
                    is_concurrency_safe: true,
                    result: Some(block.clone()),
                    prevent_continuation: false,
                    injected: Vec::new(),
                    modifiers: Vec::new(),
                    post_tool_batch_calls: post_tool_batch_calls.clone(),
                    publications: Vec::new(),
                    dispatch_facts: Some(dispatch_facts),
                });
                let publication = unknown_tool_publication(self.orch, &id, &name, &block);
                let outcome = Ok(ToolDispatchPayload {
                    dispatch: crate::turn_loop::DeferredToolDispatch {
                        results: vec![block],
                        prevent_continuation: false,
                        injected_messages: Vec::new(),
                        context_modifiers: Vec::new(),
                        post_tool_batch_calls,
                        publications: vec![publication],
                    },
                    tool_context_state: None,
                    context_modifiers: Vec::new(),
                });
                scheduler.add_ready(meta, outcome).await.map_err(|error| {
                    scheduler_error(error, "registering an unknown streamed tool")
                })?;
            }
            Some(resolved_tool) => {
                let safe =
                    crate::schema_validation::validate_tool_schema(resolved_tool.as_ref(), &input)
                        .is_ok()
                        && resolved_tool.is_concurrency_safe(&input);
                let canonical_name = resolved_tool.name().to_owned();
                self.tools.push(TrackedTool {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                    provider_id: provider_id.clone(),
                    assistant_id,
                    resolved_tool: Some(Arc::clone(&resolved_tool)),
                    status: ToolStatus::Queued,
                    is_concurrency_safe: safe,
                    result: None,
                    prevent_continuation: false,
                    injected: Vec::new(),
                    modifiers: Vec::new(),
                    post_tool_batch_calls: Vec::new(),
                    publications: Vec::new(),
                    dispatch_facts: Some(dispatch_facts.clone()),
                });
                let (dispatch_started_tx, dispatch_started_rx) = tokio::sync::oneshot::channel();
                let started = scheduler
                    .add(OwnedToolCall {
                        id,
                        presented_name: name,
                        canonical_name,
                        input,
                        provider_id,
                        assistant_id,
                        facts: dispatch_facts,
                        resolved_tool,
                        concurrency_safe: safe,
                        cancellation: tokio_util::sync::CancellationToken::new(),
                        publication_fence: None,
                        dispatch_started_tx: Some(dispatch_started_tx),
                        dispatch_started_rx: Some(dispatch_started_rx),
                        #[cfg(test)]
                        dispatch_finished_tx: self.next_dispatch_finished_tx.take(),
                        tool_context_state: None,
                    })
                    .await
                    .map_err(|error| scheduler_error(error, "admitting a streamed tool"))?;
                if started {
                    if let Some(tool) = self.tools.last_mut() {
                        tool.status = ToolStatus::Executing;
                    }
                }
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) async fn add_tool_observed_dispatch(
        &mut self,
        id: ToolUseId,
        name: String,
        input: serde_json::Value,
        provider_id: Option<String>,
        assistant_id: MessageId,
    ) -> tokio::sync::oneshot::Receiver<()> {
        let (finished_tx, finished_rx) = tokio::sync::oneshot::channel();
        assert!(self.next_dispatch_finished_tx.is_none());
        self.next_dispatch_finished_tx = Some(finished_tx);
        self.add_tool(id, name, input, provider_id, assistant_id)
            .await;
        finished_rx
    }

    #[cfg(test)]
    pub(crate) async fn add_tool(
        &mut self,
        id: ToolUseId,
        name: String,
        input: serde_json::Value,
        provider_id: Option<String>,
        assistant_id: MessageId,
    ) {
        let facts = crate::turn_loop::ToolUseDispatchFacts {
            query_history: Vec::new(),
            assistant_message: ConversationMessage::Assistant { per_turn_effort: None,
                id: assistant_id,
                content: vec![ContentBlock::ToolUse { input_projection: None,
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                    provider_id: provider_id.clone(),
                }],
                stop_reason: None,
            },
            same_turn_tool_uses: Vec::new(),
        };
        self.add_tool_with_context_owned(id, name, input, provider_id, assistant_id, facts)
            .await
            .expect("test tool admission reaches the owned scheduler");
    }

    #[cfg(test)]
    pub(crate) async fn add_tool_with_context(
        &mut self,
        id: ToolUseId,
        name: String,
        input: serde_json::Value,
        provider_id: Option<String>,
        assistant_id: MessageId,
        dispatch_facts: crate::turn_loop::ToolUseDispatchFacts,
    ) {
        self.add_tool_with_context_owned(
            id,
            name,
            input,
            provider_id,
            assistant_id,
            dispatch_facts,
        )
        .await
        .expect("test tool admission reaches the owned scheduler");
    }

    #[cfg(test)]
    pub(crate) fn inflight_is_empty(&self) -> bool {
        !self
            .tools
            .iter()
            .any(|tool| tool.status == ToolStatus::Executing)
    }

    async fn sync_statuses(&mut self) {
        let statuses = match self.scheduler.statuses().await {
            Ok(statuses) => statuses,
            Err(_) => {
                self.scheduler_failed.store(true, Ordering::Release);
                return;
            }
        };
        for (id, status) in statuses {
            let Some(tool) = self.tools.iter_mut().find(|tool| tool.id == id) else {
                continue;
            };
            if tool.status == ToolStatus::Yielded {
                continue;
            }
            tool.status = match status {
                SchedulerStatus::Queued => ToolStatus::Queued,
                SchedulerStatus::Executing => ToolStatus::Executing,
                // The status snapshot does not transfer the completion payload.
                // Keep the local state until Tn folds that payload exactly once.
                SchedulerStatus::Completed | SchedulerStatus::Yielded => tool.status,
            };
        }
    }

    pub(crate) fn orchestrator(&self) -> &'a ConversationOrchestrator {
        self.orch
    }

    pub(crate) async fn is_current_generation_idle(&mut self) -> bool {
        match self.scheduler.is_current_generation_idle().await {
            Ok(idle) => idle,
            Err(_) => {
                self.scheduler_failed.store(true, Ordering::Release);
                true
            }
        }
    }

    /// Apply the actor's already-folded context after the normal final drain.
    /// Per-event Tn only sees the context snapshot carried by its ready rows.
    pub(crate) async fn finish_context_layers(
        &mut self,
    ) -> Result<(), crate::error::OrchestratorError> {
        if self.scheduler_failed.load(Ordering::Acquire) {
            return Err(scheduler_stopped(
                "finishing context layers after an actor failure",
            ));
        }
        let state = self
            .scheduler
            .finish_normal()
            .await
            .map_err(|error| scheduler_error(error, "finishing context layers"))?;
        if let Some(state) = state {
            crate::turn_loop::apply_model_context_state(self.orch, state).await?;
        }
        Ok(())
    }

    /// Apply append-through to the canonical row at its stop-time boundary.
    /// Query/W1 and persistence consume the accepted content; the source
    /// ToolUse identity/input remain the separately registered dispatch facts.
    pub(crate) async fn append_assistant_row(
        &mut self,
        row: &mut crate::streaming_loop::CompletedAssistantRow,
    ) {
        self.orch
            .append_completed_assistant_row(row, Some(Arc::new(self.publication_fence.clone())))
            .await;
    }

    pub(crate) fn publication_fence(
        &self,
    ) -> crate::autonomous_tool_scheduler::ToolDispatchPublicationFence {
        self.publication_fence.clone()
    }

    /// `getAbortReason` for user interruption. Streaming fallback and host
    /// abandonment reset the whole scheduler generation instead of
    /// manufacturing per-call completions.
    fn abort_reason_for(&self, i: usize) -> Option<AbortReason> {
        let token = self.user_cancel.as_ref()?;
        if !token.is_cancelled() {
            return None;
        }
        let behavior = self.tools[i]
            .resolved_tool
            .as_ref()
            .map(Arc::clone)
            .or_else(|| self.orch.tools.find_by_name(&self.tools[i].name))
            .map(|tool| tool.interrupt_behavior(&self.tools[i].input));
        matches!(
            behavior,
            Some(tool_api::tool_trait::InterruptBehavior::Cancel)
        )
        .then_some(AbortReason::UserInterrupted)
    }

    /// Wait for autonomous task progress until Native Tn has ready data or the
    /// generation becomes idle. Queue promotion is owned by the actor and does
    /// not depend on this provider-loop poll.
    pub(crate) async fn drain_one(&mut self) -> Option<usize> {
        loop {
            if self.scheduler.wait_for_progress().await.is_err() {
                self.scheduler_failed.store(true, Ordering::Release);
                return None;
            }
            let drained = self.drain_ready().await;
            if drained > 0 {
                return self.actor_ready_indices.back().copied();
            }
            match self.scheduler.is_current_generation_idle().await {
                Ok(true) => return None,
                Ok(false) => {}
                Err(_) => {
                    self.scheduler_failed.store(true, Ordering::Release);
                    return None;
                }
            }
        }
    }

    /// Nonblocking Native Tn scan. The owned actor has already collected any
    /// W1 completion and advanced its queue without requiring this event poll.
    pub(crate) async fn drain_ready(&mut self) -> usize {
        let ready = match self.scheduler.take_ready().await {
            Ok(ready) => ready,
            Err(_) => {
                self.scheduler_failed.store(true, Ordering::Release);
                tracing::error!("autonomous streaming scheduler stopped before Tn");
                return 0;
            }
        };
        let count = self.ingest_owned_dispatches(ready).await;
        self.sync_statuses().await;
        count
    }

    /// Ready-only terminal Tn(false) followed by the completed-only suffix.
    /// Neither actor scan schedules a newly queued call.
    pub(crate) async fn drain_ready_without_queue(&mut self) -> usize {
        if self.scheduler.stop_scheduling().await.is_err() {
            self.scheduler_failed.store(true, Ordering::Release);
            tracing::error!("autonomous scheduler stopped before terminal scheduling barrier");
            return 0;
        }
        let first = match self.scheduler.take_ready().await {
            Ok(ready) => ready,
            Err(_) => {
                self.scheduler_failed.store(true, Ordering::Release);
                tracing::error!("autonomous streaming scheduler stopped before terminal Tn");
                return 0;
            }
        };
        let mut completed = self.ingest_owned_dispatches(first).await;
        let rest = match self.scheduler.take_all_ready().await {
            Ok(ready) => ready,
            Err(_) => {
                self.scheduler_failed.store(true, Ordering::Release);
                tracing::error!("autonomous streaming scheduler stopped before terminal drain");
                return completed;
            }
        };
        completed += self.ingest_owned_dispatches_to_terminal(rest).await;
        self.sync_statuses().await;
        completed
    }

    async fn ingest_owned_dispatches(&mut self, ready: Vec<CompletedDispatch>) -> usize {
        self.ingest_owned_dispatches_into(ready, false).await
    }

    async fn ingest_owned_dispatches_to_terminal(
        &mut self,
        ready: Vec<CompletedDispatch>,
    ) -> usize {
        self.ingest_owned_dispatches_into(ready, true).await
    }

    async fn ingest_owned_dispatches_into(
        &mut self,
        ready: Vec<CompletedDispatch>,
        terminal: bool,
    ) -> usize {
        let mut count = 0;
        for completed in ready {
            let Some(index) = self
                .tools
                .iter()
                .position(|tool| tool.id == completed.meta.id)
            else {
                tracing::error!(tool_use_id = %completed.meta.id, "scheduler returned an unregistered tool");
                continue;
            };
            let outcome = match completed.outcome {
                Ok(payload) => {
                    let dispatch = payload.dispatch;
                    let mut results = dispatch.results.into_iter();
                    match (results.next(), results.next()) {
                        (Some(block), None) => Ok((
                            block,
                            dispatch.prevent_continuation,
                            dispatch.injected_messages,
                            dispatch.context_modifiers,
                            dispatch.post_tool_batch_calls,
                            dispatch.publications,
                        )),
                        _ => Err(crate::error::OrchestratorError::StreamingProtocol(
                            "autonomous scheduler dispatch returned an invalid result count".into(),
                        )),
                    }
                }
                Err(error) => Err(error),
            };
            // The actor transfers each outcome once. Fold it even when an
            // earlier status observation or unknown-tool fast path is complete.
            self.record_completion(index, outcome).await;
            if terminal {
                self.actor_terminal_ready_indices.push_back(index);
            } else {
                self.actor_ready_indices.push_back(index);
            }
            count += 1;
        }
        count
    }

    async fn record_completion(&mut self, i: usize, outcome: DispatchOutcome) {
        // Compute the abort reason from PRIOR state (discard / user-interrupt). A
        // cancelled in-flight tool's real outcome is discarded for the synthetic.
        let abort_reason = self.abort_reason_for(i);
        // Cancelled in-flight tool: discard its real outcome for the synthetic.
        if let Some(reason) = abort_reason {
            // A completed real MCP result may already have recorded `_meta`
            // and an end-turn request before the executor notices the abort.
            // The synthetic error is the result that survives, so neither may
            // leak onto its transcript line or terminate the turn.
            self.orch
                .clear_discarded_tool_result_metadata(&self.tools[i].id)
                .await;
            // Denial-kind housekeeping: the dispatch-site catch may already have
            // recorded a kind (`interrupted` for a `ToolError::Aborted`, or
            // `cancelled` from the pre-cancel guard) for the block we are about
            // to THROW AWAY. `take_tool_denial_kind` removes on read, so a stale
            // entry would both mis-stamp the synthetic and leak. claude-code's
            // `createSyntheticErrorMessage` (2.1.220 @232972360) attaches
            // `toolDenialKind:"user-rejected"` to the `user_interrupted`
            // synthetic (@232972524) and NO kind to `streaming_fallback` /
            // `conversation_ended`, so follow the block that actually survives.
            let is_mcp = self.tools[i]
                .resolved_tool
                .as_ref()
                .map_or_else(
                    || self.orch.tools.find_by_name(&self.tools[i].name),
                    |tool| Some(Arc::clone(tool)),
                )
                .is_some_and(|tool| tool.is_mcp());
            let denial_kind = match reason {
                AbortReason::UserInterrupted => Some(if is_mcp {
                    "interrupted".to_owned()
                } else {
                    "user-rejected".to_owned()
                }),
                AbortReason::StreamingFallback => None,
            };
            let tool_use_result = synthetic_tool_use_result_for_tool(reason, is_mcp);
            let mut block =
                synthetic_error_block_for_tool(self.tools[i].id.clone(), reason, is_mcp);
            set_provider_id(&mut block, self.tools[i].provider_id.clone());
            let content = match &block {
                ContentBlock::ToolResult { content, .. } => content.clone(),
                _ => unreachable!("synthetic abort result is a ToolResult"),
            };
            let mut publication = crate::turn_loop::ToolResultPublication::frame_only(
                &self.tools[i].id,
                &self.tools[i].name,
                &content,
                serde_json::json!({ "error": content }),
            );
            publication.tool_use_result = Some((tool_use_result).into());
            publication.denial_kind = denial_kind;
            self.tools[i].publications = vec![publication];
            let post_tool_batch_call = post_tool_batch_call_for_result(
                &self.tools[i].id,
                &self.tools[i].name,
                &self.tools[i].input,
                &block,
            );
            self.tools[i].post_tool_batch_calls = vec![post_tool_batch_call];
            self.tools[i].result = Some(block);
            // A cancelled tool yields ONLY the synthetic — its injected msgs/modifiers are dropped.
            self.tools[i].status = ToolStatus::Completed;
            return;
        }
        // Otherwise record the real outcome (existing handling).
        match outcome {
            Ok((mut block, prevent, injected, modifiers, post_tool_batch_calls, publications)) => {
                // Copy the provider id onto the result for egress replay.
                set_provider_id(&mut block, self.tools[i].provider_id.clone());
                self.tools[i].result = Some(block);
                self.tools[i].prevent_continuation = prevent;
                self.tools[i].injected = injected;
                self.tools[i].modifiers = modifiers;
                self.tools[i].post_tool_batch_calls = post_tool_batch_calls;
                self.tools[i].publications = publications;
                self.tools[i].status = ToolStatus::Completed;
            }
            Err(e) => {
                // A hard orchestrator error → surface as an errored result,
                // matching claude-code's outer plumbing catch
                // (toolExecution.ts:471-480): `Error calling tool (<name>): <msg>`
                // wrapped in `<tool_use_error>`.
                let name = &self.tools[i].name;
                let block = ContentBlock::ToolResult { content_projection: None,
                    tool_use_id: self.tools[i].id.clone(),
                    content: format!(
                        "<tool_use_error>Error calling tool ({name}): {e}</tool_use_error>"
                    ),
                    is_error: Some(true),
                    provider_tool_use_id: self.tools[i].provider_id.clone(),
                    content_blocks: None,
                };
                let post_tool_batch_call = post_tool_batch_call_for_result(
                    &self.tools[i].id,
                    &self.tools[i].name,
                    &self.tools[i].input,
                    &block,
                );
                self.tools[i].post_tool_batch_calls = vec![post_tool_batch_call];
                self.tools[i].result = Some(block);
                self.tools[i].status = ToolStatus::Completed;
            }
        }
    }

    /// Reconcile user cancellation in the autonomous actor. The actor cancels
    /// only executing `InterruptBehavior::Cancel` calls and converts queued
    /// Cancel calls to completed records; Block calls keep running/queued.
    pub(crate) async fn apply_abort_to_pending_owned(&mut self) {
        if self.scheduler.apply_user_interrupt().await.is_err() {
            self.scheduler_failed.store(true, Ordering::Release);
            tracing::error!("autonomous scheduler stopped while applying user interruption");
        }
    }

    /// Reset the owned actor generation after accepted fallback handling. The
    /// scheduler synchronously cancels its generation root before acknowledging
    /// the reset, so late old-generation modifiers cannot reach the queue.
    pub(crate) async fn reset_after_server_fallback_owned(
        &mut self,
        reason: Option<ToolUseRemovalReason>,
    ) -> Result<ToolUseRemoval, crate::error::OrchestratorError> {
        let removal = self
            .scheduler
            .reset_after_server_fallback(reason)
            .await
            .map_err(|_| {
                self.scheduler_failed.store(true, Ordering::Release);
                scheduler_stopped("resetting after server fallback")
            })?;
        self.publication_fence = self.scheduler.current_publication_fence();
        self.tools.clear();
        self.actor_ready_indices.clear();
        self.actor_terminal_ready_indices.clear();
        self.tool_use_lifecycle.apply_removal(&removal);
        Ok(removal)
    }

    /// Drain every already-completed result and discard all remaining tool
    /// work after a provider error. Queued calls are never started, running
    /// calls receive the shared abort signal and their generation is fenced,
    /// and each unmatched tool use receives Native's terminal-error synthetic.
    pub(crate) async fn abandon_after_model_error(
        &mut self,
        error: &str,
    ) -> (Vec<DrainedResult>, ToolUseRemoval) {
        if self.scheduler.stop_scheduling().await.is_err() {
            tracing::error!("autonomous scheduler stopped while entering model-error drain");
        }
        self.sync_statuses().await;
        let mut unmatched = Vec::new();
        let unmatched_facts = self
            .tools
            .iter()
            .filter(|tool| matches!(tool.status, ToolStatus::Queued | ToolStatus::Executing))
            .map(|tool| {
                (
                    tool.id.clone(),
                    tool.provider_id.clone(),
                    tool.assistant_id,
                    tool.name.clone(),
                )
            })
            .collect::<Vec<_>>();
        for (id, provider_id, assistant_id, name) in unmatched_facts {
            self.orch.clear_discarded_tool_result_metadata(&id).await;
            let result_text = terminal_error_tool_result(error);
            let block = ContentBlock::ToolResult { content_projection: None,
                tool_use_id: id,
                content: result_text.clone(),
                is_error: Some(true),
                provider_tool_use_id: provider_id,
                content_blocks: None,
            };
            unmatched.push(DrainedResult {
                assistant_id,
                publication_guard: Some(Arc::new(self.publication_fence.clone())),
                tool: name,
                block,
                prevent_continuation: false,
                injected: Vec::new(),
                modifiers: Vec::new(),
                post_tool_batch_calls: Vec::new(),
                publications: Vec::new(),
                tool_use_result: Some(serde_json::Value::String(result_text).into()),
            });
        }

        let removal = ToolUseRemoval {
            ids: self.tools.iter().map(|tool| tool.id.clone()).collect(),
            reason: None,
        };
        match self.scheduler.reset_after_server_fallback(None).await {
            Ok(actor_removal) => {
                self.tool_use_lifecycle.apply_removal(&actor_removal);
            }
            Err(_) => {
                tracing::error!("autonomous scheduler stopped while abandoning model-error work")
            }
        }
        self.tools.clear();
        self.actor_ready_indices.clear();
        self.actor_terminal_ready_indices.clear();
        (unmatched, removal)
    }

    /// Cancel and drop the rest of a failed/abandoned attempt without creating
    /// user-facing tool-result synthetics. Used for host failures and chain
    /// advancement, whose Native paths discard the attempt instead.
    pub(crate) async fn abandon_without_synthetics(&mut self) -> ToolUseRemoval {
        if self.scheduler.stop_scheduling().await.is_err() {
            tracing::error!("autonomous scheduler stopped while abandoning tool work");
        }
        let removal = ToolUseRemoval {
            ids: self.tools.iter().map(|tool| tool.id.clone()).collect(),
            reason: None,
        };
        let discarded_ids = self
            .tools
            .iter()
            .filter(|tool| matches!(tool.status, ToolStatus::Queued | ToolStatus::Executing))
            .map(|tool| tool.id.clone())
            .collect::<Vec<_>>();
        for id in discarded_ids {
            self.orch.clear_discarded_tool_result_metadata(&id).await;
        }
        match self.scheduler.reset_after_server_fallback(None).await {
            Ok(actor_removal) => {
                self.tool_use_lifecycle.apply_removal(&actor_removal);
            }
            Err(_) => {
                tracing::error!("autonomous scheduler stopped while abandoning tool work")
            }
        }
        self.tools.clear();
        self.actor_ready_indices.clear();
        self.actor_terminal_ready_indices.clear();
        removal
    }

    #[cfg(test)]
    pub(crate) async fn apply_abort_to_pending(&mut self) {
        self.apply_abort_to_pending_owned().await;
    }

    #[cfg(test)]
    pub(crate) async fn reset_after_server_fallback(
        &mut self,
        reason: Option<ToolUseRemovalReason>,
    ) -> ToolUseRemoval {
        self.reset_after_server_fallback_owned(reason)
            .await
            .expect("owned actor resets its current generation")
    }

    #[cfg(test)]
    pub(crate) async fn discard(&mut self) {
        self.scheduler
            .stop_scheduling()
            .await
            .expect("owned actor stops scheduling");
        self.reset_after_server_fallback_owned(None)
            .await
            .expect("owned actor discards its current generation");
    }

    #[cfg(test)]
    pub(crate) async fn run_to_completion(
        &mut self,
    ) -> Result<Vec<ContentBlock>, crate::error::OrchestratorError> {
        self.apply_abort_to_pending_owned().await;
        while !self.is_current_generation_idle().await {
            self.drain_one().await;
        }
        self.finish_context_layers().await?;
        Ok(self
            .tools
            .iter()
            .map(|tool| tool.result.clone().expect("scheduler completed each tool"))
            .collect())
    }

    /// Apply the conversation-owned controller for an admitted streamed
    /// fallback observation. The event carries provider routing facts only;
    /// the controller decides whether this conversation accepts the hop.
    pub(crate) async fn observe_server_fallback(
        &mut self,
        info: &llm_runtime::history::HistoryServerFallback,
        discarded_had_tool_use: bool,
    ) -> Result<crate::server_fallback::ServerFallbackAdmission, crate::error::OrchestratorError>
    {
        crate::server_fallback::handle(self.orch, info, discarded_had_tool_use).await
    }

    pub(crate) async fn persist_completed_assistant_row(
        &mut self,
        row: &mut crate::streaming_loop::CompletedAssistantRow,
    ) -> Option<crate::streaming_loop::PersistedAssistantRowLink> {
        self.orch
            .persist_completed_assistant_row(row, Some(Arc::new(self.publication_fence.clone())))
            .await
    }

    pub(crate) async fn remove_assistant_stream_rows(
        &mut self,
        rows: &[crate::streaming_loop::PersistedAssistantRowLink],
    ) {
        self.orch.remove_assistant_stream_rows(rows).await;
    }

    pub(crate) fn last_request_id(&self) -> Option<String> {
        self.orch.api.last_request_id()
    }
}

/// Short `Name(arg…)` description for a tool (TS `getToolDescription`,
/// StreamingToolExecutor.ts:243-252). No longer consumed in production now that
/// the sibling-error cascade is gone, but kept as the faithful API twin.
#[allow(dead_code)]
fn tool_description(t: &TrackedTool) -> String {
    let summary = t
        .input
        .get("command")
        .or_else(|| t.input.get("file_path"))
        .or_else(|| t.input.get("pattern"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if summary.is_empty() {
        t.name.clone()
    } else {
        let truncated = if summary.chars().count() > 40 {
            format!("{}\u{2026}", summary.chars().take(40).collect::<String>())
        } else {
            summary.to_owned()
        };
        format!("{}({})", t.name, truncated)
    }
}

/// Copy a provider-issued tool-call id onto a `ToolResult` block's
/// `provider_tool_use_id` for egress replay; no-op for non-`ToolResult` blocks.
fn set_provider_id(block: &mut ContentBlock, provider_id: Option<String>) {
    if let ContentBlock::ToolResult {
        provider_tool_use_id,
        ..
    } = block
    {
        *provider_tool_use_id = provider_id;
    }
}

fn unknown_tool_publication(
    orch: &ConversationOrchestrator,
    id: &ToolUseId,
    name: &str,
    block: &ContentBlock,
) -> crate::turn_loop::ToolResultPublication {
    let suffix = unknown_tool_suffix_for(name, orch);
    let model_text = match block {
        ContentBlock::ToolResult { content, .. } => content.clone(),
        _ => format!(
            "<tool_use_error>Error: No such tool available: {name}{suffix}</tool_use_error>"
        ),
    };
    let mut publication = crate::turn_loop::ToolResultPublication::frame_only(
        id,
        name,
        &model_text,
        serde_json::json!({ "error": format!("tool not found: {name}") }),
    );
    publication.tool_use_result = Some((serde_json::Value::String(format!(
        "Error: No such tool available: {name}{suffix}"
    ))).into());
    publication
}

fn post_tool_batch_call_for_result(
    id: &ToolUseId,
    name: &str,
    input: &serde_json::Value,
    block: &ContentBlock,
) -> hooks::events::PostToolBatchCall {
    let tool_response = match block {
        ContentBlock::ToolResult {
            content,
            content_blocks,
            ..
        } => Some(content_blocks.as_ref().map_or_else(
            || serde_json::Value::String(content.clone()),
            |blocks| {
                serde_json::to_value(blocks)
                    .unwrap_or_else(|_| serde_json::Value::String(content.clone()))
            },
        )),
        _ => None,
    };
    hooks::events::PostToolBatchCall {
        tool_name: name.to_string(),
        tool_input: input.clone(),
        tool_use_id: id.clone(),
        tool_response,
    }
}

// ============================================================================
// Task 9: ordered result drain (TS getCompletedResults / hasUnfinishedTools)
// ============================================================================

/// One drained result ready for the live loop to persist, carrying everything
/// needed to build the per-result user message (TS `getCompletedResults` yields
/// one message per result). The live loop parents every result to the single
/// per-turn assistant via that assistant's captured JSONL uuid (TS
/// `sourceToolAssistantUUID`), so no per-result assistant id is carried here.
pub(crate) struct DrainedResult {
    /// Assistant content-block row that owned this tool use. For a live
    /// streaming row this is the stop-time UUID; batched/recovered callers use
    /// their merged assistant identity and may supply a persisted parent map.
    pub(crate) assistant_id: MessageId,
    /// Host-only owner for settling this result; it is not part of any Native
    /// event or serialized transcript row.
    pub(crate) publication_guard: Option<Arc<dyn hooks::attachment::HookPublicationGuard>>,
    pub(crate) tool: String,
    pub(crate) block: ContentBlock,
    pub(crate) prevent_continuation: bool,
    pub(crate) injected: Vec<(ConversationMessage, ToolUseId)>,
    pub(crate) modifiers: Vec<ContextModifier>,
    pub(crate) post_tool_batch_calls: Vec<hooks::events::PostToolBatchCall>,
    pub(crate) publications: Vec<crate::turn_loop::ToolResultPublication>,
    /// Native `toolUseResult` sidecar for terminal-error synthetics. Normal
    /// provider tool results already installed their sidecar at dispatch.
    pub(crate) tool_use_result: Option<serde_json::Value>,
}

/// Native `xl(error)` text used for unmatched tool uses on ordinary model
/// errors. Kept separate from cancelled-tool and sibling-error synthetics.
pub(crate) fn terminal_error_tool_result(error: &str) -> String {
    format!(
        "The turn ended on an error, so this tool call was cancelled. If it had already started, some of its effects may have happened. Error: {error}"
    )
}

impl<'a> StreamingToolExecutor<'a> {
    /// TS `getCompletedResults`: walk tools in order, yield each newly-`Completed`
    /// tool's result (marking it `Yielded`), and STOP at an `Executing`
    /// non-concurrency-safe tool (don't emit past an unfinished exclusive
    /// barrier). Returns results in RECEIVED order.
    pub(crate) fn take_newly_completed(&mut self) -> Vec<DrainedResult> {
        let indices = std::mem::take(&mut self.actor_ready_indices);
        indices
            .into_iter()
            .filter_map(|index| self.take_one_completed(index))
            .collect()
    }

    fn take_one_completed(&mut self, index: usize) -> Option<DrainedResult> {
        let publication_guard: Arc<dyn hooks::attachment::HookPublicationGuard> =
            Arc::new(self.publication_fence.clone());
        let tool = self.tools.get_mut(index)?;
        if tool.status != ToolStatus::Completed {
            return None;
        }
        tool.status = ToolStatus::Yielded;
        self.tool_use_lifecycle.observe_tool_result(&tool.id);
        Some(DrainedResult {
            assistant_id: tool.assistant_id,
            publication_guard: Some(publication_guard),
            tool: tool.name.clone(),
            block: tool.result.clone().expect("completed tool has result"),
            prevent_continuation: tool.prevent_continuation,
            injected: std::mem::take(&mut tool.injected),
            modifiers: std::mem::take(&mut tool.modifiers),
            post_tool_batch_calls: std::mem::take(&mut tool.post_tool_batch_calls),
            publications: std::mem::take(&mut tool.publications),
            tool_use_result: None,
        })
    }

    /// Take all completed results in received order without applying the
    /// ordinary non-concurrency-safe delivery barrier. Native's terminal
    /// finalizer first runs Tn(false), then separately collects futures which
    /// finished beyond that barrier.
    pub(crate) fn take_all_completed(&mut self) -> Vec<DrainedResult> {
        let indices = std::mem::take(&mut self.actor_terminal_ready_indices);
        indices
            .into_iter()
            .filter_map(|index| self.take_one_completed(index))
            .collect()
    }

    /// TS `hasUnfinishedTools`: any tool not yet `Yielded`. Scheduler progress
    /// is independent of this facade-level query.
    #[allow(dead_code)]
    pub(crate) fn has_unfinished(&self) -> bool {
        self.tools.iter().any(|t| t.status != ToolStatus::Yielded)
    }
}

/// Build the synthetic `tool_result` for an unknown tool (claude-code `addTool`
/// line 78-84 / `toolExecution.ts:401`). Shared by the streaming executor and
/// the batched dispatch (`turn_loop`) so this parity-critical string lives in
/// exactly one place.
pub(crate) fn synthetic_unknown_tool(
    id: ToolUseId,
    name: &str,
    provider_id: Option<String>,
    suffix: &str,
) -> ContentBlock {
    ContentBlock::ToolResult { content_projection: None,
        tool_use_id: id,
        content: format!(
            "<tool_use_error>Error: No such tool available: {name}{suffix}</tool_use_error>"
        ),
        is_error: Some(true),
        provider_tool_use_id: provider_id,
        content_blocks: None,
    }
}

/// Claude Code 2.1.263 `Ldt` suffix after `No such tool available: ${name}`.
/// Genuinely-unknown tools stay empty. Mapped arms that have substrate here:
/// subagent-restricted `d1e`/`ct("external")`, coordinator `Y7e`, catalog
/// disabled, Glob/Grep-via-shell (`Nte` + `qe`), pending-MCP `l5o`, MCP
/// disconnected (`a5o` `disconnected`).
///
/// WebFetch→Artifact is a carve-out. The WebFetch→`web-fetch` agent redirect
/// needs the live roster (`pq`); that agent is gated off by default (`xgi()`),
/// so a catalog-hidden WebFetch falls through to the disabled arm.
///
/// `d1e` names resolved from 2.1.263 `src_160256736.js` `ct("external")`.
/// `ltr` (spread into `ct`) is not fully named here; the listed tools are
/// the resolved scalars. LingXi is the external user type, so `Workflow`
/// is included.
const SUBAGENT_RESTRICTED_TOOLS: &[&str] = &[
    "TaskOutput",
    "ExitPlanMode",
    "EnterPlanMode",
    "AskUserQuestion",
    "Poll",
    "ConnectGitHub",
    "propose_skills",
    "WaitForMcpServers",
    "RefreshMcpTools",
    "Workflow",
    "ScheduleWakeup",
    "ReadNotifications",
    "ProposeGoal",
    "EndConversation",
];

/// 2.1.263 `qbt`: base coordinator tools AND worker-redirect exclusions.
const COORDINATOR_REDIRECT_EXCLUSIONS: &[&str] = &[
    "Agent",
    "TaskStop",
    "SendMessage",
    "StructuredOutput",
    "Skill",
    "ReadNotifications",
    "ListAgents",
    "Workflow",
];

#[must_use]
pub(crate) fn is_coordinator_redirect_excluded(name: &str) -> bool {
    COORDINATOR_REDIRECT_EXCLUSIONS.contains(&name)
}

#[must_use]
pub(crate) fn unknown_tool_suffix_for(name: &str, orch: &ConversationOrchestrator) -> String {
    let src = crate::config::sanitize_query_source(&orch.config.query_source);
    let is_subagent = src.starts_with("agent") || src == "subagent";
    let may_redirect = orch.is_coordinator_session()
        && orch.find_dispatchable_tool("Agent").is_some()
        && orch
            .tools
            .find_registered(name)
            .is_some_and(|tool| !orch.is_tool_pool_denied(tool.as_ref()));
    unknown_tool_suffix(name, &orch.tools, is_subagent, may_redirect)
}

#[must_use]
pub(crate) fn unknown_tool_suffix(
    name: &str,
    tools: &tool_api::registry::ToolRegistry,
    is_subagent: bool,
    is_coordinator: bool,
) -> String {
    let registered = tools.find_registered(name);
    let canonical = registered.as_ref().map(|tool| tool.name()).unwrap_or(name);
    if is_subagent && SUBAGENT_RESTRICTED_TOOLS.contains(&canonical) {
        return format!(
            ". {name} is not available inside subagents. Complete the task with the tools provided and return findings to the orchestrator."
        );
    }
    if registered.is_some() && canonical == "SendUserMessage" {
        return format!(
            ". {name} is not enabled in this session \u{2014} write your message as normal assistant text instead."
        );
    }
    let in_catalog = registered.is_some();
    // 2.1.263 `dt("external")` / Y7e. A catalog entry is not necessarily
    // available to a worker (notably AskUserQuestion and ExitPlanMode).
    const WORKER_TOOLS: &[&str] = &[
        "Read",
        "WebSearch",
        "TodoWrite",
        "Grep",
        "WebFetch",
        "Glob",
        "Bash",
        "PowerShell",
        "Edit",
        "Write",
        "NotebookEdit",
        "Skill",
        "StructuredOutput",
        "ToolSearch",
        "EnterWorktree",
        "ExitWorktree",
        "REPL",
        "Monitor",
        "TaskStop",
        "GetTask",
        "SendMessage",
        "Artifact",
        "SearchPlugins",
        "SearchSkills",
        "ListPlugins",
        "ListSkills",
    ];
    let worker_available = tools.find_by_name(name).is_some_and(|tool| {
        tool.is_enabled(&tool_api::tool_trait::ToolStaticContext {
            main_loop_model: tools.main_loop_model(),
            ..Default::default()
        }) && std::iter::once(tool.name())
            .chain(tool.underlying_v1_tool_name())
            .chain(tool.family_parent_tool_name())
            .any(|name| WORKER_TOOLS.contains(&name))
    });
    if is_coordinator
        && !is_subagent
        && in_catalog
        && tools.find_by_name("Agent").is_some()
        && !is_coordinator_redirect_excluded(canonical)
        && worker_available
    {
        return format!(
            ". {name} is not available to you as the coordinator \u{2014} run it from a worker via the Agent tool instead."
        );
    }
    if in_catalog {
        return format!(". {name} is disabled for this session, in subagents as well as here.");
    }
    let shell = if tools.find_by_name("Bash").is_some() {
        "Bash"
    } else if tools.find_by_name("Shell").is_some() {
        "Shell"
    } else {
        ""
    };
    if name.eq_ignore_ascii_case("Glob") || name.eq_ignore_ascii_case("Grep") {
        if shell.is_empty() {
            return format!(". {name} is disabled for this session.");
        }
        return if name.eq_ignore_ascii_case("Glob") {
            format!(
                ". {name} is not available in this session \u{2014} find files with `find` via the {shell} tool instead."
            )
        } else {
            format!(
                ". {name} is not available in this session \u{2014} search file contents with `grep` via the {shell} tool instead."
            )
        };
    }
    if let Some(server) = mcp_server_from_tool_name(name) {
        // `l5o` (non-subagent): WaitForMcpServers is advertised only while an
        // MCP client is `pending`. `is_enabled` is that same pending mirror.
        if !is_subagent
            && tools
                .find_by_name("WaitForMcpServers")
                .is_some_and(|t| t.is_enabled(&ToolStaticContext::default()))
        {
            return format!(
                ". The MCP server '{server}' is still connecting. Call WaitForMcpServers to wait for it, then try again."
            );
        }
        // `a5o` `disconnected`. Subagent copy is "not available in this
        // context" (reconnecting uses "not connected").
        return if is_subagent {
            format!(
                ". Its MCP server '{server}' is not available in this context. Continue without this tool."
            )
        } else {
            format!(
                ". Its MCP server '{server}' has disconnected. Continue without this tool; it becomes callable again only if the server reconnects."
            )
        };
    }
    String::new()
}

fn mcp_server_from_tool_name(name: &str) -> Option<&str> {
    let rest = name.strip_prefix("mcp__")?;
    let server = rest.split("__").next()?;
    (!server.is_empty()).then_some(server)
}

#[cfg(test)]
#[path = "streaming_executor_test.rs"]
mod streaming_executor_test;

/// O4-A step 3: denial-kind housekeeping across the executor's synthetic
/// substitution.
///
/// The dispatch-site catch (`turn_loop.rs`) records `"interrupted"` for a tool
/// that returned `ToolError::Aborted`. When the executor then DISCARDS that
/// real outcome for a synthetic (claude-code `createSyntheticErrorMessage`,
/// 2.1.220 @232972360), the persisted block is no longer the interrupted one,
/// so the recorded kind must follow the block that actually survives:
///   * `user_interrupted` ⇒ `toolDenialKind:"user-rejected"` (@232972524)
///   * `streaming_fallback` / `conversation_ended` ⇒ NO `toolDenialKind`
/// Leaving the dispatch-site entry in place would both stamp the wrong kind on
/// the synthetic and orphan the map entry (`take_tool_denial_kind` removes on
/// read, and the interrupted block never reaches persistence).
#[cfg(test)]
mod synthetic_denial_kind_tests {
    use super::*;
    use crate::OrchestratorConfig;
    use crate::conversation::ConversationOrchestrator;
    use crate::test_support::{
        MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
        noop_hook_executor,
    };
    use async_trait::async_trait;
    use lingxi_core::types::MessageId;
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tool_api::context::ToolUseContext;
    use tool_api::progress::ToolProgressSender;
    use tool_api::registry::ToolRegistry;
    use tool_api::tool_trait::{
        DescriptionOptions, InterruptBehavior, PromptOptions, Tool, ToolCallResult, ToolError,
        ToolStaticContext, ValidationError,
    };

    /// Cancel-behavior tool that returns `ToolError::Aborted` the moment its
    /// `ctx.cancel` fires — the same shape as the real Bash/MCP abort seams.
    struct CancelTool;

    #[async_trait]
    impl Tool for CancelTool {
        fn name(&self) -> &str {
            "CancelTool"
        }
        fn input_schema(&self) -> &serde_json::Value {
            static SCHEMA: once_cell::sync::Lazy<serde_json::Value> =
                once_cell::sync::Lazy::new(|| json!({ "type": "object", "properties": {} }));
            &SCHEMA
        }
        fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
            true
        }
        fn max_result_size_chars(&self) -> usize {
            1024 * 1024
        }
        fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
            true
        }
        fn is_read_only(&self, _input: &serde_json::Value) -> bool {
            true
        }
        fn interrupt_behavior(&self, _input: &serde_json::Value) -> InterruptBehavior {
            InterruptBehavior::Cancel
        }
        async fn validate_input(
            &self,
            _input: &serde_json::Value,
            _ctx: &ToolUseContext,
        ) -> Result<(), ValidationError> {
            Ok(())
        }
        async fn check_permissions(
            &self,
            _input: &serde_json::Value,
            _ctx: &ToolUseContext,
        ) -> permission::PermissionResult {
            permission::PermissionResult::Allow {
                reason: permission::PermissionDecisionReason::Other {
                    reason: "test".into(),
                },
                updated_input: None,
                update_destination: None,
                metadata: permission::result::PermissionMetadata::default(),
            }
        }
        async fn description(
            &self,
            _input: &serde_json::Value,
            _opts: &DescriptionOptions,
        ) -> String {
            "cancel-tool".into()
        }
        async fn prompt(&self, _opts: &PromptOptions) -> String {
            String::new()
        }
        async fn call(
            &self,
            _input: serde_json::Value,
            ctx: ToolUseContext,
            _tx: ToolProgressSender,
        ) -> Result<ToolCallResult, ToolError> {
            let token = ctx.cancel.clone();
            tokio::select! {
                () = tokio::time::sleep(std::time::Duration::from_millis(500)) => {
                    Ok(ToolCallResult {
                        data: json!({ "content": "ran-to-end" }),
                        model_content: None,
                        new_messages: vec![],
                        context_modifier: None,
                        is_error: false,
                        mcp_meta: None,
                    })
                }
                () = async { match token { Some(t) => t.cancelled().await, None => std::future::pending().await } } => {
                    Err(ToolError::Aborted)
                }
            }
        }
    }

    fn orch() -> Arc<ConversationOrchestrator> {
        let mut registry = ToolRegistry::new();
        registry.register_builtin(Arc::new(CancelTool) as Arc<dyn Tool>);
        ConversationOrchestrator::into_shared(ConversationOrchestrator::new(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            Arc::new(registry),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            PathBuf::from("/tmp"),
        ))
    }

    #[tokio::test]
    async fn user_interrupt_substitution_rewrites_the_kind_to_user_rejected() {
        let orch = orch();
        let user_cancel = tokio_util::sync::CancellationToken::new();
        let a = MessageId::new();
        let id = ToolUseId::new();
        let mut exec =
            StreamingToolExecutor::try_new_with_user_cancel(&orch, Vec::new(), user_cancel.clone())
                .await
                .unwrap();
        exec.add_tool(id.clone(), "CancelTool".into(), json!({}), None, a)
            .await;
        user_cancel.cancel();
        let results = exec.run_to_completion().await.unwrap();
        let mut settlement = crate::streaming_loop::StreamToolSettlement::default();
        orch.settle_stream_tool_results(
            &mut settlement,
            exec.take_newly_completed(),
            &std::collections::HashMap::new(),
            &None,
        )
        .await;

        let ContentBlock::ToolResult { content, .. } = &results[0] else {
            panic!("expected a tool_result")
        };
        assert_eq!(content, REJECT_MESSAGE, "synthetic must survive");
        assert_eq!(
            orch.transcript
                .tool_denial_kinds
                .lock()
                .await
                .get(&id.to_string())
                .map(String::as_str),
            Some("user-rejected"),
            "the substituted synthetic carries `user-rejected`, not the \
             dispatch-site `interrupted`"
        );
    }

    #[tokio::test]
    async fn streaming_fallback_substitution_clears_the_kind() {
        let orch = orch();
        let a = MessageId::new();
        let id = ToolUseId::new();
        let mut exec = StreamingToolExecutor::try_new(&orch, Vec::new())
            .await
            .unwrap();
        exec.add_tool(id.clone(), "CancelTool".into(), json!({}), None, a)
            .await;
        exec.discard().await;
        let _ = exec.run_to_completion().await.unwrap();
        assert!(
            !orch
                .transcript
                .tool_denial_kinds
                .lock()
                .await
                .contains_key(&id.to_string()),
            "a streaming-fallback synthetic carries NO toolDenialKind"
        );
    }

    /// A tool cancelled while still QUEUED never reaches dispatch, so nothing
    /// records a kind there — but `apply_abort_to_pending` still persists the
    /// very same `user_interrupted` synthetic that `drain_one` produces, and
    /// claude-code's `createSyntheticErrorMessage` (2.1.220 @232972360) stamps
    /// `toolDenialKind:"user-rejected"` (@232972524) on that message regardless
    /// of whether the tool had started. Without this the persisted JSONL line
    /// silently loses the stamp on the queued path while keeping it on the
    /// in-flight path.
    #[tokio::test]
    async fn queued_tool_cancelled_before_dispatch_is_stamped_user_rejected() {
        let orch = orch();
        let user_cancel = tokio_util::sync::CancellationToken::new();
        let a = MessageId::new();
        let id = ToolUseId::new();
        let mut exec =
            StreamingToolExecutor::try_new_with_user_cancel(&orch, Vec::new(), user_cancel.clone())
                .await
                .unwrap();
        // Cancel before admission: the actor completes this queued call
        // without entering W1, then Tn hands its synthetic to settlement.
        user_cancel.cancel();
        exec.add_tool(id.clone(), "CancelTool".into(), json!({}), None, a)
            .await;
        assert_eq!(exec.tools[0].status, ToolStatus::Queued);
        let results = exec.run_to_completion().await.unwrap();
        let mut settlement = crate::streaming_loop::StreamToolSettlement::default();
        orch.settle_stream_tool_results(
            &mut settlement,
            exec.take_newly_completed(),
            &std::collections::HashMap::new(),
            &None,
        )
        .await;

        let ContentBlock::ToolResult { content, .. } = &results[0] else {
            panic!("expected a tool_result")
        };
        assert_eq!(content, REJECT_MESSAGE, "queued tool gets the synthetic");
        assert_eq!(
            orch.transcript
                .tool_denial_kinds
                .lock()
                .await
                .get(&id.to_string())
                .map(String::as_str),
            Some("user-rejected"),
            "the queued-path synthetic must carry the same kind as the \
             in-flight-path synthetic"
        );
    }
}
