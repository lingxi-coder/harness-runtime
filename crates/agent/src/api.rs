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
//! The required method is a single non-streaming round-trip. A streaming
//! variant ([`SubagentApiClient::messages_create_stream`]) layers on top with a
//! default that wraps the non-streaming call, so retry/cost wiring lives behind
//! the concrete impl exactly as the orchestrator's `execute_one_turn` consumes
//! it. The production orchestrator adapter overrides the streaming method to
//! delegate to its real SSE transport.

use async_trait::async_trait;
use futures::stream::{BoxStream, StreamExt};
use llm_runtime::{HistoryEvent, HistoryResponse, LlmError};
use platform_api::{SubagentObservation, SubagentSpawnObserver, WorkflowQueryWatchdog};
use protocol::{AgentId, SessionId};
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
                    protocol::ConversationMessage::System {
                        subtype: Some(subtype),
                        ..
                    },
                ..
            } if subtype == "agent_idle" => true,
            SubagentObservation::Message {
                message: protocol::ConversationMessage::User { content, .. },
                ..
            } => !content.iter().any(|block| {
                matches!(
                    block,
                    protocol::ContentBlock::ToolResult { .. }
                        | protocol::ContentBlock::AdvisorToolResult { .. }
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

    /// Issue one non-streaming model round-trip.
    ///
    /// `system` is the assembled system prompt (stable across the run);
    /// `messages` is the full conversation history, oldest first; `tools` is
    /// the wire tool-definition array (`{name, description, input_schema}`)
    /// advertised to the model, from [`crate::context::SubagentContext::tool_schemas`]
    /// (empty = no tools).
    async fn messages_create(
        &self,
        model: &str,
        system: Option<&str>,
        messages: Vec<protocol::ConversationMessage>,
        tools: Vec<serde_json::Value>,
    ) -> Result<HistoryResponse, LlmError>;

    /// Issue one model round-trip over the streaming SSE transport, returning
    /// the wire-decoded [`HistoryEvent`] stream (yielding until `message_stop` or
    /// `completed`). The [`crate::runner::run_subagent`] loop drains this
    /// through `crate::accumulator::accumulate_stream` into the same
    /// `HistoryResponse` the non-streaming path returns, so the turn loop is
    /// transport-agnostic.
    ///
    /// The default wraps [`SubagentApiClient::messages_create`] in a synthetic,
    /// lossless event sequence — a client that only implements the
    /// non-streaming round-trip still satisfies this seam (the round-trip
    /// reproduces the response exactly). The production orchestrator adapter
    /// overrides this to delegate to its real `StreamingApiClient::stream`.
    /// `effort` is the per-request thinking-effort hint (claude-code
    /// `output_config.effort`): a level string or integer budget, or `None`.
    /// The default (synthetic, non-streaming) path ignores it — only the
    /// production provider adapter threads it onto the request + emits the
    /// `effort-2025-11-24` beta.
    async fn messages_create_stream(
        &self,
        model: &str,
        system: Option<&str>,
        messages: Vec<protocol::ConversationMessage>,
        tools: Vec<serde_json::Value>,
        effort: Option<serde_json::Value>,
    ) -> Result<BoxStream<'static, Result<HistoryEvent, LlmError>>, LlmError> {
        let _ = effort;
        let resp = self.messages_create(model, system, messages, tools).await?;
        let events = crate::accumulator::response_to_stream_events(resp);
        Ok(futures::stream::iter(events.into_iter().map(Ok)).boxed())
    }

    /// Like [`Self::messages_create_stream`], but FORCES the model to call the
    /// named tool (`tool_choice`) — used to make a subagent emit structured
    /// output by forcing a synthetic `StructuredOutput` tool. The default
    /// implementation ignores `forced_tool` (no forcing), so existing impls and
    /// the non-schema path are unchanged; the production provider adapter
    /// overrides it to thread `tool_choice` into the request.
    async fn messages_create_stream_forced(
        &self,
        model: &str,
        system: Option<&str>,
        messages: Vec<protocol::ConversationMessage>,
        tools: Vec<serde_json::Value>,
        forced_tool: Option<&str>,
        effort: Option<serde_json::Value>,
    ) -> Result<BoxStream<'static, Result<HistoryEvent, LlmError>>, LlmError> {
        let _ = forced_tool;
        self.messages_create_stream(model, system, messages, tools, effort)
            .await
    }

    /// Like [`Self::messages_create`], but threads a provider `profile` so the
    /// per-spawn route (e.g. a dual-LLM candidate's resolved provider) reaches
    /// the underlying multi-provider client. The DEFAULT body ignores `profile`
    /// and delegates to [`Self::messages_create`] — so the ~5 existing impls and
    /// test mocks that only implement the profile-less method keep their legacy
    /// (default-provider) behavior unchanged (frozen-trait rule). The production
    /// orchestrator adapter OVERRIDES this to forward `profile` to
    /// `ModelRuntime::messages_create(model, profile, …)`.
    async fn messages_create_in(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        messages: Vec<protocol::ConversationMessage>,
        tools: Vec<serde_json::Value>,
    ) -> Result<HistoryResponse, LlmError> {
        let _ = profile;
        self.messages_create(model, system, messages, tools).await
    }

    /// Streaming analog of [`Self::messages_create_in`] — threads the provider
    /// `profile` onto the SSE round-trip. The DEFAULT body ignores `profile` and
    /// delegates to [`Self::messages_create_stream`] (which itself defaults to a
    /// synthetic stream over [`Self::messages_create`]), so non-streaming /
    /// profile-less impls and mocks are unaffected. The production orchestrator
    /// adapter OVERRIDES this to forward `profile` to its real SSE transport.
    async fn messages_create_stream_in(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        messages: Vec<protocol::ConversationMessage>,
        tools: Vec<serde_json::Value>,
        effort: Option<serde_json::Value>,
    ) -> Result<BoxStream<'static, Result<HistoryEvent, LlmError>>, LlmError> {
        let _ = profile;
        self.messages_create_stream(model, system, messages, tools, effort)
            .await
    }

    /// Like [`Self::messages_create_stream_in`], but FORCES the named tool
    /// (`tool_choice`). The DEFAULT delegates to
    /// [`Self::messages_create_stream_forced`] (ignoring `profile`); the
    /// production adapter overrides it to thread BOTH `profile` and the forced
    /// tool. Keeps the structured-output (schema) subagent path provider-routed.
    async fn messages_create_stream_forced_in(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        messages: Vec<protocol::ConversationMessage>,
        tools: Vec<serde_json::Value>,
        forced_tool: Option<&str>,
        effort: Option<serde_json::Value>,
    ) -> Result<BoxStream<'static, Result<HistoryEvent, LlmError>>, LlmError> {
        let _ = profile;
        self.messages_create_stream_forced(model, system, messages, tools, forced_tool, effort)
            .await
    }

    /// Like [`Self::messages_create_stream_in`], with Fusion per-turn ceilings
    /// and a COGS query-source label. The default ignores `opts`.
    async fn messages_create_stream_in_opts(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        messages: Vec<protocol::ConversationMessage>,
        tools: Vec<serde_json::Value>,
        effort: Option<serde_json::Value>,
        opts: SubagentApiCallOpts,
    ) -> Result<BoxStream<'static, Result<HistoryEvent, LlmError>>, LlmError> {
        if opts.model_attempt.is_some() {
            return Err(LlmError::InvalidRequest {
                message: "registered model attempt requires an opts-aware host adapter".into(),
            });
        }
        self.messages_create_stream_in(model, profile, system, messages, tools, effort)
            .await
    }

    /// Like [`Self::messages_create_stream_forced_in`], with Fusion per-turn
    /// ceilings. The default ignores `opts`.
    async fn messages_create_stream_forced_in_opts(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        messages: Vec<protocol::ConversationMessage>,
        tools: Vec<serde_json::Value>,
        forced_tool: Option<&str>,
        effort: Option<serde_json::Value>,
        opts: SubagentApiCallOpts,
    ) -> Result<BoxStream<'static, Result<HistoryEvent, LlmError>>, LlmError> {
        if opts.model_attempt.is_some() {
            return Err(LlmError::InvalidRequest {
                message: "registered model attempt requires an opts-aware host adapter".into(),
            });
        }
        self.messages_create_stream_forced_in(
            model,
            profile,
            system,
            messages,
            tools,
            forced_tool,
            effort,
        )
        .await
    }
}

