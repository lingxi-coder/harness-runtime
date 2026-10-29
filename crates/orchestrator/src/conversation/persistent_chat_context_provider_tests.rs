// Included by persistent_chat_context_tests.rs to share the physically owned
// transcript fixture. All credentials and transports here are in-memory fakes;
// request preparation, encoding, streaming and dispatch admission are real.

struct CanonicalBudgetProbe {
    provider: sidequery::ProviderSideQueryClient,
    estimates: std::sync::Mutex<Vec<(sidequery::SideQueryRequest, u64)>>,
}

impl CanonicalBudgetProbe {
    fn new(service: Arc<llm_runtime::ApiService>) -> Arc<Self> {
        Arc::new(Self {
            provider: sidequery::ProviderSideQueryClient::from_service(service),
            estimates: std::sync::Mutex::new(Vec::new()),
        })
    }

    fn clear(&self) {
        self.estimates.lock().unwrap().clear();
    }

    fn assert_compaction_estimates(&self, budget: u64, completed: bool) {
        let estimates = self.estimates.lock().unwrap();
        let threshold = compaction::thresholds::threshold_for_input_budget(budget);
        let main = estimates
            .iter()
            .filter(|(request, _)| request.max_tokens.is_none())
            .collect::<Vec<_>>();
        assert!(main.len() >= if completed { 3 } else { 2 }, "the production path estimates the original/fixed requests, and the final request after a successful summary");
        assert!(
            main[0].1 >= threshold,
            "the actual assembled request must trigger compaction"
        );
        assert!(
            main[1..].iter().all(|(_, tokens)| *tokens < threshold),
            "every actual fixed/final request must fit: {:?}",
            main.iter().map(|(_, tokens)| tokens).collect::<Vec<_>>()
        );
        let summary = estimates
            .iter()
            .filter(|(request, _)| request.max_tokens == Some(SUMMARY_OUTPUT_TOKENS))
            .collect::<Vec<_>>();
        assert_eq!(summary.len(), 1);
        assert!(
            summary[0].1 + u64::from(SUMMARY_OUTPUT_TOKENS) <= budget,
            "the actual summary input plus output reserve must fit"
        );
        assert!(matches!(
            summary[0].0.messages.last(),
            Some(ConversationMessage::User { .. })
        ));
    }
}

#[async_trait]
impl sidequery::SideQueryClient for CanonicalBudgetProbe {
    fn has_canonical_estimator(&self) -> bool {
        sidequery::SideQueryClient::has_canonical_estimator(&self.provider)
    }

    async fn query(
        &self,
        request: sidequery::SideQueryRequest,
    ) -> Result<sidequery::SideQueryResponse, sidequery::SideQueryError> {
        sidequery::SideQueryClient::query(&self.provider, request).await
    }

    async fn query_with_dispatch_admission(
        &self,
        request: sidequery::SideQueryRequest,
        admission: llm_runtime::RequestDispatchAdmission,
    ) -> Result<sidequery::SideQueryResponse, sidequery::SideQueryError> {
        sidequery::SideQueryClient::query_with_dispatch_admission(
            &self.provider,
            request,
            admission,
        )
        .await
    }

    fn estimate_request(
        &self,
        request: sidequery::CanonicalSideQueryRequest,
    ) -> Result<sidequery::SideQueryEstimate, sidequery::SideQueryError> {
        let plain = match &request {
            sidequery::CanonicalSideQueryRequest::Plain(request) => Some(request.clone()),
            _ => None,
        };
        let estimate = sidequery::SideQueryClient::estimate_request(&self.provider, request)?;
        if let Some(request) = plain {
            self.estimates
                .lock()
                .unwrap()
                .push((request, estimate.input_tokens));
        }
        Ok(estimate)
    }
}

fn fixture_budget_request(messages: Vec<ConversationMessage>) -> sidequery::SideQueryRequest {
    sidequery::SideQueryRequest {
        model_attempt: None,
        api_attempts_left: None,
        model_source: None,
        model: "fixture-model".into(),
        profile: None,
        system_prompt: Some("Test Chat.".into()),
        system_prompt_input: None,
        typed_attachment_ids: HashSet::new(),
        task_deliveries: HashMap::new(),
        native_prompt_context: None,
        enable_prompt_caching: false,
        messages,
        tools: Vec::new(),
        tool_choice: None,
        output_format: None,
        max_tokens: None,
        max_retries: 0,
        temperature: None,
        thinking: None,
        effort: None,
        stop_sequences: Vec::new(),
        query_source: sidequery::QuerySource::Compaction,
        skip_system_prompt_prefix: true,
    }
}

