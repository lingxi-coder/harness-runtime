//! Orchestrator-side [`TaskNotificationProvider`] backed by the live
//! [`TaskRegistryHandle`].
//!
//! T35: the orchestrator folds a `<task-notification>` reminder into each turn
//! (see [`ConversationOrchestrator::task_notification_reminder_messages`]) by
//! draining the registry's terminal-not-notified tasks. The orchestrator names
//! the registry only through the narrow `platform_api::task_registry::TaskRegistryHandle`
//! seam (it has no dependency on the `tasks` crate), so this adapter closes the
//! seam from the orchestrator side: it owns an `Arc<dyn TaskRegistryHandle>` —
//! the SAME registry handle the composition root hands the tool context — and
//! forwards [`TaskNotificationProvider::take_pending_task_notifications`] to the
//! registry's `take_pending_task_notifications` (which snapshots + marks-notified
//! + evicts, so each completion surfaces exactly once).
//!
//! Best-effort: a registry error degrades to "nothing to surface" (empty) so a
//! transient registry failure never breaks the turn — matching the fold's
//! `None`-on-empty contract.
//!
//! Wiring: the composition root (`engine-desktop`) builds this over the SAME
//! `Arc` it registers as the `task_registry`, then injects it via
//! [`ConversationOrchestrator::with_task_notifications`].
//!
//! [`ConversationOrchestrator::task_notification_reminder_messages`]:
//!     crate::ConversationOrchestrator
//! [`ConversationOrchestrator::with_task_notifications`]:
//!     crate::ConversationOrchestrator

use std::sync::Arc;

use async_trait::async_trait;
use platform_api::task_registry::{TaskNotification, TaskRegistryHandle};

use crate::prompt::task_notification::TaskNotificationProvider;

/// [`TaskNotificationProvider`] that drains the live registry each turn.
pub struct RegistryTaskNotifications {
    registry: Arc<dyn TaskRegistryHandle>,
}

