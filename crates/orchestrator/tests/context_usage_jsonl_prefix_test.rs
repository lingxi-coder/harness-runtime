//! Measured history identity must survive the production Native writer/replay
//! boundary, including exact JavaScript text, rather than just DTO serde.

use lingxi_core::host::{session_context::ConversationMode, FileSystem};
use lingxi_core::types::{utf16_json::Utf16JsonProjection, ContentBlock, ConversationMessage};
use llm_runtime::{ClientConfig, LlmRequest, ModelRuntime, ProtocolFamily};
use orchestrator::test_support::{
    mock_message_response, noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
    StaticMemoryProvider,
};
use orchestrator::test_support_stream::{
    content_block_start_text, content_block_stop, message_delta_stop, message_start, message_stop,
    text_delta, MockStreamingApiClient,
};
use orchestrator::{ConversationOrchestrator, OrchestratorConfig};
use serde_json::json;
use std::sync::Arc;

fn text(message: &ConversationMessage) -> &str {
    let ConversationMessage::User { content, .. } = message else {
        panic!("user row")
    };
    match content.as_slice() {
        [ContentBlock::Text { text, .. }] | [ContentBlock::TextJsUtf16 { text, .. }] => text,
        _ => panic!("one user text block"),
    }
}

fn request(message: &ConversationMessage) -> LlmRequest {
    let messages = llm_runtime::convert::to_llm_messages(vec![message.clone()]).unwrap();
    let (input, exact) = llm_runtime::convert::history_input(
        "m",
        &messages,
        &[],
        &[],
        ProtocolFamily::OpenAiResponses,
    )
    .unwrap();
    let mut request = LlmRequest::new("m").with_profile("prefix-fixture");
    request.input = input;
    request.input.max_tokens = Some(64);
    request.execution.input_protocol = Some(ProtocolFamily::OpenAiResponses);
    request.execution.context_usage_protocol = Some(ProtocolFamily::OpenAiResponses);
    request.execution.message_json_string_overrides = exact;
    request
}

