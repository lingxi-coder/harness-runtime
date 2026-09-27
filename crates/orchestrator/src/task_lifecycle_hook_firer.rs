//! Orchestrator-side [`tool_api::TaskLifecycleHookFirer`] implementation — the
//! BLOCKING `TaskCreated` / `TaskCompleted` lifecycle-hook seam for the V2
//! `Task*` TOOL path.
//!
//! ## Why a SEPARATE seam from the registry firers
//!
//! The orchestrator already hosts two fire-and-forget firers used by the
//! `TaskRegistry`: [`OrchestratorTaskCreatedFirer`](crate::OrchestratorTaskCreatedFirer)
//! and [`OrchestratorTaskCompletedFirer`](crate::OrchestratorTaskCompletedFirer).
//! Their `fire` contract is observe-only — `hooks::TaskCreatedFirer` /
//! `hooks::TaskCompletedFirer` document that implementations "MUST NOT propagate
//! hook failures", so a blocking hook on the registry path degrades to a no-op
//! and never breaks a status transition.
//!
//! claude-code's `Task*` TOOLS, however, take the hook decision into account:
//! `executeTaskCreatedHooks` (`TaskCreateTool.ts:93-113`) deletes the task and
//! throws on a blocking error, and `executeTaskCompletedHooks`
//! (`TaskUpdateTool.ts:232-265`) returns `success:false` and does NOT apply the
//! status. This adapter closes THAT seam: it REPORTS the blocking decision
//! (`Ok(())` allow / `Err(reason)` blocked) so the tool can roll back / refuse.
//! The registry firers are left UNCHANGED.
//!
//! ## Dependency shape
//!
//! `tool-api` (where [`tool_api::TaskLifecycleHookFirer`] is defined) must NOT
//! depend on `hooks`. The orchestrator depends on BOTH, so it is the natural
//! home for an impl that bridges the `tool-api`-defined trait onto the engine's
//! `Arc<hooks::HookExecutorImpl>`. The composition root (`engine-desktop`)
//! constructs one over the SAME executor + cwd it hands the registry firers and
//! injects it via `BuiltinToolContext::task_lifecycle_hooks`.
//!
//! ## Event mapping (mirrors the registry firers exactly)
//!
//! - `TaskCreated`: the `HookEvent::TaskCreated` variant names its subject field
//!   `task_type` (serialized as the wire `task_subject`) and its detail field
//!   `description` (wire `task_description`); `teammate_name` / `team_name` ride
//!   from the creating teammate's identity (TS `getAgentName()` / `getTeamName()`,
//!   `TaskCreateTool.ts:97-98`) — matching
//!   [`OrchestratorTaskCreatedFirer`](crate::OrchestratorTaskCreatedFirer).
//! - `TaskCompleted`: a full `HookEvent::TaskCompleted`; the V2 TaskUpdate tool
//!   threads the completing teammate identity from its live `ToolUseContext`,
//!   matching claude-code's `getAgentName()` / `getTeamName()` arguments.
//!
//! [`OrchestratorTaskCreatedFirer`]: crate::OrchestratorTaskCreatedFirer
//! [`OrchestratorTaskCompletedFirer`]: crate::OrchestratorTaskCompletedFirer

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use hooks::events::HookEvent;
use hooks::registry::HookContext;
use hooks::response::HookDecision;
use hooks::HookExecutorImpl;
use tool_api::TaskLifecycleHookFirer;

/// Adapts the engine's hook executor to the V2 `Task*` tool's BLOCKING
/// `TaskCreated` / `TaskCompleted` lifecycle-hook seam.
pub struct OrchestratorTaskLifecycleHookFirer {
    /// The SAME executor the orchestrator fires its other hooks through (and the
    /// SAME one the registry firers wrap).
    hooks: Arc<HookExecutorImpl>,
    /// Engine cwd, threaded into the hook payload (`cwd`) and the per-hook
    /// Command-arm `LINGXI_PROJECT_DIR` fallback.
    cwd: PathBuf,
    /// The MAIN orchestrator session's transcript path
    /// (`<config_home>/projects/<sanitize(cwd)>/<uuid>.jsonl`, claude-code
    /// `getTranscriptPathForSession`), stamped on every lifecycle hook payload's
    /// `transcript_path` (FIX B). Empty for builds wiring neither.
    transcript_path: PathBuf,
}