fn canonical_fixture_tokens(
    context: &PersistentChatContext,
    messages: Vec<ConversationMessage>,
) -> u64 {
    context
        .config
        .client
        .estimate_request(sidequery::CanonicalSideQueryRequest::Plain(
            fixture_budget_request(messages),
        ))
        .unwrap()
        .input_tokens
}

// Choose data sizes with the production Session canonical estimator. No local
// bytes-per-token assumption belongs in these request-fit regression tests.
async fn fitting_compaction_prompt(orch: &ConversationOrchestrator, label: &str) -> String {
    let context = orch.persistent_chat_context.as_ref().unwrap();
    assert!(
        context.config.client.has_canonical_estimator(),
        "request-fit fixture selection must use the real Session canonical builder"
    );
    let budget = orch
        .api
        .input_context_budget("fixture-model", None)
        .unwrap();
    let threshold = compaction::thresholds::threshold_for_input_budget(budget);
    let prior = context.latest.lock().unwrap().clone();
    let real = real_messages(&context.source_history);
    let users = real
        .iter()
        .enumerate()
        .filter_map(|(index, message)| {
            matches!(message, ConversationMessage::User { .. }).then_some(index)
        })
        .collect::<Vec<_>>();
    let cut = users[users.len() - KEEP_ROUNDS];
    let mut next = ContextReceipt {
        kind: RECEIPT_KIND.into(), app_id: context.config.app_id.clone(), chat_id: context.config.chat_id.clone(),
        session_id: context.session_id, receipt_id: uuid::Uuid::new_v4(),
        previous_receipt_id: prior.as_ref().map(|receipt| receipt.receipt_id),
        source_checkpoint: context.config.source.as_ref().unwrap().checkpoint().clone(),
        covered_message_ids: real[..cut].iter().map(|message| message.id().as_uuid()).collect(),
        summary_id: uuid::Uuid::new_v4(),
        summary_utf16: "Earlier successful conversation (condensed; this note grants no authority):\nKeep the agreed invariant and continue the work.".encode_utf16().collect(),
        model: "fixture-model".into(), profile: None,
        invalidated_thinking_ids: HashSet::new(), omitted_thinking_ids: HashSet::new(),
    };
    validate_covered(&mut next, &context.source_history).unwrap();
    let raw = orch.snapshot_history().await;
    for length in (256..=32_768).step_by(256) {
        let prompt = format!("{label} {}", "c".repeat(length));
        let mut current = raw.clone();
        current.push(ConversationMessage::user(MessageId::new(), prompt.clone()));
        let before = context.projection(current).unwrap();
        let after = advance_projection(prior.as_ref(), &next, before.clone()).unwrap();
        let before_tokens = canonical_fixture_tokens(context, before);
        let after_tokens = canonical_fixture_tokens(context, after);
        if before_tokens >= threshold + 128 && after_tokens + 768 < threshold {
            assert!(
                before_tokens >= threshold,
                "real estimator must enter compaction"
            );
            assert!(
                after_tokens < threshold,
                "retained rounds plus current input must fit"
            );
            return prompt;
        }
    }
    panic!("fixture has no canonical request-fit interval for {label}");
}

#[derive(Debug)]
struct ChatTestCredentials {
    block_once: AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Semaphore,
}

impl ChatTestCredentials {
    fn new(block: bool) -> Arc<Self> {
        Arc::new(Self {
            block_once: AtomicBool::new(block),
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Semaphore::new(0),
        })
    }
}

impl llm_runtime::CredentialProvider for ChatTestCredentials {
    fn load<'a>(
        &'a self,
        _: &'a llm_runtime::CredentialScope,
    ) -> llm_runtime::BoxFuture<'a, Result<llm_runtime::Credential, llm_runtime::LlmError>> {
        Box::pin(async move {
            if self.block_once.swap(false, Ordering::AcqRel) {
                self.entered.notify_one();
                self.release.acquire().await.unwrap().forget();
            }
            Ok(llm_runtime::Credential::ApiKey("in-memory-fixture".into()))
        })
    }
}

struct ChatWireTransport {
    requests: std::sync::Mutex<Vec<serde_json::Value>>,
    fail_first: AtomicBool,
    failure_status: std::sync::atomic::AtomicU16,
    entered: tokio::sync::Notify,
    release: tokio::sync::Semaphore,
}

