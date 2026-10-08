use super::*;
use crate::test_support::{
    content_block_start_text, content_block_stop, message_delta_stop, message_start, message_stop,
    noop_hook_executor, text_delta, MockApiClient, MockOutputStream, MockStreamingApiClient,
    NoOpPermissionGate, StaticMemoryProvider,
};
use std::sync::Arc;

#[tokio::test]
async fn scheduled_turn_uses_saved_model_but_preserves_human_defaults() {
    let api = Arc::new(MockApiClient::new(Vec::new()));
    api.set_model_listings(vec![lingxi_core::host::ModelListing {
        request_model: "scheduled-model".into(),
        provider_id: "test-provider".into(),
        ..Default::default()
    }]);
    let events = || {
        crate::scripted![
            message_start("m1", "scheduled-model"),
            content_block_start_text(0),
            text_delta(0, "done"),
            content_block_stop(0),
            message_delta_stop("end_turn"),
            message_stop()
        ]
    };
    let streaming = Arc::new(MockStreamingApiClient::with_turns(vec![events(), events()]));
    let orch = ConversationOrchestrator::into_shared(ConversationOrchestrator::new_with_streaming(
        crate::OrchestratorConfig::default(),
        api,
        streaming.clone(),
        Arc::new(tool_api::registry::ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    ));
    let before = orch.session.lock().await.model.clone();
    let reasoning = orch.current_reasoning_selection();
    orch.run_scheduled_turn(
        "scheduled prompt",
        "test-provider/scheduled-model",
        lingxi_core::host::ReasoningSelection::Automatic,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(orch.session.lock().await.model, before);
    assert_eq!(orch.current_reasoning_selection(), reasoning);
    orch.run_turn_streaming("human prompt").await.unwrap();
    let calls = streaming.captured_calls().await;
    assert_eq!(calls[0].model, "scheduled-model");
    assert_eq!(calls[1].model, before);
}

#[tokio::test]
async fn missing_saved_model_fails_before_appending_prompt() {
    let orch = ConversationOrchestrator::new(
        crate::OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(Vec::new())),
        Arc::new(tool_api::registry::ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );
    let result = orch
        .run_scheduled_turn(
            "must not append",
            "gone/model",
            lingxi_core::host::ReasoningSelection::Automatic,
            CancellationToken::new(),
        )
        .await;
    assert!(result.unwrap_err().starts_with("paused:"));
    assert!(orch.snapshot_history().await.is_empty());
}

fn scheduled_session_fixture() -> Arc<ConversationOrchestrator> {
    let api = Arc::new(MockApiClient::new(Vec::new()));
    api.set_model_listings(vec![lingxi_core::host::ModelListing {
        request_model: "scheduled-model".into(),
        provider_id: "test-provider".into(),
        ..Default::default()
    }]);
    let streaming = Arc::new(MockStreamingApiClient::with_turns(vec![crate::scripted![
        message_start("scheduled-result", "scheduled-model"),
        content_block_start_text(0),
        text_delta(0, "summary belonging to the scheduled session"),
        content_block_stop(0),
        message_delta_stop("end_turn"),
        message_stop()
    ]]));
    ConversationOrchestrator::into_shared(ConversationOrchestrator::new_with_streaming(
        crate::OrchestratorConfig::default(),
        api,
        streaming,
        Arc::new(tool_api::registry::ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    ))
}

#[tokio::test]
async fn scheduled_target_gate_spans_binding_execution_and_result_capture() {
    let orch = scheduled_session_fixture();
    let original_session = orch.session.lock().await.session_id;
    let resumed_session = lingxi_core::types::SessionId::new();
    let (release_binding, binding_released) = tokio::sync::oneshot::channel();
    let scheduled = orch.run_scheduled_turn_in_session(
        original_session,
        "saved prompt",
        "test-provider/scheduled-model",
        lingxi_core::host::ReasoningSelection::Automatic,
        CancellationToken::new(),
        async {
            assert!(orch.turn_gate.try_lock().is_err());
            binding_released.await.map_err(|error| error.to_string())
        },
    );
    tokio::pin!(scheduled);
    assert!(futures::poll!(scheduled.as_mut()).is_pending());

    let resume =
        <ConversationOrchestrator as lingxi_core::host::OrchestratorHandle>::resume_session(
            orch.as_ref(),
            resumed_session,
            Vec::new(),
            None,
            None,
            lingxi_core::host::ResumeRuntimeSnapshot::default(),
        );
    tokio::pin!(resume);
    assert!(futures::poll!(resume.as_mut()).is_pending());
    assert_eq!(orch.session.lock().await.session_id, original_session);
    let busy = orch
        .run_scheduled_turn(
            "cannot enter while binding",
            "test-provider/scheduled-model",
            lingxi_core::host::ReasoningSelection::Automatic,
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(busy.starts_with("busy:"));

    release_binding.send(()).unwrap();
    let (result, resumed) = tokio::join!(scheduled, resume);
    resumed.unwrap();
    let (outcome, session_id, summary) = result.unwrap();
    assert_eq!(outcome, TurnOutcome::EndTurn);
    assert_eq!(session_id, original_session);
    assert_eq!(summary, "summary belonging to the scheduled session");
    assert_eq!(orch.session.lock().await.session_id, resumed_session);
    assert!(orch.snapshot_history().await.is_empty());
}

#[tokio::test]
async fn scheduled_target_mismatch_is_retryable_without_binding_or_appending() {
    let orch = scheduled_session_fixture();
    let attempted_binding = std::sync::atomic::AtomicBool::new(false);
    let result = orch
        .run_scheduled_turn_in_session(
            lingxi_core::types::SessionId::new(),
            "must never be appended to the new foreground session",
            "test-provider/scheduled-model",
            lingxi_core::host::ReasoningSelection::Automatic,
            CancellationToken::new(),
            async {
                attempted_binding.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            },
        )
        .await;
    assert!(result.unwrap_err().starts_with("busy:"));
    assert!(!attempted_binding.load(std::sync::atomic::Ordering::SeqCst));
    assert!(orch.snapshot_history().await.is_empty());
}
