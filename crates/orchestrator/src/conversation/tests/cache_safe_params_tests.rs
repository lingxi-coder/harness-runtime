use super::*;
use crate::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use lingxi_core::types::{ConversationMessage, MessageId};
use std::sync::Arc;

#[tokio::test]
async fn request_cache_snapshot_uses_physical_history_and_extracts_inline_context() {
    let slot = Arc::new(sidequery::CacheSafeParamsSlot::new());
    let orch = ConversationOrchestrator::new(
        crate::OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(tool_api::registry::ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::path::PathBuf::from("/tmp"),
    )
    .with_cache_safe_slot(slot.clone());

    let stale_session_row = ConversationMessage::user(MessageId::new(), "stale".into());
    orch.session.lock().await.history = vec![stale_session_row];

    let earlier_row = ConversationMessage::user(MessageId::new(), "earlier".into());
    let inline_context = ConversationMessage::user_meta(MessageId::new(), "context".into());
    let current_prompt = ConversationMessage::user(MessageId::new(), "current".into());
    let transient_reminder = ConversationMessage::user_meta(MessageId::new(), "reminder".into());
    let physical_request = vec![
        earlier_row.clone(),
        inline_context.clone(),
        current_prompt.clone(),
        transient_reminder.clone(),
    ];

    orch.save_cache_safe_params_for_request(
        None,
        "claude-opus-4-8",
        &[],
        &physical_request,
        Some(&inline_context),
    )
    .await
    .unwrap();

    let cached = slot.get_last().await.expect("request snapshot saved");
    assert_eq!(
        cached.fork_context_messages,
        vec![earlier_row, current_prompt, transient_reminder],
        "cache history follows the physical request and removes only the inline row"
    );
    assert_eq!(
        cached
            .user_context_message
            .as_ref()
            .map(ConversationMessage::id),
        Some(inline_context.id()),
        "inline context keeps its identity in the separate cache field"
    );
}