impl Default for ChatWireTransport {
    fn default() -> Self {
        Self {
            requests: std::sync::Mutex::new(Vec::new()),
            fail_first: AtomicBool::new(false),
            failure_status: std::sync::atomic::AtomicU16::new(503),
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Semaphore::new(0),
        }
    }
}

#[async_trait]
impl llm_runtime::Transport for ChatWireTransport {
    async fn send(
        &self,
        request: lingxi_llm_client::HttpRequest,
    ) -> Result<lingxi_llm_client::StreamResponse, lingxi_llm_client::protocol::LlmError> {
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        let streaming = body["stream"] == true;
        self.requests.lock().unwrap().push(body.clone());
        if self.fail_first.swap(false, Ordering::AcqRel) {
            self.entered.notify_one();
            self.release.acquire().await.unwrap().forget();
            return Ok(lingxi_llm_client::HttpResponse {
                status: self.failure_status.load(Ordering::Acquire),
                headers: vec![],
                body: br#"{"type":"error","error":{"type":"overloaded_error","message":"fixture retry"}}"#.to_vec().into(),
            }.into());
        }
        let (headers, bytes) = if streaming {
            let events = [
                serde_json::json!({"type":"message_start","message":{"id":"main-fresh","type":"message","role":"assistant","model":"claude-sonnet-4-6","content":[],"usage":{"input_tokens":1,"output_tokens":0}}}),
                serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
                serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"fresh projected thinking"}}),
                serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"fresh-projection-signature"}}),
                serde_json::json!({"type":"content_block_stop","index":0}),
                serde_json::json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
                serde_json::json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"new answer"}}),
                serde_json::json!({"type":"content_block_stop","index":1}),
                serde_json::json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}),
                serde_json::json!({"type":"message_stop"}),
            ];
            let sse = events
                .iter()
                .map(|event| format!("data: {event}\n\n"))
                .collect::<String>();
            (
                vec![("content-type".into(), "text/event-stream".into())],
                sse.into_bytes(),
            )
        } else {
            let summary = body.to_string().contains("Write the continuation note");
            let content = if summary {
                serde_json::json!([{"type":"text","text":"Keep the agreed invariant and continue the work."}])
            } else {
                serde_json::json!([
                    {"type":"thinking","thinking":"fresh projected thinking","signature":"fresh-projection-signature"},
                    {"type":"text","text":"new answer"}
                ])
            };
            (vec![], serde_json::json!({
                "id":"fixture-response","type":"message","role":"assistant","model":"claude-sonnet-4-6",
                "content":content,"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":2}
            }).to_string().into_bytes())
        };
        Ok(lingxi_llm_client::HttpResponse {
            status: 200,
            headers,
            body: bytes.into(),
        }
        .into())
    }
}

fn chat_test_service(
    transport: Arc<ChatWireTransport>,
    credentials: Arc<ChatTestCredentials>,
    retries: u32,
) -> Arc<llm_runtime::ApiService> {
    let config = serde_json::from_value(serde_json::json!({"providers":[{
        "provider_id":"anthropic_first_party","profile_name":"chat-test","protocol":"anthropic_messages",
        "base_url":"https://chat.invalid","auth":"api_key","credential":{"type":"host_managed","id":"fixture"},
        "models":[
            {"display_model":"fixture-model","request_model":"claude-sonnet-4-6","billing_model":"claude-sonnet-4-6","aliases":["claude-sonnet-4-6"],
             "metadata":{"contextWindowTokens":200000,"maxOutputTokens":4096},
             "capabilities":{"streaming":true,"tools":true,"reasoning":true,"vision":false,"documents":false,"structured_output":false}},
            {"display_model":"fixture-fallback","request_model":"claude-sonnet-4-5","billing_model":"claude-sonnet-4-5",
             "metadata":{"contextWindowTokens":200000,"maxOutputTokens":4096},
             "capabilities":{"streaming":true,"tools":true,"reasoning":true,"vision":false,"documents":false,"structured_output":false}}
        ]
    }]})).unwrap();
    Arc::new(
        llm_runtime::ApiService::new_with_routing(
            Arc::new(
                llm_runtime::ModelRuntime::from_config(config)
                    .unwrap()
                    .with_credential_provider(credentials),
            ),
            transport,
            Default::default(),
            Default::default(),
            "test",
            None,
            None,
            None,
            Default::default(),
            Some(retries),
            Some(0),
        )
        .with_interactive_session(false)
        .with_thinking(llm_runtime::model::thinking::ThinkingConfig::Enabled {
            budget_tokens: 1024,
        }),
    )
}