/// Optional per-round-trip Fusion / COGS knobs.
#[derive(Debug, Clone, Default)]
pub struct SubagentApiCallOpts {
    /// Trusted per-logical-call capability; retries retain this exact context.
    pub model_attempt: Option<platform_api::ModelAttemptContext>,
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

    async fn messages_create(
        &self,
        model: &str,
        system: Option<&str>,
        messages: Vec<protocol::ConversationMessage>,
        tools: Vec<serde_json::Value>,
    ) -> Result<HistoryResponse, LlmError> {
        self.inner
            .messages_create(model, system, messages, tools)
            .await
    }

    async fn messages_create_stream(
        &self,
        model: &str,
        system: Option<&str>,
        messages: Vec<protocol::ConversationMessage>,
        tools: Vec<serde_json::Value>,
        effort: Option<serde_json::Value>,
    ) -> Result<BoxStream<'static, Result<HistoryEvent, LlmError>>, LlmError> {
        self.inner
            .messages_create_stream(model, system, messages, tools, effort)
            .await
    }

    async fn messages_create_stream_forced(
        &self,
        model: &str,
        system: Option<&str>,
        messages: Vec<protocol::ConversationMessage>,
        tools: Vec<serde_json::Value>,
        forced_tool: Option<&str>,
        effort: Option<serde_json::Value>,
    ) -> Result<BoxStream<'static, Result<HistoryEvent, LlmError>>, LlmError> {
        self.inner
            .messages_create_stream_forced(model, system, messages, tools, forced_tool, effort)
            .await
    }

    async fn messages_create_in(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        messages: Vec<protocol::ConversationMessage>,
        tools: Vec<serde_json::Value>,
    ) -> Result<HistoryResponse, LlmError> {
        self.inner
            .messages_create_in(model, profile, system, messages, tools)
            .await
    }

    async fn messages_create_stream_in(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        messages: Vec<protocol::ConversationMessage>,
        tools: Vec<serde_json::Value>,
        effort: Option<serde_json::Value>,
    ) -> Result<BoxStream<'static, Result<HistoryEvent, LlmError>>, LlmError> {
        self.inner
            .messages_create_stream_in(model, profile, system, messages, tools, effort)
            .await
    }

    async fn messages_create_stream_forced_in(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        messages: Vec<protocol::ConversationMessage>,
        tools: Vec<serde_json::Value>,
        forced_tool: Option<&str>,
        effort: Option<serde_json::Value>,
    ) -> Result<BoxStream<'static, Result<HistoryEvent, LlmError>>, LlmError> {
        self.inner
            .messages_create_stream_forced_in(
                model,
                profile,
                system,
                messages,
                tools,
                forced_tool,
                effort,
            )
            .await
    }

    // WP2a item 2 (F002 sub-claim 3), round 2: WITHOUT these two overrides the
    // trait's default `_in_opts` bodies re-dispatch through `self` (this
    // wrapper)'s non-opts methods above, silently dropping `opts` — the exact
    // Fusion per-turn `max_output_tokens` ceiling and `query_source_label`
    // this seam exists to carry — on the ONLY path that spawns a Fusion panel
    // (`panel.rs` -> `handle.rs`'s `WORKFLOW_QUERY_WATCHDOG_OVERRIDE` wraps
    // `ctx.api_client` in this decorator). Delegate verbatim, same as every
    // other method on this impl.
    async fn messages_create_stream_in_opts(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        messages: Vec<protocol::ConversationMessage>,
        tools: Vec<serde_json::Value>,
        effort: Option<serde_json::Value>,
        opts: SubagentApiCallOpts,
    ) -> Result<BoxStream<'static, Result<HistoryEvent, LlmError>>, LlmError> {
        self.inner
            .messages_create_stream_in_opts(model, profile, system, messages, tools, effort, opts)
            .await
    }

    async fn messages_create_stream_forced_in_opts(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        messages: Vec<protocol::ConversationMessage>,
        tools: Vec<serde_json::Value>,
        forced_tool: Option<&str>,
        effort: Option<serde_json::Value>,
        opts: SubagentApiCallOpts,
    ) -> Result<BoxStream<'static, Result<HistoryEvent, LlmError>>, LlmError> {
        self.inner
            .messages_create_stream_forced_in_opts(
                model,
                profile,
                system,
                messages,
                tools,
                forced_tool,
                effort,
                opts,
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

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
        let mut tool_result =
            protocol::ConversationMessage::user(protocol::MessageId::new(), String::new());
        if let protocol::ConversationMessage::User { content, .. } = &mut tool_result {
            *content = vec![protocol::ContentBlock::ToolResult {
                tool_use_id: protocol::ToolUseId::new(),
                content: "ordinary tool output".into(),
                is_error: false,
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
            message: protocol::ConversationMessage::System {
                id: protocol::MessageId::new(),
                content: "idle".into(),
                subtype: Some("agent_idle".into()),
                compact_metadata: None,
                refusal_fallback: None,
            },
        });
        sink.try_emit(SubagentObservation::Message {
            agent_id,
            message: protocol::ConversationMessage::user(
                protocol::MessageId::new(),
                "resume".into(),
            ),
        });
        sink.try_emit(SubagentObservation::Message {
            agent_id,
            message: protocol::ConversationMessage::user_meta(
                protocol::MessageId::new(),
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
                        protocol::ConversationMessage::System {
                            subtype: Some(subtype),
                            ..
                        },
                    ..
                } if subtype == "agent_idle" => Some("idle"),
                SubagentObservation::Message {
                    message: protocol::ConversationMessage::User { is_meta, .. },
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

    /// A mock that implements ONLY the required `messages_create`. It records
    /// the call so we can prove the DEFAULTED `messages_create_in` routes back
    /// through it (ignoring the profile) without the impl knowing about the new
    /// method — the frozen-trait back-compat guarantee.
    struct LegacyOnlyClient {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl SubagentApiClient for LegacyOnlyClient {
        async fn messages_create(
            &self,
            _model: &str,
            _system: Option<&str>,
            _messages: Vec<protocol::ConversationMessage>,
            _tools: Vec<serde_json::Value>,
        ) -> Result<HistoryResponse, LlmError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(HistoryResponse {
                id: "mock".into(),
                model: "mock".into(),
                content: Vec::new(),
                stop_reason: Some("end_turn".into()),
                stop_details: None,
                usage: llm_runtime::Usage::default(),
                cost: None,
                provider_metadata: serde_json::Value::Null,
            })
        }
    }

    #[tokio::test]
    async fn messages_create_in_default_routes_through_legacy_method() {
        let client = Arc::new(LegacyOnlyClient {
            calls: AtomicUsize::new(0),
        });
        // Calling the NEW profile-aware method on a mock that only implements
        // the legacy one must transparently fall through to `messages_create`.
        let resp = client
            .messages_create_in(
                "some-model",
                Some("a-profile"),
                None,
                Vec::new(),
                Vec::new(),
            )
            .await;
        assert!(resp.is_ok(), "default messages_create_in delegates cleanly");
        assert_eq!(
            client.calls.load(Ordering::SeqCst),
            1,
            "the defaulted messages_create_in must route through the legacy messages_create"
        );
    }
}
