//! Session-owned terminal callback shared by the existing foreground worker and
//! persistent agent event forwarding. No callback takes ownership of a loop.

use std::sync::{Arc, Weak};

use async_trait::async_trait;
use hooks::HookEvent;
use lingxi_core::host::subagent_spawn::{SubagentStopHookFirer, SubagentStopStatus};
use lingxi_core::types::{AgentId, SessionId};

use crate::autonomous_tool_scheduler::ToolDispatchPublicationFence;
use crate::ConversationOrchestrator;

struct SessionSubagentStopFirer {
    owner: Weak<ConversationOrchestrator>,
    session_id: SessionId,
    fence: ToolDispatchPublicationFence,
    epoch_id: lingxi_core::types::MessageId,
}

impl ConversationOrchestrator {
    pub(crate) fn bind_subagent_stop_hook_owner(self: &Arc<Self>, session_id: SessionId) {
        let (root, lock) = self
            .lifecycle_runtime
            .session_tool_hook_generation
            .current();
        self.hooks.bind_subagent_stop_firer(
            session_id,
            Arc::new(SessionSubagentStopFirer {
                owner: Arc::downgrade(self),
                session_id,
                fence: ToolDispatchPublicationFence::new(root.child_token(), lock),
                epoch_id: lingxi_core::types::MessageId::new(),
            }),
        );
    }
}

#[async_trait]
impl SubagentStopHookFirer for SessionSubagentStopFirer {
    fn epoch_id(&self) -> lingxi_core::types::MessageId {
        self.epoch_id
    }
    fn retire(&self) {
        self.fence.generation_cancellation_token().cancel();
    }
    fn is_current(&self) -> bool {
        self.fence.is_current() && self.owner.strong_count() > 0
    }

    async fn fire(&self, agent_id: AgentId, agent_type: &str, status: SubagentStopStatus) {
        let Some(owner) = self.owner.upgrade().filter(|_| self.fence.is_current()) else {
            return;
        };
        if owner.session.lock().await.session_id != self.session_id {
            return;
        }
        let mut ctx = owner.expansion_hook_context().await;
        ctx.session_id = self.session_id;
        ctx.agent_id = Some(agent_id);
        ctx.agent_type = Some(agent_type.to_owned());
        ctx.subagent_stop_epoch = Some(self.epoch_id);
        ctx.last_assistant_message = None;
        ctx.inherit = owner.hook_agent_inheritance.clone();
        ctx.publication_guard = Some(Arc::new(self.fence.clone()));
        owner.populate_stop_hook_snapshot(&mut ctx).await;
        if !self.fence.is_current() {
            return;
        }
        owner
            .hooks
            .execute_excluding_agent(
                HookEvent::SubagentStop {
                    agent_id,
                    agent_type: agent_type.to_owned(),
                    status: status.as_str().to_owned(),
                },
                ctx,
                agent_id,
            )
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        mock_message_response, MockApiClient, MockOutputStream, NoOpPermissionGate,
        StaticMemoryProvider,
    };
    use hooks::{HookContext, HookModelSelection, PromptHookTranscript};
    use std::sync::Mutex;

