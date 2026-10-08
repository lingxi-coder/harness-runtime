//! Live tool execution for one streamed Agent response.
//!
//! The decoder owns provider delta parsing. This module only schedules fully
//! assembled tool calls from `content_block_stop` and retains their outcomes in
//! source order until the response is committed by the runner.

use futures::future::BoxFuture;
use futures::stream::FuturesUnordered;
use futures::{FutureExt, StreamExt};
use lingxi_core::host::tool_invoker::{
    SubagentInvocationContext, ToolInvocationResult, ToolInvokerError,
};
use lingxi_core::host::tool_use_lifecycle::{ToolUseRemoval, ToolUseRemovalReason};
use lingxi_core::host::CancellationToken;
use lingxi_core::types::ToolUseId;
use serde_json::Value;
use std::sync::Arc;

/// One decoded, provider-issued ToolUse plus the host context captured at its
/// dispatch boundary. The executor replaces `context.cancellation_token` with
/// a child token immediately before invoking the host tool pipeline.
#[derive(Debug, Clone)]
pub(crate) struct LiveAgentToolCall {
    pub block_index: u32,
    pub id: ToolUseId,
    pub name: String,
    pub input: Value,
    pub provider_id: Option<String>,
    /// Resolved from the registered host tool at the actual dispatch boundary.
    /// Native only layers returned context into later calls for non-safe tools.
    pub concurrency_safe: bool,
    pub context: SubagentInvocationContext,
}

pub(crate) type LiveAgentToolDispatch = Arc<
    dyn Fn(LiveAgentToolCall) -> BoxFuture<'static, Result<ToolInvocationResult, ToolInvokerError>>
        + Send
        + Sync,
>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CallState {
    Queued,
    Executing,
    Completed,
    LocalOnly,
}

struct CallRecord {
    call: LiveAgentToolCall,
    concurrency_safe: bool,
    state: CallState,
    result: Option<Result<ToolInvocationResult, ToolInvokerError>>,
}

/// Per-response executor. It cancels every call before dropping in-flight
/// futures on fallback, returns every registered id (including queued and
/// completed records), and creates a fresh abort tree for a later hop.
pub(crate) struct LiveAgentToolExecutor {
    calls: Vec<CallRecord>,
    tracked_blocks: Vec<(u32, ToolUseId)>,
    inflight: FuturesUnordered<
        BoxFuture<'static, (usize, Result<ToolInvocationResult, ToolInvokerError>)>,
    >,
    abort: CancellationToken,
    max_safe_concurrency: usize,
    completed_normally: bool,
}

impl LiveAgentToolExecutor {
    pub(crate) fn new(max_safe_concurrency: usize) -> Self {
        Self {
            calls: Vec::new(),
            tracked_blocks: Vec::new(),
            inflight: FuturesUnordered::new(),
            abort: CancellationToken::new(),
            max_safe_concurrency: max_safe_concurrency.max(1),
            completed_normally: false,
        }
    }

    /// Track a provider-issued tool-use id even when Agent-local policy will
    /// answer it without invoking a host tool (for example `StructuredOutput`
    /// or an allowlist rejection). A server fallback sweep removes the entire
    /// outstanding set, not only futures that happened to start.
    pub(crate) fn track_tool_use(&mut self, block_index: u32, id: ToolUseId) {
        self.tracked_blocks.push((block_index, id));
    }

    /// Register a decoded model tool call. `invoke` is false for Agent-local
    /// synthetic tools (for example StructuredOutput) and calls rejected by
    /// the current Agent allowlist; those still belong to the sweep id set.
    pub(crate) fn add_call(
        &mut self,
        call: LiveAgentToolCall,
        invoke: bool,
        dispatch: &LiveAgentToolDispatch,
    ) {
        self.add_call_record(call, invoke);
        if invoke {
            self.start_queued(dispatch);
        }
    }

    /// Register a decoded tool call without starting it yet. Host recovery
    /// policies use this until the response is qualified, so a retryable or
    /// malformed body cannot perform local side effects.
    pub(crate) fn add_call_deferred(&mut self, call: LiveAgentToolCall, invoke: bool) {
        self.add_call_record(call, invoke);
    }

    fn add_call_record(&mut self, call: LiveAgentToolCall, invoke: bool) {
        let concurrency_safe = call.concurrency_safe;
        self.calls.push(CallRecord {
            call,
            concurrency_safe,
            state: if invoke {
                CallState::Queued
            } else {
                CallState::LocalOnly
            },
            result: None,
        });
    }

    pub(crate) fn start_queued_calls(&mut self, dispatch: &LiveAgentToolDispatch) {
        self.start_queued(dispatch);
    }

