use super::*;
use crate::test_support::{
    content_block_start_text, content_block_start_tool_use, content_block_stop, input_json_delta,
    message_delta_stop, message_start, message_stop, mock_message_response, noop_hook_executor,
    text_delta, MockApiClient, MockOutputStream, MockStreamingApiClient, NoOpPermissionGate,
    StaticMemoryProvider,
};
use crate::{ConversationOutcome, OrchestratorConfig};
use async_trait::async_trait;
use hooks::definition::{HookDefinition, HookExecutor as DefinitionExecutor, HookSource};
use hooks::events::{HookEvent, HookEventType};
use hooks::executor::{BuiltinHookHandler, HookExecutorImpl};
use hooks::registry::HookRegistry;
use hooks::response::{HookDecision, HookResponse};
use hooks::{HookContext, HookOutcome, HookResult};
use lingxi_core::types::{HookId, ImageSource, SessionId};
use std::sync::atomic::AtomicUsize;
use tool_api::progress::ToolProgressSender;
use tool_api::registry::ToolRegistry;
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolInputRepairMetadata,
    ToolInputRepairSchema, ToolInputSchemaProvenance, ValidationError,
};

struct DeferredCounter(Arc<AtomicUsize>);

#[async_trait]
impl Tool for DeferredCounter {
    fn name(&self) -> &str {
        "DeferredCounter"
    }
    fn input_schema(&self) -> &Utf16JsonProjection {
        static SCHEMA: std::sync::OnceLock<Utf16JsonProjection> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| Utf16JsonProjection::parse(r#"{"type":"object"}"#).unwrap())
    }
    fn input_repair_metadata(&self) -> ToolInputRepairMetadata {
        ToolInputRepairMetadata::Known {
            schema: ToolInputRepairSchema::Advertised,
            schema_provenance: ToolInputSchemaProvenance::GeneratedBuiltin,
            exempt_top_level_keys: None,
        }
    }
    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        1024
    }
    fn is_concurrency_safe(&self, _: &Utf16JsonProjection) -> bool {
        true
    }
    fn is_read_only(&self, _: &Utf16JsonProjection) -> bool {
        false
    }
    async fn validate_input(
        &self,
        _: &Utf16JsonProjection,
        _: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        Ok(())
    }
    async fn check_permissions(
        &self,
        _: &Utf16JsonProjection,
        _: &ToolUseContext,
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
    async fn description(&self, _: &Utf16JsonProjection, _: &DescriptionOptions) -> String {
        "count executions".into()
    }
    async fn prompt(&self, _: &PromptOptions) -> Utf16JsonProjection {
        "count executions".into()
    }
    async fn call(
        &self,
        _: Utf16JsonProjection,
        _: ToolUseContext,
        _: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ToolCallResult {
            data: Utf16JsonProjection::parse(r#"{"content":"executed"}"#).unwrap(),
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            subagent_hooks_fired: false,
            mcp_meta: None,
        })
    }
}

struct UnusedHttp;

#[async_trait]
impl lingxi_core::host::HttpTransport for UnusedHttp {
    async fn request(
        &self,
        _: lingxi_core::types::HttpRequest,
    ) -> Result<lingxi_core::types::HttpResponse, lingxi_core::host::HttpError> {
        Err(lingxi_core::host::HttpError::InvalidRequest(
            "unused".into(),
        ))
    }
    async fn stream_sse(
        &self,
        _: lingxi_core::types::HttpRequest,
    ) -> Result<lingxi_core::host::http::SseStream, lingxi_core::host::HttpError> {
        Err(lingxi_core::host::HttpError::InvalidRequest(
            "unused".into(),
        ))
    }
}

struct UnusedRuntime;

#[async_trait]
impl lingxi_core::host::RuntimeSpawner for UnusedRuntime {
    async fn spawn(
        &self,
        _: &str,
        _: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
    ) -> Result<lingxi_core::host::BackgroundTaskHandle, lingxi_core::host::RuntimeError> {
        Err(lingxi_core::host::RuntimeError::Internal("unused".into()))
    }
    async fn sleep(&self, _: std::time::Duration) {}
    async fn cancel(
        &self,
        _: &lingxi_core::host::BackgroundTaskHandle,
    ) -> Result<(), lingxi_core::host::RuntimeError> {
        Ok(())
    }
}

struct DeferHook(Arc<AtomicUsize>);

#[async_trait]
impl BuiltinHookHandler for DeferHook {
    async fn handle(&self, event: &HookEvent, _: &HookContext) -> HookResult {
        let response = if matches!(event, HookEvent::PreToolUse { .. }) {
            HookResponse {
                decision: Some(HookDecision::Defer),
                additional_context: Some("DEFER-CONTEXT".into()),
                ..HookResponse::default()
            }
        } else {
            self.0.fetch_add(1, Ordering::SeqCst);
            HookResponse::default()
        };
        HookResult {
            outcome: HookOutcome::Success,
            stdout: "".into(),
            stderr: "".into(),
            exit_code: Some(0),
            response: Some(response),
        }
    }
    fn id(&self) -> &str {
        "defer-counter"
    }
}

fn defer_hooks(post_batch_calls: Arc<AtomicUsize>) -> Arc<HookExecutorImpl> {
    let mut registry = HookRegistry::new();
    registry.register(HookDefinition {
        id: HookId::new(),
        name: "defer-counter".into(),
        events: vec![HookEventType::PreToolUse, HookEventType::PostToolBatch],
        if_condition: None,
        executor: DefinitionExecutor::Builtin {
            handler_id: "defer-counter".into(),
        },
        source: HookSource::Session,
        blocking: true,
        timeout: None,
        priority: 0,
        once: false,
        status_message: None,
        async_rewake: false,
        async_timeout: None,
        rewake_message: None,
        on_failure: None,
    });
    let mut executor = HookExecutorImpl::new(
        Arc::new(tokio::sync::RwLock::new(registry)),
        Arc::new(UnusedHttp),
        Arc::new(UnusedRuntime),
    );
    executor.register_builtin(Arc::new(DeferHook(post_batch_calls)));
    Arc::new(executor)
}

#[tokio::test]
async fn streaming_headless_solo_defer_stops_without_result_and_retains_cold_replay() {
    let calls = Arc::new(AtomicUsize::new(0));
    let post_batch_calls = Arc::new(AtomicUsize::new(0));
    let tool_id = ToolUseId::from("deferred-stream-call");
    let streaming = Arc::new(MockStreamingApiClient::with_turns(vec![
        vec![
            message_start("deferred", "claude-opus-4-7"),
            content_block_start_tool_use(0, tool_id.clone(), "DeferredCounter"),
            input_json_delta(0, "{}"),
            content_block_stop(0),
            message_delta_stop("tool_use"),
            message_stop(),
        ],
        vec![
            message_start("unexpected", "claude-opus-4-7"),
            content_block_start_text(0),
            text_delta(0, "must not continue after defer"),
            content_block_stop(0),
            message_delta_stop("end_turn"),
            message_stop(),
        ],
    ]));
    let mut tools = ToolRegistry::new();
    tools.register_builtin(Arc::new(DeferredCounter(calls.clone())));
    let scratch = tempfile::tempdir().unwrap();
    let session_id = SessionId::new();
    let path = scratch
        .path()
        .join(format!("{}.jsonl", session_id.as_uuid()));
    let fs: Arc<dyn lingxi_core::host::FileSystem> = Arc::new(
        platform_posix::fs::PosixFileSystem::new(scratch.path().to_path_buf()),
    );
    let writer = Arc::new(session::jsonl::writer::JsonlWriter::new(
        path.clone(),
        fs.clone(),
    ));
    let output = Arc::new(MockOutputStream::new());
    let orch = ConversationOrchestrator::into_shared(
        ConversationOrchestrator::new_with_streaming(
            OrchestratorConfig {
                interactive_permissions: false,
                ..OrchestratorConfig::default()
            },
            Arc::new(MockApiClient::new(vec![])),
            streaming.clone(),
            Arc::new(tools),
            defer_hooks(post_batch_calls.clone()),
            Arc::new(NoOpPermissionGate),
            output.clone(),
            Arc::new(StaticMemoryProvider::empty()),
            scratch.path().to_path_buf(),
        )
        .with_conversation_mode(lingxi_core::host::session_context::ConversationMode::Code)
        .with_config_home(scratch.path().join("config"))
        .with_session_id(session_id)
        .with_jsonl_writer(writer)
        .without_host_environment_probes(),
    );
    orch.initialize_conversation_mode().await.unwrap();
    let outcome = orch.run_turn_streaming("defer the counter").await.unwrap();
    assert!(matches!(
        outcome,
        ConversationOutcome::EndTurn { turn_count: 1, .. }
    ));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "the real tool must not run"
    );
    assert_eq!(
        post_batch_calls.load(Ordering::SeqCst),
        0,
        "defer suppresses PostToolBatch"
    );
    assert_eq!(
        streaming.captured_calls().await.len(),
        1,
        "defer stops the turn"
    );
    let history = orch.session.lock().await.history.clone();
    assert!(history
        .iter()
        .any(|message| message.text_content().contains("DEFER-CONTEXT")));
    assert!(
        history.iter().all(|message| match message {
            ConversationMessage::User { content, .. } => content.iter().all(|block| !matches!(
                block, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == &tool_id
            )),
            _ => true,
        }),
        "defer cannot create a resolving error ToolResult"
    );
    assert!(output
        .snapshot()
        .await
        .iter()
        .all(|event| !matches!(event, lingxi_core::host::OutputEvent::ToolResult { .. })));
    let cold = crate::resume::replay_session_state_from_path(&path, session_id.as_uuid(), fs)
        .await
        .expect("the initialized UUID transcript must pass the real cold loader");
    let rows = &cold.messages;
    let deferred = crate::resume::deferred_tool_replays_from_messages(rows);
    assert_eq!(
        deferred.len(),
        1,
        "cold resume must still find the unresolved defer marker"
    );
    assert_eq!(deferred[0].tool_use_id, tool_id.as_str());
    assert_eq!(deferred[0].tool_name, "DeferredCounter");
    assert_eq!(
        rows.iter()
            .filter(|row| row
                .attachment_projection
                .as_ref()
                .is_some_and(
                    |attachment| attachment.string_at("/type") == Some("hook_deferred_tool")
                ))
            .count(),
        1,
        "publish the accepted defer marker once"
    );
    assert_eq!(cold.runtime_metadata.deferred_tools.len(), 1);
    assert_eq!(
        cold.runtime_metadata.deferred_tools[0].tool_use_id,
        tool_id.as_str()
    );
}