    struct Recorder(Mutex<Vec<(HookEvent, HookContext)>>);
    #[async_trait]
    impl hooks::BuiltinHookHandler for Recorder {
        fn id(&self) -> &str {
            "owned-subagent-stop"
        }
        async fn handle(&self, event: &HookEvent, ctx: &HookContext) -> hooks::HookResult {
            self.0.lock().unwrap().push((event.clone(), ctx.clone()));
            hooks::HookResult {
                outcome: hooks::HookOutcome::Success,
                stdout: String::new(),
                stderr: String::new(),
                exit_code: None,
                response: None,
            }
        }
    }
    struct Snapshot;
    #[async_trait]
    impl crate::stop_hook_snapshot::StopHookSnapshotProvider for Snapshot {
        async fn background_tasks(&self) -> Vec<hooks::HookBackgroundTask> {
            vec![hooks::HookBackgroundTask {
                is_idle: false,
                id: "background-build".into(),
                r#type: "shell".into(),
                status: "running".into(),
                description: "build".into(),
                command: Some("build".into()),
                agent_type: None,
                server: None,
                tool: None,
                name: None,
            }]
        }
        async fn session_crons(&self) -> Vec<hooks::HookSessionCron> {
            vec![hooks::HookSessionCron {
                id: "session-cron".into(),
                schedule: "* * * * *".into(),
                recurring: true,
                prompt: "inspect".into(),
            }]
        }
    }
    struct Budget;
    #[async_trait]
    impl lingxi_core::host::budget::BudgetEnforcerHandle for Budget {
        async fn check_and_charge(
            &self,
            _: u64,
        ) -> Result<(), lingxi_core::host::budget::BudgetError> {
            Ok(())
        }
        async fn snapshot_total_nano_usd(&self) -> u64 {
            0
        }
    }
    struct UnusedHttp;
    #[async_trait]
    impl lingxi_core::host::HttpTransport for UnusedHttp {
        async fn request(
            &self,
            _: lingxi_core::types::HttpRequest,
        ) -> Result<lingxi_core::types::HttpResponse, lingxi_core::host::HttpError> {
            Err(lingxi_core::host::HttpError::InvalidRequest(
                "unused".into(),
            ))
        }
        async fn stream_sse(
            &self,
            _: lingxi_core::types::HttpRequest,
        ) -> Result<lingxi_core::host::http::SseStream, lingxi_core::host::HttpError> {
            Err(lingxi_core::host::HttpError::InvalidRequest(
                "unused".into(),
            ))
        }
    }
    struct UnusedRuntime;
    #[async_trait]
    impl lingxi_core::host::RuntimeSpawner for UnusedRuntime {
        async fn spawn(
            &self,
            _: &str,
            _: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
        ) -> Result<lingxi_core::host::BackgroundTaskHandle, lingxi_core::host::RuntimeError>
        {
            Err(lingxi_core::host::RuntimeError::Internal("unused".into()))
        }
        async fn sleep(&self, _: std::time::Duration) {}
        async fn cancel(
            &self,
            _: &lingxi_core::host::BackgroundTaskHandle,
        ) -> Result<(), lingxi_core::host::RuntimeError> {
            Ok(())
        }
    }
    fn executor() -> (Arc<hooks::HookExecutorImpl>, Arc<Recorder>) {
        let recorder = Arc::new(Recorder(Mutex::new(Vec::new())));
        let mut registry = hooks::HookRegistry::new();
        registry.register(hooks::HookDefinition {
            id: lingxi_core::types::HookId::new(),
            name: "owned-subagent-stop".into(),
            events: vec![hooks::HookEventType::SubagentStop],
            if_condition: None,
            executor: hooks::HookExecutor::Builtin {
                handler_id: "owned-subagent-stop".into(),
            },
            source: hooks::HookSource::Settings(lingxi_core::types::SettingsScope::User),
            blocking: true,
            timeout: None,
            priority: 0,
            once: false,
            status_message: None,
            async_rewake: false,
            async_timeout: None,
            rewake_message: None,
        });
        let mut executor = hooks::HookExecutorImpl::new(
            Arc::new(tokio::sync::RwLock::new(registry)),
            Arc::new(UnusedHttp),
            Arc::new(UnusedRuntime),
        );
        executor.register_builtin(recorder.clone());
        (Arc::new(executor), recorder)
    }
    fn orchestrator(hooks: Arc<hooks::HookExecutorImpl>) -> Arc<ConversationOrchestrator> {
        let tools = Arc::new(tool_api::ToolRegistry::new());
        let inheritance = lingxi_core::host::SubagentInheritance {
            tool_invoker: Arc::new(tool_api::RegistryToolInvoker::new(tools.clone())),
            budget: Arc::new(Budget),
        };
        ConversationOrchestrator::into_shared(
            ConversationOrchestrator::new(
                crate::OrchestratorConfig::default(),
                Arc::new(MockApiClient::new(vec![mock_message_response(
                    vec![llm_runtime::ContentBlock::Text {
                        text: "parent turn complete".into(),
                        citations: None,
                        cache_control: None,
                    }],
                    Some("end_turn"),
                )])),
                tools,
                hooks,
                Arc::new(NoOpPermissionGate),
                Arc::new(MockOutputStream::new()),
                Arc::new(StaticMemoryProvider::empty()),
                std::env::temp_dir(),
            )
            .with_hook_agent_inheritance(inheritance)
            .with_stop_hook_snapshot(Arc::new(Snapshot)),
        )
    }
    fn publish(
        hooks: &hooks::HookExecutorImpl,
        session_id: SessionId,
        child: AgentId,
        owner: Arc<dyn SubagentStopHookFirer>,
        label: &str,
    ) {
        hooks.publish_agent_prompt_transcript(
            session_id,
            child,
            HookModelSelection {
                model: format!("{label}-model"),
                model_profile: Some(format!("{label}-profile")),
            },
            PromptHookTranscript {
                messages: vec![lingxi_core::types::ConversationMessage::user(
                    lingxi_core::types::MessageId::new(),
                    format!("{label}-evidence"),
                )],
                ..Default::default()
            },
            hooks::AgentStopMetadata {
                agent_transcript_path: "/tmp/child/subagents/agent-child.jsonl".into(),
                cwd: "/tmp/child-worktree".into(),
                last_assistant_message: Some("child final text".into()),
                depth: Some(4),
                owner: Some(owner),
            },
        );
    }

