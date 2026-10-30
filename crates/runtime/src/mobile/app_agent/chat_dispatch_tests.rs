// Included by the service's accepted_projection_tests so these tests exercise
// its real Control, AgentStore, ChatStore and terminal transaction fixture.

#[derive(Debug)]
struct DispatchTestCredentials {
    block_once: AtomicBool,
    entered: Notify,
    release: tokio::sync::Semaphore,
}

impl DispatchTestCredentials {
    fn new(block: bool) -> Arc<Self> {
        Arc::new(Self {
            block_once: AtomicBool::new(block),
            entered: Notify::new(),
            release: tokio::sync::Semaphore::new(0),
        })
    }
}

impl llm_runtime::CredentialProvider for DispatchTestCredentials {
    fn load<'a>(
        &'a self,
        _: &'a llm_runtime::CredentialScope,
    ) -> llm_runtime::BoxFuture<'a, Result<llm_runtime::Credential, llm_runtime::LlmError>> {
        Box::pin(async move {
            if self.block_once.swap(false, Ordering::AcqRel) {
                self.entered.notify_one();
                self.release.acquire().await.unwrap().forget();
            }
            Ok(llm_runtime::Credential::ApiKey("in-memory-test-key".into()))
        })
    }
}

#[derive(Default)]
struct DispatchTestTransport(std::sync::atomic::AtomicUsize);