async fn writer_replay_case(streaming: bool, lone_surrogate: bool) {
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path().join("project");
    std::fs::create_dir(&cwd).unwrap();
    let api = Arc::new(MockApiClient::new(vec![mock_message_response(
        vec![llm_runtime::ContentBlock::Text {
            text: "ok".into(),
            cache_control: None,
            citations: None,
        }],
        Some("end_turn"),
    )]));
    let streaming_api = Arc::new(MockStreamingApiClient::with_turns(vec![vec![
        message_start("msg_prefix", "m"),
        content_block_start_text(0),
        text_delta(0, "ok"),
        content_block_stop(0),
        message_delta_stop("end_turn"),
        message_stop(),
    ]]));
    let orch = ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig::default(),
        api,
        streaming_api,
        Arc::new(tool_api::ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        cwd,
    );
    let session_id = orch.session().lock().await.session_id;
    let path = temp.path().join(format!("{}.jsonl", session_id.as_uuid()));
    let fs: Arc<dyn FileSystem> = Arc::new(platform_posix::fs::PosixFileSystem::new(
        temp.path().to_path_buf(),
    ));
    let writer = Arc::new(session::JsonlWriter::new(path.clone(), fs));
    writer
        .initialize_fixed_session_mode(&session_id.as_uuid().to_string(), ConversationMode::Code, 0)
        .await
        .unwrap();
    let orch = ConversationOrchestrator::into_shared(orch.with_jsonl_writer(writer));
    if lone_surrogate {
        let content = Utf16JsonProjection::parse(r#"[{"type":"text","text":"\ud800"}]"#).unwrap();
        orch.run_turn_streaming_with_cancel_projected_content(
            &content,
            tokio_util::sync::CancellationToken::new(),
            None,
        )
        .await
        .unwrap();
    } else if streaming {
        orch.run_turn_streaming("hello").await.unwrap();
    } else {
        orch.run_turn("hello").await.unwrap();
    }
    let expected = if lone_surrogate { "\u{fffd}" } else { "hello" };
    let history = orch.snapshot_history().await;
    assert!(history.iter().rev().find_map(|message| match message {
        ConversationMessage::Assistant { content, .. } => Some(content.iter().any(|block| {
            matches!(block, ContentBlock::Text { text, .. } | ContentBlock::TextJsUtf16 { text, .. } if text == "ok")
        })),
        _ => None,
    }).unwrap_or(false), "the turn must publish the scripted provider response");
    let live = history.iter().find(|message| {
        matches!(message, ConversationMessage::User { content, .. }
            if matches!(content.as_slice(), [ContentBlock::Text { text, .. }] | [ContentBlock::TextJsUtf16 { text, .. }] if text == expected))
    }).expect("actual submitted user row").clone();
    assert!(matches!(&live, ConversationMessage::User { content, .. }
        if matches!(content.as_slice(), [ContentBlock::TextJsUtf16 { .. }])));

    let source = std::fs::read_to_string(&path).unwrap();
    let loaded = session::jsonl::route_lines(&source);
    assert_eq!(loaded.malformed_line_count, 0);
    let row = loaded
        .messages_in_order
        .iter()
        .find(|row| row.uuid == live.id().as_uuid().to_string())
        .unwrap();
    assert_eq!(row.message.string_at("/content/0/type"), Some("text"));
    let cold_state =
        orchestrator::resume::state_from_messages(session_id.as_uuid(), &loaded.messages_in_order);
    let cold = cold_state
        .history
        .iter()
        .find(|message| message.id() == live.id())
        .unwrap()
        .clone();
    let replayed =
        orchestrator::resume::replay_captured_session(path, source, loaded, session_id.as_uuid())
            .unwrap();
    assert_eq!(
        &cold,
        replayed
            .state
            .history
            .iter()
            .find(|message| message.id() == live.id())
            .unwrap()
    );
    assert_eq!(live.id(), cold.id());
    assert_eq!(text(&live), text(&cold));
    if !lone_surrogate {
        assert!(matches!(&cold, ConversationMessage::User { content, .. }
            if matches!(content.as_slice(), [ContentBlock::Text { .. }])));
    }
    let live_request = request(&live);
    let cold_request = request(&cold);
    assert_eq!(live_request.input, cold_request.input);
    assert_eq!(
        live_request.execution.message_json_string_overrides,
        cold_request.execution.message_json_string_overrides
    );
    let config: ClientConfig = serde_json::from_value(json!({"providers":[{
        "provider_id":{"custom":{"name":"prefix-fixture"}},
        "profile_name":"prefix-fixture", "base_url":"https://example.invalid/v1",
        "protocol":"open_ai_responses", "auth":"none", "credential":{"type":"none"},
        "models":[{"display_model":"m", "request_model":"m", "billing_model":"m"}]
    }]}))
    .unwrap();
    let runtime = ModelRuntime::from_config(config).unwrap();
    let live_body = runtime
        .prepare(&live_request)
        .await
        .unwrap()
        .provider_request
        .wire_body_bytes()
        .unwrap();
    let cold_body = runtime
        .prepare(&cold_request)
        .await
        .unwrap()
        .provider_request
        .wire_body_bytes()
        .unwrap();
    assert_eq!(live_body, cold_body, "encoded physical prompt is unchanged");

    let prefix = llm_runtime::context_usage::ContextUsagePrefix::capture(
        &live_request,
        std::slice::from_ref(&live),
    )
    .unwrap();
    let stored: llm_runtime::context_usage::ContextUsagePrefix =
        serde_json::from_slice(&serde_json::to_vec(&prefix).unwrap()).unwrap();
    let serde_control: Vec<ConversationMessage> =
        serde_json::from_slice(&serde_json::to_vec(&vec![live.clone()]).unwrap()).unwrap();
    assert!(stored.matches_history(&serde_control));
    assert!(
        stored.matches_history(std::slice::from_ref(&cold)),
        "real Native JSONL replay keeps the measured anchor"
    );
    let changed = ConversationMessage::user(cold.id(), "actually changed".into());
    assert!(!stored.matches_history(&[changed]));
    if lone_surrogate {
        let mut changed = cold;
        let ConversationMessage::User { content, .. } = &mut changed else {
            unreachable!()
        };
        let ContentBlock::TextJsUtf16 {
            utf16_code_units, ..
        } = &mut content[0]
        else {
            panic!("exact lone units restored")
        };
        utf16_code_units[0] = 0xd801;
        assert_eq!(
            text(&live),
            text(&changed),
            "display-only equality cannot authorize changed exact text"
        );
        assert!(!stored.matches_history(&[changed]));
    }
}

#[test]
fn measured_prefix_survives_real_native_jsonl_for_batched_streamed_and_exact_text() {
    std::thread::Builder::new()
        .stack_size(32 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    writer_replay_case(false, false).await;
                    writer_replay_case(true, false).await;
                    writer_replay_case(true, true).await;
                });
        })
        .unwrap()
        .join()
        .unwrap();
}

