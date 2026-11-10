use crate::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use crate::{ConversationOrchestrator, OrchestratorConfig};
use lingxi_core::host::session_context::ConversationMode;
use lingxi_core::types::{utf16_json::Utf16JsonProjection, ContentBlock, ConversationMessage};
use lingxi_core::types::{MessageId, SessionId};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

struct RecordingClient {
    requests: Mutex<Vec<sidequery::SideQueryRequest>>,
    started: Notify,
    block: bool,
}

#[async_trait::async_trait]
impl sidequery::SideQueryClient for RecordingClient {
    async fn query(
        &self,
        request: sidequery::SideQueryRequest,
    ) -> Result<sidequery::SideQueryResponse, sidequery::SideQueryError> {
        self.requests.lock().unwrap().push(request);
        self.started.notify_one();
        if self.block {
            std::future::pending::<()>().await;
        }
        let units = vec![b' ' as u16, b'O' as u16, 0xd802, b' ' as u16];
        Ok(sidequery::SideQueryResponse {
            text: Some(
                Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units).unwrap(),
            ),
            structured: None,
            tool_calls: Vec::new(),
            usage: cost::Usage::default(),
            stop_reason: Some("end_turn".into()),
            retry_count: 0,
        })
    }
}

fn fixture(
    root: &std::path::Path,
    block: bool,
) -> (
    Arc<ConversationOrchestrator>,
    Arc<RecordingClient>,
    Arc<tool_api::ToolRegistry>,
    Arc<sidequery::CacheSafeParamsSlot>,
    SessionId,
    std::path::PathBuf,
    uuid::Uuid,
) {
    let session = SessionId::new();
    let sid = session.as_uuid();
    let user = uuid::Uuid::new_v4();
    let skill = uuid::Uuid::new_v4();
    let assistant = uuid::Uuid::new_v4();
    let excluded = uuid::Uuid::new_v4();
    let rows = [
        serde_json::json!({"type":"fixed-session-mode","sessionId":sid,"sessionMode":"code","instructionEpoch":0}),
        serde_json::json!({"type":"user","uuid":user,"sessionId":sid,"parentUuid":null,"message":{"role":"user","content":"RAW_USER"}}),
        serde_json::json!({"type":"user","uuid":skill,"sessionId":sid,"parentUuid":user,"isMeta":true,"origin":{"type":"skill_activation","context_epoch":0},"message":{"role":"user","content":"SECRET_SKILL"}}),
        serde_json::json!({"type":"assistant","uuid":assistant,"sessionId":sid,"parentUuid":skill,"message":{"role":"assistant","content":[{"type":"text","text":"RAW_ASSISTANT"},{"type":"tool_use","id":"toolu_1","name":"Read","input":{"path":"important"}}]}}),
        serde_json::json!({"type":"user","uuid":excluded,"sessionId":sid,"parentUuid":assistant,"isModelContextExcluded":true,"message":{"role":"user","content":"EXCLUDED_HISTORY"}}),
    ];
    let source = rows
        .iter()
        .map(|row| format!("{row}\n"))
        .collect::<String>()
        .replace("RAW_USER", r"user-\ud800")
        .replace("RAW_ASSISTANT", r"assistant-\ud801");
    let path = root.join(format!("{sid}.jsonl"));
    std::fs::write(&path, source).unwrap();
    let writer = Arc::new(session::JsonlWriter::new(
        path.clone(),
        Arc::new(platform_posix::fs::PosixFileSystem::new(root.to_path_buf())),
    ));
    let tools = Arc::new(tool_api::ToolRegistry::new());
    let slot = Arc::new(sidequery::CacheSafeParamsSlot::new());
    let client = Arc::new(RecordingClient {
        requests: Mutex::new(Vec::new()),
        started: Notify::new(),
        block,
    });
    let runner = Arc::new(
        sidequery::ForkedAgentRunner::new()
            .with_side_query_client(client.clone(), "test-model".into()),
    );
    let orch = Arc::new(
        ConversationOrchestrator::new(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(Vec::new())),
            tools.clone(),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            root.to_path_buf(),
        )
        .with_session_id(session)
        .with_jsonl_writer(writer)
        .with_cache_safe_slot(slot.clone())
        .with_recap_runner(runner),
    );
    tools.set_main_loop_model(Some("unchanged-model-marker".into()));
    (orch, client, tools, slot, session, path, excluded)
}