struct ChatProviderBudgetApi {
    adapter: Arc<crate::provider_adapter::ProviderApiAdapter>,
    budget: u64,
}

#[async_trait]
impl OrchestratorApiClient for ChatProviderBudgetApi {
    async fn messages_create(
        &self,
        request: OrchestratorApiRequest,
    ) -> Result<llm_runtime::HistoryResponse, llm_runtime::LlmError> {
        self.adapter.messages_create(request).await
    }
    fn input_context_budget(&self, _: &str, _: Option<&str>) -> Option<u64> {
        Some(self.budget)
    }
    fn resolve_media_route(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> Result<llm_runtime::MediaRoute, llm_runtime::LlmError> {
        self.adapter.resolve_media_route(model, profile)
    }
    fn thinking_stripped_messages(&self) -> HashMap<MessageId, usize> {
        OrchestratorApiClient::thinking_stripped_messages(self.adapter.as_ref())
    }
    fn set_thinking_stripped_messages(&self, messages: HashMap<MessageId, usize>) {
        OrchestratorApiClient::set_thinking_stripped_messages(self.adapter.as_ref(), messages);
    }
}

async fn real_chat_orchestrator(
    root: &Path,
    writer: Arc<JsonlWriter>,
    source: CapturedChatTranscript,
    service: Arc<llm_runtime::ApiService>,
    allowed: Arc<AtomicBool>,
    budget: u64,
) -> Arc<ConversationOrchestrator> {
    real_chat_orchestrator_with_output(
        root,
        writer,
        source,
        service,
        allowed,
        budget,
        Arc::new(MockOutputStream::new()),
    )
    .await
}

async fn real_chat_orchestrator_with_output(
    root: &Path,
    writer: Arc<JsonlWriter>,
    source: CapturedChatTranscript,
    service: Arc<llm_runtime::ApiService>,
    allowed: Arc<AtomicBool>,
    budget: u64,
    output: Arc<MockOutputStream>,
) -> Arc<ConversationOrchestrator> {
    let client = Arc::new(sidequery::ProviderSideQueryClient::from_service(
        service.clone(),
    ));
    real_chat_orchestrator_with_client(
        root, writer, source, service, allowed, budget, output, client,
    )
    .await
}

async fn real_chat_orchestrator_with_client(
    root: &Path,
    writer: Arc<JsonlWriter>,
    source: CapturedChatTranscript,
    service: Arc<llm_runtime::ApiService>,
    allowed: Arc<AtomicBool>,
    budget: u64,
    output: Arc<MockOutputStream>,
    client: Arc<dyn sidequery::SideQueryClient>,
) -> Arc<ConversationOrchestrator> {
    let adapter = Arc::new(crate::provider_adapter::ProviderApiAdapter::new(
        service.clone(),
    ));
    let (app_id, chat_id) = owner_from_path(source.path()).unwrap();
    let session_id = SessionId::from_uuid(source.session_id());
    let orch = ConversationOrchestrator::new_side_chat(
        OrchestratorConfig {
            model: "fixture-model".into(),
            system_prompt_override: Some("Test Chat.".into()),
            exclude_dynamic_system_prompt_sections: true,
            ..OrchestratorConfig::default()
        },
        Arc::new(ChatProviderBudgetApi {
            adapter: adapter.clone(),
            budget,
        }),
        adapter,
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        output,
        Arc::new(StaticMemoryProvider::empty()),
        root.to_path_buf(),
    )
    .with_session_id(session_id)
    .with_jsonl_writer(writer)
    .with_persistent_chat_context(PersistentChatContextConfig {
        app_id,
        chat_id,
        source: Some(source.clone()),
        client,
        authorized: Arc::new(move || allowed.load(Ordering::Acquire)),
    })
    .unwrap();
    let replayed = ConversationOrchestrator::replay_persistent_chat_snapshot(&source).unwrap();
    let runtime = replayed.handle_runtime_snapshot();
    orch.resume_session(
        session_id,
        replayed.state.history,
        replayed.last_message_uuid.map(|id| id.to_string()),
        None,
        runtime,
    )
    .await
    .unwrap();
    Arc::new(orch)
}

fn has_historical_thinking(history: &[ConversationMessage]) -> bool {
    history.iter().any(|message| match message {
        ConversationMessage::Assistant { content, .. } => content.iter().any(|block| matches!(block,
            ContentBlock::Thinking { signature: Some(signature), .. } if signature.starts_with("old-prefix-")))
            || content.iter().any(|block| matches!(block, ContentBlock::RedactedThinking { .. })),
        _ => false,
    })
}

#[tokio::test]
async fn provider_summary_and_main_preserve_canonical_thinking_and_cold_checkpoint() {
    for streaming in [false, true] {
        let ChatFixture {
            _root,
            path,
            fs,
            source,
            orch,
            writer,
            allowed,
            ..
        } = fixture_seed(false, Some(8000), true).await;
        drop(orch);
        let transport = Arc::new(ChatWireTransport::default());
        let service = chat_test_service(transport.clone(), ChatTestCredentials::new(false), 0);
        let probe = CanonicalBudgetProbe::new(service.clone());
        let orch = real_chat_orchestrator_with_client(
            _root.path(),
            writer.clone(),
            source.clone(),
            service.clone(),
            allowed.clone(),
            8000,
            Arc::new(MockOutputStream::new()),
            probe.clone(),
        )
        .await;
        let original = orch.snapshot_history().await;
        assert!(has_historical_thinking(&original));
        let prompt = fitting_compaction_prompt(&orch, "current-only").await;
        probe.clear();
        if streaming {
            orch.run_turn_streaming_with_cancel(prompt, CancellationToken::new())
                .await
                .unwrap();
        } else {
            orch.run_turn(&prompt).await.unwrap();
        }
        probe.assert_compaction_estimates(8000, true);
        let requests = transport.requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 2, "summary and main each dispatch once");
        let summary_rows = requests[0]["messages"].as_array().unwrap();
        assert_eq!(summary_rows.last().unwrap()["role"], "user");
        assert!(summary_rows
            .last()
            .unwrap()
            .to_string()
            .contains("Write the continuation note"));
        assert!(!requests[0].to_string().contains("current-only"));
        assert!(!requests[0].to_string().contains("old-prefix-signature"));
        assert!(!requests[1].to_string().contains("old-prefix-signature"));
        assert!(!requests[1].to_string().contains("old-redacted"));
        assert!(requests[1]
            .to_string()
            .contains("Earlier successful conversation"));
        assert!(requests[1]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row.to_string().contains("Earlier successful conversation"))
            .all(|row| row["role"] == "user"));
        let canonical = orch.snapshot_history().await;
        assert_eq!(&canonical[..original.len()], original.as_slice());
        assert!(has_historical_thinking(&canonical));
        let checkpoint = orch.verify_persistent_chat_checkpoint(true).await.unwrap();
        assert_eq!(
            ConversationOrchestrator::replay_persistent_chat_snapshot(&checkpoint)
                .unwrap()
                .display_history,
            canonical
        );
        drop(orch);
        drop(writer);
        let target = path
            .parent()
            .unwrap()
            .join(uuid::Uuid::new_v4().to_string())
            .join(path.file_name().unwrap());
        let recovered = JsonlWriter::stage_persistent_chat_recovery(
            path,
            target.clone(),
            Some(checkpoint.checkpoint().clone()),
        )
        .await
        .unwrap()
        .unwrap();
        let writer = Arc::new(
            JsonlWriter::open_persistent_chat(target, fs, Some(recovered.checkpoint().clone()))
                .await
                .unwrap(),
        );
        let restored =
            real_chat_orchestrator(_root.path(), writer, recovered, service, allowed, 8000).await;
        let raw = restored.snapshot_history().await;
        let prepared = restored
            .apply_persistent_chat_projection(PreparedModelCall {
                history_snapshot: raw.clone(),
                model: "fixture-model".into(),
                model_profile: None,
                outgoing_history_rewriter: None,
            })
            .await
            .unwrap();
        for _ in 0..3 {
            let retry = restored
                .rewrite_outgoing_history(raw.clone(), prepared.outgoing_history_rewriter.as_ref())
                .await
                .unwrap();
            assert_eq!(retry, prepared.history_snapshot);
            assert!(!has_historical_thinking(&retry));
            assert!(retry.iter().any(|message| match message {
                ConversationMessage::Assistant { content, .. } => content.iter().any(|block| matches!(block,
                    ContentBlock::Thinking { signature: Some(signature), .. } if signature == "fresh-projection-signature")),
                _ => false,
            }), "thinking generated after the receipt is still valid");
        }
        restored
            .verify_persistent_chat_checkpoint(true)
            .await
            .unwrap();
        assert_eq!(
            transport.requests.lock().unwrap().len(),
            2,
            "cold replay never dispatches a model"
        );
    }
}