// Seed the actual provider-measured durable anchor, then run the real
// orchestrator estimator and either driver. A complete physical override
// makes every logical appended row and the previous output irrelevant.
mod overridden_history {
    use super::*;
    use async_trait::async_trait;
    use futures::StreamExt;
    use lingxi_core::host::{ContextMetadata, ContextPolicy, ModelMetadata};
    use lingxi_core::types::MessageId;
    use llm_runtime::services::sdk;
    use std::sync::Mutex;

    const MODEL: &str = "claude-sonnet-4-20250514";
    const PROFILE: &str = "overridden-history-fixture";
    #[derive(Default)]
    struct PhysicalTransport(Mutex<Vec<(bool, sdk::protocol::JsonPayload)>>);
    #[async_trait]
    impl llm_runtime::Transport for PhysicalTransport {
        async fn send(
            &self,
            request: sdk::HttpRequest,
        ) -> Result<sdk::StreamResponse, sdk::protocol::LlmError> {
            let body =
                sdk::protocol::JsonPayload::parse(std::str::from_utf8(&request.body).unwrap())
                    .unwrap();
            let count = request.url.contains("/count_tokens");
            let stream = body.bool_at("/stream") == Some(true);
            self.0.lock().unwrap().push((count, body));
            let (status, content_type, source) = if count {
                (
                    404,
                    "application/json",
                    json!({"error":{"type":"not_found_error","message":"counter unavailable"}})
                        .to_string(),
                )
            } else if stream {
                let frames = [
                    json!({"type":"message_start","message":{"id":"physical-response","type":"message","role":"assistant","model":MODEL,"content":[],"usage":{"input_tokens":15,"output_tokens":0}}}),
                    json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                    json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}),
                    json!({"type":"content_block_stop","index":0}),
                    json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}),
                    json!({"type":"message_stop"}),
                ];
                (
                    200,
                    "text/event-stream",
                    frames
                        .iter()
                        .map(|frame| {
                            format!(
                                "event: {}\ndata: {frame}\n\n",
                                frame["type"].as_str().unwrap()
                            )
                        })
                        .collect(),
                )
            } else {
                (200, "application/json", json!({"id":"physical-response","type":"message","role":"assistant","model":MODEL,"content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":15,"output_tokens":2}}).to_string())
            };
            Ok(sdk::StreamResponse {
                status,
                headers: vec![("content-type".into(), content_type.into())],
                body: futures::stream::once(async move { Ok(source.into_bytes().into()) }).boxed(),
            })
        }
    }
    struct EnvRestore(Option<std::ffi::OsString>);
    impl Drop for EnvRestore {
        fn drop(&mut self) {
            if let Some(value) = &self.0 {
                std::env::set_var("CLAUDE_CODE_EXTRA_BODY", value)
            } else {
                std::env::remove_var("CLAUDE_CODE_EXTRA_BODY")
            }
        }
    }
    fn api(transport: Arc<PhysicalTransport>) -> Arc<llm_runtime::ApiService> {
        let mut profile = llm_runtime::builtin_presets()
            .providers
            .into_iter()
            .find(|profile| profile.profile_name == "anthropic")
            .unwrap();
        profile.profile_name = PROFILE.into();
        profile.provider_id = llm_runtime::ProviderId::Custom {
            name: PROFILE.into(),
        };
        profile.base_url = "https://example.invalid".into();
        profile.wire_profile = None;
        profile.auth = llm_runtime::AuthStrategy::None;
        profile.credential = llm_runtime::CredentialConfig::None;
        profile.models.truncate(1);
        let model = &mut profile.models[0];
        model.display_model = MODEL.into();
        model.request_model = MODEL.into();
        model.billing_model = MODEL.into();
        model.aliases.clear();
        model.metadata = ModelMetadata {
            context_window_tokens: Some(1_000_000),
            context: Some(ContextMetadata {
                application: Some(ContextPolicy::Custom { input_tokens: 1000 }),
                ..Default::default()
            }),
            ..Default::default()
        };
        Arc::new(
            llm_runtime::ApiService::new(
                Arc::new(
                    ModelRuntime::from_config(ClientConfig {
                        providers: vec![profile],
                    })
                    .unwrap(),
                ),
                transport,
                llm_runtime::SubscriberState::default(),
                llm_runtime::model::user_agent::UserAgentEnv::default(),
                "fixture",
                None,
                None,
            )
            .with_thinking(llm_runtime::model::thinking::ThinkingConfig::Disabled),
        )
    }

    async fn case(streaming: bool) {
        std::env::set_var(
            "CLAUDE_CODE_EXTRA_BODY",
            r#"{"system":[{"type":"text","text":"fixed physical system"}],"messages":[{"role":"user","content":"fixed physical message"}],"tools":[]}"#,
        );
        let transport = Arc::new(PhysicalTransport::default());
        let api = api(transport.clone());
        let logical = vec![ConversationMessage::user(MessageId::new(), "first".into())];
        let mut request = llm_runtime::MessagesCreateRequest::new(
            MODEL,
            Some(PROFILE),
            None,
            logical.clone(),
            vec![],
        );
        request.opts.max_output_tokens = Some(64);
        let response = if streaming {
            api.messages_create_buffered_stream(request).await.unwrap()
        } else {
            api.messages_create(request).await.unwrap()
        };
        assert!(response
            .usage
            .provider_metadata
            .payload_at("/lingxi_context_usage_prefix")
            .is_some());
        let first_wire = transport.0.lock().unwrap().last().unwrap().1.clone();
        let adapter = Arc::new(orchestrator::ProviderApiAdapter::new(api));
        let temp = tempfile::tempdir().unwrap();
        let mut config = OrchestratorConfig::default();
        config.model = MODEL.into();
        let orch =
            ConversationOrchestrator::into_shared(ConversationOrchestrator::new_with_streaming(
                config,
                adapter.clone(),
                adapter,
                Arc::new(tool_api::ToolRegistry::new()),
                noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                Arc::new(MockOutputStream::new()),
                Arc::new(StaticMemoryProvider::empty()),
                temp.path().to_path_buf(),
            ));
        orch.seed_initial_model_profile(MODEL, PROFILE).await;
        let assistant_id = MessageId::new();
        let mut history = logical;
        history.push(ConversationMessage::Assistant {
            id: assistant_id,
            content: vec![ContentBlock::Text {
                text: "ok".into(),
                citations: None,
            }],
            stop_reason: Some("end_turn".into()),
            resumable: None,
            per_turn_effort: None,
        });
        let mut metadata = Utf16JsonProjection::parse("{}").unwrap();
        for (field, source) in [
            (
                "providerMessageId",
                Utf16JsonProjection::from(response.id.as_str()),
            ),
            ("requestedModel", Utf16JsonProjection::from(MODEL)),
            ("requestedProfile", Utf16JsonProjection::from(PROFILE)),
            (
                "usage",
                Utf16JsonProjection::parse(response.usage.provider_metadata.as_json_source())
                    .unwrap(),
            ),
        ] {
            metadata.set_field(field, source).unwrap();
        }
        {
            let session = orch.session();
            let mut session = session.lock().await;
            session.history = history;
            session
                .native_request_history
                .assistant_usage
                .insert(assistant_id, metadata);
        }
        transport.0.lock().unwrap().clear();
        let outcome = if streaming {
            orch.run_turn_streaming(&"x".repeat(8000)).await.unwrap()
        } else {
            orch.run_turn(&"x".repeat(8000)).await.unwrap()
        };
        let final_id = match outcome {
            orchestrator::ConversationOutcome::EndTurn {
                final_message_id, ..
            } => final_message_id,
            other => panic!("expected provider EndTurn: {other:?}"),
        };
        let history = orch.snapshot_history().await;
        let final_row = history
            .iter()
            .find(|message| message.id() == final_id)
            .unwrap();
        assert!(
            matches!(final_row, ConversationMessage::Assistant { content, .. }
            if matches!(content.as_slice(), [ContentBlock::Text { text, .. }] | [ContentBlock::TextJsUtf16 { text, .. }] if text == "ok")),
            "a synthesized error notice must not count as provider completion: {final_row:?}"
        );
        let calls = transport.0.lock().unwrap();
        assert_eq!(calls.len(), 1, "matching physical input reuses its measured prefix without even the unsupported counter");
        assert!(!calls[0].0);
        for field in ["/system", "/messages", "/tools"] {
            assert!(
                first_wire
                    .payload_at(field)
                    .unwrap()
                    .semantically_eq(&calls[0].1.payload_at(field).unwrap())
                    .unwrap(),
                "{field}"
            );
        }
    }

    #[test]
    fn complete_override_uses_physical_anchor_through_both_orchestrator_drivers() {
        std::thread::Builder::new()
            .stack_size(32 * 1024 * 1024)
            .spawn(|| {
                let _restore = EnvRestore(std::env::var_os("CLAUDE_CODE_EXTRA_BODY"));
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(async {
                        case(false).await;
                        case(true).await;
                    });
            })
            .unwrap()
            .join()
            .unwrap();
    }
}
