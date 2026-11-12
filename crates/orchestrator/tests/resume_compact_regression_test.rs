//! Model-context parity across repeated compaction, durable restore and rollback.
use lingxi_core::host::{session_context::ConversationMode, FileSystem, OrchestratorHandle};
use lingxi_core::types::{ContentBlock, ConversationMessage, MessageId, SessionId};
use orchestrator::test_support::{
    noop_hook_executor, with_compaction_client, MockApiClient, MockOutputStream,
    NoOpPermissionGate, SideQueryClient, SideQueryError, SideQueryRequest, SideQueryResponse,
    StaticMemoryProvider,
};
use orchestrator::{ConversationOrchestrator, OrchestratorConfig};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicU8, Ordering},
    Arc, Mutex,
};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct SummaryCapture {
    requests: Mutex<Vec<Vec<ConversationMessage>>>,
    mode: AtomicU8,
    entered: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl SideQueryClient for SummaryCapture {
    async fn query(&self, request: SideQueryRequest) -> Result<SideQueryResponse, SideQueryError> {
        let index = {
            let mut requests = self.requests.lock().unwrap();
            requests.push(request.messages);
            requests.len()
        };
        match self.mode.load(Ordering::Acquire) {
            1 => {
                return Err(SideQueryError::InvalidResponse(
                    "injected summary failure".into(),
                ))
            }
            2 => {
                self.entered.notify_one();
                return std::future::pending().await;
            }
            _ => {}
        }
        Ok(SideQueryResponse {
            text: Some(format!("<summary>SUMMARY-{index}</summary>").into()),
            structured: None,
            tool_calls: Vec::new(),
            usage: Default::default(),
            stop_reason: Some("end_turn".into()),
            retry_count: 0,
        })
    }
}

fn fixture(
    root: &Path,
    path: PathBuf,
    id: SessionId,
    summary: Arc<SummaryCapture>,
) -> ConversationOrchestrator {
    let fs: Arc<dyn FileSystem> = Arc::new(platform_posix::fs::PosixFileSystem::new(root.into()));
    with_compaction_client(
        ConversationOrchestrator::new(
            OrchestratorConfig {
                system_prompt_override: Some("Isolated compact regression".into()),
                ..Default::default()
            },
            Arc::new(MockApiClient::new(Vec::new())),
            Arc::new(tool_api::registry::ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            root.into(),
        )
        .without_host_environment_probes()
        .with_config_home(root.join("home"))
        .with_session_id(id)
        .with_jsonl_writer(Arc::new(session::JsonlWriter::new(path, fs))),
        summary,
    )
}

fn assistant(text: String) -> ConversationMessage {
    ConversationMessage::Assistant {
        id: MessageId::new(),
        content: vec![ContentBlock::Text {
            text,
            citations: None,
        }],
        stop_reason: Some("end_turn".into()),
        resumable: None,
        per_turn_effort: None,
    }
}

async fn pair(orch: &ConversationOrchestrator, index: usize) {
    orch.append_external_history_message(ConversationMessage::user(
        MessageId::new(),
        format!("USER-{index}"),
    ))
    .await;
    orch.append_external_history_message(assistant(format!("ASSISTANT-{index}")))
        .await;
}

fn replay(path: &Path, id: SessionId) -> orchestrator::ReplayedSession {
    let source = std::fs::read_to_string(path).unwrap();
    orchestrator::resume::replay_captured_session(
        path.into(),
        source.clone(),
        session::jsonl::route_lines(&source),
        id.as_uuid(),
    )
    .unwrap()
}

fn shape(history: &[ConversationMessage]) -> Vec<(&'static str, String)> {
    history
        .iter()
        .map(|message| {
            (
                match message {
                    ConversationMessage::User { .. } => "user",
                    ConversationMessage::Assistant { .. } => "assistant",
                    _ => "system",
                },
                message.text_content(),
            )
        })
        .collect()
}

fn run<F, Fut>(test: F)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()>,
{
    std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .worker_threads(2)
                .thread_stack_size(64 * 1024 * 1024)
                .build()
                .unwrap()
                .block_on(Box::pin(test()));
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn repeated_compact_restores_the_same_model_input_for_every_preservation_direction() {
    run(|| async {
        for case in ["ordinary", "empty", "user", "assistant", "from", "up_to"] {
            let root = tempfile::tempdir().unwrap();
            let root = root.path().canonicalize().unwrap();
            let id = SessionId::new();
            let path = root.join(format!("{}.jsonl", id.as_uuid()));
            let capture = Arc::new(SummaryCapture::default());
            let hot = fixture(&root, path.clone(), id, capture.clone());
            hot.initialize_conversation_mode().await.unwrap();
            for index in 0..4 {
                pair(&hot, index).await;
            }
            hot.force_compact().await.unwrap();
            let first = replay(&path, id);
            let source = std::fs::read_to_string(&path).unwrap();
            let cold = fixture(&root, path.clone(), id, capture.clone());
            cold.restore_captured_session_history(
                id.as_uuid(),
                &first.messages,
                &session::jsonl::route_lines(&source).messages_in_order,
                true,
                permission::PermissionMode::Default,
                ConversationMode::Code,
                &source,
            )
            .await
            .unwrap();
            match case {
                "ordinary" => {
                    for index in 4..7 {
                        pair(&cold, index).await;
                    }
                }
                "user" => {
                    cold.append_external_history_message(ConversationMessage::user(
                        MessageId::new(),
                        "ONE-NEW-USER".into(),
                    ))
                    .await;
                }
                "assistant" => {
                    cold.append_external_history_message(assistant("ONE-NEW-ASSISTANT".into()))
                        .await;
                }
                "from" | "up_to" => {
                    pair(&cold, 4).await;
                    pair(&cold, 5).await;
                }
                _ => {}
            }
            if case == "from" || case == "up_to" {
                let target = cold
                    .session()
                    .lock()
                    .await
                    .model_context_history()
                    .iter()
                    .find(|message| message.text_content() == "USER-4")
                    .unwrap()
                    .id();
                cold.summarize_at(
                    &target.to_string(),
                    None,
                    if case == "from" {
                        compaction::prompt::SummarizeDirection::From
                    } else {
                        compaction::prompt::SummarizeDirection::UpTo
                    },
                    CancellationToken::new(),
                )
                .await
                .unwrap();
            } else {
                cold.force_compact().await.unwrap();
            }
            let live = cold.session().lock().await.model_context_history();
            let restored = replay(&path, id).state.model_context_history();
            assert_eq!(
                shape(&live),
                shape(&restored),
                "{case}: cold restore changed model input"
            );
            if case == "from" {
                assert!(live
                    .iter()
                    .any(|message| message.text_content().contains("Summary:\nSUMMARY-1")));
                assert!(live
                    .iter()
                    .any(|message| message.text_content() == "ASSISTANT-3"));
            }
            let requests = capture.requests.lock().unwrap();
            assert_eq!(requests.len(), 2, "{case}");
            assert!(
                requests[1]
                    .iter()
                    .all(|message| !["USER-0", "USER-1", "USER-2"]
                        .contains(&message.text_content().as_str())),
                "{case}: summarized prefix re-entered compact input"
            );
            if case != "from" {
                assert!(
                    requests[1]
                        .iter()
                        .any(|message| message.text_content().contains("Summary:\nSUMMARY-1")),
                    "{case}: previous summary missing"
                );
            }
        }
    });
}

#[test]
fn failed_or_cancelled_repeated_compact_keeps_hot_and_durable_context() {
    run(|| async {
        for cancel in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let root = root.path().canonicalize().unwrap();
            let id = SessionId::new();
            let path = root.join(format!("{}.jsonl", id.as_uuid()));
            let capture = Arc::new(SummaryCapture::default());
            let orch = fixture(&root, path.clone(), id, capture.clone());
            orch.initialize_conversation_mode().await.unwrap();
            for index in 0..4 {
                pair(&orch, index).await;
            }
            orch.force_compact().await.unwrap();
            pair(&orch, 4).await;
            let before = shape(&orch.session().lock().await.model_context_history());
            let bytes = std::fs::read(&path).unwrap();
            capture
                .mode
                .store(if cancel { 2 } else { 1 }, Ordering::Release);
            let token = CancellationToken::new();
            let compact = orch.force_compact_with_cancel(token.clone());
            tokio::pin!(compact);
            if cancel {
                tokio::time::timeout(std::time::Duration::from_secs(10), async {
                    tokio::select! {
                        result = &mut compact => panic!("completed before cancel: {result:?}"),
                        _ = capture.entered.notified() => {}
                    }
                })
                .await
                .unwrap();
                token.cancel();
            }
            assert!(
                tokio::time::timeout(std::time::Duration::from_secs(10), compact)
                    .await
                    .unwrap()
                    .is_err()
            );
            assert_eq!(
                shape(&orch.session().lock().await.model_context_history()),
                before
            );
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
            assert_eq!(
                shape(&replay(&path, id).state.model_context_history()),
                before
            );
        }
    });
}

#[test]
fn multiple_selective_compactions_compose_all_prior_preserved_segments() {
    run(|| async {
        for replace_prefix in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let root = root.path().canonicalize().unwrap();
            let id = SessionId::new();
            let path = root.join(format!("{}.jsonl", id.as_uuid()));
            let capture = Arc::new(SummaryCapture::default());
            let orch = fixture(&root, path.clone(), id, capture.clone());
            orch.initialize_conversation_mode().await.unwrap();
            for index in 0..4 {
                pair(&orch, index).await;
            }
            orch.force_compact().await.unwrap();
            for index in 4..7 {
                pair(&orch, index).await;
                let target = orch
                    .session()
                    .lock()
                    .await
                    .model_context_history()
                    .iter()
                    .find(|message| message.text_content() == format!("USER-{index}"))
                    .unwrap()
                    .id();
                let direction = if replace_prefix && index == 5 {
                    compaction::prompt::SummarizeDirection::UpTo
                } else {
                    compaction::prompt::SummarizeDirection::From
                };
                orch.summarize_at(
                    &target.to_string(),
                    None,
                    direction,
                    CancellationToken::new(),
                )
                .await
                .unwrap();
                assert_eq!(
                    shape(&orch.session().lock().await.model_context_history()),
                    shape(&replay(&path, id).state.model_context_history()),
                    "round {index}, replace_prefix={replace_prefix}"
                );
            }
            assert_eq!(capture.requests.lock().unwrap().len(), 4);
        }
    });
}
