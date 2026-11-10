use super::*;
use crate::test_support::{
    noop_hook_executor, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use lingxi_core::host::session_context::ConversationMode;
use lingxi_core::host::OrchestratorHandle;
use std::collections::HashMap;
use std::sync::Mutex as StdMutex;

#[derive(Default)]
struct RecoveryApi(StdMutex<HashMap<MessageId, usize>>);

#[async_trait::async_trait]
impl OrchestratorApiClient for RecoveryApi {
    async fn messages_create(
        &self,
        _: crate::conversation::OrchestratorApiRequest,
    ) -> Result<llm_runtime::HistoryResponse, llm_runtime::LlmError> {
        panic!("restoration must not send a model request")
    }

    fn thinking_stripped_messages(&self) -> HashMap<MessageId, usize> {
        self.0.lock().unwrap().clone()
    }

    fn set_thinking_stripped_messages(&self, ranges: HashMap<MessageId, usize>) {
        *self.0.lock().unwrap() = ranges;
    }
}

#[async_trait::async_trait]
impl crate::conversation::StreamingApiClient for RecoveryApi {
    async fn stream(
        &self,
        _: &str,
        _: Option<&str>,
        _: Option<&lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput>,
        _: Vec<ConversationMessage>,
        _: Vec<llm_runtime::ToolDeclaration>,
        _: &str,
        _: bool,
        _: Option<llm_runtime::RequestDispatchAdmission>,
    ) -> Result<
        futures::stream::BoxStream<
            'static,
            Result<llm_runtime::HistoryEvent, llm_runtime::LlmError>,
        >,
        llm_runtime::LlmError,
    > {
        panic!("restoration must not open a model stream")
    }

    fn thinking_stripped_messages(&self) -> HashMap<MessageId, usize> {
        self.0.lock().unwrap().clone()
    }

    fn set_thinking_stripped_messages(&self, ranges: HashMap<MessageId, usize>) {
        *self.0.lock().unwrap() = ranges;
    }
}

fn signed_thinking<'a>(
    history: &'a [ConversationMessage],
    id: MessageId,
) -> Vec<(&'a str, Option<&'a str>)> {
    let ConversationMessage::Assistant { content, .. } =
        history.iter().find(|message| message.id() == id).unwrap()
    else {
        panic!("thinking belongs to an assistant");
    };
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Thinking {
                thinking,
                signature,
            } => Some((thinking.as_str(), signature.as_deref())),
            _ => None,
        })
        .collect()
}

fn captured_replay(root: &Path, session: Uuid, source: &str) -> ReplayedSession {
    let path = root.join(format!("{session}.jsonl"));
    std::fs::write(&path, source).unwrap();
    replay_captured_session(
        path,
        source.to_owned(),
        session::jsonl::route_lines(source),
        session,
    )
    .unwrap()
}

