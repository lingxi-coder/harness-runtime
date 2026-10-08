//! API seam for the subagent runner.
//!
//! [`SubagentApiClient`] is the narrow, object-safe trait the multi-turn
//! [`crate::runner::run_subagent`] loop uses to call the model. It mirrors the
//! orchestrator's `messages_create` shape but lives in the agent crate so the
//! agent crate never takes a dep on the orchestrator (which would form a
//! cycle). Production wiring (M5-Wire) implements this trait on the
//! orchestrator's API client adapter and hands an `Arc<dyn SubagentApiClient>`
//! to [`crate::handle::PoolSubagentSpawner`].
//!
//! Every implementation accepts the complete typed streaming request. Provider
//! routing, tool choice, effort and registered call options are never silently
//! discarded by a default adapter.

use async_trait::async_trait;
use futures::stream::BoxStream;
use lingxi_core::host::{SubagentObservation, SubagentSpawnObserver, WorkflowQueryWatchdog};
use lingxi_core::types::{AgentId, SessionId};
use llm_runtime::{HistoryEvent, LlmError};
use std::path::PathBuf;
use std::sync::Arc;

const OBSERVER_EVENT_BUFFER: usize = 100;

/// Host-owned inputs for the fire-and-forget near-limit checkpoint.
///
/// The agent loop owns the exact oracle timing, but the host owns persistence
/// policy and the checkpoint implementation. Keeping this request provider-
/// neutral avoids a dependency from `agent` back into the `session` crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NearLimitCheckpointRequest {
    /// Owning conversation session, not the child agent id.
    pub session_id: SessionId,
    /// Owning conversation workspace (the oracle's process cwd).
    pub cwd: PathBuf,
    /// Whether the owning conversation is non-interactive.
    pub non_interactive: bool,
}

/// Ordered, nonblocking hand-off from the child event pump to host observers.
///
/// Ordinary telemetry has bounded queue capacity. Lifecycle edges share the
/// same FIFO but cannot be dropped: otherwise a busy observer can miss a wake
/// or receive an older completion after a newer wake. No producer awaits UI
/// callbacks or spawns a separate task to enqueue an event.
#[derive(Clone)]
pub(crate) struct ObserverEventSink {
    sender: tokio::sync::mpsc::UnboundedSender<QueuedObservation>,
    telemetry_capacity: Arc<tokio::sync::Semaphore>,
}

struct QueuedObservation {
    event: SubagentObservation,
    permit: Option<tokio::sync::OwnedSemaphorePermit>,
}

impl ObserverEventSink {
    pub(crate) fn new(observers: Vec<Arc<dyn SubagentSpawnObserver>>) -> Self {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<QueuedObservation>();
        tokio::spawn(async move {
            while let Some(QueuedObservation { event, permit }) = receiver.recv().await {
                // Capacity measures queued telemetry, not the callback in flight.
                drop(permit);
                for observer in &observers {
                    observer.on_event(event.clone()).await;
                }
            }
        });
        Self {
            sender,
            telemetry_capacity: Arc::new(tokio::sync::Semaphore::new(OBSERVER_EVENT_BUFFER)),
        }
    }

    pub(crate) fn try_emit(&self, event: SubagentObservation) {
        let reliable = match &event {
            SubagentObservation::Allocated { .. }
            | SubagentObservation::Completed { .. }
            | SubagentObservation::Failed { .. }
            | SubagentObservation::Killed { .. } => true,
            // Foreground owners park without Completed, which would tear down
            // their pump. Their rest marker is lifecycle, not lossy telemetry.
            SubagentObservation::Message {
                message:
                    lingxi_core::types::ConversationMessage::System {
                        subtype: Some(subtype),
                        ..
                    },
                ..
            } if subtype == "agent_idle" => true,
            SubagentObservation::Message {
                message: lingxi_core::types::ConversationMessage::User { content, .. },
                ..
            } => !content.iter().any(|block| {
                matches!(
                    block,
                    lingxi_core::types::ContentBlock::ToolResult { .. }
                        | lingxi_core::types::ContentBlock::AdvisorToolResult { .. }
                )
            }),
            _ => false,
        };
        let permit = if reliable {
            None
        } else {
            match self.telemetry_capacity.clone().try_acquire_owned() {
                Ok(permit) => Some(permit),
                Err(_) => {
                    tracing::warn!("subagent observer queue is full; dropping telemetry event");
                    return;
                }
            }
        };
        self.enqueue(event, permit);
    }