#[tokio::test]
async fn fixed_capture_uses_committed_evidence_without_touching_turn_or_tool_caches() {
    let root = tempfile::tempdir().unwrap();
    let (orch, client, tools, slot, session, path, parent) = fixture(root.path(), false);
    let stream_id = MessageId::new();
    orch.session
        .lock()
        .await
        .history
        .push(ConversationMessage::user(
            stream_id,
            "UNCOMMITTED_STREAM".into(),
        ));
    let turn = orch.turn_gate.lock().await;
    let capture = tokio::time::timeout(
        Duration::from_secs(2),
        orch.capture_session_handoff(session, ConversationMode::Chat),
    )
    .await
    .expect("capture must not wait for the active turn")
    .unwrap();
    let later = serde_json::json!({"type":"user","uuid":uuid::Uuid::new_v4(),"sessionId":session.as_uuid(),"parentUuid":parent,"message":{"role":"user","content":"LATER_COMMIT"}});
    use std::io::Write;
    writeln!(
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap(),
        "{later}"
    )
    .unwrap();
    let summary = orch
        .summarize_session_handoff(&capture, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        summary.summary.string_units(""),
        Some(vec![b'O' as u16, 0xd802])
    );
    assert_eq!(summary.cutoff, capture.cutoff);
    assert_eq!(
        tools.main_loop_model().as_deref(),
        Some("unchanged-model-marker")
    );
    assert!(slot.get_last().await.is_none());
    assert!(orch
        .prompt_runtime
        .wire_tool_schema_cache
        .lock()
        .await
        .is_none());
    assert_eq!(orch.session.lock().await.history.len(), 1);
    let requests = client.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert!(request.tools.is_empty());
    let mut exact_user = false;
    let mut exact_assistant = false;
    let mut text = String::new();
    for message in &request.messages {
        if let ConversationMessage::User { content, .. } = message {
            for block in content {
                match block {
                    ContentBlock::Text { text: value, .. } => text.push_str(value),
                    ContentBlock::TextJsUtf16 {
                        text: value,
                        utf16_code_units,
                        ..
                    } => {
                        text.push_str(value);
                        exact_user |= utf16_code_units.ends_with(&[0xd800]);
                        exact_assistant |= utf16_code_units.ends_with(&[0xd801]);
                    }
                    ContentBlock::ToolUse { .. }
                    | ContentBlock::ToolResult { .. }
                    | ContentBlock::ProviderContent { .. }
                    | ContentBlock::Thinking { .. }
                    | ContentBlock::RedactedThinking { .. } => {
                        panic!("source provider/tool state reached the summary request")
                    }
                    _ => {}
                }
            }
        } else {
            panic!("handoff evidence must be untrusted user data");
        }
    }
    assert!(
        exact_user && exact_assistant,
        "raw UTF-16 source must survive capture"
    );
    assert!(text.contains("Tool call") && text.contains("important"));
    for omitted in [
        "SECRET_SKILL",
        "EXCLUDED_HISTORY",
        "UNCOMMITTED_STREAM",
        "LATER_COMMIT",
    ] {
        assert!(!text.contains(omitted), "unexpected evidence: {omitted}");
    }
    drop(turn);
}

#[tokio::test]
async fn cancelling_summary_keeps_the_source_turn_alive() {
    let root = tempfile::tempdir().unwrap();
    let (orch, client, _, _, session, _, _) = fixture(root.path(), true);
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let source = tokio::spawn({
        let orch = orch.clone();
        let started = started.clone();
        let release = release.clone();
        async move {
            let _turn = orch.turn_gate.lock().await;
            started.notify_one();
            release.notified().await;
        }
    });
    started.notified().await;
    let capture = tokio::time::timeout(
        Duration::from_secs(2),
        orch.capture_session_handoff(session, ConversationMode::Chat),
    )
    .await
    .unwrap()
    .unwrap();
    let cancel = CancellationToken::new();
    let summary = tokio::spawn({
        let orch = orch.clone();
        let cancel = cancel.clone();
        async move { orch.summarize_session_handoff(&capture, cancel).await }
    });
    tokio::time::timeout(Duration::from_secs(2), client.started.notified())
        .await
        .unwrap();
    cancel.cancel();
    assert!(tokio::time::timeout(Duration::from_secs(2), summary)
        .await
        .unwrap()
        .unwrap()
        .is_err());
    assert!(!source.is_finished());
    assert!(orch.turn_gate.try_lock().is_err());
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), source)
        .await
        .unwrap()
        .unwrap();
}