#[tokio::test]
async fn captured_hot_mount_restores_exact_rejected_ranges_and_clears_them_on_clean_resume() {
    for partial in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let cwd = std::fs::canonicalize(root.path()).unwrap();
        let sid = Uuid::new_v4();
        let user = Uuid::new_v4();
        let rejected = Uuid::new_v4();
        let marker = Uuid::new_v4();
        let fresh = Uuid::new_v4();
        let source = [
            serde_json::json!({"type":"fixed-session-mode","sessionId":sid,"sessionMode":"code","instructionEpoch":0}),
            serde_json::json!({"type":"user","uuid":user,"parentUuid":null,"sessionId":sid,"cwd":cwd,"message":{"role":"user","content":"question"}}),
            serde_json::json!({"type":"assistant","uuid":rejected,"parentUuid":user,"sessionId":sid,"cwd":cwd,"message":{"id":rejected,"role":"assistant","model":"claude-sonnet-4-20250514","content":[{"type":"thinking","thinking":"valid earlier reasoning","signature":"valid"},{"type":"thinking","thinking":"rejected reasoning","signature":"rejected"},{"type":"text","text":"answer"}],"stop_reason":"end_turn"}}),
            serde_json::json!({"type":"attachment","uuid":marker,"parentUuid":rejected,"sessionId":sid,"cwd":cwd,"attachment":{"type":"thinking_stripped","scope":if partial {"partial"} else {"all"},"from":{"messageId":rejected,"thinkingIndex":1}}}),
            serde_json::json!({"type":"assistant","uuid":fresh,"parentUuid":marker,"sessionId":sid,"cwd":cwd,"message":{"id":fresh,"role":"assistant","model":"claude-sonnet-4-20250514","content":[{"type":"thinking","thinking":"fresh reasoning","signature":"fresh"},{"type":"text","text":"new answer"}],"stop_reason":"end_turn"}}),
        ].iter().map(|row| format!("{row}\n")).collect::<String>();
        let replayed = captured_replay(&cwd, sid, &source);
        let runtime = replayed.handle_runtime_snapshot();
        let expected = HashMap::from([(MessageId::from_uuid(rejected), usize::from(partial))]);
        assert!(runtime.thinking_signature_stripped);
        assert_eq!(runtime.thinking_stripped_messages, expected);

        let initial_id = SessionId::new();
        let api = Arc::new(RecoveryApi::default());
        let streaming = Arc::new(RecoveryApi::default());
        let writer = Arc::new(JsonlWriter::new(
            cwd.join(format!("{}.jsonl", initial_id.as_uuid())),
            Arc::new(platform_posix::fs::PosixFileSystem::new(cwd.clone())),
        ));
        let orch = ConversationOrchestrator::new_with_streaming(
            OrchestratorConfig::default(),
            api.clone(),
            streaming.clone(),
            Arc::new(tool_api::ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            cwd.clone(),
        )
        .with_session_id(initial_id)
        .with_jsonl_writer(writer.clone())
        .without_host_environment_probes();
        orch.initialize_conversation_mode().await.unwrap();
        orch.resume_session(
            replayed.state.session_id,
            replayed.state.history,
            replayed.last_message_uuid.map(|id| id.to_string()),
            None,
            runtime,
        )
        .await
        .unwrap();
        let mounted = orch.session.lock().await;
        assert_eq!(mounted.session_id.as_uuid(), sid);
        assert!(mounted.thinking_signature_stripped);
        assert_eq!(mounted.thinking_stripped_messages, expected);
        assert_eq!(*api.0.lock().unwrap(), expected);
        assert_eq!(*streaming.0.lock().unwrap(), expected);
        assert_eq!(
            signed_thinking(&mounted.history, MessageId::from_uuid(rejected)),
            vec![
                ("valid earlier reasoning", Some("valid")),
                ("rejected reasoning", Some("rejected"))
            ],
            "resume must preserve the source history and its original signatures"
        );
        let mut outbound = mounted.history.clone();
        llm_runtime::model::thinking_signature::strip_marked_conversation_thinking(
            &mut outbound,
            &api.0.lock().unwrap(),
        );
        assert_eq!(
            signed_thinking(&outbound, MessageId::from_uuid(rejected)),
            if partial {
                vec![("valid earlier reasoning", Some("valid"))]
            } else {
                Vec::new()
            },
            "the provider projection must use the exact restored per-message range"
        );
        assert_eq!(
            signed_thinking(&outbound, MessageId::from_uuid(fresh)),
            vec![("fresh reasoning", Some("fresh"))],
            "fresh reasoning stays outside the historical rejection range"
        );
        drop(mounted);
        assert_eq!(writer.active_path(), cwd.join(format!("{sid}.jsonl")));
        assert_eq!(
            std::fs::read_to_string(writer.active_path()).unwrap(),
            source
        );

        let clean_id = Uuid::new_v4();
        let clean_user = Uuid::new_v4();
        let clean_assistant = Uuid::new_v4();
        let clean_source = [
            serde_json::json!({"type":"fixed-session-mode","sessionId":clean_id,"sessionMode":"code","instructionEpoch":0}),
            serde_json::json!({"type":"user","uuid":clean_user,"parentUuid":null,"sessionId":clean_id,"cwd":cwd,"message":{"role":"user","content":"clean question"}}),
            serde_json::json!({"type":"assistant","uuid":clean_assistant,"parentUuid":clean_user,"sessionId":clean_id,"cwd":cwd,"message":{"id":clean_assistant,"role":"assistant","model":"claude-sonnet-4-20250514","content":[{"type":"thinking","thinking":"clean reasoning","signature":"clean"},{"type":"text","text":"clean answer"}],"stop_reason":"end_turn"}}),
        ].iter().map(|row| format!("{row}\n")).collect::<String>();
        let clean = captured_replay(&cwd, clean_id, &clean_source);
        let clean_runtime = clean.handle_runtime_snapshot();
        assert!(!clean_runtime.thinking_signature_stripped);
        assert!(clean_runtime.thinking_stripped_messages.is_empty());
        orch.resume_session(
            clean.state.session_id,
            clean.state.history,
            clean.last_message_uuid.map(|id| id.to_string()),
            None,
            clean_runtime,
        )
        .await
        .unwrap();
        let mounted = orch.session.lock().await;
        assert_eq!(mounted.session_id.as_uuid(), clean_id);
        assert!(!mounted.thinking_signature_stripped);
        assert!(mounted.thinking_stripped_messages.is_empty());
        assert!(api.0.lock().unwrap().is_empty());
        assert!(streaming.0.lock().unwrap().is_empty());
        let mut outbound = mounted.history.clone();
        llm_runtime::model::thinking_signature::strip_marked_conversation_thinking(
            &mut outbound,
            &api.0.lock().unwrap(),
        );
        assert_eq!(
            signed_thinking(&outbound, MessageId::from_uuid(clean_assistant)),
            vec![("clean reasoning", Some("clean"))]
        );
        drop(mounted);
        assert_eq!(writer.active_path(), cwd.join(format!("{clean_id}.jsonl")));
        assert_eq!(
            std::fs::read_to_string(writer.active_path()).unwrap(),
            clean_source
        );
    }
}

