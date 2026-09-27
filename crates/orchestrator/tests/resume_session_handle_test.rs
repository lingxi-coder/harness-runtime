//! PLACE on a RUNNING orchestrator (the symmetric twin of `clear_session`).
//!
//! Verifies that resuming:
//! - replaces the live history with the supplied transcript (so the next turn
//!   sees prior context — observed via `conversation_transcript`),
//! - ADOPTS the named session id (resume does NOT mint a fresh one, unlike
//!   `clear_session`), and
//! - leaves the live model UNCHANGED (resume keeps the running model).
use orchestrator::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::{ConversationOrchestrator, OrchestratorConfig};
use platform_api::OrchestratorHandle;
use protocol::{ConversationMessage, MessageId, SessionId};
use std::sync::Arc;

fn make_orch() -> Arc<ConversationOrchestrator> {
    let api = Arc::new(MockApiClient::new(vec![]));
    let tools = Arc::new(tool_api::registry::ToolRegistry::new());
    let hooks = noop_hook_executor();
    let perms = Arc::new(NoOpPermissionGate);
    let output = Arc::new(MockOutputStream::new());
    let memory = Arc::new(StaticMemoryProvider::empty());
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api,
        tools,
        hooks,
        perms,
        output,
        memory,
        std::env::temp_dir(),
    );
    Arc::new(orch)
}

#[tokio::test]
async fn resume_session_adopts_history_named_id_and_runtime_model() {
    let orch = make_orch();
    let handle: Arc<dyn OrchestratorHandle> = orch.clone();

    // The fresh orchestrator starts with its auto-minted id + default model and
    // an empty history.
    let original_id = handle.current_session_id().await;
    let original_model = handle.get_status_snapshot().await.model;
    assert!(
        handle.conversation_transcript().await.is_empty(),
        "a fresh orchestrator starts with no history"
    );

    // A named target session + a two-message replayed transcript.
    let named = SessionId::new();
    assert_ne!(
        named, original_id,
        "the named target must differ from the live id"
    );
    let history = vec![
        ConversationMessage::user(MessageId::new(), "prior user turn".to_string()),
        ConversationMessage::Assistant {
            id: MessageId::new(),
            content: vec![protocol::ContentBlock::Text {
                text: "prior assistant turn".to_string(),
            }],
            stop_reason: Some("end_turn".to_string()),
        },
    ];

    handle
        .resume_session(
            named,
            history.clone(),
            Some("11111111-1111-4111-8111-111111111111".to_string()),
            None,
            platform_api::ResumeRuntimeSnapshot {
                model: "claude-opus-4-1".to_string(),
                model_profile: Some("anthropic".to_string()),
                effort: Some("high".to_string()),
                ..platform_api::ResumeRuntimeSnapshot::default()
            },
        )
        .await
        .expect("resume_session must succeed on the production handle");

    // History was adopted in place — the next turn will see prior context.
    let restored = handle.conversation_transcript().await;
    assert_eq!(
        restored, history,
        "resume must adopt the replayed transcript"
    );

    // The NAMED id was adopted (resume does NOT mint a fresh one).
    assert_eq!(
        handle.current_session_id().await,
        named,
        "resume must adopt the named session id, not mint a fresh one"
    );

    // The resumed runtime model is adopted in place, matching cold resume.
    assert_eq!(
        handle.get_status_snapshot().await.model,
        "claude-opus-4-1",
        "resume must adopt the replayed model, not keep the pre-resume live one"
    );
    assert_ne!(original_model, "claude-opus-4-1");
    let session = orch.session();
    assert_eq!(
        session.lock().await.model_profile.as_deref(),
        Some("anthropic"),
        "resume must restore the provider profile paired with the replayed model"
    );
}

#[tokio::test]
async fn resume_session_default_runtime_keeps_live_model_for_legacy_callers() {
    let orch = make_orch();
    let handle: Arc<dyn OrchestratorHandle> = orch.clone();
    let original_model = handle.get_status_snapshot().await.model;

    handle
        .resume_session(
            SessionId::new(),
            vec![ConversationMessage::user(
                MessageId::new(),
                "prior user turn".to_string(),
            )],
            None,
            None,
            platform_api::ResumeRuntimeSnapshot::default(),
        )
        .await
        .expect("legacy resume_session call must still succeed");

    assert_eq!(
        handle.get_status_snapshot().await.model,
        original_model,
        "legacy/default runtime snapshots keep the pre-resume model until callers provide one"
    );
}

#[tokio::test]
async fn resume_without_persisted_human_defaults_preserves_application_reasoning() {
    let orch = make_orch();
    let model = orch.session().lock().await.model.clone();
    let selection = platform_api::ReasoningSelection::Level { id: "high".into() };
    assert_eq!(
        orch.initialize_reasoning_selection_for_model(&model, None, selection.clone()),
        selection
    );
    orch.resume_session(
        SessionId::new(),
        Vec::new(),
        None,
        None,
        platform_api::ResumeRuntimeSnapshot::default(),
    )
    .await
    .unwrap();
    assert_eq!(orch.current_reasoning_selection(), selection);
    assert_eq!(orch.session().lock().await.model, model);
}