const IMAGE_REJECTION_REASON: &str = "user rejected with screenshot context";

fn rejection_images() -> Vec<ContentBlock> {
    ["AQID", "BAUG"]
        .into_iter()
        .map(|data| ContentBlock::Image {
            source: ImageSource::Base64 {
                media_type: "image/png".into(),
                data: data.into(),
            },
        })
        .collect()
}

struct AskRejectWithImages(Arc<AtomicUsize>);

#[async_trait]
impl crate::test_support::PermissionGate for AskRejectWithImages {
    async fn check(
        &self,
        _: &str,
        _: &Utf16JsonProjection,
    ) -> crate::test_support::PermissionDecision {
        crate::test_support::PermissionDecision::Deny {
            reason: IMAGE_REJECTION_REASON.into(),
        }
    }
    async fn resolve_detailed(
        &self,
        _: &str,
        _: &Utf16JsonProjection,
    ) -> crate::test_support::PermissionResolution {
        self.0.fetch_add(1, Ordering::SeqCst);
        crate::test_support::PermissionResolution::Deny {
            reason: IMAGE_REJECTION_REASON.into(),
            source: crate::test_support::PermissionDecisionSource::Unspecified,
            rule_source: None,
            decision_reason_type: None,
            decision_reason: None,
            behavior_ask: true,
            content_blocks: rejection_images(),
        }
    }
}