impl OrchestratorTaskLifecycleHookFirer {
    /// Build a firer over the shared hook executor, engine cwd, and the main
    /// session's transcript path. Pass the SAME `Arc<HookExecutorImpl>` handed to
    /// the orchestrator (and the registry firers) so the tool-path hooks ride the
    /// identical registry / async / sandbox plumbing.
    #[must_use]
    pub fn new(hooks: Arc<HookExecutorImpl>, cwd: PathBuf, transcript_path: PathBuf) -> Self {
        Self {
            hooks,
            cwd,
            transcript_path,
        }
    }

    /// Build the context-light [`HookContext`] used by every fire here. A task
    /// lifecycle transition has no live per-turn session, so the engine cwd (also
    /// the `LINGXI_PROJECT_DIR` fallback) and the main session's `transcript_path`
    /// (FIX B) are threaded; everything else defaults — matching the
    /// orchestrator's other context-light firers.
    fn ctx(&self) -> HookContext {
        HookContext {
            cwd: self.cwd.clone(),
            transcript_path: self.transcript_path.clone(),
            ..Default::default()
        }
    }

    /// Run an event through the shared executor and translate its aggregate into
    /// the tool's allow/block contract: a `Block` decision → `Err(reason)`,
    /// anything else (including no decision / an absent hook) → `Ok(())`.
    ///
    /// Faithful to claude-code's blocking gate: `executeTask{Created,Completed}Hooks`
    /// collect `blockingError`s, and the tool acts only when at least one is
    /// present — exactly the `Some(Block)` arm here.
    async fn run(&self, event: HookEvent) -> Result<(), String> {
        let agg = self.hooks.execute(event, self.ctx()).await;
        if agg.decision == Some(HookDecision::Block) {
            // claude-code's block case sets `blockingError = reason || "Blocked
            // by hook"` (capital B) uniformly across hook events; the task tool
            // then wraps it as "Task{Created,Completed} hook feedback:\n<reason>".
            Err(agg.reason.unwrap_or_else(|| "Blocked by hook".to_string()))
        } else {
            Ok(())
        }
    }
}

#[async_trait]
impl TaskLifecycleHookFirer for OrchestratorTaskLifecycleHookFirer {
    async fn fire_task_created(
        &self,
        task_id: &str,
        subject: &str,
        description: Option<&str>,
        teammate_name: Option<&str>,
        team_name: Option<&str>,
    ) -> Result<(), String> {
        // The `HookEvent::TaskCreated` variant names its subject field
        // `task_type` (wire `task_subject`) and its detail field `description`
        // (wire `task_description`); `teammate_name` / `team_name` ride from the
        // creating teammate's identity (TS `getAgentName()` / `getTeamName()`).
        let event = HookEvent::TaskCreated {
            task_id: task_id.to_string(),
            task_type: subject.to_string(),
            description: description.unwrap_or_default().to_string(),
            teammate_name: teammate_name.map(str::to_string),
            team_name: team_name.map(str::to_string),
        };
        self.run(event).await
    }

