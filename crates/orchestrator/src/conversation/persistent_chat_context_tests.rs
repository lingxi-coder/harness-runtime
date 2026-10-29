use super::*;
use crate::test_support::{
    mock_message_response, noop_hook_executor, MockApiClient, MockOutputStream,
    MockStreamingApiClient, NoOpPermissionGate, StaticMemoryProvider,
};
use lingxi_core::host::{FileSystem, OrchestratorHandle};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

struct BudgetApi {
    budget: Option<u64>,
    main: Arc<MockApiClient>,
}

#[async_trait]
impl OrchestratorApiClient for BudgetApi {
    async fn messages_create(
        &self,
        request: OrchestratorApiRequest,
    ) -> Result<llm_runtime::HistoryResponse, llm_runtime::LlmError> {
        self.main.messages_create(request).await
    }

    fn input_context_budget(&self, _: &str, _: Option<&str>) -> Option<u64> {
        self.budget
    }
}

struct SummaryClient {
    calls: tokio::sync::Mutex<Vec<sidequery::SideQueryRequest>>,
    allowed: Arc<AtomicBool>,
    revoke_in_response: AtomicBool,
    emit_tool_call: AtomicBool,
}

#[async_trait]
impl sidequery::SideQueryClient for SummaryClient {
    async fn query_with_dispatch_admission(
        &self,
        request: sidequery::SideQueryRequest,
        admission: llm_runtime::RequestDispatchAdmission,
    ) -> Result<sidequery::SideQueryResponse, sidequery::SideQueryError> {
        assert!(admission.is_admitted());
        self.query(request).await
    }
    async fn query(
        &self,
        request: sidequery::SideQueryRequest,
    ) -> Result<sidequery::SideQueryResponse, sidequery::SideQueryError> {
        assert!(request.tools.is_empty());
        assert!(request.task_deliveries.is_empty());
        self.calls.lock().await.push(request);
        if self.revoke_in_response.load(Ordering::Acquire) {
            self.allowed.store(false, Ordering::Release);
        }
        Ok(sidequery::SideQueryResponse {
            text: Some(
                "Keep the agreed invariant; the current request was not part of this note.".into(),
            ),
            structured: None,
            tool_calls: if self.emit_tool_call.load(Ordering::Acquire) {
                vec![Utf16JsonProjection::parse(
                    "{\"name\":\"Shell\",\"input\":{\"command\":\"touch forbidden\"}}",
                )
                .unwrap()]
            } else {
                Vec::new()
            },
            usage: cost::Usage::default(),
            stop_reason: Some("end_turn".into()),
            retry_count: 0,
        })
    }

    fn estimate_request(
        &self,
        request: sidequery::CanonicalSideQueryRequest,
    ) -> Result<sidequery::SideQueryEstimate, sidequery::SideQueryError> {
        let sidequery::CanonicalSideQueryRequest::Plain(request) = request else {
            panic!("unexpected structured request")
        };
        let bytes = request
            .messages
            .iter()
            .map(|message| message.text_content().len() as u64)
            .sum::<u64>()
            + request
                .system_prompt
                .as_ref()
                .and_then(|source| source.string_at(""))
                .map_or(0, |text| text.len() as u64)
            + 100;
        Ok(sidequery::SideQueryEstimate {
            serialized_bytes: bytes,
            input_tokens: bytes.div_ceil(4),
        })
    }
}

struct ChatFixture {
    _root: tempfile::TempDir,
    path: PathBuf,
    fs: Arc<dyn FileSystem>,
    source: CapturedChatTranscript,
    orch: Arc<ConversationOrchestrator>,
    writer: Arc<JsonlWriter>,
    summary: Arc<SummaryClient>,
    main: Arc<MockApiClient>,
    allowed: Arc<AtomicBool>,
}

fn builder(
    root: &Path,
    session_id: SessionId,
    writer: Arc<JsonlWriter>,
    main: Arc<MockApiClient>,
    budget: Option<u64>,
) -> ConversationOrchestrator {
    ConversationOrchestrator::new_side_chat(
        OrchestratorConfig {
            model: "fixture-model".into(),
            system_prompt_override: Some("Test Chat.".into()),
            exclude_dynamic_system_prompt_sections: true,
            ..OrchestratorConfig::default()
        },
        Arc::new(BudgetApi { budget, main }),
        Arc::new(MockStreamingApiClient::empty()),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        root.to_path_buf(),
    )
    .with_session_id(session_id)
    .with_jsonl_writer(writer)
}