fn matching_result_content(history: &[ConversationMessage], id: &ToolUseId) -> Vec<ContentBlock> {
    let bundles: Vec<_> = history
        .iter()
        .filter_map(|message| match message {
            ConversationMessage::User { content, .. }
                if content.iter().any(|block| {
                    matches!(
                        block, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == id
                    )
                }) =>
            {
                Some(content.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        bundles.len(),
        1,
        "one logical tool result must occupy one user row"
    );
    bundles.into_iter().next().unwrap()
}

fn assert_rejection_request_projection(messages: Vec<ConversationMessage>, id: &ToolUseId) {
    let result_index = messages
        .iter()
        .position(|message| matches!(message, ConversationMessage::User { content, .. }
            if content.iter().any(|block| matches!(block, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == id))))
        .expect("the raw request contains the denied result");
    let raw_reminder = messages[result_index + 1..]
        .iter()
        .take_while(|message| !matches!(message, ConversationMessage::Assistant { .. }))
        .find_map(|message| match message {
            ConversationMessage::User {
                is_meta: true,
                content,
                ..
            } => match content.as_slice() {
                [ContentBlock::Text { text, .. }]
                    if text.starts_with("<system-reminder>\n<total_tokens>") =>
                {
                    Some(text.clone())
                }
                _ => None,
            },
            _ => None,
        })
        .expect("the denied batch is followed by its persisted token-budget meta row");
    assert_eq!(
        raw_reminder,
        "<system-reminder>\n<total_tokens>15000000 tokens left</total_tokens>\n</system-reminder>",
        "zero-usage mock responses retain the exact default token-budget reminder"
    );
    let messages = llm_runtime::convert::normalize_messages_for_api(messages);
    let projected = llm_runtime::convert::to_llm_messages(messages)
        .expect("production-normalized recorded history must convert");
    let bundles: Vec<_> = projected.iter().filter(|message| message.content.iter().any(|block| matches!(
        block, llm_runtime::ContentBlock::ToolResult { tool_call_id, .. } if tool_call_id == id.as_str()
    ))).collect();
    assert_eq!(bundles.len(), 1);
    let content = &bundles[0].content;
    // Normalization merges the adjacent persisted token-budget meta row into
    // this user message. The raw denial bundle remains exactly three blocks.
    assert_eq!(
        content.len(),
        4,
        "the denied result and image siblings precede the token-budget reminder"
    );
    assert!(
        matches!(&content[0], llm_runtime::ContentBlock::ToolResult { tool_call_id, output, is_error: Some(true), .. }
        if tool_call_id == id.as_str() && output.string_at("").as_deref() == Some(IMAGE_REJECTION_REASON))
    );
    assert!(
        matches!(&content[1], llm_runtime::ContentBlock::Image { media_type, bytes }
        if media_type == "image/png" && bytes.as_slice() == [1, 2, 3])
    );
    assert!(
        matches!(&content[2], llm_runtime::ContentBlock::Image { media_type, bytes }
        if media_type == "image/png" && bytes.as_slice() == [4, 5, 6])
    );
    assert!(
        matches!(&content[3], llm_runtime::ContentBlock::Text { text, .. }
        if text == &raw_reminder),
        "normalization preserves the exact source meta-row text"
    );
}

#[tokio::test]
async fn public_ask_denial_image_siblings_match_batched_hot_cold_and_next_request() {
    let id = ToolUseId::from("ask-image-call");
    let mut expected = vec![ContentBlock::ToolResult {
        tool_use_id: id.clone(),
        content: IMAGE_REJECTION_REASON.into(),
        is_error: Some(true),
        provider_tool_use_id: None,
        content_blocks: None,
    }];
    expected.extend(rejection_images());
    let expected = serde_json::to_value(expected).unwrap();
    for streamed in [false, true] {
        let calls = Arc::new(AtomicUsize::new(0));
        let gate_calls = Arc::new(AtomicUsize::new(0));
        let api = Arc::new(MockApiClient::new(vec![
            mock_message_response(
                vec![llm_runtime::ContentBlock::ToolCall {
                    id: id.as_str().into(),
                    name: "DeferredCounter".into(),
                    input: lingxi_llm_client::protocol::JsonPayload::parse("{}").unwrap(),
                }],
                Some("tool_use"),
            ),
            mock_message_response(
                vec![llm_runtime::ContentBlock::Text {
                    text: "done".into(),
                    citations: None,
                    cache_control: None,
                }],
                Some("end_turn"),
            ),
        ]));
        let streaming = Arc::new(MockStreamingApiClient::with_turns(vec![
            vec![
                message_start("ask-image", "claude-opus-4-7"),
                content_block_start_tool_use(0, id.clone(), "DeferredCounter"),
                input_json_delta(0, "{}"),
                content_block_stop(0),
                message_delta_stop("tool_use"),
                message_stop(),
            ],
            vec![
                message_start("answer", "claude-opus-4-7"),
                content_block_start_text(0),
                text_delta(0, "done"),
                content_block_stop(0),
                message_delta_stop("end_turn"),
                message_stop(),
            ],
        ]));
        let scratch = tempfile::tempdir().unwrap();
        let session_id = SessionId::new();
        let path = scratch
            .path()
            .join(format!("{}.jsonl", session_id.as_uuid()));
        let fs: Arc<dyn lingxi_core::host::FileSystem> = Arc::new(
            platform_posix::fs::PosixFileSystem::new(scratch.path().to_path_buf()),
        );
        let writer = Arc::new(session::JsonlWriter::new(path.clone(), fs.clone()));
        let mut tools = ToolRegistry::new();
        tools.register_builtin(Arc::new(DeferredCounter(calls.clone())));
        let orch = ConversationOrchestrator::into_shared(
            ConversationOrchestrator::new_with_streaming(
                OrchestratorConfig::default(),
                api.clone(),
                streaming.clone(),
                Arc::new(tools),
                noop_hook_executor(),
                Arc::new(AskRejectWithImages(gate_calls.clone())),
                Arc::new(MockOutputStream::new()),
                Arc::new(StaticMemoryProvider::empty()),
                scratch.path().to_path_buf(),
            )
            .with_conversation_mode(lingxi_core::host::session_context::ConversationMode::Code)
            .with_config_home(scratch.path().join("config"))
            .with_session_id(session_id)
            .with_jsonl_writer(writer)
            .without_host_environment_probes(),
        );
        orch.initialize_conversation_mode().await.unwrap();
        let outcome = if streamed {
            orch.run_turn_streaming("inspect the rejected screenshots")
                .await
                .unwrap()
        } else {
            orch.run_turn("inspect the rejected screenshots")
                .await
                .unwrap()
        };
        assert!(matches!(outcome, ConversationOutcome::EndTurn { .. }));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "a permission-denied tool never executes"
        );
        assert_eq!(
            gate_calls.load(Ordering::SeqCst),
            1,
            "the public detailed gate supplied the images"
        );
        let hot = orch.session.lock().await.history.clone();
        assert_eq!(
            serde_json::to_value(matching_result_content(&hot, &id)).unwrap(),
            expected,
            "hot history, streaming={streamed}"
        );
        let next_messages = if streamed {
            let requests = streaming.captured_calls().await;
            assert_eq!(requests.len(), 2);
            assert!(api.captured_msgs().await.is_empty());
            requests[1].messages.clone()
        } else {
            let requests = api.captured_msgs().await;
            assert_eq!(requests.len(), 2);
            assert!(streaming.captured_calls().await.is_empty());
            requests[1].clone()
        };
        assert_eq!(
            serde_json::to_value(matching_result_content(&next_messages, &id)).unwrap(),
            expected,
            "next raw request, streaming={streamed}"
        );
        assert_rejection_request_projection(next_messages, &id);
        let raw = std::fs::read_to_string(&path).unwrap();
        let persisted: Vec<serde_json::Value> = raw
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let results: Vec<_> = persisted
            .iter()
            .filter(|row| {
                row["type"] == "user"
                    && row["message"]["content"].as_array().is_some_and(|blocks| {
                        blocks.iter().any(|block| {
                            block["type"] == "tool_result" && block["tool_use_id"] == id.as_str()
                        })
                    })
            })
            .collect();
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0]["message"]["content"], expected,
            "raw JSONL, streaming={streamed}"
        );
        let cold = crate::resume::replay_session_state_from_path(&path, session_id.as_uuid(), fs)
            .await
            .expect("the fixed-mode UUID transcript must pass the actual cold loader");
        assert_eq!(
            serde_json::to_value(matching_result_content(&cold.state.history, &id)).unwrap(),
            expected,
            "cold history, streaming={streamed}"
        );
        assert_rejection_request_projection(cold.state.history, &id);
    }
}