#[tokio::test]
async fn captured_cold_mount_restores_rejected_thinking_ranges_before_api_sync() {
    for partial in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let sid = Uuid::new_v4();
        let user = Uuid::new_v4();
        let rejected = Uuid::new_v4();
        let marker = Uuid::new_v4();
        let fresh = Uuid::new_v4();
        let source = [
            serde_json::json!({"type":"fixed-session-mode","sessionId":sid,"sessionMode":"code","instructionEpoch":0}),
            serde_json::json!({"type":"user","uuid":user,"parentUuid":null,"sessionId":sid,"message":{"role":"user","content":"question"}}),
            serde_json::json!({"type":"assistant","uuid":rejected,"parentUuid":user,"sessionId":sid,"message":{"id":rejected,"role":"assistant","model":"claude-sonnet-4-20250514","content":[{"type":"thinking","thinking":"valid earlier reasoning","signature":"valid"},{"type":"thinking","thinking":"rejected reasoning","signature":"rejected"},{"type":"text","text":"answer"}],"stop_reason":"end_turn"}}),
            serde_json::json!({"type":"attachment","uuid":marker,"parentUuid":rejected,"sessionId":sid,"attachment":{"type":"thinking_stripped","scope":if partial {"partial"} else {"all"},"from":{"messageId":rejected,"thinkingIndex":1}}}),
            serde_json::json!({"type":"assistant","uuid":fresh,"parentUuid":marker,"sessionId":sid,"message":{"id":fresh,"role":"assistant","model":"claude-sonnet-4-20250514","content":[{"type":"thinking","thinking":"fresh reasoning","signature":"fresh"},{"type":"text","text":"new answer"}],"stop_reason":"end_turn"}}),
        ].iter().map(|row| format!("{row}\n")).collect::<String>();
        let loaded = session::jsonl::route_lines(&source);
        let (chain, _) = session::jsonl::build_conversation_chain(&loaded, &sid.to_string());
        let api = Arc::new(RecoveryApi::default());
        let orch = ConversationOrchestrator::new(
            OrchestratorConfig::default(),
            api.clone(),
            Arc::new(tool_api::ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            root.path().to_path_buf(),
        )
        .without_host_environment_probes();
        orch.restore_captured_session_history(
            sid,
            &chain,
            &loaded.messages_in_order,
            false,
            permission::PermissionMode::Default,
            ConversationMode::Code,
            &source,
        )
        .await
        .unwrap();

        let mounted = orch.session.lock().await;
        let expected = HashMap::from([(MessageId::from_uuid(rejected), usize::from(partial))]);
        assert!(mounted.thinking_signature_stripped);
        assert_eq!(mounted.thinking_stripped_messages, expected);
        assert_eq!(*api.0.lock().unwrap(), expected);
        let mut projected = mounted.history.clone();
        llm_runtime::model::thinking_signature::strip_marked_conversation_thinking(
            &mut projected,
            &mounted.thinking_stripped_messages,
        );
        let reasoning = |id| {
            let ConversationMessage::Assistant { content, .. } =
                projected.iter().find(|message| message.id() == id).unwrap()
            else {
                panic!("the recovered thinking belongs to an assistant");
            };
            content
                .iter()
                .filter(|block| matches!(block, ContentBlock::Thinking { .. }))
                .count()
        };
        assert_eq!(
            reasoning(MessageId::from_uuid(rejected)),
            usize::from(partial)
        );
        assert_eq!(
            reasoning(MessageId::from_uuid(fresh)),
            1,
            "new responses remain outside the old recovery marker"
        );
    }
}
