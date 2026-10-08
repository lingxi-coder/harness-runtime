//! Restored sessions can answer /btw without a preceding live turn.
use lingxi_core::host::{OrchestratorHandle, RecapOutcome};
use lingxi_core::types::{ContentBlock, ConversationMessage, MessageId};
use orchestrator::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::{ConversationOrchestrator, OrchestratorConfig};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct CaptureClient(
    Mutex<Vec<sidequery::SideQueryRequest>>,
    Option<&'static str>,
    Vec<serde_json::Value>,
);
#[async_trait::async_trait]
impl sidequery::SideQueryClient for CaptureClient {
    async fn query(
        &self,
        request: sidequery::SideQueryRequest,
    ) -> Result<sidequery::SideQueryResponse, sidequery::SideQueryError> {
        self.0.lock().unwrap().push(request);
        Ok(sidequery::SideQueryResponse {
            text: Some(self.1.unwrap_or("Restored progress").into()),
            structured: None,
            tool_calls: self.2.clone(),
            usage: cost::Usage::default(),
            stop_reason: Some("end_turn".into()),
            retry_count: 0,
        })
    }
}

struct ErrorClient;

#[async_trait::async_trait]
impl sidequery::SideQueryClient for ErrorClient {
    async fn query(
        &self,
        _request: sidequery::SideQueryRequest,
    ) -> Result<sidequery::SideQueryResponse, sidequery::SideQueryError> {
        Err(sidequery::SideQueryError::Api(
            llm_runtime::LlmError::InvalidRequest {
                message: "bad request".into(),
            },
        ))
    }
}

#[tokio::test]
async fn side_question_prefers_text_over_a_mixed_tool_response() {
    let client = Arc::new(CaptureClient(
        Mutex::new(Vec::new()),
        Some("Answer text"),
        vec![serde_json::json!({"name": "Read"})],
    ));
    let orch = make_orch(client, Arc::new(sidequery::CacheSafeParamsSlot::new()));

    assert!(matches!(
        orch.answer_side_question("What happened?").await.unwrap(),
        RecapOutcome::Text(text) if text == "Answer text"
    ));
}

#[tokio::test]
async fn side_question_surfaces_api_errors_as_text() {
    let orch = make_orch(
        Arc::new(ErrorClient),
        Arc::new(sidequery::CacheSafeParamsSlot::new()),
    );

    assert!(matches!(
        orch.answer_side_question("What happened?").await.unwrap(),
        RecapOutcome::Text(text) if text == "(API error: invalid request: bad request)"
    ));
}