fn empty_orchestrator() -> Arc<ConversationOrchestrator> {
    ConversationOrchestrator::into_shared(ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::path::PathBuf::from("/tmp"),
    ))
}

fn completed_dispatch_fixture(
    result_count: usize,
    prevent_continuation: bool,
) -> (TrackedTool, CompletedDispatch) {
    let id = ToolUseId::from("dispatch-fixture");
    let assistant_id = MessageId::new();
    let input = Utf16JsonProjection::parse("{}").unwrap();
    let tracked = TrackedTool {
        id: id.clone(),
        name: "Fixture".into(),
        input: input.clone(),
        provider_id: None,
        assistant_id,
        resolved_tool: None,
        status: ToolStatus::Executing,
        is_concurrency_safe: true,
        result: None,
        result_siblings: Vec::new(),
        prevent_continuation: false,
        injected: vec![],
        modifiers: vec![],
        post_tool_batch_calls: vec![],
        publications: vec![],
        refusal_call_boundary: None,
    };
    let completed = CompletedDispatch {
        meta: ReadyToolMeta {
            id: id.clone(),
            presented_name: "Fixture".into(),
            canonical_name: None,
            input,
            provider_id: None,
            assistant_id,
            resolved_tool: None,
            concurrency_safe: true,
            is_agent: false,
        },
        _context_state: None,
        outcome: Ok(ToolDispatchPayload {
            dispatch: crate::turn_loop::DeferredToolDispatch {
                results: (0..result_count)
                    .map(|_| ContentBlock::ToolResult {
                        tool_use_id: id.clone(),
                        content: "unexpected".into(),
                        is_error: None,
                        provider_tool_use_id: None,
                        content_blocks: None,
                    })
                    .collect(),
                prevent_continuation,
                ..Default::default()
            },
            tool_context_state: None,
            context_modifiers: vec![],
        }),
    };
    (tracked, completed)
}