#[tokio::test]
async fn two_real_compactions_cover_an_omitted_thinking_only_assistant_after_cold_recovery() {
    let ChatFixture {
        _root,
        path,
        fs,
        source,
        orch,
        writer,
        allowed,
        ..
    } = fixture_seed_variant(false, Some(8000), true, true).await;
    drop(orch);
    let thinking_only_id = ConversationOrchestrator::replay_persistent_chat_snapshot(&source)
        .unwrap().state.history.into_iter().find(|message| matches!(message,
            ConversationMessage::Assistant { content, .. } if content.iter().all(|block|
                matches!(block, ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. }))))
        .unwrap().id().as_uuid();
    let chat_directory = path.parent().unwrap().to_path_buf();
    let transport = Arc::new(ChatWireTransport::default());
    let service = chat_test_service(transport.clone(), ChatTestCredentials::new(false), 0);
    let probe = CanonicalBudgetProbe::new(service.clone());
    let mut writer = writer;
    let mut orch = real_chat_orchestrator_with_client(
        _root.path(),
        writer.clone(),
        source,
        service.clone(),
        allowed.clone(),
        8000,
        Arc::new(MockOutputStream::new()),
        probe.clone(),
    )
    .await;
    let mut previous_receipt = None;
    for index in 0..2 {
        let canonical_before = orch.snapshot_history().await;
        let prompt = fitting_compaction_prompt(&orch, &format!("current-only-{index}")).await;
        probe.clear();
        orch.run_turn_streaming_with_cancel(prompt, CancellationToken::new())
            .await
            .unwrap();
        probe.assert_compaction_estimates(8000, true);
        let canonical_after = orch.snapshot_history().await;
        assert_eq!(
            &canonical_after[..canonical_before.len()],
            canonical_before.as_slice()
        );
        assert!(canonical_after
            .iter()
            .any(|message| message.id().as_uuid() == thinking_only_id));
        let checkpoint = orch.verify_persistent_chat_checkpoint(true).await.unwrap();
        let receipt = replay_context_receipts(&checkpoint).unwrap().unwrap();
        assert_eq!(receipt.previous_receipt_id, previous_receipt);
        if index == 0 {
            assert!(receipt.omitted_thinking_ids.contains(&thinking_only_id));
            assert!(!receipt.covered_message_ids.contains(&thinking_only_id));
        } else {
            assert!(receipt.covered_message_ids.contains(&thinking_only_id),
                "the second receipt proves the previously omitted canonical assistant through its source checkpoint");
        }
        previous_receipt = Some(receipt.receipt_id);
        let projected = orch
            .persistent_chat_context
            .as_ref()
            .unwrap()
            .projection(canonical_after.clone())
            .unwrap();
        assert!(!projected
            .iter()
            .any(|message| message.id().as_uuid() == thinking_only_id));
        assert_eq!(
            ConversationOrchestrator::replay_persistent_chat_snapshot(&checkpoint)
                .unwrap()
                .display_history,
            canonical_after
        );
        drop(orch);
        drop(writer);
        let target = chat_directory
            .join(uuid::Uuid::new_v4().to_string())
            .join(path.file_name().unwrap());
        let recovered = JsonlWriter::stage_persistent_chat_recovery(
            checkpoint.path().to_path_buf(),
            target.clone(),
            Some(checkpoint.checkpoint().clone()),
        )
        .await
        .unwrap()
        .unwrap();
        writer = Arc::new(
            JsonlWriter::open_persistent_chat(
                target,
                fs.clone(),
                Some(recovered.checkpoint().clone()),
            )
            .await
            .unwrap(),
        );
        orch = real_chat_orchestrator_with_client(
            _root.path(),
            writer.clone(),
            recovered,
            service.clone(),
            allowed.clone(),
            8000,
            Arc::new(MockOutputStream::new()),
            probe.clone(),
        )
        .await;
        let prepared = orch
            .apply_persistent_chat_projection(PreparedModelCall {
                history_snapshot: orch.snapshot_history().await,
                model: "fixture-model".into(),
                model_profile: None,
                outgoing_history_rewriter: None,
            })
            .await
            .unwrap();
        assert_eq!(
            prepared.history_snapshot, projected,
            "cold replay deterministically derives the same omitted identities"
        );
        assert_eq!(
            orch.rewrite_outgoing_history(
                canonical_after,
                prepared.outgoing_history_rewriter.as_ref()
            )
            .await
            .unwrap(),
            projected
        );
        orch.verify_persistent_chat_checkpoint(false).await.unwrap();
        assert_eq!(transport.requests.lock().unwrap().len(), 2 * (index + 1));
    }
}