    /// Replace the current-row snapshot on queued calls after a deferred Mod
    /// acceptance and terminal response metadata update.
    pub(crate) fn refresh_assistant_message(
        &mut self,
        assistant_message: &lingxi_core::types::ConversationMessage,
    ) {
        let assistant_message_id = assistant_message.id();
        for record in &mut self.calls {
            if record.state == CallState::Queued
                && record.call.context.assistant_message_id.as_ref() == Some(&assistant_message_id)
            {
                record.call.context.assistant_message = Some(assistant_message.clone());
            }
        }
    }

    /// True when a discarded provider block corresponds to a registered tool
    /// use. The executor still removes *all* records when the Native sweep is
    /// triggered; this predicate only mirrors the Native tombstoned-tool gate.
    pub(crate) fn contains_discarded_tool(&self, discarded_blocks: &[usize]) -> bool {
        self.tracked_blocks.iter().any(|(block_index, _)| {
            discarded_blocks
                .iter()
                .any(|index| u32::try_from(*index).ok() == Some(*block_index))
        })
    }

    pub(crate) fn has_inflight(&self) -> bool {
        !self.inflight.is_empty()
    }

    pub(crate) fn has_pending_calls(&self) -> bool {
        self.calls
            .iter()
            .any(|record| matches!(record.state, CallState::Queued | CallState::Executing))
    }

    pub(crate) fn call(&self, index: usize) -> Option<&LiveAgentToolCall> {
        self.calls.get(index).map(|record| &record.call)
    }

    pub(crate) async fn next_completion(
        &mut self,
    ) -> Option<(usize, Result<ToolInvocationResult, ToolInvokerError>)> {
        self.inflight.next().await
    }

    pub(crate) fn record_completion(
        &mut self,
        index: usize,
        result: Result<ToolInvocationResult, ToolInvokerError>,
        dispatch: &LiveAgentToolDispatch,
    ) {
        let update_queued_context = self
            .calls
            .get(index)
            .is_some_and(|record| !record.concurrency_safe);
        let updated_context = update_queued_context
            .then(|| {
                result
                    .as_ref()
                    .ok()
                    .and_then(|result| result.context_state.clone())
            })
            .flatten();
        self.record_completion_without_scheduling(index, result);
        if let Some(updated_context) = updated_context {
            for record in &mut self.calls {
                if record.state == CallState::Queued {
                    record.call.context.tool_context_state = Some(updated_context.clone());
                }
            }
        }
        self.start_queued(dispatch);
    }

    /// Record an already completed call without starting queued work. Stream
    /// errors use this after draining results that were ready at the error
    /// boundary, then cancel every remaining call and leave queued calls idle.
    pub(crate) fn record_completion_without_scheduling(
        &mut self,
        index: usize,
        result: Result<ToolInvocationResult, ToolInvokerError>,
    ) {
        if let Some(record) = self.calls.get_mut(index) {
            record.result = Some(result);
            record.state = CallState::Completed;
        }
    }

    /// Mark a response executor complete once its owner has observed and
    /// journaled every completion. Kept separate so the runner can preserve
    /// the Native result-row/new-message arrival order while draining.
    pub(crate) fn mark_completed_normally(&mut self) {
        self.completed_normally = true;
    }

    /// Preserve tool futures that have completed by the time the provider
    /// stream error is observed, then cancel and drop the remaining futures.
    /// This matches the query finalizer's drain-finished-then-abort ordering.
    pub(crate) async fn abort_inflight_and_collect_ready(
        &mut self,
    ) -> Vec<(usize, Result<ToolInvocationResult, ToolInvokerError>)> {
        let mut completed = Vec::new();
        loop {
            match self.inflight.next().now_or_never() {
                Some(Some(item)) => completed.push(item),
                Some(None) | None => break,
            }
        }
        self.abort.cancel();
        self.inflight = FuturesUnordered::new();
        completed
    }

    /// Remove all live executor records before continuing after a server hop.
    /// `reason` is present only for the accepted visible tombstoned-tool branch;
    /// declined fallback removal intentionally has no reason.
    pub(crate) fn reset_for_server_fallback(
        &mut self,
        reason: Option<ToolUseRemovalReason>,
    ) -> Option<ToolUseRemoval> {
        let ids = std::mem::take(&mut self.tracked_blocks)
            .into_iter()
            .map(|(_, id)| id)
            .collect::<Vec<_>>();
        self.abort.cancel();
        self.inflight = FuturesUnordered::new();
        self.calls.clear();
        self.tracked_blocks.clear();
        self.abort = CancellationToken::new();
        if ids.is_empty() {
            None
        } else {
            Some(ToolUseRemoval { ids, reason })
        }
    }