#[tokio::test]
async fn owned_dispatch_ingest_still_rejects_zero_without_stop_and_multiple_results() {
    for (result_count, prevent_continuation) in [(0, false), (2, false), (2, true)] {
        for terminal in [false, true] {
            let orch = empty_orchestrator();
            let mut executor = StreamingToolExecutor::try_new(&orch, vec![], 0)
                .await
                .unwrap();
            let (tracked, completed) =
                completed_dispatch_fixture(result_count, prevent_continuation);
            executor.tools.push(tracked);
            assert_eq!(
                executor
                    .ingest_owned_dispatches_into(vec![completed], terminal)
                    .await,
                1
            );
            let results = if terminal {
                executor.take_all_completed()
            } else {
                executor.take_newly_completed()
            };
            assert_eq!(results.len(), 1);
            assert!(
                matches!(&results[0].block, Some(ContentBlock::ToolResult { content, is_error: Some(true), .. })
                if content.string_at("").is_some_and(|text| text.contains("invalid result count")))
            );
            assert!(!results[0].prevent_continuation);
        }
    }
}

#[tokio::test]
async fn result_sibling_order_survives_ordinary_and_terminal_settlement() {
    for terminal in [false, true] {
        for result_position in 0..=2 {
            let orch = empty_orchestrator();
            let mut executor = StreamingToolExecutor::try_new(&orch, vec![], 0)
                .await
                .unwrap();
            let (tracked, mut completed) = completed_dispatch_fixture(1, false);
            let id = tracked.id.clone();
            let payload = completed.outcome.as_mut().unwrap();
            let result = payload.dispatch.results.pop().unwrap();
            let mut content = rejection_images();
            content.insert(result_position, result);
            let expected = serde_json::to_value(&content).unwrap();
            payload.dispatch.results = content;
            executor.tools.push(tracked);
            assert_eq!(
                executor
                    .ingest_owned_dispatches_into(vec![completed], terminal)
                    .await,
                1
            );
            let results = if terminal {
                executor.take_all_completed()
            } else {
                executor.take_newly_completed()
            };
            assert_eq!(results.len(), 1);
            assert!(
                matches!(&results[0].block, Some(ContentBlock::ToolResult { tool_use_id, .. }) if tool_use_id == &id)
            );
            assert_eq!(results[0].result_siblings.len(), 2);
            let mut settlement = crate::streaming_loop::StreamToolSettlement::default();
            orch.settle_stream_tool_results(
                &mut settlement,
                results,
                &std::collections::HashMap::new(),
                &None,
            )
            .await;
            let accepted: Vec<_> = settlement
                .query_rows
                .into_iter()
                .map(|(row, _)| row)
                .collect();
            assert_eq!(
                serde_json::to_value(matching_result_content(&accepted, &id)).unwrap(),
                expected
            );
        }
    }
}