impl RegistryTaskNotifications {
    /// Wrap the registry handle the composition root shares with the tool
    /// context.
    #[must_use]
    pub fn new(registry: Arc<dyn TaskRegistryHandle>) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl TaskNotificationProvider for RegistryTaskNotifications {
    fn subscribe_task_lifecycle(
        &self,
    ) -> Option<tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>> {
        self.registry.subscribe_task_lifecycle()
    }

    fn update_shell_session_activity(&self, interactive: bool, busy: bool, user_interaction: bool) {
        self.registry
            .update_shell_session_activity(interactive, busy, user_interaction);
    }

    async fn take_pending_task_notifications(&self) -> Vec<TaskNotification> {
        // Best-effort: an error degrades to "nothing to surface" so a transient
        // registry failure never breaks the turn.
        self.registry
            .take_pending_task_notifications()
            .await
            .unwrap_or_default()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use platform_api::task_registry::{
        TaskCreateInput, TaskListFilter, TaskOutputChunk, TaskRecord, TaskRegistryError,
        TaskUpdatePatch,
    };
    use std::sync::Mutex;

    /// A `TaskRegistryHandle` whose only meaningful method is the
    /// notification drain — every other method is unreachable in these tests.
    struct FakeRegistry(Mutex<Vec<TaskNotification>>, Mutex<Vec<(bool, bool, bool)>>);
    impl FakeRegistry {
        fn new(notes: Vec<TaskNotification>) -> Self {
            Self(Mutex::new(notes), Mutex::new(Vec::new()))
        }
        fn complete(&self) {
            self.0.lock().unwrap().push(TaskNotification {
                task_id: "done".into(),
                task_type: "local_bash".into(),
                status: "completed".into(),
                description: "background result".into(),
                ..Default::default()
            });
        }
    }

    #[async_trait]
    impl TaskRegistryHandle for FakeRegistry {
        fn update_shell_session_activity(&self, interactive: bool, busy: bool, human: bool) {
            self.1.lock().unwrap().push((interactive, busy, human));
        }
        async fn has_pending_task_notifications_for(
            &self,
            recipient: Option<protocol::AgentId>,
        ) -> bool {
            recipient.is_none() && !self.0.lock().unwrap().is_empty()
        }

        async fn create(&self, _i: TaskCreateInput) -> Result<TaskRecord, TaskRegistryError> {
            unreachable!()
        }
        async fn get(&self, _id: &str) -> Result<Option<TaskRecord>, TaskRegistryError> {
            unreachable!()
        }
        async fn list(&self, _f: TaskListFilter) -> Result<Vec<TaskRecord>, TaskRegistryError> {
            unreachable!()
        }
        async fn update(
            &self,
            _id: &str,
            _p: TaskUpdatePatch,
        ) -> Result<TaskRecord, TaskRegistryError> {
            unreachable!()
        }
        async fn set_status(&self, _id: &str, _s: &str) -> Result<TaskRecord, TaskRegistryError> {
            unreachable!()
        }
        async fn kill(&self, _id: &str) -> Result<TaskRecord, TaskRegistryError> {
            unreachable!()
        }
        async fn output(
            &self,
            _id: &str,
            _o: Option<u64>,
        ) -> Result<TaskOutputChunk, TaskRegistryError> {
            unreachable!()
        }
        async fn take_pending_task_notifications(
            &self,
        ) -> Result<Vec<TaskNotification>, TaskRegistryError> {
            // Drain once (consume-once), mirroring the real registry.
            Ok(std::mem::take(&mut *self.0.lock().unwrap()))
        }
    }

    #[tokio::test]
    async fn sdk_lifecycle_relay_emits_without_a_model_turn_and_stops_on_drop() {
        use crate::test_support::*;
        struct Provider(Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>>>);
        #[async_trait]
        impl TaskNotificationProvider for Provider {
            fn subscribe_task_lifecycle(
                &self,
            ) -> Option<tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>> {
                self.0.lock().unwrap().take()
            }
            async fn take_pending_task_notifications(&self) -> Vec<TaskNotification> {
                vec![]
            }
        }
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let output = Arc::new(MockOutputStream::new());
        let orch = crate::ConversationOrchestrator::new_with_streaming(
            crate::OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            Arc::new(MockStreamingApiClient::with_turns(vec![])),
            Arc::new(tool_api::ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            output.clone(),
            Arc::new(StaticMemoryProvider::empty()),
            std::env::temp_dir(),
        )
        .with_task_notifications(Arc::new(Provider(Mutex::new(Some(receiver)))));
        let event =
            serde_json::json!({"type":"system", "subtype":"task_started", "task_id":"b12345678"});
        sender.send(event.clone()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while output.lifecycle_event_snapshot().await.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(output.lifecycle_event_snapshot().await, vec![event]);
        drop(orch);
        tokio::time::timeout(std::time::Duration::from_secs(2), sender.closed())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn forwards_drain_to_registry_then_empty() {
        let n = TaskNotification {
            task_id: "b1".into(),
            task_type: "local_bash".into(),
            status: "completed".into(),
            description: "x".into(),
            tool_use_id: None,
            output_path: None,
            exit_code: Some(0),
            error: None,
            result: None,
            usage: None,
            killed_by: None,
            worktree_path: None,
            worktree_branch: None,
            workflow_failures: Vec::new(),
            workflow_agent_count: None,
            workflow_total_tokens: None,
            workflow_total_tool_calls: None,
            workflow_duration_ms: None,
            ..Default::default()
        };
        let reg = Arc::new(FakeRegistry::new(vec![n.clone()]));
        let provider = RegistryTaskNotifications::new(reg);
        assert_eq!(provider.take_pending_task_notifications().await, vec![n]);
        // Consume-once: the registry drained, so the second call is empty.
        assert!(provider.take_pending_task_notifications().await.is_empty());
    }
    fn wake_orchestrator(
        registry: Arc<FakeRegistry>,
    ) -> (
        Arc<crate::ConversationOrchestrator>,
        Arc<crate::test_support::MockStreamingApiClient>,
        Arc<crate::test_support::MockOutputStream>,
    ) {
        use crate::test_support::*;
        let output = Arc::new(MockOutputStream::new());
        let api = Arc::new(MockStreamingApiClient::with_turns(vec![vec![
            message_start("wake", "claude-opus-4-7"),
            content_block_start_text(0),
            text_delta(0, "received"),
            content_block_stop(0),
            message_delta_stop("end_turn"),
            message_stop(),
        ]]));
        let orch = crate::ConversationOrchestrator::new_with_streaming(
            crate::OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            api.clone(),
            Arc::new(tool_api::ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            output.clone(),
            Arc::new(StaticMemoryProvider::empty()),
            std::env::temp_dir(),
        )
        .with_task_notifications(Arc::new(RegistryTaskNotifications::new(registry)));
        (Arc::new(orch), api, output)
    }

    #[tokio::test]
    async fn idle_wake_delivers_preexisting_completion_without_a_human_prompt() {
        let registry = Arc::new(FakeRegistry::new(vec![]));
        registry.complete(); // No subscriber yet: startup must check existing state.
        let (orch, api, output) = wake_orchestrator(registry.clone());
        orch.run_task_notification_rewake(
            registry.as_ref(),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
        // A rewake is a turn NOBODY submitted, so it has to say so: hosts that
        // track turn liveness (Desktop's bridge, mobile's turn listener) learn
        // about a turn only from its start, and drop every event — and every
        // permission request — belonging to one they were never told about.
        assert_eq!(
            output.turn_start_count().await,
            1,
            "the rewake must announce itself exactly once",
        );
        let calls = api.captured_calls().await;
        assert_eq!(calls.len(), 1);
        let content = calls[0]
            .messages
            .iter()
            .map(|m| m.text_content())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(content.contains(platform_api::task_notification::NON_USER_INPUT_HEADER));
        assert!(!content.contains(platform_api::task_notification::IN_HUMAN_TURN_HEADER));
        assert!(calls[0]
            .messages
            .iter()
            .all(|m| m.is_meta() || !m.text_content().is_empty()));
    }

    #[tokio::test]
    async fn idle_wake_waits_for_active_turn_and_rechecks_consumed_completion() {
        let registry = Arc::new(FakeRegistry::new(vec![]));
        let (orch, api, _output) = wake_orchestrator(registry.clone());
        let gate = orch.turn_gate.lock().await;
        registry.complete();
        let wake_orch = orch.clone();
        let wake_registry = registry.clone();
        let wake = tokio::spawn(async move {
            wake_orch
                .run_task_notification_rewake(
                    wake_registry.as_ref(),
                    tokio_util::sync::CancellationToken::new(),
                )
                .await
                .unwrap();
        });
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        assert!(
            api.captured_calls().await.is_empty(),
            "busy owner must not be interrupted"
        );
        registry.take_pending_task_notifications().await.unwrap();
        drop(gate);
        wake.await.unwrap();
        assert!(
            api.captured_calls().await.is_empty(),
            "active turn already consumed the completion"
        );
        registry.complete();
        orch.run_task_notification_rewake(
            registry.as_ref(),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(api.captured_calls().await.len(), 1);
    }

    #[tokio::test]
    async fn human_turn_completion_uses_the_same_turn_provenance_header() {
        let registry = Arc::new(FakeRegistry::new(vec![]));
        registry.complete();
        let (orch, api, _output) = wake_orchestrator(registry.clone());
        orch.run_turn_streaming("a genuine user message")
            .await
            .unwrap();
        let calls = api.captured_calls().await;
        let texts = calls[0]
            .messages
            .iter()
            .map(|m| m.text_content())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(texts.contains(platform_api::task_notification::IN_HUMAN_TURN_HEADER));
        assert!(!texts.contains(platform_api::task_notification::NON_USER_INPUT_HEADER));
        assert!(texts.contains("a genuine user message"));
        let activity = registry.1.lock().unwrap();
        assert!(activity.first().unwrap().1 && activity.first().unwrap().2);
        assert!(!activity.last().unwrap().1 && !activity.last().unwrap().2);
    }
}
