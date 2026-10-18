//! Replay the pinned Native 2.1.295 refusal stop-details cases through the
//! production SDK -> provider adapter -> orchestrator -> OutputStream/JSONL
//! path. This test intentionally compares the full Native CLI result and the
//! source-preserving persisted stop_details field; it does not use a mock
//! terminal formatter or construct the result text itself.

use async_trait::async_trait;
use futures::StreamExt;
use hooks::definition::{HookDefinition, HookExecutor as DefinitionHookExecutor, HookSource};
use hooks::events::{HookEvent, HookEventType};
use hooks::executor::{BuiltinHookHandler, HookExecutorImpl};
use hooks::registry::{HookContext, HookRegistry};
use hooks::response::{HookOutcome, HookResult};
use lingxi_core::host::{
    FileSystem, HttpError, HttpTransport, OutputEvent, RuntimeError, RuntimeSpawner,
};
use lingxi_core::types::{
    utf16_json::Utf16JsonProjection, ConversationMessage, HookId, HttpRequest, HttpResponse,
    SessionId, ToolUseId,
};
use llm_runtime::model::{thinking::ThinkingConfig, user_agent::UserAgentEnv};
use llm_runtime::{ApiService, ClientConfig, ModelRuntime, SubscriberState, Transport};
use orchestrator::test_support::{
    noop_hook_executor, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::{
    ConversationOrchestrator, ConversationOutcome, OrchestratorConfig, ProviderApiAdapter,
};
use permission::gate::{PermissionCheckContext, PermissionOutcome};
use permission::result::PermissionMetadata;
use permission::{PermissionDecision, PermissionDecisionReason, PermissionGate, PermissionResult};
use platform_posix::fs::PosixFileSystem;
use serde_json::json;
use session::jsonl::{reader::JsonlReader, writer::JsonlWriter};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::future::Future;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tempfile::tempdir;
use tokio::sync::{oneshot, Notify, RwLock};
use tool_api::progress::ToolProgressSender;
use tool_api::registry::ToolRegistry;
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
    ValidationError,
};

const MODEL: &str = "claude-sonnet-4-5";
const NATIVE_REFUSAL_SAFETY_RETRY_USER_MESSAGE: &str = "Your response above was stopped by a safety classifier — this is not a tool or API error. The rest of it was withheld, and tool calls in it that had not finished did not run. Do not produce that content again, even reworded.";
const NATIVE_REFUSAL_RUNNING_TOOL_EXCEPTION: &str = " Exception: a tool call whose result reads \"Interrupted\" was already running when the response was stopped; it may have partially or fully completed.";

fn run_with_large_stack<F, Fut>(build: F)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = ()>,
{
    let handle = std::thread::Builder::new()
        .name("current-alignment-refusal".into())
        .stack_size(64 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build refusal-test runtime")
                .block_on(build());
        })
        .expect("spawn large-stack refusal test thread");
    handle.join().expect("refusal test thread panicked");
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RequestShape {
    model: Option<String>,
    stream: Option<bool>,
    refusal_safety_retry_message_count: usize,
    tool_result_ids: Vec<String>,
}

struct FixedNativeRefusal {
    response_body: Vec<u8>,
    request_shapes: Mutex<Vec<RequestShape>>,
}

fn request_shape(request: &lingxi_llm_client::HttpRequest) -> RequestShape {
    let parsed: Option<serde_json::Value> = serde_json::from_slice(request.body.as_ref()).ok();
    let refusal_safety_retry_message_count = parsed
        .as_ref()
        .and_then(|body| body.get("messages"))
        .and_then(serde_json::Value::as_array)
        .map_or(0, |messages| {
            messages
                .iter()
                .filter(|message| {
                    message.get("role").and_then(serde_json::Value::as_str) == Some("user")
                })
                .flat_map(|message| {
                    message
                        .get("content")
                        .and_then(serde_json::Value::as_array)
                        .into_iter()
                        .flatten()
                })
                .filter(|block| {
                    block
                        .get("text")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|text| {
                            text == NATIVE_REFUSAL_SAFETY_RETRY_USER_MESSAGE
                                || text
                                    == format!(
                                        "{NATIVE_REFUSAL_SAFETY_RETRY_USER_MESSAGE}{NATIVE_REFUSAL_RUNNING_TOOL_EXCEPTION}"
                                    )
                        })
                })
                .count()
        });
    let tool_result_ids = parsed
        .as_ref()
        .and_then(|body| body.get("messages"))
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .flat_map(|message| {
            message
                .get("content")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
        })
        .filter_map(|block| {
            (block.get("type").and_then(serde_json::Value::as_str) == Some("tool_result"))
                .then(|| {
                    block
                        .get("tool_use_id")
                        .and_then(serde_json::Value::as_str)
                        .map(ToOwned::to_owned)
                })
                .flatten()
        })
        .collect();
    RequestShape {
        model: parsed
            .as_ref()
            .and_then(|body| body.get("model"))
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned),
        stream: parsed
            .as_ref()
            .and_then(|body| body.get("stream"))
            .and_then(serde_json::Value::as_bool),
        refusal_safety_retry_message_count,
        tool_result_ids,
    }
}

#[async_trait]
impl Transport for FixedNativeRefusal {
    async fn send(
        &self,
        request: lingxi_llm_client::HttpRequest,
    ) -> Result<lingxi_llm_client::StreamResponse, lingxi_llm_client::protocol::LlmError> {
        self.request_shapes
            .lock()
            .expect("request-shape mutex")
            .push(request_shape(&request));
        Ok(lingxi_llm_client::HttpResponse {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body: self.response_body.clone().into(),
        }
        .into())
    }
}

fn api_service(
    transport: Arc<dyn Transport>,
    provider_id: llm_runtime::ProviderId,
) -> Arc<ApiService> {
    let mut config: ClientConfig = serde_json::from_value(json!({
        "providers": [{
            "provider_id": "anthropic_first_party",
            "profile_name": "direct",
            "base_url": "https://api.anthropic.com",
            "protocol": "anthropic_messages",
            "auth": "none",
            "credential": {"type": "none"},
            "models": [{
                "display_model": MODEL,
                "request_model": MODEL,
                "billing_model": MODEL,
                "capabilities": {
                    "streaming": true,
                    "tools": true,
                    "vision": false,
                    "documents": false,
                    "reasoning": false,
                    "structured_output": false
                }
            }, {
                "display_model": "claude-opus-4-7",
                "request_model": "claude-opus-4-7",
                "billing_model": "claude-opus-4-7",
                "capabilities": {
                    "streaming": true,
                    "tools": true,
                    "vision": false,
                    "documents": false,
                    "reasoning": false,
                    "structured_output": false
                }
            }]
        }]
    }))
    .expect("fixed provider config");
    config.providers[0].provider_id = provider_id;

    Arc::new(
        ApiService::new_with_routing(
            Arc::new(ModelRuntime::from_config(config).expect("model runtime")),
            transport,
            SubscriberState::default(),
            UserAgentEnv::default(),
            "current-alignment-native-refusal",
            None,
            None,
            None,
            Default::default(),
            Some(0),
            None,
        )
        .with_thinking(ThinkingConfig::Disabled),
    )
}

fn api_service_with_native_and_alias_routes(transport: Arc<dyn Transport>) -> Arc<ApiService> {
    let capabilities = json!({
        "streaming": true,
        "tools": true,
        "vision": false,
        "documents": false,
        "reasoning": false,
        "structured_output": false
    });
    let config: ClientConfig = serde_json::from_value(json!({
        "providers": [{
            "provider_id": "anthropic_first_party",
            "profile_name": "direct",
            "base_url": "https://api.anthropic.com",
            "protocol": "anthropic_messages",
            "auth": "none",
            "credential": {"type": "none"},
            "models": [{
                "display_model": MODEL,
                "request_model": MODEL,
                "billing_model": MODEL,
                "capabilities": capabilities
            }]
        }, {
            "provider_id": {"custom": {"name": "route-alias-provider"}},
            "profile_name": "route-alias",
            "base_url": "https://route-alias.example",
            "protocol": "anthropic_messages",
            "auth": "none",
            "credential": {"type": "none"},
            "models": [{
                "display_model": "claude-opus-4-7",
                "request_model": "claude-opus-4-7",
                "billing_model": "claude-opus-4-7",
                "capabilities": capabilities
            }]
        }]
    }))
    .expect("native and custom alias provider routes");

    Arc::new(
        ApiService::new_with_routing(
            Arc::new(ModelRuntime::from_config(config).expect("model runtime")),
            transport,
            SubscriberState::default(),
            UserAgentEnv::default(),
            "current-alignment-native-refusal-route-alias",
            None,
            None,
            None,
            Default::default(),
            Some(0),
            None,
        )
        .with_thinking(ThinkingConfig::Disabled),
    )
}

struct RefusalHarness {
    _dir: tempfile::TempDir,
    orchestrator: Arc<ConversationOrchestrator>,
    transport: Arc<FixedNativeRefusal>,
    output: Arc<MockOutputStream>,
}

fn refusal_harness(query_source: &str, response_body: Vec<u8>) -> RefusalHarness {
    let dir = tempdir().expect("isolated refusal transcript directory");
    let transcript_path = dir.path().join("session.jsonl");
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(dir.path().to_path_buf()));
    let writer = Arc::new(JsonlWriter::new(transcript_path, fs));
    let transport = Arc::new(FixedNativeRefusal {
        response_body,
        request_shapes: Mutex::new(Vec::new()),
    });
    let adapter = Arc::new(ProviderApiAdapter::new(api_service(
        transport.clone(),
        llm_runtime::ProviderId::AnthropicFirstParty,
    )));
    let output = Arc::new(MockOutputStream::new());
    let orchestrator = ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig {
            model: MODEL.to_owned(),
            max_turns: 1,
            query_source: query_source.to_owned(),
            ..OrchestratorConfig::default()
        },
        adapter.clone(),
        adapter,
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        output.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        dir.path().to_path_buf(),
    )
    .with_jsonl_writer(writer);

    RefusalHarness {
        _dir: dir,
        orchestrator: ConversationOrchestrator::into_shared(orchestrator),
        transport,
        output,
    }
}