#[tokio::test]
async fn owned_dispatch_rejects_foreign_result_ids_and_resultless_siblings() {
    for prevent_continuation in [false, true] {
        for foreign_result in [false, true] {
            let orch = empty_orchestrator();
            let mut executor = StreamingToolExecutor::try_new(&orch, vec![], 0)
                .await
                .unwrap();
            let (tracked, mut completed) = completed_dispatch_fixture(0, prevent_continuation);
            completed.outcome.as_mut().unwrap().dispatch.results = if foreign_result {
                vec![ContentBlock::ToolResult {
                    tool_use_id: "other-call".into(),
                    content: "foreign".into(),
                    is_error: None,
                    provider_tool_use_id: None,
                    content_blocks: None,
                }]
            } else {
                rejection_images()
            };
            executor.tools.push(tracked);
            assert_eq!(executor.ingest_owned_dispatches(vec![completed]).await, 1);
            let results = executor.take_newly_completed();
            assert_eq!(results.len(), 1);
            assert!(
                matches!(&results[0].block, Some(ContentBlock::ToolResult { content, is_error: Some(true), .. })
                if content.string_at("").is_some_and(|text| text.contains("invalid result count")))
            );
            assert!(results[0].result_siblings.is_empty());
            assert!(!results[0].prevent_continuation);
        }
    }
}

#[tokio::test]
async fn resultless_stop_yields_once_and_terminal_abandon_does_not_resolve_it() {
    for terminal in [false, true] {
        let orch = empty_orchestrator();
        let mut executor = StreamingToolExecutor::try_new(&orch, vec![], 0)
            .await
            .unwrap();
        let (tracked, completed) = completed_dispatch_fixture(0, true);
        executor.tools.push(tracked);
        assert_eq!(
            executor
                .ingest_owned_dispatches_into(vec![completed], terminal)
                .await,
            1
        );
        let completed = if terminal {
            executor.take_all_completed()
        } else {
            executor.take_newly_completed()
        };
        assert_eq!(completed.len(), 1);
        assert!(completed[0].block.is_none());
        assert!(completed[0].prevent_continuation);
        assert_eq!(executor.tools[0].status, ToolStatus::Yielded);
        assert!(executor.take_newly_completed().is_empty());
        assert!(executor.take_all_completed().is_empty());
        let (unmatched, _) = executor
            .abandon_after_model_error("later model failure")
            .await;
        assert!(
            unmatched.is_empty(),
            "the already-deferred call must not gain a resolving error result"
        );
    }
}