    /// Take outcomes in registration order after the stream completed. Calls
    /// cancelled by a fallback have already been removed and cannot leak a
    /// stale ToolResult into the next model request.
    pub(crate) fn into_calls(
        mut self,
    ) -> Vec<(
        LiveAgentToolCall,
        Option<Result<ToolInvocationResult, ToolInvokerError>>,
    )> {
        self.completed_normally = true;
        std::mem::take(&mut self.calls)
            .into_iter()
            .map(|record| (record.call, record.result))
            .collect()
    }

    fn start_queued(&mut self, dispatch: &LiveAgentToolDispatch) {
        loop {
            let executing_safe = self
                .calls
                .iter()
                .filter(|record| record.state == CallState::Executing)
                .map(|record| record.concurrency_safe)
                .collect::<Vec<_>>();
            let safe_count = executing_safe.iter().filter(|safe| **safe).count();
            let mut started = false;

            for index in 0..self.calls.len() {
                if self.calls[index].state != CallState::Queued {
                    continue;
                }
                let safe = self.calls[index].concurrency_safe;
                if safe && safe_count >= self.max_safe_concurrency {
                    continue;
                }
                let can_execute = executing_safe.is_empty()
                    || (safe && executing_safe.iter().all(|executing| *executing));
                if can_execute {
                    self.start_call(index, dispatch);
                    started = true;
                    break;
                }
                if !safe {
                    // An unsafe tool is a queue barrier while anything is in
                    // flight, matching the shared streaming executor ordering.
                    return;
                }
            }
            if !started {
                return;
            }
        }
    }

    fn start_call(&mut self, index: usize, dispatch: &LiveAgentToolDispatch) {
        let Some(record) = self.calls.get_mut(index) else {
            return;
        };
        record.state = CallState::Executing;
        let mut call = record.call.clone();
        call.context.cancellation_token = self.abort.child_token();
        let dispatch = dispatch.clone();
        self.inflight
            .push(Box::pin(async move { (index, dispatch(call).await) }));
    }
}

