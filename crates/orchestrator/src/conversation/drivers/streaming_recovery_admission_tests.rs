use super::*;
use crate::test_support::{
    mock_message_response, noop_hook_executor, MockOutputStream, NoOpPermissionGate,
    StaticMemoryProvider,
};
use crate::test_support_stream::{
    content_block_start_text, content_block_start_thinking, content_block_stop,
    message_delta_stop_with_usage, message_start_with_usage, message_stop, text_delta,
    thinking_delta, MockStreamingApiClient,
};
use llm_runtime::{ContentBlock as ApiContentBlock, ExecutionUsage, HistoryResponse};
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
};

const MODEL: &str = "claude-opus-4-6";
const PROBE: &str = "RecoveryEffectProbe";
const NOT_RUN: &str =
    "Not run: the response that made this tool call was stopped by a safety classifier.";

struct RecoveryApi {
    responses: tokio::sync::Mutex<VecDeque<HistoryResponse>>,
    requests: tokio::sync::Mutex<Vec<llm_runtime::MessagesCreateRequest>>,
}

impl RecoveryApi {
    fn new(responses: Vec<HistoryResponse>) -> Arc<Self> {
        Arc::new(Self {
            responses: tokio::sync::Mutex::new(responses.into()),
            requests: tokio::sync::Mutex::new(Vec::new()),
        })
    }
}

#[async_trait::async_trait]
impl crate::OrchestratorApiClient for RecoveryApi {
    async fn messages_create(
        &self,
        request: crate::OrchestratorApiRequest,
    ) -> Result<HistoryResponse, LlmError> {
        let crate::OrchestratorApiRequest::Main(request) = request else {
            panic!("recovery tests do not issue hook prompts")
        };
        self.requests.lock().await.push(request);
        self.responses
            .lock()
            .await
            .pop_front()
            .ok_or_else(|| LlmError::Transport {
                message: "recovery response script exhausted".into(),
            })
    }

    fn resolve_media_route(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> Result<llm_runtime::MediaRoute, LlmError> {
        let provider_id = llm_runtime::ProviderId::AnthropicFirstParty;
        Ok(llm_runtime::MediaRoute {
            main: llm_runtime::ResolvedRoute {
                provider_id: provider_id.clone(),
                profile_name: profile.unwrap_or("native-profile").into(),
                request_model: model.into(),
                display_model: model.into(),
                pricing_model: llm_runtime::PricingModelRef {
                    pricing_provider_id: provider_id,
                    billing_model: model.into(),
                    request_model: model.into(),
                    display_model: model.into(),
                },
                capabilities: llm_runtime::Capabilities {
                    streaming: true,
                    tools: true,
                    reasoning: true,
                    ..Default::default()
                },
                connection_chain: Vec::new(),
                failover: Default::default(),
            },
            vision_delegate: None,
        })
    }
}

struct EffectProbe(Arc<AtomicUsize>);

#[async_trait::async_trait]
impl Tool for EffectProbe {
    fn name(&self) -> &str {
        PROBE
    }