    #[tokio::test]
    async fn owned_terminal_preserves_child_and_host_context_across_normal_turn() {
        let (hooks, recorder) = executor();
        let orch = orchestrator(hooks.clone());
        let session_id = orch.session.lock().await.session_id;
        let owner = hooks.subagent_stop_firer(session_id).unwrap();
        orch.run_turn("advance the parent turn").await.unwrap();
        assert!(
            owner.is_current(),
            "ordinary turns retain captured terminal ownership"
        );
        let child = AgentId::new();
        publish(
            &hooks,
            session_id,
            child,
            owner.clone(),
            "skill-selected-child",
        );
        owner
            .fire(child, "reviewer", SubagentStopStatus::Completed)
            .await;
        owner
            .fire(child, "reviewer", SubagentStopStatus::Completed)
            .await;
        let seen = recorder.0.lock().unwrap();
        assert_eq!(
            seen.len(),
            1,
            "a consumed terminal snapshot cannot fire twice"
        );
        let (event, ctx) = &seen[0];
        assert!(
            matches!(event, HookEvent::SubagentStop { agent_id, status, .. }
            if *agent_id == child && status == "completed")
        );
        assert_eq!(
            ctx.model_selection.as_ref().unwrap().model,
            "skill-selected-child-model"
        );
        assert_eq!(
            ctx.model_selection
                .as_ref()
                .unwrap()
                .model_profile
                .as_deref(),
            Some("skill-selected-child-profile")
        );
        assert_eq!(
            ctx.prompt_transcript.as_ref().unwrap().messages[0].text_content(),
            "skill-selected-child-evidence"
        );
        assert_eq!(ctx.agent_depth, Some(4));
        assert_eq!(ctx.cwd, std::path::PathBuf::from("/tmp/child-worktree"));
        assert_eq!(
            ctx.agent_transcript_path.as_deref(),
            Some(std::path::Path::new(
                "/tmp/child/subagents/agent-child.jsonl"
            ))
        );
        assert_eq!(
            ctx.last_assistant_message.as_deref(),
            Some("child final text")
        );
        assert_eq!(ctx.permission_mode.as_deref(), Some("default"));
        assert!(Arc::ptr_eq(
            &ctx.inherit.as_ref().unwrap().tool_invoker,
            &orch.hook_agent_inheritance.as_ref().unwrap().tool_invoker
        ));
        assert_eq!(
            ctx.background_tasks.as_ref().unwrap()[0].id,
            "background-build"
        );
        assert_eq!(ctx.session_crons.as_ref().unwrap()[0].id, "session-cron");
    }