#[async_trait::async_trait]
impl llm_runtime::Transport for DispatchTestTransport {
    async fn send(
        &self,
        _: llm_runtime::services::sdk::HttpRequest,
    ) -> Result<
        llm_runtime::services::sdk::StreamResponse,
        llm_runtime::services::sdk::protocol::LlmError,
    > {
        self.0.fetch_add(1, Ordering::AcqRel);
        let events = [
            json!({"type":"message_start","message":{"id":"accepted","type":"message","role":"assistant","model":"claude-sonnet-4-6","content":[],"usage":{"input_tokens":1,"output_tokens":0}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"accepted answer"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}),
            json!({"type":"message_stop"}),
        ];
        let bytes = events
            .iter()
            .map(|event| format!("data: {event}\n\n"))
            .collect::<String>()
            .into_bytes();
        Ok(llm_runtime::services::sdk::HttpResponse {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body: bytes.into(),
        }
        .into())
    }
}

fn dispatch_test_api(
    transport: Arc<DispatchTestTransport>,
    credentials: Arc<DispatchTestCredentials>,
) -> Arc<llm_runtime::ApiService> {
    let config = serde_json::from_value(json!({"providers":[{
        "provider_id":"anthropic_first_party","profile_name":"provider","protocol":"anthropic_messages",
        "base_url":"https://control.invalid","auth":"api_key","credential":{"type":"host_managed","id":"fixture"},
        "models":[{"display_model":"model","request_model":"claude-sonnet-4-6","billing_model":"claude-sonnet-4-6",
            "metadata":{"contextWindowTokens":200000,"maxOutputTokens":2048},
            "capabilities":{"streaming":true,"tools":true,"reasoning":false,"vision":false,"documents":false,"structured_output":false}}]
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
            Some(0),
            Some(0),
        )
        .with_interactive_session(false)
        .with_thinking(llm_runtime::model::thinking::ThinkingConfig::Disabled),
    )
}

async fn dispatch_test_orchestrator(
    root: &Path,
    host: &AppAgentHost,
    control: &Arc<Control>,
    api: Arc<llm_runtime::ApiService>,
) -> Arc<ConversationOrchestrator> {
    use orchestrator::test_support::{
        noop_hook_executor, NoOpPermissionGate, StaticMemoryProvider,
    };
    let chat_id = control.chat_id.as_ref().unwrap();
    let session_id = SessionId::from_uuid(
        uuid::Uuid::parse_str(&control.chats.session_id(chat_id).unwrap()).unwrap(),
    );
    let writer = Arc::new(
        session::jsonl::JsonlWriter::open_persistent_chat(
            control.chats.transcript_path(chat_id).unwrap(),
            host.factory.context.fs.clone(),
            None,
        )
        .await
        .unwrap(),
    );
    let adapter = Arc::new(ProviderApiAdapter::new(api.clone()));
    let owner = Arc::downgrade(control);
    let orch = ConversationOrchestrator::new_side_chat(
        OrchestratorConfig {
            model: "model".into(),
            system_prompt_override: Some("Local App test.".into()),
            exclude_dynamic_system_prompt_sections: true,
            ..OrchestratorConfig::default()
        },
        adapter.clone(),
        adapter,
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        control.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        root.to_path_buf(),
    )
    .with_session_id(session_id)
    .with_jsonl_writer(writer)
    .with_persistent_app_chat()
    .with_persistent_chat_context(orchestrator::PersistentChatContextConfig {
        app_id: control.owner.app_id.clone(),
        chat_id: chat_id.clone(),
        source: None,
        client: Arc::new(sidequery::ProviderSideQueryClient::from_service(api)),
        authorized: Arc::new(move || {
            owner
                .upgrade()
                .is_some_and(|control| control.model_request_authorized())
        }),
    })
    .unwrap();
    let orch = ConversationOrchestrator::into_shared(orch);
    *control.chat_orchestrator.lock().unwrap() = Some(orch.clone());
    host.active
        .lock()
        .await
        .insert(control.id.clone(), control.clone());
    orch
}

fn advance_dispatch_test_epoch(control: &Control) {
    let mut changed = control.store.settings(&control.owner).unwrap().unwrap();
    changed.max_turns += 1;
    let change = control
        .store
        .configure_with_change(control.owner.clone(), Some(changed))
        .unwrap();
    assert!(change.changed);
    assert!(change.affected_run_ids.contains(&control.id));
    assert!(!control.store.lease_valid(&control.owner, &control.id));
    assert!(
        !control.cancel.is_cancelled(),
        "the native cancellation signal has not been delivered"
    );
}

fn assert_no_successful_chat_commit(control: &Control) {
    let task = control.store.get(&control.owner, &control.id).unwrap();
    assert!(task.state.terminal());
    assert_ne!(task.state, State::Succeeded);
    assert!(task.result.is_none());
    let events = control
        .store
        .protocol_events(&control.owner, &control.request_id, 0)
        .unwrap();
    assert!(!events
        .events
        .iter()
        .any(|event| event["type"] == "TEXT_MESSAGE_START"
            || event
                .pointer("/outcome/type")
                .is_some_and(|outcome| outcome == "success")));
    assert!(control
        .store
        .committed_protocol_assistants()
        .unwrap()
        .is_empty());
    assert!(
        control
            .store
            .successful_chat_history_task(
                &control.owner.app_id,
                control.chat_id.as_ref().unwrap(),
                &control.id
            )
            .is_err(),
        "persisted bytes alone cannot become a successful recovery frontier"
    );
}

#[tokio::test]
async fn real_mobile_epoch_revocation_during_credentials_sends_nothing_and_cannot_succeed() {
    let (root, _state, control, _user, _chat_id) = setup();
    let (host, _service) = super::progress_tests::test_wake_host(root.path()).await;
    let control = Arc::new(control);
    let transport = Arc::new(DispatchTestTransport::default());
    let credentials = DispatchTestCredentials::new(true);
    let orch = dispatch_test_orchestrator(
        root.path(),
        &host,
        &control,
        dispatch_test_api(transport.clone(), credentials.clone()),
    )
    .await;
    let cancel = control.cancel.clone();
    let run = tokio::spawn(async move {
        orch.run_turn_streaming_with_cancel_projected_content(
            &Utf16JsonProjection::parse(r#"[{"type":"text","text":"hello"}]"#).unwrap(),
            cancel,
            None,
        )
        .await
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        credentials.entered.notified(),
    )
    .await
    .unwrap();
    advance_dispatch_test_epoch(&control);
    credentials.release.add_permits(1);
    let outcome = run.await.unwrap();
    assert!(
        matches!(
            outcome,
            Ok(orchestrator::conversation::TurnOutcome::EndTurn)
        ),
        "the public stream completed by surfacing model_error"
    );
    assert_eq!(transport.0.load(Ordering::Acquire), 0);
    assert!(control.model_failed.load(Ordering::Acquire));
    assert!(!control.cancel.is_cancelled());
    let failure = control.check_model_completion_authority(false).unwrap_err();
    host.finish_execution(&control, Err(failure)).await;
    assert_no_successful_chat_commit(&control);
}

#[tokio::test]
async fn real_mobile_epoch_revocation_after_completion_fence_cannot_commit_a_checkpoint_as_success()
{
    for revoke_before_checkpoint in [false, true] {
        let (root, _state, control, _user, chat_id) = setup();
        let (host, _service) = super::progress_tests::test_wake_host(root.path()).await;
        let control = Arc::new(control);
        let transport = Arc::new(DispatchTestTransport::default());
        let orch = dispatch_test_orchestrator(
            root.path(),
            &host,
            &control,
            dispatch_test_api(transport.clone(), DispatchTestCredentials::new(false)),
        )
        .await;
        let outcome = orch
            .run_turn_streaming_with_cancel_projected_content(
                &Utf16JsonProjection::parse(r#"[{"type":"text","text":"hello"}]"#).unwrap(),
                control.cancel.clone(),
                None,
            )
            .await
            .unwrap();
        assert!(matches!(
            outcome,
            orchestrator::conversation::TurnOutcome::EndTurn
        ));
        assert_eq!(transport.0.load(Ordering::Acquire), 1);
        control.check_model_completion_authority(false).unwrap();
        let history = orch.snapshot_history().await;
        let (message_id, text) = {
            let observed_ids = control.assistant_message_ids.lock().await;
            let display_text = control.assistant_display_text.lock().await;
            accepted_assistant_projection(&history, &HashSet::new(), &*observed_ids, &*display_text)
                .unwrap()
        };
        let checkpoint = orch.verify_persistent_chat_checkpoint(true).await.unwrap();
        if revoke_before_checkpoint {
            advance_dispatch_test_epoch(&control);
        }
        control
            .store
            .record_chat_transcript_checkpoint(
                &control.owner,
                &control.id,
                &chat_id,
                ChatTranscriptCheckpoint {
                    byte_len: checkpoint.checkpoint().byte_len,
                    sha256: checkpoint.checkpoint().sha256.clone(),
                },
            )
            .unwrap();
        if !revoke_before_checkpoint {
            advance_dispatch_test_epoch(&control);
        }
        // Deliberately continue the already accepted post-model path. The real
        // protocol commit must atomically reject success after the epoch changed.
        host.finish_execution(
            &control,
            Ok((
                TaskResult {
                    text,
                    output: None,
                    attachments: Vec::new(),
                    chat_id: Some(chat_id),
                    message_id: Some(message_id),
                },
                json!({}),
            )),
        )
        .await;
        assert_no_successful_chat_commit(&control);
        assert_eq!(
            control
                .store
                .get(&control.owner, &control.id)
                .unwrap()
                .state,
            State::Cancelled
        );
    }
}