    fn input_schema(&self) -> &Utf16JsonProjection {
        static SCHEMA: std::sync::OnceLock<Utf16JsonProjection> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| Utf16JsonProjection::parse(r#"{"type":"object"}"#).unwrap())
    }

    fn input_repair_metadata(&self) -> tool_api::tool_trait::ToolInputRepairMetadata {
        tool_api::tool_trait::ToolInputRepairMetadata::Known {
            schema: tool_api::tool_trait::ToolInputRepairSchema::Advertised,
            schema_provenance: tool_api::tool_trait::ToolInputSchemaProvenance::GeneratedBuiltin,
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

    async fn description(&self, _: &Utf16JsonProjection, _: &DescriptionOptions) -> String {
        "Record an observable effect".into()
    }

    async fn prompt(&self, _: &PromptOptions) -> Utf16JsonProjection {
        "Record an observable effect".into()
    }

    async fn check_permissions(
        &self,
        _: &Utf16JsonProjection,
        _: &tool_api::context::ToolUseContext,
    ) -> permission::PermissionResult {
        permission::PermissionResult::Allow {
            reason: permission::PermissionDecisionReason::Other {
                reason: "recovery admission regression".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: permission::result::PermissionMetadata::default(),
        }
    }

    async fn call(
        &self,
        _: Utf16JsonProjection,
        _: tool_api::context::ToolUseContext,
        _: tool_api::progress::ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ToolCallResult {
            data: tool_api::tool_trait::generated_builtin_result(&serde_json::json!({"ok": true})),
            model_content: None,
            new_messages: Vec::new(),
            context_modifier: None,
            is_error: false,
            subagent_hooks_fired: false,
            mcp_meta: None,
        })
    }
}

fn orchestrator(
    config: crate::OrchestratorConfig,
    api: Arc<RecoveryApi>,
    streaming: Arc<MockStreamingApiClient>,
    effects: Arc<AtomicUsize>,
    output: Arc<MockOutputStream>,
) -> ConversationOrchestrator {
    let mut tools = tool_api::ToolRegistry::new();
    tools.register_builtin(Arc::new(EffectProbe(effects)));
    ConversationOrchestrator::new_with_streaming(
        config,
        api,
        streaming,
        Arc::new(tools),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        output,
        Arc::new(StaticMemoryProvider::empty()),
        std::path::PathBuf::from("/tmp"),
    )
    .without_host_environment_probes()
}

fn config() -> crate::OrchestratorConfig {
    crate::OrchestratorConfig {
        model: MODEL.into(),
        query_source: "sdk".into(),
        max_turns: 2,
        ..Default::default()
    }
}

fn usage(input: u64, output: u64, cache_read: u64, cache_write: u64) -> ExecutionUsage {
    ExecutionUsage {
        report: llm_runtime::UsageReport::measured(
            llm_runtime::Usage {
                input_tokens: input,
                output_tokens: output,
                cache_read_tokens: cache_read,
                cache_write_tokens: cache_write,
                ..Default::default()
            },
            llm_runtime::services::sdk::protocol::UsageState::Complete,
        ),
        ..Default::default()
    }
}

fn answer_events(measured: ExecutionUsage) -> Vec<HistoryEvent> {
    vec![
        message_start_with_usage("replacement", MODEL, measured.clone()),
        content_block_start_text(0),
        text_delta(0, "accepted replacement"),
        content_block_stop(0),
        message_delta_stop_with_usage("end_turn", measured),
        message_stop(),
    ]
}

fn tool_response(stop_reason: &str, include_unknown: bool) -> HistoryResponse {
    let mut blocks = vec![ApiContentBlock::ToolCall {
        id: "recovered-known".into(),
        name: PROBE.into(),
        input: lingxi_llm_client::protocol::JsonPayload::parse("{}").unwrap(),
    }];
    if include_unknown {
        blocks.push(ApiContentBlock::ToolCall {
            id: "recovered-utf16".into(),
            name: PROBE.into(),
            input: lingxi_llm_client::protocol::JsonPayload::parse(r#"{"value":"\ud800"}"#)
                .unwrap(),
        });
        blocks.push(ApiContentBlock::ToolCall {
            id: "recovered-unknown".into(),
            name: "UnknownRecoveryProbe".into(),
            input: lingxi_llm_client::protocol::JsonPayload::parse("{}").unwrap(),
        });
    }
    mock_message_response(blocks, Some(stop_reason))
}

async fn assert_refused_results(orch: &ConversationOrchestrator) {
    let history = orch.session.lock().await.history.clone();
    let results = history
        .iter()
        .flat_map(|message| match message {
            ConversationMessage::User { content, .. } => content.as_slice(),
            _ => &[],
        })
        .filter_map(|block| match block {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } => Some((tool_use_id.as_str(), content.string_at(""))),
            _ => None,
        })
        .collect::<Vec<_>>();
    for id in ["recovered-known", "recovered-utf16", "recovered-unknown"] {
        assert_eq!(
            results.iter().filter(|(found, _)| *found == id).count(),
            1,
            "one refused result per exact identity: {results:?}"
        );
        assert!(
            results.contains(&(id, Some(NOT_RUN))),
            "no unknown-tool or interrupted substitute: {results:?}"
        );
    }
    assert!(history.iter().flat_map(ConversationMessage::tool_calls).any(|block| {
        matches!(block, ContentBlock::ToolUse { id, input, .. } if id.as_str() == "recovered-utf16" && input.string_units("/value") == Some(vec![0xD800]))
    }), "refusal registration retains exact tool input code units");
}

#[tokio::test(start_paused = true)]
async fn ptl_complete_refusal_does_not_enter_known_or_unknown_tool() {
    let effects = Arc::new(AtomicUsize::new(0));
    let api = RecoveryApi::new(vec![tool_response("refusal", true)]);
    let streaming = Arc::new(MockStreamingApiClient::with_open_error(
        LlmError::ContextOverflow { token_gap: 1 },
        vec![answer_events(usage(20, 3, 0, 0))],
    ));
    let orch = ConversationOrchestrator::into_shared(orchestrator(
        config(),
        api,
        streaming.clone(),
        effects.clone(),
        Arc::new(MockOutputStream::new()),
    ));
    orch.run_turn_streaming("recover refusal").await.unwrap();
    assert_eq!(
        effects.load(Ordering::SeqCst),
        0,
        "known complete refusal must never reach Tool::call"
    );
    assert_eq!(
        streaming.captured_calls().await.len(),
        2,
        "same-model refusal retry remains one-shot"
    );
    assert_refused_results(&orch).await;
}

#[tokio::test(start_paused = true)]
async fn nonstream_complete_refusal_does_not_enter_known_or_unknown_tool() {
    let effects = Arc::new(AtomicUsize::new(0));
    let api = RecoveryApi::new(vec![tool_response("refusal", true)]);
    let streaming = Arc::new(MockStreamingApiClient::with_fallible_turns(vec![
        vec![
            Ok(message_start_with_usage(
                "failed",
                MODEL,
                usage(10, 0, 0, 0),
            )),
            Err(LlmError::ProviderInternal),
        ],
        answer_events(usage(20, 3, 0, 0))
            .into_iter()
            .map(Ok)
            .collect(),
    ]));
    let orch = ConversationOrchestrator::into_shared(orchestrator(
        config(),
        api.clone(),
        streaming,
        effects.clone(),
        Arc::new(MockOutputStream::new()),
    ));
    orch.run_turn_streaming("recover refusal").await.unwrap();
    assert_eq!(
        api.requests.lock().await.len(),
        1,
        "failed stream uses one complete fallback response"
    );
    assert_eq!(
        effects.load(Ordering::SeqCst),
        0,
        "complete fallback refusal must never reach Tool::call"
    );
    assert_refused_results(&orch).await;
}

#[tokio::test(start_paused = true)]
async fn auxiliary_refusal_recovery_keeps_normal_tool_dispatch() {
    let effects = Arc::new(AtomicUsize::new(0));
    let api = RecoveryApi::new(vec![tool_response("refusal", false)]);
    let streaming = Arc::new(MockStreamingApiClient::with_open_error(
        LlmError::ContextOverflow { token_gap: 1 },
        vec![answer_events(usage(20, 3, 0, 0))],
    ));
    let mut cfg = config();
    cfg.query_source = "auxiliary".into();
    let orch = ConversationOrchestrator::into_shared(orchestrator(
        cfg,
        api,
        streaming,
        effects.clone(),
        Arc::new(MockOutputStream::new()),
    ));
    orch.run_turn_streaming("recover auxiliary refusal")
        .await
        .unwrap();
    assert_eq!(
        effects.load(Ordering::SeqCst),
        1,
        "Ko=false preserves one ordinary eager call; the probe is physically callable"
    );
}

#[tokio::test(start_paused = true)]
async fn ptl_host_denied_server_model_never_publishes_or_calls_tools() {
    let effects = Arc::new(AtomicUsize::new(0));
    let mut response = tool_response("tool_use", false);
    response.content.insert(
        0,
        ApiContentBlock::Text {
            text: "denied model output".into(),
            cache_control: None,
            citations: None,
        },
    );
    response.provider_metadata = lingxi_llm_client::protocol::JsonPayload::from_generated_value(&serde_json::json!({
        "llm_client": {"server_fallback_events": [{
            "event": {"fromModel":MODEL,"toModel":"claude-sonnet-4-8","reason":"sticky","apiRefusalCategory":null,"midStream":false,"requestId":"denied-recovery","discardedBlocks":[],"retainedBlocks":[],"retainedText":"","finalStopReason":"end_turn"},
            "profile":"server-profile", "lane":{"forModel":MODEL,"model":MODEL,"mode":"explicit"}
        }]}
    })).unwrap();
    let api = RecoveryApi::new(vec![response]);
    let streaming = Arc::new(MockStreamingApiClient::with_open_error(
        LlmError::ContextOverflow { token_gap: 1 },
        vec![],
    ));
    let output = Arc::new(MockOutputStream::new());
    let mut cfg = config();
    cfg.server_fallback_model_enforcement =
        Some(llm_runtime::model::allowlist::ModelEnforcement::Active {
            allowlist: Utf16JsonProjection::array(vec!["opus".into()]).unwrap(),
            overrides: BTreeMap::new(),
        });
    let orch = ConversationOrchestrator::into_shared(orchestrator(
        cfg,
        api,
        streaming,
        effects.clone(),
        output.clone(),
    ));
    orch.run_turn_streaming("recover denied model")
        .await
        .unwrap();
    assert_eq!(
        effects.load(Ordering::SeqCst),
        0,
        "host-denied tools must never reach Tool::call"
    );
    assert_eq!(
        output.text_events().await.join(""),
        lingxi_core::host::refusal_server_control::SERVER_FALLBACK_ALLOWLIST_ERROR
    );
    let session = orch.session.lock().await;
    assert_eq!(session.model, MODEL);
    assert!(
        session
            .history
            .iter()
            .all(|message| message.tool_calls().is_empty()),
        "declined response must not append tool identities"
    );
}

#[tokio::test(start_paused = true)]
async fn active_fallback_ptl_retains_physical_model_and_profile() {
    let api = RecoveryApi::new(vec![mock_message_response(
        vec![ApiContentBlock::Text {
            text: "fallback recovery".into(),
            cache_control: None,
            citations: None,
        }],
        Some("end_turn"),
    )]);
    let streaming = Arc::new(MockStreamingApiClient::with_open_error(
        LlmError::ContextOverflow { token_gap: 1 },
        vec![],
    ));
    let mut cfg = config();
    cfg.fallback_model = Some("fallback-profile/claude-sonnet-4-6".into());
    let orch = ConversationOrchestrator::into_shared(orchestrator(
        cfg,
        api.clone(),
        streaming.clone(),
        Arc::new(AtomicUsize::new(0)),
        Arc::new(MockOutputStream::new()),
    ));
    let user_id = MessageId::new();
    {
        let mut session = orch.session.lock().await;
        session.model_profile = Some("primary-profile".into());
        session.history.push(ConversationMessage::user(
            user_id,
            "resume active fallback".into(),
        ));
    }
    let mut state = TurnLoopState::new(&orch, user_id, None);
    state.selected_route = Some(crate::query_model::ModelRoute {
        model: MODEL.into(),
        profile: Some("primary-profile".into()),
    });
    state.serving_route = Some(crate::query_model::ModelRoute {
        model: "claude-sonnet-4-6".into(),
        profile: Some("fallback-profile".into()),
    });
    state.fallback_index = 1;
    state.turn_count = 1;
    // This is the real round entry after a prior request advanced the
    // query-local serving route. The session deliberately still selects A.
    StreamingTurnDriver::run_round(&orch, &None, &None, &mut state, true)
        .await
        .unwrap();
    let calls = streaming.captured_calls().await;
    assert_eq!(
        (&*calls[0].model, calls[0].profile.as_deref()),
        ("claude-sonnet-4-6", Some("fallback-profile"))
    );
    let requests = api.requests.lock().await;
    assert_eq!(
        (&*requests[0].model, requests[0].profile.as_deref()),
        ("claude-sonnet-4-6", Some("fallback-profile")),
        "PTL retries the route that physically failed"
    );
    assert!(
        requests[0].opts.model_source.is_none(),
        "a query fallback never borrows its parent source"
    );
    assert_eq!(
        orch.session.lock().await.model,
        MODEL,
        "query fallback leaves the selected model intact"
    );
}

#[tokio::test(start_paused = true)]
async fn ptl_preserves_captured_utf16_model_source() {
    let api = RecoveryApi::new(vec![mock_message_response(
        vec![ApiContentBlock::Text {
            text: "recovery".into(),
            cache_control: None,
            citations: None,
        }],
        Some("end_turn"),
    )]);
    let streaming = Arc::new(MockStreamingApiClient::with_open_error(
        LlmError::ContextOverflow { token_gap: 1 },
        vec![],
    ));
    let source = Utf16JsonProjection::root_string(
        "source\u{FFFD}".into(),
        vec![0x73, 0x6F, 0x75, 0x72, 0x63, 0x65, 0xD800],
    )
    .unwrap();
    let orch = orchestrator(
        config(),
        api.clone(),
        streaming,
        Arc::new(AtomicUsize::new(0)),
        Arc::new(MockOutputStream::new()),
    )
    .with_main_model_selection(
        Some("source\u{FFFD}".into()),
        Some(source.clone()),
        lingxi_core::host::ModelProvenance::ManagedAdministratorDefault,
        None,
    )
    .await;
    let orch = ConversationOrchestrator::into_shared(orch);
    orch.run_turn_streaming("source recovery").await.unwrap();
    assert_eq!(
        api.requests.lock().await[0].opts.model_source.as_ref(),
        Some(&source)
    );
}

#[tokio::test(start_paused = true)]
async fn abandoned_measured_usage_and_replacement_are_recorded_once_each() {
    for nonstream in [false, true] {
        let replacement_usage = usage(200, 40, 0, 0);
        let mut response = mock_message_response(
            vec![ApiContentBlock::Text {
                text: "accepted replacement".into(),
                cache_control: None,
                citations: None,
            }],
            Some("end_turn"),
        );
        response.usage = replacement_usage.clone();
        let api = RecoveryApi::new(if nonstream { vec![response] } else { vec![] });
        let mut failed = vec![Ok(message_start_with_usage(
            "failed",
            MODEL,
            usage(100, 0, 20, 30),
        ))];
        if !nonstream {
            failed.extend([
                Ok(content_block_start_thinking(0)),
                Ok(thinking_delta(0, "abandoned reasoning")),
            ]);
        }
        failed.push(Err(if nonstream {
            LlmError::ProviderInternal
        } else {
            LlmError::Transport {
                message: "ECONNRESET: connection reset by peer".into(),
            }
        }));
        let mut turns = vec![failed];
        if !nonstream {
            turns.push(
                answer_events(replacement_usage)
                    .into_iter()
                    .map(Ok)
                    .collect(),
            );
        }
        let streaming = Arc::new(MockStreamingApiClient::with_fallible_turns(turns));
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let tracker = Arc::new(cost::CostTracker::new(
            lingxi_core::types::SessionId::new(),
            Arc::new(cost::PricingCatalog::builtin_reference()),
            tx,
        ));
        let orch = ConversationOrchestrator::into_shared(
            orchestrator(
                config(),
                api.clone(),
                streaming.clone(),
                Arc::new(AtomicUsize::new(0)),
                Arc::new(MockOutputStream::new()),
            )
            .with_cost_tracker(tracker.clone()),
        );
        let origin = orch.session.lock().await.session_id;
        let other_session = lingxi_core::types::SessionId::new();
        tracker.switch_session(other_session).await.unwrap();
        orch.run_turn_streaming("meter retry").await.unwrap();
        let snapshot = tracker.scoped(origin).snapshot().await;
        let recorded = snapshot.per_model_usage.values().next().unwrap();
        assert_eq!(
            (
                recorded.usage.tokens.input,
                recorded.usage.tokens.output,
                recorded.cache_read_input_tokens,
                recorded.cache_creation_input_tokens
            ),
            (300, 40, 20, 30),
            "nonstream={nonstream}: observed failed usage plus complete replacement, no duplicates"
        );
        assert_eq!(
            orch.snapshot_cost_real().await.api_calls,
            2,
            "each measured physical response counted once, nonstream={nonstream}"
        );
        assert_eq!(
            streaming.captured_calls().await.len(),
            if nonstream { 1 } else { 2 }
        );
        assert_eq!(api.requests.lock().await.len(), usize::from(nonstream));
        assert!(
            tracker
                .scoped(other_session)
                .snapshot()
                .await
                .per_model_usage
                .is_empty(),
            "late stream usage remains owned by the captured originating session"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn dropped_attempt_after_synchronous_handoff_keeps_usage_and_call_count() {
    let (tx, _rx) = tokio::sync::mpsc::channel(16);
    let tracker = Arc::new(cost::CostTracker::new(
        lingxi_core::types::SessionId::new(),
        Arc::new(cost::PricingCatalog::builtin_reference()),
        tx,
    ));
    let orch = ConversationOrchestrator::into_shared(
        orchestrator(
            config(),
            RecoveryApi::new(vec![]),
            Arc::new(MockStreamingApiClient::with_turns(vec![])),
            Arc::new(AtomicUsize::new(0)),
            Arc::new(MockOutputStream::new()),
        )
        .with_cost_tracker(tracker.clone()),
    );
    let origin = orch.session.lock().await.session_id;
    let scope = tracker.session_scope(origin);
    let mut response = mock_message_response(vec![], Some("end_turn"));
    response.usage = usage(100, 7, 20, 30);
    let partial = llm_response_to_pumped_turn(&response);
    let handed_off = tokio::sync::Notify::new();
    let mut attempt = Box::pin(async {
        let receipt = StreamingTurnDriver::begin_stream_cost_response(
            &orch,
            Some(&scope),
            MODEL,
            None,
            &partial,
            std::time::Duration::ZERO,
        );
        handed_off.notify_one();
        // Model the first close/settlement suspension after an abandoned
        // attempt handed off measured usage. The receipt must remain owned
        // if this caller is dropped without reaching its settlement await.
        std::future::pending::<()>().await;
        StreamingTurnDriver::settle_stream_cost_response(&orch, receipt).await;
    });
    tokio::select! {
        () = &mut attempt => panic!("the close/settlement seam must stay pending"),
        () = handed_off.notified() => {}
    }
    assert_eq!(
        orch.model_runtime.api_calls_recorded.load(Ordering::SeqCst),
        1,
        "call count transfers synchronously, before any settlement await"
    );
    drop(attempt);
    tracker.drain_owned_settlements().await.unwrap();
    let state = tracker.scoped(origin).snapshot().await;
    assert_eq!(state.cost_revision, 1);
    assert_eq!(
        state
            .per_model_usage
            .values()
            .next()
            .unwrap()
            .usage
            .tokens
            .input,
        100
    );
    assert_eq!(
        orch.snapshot_cost_real().await.api_calls,
        1,
        "receipt Drop neither loses nor duplicates the count"
    );
}

#[tokio::test(start_paused = true)]
async fn absent_failed_usage_does_not_invent_an_extra_measurement() {
    let mut response = mock_message_response(
        vec![ApiContentBlock::Text {
            text: "replacement".into(),
            cache_control: None,
            citations: None,
        }],
        Some("end_turn"),
    );
    response.usage = usage(200, 40, 0, 0);
    let api = RecoveryApi::new(vec![response]);
    let streaming = Arc::new(MockStreamingApiClient::with_fallible_turns(vec![vec![
        Err(LlmError::ProviderInternal),
    ]]));
    let (tx, _rx) = tokio::sync::mpsc::channel(16);
    let tracker = Arc::new(cost::CostTracker::new(
        lingxi_core::types::SessionId::new(),
        Arc::new(cost::PricingCatalog::builtin_reference()),
        tx,
    ));
    let orch = ConversationOrchestrator::into_shared(
        orchestrator(
            config(),
            api,
            streaming,
            Arc::new(AtomicUsize::new(0)),
            Arc::new(MockOutputStream::new()),
        )
        .with_cost_tracker(tracker.clone()),
    );
    let origin = orch.session.lock().await.session_id;
    orch.run_turn_streaming("unmeasured failure").await.unwrap();
    let snapshot = tracker.scoped(origin).snapshot().await;
    assert_eq!(
        snapshot
            .per_model_usage
            .values()
            .next()
            .unwrap()
            .usage
            .tokens
            .input,
        200
    );
    assert_eq!(orch.snapshot_cost_real().await.api_calls, 1);
}
