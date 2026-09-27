use super::*;
use crate::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, StaticMemoryProvider,
};

struct LivePermissionGate(std::sync::RwLock<String>);

#[async_trait::async_trait]
impl platform_api::PermissionGate for LivePermissionGate {
    async fn check(
        &self,
        _tool_name: &str,
        _input: &serde_json::Value,
    ) -> platform_api::PermissionDecision {
        platform_api::PermissionDecision::Allow
    }

    async fn set_permission_mode(&self, mode: &str) -> Result<(), String> {
        if !matches!(
            mode,
            "default" | "plan" | "acceptEdits" | "bypassPermissions"
        ) {
            return Err("invalid permission mode".into());
        }
        *self.0.write().unwrap() = mode.to_owned();
        Ok(())
    }

    fn permission_mode(&self) -> Option<String> {
        Some(self.0.read().unwrap().clone())
    }
}

fn orchestrator() -> ConversationOrchestrator {
    ConversationOrchestrator::new(
        crate::OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(tool_api::registry::ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(LivePermissionGate(std::sync::RwLock::new("default".into()))),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::with_files(vec![])),
        std::path::PathBuf::from("/work/repo"),
    )
}

#[tokio::test]
async fn live_permission_mode_exits_a_tool_entered_plan() {
    for mode in ["default", "acceptEdits", "bypassPermissions"] {
        let orch = orchestrator();
        // EnterPlanMode sets this independently of the policy gate.
        orch.session.lock().await.plan_mode = true;
        orch.set_permission_mode(mode).await.unwrap();
        assert_eq!(orch.permission_mode().as_deref(), Some(mode));
        assert!(!orch.session.lock().await.plan_mode);
        assert!(orch.plan_mode_turn_messages().await.is_empty());
        assert!(orch.plan_mode_exit_message().await.is_some());
        assert!(orch.plan_mode_exit_message().await.is_none());
        assert!(orch.session.lock().await.plan_mode_exited);
    }
}

#[tokio::test]
async fn live_permission_mode_enters_plan_and_rearms_reminders_only_on_transition() {
    let orch = orchestrator();
    orch.set_permission_mode("plan").await.unwrap();
    assert!(orch.session.lock().await.plan_mode);
    assert!(!orch.plan_mode_turn_messages().await.is_empty());
    assert!(orch.session.lock().await.plan_reminder_shown);
    orch.set_permission_mode("plan").await.unwrap();
    assert!(orch.session.lock().await.plan_reminder_shown);

    orch.set_permission_mode("default").await.unwrap();
    orch.set_permission_mode("plan").await.unwrap();
    let session = orch.session.lock().await;
    assert!(session.plan_mode);
    assert!(!session.plan_reminder_shown);
    assert!(!session.plan_mode_exit_pending);
}

#[tokio::test]
async fn rejected_live_permission_mode_preserves_plan_state() {
    let orch = orchestrator();
    orch.set_permission_mode("plan").await.unwrap();
    assert!(orch.set_permission_mode("invalid").await.is_err());
    assert_eq!(orch.permission_mode().as_deref(), Some("plan"));
    let session = orch.session.lock().await;
    assert!(session.plan_mode);
    assert!(!session.plan_mode_exit_pending);
}