impl Drop for LiveAgentToolExecutor {
    fn drop(&mut self) {
        if !self.completed_normally {
            self.abort.cancel();
            self.inflight = FuturesUnordered::new();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::tool_invoker::{
        SubagentForkContext, SubagentInvocationContext, ToolExecutionPolicy,
    };

    fn invocation_context() -> SubagentInvocationContext {
        SubagentInvocationContext {
            input_projection: None,
            cancellation_token: CancellationToken::new(),
            permission_pause_observer: None,
            parent_agent_id: None,
            origin_session_id: None,
            tool_execution_policy: ToolExecutionPolicy::Ordinary,
            instruction_context: None,
            fork_context: Some(SubagentForkContext {
                messages: Vec::new(),
                system_prompt: None,
            }),
            agent_name: None,
            team_name: None,
            is_async: false,
            is_non_interactive_session: false,
            can_show_permission_prompts: false,
            cwd: None,
            tool_use_id: Some("call-1".into()),
            assistant_message_id: None,
            depth: 0,
            observer: None,
            parent_model: None,
            parent_model_profile: None,
            agent_spawn_provenance: Default::default(),
            tool_context_state: None,
            current_history: Vec::new(),
            assistant_message: None,
            same_turn_tool_uses: Vec::new(),
            mode_override: None,
            request_source: None,
            frozen_command_denies: Vec::new(),
        }
    }

    fn live_call(id: &str, concurrency_safe: bool) -> LiveAgentToolCall {
        LiveAgentToolCall {
            block_index: 0,
            id: ToolUseId::from(id.to_string()),
            name: "Read".into(),
            input: Value::Null,
            provider_id: None,
            concurrency_safe,
            context: invocation_context(),
        }
    }

    fn result_with_context(
        context_state: lingxi_core::host::tool_invoker::ToolInvocationContextState,
    ) -> ToolInvocationResult {
        ToolInvocationResult {
            mcp_meta_projection: None,
            model_content_projection: None,
            data_projection: None,
            is_error: false,
            data: Value::Null,
            model_content: None,
            new_messages: Vec::new(),
            context_modifier: None,
            mcp_meta: None,
            turn_end: None,
            context: lingxi_core::types::utf16_json::Utf16JsonProjection::plain(Value::Array(
                Vec::new(),
            )),
            context_state: Some(context_state),
        }
    }

    #[tokio::test]
    async fn fallback_sweep_cancels_started_tool_before_dropping_its_future() {
        let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
        let (cancel_seen_tx, mut cancel_seen_rx) = tokio::sync::mpsc::unbounded_channel();
        let dispatch: LiveAgentToolDispatch = Arc::new(move |call| {
            let token = call.context.cancellation_token;
            let watcher = token.clone();
            let cancel_seen_tx = cancel_seen_tx.clone();
            tokio::spawn(async move {
                watcher.cancelled().await;
                let _ = cancel_seen_tx.send(());
            });
            let _ = started_tx.send(token.clone());
            Box::pin(async move {
                token.cancelled().await;
                Err(ToolInvokerError::Abort("cancelled".into()))
            })
        });
        let id = ToolUseId::from("toolu-live".to_string());
        let mut executor = LiveAgentToolExecutor::new(1);
        executor.track_tool_use(4, id.clone());
        executor.add_call(
            LiveAgentToolCall {
                block_index: 4,
                id: id.clone(),
                name: "Read".into(),
                input: Value::Null,
                provider_id: None,
                concurrency_safe: false,
                context: invocation_context(),
            },
            true,
            &dispatch,
        );

        let mut next = Box::pin(executor.next_completion());
        assert!(futures::poll!(next.as_mut()).is_pending());
        drop(next);
        let token = started_rx.recv().await.expect("tool invocation started");

        let removal = executor
            .reset_for_server_fallback(None)
            .expect("the in-flight ToolUseId is removed");
        assert_eq!(removal.ids, [id]);
        assert_eq!(removal.reason, None);
        assert!(token.is_cancelled(), "the tool's token is cancelled first");
        drop(executor);
        cancel_seen_rx
            .recv()
            .await
            .expect("a task holding the same token observes cancellation");
    }

    #[tokio::test]
    async fn declined_sweep_cancels_even_when_no_block_is_tombstoned() {
        let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
        let dispatch: LiveAgentToolDispatch = Arc::new(move |call| {
            let _ = started_tx.send(call.context.cancellation_token.clone());
            Box::pin(std::future::pending())
        });
        let mut executor = LiveAgentToolExecutor::new(1);
        let id = ToolUseId::from("toolu-not-in-discard-list".to_string());
        executor.track_tool_use(8, id.clone());
        executor.add_call(
            LiveAgentToolCall {
                block_index: 8,
                id: id.clone(),
                name: "Read".into(),
                input: Value::Null,
                provider_id: None,
                concurrency_safe: false,
                context: invocation_context(),
            },
            true,
            &dispatch,
        );
        let mut next = Box::pin(executor.next_completion());
        assert!(futures::poll!(next.as_mut()).is_pending());
        drop(next);
        let token = started_rx.recv().await.expect("tool invocation started");

        assert!(!executor.contains_discarded_tool(&[]));
        let removal = executor
            .reset_for_server_fallback(None)
            .expect("decline sweeps all tracked ids, independently of blocks");
        assert_eq!(removal.ids, [id]);
        assert_eq!(removal.reason, None);
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn concurrency_safe_completion_does_not_layer_context_into_queued_calls() {
        let dispatch: LiveAgentToolDispatch = Arc::new(|_| {
            Box::pin(std::future::pending::<
                Result<ToolInvocationResult, ToolInvokerError>,
            >())
        });
        let mut executor = LiveAgentToolExecutor::new(1);
        executor.add_call(live_call("safe-first", true), true, &dispatch);
        executor.add_call(live_call("unsafe-second", false), true, &dispatch);

        let state = lingxi_core::host::tool_invoker::ToolInvocationContextState::new(Arc::new(
            "from-safe-call".to_string(),
        ));
        executor.record_completion(0, Ok(result_with_context(state)), &dispatch);

        assert!(executor
            .call(1)
            .expect("queued call remains registered")
            .context
            .tool_context_state
            .is_none());
    }

    #[tokio::test]
    async fn non_concurrency_safe_completion_layers_context_into_queued_calls() {
        let dispatch: LiveAgentToolDispatch = Arc::new(|_| {
            Box::pin(std::future::pending::<
                Result<ToolInvocationResult, ToolInvokerError>,
            >())
        });
        let mut executor = LiveAgentToolExecutor::new(1);
        executor.add_call(live_call("unsafe-first", false), true, &dispatch);
        executor.add_call(live_call("safe-second", true), true, &dispatch);

        let state = lingxi_core::host::tool_invoker::ToolInvocationContextState::new(Arc::new(
            "from-unsafe-call".to_string(),
        ));
        executor.record_completion(0, Ok(result_with_context(state)), &dispatch);

        let layered = executor
            .call(1)
            .expect("queued call remains registered")
            .context
            .tool_context_state
            .as_ref()
            .expect("non-safe completion supplies the later context");
        assert_eq!(
            layered.downcast_arc::<String>().unwrap().as_str(),
            "from-unsafe-call"
        );
    }
}
