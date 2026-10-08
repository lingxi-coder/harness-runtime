//! Extracted tests for `agent::runner`.

use super::*;
use crate::definition::{
    AgentDefinition, AgentModel, AgentPermissionMode, AgentSource, AgentToolPolicy,
};
use crate::display::{AgentColor, AgentDisplay};
use async_trait::async_trait;
use lingxi_core::types::{ContentBlock, ConversationMessage, MessageId, RequestId, ToolUseId};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use tokio::sync::mpsc;

#[test]
fn cumulative_usage_keeps_reasoning_as_output_subset() {
    let mut total = llm_runtime::ExecutionUsage::default();
    for (output, reasoning) in [(12, 5), (20, 7)] {
        let turn = llm_runtime::ExecutionUsage::from_counts(llm_runtime::Usage {
            output_tokens: output,
            reasoning_tokens: reasoning,
            ..Default::default()
        });
        accumulate_usage(&mut total, &turn);
    }
    assert_eq!(total.counts().output_tokens, 32);
    assert_eq!(total.counts().reasoning_tokens, 12);
}

// ---- Scripted loop-mode fixtures -------------------------------------

/// `SubagentApiClient` that hands back a pre-scripted queue of responses,
/// one per typed streaming request. Counts calls so tests can assert the
/// number of model round-trips (`max_turns` bound, multi-turn loop).
struct MockSubagentApiClient {
    responses: Mutex<VecDeque<Result<llm_runtime::HistoryResponse, llm_runtime::LlmError>>>,
    calls: AtomicUsize,
    physical_calls: Mutex<Vec<CapturedSubagentCall>>,
    /// The messages the LAST call was given — lets a test assert what the
    /// runner actually seeded the conversation with.
    last_messages: Mutex<Vec<ConversationMessage>>,
    last_tools: Mutex<Vec<lingxi_core::types::utf16_json::Utf16JsonProjection>>,
}

#[derive(Clone, Debug)]
struct CapturedSubagentCall {
    request: crate::api::SubagentApiRequest,
    response: Option<llm_runtime::HistoryResponse>,
    fallback_target: Option<lingxi_core::host::refusal_driver::FallbackTargetContext>,
}

impl MockSubagentApiClient {
    fn new(
        responses: Vec<Result<llm_runtime::HistoryResponse, llm_runtime::LlmError>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(responses.into_iter().collect()),
            calls: AtomicUsize::new(0),
            physical_calls: Mutex::new(Vec::new()),
            last_messages: Mutex::new(Vec::new()),
            last_tools: Mutex::new(Vec::new()),
        })
    }
    fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
    fn last_messages(&self) -> Vec<ConversationMessage> {
        self.last_messages.lock().unwrap().clone()
    }
    fn physical_calls(&self) -> Vec<CapturedSubagentCall> {
        self.physical_calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl crate::api::SubagentApiClient for MockSubagentApiClient {
    async fn stream(
        &self,
        request: crate::api::SubagentApiRequest,
    ) -> Result<
        futures::stream::BoxStream<
            'static,
            Result<llm_runtime::HistoryEvent, llm_runtime::LlmError>,
        >,
        llm_runtime::LlmError,
    > {
        let request_for_capture = request.clone();
        let _model = request.model.as_str();
        let _system = request.system.as_deref();
        let _messages = request.messages;
        let _tools = request.tools;
        let response: Result<llm_runtime::HistoryResponse, llm_runtime::LlmError> = async {
            *self.last_messages.lock().unwrap() = _messages.clone();
            *self.last_tools.lock().unwrap() = _tools;
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| {
                    // Out of scripted responses: a non-terminal, no-tool turn keeps
                    // the loop honest (it terminates on empty tool_uses).
                    Ok(text_response("(exhausted)", Some("end_turn")))
                })
        }
        .await;
        self.physical_calls
            .lock()
            .unwrap()
            .push(CapturedSubagentCall {
                request: request_for_capture,
                response: response.as_ref().ok().cloned(),
                fallback_target: lingxi_core::host::refusal_driver::current_fallback_target(),
            });
        let events = llm_runtime::stream_accumulator::response_to_stream_events(response?);
        Ok(futures::StreamExt::boxed(futures::stream::iter(
            events.into_iter().map(Ok),
        )))
    }
}

/// Scripted decoded event streams, including real per-turn tool/history events.
struct StreamingMockApiClient {
    turns: Mutex<VecDeque<Vec<llm_runtime::HistoryEvent>>>,
    calls: AtomicUsize,
    /// Tools seen on the most recent streaming request — lets a
    /// test prove `ctx.tool_schemas` threads through the seam.
    last_tools: Mutex<Vec<lingxi_core::types::utf16_json::Utf16JsonProjection>>,
    /// The model each call was issued against, in order — lets a test prove a
    /// refusal hop actually re-issued against the fallback.
    models: Mutex<Vec<String>>,
    /// Provider profile used for each request, in order.
    profiles: Mutex<Vec<Option<String>>>,
    /// Complete physical request/response snapshots. The older migrated mock
    /// only kept the last request's routing fields, which hid lost prompt,
    /// effort, usage and stop metadata across multiple next calls.
    physical_calls: Mutex<Vec<CapturedSubagentCall>>,
    refusal_snapshot: Mutex<Option<lingxi_core::host::refusal_api_text::RefusalApiTextSnapshot>>,
    refusal_snapshot_requests: Mutex<Vec<(String, Option<String>)>>,
}

impl StreamingMockApiClient {
    fn new(turns: Vec<Vec<llm_runtime::HistoryEvent>>) -> Arc<Self> {
        Arc::new(Self {
            turns: Mutex::new(turns.into_iter().collect()),
            calls: AtomicUsize::new(0),
            last_tools: Mutex::new(Vec::new()),
            models: Mutex::new(Vec::new()),
            profiles: Mutex::new(Vec::new()),
            physical_calls: Mutex::new(Vec::new()),
            refusal_snapshot: Mutex::new(None),
            refusal_snapshot_requests: Mutex::new(Vec::new()),
        })
    }

    fn set_refusal_snapshot(
        &self,
        snapshot: lingxi_core::host::refusal_api_text::RefusalApiTextSnapshot,
    ) {
        *self.refusal_snapshot.lock().unwrap() = Some(snapshot);
    }

    fn models(&self) -> Vec<String> {
        self.models.lock().unwrap().clone()
    }
    fn profiles(&self) -> Vec<Option<String>> {
        self.profiles.lock().unwrap().clone()
    }
    fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
    fn last_tools(&self) -> Vec<lingxi_core::types::utf16_json::Utf16JsonProjection> {
        self.last_tools.lock().unwrap().clone()
    }
    fn physical_calls(&self) -> Vec<CapturedSubagentCall> {
        self.physical_calls.lock().unwrap().clone()
    }

    fn refusal_snapshot_requests(&self) -> Vec<(String, Option<String>)> {
        self.refusal_snapshot_requests.lock().unwrap().clone()
    }
}

#[async_trait]
impl crate::api::SubagentApiClient for StreamingMockApiClient {
    fn refusal_api_text_snapshot(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> Result<
        Option<lingxi_core::host::refusal_api_text::RefusalApiTextSnapshot>,
        llm_runtime::LlmError,
    > {
        self.refusal_snapshot_requests
            .lock()
            .unwrap()
            .push((model.to_owned(), profile.map(str::to_owned)));
        Ok(self.refusal_snapshot.lock().unwrap().clone())
    }

    async fn stream(
        &self,
        request: crate::api::SubagentApiRequest,
    ) -> Result<
        futures::stream::BoxStream<
            'static,
            Result<llm_runtime::HistoryEvent, llm_runtime::LlmError>,
        >,
        llm_runtime::LlmError,
    > {
        let request_for_capture = request.clone();
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.models.lock().unwrap().push(request.model);
        self.profiles.lock().unwrap().push(request.profile);
        *self.last_tools.lock().unwrap() = request.tools;
        let events = self.turns.lock().unwrap().pop_front().unwrap_or_default();
        let response = llm_runtime::stream_accumulator::accumulate_stream_salvaging(
            futures::stream::iter(events.clone().into_iter().map(Ok)).boxed(),
        )
        .await
        .ok();
        self.physical_calls
            .lock()
            .unwrap()
            .push(CapturedSubagentCall {
                request: request_for_capture,
                response,
                fallback_target: lingxi_core::host::refusal_driver::current_fallback_target(),
            });
        Ok(futures::StreamExt::boxed(futures::stream::iter(
            events.into_iter().map(Ok),
        )))
    }
}

struct GatedFallbackApiClient {
    fallback_gate: Arc<tokio::sync::Notify>,
    terminal_gate: Arc<tokio::sync::Notify>,
    terminal_waiting: Arc<AtomicBool>,
    tail_polls: Arc<AtomicUsize>,
    models: Mutex<Vec<String>>,
    first_events: Vec<llm_runtime::HistoryEvent>,
    later_responses: Mutex<VecDeque<llm_runtime::HistoryResponse>>,
    calls: AtomicUsize,
}

struct ToolThenStreamErrorApi;

#[async_trait]
impl crate::api::SubagentApiClient for ToolThenStreamErrorApi {
    async fn stream(
        &self,
        _request: crate::api::SubagentApiRequest,
    ) -> Result<
        futures::stream::BoxStream<
            'static,
            Result<llm_runtime::HistoryEvent, llm_runtime::LlmError>,
        >,
        llm_runtime::LlmError,
    > {
        let events = vec![
            ev_message_start(),
            llm_runtime::HistoryEvent::ContentBlockStart {
                index: 2,
                content_block: llm_runtime::ContentBlock::ToolCall {
                    input_projection: None,
                    id: "toolu-before-error".into(),
                    name: "Agent".into(),
                    input: serde_json::Value::Null,
                },
            },
            llm_runtime::HistoryEvent::ContentBlockDelta {
                index: 2,
                delta: llm_runtime::HistoryContentDelta::InputJsonDelta {
                    partial_json: "{}".into(),
                },
            },
            llm_runtime::HistoryEvent::ContentBlockStop { index: 2 },
        ];
        let stream = futures::stream::iter(events.into_iter().map(Ok).chain(std::iter::once(Err(
            llm_runtime::LlmError::Transport {
                message: "provider stream broke".into(),
            },
        ))));
        Ok(futures::StreamExt::boxed(stream))
    }
}

impl GatedFallbackApiClient {
    fn new(
        discarded_blocks: Vec<usize>,
        later_responses: Vec<llm_runtime::HistoryResponse>,
    ) -> Arc<Self> {
        let server_fallback: llm_runtime::HistoryEvent =
            serde_json::from_value(serde_json::json!({
                "type": "server_fallback",
                "event": {
                    "fromModel": "primary-model",
                    "toModel": "fallback-model",
                    "reason": "sticky",
                    "apiRefusalCategory": null,
                    "midStream": true,
                    "requestId": "fallback-request",
                    "discardedBlocks": discarded_blocks,
                    "retainedBlocks": [],
                    "retainedText": "",
                    "finalStopReason": null
                },
                "profile": "fallback-provider",
                "lane": {
                    "forModel": "primary-model",
                    "model": "primary-model",
                    "mode": "explicit"
                }
            }))
            .expect("typed server fallback event fixture");
        let first_events = vec![
            ev_message_start(),
            llm_runtime::HistoryEvent::ContentBlockStart {
                index: 0,
                content_block: llm_runtime::ContentBlock::ToolCall {
                    input_projection: None,
                    id: "toolu-before-hop".into(),
                    name: "Agent".into(),
                    input: serde_json::Value::Null,
                },
            },
            llm_runtime::HistoryEvent::ContentBlockDelta {
                index: 0,
                delta: llm_runtime::HistoryContentDelta::InputJsonDelta {
                    partial_json: "{}".into(),
                },
            },
            llm_runtime::HistoryEvent::ContentBlockStop { index: 0 },
            server_fallback,
            llm_runtime::HistoryEvent::ContentBlockStart {
                index: 1,
                content_block: llm_runtime::ContentBlock::ToolCall {
                    input_projection: None,
                    id: "toolu-after-hop".into(),
                    name: "Agent".into(),
                    input: serde_json::Value::Null,
                },
            },
            llm_runtime::HistoryEvent::ContentBlockDelta {
                index: 1,
                delta: llm_runtime::HistoryContentDelta::InputJsonDelta {
                    partial_json: "{}".into(),
                },
            },
            llm_runtime::HistoryEvent::ContentBlockStop { index: 1 },
            llm_runtime::HistoryEvent::MessageDelta {
                delta: llm_runtime::HistoryMessageDelta {
                    stop_reason: Some("tool_use".into()),
                    stop_details: None,
                },
                usage: None,
            },
            llm_runtime::HistoryEvent::MessageStop,
        ];
        Arc::new(Self {
            fallback_gate: Arc::new(tokio::sync::Notify::new()),
            terminal_gate: Arc::new(tokio::sync::Notify::new()),
            terminal_waiting: Arc::new(AtomicBool::new(false)),
            tail_polls: Arc::new(AtomicUsize::new(0)),
            models: Mutex::new(Vec::new()),
            first_events,
            later_responses: Mutex::new(later_responses.into()),
            calls: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl crate::api::SubagentApiClient for GatedFallbackApiClient {
    async fn stream(
        &self,
        request: crate::api::SubagentApiRequest,
    ) -> Result<
        futures::stream::BoxStream<
            'static,
            Result<llm_runtime::HistoryEvent, llm_runtime::LlmError>,
        >,
        llm_runtime::LlmError,
    > {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        self.models.lock().unwrap().push(request.model.clone());
        if call == 0 {
            let events = self.first_events.clone();
            let gate = self.fallback_gate.clone();
            let terminal_gate = self.terminal_gate.clone();
            let terminal_waiting = self.terminal_waiting.clone();
            let tail_polls = self.tail_polls.clone();
            let stream = futures::stream::unfold(0usize, move |index| {
                let events = events.clone();
                let gate = gate.clone();
                let terminal_gate = terminal_gate.clone();
                let terminal_waiting = terminal_waiting.clone();
                let tail_polls = tail_polls.clone();
                async move {
                    if index == 4 {
                        gate.notified().await;
                    } else if index == 8 {
                        terminal_waiting.store(true, Ordering::SeqCst);
                        terminal_gate.notified().await;
                        terminal_waiting.store(false, Ordering::SeqCst);
                    } else if index > 4 {
                        tail_polls.fetch_add(1, Ordering::SeqCst);
                    }
                    let event = events.get(index)?.clone();
                    Some((Ok(event), index + 1))
                }
            });
            return Ok(futures::StreamExt::boxed(stream));
        }

        let response = self
            .later_responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| text_response("exhausted", Some("end_turn")));
        let _ = request;
        let events = llm_runtime::stream_accumulator::response_to_stream_events(response);
        Ok(futures::StreamExt::boxed(futures::stream::iter(
            events.into_iter().map(Ok),
        )))
    }
}

struct FallbackCancellationProbeInvoker {
    first_started:
        Mutex<Option<tokio::sync::oneshot::Sender<lingxi_core::host::CancellationToken>>>,
    calls: AtomicUsize,
}

#[derive(Default)]
struct CaptureLiveInvocationContexts {
    contexts: Mutex<Vec<lingxi_core::host::tool_invoker::SubagentInvocationContext>>,
}

#[async_trait]
impl lingxi_core::host::ToolInvoker for CaptureLiveInvocationContexts {
    async fn invoke(
        &self,
        _name: &str,
        _input: serde_json::Value,
        ctx: lingxi_core::host::tool_invoker::SubagentInvocationContext,
    ) -> Result<serde_json::Value, lingxi_core::host::tool_invoker::ToolInvokerError> {
        self.contexts.lock().unwrap().push(ctx);
        Ok(serde_json::json!("ok"))
    }

    async fn invoke_detailed(
        &self,
        _name: &str,
        _input: serde_json::Value,
        ctx: lingxi_core::host::tool_invoker::SubagentInvocationContext,
        _workspace_lease_token: Option<u64>,
    ) -> Result<
        lingxi_core::host::tool_invoker::ToolInvocationResult,
        lingxi_core::host::tool_invoker::ToolInvokerError,
    > {
        let tool_use_id = ctx.tool_use_id.clone().unwrap_or_default();
        self.contexts.lock().unwrap().push(ctx);
        Ok(lingxi_core::host::tool_invoker::ToolInvocationResult {
            mcp_meta_projection: None,
            model_content_projection: None,
            data_projection: None,
            is_error: false,
            data: serde_json::json!("ok"),
            model_content: None,
            new_messages: vec![
                ConversationMessage::User { api_message_override: None,
                    id: MessageId::new(),
                    content: vec![
                        ContentBlock::Text {
                            text: format!("injected note {tool_use_id}"),
                            citations: None,
                        },
                        ContentBlock::Document {
                            source: lingxi_core::types::DocumentSource::Base64 {
                                media_type: "application/pdf".into(),
                                data: "cGRm".into(),
                            },
                        },
                    ],
                    is_meta: true,
                    is_compact_summary: false,
                    is_visible_in_transcript_only: false,
                },
                ConversationMessage::Assistant { per_turn_effort: None,
                    id: MessageId::new(),
                    content: vec![ContentBlock::Text {
                        text: format!("executor assistant context {tool_use_id}"),
                        citations: None,
                    }],
                    stop_reason: None,
                },
                ConversationMessage::System { api_system: None,
                    id: MessageId::new(),
                    content: format!("executor system context {tool_use_id}"),
                    subtype: None,
                    compact_metadata: None,
                    model_fallback: None,
                    refusal_fallback: None,
                },
            ],
            context_modifier: None,
            mcp_meta: None,
            turn_end: None,
            context: lingxi_core::types::utf16_json::Utf16JsonProjection::plain(
                serde_json::Value::Array(Vec::new()),
            ),
            context_state: None,
        })
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl FallbackCancellationProbeInvoker {
    fn new() -> (
        Arc<Self>,
        tokio::sync::oneshot::Receiver<lingxi_core::host::CancellationToken>,
    ) {
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        (
            Arc::new(Self {
                first_started: Mutex::new(Some(started_tx)),
                calls: AtomicUsize::new(0),
            }),
            started_rx,
        )
    }
}

#[async_trait]
impl lingxi_core::host::ToolInvoker for FallbackCancellationProbeInvoker {
    async fn invoke(
        &self,
        _name: &str,
        _input: serde_json::Value,
        ctx: lingxi_core::host::tool_invoker::SubagentInvocationContext,
    ) -> Result<serde_json::Value, lingxi_core::host::tool_invoker::ToolInvokerError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if matches!(
            ctx.tool_use_id.as_deref(),
            Some("toolu-before-hop" | "toolu-before-error")
        ) {
            if let Some(tx) = self.first_started.lock().unwrap().take() {
                let _ = tx.send(ctx.cancellation_token.clone());
            }
            ctx.cancellation_token.cancelled().await;
            return Err(lingxi_core::host::tool_invoker::ToolInvokerError::Abort(
                "server fallback swept the call".into(),
            ));
        }
        Ok(serde_json::json!("post-hop-tool-result"))
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[derive(Default)]
struct IdleFactCaptureRegistry {
    updates: Mutex<Vec<bool>>,
}

#[async_trait]
impl lingxi_core::host::TaskRegistryHandle for IdleFactCaptureRegistry {
    async fn update_agent_list_local_fact(
        &self,
        _agent_id: lingxi_core::types::AgentId,
        update: lingxi_core::host::task_registry::AgentListLocalFactUpdate,
    ) -> Result<(), lingxi_core::host::task_registry::TaskRegistryError> {
        if let lingxi_core::host::task_registry::AgentListLocalFactUpdate::IsIdle(idle) = update {
            self.updates.lock().unwrap().push(idle);
        }
        Ok(())
    }

    async fn create(
        &self,
        _input: lingxi_core::host::task_registry::TaskCreateInput,
    ) -> Result<
        lingxi_core::host::task_registry::TaskRecord,
        lingxi_core::host::task_registry::TaskRegistryError,
    > {
        Err(
            lingxi_core::host::task_registry::TaskRegistryError::Internal(
                "unused test method".into(),
            ),
        )
    }

    async fn get(
        &self,
        _id: &str,
    ) -> Result<
        Option<lingxi_core::host::task_registry::TaskRecord>,
        lingxi_core::host::task_registry::TaskRegistryError,
    > {
        Ok(None)
    }

    async fn list(
        &self,
        _filter: lingxi_core::host::task_registry::TaskListFilter,
    ) -> Result<
        Vec<lingxi_core::host::task_registry::TaskRecord>,
        lingxi_core::host::task_registry::TaskRegistryError,
    > {
        Ok(Vec::new())
    }

    async fn update(
        &self,
        _id: &str,
        _patch: lingxi_core::host::task_registry::TaskUpdatePatch,
    ) -> Result<
        lingxi_core::host::task_registry::TaskRecord,
        lingxi_core::host::task_registry::TaskRegistryError,
    > {
        Err(
            lingxi_core::host::task_registry::TaskRegistryError::Internal(
                "unused test method".into(),
            ),
        )
    }

    async fn set_status(
        &self,
        _id: &str,
        _status: &str,
    ) -> Result<
        lingxi_core::host::task_registry::TaskRecord,
        lingxi_core::host::task_registry::TaskRegistryError,
    > {
        Err(
            lingxi_core::host::task_registry::TaskRegistryError::Internal(
                "unused test method".into(),
            ),
        )
    }

    async fn kill(
        &self,
        _id: &str,
    ) -> Result<
        lingxi_core::host::task_registry::TaskRecord,
        lingxi_core::host::task_registry::TaskRegistryError,
    > {
        Err(
            lingxi_core::host::task_registry::TaskRegistryError::Internal(
                "unused test method".into(),
            ),
        )
    }

    async fn output(
        &self,
        _id: &str,
        _offset: Option<u64>,
    ) -> Result<
        lingxi_core::host::TaskOutputChunk,
        lingxi_core::host::task_registry::TaskRegistryError,
    > {
        Err(
            lingxi_core::host::task_registry::TaskRegistryError::Internal(
                "unused test method".into(),
            ),
        )
    }
}

async fn run_gated_fallback_case(
    discarded_blocks: Vec<usize>,
    enforcement_models: &[&str],
    later_responses: Vec<llm_runtime::HistoryResponse>,
) -> (
    Arc<GatedFallbackApiClient>,
    Arc<FallbackCancellationProbeInvoker>,
    Arc<IdleFactCaptureRegistry>,
    lingxi_core::host::CancellationToken,
    Vec<SubagentEvent>,
) {
    let api = GatedFallbackApiClient::new(discarded_blocks, later_responses);
    let (invoker, started_rx) = FallbackCancellationProbeInvoker::new();
    let registry = Arc::new(IdleFactCaptureRegistry::default());
    let mut ctx = loop_ctx(
        api.clone(),
        Some(invoker.clone() as Arc<dyn lingxi_core::host::ToolInvoker>),
        3,
    );
    ctx.agent_definition.model = AgentModel::Explicit("primary-model".into());
    ctx.server_fallback_model_enforcement = Some(active_model_enforcement(enforcement_models));
    ctx.task_registry = Some(registry.clone());

    let (event_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(1);
    drop(event_tx);
    let (out_tx, mut out_rx) = mpsc::channel::<SubagentEvent>(64);
    let runner = tokio::spawn(run_subagent(ctx, event_rx, out_tx));

    let token = tokio::time::timeout(std::time::Duration::from_secs(2), started_rx)
        .await
        .expect("the tool must start before the stream reaches its fallback boundary")
        .expect("tool invocation start token");
    api.fallback_gate.notify_one();
    let mut events = Vec::new();
    if enforcement_models.contains(&"fallback-model") {
        loop {
            let event = tokio::time::timeout(std::time::Duration::from_secs(2), out_rx.recv())
                .await
                .expect("accepted fallback must continue to its replacement tool")
                .expect("runner output channel remains open");
            let reached_post_hop_tool_result = matches!(
                &event,
                SubagentEvent::Message { message, .. }
                    if message.to_string().contains("toolu-after-hop")
                        && message.to_string().contains("tool_result")
            );
            events.push(event);
            if reached_post_hop_tool_result {
                while !api.terminal_waiting.load(Ordering::SeqCst) {
                    tokio::task::yield_now().await;
                }
                api.terminal_gate.notify_one();
                break;
            }
        }
    }
    runner.await.expect("subagent runner task");
    events.extend(drain(out_rx).await);
    (api, invoker, registry, token, events)
}

#[tokio::test]
async fn declined_fallback_sweeps_started_tool_before_consuming_target_body() {
    let (api, invoker, registry, token, events) =
        run_gated_fallback_case(Vec::new(), &["primary-model"], Vec::new()).await;

    assert!(
        token.is_cancelled(),
        "the started tool receives real cancellation"
    );
    assert_eq!(invoker.calls.load(Ordering::SeqCst), 1);
    assert_eq!(api.calls.load(Ordering::SeqCst), 1);
    assert_eq!(api.tail_polls.load(Ordering::SeqCst), 0);
    assert_eq!(*registry.updates.lock().unwrap(), [true, false]);
    assert!(events
        .iter()
        .any(|event| matches!(event, SubagentEvent::Failed { .. })));
    assert!(!events.iter().any(|event| match event {
        SubagentEvent::Message { message, .. } => {
            message.to_string().contains("toolu-after-hop")
        }
        _ => false,
    }));
}

#[tokio::test]
async fn declined_fallback_sweeps_even_when_discard_indexes_are_not_tools_and_admitted_hop_continues(
) {
    let denied = run_gated_fallback_case(vec![99], &["primary-model"], Vec::new()).await;
    assert!(denied.3.is_cancelled());
    assert_eq!(denied.0.tail_polls.load(Ordering::SeqCst), 0);
    assert_eq!(*denied.2.updates.lock().unwrap(), [true, false]);

    let accepted = run_gated_fallback_case(
        vec![0],
        &["fallback-model"],
        vec![text_response("after the fallback tool", Some("end_turn"))],
    )
    .await;
    assert!(accepted.3.is_cancelled());
    assert_eq!(accepted.1.calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        accepted.0.models.lock().unwrap().as_slice(),
        ["primary-model", "fallback-model"]
    );
    assert!(accepted.0.tail_polls.load(Ordering::SeqCst) > 0);
    assert_eq!(
        *accepted.2.updates.lock().unwrap(),
        [true, false, true, false]
    );
    assert!(accepted
        .4
        .iter()
        .any(|event| matches!(event, SubagentEvent::Completed { .. })));
    assert!(!accepted.4.iter().any(|event| match event {
        SubagentEvent::Message { message, .. } =>
            message.to_string().contains("toolu-before-hop")
                && message.to_string().contains("ToolResult"),
        _ => false,
    }));
}

#[tokio::test]
async fn stream_error_drains_live_tool_use_and_emits_native_cancellation_result() {
    let (invoker, started_rx) = FallbackCancellationProbeInvoker::new();
    let registry = Arc::new(IdleFactCaptureRegistry::default());
    let mut ctx = loop_ctx(
        Arc::new(ToolThenStreamErrorApi),
        Some(invoker.clone() as Arc<dyn lingxi_core::host::ToolInvoker>),
        3,
    );
    ctx.task_registry = Some(registry.clone());

    let (event_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(1);
    drop(event_tx);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(32);
    let runner = tokio::spawn(run_subagent(ctx, event_rx, out_tx));
    let token = tokio::time::timeout(std::time::Duration::from_secs(2), started_rx)
        .await
        .expect("the tool must start at its completed-block boundary")
        .expect("tool invocation token");
    runner.await.expect("subagent runner task");

    assert!(token.is_cancelled());
    assert_eq!(*registry.updates.lock().unwrap(), [true, false]);
    let events = drain(out_rx).await;
    let messages = events
        .iter()
        .filter_map(|event| match event {
            SubagentEvent::Message { message, .. } => Some(message),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(messages.iter().any(|message| {
        message["content"][0]["id"] == "toolu-before-error"
            && message["content"][0]["type"] == "tool_use"
    }));
    assert!(messages.iter().any(|message| {
        message["content"][0]["tool_use_id"] == "toolu-before-error"
            && message["content"][0]["type"] == "tool_result"
            && message["content"][0]["is_error"].as_bool() == Some(true)
            && message["content"][0]["content"]
                == "The turn ended on an error, so this tool call was cancelled. If it had already started, some of its effects may have happened. Error: provider stream broke"
    }));
    assert!(events
        .iter()
        .any(|event| matches!(event, SubagentEvent::Failed { .. })));
}

#[tokio::test]
async fn streamed_tool_invocations_keep_query_history_current_row_and_prior_siblings_separate() {
    let response = llm_runtime::HistoryResponse {
        id: "context-carrier-response".into(),
        model: "mock".into(),
        content: vec![
            llm_runtime::ContentBlock::ToolCall {
                input_projection: None,
                id: "toolu-sibling-a".into(),
                name: "Read".into(),
                input: serde_json::json!({"file_path":"a.txt"}),
            },
            llm_runtime::ContentBlock::ToolCall {
                input_projection: None,
                id: "toolu-sibling-b".into(),
                name: "Read".into(),
                input: serde_json::json!({"file_path":"b.txt"}),
            },
        ],
        stop_reason: Some("tool_use".into()),
        stop_details: None,
        usage: llm_runtime::ExecutionUsage::default(),
        cost: None,
        provider_metadata: serde_json::Value::Null,
    };
    let events = llm_runtime::stream_accumulator::response_to_stream_events(response);
    let next_events = llm_runtime::stream_accumulator::response_to_stream_events(text_response(
        "finished after tools",
        Some("end_turn"),
    ));
    let api = StreamingMockApiClient::new(vec![events, next_events]);
    let invoker = Arc::new(CaptureLiveInvocationContexts::default());
    let mut ctx = loop_ctx(
        api.clone(),
        Some(invoker.clone() as Arc<dyn lingxi_core::host::ToolInvoker>),
        2,
    );
    let transcript_dir = tempfile::tempdir().unwrap();
    ctx.transcript_subdir = transcript_dir.path().to_path_buf();
    ctx.transcript_fs = Some(Arc::new(platform_posix::PosixFileSystem::new(
        transcript_dir.path().to_path_buf(),
    )) as Arc<dyn lingxi_core::host::FileSystem>);
    let agent_id = ctx.agent_id;
    let (event_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(1);
    drop(event_tx);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(32);
    run_subagent(ctx, event_rx, out_tx).await;
    let output_events = drain(out_rx).await;
    assert!(output_events
        .iter()
        .any(|event| matches!(event, SubagentEvent::Completed { .. })));

    let physical_calls = api.physical_calls();
    assert_eq!(physical_calls.len(), 2);
    let next_messages = &physical_calls[1].request.messages;
    let assistant_tool_ids = next_messages
        .iter()
        .flat_map(|message| match message {
            ConversationMessage::Assistant { content, .. } => content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::ToolUse { id, .. } => Some(id.as_str().to_owned()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        })
        .collect::<Vec<_>>();
    let result_tool_ids = next_messages
        .iter()
        .flat_map(|message| match message {
            ConversationMessage::User { content, .. } => content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::ToolResult { tool_use_id, .. } => {
                        Some(tool_use_id.as_str().to_owned())
                    }
                    _ => None,
                })
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        })
        .collect::<Vec<_>>();
    assert_eq!(assistant_tool_ids, ["toolu-sibling-a", "toolu-sibling-b"]);
    assert_eq!(result_tool_ids, ["toolu-sibling-a", "toolu-sibling-b"]);
    let je_order = next_messages
        .iter()
        .filter_map(|message| match message {
            ConversationMessage::User { content, .. } => {
                if let Some(tool_use_id) = content.iter().find_map(|block| match block {
                    ContentBlock::ToolResult { tool_use_id, .. } => {
                        Some(tool_use_id.as_str().to_owned())
                    }
                    _ => None,
                }) {
                    Some(format!("result:{tool_use_id}"))
                } else {
                    message
                        .text_content()
                        .strip_prefix("injected note ")
                        .map(|tool_use_id| format!("note:{tool_use_id}"))
                }
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        je_order,
        [
            "result:toolu-sibling-a",
            "note:toolu-sibling-a",
            "result:toolu-sibling-b",
            "note:toolu-sibling-b",
        ]
    );
    assert!(!next_messages.iter().any(|message| {
        message
            .text_content()
            .contains("executor assistant context")
    }));
    assert!(!next_messages
        .iter()
        .any(|message| message.text_content().contains("executor system context")));
    assert!(next_messages.iter().any(|message| {
        matches!(message, ConversationMessage::User { content, .. }
            if content.iter().any(|block| matches!(block, ContentBlock::Document { .. })))
    }));
    for expected in ["executor assistant context", "executor system context"] {
        assert!(
            output_events.iter().any(|event| matches!(event,
                SubagentEvent::Message { message, .. }
                    if message.to_string().contains(expected)
            )),
            "Native event journal retains {expected}"
        );
    }
    let transcript_path = transcript_dir
        .path()
        .join(format!("agent-{agent_id}.jsonl"));
    let transcript = std::fs::read_to_string(&transcript_path).unwrap();
    assert!(transcript.contains("executor assistant context"));
    assert!(transcript.contains("executor system context"));
    let emitted_raw_k_ids = output_events
        .iter()
        .filter_map(|event| match event {
            SubagentEvent::Message { message, .. } => {
                serde_json::from_value::<ConversationMessage>(message.clone()).ok()
            }
            _ => None,
        })
        .filter_map(|message| match &message {
            ConversationMessage::Assistant { content, .. }
                if content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::ToolUse { .. })) =>
            {
                Some(message.id().as_uuid().to_string())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let requested_raw_k_ids = next_messages
        .iter()
        .filter_map(|message| match message {
            ConversationMessage::Assistant { content, id, .. }
                if content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::ToolUse { .. })) =>
            {
                Some(id.as_uuid().to_string())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(requested_raw_k_ids, emitted_raw_k_ids);
    let emitted_tool_result_row_ids = output_events
        .iter()
        .filter_map(|event| match event {
            SubagentEvent::Message { message, .. } => {
                serde_json::from_value::<ConversationMessage>(message.clone()).ok()
            }
            _ => None,
        })
        .filter_map(|message| match &message {
            ConversationMessage::User { content, .. }
                if content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::ToolResult { .. })) =>
            {
                Some(message.id().as_uuid().to_string())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let requested_tool_result_row_ids = next_messages
        .iter()
        .filter_map(|message| match message {
            ConversationMessage::User { content, id, .. }
                if content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::ToolResult { .. })) =>
            {
                Some(id.as_uuid().to_string())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(requested_tool_result_row_ids, emitted_tool_result_row_ids);
    let last_assistant_tool_row = next_messages
        .iter()
        .rposition(|message| {
            matches!(message, ConversationMessage::Assistant { content, .. }
                if content.iter().any(|block| matches!(block, ContentBlock::ToolUse { .. })))
        })
        .expect("raw K assistant rows are present");
    let first_tool_result_row = next_messages
        .iter()
        .position(|message| {
            matches!(message, ConversationMessage::User { content, .. }
                if content.iter().any(|block| matches!(block, ContentBlock::ToolResult { .. })))
        })
        .expect("je tool-result rows are present");
    assert!(last_assistant_tool_row < first_tool_result_row);

    let contexts = invoker.contexts.lock().unwrap();
    assert_eq!(contexts.len(), 2);
    assert!(contexts.iter().all(|context| {
        !context.current_history.iter().any(|message| {
            matches!(message, ConversationMessage::Assistant { content, .. }
                if content.iter().any(|block| matches!(block, ContentBlock::ToolUse { .. })))
        })
    }));
    assert!(matches!(
        contexts[0].assistant_message.as_ref(),
        Some(ConversationMessage::Assistant { content, .. })
            if matches!(content.as_slice(), [ContentBlock::ToolUse { id, .. }] if id.as_str() == "toolu-sibling-a")
    ));
    assert!(contexts[0].same_turn_tool_uses.is_empty());
    assert!(matches!(
        contexts[1].assistant_message.as_ref(),
        Some(ConversationMessage::Assistant { content, .. })
            if matches!(content.as_slice(), [ContentBlock::ToolUse { id, .. }] if id.as_str() == "toolu-sibling-b")
    ));
    assert!(matches!(
        contexts[1].same_turn_tool_uses.as_slice(),
        [ContentBlock::ToolUse { id, .. }] if id.as_str() == "toolu-sibling-a"
    ));
}

#[tokio::test]
async fn accepted_fallback_prunes_discarded_k_without_clearing_je() {
    let discarded = ConversationMessage::Assistant { per_turn_effort: None,
        id: MessageId::new(),
        content: vec![ContentBlock::Text {
            text: "discarded non-tool K row".into(),
            citations: None,
        }],
        stop_reason: None,
    };
    let retained = ConversationMessage::Assistant { per_turn_effort: None,
        id: MessageId::new(),
        content: vec![ContentBlock::Text {
            text: "retained K row".into(),
            citations: None,
        }],
        stop_reason: None,
    };
    let result_a_id = MessageId::new();
    let note_a_id = MessageId::new();
    let result_b_id = MessageId::new();
    let je_a = ConversationMessage::User { api_message_override: None,
        id: result_a_id,
        content: vec![ContentBlock::ToolResult {
            content_projection: None,
            tool_use_id: ToolUseId::from("toolu-result-a".to_string()),
            content: "result A".into(),
            is_error: Some(false),
            provider_tool_use_id: None,
            content_blocks: None,
        }],
        is_meta: false,
        is_compact_summary: false,
        is_visible_in_transcript_only: false,
    };
    let note_a = ConversationMessage::user(note_a_id, "attachment/user note A".into());
    let je_b = ConversationMessage::User { api_message_override: None,
        id: result_b_id,
        content: vec![ContentBlock::ToolResult {
            content_projection: None,
            tool_use_id: ToolUseId::from("toolu-result-b".to_string()),
            content: "result B".into(),
            is_error: Some(false),
            provider_tool_use_id: None,
            content_blocks: None,
        }],
        is_meta: false,
        is_compact_summary: false,
        is_visible_in_transcript_only: false,
    };
    let mut assistant_rows = vec![
        crate::transcript::StagedAssistantRow::from_source(4, discarded.clone()),
        crate::transcript::StagedAssistantRow::from_source(5, retained.clone()),
    ];
    let mut ordered_rows = vec![
        discarded.clone(),
        retained.clone(),
        je_a.clone(),
        note_a.clone(),
        je_b.clone(),
    ];
    let je_rows = vec![je_a, note_a, je_b];
    let mut assistant_row_models = std::collections::HashMap::from([
        (discarded.id(), "primary-model".to_string()),
        (retained.id(), "primary-model".to_string()),
    ]);

    let removed = remove_discarded_assistant_rows(
        &mut assistant_rows,
        &mut ordered_rows,
        &[4],
        &mut assistant_row_models,
        None,
    );
    assert_eq!(removed.len(), 1);
    assert_eq!(removed[0].0.accepted.id(), discarded.id());

    let mut history = Vec::new();
    let mut transcript_written = 0;
    append_live_stream_rows(
        ordered_rows,
        &je_rows,
        &mut assistant_rows,
        None,
        &mut history,
        &mut transcript_written,
        None,
    )
    .await;

    assert_eq!(
        history
            .iter()
            .map(ConversationMessage::id)
            .collect::<Vec<_>>(),
        [retained.id(), result_a_id, note_a_id, result_b_id]
    );
    assert!(history
        .iter()
        .all(|message| message.text_content() != "discarded non-tool K row"));
}

static NEAR_LIMIT_WRAP_UP_FLAG_LOCK: RwLock<()> = RwLock::new(());

fn near_limit_wrap_up_write() -> std::sync::RwLockWriteGuard<'static, ()> {
    NEAR_LIMIT_WRAP_UP_FLAG_LOCK
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct NearLimitWrapUpFlagOn;

impl NearLimitWrapUpFlagOn {
    fn set() -> Self {
        ::telemetry::test_set_flag("tengu_vellum_anchor", true);
        Self
    }
}

impl Drop for NearLimitWrapUpFlagOn {
    fn drop(&mut self) {
        ::telemetry::test_clear_flag("tengu_vellum_anchor");
    }
}

struct NearLimitHintApiClient {
    response: llm_runtime::HistoryResponse,
    last_messages: Mutex<Vec<ConversationMessage>>,
    pending_hint: AtomicBool,
    consume_calls: AtomicUsize,
    near_limit_observations: AtomicUsize,
    checkpoint_requests: Mutex<Vec<crate::api::NearLimitCheckpointRequest>>,
}

impl NearLimitHintApiClient {
    fn new(response: llm_runtime::HistoryResponse, pending_hint: bool) -> Arc<Self> {
        Arc::new(Self {
            response,
            last_messages: Mutex::new(Vec::new()),
            pending_hint: AtomicBool::new(pending_hint),
            consume_calls: AtomicUsize::new(0),
            near_limit_observations: AtomicUsize::new(0),
            checkpoint_requests: Mutex::new(Vec::new()),
        })
    }

    fn last_messages(&self) -> Vec<ConversationMessage> {
        self.last_messages.lock().unwrap().clone()
    }

    fn checkpoint_requests(&self) -> Vec<crate::api::NearLimitCheckpointRequest> {
        self.checkpoint_requests.lock().unwrap().clone()
    }
}

#[async_trait]
impl crate::api::SubagentApiClient for NearLimitHintApiClient {
    fn consume_pending_near_limit_wrap_up_hint(&self) -> bool {
        self.consume_calls.fetch_add(1, Ordering::SeqCst);
        self.pending_hint.swap(false, Ordering::SeqCst)
    }

    fn dispatch_near_limit_checkpoint(&self, request: crate::api::NearLimitCheckpointRequest) {
        self.checkpoint_requests.lock().unwrap().push(request);
    }

    fn record_usage_limit_near_wrap_up(&self) {
        self.near_limit_observations.fetch_add(1, Ordering::SeqCst);
    }

    async fn stream(
        &self,
        request: crate::api::SubagentApiRequest,
    ) -> Result<
        futures::stream::BoxStream<
            'static,
            Result<llm_runtime::HistoryEvent, llm_runtime::LlmError>,
        >,
        llm_runtime::LlmError,
    > {
        let _model = request.model.as_str();
        let _system = request.system.as_deref();
        let messages = request.messages;
        let _tools = request.tools;
        let response: Result<llm_runtime::HistoryResponse, llm_runtime::LlmError> = async {
            *self.last_messages.lock().unwrap() = messages;
            Ok(self.response.clone())
        }
        .await;
        let events = llm_runtime::stream_accumulator::response_to_stream_events(response?);
        Ok(futures::StreamExt::boxed(futures::stream::iter(
            events.into_iter().map(Ok),
        )))
    }
}

/// Build the `message_start` envelope shared by the streamed-turn builders.
fn ev_message_start() -> llm_runtime::HistoryEvent {
    llm_runtime::HistoryEvent::MessageStart {
        response: Box::new(llm_runtime::HistoryResponse {
            id: "mock".into(),
            model: "mock".into(),
            content: vec![],
            stop_reason: None,
            stop_details: None,
            usage: llm_runtime::ExecutionUsage::default(),
            cost: None,
            provider_metadata: serde_json::Value::Null,
        }),
    }
}

/// One streamed turn carrying a single text block + `stop` reason.
fn streamed_text_turn(text: &str, stop: &str) -> Vec<llm_runtime::HistoryEvent> {
    use llm_runtime::{ContentBlock, HistoryContentDelta, HistoryEvent, HistoryMessageDelta};
    vec![
        ev_message_start(),
        HistoryEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlock::Text {
                text: String::new(),
                cache_control: None,
                citations: None,
            },
        },
        HistoryEvent::ContentBlockDelta {
            index: 0,
            delta: HistoryContentDelta::TextDelta { text: text.into() },
        },
        HistoryEvent::ContentBlockStop { index: 0 },
        HistoryEvent::MessageDelta {
            delta: HistoryMessageDelta {
                stop_reason: Some(stop.into()),
                stop_details: None,
            },
            usage: None,
        },
        HistoryEvent::MessageStop,
    ]
}

/// One streamed turn carrying a single `tool_call` block + `stop` reason.
fn streamed_tool_use_turn(name: &str, stop: &str) -> Vec<llm_runtime::HistoryEvent> {
    use llm_runtime::{ContentBlock, HistoryContentDelta, HistoryEvent, HistoryMessageDelta};
    vec![
        ev_message_start(),
        HistoryEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlock::ToolCall {
                input_projection: None,
                id: ToolUseId::new().to_string(),
                name: name.into(),
                input: serde_json::Value::Null,
            },
        },
        HistoryEvent::ContentBlockDelta {
            index: 0,
            delta: HistoryContentDelta::InputJsonDelta {
                partial_json: "{}".into(),
            },
        },
        HistoryEvent::ContentBlockStop { index: 0 },
        HistoryEvent::MessageDelta {
            delta: HistoryMessageDelta {
                stop_reason: Some(stop.into()),
                stop_details: None,
            },
            usage: None,
        },
        HistoryEvent::MessageStop,
    ]
}

fn streamed_tool_use_with_input_turn(
    id: &str,
    name: &str,
    input: &serde_json::Value,
    stop: &str,
) -> Vec<llm_runtime::HistoryEvent> {
    use llm_runtime::{ContentBlock, HistoryContentDelta, HistoryEvent, HistoryMessageDelta};
    vec![
        ev_message_start(),
        HistoryEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlock::Text {
                text: String::new(),
                cache_control: None,
                citations: None,
            },
        },
        HistoryEvent::ContentBlockDelta {
            index: 0,
            delta: HistoryContentDelta::TextDelta {
                text: "provider prelude".into(),
            },
        },
        HistoryEvent::ContentBlockStop { index: 0 },
        HistoryEvent::ContentBlockStart {
            index: 1,
            content_block: ContentBlock::ToolCall {
                input_projection: None,
                id: id.to_owned(),
                name: name.to_owned(),
                input: serde_json::Value::Null,
            },
        },
        HistoryEvent::ContentBlockDelta {
            index: 1,
            delta: HistoryContentDelta::InputJsonDelta {
                partial_json: serde_json::to_string(input).expect("tool input serializes"),
            },
        },
        HistoryEvent::ContentBlockStop { index: 1 },
        HistoryEvent::MessageDelta {
            delta: HistoryMessageDelta {
                stop_reason: Some(stop.to_owned()),
                stop_details: None,
            },
            usage: None,
        },
        HistoryEvent::MessageStop,
    ]
}

/// `ToolInvoker` that counts invocations and returns a canned value.
struct CountingInvoker {
    calls: AtomicUsize,
    policies: Mutex<Vec<lingxi_core::host::tool_invoker::ToolExecutionPolicy>>,
    parent_models: Mutex<Vec<Option<String>>>,
    parent_profiles: Mutex<Vec<Option<String>>>,
    cleanups: Mutex<Vec<(AgentId, Option<lingxi_core::types::SessionId>)>>,
    cleanup_failures: AtomicUsize,
}

#[derive(Default)]
struct InputCapturingInvoker {
    calls: Mutex<
        Vec<(
            String,
            serde_json::Value,
            Vec<ConversationMessage>,
            Option<ConversationMessage>,
            Vec<ContentBlock>,
        )>,
    >,
}

#[async_trait]
impl lingxi_core::host::ToolInvoker for InputCapturingInvoker {
    async fn invoke(
        &self,
        name: &str,
        input: serde_json::Value,
        ctx: lingxi_core::host::tool_invoker::SubagentInvocationContext,
    ) -> Result<serde_json::Value, lingxi_core::host::tool_invoker::ToolInvokerError> {
        self.calls.lock().unwrap().push((
            name.to_owned(),
            input,
            ctx.current_history,
            ctx.assistant_message,
            ctx.same_turn_tool_uses,
        ));
        Ok(serde_json::json!("tool-output"))
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
impl CountingInvoker {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            policies: Mutex::new(Vec::new()),
            parent_models: Mutex::new(Vec::new()),
            parent_profiles: Mutex::new(Vec::new()),
            cleanups: Mutex::new(Vec::new()),
            cleanup_failures: AtomicUsize::new(0),
        })
    }
    fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
    fn policies(&self) -> Vec<lingxi_core::host::tool_invoker::ToolExecutionPolicy> {
        self.policies.lock().unwrap().clone()
    }
    fn parent_routes(&self) -> Vec<(Option<String>, Option<String>)> {
        self.parent_models
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .zip(self.parent_profiles.lock().unwrap().iter().cloned())
            .collect()
    }
}
#[async_trait]
impl lingxi_core::host::ToolInvoker for CountingInvoker {
    async fn cleanup_computer_inputs(
        &self,
        agent_id: AgentId,
        origin_session_id: Option<lingxi_core::types::SessionId>,
    ) -> Result<(), lingxi_core::host::tool_invoker::ToolInvokerError> {
        self.cleanups
            .lock()
            .unwrap()
            .push((agent_id, origin_session_id));
        if self
            .cleanup_failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return Err(lingxi_core::host::tool_invoker::ToolInvokerError::Internal(
                "key release failed".into(),
            ));
        }
        Ok(())
    }
    async fn invoke(
        &self,
        _name: &str,
        _input: serde_json::Value,
        ctx: lingxi_core::host::tool_invoker::SubagentInvocationContext,
    ) -> Result<serde_json::Value, lingxi_core::host::tool_invoker::ToolInvokerError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.parent_models
            .lock()
            .unwrap()
            .push(ctx.parent_model.clone());
        self.parent_profiles
            .lock()
            .unwrap()
            .push(ctx.parent_model_profile.clone());
        self.policies
            .lock()
            .unwrap()
            .push(ctx.tool_execution_policy);
        Ok(serde_json::json!("tool-output"))
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

struct AbortInvoker;

#[async_trait]
impl lingxi_core::host::ToolInvoker for AbortInvoker {
    async fn invoke(
        &self,
        _name: &str,
        _input: serde_json::Value,
        _ctx: lingxi_core::host::tool_invoker::SubagentInvocationContext,
    ) -> Result<serde_json::Value, lingxi_core::host::tool_invoker::ToolInvokerError> {
        Err(lingxi_core::host::tool_invoker::ToolInvokerError::Abort(
            "Agent aborted: too many classifier denials in headless mode".into(),
        ))
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

struct SessionModeRecordingInvoker {
    captured: Mutex<Option<bool>>,
}

impl SessionModeRecordingInvoker {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            captured: Mutex::new(None),
        })
    }
}

#[async_trait]
impl lingxi_core::host::ToolInvoker for SessionModeRecordingInvoker {
    async fn invoke(
        &self,
        _name: &str,
        _input: serde_json::Value,
        ctx: lingxi_core::host::tool_invoker::SubagentInvocationContext,
    ) -> Result<serde_json::Value, lingxi_core::host::tool_invoker::ToolInvokerError> {
        *self.captured.lock().unwrap() = Some(ctx.is_non_interactive_session);
        Ok(serde_json::json!("tool-output"))
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// `BudgetEnforcerHandle` that reports the budget already exhausted when
/// `exceeded` is set. `check_and_charge` returns
/// `Err(BudgetError::Exceeded { current_nano_usd: 1_500_000_000 })`
/// (i.e. $1.50) when exhausted, else `Ok`. Mirrors the real enforcer's
/// charge-0 consult used by the per-turn budget gate.
struct MockBudget {
    exceeded: bool,
}
#[async_trait]
impl lingxi_core::host::budget::BudgetEnforcerHandle for MockBudget {
    async fn check_and_charge(&self, _: u64) -> Result<(), lingxi_core::host::budget::BudgetError> {
        if self.exceeded {
            Err(lingxi_core::host::budget::BudgetError::Exceeded {
                current_nano_usd: 1_500_000_000,
            })
        } else {
            Ok(())
        }
    }
    async fn snapshot_total_nano_usd(&self) -> u64 {
        1_500_000_000
    }

    fn max_session_nano_usd(&self) -> Option<u64> {
        Some(1_000_000_000)
    }
}

/// Build an `HistoryResponse` carrying a single text block.
fn text_response(text: &str, stop_reason: Option<&str>) -> llm_runtime::HistoryResponse {
    llm_runtime::HistoryResponse {
        id: "mock".into(),
        model: "mock".into(),
        content: vec![llm_runtime::ContentBlock::Text {
            text: text.into(),
            cache_control: None,
            citations: None,
        }],
        stop_reason: stop_reason.map(str::to_string),
        stop_details: None,
        usage: llm_runtime::ExecutionUsage::default(),
        cost: None,
        provider_metadata: serde_json::Value::Null,
    }
}

/// Build an `HistoryResponse` carrying one `tool_call` block (+ the given `stop_reason`).
fn tool_use_response(name: &str, stop_reason: Option<&str>) -> llm_runtime::HistoryResponse {
    llm_runtime::HistoryResponse {
        id: "mock".into(),
        model: "mock".into(),
        content: vec![llm_runtime::ContentBlock::ToolCall {
            input_projection: None,
            id: ToolUseId::new().to_string(),
            name: name.into(),
            input: serde_json::json!({}),
        }],
        stop_reason: stop_reason.map(str::to_string),
        stop_details: None,
        usage: llm_runtime::ExecutionUsage::default(),
        cost: None,
        provider_metadata: serde_json::Value::Null,
    }
}

fn tool_uses_response(names: &[&str]) -> llm_runtime::HistoryResponse {
    llm_runtime::HistoryResponse {
        id: "mock".into(),
        model: "mock".into(),
        content: names
            .iter()
            .map(|name| llm_runtime::ContentBlock::ToolCall {
                input_projection: None,
                id: ToolUseId::new().to_string(),
                name: (*name).into(),
                input: serde_json::json!({}),
            })
            .collect(),
        stop_reason: Some("tool_use".into()),
        stop_details: None,
        usage: llm_runtime::ExecutionUsage::default(),
        cost: None,
        provider_metadata: serde_json::Value::Null,
    }
}

/// Build an `HistoryResponse` carrying a text block AND a `tool_call` block
/// (+ the given `stop_reason`) — for the G2 backward-scan test (a turn that
/// surfaces text then a later turn that is tool-only).
fn text_and_tool_response(
    text: &str,
    name: &str,
    stop_reason: Option<&str>,
) -> llm_runtime::HistoryResponse {
    llm_runtime::HistoryResponse {
        id: "mock".into(),
        model: "mock".into(),
        content: vec![
            llm_runtime::ContentBlock::Text {
                text: text.into(),
                cache_control: None,
                citations: None,
            },
            llm_runtime::ContentBlock::ToolCall {
                input_projection: None,
                id: ToolUseId::new().to_string(),
                name: name.into(),
                input: serde_json::json!({}),
            },
        ],
        stop_reason: stop_reason.map(str::to_string),
        stop_details: None,
        usage: llm_runtime::ExecutionUsage::default(),
        cost: None,
        provider_metadata: serde_json::Value::Null,
    }
}

/// Build an `HistoryResponse` carrying one `tool_call` block AND a non-default
/// `usage` (for the G1 usage-threading test).
fn tool_use_response_with_usage(
    name: &str,
    stop_reason: Option<&str>,
    usage: llm_runtime::ExecutionUsage,
) -> llm_runtime::HistoryResponse {
    llm_runtime::HistoryResponse {
        usage,
        ..tool_use_response(name, stop_reason)
    }
}

/// `fresh_subagent_ctx` plus a scripted `api_client` (and optional invoker),
/// raising `max_turns` so multi-turn loops are reachable.
fn loop_ctx(
    api_client: Arc<dyn crate::api::SubagentApiClient>,
    tool_invoker: Option<Arc<dyn lingxi_core::host::ToolInvoker>>,
    max_turns: u32,
) -> SubagentContext {
    let mut ctx = fresh_subagent_ctx();
    ctx.agent_definition.max_turns = max_turns;
    ctx.api_client = Some(api_client);
    ctx.tool_invoker = tool_invoker;
    ctx
}

/// Build a `SubagentContext` with the minimum fields the runner reads.
fn fresh_subagent_ctx() -> SubagentContext {
    SubagentContext {
        task_registry: None,
        agent_id: AgentId::new(),
        parent_agent_id: None,
        agent_spawn_provenance: Default::default(),
        agent_name: None,
        team_name: None,
        agent_definition: AgentDefinition {
            omit_instructions: false,
            cache_ttl: None,
            agent_type: "test".into(),
            when_to_use: String::new(),
            tools: AgentToolPolicy::All {
                use_exact_tools: true,
            },
            max_turns: 1,
            model: AgentModel::Inherit,
            permission_mode: AgentPermissionMode::Bubble,
            source: AgentSource::BuiltIn,
            base_dir: "/tmp".into(),
            system_prompt: None,
            mcp_servers: vec![],
            frontmatter_hooks: vec![],
            icon: None,
            allowed_tools: vec![],
            worktree_requirement: None,
            disallowed_tools: vec![],
            skills: vec![],
            required_mcp_servers: vec![],
            background: false,
            isolation: None,
            memory: None,
            effort: None,
            initial_prompt: None,
            color: None,
            observer: None,
            offer_provider: None,
        },
        prompt_messages: vec![],
        fork_context_messages: None,
        allowed_tools: vec![],
        worktree_handle: None,
        cwd: None,
        is_async: false,
        persistent: false,
        can_show_permission_prompts: true,
        session_interactive: None,
        origin_session_id: None,
        mcp_clients: vec![],
        transcript_subdir: "/tmp".into(),
        transcript_fs: None,
        resumed_history: None,
        rendered_system_prompt: Some(Arc::from("")),
        mobile_runtime_environment_reminder: None,
        instruction_context: Default::default(),
        instruction_context_is_override: false,
        instruction_provider: None,
        mobile_runtime_workspace_reminder: None,
        content_replacement_state: None,
        agent_memory: None,
        display: AgentDisplay {
            color: AgentColor::Cyan,
            icon: None,
        },
        model_profile: None,
        model_resolution_context_provider: None,
        server_fallback_model_enforcement: None,
        api_client: None,
        tool_invoker: None,
        new_diagnostics_source: None,
        tool_schemas: vec![],
        schema: None,
        structured_output_parse_retries: 0,
        structured_output_mode: lingxi_core::host::subagent_spawn::StructuredOutputMode::Forced,
        budget: None,
        hook_executor: None,
        agent_spawn_token: None,
        stop_hook_scope: lingxi_core::host::subagent_spawn::SubagentStopScope::Session,
        subagent_stop_firer: None,
        strict_plugin_only_hooks: false,
        skill_loader: None,
        hook_session_id: lingxi_core::types::SessionId::nil(),
        hook_cwd: std::path::PathBuf::new(),
        depth: 0,
        observer: None,
        permission_mode_override: None,
        frozen_command_denies: Vec::new(),
        max_output_tokens_per_turn: None,
        max_input_bytes_per_turn: None,
        query_source_label: None,
        correlation_id: None,
        model_attempt: None,
        handback: None,
        handback_restore_start: None,
        refusal_fallback_chain: Vec::new(),
    }
}

#[tokio::test]
async fn agent_terminal_paths_await_cleanup_for_the_exact_agent_and_session() {
    for response in [
        Ok(text_response("done", Some("end_turn"))),
        Err(llm_runtime::LlmError::InvalidRequest {
            message: "failed".into(),
        }),
    ] {
        let invoker = CountingInvoker::new();
        let mut ctx = loop_ctx(
            MockSubagentApiClient::new(vec![response]),
            Some(invoker.clone()),
            1,
        );
        let session = lingxi_core::types::SessionId::new();
        ctx.origin_session_id = Some(session);
        let agent = ctx.agent_id;
        let (_event_tx, event_rx) = mpsc::channel(8);
        let (out_tx, out_rx) = mpsc::channel(64);
        run_subagent(ctx, event_rx, out_tx).await;
        assert!(invoker
            .cleanups
            .lock()
            .unwrap()
            .iter()
            .all(|identity| *identity == (agent, Some(session))));
        assert!(!invoker.cleanups.lock().unwrap().is_empty());
        assert!(drain(out_rx).await.iter().any(|event| matches!(
            event,
            SubagentEvent::Completed { .. } | SubagentEvent::Failed { .. }
        )));
    }
}

#[tokio::test]
async fn failed_input_cleanup_never_reports_success_or_parks_a_persistent_agent() {
    for persistent in [false, true] {
        let invoker = CountingInvoker::new();
        invoker.cleanup_failures.store(usize::MAX, Ordering::SeqCst);
        let mut ctx = loop_ctx(
            MockSubagentApiClient::new(vec![Ok(text_response("done", Some("end_turn")))]),
            Some(invoker.clone()),
            1,
        );
        ctx.persistent = persistent;
        let (_event_tx, event_rx) = mpsc::channel(8);
        let (out_tx, out_rx) = mpsc::channel(64);
        tokio::time::timeout(Duration::from_secs(3), run_subagent(ctx, event_rx, out_tx))
            .await
            .unwrap();
        let events = drain(out_rx).await;
        assert!(events.iter().any(|event| matches!(event, SubagentEvent::Failed { error, .. } if error.contains("desktop remains reserved"))));
        assert!(!events
            .iter()
            .any(|event| matches!(event, SubagentEvent::Completed { .. })));
        assert!(invoker.cleanups.lock().unwrap().len() >= 3);
    }
}

fn attach_failure_hook_executor(ctx: &mut SubagentContext) -> Arc<hooks::HookExecutorImpl> {
    let executor = Arc::new(hooks::HookExecutorImpl::new(
        Arc::new(tokio::sync::RwLock::new(hooks::HookRegistry::new())),
        Arc::new(test_harness::mocks::MockHttpTransport::new()),
        Arc::new(test_harness::mocks::MockRuntimeSpawner::default()),
    ));
    ctx.hook_executor = Some(executor.clone());
    ctx.hook_session_id = lingxi_core::types::SessionId::new();
    ctx.agent_definition.model = AgentModel::Explicit("failure-child-model".into());
    ctx.model_profile = Some("failure-child-profile".into());
    executor
}

#[tokio::test]
async fn startup_instruction_failure_publishes_child_route_and_retained_history() {
    struct StartupPromptRecorder(Mutex<Vec<hooks::PromptHookRequest>>);
    #[async_trait]
    impl hooks::HookPromptRunner for StartupPromptRecorder {
        async fn run(
            &self,
            request: hooks::PromptHookRequest,
        ) -> Result<String, hooks::PromptHookError> {
            self.0.lock().unwrap().push(request);
            Ok(r#"{"ok":true}"#.into())
        }
    }
    struct UnavailableInstructions;
    #[async_trait]
    impl lingxi_core::host::instructions::InstructionContextProvider for UnavailableInstructions {
        async fn load(
            &self,
            _: &std::path::Path,
            _: lingxi_core::host::instructions::InstructionScope,
        ) -> Result<lingxi_core::host::instructions::InstructionContext, String> {
            Err("initial instruction load failed".into())
        }
    }
    for resumed in [false, true] {
        let api = MockSubagentApiClient::new(Vec::new());
        let mut ctx = loop_ctx(api.clone(), None, 1);
        ctx.instruction_provider = Some(Arc::new(UnavailableInstructions));
        if resumed {
            ctx.resumed_history = Some(vec![ConversationMessage::user(
                MessageId::new(),
                "retained child evidence".into(),
            )]);
        }
        attach_failure_hook_executor(&mut ctx);
        let prompt_runner = Arc::new(StartupPromptRecorder(Mutex::new(Vec::new())));
        let executor = Arc::new(
            hooks::HookExecutorImpl::new(
                Arc::new(tokio::sync::RwLock::new(hooks::HookRegistry::new())),
                Arc::new(test_harness::mocks::MockHttpTransport::new()),
                Arc::new(test_harness::mocks::MockRuntimeSpawner::default()),
            )
            .with_prompt_runner(prompt_runner.clone()),
        );
        ctx.hook_executor = Some(executor.clone());
        let mut stop_hook = frontmatter_stop_hook("startup-retained-prompt-stop");
        stop_hook.executor = hooks::definition::HookExecutor::Prompt {
            prompt: "Inspect the retained child evidence. $ARGUMENTS".into(),
            model: None,
            continue_on_block: false,
        };
        ctx.agent_definition.frontmatter_hooks = vec![stop_hook];
        let transcript_dir = tempfile::tempdir().unwrap();
        ctx.transcript_subdir = transcript_dir.path().to_path_buf();
        let session_id = ctx.hook_session_id;
        let agent_id = ctx.agent_id;
        let expected_history = ctx.resumed_history.clone().unwrap_or_default();
        let (_event_tx, event_rx) = mpsc::channel(1);
        let (out_tx, mut out_rx) = mpsc::channel(1);
        let runner = tokio::spawn(run_subagent(ctx, event_rx, out_tx));
        assert!(
            matches!(out_rx.recv().await, Some(SubagentEvent::Failed { error, .. })
            if error == "initial instruction load failed")
        );
        let (route, transcript) = executor
            .take_agent_prompt_transcript(session_id, agent_id)
            .expect("the child's snapshot precedes its Failed event");
        assert_eq!(route.model, "failure-child-model");
        assert_eq!(
            route.model_profile.as_deref(),
            Some("failure-child-profile")
        );
        assert_eq!(transcript.messages, expected_history);
        assert_eq!(api.call_count(), 0);
        runner.await.unwrap();
        assert!(
            out_rx.recv().await.is_none(),
            "startup failure must emit no synthetic rows"
        );
        let requests = prompt_runner.0.lock().unwrap();
        assert_eq!(
            requests.len(),
            1,
            "the actual scoped prompt Stop fired once"
        );
        assert_eq!(
            requests[0].transcript.as_ref().unwrap().messages,
            expected_history
        );
        assert_eq!(
            requests[0].model_selection.as_ref().unwrap().model,
            "failure-child-model"
        );
        assert_eq!(
            requests[0]
                .model_selection
                .as_ref()
                .unwrap()
                .model_profile
                .as_deref(),
            Some("failure-child-profile")
        );
        assert_eq!(
            std::fs::read_dir(transcript_dir.path()).unwrap().count(),
            0,
            "instruction failure must not create transcript rows"
        );
    }
}

struct RouteChangingCleanupInvoker;

#[async_trait]
impl lingxi_core::host::ToolInvoker for RouteChangingCleanupInvoker {
    async fn invoke(
        &self,
        _: &str,
        _: serde_json::Value,
        _: lingxi_core::host::tool_invoker::SubagentInvocationContext,
    ) -> Result<serde_json::Value, lingxi_core::host::tool_invoker::ToolInvokerError> {
        Ok(serde_json::Value::Null)
    }
    async fn invoke_detailed(
        &self,
        _: &str,
        _: serde_json::Value,
        ctx: lingxi_core::host::tool_invoker::SubagentInvocationContext,
        _: Option<u64>,
    ) -> Result<
        lingxi_core::host::tool_invoker::ToolInvocationResult,
        lingxi_core::host::tool_invoker::ToolInvokerError,
    > {
        let mut context = tool_api::test_support::fresh_ctx();
        context.options.main_loop_model = ctx.parent_model.unwrap();
        context.options.model_profile = ctx.parent_model_profile;
        Ok(lingxi_core::host::tool_invoker::ToolInvocationResult {
            mcp_meta_projection: None,
            model_content_projection: None,
            data_projection: None,
            is_error: false,
            data: serde_json::Value::Null,
            model_content: None,
            new_messages: Vec::new(),
            context_modifier: Some(nested_route_modifier(
                "failure-live-model",
                Some("failure-live-profile"),
            )),
            mcp_meta: None,
            turn_end: None,
            context: lingxi_core::types::utf16_json::Utf16JsonProjection::plain(serde_json::json!(
                []
            )),
            context_state: Some(
                lingxi_core::host::tool_invoker::ToolInvocationContextState::new(Arc::new(context)),
            ),
        })
    }
    async fn cleanup_computer_inputs(
        &self,
        _: AgentId,
        _: Option<lingxi_core::types::SessionId>,
    ) -> Result<(), lingxi_core::host::tool_invoker::ToolInvokerError> {
        Err(lingxi_core::host::tool_invoker::ToolInvokerError::Internal(
            "key release failed".into(),
        ))
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[tokio::test]
async fn cleanup_failure_publishes_live_skill_route_before_failed_terminal() {
    let api = MockSubagentApiClient::new(vec![
        Ok(tool_use_response("Skill", Some("tool_use"))),
        Ok(text_response("done on the new route", Some("end_turn"))),
    ]);
    let mut ctx = loop_ctx(api.clone(), Some(Arc::new(RouteChangingCleanupInvoker)), 2);
    let executor = attach_failure_hook_executor(&mut ctx);
    ctx.model_resolution_context_provider = Some(Arc::new(|model: &str, profile: Option<&str>| {
        Ok(crate::model_resolution::ModelResolutionContext {
            route: crate::model_resolution::ModelRouteFacts {
                model: model.to_string(),
                profile: profile.map(str::to_string),
                ..Default::default()
            },
            ..Default::default()
        })
    }));
    let session_id = ctx.hook_session_id;
    let agent_id = ctx.agent_id;
    let (_event_tx, event_rx) = mpsc::channel(1);
    let (out_tx, mut out_rx) = mpsc::channel(1);
    let runner = tokio::spawn(run_subagent(ctx, event_rx, out_tx));
    loop {
        match out_rx.recv().await.expect("a terminal event") {
            SubagentEvent::Failed { error, .. } => {
                assert!(error.contains("desktop remains reserved"));
                break;
            }
            SubagentEvent::Completed { .. } => panic!("cleanup failure cannot complete"),
            _ => {}
        }
    }
    let (route, transcript) = executor
        .take_agent_prompt_transcript(session_id, agent_id)
        .expect("cleanup failure publishes before Failed");
    assert_eq!(route.model, "failure-live-model");
    assert_eq!(route.model_profile.as_deref(), Some("failure-live-profile"));
    assert!(transcript
        .messages
        .iter()
        .any(|message| message.text_content() == "done on the new route"));
    assert_eq!(api.call_count(), 2);
    runner.await.unwrap();
}

#[tokio::test]
async fn transient_input_cleanup_retries_before_completed_publication() {
    let invoker = CountingInvoker::new();
    invoker.cleanup_failures.store(2, Ordering::SeqCst);
    let ctx = loop_ctx(
        MockSubagentApiClient::new(vec![Ok(text_response("done", Some("end_turn")))]),
        Some(invoker.clone()),
        1,
    );
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (out_tx, out_rx) = mpsc::channel(64);
    run_subagent(ctx, event_rx, out_tx).await;
    let events = drain(out_rx).await;
    assert!(events
        .iter()
        .any(|event| matches!(event, SubagentEvent::Completed { .. })));
    assert!(!events
        .iter()
        .any(|event| matches!(event, SubagentEvent::Failed { .. })));
    assert!(invoker.cleanups.lock().unwrap().len() >= 3);
}

struct PendingAfterInputApi {
    first: Arc<MockSubagentApiClient>,
    calls: AtomicUsize,
}
#[async_trait]
impl crate::api::SubagentApiClient for PendingAfterInputApi {
    async fn stream(
        &self,
        request: crate::api::SubagentApiRequest,
    ) -> Result<
        futures::stream::BoxStream<
            'static,
            Result<llm_runtime::HistoryEvent, llm_runtime::LlmError>,
        >,
        llm_runtime::LlmError,
    > {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.first.stream(request).await
        } else {
            std::future::pending().await
        }
    }
}

#[tokio::test]
async fn cancellation_between_agent_tool_calls_awaits_input_cleanup() {
    let api = Arc::new(PendingAfterInputApi {
        first: MockSubagentApiClient::new(vec![Ok(tool_use_response(
            "computer",
            Some("tool_use"),
        ))]),
        calls: AtomicUsize::new(0),
    });
    let invoker = CountingInvoker::new();
    let mut ctx = loop_ctx(api.clone(), Some(invoker.clone()), 3);
    let session = lingxi_core::types::SessionId::new();
    ctx.origin_session_id = Some(session);
    let agent = ctx.agent_id;
    let (event_tx, event_rx) = mpsc::channel(8);
    let (out_tx, out_rx) = mpsc::channel(64);
    let run = tokio::spawn(run_subagent(ctx, event_rx, out_tx));
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while api.calls.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(invoker.calls.load(Ordering::SeqCst), 1);
    event_tx
        .send(lingxi_core::Event::UserInterrupt)
        .await
        .unwrap();
    run.await.unwrap();
    assert!(invoker
        .cleanups
        .lock()
        .unwrap()
        .iter()
        .all(|identity| *identity == (agent, Some(session))));
    assert!(!invoker.cleanups.lock().unwrap().is_empty());
    assert!(drain(out_rx)
        .await
        .iter()
        .any(|event| matches!(event, SubagentEvent::Killed { .. })));
}

#[tokio::test]
async fn persistent_agent_releases_inputs_before_completed_event_and_pool_abort() {
    let invoker = CountingInvoker::new();
    let api = MockSubagentApiClient::new(vec![
        Ok(tool_use_response("computer", Some("tool_use"))),
        Ok(text_response("done", Some("end_turn"))),
    ]);
    let mut ctx = loop_ctx(api, Some(invoker.clone()), 3);
    ctx.persistent = true;
    let session = lingxi_core::types::SessionId::new();
    ctx.origin_session_id = Some(session);
    let agent = ctx.agent_id;
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (out_tx, mut out_rx) = mpsc::channel(64);
    let run = tokio::spawn(run_subagent(ctx, event_rx, out_tx));
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if matches!(out_rx.recv().await, Some(SubagentEvent::Completed { .. })) {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(invoker.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        *invoker.cleanups.lock().unwrap(),
        vec![(agent, Some(session))]
    );
    assert!(
        !run.is_finished(),
        "persistent Agent still owns its idle event pump"
    );
    // The real pool deallocates/aborts upon seeing terminal publication.
    run.abort();
    assert!(run.await.unwrap_err().is_cancelled());
}

fn completed_with_server_fallback(
    mut response: llm_runtime::HistoryResponse,
    declared_model: &str,
    received_model: &str,
    profile: &str,
    reason: &str,
) -> llm_runtime::HistoryEvent {
    response.provider_metadata = serde_json::json!({
        "llm_client": {
            "server_fallback_events": [{
                "event": {
                    "fromModel": "primary-model",
                    "toModel": received_model,
                    "reason": reason,
                    "apiRefusalCategory": null,
                    "midStream": true,
                    "requestId": "server-fallback-request",
                    "discardedBlocks": [],
                    "retainedBlocks": [],
                    "retainedText": "",
                    "finalStopReason": null
                },
                "profile": profile,
                "lane": {
                    "forModel": "primary-model",
                    "model": declared_model,
                    "mode": "explicit"
                }
            }]
        }
    });
    llm_runtime::HistoryEvent::Completed {
        response: Box::new(response),
    }
}

fn active_model_enforcement(models: &[&str]) -> llm_runtime::model::allowlist::ModelEnforcement {
    llm_runtime::model::allowlist::ModelEnforcement::Active {
        allowlist: models.iter().map(|model| (*model).to_string()).collect(),
        overrides: Default::default(),
    }
}

fn test_refusal_api_text_snapshot(
    serving_model: &str,
) -> lingxi_core::host::refusal_api_text::RefusalApiTextSnapshot {
    lingxi_core::host::refusal_api_text::RefusalApiTextSnapshot {
        serving_model: Some(serving_model.to_owned()),
        model_eligible: true,
        display_label: Some("Test model".into()),
        model_family: None,
        fable_copy_suppressed: false,
        opus_5_5_exception: false,
        help_url: Some("https://example.test/refusal-help".into()),
        provider: lingxi_core::host::refusal_api_text::RefusalProviderKind::Other,
        interactive: true,
        feedback_eligible: false,
        brand: lingxi_core::host::refusal_api_text::RefusalBrandCopyOwned {
            api_error_prefix: "API Error".into(),
            product_name: "Test Host".into(),
            generic_model_label: "Model".into(),
        },
    }
}

fn refusal_fallback_event(
    source_model: &str,
    received_model: &str,
    profile: &str,
) -> llm_runtime::HistoryEvent {
    let mut event = completed_with_server_fallback(
        text_response("refused content must not be shown", Some("refusal")),
        source_model,
        received_model,
        profile,
        "refusal",
    );
    let llm_runtime::HistoryEvent::Completed { response } = &mut event else {
        unreachable!("completed fallback fixture")
    };
    response.provider_metadata["llm_client"]["server_fallback_events"][0]["event"]
        ["apiRefusalCategory"] = serde_json::Value::String("cyber".into());
    response.provider_metadata["llm_client"]["server_fallback_events"][0]["lane"]["forModel"] =
        serde_json::Value::String(source_model.into());
    response.provider_metadata["llm_client"]["server_fallback_events"][0]["event"]["fromModel"] =
        serde_json::Value::String(source_model.into());
    event
}

fn streamed_refusal_fallback_event(
    source_model: &str,
    received_model: &str,
    profile: &str,
) -> llm_runtime::HistoryEvent {
    serde_json::from_value(serde_json::json!({
        "type": "server_fallback",
        "event": {
            "fromModel": source_model,
            "toModel": received_model,
            "reason": "refusal",
            "apiRefusalCategory": "cyber",
            "midStream": true,
            "requestId": "stream-refusal-request",
            "discardedBlocks": [],
            "retainedBlocks": [],
            "retainedText": "",
            "finalStopReason": null
        },
        "profile": profile,
        "lane": {
            "forModel": source_model,
            "model": source_model,
            "mode": "explicit"
        }
    }))
    .expect("typed streamed refusal fallback event")
}

#[tokio::test]
async fn server_fallback_updates_only_the_child_route_for_its_next_request() {
    let api = StreamingMockApiClient::new(vec![
        vec![completed_with_server_fallback(
            llm_runtime::HistoryResponse {
                model: "untrusted-response-model".into(),
                ..text_and_tool_response("before fallback", "Read", Some("tool_use"))
            },
            "declared-model-not-in-policy",
            "server-selected-model",
            "fallback-provider",
            "refusal",
        )],
        llm_runtime::stream_accumulator::response_to_stream_events(text_response(
            "finished",
            Some("end_turn"),
        )),
    ]);
    let invoker = CountingInvoker::new();
    let mut ctx = loop_ctx(api.clone(), Some(invoker.clone()), 3);
    ctx.agent_definition.model = AgentModel::Explicit("primary-model".into());
    ctx.model_profile = Some("origin-provider".into());
    ctx.server_fallback_model_enforcement =
        Some(active_model_enforcement(&["server-selected-model"]));
    let caller_context = ctx.clone();
    let (_event_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(32);

    run_subagent(ctx, event_rx, out_tx).await;
    let events = drain(out_rx).await;

    assert_eq!(api.models(), ["primary-model", "server-selected-model"]);
    assert_eq!(
        api.profiles(),
        [
            Some("origin-provider".into()),
            Some("fallback-provider".into())
        ]
    );
    assert_eq!(invoker.call_count(), 1);
    assert_eq!(
        invoker.parent_routes(),
        [(Some("primary-model".into()), Some("origin-provider".into()))]
    );
    assert_eq!(one_completed(&events)["text"], "finished");
    // The runner's override is local; its spawn input stays the original route.
    assert_eq!(
        caller_context.model_profile.as_deref(),
        Some("origin-provider")
    );
}

#[tokio::test]
async fn response_without_server_fallback_keeps_child_model_and_profile() {
    let first_response = llm_runtime::HistoryResponse {
        model: "untrusted-response-model".into(),
        ..text_and_tool_response("before", "Read", Some("tool_use"))
    };
    let api = StreamingMockApiClient::new(vec![
        vec![llm_runtime::HistoryEvent::Completed {
            response: Box::new(first_response),
        }],
        llm_runtime::stream_accumulator::response_to_stream_events(text_response(
            "finished",
            Some("end_turn"),
        )),
    ]);
    let mut ctx = loop_ctx(api.clone(), Some(CountingInvoker::new()), 3);
    ctx.agent_definition.model = AgentModel::Explicit("primary-model".into());
    ctx.model_profile = Some("origin-provider".into());
    ctx.server_fallback_model_enforcement = Some(active_model_enforcement(&["other-model"]));
    let (_event_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(32);

    run_subagent(ctx, event_rx, out_tx).await;
    let events = drain(out_rx).await;

    assert_eq!(api.models(), ["primary-model", "primary-model"]);
    assert_eq!(
        api.profiles(),
        [
            Some("origin-provider".into()),
            Some("origin-provider".into())
        ]
    );
    assert_eq!(one_completed(&events)["text"], "finished");
}

#[tokio::test]
async fn other_server_fallback_reason_is_not_adopted_by_the_child_query() {
    let api = StreamingMockApiClient::new(vec![
        vec![completed_with_server_fallback(
            llm_runtime::HistoryResponse {
                model: "untrusted-response-model".into(),
                ..text_and_tool_response("before", "Read", Some("tool_use"))
            },
            "declared-model",
            "received-other-model",
            "other-provider",
            "other",
        )],
        llm_runtime::stream_accumulator::response_to_stream_events(text_response(
            "finished",
            Some("end_turn"),
        )),
    ]);
    let mut ctx = loop_ctx(api.clone(), Some(CountingInvoker::new()), 3);
    ctx.agent_definition.model = AgentModel::Explicit("primary-model".into());
    ctx.model_profile = Some("origin-provider".into());
    ctx.server_fallback_model_enforcement = Some(active_model_enforcement(&["other-model"]));
    let (_event_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(32);

    run_subagent(ctx, event_rx, out_tx).await;
    let events = drain(out_rx).await;

    assert_eq!(api.models(), ["primary-model", "primary-model"]);
    assert_eq!(
        api.profiles(),
        [
            Some("origin-provider".into()),
            Some("origin-provider".into())
        ]
    );
    assert_eq!(one_completed(&events)["text"], "finished");
}

#[tokio::test]
async fn managed_denial_discards_received_fallback_before_tools_or_reply() {
    let api = StreamingMockApiClient::new(vec![vec![completed_with_server_fallback(
        llm_runtime::HistoryResponse {
            model: "untrusted-response-model".into(),
            ..text_and_tool_response("forbidden target reply", "Read", Some("tool_use"))
        },
        "allowed-declared-model",
        "denied-received-model",
        "denied-provider",
        "sticky",
    )]]);
    let invoker = CountingInvoker::new();
    let mut ctx = loop_ctx(api.clone(), Some(invoker.clone()), 3);
    ctx.agent_definition.model = AgentModel::Explicit("primary-model".into());
    ctx.prompt_messages = vec![lingxi_core::types::ConversationMessage::user(
        MessageId::new(),
        "fallback test prompt".into(),
    )];
    ctx.server_fallback_model_enforcement =
        Some(active_model_enforcement(&["allowed-declared-model"]));
    let (_event_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(32);

    run_subagent(ctx, event_rx, out_tx).await;
    let events = drain(out_rx).await;

    assert_eq!(api.call_count(), 1);
    assert_eq!(invoker.call_count(), 0);
    let api_error_rows = events
        .iter()
        .filter_map(|event| match event {
            SubagentEvent::ServerFallbackApiErrorRow { row, .. } => Some(row),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        api_error_rows.len(),
        1,
        "decline emits one typed host API-error row"
    );
    assert_eq!(api_error_rows[0].error, "invalid_request");
    assert_eq!(api_error_rows[0].message.model, "<synthetic>");
    assert_eq!(
        api_error_rows[0].message.content,
        [ContentBlock::Text {
            text: lingxi_core::host::refusal_server_control::SERVER_FALLBACK_ALLOWLIST_ERROR.into(),
            citations: None,
        }]
    );
    let settled_history = events.iter().find_map(|event| match event {
        SubagentEvent::TranscriptSnapshot { messages, .. } => Some(messages),
        _ => None,
    });
    assert!(
        settled_history.is_some_and(|messages| messages
            .iter()
            .all(|message| { !message.text_content().contains("forbidden target reply") })),
        "failed server-fallback discard snapshot cannot reintroduce received text"
    );
    assert!(
        events.iter().all(|event| match event {
            SubagentEvent::Message { message, .. } => {
                !message.to_string().contains("forbidden target reply")
                    && !message.to_string().contains(
                        lingxi_core::host::refusal_server_control::SERVER_FALLBACK_ALLOWLIST_ERROR,
                    )
            }
            _ => true,
        }),
        "the typed API-error row is not duplicated as a legacy message event"
    );
    assert!(!events
        .iter()
        .any(|event| matches!(event, SubagentEvent::Completed { .. })));
    assert!(events
        .iter()
        .any(|event| matches!(event, SubagentEvent::Failed { .. })));
}

#[tokio::test]
async fn denied_refusal_fallback_terminates_without_entering_child_cascade() {
    let api = StreamingMockApiClient::new(vec![vec![completed_with_server_fallback(
        text_response("forbidden fallback refusal", Some("refusal")),
        "allowed-declared-model",
        "denied-received-model",
        "denied-provider",
        "refusal",
    )]]);
    let invoker = CountingInvoker::new();
    let mut ctx = loop_ctx(api.clone(), Some(invoker.clone()), 3);
    ctx.agent_definition.model = AgentModel::Explicit("primary-model".into());
    ctx.refusal_fallback_chain = vec!["local-fallback-model".into()];
    ctx.server_fallback_model_enforcement =
        Some(active_model_enforcement(&["allowed-declared-model"]));
    let (_event_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(32);

    run_subagent(ctx, event_rx, out_tx).await;
    let events = drain(out_rx).await;
    let public_events = events
        .iter()
        .filter(|event| !matches!(event, SubagentEvent::TranscriptSnapshot { .. }))
        .cloned()
        .collect::<Vec<_>>();
    let serialized = serde_json::to_string(&public_events).unwrap();

    assert_eq!(api.call_count(), 1);
    assert_eq!(api.models(), ["primary-model"]);
    assert_eq!(invoker.call_count(), 0);
    assert!(serialized.contains("declining the swap"));
    assert!(!serialized.contains("forbidden fallback refusal"));
    assert!(events
        .iter()
        .any(|event| matches!(event, SubagentEvent::Failed { .. })));
    assert!(
        events
            .iter()
            .all(|event| !matches!(event, SubagentEvent::ServerFallbackApiErrorRow { .. })),
        "an unprovided host text snapshot must not be replaced with guessed refusal copy"
    );
}

#[tokio::test]
async fn denied_refusal_fallback_uses_the_source_route_snapshot_and_preserves_native_row() {
    let source_model = "primary-route-model";
    let profile = "primary-profile";
    let api = StreamingMockApiClient::new(vec![vec![refusal_fallback_event(
        source_model,
        "denied-received-model",
        profile,
    )]]);
    let snapshot = test_refusal_api_text_snapshot(source_model);
    api.set_refusal_snapshot(snapshot.clone());
    let mut ctx = loop_ctx(api.clone(), Some(CountingInvoker::new()), 3);
    ctx.agent_definition.model = AgentModel::Explicit(source_model.into());
    ctx.model_profile = Some(profile.into());
    ctx.server_fallback_model_enforcement = Some(active_model_enforcement(&[source_model]));
    let (_event_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(32);

    run_subagent(ctx, event_rx, out_tx).await;
    let events = drain(out_rx).await;

    assert_eq!(
        api.refusal_snapshot_requests(),
        [(source_model.to_owned(), Some(profile.to_owned()))],
        "the request must resolve text facts against the pre-hop route, not the denied target"
    );
    let row = events
        .iter()
        .find_map(|event| match event {
            SubagentEvent::ServerFallbackApiErrorRow {
                row, message_index, ..
            } => Some((row, *message_index)),
            _ => None,
        })
        .expect("resolved refusal facts produce the native API-error row");
    assert_eq!(row.0.request_id.as_deref(), Some("server-fallback-request"));
    assert_eq!(row.0.error, "invalid_request");
    assert_eq!(row.0.message.stop_reason, "refusal");
    assert_eq!(
        row.0.message.stop_details,
        Some(serde_json::json!({
            "type": "refusal",
            "category": "cyber",
            "explanation": null,
            "fallback_credit_token": null,
            "fallback_has_prefill_claim": null,
            "recommended_model": null,
        }))
    );
    assert_eq!(
        row.0.message.content,
        [ContentBlock::Text {
            text: snapshot
                .format(Some("cyber"), Some("server-fallback-request"))
                .unwrap(),
            citations: None,
        }]
    );
    assert_eq!(
        row.1, 0,
        "the first visible row receives stable zero-based stream index zero"
    );
    let snapshot_messages = events.iter().find_map(|event| match event {
        SubagentEvent::TranscriptSnapshot { messages, .. } => Some(messages),
        _ => None,
    });
    assert!(
        snapshot_messages.is_some_and(|messages| messages.iter().any(|message| {
            message.id() == row.0.uuid
                && matches!(
                    message,
                    ConversationMessage::Assistant {
                        stop_reason: Some(reason),
                        ..
                    } if reason == "refusal"
                )
                && message
                    .text_content()
                    .contains("Test model's safeguards flagged this message")
        })),
        "the API-error row is appended to Agent history before terminal publication"
    );
    let row_event_pos = events
        .iter()
        .position(|event| matches!(event, SubagentEvent::ServerFallbackApiErrorRow { .. }))
        .unwrap();
    let failed_pos = events
        .iter()
        .position(|event| matches!(event, SubagentEvent::Failed { .. }))
        .unwrap();
    assert!(
        row_event_pos < failed_pos,
        "row publication precedes terminal failure"
    );
}

#[tokio::test]
async fn streamed_declined_refusal_uses_event_source_profile_and_emits_typed_row() {
    let source_model = "primary-stream-model";
    let source_profile = "resolved-stream-profile";
    let mut refusal_event =
        streamed_refusal_fallback_event(source_model, "denied-stream-model", source_profile);
    if let llm_runtime::HistoryEvent::ServerFallback { lane, .. } = &mut refusal_event {
        lane.mode =
            llm_runtime::services::sdk::providers::anthropic::fallback_request::LaneMode::Default;
    }
    let api = StreamingMockApiClient::new(vec![vec![ev_message_start(), refusal_event]]);
    let snapshot = test_refusal_api_text_snapshot(source_model);
    api.set_refusal_snapshot(snapshot.clone());
    let mut ctx = loop_ctx(api.clone(), Some(CountingInvoker::new()), 3);
    ctx.agent_definition.model = AgentModel::Explicit(source_model.into());
    ctx.model_profile = Some("request-profile-before-resolution".into());
    ctx.server_fallback_model_enforcement = Some(active_model_enforcement(&[source_model]));
    let (_event_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(32);

    run_subagent(ctx, event_rx, out_tx).await;
    let events = drain(out_rx).await;

    assert_eq!(
        api.refusal_snapshot_requests(),
        [(source_model.to_owned(), Some(source_profile.to_owned()))],
        "stream declines resolve against the source lane and resolved profile"
    );
    let row = events.iter().find_map(|event| match event {
        SubagentEvent::ServerFallbackApiErrorRow { row, .. } => Some(row),
        _ => None,
    });
    let row = row.expect("streamed refusal decline emits its typed API-error row");
    assert_eq!(row.request_id.as_deref(), Some("stream-refusal-request"));
    assert_eq!(row.message.stop_reason, "refusal");
    assert_eq!(
        row.message.content,
        [ContentBlock::Text {
            text: snapshot
                .format(Some("cyber"), Some("stream-refusal-request"))
                .unwrap(),
            citations: None,
        }]
    );
}

#[tokio::test]
async fn refusal_snapshot_after_an_accepted_hop_uses_the_current_source_route() {
    let api = StreamingMockApiClient::new(vec![
        vec![completed_with_server_fallback(
            text_and_tool_response("before accepted hop", "Read", Some("tool_use")),
            "primary-model",
            "server-selected-model",
            "fallback-provider",
            "refusal",
        )],
        vec![refusal_fallback_event(
            "server-selected-model",
            "denied-next-model",
            "fallback-provider",
        )],
    ]);
    let snapshot = test_refusal_api_text_snapshot("server-selected-model");
    api.set_refusal_snapshot(snapshot);
    let mut ctx = loop_ctx(api.clone(), Some(CountingInvoker::new()), 4);
    ctx.agent_definition.model = AgentModel::Explicit("primary-model".into());
    ctx.model_profile = Some("origin-provider".into());
    ctx.server_fallback_model_enforcement =
        Some(active_model_enforcement(&["server-selected-model"]));
    let (_event_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(32);

    run_subagent(ctx, event_rx, out_tx).await;
    let events = drain(out_rx).await;

    assert_eq!(api.models(), ["primary-model", "server-selected-model"]);
    assert_eq!(
        api.refusal_snapshot_requests(),
        [(
            "server-selected-model".into(),
            Some("fallback-provider".into())
        )],
        "a later refusal resolves facts for the currently served route, never its next target"
    );
    assert!(events.iter().any(|event| matches!(
        event,
        SubagentEvent::ServerFallbackApiErrorRow { row, .. }
            if row.message.model == "<synthetic>"
                && row.message.content.iter().any(|block| matches!(
                    block,
                    ContentBlock::Text { text, .. }
                        if text.contains("Test model's safeguards flagged this message")
                ))
    )));
}

#[tokio::test]
async fn same_request_refusal_decline_uses_the_last_accepted_hop_not_lane_source_or_candidate() {
    let original_model = "original-request-model";
    let accepted_model = "accepted-hop-model[1m]";
    let accepted_wire_model = "accepted-hop-model";
    let denied_model = "denied-hop-model";
    let profile = "one-physical-profile";
    let mut accepted_fallback =
        streamed_refusal_fallback_event(original_model, accepted_wire_model, profile);
    let llm_runtime::HistoryEvent::ServerFallback { lane, .. } = &mut accepted_fallback else {
        unreachable!("typed streamed fallback event")
    };
    lane.model = accepted_model.into();
    let api = StreamingMockApiClient::new(vec![vec![
        ev_message_start(),
        llm_runtime::HistoryEvent::ResponseObserved {
            model: accepted_wire_model.into(),
            response_id: Some("resp-candidate-one".into()),
        },
        accepted_fallback,
        llm_runtime::HistoryEvent::ResponseObserved {
            model: denied_model.into(),
            response_id: Some("resp-candidate-two".into()),
        },
        streamed_refusal_fallback_event(original_model, denied_model, profile),
    ]]);
    let snapshot = test_refusal_api_text_snapshot(accepted_model);
    api.set_refusal_snapshot(snapshot);
    let mut ctx = loop_ctx(api.clone(), Some(CountingInvoker::new()), 3);
    ctx.agent_definition.model = AgentModel::Explicit(original_model.into());
    ctx.model_profile = Some(profile.into());
    ctx.server_fallback_model_enforcement = Some(active_model_enforcement(&[
        accepted_model,
        accepted_wire_model,
    ]));
    let (_event_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(32);

    run_subagent(ctx, event_rx, out_tx).await;
    let events = drain(out_rx).await;

    assert_eq!(
        api.call_count(),
        1,
        "both hops belong to one physical request"
    );
    assert_eq!(
        api.refusal_snapshot_requests(),
        [(accepted_model.to_owned(), Some(profile.to_owned()))],
        "the declined hop uses the route advanced by the prior accepted hop, not lane.for_model, ResponseObserved candidate, or the denied target"
    );
    assert!(events.iter().any(|event| matches!(
        event,
        SubagentEvent::ServerFallbackApiErrorRow { row, .. }
            if row.message.content.iter().any(|block| matches!(
                block,
                ContentBlock::Text { text, .. }
                    if text.contains("Test model's safeguards flagged this message")
            ))
    )));
}

#[tokio::test]
async fn completed_response_fallback_metadata_advances_source_across_hops() {
    let original_model = "metadata-original-model";
    let accepted_model = "metadata-accepted-model[1m]";
    let accepted_wire_model = "metadata-accepted-model";
    let denied_model = "metadata-denied-model";
    let profile = "metadata-profile";
    let mut completed = completed_with_server_fallback(
        text_response("metadata refusal content", Some("refusal")),
        accepted_model,
        accepted_wire_model,
        profile,
        "refusal",
    );
    let llm_runtime::HistoryEvent::Completed { response } = &mut completed else {
        unreachable!("completed fallback fixture")
    };
    let events = response.provider_metadata["llm_client"]["server_fallback_events"]
        .as_array_mut()
        .expect("fallback event array");
    events[0]["event"]["fromModel"] = serde_json::Value::String(original_model.into());
    events[0]["lane"]["forModel"] = serde_json::Value::String(original_model.into());
    events.push(serde_json::json!({
        "event": {
            "fromModel": accepted_model,
            "toModel": denied_model,
            "reason": "refusal",
            "apiRefusalCategory": "cyber",
            "midStream": false,
            "requestId": "second-metadata-request",
            "discardedBlocks": [],
            "retainedBlocks": [],
            "retainedText": "",
            "finalStopReason": "refusal"
        },
        "profile": profile,
        "lane": {
            "forModel": original_model,
            "model": denied_model,
            "mode": "default"
        }
    }));

    let api = StreamingMockApiClient::new(vec![vec![completed]]);
    let snapshot = test_refusal_api_text_snapshot(accepted_model);
    api.set_refusal_snapshot(snapshot);
    let mut ctx = loop_ctx(api.clone(), Some(CountingInvoker::new()), 3);
    ctx.agent_definition.model = AgentModel::Explicit(original_model.into());
    ctx.model_profile = Some(profile.into());
    ctx.server_fallback_model_enforcement = Some(active_model_enforcement(&[
        accepted_model,
        accepted_wire_model,
    ]));
    let (_event_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(32);

    run_subagent(ctx, event_rx, out_tx).await;
    let events = drain(out_rx).await;

    assert_eq!(api.call_count(), 1);
    assert_eq!(
        api.refusal_snapshot_requests(),
        [(accepted_model.to_owned(), Some(profile.to_owned()))],
        "the completion-metadata path applies admitted preceding hops before resolving a later decline"
    );
    assert!(events.iter().any(|event| matches!(
        event,
        SubagentEvent::ServerFallbackApiErrorRow { row, .. }
            if row.request_id.as_deref() == Some("second-metadata-request")
    )));
}

struct OneShotDiagnostics {
    block: Mutex<Option<String>>,
    closed: Arc<AtomicBool>,
}

#[async_trait]
impl lingxi_core::host::NewDiagnosticsSource for OneShotDiagnostics {
    async fn take_new_diagnostics_block(&self) -> Option<String> {
        self.block.lock().unwrap().take()
    }

    async fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }
}

/// Records the exact agent cache-TTL override visible where the real request
/// would be built, including explicit `5m` and no override.
struct CacheTtlProbeClient {
    seen: Mutex<Option<Option<llm_runtime::AgentPromptCacheTtlOverride>>>,
}

#[async_trait]
impl crate::api::SubagentApiClient for CacheTtlProbeClient {
    async fn stream(
        &self,
        request: crate::api::SubagentApiRequest,
    ) -> Result<
        futures::stream::BoxStream<
            'static,
            Result<llm_runtime::HistoryEvent, llm_runtime::LlmError>,
        >,
        llm_runtime::LlmError,
    > {
        let _model = request.model.as_str();
        let _system = request.system.as_deref();
        let _messages = request.messages;
        let _tools = request.tools;
        let response: Result<llm_runtime::HistoryResponse, llm_runtime::LlmError> = async {
            *self.seen.lock().unwrap() = Some(llm_runtime::agent_prompt_cache_ttl_override());
            Ok(text_response("done", Some("end_turn")))
        }
        .await;
        let events = llm_runtime::stream_accumulator::response_to_stream_events(response?);
        Ok(futures::StreamExt::boxed(futures::stream::iter(
            events.into_iter().map(Ok),
        )))
    }
}

/// Drive ONE real subagent run and report what the request site saw.
///
/// One run per test: three in a single `#[tokio::test]` overflowed the stack —
/// `run_subagent`'s future is large, and they compose.
async fn cache_ttl_seen_at_request_site(
    ttl: Option<crate::definition::AgentCacheTtl>,
) -> Option<llm_runtime::AgentPromptCacheTtlOverride> {
    let probe = Arc::new(CacheTtlProbeClient {
        seen: Mutex::new(None),
    });
    let mut ctx = loop_ctx(probe.clone(), None, 2);
    ctx.agent_definition.cache_ttl = ttl;
    let (event_tx, event_rx) = mpsc::channel(1);
    drop(event_tx);
    let (out_tx, out_rx) = mpsc::channel(32);

    run_subagent(ctx, event_rx, out_tx).await;
    let _ = drain(out_rx).await;

    let seen = *probe.seen.lock().unwrap();
    seen.expect("the runner must have made a round-trip, or this test proves nothing")
}

/// `1h` survives the real runner-to-request path as a typed override.
#[tokio::test]
async fn agent_cache_ttl_one_hour_reaches_the_request_site() {
    assert_eq!(
        cache_ttl_seen_at_request_site(Some(crate::definition::AgentCacheTtl::OneHour)).await,
        Some(llm_runtime::AgentPromptCacheTtlOverride::OneHour)
    );
}

/// Explicit `5m` stays distinct from an absent override at request construction.
#[tokio::test]
async fn agent_cache_ttl_five_minutes_reaches_the_request_site() {
    assert_eq!(
        cache_ttl_seen_at_request_site(Some(crate::definition::AgentCacheTtl::FiveMinutes)).await,
        Some(llm_runtime::AgentPromptCacheTtlOverride::FiveMinutes)
    );
}

/// Absence stays available for environment/settings resolution.
#[tokio::test]
async fn no_agent_cache_ttl_reaches_the_request_site_as_absent() {
    assert_eq!(cache_ttl_seen_at_request_site(None).await, None);
}

#[tokio::test]
async fn write_result_injects_independent_lsp_diagnostics_before_next_round_trip() {
    let api = MockSubagentApiClient::new(vec![
        Ok(tool_use_response("Write", Some("tool_use"))),
        Ok(text_response("fixed", Some("end_turn"))),
    ]);
    let mut ctx = loop_ctx(api.clone(), Some(CountingInvoker::new()), 3);
    let diagnostics_closed = Arc::new(AtomicBool::new(false));
    ctx.new_diagnostics_source = Some(Arc::new(OneShotDiagnostics {
        block: Mutex::new(Some(
            "<new-diagnostics>\napp/app.jsx: [Line 2:1] unknownName\n</new-diagnostics>".into(),
        )),
        closed: diagnostics_closed.clone(),
    }));
    let (event_tx, event_rx) = mpsc::channel(1);
    drop(event_tx);
    let (out_tx, out_rx) = mpsc::channel(32);

    run_subagent(ctx, event_rx, out_tx).await;
    let _ = drain(out_rx).await;

    assert!(api.last_messages().iter().any(|message| {
        matches!(message, ConversationMessage::User { content, is_meta: true, .. }
        if content.iter().any(|block| matches!(block,
            ContentBlock::Text { text, .. }
                if text.contains("<system-reminder>\n<new-diagnostics>")
                    && text.contains("unknownName")
        )))
    }));
    assert!(
        diagnostics_closed.load(Ordering::SeqCst),
        "the agent terminal path must release its workspace diagnostics source"
    );
}

/// Drain the `SubagentEvent` receiver into a `Vec`.
async fn drain(mut rx: mpsc::Receiver<SubagentEvent>) -> Vec<SubagentEvent> {
    let mut out = Vec::new();
    while let Some(ev) = rx.recv().await {
        out.push(ev);
    }
    out
}

#[tokio::test]
async fn subagent_near_limit_wrap_up_is_exact_and_dispatches_once_through_wrapper() {
    let _flag_lock = near_limit_wrap_up_write();
    let _flag = NearLimitWrapUpFlagOn::set();

    let tempdir = tempfile::tempdir().expect("tempdir");
    let api = NearLimitHintApiClient::new(text_response("done", Some("end_turn")), true);
    let wrapped = Arc::new(crate::api::WorkflowWatchdogApiClient::new(
        api.clone(),
        lingxi_core::host::WorkflowQueryWatchdog {
            stall_timeout_ms: 1_000,
            max_retries: 0,
            retry_response_body: false,
        },
        Vec::new(),
    ));
    let mut ctx = loop_ctx(wrapped, None, 1);
    ctx.depth = 1;
    ctx.session_interactive = Some(true);
    ctx.is_async = true;
    ctx.hook_session_id = lingxi_core::types::SessionId::new();
    ctx.hook_cwd = tempdir.path().to_path_buf();

    let (event_tx, event_rx) = mpsc::channel(1);
    drop(event_tx);
    let (out_tx, out_rx) = mpsc::channel(16);
    run_subagent(ctx.clone(), event_rx, out_tx).await;
    let events = drain(out_rx).await;

    let messages = api.last_messages();
    let note_count = messages
        .iter()
        .filter(|message| {
            matches!(
                message,
                ConversationMessage::User { content, is_meta: true, .. }
                    if matches!(content.as_slice(), [ContentBlock::Text { text, .. }]
                        if text == NEAR_LIMIT_WRAP_UP_NOTE)
            )
        })
        .count();
    assert_eq!(note_count, 1, "the model-visible note is one-shot");
    let last = messages.last().expect("near-limit note");
    assert!(matches!(
        last,
        ConversationMessage::User { content, is_meta: true, .. }
            if matches!(content.as_slice(), [ContentBlock::Text { text, .. }]
                if text == NEAR_LIMIT_WRAP_UP_NOTE)
    ));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, SubagentEvent::Message { message, .. }
                if serde_json::from_value::<ConversationMessage>(message.clone()).is_ok_and(|message|
                    matches!(message,
                        ConversationMessage::User { content, is_meta: true, .. }
                            if matches!(content.as_slice(), [ContentBlock::Text { text, .. }]
                                if text == NEAR_LIMIT_WRAP_UP_NOTE)))))
            .count(),
        1,
        "the note is yielded exactly once"
    );

    assert!(api.consume_calls.load(Ordering::SeqCst) >= 1);
    assert_eq!(api.near_limit_observations.load(Ordering::SeqCst), 1);
    let requests = api.checkpoint_requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0],
        crate::api::NearLimitCheckpointRequest {
            session_id: ctx.hook_session_id,
            cwd: tempdir.path().to_path_buf(),
            non_interactive: false,
        },
        "an async child still belongs to its interactive parent session"
    );
}

#[tokio::test]
async fn disabled_near_limit_flag_consumes_and_drops_the_pending_hint() {
    let _flag_lock = near_limit_wrap_up_write();
    ::telemetry::test_clear_flag("tengu_vellum_anchor");
    let api = NearLimitHintApiClient::new(text_response("done", Some("end_turn")), true);
    let mut ctx = loop_ctx(api.clone(), None, 1);
    ctx.depth = 1;

    let (event_tx, event_rx) = mpsc::channel(1);
    drop(event_tx);
    let (out_tx, out_rx) = mpsc::channel(16);
    run_subagent(ctx, event_rx, out_tx).await;
    let _ = drain(out_rx).await;

    assert!(api.consume_calls.load(Ordering::SeqCst) >= 1);
    assert_eq!(api.near_limit_observations.load(Ordering::SeqCst), 0);
    assert!(!api.pending_hint.load(Ordering::SeqCst));
    assert!(api.checkpoint_requests().is_empty());
    assert!(!api.last_messages().iter().any(|message| matches!(
        message,
        ConversationMessage::User { content, .. }
            if content.iter().any(|block| matches!(block,
                ContentBlock::Text { text, .. } if text == NEAR_LIMIT_WRAP_UP_NOTE
            ))
    )));
}

#[tokio::test]
async fn main_depth_does_not_consume_the_subagent_near_limit_hint() {
    let _flag_lock = near_limit_wrap_up_write();
    let _flag = NearLimitWrapUpFlagOn::set();
    let api = NearLimitHintApiClient::new(text_response("done", Some("end_turn")), true);
    let ctx = loop_ctx(api.clone(), None, 1);

    let (event_tx, event_rx) = mpsc::channel(1);
    drop(event_tx);
    let (out_tx, out_rx) = mpsc::channel(16);
    run_subagent(ctx, event_rx, out_tx).await;
    let _ = drain(out_rx).await;

    assert_eq!(api.consume_calls.load(Ordering::SeqCst), 0);
    assert_eq!(api.near_limit_observations.load(Ordering::SeqCst), 0);
    assert!(api.pending_hint.load(Ordering::SeqCst));
    assert!(api.checkpoint_requests().is_empty());
}

#[tokio::test]
async fn near_limit_noninteractive_subagent_gets_hint_but_does_not_dispatch_checkpoint() {
    let _flag_lock = near_limit_wrap_up_write();
    let _flag = NearLimitWrapUpFlagOn::set();
    let api = NearLimitHintApiClient::new(text_response("done", Some("end_turn")), true);
    let mut ctx = loop_ctx(api.clone(), None, 1);
    ctx.depth = 1;
    ctx.session_interactive = Some(false);

    let (event_tx, event_rx) = mpsc::channel(1);
    drop(event_tx);
    let (out_tx, out_rx) = mpsc::channel(16);
    run_subagent(ctx, event_rx, out_tx).await;
    let _ = drain(out_rx).await;

    assert!(api.last_messages().iter().any(|message| matches!(
        message,
        ConversationMessage::User { content, is_meta: true, .. }
            if content.iter().any(|block| matches!(block,
                ContentBlock::Text { text, .. } if text == NEAR_LIMIT_WRAP_UP_NOTE
            ))
    )));
    assert_eq!(api.near_limit_observations.load(Ordering::SeqCst), 1);
    assert!(api.checkpoint_requests().is_empty());
}

#[tokio::test]
async fn workflow_watchdog_times_out_stream_open() {
    let policy = lingxi_core::host::WorkflowQueryWatchdog {
        stall_timeout_ms: 10,
        max_retries: 0,
        retry_response_body: false,
    };
    let result = await_workflow_query_phase::<(), _>(
        async {
            std::future::pending::<()>().await;
            Ok(())
        },
        Some(policy),
        "opening the response stream",
    )
    .await;
    assert!(matches!(result, Err(ref error) if is_workflow_watchdog_timeout(error)));
}

#[tokio::test]
async fn workflow_watchdog_times_out_before_first_event() {
    use futures::StreamExt;
    let stream =
        futures::stream::pending::<Result<llm_runtime::HistoryEvent, llm_runtime::LlmError>>()
            .boxed();
    let mut watched = with_workflow_stream_watchdog(
        stream,
        Some(lingxi_core::host::WorkflowQueryWatchdog {
            stall_timeout_ms: 10,
            max_retries: 0,
            retry_response_body: false,
        }),
    );
    let event = watched.next().await.expect("watchdog error event");
    assert!(matches!(event, Err(ref error) if is_workflow_watchdog_timeout(error)));
    assert!(
        watched.next().await.is_none(),
        "timeout terminates the stream"
    );
}

#[tokio::test]
async fn workflow_watchdog_resets_between_events_and_has_no_total_deadline() {
    use futures::StreamExt;
    let stream = futures::stream::unfold(0_u8, |index| async move {
        if index == 4 {
            return None;
        }
        tokio::time::sleep(std::time::Duration::from_millis(8)).await;
        Some((Ok(ev_message_start()), index + 1))
    })
    .boxed();
    let watched = with_workflow_stream_watchdog(
        stream,
        Some(lingxi_core::host::WorkflowQueryWatchdog {
            stall_timeout_ms: 20,
            max_retries: 0,
            retry_response_body: false,
        }),
    );
    let events = watched.collect::<Vec<_>>().await;
    assert_eq!(events.len(), 4);
    assert!(events.iter().all(Result::is_ok));
    // Four 8ms waits exceed the 20ms idle threshold in aggregate. Success
    // proves the deadline resets after every event instead of wrapping the
    // whole accumulator/model round-trip.
}

struct SlowInvoker {
    delay: std::time::Duration,
}

#[async_trait]
impl lingxi_core::host::ToolInvoker for SlowInvoker {
    async fn invoke(
        &self,
        _name: &str,
        _input: serde_json::Value,
        _ctx: lingxi_core::host::tool_invoker::SubagentInvocationContext,
    ) -> Result<serde_json::Value, lingxi_core::host::tool_invoker::ToolInvokerError> {
        tokio::time::sleep(self.delay).await;
        Ok(serde_json::json!("slow-tool-finished"))
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[tokio::test]
async fn workflow_watchdog_does_not_cover_tool_execution() {
    let inner = StreamingMockApiClient::new(vec![
        streamed_tool_use_turn("Read", "tool_use"),
        streamed_text_turn("done", "end_turn"),
    ]);
    let api: Arc<dyn crate::api::SubagentApiClient> =
        Arc::new(crate::api::WorkflowWatchdogApiClient::new(
            inner,
            lingxi_core::host::WorkflowQueryWatchdog {
                stall_timeout_ms: 10,
                max_retries: 0,
                retry_response_body: false,
            },
            Vec::new(),
        ));
    let invoker: Arc<dyn lingxi_core::host::ToolInvoker> = Arc::new(SlowInvoker {
        delay: std::time::Duration::from_millis(35),
    });
    let ctx = loop_ctx(api, Some(invoker), 3);
    let (event_tx, event_rx) = mpsc::channel(1);
    drop(event_tx);
    let (out_tx, out_rx) = mpsc::channel(32);
    run_subagent(ctx, event_rx, out_tx).await;
    let events = drain(out_rx).await;
    assert!(events.iter().any(
        |event| matches!(event, SubagentEvent::Completed { result, .. } if result["text"] == "done")
    ));
    assert!(!events
        .iter()
        .any(|event| matches!(event, SubagentEvent::Failed { .. })));
}

struct TimeoutAfterToolApi {
    calls: AtomicUsize,
}

#[async_trait]
impl crate::api::SubagentApiClient for TimeoutAfterToolApi {
    async fn stream(
        &self,
        request: crate::api::SubagentApiRequest,
    ) -> Result<
        futures::stream::BoxStream<
            'static,
            Result<llm_runtime::HistoryEvent, llm_runtime::LlmError>,
        >,
        llm_runtime::LlmError,
    > {
        let _model = request.model.as_str();
        let _system = request.system.as_deref();
        let _messages = request.messages;
        let _tools = request.tools;
        let response: Result<llm_runtime::HistoryResponse, llm_runtime::LlmError> = async {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                Ok(text_and_tool_response("partial", "Read", Some("tool_use")))
            } else {
                std::future::pending().await
            }
        }
        .await;
        let events = llm_runtime::stream_accumulator::response_to_stream_events(response?);
        Ok(futures::StreamExt::boxed(futures::stream::iter(
            events.into_iter().map(Ok),
        )))
    }
}

#[derive(Default)]
struct RetryObserver {
    attempts: Mutex<Vec<u32>>,
    reasons: Mutex<Vec<String>>,
}

#[async_trait]
impl lingxi_core::host::SubagentSpawnObserver for RetryObserver {
    async fn on_event(&self, event: lingxi_core::host::SubagentObservation) {
        if let lingxi_core::host::SubagentObservation::Retry {
            attempt, reason, ..
        } = event
        {
            self.attempts.lock().unwrap().push(attempt);
            self.reasons.lock().unwrap().push(reason);
        }
    }
}

#[tokio::test]
async fn workflow_watchdog_retries_five_times_then_fails_without_partial_salvage() {
    let inner = Arc::new(TimeoutAfterToolApi {
        calls: AtomicUsize::new(0),
    });
    let observer = Arc::new(RetryObserver::default());
    let observer_dyn: Arc<dyn lingxi_core::host::SubagentSpawnObserver> = observer.clone();
    let api: Arc<dyn crate::api::SubagentApiClient> =
        Arc::new(crate::api::WorkflowWatchdogApiClient::new(
            inner.clone(),
            lingxi_core::host::WorkflowQueryWatchdog {
                stall_timeout_ms: 10,
                max_retries: 5,
                retry_response_body: false,
            },
            vec![observer_dyn],
        ));
    let ctx = loop_ctx(api, Some(CountingInvoker::new()), 3);
    let (event_tx, event_rx) = mpsc::channel(1);
    drop(event_tx);
    let (out_tx, out_rx) = mpsc::channel(32);
    run_subagent(ctx, event_rx, out_tx).await;
    let events = drain(out_rx).await;

    assert_eq!(inner.calls.load(Ordering::SeqCst), 7);
    assert_eq!(*observer.attempts.lock().unwrap(), vec![2, 3, 4, 5, 6]);
    assert!(observer
        .reasons
        .lock()
        .unwrap()
        .iter()
        .all(|reason| reason.contains("opening the response stream")));
    assert!(events.iter().any(|event| matches!(
        event,
        SubagentEvent::Failed { error, .. }
            if error.contains("workflow query timeout")
    )));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, SubagentEvent::Completed { .. })),
        "watchdog exhaustion must not salvage prior partial text as success"
    );
}

/// Build a minimal assistant message with a single text block.
fn assistant_text(text: &str) -> ConversationMessage {
    ConversationMessage::Assistant { per_turn_effort: None,
        id: MessageId::new(),
        content: vec![ContentBlock::Text {
            text: text.to_string(),
            citations: None,
        }],
        stop_reason: Some("end_turn".into()),
    }
}

#[tokio::test]
async fn run_subagent_emits_streamed_message_before_completed() {
    let api = StreamingMockApiClient::new(vec![streamed_text_turn("hello world", "end_turn")]);
    let ctx = loop_ctx(api.clone(), None, 1);
    let agent_id = ctx.agent_id;
    let (event_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(8);
    drop(event_tx);
    let date_before = chrono::Local::now().format("%Y-%m-%d").to_string();
    run_subagent(ctx, event_rx, out_tx).await;
    let date_after = chrono::Local::now().format("%Y-%m-%d").to_string();
    let events = drain(out_rx).await;
    assert_eq!(api.call_count(), 1);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, SubagentEvent::Completed { .. }))
            .count(),
        1
    );
    assert_eq!(one_completed(&events)["stop_reason"], "end_turn");
    assert!(!events
        .iter()
        .any(|event| matches!(event, SubagentEvent::Failed { .. })));
    let messages: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            SubagentEvent::Message {
                agent_id, message, ..
            } => Some((*agent_id, message)),
            _ => None,
        })
        .collect();
    assert_eq!(
        messages.len(),
        2,
        "one date and one assistant are published"
    );
    let startup: Vec<_> = messages
        .iter()
        .filter(|(_, message)| message["type"] == "attachment")
        .collect();
    assert_eq!(startup.len(), 1, "one startup date attachment is published");
    assert_eq!(startup[0].0, agent_id);
    let _: MessageId = serde_json::from_value(startup[0].1["uuid"].clone()).unwrap();
    let date = startup[0].1["attachment"]["date"].as_str().unwrap();
    assert!(date == date_before || date == date_after);
    assert_eq!(
        startup[0].1["attachment"],
        serde_json::json!({"type":"date","date":date})
    );
    let assistants: Vec<_> = messages
        .iter()
        .filter(|(_, message)| message["role"] == "assistant")
        .collect();
    assert_eq!(assistants.len(), 1, "one completed assistant is published");
    assert_eq!(assistants[0].0, agent_id);
    let message: ConversationMessage = serde_json::from_value(assistants[0].1.clone()).unwrap();
    let ConversationMessage::Assistant {
        content,
        stop_reason,
        ..
    } = message
    else {
        panic!("streamed completion must publish an assistant message");
    };
    assert_eq!(
        content,
        vec![ContentBlock::Text {
            text: "hello world".into(),
            citations: None
        }]
    );
    // A content_block_stop row is yielded before message_delta supplies the
    // terminal stop reason. The completed response carries end_turn separately.
    assert_eq!(stop_reason, None);
    let message_position = events
        .iter()
        .position(|event| {
            matches!(event, SubagentEvent::Message { message, .. }
            if message["role"] == "assistant")
        })
        .unwrap();
    let startup_position = events
        .iter()
        .position(|event| {
            matches!(event, SubagentEvent::Message { message, .. }
            if message["attachment"]["type"] == "date")
        })
        .unwrap();
    let completed_position = events
        .iter()
        .position(|event| matches!(event, SubagentEvent::Completed { .. }))
        .unwrap();
    assert!(startup_position < message_position);
    assert!(message_position < completed_position);
}

#[tokio::test]
async fn run_subagent_emits_killed_on_user_exit_terminal() {
    let mut ctx = loop_ctx(Arc::new(PendingApiClient), None, 1);
    ctx.prompt_messages = vec![lingxi_core::types::ConversationMessage::user(
        MessageId::new(),
        "cancelled prompt".into(),
    )];
    let agent_id = ctx.agent_id;

    let (event_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(8);

    let handle = tokio::spawn(run_subagent(ctx, event_rx, out_tx));

    // Drive directly to terminal via UserExit. The fast-path in the
    // runner short-circuits to Killed on this input event.
    event_tx.send(lingxi_core::Event::UserExit).await.unwrap();
    drop(event_tx);

    handle.await.unwrap();
    let evs = drain(out_rx).await;

    let killed_count = evs
        .iter()
        .filter(|e| matches!(e, SubagentEvent::Killed { .. }))
        .count();
    let completed_count = evs
        .iter()
        .filter(|e| matches!(e, SubagentEvent::Completed { .. }))
        .count();

    // Exactly one Killed, no Completed (Killed is the terminal here).
    assert_eq!(
        killed_count, 1,
        "exactly one Killed expected on UserExit; got events: {evs:?}"
    );
    assert_eq!(
        completed_count, 0,
        "no Completed expected when terminal is UserExit; got events: {evs:?}"
    );
    let snapshot_position = evs
        .iter()
        .position(|event| matches!(event, SubagentEvent::TranscriptSnapshot { .. }))
        .expect("killed runner emits a host transcript snapshot");
    let killed_position = evs
        .iter()
        .position(|event| matches!(event, SubagentEvent::Killed { .. }))
        .unwrap();
    assert!(snapshot_position < killed_position);

    // Killed carries the agent id.
    let killed_aid = evs
        .iter()
        .find_map(|e| match e {
            SubagentEvent::Killed { agent_id } => Some(*agent_id),
            _ => None,
        })
        .unwrap();
    assert_eq!(killed_aid, agent_id);
}

#[tokio::test]
async fn run_subagent_emits_killed_on_user_interrupt_terminal() {
    // Cancellation terminates an actual pending model request.
    let ctx = loop_ctx(Arc::new(PendingApiClient), None, 1);
    let agent_id = ctx.agent_id;

    let (event_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(8);

    let handle = tokio::spawn(run_subagent(ctx, event_rx, out_tx));

    event_tx
        .send(lingxi_core::Event::UserInterrupt)
        .await
        .unwrap();
    drop(event_tx);

    handle.await.unwrap();
    let evs = drain(out_rx).await;

    assert!(
        evs.iter()
            .any(|e| matches!(e, SubagentEvent::Killed { agent_id: aid } if *aid == agent_id)),
        "expected exactly one Killed on UserInterrupt; got events: {evs:?}"
    );
    assert!(
        !evs.iter()
            .any(|e| matches!(e, SubagentEvent::Failed { .. })),
        "no Failed expected on UserInterrupt; got events: {evs:?}"
    );
}

#[tokio::test]
async fn run_subagent_rejects_an_unconfigured_api_before_any_work() {
    let ctx = fresh_subagent_ctx();
    let agent_id = ctx.agent_id;

    let (event_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(8);

    let handle = tokio::spawn(run_subagent(ctx, event_rx, out_tx));

    // Configuration failure does not depend on the event channel staying open.
    drop(event_tx);

    handle.await.unwrap();
    let evs = drain(out_rx).await;

    let failed = evs.iter().find_map(|e| match e {
        SubagentEvent::Failed {
            agent_id: aid,
            error,
            ..
        } => Some((*aid, error.clone())),
        _ => None,
    });
    assert!(
        failed.is_some(),
        "exactly one Failed expected on premature EOF; got events: {evs:?}"
    );
    let (failed_aid, failed_err) = failed.unwrap();
    assert_eq!(failed_aid, agent_id);
    assert_eq!(
        failed_err, "Subagent model API is not configured",
        "byte-locked error message"
    );

    let completed_count = evs
        .iter()
        .filter(|e| matches!(e, SubagentEvent::Completed { .. }))
        .count();
    assert_eq!(
        completed_count, 0,
        "no Completed expected; got events: {evs:?}"
    );
}

struct PendingApiClient;

#[async_trait]
impl crate::api::SubagentApiClient for PendingApiClient {
    async fn stream(
        &self,
        _request: crate::api::SubagentApiRequest,
    ) -> Result<
        futures::stream::BoxStream<
            'static,
            Result<llm_runtime::HistoryEvent, llm_runtime::LlmError>,
        >,
        llm_runtime::LlmError,
    > {
        std::future::pending().await
    }
}

// ---- Model-request loop tests ----------------------------------------

/// Pull the single `Completed.result` payload (panics if none / many).
fn one_completed(evs: &[SubagentEvent]) -> serde_json::Value {
    let mut found = evs.iter().filter_map(|e| match e {
        SubagentEvent::Completed { result, .. } => Some(result.clone()),
        _ => None,
    });
    let r = found.next().expect("exactly one Completed");
    assert!(found.next().is_none(), "more than one Completed: {evs:?}");
    r
}

#[tokio::test]
async fn loop_single_end_turn_completes_with_aggregated_text() {
    // One turn: end_turn, no tools. Asserts the terminal-text path and the
    // `{text, stop_reason}` result shape, and that the model was called once.
    let api = MockSubagentApiClient::new(vec![Ok(text_response("final answer", Some("end_turn")))]);
    let ctx = loop_ctx(api.clone(), None, 4);

    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    run_subagent(ctx, event_rx, out_tx).await;
    let evs = drain(out_rx).await;

    assert_eq!(api.call_count(), 1, "exactly one model round-trip");
    let result = one_completed(&evs);
    assert_eq!(result["text"], "final answer");
    assert_eq!(result["stop_reason"], "end_turn");
}

#[tokio::test]
async fn child_turn_complete_mod_covers_answer_error_and_abort() {
    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("child-turn-complete.js");
    std::fs::write(
        &module,
        r#"let seen;
let count = 0;
export function register(on) {
  on('turn.complete', ($, e, next) => {
    seen = { ...e, probeCount: ++count };
    return next(e);
  });
  on('prompt.submit', ($, e, next) => next({ ...e, text: JSON.stringify(seen ?? null) }));
}"#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load(
        "child-turn-complete",
        dir.path(),
        &module,
        serde_json::json!({}),
    )
    .await
    .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host.clone());
    let executor = Arc::new(hooks::HookExecutorImpl::new(
        Arc::new(tokio::sync::RwLock::new(registry)),
        Arc::new(test_harness::mocks::MockHttpTransport::new()),
        Arc::new(test_harness::mocks::MockRuntimeSpawner::default()),
    ));
    let mut response = text_response("child answer", Some("end_turn"));
    response.usage.counts_mut().input_tokens = 17;
    response.usage.counts_mut().output_tokens = 9;
    let api = MockSubagentApiClient::new(vec![Ok(response)]);
    let mut ctx = loop_ctx(api, None, 1);
    ctx.hook_executor = Some(executor.clone());
    ctx.hook_cwd = dir.path().to_path_buf();
    let agent_id = ctx.agent_id.to_string();
    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    run_subagent(ctx, event_rx, out_tx).await;
    assert_eq!(one_completed(&drain(out_rx).await)["text"], "child answer");
    let observed = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let result = host
                .dispatch(
                    "prompt.submit",
                    serde_json::json!({"text":"probe"}),
                    |event| async move { Ok(event) },
                )
                .await
                .unwrap();
            if result["text"] != "null" {
                break result;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("child turn.complete dispatches");
    let event: serde_json::Value =
        serde_json::from_str(observed["text"].as_str().unwrap()).unwrap();
    assert_eq!(event["agentId"], agent_id);
    assert_eq!(event["answer"], "child answer");
    assert_eq!(event["reason"], "answer");
    assert_eq!(event["usage"]["input_tokens"], 17);
    assert_eq!(event["usage"]["output_tokens"], 9);
    assert_eq!(event["probeCount"], 1);

    let failing_api = MockSubagentApiClient::new(vec![Err(llm_runtime::LlmError::Transport {
        message: "provider failed".into(),
    })]);
    let mut failing_ctx = loop_ctx(failing_api, None, 1);
    failing_ctx.hook_executor = Some(executor.clone());
    failing_ctx.hook_cwd = dir.path().to_path_buf();
    let failing_id = failing_ctx.agent_id.to_string();
    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    run_subagent(failing_ctx, event_rx, out_tx).await;
    assert!(drain(out_rx)
        .await
        .iter()
        .any(|event| matches!(event, SubagentEvent::Failed { .. })));
    let failed_event = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let result = host
                .dispatch(
                    "prompt.submit",
                    serde_json::json!({"text":"probe"}),
                    |event| async move { Ok(event) },
                )
                .await
                .unwrap();
            let event: serde_json::Value =
                serde_json::from_str(result["text"].as_str().unwrap()).unwrap();
            if event["agentId"] == failing_id {
                break event;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("failed child dispatches turn.complete");
    assert_eq!(failed_event["reason"], "error");
    assert!(failed_event.get("usage").is_none());
    assert_eq!(failed_event["probeCount"], 2);

    let unused_api =
        MockSubagentApiClient::new(vec![Ok(text_response("unused", Some("end_turn")))]);
    let mut abort_ctx = loop_ctx(unused_api, None, 1);
    abort_ctx.hook_executor = Some(executor.clone());
    abort_ctx.hook_cwd = dir.path().to_path_buf();
    let abort_id = abort_ctx.agent_id.to_string();
    let (event_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    event_tx
        .send(lingxi_core::Event::UserInterrupt)
        .await
        .unwrap();
    run_subagent(abort_ctx, event_rx, out_tx).await;
    assert!(drain(out_rx)
        .await
        .iter()
        .any(|event| matches!(event, SubagentEvent::Killed { .. })));
    let aborted_event = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let result = host
                .dispatch(
                    "prompt.submit",
                    serde_json::json!({"text":"probe"}),
                    |event| async move { Ok(event) },
                )
                .await
                .unwrap();
            let event: serde_json::Value =
                serde_json::from_str(result["text"].as_str().unwrap()).unwrap();
            if event["agentId"] == abort_id {
                break event;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("aborted child dispatches turn.complete");
    assert_eq!(aborted_event["reason"], "aborted");
    assert_eq!(aborted_event["isAborted"], true);
    assert_eq!(aborted_event["probeCount"], 3);

    let mut tool_response = tool_use_response("Read", Some("tool_use"));
    tool_response.usage.counts_mut().input_tokens = 4;
    tool_response.usage.counts_mut().output_tokens = 2;
    let mut final_response = text_response("after tool", Some("end_turn"));
    final_response.usage.counts_mut().input_tokens = 17;
    final_response.usage.counts_mut().output_tokens = 9;
    let api = MockSubagentApiClient::new(vec![Ok(tool_response), Ok(final_response)]);
    let mut multi_ctx = loop_ctx(api, Some(CountingInvoker::new()), 2);
    multi_ctx.hook_executor = Some(executor);
    multi_ctx.hook_cwd = dir.path().to_path_buf();
    let multi_id = multi_ctx.agent_id.to_string();
    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    run_subagent(multi_ctx, event_rx, out_tx).await;
    assert_eq!(one_completed(&drain(out_rx).await)["text"], "after tool");
    let multi_event = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let result = host
                .dispatch(
                    "prompt.submit",
                    serde_json::json!({"text":"probe"}),
                    |event| async move { Ok(event) },
                )
                .await
                .unwrap();
            let event: serde_json::Value =
                serde_json::from_str(result["text"].as_str().unwrap()).unwrap();
            if event["agentId"] == multi_id {
                break event;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("multi-response child dispatches turn.complete");
    assert_eq!(multi_event["answer"], "after tool");
    assert_eq!(multi_event["usage"]["input_tokens"], 21);
    assert_eq!(multi_event["usage"]["output_tokens"], 11);
    assert_eq!(multi_event["probeCount"], 4);
}

#[tokio::test]
async fn child_turn_complete_mod_uses_bound_session_ui() {
    struct ChildSession {
        cwd: std::path::PathBuf,
        logs: Arc<tokio::sync::Mutex<Vec<String>>>,
    }
    #[async_trait]
    impl hooks::mods::ModSessionContext for ChildSession {
        fn cwd(&self) -> std::path::PathBuf {
            self.cwd.clone()
        }
        fn root(&self) -> std::path::PathBuf {
            self.cwd.clone()
        }
        async fn model(&self) -> String {
            "test-model".into()
        }
        async fn id(&self) -> String {
            "test-session".into()
        }
        async fn turns(&self) -> u64 {
            1
        }
        async fn emit_mod_log(&self, _plugin: &str, text: &str) {
            self.logs.lock().await.push(text.to_owned());
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("child-ui.js");
    std::fs::write(
        &module,
        r#"export function register(on) {
  on('turn.start', async ($, e, next) => {
    $.ui.log(`child-start:${await $.session.cwd()}:${e.text}`);
    return next(e);
  });
  on('turn.complete', async ($, e, next) => {
    $.ui.log(`child-ui:${await $.session.cwd()}:${e.reason}:${e.answer}`);
    return next(e);
  });
}"#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load("child-ui", dir.path(), &module, serde_json::json!({}))
        .await
        .unwrap();
    let logs = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let session: Arc<dyn hooks::mods::ModSessionContext> = Arc::new(ChildSession {
        cwd: dir.path().to_path_buf(),
        logs: logs.clone(),
    });
    host.attach_background_context(Arc::downgrade(&session));
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host);
    let executor = Arc::new(hooks::HookExecutorImpl::new(
        Arc::new(tokio::sync::RwLock::new(registry)),
        Arc::new(test_harness::mocks::MockHttpTransport::new()),
        Arc::new(test_harness::mocks::MockRuntimeSpawner::default()),
    ));
    let api = MockSubagentApiClient::new(vec![Ok(text_response("visible", Some("end_turn")))]);
    let mut ctx = loop_ctx(api, None, 1);
    ctx.hook_executor = Some(executor.clone());
    ctx.hook_cwd = dir.path().to_path_buf();
    ctx.prompt_messages = vec![ConversationMessage::user(
        MessageId::new(),
        "visible question".to_string(),
    )];
    let child_cwd = dir.path().join("child");
    std::fs::create_dir(&child_cwd).unwrap();
    ctx.cwd = Some(child_cwd.clone());
    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    run_subagent(ctx, event_rx, out_tx).await;
    assert_eq!(one_completed(&drain(out_rx).await)["text"], "visible");
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if logs
                .lock()
                .await
                .iter()
                .any(|text| text == &format!("child-ui:{}:answer:visible", child_cwd.display()))
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("child Mod log reaches owning session");
    assert!(logs
        .lock()
        .await
        .iter()
        .any(|text| text == &format!("child-start:{}:visible question", child_cwd.display())));
}

#[tokio::test]
async fn child_turn_step_mod_rewrites_provider_model_and_visible_text() {
    struct CapturingApi {
        models: Arc<Mutex<Vec<String>>>,
    }
    #[async_trait]
    impl crate::api::SubagentApiClient for CapturingApi {
        async fn stream(
            &self,
            request: crate::api::SubagentApiRequest,
        ) -> Result<
            futures::stream::BoxStream<
                'static,
                Result<llm_runtime::HistoryEvent, llm_runtime::LlmError>,
            >,
            llm_runtime::LlmError,
        > {
            use futures::StreamExt;
            self.models.lock().unwrap().push(request.model);
            let mut response = text_response("physical text", Some("end_turn"));
            response.usage.counts_mut().input_tokens = 11;
            response.usage.counts_mut().output_tokens = 7;
            let events = llm_runtime::stream_accumulator::response_to_stream_events(response);
            Ok(futures::stream::iter(events.into_iter().map(Ok)).boxed())
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("child-step.js");
    std::fs::write(
        &module,
        r#"let seen;
export function register(on) {
  on('turn.step', async function* ($, e, next) {
    if (typeof e.agentId !== 'string' || !e.turnId || e.index !== 0) {
      throw new Error('child step identity missing');
    }
    if (!(await $.session.cwd()).endsWith('/child')) {
      throw new Error('child step cwd missing');
    }
    const below = next({ ...e, model: 'child-rewrite-model' });
    for await (const chunk of below) {
      yield chunk.kind === 'text' ? { ...chunk, text: 'hooked child text' } : chunk;
    }
    return await below.result;
  });
  on('turn.complete', ($, e, next) => {
    seen = e;
    return next(e);
  });
  on('prompt.submit', ($, e, next) => next({ ...e, text: JSON.stringify(seen ?? null) }));
}"#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load("child-step", dir.path(), &module, serde_json::json!({}))
        .await
        .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host.clone());
    let executor = Arc::new(hooks::HookExecutorImpl::new(
        Arc::new(tokio::sync::RwLock::new(registry)),
        Arc::new(test_harness::mocks::MockHttpTransport::new()),
        Arc::new(test_harness::mocks::MockRuntimeSpawner::default()),
    ));
    let models = Arc::new(Mutex::new(Vec::new()));
    let api = Arc::new(CapturingApi {
        models: models.clone(),
    });
    let mut ctx = loop_ctx(api, None, 1);
    ctx.hook_executor = Some(executor);
    ctx.hook_cwd = dir.path().to_path_buf();
    let child_cwd = dir.path().join("child");
    std::fs::create_dir(&child_cwd).unwrap();
    ctx.cwd = Some(child_cwd);
    ctx.prompt_messages = vec![ConversationMessage::user(
        MessageId::new(),
        "question".into(),
    )];
    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        run_subagent(ctx, event_rx, out_tx),
    )
    .await
    .expect("child Mod stream completes");
    let events = drain(out_rx).await;
    assert_eq!(one_completed(&events)["text"], "hooked child text");
    assert_eq!(*models.lock().unwrap(), vec!["child-rewrite-model"]);
    let event = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let result = host
                .dispatch(
                    "prompt.submit",
                    serde_json::json!({"text":"probe"}),
                    |event| async move { Ok(event) },
                )
                .await
                .unwrap();
            let event: serde_json::Value =
                serde_json::from_str(result["text"].as_str().unwrap()).unwrap();
            if !event.is_null() {
                break event;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("physical usage reaches turn.complete");
    assert_eq!(event["answer"], "hooked child text");
    assert_eq!(event["usage"]["input_tokens"], 11);
    assert_eq!(event["usage"]["output_tokens"], 7);
}

#[tokio::test]
async fn child_turn_step_result_summarizes_real_stream_events() {
    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("stream-result.js");
    std::fs::write(
        &module,
        r#"let seen;
export function register(on) {
  on('turn.step', async function* ($, e, next) {
    const below = next(e);
    for await (const chunk of below) yield chunk;
    seen = await below.result;
    return seen;
  });
  on('prompt.submit', ($, e, next) => next({ ...e, text: JSON.stringify(seen ?? null) }));
}"#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load("stream-result", dir.path(), &module, serde_json::json!({}))
        .await
        .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host.clone());
    let executor = Arc::new(hooks::HookExecutorImpl::new(
        Arc::new(tokio::sync::RwLock::new(registry)),
        Arc::new(test_harness::mocks::MockHttpTransport::new()),
        Arc::new(test_harness::mocks::MockRuntimeSpawner::default()),
    ));
    let mut response = text_response("streamed child", Some("end_turn"));
    response.usage.counts_mut().input_tokens = 11;
    response.usage.counts_mut().output_tokens = 7;
    let api = StreamingMockApiClient::new(vec![
        llm_runtime::stream_accumulator::response_to_stream_events(response),
    ]);
    let mut ctx = loop_ctx(api.clone(), None, 1);
    ctx.hook_executor = Some(executor);
    ctx.hook_cwd = dir.path().to_path_buf();
    ctx.prompt_messages = vec![ConversationMessage::user(
        MessageId::new(),
        "question".into(),
    )];
    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        run_subagent(ctx, event_rx, out_tx),
    )
    .await
    .expect("child streamed Mod step completes");
    assert_eq!(
        one_completed(&drain(out_rx).await)["text"],
        "streamed child"
    );
    assert_eq!(api.call_count(), 1);
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let result = host
                .dispatch(
                    "prompt.submit",
                    serde_json::json!({"text":"probe"}),
                    |event| async move { Ok(event) },
                )
                .await
                .unwrap();
            let result: serde_json::Value =
                serde_json::from_str(result["text"].as_str().unwrap()).unwrap();
            if !result.is_null() {
                break result;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("streamed next.result settles");
    assert_eq!(result["answer"], "streamed child");
    assert_eq!(result["stopReason"], "end_turn");
    assert_eq!(result["usage"]["input_tokens"], 11);
    assert_eq!(result["usage"]["output_tokens"], 7);
}

#[tokio::test]
async fn child_turn_step_managed_policy_rejects_model_rewrite() {
    struct DenyRewrite {
        checked: Arc<Mutex<Vec<String>>>,
    }
    #[async_trait]
    impl hooks::mods::ModSettingsReader for DenyRewrite {
        async fn read(
            &self,
            _input: serde_json::Value,
        ) -> Result<serde_json::Value, hooks::mods::ModError> {
            Ok(serde_json::json!({}))
        }

        async fn model_allowed(&self, model: &str) -> Result<Option<bool>, hooks::mods::ModError> {
            self.checked.lock().unwrap().push(model.to_owned());
            Ok(Some(model != "wire-child-model"))
        }
    }

    struct CapturingApi(Arc<Mutex<Vec<String>>>);
    #[async_trait]
    impl crate::api::SubagentApiClient for CapturingApi {
        fn resolve_mod_media_route(
            &self,
            model: &str,
            _profile: Option<&str>,
        ) -> Option<llm_runtime::MediaRoute> {
            if model != "denied-child-model" {
                return None;
            }
            Some(llm_runtime::MediaRoute {
                main: llm_runtime::ResolvedRoute {
                    provider_id: llm_runtime::ProviderId::AnthropicFirstParty,
                    profile_name: "rewritten-profile".into(),
                    request_model: "wire-child-model".into(),
                    display_model: model.into(),
                    pricing_model: llm_runtime::PricingModelRef {
                        pricing_provider_id: llm_runtime::ProviderId::AnthropicFirstParty,
                        billing_model: model.into(),
                        request_model: "wire-child-model".into(),
                        display_model: model.into(),
                    },
                    capabilities: llm_runtime::Capabilities::default(),
                    connection_chain: Vec::new(),
                    failover: llm_runtime::FailoverTriggers::default(),
                },
                vision_delegate: None,
            })
        }

        async fn stream(
            &self,
            request: crate::api::SubagentApiRequest,
        ) -> Result<
            futures::stream::BoxStream<
                'static,
                Result<llm_runtime::HistoryEvent, llm_runtime::LlmError>,
            >,
            llm_runtime::LlmError,
        > {
            use futures::StreamExt;
            self.0.lock().unwrap().push(request.model);
            let events = llm_runtime::stream_accumulator::response_to_stream_events(text_response(
                "original model answer",
                Some("end_turn"),
            ));
            Ok(futures::stream::iter(events.into_iter().map(Ok)).boxed())
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("deny-child-model.js");
    std::fs::write(
        &module,
        r#"export function register(on) {
  on('turn.step', async function* ($, e, next) {
    const below = next({ ...e, model: 'denied-child-model' });
    for await (const chunk of below) yield chunk;
    return await below.result;
  });
}"#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load(
        "deny-child-model",
        dir.path(),
        &module,
        serde_json::json!({}),
    )
    .await
    .unwrap();
    let checked = Arc::new(Mutex::new(Vec::new()));
    let mut registry = hooks::HookRegistry::new();
    registry.attach_mod_model_policy(Arc::new(DenyRewrite {
        checked: checked.clone(),
    }));
    registry.set_mod_host(host);
    let executor = Arc::new(hooks::HookExecutorImpl::new(
        Arc::new(tokio::sync::RwLock::new(registry)),
        Arc::new(test_harness::mocks::MockHttpTransport::new()),
        Arc::new(test_harness::mocks::MockRuntimeSpawner::default()),
    ));
    let models = Arc::new(Mutex::new(Vec::new()));
    let mut ctx = loop_ctx(Arc::new(CapturingApi(models.clone())), None, 1);
    ctx.hook_executor = Some(executor);
    ctx.hook_cwd = dir.path().to_path_buf();
    ctx.prompt_messages = vec![ConversationMessage::user(
        MessageId::new(),
        "question".into(),
    )];
    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        run_subagent(ctx, event_rx, out_tx),
    )
    .await
    .expect("child Mod stream completes");
    assert_eq!(
        one_completed(&drain(out_rx).await)["text"],
        "original model answer"
    );
    assert_eq!(*checked.lock().unwrap(), vec!["wire-child-model"]);
    assert_eq!(*models.lock().unwrap(), vec!["inherit"]);
}

#[tokio::test]
async fn child_turn_step_synthetic_answer_skips_provider() {
    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("child-synthetic-step.js");
    std::fs::write(
        &module,
        r#"let seen;
export function register(on) {
  on('turn.step', async function* ($, e) {
    yield { kind: 'text', index: 0, text: 'synthetic child' };
    yield { kind: 'stop', stopReason: 'end_turn', usage: null };
    return { turnId: e.turnId, index: e.index, answer: 'synthetic child',
      toolUses: [], stopReason: 'end_turn', usage: null };
  });
  on('turn.complete', ($, e, next) => {
    seen = e;
    return next(e);
  });
  on('prompt.submit', ($, e, next) => next({ ...e, text: JSON.stringify(seen ?? null) }));
}"#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load(
        "child-synthetic-step",
        dir.path(),
        &module,
        serde_json::json!({}),
    )
    .await
    .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host.clone());
    let executor = Arc::new(hooks::HookExecutorImpl::new(
        Arc::new(tokio::sync::RwLock::new(registry)),
        Arc::new(test_harness::mocks::MockHttpTransport::new()),
        Arc::new(test_harness::mocks::MockRuntimeSpawner::default()),
    ));
    let api = MockSubagentApiClient::new(vec![]);
    let mut ctx = loop_ctx(api.clone(), None, 1);
    ctx.hook_executor = Some(executor);
    ctx.hook_cwd = dir.path().to_path_buf();
    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    run_subagent(ctx, event_rx, out_tx).await;
    assert_eq!(
        one_completed(&drain(out_rx).await)["text"],
        "synthetic child"
    );
    assert_eq!(api.call_count(), 0);
    let event = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let result = host
                .dispatch(
                    "prompt.submit",
                    serde_json::json!({"text":"probe"}),
                    |event| async move { Ok(event) },
                )
                .await
                .unwrap();
            let event: serde_json::Value =
                serde_json::from_str(result["text"].as_str().unwrap()).unwrap();
            if !event.is_null() {
                break event;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("synthetic turn completes");
    assert_eq!(event["answer"], "synthetic child");
    assert!(event.get("usage").is_none(), "event: {event}");
}

#[tokio::test]
async fn child_turn_step_keeps_synthetic_and_physical_responses_in_one_step() {
    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("child-two-records.js");
    std::fs::write(
        &module,
        r#"export function register(on) {
  on('turn.step', async function* ($, e, next) {
    yield { kind: 'text', index: 0, text: 'synthetic first' };
    const below = next(e);
    for await (const chunk of below) yield chunk;
    return await below.result;
  });
}"#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load(
        "child-two-records",
        dir.path(),
        &module,
        serde_json::json!({}),
    )
    .await
    .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host);
    let executor = Arc::new(hooks::HookExecutorImpl::new(
        Arc::new(tokio::sync::RwLock::new(registry)),
        Arc::new(test_harness::mocks::MockHttpTransport::new()),
        Arc::new(test_harness::mocks::MockRuntimeSpawner::default()),
    ));
    let api =
        MockSubagentApiClient::new(vec![Ok(text_response("physical second", Some("end_turn")))]);
    let mut ctx = loop_ctx(api.clone(), None, 1);
    ctx.hook_executor = Some(executor);
    ctx.hook_cwd = dir.path().to_path_buf();
    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        run_subagent(ctx, event_rx, out_tx),
    )
    .await
    .expect("child Mod stream completes");
    assert_eq!(api.call_count(), 1);
    let events = drain(out_rx).await;
    let assistant_texts: Vec<String> = events
        .iter()
        .filter_map(|event| match event {
            SubagentEvent::Message { message, .. } => {
                serde_json::from_value::<ConversationMessage>(message.clone())
                    .ok()
                    .and_then(|message| match message {
                        ConversationMessage::Assistant { content, .. } => Some(
                            content
                                .iter()
                                .filter_map(|block| match block {
                                    ContentBlock::Text { text, .. } => Some(text.as_str()),
                                    _ => None,
                                })
                                .collect(),
                        ),
                        _ => None,
                    })
            }
            _ => None,
        })
        .collect();
    assert_eq!(assistant_texts.len(), 2, "{events:?}");
    assert!(assistant_texts[0].contains("synthetic first"));
    assert!(assistant_texts[1].contains("physical second"));
    assert_eq!(one_completed(&events)["text"], "physical second");
}

#[tokio::test]
async fn child_turn_step_drains_tool_before_later_response() {
    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("child-tool-then-answer.js");
    std::fs::write(
        &module,
        r#"export function register(on) {
  on('turn.step', async function* ($, e, next) {
    yield { kind: 'tool', index: 0, id: 'toolu_child_first', name: 'Read' };
    yield { kind: 'input', index: 0, json: '{}' };
    yield { kind: 'stop', stopReason: 'tool_use', usage: null };
    const below = next(e);
    for await (const chunk of below) yield chunk;
    return await below.result;
  });
}"#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load(
        "child-tool-then-answer",
        dir.path(),
        &module,
        serde_json::json!({}),
    )
    .await
    .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host);
    let executor = Arc::new(hooks::HookExecutorImpl::new(
        Arc::new(tokio::sync::RwLock::new(registry)),
        Arc::new(test_harness::mocks::MockHttpTransport::new()),
        Arc::new(test_harness::mocks::MockRuntimeSpawner::default()),
    ));
    let api = MockSubagentApiClient::new(vec![Ok(text_response(
        "after child tool",
        Some("end_turn"),
    ))]);
    let invoker = CountingInvoker::new();
    let mut ctx = loop_ctx(api.clone(), Some(invoker.clone()), 1);
    ctx.hook_executor = Some(executor);
    ctx.hook_cwd = dir.path().to_path_buf();
    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        run_subagent(ctx, event_rx, out_tx),
    )
    .await
    .expect("child tool and later answer complete");
    assert_eq!(api.call_count(), 1);
    assert_eq!(invoker.call_count(), 1);
    let events = drain(out_rx).await;
    let messages: Vec<ConversationMessage> = events
        .iter()
        .filter_map(|event| match event {
            SubagentEvent::Message { message, .. } => serde_json::from_value(message.clone()).ok(),
            _ => None,
        })
        .collect();
    let tool = messages
        .iter()
        .position(|message| {
            matches!(message,
                ConversationMessage::Assistant { content, .. }
                    if content.iter().any(|block| matches!(block, ContentBlock::ToolUse { .. }))
            )
        })
        .expect("tool response");
    let result = messages
        .iter()
        .position(|message| {
            matches!(message,
                ConversationMessage::User { content, .. }
                    if content.iter().any(|block| matches!(block, ContentBlock::ToolResult { .. }))
            )
        })
        .expect("tool result");
    let answer = messages
        .iter()
        .position(|message| {
            matches!(message,
                ConversationMessage::Assistant { content, .. }
                    if content.iter().any(|block| matches!(block,
                        ContentBlock::Text { text, .. } if text == "after child tool"))
            )
        })
        .expect("later answer");
    assert!(tool < result && result < answer);
    assert_eq!(one_completed(&events)["text"], "after child tool");
}

#[tokio::test]
async fn child_turn_step_two_visible_next_calls_share_one_turn_index() {
    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("child-two-visible-next.js");
    std::fs::write(
        &module,
        r#"export function register(on) {
  on('turn.step', async function* ($, e, next) {
    if (e.index !== 0) throw new Error('second assistant must not advance turn index');
    const first = next(e);
    for await (const chunk of first) yield chunk;
    await first.result;
    const second = next(e);
    for await (const chunk of second) yield chunk;
    return await second.result;
  });
}"#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load(
        "child-two-visible-next",
        dir.path(),
        &module,
        serde_json::json!({}),
    )
    .await
    .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host);
    let executor = Arc::new(hooks::HookExecutorImpl::new(
        Arc::new(tokio::sync::RwLock::new(registry)),
        Arc::new(test_harness::mocks::MockHttpTransport::new()),
        Arc::new(test_harness::mocks::MockRuntimeSpawner::default()),
    ));
    let mut first = text_response("first physical", Some("end_turn"));
    first.usage.counts_mut().output_tokens = 3;
    let mut second = text_response("second physical", Some("end_turn"));
    second.usage.counts_mut().output_tokens = 5;
    let api = MockSubagentApiClient::new(vec![Ok(first), Ok(second)]);
    let mut ctx = loop_ctx(api.clone(), None, 1);
    ctx.hook_executor = Some(executor);
    ctx.hook_cwd = dir.path().to_path_buf();
    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        run_subagent(ctx, event_rx, out_tx),
    )
    .await
    .expect("two visible child responses complete");
    assert_eq!(api.call_count(), 2);
    let events = drain(out_rx).await;
    let texts: Vec<String> = events
        .iter()
        .filter_map(|event| match event {
            SubagentEvent::Message { message, .. } => {
                serde_json::from_value::<ConversationMessage>(message.clone())
                    .ok()
                    .and_then(|message| match message {
                        ConversationMessage::Assistant { content, .. } => Some(
                            content
                                .iter()
                                .filter_map(|block| match block {
                                    ContentBlock::Text { text, .. } => Some(text.as_str()),
                                    _ => None,
                                })
                                .collect(),
                        ),
                        _ => None,
                    })
            }
            _ => None,
        })
        .collect();
    assert_eq!(texts, ["first physical", "second physical"]);
    assert_eq!(one_completed(&events)["text"], "second physical");
    let usage = events
        .iter()
        .find_map(|event| match event {
            SubagentEvent::Completed {
                cumulative_usage, ..
            } => Some(cumulative_usage.counts()),
            _ => None,
        })
        .unwrap();
    assert_eq!(usage.output_tokens, 8);
    let calls = api.physical_calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].request.model, calls[1].request.model);
    assert_eq!(calls[0].request.profile, calls[1].request.profile);
    assert_eq!(calls[0].request.system, calls[1].request.system);
    assert_eq!(calls[0].request.messages, calls[1].request.messages);
    assert_eq!(calls[0].request.tools, calls[1].request.tools);
    assert_eq!(calls[0].request.forced_tool, calls[1].request.forced_tool);
    assert_eq!(calls[0].request.effort, calls[1].request.effort);
    assert_eq!(
        calls[0].request.opts.max_output_tokens,
        calls[1].request.opts.max_output_tokens
    );
    assert_eq!(
        calls[0].request.opts.query_source_label,
        calls[1].request.opts.query_source_label
    );
    assert_eq!(
        calls
            .iter()
            .map(|call| {
                call.response
                    .as_ref()
                    .map(|response| response.usage.counts().output_tokens)
            })
            .collect::<Vec<_>>(),
        [Some(3), Some(5)]
    );
    assert_eq!(
        calls
            .iter()
            .map(|call| {
                call.response
                    .as_ref()
                    .and_then(|response| response.stop_reason.as_deref())
            })
            .collect::<Vec<_>>(),
        [Some("end_turn"), Some("end_turn")]
    );
}

#[tokio::test]
async fn child_turn_step_multiple_next_calls_meter_all_physical_responses() {
    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("child-multiple-next.js");
    std::fs::write(
        &module,
        r#"let seen;
export function register(on) {
  on('turn.step', async function* ($, e, next) {
    const first = next(e);
    for await (const _chunk of first) {}
    const second = next(e);
    for await (const chunk of second) yield chunk;
    return await second.result;
  });
  on('turn.complete', ($, e, next) => {
    seen = e;
    return next(e);
  });
  on('prompt.submit', ($, e, next) => next({ ...e, text: JSON.stringify(seen ?? null) }));
}"#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load(
        "child-multiple-next",
        dir.path(),
        &module,
        serde_json::json!({}),
    )
    .await
    .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host.clone());
    let executor = Arc::new(hooks::HookExecutorImpl::new(
        Arc::new(tokio::sync::RwLock::new(registry)),
        Arc::new(test_harness::mocks::MockHttpTransport::new()),
        Arc::new(test_harness::mocks::MockRuntimeSpawner::default()),
    ));
    let mut first = text_response("hidden first", Some("end_turn"));
    first.usage.counts_mut().input_tokens = 11;
    first.usage.counts_mut().output_tokens = 7;
    let mut second = text_response("visible second", Some("end_turn"));
    second.usage.counts_mut().input_tokens = 13;
    second.usage.counts_mut().output_tokens = 5;
    let api = MockSubagentApiClient::new(vec![Ok(first), Ok(second)]);
    let mut ctx = loop_ctx(api.clone(), None, 1);
    ctx.hook_executor = Some(executor);
    ctx.hook_cwd = dir.path().to_path_buf();
    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    run_subagent(ctx, event_rx, out_tx).await;
    assert_eq!(api.call_count(), 2);
    let events = drain(out_rx).await;
    assert_eq!(one_completed(&events)["text"], "visible second");
    let completed_usage = events
        .iter()
        .find_map(|event| match event {
            SubagentEvent::Completed {
                cumulative_usage, ..
            } => Some(cumulative_usage.counts()),
            _ => None,
        })
        .unwrap();
    assert_eq!(completed_usage.input_tokens, 24);
    assert_eq!(completed_usage.output_tokens, 12);
    let event = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let result = host
                .dispatch(
                    "prompt.submit",
                    serde_json::json!({"text":"probe"}),
                    |event| async move { Ok(event) },
                )
                .await
                .unwrap();
            let event: serde_json::Value =
                serde_json::from_str(result["text"].as_str().unwrap()).unwrap();
            if !event.is_null() {
                break event;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("multi-next turn completes");
    assert_eq!(event["answer"], "visible second");
    assert_eq!(event["usage"]["input_tokens"], 24);
    assert_eq!(event["usage"]["output_tokens"], 12);
}

#[tokio::test]
async fn child_turn_step_cancel_after_paid_next_keeps_usage() {
    struct FirstThenPending {
        calls: AtomicUsize,
        second_started: Arc<tokio::sync::Notify>,
        first: llm_runtime::HistoryResponse,
    }
    #[async_trait]
    impl crate::api::SubagentApiClient for FirstThenPending {
        async fn stream(
            &self,
            _request: crate::api::SubagentApiRequest,
        ) -> Result<
            futures::stream::BoxStream<
                'static,
                Result<llm_runtime::HistoryEvent, llm_runtime::LlmError>,
            >,
            llm_runtime::LlmError,
        > {
            use futures::StreamExt;
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                let events =
                    llm_runtime::stream_accumulator::response_to_stream_events(self.first.clone());
                return Ok(futures::stream::iter(events.into_iter().map(Ok)).boxed());
            }
            self.second_started.notify_one();
            std::future::pending().await
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("child-cancel-step.js");
    std::fs::write(
        &module,
        r#"let seen;
export function register(on) {
  on('turn.step', async function* ($, e, next) {
    const first = next(e);
    for await (const _chunk of first) {}
    const second = next(e);
    for await (const chunk of second) yield chunk;
    return await second.result;
  });
  on('turn.complete', ($, e, next) => {
    seen = e;
    return next(e);
  });
  on('prompt.submit', ($, e, next) => next({ ...e, text: JSON.stringify(seen ?? null) }));
}"#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load(
        "child-cancel-step",
        dir.path(),
        &module,
        serde_json::json!({}),
    )
    .await
    .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host.clone());
    let executor = Arc::new(hooks::HookExecutorImpl::new(
        Arc::new(tokio::sync::RwLock::new(registry)),
        Arc::new(test_harness::mocks::MockHttpTransport::new()),
        Arc::new(test_harness::mocks::MockRuntimeSpawner::default()),
    ));
    let mut first = text_response("paid first", Some("end_turn"));
    first.usage.counts_mut().input_tokens = 11;
    first.usage.counts_mut().output_tokens = 7;
    let second_started = Arc::new(tokio::sync::Notify::new());
    let api = Arc::new(FirstThenPending {
        calls: AtomicUsize::new(0),
        second_started: second_started.clone(),
        first,
    });
    let mut ctx = loop_ctx(api.clone(), None, 2);
    ctx.hook_executor = Some(executor);
    ctx.hook_cwd = dir.path().to_path_buf();
    let (event_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    let runner = tokio::spawn(run_subagent(ctx, event_rx, out_tx));
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        second_started.notified(),
    )
    .await
    .expect("second request starts");
    event_tx
        .send(lingxi_core::Event::UserInterrupt)
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), runner)
        .await
        .expect("cancelled child stops")
        .unwrap();
    assert!(drain(out_rx)
        .await
        .iter()
        .any(|event| matches!(event, SubagentEvent::Killed { .. })));
    assert_eq!(api.calls.load(Ordering::SeqCst), 2);
    let event = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let result = host
                .dispatch(
                    "prompt.submit",
                    serde_json::json!({"text":"probe"}),
                    |event| async move { Ok(event) },
                )
                .await
                .unwrap();
            let event: serde_json::Value =
                serde_json::from_str(result["text"].as_str().unwrap()).unwrap();
            if !event.is_null() {
                break event;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("cancelled child turn completes");
    assert_eq!(event["reason"], "aborted");
    assert_eq!(event["usage"]["input_tokens"], 11);
    assert_eq!(event["usage"]["output_tokens"], 7);
}

#[tokio::test]
async fn child_turn_step_failure_before_next_runs_original_request() {
    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("child-fallback-step.js");
    std::fs::write(
        &module,
        r#"export function register(on) {
  on('turn.step', async function* () {
    yield { kind: 'text', index: 0, text: 'discard synthetic text' };
    throw new Error('fails before next');
  });
}"#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load(
        "child-fallback-step",
        dir.path(),
        &module,
        serde_json::json!({}),
    )
    .await
    .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host);
    let executor = Arc::new(hooks::HookExecutorImpl::new(
        Arc::new(tokio::sync::RwLock::new(registry)),
        Arc::new(test_harness::mocks::MockHttpTransport::new()),
        Arc::new(test_harness::mocks::MockRuntimeSpawner::default()),
    ));
    let api = MockSubagentApiClient::new(vec![Ok(text_response("fallback", Some("end_turn")))]);
    let mut ctx = loop_ctx(api.clone(), None, 1);
    ctx.hook_executor = Some(executor);
    ctx.hook_cwd = dir.path().to_path_buf();
    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    run_subagent(ctx, event_rx, out_tx).await;
    let events = drain(out_rx).await;
    assert_eq!(one_completed(&events)["text"], "fallback");
    let texts: Vec<String> = events
        .iter()
        .filter_map(|event| match event {
            SubagentEvent::Message { message, .. } => {
                serde_json::from_value::<ConversationMessage>(message.clone())
                    .ok()
                    .and_then(|message| match message {
                        ConversationMessage::Assistant { content, .. } => Some(
                            content
                                .iter()
                                .filter_map(|block| match block {
                                    ContentBlock::Text { text, .. } => Some(text.as_str()),
                                    _ => None,
                                })
                                .collect(),
                        ),
                        _ => None,
                    })
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        texts,
        ["fallback"],
        "Native buffers pre-request synthetic assistant output and discards it when the hook fails before next(e)"
    );
    assert_eq!(api.call_count(), 1);
    let calls = api.physical_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].request.model, "inherit");
    assert_eq!(
        calls[0]
            .response
            .as_ref()
            .and_then(|response| response.stop_reason.as_deref()),
        Some("end_turn")
    );
}

#[tokio::test]
async fn fusion_panel_tools_receive_the_trusted_deterministic_policy() {
    let api = MockSubagentApiClient::new(vec![
        Ok(tool_use_response("WebFetch", Some("tool_use"))),
        Ok(text_response("done", Some("end_turn"))),
    ]);
    let invoker = CountingInvoker::new();
    let mut ctx = loop_ctx(api, Some(invoker.clone()), 3);
    ctx.agent_definition.agent_type = lingxi_core::host::FUSION_PANEL_TYPE.to_string();

    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    run_subagent(ctx, event_rx, out_tx).await;
    drop(drain(out_rx).await);

    assert_eq!(
        invoker.policies(),
        vec![lingxi_core::host::tool_invoker::ToolExecutionPolicy::FusionPanel],
        "the hidden resolved Fusion definition, not model input, selects deterministic WebFetch"
    );
}

#[tokio::test]
async fn loop_completed_result_carries_claude_content_array() {
    // #3: the terminal result carries claude's `content` array of text
    // blocks (one per text block), not only the joined `text` string.
    let api = MockSubagentApiClient::new(vec![Ok(text_response("final answer", Some("end_turn")))]);
    let ctx = loop_ctx(api.clone(), None, 4);
    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    run_subagent(ctx, event_rx, out_tx).await;
    let evs = drain(out_rx).await;
    let result = one_completed(&evs);
    assert_eq!(
        result["content"],
        serde_json::json!([{ "type": "text", "text": "final answer" }]),
        "result carries claude content[] array"
    );
    assert_eq!(
        result["text"], "final answer",
        "legacy `text` still present"
    );
}

#[tokio::test]
async fn loop_g2_backward_scan_recovers_text_from_earlier_turn() {
    // G2 (agentToolUtils.ts:304-317): when the FINAL assistant turn is
    // tool-only (no text), the result content falls back to the most recent
    // assistant message that HAS text. Turn 1: text "partial" + a tool_use
    // (continues). Turn 2: tool-only with end_turn (terminates, no text in
    // the final block) → content must be "partial" from turn 1.
    let api = MockSubagentApiClient::new(vec![
        Ok(text_and_tool_response("partial", "Read", Some("tool_use"))),
        Ok(tool_use_response("Read", Some("end_turn"))),
    ]);
    let invoker = CountingInvoker::new();
    let ctx = loop_ctx(api.clone(), Some(invoker.clone()), 4);
    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    run_subagent(ctx, event_rx, out_tx).await;
    let evs = drain(out_rx).await;
    let result = one_completed(&evs);
    // The final turn was tool-only; the backward scan recovered turn 1's text.
    assert_eq!(
        result["content"],
        serde_json::json!([{ "type": "text", "text": "partial" }]),
        "backward scan recovers the most recent assistant text; got {result:?}"
    );
    assert_eq!(result["text"], "partial");
}

#[tokio::test]
async fn schema_forces_structured_output_and_returns_the_tool_input() {
    // With `ctx.schema` set, the runner injects+forces a `StructuredOutput`
    // tool; the model's tool input IS the run's result (it is NOT dispatched).
    let structured = serde_json::json!({ "answer": 42, "ok": true });
    let resp = llm_runtime::HistoryResponse {
        content: vec![llm_runtime::ContentBlock::ToolCall {
            input_projection: None,
            id: ToolUseId::new().to_string(),
            name: "StructuredOutput".into(),
            input: structured.clone(),
        }],
        ..tool_use_response("StructuredOutput", Some("tool_use"))
    };
    let api = MockSubagentApiClient::new(vec![Ok(resp)]);
    let invoker = CountingInvoker::new();
    let mut ctx = loop_ctx(api, Some(invoker.clone()), 4);
    ctx.schema = Some(r#"{"type":"object"}"#.to_string());
    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    run_subagent(ctx, event_rx, out_tx).await;
    let evs = drain(out_rx).await;
    // The Completed result IS the captured StructuredOutput tool input.
    assert_eq!(one_completed(&evs), structured);
}

/// A StructuredOutput call whose input FAILS the schema is fed back as an
/// `is_error` result (the model retries); a later VALID call is captured.
#[tokio::test]
async fn schema_invalid_output_retried_then_captured() {
    let so = |input: serde_json::Value| llm_runtime::HistoryResponse {
        content: vec![llm_runtime::ContentBlock::ToolCall {
            input_projection: None,
            id: ToolUseId::new().to_string(),
            name: "StructuredOutput".into(),
            input,
        }],
        ..tool_use_response("StructuredOutput", Some("tool_use"))
    };
    let valid = serde_json::json!({ "answer": 42 });
    let api = MockSubagentApiClient::new(vec![
        Ok(so(serde_json::json!({ "answer": "not-an-int" }))), // fails: wrong type
        Ok(so(valid.clone())),                                 // passes
    ]);
    let invoker = CountingInvoker::new();
    let mut ctx = loop_ctx(api.clone(), Some(invoker.clone()), 6);
    ctx.schema = Some(
        r#"{"type":"object","required":["answer"],"properties":{"answer":{"type":"integer"}}}"#
            .to_string(),
    );
    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    run_subagent(ctx, event_rx, out_tx).await;
    let evs = drain(out_rx).await;
    assert_eq!(one_completed(&evs), valid, "the valid retry is captured");
    assert_eq!(
        api.call_count(),
        2,
        "the model retried once after the failure"
    );
}

/// cc 2.1.196 (M9): a schema-rejected StructuredOutput attempt does NOT
/// render beside its retry — no duplicate recap. The result surface a
/// workflow/Agent consumer renders is the terminal `Completed.result`
/// (`PoolSubagentSpawner::spawn` ignores Message events): after a rejected
/// attempt + a valid retry there is EXACTLY ONE Completed, its payload is the
/// retry's, and the rejected payload appears nowhere in it.
#[tokio::test]
async fn schema_rejected_attempt_is_not_surfaced_beside_its_retry() {
    let so = |input: serde_json::Value| llm_runtime::HistoryResponse {
        content: vec![llm_runtime::ContentBlock::ToolCall {
            input_projection: None,
            id: ToolUseId::new().to_string(),
            name: "StructuredOutput".into(),
            input,
        }],
        ..tool_use_response("StructuredOutput", Some("tool_use"))
    };
    let rejected = serde_json::json!({ "answer": "REJECTED-SENTINEL" });
    let valid = serde_json::json!({ "answer": 7 });
    let api = MockSubagentApiClient::new(vec![
        Ok(so(rejected.clone())), // schema-rejected attempt
        Ok(so(valid.clone())),    // its retry
    ]);
    let mut ctx = loop_ctx(api, Some(CountingInvoker::new()), 6);
    ctx.schema = Some(
        r#"{"type":"object","required":["answer"],"properties":{"answer":{"type":"integer"}}}"#
            .to_string(),
    );
    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    run_subagent(ctx, event_rx, out_tx).await;
    let evs = drain(out_rx).await;

    // Exactly one terminal Completed (one_completed asserts uniqueness) whose
    // payload is the RETRY's — the rejected attempt is suppressed from the
    // surfaced result, not rendered beside it.
    let result = one_completed(&evs);
    assert_eq!(result, valid, "the surfaced recap is the valid retry only");
    assert!(
        !result.to_string().contains("REJECTED-SENTINEL"),
        "rejected payload must not leak into the surfaced result: {result}"
    );
    assert!(
        !evs.iter()
            .any(|e| matches!(e, SubagentEvent::Failed { .. })),
        "a recovered retry is not a failure; got: {evs:?}"
    );
}

/// Repeated schema-invalid StructuredOutput calls exhaust the retry cap (5)
/// and abort with the byte-exact retry-cap-exceeded message.
#[tokio::test]
async fn schema_retry_cap_exceeded_aborts() {
    let bad_so = || llm_runtime::HistoryResponse {
        content: vec![llm_runtime::ContentBlock::ToolCall {
            input_projection: None,
            id: ToolUseId::new().to_string(),
            name: "StructuredOutput".into(),
            input: serde_json::json!({ "answer": "still-wrong" }),
        }],
        ..tool_use_response("StructuredOutput", Some("tool_use"))
    };
    // 5 failing calls → kn reaches the default cap (5) → abort.
    let api = MockSubagentApiClient::new((0..5).map(|_| Ok(bad_so())).collect());
    let invoker = CountingInvoker::new();
    let mut ctx = loop_ctx(api, Some(invoker), 10);
    ctx.schema =
        Some(r#"{"type":"object","properties":{"answer":{"type":"integer"}}}"#.to_string());
    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    // Five invalid schema turns each surface multiple non-terminal events
    // before the runner returns. Keep the fixture from back-pressuring the
    // runner while this synchronous test waits to drain after completion.
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(32);
    run_subagent(ctx, event_rx, out_tx).await;
    let evs = drain(out_rx).await;
    let err = evs
        .iter()
        .find_map(|e| match e {
            SubagentEvent::Failed { error, .. } => Some(error.clone()),
            _ => None,
        })
        .expect("a Failed event");
    assert_eq!(
        err,
        "agent({schema}): StructuredOutput retry cap (5) exceeded \u{2014} 5 failed calls with no valid output"
    );
}

/// When the model ends turns WITHOUT calling StructuredOutput, the runner
/// nudges up to 2 times then aborts with the byte-exact "completed without
/// calling" message.
#[tokio::test]
async fn schema_no_call_nudges_twice_then_aborts() {
    // 3 end_turn text turns: turn 1 → nudge, turn 2 → nudge, turn 3 → abort.
    let api = MockSubagentApiClient::new(vec![
        Ok(text_response("no tool here", Some("end_turn"))),
        Ok(text_response("still none", Some("end_turn"))),
        Ok(text_response("nope", Some("end_turn"))),
    ]);
    let invoker = CountingInvoker::new();
    let api2 = api.clone();
    let mut ctx = loop_ctx(api, Some(invoker), 10);
    ctx.schema = Some(r#"{"type":"object"}"#.to_string());
    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    run_subagent(ctx, event_rx, out_tx).await;
    let evs = drain(out_rx).await;
    let err = evs
        .iter()
        .find_map(|e| match e {
            SubagentEvent::Failed { error, .. } => Some(error.clone()),
            _ => None,
        })
        .expect("a Failed event");
    assert_eq!(
        err,
        "agent({schema}): subagent completed without calling StructuredOutput (after in-conversation nudge)"
    );
    // 3 round-trips: the original turn + 2 nudge re-runs.
    assert_eq!(api2.call_count(), 3);
}

/// Records each complete request's tool choice and conversation history.
struct StructuredOutputModeCapturingApiClient {
    responses: Mutex<VecDeque<llm_runtime::HistoryResponse>>,
    forced_calls: Mutex<Vec<bool>>,
    /// The `messages` argument of every round-trip, in call order — lets a
    /// test inspect the REQUEST shape the runner built for a given turn
    /// (e.g. whether its last message is user- or assistant-authored), not
    /// just whether `tool_choice` was forced.
    messages_seen: Mutex<Vec<Vec<ConversationMessage>>>,
}

impl StructuredOutputModeCapturingApiClient {
    fn new(responses: Vec<llm_runtime::HistoryResponse>) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(responses.into_iter().collect()),
            forced_calls: Mutex::new(Vec::new()),
            messages_seen: Mutex::new(Vec::new()),
        })
    }

    fn forced_calls(&self) -> Vec<bool> {
        self.forced_calls.lock().unwrap().clone()
    }

    fn messages_seen(&self) -> Vec<Vec<ConversationMessage>> {
        self.messages_seen.lock().unwrap().clone()
    }

    fn next_stream(
        &self,
    ) -> Result<
        futures::stream::BoxStream<
            'static,
            Result<llm_runtime::HistoryEvent, llm_runtime::LlmError>,
        >,
        llm_runtime::LlmError,
    > {
        use futures::StreamExt;
        let resp = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| text_response("(exhausted)", Some("end_turn")));
        let events = llm_runtime::stream_accumulator::response_to_stream_events(resp);
        Ok(futures::stream::iter(events.into_iter().map(Ok)).boxed())
    }
}

#[async_trait]
impl crate::api::SubagentApiClient for StructuredOutputModeCapturingApiClient {
    async fn stream(
        &self,
        request: crate::api::SubagentApiRequest,
    ) -> Result<
        futures::stream::BoxStream<
            'static,
            Result<llm_runtime::HistoryEvent, llm_runtime::LlmError>,
        >,
        llm_runtime::LlmError,
    > {
        self.forced_calls
            .lock()
            .unwrap()
            .push(request.forced_tool.is_some());
        self.messages_seen.lock().unwrap().push(request.messages);
        self.next_stream()
    }
}

/// Build an `HistoryResponse` carrying a valid `StructuredOutput` tool call.
fn structured_output_call_response(input: serde_json::Value) -> llm_runtime::HistoryResponse {
    llm_runtime::HistoryResponse {
        content: vec![llm_runtime::ContentBlock::ToolCall {
            input_projection: None,
            id: ToolUseId::new().to_string(),
            name: "StructuredOutput".into(),
            input,
        }],
        ..tool_use_response("StructuredOutput", Some("tool_use"))
    }
}

/// WP2a item 1 / `StructuredOutputMode::WhenDone`: while tools remain and the
/// run is not on its last turn, the runner uses AUTO `tool_choice` (the model
/// is free to call its other tools); only the LAST turn forces
/// `StructuredOutput`. Turns 1-2 call the ordinary `Read` tool (dispatched,
/// keeping the loop going); turn 3 (the last, `max_turns == 3`) is forced and
/// returns a schema-valid `StructuredOutput` call.
#[tokio::test]
async fn when_done_uses_auto_tool_choice_until_the_last_turn() {
    let structured = serde_json::json!({ "answer": 1 });
    let api = StructuredOutputModeCapturingApiClient::new(vec![
        tool_use_response("Read", Some("tool_use")),
        tool_use_response("Read", Some("tool_use")),
        structured_output_call_response(structured.clone()),
    ]);
    let invoker = CountingInvoker::new();
    let mut ctx = loop_ctx(api.clone(), Some(invoker.clone()), 3);
    ctx.schema = Some(r#"{"type":"object"}"#.to_string());
    ctx.structured_output_mode = lingxi_core::host::subagent_spawn::StructuredOutputMode::WhenDone;
    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    run_subagent(ctx, event_rx, out_tx).await;
    let evs = drain(out_rx).await;
    assert_eq!(
        one_completed(&evs),
        structured,
        "the forced final turn's StructuredOutput call is the result"
    );
    assert_eq!(
        api.forced_calls(),
        vec![false, false, true],
        "WhenDone forces tool_choice only on the last turn"
    );
    assert_eq!(
        invoker.call_count(),
        2,
        "the two non-final turns actually dispatched Read (auto tool_choice let the model use it)"
    );
}

/// Round-3 review items 3/7 regression test. Every REAL Fusion panel
/// advertises more than one tool (`fusion_panel_definition`:
/// `Read`/`Grep`/`Glob`/`WebFetch` plus the injected `StructuredOutput`) —
/// a shape `when_done_uses_auto_tool_choice_until_the_last_turn` above never
/// exercises, since it builds on `tool_schemas: vec![]` and StructuredOutput
/// ends up the sole advertised tool. Before the round-3 fix, the runner's
/// wire gate ANDed `force_this_turn` with the P0-1 single-tool check
/// (`force_tool_choice_for_api.is_some()`, `true` only when
/// `tool_schemas.len() == 1`), so with `Read`/`Grep` also advertised that
/// second conjunct was permanently `false` and WhenDone's last-turn force
/// never reached the wire for any panel — turn 3 here would have gone out
/// with `tool_choice` unpinned. Reproduces the panel shape directly: two
/// tools stay advertised across all three turns; turns 1-2 dispatch `Read`
/// under AUTO tool_choice, and turn 3 (the last) MUST be forced.
#[tokio::test]
async fn when_done_forces_the_last_turn_even_with_other_tools_still_advertised() {
    let structured = serde_json::json!({ "answer": 9 });
    let api = StructuredOutputModeCapturingApiClient::new(vec![
        tool_use_response("Read", Some("tool_use")),
        tool_use_response("Read", Some("tool_use")),
        structured_output_call_response(structured.clone()),
    ]);
    let invoker = CountingInvoker::new();
    let mut ctx = loop_ctx(api.clone(), Some(invoker.clone()), 3);
    ctx.schema = Some(r#"{"type":"object"}"#.to_string());
    ctx.structured_output_mode = lingxi_core::host::subagent_spawn::StructuredOutputMode::WhenDone;
    // The exact shape every real Fusion panel spawns with: MORE than one
    // tool advertised alongside the injected `StructuredOutput`, so
    // `tool_schemas.len() != 1` and the P0-1 `force_tool_choice_for_api`
    // gate is `None` for the whole run.
    ctx.tool_schemas = vec![
        serde_json::json!({"name": "Read", "input_schema": {"type": "object"}}),
        serde_json::json!({"name": "Grep", "input_schema": {"type": "object"}}),
    ];
    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    run_subagent(ctx, event_rx, out_tx).await;
    let evs = drain(out_rx).await;
    assert_eq!(
        one_completed(&evs),
        structured,
        "the forced final turn's StructuredOutput call must be the result \
         even with Read/Grep still advertised — a real panel that answers \
         in prose on the unforced last turn ends Failed with no report"
    );
    assert_eq!(
        api.forced_calls(),
        vec![false, false, true],
        "WhenDone must pin tool_choice on the last turn regardless of how \
         many other tools are advertised — this is exactly the >1-tool \
         configuration every real Fusion panel runs in"
    );
    assert_eq!(
        invoker.call_count(),
        2,
        "the two non-final turns still dispatch Read under auto tool_choice"
    );
}

/// WP2a item 1 / `StructuredOutputMode::WhenDone`: once the model has
/// produced TWO consecutive turns with no tool call and no `StructuredOutput`
/// call, the runner forces `StructuredOutput` on the very next turn — even
/// though `max_turns` (5) is far from exhausted.
#[tokio::test]
async fn when_done_forces_after_two_consecutive_idle_turns() {
    let structured = serde_json::json!({ "answer": 2 });
    let api = StructuredOutputModeCapturingApiClient::new(vec![
        text_response("thinking...", Some("end_turn")),
        text_response("still thinking...", Some("end_turn")),
        structured_output_call_response(structured.clone()),
    ]);
    let mut ctx = loop_ctx(api.clone(), Some(CountingInvoker::new()), 5);
    ctx.schema = Some(r#"{"type":"object"}"#.to_string());
    ctx.structured_output_mode = lingxi_core::host::subagent_spawn::StructuredOutputMode::WhenDone;
    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    run_subagent(ctx, event_rx, out_tx).await;
    let evs = drain(out_rx).await;
    assert_eq!(
        one_completed(&evs),
        structured,
        "the idle-triggered forced turn's StructuredOutput call is the result"
    );
    assert_eq!(
        api.forced_calls(),
        vec![false, false, true],
        "two idle (no-tool-call) turns force the third turn, well short of max_turns (5)"
    );
}

/// `StructuredOutputMode::WhenDone`: an idle (text-only, non-forced) turn
/// that loops back must APPEND a user message to `history` before doing so.
/// Every other exit from the "no tool call" arm pushes a user message first
/// (the nudge under the forced-turn escalation, or a dispatched tool's
/// tool_results); this is the one path that used to `continue` with nothing
/// appended, which would send the NEXT round-trip a request whose last
/// message is assistant-authored — a prefill continuation rather than a
/// fresh turn, and something a real provider can reject outright. Turn 1 is
/// idle (`end_turn`, no tool call, not forced since it's neither the last
/// turn nor two-idle-turns-deep); turn 2 must see a freshly appended user
/// message as the LAST message of its request.
#[tokio::test]
async fn when_done_idle_turn_appends_a_user_message_before_looping_back() {
    let structured = serde_json::json!({ "answer": 5 });
    let api = StructuredOutputModeCapturingApiClient::new(vec![
        text_response("thinking...", Some("end_turn")),
        structured_output_call_response(structured.clone()),
    ]);
    let mut ctx = loop_ctx(api.clone(), Some(CountingInvoker::new()), 5);
    ctx.schema = Some(r#"{"type":"object"}"#.to_string());
    ctx.structured_output_mode = lingxi_core::host::subagent_spawn::StructuredOutputMode::WhenDone;
    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    run_subagent(ctx, event_rx, out_tx).await;
    let evs = drain(out_rx).await;
    assert_eq!(
        one_completed(&evs),
        structured,
        "the second (forced) turn's StructuredOutput call is the result"
    );
    let calls = api.messages_seen();
    assert_eq!(
        calls.len(),
        2,
        "one idle turn followed by one forced turn: forced_calls={:?}",
        api.forced_calls()
    );
    let second_call_last = calls[1]
        .last()
        .expect("the second call's request must carry at least one message");
    assert!(
        matches!(second_call_last, ConversationMessage::User { .. }),
        "the second call's LAST message must be user-authored, not the bare \
         first-turn assistant reply: {second_call_last:?}"
    );
}

/// `StructuredOutputMode::Forced` (the default) is byte-identical to the
/// pre-WP2a behavior: EVERY turn is forced, even the first.
#[tokio::test]
async fn forced_mode_forces_every_turn_including_the_first() {
    let structured = serde_json::json!({ "answer": 3 });
    let api = StructuredOutputModeCapturingApiClient::new(vec![structured_output_call_response(
        structured.clone(),
    )]);
    let mut ctx = loop_ctx(api.clone(), Some(CountingInvoker::new()), 4);
    ctx.schema = Some(r#"{"type":"object"}"#.to_string());
    // `Forced` is also `SubagentContext::structured_output_mode`'s default —
    // set explicitly here so the test documents the invariant rather than
    // relying on the struct's field order.
    ctx.structured_output_mode = lingxi_core::host::subagent_spawn::StructuredOutputMode::Forced;
    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    run_subagent(ctx, event_rx, out_tx).await;
    let evs = drain(out_rx).await;
    assert_eq!(one_completed(&evs), structured);
    assert_eq!(api.forced_calls(), vec![true]);
}

// ---- P0-1 (2026-09-02): a schema subagent must still be able to call
// tools — the wire `tool_choice` must NOT be pinned to `StructuredOutput` on
// every round when other tools are advertised. See
// `<scratchpad>/WP3-oracle.txt` for the claude-code 2.1.258 oracle evidence
// this is verified against (the shared subagent turn loop hardcodes
// `toolChoice: void 0` and enforces StructuredOutput purely via a repeated
// in-conversation nudge, never via `tool_choice`). ------------------------

/// `SubagentApiClient` that drives the runner exclusively through the
/// `_opts` streaming seam (the one `run_subagent_loop` actually calls) and
/// records, per round-trip, whether it was called through the FORCED variant
/// (`Some(forced_tool)`) or the plain one (`None`) — a direct proxy for what
/// `tool_choice` the wire request would have carried. Also records the
/// message history each round was given, so a test can assert the nudge
/// text rides on a specific round-trip.
struct RecordingForceApiClient {
    responses: Mutex<VecDeque<Result<llm_runtime::HistoryResponse, llm_runtime::LlmError>>>,
    forced_tools: Mutex<Vec<Option<String>>>,
    messages_per_call: Mutex<Vec<Vec<ConversationMessage>>>,
}

impl RecordingForceApiClient {
    fn new(
        responses: Vec<Result<llm_runtime::HistoryResponse, llm_runtime::LlmError>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(responses.into_iter().collect()),
            forced_tools: Mutex::new(Vec::new()),
            messages_per_call: Mutex::new(Vec::new()),
        })
    }

    fn forced_tools(&self) -> Vec<Option<String>> {
        self.forced_tools.lock().unwrap().clone()
    }

    fn messages_per_call(&self) -> Vec<Vec<ConversationMessage>> {
        self.messages_per_call.lock().unwrap().clone()
    }

    fn next_response(&self) -> Result<llm_runtime::HistoryResponse, llm_runtime::LlmError> {
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Ok(text_response("(exhausted)", Some("end_turn"))))
    }

    fn record_and_stream(
        &self,
        messages: Vec<ConversationMessage>,
        forced_tool: Option<&str>,
    ) -> Result<
        futures::stream::BoxStream<
            'static,
            Result<llm_runtime::HistoryEvent, llm_runtime::LlmError>,
        >,
        llm_runtime::LlmError,
    > {
        use futures::StreamExt;
        self.forced_tools
            .lock()
            .unwrap()
            .push(forced_tool.map(str::to_string));
        self.messages_per_call.lock().unwrap().push(messages);
        let resp = self.next_response()?;
        let events = llm_runtime::stream_accumulator::response_to_stream_events(resp);
        Ok(futures::stream::iter(events.into_iter().map(Ok)).boxed())
    }
}

#[async_trait]
impl crate::api::SubagentApiClient for RecordingForceApiClient {
    async fn stream(
        &self,
        request: crate::api::SubagentApiRequest,
    ) -> Result<
        futures::stream::BoxStream<
            'static,
            Result<llm_runtime::HistoryEvent, llm_runtime::LlmError>,
        >,
        llm_runtime::LlmError,
    > {
        self.record_and_stream(request.messages, request.forced_tool.as_deref())
    }
}

/// (a) + (b) + (d): a schema run with another tool ALSO advertised must
/// never pin `tool_choice` — not on round 1 (other tools available), and
/// NOT on the round immediately after a turn that ended without a valid
/// StructuredOutput call either (verified oracle behavior: enforcement is
/// the in-conversation nudge alone, never a wire-level restriction). The
/// structured result is still captured and schema-validated once produced.
#[tokio::test]
async fn schema_with_other_tools_never_pins_tool_choice_even_after_a_nudge() {
    let structured = serde_json::json!({ "answer": 42 });
    let api = RecordingForceApiClient::new(vec![
        // Round 1: the model uses an unrelated tool — only possible at all
        // if round 1 was NOT forced to StructuredOutput.
        Ok(tool_use_response("OtherTool", Some("tool_use"))),
        // Round 2: the model ends its turn without calling StructuredOutput
        // at all — triggers the in-conversation nudge.
        Ok(text_response("still thinking", Some("end_turn"))),
        // Round 3 (post-nudge): the model finally calls StructuredOutput.
        Ok(llm_runtime::HistoryResponse {
            content: vec![llm_runtime::ContentBlock::ToolCall {
                input_projection: None,
                id: ToolUseId::new().to_string(),
                name: "StructuredOutput".into(),
                input: structured.clone(),
            }],
            ..tool_use_response("StructuredOutput", Some("tool_use"))
        }),
    ]);
    let invoker = CountingInvoker::new();
    let mut ctx = loop_ctx(api.clone(), Some(invoker.clone()), 6);
    // A non-empty `tool_schemas` means the injected StructuredOutput tool is
    // NOT the only tool in the registry.
    ctx.tool_schemas = vec![serde_json::json!({
        "name": "OtherTool",
        "description": "an unrelated tool the schema run must still be able to call",
        "input_schema": {"type": "object"}
    })];
    ctx.schema = Some(
        r#"{"type":"object","required":["answer"],"properties":{"answer":{"type":"integer"}}}"#
            .to_string(),
    );
    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    run_subagent(ctx, event_rx, out_tx).await;
    let evs = drain(out_rx).await;

    // (d) unchanged capture + schema validation.
    assert_eq!(one_completed(&evs), structured);

    // Precondition, not the gate: the run really did reach the other tool's
    // dispatch path. (This one line is polarity-neutral — the canned response
    // names `OtherTool` whether or not the round was forced; the
    // `forced_tools()` assertion below is what flips.)
    assert_eq!(
        invoker.call_count(),
        1,
        "OtherTool must have been dispatched"
    );
    // (a) no round is forced while another tool is advertised.
    let forced = api.forced_tools();
    assert_eq!(
        forced,
        vec![None, None, None],
        "tool_choice must stay unpinned on every round while another tool is \
         advertised, including the round right after a no-call nudge — got {forced:?}"
    );

    // (b, reinterpreted per verified oracle behavior): the nudge text rides
    // as a plain conversational message on round 3's request, NOT a
    // tool_choice restriction.
    let round3 = &api.messages_per_call()[2];
    let nudged = round3.iter().any(|m| {
        matches!(m, ConversationMessage::User { content, .. }
            if content.iter().any(|b| matches!(b, ContentBlock::Text { text, .. }
                if text.contains("You did not call StructuredOutput"))))
    });
    assert!(
        nudged,
        "round 3 must carry the in-conversation nudge from round 2's no-call turn: {round3:?}"
    );
}

/// (c): when the registry has NOTHING but the synthetic StructuredOutput
/// tool, the first round is forced immediately (nothing else the model
/// could usefully call, so pinning it removes a redundant no-call round-trip
/// without changing observable model behavior).
#[tokio::test]
async fn schema_only_tool_in_registry_forces_from_round_one() {
    let structured = serde_json::json!({ "answer": 7 });
    let api = RecordingForceApiClient::new(vec![Ok(llm_runtime::HistoryResponse {
        content: vec![llm_runtime::ContentBlock::ToolCall {
            input_projection: None,
            id: ToolUseId::new().to_string(),
            name: "StructuredOutput".into(),
            input: structured.clone(),
        }],
        ..tool_use_response("StructuredOutput", Some("tool_use"))
    })]);
    let mut ctx = loop_ctx(api.clone(), Some(CountingInvoker::new()), 4);
    // No pre-existing tools: after injection the registry holds ONLY the
    // synthetic StructuredOutput tool.
    ctx.tool_schemas = vec![];
    ctx.schema = Some(
        r#"{"type":"object","required":["answer"],"properties":{"answer":{"type":"integer"}}}"#
            .to_string(),
    );
    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    run_subagent(ctx, event_rx, out_tx).await;
    let evs = drain(out_rx).await;

    assert_eq!(one_completed(&evs), structured, "(d) capture still works");
    assert_eq!(
        api.forced_tools(),
        vec![Some("StructuredOutput".to_string())],
        "the sole-tool registry must force StructuredOutput from round 1"
    );
}

/// Companion to the relaxation above: once the model is free to keep calling
/// other tools, a schema run CAN reach `max_turns` without ever producing a
/// structured result — an exit that was unreachable while `tool_choice` was
/// pinned every round. That exit must still honour the schema contract and
/// fail, not resolve the caller's `agent()` with the off-schema
/// `{"reason":"max_turns_exhausted"}` completion payload. Oracle 2.1.258
/// @172635430 checks `structured === undefined` after the whole attempt ends,
/// for any exit reason, and throws this exact error.
#[tokio::test]
async fn schema_run_exhausting_max_turns_fails_instead_of_completing_off_schema() {
    // Every round the model calls the unrelated tool and never StructuredOutput;
    // `max_turns` (3) is reached with `should_continue` still true each time.
    let api = RecordingForceApiClient::new(vec![
        Ok(tool_use_response("OtherTool", Some("tool_use"))),
        Ok(tool_use_response("OtherTool", Some("tool_use"))),
        Ok(tool_use_response("OtherTool", Some("tool_use"))),
    ]);
    let invoker = CountingInvoker::new();
    let mut ctx = loop_ctx(api.clone(), Some(invoker.clone()), 3);
    ctx.tool_schemas = vec![serde_json::json!({
        "name": "OtherTool",
        "description": "an unrelated tool the schema run keeps calling",
        "input_schema": {"type": "object"}
    })];
    ctx.schema = Some(
        r#"{"type":"object","required":["answer"],"properties":{"answer":{"type":"integer"}}}"#
            .to_string(),
    );
    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    run_subagent(ctx, event_rx, out_tx).await;
    let evs = drain(out_rx).await;

    // The precondition this test exists for: the run really did burn all 3
    // turns on the other tool rather than ending early.
    assert_eq!(api.forced_tools().len(), 3, "3 round-trips expected");
    assert_eq!(invoker.call_count(), 3, "OtherTool dispatched every turn");

    let completed: Vec<&serde_json::Value> = evs
        .iter()
        .filter_map(|e| match e {
            SubagentEvent::Completed { result, .. } => Some(result),
            _ => None,
        })
        .collect();
    assert!(
        completed.is_empty(),
        "a schema run must not report success with an off-schema payload \u{2014} got {completed:?}"
    );
    let err = evs
        .iter()
        .find_map(|e| match e {
            SubagentEvent::Failed { error, .. } => Some(error.clone()),
            _ => None,
        })
        .expect("a Failed event naming the missing StructuredOutput");
    assert_eq!(
        err,
        "agent({schema}): subagent completed without calling StructuredOutput (after in-conversation nudge)"
    );
}

#[test]
fn structured_output_validation_and_cap_helpers() {
    // Valid input passes; type-mismatch fails with a leaf message.
    let schema = r#"{"type":"object","required":["n"],"properties":{"n":{"type":"integer"}}}"#;
    assert!(validate_structured_output(Some(schema), &serde_json::json!({"n":1})).is_ok());
    assert!(validate_structured_output(Some(schema), &serde_json::json!({"n":"x"})).is_err());
    // No schema ⇒ always Ok. A malformed schema ⇒ Ok (LingXi schema bug,
    // not a model error).
    assert!(validate_structured_output(None, &serde_json::json!({})).is_ok());
    assert!(validate_structured_output(Some("{ not json"), &serde_json::json!({})).is_ok());
    // Default cap is 5 (claude `OBp`).
    assert_eq!(structured_output_retry_cap(), 5);
}

#[test]
fn local_app_operator_schema_requires_complete_host_qa_projection() {
    let document: serde_json::Value = serde_json::from_str(include_str!(
        "../../plugins/lingxi-local-app/schemas/workflow-agent-results.schema.json"
    ))
    .expect("parse checked-in Local App workflow role schemas");
    let mut schema = document["$defs"]["operator_result"].clone();
    schema["$defs"] = document["$defs"].clone();
    let schema = serde_json::to_string(&schema).expect("serialize operator role schema");

    let complete = serde_json::json!({
        "ok": true,
        "qa_handle": "qa_00000000000000000000000000000000",
        "evidence_ids": ["evidence-1"],
        "status": "evidence_collected",
        "issues": [],
        "summary": "Host evidence collected",
        "verification_scope": {
            "declared_target_ids": ["primary", "ipad"],
            "in_scope_target_ids": ["primary"],
            "unverified_target_ids": ["ipad"],
            "unverified_scenario_ids": ["ipad-layout"]
        },
        "upstream_failures": [{
            "id": "source:save",
            "message": "save did not persist",
            "introduced_at_ms": 10
        }],
        "upstream_findings": [{
            "id": "source:save",
            "message": "save did not persist",
            "blocking": true,
            "resolved_by_evidence_ids": []
        }]
    });
    assert!(
        validate_structured_output(Some(&schema), &complete).is_ok(),
        "the complete Host QaBegin projection must satisfy the production validator"
    );

    let mut missing_scope = complete.clone();
    missing_scope
        .as_object_mut()
        .expect("operator result object")
        .remove("verification_scope");
    assert!(
        validate_structured_output(Some(&schema), &missing_scope).is_err(),
        "operator output without canonical Host scope must fail closed"
    );

    for ledger_field in ["upstream_failures", "upstream_findings"] {
        let mut missing_ledger = complete.clone();
        missing_ledger
            .as_object_mut()
            .expect("operator result object")
            .remove(ledger_field);
        assert!(
            validate_structured_output(Some(&schema), &missing_ledger).is_err(),
            "operator output without {ledger_field} must fail closed"
        );
    }

    let mut incomplete_finding = complete;
    incomplete_finding["upstream_findings"][0]
        .as_object_mut()
        .expect("upstream finding object")
        .remove("resolved_by_evidence_ids");
    assert!(
        validate_structured_output(Some(&schema), &incomplete_finding).is_err(),
        "Host upstream finding projection must include resolution evidence ids"
    );
}

#[tokio::test]
async fn loop_g1_completed_carries_final_turn_usage_and_tool_count() {
    // G1: the terminal Completed event carries the FINAL turn's usage (claude
    // reads only the last message usage, not a cross-turn sum) plus the
    // run-wide tool-use count. Turn 1: tool_use with usage A (dispatched).
    // Turn 2: end_turn text with usage B → carried usage == B; tool_uses == 1.
    let usage_a = llm_runtime::ExecutionUsage::from_counts(llm_runtime::Usage {
        input_tokens: 1000,
        output_tokens: 1,
        ..Default::default()
    });
    let usage_b = llm_runtime::ExecutionUsage::from_counts(llm_runtime::Usage {
        input_tokens: 10,
        output_tokens: 5,
        cache_write_tokens: 3,
        cache_read_tokens: 2,
        ..Default::default()
    });
    let api = MockSubagentApiClient::new(vec![
        Ok(tool_use_response_with_usage(
            "Read",
            Some("tool_use"),
            usage_a,
        )),
        Ok(llm_runtime::HistoryResponse {
            usage: usage_b.clone(),
            ..text_response("done", Some("end_turn"))
        }),
    ]);
    let invoker = CountingInvoker::new();
    let ctx = loop_ctx(api.clone(), Some(invoker.clone()), 4);
    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    run_subagent(ctx, event_rx, out_tx).await;
    let evs = drain(out_rx).await;
    let (usage, tool_count) = evs
        .iter()
        .find_map(|e| match e {
            SubagentEvent::Completed {
                usage,
                total_tool_use_count,
                ..
            } => Some((usage.clone(), *total_tool_use_count)),
            _ => None,
        })
        .expect("one Completed");
    // The carried usage is the FINAL turn's (B), NOT a sum with A.
    assert_eq!(
        usage.counts().input_tokens,
        10,
        "final-turn input, not summed"
    );
    assert_eq!(
        usage
            .counts()
            .output_tokens
            .saturating_sub(usage.counts().reasoning_tokens),
        5
    );
    assert_eq!(usage.counts().cache_write_tokens, 3);
    assert_eq!(usage.counts().cache_read_tokens, 2);
    // One tool_use across the run (turn 1).
    assert_eq!(tool_count, 1, "run-wide tool-use count");
}

/// WP2a item 2: the runner no longer clamps REPORTED usage against
/// `max_output_tokens_per_turn` — that ceiling reaches the provider on the
/// WIRE (via `SubagentApiCallOpts::max_output_tokens`, asserted in
/// `orchestrator::provider_adapter`'s own tests), and the terminal `Completed`
/// carries the provider's REAL usage even when it exceeds the requested cap
/// (a provider can still overrun its own advertised ceiling).
#[tokio::test]
async fn loop_completed_cumulative_usage_sums_turns_and_reports_real_uncapped_output() {
    let usage_a = llm_runtime::ExecutionUsage::from_counts(llm_runtime::Usage {
        input_tokens: 1000,
        output_tokens: 40,
        ..Default::default()
    });
    let usage_b = llm_runtime::ExecutionUsage::from_counts(llm_runtime::Usage {
        input_tokens: 10,
        output_tokens: 50,
        cache_write_tokens: 3,
        cache_read_tokens: 2,
        ..Default::default()
    });
    let api = MockSubagentApiClient::new(vec![
        Ok(tool_use_response_with_usage(
            "Read",
            Some("tool_use"),
            usage_a,
        )),
        Ok(llm_runtime::HistoryResponse {
            usage: usage_b.clone(),
            ..text_response("done", Some("end_turn"))
        }),
    ]);
    let invoker = CountingInvoker::new();
    let mut ctx = loop_ctx(api.clone(), Some(invoker.clone()), 4);
    ctx.max_output_tokens_per_turn = Some(8);
    ctx.max_input_bytes_per_turn = Some(4096);
    // Keep the cap active while leaving both mandatory seed units safely
    // representable; strict over-cap rejection is covered by the focused
    // cap helper tests below.
    ctx.prompt_messages = vec![
        ConversationMessage::user(MessageId::new(), "keep-me".into()),
        ConversationMessage::user(MessageId::new(), "x".repeat(200)),
    ];
    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    run_subagent(ctx, event_rx, out_tx).await;
    let evs = drain(out_rx).await;
    let (usage, cumulative) = evs
        .iter()
        .find_map(|e| match e {
            SubagentEvent::Completed {
                usage,
                cumulative_usage,
                ..
            } => Some((usage.clone(), cumulative_usage.clone())),
            _ => None,
        })
        .expect("one Completed");
    assert_eq!(
        usage
            .counts()
            .output_tokens
            .saturating_sub(usage.counts().reasoning_tokens),
        50,
        "final-turn output is the provider's REAL usage, not clamped to the \
         requested per-turn ceiling (8)"
    );
    assert_ne!(
        usage.counts().input_tokens,
        cumulative.counts().input_tokens,
        "cumulative input must include earlier turns"
    );
    assert_eq!(cumulative.counts().input_tokens, 1010);
    assert_eq!(
        cumulative
            .counts()
            .output_tokens
            .saturating_sub(cumulative.counts().reasoning_tokens),
        90,
        "real 40 + real 50, uncapped by max_output_tokens_per_turn"
    );
    let progress_tokens: Vec<u64> = evs
        .iter()
        .filter_map(|event| match event {
            SubagentEvent::Progress { token_count, .. } => Some(*token_count),
            _ => None,
        })
        .collect();
    assert_eq!(
        progress_tokens.last().copied(),
        Some(1105),
        "progress must expose the cross-turn cumulative token count"
    );
    let last = api.last_messages();
    let joined = format!("{last:?}");
    assert!(
        joined.contains(&"x".repeat(200)),
        "the second seed unit fits the active cap and must remain in the API call"
    );
    assert!(
        serde_json::to_vec(&last).unwrap().len() <= 4096,
        "the request must still honor max_input_bytes_per_turn"
    );
    assert!(
        joined.contains("keep-me"),
        "the pinned head unit must survive trimming: {joined}"
    );
    assert!(!last.is_empty(), "at least the newest message is retained");
}

/// Records registered call options on each typed streaming request.
struct OptsOnlyCapturingApiClient {
    responses: Mutex<VecDeque<llm_runtime::HistoryResponse>>,
    opts_seen: Mutex<Vec<crate::api::SubagentApiCallOpts>>,
}

impl OptsOnlyCapturingApiClient {
    fn new(responses: Vec<llm_runtime::HistoryResponse>) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(responses.into_iter().collect()),
            opts_seen: Mutex::new(Vec::new()),
        })
    }

    fn opts_seen(&self) -> Vec<crate::api::SubagentApiCallOpts> {
        self.opts_seen.lock().unwrap().clone()
    }

    fn record_and_stream(
        &self,
        opts: crate::api::SubagentApiCallOpts,
    ) -> Result<
        futures::stream::BoxStream<
            'static,
            Result<llm_runtime::HistoryEvent, llm_runtime::LlmError>,
        >,
        llm_runtime::LlmError,
    > {
        use futures::StreamExt;
        self.opts_seen.lock().unwrap().push(opts);
        let resp = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| text_response("(exhausted)", Some("end_turn")));
        let events = llm_runtime::stream_accumulator::response_to_stream_events(resp);
        Ok(futures::stream::iter(events.into_iter().map(Ok)).boxed())
    }
}

#[async_trait]
impl crate::api::SubagentApiClient for OptsOnlyCapturingApiClient {
    async fn stream(
        &self,
        request: crate::api::SubagentApiRequest,
    ) -> Result<
        futures::stream::BoxStream<
            'static,
            Result<llm_runtime::HistoryEvent, llm_runtime::LlmError>,
        >,
        llm_runtime::LlmError,
    > {
        self.record_and_stream(request.opts)
    }
}

/// Registered Fusion rounds keep their capability while deriving fresh calls.
#[tokio::test]
async fn registered_panel_rounds_derive_fresh_calls_and_preserve_registration() {
    let api = OptsOnlyCapturingApiClient::new(vec![
        tool_use_response("Read", Some("tool_use")),
        text_response("done", Some("end_turn")),
    ]);
    let mut ctx = loop_ctx(api.clone(), Some(CountingInvoker::new()), 4);
    let registration = lingxi_core::host::ModelAttemptRun::new(Arc::new(()));
    let original = registration
        .context(lingxi_core::host::ModelAttemptStage::Panel, Some(2))
        .unwrap();
    ctx.model_attempt = Some(original.clone());
    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    run_subagent(ctx, event_rx, out_tx).await;
    assert!(drain(out_rx)
        .await
        .iter()
        .any(|event| matches!(event, SubagentEvent::Completed { .. })));
    let opts = api.opts_seen();
    assert_eq!(opts.len(), 2);
    let first = opts[0].model_attempt.as_ref().unwrap();
    let second = opts[1].model_attempt.as_ref().unwrap();
    assert_eq!(first.registration_id(), original.registration_id());
    assert_eq!(second.registration_id(), original.registration_id());
    assert_eq!(first.panel_slot(), Some(2));
    assert_ne!(first.logical_call_id(), second.logical_call_id());
}

#[tokio::test]
async fn workflow_watchdog_wrapper_threads_opts_to_the_inner_client() {
    let api = OptsOnlyCapturingApiClient::new(vec![
        tool_use_response("Read", Some("tool_use")),
        text_response("done", Some("end_turn")),
    ]);
    let wrapped: Arc<dyn crate::api::SubagentApiClient> =
        Arc::new(crate::api::WorkflowWatchdogApiClient::new(
            api.clone(),
            lingxi_core::host::WorkflowQueryWatchdog {
                stall_timeout_ms: 60_000,
                max_retries: 0,
                retry_response_body: false,
            },
            Vec::new(),
        ));
    let invoker = CountingInvoker::new();
    let mut ctx = loop_ctx(wrapped, Some(invoker.clone()), 4);
    ctx.max_output_tokens_per_turn = Some(4096);
    ctx.query_source_label = Some("fusion_panel".to_string());
    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    run_subagent(ctx, event_rx, out_tx).await;
    let evs = drain(out_rx).await;
    assert!(
        evs.iter()
            .any(|e| matches!(e, SubagentEvent::Completed { .. })),
        "the run must complete through the wrapper: {evs:?}"
    );
    let opts = api.opts_seen();
    assert_eq!(
        opts.len(),
        2,
        "both rounds must carry their registered call options to the inner client"
    );
    for (turn, o) in opts.iter().enumerate() {
        assert_eq!(
            o.max_output_tokens,
            Some(4096),
            "turn {turn}: WorkflowWatchdogApiClient dropped max_output_tokens"
        );
        assert_eq!(
            o.query_source_label.as_deref(),
            Some("fusion_panel"),
            "turn {turn}: WorkflowWatchdogApiClient dropped query_source_label"
        );
    }
}

#[tokio::test]
async fn loop_consumes_streaming_seam_end_to_end() {
    // The required stream returns actual decoded turn events. Turn 1
    // streams a `tool_use` (dispatched via the invoker); turn 2 streams the
    // final text. Asserts two streamed round-trips, one tool dispatch, and
    // that the accumulated turn carries the streamed text + stop_reason.
    let api = StreamingMockApiClient::new(vec![
        streamed_tool_use_turn("Read", "tool_use"),
        streamed_text_turn("streamed answer", "end_turn"),
    ]);
    let invoker = CountingInvoker::new();
    let ctx = loop_ctx(api.clone(), Some(invoker.clone()), 4);

    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    run_subagent(ctx, event_rx, out_tx).await;
    let evs = drain(out_rx).await;

    assert_eq!(api.call_count(), 2, "two streamed round-trips");
    assert_eq!(invoker.call_count(), 1, "streamed tool_use dispatched once");
    let result = one_completed(&evs);
    assert_eq!(result["text"], "streamed answer");
    assert_eq!(result["stop_reason"], "end_turn");
}

#[tokio::test]
async fn synchronous_child_dispatch_preserves_scheduled_headless_session_mode() {
    let api = StreamingMockApiClient::new(vec![
        streamed_tool_use_turn("Read", "tool_use"),
        streamed_text_turn("done", "end_turn"),
    ]);
    let invoker = SessionModeRecordingInvoker::new();
    let mut ctx = loop_ctx(api, Some(invoker.clone()), 4);
    ctx.is_async = false;
    ctx.session_interactive = Some(false);

    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    run_subagent(ctx, event_rx, out_tx).await;
    let _ = drain(out_rx).await;

    assert_eq!(
        *invoker.captured.lock().unwrap(),
        Some(true),
        "scheduled work must not become interactive at the child tool boundary"
    );
}

#[tokio::test]
async fn loop_advertises_context_tool_schemas_to_the_seam() {
    // `ctx.tool_schemas` must reach the typed request's `tools` field
    // on every round-trip (this is what lets the model emit `tool_use`).
    let api = StreamingMockApiClient::new(vec![streamed_text_turn("done", "end_turn")]);
    let mut ctx = loop_ctx(api.clone(), None, 4);
    let schemas = vec![serde_json::json!({
        "name": "Read",
        "description": "Reads a file.",
        "input_schema": {"type": "object"}
    })];
    ctx.tool_schemas = schemas.clone();

    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    run_subagent(ctx, event_rx, out_tx).await;
    let _ = drain(out_rx).await;

    assert_eq!(
        api.last_tools(),
        schemas,
        "ctx.tool_schemas must be forwarded verbatim to the streaming seam"
    );
}

#[tokio::test]
async fn schema_replaces_inherited_structured_output_tool() {
    let api =
        StreamingMockApiClient::new(vec![streamed_tool_use_turn("StructuredOutput", "tool_use")]);
    let mut ctx = loop_ctx(api.clone(), Some(CountingInvoker::new()), 4);
    ctx.tool_schemas = vec![
        serde_json::json!({
            "name": "Read",
            "description": "Reads a file.",
            "input_schema": {"type": "object"}
        }),
        serde_json::json!({
            "name": "StructuredOutput",
            "description": "Generic structured output inherited from the registry.",
            "input_schema": {
                "type": "object",
                "additionalProperties": true
            }
        }),
    ];
    let stage_schema = serde_json::json!({
        "type": "object",
        "required": ["ok"],
        "properties": {"ok": {"type": "boolean"}}
    });
    ctx.schema = Some(stage_schema.to_string());

    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    run_subagent(ctx, event_rx, out_tx).await;
    let _ = drain(out_rx).await;

    let tools = api.last_tools();
    let structured: Vec<&serde_json::Value> = tools
        .iter()
        .filter(|tool| tool["name"] == "StructuredOutput")
        .collect();
    assert_eq!(
        structured.len(),
        1,
        "the provider request must not contain duplicate tool names: {tools:?}"
    );
    assert_eq!(
        structured[0]["input_schema"], stage_schema,
        "the workflow stage schema must replace the inherited generic schema"
    );
}

#[tokio::test]
async fn loop_streaming_protocol_error_surfaces_failed() {
    // A streamed turn that ends without `message_stop` accumulates to
    // `LlmError::StreamInterrupted`, which the loop surfaces as Failed
    // (same path as a non-streaming api error).
    let truncated = vec![
        ev_message_start(),
        llm_runtime::HistoryEvent::ContentBlockStart {
            index: 0,
            content_block: llm_runtime::ContentBlock::Text {
                text: String::new(),
                cache_control: None,
                citations: None,
            },
        },
        // no content_block_stop, no message_stop
    ];
    let api = StreamingMockApiClient::new(vec![truncated]);
    let ctx = loop_ctx(api.clone(), None, 4);

    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    run_subagent(ctx, event_rx, out_tx).await;
    let evs = drain(out_rx).await;

    let failed = evs.iter().find_map(|e| match e {
        SubagentEvent::Failed { error, .. } => Some(error.clone()),
        _ => None,
    });
    let err = failed.expect("Failed on truncated stream");
    assert!(err.starts_with("subagent api error:"), "got: {err}");
}

#[tokio::test]
async fn loop_refuses_tool_outside_allowed_list_without_dispatching() {
    // allowed_tools = ["Bash"]; the model asks for "Read" → refused WITHOUT
    // dispatch (invoker never called), surfaced as an is_error ToolResult,
    // and the loop continues to a clean end_turn.
    let api = StreamingMockApiClient::new(vec![
        streamed_tool_use_turn("Read", "tool_use"),
        streamed_text_turn("done", "end_turn"),
    ]);
    let invoker = CountingInvoker::new();
    let mut ctx = loop_ctx(api.clone(), Some(invoker.clone()), 4);
    ctx.allowed_tools = vec!["Bash".to_string()];

    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    run_subagent(ctx, event_rx, out_tx).await;
    let evs = drain(out_rx).await;

    assert_eq!(
        invoker.call_count(),
        0,
        "a tool outside allowed_tools must NOT be dispatched"
    );
    // The refusal must be a structured is_error ToolResult carrying the
    // reason (so the model sees it like any tool error and can recover) —
    // deserialize the emitted Message rather than substring-matching.
    let refused = evs.iter().any(|e| {
        let SubagentEvent::Message { message, .. } = e else {
            return false;
        };
        let Ok(ConversationMessage::User { content, .. }) =
            serde_json::from_value::<ConversationMessage>(message.clone())
        else {
            return false;
        };
        content.iter().any(|b| {
            matches!(b, ContentBlock::ToolResult { is_error: Some(true), content, .. }
                if content.contains("not in this agent's allowed tools"))
        })
    });
    assert!(
        refused,
        "refusal must be an is_error ToolResult carrying the reason; got {evs:?}"
    );
    let result = one_completed(&evs);
    assert_eq!(result["stop_reason"], "end_turn");
}

// ── companion note (yyo / nke) tests ──────────────────────────────────

/// Unit test for `companion_note_for_disallowed_tool` (pure function, no
/// async). Verifies the `nke` set membership + the exact §7 note text.
#[test]
fn companion_note_returned_for_nke_tools() {
    // Tools unavailable in every subagent receive the guidance note.
    for name in &[
        "TaskOutput",
        "ExitPlanMode",
        "EnterPlanMode",
        "AskUserQuestion",
        "ConnectGitHub",
        "WaitForMcpServers",
        "ScheduleWakeup",
    ] {
        let note = companion_note_for_disallowed_tool(name);
        assert!(
            note.is_some(),
            "expected companion note for {name}; got None"
        );
        let note = note.unwrap();
        // Binary §7 verbatim: leading `. `, toolName interpolated.
        assert!(
            note.starts_with(". "),
            "note must start with \". \"; got {note:?}"
        );
        assert!(
            note.contains(name),
            "note must contain tool name {name:?}; got {note:?}"
        );
        assert!(
            note.contains("not available inside subagents"),
            "note must contain 'not available inside subagents'; got {note:?}"
        );
        assert!(
            note.contains("return findings to the orchestrator"),
            "note must contain 'return findings to the orchestrator'; got {note:?}"
        );
    }
}

#[test]
fn companion_note_absent_for_non_nke_tools() {
    // Regular tools must NOT get the companion note.
    for name in &["Read", "Bash", "Grep", "Glob", "Agent"] {
        assert!(
            companion_note_for_disallowed_tool(name).is_none(),
            "unexpected companion note for non-nke tool {name}"
        );
    }
}

#[test]
fn companion_note_workflow_follows_tool_permissions() {
    assert!(companion_note_for_disallowed_tool("Workflow").is_none());
    assert!(companion_note_for_disallowed_tool("TaskOutput").is_some());
}

#[tokio::test]
async fn loop_nke_tool_refusal_includes_companion_note() {
    // These tools are unavailable in every subagent.
    for nke_tool in &["TaskOutput", "AskUserQuestion"] {
        let api = StreamingMockApiClient::new(vec![
            streamed_tool_use_turn(nke_tool, "tool_use"),
            streamed_text_turn("done", "end_turn"),
        ]);
        let invoker = CountingInvoker::new();
        let mut ctx = loop_ctx(api.clone(), Some(invoker.clone()), 4);
        ctx.allowed_tools = vec!["Read".to_string()];

        let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
        let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
        run_subagent(ctx, event_rx, out_tx).await;
        let evs = drain(out_rx).await;

        let note_present = evs.iter().any(|e| {
            let SubagentEvent::Message { message, .. } = e else {
                return false;
            };
            let Ok(ConversationMessage::User { content, .. }) =
                serde_json::from_value::<ConversationMessage>(message.clone())
            else {
                return false;
            };
            content.iter().any(|b| {
                matches!(b,
                    ContentBlock::ToolResult { is_error: Some(true), content, .. }
                    if content.contains("not available inside subagents")
                        && content.contains(nke_tool)
                        && content.contains("return findings to the orchestrator")
                )
            })
        });
        assert!(
            note_present,
            "§7 companion note missing from refusal for nke tool {nke_tool}; got {evs:?}"
        );
    }
}

#[tokio::test]
async fn loop_non_nke_tool_refusal_has_no_companion_note() {
    // A non-nke tool (e.g. "Bash") blocked by the allow-list must NOT include
    // the companion note — the note is nke-specific.
    let api = StreamingMockApiClient::new(vec![
        streamed_tool_use_turn("Bash", "tool_use"),
        streamed_text_turn("done", "end_turn"),
    ]);
    let invoker = CountingInvoker::new();
    let mut ctx = loop_ctx(api.clone(), Some(invoker.clone()), 4);
    ctx.allowed_tools = vec!["Read".to_string()];

    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    run_subagent(ctx, event_rx, out_tx).await;
    let evs = drain(out_rx).await;

    let companion_note_absent = evs.iter().all(|e| {
        let SubagentEvent::Message { message, .. } = e else {
            return true;
        };
        let Ok(ConversationMessage::User { content, .. }) =
            serde_json::from_value::<ConversationMessage>(message.clone())
        else {
            return true;
        };
        content.iter().all(|b| {
            !matches!(b,
                ContentBlock::ToolResult { is_error: Some(true), content, .. }
                if content.contains("not available inside subagents")
            )
        })
    });
    assert!(
        companion_note_absent,
        "companion note must NOT appear for non-nke tool 'Bash'; got {evs:?}"
    );
}

#[tokio::test]
async fn loop_allows_tool_in_allowed_list() {
    // allowed_tools = ["Read"]; "Read" is dispatched normally.
    let api = StreamingMockApiClient::new(vec![
        streamed_tool_use_turn("Read", "tool_use"),
        streamed_text_turn("done", "end_turn"),
    ]);
    let invoker = CountingInvoker::new();
    let mut ctx = loop_ctx(api.clone(), Some(invoker.clone()), 4);
    ctx.allowed_tools = vec!["Read".to_string()];

    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    run_subagent(ctx, event_rx, out_tx).await;
    let _ = drain(out_rx).await;

    assert_eq!(
        invoker.call_count(),
        1,
        "a tool inside allowed_tools is dispatched"
    );
}

#[tokio::test]
async fn loop_budget_exhausted_stops_before_any_round_trip() {
    // Test A: the inherited budget is already over the limit. The per-turn
    // gate fires BEFORE the first model round-trip, so the loop emits a
    // single budget-exhausted Failed and makes ZERO model calls. The error
    // is the 2.1.217 byte-locked background-agent halt string formatted from
    // current_nano_usd (1.5e9 -> "$1.50") and the $1 ceiling.
    let api = MockSubagentApiClient::new(vec![Ok(text_response("unused", Some("end_turn")))]);
    let mut ctx = loop_ctx(api.clone(), None, 4);
    ctx.budget = Some(Arc::new(MockBudget { exceeded: true }));
    let statistics =
        Arc::new(lingxi_core::host::agent_statistics::AgentSessionStatistics::default());
    let token = statistics.prepare_spawn("worker".into(), None, true, 1);
    token.started();
    ctx.agent_spawn_token = Some(token);

    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    run_subagent(ctx, event_rx, out_tx).await;
    let evs = drain(out_rx).await;
    assert_eq!(statistics.snapshot().killed.system, 1);
    assert_eq!(statistics.snapshot().failed, 0);

    assert_eq!(
        api.call_count(),
        0,
        "the budget gate precedes the streaming request — no round-trip"
    );
    let failed = evs.iter().find_map(|e| match e {
        SubagentEvent::Failed { error, .. } => Some(error.clone()),
        _ => None,
    });
    assert_eq!(
        failed.as_deref(),
        Some("Budget limit reached ($1.50 of $1); stopping background agents."),
        "byte-locked 2.1.217 denial string; got events: {evs:?}"
    );
    assert!(
        !evs.iter()
            .any(|e| matches!(e, SubagentEvent::Completed { .. })),
        "no Completed when stopped on budget; got events: {evs:?}"
    );
}

#[tokio::test]
async fn loop_budget_ok_does_not_interfere_with_completion() {
    // Test B (non-interference): a within-limit budget lets the existing
    // single-end_turn path complete with aggregated text and the api is
    // called exactly once — the gate is transparent when `Ok`.
    let api = MockSubagentApiClient::new(vec![Ok(text_response("final answer", Some("end_turn")))]);
    let mut ctx = loop_ctx(api.clone(), None, 4);
    ctx.budget = Some(Arc::new(MockBudget { exceeded: false }));

    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    run_subagent(ctx, event_rx, out_tx).await;
    let evs = drain(out_rx).await;

    assert_eq!(api.call_count(), 1, "exactly one model round-trip");
    let result = one_completed(&evs);
    assert_eq!(result["text"], "final answer");
    assert_eq!(result["stop_reason"], "end_turn");
}

#[tokio::test]
async fn loop_tool_use_then_end_turn_invokes_tool_and_runs_two_turns() {
    // Core happy path: turn 1 emits a tool_use (stop_reason tool_use) ->
    // tool is invoked -> results fed back -> turn 2 ends. Asserts 2 model
    // calls, exactly one tool invocation, and final aggregated text.
    let api = MockSubagentApiClient::new(vec![
        Ok(tool_use_response("Read", Some("tool_use"))),
        Ok(text_response("done", Some("end_turn"))),
    ]);
    let invoker = CountingInvoker::new();
    let ctx = loop_ctx(api.clone(), Some(invoker.clone()), 4);

    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    run_subagent(ctx, event_rx, out_tx).await;
    let evs = drain(out_rx).await;

    assert_eq!(api.call_count(), 2, "two model round-trips");
    assert_eq!(
        invoker.call_count(),
        1,
        "tool invoked once (1:1 with tool_use)"
    );
    let result = one_completed(&evs);
    assert_eq!(result["text"], "done");
    assert_eq!(result["stop_reason"], "end_turn");
}

#[tokio::test]
async fn permission_abort_fails_subagent_without_recoverable_tool_result() {
    let api = MockSubagentApiClient::new(vec![
        Ok(tool_use_response("Bash", Some("tool_use"))),
        Ok(text_response("must not run", Some("end_turn"))),
    ]);
    let ctx = loop_ctx(api.clone(), Some(Arc::new(AbortInvoker)), 4);

    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    run_subagent(ctx, event_rx, out_tx).await;
    let events = drain(out_rx).await;

    assert_eq!(api.call_count(), 1, "the abort must stop the model loop");
    assert!(events.iter().any(|event| matches!(
        event,
        SubagentEvent::Failed { error, .. }
            if error == "Agent aborted: too many classifier denials in headless mode"
    )));
    assert!(!events
        .iter()
        .any(|event| matches!(event, SubagentEvent::Completed { .. })));
    assert!(
        !events.iter().any(|event| matches!(
            event,
            SubagentEvent::Message { message, .. }
                if message.to_string().contains("tool_result")
        )),
        "a terminal permission abort must not be fed back to the model: {events:?}"
    );
}

#[tokio::test]
async fn loop_end_turn_with_tool_use_still_dispatches_then_completes() {
    // MAJOR #1 regression: an `end_turn` response that ALSO carries a
    // tool_use must NOT silently drop the tool. The reference dispatches
    // tools whenever present, then terminates on end_turn. Assert the tool
    // was invoked AND the run completed in a single turn (no continuation).
    let api = MockSubagentApiClient::new(vec![Ok(tool_use_response("Read", Some("end_turn")))]);
    let invoker = CountingInvoker::new();
    let ctx = loop_ctx(api.clone(), Some(invoker.clone()), 4);

    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    run_subagent(ctx, event_rx, out_tx).await;
    let evs = drain(out_rx).await;

    assert_eq!(
        api.call_count(),
        1,
        "end_turn terminates after one round-trip"
    );
    assert_eq!(
        invoker.call_count(),
        1,
        "tool_use on an end_turn response is still dispatched"
    );
    let result = one_completed(&evs);
    assert_eq!(result["stop_reason"], "end_turn");
}

#[tokio::test]
async fn loop_truncated_tool_use_terminates_instead_of_looping() {
    // MAJOR #2 regression: a non-`tool_use` reason (e.g. max_tokens) that
    // also carried a tool_use must dispatch the tool then TERMINATE — not
    // continue looping until max_turns. With max_turns=4 the loop would
    // make 4 calls if it (incorrectly) continued; the fix caps it at 1.
    let api = MockSubagentApiClient::new(vec![Ok(tool_use_response("Read", Some("max_tokens")))]);
    let invoker = CountingInvoker::new();
    let ctx = loop_ctx(api.clone(), Some(invoker.clone()), 4);

    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    run_subagent(ctx, event_rx, out_tx).await;
    let evs = drain(out_rx).await;

    assert_eq!(
        api.call_count(),
        1,
        "max_tokens terminates after one round-trip (no loop-to-max_turns)"
    );
    assert_eq!(
        invoker.call_count(),
        1,
        "the truncated turn's tool is still dispatched"
    );
    let result = one_completed(&evs);
    assert_eq!(result["stop_reason"], "max_tokens");
}

#[tokio::test]
async fn loop_api_error_surfaces_failed() {
    let api = MockSubagentApiClient::new(vec![Err(llm_runtime::LlmError::InvalidRequest {
        message: "boom".into(),
    })]);
    let ctx = loop_ctx(api.clone(), None, 4);

    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    run_subagent(ctx, event_rx, out_tx).await;
    let evs = drain(out_rx).await;

    let failed = evs.iter().find_map(|e| match e {
        SubagentEvent::Failed { error, .. } => Some(error.clone()),
        _ => None,
    });
    let err = failed.expect("Failed on api error");
    assert!(err.starts_with("subagent api error:"), "got: {err}");
}

#[tokio::test]
async fn loop_api_error_persists_seed_and_terminal_reason() {
    let dir = tempfile::tempdir().unwrap();
    let api = MockSubagentApiClient::new(vec![Err(llm_runtime::LlmError::InvalidRequest {
        message: "provider rejected tool_choice".into(),
    })]);
    let mut ctx = loop_ctx(api, None, 4);
    ctx.transcript_subdir = dir.path().to_path_buf();
    ctx.transcript_fs = Some(Arc::new(platform_posix::PosixFileSystem::new(
        dir.path().to_path_buf(),
    )) as Arc<dyn lingxi_core::host::FileSystem>);
    ctx.prompt_messages = vec![lingxi_core::types::ConversationMessage::user(
        MessageId::new(),
        "design the local app".to_string(),
    )];
    let agent_id = ctx.agent_id;

    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    run_subagent(ctx, event_rx, out_tx).await;
    let _ = drain(out_rx).await;

    let path = dir.path().join(format!("agent-{agent_id}.jsonl"));
    let body = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "failed run transcript at {} should exist: {e}",
            path.display()
        )
    });
    assert!(
        body.contains("design the local app"),
        "the seed must survive a first-request API failure: {body}"
    );
    assert!(
        body.contains("provider rejected tool_choice"),
        "the terminal provider reason must be inspectable: {body}"
    );
    assert!(
        body.contains("\"status\":\"failed\""),
        "the transcript must distinguish terminal failure: {body}"
    );
}

#[tokio::test]
async fn loop_tool_use_without_invoker_fails() {
    // A tool_use with tool_invoker = None surfaces Failed.
    let api = MockSubagentApiClient::new(vec![Ok(tool_use_response("Read", Some("tool_use")))]);
    let ctx = loop_ctx(api.clone(), None, 4);

    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    run_subagent(ctx, event_rx, out_tx).await;
    let evs = drain(out_rx).await;

    let failed = evs.iter().find_map(|e| match e {
        SubagentEvent::Failed { error, .. } => Some(error.clone()),
        _ => None,
    });
    assert_eq!(
        failed.as_deref(),
        Some("subagent requested a tool but no tool_invoker was inherited")
    );
}

#[tokio::test]
async fn loop_exhausts_max_turns_when_never_terminal() {
    // Every turn emits a tool_use with stop_reason tool_use, so the loop
    // continues. With max_turns=3 it makes exactly 3 model calls then
    // surfaces Completed{reason: "max_turns_exhausted"}.
    let api = MockSubagentApiClient::new(vec![
        Ok(tool_use_response("Read", Some("tool_use"))),
        Ok(tool_use_response("Read", Some("tool_use"))),
        Ok(tool_use_response("Read", Some("tool_use"))),
        // a 4th would only be reached on an off-by-one bug:
        Ok(text_response("should-not-reach", Some("end_turn"))),
    ]);
    let invoker = CountingInvoker::new();
    let ctx = loop_ctx(api.clone(), Some(invoker.clone()), 3);

    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    run_subagent(ctx, event_rx, out_tx).await;
    let evs = drain(out_rx).await;

    assert_eq!(api.call_count(), 3, "exactly max_turns model round-trips");
    assert_eq!(invoker.call_count(), 3, "one tool dispatch per turn");
    let result = one_completed(&evs);
    assert_eq!(result["reason"], "max_turns_exhausted");
    assert_eq!(result["max_turns"], 3);
}

#[tokio::test]
async fn loop_user_interrupt_mid_flight_surfaces_killed() {
    // A UserInterrupt delivered while the loop is racing the API future
    // aborts to Killed. We pre-load the event so the biased select! takes
    // the termination arm on the first poll.
    let api = MockSubagentApiClient::new(vec![Ok(text_response("unused", Some("end_turn")))]);
    let ctx = loop_ctx(api.clone(), None, 4);

    let (event_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    event_tx
        .send(lingxi_core::Event::UserInterrupt)
        .await
        .unwrap();

    run_subagent(ctx, event_rx, out_tx).await;
    let evs = drain(out_rx).await;

    assert!(
        evs.iter()
            .any(|e| matches!(e, SubagentEvent::Killed { .. })),
        "expected Killed on UserInterrupt; got: {evs:?}"
    );
    assert!(
        !evs.iter()
            .any(|e| matches!(e, SubagentEvent::Completed { .. })),
        "no Completed when killed mid-flight; got: {evs:?}"
    );
}

// ---- Persist-mode tests (ctx.persistent = true) ----------------------

#[tokio::test]
async fn persistent_resume_emits_user_message_before_stalled_provider_once() {
    struct ResumeApi {
        calls: AtomicUsize,
        started: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }
    #[async_trait]
    impl crate::api::SubagentApiClient for ResumeApi {
        async fn stream(
            &self,
            request: crate::api::SubagentApiRequest,
        ) -> Result<
            futures::stream::BoxStream<
                'static,
                Result<llm_runtime::HistoryEvent, llm_runtime::LlmError>,
            >,
            llm_runtime::LlmError,
        > {
            let _model = request.model.as_str();
            let _system = request.system.as_deref();
            let _messages = request.messages;
            let _tools = request.tools;
            let response: Result<llm_runtime::HistoryResponse, llm_runtime::LlmError> = async {
                if self.calls.fetch_add(1, Ordering::SeqCst) > 0 {
                    self.started.notify_one();
                    self.release.notified().await;
                }
                Ok(text_response("answer", Some("end_turn")))
            }
            .await;
            let events = llm_runtime::stream_accumulator::response_to_stream_events(response?);
            Ok(futures::StreamExt::boxed(futures::stream::iter(
                events.into_iter().map(Ok),
            )))
        }
    }
    let api = Arc::new(ResumeApi {
        calls: AtomicUsize::new(0),
        started: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let mut ctx = loop_ctx(api.clone(), None, 4);
    ctx.persistent = true;
    let (event_tx, event_rx) = mpsc::channel(8);
    let (out_tx, mut out_rx) = mpsc::channel(16);
    let handle = tokio::spawn(run_subagent(ctx, event_rx, out_tx));
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while !matches!(out_rx.recv().await, Some(SubagentEvent::Completed { .. })) {}
    })
    .await
    .expect("first turn completes");
    event_tx
        .send(lingxi_core::Event::UserMessage {
            message_id: MessageId::new(),
            request_id: RequestId::new(),
            content: "second question".into(),
        })
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), api.started.notified())
        .await
        .expect("resumed provider starts");
    let is_resumed_user = |event: &SubagentEvent| {
        matches!(event, SubagentEvent::Message { message, .. }
            if message["role"] == "user" && message.to_string().contains("second question"))
    };
    tokio::time::timeout(std::time::Duration::from_millis(250), async {
        loop {
            let event = out_rx.recv().await.expect("resumed runner remains live");
            if is_resumed_user(&event) {
                break;
            }
        }
    })
    .await
    .expect("resumed user is visible while provider response is still blocked");
    api.release.notify_one();
    let duplicates = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        let mut duplicates = 0;
        loop {
            let event = out_rx.recv().await.expect("resumed turn completes");
            duplicates += usize::from(is_resumed_user(&event));
            if matches!(event, SubagentEvent::Completed { .. }) {
                return duplicates;
            }
        }
    })
    .await
    .expect("resumed turn completes after provider release");
    event_tx.send(lingxi_core::Event::UserExit).await.unwrap();
    handle.await.unwrap();
    assert_eq!(duplicates, 0, "resumed user must be emitted exactly once");
    assert_eq!(api.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn persist_mode_processes_second_message_after_idling() {
    // Turn-set 1: a single end_turn turn completes, then the runner parks
    // (it does NOT return because persistent = true). We then inject a
    // second UserMessage which un-idles it and drives turn-set 2; finally
    // we close the channel to terminate gracefully. Asserts: exactly two
    // model round-trips and two Completed events (one per turn-set).
    let api = MockSubagentApiClient::new(vec![
        Ok(text_response("answer one", Some("end_turn"))),
        Ok(text_response("answer two", Some("end_turn"))),
    ]);
    let mut ctx = loop_ctx(api.clone(), None, 4);
    ctx.persistent = true;

    let (event_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);

    let handle = tokio::spawn(run_subagent(ctx, event_rx, out_tx));

    // Wait for turn-set 1 to complete (the runner has now idled), then
    // inject the second message that drives turn-set 2.
    let mut out_rx = out_rx;
    let first_completed = loop {
        let ev = out_rx.recv().await.expect("turn-set 1 should complete");
        if matches!(ev, SubagentEvent::Completed { .. }) {
            break ev;
        }
    };
    let SubagentEvent::Completed { result, .. } = &first_completed else {
        unreachable!()
    };
    assert_eq!(result["text"], "answer one", "turn-set 1 result");

    event_tx
        .send(lingxi_core::Event::UserMessage {
            message_id: MessageId::new(),
            request_id: RequestId::new(),
            content: "second question".into(),
        })
        .await
        .unwrap();

    // Wait for turn-set 2 to complete.
    let second_completed = loop {
        let ev = out_rx.recv().await.expect("turn-set 2 should complete");
        if matches!(ev, SubagentEvent::Completed { .. }) {
            break ev;
        }
    };
    let SubagentEvent::Completed { result, .. } = &second_completed else {
        unreachable!()
    };
    assert_eq!(result["text"], "answer two", "turn-set 2 result");

    // Close the channel: the parked runner terminates gracefully.
    drop(event_tx);
    handle.await.unwrap();

    assert_eq!(
        api.call_count(),
        2,
        "exactly two model round-trips (one per turn-set)"
    );
}

#[tokio::test]
async fn persistent_child_mod_turn_complete_fires_once_per_turn_set() {
    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("persistent-turn-complete.js");
    std::fs::write(
        &module,
        r#"let seen = [];
let started = [];
export function register(on) {
  on('turn.start', ($, e) => {
    started.push(e);
    return { turnId: 'ignored-by-engine' };
  });
  on('turn.complete', ($, e, next) => {
    seen.push(e);
    return next(e);
  });
  on('prompt.submit', ($, e, next) => next({ ...e, text: JSON.stringify({ started, seen }) }));
}"#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load(
        "persistent-turn-complete",
        dir.path(),
        &module,
        serde_json::json!({}),
    )
    .await
    .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host.clone());
    let executor = Arc::new(hooks::HookExecutorImpl::new(
        Arc::new(tokio::sync::RwLock::new(registry)),
        Arc::new(test_harness::mocks::MockHttpTransport::new()),
        Arc::new(test_harness::mocks::MockRuntimeSpawner::default()),
    ));
    let mut first = text_response("first turn", Some("end_turn"));
    first.usage.counts_mut().input_tokens = 3;
    first.usage.counts_mut().output_tokens = 2;
    let mut second = text_response("second turn", Some("end_turn"));
    second.usage.counts_mut().input_tokens = 5;
    second.usage.counts_mut().output_tokens = 4;
    let api = MockSubagentApiClient::new(vec![Ok(first), Ok(second)]);
    let mut ctx = loop_ctx(api, None, 4);
    ctx.persistent = true;
    ctx.prompt_messages = vec![ConversationMessage::user(
        MessageId::new(),
        "first question".to_string(),
    )];
    ctx.hook_executor = Some(executor.clone());
    ctx.hook_cwd = dir.path().to_path_buf();
    let (event_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, mut out_rx) = mpsc::channel::<SubagentEvent>(16);
    let runner = tokio::spawn(run_subagent(ctx, event_rx, out_tx));
    while !matches!(
        out_rx.recv().await.unwrap(),
        SubagentEvent::Completed { .. }
    ) {}
    event_tx
        .send(lingxi_core::Event::UserMessage {
            message_id: MessageId::new(),
            request_id: RequestId::new(),
            content: "continue".into(),
        })
        .await
        .unwrap();
    while !matches!(
        out_rx.recv().await.unwrap(),
        SubagentEvent::Completed { .. }
    ) {}
    drop(event_tx);
    runner.await.unwrap();
    let events = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let result = host
                .dispatch(
                    "prompt.submit",
                    serde_json::json!({"text":"probe"}),
                    |event| async move { Ok(event) },
                )
                .await
                .unwrap();
            let events: serde_json::Value =
                serde_json::from_str(result["text"].as_str().unwrap()).unwrap();
            if events["seen"]
                .as_array()
                .is_some_and(|seen| seen.len() == 2)
                && events["started"]
                    .as_array()
                    .is_some_and(|started| started.len() == 2)
            {
                break events;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("both persistent turns dispatch");
    assert_eq!(events["started"][0]["text"], "first question");
    assert_eq!(events["started"][1]["text"], "continue");
    assert_eq!(events["seen"][0]["answer"], "first turn");
    assert_eq!(events["seen"][1]["answer"], "second turn");
    assert_eq!(events["started"][0]["turnId"], events["seen"][0]["turnId"]);
    assert_eq!(events["started"][1]["turnId"], events["seen"][1]["turnId"]);
    assert_ne!(events["seen"][0]["turnId"], events["seen"][1]["turnId"]);
    assert_eq!(events["seen"][0]["usage"]["input_tokens"], 3);
    assert_eq!(events["seen"][1]["usage"]["input_tokens"], 5);

    let mut rejected = loop_ctx(MockSubagentApiClient::new(vec![]), None, 1);
    rejected.hook_executor = Some(executor);
    rejected.hook_cwd = dir.path().to_path_buf();
    rejected.budget = Some(Arc::new(MockBudget { exceeded: true }));
    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    run_subagent(rejected, event_rx, out_tx).await;
    assert!(drain(out_rx)
        .await
        .iter()
        .any(|event| matches!(event, SubagentEvent::Failed { .. })));
    let result = host
        .dispatch(
            "prompt.submit",
            serde_json::json!({"text":"probe"}),
            |event| async move { Ok(event) },
        )
        .await
        .unwrap();
    let after_rejection: serde_json::Value =
        serde_json::from_str(result["text"].as_str().unwrap()).unwrap();
    assert_eq!(after_rejection["started"].as_array().unwrap().len(), 2);
    assert_eq!(after_rejection["seen"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn persist_mode_transcript_distinguishes_idle_from_true_terminal() {
    let dir = tempfile::tempdir().unwrap();
    let api = MockSubagentApiClient::new(vec![
        Ok(text_response("one", Some("end_turn"))),
        Ok(text_response("two", Some("end_turn"))),
    ]);
    let mut ctx = loop_ctx(api, None, 4);
    ctx.persistent = true;
    ctx.transcript_subdir = dir.path().to_path_buf();
    ctx.transcript_fs = Some(Arc::new(platform_posix::PosixFileSystem::new(
        dir.path().to_path_buf(),
    )) as Arc<dyn lingxi_core::host::FileSystem>);
    let agent_id = ctx.agent_id;
    let path = dir.path().join(format!("agent-{agent_id}.jsonl"));

    let (event_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, mut out_rx) = mpsc::channel::<SubagentEvent>(16);
    let handle = tokio::spawn(run_subagent(ctx, event_rx, out_tx));

    // The first completed turn parks the persistent runner; its durable state
    // must be idle, not terminal-completed.
    while !matches!(
        out_rx.recv().await.expect("first turn"),
        SubagentEvent::Completed { .. }
    ) {}
    let body = tokio::fs::read_to_string(&path).await.unwrap();
    let statuses: Vec<String> = body
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter_map(|line| {
            line.get("status")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .collect();
    assert_eq!(statuses.first().map(String::as_str), Some("running"));
    assert_eq!(statuses.last().map(String::as_str), Some("idle"));
    assert!(
        body.contains("\"agent_type\":\"test\""),
        "resolved agent type metadata is persisted"
    );

    event_tx
        .send(lingxi_core::Event::UserMessage {
            message_id: MessageId::new(),
            request_id: RequestId::new(),
            content: "next".into(),
        })
        .await
        .unwrap();
    while !matches!(
        out_rx.recv().await.expect("second turn"),
        SubagentEvent::Completed { .. }
    ) {}
    let body = tokio::fs::read_to_string(&path).await.unwrap();
    let last_status = body
        .lines()
        .rev()
        .find_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .and_then(|line| {
            line.get("status")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        });
    assert_eq!(last_status.as_deref(), Some("idle"));

    event_tx.send(lingxi_core::Event::UserExit).await.unwrap();
    handle.await.unwrap();
    let body = tokio::fs::read_to_string(&path).await.unwrap();
    let last_status = body
        .lines()
        .rev()
        .find_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .and_then(|line| {
            line.get("status")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        });
    assert_eq!(last_status.as_deref(), Some("cancelled"));
}

#[tokio::test]
async fn persist_mode_terminates_on_channel_close_after_turn_set() {
    // With persistent = true, closing the event channel after the first
    // turn-set completes makes the parked runner return gracefully (no
    // further events, no Failed).
    let api = MockSubagentApiClient::new(vec![Ok(text_response("done", Some("end_turn")))]);
    let mut ctx = loop_ctx(api.clone(), None, 4);
    ctx.persistent = true;

    let (event_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);

    // Drop the sender immediately: the runner runs turn-set 1, parks, sees
    // the channel already closed, and returns.
    drop(event_tx);

    run_subagent(ctx, event_rx, out_tx).await;
    let evs = drain(out_rx).await;

    assert_eq!(api.call_count(), 1, "one turn-set ran before EOF");
    let completed = evs
        .iter()
        .filter(|e| matches!(e, SubagentEvent::Completed { .. }))
        .count();
    assert_eq!(completed, 1, "one Completed; got: {evs:?}");
    assert!(
        !evs.iter()
            .any(|e| matches!(e, SubagentEvent::Failed { .. })),
        "no Failed on graceful EOF; got: {evs:?}"
    );
}

#[tokio::test]
async fn persist_mode_user_exit_while_idle_surfaces_killed() {
    // While parked between turn-sets, a UserExit terminates the teammate
    // with Killed (cooperative shutdown).
    let api = MockSubagentApiClient::new(vec![Ok(text_response("done", Some("end_turn")))]);
    let mut ctx = loop_ctx(api.clone(), None, 4);
    ctx.persistent = true;
    let agent_id = ctx.agent_id;

    let (event_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);

    let handle = tokio::spawn(run_subagent(ctx, event_rx, out_tx));

    let mut out_rx = out_rx;
    // Wait for turn-set 1 to complete (runner now idle).
    loop {
        let ev = out_rx.recv().await.expect("turn-set 1 completes");
        if matches!(ev, SubagentEvent::Completed { .. }) {
            break;
        }
    }
    // Deliver UserExit to the idle runner.
    event_tx.send(lingxi_core::Event::UserExit).await.unwrap();
    handle.await.unwrap();

    let evs = drain(out_rx).await;
    assert!(
        evs.iter()
            .any(|e| matches!(e, SubagentEvent::Killed { agent_id: aid } if *aid == agent_id)),
        "UserExit while idle yields Killed; got: {evs:?}"
    );
}

// ---- Wake-on-message (cc 2.1.198, M9) ---------------------------------

/// `SubagentApiClient` whose FIRST round-trip hangs forever (a teammate
/// "stuck" mid-request / in the client's internal retry backoff) and whose
/// subsequent round-trips capture their `messages` then answer end_turn.
struct StuckThenCapturingApiClient {
    calls: AtomicUsize,
    first_call_started: tokio::sync::Notify,
    later_messages: Mutex<Vec<Vec<ConversationMessage>>>,
}
impl StuckThenCapturingApiClient {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            first_call_started: tokio::sync::Notify::new(),
            later_messages: Mutex::new(Vec::new()),
        })
    }
    fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}
#[async_trait]
impl crate::api::SubagentApiClient for StuckThenCapturingApiClient {
    async fn stream(
        &self,
        request: crate::api::SubagentApiRequest,
    ) -> Result<
        futures::stream::BoxStream<
            'static,
            Result<llm_runtime::HistoryEvent, llm_runtime::LlmError>,
        >,
        llm_runtime::LlmError,
    > {
        let _model = request.model.as_str();
        let _system = request.system.as_deref();
        let messages = request.messages;
        let _tools = request.tools;
        let response: Result<llm_runtime::HistoryResponse, llm_runtime::LlmError> = async {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                // Stuck: never resolves. The runner's select must drop this
                // future on the inbound UserMessage and re-issue.
                self.first_call_started.notify_one();
                std::future::pending::<()>().await;
                unreachable!("the stuck first call must be dropped, not resolved")
            }
            self.later_messages.lock().unwrap().push(messages);
            Ok(text_response("woke and answered", Some("end_turn")))
        }
        .await;
        let events = llm_runtime::stream_accumulator::response_to_stream_events(response?);
        Ok(futures::StreamExt::boxed(futures::stream::iter(
            events.into_iter().map(Ok),
        )))
    }
}

/// cc 2.1.198 (M9): messaging a stuck teammate wakes it to retry immediately.
/// Binary mechanism: SendMessage emits the recipient task's `retryWake` signal
/// after the mailbox write (`TDo` @215134403: `r.retryWake?.emit()`), which
/// `subscribeRetryWake` (@216289770) threads into the API retry loop so the
/// backoff sleep is interrupted and the queued message rides into the turn.
/// Rust analog: a `UserMessage` racing the in-flight round-trip drops the
/// stuck `api_call` future, appends the message to history, and re-issues the
/// round-trip immediately — previously the message text was silently DROPPED.
#[tokio::test]
async fn persist_mode_message_wakes_stuck_round_trip_and_carries_the_text() {
    let api = StuckThenCapturingApiClient::new();
    let mut ctx = loop_ctx(api.clone(), None, 4);
    ctx.persistent = true;

    let (event_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    let handle = tokio::spawn(run_subagent(ctx, event_rx, out_tx));

    // Wait until the first round-trip is in flight (stuck).
    api.first_call_started.notified().await;

    // Message the stuck teammate.
    event_tx
        .send(lingxi_core::Event::UserMessage {
            message_id: MessageId::new(),
            request_id: RequestId::new(),
            content: "are you alive? try again".into(),
        })
        .await
        .unwrap();

    // The wake re-issues the round-trip immediately; the retry completes.
    let mut out_rx = out_rx;
    let completed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let ev = out_rx.recv().await.expect("the woken turn-set completes");
            if matches!(ev, SubagentEvent::Completed { .. }) {
                break ev;
            }
        }
    })
    .await
    .expect("a UserMessage must wake the stuck model request within the bound");
    let SubagentEvent::Completed { result, .. } = &completed else {
        unreachable!()
    };
    assert_eq!(result["text"], "woke and answered");
    assert_eq!(api.call_count(), 2, "stuck call dropped + immediate retry");

    // The retried round-trip CARRIES the message (cc queues it via
    // pendingUserMessages; here it rides on the re-issued history).
    let later = api.later_messages.lock().unwrap().clone();
    let retry_history = later.first().expect("retry captured");
    let carried = retry_history.iter().any(|m| {
        matches!(m, ConversationMessage::User { content, .. }
            if content.iter().any(|b| matches!(b, ContentBlock::Text { text, .. }
                if text.contains("are you alive? try again"))))
    });
    assert!(
        carried,
        "the wake message must ride on the retried round-trip, not be dropped: {retry_history:?}"
    );

    // Cooperative shutdown of the parked (persistent) runner.
    event_tx.send(lingxi_core::Event::UserExit).await.unwrap();
    handle.await.unwrap();
}

// ---- Launcher message ≠ permission approval (cc 2.1.198, M10) ----------

/// `ToolInvoker` that PARKS inside `invoke` — a pending permission prompt
/// living in the permission gate below the invoker seam — until the test
/// releases it, then resolves as the gate's DENY. Tracks whether the pending
/// prompt was resolved and how many times the tool ran.
struct PendingPermissionInvoker {
    invoke_started: tokio::sync::Notify,
    release: tokio::sync::Notify,
    calls: AtomicUsize,
    resolved: std::sync::atomic::AtomicBool,
}
impl PendingPermissionInvoker {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            invoke_started: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
            calls: AtomicUsize::new(0),
            resolved: std::sync::atomic::AtomicBool::new(false),
        })
    }
}
#[async_trait]
impl lingxi_core::host::ToolInvoker for PendingPermissionInvoker {
    async fn invoke(
        &self,
        _name: &str,
        _input: serde_json::Value,
        _ctx: lingxi_core::host::tool_invoker::SubagentInvocationContext,
    ) -> Result<serde_json::Value, lingxi_core::host::tool_invoker::ToolInvokerError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.invoke_started.notify_one();
        self.release.notified().await;
        self.resolved.store(true, Ordering::SeqCst);
        Err(lingxi_core::host::tool_invoker::ToolInvokerError::Internal(
            "Permission to use SlowTool has been denied.".into(),
        ))
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// `SubagentApiClient` whose FIRST round-trip returns one `SlowTool` tool_use
/// and whose subsequent round-trips capture their `messages` then end the turn.
struct ToolUseThenCapturingApiClient {
    calls: AtomicUsize,
    later_messages: Mutex<Vec<Vec<ConversationMessage>>>,
}
impl ToolUseThenCapturingApiClient {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            later_messages: Mutex::new(Vec::new()),
        })
    }
}
#[async_trait]
impl crate::api::SubagentApiClient for ToolUseThenCapturingApiClient {
    async fn stream(
        &self,
        request: crate::api::SubagentApiRequest,
    ) -> Result<
        futures::stream::BoxStream<
            'static,
            Result<llm_runtime::HistoryEvent, llm_runtime::LlmError>,
        >,
        llm_runtime::LlmError,
    > {
        let _model = request.model.as_str();
        let _system = request.system.as_deref();
        let messages = request.messages;
        let _tools = request.tools;
        let response: Result<llm_runtime::HistoryResponse, llm_runtime::LlmError> = async {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                return Ok(tool_use_response("SlowTool", Some("tool_use")));
            }
            self.later_messages.lock().unwrap().push(messages);
            Ok(text_response("done after direction", Some("end_turn")))
        }
        .await;
        let events = llm_runtime::stream_accumulator::response_to_stream_events(response?);
        Ok(futures::StreamExt::boxed(futures::stream::iter(
            events.into_iter().map(Ok),
        )))
    }
}

/// cc 2.1.198 (M10): "Fixed an issue where messages sent by the agent that
/// launched a subagent could be treated as user approval" — a launcher/lead
/// message to a running subagent is NEW TASK DIRECTION and must NEVER satisfy
/// a pending permission request. Structurally, LingXi keeps the two channels
/// separate: permission approval reaches a pending prompt only through the
/// permission gate below the `ToolInvoker` seam, while a launcher message
/// arrives as `lingxi_core::Event::UserMessage` on the runner's event channel and
/// is appended to history as a user message. This test locks that separation:
/// with a permission prompt PENDING inside `invoke`, an inbound launcher
/// message (1) does not resolve/approve the prompt, (2) does not re-run the
/// tool, and (3) rides into the next round-trip as a plain user message AFTER
/// the gate's own deny result.
#[tokio::test]
async fn launcher_message_is_direction_not_approval_of_pending_permission() {
    let api = ToolUseThenCapturingApiClient::new();
    let invoker = PendingPermissionInvoker::new();
    let mut ctx = loop_ctx(
        api.clone(),
        Some(invoker.clone() as Arc<dyn lingxi_core::host::ToolInvoker>),
        4,
    );
    ctx.persistent = true;

    let (event_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    let handle = tokio::spawn(run_subagent(ctx, event_rx, out_tx));

    // Wait until the tool call is in flight with its permission prompt pending.
    invoker.invoke_started.notified().await;

    // The launcher messages the running subagent (SendMessage → UserMessage).
    event_tx
        .send(lingxi_core::Event::UserMessage {
            message_id: MessageId::new(),
            request_id: RequestId::new(),
            content: "switch to auditing the docs instead".into(),
        })
        .await
        .unwrap();

    // The message must NOT satisfy the pending permission: the prompt is still
    // parked after the message has been delivered.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(
        !invoker.resolved.load(Ordering::SeqCst),
        "a launcher message must never resolve a pending permission request"
    );

    // Only the permission gate's own channel resolves the prompt — as a DENY.
    invoker.release.notify_one();

    // The turn-set completes: deny tool_result fed back, launcher message
    // drained as task direction, final end_turn.
    let mut out_rx = out_rx;
    loop {
        let ev = out_rx.recv().await.expect("the turn-set completes");
        if matches!(ev, SubagentEvent::Completed { .. }) {
            break;
        }
    }

    // The tool ran exactly once — the message triggered no approval-driven
    // (re-)execution.
    assert_eq!(invoker.calls.load(Ordering::SeqCst), 1);

    // The next round-trip's history carries the gate's DENY as the tool_result
    // and the launcher message as a plain user TEXT message after it.
    let later = api.later_messages.lock().unwrap().clone();
    let hist = later.first().expect("second round-trip captured");
    let deny_idx = hist
        .iter()
        .position(|m| {
            matches!(m, ConversationMessage::User { content, .. }
                if content.iter().any(|b| matches!(b, ContentBlock::ToolResult { content, is_error, .. }
                    if is_error.unwrap_or(false) && content == "Error: Permission to use SlowTool has been denied.")))
        })
        .expect("the deny tool_result is in history");
    let direction_idx = hist
        .iter()
        .position(|m| {
            matches!(m, ConversationMessage::User { content, .. }
                if content.iter().any(|b| matches!(b, ContentBlock::Text { text, .. }
                    if text.contains("switch to auditing the docs instead"))))
        })
        .expect("the launcher message rides as task direction");
    assert!(
        direction_idx > deny_idx,
        "direction is appended after the deny result, never in its place"
    );

    // Cooperative shutdown of the parked (persistent) runner.
    event_tx.send(lingxi_core::Event::UserExit).await.unwrap();
    handle.await.unwrap();
}

// ── G4 (SubagentStart additionalContext) + G5 (skills preload) ──────────

/// `SubagentApiClient` that captures the `messages` of its FIRST round-trip
/// so a test can assert what the runner seeded as the child's initial
/// history (the preload messages are sent to the model, not emitted as
/// events). Replies with a single end_turn text turn.
struct CapturingApiClient {
    first_messages: Mutex<Option<Vec<ConversationMessage>>>,
}
impl CapturingApiClient {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            first_messages: Mutex::new(None),
        })
    }
    fn captured(&self) -> Vec<ConversationMessage> {
        self.first_messages
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_default()
    }
}
#[async_trait]
impl crate::api::SubagentApiClient for CapturingApiClient {
    async fn stream(
        &self,
        request: crate::api::SubagentApiRequest,
    ) -> Result<
        futures::stream::BoxStream<
            'static,
            Result<llm_runtime::HistoryEvent, llm_runtime::LlmError>,
        >,
        llm_runtime::LlmError,
    > {
        let _model = request.model.as_str();
        let _system = request.system.as_deref();
        let messages = request.messages;
        let _tools = request.tools;
        let response: Result<llm_runtime::HistoryResponse, llm_runtime::LlmError> = async {
            let mut slot = self.first_messages.lock().unwrap();
            if slot.is_none() {
                *slot = Some(messages);
            }
            Ok(text_response("done", Some("end_turn")))
        }
        .await;
        let events = llm_runtime::stream_accumulator::response_to_stream_events(response?);
        Ok(futures::StreamExt::boxed(futures::stream::iter(
            events.into_iter().map(Ok),
        )))
    }
}

/// A `SubagentStart` builtin hook handler that returns one `additionalContext`
/// string so the runner injects it into the child's initial history (G4).
/// `handler_id` lets a test register more than one handler (distinct ids).
struct AdditionalContextStartHook {
    handler_id: String,
    context: String,
}
#[async_trait]
impl hooks::executor::BuiltinHookHandler for AdditionalContextStartHook {
    fn id(&self) -> &str {
        &self.handler_id
    }
    async fn handle(
        &self,
        _event: &hooks::events::HookEvent,
        _ctx: &hooks::registry::HookContext,
    ) -> hooks::response::HookResult {
        hooks::response::HookResult {
            outcome: hooks::response::HookOutcome::Success,
            stdout: String::new(),
            stderr: String::new(),
            exit_code: None,
            response: Some(hooks::response::HookResponse {
                additional_context: Some(self.context.clone().into()),
                ..Default::default()
            }),
        }
    }
}

/// Build an `Arc<HookExecutorImpl>` with ONE registered SubagentStart hook
/// that returns `context` as additionalContext.
async fn exec_with_start_context(context: &str) -> Arc<hooks::HookExecutorImpl> {
    use hooks::definition::{HookDefinition, HookExecutor, HookSource};
    use hooks::events::HookEventType;
    let registry = Arc::new(tokio::sync::RwLock::new(hooks::HookRegistry::new()));
    registry.write().await.register(HookDefinition {
        id: lingxi_core::types::HookId::new(),
        name: "additional-context-start".into(),
        events: vec![HookEventType::SubagentStart],
        if_condition: None,
        executor: HookExecutor::Builtin {
            handler_id: "additional-context-start".into(),
        },
        source: HookSource::Settings(lingxi_core::types::SettingsScope::User),
        blocking: true,
        timeout: None,
        priority: 0,
        once: false,
        status_message: None,
        async_rewake: false,
        async_timeout: None,
        rewake_message: None,
    });
    let mut exec = hooks::HookExecutorImpl::new(
        registry,
        Arc::new(test_harness::mocks::MockHttpTransport::new()),
        Arc::new(test_harness::mocks::MockRuntimeSpawner::default()),
    );
    exec.register_builtin(Arc::new(AdditionalContextStartHook {
        handler_id: "additional-context-start".into(),
        context: context.to_string(),
    }));
    Arc::new(exec)
}

/// Build an `Arc<HookExecutorImpl>` with TWO registered SubagentStart hooks,
/// each returning its own additionalContext — to prove the runner JOINS them
/// into a single `<system-reminder>` message (claude byte-parity).
async fn exec_with_two_start_contexts(c0: &str, c1: &str) -> Arc<hooks::HookExecutorImpl> {
    use hooks::definition::{HookDefinition, HookExecutor, HookSource};
    use hooks::events::HookEventType;
    let registry = Arc::new(tokio::sync::RwLock::new(hooks::HookRegistry::new()));
    for (i, handler_id) in ["start-ctx-0", "start-ctx-1"].iter().enumerate() {
        registry.write().await.register(HookDefinition {
            id: lingxi_core::types::HookId::new(),
            name: (*handler_id).into(),
            events: vec![HookEventType::SubagentStart],
            if_condition: None,
            executor: HookExecutor::Builtin {
                handler_id: (*handler_id).into(),
            },
            source: HookSource::Settings(lingxi_core::types::SettingsScope::User),
            blocking: true,
            timeout: None,
            // Distinct DESCENDING priorities pin the firing order so the
            // join is deterministic (c0 then c1). `match_event` sorts
            // priority-descending, so index 0 (priority 0) fires before
            // index 1 (priority -1).
            #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
            priority: -(i as i32),
            once: false,
            status_message: None,
            async_rewake: false,
            async_timeout: None,
            rewake_message: None,
        });
    }
    let mut exec = hooks::HookExecutorImpl::new(
        registry,
        Arc::new(test_harness::mocks::MockHttpTransport::new()),
        Arc::new(test_harness::mocks::MockRuntimeSpawner::default()),
    );
    exec.register_builtin(Arc::new(AdditionalContextStartHook {
        handler_id: "start-ctx-0".into(),
        context: c0.to_string(),
    }));
    exec.register_builtin(Arc::new(AdditionalContextStartHook {
        handler_id: "start-ctx-1".into(),
        context: c1.to_string(),
    }));
    Arc::new(exec)
}

/// A builtin hook handler that records every `SubagentStop` it sees (the
/// `status` carried on the event) — used to prove a frontmatter
/// `Stop`→`SubagentStop` hook actually fires inside the child runner (#9).
struct RecordingStopHook {
    seen: Arc<Mutex<Vec<String>>>,
}
#[async_trait]
impl hooks::executor::BuiltinHookHandler for RecordingStopHook {
    fn id(&self) -> &str {
        "record-subagent-stop-in-runner"
    }
    async fn handle(
        &self,
        event: &hooks::events::HookEvent,
        _ctx: &hooks::registry::HookContext,
    ) -> hooks::response::HookResult {
        if let hooks::events::HookEvent::SubagentStop { status, .. } = event {
            self.seen.lock().unwrap().push(status.clone());
        }
        hooks::response::HookResult {
            outcome: hooks::response::HookOutcome::Success,
            stdout: String::new(),
            stderr: String::new(),
            exit_code: None,
            response: None,
        }
    }
}

/// Build an executor wired with the [`RecordingStopHook`] builtin and NO
/// source/plugin hooks — so any SubagentStop the recorder sees must have come
/// from the agent-scoped frontmatter fire (the runner path), not a chokepoint.
fn exec_recording_stop(seen: Arc<Mutex<Vec<String>>>) -> Arc<hooks::HookExecutorImpl> {
    let registry = Arc::new(tokio::sync::RwLock::new(hooks::HookRegistry::new()));
    let mut exec = hooks::HookExecutorImpl::new(
        registry,
        Arc::new(test_harness::mocks::MockHttpTransport::new()),
        Arc::new(test_harness::mocks::MockRuntimeSpawner::default()),
    );
    exec.register_builtin(Arc::new(RecordingStopHook { seen }));
    Arc::new(exec)
}

/// Records the `HookContext.last_assistant_message` carried by a runner-fired
/// agent-scoped `SubagentStop`.
struct RecordingStopContextHook {
    seen: Arc<Mutex<Vec<Option<String>>>>,
}
#[async_trait]
impl hooks::executor::BuiltinHookHandler for RecordingStopContextHook {
    fn id(&self) -> &str {
        "record-subagent-stop-context"
    }
    async fn handle(
        &self,
        event: &hooks::events::HookEvent,
        ctx: &hooks::registry::HookContext,
    ) -> hooks::response::HookResult {
        if matches!(event, hooks::events::HookEvent::SubagentStop { .. }) {
            let live = ctx
                .prompt_transcript
                .as_ref()
                .expect("worker Stop must carry live history");
            assert!(live.messages.iter().any(|message| matches!(message,
                ConversationMessage::User { content, .. } if content.iter().any(|block| matches!(block, lingxi_core::types::ContentBlock::Text { text, .. } if text == "go")))));
            assert!(live.messages.iter().any(|message| matches!(message,
                ConversationMessage::Assistant { content, .. } if content.iter().any(|block| matches!(block, lingxi_core::types::ContentBlock::Text { text, .. } if text == "done")))));
            self.seen
                .lock()
                .unwrap()
                .push(ctx.last_assistant_message.clone());
        }
        hooks::response::HookResult {
            outcome: hooks::response::HookOutcome::Success,
            stdout: String::new(),
            stderr: String::new(),
            exit_code: None,
            response: None,
        }
    }
}

fn exec_recording_stop_context(
    seen: Arc<Mutex<Vec<Option<String>>>>,
) -> Arc<hooks::HookExecutorImpl> {
    let registry = Arc::new(tokio::sync::RwLock::new(hooks::HookRegistry::new()));
    let mut exec = hooks::HookExecutorImpl::new(
        registry,
        Arc::new(test_harness::mocks::MockHttpTransport::new()),
        Arc::new(test_harness::mocks::MockRuntimeSpawner::default()),
    );
    exec.register_builtin(Arc::new(RecordingStopContextHook { seen }));
    Arc::new(exec)
}

/// A frontmatter `Stop` hook (Builtin executor) the runner retargets to
/// `SubagentStop` (registerFrontmatterHooks isAgent=true).
fn frontmatter_stop_hook(handler_id: &str) -> hooks::definition::HookDefinition {
    use hooks::definition::{HookExecutor, HookSource};
    use hooks::events::HookEventType;
    hooks::definition::HookDefinition {
        id: lingxi_core::types::HookId::new(),
        name: handler_id.into(),
        events: vec![HookEventType::Stop],
        if_condition: None,
        executor: HookExecutor::Builtin {
            handler_id: handler_id.into(),
        },
        source: HookSource::Settings(lingxi_core::types::SettingsScope::User),
        blocking: true,
        timeout: None,
        priority: 0,
        once: false,
        status_message: None,
        async_rewake: false,
        async_timeout: None,
        rewake_message: None,
    }
}

#[tokio::test]
async fn scoped_only_child_fires_frontmatter_stop_without_global_snapshot_buffer() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let api = MockSubagentApiClient::new(vec![Ok(text_response("done", Some("end_turn")))]);
    let mut ctx = loop_ctx(api, None, 1);
    ctx.stop_hook_scope = lingxi_core::host::subagent_spawn::SubagentStopScope::AgentScoped;
    ctx.agent_definition.frontmatter_hooks =
        vec![frontmatter_stop_hook("record-subagent-stop-in-runner")];
    let executor = exec_recording_stop(seen.clone());
    ctx.hook_executor = Some(executor.clone());
    let session = ctx.hook_session_id;
    let child = ctx.agent_id;
    let (events, input) = mpsc::channel(8);
    drop(events);
    let (output, results) = mpsc::channel(32);
    run_subagent(ctx, input, output).await;
    assert!(drain(results)
        .await
        .iter()
        .any(|event| matches!(event, SubagentEvent::Completed { .. })));
    assert_eq!(*seen.lock().unwrap(), vec!["completed".to_string()]);
    assert!(
        executor
            .take_agent_prompt_transcript(session, child)
            .is_none(),
        "a child without a global consumer cannot retain a global snapshot"
    );
}

#[tokio::test]
async fn frontmatter_stop_hook_fires_as_subagent_stop_in_runner() {
    // #9: a frontmatter `Stop` hook is retargeted to `SubagentStop`
    // (isAgent=true) and MUST fire at the child loop's clean end — BEFORE
    // `clear_agent_hooks` removes it. A clean (end_turn) run yields one
    // `SubagentStop` with status "completed".
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let api = CapturingApiClient::new();
    let mut ctx = loop_ctx(api.clone(), None, 2);
    ctx.prompt_messages = vec![ConversationMessage::user(MessageId::new(), "go".into())];
    ctx.agent_definition.agent_type = "stop-agent".into();
    ctx.agent_definition.frontmatter_hooks =
        vec![frontmatter_stop_hook("record-subagent-stop-in-runner")];
    ctx.hook_executor = Some(exec_recording_stop(seen.clone()));

    let (event_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    drop(event_tx);
    run_subagent(ctx, event_rx, out_tx).await;
    let _ = drain(out_rx).await;

    let recorded = seen.lock().unwrap().clone();
    assert_eq!(
        recorded,
        vec!["completed".to_string()],
        "frontmatter Stop→SubagentStop must fire exactly once (status completed): {recorded:?}"
    );
}

#[tokio::test]
async fn runner_subagent_stop_context_carries_final_assistant_text() {
    let seen = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
    let api = CapturingApiClient::new();
    let mut ctx = loop_ctx(api.clone(), None, 2);
    ctx.prompt_messages = vec![ConversationMessage::user(MessageId::new(), "go".into())];
    ctx.agent_definition.agent_type = "stop-agent".into();
    ctx.agent_definition.frontmatter_hooks =
        vec![frontmatter_stop_hook("record-subagent-stop-context")];
    ctx.hook_executor = Some(exec_recording_stop_context(seen.clone()));

    let (event_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    drop(event_tx);
    run_subagent(ctx, event_rx, out_tx).await;
    let _ = drain(out_rx).await;

    assert_eq!(
        seen.lock().unwrap().clone(),
        vec![Some("done".to_string())],
        "agent-scoped SubagentStop should carry the final assistant text"
    );
}

#[tokio::test]
async fn no_frontmatter_hooks_means_no_runner_subagent_stop() {
    // With NO frontmatter hooks the runner takes the passthrough path and
    // fires NO agent-scoped SubagentStop (the orchestrator chokepoint owns
    // session/plugin SubagentStop). The recorder sees nothing.
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let api = CapturingApiClient::new();
    let mut ctx = loop_ctx(api.clone(), None, 2);
    ctx.prompt_messages = vec![ConversationMessage::user(MessageId::new(), "go".into())];
    // No frontmatter_hooks (default empty). Executor still wired.
    ctx.hook_executor = Some(exec_recording_stop(seen.clone()));

    let (event_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    drop(event_tx);
    run_subagent(ctx, event_rx, out_tx).await;
    let _ = drain(out_rx).await;

    assert!(
        seen.lock().unwrap().is_empty(),
        "no frontmatter hooks ⇒ no agent-scoped SubagentStop fired"
    );
}

/// Counts every SubagentStart and SubagentStop event the runner fires, so a
/// test can assert each canonical lifecycle hook fires EXACTLY once through
/// the REAL runner (R7 — no double-fire).
struct StartStopCounter {
    starts: Arc<Mutex<u32>>,
    stops: Arc<Mutex<Vec<String>>>,
}
#[async_trait]
impl hooks::executor::BuiltinHookHandler for StartStopCounter {
    fn id(&self) -> &str {
        "r7-start-stop-counter"
    }
    async fn handle(
        &self,
        event: &hooks::events::HookEvent,
        _ctx: &hooks::registry::HookContext,
    ) -> hooks::response::HookResult {
        match event {
            hooks::events::HookEvent::SubagentStart { .. } => {
                *self.starts.lock().unwrap() += 1;
            }
            hooks::events::HookEvent::SubagentStop { status, .. } => {
                self.stops.lock().unwrap().push(status.clone());
            }
            _ => {}
        }
        hooks::response::HookResult {
            outcome: hooks::response::HookOutcome::Success,
            stdout: String::new(),
            stderr: String::new(),
            exit_code: None,
            response: None,
        }
    }
}

#[tokio::test]
async fn runner_fires_subagent_start_and_frontmatter_stop_exactly_once_each() {
    // R7 integration: a REAL runner run with BOTH a SubagentStart hook AND a
    // frontmatter `Stop`→`SubagentStop` hook (isAgent=true) must fire
    // SubagentStart EXACTLY once (the canonical, additionalContext-collecting
    // fire) and the frontmatter SubagentStop EXACTLY once (agent-scoped,
    // BEFORE clear_agent_hooks). This is the assertion the orchestrator-side
    // FakeAgentTool fixtures (no runner) cannot make.
    use hooks::definition::{HookDefinition, HookExecutor, HookSource};
    use hooks::events::HookEventType;

    let starts = Arc::new(Mutex::new(0u32));
    let stops = Arc::new(Mutex::new(Vec::<String>::new()));

    // One executor with: a SESSION-level SubagentStart hook (fires in G4) and
    // the frontmatter Stop hook is supplied via `frontmatter_hooks` below
    // (the runner registers + retargets it to SubagentStop, isAgent=true).
    let registry = Arc::new(tokio::sync::RwLock::new(hooks::HookRegistry::new()));
    registry.write().await.register(HookDefinition {
        id: lingxi_core::types::HookId::new(),
        name: "r7-start".into(),
        events: vec![HookEventType::SubagentStart],
        if_condition: None,
        executor: HookExecutor::Builtin {
            handler_id: "r7-start-stop-counter".into(),
        },
        source: HookSource::Settings(lingxi_core::types::SettingsScope::User),
        blocking: true,
        timeout: None,
        priority: 0,
        once: false,
        status_message: None,
        async_rewake: false,
        async_timeout: None,
        rewake_message: None,
    });
    let mut exec = hooks::HookExecutorImpl::new(
        registry,
        Arc::new(test_harness::mocks::MockHttpTransport::new()),
        Arc::new(test_harness::mocks::MockRuntimeSpawner::default()),
    );
    exec.register_builtin(Arc::new(StartStopCounter {
        starts: starts.clone(),
        stops: stops.clone(),
    }));
    let exec = Arc::new(exec);

    let api = CapturingApiClient::new();
    let mut ctx = loop_ctx(api.clone(), None, 2);
    ctx.prompt_messages = vec![ConversationMessage::user(MessageId::new(), "go".into())];
    ctx.agent_definition.agent_type = "r7-agent".into();
    // The frontmatter Stop hook points at the SAME counter handler; the
    // runner retargets Stop→SubagentStop (isAgent=true) and fires it
    // agent-scoped at the loop's clean end.
    ctx.agent_definition.frontmatter_hooks = vec![frontmatter_stop_hook("r7-start-stop-counter")];
    ctx.hook_executor = Some(exec);

    let (event_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    drop(event_tx);
    run_subagent(ctx, event_rx, out_tx).await;
    let _ = drain(out_rx).await;

    assert_eq!(
        *starts.lock().unwrap(),
        1,
        "SubagentStart must fire EXACTLY once through the real runner (no double-fire)"
    );
    assert_eq!(
        stops.lock().unwrap().clone(),
        vec!["completed".to_string()],
        "frontmatter Stop→SubagentStop must fire EXACTLY once (status completed) in the runner"
    );
}

/// Mock [`SkillLoader`] that resolves a fixed name to canned content, else None.
struct MockSkillLoader {
    known: String,
    content_text: String,
    expected_model: Option<String>,
}
#[async_trait]
impl lingxi_core::host::skill_loader::SkillLoader for MockSkillLoader {
    async fn resolve_and_load(
        &self,
        skill_name: &str,
        _agent_type: &str,
        _cwd: Option<&std::path::Path>,
        model: Option<&str>,
    ) -> Result<Option<lingxi_core::host::skill_loader::SkillLoad>, String> {
        if let Some(expected) = &self.expected_model {
            assert_eq!(model, Some(expected.as_str()));
        }
        Ok(if skill_name == self.known {
            Some(lingxi_core::host::skill_loader::SkillLoad {
                display_name: skill_name.to_string(),
                progress_message: None,
                content: vec![ContentBlock::Text {
                    text: self.content_text.clone(),
                    citations: None,
                }],
            })
        } else {
            None
        })
    }
}

fn user_text(msg: &ConversationMessage) -> Option<String> {
    if let ConversationMessage::User { content, .. } = msg {
        let t: Vec<String> = content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        Some(t.join("\n"))
    } else {
        None
    }
}

#[tokio::test]
async fn subagent_start_additional_context_injected_as_system_reminder() {
    // G4: a SubagentStart hook's additionalContext lands as a
    // `<system-reminder>` user message in the child's initial history,
    // AFTER the prompt seed and BEFORE turn 1.
    let api = CapturingApiClient::new();
    let mut ctx = loop_ctx(api.clone(), None, 2);
    ctx.prompt_messages = vec![ConversationMessage::user(MessageId::new(), "do it".into())];
    ctx.hook_executor = Some(exec_with_start_context("extra from hook").await);

    let (event_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    drop(event_tx);
    run_subagent(ctx, event_rx, out_tx).await;
    let _ = drain(out_rx).await;

    let msgs = api.captured();
    // The prompt is first, the injected system-reminder follows it.
    let texts: Vec<String> = msgs.iter().filter_map(user_text).collect();
    assert!(
        texts.iter().any(|t| t == "do it"),
        "prompt seed present: {texts:?}"
    );
    assert!(
        texts.iter().any(|t| t
            == "<system-reminder>\nSubagentStart hook additional context: extra from hook\n</system-reminder>"),
        "additionalContext injected as the claude-byte <system-reminder> message: {texts:?}"
    );
}

// ── G008 (Fusion panel is hook-silent) ───────────────────────────────────

/// A `SubagentStart` builtin hook handler that BOTH counts every invocation
/// AND returns an `additionalContext` string, so a single fixture can pin
/// both halves of G008 at once: whether the hook machinery fired at all
/// (the count) and whether the message it would have produced landed in
/// history (the text). A hook that fires but returns nothing useful would
/// pass a text-only assertion while still violating the "chokepoint already
/// accounts for Fusion as ONE hook pair" contract in `build_preload_messages`
/// — hence asserting the call count separately from the message.
struct CountingAdditionalContextStartHook {
    calls: Arc<Mutex<u32>>,
    context: String,
}
#[async_trait]
impl hooks::executor::BuiltinHookHandler for CountingAdditionalContextStartHook {
    fn id(&self) -> &str {
        "g008-counting-start-hook"
    }
    async fn handle(
        &self,
        _event: &hooks::events::HookEvent,
        _ctx: &hooks::registry::HookContext,
    ) -> hooks::response::HookResult {
        *self.calls.lock().unwrap() += 1;
        hooks::response::HookResult {
            outcome: hooks::response::HookOutcome::Success,
            stdout: String::new(),
            stderr: String::new(),
            exit_code: None,
            response: Some(hooks::response::HookResponse {
                additional_context: Some(self.context.clone().into()),
                ..Default::default()
            }),
        }
    }
}

/// Build an `Arc<HookExecutorImpl>` with ONE registered SubagentStart hook
/// that both counts its invocations into `calls` and would inject `context`
/// as additionalContext if it fires.
async fn exec_with_counting_start_context(
    calls: Arc<Mutex<u32>>,
    context: &str,
) -> Arc<hooks::HookExecutorImpl> {
    use hooks::definition::{HookDefinition, HookExecutor, HookSource};
    use hooks::events::HookEventType;
    let registry = Arc::new(tokio::sync::RwLock::new(hooks::HookRegistry::new()));
    registry.write().await.register(HookDefinition {
        id: lingxi_core::types::HookId::new(),
        name: "g008-counting-start".into(),
        events: vec![HookEventType::SubagentStart],
        if_condition: None,
        executor: HookExecutor::Builtin {
            handler_id: "g008-counting-start-hook".into(),
        },
        source: HookSource::Settings(lingxi_core::types::SettingsScope::User),
        blocking: true,
        timeout: None,
        priority: 0,
        once: false,
        status_message: None,
        async_rewake: false,
        async_timeout: None,
        rewake_message: None,
    });
    let mut exec = hooks::HookExecutorImpl::new(
        registry,
        Arc::new(test_harness::mocks::MockHttpTransport::new()),
        Arc::new(test_harness::mocks::MockRuntimeSpawner::default()),
    );
    exec.register_builtin(Arc::new(CountingAdditionalContextStartHook {
        calls,
        context: context.to_string(),
    }));
    Arc::new(exec)
}

#[tokio::test]
async fn fusion_panel_agent_type_fires_no_subagent_start_hook() {
    // G008 runner half: a `fusion-panel` child must invoke the SubagentStart
    // hook machinery ZERO times — the orchestrator chokepoint (turn_loop.rs)
    // already accounts for a whole Fusion run as a SINGLE hook pair via
    // `fusion_tool_result`'s `subagentHooksFired` marker. Deleting the
    // `agent_type != FUSION_PANEL_TYPE` guard in `build_preload_messages`
    // must turn this red.
    let calls = Arc::new(Mutex::new(0u32));
    let api = CapturingApiClient::new();
    let mut ctx = loop_ctx(api.clone(), None, 2);
    ctx.prompt_messages = vec![ConversationMessage::user(MessageId::new(), "go".into())];
    ctx.agent_definition.agent_type = lingxi_core::host::FUSION_PANEL_TYPE.into();
    ctx.hook_executor = Some(exec_with_counting_start_context(calls.clone(), "panel-marker").await);

    let (event_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    drop(event_tx);
    run_subagent(ctx, event_rx, out_tx).await;
    let _ = drain(out_rx).await;

    assert_eq!(
        *calls.lock().unwrap(),
        0,
        "fusion-panel agent_type must fire the SubagentStart hook handler ZERO times"
    );
    let texts: Vec<String> = api.captured().iter().filter_map(user_text).collect();
    assert!(
        texts
            .iter()
            .all(|t| !t.contains("SubagentStart hook additional context")),
        "fusion-panel history must not contain a SubagentStart additionalContext message: {texts:?}"
    );
}

#[tokio::test]
async fn ordinary_agent_type_still_fires_subagent_start_hook_once() {
    // Companion to `fusion_panel_agent_type_fires_no_subagent_start_hook`:
    // proves the G008 gate is scoped to EXACTLY `fusion-panel` and cannot be
    // widened (e.g. inverted, or matched on a prefix) to also swallow
    // ordinary Agent-tool children — the hook must still fire once and its
    // additionalContext message must still land in history.
    let calls = Arc::new(Mutex::new(0u32));
    let api = CapturingApiClient::new();
    let mut ctx = loop_ctx(api.clone(), None, 2);
    ctx.prompt_messages = vec![ConversationMessage::user(MessageId::new(), "go".into())];
    ctx.agent_definition.agent_type = "ordinary-agent".into();
    ctx.hook_executor =
        Some(exec_with_counting_start_context(calls.clone(), "ordinary-marker").await);

    let (event_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    drop(event_tx);
    run_subagent(ctx, event_rx, out_tx).await;
    let _ = drain(out_rx).await;

    assert_eq!(
        *calls.lock().unwrap(),
        1,
        "ordinary agent_type must still fire the SubagentStart hook handler exactly once"
    );
    let texts: Vec<String> = api.captured().iter().filter_map(user_text).collect();
    assert!(
        texts.iter().any(|t| t
            == "<system-reminder>\nSubagentStart hook additional context: ordinary-marker\n</system-reminder>"),
        "ordinary agent_type history must contain the additionalContext message: {texts:?}"
    );
}

#[tokio::test]
async fn mobile_runtime_reminder_is_the_fixed_prefix_before_task_and_hooks() {
    let api = CapturingApiClient::new();
    let mut ctx = loop_ctx(api.clone(), None, 2);
    ctx.prompt_messages = vec![ConversationMessage::user(MessageId::new(), "do it".into())];
    ctx.mobile_runtime_environment_reminder = Some(Arc::from(
        "<system-reminder>\nMOBILE RUNTIME\n</system-reminder>",
    ));
    ctx.mobile_runtime_workspace_reminder = Some(Arc::from(
        "<system-reminder>\nMOBILE WORKSPACE\n</system-reminder>",
    ));
    ctx.hook_executor = Some(exec_with_start_context("extra from hook").await);

    let (event_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    drop(event_tx);
    run_subagent(ctx, event_rx, out_tx).await;
    let _ = drain(out_rx).await;

    let texts: Vec<String> = api.captured().iter().filter_map(user_text).collect();
    assert_eq!(
        texts[0],
        "<system-reminder>\nMOBILE RUNTIME\n</system-reminder>"
    );
    assert_eq!(
        texts[1],
        "<system-reminder>\nMOBILE WORKSPACE\n</system-reminder>"
    );
    assert_eq!(texts[2], "do it");
    assert!(texts[3].contains("SubagentStart hook additional context"));
}

#[tokio::test]
async fn subagent_start_multiple_contexts_join_into_one_reminder() {
    // G4 byte-parity (runAgent.ts:530-555 + messages.ts:4117-4128): when
    // multiple SubagentStart hooks each return additionalContext, claude
    // collects them into ONE `string[]` and emits a SINGLE
    // `hook_additional_context` attachment whose body is
    // `SubagentStart hook additional context: ` + contexts.join("\n").
    let api = CapturingApiClient::new();
    let mut ctx = loop_ctx(api.clone(), None, 2);
    ctx.prompt_messages = vec![ConversationMessage::user(MessageId::new(), "do it".into())];
    ctx.hook_executor = Some(exec_with_two_start_contexts("alpha", "beta").await);

    let (event_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    drop(event_tx);
    run_subagent(ctx, event_rx, out_tx).await;
    let _ = drain(out_rx).await;

    let texts: Vec<String> = api.captured().iter().filter_map(user_text).collect();
    // Exactly ONE system-reminder message (not one per context).
    let reminders: Vec<&String> = texts
        .iter()
        .filter(|t| t.starts_with("<system-reminder>\nSubagentStart hook additional context: "))
        .collect();
    assert_eq!(
        reminders.len(),
        1,
        "exactly one joined SubagentStart reminder: {texts:?}"
    );
    assert_eq!(
        reminders[0],
        "<system-reminder>\nSubagentStart hook additional context: alpha\nbeta\n</system-reminder>",
        "contexts joined with \\n in a single reminder"
    );
}

#[tokio::test]
async fn no_hook_executor_means_no_preload_injection() {
    // G4: no SubagentStart fire or skill preload. The normal .286 date
    // announcement still follows the prompt seed.
    let expected_date = format!(
        "<system-reminder>\nToday's date is {}.\n</system-reminder>",
        chrono::Local::now().format("%Y-%m-%d")
    );
    let api = CapturingApiClient::new();
    let mut ctx = loop_ctx(api.clone(), None, 2);
    ctx.prompt_messages = vec![ConversationMessage::user(MessageId::new(), "do it".into())];
    // hook_executor + skill_loader both unset (default).

    let (event_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    drop(event_tx);
    run_subagent(ctx, event_rx, out_tx).await;
    let _ = drain(out_rx).await;

    let msgs = api.captured();
    assert_eq!(
        msgs.len(),
        2,
        "prompt seed and native date announcement: {msgs:?}"
    );
    assert_eq!(user_text(&msgs[0]).as_deref(), Some("do it"));
    assert!(matches!(
        &msgs[0],
        ConversationMessage::User { is_meta: false, .. }
    ));
    assert_eq!(user_text(&msgs[1]).as_deref(), Some(expected_date.as_str()));
    assert!(matches!(
        &msgs[1],
        ConversationMessage::User {
            is_meta: true,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
            ..
        }
    ));
    for text in msgs.iter().filter_map(user_text) {
        assert!(!text.contains("SubagentStart hook additional context"));
        assert!(!text.contains("<skill-format>"));
    }
}

#[tokio::test]
async fn resolved_skill_prepends_metadata_then_content() {
    // G5: a resolved skill is injected as a user message whose first block is
    // the byte-locked loading metadata, followed by the loaded content.
    let api = CapturingApiClient::new();
    let mut ctx = loop_ctx(api.clone(), None, 2);
    ctx.prompt_messages = vec![ConversationMessage::user(MessageId::new(), "go".into())];
    ctx.agent_definition.agent_type = "my-agent".into();
    ctx.agent_definition.skills = vec!["my-skill".into()];
    ctx.agent_definition.model = crate::definition::AgentModel::Explicit("claude-fable-5".into());
    ctx.skill_loader = Some(Arc::new(MockSkillLoader {
        known: "my-skill".into(),
        content_text: "SKILL BODY".into(),
        expected_model: Some("claude-fable-5".into()),
    }));

    let (event_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    drop(event_tx);
    run_subagent(ctx, event_rx, out_tx).await;
    let _ = drain(out_rx).await;

    let msgs = api.captured();
    // The skill meta message carries the <skill-format> marker block first,
    // then the loaded content.
    let skill_text = msgs
        .iter()
        .filter_map(user_text)
        .find(|t| t.contains("<skill-format>true</skill-format>"))
        .expect("skill meta message present");
    assert_eq!(
        skill_text,
        "<command-message>my-skill</command-message>\n\
<command-name>my-skill</command-name>\n\
<skill-format>true</skill-format>\nSKILL BODY",
        "metadata block then content"
    );
}

#[tokio::test]
async fn missing_skill_is_skipped_no_message() {
    // G5: an unresolved skill injects NOTHING (claude logs the warn + skips).
    let expected_date = format!(
        "<system-reminder>\nToday's date is {}.\n</system-reminder>",
        chrono::Local::now().format("%Y-%m-%d")
    );
    let api = CapturingApiClient::new();
    let mut ctx = loop_ctx(api.clone(), None, 2);
    ctx.prompt_messages = vec![ConversationMessage::user(MessageId::new(), "go".into())];
    ctx.agent_definition.agent_type = "my-agent".into();
    ctx.agent_definition.skills = vec!["nope".into()];
    ctx.skill_loader = Some(Arc::new(MockSkillLoader {
        known: "my-skill".into(),
        content_text: "SKILL BODY".into(),
        expected_model: None,
    }));

    let (event_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    drop(event_tx);
    run_subagent(ctx, event_rx, out_tx).await;
    let _ = drain(out_rx).await;

    let msgs = api.captured();
    assert_eq!(
        msgs.len(),
        2,
        "prompt seed and native date announcement (missing skill skipped): {msgs:?}"
    );
    assert_eq!(user_text(&msgs[0]).as_deref(), Some("go"));
    assert!(matches!(
        &msgs[0],
        ConversationMessage::User { is_meta: false, .. }
    ));
    assert_eq!(user_text(&msgs[1]).as_deref(), Some(expected_date.as_str()));
    assert!(matches!(
        &msgs[1],
        ConversationMessage::User {
            is_meta: true,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
            ..
        }
    ));
    for text in msgs.iter().filter_map(user_text) {
        assert!(!text.contains("SubagentStart hook additional context"));
        assert!(!text.contains("<skill-format>"));
        assert!(!text.contains("SKILL BODY"));
    }
}

#[tokio::test]
async fn preload_order_additional_context_then_skills() {
    // Ordering parity (runAgent.ts 530→577): additionalContext message(s)
    // come BEFORE the skills message(s) in the seeded history.
    let api = CapturingApiClient::new();
    let mut ctx = loop_ctx(api.clone(), None, 2);
    ctx.prompt_messages = vec![ConversationMessage::user(MessageId::new(), "go".into())];
    ctx.agent_definition.agent_type = "my-agent".into();
    ctx.agent_definition.skills = vec!["my-skill".into()];
    ctx.hook_executor = Some(exec_with_start_context("ctx0").await);
    ctx.skill_loader = Some(Arc::new(MockSkillLoader {
        known: "my-skill".into(),
        content_text: "SKILL BODY".into(),
        expected_model: None,
    }));

    let (event_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    drop(event_tx);
    run_subagent(ctx, event_rx, out_tx).await;
    let _ = drain(out_rx).await;

    let texts: Vec<String> = api.captured().iter().filter_map(user_text).collect();
    let ac_idx = texts
        .iter()
        .position(|t| t.contains("ctx0"))
        .expect("additionalContext present");
    let skill_idx = texts
        .iter()
        .position(|t| t.contains("<skill-format>"))
        .expect("skill present");
    assert!(
        ac_idx < skill_idx,
        "additionalContext must precede skills: {texts:?}"
    );
}

// ── P1-04: CC 2.1.207 subagent `api_error_partial` recovery ──────────────
//
// On a mid-stream API termination whose kind is in `CTy`
// ({rate_limit,overloaded,server_error}) AND with content already produced, the
// runner recovers the partial work as a `completed` result with the byte-locked
// `cutoffNote` prepended — instead of failing the tool and discarding the
// child's work (`Wyd`/`wTy`, AgentTool sync recovery).

/// Streaming mock whose per-turn scripts are RAW `Result<HistoryEvent, LlmError>`
/// sequences, so a turn can inject a trailing MID-STREAM `Err` (no
/// `message_stop`). Overrides the streaming seam; the non-streaming path is
/// unreachable.
struct ResultStreamMockApiClient {
    turns: Mutex<VecDeque<Vec<Result<llm_runtime::HistoryEvent, llm_runtime::LlmError>>>>,
    calls: AtomicUsize,
    histories: Mutex<Vec<Vec<ConversationMessage>>>,
    retry_stop: Mutex<Option<Arc<ParseRetryStop>>>,
    workflow_watchdog: Option<lingxi_core::host::WorkflowQueryWatchdog>,
}
impl ResultStreamMockApiClient {
    fn new(turns: Vec<Vec<Result<llm_runtime::HistoryEvent, llm_runtime::LlmError>>>) -> Arc<Self> {
        Arc::new(Self {
            turns: Mutex::new(turns.into_iter().collect()),
            calls: AtomicUsize::new(0),
            histories: Mutex::new(Vec::new()),
            retry_stop: Mutex::new(None),
            workflow_watchdog: None,
        })
    }
    fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}
#[async_trait]
impl crate::api::SubagentApiClient for ResultStreamMockApiClient {
    async fn stream(
        &self,
        request: crate::api::SubagentApiRequest,
    ) -> Result<
        futures::stream::BoxStream<
            'static,
            Result<llm_runtime::HistoryEvent, llm_runtime::LlmError>,
        >,
        llm_runtime::LlmError,
    > {
        self.histories.lock().unwrap().push(request.messages);
        self.calls.fetch_add(1, Ordering::SeqCst);
        let events = self.turns.lock().unwrap().pop_front().unwrap_or_default();
        Ok(futures::StreamExt::boxed(futures::stream::iter(events)))
    }

    fn workflow_query_watchdog(&self) -> Option<lingxi_core::host::WorkflowQueryWatchdog> {
        self.workflow_watchdog
    }

    // Accept registered contexts so the scripted malformed response reaches
    // the runner's ownership guard instead of the trait's adapter guard.

    async fn observe_workflow_query_retry(&self, agent_id: AgentId, attempt: u32, reason: String) {
        let stop = self.retry_stop.lock().unwrap().clone();
        if let Some(stop) = stop {
            lingxi_core::host::SubagentSpawnObserver::on_event(
                stop.as_ref(),
                lingxi_core::host::SubagentObservation::Retry {
                    agent_id,
                    attempt,
                    reason,
                },
            )
            .await;
        }
    }
}

/// A partial streamed turn: one COMPLETED text block, then a mid-stream `Err`
/// (no `message_stop`). The block is salvageable; the error is not.
fn partial_text_then_err(
    text: &str,
    err: llm_runtime::LlmError,
) -> Vec<Result<llm_runtime::HistoryEvent, llm_runtime::LlmError>> {
    use llm_runtime::{ContentBlock as LB, HistoryContentDelta, HistoryEvent};
    vec![
        Ok(ev_message_start()),
        Ok(HistoryEvent::ContentBlockStart {
            index: 0,
            content_block: LB::Text {
                citations: None,
                text: String::new(),
                cache_control: None,
            },
        }),
        Ok(HistoryEvent::ContentBlockDelta {
            index: 0,
            delta: HistoryContentDelta::TextDelta { text: text.into() },
        }),
        Ok(HistoryEvent::ContentBlockStop { index: 0 }),
        Err(err),
    ]
}

fn response_body_transport_error() -> llm_runtime::LlmError {
    llm_runtime::LlmError::Transport {
        message: "connection failed: error decoding response body".into(),
    }
}

#[tokio::test]
async fn generic_workflow_does_not_retry_response_body_transport_errors() {
    let api = ResultStreamMockApiClient::new(vec![
        vec![Err(response_body_transport_error())],
        valid_design_turn(),
    ]);
    let wrapped = Arc::new(crate::api::WorkflowWatchdogApiClient::new(
        api.clone(),
        lingxi_core::host::WorkflowQueryWatchdog::default(),
        Vec::new(),
    ));
    let ctx = loop_ctx(wrapped, None, 6);
    let (_tx, rx) = mpsc::channel(8);
    let (out, events) = mpsc::channel(64);
    run_subagent(ctx, rx, out).await;
    let events = drain(events).await;
    assert_eq!(api.call_count(), 1);
    assert!(events.iter().any(|event| matches!(event,
        SubagentEvent::Failed { error, .. } if error.contains("error decoding response body")
    )));
}

#[tokio::test]
async fn generic_workflow_watchdog_preserves_server_content_retry_and_usage_behavior() {
    let api = ResultStreamMockApiClient::new(vec![
        vec![
            Ok(ev_message_start()),
            Ok(HistoryEvent::ContentBlockStart {
                index: 0,
                content_block: llm_runtime::ContentBlock::ServerToolUse {
                    id: "server-1".into(),
                    name: "remote_action".into(),
                    input: serde_json::json!({}),
                },
            }),
            Err(workflow_watchdog_timeout_error(
                "waiting for the next response event",
                Duration::from_secs(1),
            )),
        ],
        streamed_text_turn("done", "end_turn")
            .into_iter()
            .map(Ok)
            .collect(),
    ]);
    let wrapped = Arc::new(crate::api::WorkflowWatchdogApiClient::new(
        api.clone(),
        lingxi_core::host::WorkflowQueryWatchdog::default(),
        Vec::new(),
    ));
    let ctx = loop_ctx(wrapped, None, 6);
    let (_tx, rx) = mpsc::channel(8);
    let (out, events) = mpsc::channel(64);
    run_subagent(ctx, rx, out).await;
    let events = drain(events).await;
    assert_eq!(
        api.call_count(),
        2,
        "default watchdog behavior is unchanged"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            SubagentEvent::Completed {
                usage_complete: true,
                ..
            }
        )),
        "events: {events:?}"
    );
}

#[tokio::test]
async fn local_app_create_transport_retry_preserves_completed_scaffold_and_read() {
    let api = ResultStreamMockApiClient::new(vec![
        streamed_tool_use_turn("LocalAppScaffold", "tool_use")
            .into_iter()
            .map(Ok)
            .collect(),
        streamed_tool_use_turn("Read", "tool_use")
            .into_iter()
            .map(Ok)
            .collect(),
        partial_text_then_err("Preparing source", response_body_transport_error()),
        valid_design_turn(),
    ]);
    let observer = Arc::new(RetryObserver::default());
    let wrapped = Arc::new(crate::api::WorkflowWatchdogApiClient::new(
        api.clone(),
        lingxi_core::host::WorkflowQueryWatchdog {
            stall_timeout_ms: 60_000,
            max_retries: 1,
            retry_response_body: true,
        },
        vec![observer.clone()],
    ));
    let invoker = CountingInvoker::new();
    let mut ctx = loop_ctx(wrapped, Some(invoker.clone()), 6);
    enable_design_parse_recovery(&mut ctx);
    let (_tx, rx) = mpsc::channel(8);
    let (out, events) = mpsc::channel(64);
    run_subagent(ctx, rx, out).await;
    let events = drain(events).await;

    assert_eq!(api.call_count(), 4, "retry the interrupted model request");
    assert_eq!(invoker.call_count(), 2, "never replay scaffold or read");
    let histories = api.histories.lock().unwrap();
    assert_eq!(histories[2], histories[3]);
    drop(histories);
    assert!(events.iter().any(|event| matches!(event,
        SubagentEvent::Completed { result, usage_complete: false, .. }
            if result == &serde_json::json!({"design": {}})
    )));
    assert!(!events
        .iter()
        .any(|event| matches!(event, SubagentEvent::Failed { .. })));
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while observer.attempts.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("retry observer delivered");
    assert_eq!(*observer.attempts.lock().unwrap(), vec![2]);
}

#[tokio::test]
async fn local_app_create_transport_retry_shares_watchdog_limit_and_fails_closed() {
    let api = ResultStreamMockApiClient::new(vec![
        vec![Err(response_body_transport_error())],
        vec![Err(workflow_watchdog_timeout_error(
            "waiting for the next response event",
            Duration::from_secs(1),
        ))],
        vec![Err(response_body_transport_error())],
        valid_design_turn(),
    ]);
    let wrapped = Arc::new(crate::api::WorkflowWatchdogApiClient::new(
        api.clone(),
        lingxi_core::host::WorkflowQueryWatchdog {
            stall_timeout_ms: 60_000,
            max_retries: 2,
            retry_response_body: true,
        },
        Vec::new(),
    ));
    let mut ctx = loop_ctx(wrapped, None, 6);
    enable_design_parse_recovery(&mut ctx);
    let (_tx, rx) = mpsc::channel(8);
    let (out, events) = mpsc::channel(64);
    run_subagent(ctx, rx, out).await;
    let events = drain(events).await;
    assert_eq!(api.call_count(), 3);
    assert!(events.iter().any(|event| matches!(event,
        SubagentEvent::Failed { error, .. } if error.contains("error decoding response body")
    )));
    assert!(!events
        .iter()
        .any(|event| matches!(event, SubagentEvent::Completed { .. })));
}

#[tokio::test]
async fn local_app_create_transport_retry_discards_tools_from_interrupted_response() {
    let mut interrupted: Vec<_> = streamed_tool_use_turn("Write", "tool_use")
        .into_iter()
        .take_while(|event| {
            !matches!(
                event,
                HistoryEvent::MessageDelta { .. } | HistoryEvent::MessageStop
            )
        })
        .map(Ok)
        .collect();
    interrupted.push(Err(response_body_transport_error()));
    let api = ResultStreamMockApiClient::new(vec![interrupted, valid_design_turn()]);
    let wrapped = Arc::new(crate::api::WorkflowWatchdogApiClient::new(
        api.clone(),
        lingxi_core::host::WorkflowQueryWatchdog {
            stall_timeout_ms: 60_000,
            max_retries: 1,
            retry_response_body: true,
        },
        Vec::new(),
    ));
    let invoker = CountingInvoker::new();
    let mut ctx = loop_ctx(wrapped, Some(invoker.clone()), 4);
    enable_design_parse_recovery(&mut ctx);
    let (_tx, rx) = mpsc::channel(8);
    let (out, events) = mpsc::channel(64);
    run_subagent(ctx, rx, out).await;
    let events = drain(events).await;
    assert_eq!(api.call_count(), 2);
    assert_eq!(
        invoker.call_count(),
        0,
        "discard even a completed local tool block when its response is interrupted"
    );
    let histories = api.histories.lock().unwrap();
    assert_eq!(histories[0], histories[1]);
    assert!(events.iter().any(|event| matches!(event,
        SubagentEvent::Completed { result, usage_complete: false, .. }
            if result == &serde_json::json!({"design": {}})
    )));
}

#[tokio::test]
async fn local_app_create_transport_retry_never_replays_an_unclosed_server_tool() {
    for error in [
        response_body_transport_error(),
        workflow_watchdog_timeout_error(
            "waiting for the next response event",
            Duration::from_secs(1),
        ),
    ] {
        let api = ResultStreamMockApiClient::new(vec![
            vec![
                Ok(ev_message_start()),
                Ok(HistoryEvent::ContentBlockStart {
                    index: 0,
                    content_block: llm_runtime::ContentBlock::ServerToolUse {
                        id: "server-1".into(),
                        name: "remote_action".into(),
                        input: serde_json::json!({}),
                    },
                }),
                Err(error),
            ],
            valid_design_turn(),
        ]);
        let wrapped = Arc::new(crate::api::WorkflowWatchdogApiClient::new(
            api.clone(),
            lingxi_core::host::WorkflowQueryWatchdog {
                stall_timeout_ms: 60_000,
                max_retries: 2,
                retry_response_body: true,
            },
            Vec::new(),
        ));
        let mut ctx = loop_ctx(wrapped, None, 6);
        enable_design_parse_recovery(&mut ctx);
        let (_tx, rx) = mpsc::channel(8);
        let (out, events) = mpsc::channel(64);
        run_subagent(ctx, rx, out).await;
        let events = drain(events).await;
        assert_eq!(
            api.call_count(),
            1,
            "a server tool may already have executed"
        );
        assert!(events
            .iter()
            .any(|event| matches!(event, SubagentEvent::Failed { .. })));
    }
}

#[tokio::test]
async fn local_app_create_transport_retry_is_not_a_generic_error_retry() {
    for (workflow, failure) in [
        (false, vec![Err(response_body_transport_error())]),
        (
            true,
            vec![Err(llm_runtime::LlmError::InvalidRequest {
                message: "invalid request".into(),
            })],
        ),
        (
            true,
            vec![Err(llm_runtime::LlmError::Authentication {
                message: "expired credential".into(),
            })],
        ),
        (true, malformed_structured_turn("Write")),
    ] {
        let api = ResultStreamMockApiClient::new(vec![failure, valid_design_turn()]);
        let client: Arc<dyn crate::api::SubagentApiClient> = if workflow {
            Arc::new(crate::api::WorkflowWatchdogApiClient::new(
                api.clone(),
                lingxi_core::host::WorkflowQueryWatchdog {
                    stall_timeout_ms: 60_000,
                    max_retries: 2,
                    retry_response_body: true,
                },
                Vec::new(),
            ))
        } else {
            api.clone()
        };
        let mut ctx = loop_ctx(client, None, 6);
        enable_design_parse_recovery(&mut ctx);
        let (_tx, rx) = mpsc::channel(8);
        let (out, events) = mpsc::channel(64);
        run_subagent(ctx, rx, out).await;
        let events = drain(events).await;
        assert_eq!(api.call_count(), 1);
        assert!(events
            .iter()
            .any(|event| matches!(event, SubagentEvent::Failed { .. })));
    }
}

/// The exact `cutoffNote` for a server-error-class termination: the
/// `AgentApiErrorTerminationError` message + the byte-locked incomplete-output
/// notice (`\u{2014}` = em dash), joined by a blank line (two newlines), matching
/// CC 2.1.207/2.1.208 `wTy` (`cutoffNote:`+"${e.message}\n\n"+"Everything below…").
const EXPECTED_SERVER_ERROR_CUTOFF: &str = "Agent terminated early due to an API error: API Error: Server error mid-response. The response above may be incomplete.\n\nEverything below is PARTIAL output recovered from the agent before it was cut off. The agent did NOT finish its task \u{2014} treat these results as incomplete.";

/// A workflow role with a schema must report the stream failure, rather than
/// returning a successful prose envelope that bypasses its output contract.
#[tokio::test]
async fn structured_agent_midstream_error_does_not_complete_with_partial_prose() {
    let api = ResultStreamMockApiClient::new(vec![partial_text_then_err(
        "I will prepare the design now.",
        llm_runtime::LlmError::StreamInterrupted {
            message: "stream ended before message_stop".into(),
        },
    )]);
    let mut ctx = loop_ctx(api, None, 10);
    ctx.schema = Some(
        r#"{"type":"object","required":["design"],"properties":{"design":{"type":"object"}}}"#
            .to_string(),
    );
    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    run_subagent(ctx, event_rx, out_tx).await;
    let events = drain(out_rx).await;

    assert!(
        !events
            .iter()
            .any(|event| matches!(event, SubagentEvent::Completed { .. })),
        "partial prose must not satisfy the required design schema: {events:?}"
    );
    let error = events
        .iter()
        .find_map(|event| match event {
            SubagentEvent::Failed { error, .. } => Some(error),
            _ => None,
        })
        .expect("the structured agent must emit Failed");
    assert!(
        error.contains("stream ended before message_stop"),
        "original stream error must survive: {error}"
    );
}

// Drive the real stream parser instead of injecting an already-classified error.
fn malformed_structured_turn(
    tool: &str,
) -> Vec<Result<llm_runtime::HistoryEvent, llm_runtime::LlmError>> {
    let mut events = streamed_tool_use_turn(tool, "tool_use");
    for event in &mut events {
        if let llm_runtime::HistoryEvent::ContentBlockDelta {
            delta: llm_runtime::HistoryContentDelta::InputJsonDelta { partial_json },
            ..
        } = event
        {
            *partial_json = r#"{"design":{}"#.into();
        }
    }
    events.into_iter().map(Ok).collect()
}

fn valid_design_turn() -> Vec<Result<llm_runtime::HistoryEvent, llm_runtime::LlmError>> {
    llm_runtime::stream_accumulator::response_to_stream_events(structured_output_call_response(
        serde_json::json!({"design": {}}),
    ))
    .into_iter()
    .map(Ok)
    .collect()
}

fn enable_design_parse_recovery(ctx: &mut SubagentContext) {
    ctx.schema = Some(r#"{"type":"object","required":["design"],"properties":{"design":{"type":"object"}},"additionalProperties":false}"#.into());
    ctx.structured_output_parse_retries = 2;
}

fn create_parse_recovery_api(
    turns: Vec<Vec<Result<llm_runtime::HistoryEvent, llm_runtime::LlmError>>>,
) -> Arc<ResultStreamMockApiClient> {
    let mut api = ResultStreamMockApiClient::new(turns);
    Arc::get_mut(&mut api).unwrap().workflow_watchdog =
        Some(lingxi_core::host::WorkflowQueryWatchdog {
            stall_timeout_ms: 60_000,
            max_retries: 0,
            retry_response_body: true,
        });
    api
}

fn enable_create_parse_recovery(ctx: &mut SubagentContext) {
    enable_design_parse_recovery(ctx);
    ctx.tool_schemas = ["Write", "Read", "LocalAppScaffold"]
        .into_iter()
        .map(|name| serde_json::json!({"name": name, "input_schema": {"type": "object"}}))
        .collect();
    ctx.allowed_tools = vec!["Write".into(), "Read".into(), "LocalAppScaffold".into()];
}

fn reasoning_only_truncated_turn() -> Vec<Result<llm_runtime::HistoryEvent, llm_runtime::LlmError>>
{
    llm_runtime::stream_accumulator::response_to_stream_events(llm_runtime::HistoryResponse {
        content: vec![llm_runtime::ContentBlock::Reasoning {
            text: "unfinished reasoning".into(),
            signature: None,
        }],
        ..text_response("", Some("max_tokens"))
    })
    .into_iter()
    .map(Ok)
    .collect()
}

#[tokio::test]
async fn local_app_create_truncation_continues_without_replaying_completed_tools() {
    let api = create_parse_recovery_api(vec![
        streamed_tool_use_turn("LocalAppScaffold", "tool_use")
            .into_iter()
            .map(Ok)
            .collect(),
        reasoning_only_truncated_turn(),
        streamed_tool_use_turn("Write", "max_tokens")
            .into_iter()
            .map(Ok)
            .collect(),
        valid_design_turn(),
    ]);
    let invoker = CountingInvoker::new();
    let mut ctx = loop_ctx(api.clone(), Some(invoker.clone()), 6);
    enable_create_parse_recovery(&mut ctx);
    let (_tx, rx) = mpsc::channel(8);
    let (out, events) = mpsc::channel(64);
    run_subagent(ctx, rx, out).await;
    let events = drain(events).await;
    assert_eq!(one_completed(&events), serde_json::json!({"design": {}}));
    assert_eq!(api.call_count(), 4);
    assert_eq!(
        invoker.call_count(),
        2,
        "scaffold and write must each execute once"
    );
    let histories = api.histories.lock().unwrap();
    for history in &histories[2..] {
        let serialized = serde_json::to_string(history).unwrap();
        assert!(serialized.contains("Continue the unfinished work"));
        assert!(!serialized.contains("You MUST call StructuredOutput"));
        assert!(
            serialized.contains("unfinished reasoning"),
            "retain the transcript"
        );
        assert!(
            serialized.contains("tool_result"),
            "retain completed tool results"
        );
    }
}

#[tokio::test]
async fn local_app_create_truncation_recovery_is_bounded_by_retries_and_turns() {
    for (max_turns, expected_calls) in [(8, 3), (1, 1)] {
        let api =
            create_parse_recovery_api((0..3).map(|_| reasoning_only_truncated_turn()).collect());
        let mut ctx = loop_ctx(api.clone(), None, max_turns);
        enable_create_parse_recovery(&mut ctx);
        let (_tx, rx) = mpsc::channel(8);
        let (out, events) = mpsc::channel(64);
        run_subagent(ctx, rx, out).await;
        let events = drain(events).await;
        assert_eq!(api.call_count(), expected_calls);
        assert!(events.iter().any(|event| matches!(event,
            SubagentEvent::Failed { error, .. } if error.contains("output token limit reached before completion")
        )));
        assert!(!events
            .iter()
            .any(|event| matches!(event, SubagentEvent::Completed { .. })));
    }
}

#[tokio::test]
async fn generic_schema_truncation_keeps_existing_structured_output_nudge() {
    let api =
        ResultStreamMockApiClient::new(vec![reasoning_only_truncated_turn(), valid_design_turn()]);
    let mut ctx = loop_ctx(api.clone(), Some(CountingInvoker::new()), 4);
    enable_create_parse_recovery(&mut ctx);
    let (_tx, rx) = mpsc::channel(8);
    let (out, events) = mpsc::channel(64);
    run_subagent(ctx, rx, out).await;
    assert_eq!(
        one_completed(&drain(events).await),
        serde_json::json!({"design": {}})
    );
    let histories = api.histories.lock().unwrap();
    let serialized = serde_json::to_string(&histories[1]).unwrap();
    assert!(serialized.contains("You MUST call StructuredOutput"));
    assert!(!serialized.contains("Continue the unfinished work"));
}

#[tokio::test]
async fn local_app_create_json_correction_preserves_completed_scaffold_and_read() {
    let api = create_parse_recovery_api(vec![
        streamed_tool_use_turn("LocalAppScaffold", "tool_use")
            .into_iter()
            .map(Ok)
            .collect(),
        streamed_tool_use_turn("Read", "tool_use")
            .into_iter()
            .map(Ok)
            .collect(),
        malformed_structured_turn("Write"),
        streamed_tool_use_turn("Write", "tool_use")
            .into_iter()
            .map(Ok)
            .collect(),
        valid_design_turn(),
    ]);
    let invoker = CountingInvoker::new();
    let mut ctx = loop_ctx(api.clone(), Some(invoker.clone()), 8);
    enable_create_parse_recovery(&mut ctx);
    let (_tx, rx) = mpsc::channel(8);
    let (out, events) = mpsc::channel(64);
    run_subagent(ctx, rx, out).await;
    let events = drain(events).await;

    assert_eq!(api.call_count(), 5);
    assert_eq!(
        invoker.call_count(),
        3,
        "scaffold, read, corrected write only"
    );
    assert!(events.iter().any(|event| matches!(event,
        SubagentEvent::Completed { result, usage_complete: false, .. }
            if result == &serde_json::json!({"design": {}})
    )));
    assert!(!events
        .iter()
        .any(|event| matches!(event, SubagentEvent::Failed { .. })));
    let histories = api.histories.lock().unwrap();
    assert!(histories[3].starts_with(&histories[2]));
    assert_eq!(histories[3].len(), histories[2].len() + 1);
    let correction = user_text(histories[3].last().unwrap()).unwrap();
    assert!(correction.contains("Write JSON correction 1/2"));
    assert!(correction.contains("invalid JSON tool input"));
    assert!(correction.contains("Call Write again with one complete, concise JSON object"));
    assert!(correction.contains("do not repeat completed tools"));
    assert!(
        !correction.contains(r#"{"design":{}"#),
        "discard malformed arguments"
    );
}

#[tokio::test]
async fn local_app_create_json_correction_shares_hard_run_cap_with_structured_output() {
    for configured in [2, 99] {
        let api = create_parse_recovery_api(vec![
            malformed_structured_turn("Write"),
            streamed_tool_use_turn("Read", "tool_use")
                .into_iter()
                .map(Ok)
                .collect(),
            malformed_structured_turn("StructuredOutput"),
            malformed_structured_turn("Write"),
            valid_design_turn(),
        ]);
        let invoker = CountingInvoker::new();
        let mut ctx = loop_ctx(api.clone(), Some(invoker.clone()), 10);
        enable_create_parse_recovery(&mut ctx);
        ctx.structured_output_parse_retries = configured;
        let (_tx, rx) = mpsc::channel(8);
        let (out, events) = mpsc::channel(64);
        run_subagent(ctx, rx, out).await;
        let events = drain(events).await;
        assert_eq!(api.call_count(), 4);
        assert_eq!(invoker.call_count(), 1);
        assert!(events.iter().any(|event| matches!(event,
            SubagentEvent::Failed { error, .. }
                if error.contains("Write JSON recovery stopped after 2 retries")
        )));
    }
}

#[tokio::test]
async fn local_app_create_json_correction_requires_host_policy_and_advertised_local_tool() {
    for case in [
        "ordinary agent",
        "default workflow",
        "no parse retries",
        "unadvertised tool",
        "disallowed tool",
        "server tool definition",
        "no tool invoker",
        "registered model attempt",
        "last turn",
    ] {
        let mut api = create_parse_recovery_api(vec![
            malformed_structured_turn("Write"),
            valid_design_turn(),
        ]);
        if case == "ordinary agent" {
            Arc::get_mut(&mut api).unwrap().workflow_watchdog = None;
        } else if case == "default workflow" {
            Arc::get_mut(&mut api).unwrap().workflow_watchdog =
                Some(lingxi_core::host::WorkflowQueryWatchdog::default());
        }
        let invoker = CountingInvoker::new();
        let mut ctx = loop_ctx(api.clone(), Some(invoker.clone()), 5);
        enable_create_parse_recovery(&mut ctx);
        let registration = lingxi_core::host::ModelAttemptRun::new(Arc::new(()));
        match case {
            "no parse retries" => ctx.structured_output_parse_retries = 0,
            "unadvertised tool" => ctx.tool_schemas.clear(),
            "disallowed tool" => ctx.allowed_tools = vec!["Read".into()],
            "server tool definition" => ctx.tool_schemas[0]["type"] = "server_tool".into(),
            "no tool invoker" => ctx.tool_invoker = None,
            "registered model attempt" => {
                ctx.model_attempt = Some(
                    registration
                        .context(lingxi_core::host::ModelAttemptStage::Panel, Some(0))
                        .unwrap(),
                );
            }
            "last turn" => ctx.agent_definition.max_turns = 1,
            _ => {}
        }
        let (_tx, rx) = mpsc::channel(8);
        let (out, events) = mpsc::channel(64);
        run_subagent(ctx, rx, out).await;
        let events = drain(events).await;
        assert_eq!(api.call_count(), 1, "{case}");
        assert_eq!(invoker.call_count(), 0, "{case}");
        assert!(
            events
                .iter()
                .any(|event| matches!(event, SubagentEvent::Failed { .. })),
            "{case}: {events:?}"
        );
    }
}

#[tokio::test]
async fn local_app_create_json_correction_rejects_mixed_server_or_incomplete_response() {
    for (other_tool, other_first) in [
        ("local", true),
        ("local", false),
        ("server", true),
        ("server", false),
        ("server result", true),
        ("server result", false),
        ("incomplete", false),
        ("interrupted", false),
    ] {
        let mut malformed = malformed_structured_turn("Write");
        if matches!(other_tool, "incomplete" | "interrupted") {
            malformed.pop(); // No MessageStop: absence of later server work is unknown.
            if other_tool == "interrupted" {
                malformed.push(Err(response_body_transport_error()));
            }
        } else {
            let content_block = if other_tool == "server result" {
                llm_runtime::ContentBlock::AdvisorToolResult {
                    tool_use_id: "server-other".into(),
                    content: serde_json::json!({"result": "executed"}),
                    is_error: false,
                }
            } else if other_tool == "server" {
                llm_runtime::ContentBlock::ServerToolUse {
                    id: "other".into(),
                    name: "remote_action".into(),
                    input: serde_json::json!({}),
                }
            } else {
                llm_runtime::ContentBlock::ToolCall {
                    input_projection: None,
                    id: "other".into(),
                    name: "Read".into(),
                    input: serde_json::json!({}),
                }
            };
            let other = [
                Ok(llm_runtime::HistoryEvent::ContentBlockStart {
                    index: 1,
                    content_block,
                }),
                Ok(llm_runtime::HistoryEvent::ContentBlockStop { index: 1 }),
            ];
            let index = if other_first { 1 } else { malformed.len() - 2 };
            malformed.splice(index..index, other);
        }
        let api = create_parse_recovery_api(vec![malformed, valid_design_turn()]);
        let invoker = CountingInvoker::new();
        let mut ctx = loop_ctx(api.clone(), Some(invoker.clone()), 5);
        enable_create_parse_recovery(&mut ctx);
        let (_tx, rx) = mpsc::channel(8);
        let (out, events) = mpsc::channel(64);
        run_subagent(ctx, rx, out).await;
        let events = drain(events).await;
        assert_eq!(api.call_count(), 1, "{other_tool}, first={other_first}");
        assert_eq!(invoker.call_count(), 0);
        assert!(events
            .iter()
            .any(|event| matches!(event, SubagentEvent::Failed { .. })));
    }
}

#[tokio::test]
async fn local_app_create_json_correction_rechecks_cancellation_and_budget() {
    for cancel in [true, false] {
        let api = create_parse_recovery_api(vec![
            malformed_structured_turn("Write"),
            valid_design_turn(),
        ]);
        let (tx, rx) = mpsc::channel(8);
        let stop = Arc::new(ParseRetryStop {
            cancel: cancel.then_some(tx),
            exhausted: AtomicBool::new(false),
        });
        *api.retry_stop.lock().unwrap() = Some(stop.clone());
        let invoker = CountingInvoker::new();
        let mut ctx = loop_ctx(api.clone(), Some(invoker.clone()), 5);
        enable_create_parse_recovery(&mut ctx);
        ctx.budget = Some(stop);
        let (out, events) = mpsc::channel(64);
        run_subagent(ctx, rx, out).await;
        let events = drain(events).await;
        if cancel {
            assert!(events
                .iter()
                .any(|event| matches!(event, SubagentEvent::Killed { .. })));
        } else {
            assert!(events.iter().any(|event| matches!(event,
                SubagentEvent::Failed { error, .. } if error.contains("Budget")
            )));
        }
    }
}

#[tokio::test]
async fn design_parse_recovery_preserves_prior_tools_and_reports_retry() {
    let api = ResultStreamMockApiClient::new(vec![
        streamed_tool_use_turn("Read", "tool_use")
            .into_iter()
            .map(Ok)
            .collect(),
        malformed_structured_turn("StructuredOutput"),
        valid_design_turn(),
    ]);
    let observer = Arc::new(RetryObserver::default());
    let wrapped = Arc::new(crate::api::WorkflowWatchdogApiClient::new(
        api.clone(),
        lingxi_core::host::WorkflowQueryWatchdog {
            stall_timeout_ms: 60_000,
            max_retries: 0,
            retry_response_body: false,
        },
        vec![observer.clone()],
    ));
    let invoker = CountingInvoker::new();
    let mut ctx = loop_ctx(wrapped, Some(invoker.clone()), 6);
    enable_design_parse_recovery(&mut ctx);
    let (_tx, rx) = mpsc::channel(8);
    let (out, events) = mpsc::channel(64);
    run_subagent(ctx, rx, out).await;
    let events = drain(events).await;
    assert_eq!(api.call_count(), 3);
    assert_eq!(
        invoker.call_count(),
        1,
        "completed tool must not be replayed"
    );
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while observer.attempts.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("retry observation delivery");
    assert_eq!(*observer.attempts.lock().unwrap(), vec![2]);
    assert!(events.iter().any(|event| matches!(event, SubagentEvent::Completed { result, usage_complete: false, .. } if result == &serde_json::json!({"design":{}}))));
    assert!(!events
        .iter()
        .any(|event| matches!(event, SubagentEvent::Failed { .. })));
    let history = api.histories.lock().unwrap();
    assert!(
        history[2].starts_with(&history[1]),
        "completed conversation must survive"
    );
    assert!(history[2]
        .iter()
        .filter_map(user_text)
        .any(|text| text.contains("JSON correction 1/2")));
}

#[tokio::test]
async fn design_parse_recovery_still_requires_schema_valid_output() {
    let invalid = llm_runtime::stream_accumulator::response_to_stream_events(
        structured_output_call_response(serde_json::json!({"identity": "wrong"})),
    )
    .into_iter()
    .map(Ok)
    .collect();
    let api = ResultStreamMockApiClient::new(vec![
        malformed_structured_turn("StructuredOutput"),
        invalid,
        valid_design_turn(),
    ]);
    let mut ctx = loop_ctx(api.clone(), Some(CountingInvoker::new()), 5);
    enable_design_parse_recovery(&mut ctx);
    let (_tx, rx) = mpsc::channel(8);
    let (out, events) = mpsc::channel(64);
    run_subagent(ctx, rx, out).await;
    let events = drain(events).await;
    assert_eq!(api.call_count(), 3);
    assert!(events.iter().any(|event| matches!(event, SubagentEvent::Completed { result, .. } if result == &serde_json::json!({"design": {}}))));
    assert!(api.histories.lock().unwrap()[2].iter().any(|message| {
        matches!(message, ConversationMessage::User { content, .. }
            if content.iter().any(|block| matches!(block, ContentBlock::ToolResult { is_error: Some(true), content, .. }
                if content.contains("Output does not match required schema"))))
    }));
}

#[tokio::test]
async fn design_parse_recovery_cap_is_run_scoped_and_hard_bounded() {
    for configured in [2, 99] {
        let api = ResultStreamMockApiClient::new(vec![
            malformed_structured_turn("StructuredOutput"),
            streamed_tool_use_turn("Read", "tool_use")
                .into_iter()
                .map(Ok)
                .collect(),
            malformed_structured_turn("StructuredOutput"),
            malformed_structured_turn("StructuredOutput"),
            valid_design_turn(),
        ]);
        let mut ctx = loop_ctx(api.clone(), Some(CountingInvoker::new()), 10);
        enable_design_parse_recovery(&mut ctx);
        ctx.structured_output_parse_retries = configured;
        let (_tx, rx) = mpsc::channel(8);
        let (out, events) = mpsc::channel(64);
        run_subagent(ctx, rx, out).await;
        let events = drain(events).await;
        assert_eq!(
            api.call_count(),
            4,
            "two corrections total, even across a successful tool turn"
        );
        assert!(events.iter().any(|event| matches!(event, SubagentEvent::Failed { error, .. } if error.contains("after 2 retries"))));
        assert!(!events
            .iter()
            .any(|event| matches!(event, SubagentEvent::Completed { .. })));
    }
}

#[tokio::test]
async fn design_parse_recovery_requires_opt_in_schema_and_structured_tool() {
    for (tool, schema, retries, turns) in [
        ("StructuredOutput", true, 0, 5),
        ("StructuredOutput", false, 2, 5),
        ("Write", true, 2, 5),
        ("StructuredOutput", true, 2, 1),
    ] {
        let api = ResultStreamMockApiClient::new(vec![
            malformed_structured_turn(tool),
            valid_design_turn(),
        ]);
        let invoker = CountingInvoker::new();
        let mut ctx = loop_ctx(api.clone(), Some(invoker.clone()), turns);
        enable_design_parse_recovery(&mut ctx);
        if !schema {
            ctx.schema = None;
        }
        ctx.structured_output_parse_retries = retries;
        let (_tx, rx) = mpsc::channel(8);
        let (out, events) = mpsc::channel(64);
        run_subagent(ctx, rx, out).await;
        let events = drain(events).await;
        assert_eq!(api.call_count(), 1);
        assert_eq!(invoker.call_count(), 0);
        assert!(events
            .iter()
            .any(|event| matches!(event, SubagentEvent::Failed { .. })));
    }
}

#[tokio::test]
async fn design_parse_recovery_does_not_dispatch_or_retry_mixed_tool_response() {
    for other_first in [true, false] {
        let mut mixed = malformed_structured_turn("StructuredOutput");
        let other = vec![
            Ok(llm_runtime::HistoryEvent::ContentBlockStart {
                index: 1,
                content_block: llm_runtime::ContentBlock::ToolCall {
                    input_projection: None,
                    id: "other".into(),
                    name: "Write".into(),
                    input: serde_json::json!({}),
                },
            }),
            Ok(llm_runtime::HistoryEvent::ContentBlockStop { index: 1 }),
        ];
        let index = if other_first { 1 } else { mixed.len() - 2 };
        mixed.splice(index..index, other);
        let api = ResultStreamMockApiClient::new(vec![mixed, valid_design_turn()]);
        let invoker = CountingInvoker::new();
        let mut ctx = loop_ctx(api.clone(), Some(invoker.clone()), 5);
        enable_design_parse_recovery(&mut ctx);
        let (_tx, rx) = mpsc::channel(8);
        let (out, events) = mpsc::channel(64);
        run_subagent(ctx, rx, out).await;
        let events = drain(events).await;
        assert_eq!(api.call_count(), 1);
        assert_eq!(invoker.call_count(), 0);
        assert!(events
            .iter()
            .any(|event| matches!(event, SubagentEvent::Failed { .. })));
    }
}

struct ParseRetryStop {
    cancel: Option<mpsc::Sender<lingxi_core::Event>>,
    exhausted: AtomicBool,
}

#[async_trait]
impl lingxi_core::host::SubagentSpawnObserver for ParseRetryStop {
    async fn on_event(&self, event: lingxi_core::host::SubagentObservation) {
        if matches!(event, lingxi_core::host::SubagentObservation::Retry { .. }) {
            if let Some(tx) = &self.cancel {
                tx.send(lingxi_core::Event::UserInterrupt).await.unwrap();
            } else {
                self.exhausted.store(true, Ordering::SeqCst);
            }
        }
    }
}

#[async_trait]
impl lingxi_core::host::budget::BudgetEnforcerHandle for ParseRetryStop {
    async fn check_and_charge(&self, _: u64) -> Result<(), lingxi_core::host::budget::BudgetError> {
        if self.exhausted.load(Ordering::SeqCst) {
            Err(lingxi_core::host::budget::BudgetError::Exceeded {
                current_nano_usd: 1,
            })
        } else {
            Ok(())
        }
    }
    async fn snapshot_total_nano_usd(&self) -> u64 {
        0
    }
}

#[tokio::test]
async fn design_parse_recovery_rechecks_cancellation_and_budget() {
    for cancel in [true, false] {
        let api = ResultStreamMockApiClient::new(vec![
            malformed_structured_turn("StructuredOutput"),
            valid_design_turn(),
        ]);
        let (tx, rx) = mpsc::channel(8);
        let stop = Arc::new(ParseRetryStop {
            cancel: cancel.then_some(tx),
            exhausted: AtomicBool::new(false),
        });
        *api.retry_stop.lock().unwrap() = Some(stop.clone());
        let mut ctx = loop_ctx(api.clone(), None, 5);
        enable_design_parse_recovery(&mut ctx);
        ctx.budget = Some(stop);
        let (out, events) = mpsc::channel(64);
        run_subagent(ctx, rx, out).await;
        let events = drain(events).await;
        assert_eq!(api.call_count(), 1);
        if cancel {
            assert!(events
                .iter()
                .any(|event| matches!(event, SubagentEvent::Killed { .. })));
        } else {
            assert!(events.iter().any(|event| matches!(event, SubagentEvent::Failed { error, .. } if error.contains("Budget"))));
        }
    }
}

/// A second round-trip cut off by `RateLimited` mid-stream — after a completed
/// first turn AND with a block salvaged from the failing turn — recovers as a
/// `Completed` result whose FIRST content block is the exact `cutoffNote`, with
/// the salvaged partial text following it. NOT a `Failed`.
#[tokio::test]
async fn rate_limit_midstream_recovers_partial_with_cutoff_note() {
    let dir = tempfile::tempdir().unwrap();
    // Turn 1: a complete tool_use turn (drives the loop into turn 2 after the
    // tool is dispatched). Turn 2: a partial text block then a mid-stream 429.
    let turn1: Vec<Result<llm_runtime::HistoryEvent, llm_runtime::LlmError>> =
        streamed_tool_use_turn("Read", "tool_use")
            .into_iter()
            .map(Ok)
            .collect();
    let turn2 = partial_text_then_err(
        "Partial answer before the cutoff",
        llm_runtime::LlmError::RateLimited {
            retry_after: None,
            scope: None,
        },
    );
    let api = ResultStreamMockApiClient::new(vec![turn1, turn2]);
    let api2 = api.clone();
    let invoker = CountingInvoker::new();
    let mut ctx = loop_ctx(api, Some(invoker), 10);
    ctx.transcript_subdir = dir.path().to_path_buf();
    ctx.transcript_fs = Some(Arc::new(platform_posix::PosixFileSystem::new(
        dir.path().to_path_buf(),
    )) as Arc<dyn lingxi_core::host::FileSystem>);
    let agent_id = ctx.agent_id;
    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    run_subagent(ctx, event_rx, out_tx).await;
    let evs = drain(out_rx).await;

    // No Failed event — the partial was preserved as a completion.
    assert!(
        !evs.iter()
            .any(|e| matches!(e, SubagentEvent::Failed { .. })),
        "a recoverable mid-stream 429 must NOT surface as Failed: {evs:?}"
    );
    let result = evs
        .iter()
        .find_map(|e| match e {
            SubagentEvent::Completed { result, .. } => Some(result.clone()),
            _ => None,
        })
        .expect("a Completed event carrying the recovered partial");

    let content = result
        .get("content")
        .and_then(serde_json::Value::as_array)
        .expect("content array");
    // First text block is the exact cutoffNote.
    assert_eq!(
        content[0].get("text").and_then(serde_json::Value::as_str),
        Some(EXPECTED_SERVER_ERROR_CUTOFF),
        "first content block must be the byte-exact cutoffNote"
    );
    // The salvaged partial text follows the note.
    assert!(
        content
            .iter()
            .any(|b| b.get("text").and_then(serde_json::Value::as_str)
                == Some("Partial answer before the cutoff")),
        "the salvaged mid-turn block must survive: {content:?}"
    );
    // Both round-trips were attempted (the loop reached turn 2 before erroring).
    assert_eq!(api2.call_count(), 2);
    let transcript = std::fs::read_to_string(dir.path().join(format!("agent-{agent_id}.jsonl")))
        .expect("partial completion must persist its transcript before Completed");
    assert!(
        transcript.contains("Partial answer before the cutoff"),
        "salvaged response must remain inspectable: {transcript}"
    );
    assert!(
        transcript.contains("\"status\":\"completed\""),
        "partial recovery must persist a true terminal marker: {transcript}"
    );
}

/// A qualifying error (`RateLimited`) at request-start on the FIRST turn — with
/// no content produced yet — still fails (CC's `Zor(r)===void 0` guard: nothing
/// to recover).
#[tokio::test]
async fn qualifying_error_with_no_content_fails() {
    let api = MockSubagentApiClient::new(vec![Err(llm_runtime::LlmError::RateLimited {
        retry_after: None,
        scope: None,
    })]);
    let ctx = loop_ctx(api, Some(CountingInvoker::new()), 10);
    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    run_subagent(ctx, event_rx, out_tx).await;
    let evs = drain(out_rx).await;
    assert!(
        evs.iter()
            .any(|e| matches!(e, SubagentEvent::Failed { .. })),
        "a qualifying error with an empty transcript must Fail: {evs:?}"
    );
    assert!(
        !evs.iter()
            .any(|e| matches!(e, SubagentEvent::Completed { .. })),
        "no partial exists to recover, so no Completed: {evs:?}"
    );
}

/// A NON-qualifying error (`QuotaExceeded`/`InvalidRequest` — kinds NOT in
/// `CTy`) fails even when content exists: CC rethrows these terminal API errors.
#[tokio::test]
async fn nonqualifying_error_after_content_still_fails() {
    // Turn 1 completes with a tool_use (content in history + tool dispatched);
    // turn 2 errors mid-stream with a NON-CTy kind → no recovery.
    let turn1: Vec<Result<llm_runtime::HistoryEvent, llm_runtime::LlmError>> =
        streamed_tool_use_turn("Read", "tool_use")
            .into_iter()
            .map(Ok)
            .collect();
    let turn2 = partial_text_then_err(
        "text that will NOT be recovered",
        llm_runtime::LlmError::QuotaExceeded,
    );
    let api = ResultStreamMockApiClient::new(vec![turn1, turn2]);
    let ctx = loop_ctx(api, Some(CountingInvoker::new()), 10);
    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    run_subagent(ctx, event_rx, out_tx).await;
    let evs = drain(out_rx).await;
    assert!(
        evs.iter()
            .any(|e| matches!(e, SubagentEvent::Failed { .. })),
        "a non-CTy error must Fail even with content: {evs:?}"
    );
    // The salvaged text must NOT leak into any Completed result.
    assert!(
        !evs.iter()
            .any(|e| matches!(e, SubagentEvent::Completed { .. })),
        "non-qualifying error must not recover a partial: {evs:?}"
    );
}

/// The per-agent transcript is actually WRITTEN. Before this, the
/// `SubagentStop` hook reported an `agent_transcript_path` while nothing
/// created the file — the payload named something that did not exist, and a
/// background agent's conversation lived only in memory.
#[tokio::test]
async fn run_subagent_persists_its_conversation_to_the_agent_transcript() {
    let dir = tempfile::tempdir().unwrap();
    let api = MockSubagentApiClient::new(vec![Ok(text_response("the answer", Some("end_turn")))]);
    let mut ctx = loop_ctx(api, None, 4);
    ctx.transcript_subdir = dir.path().to_path_buf();
    ctx.transcript_fs = Some(Arc::new(platform_posix::PosixFileSystem::new(
        dir.path().to_path_buf(),
    )) as Arc<dyn lingxi_core::host::FileSystem>);
    ctx.prompt_messages = vec![lingxi_core::types::ConversationMessage::user(
        MessageId::new(),
        "do the thing".to_string(),
    )];
    let agent_id = ctx.agent_id;

    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    run_subagent(ctx, event_rx, out_tx).await;
    let events = drain(out_rx).await;
    let snapshot = events
        .iter()
        .find_map(|event| match event {
            SubagentEvent::TranscriptSnapshot { messages, .. } => Some(messages),
            _ => None,
        })
        .expect("terminal runner emits its settled history to the host");
    assert!(snapshot.iter().any(|message| {
        matches!(
            message,
            lingxi_core::types::ConversationMessage::Assistant { .. }
        ) && message.text_content() == "the answer"
    }));
    assert!(snapshot.iter().any(|message| {
        matches!(
            message,
            lingxi_core::types::ConversationMessage::User { .. }
        ) && message.text_content() == "do the thing"
    }));
    let snapshot_event = events
        .iter()
        .find(|event| matches!(event, SubagentEvent::TranscriptSnapshot { .. }))
        .unwrap();
    assert!(
        serde_json::to_value(snapshot_event).is_err(),
        "host transcript snapshots cannot serialize onto a public event wire"
    );

    let path = dir.path().join(format!("agent-{agent_id}.jsonl"));
    let body = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("transcript at {} should exist: {e}", path.display()));
    let lines: Vec<&str> = body.lines().filter(|l| !l.trim().is_empty()).collect();
    assert!(!lines.is_empty(), "transcript has content");
    for line in &lines {
        let entry: serde_json::Value = serde_json::from_str(line).expect("a JSON line");
        assert!(entry.get("agent_id").is_some(), "stamped with its agent");
        assert!(entry.get("message").is_some(), "carries the message");
    }
    assert!(
        body.contains("\"status\":\"completed\""),
        "terminal completion is persisted for session-agent status discovery"
    );
    // The SEEDED prompt is on disk, not only the assistant turns — a resume
    // needs the conversation from its start.
    assert!(
        body.contains("do the thing"),
        "the seeded prompt is persisted: {body}"
    );
    assert!(
        body.contains("the answer"),
        "the reply is persisted: {body}"
    );
}

/// A child transcript is also the live UI's source of truth. The seeded user
/// prompt must therefore be durable and observable before the first model
/// response; otherwise a stalled first request leaves the agent detail screen
/// empty even though the model already received its task.
#[tokio::test]
async fn run_subagent_exposes_seed_before_first_model_response() {
    let dir = tempfile::tempdir().unwrap();
    let api = StuckThenCapturingApiClient::new();
    let mut ctx = loop_ctx(api.clone(), None, 4);
    ctx.transcript_subdir = dir.path().to_path_buf();
    ctx.transcript_fs = Some(Arc::new(platform_posix::PosixFileSystem::new(
        dir.path().to_path_buf(),
    )) as Arc<dyn lingxi_core::host::FileSystem>);
    ctx.prompt_messages = vec![lingxi_core::types::ConversationMessage::user(
        MessageId::new(),
        "design the airplane game".to_string(),
    )];
    let agent_id = ctx.agent_id;

    let (event_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, mut out_rx) = mpsc::channel::<SubagentEvent>(16);
    let task = tokio::spawn(run_subagent(ctx, event_rx, out_tx));

    api.first_call_started.notified().await;

    let path = dir.path().join(format!("agent-{agent_id}.jsonl"));
    let body = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("seed transcript at {} should exist: {e}", path.display()));
    assert!(
        body.contains("design the airplane game"),
        "the prompt must be readable while the first request is still pending: {body}"
    );

    let event = tokio::time::timeout(std::time::Duration::from_secs(1), out_rx.recv())
        .await
        .expect("seed message event before the first model response")
        .expect("runner output remains open");
    let SubagentEvent::Message { message, .. } = event else {
        panic!("expected the seeded prompt as the first live message, got {event:?}");
    };
    let message: ConversationMessage =
        serde_json::from_value(message).expect("seed event is a conversation message");
    assert_eq!(message.text_content(), "design the airplane game");

    event_tx.send(lingxi_core::Event::UserExit).await.unwrap();
    task.await.unwrap();
}

/// A host that wires no transcript filesystem persists nothing and behaves
/// exactly as before — the seam is additive.
#[tokio::test]
async fn run_subagent_without_a_transcript_fs_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let api = MockSubagentApiClient::new(vec![Ok(text_response("x", Some("end_turn")))]);
    let mut ctx = loop_ctx(api, None, 4);
    ctx.transcript_subdir = dir.path().to_path_buf();
    let agent_id = ctx.agent_id;

    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    run_subagent(ctx, event_rx, out_tx).await;
    let _ = drain(out_rx).await;

    assert!(!dir.path().join(format!("agent-{agent_id}.jsonl")).exists());
}

/// A RESTORED agent is seeded from its recovered conversation, and that history
/// REPLACES the normal seeding rather than prefixing it: the prompt, the fork
/// context and the `SubagentStart` / skills preload are all already inside the
/// recovered messages, so re-adding them would duplicate context the agent has
/// seen and re-fire start hooks for a run that began in another process.
#[tokio::test]
async fn a_resumed_history_replaces_the_seed_rather_than_prefixing_it() {
    let api = MockSubagentApiClient::new(vec![Ok(text_response("ok", Some("end_turn")))]);
    let mut ctx = loop_ctx(api.clone(), None, 4);
    ctx.prompt_messages = vec![lingxi_core::types::ConversationMessage::user(
        MessageId::new(),
        "ORIGINAL PROMPT".to_string(),
    )];
    ctx.fork_context_messages = Some(vec![lingxi_core::types::ConversationMessage::user(
        MessageId::new(),
        "FORK CONTEXT".to_string(),
    )]);
    ctx.resumed_history = Some(vec![lingxi_core::types::ConversationMessage::user(
        MessageId::new(),
        "RECOVERED".to_string(),
    )]);
    ctx.mobile_runtime_environment_reminder = Some(Arc::from(
        "<system-reminder>\nMOBILE RUNTIME MUST NOT DUPLICATE\n</system-reminder>",
    ));
    ctx.mobile_runtime_workspace_reminder = Some(Arc::from(
        "<system-reminder>\nMOBILE WORKSPACE MUST NOT DUPLICATE\n</system-reminder>",
    ));

    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    run_subagent(ctx, event_rx, out_tx).await;
    let _ = drain(out_rx).await;

    let sent = api.last_messages();
    let rendered = format!("{sent:?}");
    assert!(
        rendered.contains("RECOVERED"),
        "resumed history is sent: {rendered}"
    );
    assert!(
        !rendered.contains("ORIGINAL PROMPT"),
        "the prompt is NOT re-sent: {rendered}"
    );
    assert!(
        !rendered.contains("FORK CONTEXT"),
        "the fork-context prefix is NOT re-added: {rendered}"
    );
    assert!(
        !rendered.contains("MOBILE RUNTIME MUST NOT DUPLICATE"),
        "the runtime reminder is already persisted in recovered history and is NOT re-added: {rendered}"
    );
    assert!(
        !rendered.contains("MOBILE WORKSPACE MUST NOT DUPLICATE"),
        "the workspace reminder is already persisted in recovered history and is NOT re-added: {rendered}"
    );
}

/// A RESTORED run must not re-append its recovered conversation to the
/// transcript — the watermark starts past it, so the file grows by what is new
/// rather than doubling every time the agent is restored.
#[tokio::test]
async fn a_restored_run_appends_only_new_messages_to_its_transcript() {
    let dir = tempfile::tempdir().unwrap();
    let api = MockSubagentApiClient::new(vec![Ok(text_response("fresh reply", Some("end_turn")))]);
    let mut ctx = loop_ctx(api, None, 4);
    ctx.transcript_subdir = dir.path().to_path_buf();
    ctx.transcript_fs = Some(Arc::new(platform_posix::PosixFileSystem::new(
        dir.path().to_path_buf(),
    )) as Arc<dyn lingxi_core::host::FileSystem>);
    ctx.resumed_history = Some(vec![lingxi_core::types::ConversationMessage::user(
        MessageId::new(),
        "ALREADY ON DISK".to_string(),
    )]);
    let agent_id = ctx.agent_id;

    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    run_subagent(ctx, event_rx, out_tx).await;
    let _ = drain(out_rx).await;

    let body = std::fs::read_to_string(dir.path().join(format!("agent-{agent_id}.jsonl"))).unwrap();
    assert!(
        !body.contains("ALREADY ON DISK"),
        "the recovered conversation is not written a second time: {body}"
    );
    assert!(
        body.contains("fresh reply"),
        "new turns are appended: {body}"
    );
}

/// A stalled stream must NOT be recovered as `api_error_partial`.
///
/// Claude Code 2.1.238 classifies it through `xtt` as `"api_timeout"`, and
/// `CTy = {"rate_limit","overloaded","server_error"}` does not contain that, so
/// the oracle rethrows. Recovering it instead is a one-bit difference with a
/// large blast radius: the caller receives a `completed` result whose content
/// says the agent was cut off and did nothing, which a workflow stage records
/// as output and moves past. Three stalled generate attempts each "succeeded"
/// that way before the verify stage noticed the app was still the template.
#[test]
fn a_stalled_stream_is_terminal_and_an_ordinary_interruption_still_recovers() {
    use llm_runtime::model::stream_watchdog::{
        STREAM_IDLE_TIMEOUT_PREFIX, STREAM_SUSPENDED_PREFIX,
    };

    for message in [
        format!("{STREAM_IDLE_TIMEOUT_PREFIX}: no bytes for 300000ms"),
        format!("{STREAM_SUSPENDED_PREFIX}; aborting to retry on a fresh connection"),
    ] {
        assert!(
            super::classify_api_termination(&llm_runtime::LlmError::StreamInterrupted {
                message: message.clone(),
            })
            .is_none(),
            "a stall must rethrow, not recover: {message}"
        );
    }

    // The kinds CTy DOES contain keep recovering, so this change cannot be
    // mistaken for "stop recovering api errors".
    for error in [
        llm_runtime::LlmError::Overloaded { repeated: false },
        llm_runtime::LlmError::ProviderInternal,
        llm_runtime::LlmError::StreamInterrupted {
            message: "stream ended before message_stop".to_string(),
        },
    ] {
        assert!(
            super::classify_api_termination(&error).is_some(),
            "{error:?} is in CTy and must still recover as api_error_partial"
        );
    }
}

// ---- Fusion panel input cap must not split tool_use/tool_result pairs ----

#[tokio::test]
async fn oversized_mandatory_prompt_is_rejected_before_provider_call() {
    let api = MockSubagentApiClient::new(vec![Ok(text_response("must not run", Some("end_turn")))]);
    let mut ctx = loop_ctx(api.clone(), None, 1);
    ctx.prompt_messages = vec![ConversationMessage::user(
        MessageId::new(),
        "TASK-MARKER: ".to_string() + &"x".repeat(10_000),
    )];
    ctx.max_input_bytes_per_turn = Some(128);
    let (_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);

    run_subagent(ctx, event_rx, out_tx).await;
    let events = drain(out_rx).await;
    assert_eq!(
        api.call_count(),
        0,
        "the provider must not receive an over-cap mandatory prompt"
    );
    assert!(events.iter().any(|event| {
        matches!(event, SubagentEvent::Failed { error, .. } if error.contains("mandatory initial prompt exceeds"))
    }));
}

/// Build an assistant message whose only content block is a `ToolUse`.
fn assistant_tool_use(id: ToolUseId, name: &str) -> ConversationMessage {
    ConversationMessage::Assistant { per_turn_effort: None,
        id: MessageId::new(),
        content: vec![ContentBlock::ToolUse {
            input_projection: None,
            id,
            name: name.to_string(),
            input: serde_json::json!({}),
            provider_id: None,
        }],
        stop_reason: Some("tool_use".into()),
    }
}

/// Build a user message whose only content block is the matching `ToolResult`.
fn user_tool_result(tool_use_id: ToolUseId, content: &str) -> ConversationMessage {
    ConversationMessage::User { api_message_override: None,
        id: MessageId::new(),
        content: vec![ContentBlock::ToolResult {
            content_projection: None,
            tool_use_id,
            content: content.to_string(),
            is_error: Some(false),
            provider_tool_use_id: None,
            content_blocks: None,
        }],
        is_meta: false,
        is_compact_summary: false,
        is_visible_in_transcript_only: false,
    }
}

/// A byte cap tight enough to keep only the LAST message by itself would,
/// under naive whole-message trimming from the tail, keep a trailing
/// `ToolResult` while dropping the `ToolUse` message that produced it —
/// wire-invalid history (a `tool_result` with no matching `tool_use` in the
/// same request). `cap_input_bytes` must keep or drop such a pair together.
#[test]
fn cap_input_bytes_keeps_tool_use_and_tool_result_paired() {
    let tool_id = ToolUseId::new();
    let history = vec![
        assistant_text("turn one, filler text to add up bytes so the cap bites here"),
        assistant_tool_use(tool_id.clone(), "Read"),
        user_tool_result(tool_id.clone(), "file contents"),
    ];
    // The complete seed + tool pair fits. The cap implementation must retain
    // that pair atomically, rather than trimming one half to satisfy a
    // per-message approximation.
    let max = serde_json::to_vec(&history).unwrap().len() as u64;
    let capped = super::cap_input_bytes(&history, Some(max)).expect("cap fits");

    let has_tool_use = capped.iter().any(|m| {
        matches!(m, ConversationMessage::Assistant { content, .. }
            if content.iter().any(|b| matches!(b, ContentBlock::ToolUse { id, .. } if *id == tool_id)))
    });
    let has_tool_result = capped.iter().any(|m| {
        matches!(m, ConversationMessage::User { content, .. }
            if content.iter().any(|b| matches!(b, ContentBlock::ToolResult { tool_use_id, .. } if *tool_use_id == tool_id)))
    });
    assert_eq!(
        has_tool_use, has_tool_result,
        "cap_input_bytes split a tool_use/tool_result pair: tool_use kept={has_tool_use}, tool_result kept={has_tool_result}"
    );
    assert!(
        has_tool_result,
        "the pair that fits the budget must be kept, not dropped entirely"
    );
}

/// F009: the task seed and newest tool pair are both mandatory. Even when
/// each fits separately, their exact serialized array may not; reject that
/// request instead of splitting the pair or exceeding the declared cap.
#[test]
fn cap_input_bytes_rejects_task_plus_latest_pair_over_the_exact_cap() {
    let tool_id = ToolUseId::new();
    let prompt = ConversationMessage::user(MessageId::new(), "TASK-MARKER".repeat(20));
    let pair = [
        assistant_tool_use(tool_id.clone(), "Read"),
        user_tool_result(tool_id, &"result".repeat(20)),
    ];
    let mut history = vec![prompt.clone()];
    history.extend(pair);
    let exact_bytes = u64::try_from(serde_json::to_vec(&history).unwrap().len()).unwrap();
    let cap = exact_bytes - 1;
    assert!(u64::try_from(serde_json::to_vec(&vec![prompt]).unwrap().len()).unwrap() <= cap);

    let units = super::tool_pair_units(&history);
    let expected_measured_bytes = units
        .first()
        .into_iter()
        .chain(units.last())
        .map(|unit| u64::try_from(serde_json::to_vec(unit).unwrap().len()).unwrap())
        .sum();
    super::reset_cap_input_measurements();
    let error = super::cap_input_bytes(&history, Some(cap))
        .expect_err("the mandatory task/latest pair is one serialized byte over cap");
    assert!(error.contains("mandatory latest tool/message unit exceeds"));
    assert_eq!(
        super::cap_input_measurements(),
        (2, expected_measured_bytes),
        "a mandatory latest-unit rejection must not visit older history"
    );
}

/// [Finding 9] A Fusion panel seeds its ENTIRE task text as `history`'s
/// first unit (`ctx.prompt_messages`, extended in before the first turn)
/// and nothing re-injects it on later turns — it is the only production
/// caller that sets `max_input_bytes_per_turn` (`panel.rs`'s
/// `spawn_request`). The tail-only fill in `cap_input_bytes` must not evict
/// that first unit while a newer tool-result pair still fits the budget: two
/// large tool-result pairs from earlier turns must not silently erase the
/// task, leaving the panel to emit a `PanelReport` written against no task
/// at all.
#[test]
fn cap_input_bytes_pins_the_task_prompt_when_tool_results_crowd_it_out() {
    let prompt = ConversationMessage::user(MessageId::new(), "TASK-MARKER: what is 2+2?".into());
    let tool_id_1 = ToolUseId::new();
    let tool_id_2 = ToolUseId::new();
    let pair1 = [
        assistant_tool_use(tool_id_1.clone(), "Read"),
        user_tool_result(tool_id_1, &"y".repeat(50_000)),
    ];
    let pair2 = [
        assistant_tool_use(tool_id_2.clone(), "Read"),
        user_tool_result(tool_id_2, &"z".repeat(50_000)),
    ];
    let mut history = vec![prompt.clone()];
    history.extend(pair1.iter().cloned());
    history.extend(pair2.iter().cloned());

    let prompt_bytes = serde_json::to_vec(&prompt).unwrap().len() as u64;
    let newest_pair_bytes: u64 = pair2
        .iter()
        .map(|m| serde_json::to_vec(m).unwrap().len() as u64)
        .sum();
    // Room for the prompt plus exactly the NEWEST pair, not both pairs.
    let max = prompt_bytes + newest_pair_bytes + 16;

    let capped = super::cap_input_bytes(&history, Some(max)).expect("cap fits");
    let joined = format!("{capped:?}");
    assert!(
        joined.contains("TASK-MARKER"),
        "the panel's task prompt must survive per-turn trimming while a \
         newer tool-result pair still fits the budget; capped history: {joined}"
    );
    assert!(
        !joined.contains(&"y".repeat(50_000)),
        "the OLDER, over-budget tool-result pair must still be dropped — a \
         `cap_input_bytes` that just returned the whole history unchanged \
         would also contain TASK-MARKER, so this must go red on its own"
    );
}

/// F009: a task prompt that cannot fit the declared cap must be rejected.
/// Sending it whole violates the cap, while dropping it silently removes the
/// only task-carrying unit from later turns.
#[test]
fn cap_input_bytes_rejects_when_the_task_prompt_alone_exceeds_the_cap() {
    let prompt = ConversationMessage::user(
        MessageId::new(),
        format!("TASK-MARKER: {}", "x".repeat(50_000)),
    );
    let tool_id = ToolUseId::new();
    let pair = [
        assistant_tool_use(tool_id.clone(), "Read"),
        user_tool_result(tool_id, "small result"),
    ];
    let mut history = vec![prompt];
    history.extend(pair.iter().cloned());

    let pair_bytes: u64 = pair
        .iter()
        .map(|m| serde_json::to_vec(m).unwrap().len() as u64)
        .sum();
    // Comfortably fits the newest pair, nowhere near fitting the ~50 KB
    // prompt too.
    let max = pair_bytes + 32;

    let error = super::cap_input_bytes(&history, Some(max))
        .expect_err("an oversized mandatory prompt must be rejected before sending");
    assert!(
        error.contains("mandatory initial prompt exceeds"),
        "the rejection must explain the mandatory prompt cap violation: {error}"
    );
}

/// [F009] A mandatory oversized seed must be rejected instead of being sent
/// whole (over the declared cap) or silently dropped (losing the task).
#[test]
fn cap_input_bytes_rejects_an_oversized_head_without_sending_it() {
    let prompt = ConversationMessage::user(MessageId::new(), "x".repeat(50_000));
    let pair1 = [
        assistant_tool_use(ToolUseId::new(), "Read"),
        user_tool_result(ToolUseId::new(), &"y".repeat(200)),
    ];
    let pair2 = [
        assistant_tool_use(ToolUseId::new(), "Read"),
        user_tool_result(ToolUseId::new(), &"z".repeat(50_000)),
    ];
    let mut history = vec![prompt];
    history.extend(pair1.iter().cloned());
    history.extend(pair2.iter().cloned());

    let pair1_bytes: u64 = pair1
        .iter()
        .map(|m| serde_json::to_vec(m).unwrap().len() as u64)
        .sum();
    // Room for one small pair, nowhere near enough for the ~50 KB mandatory
    // head.
    let max = pair1_bytes + 16;

    let expected_head_bytes = u64::try_from(
        serde_json::to_vec(super::tool_pair_units(&history)[0])
            .unwrap()
            .len(),
    )
    .unwrap();
    super::reset_cap_input_measurements();
    let error = super::cap_input_bytes(&history, Some(max))
        .expect_err("an oversized mandatory prompt must be rejected before sending");
    assert!(error.contains("mandatory initial prompt exceeds"));
    assert_eq!(
        super::cap_input_measurements(),
        (1, expected_head_bytes),
        "an oversized head must reject before visiting any tail unit"
    );
}

/// Keep the pre-optimization selection and ordering as a golden: the
/// implementation may change how it measures candidates, but it must still
/// retain the seed, the newest unit, and the newest older unit that fits.
#[test]
fn cap_input_bytes_preserves_legacy_selection_and_order() {
    let head = ConversationMessage::user(
        MessageId::new(),
        r#"任务 seed with escaped "quotes" and \slashes\"#.to_string(),
    );
    let older = assistant_text("older CJK context: 你好世界");
    let rejected = assistant_text(&"oversized middle context ".repeat(256));
    let newest = assistant_text("newest continuation");
    let history = vec![head.clone(), older.clone(), rejected, newest.clone()];
    let expected = vec![head, older, newest];
    let max = serde_json::to_vec(&expected).unwrap().len() as u64;

    let capped = super::cap_input_bytes(&history, Some(max)).expect("golden selection fits");
    assert_eq!(
        serde_json::to_vec(&capped).unwrap(),
        serde_json::to_vec(&expected).unwrap(),
        "byte-cap optimization changed the legacy retained order"
    );
}

/// Independent copy of the old clone-and-reserialize algorithm. Keeping this
/// in the test module makes parity explicit while the production measurement
/// path is replaced with cached borrowed-unit metadata.
fn legacy_cap_input_bytes_reference(
    messages: &[ConversationMessage],
    max_bytes: Option<u64>,
) -> Result<Vec<ConversationMessage>, String> {
    let Some(max) = max_bytes else {
        return Ok(messages.to_vec());
    };
    let units = super::tool_pair_units(messages);
    if units.is_empty() {
        return Ok(Vec::new());
    }

    let serialized_bytes = |candidate: &[&[ConversationMessage]]| -> u64 {
        let flattened: Vec<ConversationMessage> = candidate
            .iter()
            .flat_map(|unit| unit.iter().cloned())
            .collect();
        serde_json::to_vec(&flattened)
            .map(|bytes| u64::try_from(bytes.len()).unwrap_or(u64::MAX))
            .unwrap_or(u64::MAX)
    };

    let head = units[0];
    let head_bytes = serialized_bytes(&[head]);
    if head_bytes > max {
        return Err(format!(
            "mandatory initial prompt exceeds max_input_bytes_per_turn ({head_bytes} > {max})"
        ));
    }

    let mut selected_tail: Vec<&[ConversationMessage]> = Vec::new();
    if let Some(newest) = units.get(1..).and_then(|tail| tail.last()).copied() {
        if serialized_bytes(&[head, newest]) > max {
            let newest_bytes = serialized_bytes(&[newest]);
            return Err(format!(
                "mandatory latest tool/message unit exceeds max_input_bytes_per_turn when combined with the initial prompt ({head_bytes} + {newest_bytes} > {max})"
            ));
        }
        selected_tail.push(newest);
    }

    if units.len() > 2 {
        for unit in units[1..units.len() - 1].iter().rev() {
            let mut candidate = Vec::with_capacity(selected_tail.len() + 2);
            candidate.push(head);
            candidate.extend(selected_tail.iter().copied());
            candidate.push(unit);
            if serialized_bytes(&candidate) <= max {
                selected_tail.push(unit);
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
    Ok(out)
}

/// CJK, JSON escaping, and atomic tool pairs must produce byte-for-byte the
/// same output and errors as the old implementation.
#[test]
fn cap_input_bytes_matches_legacy_for_unicode_and_tool_pairs() {
    let head = ConversationMessage::user(
        MessageId::new(),
        r#"任务 "quoted" \ escaped / 你好世界"#.to_string(),
    );
    let mut history = vec![head];
    for index in 0..12 {
        let tool_id = ToolUseId::new();
        history.push(assistant_tool_use(
            tool_id.clone(),
            &format!("读取-{index}"),
        ));
        history.push(user_tool_result(
            tool_id,
            &format!(r#"结果 {index}: 你好 "quoted" \ slash"#),
        ));
    }
    let units = super::tool_pair_units(&history);
    let serialized_candidate_bytes = |candidate: &[&[ConversationMessage]]| -> u64 {
        let flattened: Vec<ConversationMessage> = candidate
            .iter()
            .flat_map(|unit| unit.iter().cloned())
            .collect();
        u64::try_from(serde_json::to_vec(&flattened).unwrap().len()).unwrap()
    };

    // Exercise both sides of every retained-unit framing boundary, plus the
    // mandatory-head/latest errors and the uncapped compatibility path.
    let head = units[0];
    let newest = *units.last().expect("history has a newest unit");
    let mut candidate = vec![head, newest];
    let mut caps = vec![
        0,
        serialized_candidate_bytes(&[head]).saturating_sub(1),
        serialized_candidate_bytes(&[head]),
        serialized_candidate_bytes(&candidate).saturating_sub(1),
        serialized_candidate_bytes(&candidate),
        u64::MAX,
    ];
    for unit in units[1..units.len() - 1].iter().rev().copied() {
        candidate.push(unit);
        let boundary = serialized_candidate_bytes(&candidate);
        caps.extend([
            boundary.saturating_sub(1),
            boundary,
            boundary.saturating_add(1),
        ]);
    }
    caps.sort_unstable();
    caps.dedup();

    assert_eq!(
        super::cap_input_bytes(&history, None),
        legacy_cap_input_bytes_reference(&history, None),
        "uncapped input changed"
    );
    for max in caps {
        let expected = legacy_cap_input_bytes_reference(&history, Some(max));
        let actual = super::cap_input_bytes(&history, Some(max));
        assert_eq!(
            actual, expected,
            "optimized cap changed Unicode/tool-pair output or error at {max} bytes"
        );
        if let Ok(capped) = actual {
            assert!(
                u64::try_from(serde_json::to_vec(&capped).unwrap().len()).unwrap() <= max,
                "optimized cap returned an over-limit candidate at {max} bytes"
            );
        }
    }
}

/// The optimized path must serialize every reached borrowed unit exactly once,
/// independent of history length. This is deterministic test-only
/// instrumentation, not a wall-clock test.
#[test]
fn cap_input_bytes_measurement_work_is_linear_for_many_pairs() {
    let head = ConversationMessage::user(
        MessageId::new(),
        r#"任务 seed "quoted" 你好世界"#.to_string(),
    );
    let mut history = vec![head];
    for index in 0..96 {
        let tool_id = ToolUseId::new();
        history.push(assistant_tool_use(
            tool_id.clone(),
            &format!("read-{index}"),
        ));
        history.push(user_tool_result(
            tool_id,
            &format!(r#"result-{index}: "escaped" 你好"#),
        ));
    }
    let expected_units = 1 + 96;
    let expected_serialized_bytes = super::tool_pair_units(&history)
        .into_iter()
        .map(|unit| u64::try_from(serde_json::to_vec(unit).unwrap().len()).unwrap())
        .sum();
    let max = serde_json::to_vec(&history).unwrap().len() as u64;

    super::reset_cap_input_measurements();
    let capped = super::cap_input_bytes(&history, Some(max))
        .expect("the complete Unicode/tool-pair history fits");
    assert_eq!(
        super::cap_input_measurements(),
        (expected_units, expected_serialized_bytes),
        "each trimming unit and its bytes must be visited exactly once"
    );
    assert_eq!(
        serde_json::to_vec(&capped).unwrap().len() as u64,
        max,
        "linear metadata must preserve the exact full-history byte count"
    );
}

struct OwnerNotificationRegistry {
    park_foreground: bool,
    rest_acknowledged: AtomicBool,
    wake_checked: tokio::sync::Notify,
    drains: AtomicUsize,
    parked_fold: tokio::sync::Notify,
    owner: lingxi_core::types::AgentId,
    pending: Mutex<Vec<lingxi_core::host::task_registry::TaskNotification>>,
    agent_fact_updates: Mutex<Vec<lingxi_core::host::task_registry::AgentListLocalFactUpdate>>,
    records: Mutex<Vec<lingxi_core::host::task_registry::TaskRecord>>,
    revision: tokio::sync::watch::Sender<u64>,
}
impl OwnerNotificationRegistry {
    fn publish(&self) {
        self.pending
            .lock()
            .unwrap()
            .push(lingxi_core::host::task_registry::TaskNotification {
                task_id: "achild".into(),
                task_type: "local_agent".into(),
                status: "completed".into(),
                recipient_agent_id: Some(self.owner),
                description: "child finished".into(),
                ..Default::default()
            });
        self.revision.send_modify(|n| *n += 1);
    }
}
#[async_trait]
impl lingxi_core::host::task_registry::TaskRegistryHandle for OwnerNotificationRegistry {
    async fn update_agent_list_local_fact(
        &self,
        agent_id: lingxi_core::types::AgentId,
        update: lingxi_core::host::task_registry::AgentListLocalFactUpdate,
    ) -> Result<(), lingxi_core::host::task_registry::TaskRegistryError> {
        assert_eq!(agent_id, self.owner);
        self.agent_fact_updates.lock().unwrap().push(update);
        Ok(())
    }

    async fn create(
        &self,
        _: lingxi_core::host::task_registry::TaskCreateInput,
    ) -> Result<
        lingxi_core::host::task_registry::TaskRecord,
        lingxi_core::host::task_registry::TaskRegistryError,
    > {
        unreachable!()
    }
    async fn get(
        &self,
        task_id: &str,
    ) -> Result<
        Option<lingxi_core::host::task_registry::TaskRecord>,
        lingxi_core::host::task_registry::TaskRegistryError,
    > {
        Ok(self
            .records
            .lock()
            .unwrap()
            .iter()
            .find(|record| record.task_id == task_id)
            .cloned())
    }
    async fn list(
        &self,
        _: lingxi_core::host::task_registry::TaskListFilter,
    ) -> Result<
        Vec<lingxi_core::host::task_registry::TaskRecord>,
        lingxi_core::host::task_registry::TaskRegistryError,
    > {
        Ok(self.records.lock().unwrap().clone())
    }
    async fn update(
        &self,
        _: &str,
        _: lingxi_core::host::task_registry::TaskUpdatePatch,
    ) -> Result<
        lingxi_core::host::task_registry::TaskRecord,
        lingxi_core::host::task_registry::TaskRegistryError,
    > {
        unreachable!()
    }
    async fn set_status(
        &self,
        _: &str,
        _: &str,
    ) -> Result<
        lingxi_core::host::task_registry::TaskRecord,
        lingxi_core::host::task_registry::TaskRegistryError,
    > {
        unreachable!()
    }
    async fn kill(
        &self,
        _: &str,
    ) -> Result<
        lingxi_core::host::task_registry::TaskRecord,
        lingxi_core::host::task_registry::TaskRegistryError,
    > {
        unreachable!()
    }
    async fn output(
        &self,
        _: &str,
        _: Option<u64>,
    ) -> Result<
        lingxi_core::host::task_registry::TaskOutputChunk,
        lingxi_core::host::task_registry::TaskRegistryError,
    > {
        unreachable!()
    }
    async fn park_foreground_agent(
        &self,
        agent_id: lingxi_core::types::AgentId,
        _: lingxi_core::host::task_registry::AgentTerminalOutcome,
    ) -> bool {
        assert_eq!(agent_id, self.owner);
        self.park_foreground
    }
    async fn can_wake_agent_for_task_notification(&self, _: lingxi_core::types::AgentId) -> bool {
        let acknowledged = self.rest_acknowledged.load(Ordering::SeqCst);
        self.wake_checked.notify_one();
        acknowledged
    }
    fn subscribe_task_notifications(&self) -> Option<tokio::sync::watch::Receiver<u64>> {
        Some(self.revision.subscribe())
    }
    async fn take_pending_task_notifications_for(
        &self,
        recipient: Option<lingxi_core::types::AgentId>,
    ) -> Result<
        Vec<lingxi_core::host::task_registry::TaskNotification>,
        lingxi_core::host::task_registry::TaskRegistryError,
    > {
        assert_eq!(recipient, Some(self.owner));
        let pending = std::mem::take(&mut *self.pending.lock().unwrap());
        if self.drains.fetch_add(1, Ordering::SeqCst) >= 2 {
            self.parked_fold.notify_one();
        }
        Ok(pending)
    }
}

#[tokio::test]
async fn local_agent_idle_fact_tracks_native_outstanding_agent_tool_set() {
    // Native adds a whole assistant row before checking whether all pending
    // tool calls are Agent calls. It then removes each id when that call's
    // result is produced. The second row intentionally puts Read first so
    // removing it exposes the still-pending Agent call as idle.
    let api = MockSubagentApiClient::new(vec![
        Ok(tool_use_response("Agent", Some("tool_use"))),
        Ok(tool_uses_response(&["Read", "Agent"])),
        Ok(text_response("done", Some("end_turn"))),
    ]);
    let invoker = CountingInvoker::new();
    let mut ctx = loop_ctx(api.clone(), Some(invoker.clone()), 3);
    let registry = Arc::new(OwnerNotificationRegistry {
        park_foreground: false,
        rest_acknowledged: AtomicBool::new(true),
        wake_checked: tokio::sync::Notify::new(),
        drains: AtomicUsize::new(0),
        parked_fold: tokio::sync::Notify::new(),
        owner: ctx.agent_id,
        pending: Mutex::new(vec![]),
        agent_fact_updates: Mutex::new(vec![]),
        records: Mutex::new(vec![]),
        revision: tokio::sync::watch::channel(0).0,
    });
    ctx.task_registry = Some(registry.clone());

    let (_event_tx, event_rx) = mpsc::channel(8);
    let (out_tx, mut out_rx) = mpsc::channel(32);
    let runner = tokio::spawn(run_subagent(ctx, event_rx, out_tx));
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !matches!(out_rx.recv().await, Some(SubagentEvent::Completed { .. })) {}
    })
    .await
    .expect("scripted local agent finishes");
    runner.await.unwrap();

    assert_eq!(api.call_count(), 3);
    assert_eq!(invoker.call_count(), 3);
    assert_eq!(
        *registry.agent_fact_updates.lock().unwrap(),
        vec![
            lingxi_core::host::task_registry::AgentListLocalFactUpdate::IsIdle(true),
            lingxi_core::host::task_registry::AgentListLocalFactUpdate::IsIdle(false),
            lingxi_core::host::task_registry::AgentListLocalFactUpdate::IsIdle(true),
            lingxi_core::host::task_registry::AgentListLocalFactUpdate::IsIdle(false),
        ],
        "all assistant tool IDs must be registered before dispatch, and each matching result removes its own ID"
    );
}

#[tokio::test]
async fn notified_local_agent_removes_only_its_parent_keepalive_reason() {
    let mut ctx = loop_ctx(MockSubagentApiClient::new(vec![]), None, 1);
    let child_agent_id = lingxi_core::types::AgentId::new();
    let active_child_agent_id = lingxi_core::types::AgentId::new();
    let stale_child_agent_id = lingxi_core::types::AgentId::new();
    let child_reason = format!("agent:{}", child_agent_id.as_uuid());
    let active_reason = format!("agent:{}", active_child_agent_id.as_uuid());
    let stale_reason = format!("agent:{}", stale_child_agent_id.as_uuid());
    let add = |reason: String| {
        lingxi_core::host::task_registry::AgentListLocalFactUpdate::KeepaliveReason {
            reason,
            active: true,
        }
    };
    let parent = lingxi_core::host::task_registry::TaskRecord {
        task_id: "aparent".into(),
        task_type: "local_agent".into(),
        status: "running".into(),
        ..Default::default()
    };
    let mut parent = parent;
    parent.agent_facts = Some(lingxi_core::host::task_registry::AgentTaskFacts {
        stable_agent_id: lingxi_core::host::task_registry::FieldPresence::Value(
            ctx.agent_id.as_uuid().to_string(),
        ),
        keepalive_reasons: lingxi_core::host::task_registry::FieldPresence::Value(vec![
            child_reason.clone(),
            active_reason.clone(),
            stale_reason.clone(),
            "bash:background-task".into(),
        ]),
        ..Default::default()
    });
    let mut child = lingxi_core::host::task_registry::TaskRecord {
        task_id: "achild".into(),
        task_type: "local_agent".into(),
        status: "completed".into(),
        notified: true,
        owner_agent_id: Some(child_agent_id.to_string()),
        ..Default::default()
    };
    child.agent_facts = Some(lingxi_core::host::task_registry::AgentTaskFacts {
        stable_agent_id: lingxi_core::host::task_registry::FieldPresence::Value(
            child_agent_id.as_uuid().to_string(),
        ),
        parent_id: lingxi_core::host::task_registry::FieldPresence::Value(serde_json::json!(ctx
            .agent_id
            .to_string())),
        ..Default::default()
    });
    let mut active_child = lingxi_core::host::task_registry::TaskRecord {
        task_id: "aactive".into(),
        task_type: "local_agent".into(),
        status: "running".into(),
        notified: false,
        ..Default::default()
    };
    active_child.agent_facts = Some(lingxi_core::host::task_registry::AgentTaskFacts {
        stable_agent_id: lingxi_core::host::task_registry::FieldPresence::Value(
            active_child_agent_id.as_uuid().to_string(),
        ),
        parent_id: lingxi_core::host::task_registry::FieldPresence::Value(serde_json::json!(ctx
            .agent_id
            .to_string())),
        ..Default::default()
    });
    let registry = Arc::new(OwnerNotificationRegistry {
        park_foreground: false,
        rest_acknowledged: AtomicBool::new(true),
        wake_checked: tokio::sync::Notify::new(),
        drains: AtomicUsize::new(0),
        parked_fold: tokio::sync::Notify::new(),
        owner: ctx.agent_id,
        pending: Mutex::new(vec![]),
        agent_fact_updates: Mutex::new(vec![
            add(child_reason.clone()),
            add(active_reason.clone()),
            add(stale_reason.clone()),
        ]),
        records: Mutex::new(vec![parent, child, active_child]),
        revision: tokio::sync::watch::channel(0).0,
    });
    registry.publish();
    ctx.task_registry = Some(registry.clone());

    let mut history = Vec::new();
    assert!(fold_task_notifications(&ctx, &mut history).await);
    assert_eq!(history.len(), 1, "the completion notice is folded once");
    assert_eq!(
        *registry.agent_fact_updates.lock().unwrap(),
        vec![
            add(child_reason.clone()),
            add(active_reason),
            add(stale_reason.clone()),
            lingxi_core::host::task_registry::AgentListLocalFactUpdate::KeepaliveReason {
                reason: child_reason,
                active: false,
            },
            lingxi_core::host::task_registry::AgentListLocalFactUpdate::KeepaliveReason {
                reason: stale_reason,
                active: false,
            },
        ],
        "notified or missing Agent rows are pruned while live Agent and non-Agent reasons stay"
    );
}

#[tokio::test]
async fn owner_notification_wakes_parked_runner_without_user_message() {
    let api = MockSubagentApiClient::new(vec![
        Ok(text_response("first", Some("end_turn"))),
        Ok(text_response("second", Some("end_turn"))),
    ]);
    let mut ctx = loop_ctx(api.clone(), None, 4);
    ctx.persistent = true;
    let registry = Arc::new(OwnerNotificationRegistry {
        park_foreground: false,
        rest_acknowledged: AtomicBool::new(true),
        wake_checked: tokio::sync::Notify::new(),
        drains: AtomicUsize::new(0),
        parked_fold: tokio::sync::Notify::new(),
        owner: ctx.agent_id,
        pending: Mutex::new(vec![]),
        agent_fact_updates: Mutex::new(vec![]),
        records: Mutex::new(vec![]),
        revision: tokio::sync::watch::channel(0).0,
    });
    ctx.task_registry = Some(registry.clone());
    let (event_tx, event_rx) = mpsc::channel(8);
    let (out_tx, mut out_rx) = mpsc::channel(32);
    let runner = tokio::spawn(run_subagent(ctx, event_rx, out_tx));
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !matches!(out_rx.recv().await, Some(SubagentEvent::Completed { .. })) {}
        registry.parked_fold.notified().await;
        registry.publish();
        let wake = out_rx
            .recv()
            .await
            .expect("notification wakes the idle observer");
        let SubagentEvent::Message { message, .. } = wake else {
            panic!("wake message must precede resumed provider progress");
        };
        let wake: ConversationMessage = serde_json::from_value(message).unwrap();
        assert!(matches!(
            wake,
            ConversationMessage::User { is_meta: true, .. }
        ));
        assert!(serde_json::to_string(&wake)
            .unwrap()
            .contains("<task-id>achild</task-id>"));
        while let Some(event) = out_rx.recv().await {
            if let SubagentEvent::Message { message, .. } = &event {
                assert!(
                    message["role"] != "user",
                    "notification wake is emitted exactly once"
                );
            }
            if matches!(event, SubagentEvent::Completed { .. }) {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(api.call_count(), 2);
    let json = serde_json::to_string(&api.last_messages()).unwrap();
    assert_eq!(json.matches("<task-id>achild</task-id>").count(), 1);
    drop(event_tx);
    runner.await.unwrap();
}

struct NotificationDuringRequestApi {
    registry: Arc<OwnerNotificationRegistry>,
    completed: AtomicBool,
    calls: AtomicUsize,
    last_messages: Mutex<Vec<ConversationMessage>>,
}
#[async_trait]
impl crate::api::SubagentApiClient for NotificationDuringRequestApi {
    async fn stream(
        &self,
        request: crate::api::SubagentApiRequest,
    ) -> Result<
        futures::stream::BoxStream<
            'static,
            Result<llm_runtime::HistoryEvent, llm_runtime::LlmError>,
        >,
        llm_runtime::LlmError,
    > {
        let messages = request.messages;
        let response: Result<llm_runtime::HistoryResponse, llm_runtime::LlmError> = async {
            *self.last_messages.lock().unwrap() = messages;
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                self.registry.publish();
                // Notification arrives with a genuinely pending provider future.
                tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                self.completed.store(true, Ordering::SeqCst);
            }
            Ok(text_response("done", Some("end_turn")))
        }
        .await;
        let events = llm_runtime::stream_accumulator::response_to_stream_events(response?);
        Ok(futures::StreamExt::boxed(futures::stream::iter(
            events.into_iter().map(Ok),
        )))
    }
}
#[tokio::test]
async fn owner_notification_folds_after_inflight_request_without_cancelling_it() {
    let mut ctx = fresh_subagent_ctx();
    let registry = Arc::new(OwnerNotificationRegistry {
        park_foreground: false,
        rest_acknowledged: AtomicBool::new(true),
        wake_checked: tokio::sync::Notify::new(),
        drains: AtomicUsize::new(0),
        parked_fold: tokio::sync::Notify::new(),
        owner: ctx.agent_id,
        pending: Mutex::new(vec![]),
        agent_fact_updates: Mutex::new(vec![]),
        records: Mutex::new(vec![]),
        revision: tokio::sync::watch::channel(0).0,
    });
    let api = Arc::new(NotificationDuringRequestApi {
        registry: registry.clone(),
        completed: AtomicBool::new(false),
        calls: AtomicUsize::new(0),
        last_messages: Mutex::new(vec![]),
    });
    ctx.api_client = Some(api.clone());
    ctx.task_registry = Some(registry);
    ctx.agent_definition.max_turns = 4;
    let (event_tx, event_rx) = mpsc::channel(8);
    let (out_tx, mut out_rx) = mpsc::channel(32);
    let runner = tokio::spawn(run_subagent(ctx, event_rx, out_tx));
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !matches!(out_rx.recv().await, Some(SubagentEvent::Completed { .. })) {}
    })
    .await
    .unwrap();
    assert!(
        api.completed.load(Ordering::SeqCst),
        "original provider future survived notification"
    );
    assert_eq!(api.calls.load(Ordering::SeqCst), 2);
    assert!(serde_json::to_string(&*api.last_messages.lock().unwrap())
        .unwrap()
        .contains("<task-id>achild</task-id>"));
    drop(event_tx);
    runner.await.unwrap();
}

#[tokio::test]
async fn owner_notification_waits_for_handler_rest_acknowledgement() {
    let api = MockSubagentApiClient::new(vec![
        Ok(text_response("first", Some("end_turn"))),
        Ok(text_response("second", Some("end_turn"))),
    ]);
    let mut ctx = loop_ctx(api.clone(), None, 4);
    ctx.persistent = true;
    let registry = Arc::new(OwnerNotificationRegistry {
        park_foreground: false,
        rest_acknowledged: AtomicBool::new(false),
        wake_checked: tokio::sync::Notify::new(),
        drains: AtomicUsize::new(0),
        parked_fold: tokio::sync::Notify::new(),
        owner: ctx.agent_id,
        pending: Mutex::new(vec![]),
        agent_fact_updates: Mutex::new(vec![]),
        records: Mutex::new(vec![]),
        revision: tokio::sync::watch::channel(0).0,
    });
    ctx.task_registry = Some(registry.clone());
    let (event_tx, event_rx) = mpsc::channel(8);
    let (out_tx, mut out_rx) = mpsc::channel(32);
    let runner = tokio::spawn(run_subagent(ctx, event_rx, out_tx));
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !matches!(out_rx.recv().await, Some(SubagentEvent::Completed { .. })) {}
        registry.wake_checked.notified().await;
        registry.publish();
        registry.wake_checked.notified().await;
        assert_eq!(
            api.call_count(),
            1,
            "pending notification cannot outrun handler rest acknowledgement"
        );
        registry.rest_acknowledged.store(true, Ordering::SeqCst);
        registry.revision.send_modify(|revision| *revision += 1);
        while !matches!(out_rx.recv().await, Some(SubagentEvent::Completed { .. })) {}
    })
    .await
    .unwrap();
    assert_eq!(api.call_count(), 2);
    drop(event_tx);
    runner.await.unwrap();
}

// ── Subagent refusal cascade ───────────────────────────────────────────────
//
// claude-code runs subagents through the SAME query generator as the main
// thread, so a refusing subagent hops to the fallback model and retries. This
// port's subagent loop is separate and treated `refusal` as an ordinary
// terminal stop reason, so the run simply ended.

#[tokio::test]
async fn a_refusing_subagent_hops_to_the_fallback_and_retries() {
    let mut refusal = text_response("", Some("refusal"));
    refusal.stop_details = Some(llm_runtime::HistoryStopDetails {
        category: Some("cyber".into()),
        explanation: Some("fixture detail".into()),
    });
    refusal.usage.counts_mut().input_tokens = 11;
    let mut answer = text_response("done", Some("end_turn"));
    answer.usage.counts_mut().output_tokens = 7;
    let api = StreamingMockApiClient::new(vec![
        llm_runtime::stream_accumulator::response_to_stream_events(refusal),
        llm_runtime::stream_accumulator::response_to_stream_events(answer),
    ]);
    let mut ctx = loop_ctx(api.clone(), None, 3);
    ctx.refusal_fallback_chain = vec!["fallback-model".to_string()];

    let (event_tx, event_rx) = mpsc::channel(1);
    drop(event_tx);
    let (out_tx, out_rx) = mpsc::channel(32);
    run_subagent(ctx, event_rx, out_tx).await;

    let events = drain(out_rx).await;
    let text = events
        .iter()
        .find_map(|e| match e {
            SubagentEvent::Completed { result, .. } => {
                Some(result["text"].as_str().unwrap_or_default().to_string())
            }
            _ => None,
        })
        .expect("the run completed");
    assert!(
        text.contains("done"),
        "the retry's answer is the run's result: {text:?}"
    );
    // The answer also carries the `ICe` note naming the model that produced it
    // — see `a_hopped_subagents_answer_carries_the_refusal_note`.
    assert!(text.contains('\u{26A0}'), "…prefixed by the note: {text:?}");
    assert_eq!(
        api.call_count(),
        2,
        "a refusal with a hop left must re-issue, not end the run"
    );
    assert_eq!(
        api.models().get(1).map(String::as_str),
        Some("fallback-model"),
        "the retry must go to the fallback, not back to the refusing model: {:?}",
        api.models()
    );
    let calls = api.physical_calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(
        calls
            .iter()
            .map(|call| call.request.model.as_str())
            .collect::<Vec<_>>(),
        ["inherit", "fallback-model"]
    );
    assert_eq!(
        calls
            .iter()
            .map(|call| {
                call.response
                    .as_ref()
                    .and_then(|response| response.stop_reason.as_deref())
            })
            .collect::<Vec<_>>(),
        [Some("refusal"), Some("end_turn")],
        "the stream fixture keeps each physical response's stop metadata"
    );
    assert_eq!(
        calls[0]
            .response
            .as_ref()
            .and_then(|response| response.stop_details.as_ref())
            .and_then(|details| details.category.as_deref()),
        Some("cyber")
    );
    assert_eq!(
        calls[0]
            .response
            .as_ref()
            .map(|response| response.usage.counts().input_tokens),
        Some(11)
    );
    assert_eq!(
        calls[1]
            .response
            .as_ref()
            .map(|response| response.usage.counts().output_tokens),
        Some(7)
    );
    assert!(
        calls[1].request.messages.iter().any(|message| matches!(
            message,
            ConversationMessage::System { subtype: Some(subtype), .. }
                if subtype == "model_refusal_fallback"
        )),
        "the retry request includes the local refusal note"
    );
}

#[tokio::test]
async fn skill_selection_of_the_serving_model_clears_the_refusal_target() {
    for second_tool in [
        Some("NestedMutateSecond"),
        Some("NestedReselectInitial"),
        None,
    ] {
        let mut registry = tool_api::ToolRegistry::new();
        for name in std::iter::once("Skill").chain(second_tool) {
            registry.register_builtin(Arc::new(NestedToolEffectsProbe {
                name,
                schema: serde_json::json!({"type":"object","additionalProperties":true}),
                snapshots: Arc::new(Mutex::new(Vec::new())),
                modifier_order: Arc::new(Mutex::new(Vec::new())),
                meta_seen: Arc::new(AtomicUsize::new(0)),
                modifier_calls: Arc::new(AtomicUsize::new(0)),
            }));
        }
        let invoker: Arc<dyn lingxi_core::host::ToolInvoker> =
            Arc::new(tool_api::RegistryToolInvoker::new(Arc::new(registry)));
        let mut picked = tool_use_response("Skill", Some("tool_use"));
        if let Some(name) = second_tool {
            picked.content.push(llm_runtime::ContentBlock::ToolCall {
                input_projection: None,
                id: ToolUseId::new().to_string(),
                name: name.into(),
                input: serde_json::json!({}),
            });
        }
        let api = MockSubagentApiClient::new(vec![
            Ok(text_response("", Some("refusal"))),
            Ok(picked),
            Ok(text_response("done", Some("end_turn"))),
        ]);
        let mut ctx = loop_ctx(api.clone(), Some(invoker), 4);
        ctx.agent_definition.model = AgentModel::Explicit("initial".into());
        ctx.model_profile = Some("origin-provider".into());
        ctx.refusal_fallback_chain = vec!["nested-model-final".into()];
        ctx.model_resolution_context_provider =
            Some(Arc::new(|model: &str, profile: Option<&str>| {
                Ok(crate::model_resolution::ModelResolutionContext {
                    route: crate::model_resolution::ModelRouteFacts {
                        model: model.to_string(),
                        profile: profile.or(Some("origin-provider")).map(str::to_string),
                        ..Default::default()
                    },
                    ..Default::default()
                })
            }));
        let transcript_dir = tempfile::tempdir().unwrap();
        ctx.transcript_subdir = transcript_dir.path().to_path_buf();
        ctx.transcript_fs = Some(Arc::new(platform_posix::PosixFileSystem::new(
            transcript_dir.path().to_path_buf(),
        )));
        let transcript_path = transcript_dir
            .path()
            .join(format!("agent-{}.jsonl", ctx.agent_id));
        let (event_tx, event_rx) = mpsc::channel(1);
        drop(event_tx);
        let (out_tx, out_rx) = mpsc::channel(64);
        run_subagent(ctx, event_rx, out_tx).await;
        assert!(drain(out_rx)
            .await
            .iter()
            .any(|event| matches!(event, SubagentEvent::Completed { .. })));
        let calls = api.physical_calls();
        let physical_model = if second_tool == Some("NestedReselectInitial") {
            "initial"
        } else {
            "nested-model-final"
        };
        assert_eq!(
            calls
                .iter()
                .map(|call| call.request.model.as_str())
                .collect::<Vec<_>>(),
            ["initial", "nested-model-final", physical_model]
        );
        let serving = calls[1].fallback_target.as_ref().unwrap();
        assert_eq!(serving.user_model, "initial");
        assert!(serving.is_target("nested-model-final", str::to_string));
        let selected = calls[2].fallback_target.as_ref().unwrap();
        if second_tool.is_some() {
            assert_eq!(selected.user_model, physical_model);
            assert_eq!(selected.turn_override, None);
            assert!(!selected.is_target(physical_model, str::to_string));
        } else {
            assert_eq!(selected.user_model, "initial");
            assert_eq!(
                selected.turn_override.as_deref(),
                Some("nested-model-final")
            );
            assert!(
                selected.is_target(physical_model, str::to_string),
                "a prompt-only modifier retains the serving refusal route"
            );
        }
        let expected_profile = if second_tool == Some("NestedMutateSecond") {
            "nested-profile-final"
        } else {
            "origin-provider"
        };
        assert_eq!(calls[2].request.profile.as_deref(), Some(expected_profile));
        let transcript = tokio::fs::read_to_string(transcript_path).await.unwrap();
        let selections: Vec<serde_json::Value> = transcript
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .filter(|row| row["type"] == "model-selection")
            .collect();
        assert_eq!(selections.len(), usize::from(second_tool.is_some()));
        if let Some(selection) = selections.first() {
            assert_eq!(selection["model"], physical_model);
            assert_eq!(selection["model_profile"], expected_profile);
        }
    }
}

#[tokio::test]
async fn local_refusal_serving_model_does_not_rewrite_tool_context() {
    struct ModelScopeInvoker {
        route: Mutex<Option<(Option<String>, Option<String>)>>,
    }
    #[async_trait]
    impl lingxi_core::host::ToolInvoker for ModelScopeInvoker {
        async fn invoke(
            &self,
            _: &str,
            _: serde_json::Value,
            context: lingxi_core::host::tool_invoker::SubagentInvocationContext,
        ) -> Result<serde_json::Value, lingxi_core::host::tool_invoker::ToolInvokerError> {
            *self.route.lock().unwrap() =
                Some((context.parent_model, context.parent_model_profile));
            Ok(serde_json::json!("done"))
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../tests/fixtures/child_refusal_scope_2_1_287.json"
    ))
    .unwrap();
    let expected = fixture["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["main"] == false && row["silent"] == false)
        .unwrap();
    let api = StreamingMockApiClient::new(vec![
        streamed_text_turn("", "refusal"),
        streamed_tool_use_turn("Read", "tool_use"),
        streamed_text_turn("done", "end_turn"),
    ]);
    let invoker = Arc::new(ModelScopeInvoker {
        route: Mutex::new(None),
    });
    let mut context = loop_ctx(api.clone(), Some(invoker.clone()), 3);
    context.agent_definition.model = AgentModel::Explicit("initial".into());
    context.model_profile = Some("origin-provider".into());
    context.refusal_fallback_chain = vec!["fallback".into()];
    let (events, receiver) = mpsc::channel(1);
    drop(events);
    let (output, output_receiver) = mpsc::channel(32);
    run_subagent(context, receiver, output).await;
    let _ = drain(output_receiver).await;
    assert_eq!(
        api.models().get(1).map(String::as_str),
        expected["expected"]["requestModel"].as_str()
    );
    let tool_route = invoker
        .route
        .lock()
        .unwrap()
        .clone()
        .expect("Read receives the current logical tool route");
    assert_eq!(
        tool_route.0.as_deref(),
        expected["expected"]["toolModel"].as_str()
    );
    assert_eq!(
        tool_route.1.as_deref(),
        Some("origin-provider"),
        "a serving-model fallback does not rewrite the tool's logical profile"
    );
}

/// The control: with no chain configured a refusal is still terminal, which is
/// every subagent's behaviour before this. Without it the test above would
/// pass just as well if the loop retried unconditionally.
#[tokio::test]
async fn a_refusal_with_no_chain_configured_still_ends_the_run() {
    let api = StreamingMockApiClient::new(vec![
        streamed_text_turn("", "refusal"),
        streamed_text_turn("done", "end_turn"),
    ]);
    let ctx = loop_ctx(api.clone(), None, 3);
    assert!(
        ctx.refusal_fallback_chain.is_empty(),
        "precondition: nothing configured"
    );

    let (event_tx, event_rx) = mpsc::channel(1);
    drop(event_tx);
    let (out_tx, _out_rx) = mpsc::channel(32);
    run_subagent(ctx, event_rx, out_tx).await;

    assert_eq!(
        api.call_count(),
        1,
        "no chain ⇒ the refusal is terminal, as before"
    );
}

/// The cascade is bounded by its chain: each hop is consumed, so a subagent
/// that keeps refusing stops rather than looping over the same models.
#[tokio::test]
async fn a_subagent_cascade_stops_when_the_chain_is_exhausted() {
    let api = StreamingMockApiClient::new(vec![
        streamed_text_turn("", "refusal"),
        streamed_text_turn("", "refusal"),
        streamed_text_turn("", "refusal"),
        streamed_text_turn("done", "end_turn"),
    ]);
    let mut ctx = loop_ctx(api.clone(), None, 8);
    ctx.refusal_fallback_chain = vec!["hop-one".to_string(), "hop-two".to_string()];

    let (event_tx, event_rx) = mpsc::channel(1);
    drop(event_tx);
    let (out_tx, _out_rx) = mpsc::channel(32);
    run_subagent(ctx, event_rx, out_tx).await;

    assert_eq!(
        api.call_count(),
        3,
        "the original call plus one per chain entry, then terminal: {:?}",
        api.models()
    );
    assert_eq!(
        api.models()[1..].to_vec(),
        vec!["hop-one".to_string(), "hop-two".to_string()],
        "each hop is consumed once, in order"
    );
    let calls = api.physical_calls();
    assert_eq!(calls.len(), 3);
    assert_eq!(
        calls
            .iter()
            .map(|call| {
                call.response
                    .as_ref()
                    .and_then(|response| response.stop_reason.as_deref())
            })
            .collect::<Vec<_>>(),
        [Some("refusal"), Some("refusal"), Some("refusal")]
    );
}

/// A subagent's swap is `scope: "local"` — it lasts for this run and does not
/// touch the session model, unlike the main thread's, which is `"session"`.
/// claude-code's `ICe` looks for exactly the local one.
#[test]
fn a_subagent_refusal_frame_is_scoped_local() {
    let frame = super::refusal_fallback_frame(
        MessageId::new(),
        &lingxi_core::host::refusal_notice::RefusalNotice {
            origin_model: "refusing-model".to_string(),
            serving_model: "fallback-model".to_string(),
            ..lingxi_core::host::refusal_notice::RefusalNotice::default()
        },
    );
    match frame {
        ConversationMessage::System {
            subtype,
            refusal_fallback: Some(meta),
            ..
        } => {
            assert_eq!(subtype.as_deref(), Some("model_refusal_fallback"));
            assert_eq!(meta.scope.as_deref(), Some("local"));
            assert_eq!(meta.original_model, "refusing-model");
            assert_eq!(meta.fallback_model, "fallback-model");
        }
        other => panic!("expected a typed system frame, got {other:?}"),
    }
}

// ── `ICe` / `PZo`: the harness note and the retraction filter ───────────────

/// `iht`'s `⚠ ${notice.content}` note. The parent asked a subagent a question
/// and got an answer from a DIFFERENT model than it dispatched; upstream says
/// so in the result. Without the note the swap is invisible to the caller.
#[tokio::test]
async fn a_hopped_subagents_answer_carries_the_refusal_note() {
    let api = StreamingMockApiClient::new(vec![
        streamed_text_turn("", "refusal"),
        streamed_text_turn("the answer", "end_turn"),
    ]);
    let mut ctx = loop_ctx(api.clone(), None, 3);
    ctx.refusal_fallback_chain = vec!["fallback-model".to_string()];

    let (event_tx, event_rx) = mpsc::channel(1);
    drop(event_tx);
    let (out_tx, out_rx) = mpsc::channel(32);
    run_subagent(ctx, event_rx, out_tx).await;

    let events = drain(out_rx).await;
    let result = events
        .iter()
        .find_map(|e| match e {
            SubagentEvent::Completed { result, .. } => Some(result.clone()),
            _ => None,
        })
        .expect("the run completed");
    let text = result["text"].as_str().unwrap_or_default();
    assert!(
        text.contains('\u{26A0}') && text.contains("fallback-model"),
        "the answer must name the model that actually produced it: {text:?}"
    );
    assert!(
        text.contains("the answer"),
        "and it must still carry the report: {text:?}"
    );
    let first = result["content"][0]["text"].as_str().unwrap_or_default();
    assert!(
        first.starts_with('\u{26A0}'),
        "the note is a leading block, ahead of the report: {first:?}"
    );
}

/// The control: a run that never hopped has no note. Without it the test above
/// would pass just as well if the note were unconditional.
#[tokio::test]
async fn a_subagent_that_never_refused_carries_no_note() {
    let api = StreamingMockApiClient::new(vec![streamed_text_turn("the answer", "end_turn")]);
    let mut ctx = loop_ctx(api.clone(), None, 3);
    ctx.refusal_fallback_chain = vec!["fallback-model".to_string()];

    let (event_tx, event_rx) = mpsc::channel(1);
    drop(event_tx);
    let (out_tx, out_rx) = mpsc::channel(32);
    run_subagent(ctx, event_rx, out_tx).await;

    let events = drain(out_rx).await;
    let result = events
        .iter()
        .find_map(|e| match e {
            SubagentEvent::Completed { result, .. } => Some(result.clone()),
            _ => None,
        })
        .expect("the run completed");
    assert_eq!(result["text"].as_str(), Some("the answer"));
}

/// `PZo` — a notice that supersedes an earlier hop names the messages that hop
/// produced, and those must not survive into the answer. System messages always
/// do: the notices are how the retraction is expressed at all.
#[test]
fn retracted_messages_are_dropped_but_notices_survive() {
    let assistant = |text: &str| ConversationMessage::Assistant { per_turn_effort: None,
        id: MessageId::new(),
        content: vec![lingxi_core::types::ContentBlock::Text {
            text: text.to_string(),
            citations: None,
        }],
        stop_reason: None,
    };
    let doomed = assistant("superseded output");
    let doomed_uuid = doomed.id().as_uuid().to_string();
    let kept = assistant("live output");
    let notice = super::refusal_fallback_frame(
        MessageId::new(),
        &lingxi_core::host::refusal_notice::RefusalNotice {
            origin_model: "refusing".to_string(),
            serving_model: "fallback".to_string(),
            retracted_message_uuids: vec![doomed_uuid],
            ..lingxi_core::host::refusal_notice::RefusalNotice::default()
        },
    );

    let live = super::drop_retracted(&[doomed, notice, kept]);

    assert_eq!(live.len(), 2, "the superseded message is gone: {live:?}");
    assert!(
        live.iter()
            .any(|m| matches!(m, ConversationMessage::System { .. })),
        "the notice itself survives"
    );
    assert!(
        live.iter().any(|m| matches!(
            m,
            ConversationMessage::Assistant { content, .. }
                if content.iter().any(|b| matches!(b, lingxi_core::types::ContentBlock::Text { text, .. } if text == "live output"))
        )),
        "the unretracted message survives"
    );
}

/// A notice for a model that is NOT the one serving the answer is not this
/// run's explanation — matching upstream's `fallbackModel === answer's model`.
#[test]
fn a_notice_for_another_model_is_not_picked() {
    let notice = super::refusal_fallback_frame(
        MessageId::new(),
        &lingxi_core::host::refusal_notice::RefusalNotice {
            serving_model: "hop-one".to_string(),
            ..lingxi_core::host::refusal_notice::RefusalNotice::default()
        },
    );
    let history = vec![notice];
    assert!(super::local_refusal_notice(&history, "hop-two").is_none());
    assert!(super::local_refusal_notice(&history, "hop-one").is_some());
}

#[tokio::test]
async fn skill_preload_read_error_fails_before_model_request() {
    struct FailingLoader;
    #[async_trait]
    impl lingxi_core::host::skill_loader::SkillLoader for FailingLoader {
        async fn resolve_and_load(
            &self,
            _: &str,
            _: &str,
            _cwd: Option<&std::path::Path>,
            _model: Option<&str>,
        ) -> Result<Option<lingxi_core::host::skill_loader::SkillLoad>, String> {
            Err("loop.md denied".into())
        }
    }
    let api = CapturingApiClient::new();
    let mut ctx = loop_ctx(api.clone(), None, 2);
    ctx.agent_definition.skills = vec!["loop".into()];
    ctx.skill_loader = Some(Arc::new(FailingLoader));
    ctx.prompt_messages = vec![ConversationMessage::user(
        MessageId::new(),
        "preload child evidence".into(),
    )];
    let executor = attach_failure_hook_executor(&mut ctx);
    let session_id = ctx.hook_session_id;
    let agent_id = ctx.agent_id;
    let (event_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(16);
    drop(event_tx);
    run_subagent(ctx, event_rx, out_tx).await;
    let events = drain(out_rx).await;
    assert!(api.captured().is_empty());
    assert!(events.iter().any(|event| matches!(event, SubagentEvent::Failed { error, .. } if error.contains("loop.md denied"))));
    let (route, transcript) = executor
        .take_agent_prompt_transcript(session_id, agent_id)
        .expect("preload failures retain the child's hook snapshot");
    assert_eq!(route.model, "failure-child-model");
    assert_eq!(
        route.model_profile.as_deref(),
        Some("failure-child-profile")
    );
    assert!(transcript
        .messages
        .iter()
        .any(|message| message.text_content() == "preload child evidence"));
}

#[tokio::test]
async fn tool_reported_error_and_context_reach_model_history() {
    struct ReportedErrorInvoker;
    #[async_trait]
    impl lingxi_core::host::ToolInvoker for ReportedErrorInvoker {
        async fn invoke(
            &self,
            _: &str,
            _: serde_json::Value,
            _: lingxi_core::host::tool_invoker::SubagentInvocationContext,
        ) -> Result<serde_json::Value, lingxi_core::host::tool_invoker::ToolInvokerError> {
            unreachable!("runner must preserve detailed results")
        }
        async fn invoke_detailed(
            &self,
            _: &str,
            _: serde_json::Value,
            _: lingxi_core::host::tool_invoker::SubagentInvocationContext,
            _: Option<u64>,
        ) -> Result<
            lingxi_core::host::tool_invoker::ToolInvocationResult,
            lingxi_core::host::tool_invoker::ToolInvokerError,
        > {
            Ok(lingxi_core::host::tool_invoker::ToolInvocationResult {
                mcp_meta_projection: None,
                model_content_projection: None,
                data_projection: None,
                data: serde_json::json!({"code": "unavailable"}),
                model_content: Some("Contract unavailable".into()),
                is_error: true,
                turn_end: None,
                new_messages: Vec::new(),
                context_modifier: None,
                mcp_meta: None,
                context: lingxi_core::types::utf16_json::Utf16JsonProjection::plain(
                    serde_json::json!(["Contents of /tmp/AGENTS.md:\n\nNested rule"]),
                ),
                context_state: None,
            })
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }
    let api = ResultStreamMockApiClient::new(vec![
        streamed_tool_use_turn("Read", "tool_use")
            .into_iter()
            .map(Ok)
            .collect(),
        llm_runtime::stream_accumulator::response_to_stream_events(text_response(
            "stopped",
            Some("end_turn"),
        ))
        .into_iter()
        .map(Ok)
        .collect(),
    ]);
    let ctx = loop_ctx(api.clone(), Some(Arc::new(ReportedErrorInvoker)), 3);
    let (_tx, rx) = mpsc::channel(8);
    let (out, events) = mpsc::channel(32);
    tokio::spawn(run_subagent(ctx, rx, out)).await.unwrap();
    let events = drain(events).await;
    assert_eq!(api.call_count(), 2);
    one_completed(&events);
    assert!(api.histories.lock().unwrap()[1].iter().any(|message| {
        matches!(message, ConversationMessage::User { content, .. } if content.iter().any(|block| {
            matches!(block, ContentBlock::ToolResult { is_error: Some(true), content, .. } if content == "Contract unavailable")
        }))
    }));
    assert!(api.histories.lock().unwrap()[1].iter().any(|message| {
        message.is_meta()
            && message.text_content()
                == "<system-reminder>\ntool.call hook additional context: Contents of /tmp/AGENTS.md:\n\nNested rule\n</system-reminder>"
    }));
    assert!(events.iter().any(|event| {
        matches!(event, SubagentEvent::Message { message, .. } if message["content"].as_array().is_some_and(|blocks| blocks.iter().any(|block| block["type"] == "tool_result" && block["is_error"] == true && block["content"] == "Contract unavailable")))
    }));
}

#[tokio::test]
async fn child_tool_context_is_one_plugin_attachment_in_the_model_snapshot() {
    struct ContextInvoker;
    #[async_trait]
    impl lingxi_core::host::ToolInvoker for ContextInvoker {
        async fn invoke(
            &self,
            _: &str,
            _: serde_json::Value,
            _: lingxi_core::host::tool_invoker::SubagentInvocationContext,
        ) -> Result<serde_json::Value, lingxi_core::host::tool_invoker::ToolInvokerError> {
            unreachable!("runner must preserve detailed results")
        }

        async fn invoke_detailed(
            &self,
            _: &str,
            _: serde_json::Value,
            _: lingxi_core::host::tool_invoker::SubagentInvocationContext,
            _: Option<u64>,
        ) -> Result<
            lingxi_core::host::tool_invoker::ToolInvocationResult,
            lingxi_core::host::tool_invoker::ToolInvokerError,
        > {
            Ok(lingxi_core::host::tool_invoker::ToolInvocationResult {
                mcp_meta_projection: None,
                model_content_projection: None,
                data_projection: None,
                data: serde_json::json!({"ok":true}),
                model_content: Some("read ok".into()),
                is_error: false,
                turn_end: None,
                new_messages: Vec::new(),
                context_modifier: None,
                mcp_meta: None,
                context: lingxi_core::types::utf16_json::Utf16JsonProjection::plain(
                    serde_json::json!(["first", "second"]),
                ),
                context_state: None,
            })
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("attachment.js");
    std::fs::write(
        &module,
        r#"export function register(on) {
  let appended = [];
  on('prompt.attachment', { type: 'hook_additional_context' }, ($, e, next) => {
    if (e.origin.kind !== 'plugin' || e.origin.event !== 'tool.call' || !e.agentId)
      throw new Error('child attachment lost its origin or agentId');
    return { text: `rewritten:${e.agentId}:${e.text}` };
  });
  on('session.append', ($, e, next) => {
    appended.push({type:e.message.type, name:e.message.name, door:e.door,
      origin:e.origin, uuid:e.uuid, agentId:e.agentId,
      content:e.message.content});
    if (e.message.type === 'attachment' && e.message.name === 'hook_additional_context') {
      return next({ ...e, message: { ...e.message,
        content: [{type:'text', text:'source rewritten'}] } });
    }
    if (e.message.type === 'user' && e.message.isMeta === true &&
        e.message.content.some(block => block.text?.includes('tool.call hook additional context'))) {
      return next({ ...e, message: { ...e.message,
        content: e.message.content.map(block => ({...block,
          text:block.text.replace('\n</system-reminder>', '\nrow rewritten\n</system-reminder>')})) } });
    }
    return next(e);
  });
  on('prompt.submit', ($, e, next) => next({ ...e, text: JSON.stringify(appended) }));
}"#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load(
        "child-attachment",
        dir.path(),
        &module,
        serde_json::json!({}),
    )
    .await
    .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host.clone());
    let executor = Arc::new(hooks::HookExecutorImpl::new(
        Arc::new(tokio::sync::RwLock::new(registry)),
        Arc::new(test_harness::mocks::MockHttpTransport::new()),
        Arc::new(test_harness::mocks::MockRuntimeSpawner::default()),
    ));
    let api = ResultStreamMockApiClient::new(vec![
        streamed_tool_use_turn("Read", "tool_use")
            .into_iter()
            .map(Ok)
            .collect(),
        llm_runtime::stream_accumulator::response_to_stream_events(text_response(
            "done",
            Some("end_turn"),
        ))
        .into_iter()
        .map(Ok)
        .collect(),
    ]);
    let mut ctx = loop_ctx(api.clone(), Some(Arc::new(ContextInvoker)), 3);
    ctx.hook_executor = Some(executor);
    ctx.hook_cwd = dir.path().to_path_buf();
    ctx.transcript_subdir = dir.path().to_path_buf();
    ctx.transcript_fs = Some(Arc::new(platform_posix::PosixFileSystem::new(
        dir.path().to_path_buf(),
    )));
    let agent_id = ctx.agent_id.as_uuid().to_string();
    let agent_file_id = ctx.agent_id.to_string();
    let (_tx, rx) = mpsc::channel(8);
    let (out, events) = mpsc::channel(32);
    run_subagent(ctx, rx, out).await;
    let events = drain(events).await;
    assert_eq!(one_completed(&events)["text"], "done");
    let histories = api.histories.lock().unwrap();
    assert_eq!(histories.len(), 2);
    let second = &histories[1];
    let attachments = second
        .iter()
        .filter(|message| {
            message.is_meta()
                && message
                    .text_content()
                    .contains("tool.call hook additional context")
        })
        .collect::<Vec<_>>();
    assert_eq!(attachments.len(), 1);
    assert_eq!(
        attachments[0].text_content(),
        format!(
            "<system-reminder>\nrewritten:{agent_id}:tool.call hook additional context: first\nsecond\nrow rewritten\n</system-reminder>"
        )
    );
    assert!(events.iter().any(|event| matches!(
        event,
        SubagentEvent::Message { message, .. }
            if message.to_string().contains("tool.call hook additional context: first\\nsecond")
                && !message.to_string().contains("rewritten:")
    )));
    let transcript =
        std::fs::read_to_string(dir.path().join(format!("agent-{agent_file_id}.jsonl"))).unwrap();
    let sources = transcript
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter_map(|row| row.get("source_attachment").cloned())
        .collect::<Vec<_>>();
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0]["type"], "hook_additional_context");
    assert_eq!(
        sources[0]["content"],
        serde_json::json!(["source rewritten"])
    );
    assert_eq!(sources[0]["hookName"], "tool.call");
    assert!(sources[0]["toolUseID"]
        .as_str()
        .is_some_and(|id| id.ends_with("-context")));
    assert_eq!(sources[0]["hookEvent"], "PostToolUse");
    let entry = transcript
        .lines()
        .filter_map(|line| serde_json::from_str::<crate::transcript::TranscriptEntry>(line).ok())
        .find(|entry| entry.source_attachment.is_some())
        .expect("the rewritten source attachment is persisted with its row");
    assert!(entry.message.text_content().contains("row rewritten"));
    assert!(entry.source_attachment_uuid.is_some());
    let append_events: Vec<serde_json::Value> = host
        .dispatch(
            "prompt.submit",
            serde_json::json!({"text":"probe"}),
            |event| async move { Ok(event) },
        )
        .await
        .unwrap()["text"]
        .as_str()
        .and_then(|text| serde_json::from_str(text).ok())
        .expect("the Mod captured session.append inputs");
    let attachment_event = append_events
        .iter()
        .find(|event| event["type"] == "attachment" && event["name"] == "hook_additional_context")
        .expect("the original attachment side row is appended");
    let user_event = append_events
        .iter()
        .find(|event| {
            event["type"] == "user"
                && event["content"]
                    .to_string()
                    .contains("tool.call hook additional context")
        })
        .expect("the retained child user row is appended");
    assert_eq!(attachment_event["door"], "hook-context");
    assert_eq!(
        attachment_event["origin"],
        serde_json::json!({"kind":"plugin","event":"tool.call"})
    );
    assert_eq!(attachment_event["agentId"], agent_id);
    assert_eq!(attachment_event["content"][0]["text"], "first");
    assert_eq!(user_event["door"], "note");
    assert_eq!(user_event["origin"], serde_json::json!({"kind":"engine"}));
    assert_eq!(user_event["agentId"], agent_id);
    assert!(
        append_events
            .iter()
            .position(|event| event == attachment_event)
            < append_events.iter().position(|event| event == user_event)
    );
}

#[tokio::test]
async fn child_session_append_runs_without_a_transcript_filesystem() {
    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("append-without-transcript.js");
    std::fs::write(
        &module,
        r#"export function register(on) {
  const types = [];
  on('session.append', ($, e, next) => {
    types.push(e.message.type);
    return next(e);
  });
  on('prompt.submit', ($, e, next) => next({ ...e, text: types.join(',') }));
}"#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load(
        "append-without-transcript",
        dir.path(),
        &module,
        serde_json::json!({}),
    )
    .await
    .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host.clone());
    let executor = Arc::new(hooks::HookExecutorImpl::new(
        Arc::new(tokio::sync::RwLock::new(registry)),
        Arc::new(test_harness::mocks::MockHttpTransport::new()),
        Arc::new(test_harness::mocks::MockRuntimeSpawner::default()),
    ));
    let api = MockSubagentApiClient::new(vec![Ok(text_response("done", Some("end_turn")))]);
    let mut ctx = loop_ctx(api, None, 1);
    ctx.hook_executor = Some(executor);
    ctx.hook_cwd = dir.path().to_path_buf();
    assert!(ctx.transcript_fs.is_none());

    let (_tx, rx) = mpsc::channel(8);
    let (out, events) = mpsc::channel(16);
    run_subagent(ctx, rx, out).await;
    assert_eq!(one_completed(&drain(events).await)["text"], "done");
    let observed = host
        .dispatch(
            "prompt.submit",
            serde_json::json!({"text":"probe"}),
            |event| async move { Ok(event) },
        )
        .await
        .unwrap();
    assert!(observed["text"]
        .as_str()
        .is_some_and(|types| types.split(',').any(|kind| kind == "assistant")));
}

#[tokio::test]
async fn accepted_text_only_session_append_updates_query_row_without_changing_tool_use_or_request_snapshot(
) {
    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("accepted-tool-row.js");
    std::fs::write(
        &module,
        r#"export function register(on) {
  const rows = [];
  on('session.append', ($, e, next) => {
    if (e.message.type === 'assistant' && e.message.content.some(block => block.type === 'tool_use')) {
      rows.push({ uuid:e.uuid, type:e.message.type, content:e.message.content });
      return next({ ...e, message:{ ...e.message,
        content:[{type:'text', text:'Mod accepted text only'}] } });
    }
    return next(e);
  });
  on('prompt.submit', ($, e, next) => next({ ...e, text:JSON.stringify(rows) }));
}"#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load(
        "accepted-tool-row",
        dir.path(),
        &module,
        serde_json::json!({}),
    )
    .await
    .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host.clone());
    let executor = Arc::new(hooks::HookExecutorImpl::new(
        Arc::new(tokio::sync::RwLock::new(registry)),
        Arc::new(test_harness::mocks::MockHttpTransport::new()),
        Arc::new(test_harness::mocks::MockRuntimeSpawner::default()),
    ));

    let tool_id = "toolu_accepted_row_original";
    let tool_input = serde_json::json!({"command":"echo original input"});
    let api = StreamingMockApiClient::new(vec![
        streamed_tool_use_with_input_turn(tool_id, "Bash", &tool_input, "tool_use"),
        streamed_text_turn("next request answer", "end_turn"),
    ]);
    let invoker = Arc::new(InputCapturingInvoker::default());
    let pre_query = ConversationMessage::user(MessageId::new(), "pre-query snapshot".into());
    let mut ctx = loop_ctx(api.clone(), Some(invoker.clone()), 3);
    ctx.prompt_messages = vec![pre_query.clone()];
    ctx.hook_executor = Some(executor);
    ctx.hook_cwd = dir.path().to_path_buf();
    ctx.transcript_subdir = dir.path().to_path_buf();
    ctx.transcript_fs = Some(Arc::new(platform_posix::PosixFileSystem::new(
        dir.path().to_path_buf(),
    )));
    let agent_file_id = ctx.agent_id.to_string();

    let (_event_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(32);
    run_subagent(ctx, event_rx, out_tx).await;
    let events = drain(out_rx).await;
    assert_eq!(one_completed(&events)["text"], "next request answer");
    let invoked = invoker.calls.lock().unwrap().clone();
    assert_eq!(invoked.len(), 1, "the source ToolUse is dispatched once");
    assert_eq!(invoked[0].0, "Bash");
    assert_eq!(invoked[0].1, tool_input);
    assert!(invoked[0]
        .2
        .iter()
        .any(|message| message.id() == pre_query.id()));
    assert!(
        !invoked[0]
            .2
            .iter()
            .any(|message| message.text_content().contains("Mod accepted text only")),
        "W1 retains the immutable pre-query history snapshot"
    );
    let w1_assistant_row = invoked[0]
        .3
        .as_ref()
        .expect("W1 receives the accepted current assistant row");
    assert!(w1_assistant_row
        .text_content()
        .contains("Mod accepted text only"));
    assert_eq!(
        match w1_assistant_row {
            ConversationMessage::Assistant { content, .. } => content
                .iter()
                .filter(|block| matches!(block, ContentBlock::ToolUse { .. }))
                .count(),
            _ => 0,
        },
        1,
        "W1 sees accepted Text and the original ToolUse in the same row"
    );
    assert!(
        invoked[0].4.is_empty(),
        "a preceding text-only row does not leak into Native sameTurnToolUses"
    );

    let accepted_row = events
        .iter()
        .filter_map(|event| match event {
            SubagentEvent::Message { message, .. } => {
                serde_json::from_value::<ConversationMessage>(message.clone()).ok()
            }
            _ => None,
        })
        .find(|message| match message {
            ConversationMessage::Assistant { content, .. } => content.iter().any(|block| {
                matches!(
                    block,
                    ContentBlock::Text { text, .. } if text == "Mod accepted text only"
                )
            }),
            _ => false,
        })
        .expect("the accepted query row is emitted through the Agent event stream");
    let accepted_id = accepted_row.id();
    let accepted_content = match &accepted_row {
        ConversationMessage::Assistant { content, .. } => content,
        _ => unreachable!("accepted row was selected as an assistant message"),
    };
    assert_eq!(accepted_content.len(), 2);
    assert!(matches!(
        &accepted_content[0],
        ContentBlock::Text { text, .. } if text == "Mod accepted text only"
    ));
    assert!(matches!(
        &accepted_content[1],
        ContentBlock::ToolUse { id, name, input, .. }
            if id.as_str() == tool_id && name == "Bash" && input == &tool_input
    ));
    assert_eq!(
        accepted_content
            .iter()
            .filter(|block| matches!(block, ContentBlock::ToolUse { .. }))
            .count(),
        1,
        "the merge restores one source ToolUse rather than duplicating or dropping it"
    );

    let physical_calls = api.physical_calls();
    assert_eq!(physical_calls.len(), 2);
    let first_request = &physical_calls[0].request.messages;
    assert!(first_request
        .iter()
        .any(|message| message.id() == pre_query.id()));
    assert!(
        !first_request
            .iter()
            .any(|message| { message.text_content().contains("Mod accepted text only") }),
        "the already-dispatched request snapshot is not retroactively rewritten"
    );
    let next_request_row = physical_calls[1]
        .request
        .messages
        .iter()
        .find(|message| message.id() == accepted_id)
        .expect("the accepted row enters K for the next provider request");
    assert_eq!(
        match next_request_row {
            ConversationMessage::Assistant { content, .. } => content,
            _ => unreachable!("the next request accepted row is assistant"),
        },
        accepted_content
    );
    assert!(matches!(
        next_request_row,
        ConversationMessage::Assistant { stop_reason: Some(stop_reason), .. }
            if stop_reason == "tool_use"
    ));

    let hook_rows: Vec<serde_json::Value> = host
        .dispatch(
            "prompt.submit",
            serde_json::json!({"text":"inspect accepted row"}),
            |event| async move { Ok(event) },
        )
        .await
        .unwrap()["text"]
        .as_str()
        .and_then(|text| serde_json::from_str(text).ok())
        .expect("the hook captured its accepted session.append row");
    assert_eq!(hook_rows.len(), 1);
    assert_eq!(hook_rows[0]["uuid"], accepted_id.as_uuid().to_string());
    assert_eq!(hook_rows[0]["content"][0]["type"], "tool_use");
    assert_eq!(hook_rows[0]["content"][0]["id"], tool_id);
    assert_eq!(hook_rows[0]["content"][0]["input"], tool_input);
    assert_eq!(hook_rows[0]["content"].as_array().unwrap().len(), 1);

    let transcript =
        std::fs::read_to_string(dir.path().join(format!("agent-{agent_file_id}.jsonl"))).unwrap();
    let persisted_rows = transcript
        .lines()
        .filter_map(|line| serde_json::from_str::<crate::transcript::TranscriptEntry>(line).ok())
        .filter(|entry| entry.message.id() == accepted_id)
        .collect::<Vec<_>>();
    assert_eq!(
        persisted_rows.len(),
        1,
        "the accepted outer UUID is appended once"
    );
    assert_eq!(persisted_rows[0].message.id(), accepted_id);
    assert_eq!(
        match &persisted_rows[0].message {
            ConversationMessage::Assistant { content, .. } => content,
            _ => unreachable!("the persisted row is assistant"),
        },
        accepted_content,
        "JSONL persists the same accepted block sequence"
    );
    assert!(matches!(
        &persisted_rows[0].message,
        ConversationMessage::Assistant { stop_reason: Some(stop_reason), .. }
            if stop_reason == "tool_use"
    ));
}

#[tokio::test]
async fn complete_only_assistant_row_keeps_one_native_row_for_mod_and_sibling_tool_context() {
    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("complete-row.js");
    std::fs::write(
        &module,
        r#"export function register(on) {
  const rows = [];
  on('session.append', ($, e, next) => {
    if (e.message.type === 'assistant' &&
        e.message.content.some(block => block.type === 'tool_use')) {
      rows.push({ uuid:e.uuid, content:e.message.content,
        stop_reason_present:Object.hasOwn(e.message, 'stop_reason') });
      const text = e.message.content
        .filter(block => block.type === 'text')
        .map(block => ({ ...block, text:'accepted ' + block.text }));
      return next({ ...e, message:{ ...e.message, content:text } });
    }
    return next(e);
  });
  on('prompt.submit', ($, e, next) => next({ ...e, text:JSON.stringify(rows) }));
}"#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load("complete-row", dir.path(), &module, serde_json::json!({}))
        .await
        .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host.clone());
    let executor = Arc::new(hooks::HookExecutorImpl::new(
        Arc::new(tokio::sync::RwLock::new(registry)),
        Arc::new(test_harness::mocks::MockHttpTransport::new()),
        Arc::new(test_harness::mocks::MockRuntimeSpawner::default()),
    ));

    let tool_a_id = "toolu_complete_a";
    let tool_b_id = "toolu_complete_b";
    let tool_a_input = serde_json::json!({"command":"first source input"});
    let tool_b_input = serde_json::json!({"command":"second source input"});
    let response = llm_runtime::HistoryResponse {
        id: "complete-row-response".into(),
        model: "mock".into(),
        content: vec![
            llm_runtime::ContentBlock::Text {
                text: "TextA".into(),
                cache_control: None,
                citations: None,
            },
            llm_runtime::ContentBlock::ToolCall {
                input_projection: None,
                id: tool_a_id.into(),
                name: "ToolA".into(),
                input: tool_a_input.clone(),
            },
            llm_runtime::ContentBlock::Text {
                text: "TextB".into(),
                cache_control: None,
                citations: None,
            },
            llm_runtime::ContentBlock::ToolCall {
                input_projection: None,
                id: tool_b_id.into(),
                name: "ToolB".into(),
                input: tool_b_input.clone(),
            },
        ],
        stop_reason: Some("tool_use".into()),
        stop_details: None,
        usage: llm_runtime::ExecutionUsage::default(),
        cost: None,
        provider_metadata: serde_json::Value::Null,
    };
    let api = StreamingMockApiClient::new(vec![
        vec![llm_runtime::HistoryEvent::Completed {
            response: Box::new(response),
        }],
        streamed_text_turn("next request answer", "end_turn"),
    ]);
    let watchdog_api: Arc<dyn crate::api::SubagentApiClient> =
        Arc::new(crate::api::WorkflowWatchdogApiClient::new(
            api.clone(),
            lingxi_core::host::WorkflowQueryWatchdog {
                stall_timeout_ms: 60_000,
                max_retries: 1,
                retry_response_body: true,
            },
            Vec::new(),
        ));
    let invoker = Arc::new(InputCapturingInvoker::default());
    let pre_query = ConversationMessage::user(MessageId::new(), "pre-query snapshot".into());
    let mut ctx = loop_ctx(watchdog_api, Some(invoker.clone()), 3);
    ctx.prompt_messages = vec![pre_query.clone()];
    ctx.hook_executor = Some(executor);
    ctx.hook_cwd = dir.path().to_path_buf();
    ctx.transcript_subdir = dir.path().to_path_buf();
    ctx.transcript_fs = Some(Arc::new(platform_posix::PosixFileSystem::new(
        dir.path().to_path_buf(),
    )));
    let agent_file_id = ctx.agent_id.to_string();

    let (_event_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(8);
    let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(32);
    run_subagent(ctx, event_rx, out_tx).await;
    let events = drain(out_rx).await;
    assert_eq!(one_completed(&events)["text"], "next request answer");

    let assistant_rows = events
        .iter()
        .filter_map(|event| match event {
            SubagentEvent::Message { message, .. } => {
                serde_json::from_value::<ConversationMessage>(message.clone()).ok()
            }
            _ => None,
        })
        .filter(|message| match message {
            ConversationMessage::Assistant { content, .. } => {
                content.iter().any(|block| {
                    matches!(block,
                    ContentBlock::ToolUse { id, .. } if id.as_str() == tool_a_id)
                }) && content.iter().any(|block| {
                    matches!(block,
                    ContentBlock::ToolUse { id, .. } if id.as_str() == tool_b_id)
                })
            }
            _ => false,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        assistant_rows.len(),
        1,
        "the complete response is one assistant row"
    );
    let accepted_row = assistant_rows[0].clone();
    let accepted_id = accepted_row.id();
    assert!(
        matches!(
            &accepted_row,
            ConversationMessage::Assistant {
                stop_reason: Some(stop_reason),
                ..
            } if stop_reason == "tool_use"
        ),
        "the complete response stop reason is available at W1 and event time"
    );
    let accepted_content = match &accepted_row {
        ConversationMessage::Assistant { content, .. } => content,
        _ => unreachable!("the completed provider row is assistant"),
    };
    assert!(
        matches!(
            accepted_content.as_slice(),
            [
                ContentBlock::Text { text: accepted_a, .. },
                ContentBlock::Text { text: accepted_b, .. },
                ContentBlock::ToolUse {
                    id: id_a,
                    name: name_a,
                    input: input_a,
                    ..
                },
                ContentBlock::ToolUse {
                    id: id_b,
                    name: name_b,
                    input: input_b,
                    ..
                },
            ] if accepted_a.as_str() == "accepted TextA"
                && accepted_b.as_str() == "accepted TextB"
                && id_a.as_str() == tool_a_id
                && name_a == "ToolA"
                && input_a == &tool_a_input
                && id_b.as_str() == tool_b_id
                && name_b == "ToolB"
                && input_b == &tool_b_input
        ),
        "Native projection places unanchored accepted text before source ToolUses: {accepted_content:?}"
    );

    let calls = invoker.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 2);
    assert_eq!((calls[0].0.as_str(), &calls[0].1), ("ToolA", &tool_a_input));
    assert_eq!((calls[1].0.as_str(), &calls[1].1), ("ToolB", &tool_b_input));
    for call in &calls {
        assert!(
            call.2.iter().any(|message| message.id() == pre_query.id()),
            "each W1 snapshot keeps the immutable pre-query history"
        );
        assert!(
            !call.2.iter().any(|message| message.id() == accepted_id),
            "the current assistant row is passed separately from history"
        );
        assert_eq!(
            call.3.as_ref(),
            Some(&accepted_row),
            "each W1 call receives the full accepted assistant source row"
        );
        assert!(matches!(
            call.3.as_ref(),
            Some(ConversationMessage::Assistant {
                stop_reason: Some(stop_reason),
                ..
            }) if stop_reason == "tool_use"
        ));
    }
    assert!(
        calls[0].4.is_empty(),
        "the first ToolUse has no prior sibling"
    );
    assert!(
        matches!(
            calls[1].4.as_slice(),
            [ContentBlock::ToolUse { id, name, input, .. }]
                if id.as_str() == tool_a_id && name == "ToolA" && input == &tool_a_input
        ),
        "the second ToolUse receives only its earlier same-row sibling"
    );

    let physical_calls = api.physical_calls();
    assert_eq!(physical_calls.len(), 2);
    let next_assistant_rows = physical_calls[1]
        .request
        .messages
        .iter()
        .filter(|message| message.id() == accepted_id)
        .collect::<Vec<_>>();
    assert_eq!(
        next_assistant_rows.len(),
        1,
        "the full accepted response enters the next request once"
    );
    assert_eq!(next_assistant_rows[0], &accepted_row);

    let hook_rows: Vec<serde_json::Value> = host
        .dispatch(
            "prompt.submit",
            serde_json::json!({"text":"inspect complete row"}),
            |event| async move { Ok(event) },
        )
        .await
        .unwrap()["text"]
        .as_str()
        .and_then(|text| serde_json::from_str(text).ok())
        .expect("the real Mod captured the accepted session.append input");
    assert_eq!(
        hook_rows.len(),
        1,
        "Native complete-only content appends once"
    );
    assert_eq!(hook_rows[0]["uuid"], accepted_id.as_uuid().to_string());
    assert_eq!(hook_rows[0]["stop_reason_present"], false);
    assert_eq!(
        hook_rows[0]["content"]
            .as_array()
            .unwrap()
            .iter()
            .map(|block| {
                if block["type"] == "text" {
                    block["text"].as_str().unwrap().to_owned()
                } else {
                    block["id"].as_str().unwrap().to_owned()
                }
            })
            .collect::<Vec<_>>(),
        vec![
            "TextA".to_owned(),
            tool_a_id.to_owned(),
            "TextB".to_owned(),
            tool_b_id.to_owned(),
        ]
    );

    let transcript =
        std::fs::read_to_string(dir.path().join(format!("agent-{agent_file_id}.jsonl"))).unwrap();
    let persisted_rows = transcript
        .lines()
        .filter_map(|line| serde_json::from_str::<crate::transcript::TranscriptEntry>(line).ok())
        .filter(|entry| entry.message.id() == accepted_id)
        .collect::<Vec<_>>();
    assert_eq!(
        persisted_rows.len(),
        1,
        "the one accepted outer UUID is stored once"
    );
    assert_eq!(persisted_rows[0].message, accepted_row);
    assert!(matches!(
        &persisted_rows[0].message,
        ConversationMessage::Assistant {
            stop_reason: Some(stop_reason),
            ..
        } if stop_reason == "tool_use"
    ));
}

#[tokio::test]
async fn cold_resumed_child_screens_source_attachment_without_sending_restore_marker() {
    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("resume-attachment.js");
    std::fs::write(
        &module,
        r#"export function register(on) {
  on('prompt.attachment', { type: 'hook_additional_context' }, ($, e) => {
    if (e.origin.kind !== 'plugin' || e.origin.event !== 'tool.call' || !e.agentId)
      throw new Error('cold child lost source identity');
    return { text: `resumed:${e.text}` };
  });
}"#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load(
        "resume-attachment",
        dir.path(),
        &module,
        serde_json::json!({}),
    )
    .await
    .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host);
    let executor = Arc::new(hooks::HookExecutorImpl::new(
        Arc::new(tokio::sync::RwLock::new(registry)),
        Arc::new(test_harness::mocks::MockHttpTransport::new()),
        Arc::new(test_harness::mocks::MockRuntimeSpawner::default()),
    ));
    let original = ConversationMessage::user_meta(
        MessageId::new(),
        "<system-reminder>\ntool.call hook additional context: original\n</system-reminder>".into(),
    );
    let marker = ConversationMessage::System { api_system: None,
        id: MessageId::new(),
        content: serde_json::json!({
            "messageId":original.id(),
            "attachment":{
                "type":"hook_additional_context",
                "content":["original"],
                "hookName":"tool.call",
                "toolUseID":"toolu_before_restart-context",
                "hookEvent":"PostToolUse",
            },
        })
        .to_string(),
        subtype: Some("mod_attachment_source".into()),
        compact_metadata: None,
        model_fallback: None,
        refusal_fallback: None,
    };
    let api = MockSubagentApiClient::new(vec![Ok(text_response("done", Some("end_turn")))]);
    let mut ctx = loop_ctx(api.clone(), None, 1);
    ctx.resumed_history = Some(vec![marker, original.clone()]);
    ctx.hook_executor = Some(executor);
    ctx.hook_cwd = dir.path().to_path_buf();
    let (_tx, rx) = mpsc::channel(8);
    let (out, events) = mpsc::channel(16);
    run_subagent(ctx, rx, out).await;
    assert_eq!(one_completed(&drain(events).await)["text"], "done");
    let model_messages = api.last_messages();
    assert_eq!(
        model_messages.len(),
        2,
        "cold announced context adds its date row"
    );
    let restored_attachment = model_messages
        .iter()
        .find(|message| message.id() == original.id())
        .expect("the restored attachment row remains in the request");
    assert_eq!(
        restored_attachment.text_content(),
        "<system-reminder>\nresumed:tool.call hook additional context: original\n</system-reminder>"
    );
    assert!(matches!(
        &model_messages[1],
        ConversationMessage::User { content, is_meta: true, .. }
            if content.iter().any(|block| matches!(block,
                ContentBlock::Text { text, .. }
                    if text.starts_with("<system-reminder>\nToday's date is ")
                        && text.ends_with(".\n</system-reminder>")))
    ));
    assert!(!model_messages.iter().any(|message| matches!(
        message,
        ConversationMessage::System { subtype: Some(subtype), .. }
            if subtype == "mod_attachment_source"
    )));
}

#[tokio::test]
async fn foreground_park_publishes_idle_and_preserves_notification_wake() {
    for max_turn_exit in [false, true] {
        let first = if max_turn_exit {
            tool_use_response("Read", Some("tool_use"))
        } else {
            text_response("first", Some("end_turn"))
        };
        let api = MockSubagentApiClient::new(vec![
            Ok(first),
            Ok(text_response("second", Some("end_turn"))),
        ]);
        let mut ctx = loop_ctx(api.clone(), Some(CountingInvoker::new()), 1);
        let registry = Arc::new(OwnerNotificationRegistry {
            park_foreground: true,
            rest_acknowledged: AtomicBool::new(true),
            wake_checked: tokio::sync::Notify::new(),
            drains: AtomicUsize::new(0),
            parked_fold: tokio::sync::Notify::new(),
            owner: ctx.agent_id,
            pending: Mutex::new(vec![]),
            agent_fact_updates: Mutex::new(vec![]),
            records: Mutex::new(vec![]),
            revision: tokio::sync::watch::channel(0).0,
        });
        ctx.task_registry = Some(registry.clone());
        let (event_tx, event_rx) = mpsc::channel(8);
        let (out_tx, mut out_rx) = mpsc::channel(32);
        let runner = tokio::spawn(run_subagent(ctx, event_rx, out_tx));
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            for turn in 0..2 {
                loop {
                    match out_rx.recv().await.expect("parked runner remains live") {
                        SubagentEvent::Message { message, .. }
                            if message["subtype"] == "agent_idle" =>
                        {
                            break;
                        }
                        SubagentEvent::Completed { .. } => {
                            panic!("rest must not tear down the foreground pump")
                        }
                        _ => {}
                    }
                }
                assert!(!runner.is_finished());
                if turn == 0 {
                    registry.publish();
                    let Some(SubagentEvent::Message { message, .. }) = out_rx.recv().await else {
                        panic!("notification wake must precede the resumed turn")
                    };
                    let wake: ConversationMessage = serde_json::from_value(message).unwrap();
                    assert!(matches!(
                        wake,
                        ConversationMessage::User { is_meta: true, .. }
                    ));
                }
            }
        })
        .await
        .expect("both parked turn-sets publish their idle lifecycle");
        assert_eq!(api.call_count(), 2);
        assert!(
            !serde_json::to_string(&api.last_messages())
                .unwrap()
                .contains("agent_idle"),
            "internal rest observation must not become model history"
        );
        drop(event_tx);
        runner.await.unwrap();
    }
}

#[path = "instruction_request_tests.rs"]
mod instruction_request_tests;

#[path = "runner_turn_end_tests.rs"]
mod runner_turn_end_tests;

#[path = "runner_handback_tests.rs"]
mod runner_handback_tests;

#[derive(Clone, Debug)]
struct NestedToolContextSnapshot {
    name: String,
    model: String,
    profile: Option<String>,
    verbose: bool,
    custom_system_prompt: Option<String>,
    append_system_prompt: Option<String>,
    messages: Vec<ConversationMessage>,
}

struct NestedToolEffectsProbe {
    name: &'static str,
    schema: serde_json::Value,
    snapshots: Arc<Mutex<Vec<NestedToolContextSnapshot>>>,
    modifier_order: Arc<Mutex<Vec<&'static str>>>,
    meta_seen: Arc<AtomicUsize>,
    modifier_calls: Arc<AtomicUsize>,
}

struct RecordingChildHookRoute {
    seen: Arc<Mutex<Vec<(hooks::HookEventType, hooks::HookContext)>>>,
}

#[async_trait]
impl hooks::executor::BuiltinHookHandler for RecordingChildHookRoute {
    fn id(&self) -> &str {
        "record-child-hook-route"
    }

    async fn handle(
        &self,
        event: &hooks::HookEvent,
        ctx: &hooks::HookContext,
    ) -> hooks::HookResult {
        self.seen
            .lock()
            .unwrap()
            .push((event.event_type(), ctx.clone()));
        hooks::HookResult {
            outcome: hooks::HookOutcome::Success,
            stdout: String::new(),
            stderr: String::new(),
            exit_code: None,
            response: None,
        }
    }
}

fn nested_route_modifier(
    model: &'static str,
    profile: Option<&'static str>,
) -> lingxi_core::host::tool_invoker::ToolInvocationContextModifier {
    lingxi_core::host::tool_invoker::ToolInvocationContextModifier::new(
        move |mut context: tool_api::ToolUseContext| {
            context.options.main_loop_model = model.into();
            context.options.model_profile = profile.map(str::to_string);
            context
        },
    )
}

#[test]
fn nested_model_modifiers_resolve_in_order_without_mutating_original_state() {
    use crate::model_resolution::{ModelResolutionContext, ModelResolutionError, ModelRouteFacts};
    let provider = |model: &str, profile: Option<&str>| {
        let (model, profile) = match (model, profile) {
            ("start", Some("a")) => ("start", "a"),
            ("b/shared", _) | ("shared", Some("b")) => ("shared", "b"),
            ("balanced", Some("b")) | ("balanced-b", Some("b")) => ("balanced-b", "b"),
            _ => {
                return Err(ModelResolutionError::RouteUnavailable {
                    model: model.into(),
                    profile: profile.map(str::to_string),
                    reason: "not configured".into(),
                });
            }
        };
        Ok(ModelResolutionContext {
            route: ModelRouteFacts {
                model: model.into(),
                profile: Some(profile.into()),
                ..Default::default()
            },
            ..Default::default()
        })
    };
    let state = Some(
        lingxi_core::host::tool_invoker::ToolInvocationContextState::new(Arc::new(
            tool_api::ToolUseContext::model_seed("start".into(), Some("a".into())),
        )),
    );
    let prepared = apply_nested_tool_context_modifiers(
        &state,
        vec![],
        vec![
            nested_route_modifier("b/shared", None),
            nested_route_modifier("balanced", None),
        ],
        "start",
        Some("a"),
        Some(&provider),
    )
    .unwrap();
    assert_eq!(prepared.model, "balanced-b");
    assert_eq!(prepared.model_profile.as_deref(), Some("b"));
    let original = state
        .as_ref()
        .unwrap()
        .downcast_arc::<tool_api::ToolUseContext>()
        .unwrap();
    assert_eq!(original.options.main_loop_model, "start");
    assert_eq!(original.options.model_profile.as_deref(), Some("a"));

    let rejected = apply_nested_tool_context_modifiers(
        &state,
        vec![],
        vec![
            nested_route_modifier("b/shared", None),
            nested_route_modifier("missing", None),
        ],
        "start",
        Some("a"),
        Some(&provider),
    );
    assert!(rejected.is_err());
    let original = state
        .as_ref()
        .unwrap()
        .downcast_arc::<tool_api::ToolUseContext>()
        .unwrap();
    assert_eq!(original.options.main_loop_model, "start");
    assert_eq!(original.options.model_profile.as_deref(), Some("a"));
    assert!(apply_nested_tool_context_modifiers(
        &state,
        vec![],
        vec![nested_route_modifier("balanced", None)],
        "start",
        Some("a"),
        None,
    )
    .is_err());
}

#[async_trait]
impl tool_api::Tool for NestedToolEffectsProbe {
    fn name(&self) -> &str {
        self.name
    }

    fn input_schema(&self) -> &serde_json::Value {
        &self.schema
    }

    fn is_enabled(&self, _: &tool_api::ToolStaticContext) -> bool {
        true
    }

    fn max_result_size_chars(&self) -> usize {
        1024
    }

    fn is_concurrency_safe(&self, _: &serde_json::Value) -> bool {
        true
    }

    fn is_read_only(&self, _: &serde_json::Value) -> bool {
        true
    }

    fn result_ends_turn(&self, result: &tool_api::ToolCallResult) -> bool {
        if self.name == "Skill" {
            assert_eq!(result.data, serde_json::json!({"structured":"first"}));
            assert_eq!(result.model_content.as_deref(), Some("first model text"));
            assert!(!result.is_error);
            assert_eq!(
                result.mcp_meta,
                Some(serde_json::json!({"opaque":"nested"}))
            );
            self.meta_seen.fetch_add(1, Ordering::SeqCst);
        }
        false
    }

    async fn check_permissions(
        &self,
        _: &serde_json::Value,
        _: &tool_api::ToolUseContext,
    ) -> permission::PermissionResult {
        permission::PermissionResult::Allow {
            reason: permission::PermissionDecisionReason::Other {
                reason: "nested effects test".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: permission::result::PermissionMetadata::default(),
        }
    }

    async fn description(&self, _: &serde_json::Value, _: &tool_api::DescriptionOptions) -> String {
        String::new()
    }

    async fn prompt(&self, _: &tool_api::PromptOptions) -> String {
        String::new()
    }

    async fn call(
        &self,
        _: serde_json::Value,
        context: tool_api::ToolUseContext,
        _: tool_api::ToolProgressSender,
    ) -> Result<tool_api::ToolCallResult, tool_api::ToolError> {
        self.snapshots
            .lock()
            .unwrap()
            .push(NestedToolContextSnapshot {
                name: self.name.to_string(),
                model: context.options.main_loop_model.clone(),
                profile: context.options.model_profile.clone(),
                verbose: context.options.verbose,
                custom_system_prompt: context.options.custom_system_prompt.clone(),
                append_system_prompt: context.options.append_system_prompt.clone(),
                messages: context.messages.clone(),
            });

        match self.name {
            // Skill is concurrency-safe in the real registry and its inline
            // model override is a post-batch context modifier. Keep this
            // fixture on the live runner path to distinguish its final fold
            // from executor-only context layers used to start queued calls.
            "Skill" => {
                let order = self.modifier_order.clone();
                let calls = self.modifier_calls.clone();
                Ok(tool_api::ToolCallResult {
                    mcp_meta_projection: None,
                    model_content_projection: None,
                    data_projection: None,
                    data: serde_json::json!({"structured":"first"}),
                    model_content: Some("first model text".into()),
                    new_messages: vec![ConversationMessage::user(
                        MessageId::new(),
                        "nested injected first".into(),
                    )],
                    context_modifier: Some(Box::new(move |mut context| {
                        calls.fetch_add(1, Ordering::SeqCst);
                        order.lock().unwrap().push("first");
                        context.options.custom_system_prompt = Some("modifier one".into());
                        context.options.verbose = true;
                        context
                    })),
                    mcp_meta: Some(serde_json::json!({"opaque":"nested"})),
                    is_error: false,
                })
            }
            "NestedMutateSecond" => {
                let order = self.modifier_order.clone();
                let calls = self.modifier_calls.clone();
                Ok(tool_api::ToolCallResult {
                    mcp_meta_projection: None,
                    model_content_projection: None,
                    data_projection: None,
                    data: serde_json::json!({"structured":"second"}),
                    model_content: Some("second model text".into()),
                    new_messages: vec![ConversationMessage::user(
                        MessageId::new(),
                        "nested injected second".into(),
                    )],
                    context_modifier: Some(Box::new(move |mut context| {
                        calls.fetch_add(1, Ordering::SeqCst);
                        assert_eq!(
                            context.options.custom_system_prompt.as_deref(),
                            Some("modifier one"),
                            "modifiers fold in the assistant tool-use order"
                        );
                        order.lock().unwrap().push("second");
                        context.options.main_loop_model = "nested-model-final".into();
                        context.options.model_profile = Some("nested-profile-final".into());
                        context.options.append_system_prompt = Some("modifier two".into());
                        context
                    })),
                    mcp_meta: None,
                    is_error: false,
                })
            }
            "NestedReselectInitial" => Ok(tool_api::ToolCallResult {
                mcp_meta_projection: None,
                model_content_projection: None,
                data_projection: None,
                data: serde_json::json!({"selected":"initial"}),
                model_content: None,
                new_messages: Vec::new(),
                context_modifier: Some(Box::new(|mut context| {
                    context.options.main_loop_model = "initial".into();
                    context.options.model_profile = None;
                    context
                })),
                mcp_meta: None,
                is_error: false,
            }),
            _ => Ok(tool_api::ToolCallResult::from_data(
                serde_json::json!({"observed":true}),
            )),
        }
    }
}

#[tokio::test]
async fn concurrency_safe_skill_override_updates_the_next_query_after_same_turn_tools() {
    let snapshots = Arc::new(Mutex::new(Vec::new()));
    let modifier_order = Arc::new(Mutex::new(Vec::new()));
    let meta_seen = Arc::new(AtomicUsize::new(0));
    let modifier_calls = Arc::new(AtomicUsize::new(0));
    let mut registry = tool_api::ToolRegistry::new();
    for name in ["Skill", "NestedMutateSecond", "NestedObserve"] {
        registry.register_builtin(Arc::new(NestedToolEffectsProbe {
            name,
            schema: serde_json::json!({"type":"object","additionalProperties":true}),
            snapshots: snapshots.clone(),
            modifier_order: modifier_order.clone(),
            meta_seen: meta_seen.clone(),
            modifier_calls: modifier_calls.clone(),
        }));
    }
    let invoker: Arc<dyn lingxi_core::host::ToolInvoker> =
        Arc::new(tool_api::RegistryToolInvoker::new(Arc::new(registry)));

    let mut first = tool_use_response("Skill", Some("tool_use"));
    first.content.push(llm_runtime::ContentBlock::ToolCall {
        input_projection: None,
        id: ToolUseId::new().to_string(),
        name: "NestedMutateSecond".into(),
        input: serde_json::json!({}),
    });
    let api = MockSubagentApiClient::new(vec![
        Ok(first),
        Ok(tool_use_response("NestedObserve", Some("tool_use"))),
        Ok(text_response("done", Some("end_turn"))),
    ]);
    let mut ctx = loop_ctx(api.clone(), Some(invoker), 4);
    let hook_contexts = Arc::new(Mutex::new(Vec::new()));
    let mut start_hook = frontmatter_stop_hook("record-child-hook-route");
    start_hook.events = vec![hooks::HookEventType::SubagentStart];
    let mut hook_registry = hooks::HookRegistry::new();
    hook_registry.register(start_hook);
    let mut hook_executor = hooks::HookExecutorImpl::new(
        Arc::new(tokio::sync::RwLock::new(hook_registry)),
        Arc::new(test_harness::mocks::MockHttpTransport::new()),
        Arc::new(test_harness::mocks::MockRuntimeSpawner::default()),
    );
    hook_executor.register_builtin(Arc::new(RecordingChildHookRoute {
        seen: hook_contexts.clone(),
    }));
    let hook_executor = Arc::new(hook_executor);
    ctx.hook_executor = Some(hook_executor.clone());
    ctx.agent_definition.frontmatter_hooks = vec![frontmatter_stop_hook("record-child-hook-route")];
    ctx.budget = Some(Arc::new(MockBudget { exceeded: false }));
    ctx.depth = 3;
    ctx.permission_mode_override = Some("plan".into());
    let expected_inherit = subagent_hook_inheritance(&ctx).unwrap();
    ctx.agent_definition.model = AgentModel::Explicit("nested-model-initial".into());
    ctx.model_profile = Some("nested-profile-initial".into());
    ctx.model_resolution_context_provider = Some(Arc::new(|model: &str, profile: Option<&str>| {
        Ok(crate::model_resolution::ModelResolutionContext {
            route: crate::model_resolution::ModelRouteFacts {
                model: model.to_string(),
                profile: profile.map(str::to_string),
                ..Default::default()
            },
            ..Default::default()
        })
    }));
    let transcript_dir = tempfile::tempdir().unwrap();
    ctx.transcript_subdir = transcript_dir.path().to_path_buf();
    ctx.transcript_fs = Some(Arc::new(platform_posix::PosixFileSystem::new(
        transcript_dir.path().to_path_buf(),
    )));
    let transcript_path = transcript_dir
        .path()
        .join(format!("agent-{}.jsonl", ctx.agent_id));
    let hook_session_id = ctx.hook_session_id;
    let agent_id = ctx.agent_id;
    let (event_tx, event_rx) = mpsc::channel(1);
    drop(event_tx);
    let (out_tx, out_rx) = mpsc::channel(64);
    run_subagent(ctx, event_rx, out_tx).await;
    let events = drain(out_rx).await;

    assert!(events
        .iter()
        .any(|event| matches!(event, SubagentEvent::Completed { .. })));
    assert_eq!(api.call_count(), 3);
    let (cached_route, cached_transcript) = hook_executor
        .take_agent_prompt_transcript(hook_session_id, agent_id)
        .expect("the parent stop evaluator receives the completed child snapshot");
    assert_eq!(cached_route.model, "nested-model-final");
    assert_eq!(
        cached_route.model_profile.as_deref(),
        Some("nested-profile-final"),
        "the cached route follows the accepted Skill batch, not the spawn route"
    );
    assert!(cached_transcript.messages.iter().any(|message| {
        matches!(message, ConversationMessage::Assistant { content, .. }
            if content.iter().any(|block| matches!(block, ContentBlock::Text { text, .. } if text == "done")))
    }));
    let hook_contexts = hook_contexts.lock().unwrap();
    assert_eq!(hook_contexts.len(), 2);
    assert_eq!(hook_contexts[0].0, hooks::HookEventType::SubagentStart);
    assert_eq!(hook_contexts[1].0, hooks::HookEventType::SubagentStop);
    assert_eq!(
        hook_contexts[0].1.model_selection.as_ref().unwrap().model,
        "nested-model-initial"
    );
    assert_eq!(
        hook_contexts[0]
            .1
            .model_selection
            .as_ref()
            .unwrap()
            .model_profile
            .as_deref(),
        Some("nested-profile-initial")
    );
    assert_eq!(
        hook_contexts[1].1.model_selection.as_ref().unwrap().model,
        "nested-model-final"
    );
    assert_eq!(
        hook_contexts[1]
            .1
            .model_selection
            .as_ref()
            .unwrap()
            .model_profile
            .as_deref(),
        Some("nested-profile-final")
    );
    for (_, context) in hook_contexts.iter() {
        assert_eq!(context.agent_depth, Some(3));
        assert_eq!(context.permission_mode.as_deref(), Some("plan"));
        let inherit = context.inherit.as_ref().unwrap();
        assert!(Arc::ptr_eq(
            &inherit.tool_invoker,
            &expected_inherit.tool_invoker
        ));
        assert!(Arc::ptr_eq(&inherit.budget, &expected_inherit.budget));
    }
    drop(hook_contexts);
    let transcript = tokio::fs::read_to_string(transcript_path).await.unwrap();
    let rows: Vec<serde_json::Value> = transcript
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let selected = rows
        .iter()
        .position(|row| row["type"] == "model-selection")
        .expect("the committed nested route is durable");
    assert_eq!(rows[selected]["model"], "nested-model-final");
    assert_eq!(rows[selected]["model_profile"], "nested-profile-final");
    for row in &rows[selected + 1..] {
        if row.get("model").is_some() {
            assert_eq!(row["model"], "nested-model-final");
            assert_eq!(row["model_profile"], "nested-profile-final");
        }
    }
    assert_eq!(
        meta_seen.load(Ordering::SeqCst),
        1,
        "opaque mcp_meta reaches the tool-result adapter"
    );
    assert_eq!(
        modifier_calls.load(Ordering::SeqCst),
        2,
        "each selected one-shot modifier is applied once"
    );
    assert_eq!(*modifier_order.lock().unwrap(), vec!["first", "second"]);

    let calls = api.physical_calls();
    let next_request = &calls[1].request;
    assert_eq!(next_request.model, "nested-model-final");
    assert_eq!(
        next_request.profile.as_deref(),
        Some("nested-profile-final")
    );
    let first_result_index = next_request
        .messages
        .iter()
        .position(|message| {
            matches!(message, ConversationMessage::User { content, .. } if content.iter().any(|block| {
                matches!(block, ContentBlock::ToolResult { content, .. } if content == "first model text")
            }))
        })
        .expect("structured results retain their model-facing projection");
    let injected_first_index = next_request
        .messages
        .iter()
        .position(|message| message.text_content() == "nested injected first")
        .expect("the first injected message enters model history");
    let injected_second_index = next_request
        .messages
        .iter()
        .position(|message| message.text_content() == "nested injected second")
        .expect("the second injected message enters model history");
    assert!(first_result_index < injected_first_index);
    assert!(injected_first_index < injected_second_index);

    let snapshots = snapshots.lock().unwrap();
    assert_eq!(snapshots.len(), 3);
    assert_eq!(
        snapshots
            .iter()
            .map(|snapshot| snapshot.name.as_str())
            .collect::<Vec<_>>(),
        vec!["Skill", "NestedMutateSecond", "NestedObserve"]
    );
    let initial_model = snapshots[0].model.clone();
    assert_eq!(
        snapshots[1].model, initial_model,
        "tools in one batch share the pre-batch route"
    );
    assert_eq!(snapshots[1].custom_system_prompt, None);
    assert_eq!(snapshots[0].messages, snapshots[1].messages);
    assert_eq!(snapshots[2].model, "nested-model-final");
    assert_eq!(
        snapshots[2].profile.as_deref(),
        Some("nested-profile-final")
    );
    assert!(snapshots[2].verbose);
    assert_eq!(
        snapshots[2].custom_system_prompt.as_deref(),
        Some("modifier one")
    );
    assert_eq!(
        snapshots[2].append_system_prompt.as_deref(),
        Some("modifier two")
    );
    assert!(snapshots[2]
        .messages
        .iter()
        .any(|message| message.text_content() == "nested injected second"));
}
