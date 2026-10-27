use super::*;
use crate::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use crate::OrchestratorConfig;
use lingxi_core::types::{ContentBlock, ConversationMessage, MessageId, ToolUseId};
use std::sync::Arc;
use tool_api::registry::ToolRegistry;

#[test]
fn playback_ack_commits_once_and_interrupt_discards_unheard_transcripts() {
    let mut ledger = TranscriptLedger::default();
    let limits = RealtimeAgentLimits::default();
    ledger.generated.insert(Some("heard".into()));
    assert!(ledger
        .output_final(Some("heard".into()), "heard text".into(), limits)
        .unwrap()
        .is_none());
    assert_eq!(
        ledger.playback(Some("heard".into())).as_deref(),
        Some("heard text")
    );
    assert!(ledger
        .output_final(Some("heard".into()), "duplicate".into(), limits)
        .unwrap()
        .is_none());
    ledger.generated.insert(Some("unheard".into()));
    ledger.interrupt();
    assert!(ledger
        .output_final(Some("unheard".into()), "not heard".into(), limits)
        .unwrap()
        .is_none());
    assert!(ledger.playback(Some("unheard".into())).is_none());
    ledger.generated.insert(Some("ack-first".into()));
    assert!(ledger.playback(Some("ack-first".into())).is_none());
    assert_eq!(
        ledger
            .output_final(Some("ack-first".into()), "heard early".into(), limits)
            .unwrap()
            .as_deref(),
        Some("heard early")
    );
}

#[tokio::test]
async fn media_handoff_commit_matches_successful_json_tool_result_text() {
    let orchestrator = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(Vec::new())),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );
    let assistant_message_id = MessageId::new();
    let merged_assistant_message_id = MessageId::new();
    assert_ne!(assistant_message_id, merged_assistant_message_id);
    let tool_use_id = ToolUseId::new();
    let session_id = orchestrator
        .session
        .lock()
        .await
        .session_id
        .as_uuid()
        .to_string();
    let tool_use = ContentBlock::ToolUse {
        id: tool_use_id.clone(),
        name: "media_session_start".into(),
        input: lingxi_core::types::utf16_json::Utf16JsonProjection::from_serde_value(
            &serde_json::json!({"intent":"conversation","mode":"audio"}),
        )
        .expect("tool input is valid JSON"),
        provider_id: None,
    };
    let assistant_provenance = ConversationMessage::Assistant {
        id: assistant_message_id,
        content: vec![tool_use.clone()],
        stop_reason: Some("tool_use".into()),
        per_turn_effort: None,
        resumable: None,
    };
    let merged_assistant = ConversationMessage::Assistant {
        id: merged_assistant_message_id,
        content: vec![tool_use],
        stop_reason: Some("tool_use".into()),
        per_turn_effort: None,
        resumable: None,
    };
    let mut result = ConversationMessage::user(MessageId::new(), String::new());
    let ConversationMessage::User { content, .. } = &mut result else {
        unreachable!("the test helper creates a user message")
    };
    content.push(ContentBlock::ToolResult {
        tool_use_id: tool_use_id.clone(),
        content: r#"{"session_ref":"media-session-1","status":"ready"}"#.into(),
        is_error: Some(false),
        provider_tool_use_id: None,
        content_blocks: None,
    });
    {
        let mut session = orchestrator.session.lock().await;
        session.history.push(merged_assistant);
        session.history.push(result);
    }

    assert!(
        orchestrator
            .media_handoff_result_committed(
                &session_id,
                &assistant_message_id,
                &assistant_provenance,
                &tool_use_id,
                "media-session-1",
            )
            .await,
        "the model-facing JSON string must identify the exact successful media session"
    );
    assert!(
        !orchestrator
            .media_handoff_result_committed(
                &session_id,
                &assistant_message_id,
                &assistant_provenance,
                &tool_use_id,
                "different-session",
            )
            .await,
        "a committed result for another session cannot authorize this handoff"
    );
    assert!(
        !orchestrator
            .media_handoff_result_committed(
                &session_id,
                &MessageId::new(),
                &assistant_provenance,
                &tool_use_id,
                "media-session-1",
            )
            .await,
        "a different assistant message cannot authorize this handoff"
    );
    assert!(
        !orchestrator
            .media_handoff_result_committed(
                &session_id,
                &assistant_message_id,
                &assistant_provenance,
                &ToolUseId::new(),
                "media-session-1",
            )
            .await,
        "a different tool-use row cannot authorize this handoff"
    );

    let mismatched_source = ConversationMessage::Assistant {
        id: assistant_message_id,
        content: vec![ContentBlock::ToolUse {
            id: tool_use_id.clone(),
            name: "media_session_start".into(),
            input: lingxi_core::types::utf16_json::Utf16JsonProjection::from_serde_value(
                &serde_json::json!({"intent":"observation","mode":"audio"}),
            )
            .expect("tool input is valid JSON"),
            provider_id: None,
        }],
        stop_reason: Some("tool_use".into()),
        per_turn_effort: None,
        resumable: None,
    };
    assert!(
        !orchestrator
            .media_handoff_result_committed(
                &session_id,
                &assistant_message_id,
                &mismatched_source,
                &tool_use_id,
                "media-session-1",
            )
            .await,
        "the source ToolUse block must exactly match the committed call"
    );

    {
        let mut session = orchestrator.session.lock().await;
        let ConversationMessage::User { content, .. } = session.history.last_mut().unwrap() else {
            unreachable!("the result message is the last history row")
        };
        let ContentBlock::ToolResult { is_error, .. } = content.last_mut().unwrap() else {
            unreachable!("the result message contains the committed tool result")
        };
        *is_error = Some(true);
    }
    assert!(
        !orchestrator
            .media_handoff_result_committed(
                &session_id,
                &assistant_message_id,
                &assistant_provenance,
                &tool_use_id,
                "media-session-1",
            )
            .await,
        "an error tool result cannot authorize this handoff"
    );
}
