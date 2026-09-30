use super::*;

use crate::test_support::{
    mock_message_response, noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
    StaticMemoryProvider,
};
use crate::test_support_stream::{
    content_block_start_text, content_block_stop, message_delta_stop, message_start, message_stop,
    text_delta, MockStreamingApiClient,
};
use lingxi_core::types::{ContentBlock, MessageId};
use std::sync::{Arc, Mutex as StdMutex};
use tool_api::registry::ToolRegistry;

static CONTEXT_COLLAPSE_ENV_LOCK: StdMutex<()> = StdMutex::new(());

#[derive(Default)]
struct RecordingPreparer {
    paths: StdMutex<Vec<ModelCallPath>>,
}

struct MarkerRewriter {
    marker: String,
}

#[async_trait]
impl OutgoingHistoryRewriter for MarkerRewriter {
    async fn rewrite(
        &self,
        _orch: &ConversationOrchestrator,
        mut raw_history: Vec<ConversationMessage>,
    ) -> Result<Vec<ConversationMessage>, OrchestratorError> {
        raw_history.push(ConversationMessage::user_meta(
            MessageId::new(),
            self.marker.clone(),
        ));
        Ok(raw_history)
    }
}

#[async_trait]
impl ModelCallPreparer for RecordingPreparer {
    async fn prepare(
        &self,
        orch: &ConversationOrchestrator,
        path: ModelCallPath,
        _system_prompt: Option<&str>,
        _cancel: Option<&tokio_util::sync::CancellationToken>,
        draft: PreparedModelCall,
    ) -> Result<PreparedModelCall, OrchestratorError> {
        self.paths.lock().unwrap().push(path);
        let marker = match path {
            ModelCallPath::Batched => "[prepared batched]",
            ModelCallPath::Streaming => "[prepared streaming]",
        };
        let rewriter: Arc<dyn OutgoingHistoryRewriter> = Arc::new(MarkerRewriter {
            marker: marker.to_string(),
        });
        Ok(PreparedModelCall {
            history_snapshot: orch
                .rewrite_outgoing_history(draft.history_snapshot, Some(&rewriter))
                .await?,
            model: draft.model,
            model_profile: draft.model_profile,
            outgoing_history_rewriter: Some(rewriter),
        })
    }
}

fn request_contains_marker(messages: &[ConversationMessage], marker: &str) -> bool {
    messages.iter().any(|message| match message {
        ConversationMessage::User {
            content, is_meta, ..
        } if *is_meta => content
            .iter()
            .any(|block| matches!(block, ContentBlock::Text { text } if text == marker)),
        _ => false,
    })
}

#[tokio::test(flavor = "current_thread")]
async fn context_collapse_projects_initial_and_retry_snapshots_without_mutating_history() {
    let _env_guard = CONTEXT_COLLAPSE_ENV_LOCK.lock().unwrap();
    let saved = std::env::var(compaction::CONTEXT_COLLAPSE_ENV).ok();
    std::env::set_var(compaction::CONTEXT_COLLAPSE_ENV, "true");

    let first = "11111111-1111-4111-8111-111111111111";
    let last = "22222222-2222-4222-8222-222222222222";
    let tail = "33333333-3333-4333-8333-333333333333";
    let summary_uuid = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    let raw = vec![
        ConversationMessage::user(MessageId::parse_prefixed(first).unwrap(), "one".into()),
        ConversationMessage::user(MessageId::parse_prefixed(last).unwrap(), "two".into()),
        ConversationMessage::user(MessageId::parse_prefixed(tail).unwrap(), "tail".into()),
    ];
    let compactor = Arc::new(compaction::CompactionOrchestrator::new(1_000_000));
    compactor.context_collapse.restore_from_entries(
        vec![compaction::ContextCollapseCommit {
            collapse_id: "0000000000000001".into(),
            summary_uuid: summary_uuid.into(),
            summary_content: "<collapsed id=\"0000000000000001\">summary</collapsed>".into(),
            summary: "summary".into(),
            first_archived_uuid: first.into(),
            last_archived_uuid: last.into(),
        }],
        None,
    );
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    )
    .with_compaction(compactor);

    let prepared = orch.apply_context_collapse_projection(PreparedModelCall {
        history_snapshot: raw.clone(),
        model: "test-model".into(),
        model_profile: None,
        outgoing_history_rewriter: None,
    });
    assert_eq!(prepared.history_snapshot.len(), 2);
    assert_eq!(
        prepared.history_snapshot[0].text_content(),
        "<collapsed id=\"0000000000000001\">summary</collapsed>"
    );

    let retry = orch
        .rewrite_outgoing_history(raw.clone(), prepared.outgoing_history_rewriter.as_ref())
        .await
        .expect("retry projection");
    assert_eq!(retry, prepared.history_snapshot);
    assert_eq!(raw.len(), 3, "raw REPL history remains intact");

    match saved {
        Some(value) => std::env::set_var(compaction::CONTEXT_COLLAPSE_ENV, value),
        None => std::env::remove_var(compaction::CONTEXT_COLLAPSE_ENV),
    }
}

