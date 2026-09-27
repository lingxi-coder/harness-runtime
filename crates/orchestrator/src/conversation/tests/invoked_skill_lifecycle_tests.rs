use super::*;
use crate::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use std::sync::Arc;

#[test]
fn dropping_orchestrator_clears_its_invoked_skills() {
    let _registry_lock = compaction::invoked_skills::TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    compaction::invoked_skills::reset_for_test();
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(Vec::new())),
        Arc::new(tool_api::registry::ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::path::PathBuf::from("/work"),
    );
    let session_id = orch
        .session
        .try_lock()
        .expect("new orchestrator is not running a turn")
        .session_id
        .to_string();
    compaction::invoked_skills::register_scoped(
        "build",
        std::path::Path::new("/skill/build"),
        "body",
        compaction::invoked_skills::InvokedSkillScopeRef::new(Some(&session_id), None),
    );

    drop(orch);

    assert!(compaction::invoked_skills::filter_for_scope(
        compaction::invoked_skills::InvokedSkillScopeRef::new(Some(&session_id), None)
    )
    .is_empty());
    compaction::invoked_skills::reset_for_test();
}