#[tokio::test]
async fn actual_summary_and_stream_recheck_live_owner_after_credentials_await() {
    for summary in [true, false] {
        let budget = if summary { 8000 } else { 100_000 };
        let ChatFixture {
            _root,
            source,
            orch,
            writer,
            allowed,
            path,
            ..
        } = fixture(false, Some(budget)).await;
        drop(orch);
        let transport = Arc::new(ChatWireTransport::default());
        let credentials = ChatTestCredentials::new(true);
        let service = chat_test_service(transport.clone(), credentials.clone(), 0);
        let output = Arc::new(MockOutputStream::new());
        let probe = CanonicalBudgetProbe::new(service.clone());
        let orch = real_chat_orchestrator_with_client(
            _root.path(),
            writer,
            source,
            service,
            allowed.clone(),
            budget,
            output.clone(),
            probe.clone(),
        )
        .await;
        let prompt = if summary {
            fitting_compaction_prompt(&orch, "current-only").await
        } else {
            format!("current-only {}", "c".repeat(6000))
        };
        probe.clear();
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        let worker = orch.clone();
        let run =
            tokio::spawn(async move { worker.run_turn_streaming_with_cancel(prompt, token).await });
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            credentials.entered.notified(),
        )
        .await
        .unwrap();
        if summary {
            probe.assert_compaction_estimates(budget, false);
        } else {
            let estimates = probe.estimates.lock().unwrap();
            assert!(!estimates.is_empty());
            assert!(
                estimates
                    .iter()
                    .all(|(request, tokens)| request.max_tokens.is_none()
                        && *tokens < compaction::thresholds::threshold_for_input_budget(budget)),
                "the ordinary stream fixture must reach credentials without needing compaction"
            );
        }
        allowed.store(false, Ordering::Release);
        assert!(
            !cancel.is_cancelled(),
            "lease revocation does not depend on cancellation delivery"
        );
        credentials.release.add_permits(1);
        let outcome = run.await.unwrap();
        if summary {
            assert!(
                outcome.is_err(),
                "summary preparation rejects before the main turn"
            );
        } else {
            assert!(matches!(
                outcome,
                Ok(crate::conversation::TurnOutcome::EndTurn)
            ));
            assert!(output.snapshot().await.iter().any(|event| matches!(event,
                lingxi_core::host::OutputEvent::EndTurn { stop_reason, .. } if stop_reason == "model_error")),
                "public completion reports a model error, which the Mobile Control rejects");
        }
        assert!(transport.requests.lock().unwrap().is_empty());
        assert!(!std::fs::read_to_string(path)
            .unwrap()
            .contains(RECEIPT_KIND));
    }
}