    fn enqueue(
        &self,
        event: SubagentObservation,
        permit: Option<tokio::sync::OwnedSemaphorePermit>,
    ) {
        if self
            .sender
            .send(QueuedObservation { event, permit })
            .is_err()
        {
            tracing::debug!("subagent observer queue closed");
        }
    }

    /// Terminal lifecycle events join the same FIFO synchronously so a later
    /// wake cannot overtake a completion while the observer is saturated.
    pub(crate) fn emit_terminal(&self, event: SubagentObservation) {
        self.enqueue(event, None);
    }
}

/// `messages.create` seam used by the subagent loop.
///
/// Object-safe: callers hold an `Arc<dyn SubagentApiClient>`. The concrete
/// production impl lives in the orchestrator (the Wire step); test fixtures
/// provide a scripted mock (see [`crate::runner`] tests).
#[async_trait]
pub trait SubagentApiClient: Send + Sync {
    /// Resolve the already-captured Native refusal-text facts for a physical
    /// provider route. `None` means the host has not supplied those facts; the
    /// Agent must not infer them from its model allowlist or session mode.
    fn refusal_api_text_snapshot(
        &self,
        _model: &str,
        _profile: Option<&str>,
    ) -> Result<Option<lingxi_core::host::refusal_api_text::RefusalApiTextSnapshot>, LlmError> {
        Ok(None)
    }

    /// Resolve a Mod's model rewrite through the provider's route catalog.
    fn resolve_mod_media_route(
        &self,
        _model: &str,
        _profile: Option<&str>,
    ) -> Option<llm_runtime::MediaRoute> {
        None
    }

    /// Consume a pending near-limit wrap-up hint for the current subagent
    /// query loop. Default no-op preserves existing mocks and non-provider
    /// implementations.
    fn consume_pending_near_limit_wrap_up_hint(&self) -> bool {
        false
    }

    /// Dispatch the near-limit resume checkpoint without blocking the query
    /// loop. The production provider adapter delegates to the session-owned
    /// checkpoint machinery; mocks and hosts without persistence stay no-op.
    fn dispatch_near_limit_checkpoint(&self, _request: NearLimitCheckpointRequest) {}

    /// Record the oracle's `y("usage_limit_near_wrapup")` success gate. The
    /// provider host owns the telemetry transport; agent-only mocks remain
    /// no-op and the query loop does not depend on a concrete sink.
    fn record_usage_limit_near_wrap_up(&self) {}

    /// Workflow-only watchdog policy attached by the spawn adapter. Ordinary
    /// clients return `None`, so non-workflow subagents retain their existing
    /// transport/retry behavior.
    fn workflow_query_watchdog(&self) -> Option<WorkflowQueryWatchdog> {
        None
    }

    /// Publish a typed retry observation. The default is a no-op; the
    /// per-workflow wrapper fans the event out to the spawn observers without
    /// adding another polling/event channel.
    async fn observe_workflow_query_retry(
        &self,
        _agent_id: AgentId,
        _attempt: u32,
        _reason: String,
    ) {
    }

    /// Issue one round-trip with every model, routing and execution option.
    /// The returned stream contains the provider's decoded history events.
    async fn stream(
        &self,
        request: SubagentApiRequest,
    ) -> Result<BoxStream<'static, Result<HistoryEvent, LlmError>>, LlmError>;
}