fn make_orch(
    client: Arc<dyn sidequery::SideQueryClient>,
    slot: Arc<sidequery::CacheSafeParamsSlot>,
) -> ConversationOrchestrator {
    ConversationOrchestrator::new(
        OrchestratorConfig {
            effort: Some("medium".into()),
            system_prompt_override: Some("Custom system prompt for resumed session".into()),
            ..OrchestratorConfig::default()
        },
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(tool_api::registry::ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    )
    .with_cache_safe_slot(slot)
    .with_recap_runner(Arc::new(
        sidequery::ForkedAgentRunner::new().with_side_query_client(client, "fallback-model".into()),
    ))
}
#[tokio::test]
async fn side_question_rebuilds_resumed_context_without_writing_slot_or_history() {
    let client = Arc::new(CaptureClient::default());
    let slot = Arc::new(sidequery::CacheSafeParamsSlot::new());
    let orch = make_orch(client.clone(), slot.clone());
    let sid = uuid::Uuid::new_v4();
    let messages = [
        (
            "user",
            serde_json::json!({"role": "user", "content": "Restore project"}),
        ),
        (
            "assistant",
            serde_json::json!({
                "role": "assistant", "content": [{"type": "text", "text": "Completed step one"}],
                "stop_reason": "end_turn", "model": "resumed-model"
            }),
        ),
    ]
    .into_iter()
    .map(|(kind, message)| {
        serde_json::from_value(serde_json::json!({
            "type": kind, "uuid": uuid::Uuid::new_v4().to_string(), "parentUuid": null,
            "sessionId": sid.to_string(), "timestamp": "2026-09-21T00:00:00.000Z",
            "cwd": "/tmp", "version": "0.12.0", "isSidechain": false,
            "message": message
        }))
        .unwrap()
    })
    .collect::<Vec<_>>();
    let restored = orchestrator::state_from_messages(sid, &messages);
    let history = restored.history.clone();
    assert_eq!(history.len(), 2);
    assert!(
        matches!(history.last(), Some(ConversationMessage::Assistant { stop_reason: Some(reason), .. }) if reason == "end_turn")
    );
    {
        let session = orch.session();
        let mut state = session.lock().await;
        *state = restored;
        state.model = "resumed-model".into();
        state.model_profile = Some("resumed-provider".into());
    }
    assert!(
        matches!(orch.answer_side_question("Progress?").await.unwrap(), RecapOutcome::Text(text) if text == "Restored progress")
    );
    assert!(slot.get_last().await.is_none());
    assert_eq!(orch.session().lock().await.history, history);
    let requests = client.0.lock().unwrap();
    let req = &requests[0];
    assert_eq!(req.model, "resumed-model");
    assert_eq!(req.profile.as_deref(), Some("resumed-provider"));
    assert_eq!(
        req.system_prompt.as_deref(),
        Some("Custom system prompt for resumed session"),
        "fallback must honor the effective custom prompt instead of rebuilding defaults"
    );
    assert!(
        req.messages[0].is_meta(),
        "fallback replays transient context first"
    );
    assert_eq!(&req.messages[1..3], &history[..2]);
    assert_eq!(
        req.messages.len(),
        4,
        "transient context + latest restored answer + question appended"
    );
    assert!(format!("{:?}", req.messages.last().unwrap()).contains("Progress?"));
}
#[tokio::test]
async fn side_question_combines_saved_system_with_current_completed_messages() {
    let client = Arc::new(CaptureClient::default());
    let slot = Arc::new(sidequery::CacheSafeParamsSlot::new());
    let orch = make_orch(client.clone(), slot.clone());
    let prefix = vec![ConversationMessage::user(
        MessageId::new(),
        "Frozen prefix".into(),
    )];
    slot.save(sidequery::CacheSafeParams {
        system_prompt: "frozen system".into(),
        user_context: Default::default(),
        system_context: Default::default(),
        user_context_message: None,
        tool_use_options: tool_api::ToolUseOptions {
            debug: false,
            verbose: false,
            main_loop_model: "frozen-model".into(),
            model_profile: None,
            max_budget_nano_usd: None,
            mcp_clients: vec![],
            is_non_interactive_session: false,
            custom_system_prompt: None,
            append_system_prompt: None,
        },
        tools: vec![serde_json::json!({
            "name": "Read",
            "description": "Read a file.",
            "input_schema": {"type": "object"}
        })],
        effort: Some(serde_json::json!("high")),
        fork_context_messages: prefix.clone(),
        transcript_path: None,
        generation: 0,
    })
    .await;
    let generation = slot.get_last().await.unwrap().generation;
    let history = vec![
        prefix[0].clone(),
        ConversationMessage::Assistant { per_turn_effort: None,
            id: MessageId::new(),
            content: vec![ContentBlock::Text {
                text: "New completed answer".into(), citations: None,
            }],
            stop_reason: Some("end_turn".into()),
        },
        ConversationMessage::Assistant { per_turn_effort: None,
            id: MessageId::new(),
            content: vec![ContentBlock::Text {
                text: "Partial answer".into(), citations: None,
            }],
            stop_reason: None,
        },
    ];
    orch.session().lock().await.history = history.clone();
    let current_model = orch.session().lock().await.model.clone();
    orch.answer_side_question("Progress?").await.unwrap();
    assert_eq!(slot.get_last().await.unwrap().generation, generation);
    let requests = client.0.lock().unwrap();
    assert_eq!(requests[0].system_prompt.as_deref(), Some("frozen system"));
    assert_eq!(requests[0].model, current_model);
    assert_eq!(requests[0].effort, Some(serde_json::json!("medium")));
    assert_eq!(
        requests[0].tools,
        vec![serde_json::json!({
            "name": "Read",
            "description": "Read a file.",
            "input_schema": {"type": "object"}
        })],
        "a captured side-query slot must preserve the parent tool schemas"
    );
    assert_eq!(&requests[0].messages[..2], &history[..2]);
    assert_eq!(requests[0].messages.len(), 3);
}