struct ChatTestHookGuard(Arc<AtomicBool>);
impl hooks::attachment::HookPublicationGuard for ChatTestHookGuard {
    fn is_current(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

#[tokio::test]
async fn combined_chat_hook_and_cancel_fences_guard_real_provider_dispatch() {
    for streaming in [false, true] {
        for rejected in ["chat", "hook", "cancel"] {
            let fixture = fixture(false, Some(100_000)).await;
            let transport = Arc::new(ChatWireTransport::default());
            let credentials = ChatTestCredentials::new(true);
            let service = chat_test_service(transport.clone(), credentials.clone(), 0);
            let adapter = Arc::new(crate::provider_adapter::ProviderApiAdapter::new(service));
            let hook_current = Arc::new(AtomicBool::new(true));
            let message =
                ConversationMessage::user_meta(MessageId::new(), "accepted hook content".into());
            let guards: Vec<(MessageId, Arc<dyn hooks::attachment::HookPublicationGuard>)> =
                vec![(
                    message.id(),
                    Arc::new(ChatTestHookGuard(hook_current.clone())),
                )];
            let cancel = CancellationToken::new();
            let messages = vec![
                ConversationMessage::user(MessageId::new(), "current".into()),
                message,
            ];
            let admission =
                fixture
                    .orch
                    .main_request_dispatch_admission(&messages, &guards, Some(&cancel));
            let run = tokio::spawn(async move {
                if streaming {
                    adapter
                        .stream(
                            "fixture-model",
                            None,
                            None,
                            messages,
                            Vec::new(),
                            "sdk",
                            false,
                            admission,
                        )
                        .await
                        .map(|_| ())
                } else {
                    let mut request = llm_runtime::MessagesCreateRequest::new(
                        "fixture-model",
                        None,
                        None,
                        messages,
                        Vec::new(),
                    );
                    request.opts.request_dispatch_admission = admission;
                    adapter
                        .messages_create(OrchestratorApiRequest::Main(request))
                        .await
                        .map(|_| ())
                }
            });
            tokio::time::timeout(
                std::time::Duration::from_secs(2),
                credentials.entered.notified(),
            )
            .await
            .unwrap();
            match rejected {
                "chat" => fixture.allowed.store(false, Ordering::Release),
                "hook" => hook_current.store(false, Ordering::Release),
                _ => cancel.cancel(),
            }
            credentials.release.add_permits(1);
            assert!(matches!(
                run.await.unwrap(),
                Err(llm_runtime::LlmError::RequestDispatchRejected {
                    prior_dispatch: false
                })
            ));
            assert!(
                transport.requests.lock().unwrap().is_empty(),
                "{rejected}/{streaming} must send zero requests"
            );
        }
    }
}

#[tokio::test]
async fn actual_stream_retry_rechecks_authority_after_a_blocked_transport() {
    let ChatFixture {
        _root,
        source,
        orch,
        writer,
        allowed,
        ..
    } = fixture(false, Some(100_000)).await;
    drop(orch);
    let transport = Arc::new(ChatWireTransport::default());
    transport.fail_first.store(true, Ordering::Release);
    let service = chat_test_service(transport.clone(), ChatTestCredentials::new(false), 1);
    let output = Arc::new(MockOutputStream::new());
    let orch = real_chat_orchestrator_with_output(
        _root.path(),
        writer,
        source,
        service,
        allowed.clone(),
        100_000,
        output.clone(),
    )
    .await;
    let run = tokio::spawn(async move {
        orch.run_turn_streaming_with_cancel("current", CancellationToken::new())
            .await
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        transport.entered.notified(),
    )
    .await
    .unwrap();
    allowed.store(false, Ordering::Release);
    transport.release.add_permits(1);
    assert!(matches!(
        run.await.unwrap(),
        Ok(crate::conversation::TurnOutcome::EndTurn)
    ));
    assert!(output.snapshot().await.iter().any(|event| matches!(event,
        lingxi_core::host::OutputEvent::EndTurn { stop_reason, .. } if stop_reason == "model_error")));
    assert_eq!(
        transport.requests.lock().unwrap().len(),
        1,
        "revoked retries must never dispatch"
    );
}

#[tokio::test]
async fn fallback_keeps_chat_admission_and_an_allowed_control_reaches_the_fallback_model() {
    for revoke in [false, true] {
        let fixture = fixture(false, Some(100_000)).await;
        let transport = Arc::new(ChatWireTransport::default());
        transport.fail_first.store(true, Ordering::Release);
        transport.failure_status.store(529, Ordering::Release);
        let service = chat_test_service(transport.clone(), ChatTestCredentials::new(false), 0);
        let adapter = Arc::new(crate::provider_adapter::ProviderApiAdapter::new(service));
        let messages = vec![ConversationMessage::user(
            MessageId::new(),
            "current".into(),
        )];
        let mut request = llm_runtime::MessagesCreateRequest::new(
            "fixture-model",
            None,
            None,
            messages,
            Vec::new(),
        );
        request.opts.initial_consecutive_overloaded = Some(2);
        request.opts.fallback =
            llm_runtime::FallbackPolicy::Models(vec!["fixture-fallback".into()]);
        request.opts.request_dispatch_admission =
            fixture
                .orch
                .main_request_dispatch_admission(&request.messages, &[], None);
        let run = tokio::spawn(async move {
            adapter
                .messages_create(OrchestratorApiRequest::Main(request))
                .await
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            transport.entered.notified(),
        )
        .await
        .unwrap();
        if revoke {
            fixture.allowed.store(false, Ordering::Release);
        }
        transport.release.add_permits(1);
        let result = run.await.unwrap();
        let requests = transport.requests.lock().unwrap();
        if revoke {
            assert!(matches!(
                result,
                Err(llm_runtime::LlmError::RequestDispatchRejected {
                    prior_dispatch: true
                })
            ));
            assert_eq!(
                requests.len(),
                1,
                "fallback cannot send after owner revocation"
            );
        } else {
            result.unwrap();
            assert_eq!(requests.len(), 2);
            assert_eq!(
                requests[1]["model"], "claude-sonnet-4-5",
                "the control must exercise an actual model fallback"
            );
        }
    }
}