    async fn fire_task_completed(
        &self,
        task_id: &str,
        status: &str,
        subject: &str,
        description: Option<&str>,
        teammate_name: Option<&str>,
        team_name: Option<&str>,
    ) -> Result<(), String> {
        let event = HookEvent::TaskCompleted {
            task_id: task_id.to_string(),
            status: status.to_string(),
            task_subject: subject.to_string(),
            task_description: description.map(str::to_string),
            teammate_name: teammate_name.map(str::to_string),
            team_name: team_name.map(str::to_string),
        };
        self.run(event).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::noop_hook_executor;
    use async_trait::async_trait;
    use hooks::{
        HookDefinition, HookEventType, HookExecutor as DefHookExecutor, HookPromptRunner,
        HookRegistry, HookSource, PromptHookError, PromptHookRequest,
    };
    use protocol::HookId;
    use tokio::sync::RwLock;

    // ── empty-registry (no hook) → allow ────────────────────────────────────

    #[tokio::test]
    async fn no_matching_hook_allows_create_and_complete() {
        let firer = OrchestratorTaskLifecycleHookFirer::new(
            noop_hook_executor(),
            PathBuf::from("/work"),
            PathBuf::from("/work/.t.jsonl"),
        );
        assert_eq!(
            firer
                .fire_task_created("t1", "ship it", Some("do the work"), None, None)
                .await,
            Ok(()),
            "no TaskCreated hook → allow"
        );
        assert_eq!(
            firer
                .fire_task_completed(
                    "t1",
                    "completed",
                    "ship it",
                    Some("do the work"),
                    None,
                    None,
                )
                .await,
            Ok(()),
            "no TaskCompleted hook → allow"
        );
    }

    // ── a registered BLOCKING hook → Err(reason) ─────────────────────────────
    //
    // Drive the block through the `Prompt` hook arm (a one-method
    // `HookPromptRunner` seam, no process/sandbox plumbing): a `{ok:false}`
    // verdict surfaces in the aggregate as a `Block`, which `run()` must
    // translate to `Err`.

    /// Unused HTTP arm (the Prompt hook never touches it).
    struct UnusedHttp;
    #[async_trait]
    impl platform_api::HttpTransport for UnusedHttp {
        async fn request(
            &self,
            _req: protocol::HttpRequest,
        ) -> Result<protocol::HttpResponse, platform_api::HttpError> {
            Err(platform_api::HttpError::InvalidRequest("unused".into()))
        }
        async fn stream_sse(
            &self,
            _req: protocol::HttpRequest,
        ) -> Result<platform_api::http::SseStream, platform_api::HttpError> {
            Err(platform_api::HttpError::InvalidRequest("unused".into()))
        }
    }
    /// Unused runtime arm.
    struct UnusedRuntime;
    #[async_trait]
    impl platform_api::RuntimeSpawner for UnusedRuntime {
        async fn spawn(
            &self,
            _name: &str,
            _task: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
        ) -> Result<platform_api::BackgroundTaskHandle, platform_api::RuntimeError> {
            Err(platform_api::RuntimeError::Internal("unused".into()))
        }
        async fn sleep(&self, _d: std::time::Duration) {}
        async fn cancel(
            &self,
            _h: &platform_api::BackgroundTaskHandle,
        ) -> Result<(), platform_api::RuntimeError> {
            Ok(())
        }
    }
    /// A `HookPromptRunner` returning a fixed `{ok:false,reason}` verdict body.
    struct BlockingRunner(String);
    #[async_trait]
    impl HookPromptRunner for BlockingRunner {
        async fn run(&self, _req: PromptHookRequest) -> Result<String, PromptHookError> {
            Ok(format!(r#"{{"ok": false, "reason": "{}"}}"#, self.0))
        }
    }

    /// Build an executor whose registry holds ONE blocking Prompt hook for
    /// `event_type`, wired to a runner that always blocks with `reason`.
    fn blocking_executor_for(event_type: HookEventType, reason: &str) -> Arc<HookExecutorImpl> {
        let mut registry = HookRegistry::new();
        registry.register(HookDefinition {
            id: HookId::new(),
            name: "blocking-test".into(),
            events: vec![event_type],
            if_condition: None,
            executor: DefHookExecutor::Prompt {
                prompt: "block? $ARGUMENTS".into(),
                model: None,
                continue_on_block: false,
            },
            source: HookSource::Settings(protocol::SettingsScope::User),
            blocking: true,
            timeout: None,
            priority: 0,
            once: false,
            status_message: None,
            async_rewake: false,
            async_timeout: None,
            rewake_message: None,
        });
        let reg = Arc::new(RwLock::new(registry));
        Arc::new(
            HookExecutorImpl::new(reg, Arc::new(UnusedHttp), Arc::new(UnusedRuntime))
                .with_prompt_runner(Arc::new(BlockingRunner(reason.to_string()))),
        )
    }

    #[tokio::test]
    async fn blocking_task_created_hook_returns_err_with_reason() {
        let firer = OrchestratorTaskLifecycleHookFirer::new(
            blocking_executor_for(HookEventType::TaskCreated, "creation denied"),
            PathBuf::from("/work"),
            PathBuf::from("/work/.t.jsonl"),
        );
        let res = firer
            .fire_task_created("t1", "ship it", Some("desc"), None, None)
            .await;
        // The Prompt arm surfaces the block as `[<hook prompt>]: <reason>`.
        assert_eq!(
            res,
            Err("[block? $ARGUMENTS]: creation denied".to_string()),
            "a blocking TaskCreated hook must surface as Err(reason)"
        );
    }

    #[tokio::test]
    async fn blocking_task_completed_hook_returns_err_with_reason() {
        let firer = OrchestratorTaskLifecycleHookFirer::new(
            blocking_executor_for(HookEventType::TaskCompleted, "not done yet"),
            PathBuf::from("/work"),
            PathBuf::from("/work/.t.jsonl"),
        );
        let res = firer
            .fire_task_completed("t1", "completed", "ship it", Some("desc"), None, None)
            .await;
        assert_eq!(
            res,
            Err("[block? $ARGUMENTS]: not done yet".to_string()),
            "a blocking TaskCompleted hook must surface as Err(reason)"
        );
    }
}