fn answer(text: String) -> ConversationMessage {
    ConversationMessage::Assistant {
        id: MessageId::new(),
        content: vec![ContentBlock::Text {
            text,
            citations: None,
        }],
        stop_reason: Some("stop_sequence".into()),
        resumable: None,
        per_turn_effort: None,
    }
}

async fn fixture(media_in_prefix: bool, budget: Option<u64>) -> ChatFixture {
    fixture_seed(media_in_prefix, budget, false).await
}

async fn fixture_seed(
    media_in_prefix: bool,
    budget: Option<u64>,
    historical_thinking: bool,
) -> ChatFixture {
    fixture_seed_variant(media_in_prefix, budget, historical_thinking, false).await
}

async fn fixture_seed_variant(
    media_in_prefix: bool,
    budget: Option<u64>,
    historical_thinking: bool,
    thinking_only_tail: bool,
) -> ChatFixture {
    let root = tempfile::tempdir().unwrap();
    let chat_id = uuid::Uuid::new_v4().to_string();
    let session_id = SessionId::new();
    let directory = root
        .path()
        .join("apps/test-app/agent/agui-v1/chats")
        .join(&chat_id);
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join(format!("{}.jsonl", session_id.as_uuid()));
    let fs: Arc<dyn FileSystem> = Arc::new(platform_posix::fs::PosixFileSystem::new(
        root.path().to_path_buf(),
    ));
    let main = Arc::new(MockApiClient::new(vec![mock_message_response(
        vec![llm_runtime::ContentBlock::Text {
            text: "new answer".into(),
            cache_control: None,
            citations: None,
        }],
        Some("end_turn"),
    )]));
    let seed_writer = Arc::new(
        JsonlWriter::open_persistent_chat(path.clone(), fs.clone(), None)
            .await
            .unwrap(),
    );
    let seed = builder(
        root.path(),
        session_id,
        seed_writer.clone(),
        main.clone(),
        budget,
    );
    for round in 0..6 {
        let user_len = if thinking_only_tail && round == 4 {
            5000
        } else {
            2000
        };
        let mut user = ConversationMessage::user(
            MessageId::new(),
            format!("old user {round}: {}", "u".repeat(user_len)),
        );
        if media_in_prefix && round == 0 {
            if let ConversationMessage::User { content, .. } = &mut user {
                content.push(ContentBlock::Image {
                    source: lingxi_core::types::ImageSource::Base64 {
                        media_type: "image/png".into(),
                        data: "AQID".into(),
                    },
                });
            }
        }
        seed.append_external_history_message(user).await;
        let mut assistant = answer(format!("old answer {round}: {}", "a".repeat(2000)));
        if historical_thinking {
            if let ConversationMessage::Assistant { content, .. } = &mut assistant {
                content.insert(
                    0,
                    ContentBlock::Thinking {
                        thinking: format!("historical thinking {round}"),
                        signature: Some(format!("old-prefix-signature-{round}")),
                    },
                );
                content.insert(
                    1,
                    ContentBlock::RedactedThinking {
                        data: format!("old-redacted-{round}"),
                    },
                );
                if thinking_only_tail && round == 4 {
                    *content = vec![
                        ContentBlock::Thinking {
                            thinking: "historical-only ".repeat(220),
                            signature: Some("old-prefix-thinking-only".into()),
                        },
                        ContentBlock::RedactedThinking {
                            data: "old-redacted-only".into(),
                        },
                    ];
                }
            }
        }
        seed.append_external_history_message(assistant).await;
    }
    let source = seed.verify_persistent_chat_checkpoint(true).await.unwrap();
    drop(seed);
    drop(seed_writer);
    let writer = Arc::new(
        JsonlWriter::open_persistent_chat(
            path.clone(),
            fs.clone(),
            Some(source.checkpoint().clone()),
        )
        .await
        .unwrap(),
    );
    let allowed = Arc::new(AtomicBool::new(true));
    let summary = Arc::new(SummaryClient {
        calls: tokio::sync::Mutex::new(Vec::new()),
        allowed: allowed.clone(),
        revoke_in_response: AtomicBool::new(false),
        emit_tool_call: AtomicBool::new(false),
    });
    let authorize = allowed.clone();
    let orch = builder(
        root.path(),
        session_id,
        writer.clone(),
        main.clone(),
        budget,
    )
    .with_persistent_chat_context(PersistentChatContextConfig {
        app_id: "test-app".into(),
        chat_id,
        source: Some(source.clone()),
        client: summary.clone(),
        authorized: Arc::new(move || authorize.load(Ordering::Acquire)),
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
    ChatFixture {
        _root: root,
        path,
        fs,
        source,
        orch: Arc::new(orch),
        writer,
        summary,
        main,
        allowed,
    }
}

async fn prepare_current(
    fixture: &ChatFixture,
) -> Result<Vec<ConversationMessage>, OrchestratorError> {
    fixture
        .orch
        .append_external_history_message(ConversationMessage::user(
            MessageId::new(),
            format!("current-only {}", "c".repeat(6000)),
        ))
        .await;
    fixture
        .orch
        .prepare_persistent_chat_request(
            fixture.orch.snapshot_history().await,
            None,
            "fixture-model",
            None,
            &[],
            None,
        )
        .await
}

#[tokio::test]
async fn actual_turn_uses_only_the_projection_and_keeps_canonical_history() {
    let fixture = fixture(false, Some(8000)).await;
    fixture
        .orch
        .run_turn(&format!("current-only {}", "c".repeat(6000)))
        .await
        .unwrap();
    let calls = fixture.main.captured_msgs().await;
    assert_eq!(calls.len(), 1);
    assert!(calls[0].iter().any(ConversationMessage::is_compact_summary));
    assert!(!calls[0]
        .iter()
        .any(|message| message.text_content().starts_with("old user 0:")));
    let summaries = fixture.summary.calls.lock().await;
    assert_eq!(summaries.len(), 1);
    assert!(matches!(
        summaries[0].messages.last(),
        Some(ConversationMessage::User { .. })
    ));
    assert!(!summaries[0]
        .messages
        .iter()
        .any(|message| message.text_content().contains("current-only")));
    drop(summaries);
    let raw = fixture.orch.snapshot_history().await;
    assert!(raw
        .iter()
        .any(|message| message.text_content().starts_with("old user 0:")));
    assert!(!raw.iter().any(ConversationMessage::is_compact_summary));
    let committed = fixture
        .orch
        .verify_persistent_chat_checkpoint(true)
        .await
        .unwrap();
    let replayed = ConversationOrchestrator::replay_persistent_chat_snapshot(&committed).unwrap();
    assert_eq!(raw, replayed.display_history);
    assert!(replay_context_receipts(&committed).unwrap().is_some());
    assert_eq!(replayed.state.compacted_user_turns, 0);
    assert!(!replayed.runtime_metadata.compaction_tracking.compacted);
}

include!("persistent_chat_context_provider_tests.rs");

#[tokio::test]
async fn retries_reapply_the_verified_projection_without_calling_the_summarizer() {
    let fixture = fixture(false, Some(8000)).await;
    let projected = prepare_current(&fixture).await.unwrap();
    let raw = fixture.orch.snapshot_history().await;
    let prepared = fixture
        .orch
        .apply_persistent_chat_projection(PreparedModelCall {
            history_snapshot: raw.clone(),
            model: "fixture-model".into(),
            model_profile: None,
            outgoing_history_rewriter: None,
        })
        .await
        .unwrap();
    for _ in 0..3 {
        let retry = fixture
            .orch
            .rewrite_outgoing_history(raw.clone(), prepared.outgoing_history_rewriter.as_ref())
            .await
            .unwrap();
        assert_eq!(retry, projected);
    }
    assert_eq!(fixture.summary.calls.lock().await.len(), 1);
    fixture.allowed.store(false, Ordering::Release);
    assert!(fixture
        .orch
        .rewrite_outgoing_history(raw, prepared.outgoing_history_rewriter.as_ref())
        .await
        .is_err());
}

#[tokio::test]
async fn revocation_while_summarizing_cannot_commit_or_install_a_receipt() {
    let fixture = fixture(false, Some(8000)).await;
    fixture
        .summary
        .revoke_in_response
        .store(true, Ordering::Release);
    assert!(prepare_current(&fixture).await.is_err());
    assert_eq!(fixture.summary.calls.lock().await.len(), 1);
    assert!(!std::fs::read_to_string(&fixture.path)
        .unwrap()
        .contains(RECEIPT_KIND));
    assert!(fixture
        .orch
        .persistent_chat_context
        .as_ref()
        .unwrap()
        .latest
        .lock()
        .unwrap()
        .is_none());
    assert!(fixture.main.captured_msgs().await.is_empty());
}

#[tokio::test]
async fn cancelled_owner_and_requested_summary_tools_never_dispatch_side_effects() {
    let fixture = fixture(false, Some(8000)).await;
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(fixture
        .orch
        .prepare_persistent_chat_request(
            fixture.orch.snapshot_history().await,
            None,
            "fixture-model",
            None,
            &[],
            Some(&cancel)
        )
        .await
        .is_err());
    assert!(fixture.summary.calls.lock().await.is_empty());
    fixture
        .summary
        .emit_tool_call
        .store(true, Ordering::Release);
    assert!(prepare_current(&fixture)
        .await
        .unwrap_err()
        .to_string()
        .contains(UNSUPPORTED));
    assert_eq!(fixture.summary.calls.lock().await.len(), 1);
    assert!(fixture.main.captured_msgs().await.is_empty());
    assert!(!fixture._root.path().join("forbidden").exists());
    assert!(!std::fs::read_to_string(&fixture.path)
        .unwrap()
        .contains(RECEIPT_KIND));
}

#[tokio::test]
async fn current_attachment_survives_the_request_projection_byte_for_byte() {
    let fixture = fixture(false, Some(8000)).await;
    let image = ContentBlock::Image {
        source: lingxi_core::types::ImageSource::Base64 {
            media_type: "image/png".into(),
            data: "AQID".into(),
        },
    };
    let mut input = ConversationMessage::user(MessageId::new(), "c".repeat(6000));
    if let ConversationMessage::User { content, .. } = &mut input {
        content.push(image.clone());
    }
    fixture
        .orch
        .append_external_history_message(input.clone())
        .await;
    let projected = fixture
        .orch
        .prepare_persistent_chat_request(
            fixture.orch.snapshot_history().await,
            None,
            "fixture-model",
            None,
            &[],
            None,
        )
        .await
        .unwrap();
    assert_eq!(projected.last(), Some(&input));
    let wire = llm_runtime::convert::to_llm_messages(projected).unwrap();
    assert!(serde_json::to_string(&wire).unwrap().contains("image/png"));
    assert!(fixture
        .orch
        .snapshot_history()
        .await
        .iter()
        .any(|row| row == &input));
}

#[tokio::test]
async fn an_oversized_current_input_fails_without_installing_a_useless_summary() {
    let fixture = fixture(false, Some(8000)).await;
    fixture
        .orch
        .append_external_history_message(ConversationMessage::user(
            MessageId::new(),
            "x".repeat(50_000),
        ))
        .await;
    assert!(fixture
        .orch
        .prepare_persistent_chat_request(
            fixture.orch.snapshot_history().await,
            None,
            "fixture-model",
            None,
            &[],
            None
        )
        .await
        .unwrap_err()
        .to_string()
        .contains(LIMIT));
    assert!(!std::fs::read_to_string(&fixture.path)
        .unwrap()
        .contains(RECEIPT_KIND));
    assert!(fixture.main.captured_msgs().await.is_empty());
    assert!(fixture.summary.calls.lock().await.is_empty());
}

#[tokio::test]
async fn protected_attachment_prefix_fails_without_losing_its_bytes() {
    let fixture = fixture(true, Some(8000)).await;
    let result = prepare_current(&fixture).await;
    assert!(result.unwrap_err().to_string().contains(UNSUPPORTED));
    assert!(fixture.summary.calls.lock().await.is_empty());
    let captured = fixture
        .writer
        .capture_persistent_chat_snapshot(false)
        .await
        .unwrap();
    let replayed = ConversationOrchestrator::replay_persistent_chat_snapshot(&captured).unwrap();
    assert!(replayed.state.history.iter().any(|message| match message {
        ConversationMessage::User { content, .. } => content.iter().any(|block| matches!(block,
            ContentBlock::Image { source: lingxi_core::types::ImageSource::Base64 { data, .. } } if data == "AQID")),
        _ => false,
    }));
}

#[tokio::test]
async fn unknown_budget_fails_before_any_summary_or_main_request() {
    let fixture = fixture(false, None).await;
    assert!(prepare_current(&fixture)
        .await
        .unwrap_err()
        .to_string()
        .contains("chat_context_budget_unknown"));
    assert!(fixture.summary.calls.lock().await.is_empty());
    assert!(fixture.main.captured_msgs().await.is_empty());
    assert!(!std::fs::read_to_string(&fixture.path)
        .unwrap()
        .contains(RECEIPT_KIND));
}

#[tokio::test]
async fn durable_receipt_in_an_unfinished_turn_is_not_a_success_frontier() {
    for tail in ["none", "failed-assistant", "torn-record"] {
        let fixture = fixture(false, Some(8000)).await;
        prepare_current(&fixture).await.unwrap();
        if tail == "failed-assistant" {
            fixture
                .orch
                .append_external_history_message(answer("FAILED-PRIVATE-ANSWER".into()))
                .await;
        }
        if tail == "torn-record" {
            use std::io::Write as _;
            std::fs::OpenOptions::new()
                .append(true)
                .open(&fixture.path)
                .unwrap()
                .write_all(b"{\"type\":")
                .unwrap();
        }
        let ChatFixture {
            _root,
            path,
            source,
            orch,
            writer,
            ..
        } = fixture;
        drop(orch);
        drop(writer);
        let original = std::fs::read(&path).unwrap();
        let target = path
            .parent()
            .unwrap()
            .join(uuid::Uuid::new_v4().to_string())
            .join(path.file_name().unwrap());
        let recovered = JsonlWriter::stage_persistent_chat_recovery(
            path.clone(),
            target,
            Some(source.checkpoint().clone()),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(recovered.source(), source.source());
        assert!(replay_context_receipts(&recovered).unwrap().is_none());
        assert_eq!(
            std::fs::read(&path).unwrap(),
            original,
            "failed/unknown tail remains audit data"
        );
        let raw = ConversationOrchestrator::replay_persistent_chat_snapshot(&recovered).unwrap();
        assert!(!raw
            .display_history
            .iter()
            .any(|message| message.text_content().contains("FAILED-PRIVATE-ANSWER")));
        drop(_root);
    }
}

#[tokio::test]
async fn cold_reopen_and_recovery_generation_restore_the_same_request_projection() {
    let fixture = fixture(false, Some(8000)).await;
    let projected = prepare_current(&fixture).await.unwrap();
    fixture
        .orch
        .append_external_history_message(answer("successful current answer".into()))
        .await;
    let committed = fixture
        .orch
        .verify_persistent_chat_checkpoint(true)
        .await
        .unwrap();
    let ChatFixture {
        _root,
        path,
        fs,
        orch,
        writer,
        main,
        summary,
        allowed,
        ..
    } = fixture;
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
        Some(committed.checkpoint().clone()),
    )
    .await
    .unwrap()
    .unwrap();
    let writer = Arc::new(
        JsonlWriter::open_persistent_chat(target, fs, Some(recovered.checkpoint().clone()))
            .await
            .unwrap(),
    );
    let (app_id, chat_id) = owner_from_path(recovered.path()).unwrap();
    let authorize = allowed.clone();
    let restored = builder(
        _root.path(),
        SessionId::from_uuid(recovered.session_id()),
        writer,
        main,
        Some(8000),
    )
    .with_persistent_chat_context(PersistentChatContextConfig {
        app_id,
        chat_id,
        source: Some(recovered.clone()),
        client: summary.clone(),
        authorized: Arc::new(move || authorize.load(Ordering::Acquire)),
    })
    .unwrap();
    let replayed = ConversationOrchestrator::replay_persistent_chat_snapshot(&recovered).unwrap();
    let actual = restored
        .persistent_chat_context
        .as_ref()
        .unwrap()
        .projection(replayed.state.history)
        .unwrap();
    assert_eq!(&actual[..projected.len()], projected.as_slice());
    assert_eq!(
        actual.last().unwrap().text_content(),
        "successful current answer"
    );
    assert_eq!(
        summary.calls.lock().await.len(),
        1,
        "recovery has no model or tool dispatch"
    );
}

#[tokio::test]
async fn receipts_reject_wrong_chat_wrong_source_and_unknown_versions() {
    for mutation in ["app", "chat", "session", "source", "version", "role"] {
        let fixture = fixture(false, Some(8000)).await;
        prepare_current(&fixture).await.unwrap();
        let captured = fixture
            .writer
            .capture_persistent_chat_snapshot(false)
            .await
            .unwrap();
        let altered = captured
            .source()
            .split_inclusive('\n')
            .map(|line| {
                let mut row = serde_json::from_str::<serde_json::Value>(line).unwrap();
                if row
                    .pointer("/attachment/type")
                    .and_then(serde_json::Value::as_str)
                    != Some(RECEIPT_KIND)
                {
                    return line.to_owned();
                }
                match mutation {
                    "app" => row["attachment"]["appId"] = "another-app".into(),
                    "chat" => row["attachment"]["chatId"] = uuid::Uuid::new_v4().to_string().into(),
                    "session" => {
                        row["attachment"]["sessionId"] = uuid::Uuid::new_v4().to_string().into()
                    }
                    "source" => {
                        row["attachment"]["sourceCheckpoint"]["sha256"] = "0".repeat(64).into()
                    }
                    "version" => row["attachment"]["type"] = "chat_model_context_v2".into(),
                    _ => row["attachment"]["role"] = "system".into(),
                }
                serde_json::to_string(&row).unwrap() + "\n"
            })
            .collect::<String>();
        let ChatFixture {
            _root,
            path,
            fs,
            orch,
            writer,
            ..
        } = fixture;
        drop(orch);
        drop(writer);
        std::fs::write(&path, &altered).unwrap();
        // Even a physically consistent capture with a matching outer hash
        // cannot launder a foreign/unknown/unbound receipt into model state.
        let writer = JsonlWriter::open_persistent_chat(path, fs, Some(proof(altered.as_bytes())))
            .await
            .unwrap();
        let altered = writer
            .capture_persistent_chat_snapshot(false)
            .await
            .unwrap();
        assert!(ConversationOrchestrator::replay_persistent_chat_snapshot(&altered).is_err());
        drop(_root);
    }
}

#[test]
fn extending_a_projection_preserves_pinned_rows_and_summary_is_user_context() {
    let history = (0..8)
        .map(|index| {
            if index % 2 == 0 {
                ConversationMessage::user(MessageId::new(), format!("u{index}"))
            } else {
                answer(format!("a{index}"))
            }
        })
        .collect::<Vec<_>>();
    let previous = ContextReceipt {
        kind: RECEIPT_KIND.into(),
        app_id: "test-app".into(),
        chat_id: uuid::Uuid::new_v4().to_string(),
        session_id: uuid::Uuid::new_v4(),
        receipt_id: uuid::Uuid::new_v4(),
        previous_receipt_id: None,
        source_checkpoint: proof(b"source\n"),
        covered_message_ids: history[..4]
            .iter()
            .map(|message| message.id().as_uuid())
            .collect(),
        summary_id: uuid::Uuid::new_v4(),
        summary_utf16: "old note".encode_utf16().collect(),
        model: "test".into(),
        profile: None,
        invalidated_thinking_ids: HashSet::new(),
        omitted_thinking_ids: HashSet::new(),
    };
    let mut next = previous.clone();
    next.receipt_id = uuid::Uuid::new_v4();
    next.previous_receipt_id = Some(previous.receipt_id);
    next.summary_id = uuid::Uuid::new_v4();
    next.summary_utf16 = "new note".encode_utf16().collect();
    next.covered_message_ids = history[..6]
        .iter()
        .map(|message| message.id().as_uuid())
        .collect();
    let pinned = ConversationMessage::user_meta(MessageId::new(), "pinned policy context".into());
    let mut projected = project(Some(&previous), history.clone()).unwrap();
    projected.insert(1, pinned.clone());
    let mut missing_visible = projected.clone();
    missing_visible.retain(|message| message.id() != history[4].id());
    assert!(advance_projection(Some(&previous), &next, missing_visible).is_err(),
        "only source-proven omitted thinking rows may be absent; visible text must not silently disappear");
    let advanced = advance_projection(Some(&previous), &next, projected).unwrap();
    assert!(matches!(
        &advanced[0],
        ConversationMessage::User {
            is_compact_summary: true,
            ..
        }
    ));
    assert_eq!(advanced[1], pinned);
    assert_eq!(&advanced[2..], &history[6..]);
    assert!(advanced
        .iter()
        .all(|message| !matches!(message, ConversationMessage::System { .. })));
    let normalized = llm_runtime::convert::normalize_messages_for_api(advanced);
    let provider = llm_runtime::convert::to_llm_messages(normalized).unwrap();
    let wire = serde_json::to_value(provider).unwrap();
    let summary = wire
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message.to_string().contains("new note"))
        .unwrap();
    assert_eq!(
        summary["role"], "user",
        "summary must reach the actual provider conversion as untrusted user content"
    );
    assert!(!wire.to_string().contains("isVisibleInTranscriptOnly"));
}