fn refusal_response_without_stop_details(response_body: &[u8]) -> Vec<u8> {
    let response = std::str::from_utf8(response_body).expect("Native SSE fixture is UTF-8");
    response
        .split("\n\n")
        .filter(|frame| !frame.is_empty())
        .map(|frame| {
            let Some(data) = frame.strip_prefix("event: message_delta\ndata: ") else {
                return format!("{frame}\n\n");
            };
            let mut event = Utf16JsonProjection::parse(data)
                .expect("message_delta fixture is exact UTF-16 JSON");
            let mut delta = event
                .subprojection("/delta")
                .expect("message_delta has delta object");
            delta
                .remove_field("stop_details")
                .expect("message_delta delta is an object");
            event
                .set_field("delta", delta)
                .expect("message_delta has a delta object");
            format!(
                "event: message_delta\ndata: {}\n\n",
                event
                    .to_json_string()
                    .expect("message_delta preserves its JSON source units")
            )
        })
        .collect::<String>()
        .into_bytes()
}

struct NativeReply {
    chunks: Vec<(std::time::Duration, Vec<u8>)>,
}

struct SequencedNativeReplies {
    replies: Mutex<VecDeque<NativeReply>>,
    request_shapes: Mutex<Vec<RequestShape>>,
}

struct BatchedNativeReplies {
    replies: Mutex<VecDeque<serde_json::Value>>,
    request_shapes: Mutex<Vec<RequestShape>>,
}

#[async_trait]
impl Transport for BatchedNativeReplies {
    async fn send(
        &self,
        request: lingxi_llm_client::HttpRequest,
    ) -> Result<lingxi_llm_client::StreamResponse, lingxi_llm_client::protocol::LlmError> {
        self.request_shapes
            .lock()
            .expect("batched request-shape mutex")
            .push(request_shape(&request));
        let reply = self
            .replies
            .lock()
            .expect("batched reply queue mutex")
            .pop_front()
            .expect("batched transport received an unexpected extra request");
        let bytes = serde_json::to_vec(&reply).expect("batched Native response is JSON");
        let body = futures::stream::iter([Ok::<_, lingxi_llm_client::protocol::LlmError>(
            bytes::Bytes::from(bytes),
        )])
        .boxed();
        Ok(lingxi_llm_client::StreamResponse {
            status: 200,
            headers: vec![("content-type".into(), "application/json".into())],
            body,
        })
    }
}

#[async_trait]
impl Transport for SequencedNativeReplies {
    async fn send(
        &self,
        request: lingxi_llm_client::HttpRequest,
    ) -> Result<lingxi_llm_client::StreamResponse, lingxi_llm_client::protocol::LlmError> {
        self.request_shapes
            .lock()
            .expect("request-shape mutex")
            .push(request_shape(&request));
        let reply = self
            .replies
            .lock()
            .expect("reply queue mutex")
            .pop_front()
            .expect("test transport received an unexpected extra request");
        let body = futures::stream::iter(reply.chunks)
            .then(|(delay, chunk)| async move {
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                Ok::<_, lingxi_llm_client::protocol::LlmError>(bytes::Bytes::from(chunk))
            })
            .boxed();
        Ok(lingxi_llm_client::StreamResponse {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body,
        })
    }
}

fn native_event_frame(event: &str, data: serde_json::Value) -> Vec<u8> {
    format!(
        "event: {event}\ndata: {}\n\n",
        serde_json::to_string(&data).expect("Native event JSON serializes")
    )
    .into_bytes()
}

fn native_reply_from_bytes(body: Vec<u8>) -> NativeReply {
    let text = String::from_utf8(body).expect("Native fixture SSE is UTF-8");
    NativeReply {
        chunks: text
            .split_inclusive("\n\n")
            .filter(|frame| !frame.is_empty())
            .map(|frame| (std::time::Duration::ZERO, frame.as_bytes().to_vec()))
            .collect(),
    }
}

fn refusal_with_tool_uses_reply(
    completed_id: &str,
    running_id: &str,
    queued_id: &str,
) -> NativeReply {
    refusal_with_named_tool_uses_reply(completed_id, running_id, queued_id, "RunningRefusalProbe")
}

fn one_tool_reply(tool_id: &str, tool_name: &str, stop_reason: &str) -> NativeReply {
    NativeReply {
        chunks: vec![
            (
                std::time::Duration::ZERO,
                native_event_frame(
                    "message_start",
                    json!({
                        "type": "message_start",
                        "message": {
                            "id": "msg_one_tool_refusal_295",
                            "type": "message",
                            "role": "assistant",
                            "model": MODEL,
                            "content": [],
                            "stop_reason": null,
                            "stop_sequence": null,
                            "usage": {"input_tokens": 1, "output_tokens": 0}
                        }
                    }),
                ),
            ),
            (
                std::time::Duration::ZERO,
                native_event_frame(
                    "content_block_start",
                    json!({
                        "type": "content_block_start",
                        "index": 0,
                        "content_block": {
                            "type": "tool_use",
                            "id": tool_id,
                            "name": tool_name,
                            "input": {}
                        }
                    }),
                ),
            ),
            (
                std::time::Duration::ZERO,
                native_event_frame(
                    "content_block_stop",
                    json!({"type": "content_block_stop", "index": 0}),
                ),
            ),
            (
                std::time::Duration::ZERO,
                native_event_frame(
                    "message_delta",
                    json!({
                        "type": "message_delta",
                        "delta": {"stop_reason": stop_reason, "stop_sequence": null},
                        "usage": {"output_tokens": 1}
                    }),
                ),
            ),
            (
                std::time::Duration::ZERO,
                native_event_frame("message_stop", json!({"type": "message_stop"})),
            ),
        ],
    }
}

fn refusal_with_one_tool_reply(tool_id: &str, tool_name: &str) -> NativeReply {
    one_tool_reply(tool_id, tool_name, "refusal")
}

fn refusal_with_delayed_tool_uses_reply(
    completed_id: &str,
    running_id: &str,
    queued_id: &str,
) -> NativeReply {
    refusal_with_named_tool_uses_reply(completed_id, running_id, queued_id, "DelayedRefusalProbe")
}

fn refusal_with_named_tool_uses_reply(
    completed_id: &str,
    running_id: &str,
    queued_id: &str,
    running_name: &str,
) -> NativeReply {
    let mut chunks = vec![(
        std::time::Duration::ZERO,
        native_event_frame(
            "message_start",
            json!({
                "type": "message_start",
                "message": {
                    "id": "msg_tool_refusal_295",
                    "type": "message",
                    "role": "assistant",
                    "model": MODEL,
                    "content": [],
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": {"input_tokens": 1, "output_tokens": 0}
                }
            }),
        ),
    )];
    let mut delay_before_next = std::time::Duration::ZERO;
    for (index, id, name, delay_after) in [
        (0, completed_id, "CompletedRefusalProbe", 25),
        (1, running_id, running_name, 25),
        (2, queued_id, "QueuedRefusalProbe", 0),
    ] {
        chunks.push((
            delay_before_next,
            native_event_frame(
                "content_block_start",
                json!({
                    "type": "content_block_start",
                    "index": index,
                    "content_block": {
                        "type": "tool_use",
                        "id": id,
                        "name": name,
                        "input": {}
                    }
                }),
            ),
        ));
        delay_before_next = std::time::Duration::ZERO;
        chunks.push((
            std::time::Duration::ZERO,
            native_event_frame(
                "content_block_stop",
                json!({"type": "content_block_stop", "index": index}),
            ),
        ));
        if delay_after > 0 {
            delay_before_next = std::time::Duration::from_millis(delay_after);
        }
    }
    chunks.push((
        std::time::Duration::ZERO,
        native_event_frame(
            "message_delta",
            json!({
                "type": "message_delta",
                "delta": {"stop_reason": "refusal", "stop_sequence": null},
                "usage": {"output_tokens": 1}
            }),
        ),
    ));
    chunks.push((
        std::time::Duration::ZERO,
        native_event_frame("message_stop", json!({"type": "message_stop"})),
    ));
    NativeReply { chunks }
}

fn native_end_turn_reply(response_id: &str) -> NativeReply {
    NativeReply {
        chunks: vec![
            (
                std::time::Duration::ZERO,
                native_event_frame(
                    "message_start",
                    json!({
                        "type": "message_start",
                        "message": {
                            "id": response_id,
                            "type": "message",
                            "role": "assistant",
                            "model": "claude-opus-4-7",
                            "content": [],
                            "stop_reason": null,
                            "stop_sequence": null,
                            "usage": {"input_tokens": 1, "output_tokens": 0}
                        }
                    }),
                ),
            ),
            (
                std::time::Duration::ZERO,
                native_event_frame(
                    "content_block_start",
                    json!({
                        "type": "content_block_start",
                        "index": 0,
                        "content_block": {"type": "text", "text": ""}
                    }),
                ),
            ),
            (
                std::time::Duration::ZERO,
                native_event_frame(
                    "content_block_delta",
                    json!({
                        "type": "content_block_delta",
                        "index": 0,
                        "delta": {"type": "text_delta", "text": "fallback completed"}
                    }),
                ),
            ),
            (
                std::time::Duration::ZERO,
                native_event_frame(
                    "content_block_stop",
                    json!({"type": "content_block_stop", "index": 0}),
                ),
            ),
            (
                std::time::Duration::ZERO,
                native_event_frame(
                    "message_delta",
                    json!({
                        "type": "message_delta",
                        "delta": {"stop_reason": "end_turn", "stop_sequence": null},
                        "usage": {"output_tokens": 2}
                    }),
                ),
            ),
            (
                std::time::Duration::ZERO,
                native_event_frame("message_stop", json!({"type": "message_stop"})),
            ),
        ],
    }
}

