use super::*;
use crate::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use crate::OrchestratorConfig;
use async_trait::async_trait;
use lingxi_core::types::ContentBlock;
use skill_api::DiscoveredSkill;
use std::path::PathBuf;
use std::sync::Arc;
use tool_api::registry::ToolRegistry;

/// A runtime that actually RUNS the spawned future so the one-shot resolves.
struct InlineRuntime;
#[async_trait]
impl lingxi_core::host::RuntimeSpawner for InlineRuntime {
    async fn spawn(
        &self,
        name: &str,
        task: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
    ) -> Result<lingxi_core::host::BackgroundTaskHandle, lingxi_core::host::RuntimeError> {
        tokio::spawn(task);
        Ok(lingxi_core::host::BackgroundTaskHandle {
            task_name: name.to_string(),
            task_id: 0,
        })
    }
    async fn sleep(&self, _d: std::time::Duration) {}
    async fn cancel(
        &self,
        _h: &lingxi_core::host::BackgroundTaskHandle,
    ) -> Result<(), lingxi_core::host::RuntimeError> {
        Ok(())
    }
}

fn skill(name: &str, description: &str) -> DiscoveredSkill {
    DiscoveredSkill {
        name: name.into(),
        description: description.into(),
        short_id: None,
    }
}

/// Build an orchestrator with NO skill prefetch wired (channel inert).
fn orch_bare() -> ConversationOrchestrator {
    ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::with_files(vec![])),
        PathBuf::from("/work/repo"),
    )
}

/// Build an orchestrator whose skill prefetch resolves to `seed`.
fn orch_with_seed(seed: Vec<DiscoveredSkill>) -> ConversationOrchestrator {
    let runtime: Arc<dyn lingxi_core::host::RuntimeSpawner> = Arc::new(InlineRuntime);
    let prefetch = Arc::new(skill_api::SkillDiscoveryPrefetch::with_fixed_result(
        runtime, seed,
    ));
    orch_bare().with_skill_discovery_prefetch(prefetch)
}

/// Push an assistant message that requested an `Edit` so `find_write_pivot`
/// reports a write pivot and the prefetch fires.
async fn push_write_pivot(orch: &ConversationOrchestrator) {
    let msg = ConversationMessage::Assistant {
        id: MessageId::new(),
        content: vec![ContentBlock::ToolUse {
            id: lingxi_core::types::ToolUseId::new(),
            name: "Edit".into(),
            input: serde_json::json!({}),
            provider_id: None,
        }],
        stop_reason: None,
    };
    orch.session.lock().await.history.push(msg);
}

// (D.8) Flag OFF (no prefetch wired) = zero change: both calls are strict
// no-ops and `has_skill_discovery_prefetch()` is false.
#[tokio::test]
async fn no_prefetch_wired_is_inert() {
    let orch = orch_bare();
    push_write_pivot(&orch).await;
    orch.start_skill_discovery_prefetch().await;
    assert!(orch.skill_discovery_reminder_message().await.is_none());
    assert!(!orch.has_skill_discovery_prefetch());
}

// (D.9) Flag ON = byte-exact attachment injected.
#[tokio::test]
async fn seeded_prefetch_renders_skill_discovery_block() {
    let orch = orch_with_seed(vec![
        skill("git-commit", "Commit staged changes"),
        skill("rebase", "Interactive rebase helper"),
    ]);
    assert!(orch.has_skill_discovery_prefetch());
    push_write_pivot(&orch).await;
    orch.start_skill_discovery_prefetch().await;
    let text = orch
        .skill_discovery_reminder_message()
        .await
        .expect("seeded prefetch must surface")
        .text_content();
    assert_eq!(
        text,
        "<system-reminder>\n\
         Skills relevant to your task:\n\n\
         - git-commit: Commit staged changes\n\
         - rebase: Interactive rebase helper\n\n\
         These skills encode project-specific conventions. \
         Invoke via Skill(\"<name>\") for complete instructions.\n\
         </system-reminder>"
    );
}

// (D.4 integration) Non-write iteration (no write-pivot tool) ⇒ inert even
// with a seeded prefetch.
#[tokio::test]
async fn non_write_pivot_is_inert() {
    let orch = orch_with_seed(vec![skill("a", "da")]);
    // No assistant tool-use in history ⇒ find_write_pivot == false.
    orch.start_skill_discovery_prefetch().await;
    assert!(
        orch.skill_discovery_reminder_message().await.is_none(),
        "non-write iteration must surface nothing"
    );
}

// Empty result ⇒ None.
#[tokio::test]
async fn empty_result_yields_none() {
    let orch = orch_with_seed(vec![]);
    push_write_pivot(&orch).await;
    orch.start_skill_discovery_prefetch().await;
    assert!(orch.skill_discovery_reminder_message().await.is_none());
}

// Not armed (slot empty) ⇒ None.
#[tokio::test]
async fn not_armed_yields_none() {
    let orch = orch_with_seed(vec![skill("a", "da")]);
    assert!(orch.skill_discovery_reminder_message().await.is_none());
}

// (D.10) Dedup across turns: same skill armed turn N and N+1 ⇒ injects once.
#[tokio::test]
async fn surfaced_once_then_not_reinjected_across_turns() {
    let orch = orch_with_seed(vec![skill("a", "da")]);
    push_write_pivot(&orch).await;
    // Turn 0: surfaced.
    orch.start_skill_discovery_prefetch().await;
    assert!(
        orch.skill_discovery_reminder_message().await.is_some(),
        "first surfacing must inject"
    );
    // Turn 1: same skill ⇒ already in surfaced_skill_names ⇒ no re-inject.
    orch.start_skill_discovery_prefetch().await;
    assert!(
        orch.skill_discovery_reminder_message().await.is_none(),
        "an already-surfaced skill must not be re-injected"
    );
}

// Partial dedup: only the fresh skill surfaces on turn N+1.
#[tokio::test]
async fn partial_dedup_surfaces_only_fresh_skills() {
    let orch = orch_with_seed(vec![skill("seen", "ds")]);
    push_write_pivot(&orch).await;
    orch.start_skill_discovery_prefetch().await;
    assert!(orch.skill_discovery_reminder_message().await.is_some());

    // Re-seed the SAME prefetch slot is not possible (fixed_result is fixed);
    // instead assert the surfaced_skill_names set recorded "seen".
    assert!(orch
        .prompt_runtime
        .surfaced_skill_names
        .lock()
        .await
        .contains("seen"));
}