/// Complete owned input for one subagent model round-trip.
#[derive(Debug, Clone)]
pub struct SubagentApiRequest {
    /// Resolved provider wire model identifier.
    pub model: String,
    /// Provider profile; absence selects the host's current default route.
    pub profile: Option<String>,
    /// Stable rendered system prompt for this run.
    pub system: Option<String>,
    /// Current oldest-first conversation history.
    pub messages: Vec<lingxi_core::types::ConversationMessage>,
    /// Advertised tool definitions for this round-trip.
    pub tools: Vec<lingxi_core::types::utf16_json::Utf16JsonProjection>,
    /// Explicit tool choice for the designated structured-output turn.
    pub forced_tool: Option<String>,
    /// Per-request thinking-effort hint.
    pub effort: Option<serde_json::Value>,
    /// Registered attempt, output ceiling and query-source accounting.
    pub opts: SubagentApiCallOpts,
}

/// Optional per-round-trip Fusion / COGS knobs.
#[derive(Debug, Clone, Default)]
pub struct SubagentApiCallOpts {
    /// Trusted per-logical-call capability; retries retain this exact context.
    pub model_attempt: Option<lingxi_core::host::ModelAttemptContext>,
    /// Output token cap for this turn.
    pub max_output_tokens: Option<u32>,
    /// COGS query-source label.
    pub query_source_label: Option<String>,
}

/// Per-spawn API wrapper that enables the workflow query watchdog and routes
/// retry notifications onto the same typed observer stream as child lifecycle
/// events. It deliberately delegates every model operation to the original
/// client so provider/profile/forced-tool routing stays unchanged.
pub(crate) struct WorkflowWatchdogApiClient {
    inner: Arc<dyn SubagentApiClient>,
    policy: WorkflowQueryWatchdog,
    observer_events: ObserverEventSink,
}

impl WorkflowWatchdogApiClient {
    pub(crate) fn new(
        inner: Arc<dyn SubagentApiClient>,
        policy: WorkflowQueryWatchdog,
        observers: Vec<Arc<dyn SubagentSpawnObserver>>,
    ) -> Self {
        Self::with_observer_events(inner, policy, ObserverEventSink::new(observers))
    }

    pub(crate) fn with_observer_events(
        inner: Arc<dyn SubagentApiClient>,
        policy: WorkflowQueryWatchdog,
        observer_events: ObserverEventSink,
    ) -> Self {
        Self {
            inner,
            policy,
            observer_events,
        }
    }
}

