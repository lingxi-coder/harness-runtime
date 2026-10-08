//! Host-resolved facts are admitted per query and reach the physical SDK call.
use crate::messages_288_fixture as physical;
use crate::test_support::{
    noop_hook_executor, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use crate::{ConversationOrchestrator, OrchestratorConfig, ProviderApiAdapter};
use lingxi_core::host::refusal_server::Policy;
use lingxi_core::host::OrchestratorHandle;
use physical::{captures, service, Action, MODEL};
use serde_json::json;
use std::sync::Arc;

#[tokio::test]
async fn host_query_admission_typed_disable_and_clear_reach_sdk_requests() {
    physical::variable(branding::DISABLE_REFUSAL_FALLBACK_ENV, None);
    let dir = tempfile::tempdir().unwrap();
    let capture = captures(vec![Action::Success; 4]);
    let adapter = Arc::new(ProviderApiAdapter::new(Arc::new(service(
        capture.clone(),
        true,
        false,
        MODEL,
    ))));
    let root = ConversationOrchestrator::into_shared(ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig {
            model: MODEL.into(),
            max_turns: 1,
            server_refusal_fallback: Some(Policy {
                candidate_model: Some("Target[1M]".into()),
                enabled: true,
                switch_models_on_flag: true,
                server_allowed: true,
                explicit_target_eligible: true,
                beta_transport_enabled: true,
                ..Default::default()
            }),
            ..Default::default()
        },
        adapter.clone(),
        adapter,
        Arc::new(tool_api::registry::ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        dir.path().to_owned(),
    ));
    root.attach_owned_session_switches();
    assert!(root.run_turn_streaming("first").await.is_ok());
    physical::variable(branding::DISABLE_REFUSAL_FALLBACK_ENV, Some("0"));
    assert!(root.run_turn("typed false").await.is_ok());
    physical::variable(branding::DISABLE_REFUSAL_FALLBACK_ENV, Some("1"));
    assert!(root.run_turn_streaming("disabled").await.is_ok());
    root.clear_session().await.unwrap();
    assert!(root.run_turn_streaming("cleared").await.is_ok());
    physical::variable(branding::DISABLE_REFUSAL_FALLBACK_ENV, None);
    let selected_model = root.session.lock().await.model.clone();
    let rows = capture.requests.lock().unwrap();
    assert_eq!(rows.len(), 4);
    for index in [0, 1] {
        assert_eq!(rows[index].0["fallbacks"], json!([{"model":"Target"}]));
    }
    for index in [2, 3] {
        assert!(rows[index].0.get("fallbacks").is_none());
    }
    let beta = lingxi_llm_client::providers::anthropic::fallback_request::EXPLICIT_BETA;
    let has = |index: usize| {
        rows[index]
            .1
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("anthropic-beta"))
            .any(|(_, value)| value.split(',').any(|token| token == beta))
    };
    assert!(has(0) && has(1) && has(2));
    assert!(!has(3));
    assert_eq!(selected_model, MODEL);
}