#[tokio::test]
async fn batched_turn_uses_shared_model_call_preparer() {
    let preparer = Arc::new(RecordingPreparer::default());
    let api = Arc::new(MockApiClient::new(vec![mock_message_response(
        vec![llm_runtime::ContentBlock::Text {
            text: "done".into(),
            cache_control: None,
        }],
        Some("end_turn"),
    )]));
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api.clone(),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    )
    .with_model_call_preparer(preparer.clone());

    let outcome = orch.run_turn("hello").await.expect("batched turn succeeds");
    assert!(matches!(outcome, ConversationOutcome::EndTurn { .. }));

    let calls = api.captured_msgs().await;
    assert_eq!(calls.len(), 1);
    assert!(
        request_contains_marker(&calls[0], "[prepared batched]"),
        "batched request should include the shared pre-call rewrite"
    );
    assert_eq!(
        preparer.paths.lock().unwrap().as_slice(),
        &[ModelCallPath::Batched]
    );
}

#[tokio::test]
async fn streaming_turn_uses_shared_model_call_preparer() {
    let preparer = Arc::new(RecordingPreparer::default());
    let streaming = Arc::new(MockStreamingApiClient::with_turns(vec![vec![
        message_start("msg_stream", "claude-opus-4-7"),
        content_block_start_text(0),
        text_delta(0, "done"),
        content_block_stop(0),
        message_delta_stop("end_turn"),
        message_stop(),
    ]]));
    let orch = ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        streaming.clone(),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    )
    .with_model_call_preparer(preparer.clone());

    let outcome = orch
        .run_turn_streaming("hello")
        .await
        .expect("streaming turn succeeds");
    assert!(matches!(
        outcome,
        crate::ConversationOutcome::EndTurn { .. }
    ));

    let calls = streaming.captured_calls().await;
    assert_eq!(calls.len(), 1);
    assert!(
        request_contains_marker(&calls[0].messages, "[prepared streaming]"),
        "streaming request should include the shared pre-call rewrite"
    );
    assert_eq!(
        preparer.paths.lock().unwrap().as_slice(),
        &[ModelCallPath::Streaming]
    );
}

#[tokio::test]
async fn thinking_strip_persistence_ignores_worker_only_rejections() {
    struct SharedRecoveryApi(std::collections::HashMap<MessageId, usize>);
    #[async_trait]
    impl OrchestratorApiClient for SharedRecoveryApi {
        async fn messages_create(
            &self,
            _model: &str,
            _profile: Option<&str>,
            _system: Option<&str>,
            _msgs: Vec<ConversationMessage>,
            _tools: Vec<serde_json::Value>,
        ) -> Result<llm_runtime::HistoryResponse, llm_runtime::LlmError> {
            unreachable!("persistence must not call the provider")
        }
        fn thinking_stripped_messages(&self) -> std::collections::HashMap<MessageId, usize> {
            self.0.clone()
        }
    }
    let worker = MessageId::new();
    let main = MessageId::new();
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(SharedRecoveryApi([(worker, 0)].into_iter().collect())),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );
    orch.session
        .lock()
        .await
        .history
        .push(ConversationMessage::Assistant {
            id: main,
            content: vec![ContentBlock::Thinking {
                thinking: "main".into(),
                signature: Some("sig".into()),
            }],
            stop_reason: None,
        });
    orch.persist_thinking_signature_strip_latch().await;
    let session = orch.session.lock().await;
    assert!(!session.thinking_signature_stripped);
    assert!(session.thinking_stripped_messages.is_empty());
}