#[async_trait]
impl SubagentApiClient for WorkflowWatchdogApiClient {
    fn refusal_api_text_snapshot(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> Result<Option<lingxi_core::host::refusal_api_text::RefusalApiTextSnapshot>, LlmError> {
        self.inner.refusal_api_text_snapshot(model, profile)
    }

    fn resolve_mod_media_route(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> Option<llm_runtime::MediaRoute> {
        self.inner.resolve_mod_media_route(model, profile)
    }

    fn consume_pending_near_limit_wrap_up_hint(&self) -> bool {
        self.inner.consume_pending_near_limit_wrap_up_hint()
    }

    fn dispatch_near_limit_checkpoint(&self, request: NearLimitCheckpointRequest) {
        self.inner.dispatch_near_limit_checkpoint(request);
    }

    fn record_usage_limit_near_wrap_up(&self) {
        self.inner.record_usage_limit_near_wrap_up();
    }

    fn workflow_query_watchdog(&self) -> Option<WorkflowQueryWatchdog> {
        Some(self.policy)
    }

    async fn observe_workflow_query_retry(&self, agent_id: AgentId, attempt: u32, reason: String) {
        self.observer_events.try_emit(SubagentObservation::Retry {
            agent_id,
            attempt,
            reason,
        });
    }

    async fn stream(
        &self,
        request: SubagentApiRequest,
    ) -> Result<BoxStream<'static, Result<HistoryEvent, LlmError>>, LlmError> {
        self.inner.stream(request).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    struct CapturedRequest(std::sync::Mutex<Option<SubagentApiRequest>>);

    #[async_trait]
    impl SubagentApiClient for CapturedRequest {
        async fn stream(
            &self,
            request: SubagentApiRequest,
        ) -> Result<BoxStream<'static, Result<HistoryEvent, LlmError>>, LlmError> {
            *self.0.lock().unwrap() = Some(request);
            Ok(Box::pin(futures::stream::empty()))
        }
    }

    #[tokio::test]
    async fn workflow_wrapper_preserves_the_complete_stream_request() {
        for forced_tool in [None, Some("StructuredOutput".to_owned())] {
            let inner = Arc::new(CapturedRequest(std::sync::Mutex::new(None)));
            let wrapper = WorkflowWatchdogApiClient::new(
                inner.clone(),
                WorkflowQueryWatchdog::default(),
                Vec::new(),
            );
            let run = lingxi_core::host::ModelAttemptRun::new(Arc::new(()));
            let attempt = run
                .context(lingxi_core::host::ModelAttemptStage::Panel, Some(2))
                .unwrap();
            let request = SubagentApiRequest {
                model: "exact-model".into(),
                profile: Some("second-provider".into()),
                system: Some("EXACT SYSTEM".into()),
                messages: vec![lingxi_core::types::ConversationMessage::user(
                    lingxi_core::types::MessageId::new(),
                    "EXACT HISTORY".into(),
                )],
                tools: vec![serde_json::json!({"name":"Read","input_schema":{"type":"object"}})],
                forced_tool,
                effort: Some(serde_json::json!(8192)),
                opts: SubagentApiCallOpts {
                    model_attempt: Some(attempt.clone()),
                    max_output_tokens: Some(4096),
                    query_source_label: Some("fusion_panel".into()),
                },
            };
            let _stream = wrapper.stream(request.clone()).await.unwrap();
            let captured = inner.0.lock().unwrap().take().unwrap();
            assert_eq!(captured.model, request.model);
            assert_eq!(captured.profile, request.profile);
            assert_eq!(captured.system, request.system);
            assert_eq!(captured.messages, request.messages);
            assert_eq!(captured.tools, request.tools);
            assert_eq!(captured.forced_tool, request.forced_tool);
            assert_eq!(captured.effort, request.effort);
            assert_eq!(captured.opts.max_output_tokens, Some(4096));
            assert_eq!(
                captured.opts.query_source_label.as_deref(),
                Some("fusion_panel")
            );
            let captured_attempt = captured.opts.model_attempt.unwrap();
            assert_eq!(
                captured_attempt.registration_id(),
                attempt.registration_id()
            );
            assert_eq!(
                captured_attempt.logical_call_id(),
                attempt.logical_call_id()
            );
            assert_eq!(captured_attempt.panel_slot(), Some(2));
        }
    }

    #[tokio::test]
    async fn observer_saturation_preserves_lifecycle_fifo_without_blocking_producer() {
        struct BlockingObserver {
            events: std::sync::Mutex<Vec<SubagentObservation>>,
            started: tokio::sync::Notify,
            release: tokio::sync::Notify,
            finished: tokio::sync::Notify,
        }
        #[async_trait]
        impl SubagentSpawnObserver for BlockingObserver {
            async fn on_event(&self, event: SubagentObservation) {
                let first = self.events.lock().unwrap().is_empty();
                if first {
                    self.started.notify_one();
                    self.release.notified().await;
                }
                let finished = matches!(event, SubagentObservation::Killed { .. });
                self.events.lock().unwrap().push(event);
                if finished {
                    self.finished.notify_one();
                }
            }
        }
        let observer = Arc::new(BlockingObserver {
            events: std::sync::Mutex::new(Vec::new()),
            started: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
            finished: tokio::sync::Notify::new(),
        });
        let sink = ObserverEventSink::new(vec![observer.clone()]);
        let agent_id = AgentId::new();
        let progress = || SubagentObservation::Progress {
            agent_id,
            tool_use_count: 0,
            token_count: 1,
        };
        let completed = || SubagentObservation::Completed {
            agent_id,
            content: serde_json::Value::Null,
            usage: Default::default(),
            total_tool_use_count: 0,
            total_duration_ms: 0,
            assistant_message_count: 0,
            last_request_id: None,
        };
        sink.try_emit(progress());
        observer.started.notified().await;
        for _ in 0..OBSERVER_EVENT_BUFFER + 10 {
            sink.try_emit(progress());
        }
        let mut tool_result = lingxi_core::types::ConversationMessage::user(
            lingxi_core::types::MessageId::new(),
            String::new(),
        );
        if let lingxi_core::types::ConversationMessage::User { content, .. } = &mut tool_result {
            *content = vec![lingxi_core::types::ContentBlock::ToolResult { content_projection: None,
                tool_use_id: lingxi_core::types::ToolUseId::new(),
                content: "ordinary tool output".into(),
                is_error: Some(false),
                provider_tool_use_id: None,
                content_blocks: None,
            }];
        }
        sink.try_emit(SubagentObservation::Message {
            agent_id,
            message: tool_result,
        });
        sink.try_emit(SubagentObservation::Allocated {
            agent_id,
            agent_type: "general-purpose".into(),
            name: None,
            model: "test".into(),
            model_profile: None,
            persistent: true,
            initial_message_index: 0,
            origin_session_id: None,
        });
        sink.emit_terminal(completed());
        sink.try_emit(SubagentObservation::Message {
            agent_id,
            message: lingxi_core::types::ConversationMessage::System { api_system: None,
                id: lingxi_core::types::MessageId::new(),
                content: "idle".into(),
                subtype: Some("agent_idle".into()),
                compact_metadata: None,
                model_fallback: None,
                refusal_fallback: None,
            },
        });
        sink.try_emit(SubagentObservation::Message {
            agent_id,
            message: lingxi_core::types::ConversationMessage::user(
                lingxi_core::types::MessageId::new(),
                "resume".into(),
            ),
        });
        sink.try_emit(SubagentObservation::Message {
            agent_id,
            message: lingxi_core::types::ConversationMessage::user_meta(
                lingxi_core::types::MessageId::new(),
                "wake".into(),
            ),
        });
        sink.emit_terminal(completed());
        sink.emit_terminal(SubagentObservation::Failed {
            agent_id,
            error: "failure".into(),
        });
        sink.emit_terminal(SubagentObservation::Killed { agent_id });
        assert!(
            observer.events.lock().unwrap().is_empty(),
            "producer did not wait for blocked observer"
        );
        observer.release.notify_one();
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            observer.finished.notified(),
        )
        .await
        .expect("all queued lifecycle events arrive");
        let events = observer.events.lock().unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, SubagentObservation::Progress { .. }))
                .count(),
            OBSERVER_EVENT_BUFFER + 1
        );
        let lifecycle = events
            .iter()
            .filter_map(|event| match event {
                SubagentObservation::Allocated { .. } => Some("allocated"),
                SubagentObservation::Completed { .. } => Some("completed"),
                SubagentObservation::Message {
                    message:
                        lingxi_core::types::ConversationMessage::System {
                            subtype: Some(subtype),
                            ..
                        },
                    ..
                } if subtype == "agent_idle" => Some("idle"),
                SubagentObservation::Message {
                    message: lingxi_core::types::ConversationMessage::User { is_meta, .. },
                    ..
                } => Some(if *is_meta { "wake" } else { "resume" }),
                SubagentObservation::Failed { .. } => Some("failed"),
                SubagentObservation::Killed { .. } => Some("killed"),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            lifecycle,
            [
                "allocated",
                "completed",
                "idle",
                "resume",
                "wake",
                "completed",
                "failed",
                "killed"
            ]
        );
    }
}