fn native_batched_response(
    response_id: &str,
    content: Vec<serde_json::Value>,
    stop_reason: &str,
) -> serde_json::Value {
    json!({
        "id": response_id,
        "type": "message",
        "role": "assistant",
        "model": MODEL,
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
}

#[derive(Default)]
struct RefusalToolCounters {
    completed: AtomicUsize,
    running_started: AtomicUsize,
    queued: AtomicUsize,
    permission_blocked_started: AtomicUsize,
    permission_blocked_validations: AtomicUsize,
}

#[derive(Clone, Copy)]
enum RefusalToolKind {
    Completed,
    Running,
    Delayed,
    Queued,
    PermissionBlocked,
}

struct CountedRefusalTool {
    name: &'static str,
    kind: RefusalToolKind,
    counters: Arc<RefusalToolCounters>,
}

#[async_trait]
impl Tool for CountedRefusalTool {
    fn name(&self) -> &str {
        self.name
    }

    fn input_schema(&self) -> &Utf16JsonProjection {
        static SCHEMA: once_cell::sync::Lazy<serde_json::Value> =
            once_cell::sync::Lazy::new(|| json!({"type": "object"}));
        static SCHEMA_SOURCE: once_cell::sync::Lazy<Utf16JsonProjection> =
            once_cell::sync::Lazy::new(|| tool_api::tool_trait::generated_builtin_schema(&SCHEMA));
        &SCHEMA_SOURCE
    }

    fn input_repair_metadata(&self) -> tool_api::tool_trait::ToolInputRepairMetadata {
        tool_api::tool_trait::ToolInputRepairMetadata::Known {
            schema: tool_api::tool_trait::ToolInputRepairSchema::Advertised,
            schema_provenance: tool_api::tool_trait::ToolInputSchemaProvenance::GeneratedBuiltin,
            exempt_top_level_keys: None,
        }
    }

    fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
        true
    }

    fn max_result_size_chars(&self) -> usize {
        1024
    }

    fn is_concurrency_safe(&self, _input: &Utf16JsonProjection) -> bool {
        !matches!(
            self.kind,
            RefusalToolKind::Running
                | RefusalToolKind::Delayed
                | RefusalToolKind::PermissionBlocked
        )
    }

    fn is_read_only(&self, _input: &Utf16JsonProjection) -> bool {
        true
    }

    async fn validate_input(
        &self,
        _input: &Utf16JsonProjection,
        _ctx: &tool_api::context::ToolUseContext,
    ) -> Result<(), ValidationError> {
        if matches!(self.kind, RefusalToolKind::PermissionBlocked) {
            self.counters
                .permission_blocked_validations
                .fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }

    async fn check_permissions(
        &self,
        _input: &Utf16JsonProjection,
        _ctx: &tool_api::context::ToolUseContext,
    ) -> PermissionResult {
        if matches!(self.kind, RefusalToolKind::PermissionBlocked) {
            return PermissionResult::Ask {
                reason: PermissionDecisionReason::Other {
                    reason: "refusal boundary permission probe".into(),
                },
                prompt: permission::result::PermissionPrompt {
                    title: "Permission required".into(),
                    message: "Waiting for refusal boundary".into(),
                    options: vec!["Allow once".into(), "Deny".into()],
                },
                pending_classifier_check: None,
                metadata: PermissionMetadata::default(),
            };
        }
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "refusal retry test".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    async fn description(
        &self,
        _input: &Utf16JsonProjection,
        _opts: &DescriptionOptions,
    ) -> String {
        self.name.to_owned()
    }

    async fn prompt(&self, _opts: &PromptOptions) -> Utf16JsonProjection {
        String::new().into()
    }

    async fn call(
        &self,
        _input: Utf16JsonProjection,
        ctx: tool_api::context::ToolUseContext,
        _tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        match self.kind {
            RefusalToolKind::Completed => {
                self.counters.completed.fetch_add(1, Ordering::SeqCst);
            }
            RefusalToolKind::Running => {
                self.counters.running_started.fetch_add(1, Ordering::SeqCst);
                let Some(cancel) = ctx.cancel else {
                    return Err(ToolError::Internal(
                        "running refusal probe had no cancellation token".into(),
                    ));
                };
                cancel.cancelled().await;
                return Err(ToolError::Aborted);
            }
            RefusalToolKind::Delayed => {
                self.counters.running_started.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            RefusalToolKind::Queued => {
                self.counters.queued.fetch_add(1, Ordering::SeqCst);
            }
            RefusalToolKind::PermissionBlocked => {
                self.counters
                    .permission_blocked_started
                    .fetch_add(1, Ordering::SeqCst);
            }
        }
        Ok(ToolCallResult {
            data: tool_api::tool_trait::generated_builtin_result(&json!({"ok": true})),
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            subagent_hooks_fired: false,
            mcp_meta: None,
        })
    }
}

fn refusal_probe_tool_registry(counters: Arc<RefusalToolCounters>) -> Arc<ToolRegistry> {
    refusal_probe_tool_registry_with_first_kind(counters, RefusalToolKind::Completed)
}

fn refusal_probe_tool_registry_with_permission_block(
    counters: Arc<RefusalToolCounters>,
) -> Arc<ToolRegistry> {
    refusal_probe_tool_registry_with_first_kind(counters, RefusalToolKind::PermissionBlocked)
}

fn refusal_probe_tool_registry_with_first_kind(
    counters: Arc<RefusalToolCounters>,
    first_kind: RefusalToolKind,
) -> Arc<ToolRegistry> {
    let mut tools = ToolRegistry::new();
    for (name, kind) in [
        ("CompletedRefusalProbe", first_kind),
        ("RunningRefusalProbe", RefusalToolKind::Running),
        ("DelayedRefusalProbe", RefusalToolKind::Delayed),
        ("QueuedRefusalProbe", RefusalToolKind::Queued),
    ] {
        tools.register_builtin(Arc::new(CountedRefusalTool {
            name,
            kind,
            counters: Arc::clone(&counters),
        }));
    }
    Arc::new(tools)
}

#[derive(Default)]
struct RefusalHookObservations {
    events: Mutex<Vec<String>>,
}

struct RefusalHookCounter(Arc<RefusalHookObservations>);

#[async_trait]
impl BuiltinHookHandler for RefusalHookCounter {
    fn id(&self) -> &str {
        "current-alignment-refusal-counter"
    }

    async fn handle(&self, event: &HookEvent, _ctx: &HookContext) -> HookResult {
        let mut observations = self
            .0
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match event {
            HookEvent::PreToolUse { tool_use_id, .. } => {
                observations.push(format!("pre:{tool_use_id}"));
            }
            HookEvent::PostToolUse { tool_use_id, .. } => {
                observations.push(format!("post:{tool_use_id}"));
            }
            HookEvent::PostToolUseFailure { tool_use_id, .. } => {
                observations.push(format!("failure:{tool_use_id}"));
            }
            HookEvent::PostToolBatch { tool_calls } => {
                observations.extend(
                    tool_calls
                        .iter()
                        .map(|call| format!("batch:{}", call.tool_use_id)),
                );
            }
            _ => {}
        }
        HookResult {
            outcome: HookOutcome::Success,
            stdout: String::new().into(),
            stderr: String::new().into(),
            exit_code: None,
            response: None,
        }
    }
}

struct RefusalUnusedHttp;

#[async_trait]
impl HttpTransport for RefusalUnusedHttp {
    async fn request(&self, _req: HttpRequest) -> Result<HttpResponse, HttpError> {
        Err(HttpError::InvalidRequest(
            "unused refusal test hook HTTP".into(),
        ))
    }

    async fn stream_sse(
        &self,
        _req: HttpRequest,
    ) -> Result<lingxi_core::host::http::SseStream, HttpError> {
        Err(HttpError::InvalidRequest(
            "unused refusal test hook SSE".into(),
        ))
    }
}

struct RefusalUnusedRuntime;

#[async_trait]
impl RuntimeSpawner for RefusalUnusedRuntime {
    async fn spawn(
        &self,
        _name: &str,
        _task: std::pin::Pin<Box<dyn Future<Output = ()> + Send + 'static>>,
    ) -> Result<lingxi_core::host::BackgroundTaskHandle, RuntimeError> {
        Err(RuntimeError::Internal("unused refusal test runtime".into()))
    }

    async fn sleep(&self, _duration: std::time::Duration) {}

    async fn cancel(
        &self,
        _handle: &lingxi_core::host::BackgroundTaskHandle,
    ) -> Result<(), RuntimeError> {
        Ok(())
    }
}

fn refusal_hook_definition(event_type: HookEventType) -> HookDefinition {
    HookDefinition {
        id: HookId::new(),
        name: "refusal tool probe".into(),
        events: vec![event_type],
        if_condition: None,
        executor: DefinitionHookExecutor::Builtin {
            handler_id: "current-alignment-refusal-counter".into(),
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
        on_failure: None,
    }
}

async fn refusal_hook_executor(
    observations: Arc<RefusalHookObservations>,
) -> Arc<HookExecutorImpl> {
    let registry = Arc::new(RwLock::new(HookRegistry::new()));
    for event_type in [
        HookEventType::PreToolUse,
        HookEventType::PostToolUse,
        HookEventType::PostToolUseFailure,
        HookEventType::PostToolBatch,
    ] {
        registry
            .write()
            .await
            .register(refusal_hook_definition(event_type));
    }
    let http: Arc<dyn HttpTransport> = Arc::new(RefusalUnusedHttp);
    let runtime: Arc<dyn RuntimeSpawner> = Arc::new(RefusalUnusedRuntime);
    let mut executor = HookExecutorImpl::new(registry, http, runtime);
    executor.register_builtin(Arc::new(RefusalHookCounter(observations)));
    Arc::new(executor)
}

struct NativeSequenceHarness {
    _dir: tempfile::TempDir,
    orchestrator: Arc<ConversationOrchestrator>,
    transport: Arc<SequencedNativeReplies>,
    output: Arc<MockOutputStream>,
}

struct BatchedNativeHarness {
    _dir: tempfile::TempDir,
    orchestrator: Arc<ConversationOrchestrator>,
    transport: Arc<BatchedNativeReplies>,
}

struct RefusalPermissionBarrier {
    entered: Mutex<Option<oneshot::Sender<()>>>,
    release: Arc<Notify>,
}

struct RefusalBlockingPermissionGate(Arc<RefusalPermissionBarrier>);

#[async_trait]
impl PermissionGate for RefusalBlockingPermissionGate {
    async fn check(&self, _name: &str, _input: &Utf16JsonProjection) -> PermissionDecision {
        PermissionDecision::Allow
    }

    async fn check_with_context(
        &self,
        _name: &str,
        _input: &Utf16JsonProjection,
        _ctx: &PermissionCheckContext,
    ) -> PermissionOutcome {
        if let Some(entered) = self
            .0
            .entered
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = entered.send(());
        }
        self.0.release.notified().await;
        PermissionOutcome::Allow {
            updated_input: None,
            permission_updates: Vec::new(),
            decision_classification: None,
        }
    }
}

fn native_sequence_harness(
    query_source: &str,
    refusal_fallback_model: Option<String>,
    replies: Vec<NativeReply>,
    tools: Arc<ToolRegistry>,
    hooks: Arc<HookExecutorImpl>,
) -> NativeSequenceHarness {
    native_sequence_harness_with_max_turns(
        query_source,
        refusal_fallback_model,
        replies,
        tools,
        hooks,
        1,
    )
}

fn native_sequence_harness_with_max_turns(
    query_source: &str,
    refusal_fallback_model: Option<String>,
    replies: Vec<NativeReply>,
    tools: Arc<ToolRegistry>,
    hooks: Arc<HookExecutorImpl>,
    max_turns: u32,
) -> NativeSequenceHarness {
    native_sequence_harness_with_max_turns_and_permission(
        query_source,
        refusal_fallback_model,
        replies,
        tools,
        hooks,
        max_turns,
        Arc::new(NoOpPermissionGate),
    )
}

fn native_sequence_harness_with_max_turns_and_permission(
    query_source: &str,
    refusal_fallback_model: Option<String>,
    replies: Vec<NativeReply>,
    tools: Arc<ToolRegistry>,
    hooks: Arc<HookExecutorImpl>,
    max_turns: u32,
    permission_gate: Arc<dyn PermissionGate>,
) -> NativeSequenceHarness {
    let dir = tempdir().expect("isolated Native refusal sequence directory");
    let writer = Arc::new(JsonlWriter::new(
        dir.path().join("session.jsonl"),
        Arc::new(PosixFileSystem::new(dir.path().to_path_buf())),
    ));
    let transport = Arc::new(SequencedNativeReplies {
        replies: Mutex::new(replies.into()),
        request_shapes: Mutex::new(Vec::new()),
    });
    let adapter = Arc::new(ProviderApiAdapter::new(api_service(
        transport.clone(),
        llm_runtime::ProviderId::AnthropicFirstParty,
    )));
    let output = Arc::new(MockOutputStream::new());
    let orchestrator = ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig {
            model: MODEL.to_owned(),
            max_turns,
            query_source: query_source.to_owned(),
            refusal_fallback_model,
            ..OrchestratorConfig::default()
        },
        adapter.clone(),
        adapter,
        tools,
        hooks,
        permission_gate,
        output.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        dir.path().to_path_buf(),
    )
    .with_jsonl_writer(writer);
    NativeSequenceHarness {
        _dir: dir,
        orchestrator: ConversationOrchestrator::into_shared(orchestrator),
        transport,
        output,
    }
}

fn batched_native_harness(
    replies: Vec<serde_json::Value>,
    tools: Arc<ToolRegistry>,
) -> BatchedNativeHarness {
    let dir = tempdir().expect("isolated batched Native refusal directory");
    let writer = Arc::new(JsonlWriter::new(
        dir.path().join("session.jsonl"),
        Arc::new(PosixFileSystem::new(dir.path().to_path_buf())),
    ));
    let transport = Arc::new(BatchedNativeReplies {
        replies: Mutex::new(replies.into()),
        request_shapes: Mutex::new(Vec::new()),
    });
    let adapter = Arc::new(ProviderApiAdapter::new(api_service(
        transport.clone(),
        llm_runtime::ProviderId::AnthropicFirstParty,
    )));
    let output = Arc::new(MockOutputStream::new());
    let orchestrator = ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig {
            model: MODEL.to_owned(),
            max_turns: 1,
            query_source: "repl_main_thread".into(),
            ..OrchestratorConfig::default()
        },
        adapter.clone(),
        adapter,
        tools,
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        output.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        dir.path().to_path_buf(),
    )
    .with_jsonl_writer(writer);
    BatchedNativeHarness {
        _dir: dir,
        orchestrator: ConversationOrchestrator::into_shared(orchestrator),
        transport,
    }
}

async fn assert_refusal_error_precedes_tool_results(
    harness: &NativeSequenceHarness,
    tool_ids: &[String],
) {
    let rows = JsonlReader::new(
        harness._dir.path().join("session.jsonl"),
        Arc::new(PosixFileSystem::new(harness._dir.path().to_path_buf())),
    )
    .read_all()
    .await
    .expect("Native refusal follow-up rows parse from JSONL");
    let error_index = rows
        .iter()
        .position(|row| {
            row.message
                .string_at("/content/0/text")
                .is_some_and(|text| text.starts_with("API Error:"))
                && row.message.string_at("/stop_reason") == Some("refusal")
        })
        .expect("Ko=false emits the refusal API-error row");
    for tool_id in tool_ids {
        let result_index = rows
            .iter()
            .position(|row| {
                row.message.string_at("/content/0/tool_use_id") == Some(tool_id.as_str())
            })
            .unwrap_or_else(|| panic!("missing tool result row for {tool_id}"));
        assert!(
            error_index < result_index,
            "refusal API-error row precedes tool results still pending at the refusal boundary: {tool_id}"
        );
    }
}

fn fixed_session_id(id: &str) -> SessionId {
    SessionId::from_uuid(uuid::Uuid::parse_str(id).expect("fixed test session id"))
}

fn result_units(baseline_case: &std::path::Path) -> Vec<u16> {
    let raw = std::fs::read(baseline_case.join("native.stdout.raw"))
        .expect("captured original Native stdout bytes");
    Utf16JsonProjection::parse(&String::from_utf8(raw).expect("Native stdout is UTF-8 JSON"))
        .expect("Native JSON parses without reserializing")
        .string_units("/result")
        .expect("Native result text")
}

fn verify_fixture_checksums(fixture_root: &std::path::Path) {
    let manifest = std::fs::read_to_string(fixture_root.join("SHA256SUMS"))
        .expect("pinned Native 2.1.295 fixture checksums");
    let mut checked = 0;
    for line in manifest.lines() {
        let (expected, relative_path) = line
            .split_once("  ")
            .expect("sha256sum entry uses two-space separator");
        let relative_path = std::path::Path::new(relative_path);
        assert!(
            !relative_path.is_absolute(),
            "fixture checksum path must stay relative: {relative_path:?}"
        );
        let raw = std::fs::read(fixture_root.join(relative_path))
            .unwrap_or_else(|error| panic!("read fixture artifact {relative_path:?}: {error}"));
        let actual = format!("{:x}", Sha256::digest(raw));
        assert_eq!(
            actual, expected,
            "fixture artifact changed: {relative_path:?}"
        );
        checked += 1;
    }
    assert!(checked > 0, "fixture checksum manifest must not be empty");
}

fn verify_native_295_receipt(
    baseline_case: &std::path::Path,
    case: &str,
    session_id: &str,
    response_id: &str,
    stdout_result_units: &[u16],
) {
    let receipt: serde_json::Value = serde_json::from_slice(
        &std::fs::read(baseline_case.join("native-receipt.json"))
            .expect("captured Native receipt bytes"),
    )
    .expect("valid Native receipt JSON");
    assert_eq!(
        receipt["native_version"].as_str(),
        Some("2.1.295 (Claude Code)")
    );
    assert_eq!(
        receipt["native_sha256"].as_str(),
        Some("0116ee2e0a513900b633d9951367f18747686478e2b462805b8c31609f047f70")
    );
    assert_eq!(receipt["case_id"].as_str(), Some(case));
    assert_eq!(receipt["fixed_inputs"]["model"].as_str(), Some(MODEL));
    assert_eq!(
        receipt["fixed_inputs"]["session_id"].as_str(),
        Some(session_id)
    );
    assert_eq!(
        receipt["fixed_inputs"]["response_id"].as_str(),
        Some(response_id)
    );
    assert_eq!(receipt["native_exit_code"].as_i64(), Some(1));
    assert_eq!(receipt["timed_out"].as_bool(), Some(false));

    let receipt_result_units: Vec<u16> = serde_json::from_value(
        receipt["stop_details_observation"]["native_cli_result_utf16_units"].clone(),
    )
    .expect("receipt contains the raw Native result UTF-16 units");
    assert_eq!(receipt_result_units.as_slice(), stdout_result_units);
    assert_eq!(
        receipt["stop_details_observation"]["native_session_stop_details"]
            .as_array()
            .map(Vec::len),
        Some(3),
        "Native 2.1.295 persisted three stop-details rows: {case}"
    );
}

fn explanation_units_hex(fixture_root: &std::path::Path) -> Vec<u16> {
    let raw =
        std::fs::read_to_string(fixture_root.join("cases.json")).expect("fixed stop-details cases");
    let cases: serde_json::Value = serde_json::from_str(&raw).expect("valid cases JSON");
    let hex = cases["explanation_utf16_units_hex"]
        .as_str()
        .expect("fixed explanation UTF-16 hex");
    hex.as_bytes()
        .chunks_exact(4)
        .map(|chunk| {
            u16::from_str_radix(std::str::from_utf8(chunk).expect("ASCII UTF-16 hex"), 16)
                .expect("valid UTF-16 code unit")
        })
        .collect()
}

#[test]
fn native_refusal_results_and_source_stop_details_reach_real_host_output_and_jsonl() {
    run_with_large_stack(
        native_refusal_results_and_source_stop_details_reach_real_host_output_and_jsonl_inner,
    );
}

async fn native_refusal_results_and_source_stop_details_reach_real_host_output_and_jsonl_inner() {
    let fixture_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/parity/current-alignment/fixtures/stop-details-295");
    verify_fixture_checksums(&fixture_root);
    let explanation_units = explanation_units_hex(&fixture_root);
    let cases = [
        (
            "refusal-cyber",
            "00000000-0000-4000-8000-000000002950",
            "cyber",
            "msg_fixed_stop_details_cyber_293",
        ),
        (
            "refusal-generic",
            "00000000-0000-4000-8000-000000002951",
            "",
            "msg_fixed_stop_details_generic_293",
        ),
    ];

    for (case, session_id, category, response_id) in cases {
        let baseline = fixture_root.join("native-baseline").join(case);
        let response_body = std::fs::read(baseline.join("native-response-01.sse.raw"))
            .expect("original Native SSE response bytes");
        let expected_result = result_units(&baseline);
        verify_native_295_receipt(&baseline, case, session_id, response_id, &expected_result);
        let expected_result_text =
            String::from_utf16(&expected_result).expect("Native CLI refusal result is scalar text");
        assert!(
            response_body
                .windows(response_id.len())
                .any(|window| window == response_id.as_bytes()),
            "captured provider response must carry the response ID named by Native output: {case}"
        );
        assert!(
            expected_result_text.ends_with(&format!("\n\nMessage ID: {response_id}")),
            "Native result must be linked to the fixed provider response id: {case}"
        );

        let dir = tempdir().expect("isolated transcript directory");
        let transcript_path = dir.path().join("session.jsonl");
        let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(dir.path().to_path_buf()));
        let writer = Arc::new(JsonlWriter::new(transcript_path.clone(), fs.clone()));
        let transport = Arc::new(FixedNativeRefusal {
            response_body,
            request_shapes: Mutex::new(Vec::new()),
        });
        let adapter = Arc::new(ProviderApiAdapter::new(api_service(
            transport.clone(),
            llm_runtime::ProviderId::AnthropicFirstParty,
        )));
        let output = Arc::new(MockOutputStream::new());
        let orchestrator = ConversationOrchestrator::new_with_streaming(
            OrchestratorConfig {
                model: MODEL.to_owned(),
                max_turns: 1,
                system_prompt_override: Some("Fixed Native refusal source test prompt.".into()),
                ..OrchestratorConfig::default()
            },
            adapter.clone(),
            adapter,
            Arc::new(ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            output.clone(),
            Arc::new(StaticMemoryProvider::empty()),
            dir.path().to_path_buf(),
        )
        .with_session_id(fixed_session_id(session_id))
        .with_jsonl_writer(writer);
        let orchestrator = ConversationOrchestrator::into_shared(orchestrator);

        let outcome = orchestrator
            .run_turn_streaming("Return the fixed refusal stop-details fixture response.")
            .await
            .expect("fixed refusal reaches the terminal turn boundary");
        assert!(matches!(
            outcome,
            ConversationOutcome::EndTurn { turn_count: 1, .. }
        ));
        let metrics = orchestrator
            .completed_turn_metrics()
            .expect("refusal publishes completed execution metrics");
        assert_eq!(metrics.num_turns, 1);
        assert_eq!(metrics.api_error_status, None);
        assert_eq!(metrics.api_error_stop_reason.as_deref(), Some("refusal"));

        let request_shapes = transport
            .request_shapes
            .lock()
            .expect("request-shape mutex")
            .clone();
        assert_eq!(
            request_shapes.len(),
            2,
            "Native 2.1.295 sends a second request after the safety-classifier refusal: {case}"
        );
        assert!(request_shapes
            .iter()
            .all(|shape| { shape.model.as_deref() == Some(MODEL) && shape.stream == Some(true) }));
        assert_eq!(request_shapes[0].refusal_safety_retry_message_count, 0);
        assert_eq!(request_shapes[1].refusal_safety_retry_message_count, 1);

        let expected_notice =
            "Sonnet 4.5's safeguards stopped the response above · continuing once with that noted"
                .to_owned();

        let events = output.snapshot().await;
        let terminal_api_errors = events
            .iter()
            .filter_map(|event| match event {
                OutputEvent::TerminalApiError {
                    api_error_status,
                    terminal_reason,
                    stop_reason,
                } => Some((
                    *api_error_status,
                    terminal_reason.as_str(),
                    stop_reason.as_str(),
                )),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            terminal_api_errors,
            vec![(None, "api_error", "refusal")],
            "the Host terminal marker must match the Native refusal envelope: {case}"
        );
        let system_notices = events
            .iter()
            .filter_map(|event| match event {
                OutputEvent::SystemNotice { body, is_error } => {
                    Some((body.string_at("").map(str::to_owned), *is_error))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            system_notices,
            vec![(Some(expected_notice), false)],
            "the public Host reports the fixed Native safety-classifier continuation: {case}"
        );
        let text_events = events
            .iter()
            .filter_map(|event| match event {
                OutputEvent::Text { text } => text.string_units(""),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            text_events.last(),
            Some(&expected_result),
            "the real Host terminal text must match the entire Native result, including response.id: {case}"
        );

        let rows = JsonlReader::new(transcript_path, fs)
            .read_all()
            .await
            .expect("read actual runtime session JSONL");
        let assistant_rows = rows
            .iter()
            .filter(|row| row.message_type == "assistant")
            .collect::<Vec<_>>();
        let stop_details_rows = assistant_rows
            .iter()
            .filter(|row| {
                row.message
                    .string_units("/stop_details/explanation")
                    .is_some()
            })
            .count();
        assert_eq!(
            stop_details_rows, 3,
            "Native persists two refusal responses and the terminal source row: {case}"
        );
        assert!(
            assistant_rows.iter().any(|row| {
                row.extra
                    .at_pointer("/isApiErrorMessage")
                    .and_then(|value| value.as_bool())
                    == Some(true)
                    && row.extra.string_units("/refusedMessageId")
                        == Some(response_id.encode_utf16().collect())
                    && row.message.string_units("/stop_details/explanation")
                        == Some(explanation_units.clone())
            }),
            "the synthetic refusal API-error row retains Native stop_details: {case}"
        );
        assert!(
            assistant_rows.iter().any(|row| {
                row.message.string_units("/stop_details/explanation")
                    == Some(explanation_units.clone())
            }),
            "the production transcript row must retain Native explanation source units: {case}"
        );
        if category.is_empty() {
            assert!(
                assistant_rows.iter().any(|row| {
                    row.message
                        .at_pointer("/stop_details")
                        .and_then(|details| details.get_property("category"))
                        .is_none()
                }),
                "Native category is absent in the generic case: {case}"
            );
        } else {
            assert!(
                assistant_rows.iter().any(|row| {
                    row.message.string_at("/stop_details/category") == Some(category)
                }),
                "Native refusal category must survive persistence: {case}"
            );
        }
        assert!(rows.iter().any(|row| {
            row.message_type == "user"
                && row.turn_companion == Some(Some(true))
                && row
                    .extra
                    .at_pointer("/isMeta")
                    .and_then(|value| value.as_bool())
                    == Some(true)
                && row.message.string_at("/content/0/text")
                    == Some(NATIVE_REFUSAL_SAFETY_RETRY_USER_MESSAGE)
                && row.extra.string_units("/refusedMessageId")
                    == Some(response_id.encode_utf16().collect())
        }));
    }
}

#[test]
fn refusal_safety_classifier_retry_stays_on_native_model_routes() {
    run_with_large_stack(|| async {
        let fixture_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/parity/current-alignment/fixtures/stop-details-295");
        let baseline = fixture_root.join("native-baseline").join("refusal-cyber");
        let response_body = std::fs::read(baseline.join("native-response-01.sse.raw"))
            .expect("original Native SSE response bytes");
        let dir = tempdir().expect("isolated custom-route transcript directory");
        let transcript_path = dir.path().join("session.jsonl");
        let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(dir.path().to_path_buf()));
        let writer = Arc::new(JsonlWriter::new(transcript_path, fs));
        let transport = Arc::new(FixedNativeRefusal {
            response_body,
            request_shapes: Mutex::new(Vec::new()),
        });
        let adapter = Arc::new(ProviderApiAdapter::new(api_service(
            transport.clone(),
            llm_runtime::ProviderId::Custom {
                name: "anthropic-compatible-test".into(),
            },
        )));
        let output = Arc::new(MockOutputStream::new());
        let orchestrator = ConversationOrchestrator::new_with_streaming(
            OrchestratorConfig {
                model: MODEL.to_owned(),
                max_turns: 1,
                ..OrchestratorConfig::default()
            },
            adapter.clone(),
            adapter,
            Arc::new(ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            output.clone(),
            Arc::new(StaticMemoryProvider::empty()),
            dir.path().to_path_buf(),
        )
        .with_jsonl_writer(writer);
        let orchestrator = ConversationOrchestrator::into_shared(orchestrator);

        orchestrator
            .run_turn_streaming("Return the fixed refusal stop-details fixture response.")
            .await
            .expect("custom Anthropic-compatible refusal reaches terminal boundary");
        let requests = transport
            .request_shapes
            .lock()
            .expect("request-shape mutex")
            .clone();
        assert_eq!(requests.len(), 1, "a custom provider is not safety-retried");
        assert_eq!(requests[0].refusal_safety_retry_message_count, 0);
        assert!(!output.snapshot().await.iter().any(|event| matches!(
            event,
            OutputEvent::SystemNotice { body, .. }
                if body.string_at("") == Some("Sonnet 4.5's safeguards stopped the response above · continuing once with that noted")
        )));
    });
}

#[test]
fn refusal_safety_retry_marker_does_not_suppress_a_later_user_turn() {
    run_with_large_stack(|| async {
        let fixture_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/parity/current-alignment/fixtures/stop-details-295");
        let baseline = fixture_root.join("native-baseline").join("refusal-cyber");
        let response_body = refusal_response_without_stop_details(
            &std::fs::read(baseline.join("native-response-01.sse.raw"))
                .expect("original Native SSE refusal response bytes"),
        );
        let harness = refusal_harness("hook_agent", response_body);

        for user_text in ["first user turn", NATIVE_REFUSAL_SAFETY_RETRY_USER_MESSAGE] {
            let outcome = harness
                .orchestrator
                .run_turn_streaming(user_text)
                .await
                .expect("each refusal turn reaches the terminal boundary");
            assert!(matches!(
                outcome,
                ConversationOutcome::EndTurn { turn_count: 1, .. }
            ));
        }

        let requests = harness
            .transport
            .request_shapes
            .lock()
            .expect("request-shape mutex")
            .clone();
        assert_eq!(requests.len(), 4, "each user turn gets one safety retry");
        assert_eq!(
            requests
                .iter()
                .map(|shape| shape.refusal_safety_retry_message_count)
                .collect::<Vec<_>>(),
            vec![0, 1, 2, 3],
            "the second turn's ordinary user message can equal the refusal marker; each in-turn retry still adds exactly one further occurrence"
        );
        let notices = harness
            .output
            .snapshot()
            .await
            .iter()
            .filter(|event| matches!(event, OutputEvent::SystemNotice { .. }))
            .count();
        assert_eq!(notices, 2, "each turn announces its own one-time retry");
    });
}

#[test]
fn refusal_safety_retry_matches_native_query_source_classifier() {
    run_with_large_stack(|| async {
        let fixture_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/parity/current-alignment/fixtures/stop-details-295");
        let baseline = fixture_root.join("native-baseline").join("refusal-generic");
        let response_body = refusal_response_without_stop_details(
            &std::fs::read(baseline.join("native-response-01.sse.raw"))
                .expect("original Native SSE refusal response bytes"),
        );
        let cases = [
            ("repl_main_thread", true),
            ("repl_main_thread:fork", true),
            ("sdk", true),
            ("agent:reviewer", true),
            ("hook_agent", true),
            ("", false),
            ("other", false),
        ];

        for (query_source, should_retry) in cases {
            let harness = refusal_harness(query_source, response_body.clone());
            harness
                .orchestrator
                .run_turn_streaming("Return the fixed refusal stop-details fixture response.")
                .await
                .expect("query-source refusal reaches its terminal boundary");
            let requests = harness
                .transport
                .request_shapes
                .lock()
                .expect("request-shape mutex")
                .clone();
            assert_eq!(
                requests.len(),
                if should_retry { 2 } else { 1 },
                "Native querySource classification for {query_source:?}"
            );
            assert_eq!(requests[0].refusal_safety_retry_message_count, 0);
            if should_retry {
                assert_eq!(requests[1].refusal_safety_retry_message_count, 1);
            }
        }
    });
}

#[test]
fn refusal_safety_retry_is_skipped_for_auxiliary_queries() {
    run_with_large_stack(|| async {
        let fixture_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/parity/current-alignment/fixtures/stop-details-295");
        let baseline = fixture_root.join("native-baseline").join("refusal-generic");
        let response_body = refusal_response_without_stop_details(
            &std::fs::read(baseline.join("native-response-01.sse.raw"))
                .expect("original Native SSE refusal response bytes"),
        );
        for query_source in ["side_question", "compact", "btw"] {
            let harness = refusal_harness(query_source, response_body.clone());
            harness
                .orchestrator
                .run_turn_streaming("Return the fixed refusal stop-details fixture response.")
                .await
                .expect("auxiliary refusal reaches terminal boundary");
            let requests = harness
                .transport
                .request_shapes
                .lock()
                .expect("request-shape mutex")
                .clone();
            assert_eq!(requests.len(), 1, "auxiliary source {query_source}");
            assert_eq!(requests[0].refusal_safety_retry_message_count, 0);
            assert!(!harness.output.snapshot().await.iter().any(|event| matches!(
                event,
                OutputEvent::SystemNotice { body, .. }
                    if body.string_at("") == Some("Sonnet 4.5's safeguards stopped the response above · continuing once with that noted")
            )));
        }
    });
}

#[test]
fn refusal_safety_retry_respects_disable_environment_flag() {
    const CHILD_MARKER: &str = "LINGXI_R379_REFUSAL_RETRY_TEST_CHILD";
    if std::env::var_os(CHILD_MARKER).is_some() {
        run_with_large_stack(|| async {
            let completed_id = ToolUseId::new().to_string();
            let running_id = ToolUseId::new().to_string();
            let queued_id = ToolUseId::new().to_string();
            let counters = Arc::new(RefusalToolCounters::default());
            let first =
                refusal_with_delayed_tool_uses_reply(&completed_id, &running_id, &queued_id);
            let harness = native_sequence_harness_with_max_turns(
                "sdk",
                None,
                vec![first, native_end_turn_reply("msg_disabled_followup_295")],
                refusal_probe_tool_registry(Arc::clone(&counters)),
                noop_hook_executor(),
                2,
            );
            let outcome = harness
                .orchestrator
                .run_turn_streaming("Exercise disabled refusal retry with tool follow-up.")
                .await
                .expect("disabled retry follows up after executing refused tool uses");
            assert!(matches!(
                outcome,
                ConversationOutcome::EndTurn { turn_count: 2, .. }
            ));
            let requests = harness
                .transport
                .request_shapes
                .lock()
                .expect("request-shape mutex")
                .clone();
            assert_eq!(
                requests.len(),
                2,
                "disabled gate skips the retry but follows up"
            );
            assert_eq!(
                requests
                    .iter()
                    .map(|shape| shape.refusal_safety_retry_message_count)
                    .collect::<Vec<_>>(),
                vec![0, 0],
                "disabled refusal handling does not add a same-model nudge"
            );
            assert_eq!(
                requests[1].tool_result_ids,
                vec![completed_id.clone(), running_id.clone(), queued_id.clone()],
                "the generic follow-up includes all normally completed tool results"
            );
            assert_eq!(counters.completed.load(Ordering::SeqCst), 1);
            assert_eq!(counters.running_started.load(Ordering::SeqCst), 1);
            assert_eq!(counters.queued.load(Ordering::SeqCst), 1);
            assert!(!harness.output.snapshot().await.iter().any(|event| matches!(
                event,
                OutputEvent::SystemNotice { body, .. }
                    if body.string_at("") == Some("Sonnet 4.5's safeguards stopped the response above · continuing once with that noted")
            )));
            assert_refusal_error_precedes_tool_results(&harness, &[running_id, queued_id]).await;
        });
        return;
    }

    let status = Command::new(std::env::current_exe().expect("current integration-test binary"))
        .args([
            "--exact",
            "refusal_safety_retry_respects_disable_environment_flag",
            "--nocapture",
        ])
        .env(CHILD_MARKER, "1")
        .env("CLAUDE_CODE_DISABLE_REFUSAL_RETRY", "1")
        .status()
        .expect("launch isolated test child with Native refusal-retry disable env");
    assert!(
        status.success(),
        "disabled refusal retry child failed: {status}"
    );
}

#[test]
fn tool_bearing_native_refusal_keeps_completed_and_never_dispatches_queued_calls() {
    run_with_large_stack(|| async {
        let fixture_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/parity/current-alignment/fixtures/stop-details-295");
        let baseline = fixture_root.join("native-baseline").join("refusal-generic");
        let completed_id = ToolUseId::new().to_string();
        let running_id = ToolUseId::new().to_string();
        let queued_id = ToolUseId::new().to_string();
        let counters = Arc::new(RefusalToolCounters::default());
        let tools = refusal_probe_tool_registry(Arc::clone(&counters));
        let observations = Arc::new(RefusalHookObservations::default());
        let hooks = refusal_hook_executor(Arc::clone(&observations)).await;
        let first = refusal_with_tool_uses_reply(&completed_id, &running_id, &queued_id);
        let second = native_reply_from_bytes(
            std::fs::read(baseline.join("native-response-01.sse.raw"))
                .expect("pinned Native refusal fixture for terminal retry response"),
        );
        let harness = native_sequence_harness("sdk", None, vec![first, second], tools, hooks);

        let outcome = harness
            .orchestrator
            .run_turn_streaming("Exercise tool settlement at a Native refusal boundary.")
            .await
            .expect("tool-bearing refusal settles and retries once");
        assert!(matches!(
            outcome,
            ConversationOutcome::EndTurn { turn_count: 1, .. }
        ));

        let request_shapes = harness
            .transport
            .request_shapes
            .lock()
            .expect("request-shape mutex")
            .clone();
        assert_eq!(request_shapes.len(), 2, "same-model retry is one-shot");
        assert_eq!(request_shapes[0].refusal_safety_retry_message_count, 0);
        assert_eq!(request_shapes[1].refusal_safety_retry_message_count, 1);

        let history = harness.orchestrator.session().lock().await.history.clone();
        let results = history
            .iter()
            .flat_map(|message| match message {
                ConversationMessage::User { content, .. } => content.as_slice(),
                _ => &[],
            })
            .filter_map(|block| match block {
                lingxi_core::types::ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                    ..
                } => Some((
                    tool_use_id.to_string(),
                    content.string_at("").unwrap_or_default().to_owned(),
                    is_error.unwrap_or(false),
                )),
                _ => None,
            })
            .collect::<Vec<(String, String, bool)>>();
        assert!(
            results.iter().any(|(id, text, is_error)| {
                id == &completed_id && !is_error && text.contains("ok")
            }),
            "the already-completed tool keeps its real result: {results:?}"
        );
        assert!(results.iter().any(|(id, text, is_error)| {
            id == &running_id
                && *is_error
                && text == "Interrupted: the response that made this tool call was stopped by a safety classifier while the call was running; it may have partially or fully completed."
        }));
        assert!(results.iter().any(|(id, text, is_error)| {
            id == &queued_id
                && *is_error
                && text == "Not run: the response that made this tool call was stopped by a safety classifier."
        }));
        assert_eq!(counters.completed.load(Ordering::SeqCst), 1);
        assert_eq!(counters.running_started.load(Ordering::SeqCst), 1);
        assert_eq!(counters.queued.load(Ordering::SeqCst), 0);
        let running_tool_nudge = format!(
            "{NATIVE_REFUSAL_SAFETY_RETRY_USER_MESSAGE}{NATIVE_REFUSAL_RUNNING_TOOL_EXCEPTION}"
        );
        assert!(history.iter().any(|message| match message {
            ConversationMessage::User {
                content,
                is_meta: true,
                ..
            } => content
                .iter()
                .any(|block| block.visible_text() == Some(running_tool_nudge.as_str())),
            _ => false,
        }));

        let rows = JsonlReader::new(
            harness._dir.path().join("session.jsonl"),
            Arc::new(PosixFileSystem::new(harness._dir.path().to_path_buf())),
        )
        .read_all()
        .await
        .expect("tool refusal rows parse from JSONL");
        let denial_kinds = rows
            .iter()
            .filter_map(|row| {
                let id = row.message.string_at("/content/0/tool_use_id")?;
                Some((
                    id.to_owned(),
                    row.extra.string_at("/toolDenialKind").map(str::to_owned),
                ))
            })
            .collect::<Vec<_>>();
        assert!(denial_kinds.contains(&(running_id.clone(), Some("interrupted".into()))));
        assert!(denial_kinds.contains(&(queued_id.clone(), Some("cancelled".into()))));

        let observed = observations
            .events
            .lock()
            .expect("hook observation mutex")
            .clone();
        assert!(observed.contains(&format!("pre:{completed_id}")));
        assert!(observed.contains(&format!("post:{completed_id}")));
        assert!(observed.contains(&format!("batch:{completed_id}")));
        assert_eq!(
            observed
                .iter()
                .filter(|event| event.as_str() == format!("pre:{running_id}"))
                .count(),
            1,
            "the running call's real pre-hook remains exactly once"
        );
        assert!(!observed.contains(&format!("batch:{running_id}")));
        assert!(!observed.contains(&format!("batch:{queued_id}")));
        assert!(
            !observed.iter().any(|event| event.ends_with(&queued_id)),
            "a never-dispatched queued call must not trigger any hook: {observed:?}"
        );
        assert!(
            observed
                .iter()
                .filter(|event| {
                    event.as_str() == format!("post:{running_id}")
                        || event.as_str() == format!("failure:{running_id}")
                })
                .count()
                <= 1,
            "an in-flight tool may retain its one real terminal hook, never a synthetic duplicate: {observed:?}"
        );
    });
}

#[test]
fn native_refusal_closes_a_permission_waiting_tool_before_physical_call() {
    run_with_large_stack(|| async {
        let permission_id = ToolUseId::new().to_string();
        let counters = Arc::new(RefusalToolCounters::default());
        let observations = Arc::new(RefusalHookObservations::default());
        let hooks = refusal_hook_executor(Arc::clone(&observations)).await;
        let (entered_tx, entered_rx) = oneshot::channel();
        let barrier = Arc::new(RefusalPermissionBarrier {
            entered: Mutex::new(Some(entered_tx)),
            release: Arc::new(Notify::new()),
        });
        let harness = native_sequence_harness_with_max_turns_and_permission(
            "sdk",
            None,
            vec![
                refusal_with_one_tool_reply(&permission_id, "CompletedRefusalProbe"),
                native_end_turn_reply("msg_permission_wait_followup_295"),
            ],
            refusal_probe_tool_registry_with_permission_block(Arc::clone(&counters)),
            hooks,
            1,
            Arc::new(RefusalBlockingPermissionGate(Arc::clone(&barrier))),
        );

        let orchestrator = Arc::clone(&harness.orchestrator);
        let run = tokio::spawn(async move {
            orchestrator
                .run_turn_streaming("Exercise refusal while permission is pending.")
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), entered_rx)
            .await
            .expect("permission gate is reached before the refusal boundary")
            .expect("permission-entry signal remains connected");
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if harness
                    .transport
                    .request_shapes
                    .lock()
                    .expect("request-shape mutex")
                    .len()
                    == 2
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("Native refusal retry is sent while the original permission waits");
        barrier.release.notify_one();
        // The second validation occurs after the permission gate returns and
        // immediately before native-computer admission and the call-start CAS.
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while counters
                .permission_blocked_validations
                .load(Ordering::SeqCst)
                < 2
            {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("released permission waiter reaches the final pre-call validation");
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), run)
            .await
            .expect("permission waiter is released after the retry request")
            .expect("refusal test task joins")
            .expect("permission-waiting refusal reaches terminal response");
        assert!(matches!(
            outcome,
            ConversationOutcome::EndTurn { turn_count: 1, .. }
        ));

        let requests = harness
            .transport
            .request_shapes
            .lock()
            .expect("request-shape mutex")
            .clone();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].refusal_safety_retry_message_count, 1);
        assert_eq!(requests[1].tool_result_ids, vec![permission_id.clone()]);
        assert_eq!(counters.completed.load(Ordering::SeqCst), 0);
        assert!(
            counters
                .permission_blocked_validations
                .load(Ordering::SeqCst)
                >= 2,
            "the old permission waiter resumed and reached pre-call validation"
        );
        assert_eq!(
            counters.permission_blocked_started.load(Ordering::SeqCst),
            0
        );
        assert_eq!(counters.running_started.load(Ordering::SeqCst), 0);
        assert_eq!(counters.queued.load(Ordering::SeqCst), 0);

        let history = harness.orchestrator.session().lock().await.history.clone();
        let refusal_results = history
            .iter()
            .flat_map(|message| match message {
                ConversationMessage::User { content, .. } => content.as_slice(),
                _ => &[],
            })
            .filter_map(|block| match block {
                lingxi_core::types::ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                    ..
                } => Some((
                    tool_use_id.to_string(),
                    content.string_at("").unwrap_or_default().to_owned(),
                    is_error.unwrap_or(false),
                )),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(refusal_results.iter().any(|(result_id, text, is_error)| {
            result_id == &permission_id
                && *is_error
                && text
                    == "Not run: the response that made this tool call was stopped by a safety classifier."
        }));
        let observed = observations
            .events
            .lock()
            .expect("hook observation mutex")
            .clone();
        assert!(observed.contains(&format!("pre:{permission_id}")));
        assert!(!observed.contains(&format!("post:{permission_id}")));
        assert!(!observed.contains(&format!("failure:{permission_id}")));
        assert!(!observed.contains(&format!("batch:{permission_id}")));
        assert!(history.iter().any(|message| matches!(
            message,
            ConversationMessage::User {
                content,
                is_meta: true,
                ..
            } if content.iter().any(|block| block.visible_text() == Some(NATIVE_REFUSAL_SAFETY_RETRY_USER_MESSAGE))
        )));
    });
}

#[test]
fn auxiliary_native_tool_refusal_runs_queued_work_and_follows_up() {
    run_with_large_stack(|| async {
        let completed_id = ToolUseId::new().to_string();
        let running_id = ToolUseId::new().to_string();
        let queued_id = ToolUseId::new().to_string();
        let counters = Arc::new(RefusalToolCounters::default());
        let first = refusal_with_delayed_tool_uses_reply(&completed_id, &running_id, &queued_id);
        let harness = native_sequence_harness_with_max_turns(
            "side_question",
            None,
            vec![first, native_end_turn_reply("msg_aux_followup_295")],
            refusal_probe_tool_registry(Arc::clone(&counters)),
            noop_hook_executor(),
            2,
        );

        let outcome = harness
            .orchestrator
            .run_turn_streaming("Exercise an auxiliary query's refusal tool follow-up.")
            .await
            .expect("auxiliary tool refusal follows up after normal tool execution");
        assert!(matches!(
            outcome,
            ConversationOutcome::EndTurn { turn_count: 2, .. }
        ));
        let requests = harness
            .transport
            .request_shapes
            .lock()
            .expect("request-shape mutex")
            .clone();
        assert_eq!(
            requests.len(),
            2,
            "auxiliary query skips only the safety retry"
        );
        assert_eq!(requests[0].refusal_safety_retry_message_count, 0);
        assert_eq!(requests[1].refusal_safety_retry_message_count, 0);
        assert_eq!(
            requests[1].tool_result_ids,
            vec![completed_id.clone(), running_id.clone(), queued_id.clone()],
            "Native's generic needsFollowUp request includes all three results"
        );
        assert_eq!(counters.completed.load(Ordering::SeqCst), 1);
        assert_eq!(counters.running_started.load(Ordering::SeqCst), 1);
        assert_eq!(counters.queued.load(Ordering::SeqCst), 1);

        let history = harness.orchestrator.session().lock().await.history.clone();
        let successful_ids = history
            .iter()
            .flat_map(|message| match message {
                ConversationMessage::User { content, .. } => content.as_slice(),
                _ => &[],
            })
            .filter_map(|block| match block {
                lingxi_core::types::ContentBlock::ToolResult {
                    tool_use_id,
                    is_error,
                    ..
                } if !is_error.unwrap_or(false) => Some(tool_use_id.to_string()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(successful_ids.contains(&completed_id));
        assert!(successful_ids.contains(&running_id));
        assert!(successful_ids.contains(&queued_id));
        assert_refusal_error_precedes_tool_results(&harness, &[running_id, queued_id]).await;
    });
}

#[test]
fn spent_native_refusal_retry_runs_later_tool_uses_and_follows_up() {
    run_with_large_stack(|| async {
        let fixture_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/parity/current-alignment/fixtures/stop-details-295");
        let baseline = fixture_root.join("native-baseline").join("refusal-generic");
        let first_refusal = refusal_response_without_stop_details(
            &std::fs::read(baseline.join("native-response-01.sse.raw"))
                .expect("pinned Native refusal response"),
        );
        let completed_id = ToolUseId::new().to_string();
        let running_id = ToolUseId::new().to_string();
        let queued_id = ToolUseId::new().to_string();
        let counters = Arc::new(RefusalToolCounters::default());
        let harness = native_sequence_harness_with_max_turns(
            "sdk",
            None,
            vec![
                native_reply_from_bytes(first_refusal),
                refusal_with_delayed_tool_uses_reply(&completed_id, &running_id, &queued_id),
                native_end_turn_reply("msg_spent_retry_followup_295"),
            ],
            refusal_probe_tool_registry(Arc::clone(&counters)),
            noop_hook_executor(),
            2,
        );

        let outcome = harness
            .orchestrator
            .run_turn_streaming("Exercise the spent refusal retry tool boundary.")
            .await
            .expect("spent retry refusal follows up after executing tool uses");
        assert!(matches!(
            outcome,
            ConversationOutcome::EndTurn { turn_count: 2, .. }
        ));
        let requests = harness
            .transport
            .request_shapes
            .lock()
            .expect("request-shape mutex")
            .clone();
        assert_eq!(requests.len(), 3);
        assert_eq!(
            requests
                .iter()
                .map(|shape| shape.refusal_safety_retry_message_count)
                .collect::<Vec<_>>(),
            vec![0, 1, 1],
            "a second refusal after the same-model retry is spent does not add another nudge"
        );
        assert_eq!(
            requests[2].tool_result_ids,
            vec![completed_id.clone(), running_id.clone(), queued_id.clone()],
            "the spent-retry refusal uses normal tool follow-up"
        );
        assert_eq!(counters.completed.load(Ordering::SeqCst), 1);
        assert_eq!(counters.running_started.load(Ordering::SeqCst), 1);
        assert_eq!(counters.queued.load(Ordering::SeqCst), 1);
        assert_refusal_error_precedes_tool_results(&harness, &[running_id, queued_id]).await;
    });
}

#[test]
fn native_same_model_refusal_retry_precedes_configured_refusal_fallback() {
    run_with_large_stack(|| async {
        let fixture_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/parity/current-alignment/fixtures/stop-details-295");
        let baseline = fixture_root.join("native-baseline").join("refusal-generic");
        let refusal = std::fs::read(baseline.join("native-response-01.sse.raw"))
            .expect("pinned Native refusal fixture");
        let harness = native_sequence_harness(
            "sdk",
            Some("claude-opus-4-7".into()),
            vec![
                native_reply_from_bytes(refusal.clone()),
                native_reply_from_bytes(refusal),
                native_end_turn_reply("msg_fallback_complete_295"),
            ],
            Arc::new(ToolRegistry::new()),
            noop_hook_executor(),
        );
        let outcome = harness
            .orchestrator
            .run_turn_streaming("Verify same-model refusal retry wins before fallback.")
            .await
            .expect("refusal sequence reaches configured fallback completion");
        assert!(matches!(
            outcome,
            ConversationOutcome::EndTurn { turn_count: 1, .. }
        ));

        let requests = harness
            .transport
            .request_shapes
            .lock()
            .expect("request-shape mutex")
            .clone();
        assert_eq!(requests.len(), 3);
        assert_eq!(
            requests
                .iter()
                .map(|shape| shape.model.as_deref())
                .collect::<Vec<_>>(),
            vec![Some(MODEL), Some(MODEL), Some("claude-opus-4-7")],
            "the first refusal uses Native's same-model retry before the once-only fallback"
        );
        assert_eq!(
            requests
                .iter()
                .map(|shape| shape.refusal_safety_retry_message_count)
                .collect::<Vec<_>>(),
            vec![0, 1, 1]
        );
        assert!(harness.output.snapshot().await.iter().any(|event| matches!(
            event,
            OutputEvent::Text { text }
                if text.string_at("").is_some_and(|text| text.contains("Switched to Opus 4.7"))
        )));
    });
}

#[test]
fn refusal_reissue_credit_does_not_cover_a_normal_next_turn_at_max_turns_one() {
    run_with_large_stack(|| async {
        let fixture_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/parity/current-alignment/fixtures/stop-details-295");
        let baseline = fixture_root.join("native-baseline").join("refusal-generic");
        let refusal = refusal_response_without_stop_details(
            &std::fs::read(baseline.join("native-response-01.sse.raw"))
                .expect("pinned Native refusal fixture"),
        );
        let tool_id = ToolUseId::new().to_string();
        let counters = Arc::new(RefusalToolCounters::default());
        let harness = native_sequence_harness_with_max_turns(
            "sdk",
            None,
            vec![
                native_reply_from_bytes(refusal),
                one_tool_reply(&tool_id, "CompletedRefusalProbe", "tool_use"),
            ],
            refusal_probe_tool_registry(Arc::clone(&counters)),
            noop_hook_executor(),
            1,
        );

        let result = harness
            .orchestrator
            .run_turn_streaming("Verify refusal retry credit is consumed exactly once.")
            .await;
        assert!(matches!(
            result,
            Err(orchestrator::OrchestratorError::MaxTurnsReached { max_turns: 1 })
        ));
        let requests = harness
            .transport
            .request_shapes
            .lock()
            .expect("request-shape mutex")
            .clone();
        assert_eq!(requests.len(), 2, "the normal NextTurn is capped");
        assert_eq!(requests[0].refusal_safety_retry_message_count, 0);
        assert_eq!(requests[1].refusal_safety_retry_message_count, 1);
        assert_eq!(counters.completed.load(Ordering::SeqCst), 1);
    });
}

#[test]
fn batched_native_refusal_closes_tool_uses_before_dispatch_and_retries_once() {
    run_with_large_stack(|| async {
        let tool_id = ToolUseId::new().to_string();
        let counters = Arc::new(RefusalToolCounters::default());
        let harness = batched_native_harness(
            vec![
                native_batched_response(
                    "msg_batched_refusal_295",
                    vec![json!({
                        "type": "tool_use",
                        "id": tool_id,
                        "name": "CompletedRefusalProbe",
                        "input": {}
                    })],
                    "refusal",
                ),
                native_batched_response(
                    "msg_batched_refusal_retry_done_295",
                    vec![json!({"type": "text", "text": "Done"})],
                    "end_turn",
                ),
            ],
            refusal_probe_tool_registry(Arc::clone(&counters)),
        );

        let outcome = harness
            .orchestrator
            .run_turn("Exercise the buffered Native refusal tool boundary.")
            .await
            .expect("buffered refusal follows the same-model retry");
        assert!(matches!(
            outcome,
            ConversationOutcome::EndTurn { turn_count: 1, .. }
        ));

        let requests = harness
            .transport
            .request_shapes
            .lock()
            .expect("batched request-shape mutex")
            .clone();
        assert_eq!(requests.len(), 2, "the response gets one same-model retry");
        assert_eq!(requests[0].refusal_safety_retry_message_count, 0);
        assert_eq!(requests[1].refusal_safety_retry_message_count, 1);
        assert_eq!(requests[1].tool_result_ids, vec![tool_id.clone()]);
        assert_eq!(counters.completed.load(Ordering::SeqCst), 0);

        let history = harness.orchestrator.session().lock().await.history.clone();
        assert!(history.iter().any(|message| matches!(
            message,
            ConversationMessage::User { content, .. }
                if content.iter().any(|block| matches!(
                    block,
                    lingxi_core::types::ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error: Some(true),
                        ..
                    } if tool_use_id.as_str() == tool_id.as_str()
                        && content.string_at("")
                            == Some("Not run: the response that made this tool call was stopped by a safety classifier.")
                ))
        )));
        assert!(history.iter().any(|message| matches!(
            message,
            ConversationMessage::User {
                content,
                is_meta: true,
                ..
            } if content.iter().any(|block| {
                block.visible_text() == Some(NATIVE_REFUSAL_SAFETY_RETRY_USER_MESSAGE)
            })
        )));
    });
}

#[test]
fn batched_refusal_catalog_gate_uses_the_physical_route_not_reported_model() {
    run_with_large_stack(|| async {
        let dir = tempdir().expect("isolated route-alias refusal directory");
        let module = dir.path().join("turn-step-route-alias.js");
        std::fs::write(
            &module,
            r#"export function register(on) {
              on('turn.step', async function* ($, event, next) {
                const below = next({ ...event, model: 'claude-opus-4-7' });
                for await (const chunk of below) yield chunk;
                return await below.result;
              });
            }"#,
        )
        .expect("route-alias turn.step Mod source");
        let host = hooks::mods::ModHost::start(None)
            .await
            .expect("Native turn.step Mod host");
        host.load(
            "route-alias-refusal",
            dir.path(),
            &module,
            Utf16JsonProjection::parse("{}").expect("empty Mod options"),
        )
        .await
        .expect("load route-alias turn.step Mod");
        let mut registry = HookRegistry::new();
        registry.set_mod_host(host);

        let writer = Arc::new(JsonlWriter::new(
            dir.path().join("session.jsonl"),
            Arc::new(PosixFileSystem::new(dir.path().to_path_buf())),
        ));
        let transport = Arc::new(BatchedNativeReplies {
            replies: Mutex::new(
                vec![
                    native_batched_response(
                        "msg_custom_route_refusal_295",
                        vec![json!({"type": "text", "text": "provider refusal"})],
                        "refusal",
                    ),
                    native_batched_response(
                        "msg_unexpected_native_retry_295",
                        vec![json!({"type": "text", "text": "unexpected retry"})],
                        "end_turn",
                    ),
                ]
                .into(),
            ),
            request_shapes: Mutex::new(Vec::new()),
        });
        let adapter = Arc::new(ProviderApiAdapter::new(
            api_service_with_native_and_alias_routes(transport.clone()),
        ));
        let output = Arc::new(MockOutputStream::new());
        let orchestrator = ConversationOrchestrator::new_with_streaming(
            OrchestratorConfig {
                model: MODEL.to_owned(),
                max_turns: 1,
                query_source: "repl_main_thread".into(),
                ..OrchestratorConfig::default()
            },
            adapter.clone(),
            adapter,
            Arc::new(ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            output,
            Arc::new(StaticMemoryProvider::empty()),
            dir.path().to_path_buf(),
        )
        .with_jsonl_writer(writer)
        .with_hook_registry(Arc::new(RwLock::new(registry)));
        let orchestrator = ConversationOrchestrator::into_shared(orchestrator);
        orchestrator.session().lock().await.model_profile = Some("direct".into());

        let outcome = orchestrator
            .run_turn("Check the selected provider route for refusal retry.")
            .await
            .expect("custom-route refusal reaches terminal handling");
        assert!(matches!(
            outcome,
            ConversationOutcome::EndTurn { turn_count: 1, .. }
        ));

        let requests = transport
            .request_shapes
            .lock()
            .expect("route-alias request-shape mutex")
            .clone();
        assert_eq!(
            requests.len(),
            1,
            "a provider-reported Native model does not reclassify the custom physical route"
        );
        assert_eq!(requests[0].model.as_deref(), Some("claude-opus-4-7"));
        assert_eq!(requests[0].refusal_safety_retry_message_count, 0);

        let history = orchestrator.session().lock().await.history.clone();
        let terminal_error = history.iter().find_map(|message| match message {
            ConversationMessage::Assistant { content, .. } => content.iter().find_map(|block| {
                block
                    .visible_text()
                    .filter(|text| text.starts_with("API Error:"))
            }),
            _ => None,
        });
        let terminal_error = terminal_error.expect("terminal refusal API-error text");
        assert!(
            terminal_error.contains("Opus 4.7 can't help"),
            "terminal label follows the actual custom route: {terminal_error}"
        );
        assert!(
            !terminal_error.contains("Sonnet 4.5 can't help"),
            "provider-reported model does not replace the request route label: {terminal_error}"
        );
    });
}
