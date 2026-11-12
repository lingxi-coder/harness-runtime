//! PLACE on a RUNNING orchestrator (the symmetric twin of `clear_session`).
//!
//! Verifies that resuming:
//! - replaces the live history with the supplied transcript (so the next turn
//!   sees prior context — observed via `conversation_transcript`),
//! - ADOPTS the named session id (resume does NOT mint a fresh one, unlike
//!   `clear_session`), and
//! - restores a persisted model, or keeps the live model when none was captured.
use lingxi_core::host::OrchestratorHandle;
use lingxi_core::types::{ConversationMessage, MessageId, SessionId};
use orchestrator::test_support::{
    code_resume_runtime_snapshot, noop_hook_executor, MockApiClient, MockOutputStream,
    NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::{ConversationOrchestrator, OrchestratorConfig};
use std::sync::Arc;

fn make_orch() -> (tempfile::TempDir, Arc<ConversationOrchestrator>) {
    let root = tempfile::tempdir().unwrap();
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
        root.path().to_path_buf(),
    )
    .with_config_home(root.path().join("home"));
    (root, Arc::new(orch))
}

#[tokio::test]
async fn resume_session_adopts_history_named_id_and_runtime_model() {
    let (_root, orch) = make_orch();
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
            per_turn_effort: None,
            id: MessageId::new(),
            content: vec![lingxi_core::types::ContentBlock::Text {
                text: "prior assistant turn".to_string(),
                citations: None,
            }],
            stop_reason: Some("end_turn".to_string()),
            resumable: None,
        },
    ];

    handle
        .resume_session(
            named,
            history.clone(),
            Some("11111111-1111-4111-8111-111111111111".to_string()),
            None,
            lingxi_core::host::ResumeRuntimeSnapshot {
                model: "claude-opus-4-1".to_string(),
                model_profile: Some("anthropic".to_string()),
                effort: Some("high".to_string()),
                ..code_resume_runtime_snapshot(named)
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
async fn resume_session_without_persisted_model_keeps_live_model() {
    let (_root, orch) = make_orch();
    let handle: Arc<dyn OrchestratorHandle> = orch.clone();
    let original_model = handle.get_status_snapshot().await.model;

    let named = SessionId::new();
    handle
        .resume_session(
            named,
            vec![ConversationMessage::user(
                MessageId::new(),
                "prior user turn".to_string(),
            )],
            None,
            None,
            code_resume_runtime_snapshot(named),
        )
        .await
        .expect("fixed-mode resume without a model must succeed");

    assert_eq!(
        handle.get_status_snapshot().await.model,
        original_model,
        "runtime snapshots without a persisted model keep the pre-resume model"
    );
}

#[tokio::test]
async fn resume_without_persisted_human_defaults_preserves_application_reasoning() {
    let (_root, orch) = make_orch();
    let model = orch.session().lock().await.model.clone();
    let selection = lingxi_core::host::ReasoningSelection::Level { id: "high".into() };
    assert_eq!(
        orch.initialize_reasoning_selection_for_model(&model, None, selection.clone()),
        selection
    );
    let named = SessionId::new();
    orch.resume_session(
        named,
        Vec::new(),
        None,
        None,
        code_resume_runtime_snapshot(named),
    )
    .await
    .unwrap();
    assert_eq!(orch.current_reasoning_selection(), selection);
    assert_eq!(orch.session().lock().await.model, model);
}