    #[tokio::test]
    async fn reset_rejects_late_old_publication_and_preserves_reused_child_identity() {
        let (hooks, recorder) = executor();
        let orch = orchestrator(hooks.clone());
        let session_id = orch.session.lock().await.session_id;
        let old = hooks.subagent_stop_firer(session_id).unwrap();
        let child = AgentId::new();
        publish(&hooks, session_id, child, old.clone(), "before-reset");
        orch.lifecycle_runtime
            .session_tool_hook_generation
            .reset()
            .await;
        orch.bind_subagent_stop_hook_owner(session_id);
        let fresh = hooks.subagent_stop_firer(session_id).unwrap();
        assert!(!old.is_current());
        assert_ne!(old.epoch_id(), fresh.epoch_id());
        assert!(
            hooks
                .take_agent_prompt_transcript(session_id, child)
                .is_none(),
            "reset binding drops retired buffers"
        );
        publish(&hooks, session_id, child, old.clone(), "late-old");
        assert!(
            hooks
                .take_agent_prompt_transcript(session_id, child)
                .is_none(),
            "a late old terminal leaves no orphan"
        );
        publish(&hooks, session_id, child, fresh.clone(), "new-child");
        publish(&hooks, session_id, child, old.clone(), "late-old-overwrite");
        hooks.discard_agent_prompt_transcript(session_id, child, Some(&old));
        old.fire(child, "old", SubagentStopStatus::Failed).await;
        assert!(recorder.0.lock().unwrap().is_empty());
        fresh.fire(child, "new", SubagentStopStatus::Failed).await;
        let seen = recorder.0.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(
            seen[0].1.model_selection.as_ref().unwrap().model,
            "new-child-model"
        );
        assert!(matches!(&seen[0].0, HookEvent::SubagentStop { status, .. } if status == "failed"));
        assert!(hooks
            .take_agent_prompt_transcript(session_id, child)
            .is_none());
    }

    #[tokio::test]
    async fn shared_executor_keeps_sessions_separate_and_retires_replaced_owner() {
        let (hooks, recorder) = executor();
        let first = orchestrator(hooks.clone());
        let second = orchestrator(hooks.clone());
        let a = first.session.lock().await.session_id;
        let b = second.session.lock().await.session_id;
        let owner_a = hooks.subagent_stop_firer(a).unwrap();
        let owner_b = hooks.subagent_stop_firer(b).unwrap();
        let child = AgentId::new();
        publish(&hooks, a, child, owner_a.clone(), "first");
        publish(&hooks, b, child, owner_b.clone(), "second");
        owner_b
            .fire(child, "second", SubagentStopStatus::Completed)
            .await;
        owner_a
            .fire(child, "first", SubagentStopStatus::Failed)
            .await;
        assert_eq!(recorder.0.lock().unwrap().len(), 2);
        // Cold restoration may reuse a session id while another finalized host
        // still exists. Replacing the binding retires that exact old lease.
        second.session.lock().await.session_id = a;
        second.bind_subagent_stop_hook_owner(a);
        let replacement = hooks.subagent_stop_firer(a).unwrap();
        assert!(!owner_a.is_current());
        publish(&hooks, a, child, replacement.clone(), "replacement");
        hooks.discard_agent_prompt_transcript(a, child, Some(&owner_a));
        owner_a
            .fire(child, "first", SubagentStopStatus::Failed)
            .await;
        replacement
            .fire(child, "replacement", SubagentStopStatus::Completed)
            .await;
        let seen = recorder.0.lock().unwrap();
        assert_eq!(seen.len(), 3);
        assert_eq!(
            seen[2].1.model_selection.as_ref().unwrap().model,
            "replacement-model"
        );
    }
}
