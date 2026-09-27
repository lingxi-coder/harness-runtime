//! Foreground registration and the non-cancelling Ctrl+B handoff (`fln/s9/mln`).
use async_trait::async_trait;
use platform_api::subagent_spawn::{AsyncLaunch, SubagentObservation, SubagentSpawnObserver};
use platform_api::task_registry::{
    AgentRunUsage, AgentTerminalOutcome, ForegroundAgentHandle, ForegroundAgentRegistration,
    TaskBackgrounder, TaskKiller, TaskRegistryHandle,
};
use platform_api::{
    SubagentInheritance, SubagentResult, SubagentSpawnError, SubagentSpawnRequest, SubagentSpawner,
};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, watch};

pub(super) enum ForegroundResult {
    Finished(
        Result<SubagentResult, SubagentSpawnError>,
        Option<(String, String)>,
    ),
    Backgrounded(AsyncLaunch, String),
}

struct Control {
    registry: Arc<dyn TaskRegistryHandle>,
    spawner: Arc<dyn SubagentSpawner>,
    request: SubagentSpawnRequest,
    inheritance: SubagentInheritance,
    handle: tokio::sync::Mutex<Option<ForegroundAgentHandle>>,
    abort: Mutex<Option<tokio::task::AbortHandle>>,
    stop_requested: Arc<std::sync::atomic::AtomicBool>,
    allocated_agent_id: Mutex<Option<protocol::AgentId>>,
    signal: watch::Sender<bool>,
    hint_progress: tool_api::ToolProgressSender,
    tool_use_id: Option<protocol::ToolUseId>,
}

struct TaskControl(std::sync::Weak<Control>);
#[async_trait]
impl TaskBackgrounder for TaskControl {
    async fn background(&self) {
        if let Some(control) = self.0.upgrade() {
            control.signal.send_replace(true);
        }
    }
}
#[async_trait]
impl TaskKiller for TaskControl {
    async fn kill(&self) {
        if let Some(control) = self.0.upgrade() {
            control
                .stop_requested
                .store(true, std::sync::atomic::Ordering::SeqCst);
            if let Some(handle) = control.abort.lock().unwrap().as_ref() {
                handle.abort();
            }
        }
    }
}
#[async_trait]
impl platform_api::task_registry::TaskMessageReceiver for TaskControl {
    async fn send(
        &self,
        message: String,
    ) -> Result<(), platform_api::task_registry::TaskRegistryError> {
        let control = self.0.upgrade().ok_or_else(|| {
            platform_api::task_registry::TaskRegistryError::Internal("agent loop ended".into())
        })?;
        let id = (*control.allocated_agent_id.lock().unwrap()).ok_or_else(|| {
            platform_api::task_registry::TaskRegistryError::Internal("agent not allocated".into())
        })?;
        let task_id = control
            .handle
            .lock()
            .await
            .as_ref()
            .map(|handle| handle.task_id.clone())
            .ok_or_else(|| {
                platform_api::task_registry::TaskRegistryError::Internal(
                    "agent task not registered".into(),
                )
            })?;
        let record = control.registry.get(&task_id).await?.ok_or_else(|| {
            platform_api::task_registry::TaskRegistryError::NotFound(task_id.clone())
        })?;
        if record.status != "running" && !record.is_parked {
            return Err(platform_api::task_registry::TaskRegistryError::NotFound(
                task_id,
            ));
        }
        if control
            .registry
            .set_status(&task_id, "running")
            .await?
            .status
            != "running"
        {
            return Err(platform_api::task_registry::TaskRegistryError::NotFound(
                task_id,
            ));
        }
        if let Err(error) = control.spawner.resume_foreground(&id, message).await {
            // A closed pool channel cannot leave a task advertising active work.
            let _ = control.registry.set_status(&task_id, "failed").await;
            return Err(platform_api::task_registry::TaskRegistryError::Internal(
                error.to_string(),
            ));
        }
        Ok(())
    }
}
struct Observer(Arc<Control>);
#[async_trait]
impl SubagentSpawnObserver for Observer {
    async fn on_model_selected(&self, event: &SubagentObservation, effort: Option<&str>) {
        if let SubagentObservation::Allocated { model, .. } = event {
            if let Some(handle) = self.0.handle.lock().await.as_ref() {
                self.0
                    .registry
                    .set_agent_display(&handle.task_id, model.clone(), effort.map(str::to_string))
                    .await;
            }
        }
    }

    async fn before_start(&self, event: &SubagentObservation) -> Result<(), SubagentSpawnError> {
        if let SubagentObservation::Allocated {
            agent_id,
            agent_type,
            model,
            ..
        } = event.clone()
        {
            let request = &self.0.request;
            let registration = ForegroundAgentRegistration {
                agent_id,
                agent_type,
                description: request.description.clone().unwrap_or_default(),
                prompt: request.prompt.clone(),
                tool_use_id: request.tool_use_id.clone(),
                creator_agent_id: request.creator_agent_id,
                creator_teammate_name: request.creator_teammate_name.clone(),
                creator_team_name: request.creator_team_name.clone(),
            };
            *self.0.allocated_agent_id.lock().unwrap() = Some(agent_id);
            match self
                .0
                .registry
                .register_foreground_agent(registration)
                .await
            {
                Ok(handle) => {
                    let id = handle.task_id.clone();
                    self.0
                        .registry
                        .set_agent_display(
                            &id,
                            model,
                            request
                                .effort
                                .as_ref()
                                .and_then(|value| value.as_str())
                                .map(str::to_string),
                        )
                        .await;
                    *self.0.handle.lock().await = Some(handle);
                    // Save the exact launch capability bundle after its stable task
                    // alias exists and before the startup gate releases the model.
                    self.0
                        .registry
                        .register_agent_resume_recipe(
                            &id,
                            request.clone(),
                            self.0.inheritance.clone(),
                        )
                        .await
                        .map_err(|error| SubagentSpawnError::Internal(error.to_string()))?;
                    if let Some(path) = self.0.spawner.transcript_path(agent_id) {
                        let _ = self.0.registry.link_agent_output(&id, &path).await;
                    }
                    let task_control = Arc::new(TaskControl(Arc::downgrade(&self.0)));
                    self.0
                        .registry
                        .bind_background_killer(&id, task_control.clone())
                        .await
                        .map_err(|error| SubagentSpawnError::Internal(error.to_string()))?;
                    self.0
                        .registry
                        .bind_background_requester(&id, task_control.clone())
                        .await
                        .map_err(|error| SubagentSpawnError::Internal(error.to_string()))?;
                    self.0
                        .registry
                        .bind_agent_message_receiver(&id, task_control)
                        .await
                        .map_err(|error| SubagentSpawnError::Internal(error.to_string()))?;
                    self.0
                        .spawner
                        .connect_foreground_route(agent_id, &id, None)
                        .await?;
                    if let Some(tool_use_id) = self.0.tool_use_id.clone() {
                        let weak = Arc::downgrade(&self.0);
                        tokio::spawn(async move {
                            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                            if let Some(control) = weak.upgrade() {
                                let active = control
                                    .abort
                                    .lock()
                                    .unwrap()
                                    .as_ref()
                                    .is_some_and(|handle| !handle.is_finished());
                                if active && !*control.signal.borrow() {
                                    let _ = control.hint_progress.send(tool_api::ToolProgress {tool_use_id: tool_use_id.clone(), data: serde_json::json!({"kind": "background_hint", "toolUseId": tool_use_id})}).await;
                                }
                            }
                        });
                    }
                }
                Err(error) => {
                    return Err(SubagentSpawnError::Internal(format!(
                        "foreground agent registration failed: {error}"
                    )))
                }
            }
        }
        Ok(())
    }
    async fn on_event(&self, _event: SubagentObservation) {}
}

/// Abandoning a foreground tool cancels its worker; a requested background
/// handoff deliberately releases that cancellation ownership.
struct CancelOnDrop(
    Option<tokio::task::AbortHandle>,
    Arc<std::sync::atomic::AtomicBool>,
);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            self.1.store(true, std::sync::atomic::Ordering::SeqCst);
            handle.abort();
        }
    }
}

pub(super) async fn run(
    spawner: Arc<dyn SubagentSpawner>,
    request: SubagentSpawnRequest,
    inherit: SubagentInheritance,
    progress: mpsc::Sender<String>,
    hint_progress: tool_api::ToolProgressSender,
    tool_use_id: Option<protocol::ToolUseId>,
    registry: Arc<dyn TaskRegistryHandle>,
    ctx: tool_api::BuiltinToolContext,
) -> ForegroundResult {
    let worktree = request.worktree.clone();
    let (signal, mut changed) = watch::channel(false);
    let control = Arc::new(Control {
        registry: registry.clone(),
        spawner: spawner.clone(),
        request: request.clone(),
        inheritance: inherit.clone(),
        handle: Default::default(),
        abort: Default::default(),
        stop_requested: Default::default(),
        allocated_agent_id: Default::default(),
        signal,
        hint_progress,
        tool_use_id,
    });
    let observer = Arc::new(Observer(control.clone()));
    let (release_worker, worker_start) = tokio::sync::oneshot::channel();
    let worker = tokio::spawn(async move {
        worker_start
            .await
            .map_err(|_| SubagentSpawnError::Internal("foreground launch cancelled".into()))?;
        spawner
            .spawn_with_observer(request, inherit, Some(progress), Some(observer))
            .await
    });
    let mut guard = CancelOnDrop(Some(worker.abort_handle()), control.stop_requested.clone());
    *control.abort.lock().unwrap() = Some(worker.abort_handle());
    let _ = release_worker.send(());
    let completion_control = control.clone();
    let mut completion = tokio::spawn(async move {
        let outcome = worker.await.unwrap_or_else(|error| {
            if error.is_cancelled()
                && completion_control
                    .stop_requested
                    .load(std::sync::atomic::Ordering::SeqCst)
            {
                if let Some(agent_id) = *completion_control.allocated_agent_id.lock().unwrap() {
                    return Ok(SubagentResult::Killed { agent_id });
                }
            }
            Err(SubagentSpawnError::Internal(format!(
                "foreground agent worker: {error}"
            )))
        });
        let worktree_result = match worktree {
            Some(handle) => {
                platform_api::worktree::agent_worktree_result(ctx.worktree.as_ref(), &handle).await
            }
            None => None,
        };
        if let Some(handle) = completion_control.handle.lock().await.as_ref() {
            // Removal and a Ctrl+B request serialize at the registry write
            // lock. A retained row belongs to the background lifecycle.
            registry.unregister_foreground_agent(&handle.task_id).await;
            if matches!(registry.get(&handle.task_id).await, Ok(Some(_))) {
                let allocated = *completion_control.allocated_agent_id.lock().unwrap();
                if let Some(path) =
                    allocated.and_then(|id| completion_control.spawner.transcript_path(id))
                {
                    let _ = registry.link_agent_output(&handle.task_id, &path).await;
                }
                let mut terminal = AgentTerminalOutcome::default();
                let status = match &outcome {
                    Ok(SubagentResult::Completed {
                        content,
                        total_tokens,
                        total_tool_use_count,
                        total_duration_ms,
                        ..
                    }) => {
                        terminal.result = Some(super::extract_content_texts(content).join("\n"));
                        terminal.max_turns_reached = super::max_turns_reached_from_result(content);
                        terminal.usage = Some(AgentRunUsage {
                            subagent_tokens: *total_tokens,
                            tool_uses: *total_tool_use_count,
                            duration_ms: *total_duration_ms,
                        });
                        "completed"
                    }
                    Ok(SubagentResult::Failed { reason, .. }) => {
                        terminal.error = Some(reason.clone());
                        "failed"
                    }
                    Ok(SubagentResult::Killed { .. }) => "killed",
                    Err(error) => {
                        terminal.error = Some(error.to_string());
                        "failed"
                    }
                };
                if let Some((path, branch)) = &worktree_result {
                    terminal.worktree_path = Some(path.clone());
                    terminal.worktree_branch = Some(branch.clone());
                }
                registry.set_agent_outcome(&handle.task_id, terminal).await;
                let _ = registry.set_status(&handle.task_id, status).await;
            }
        }
        (outcome, worktree_result)
    });
    tokio::select! {
        biased;
        result = changed.changed() => {
            if result.is_ok() && *changed.borrow() {
                let handle = control.handle.lock().await;
                if let Some(handle) = handle.as_ref() {
                    guard.0 = None;
                    let agent_id = *control.allocated_agent_id.lock().unwrap();
                    if let Some(agent_id) = agent_id {
                        return ForegroundResult::Backgrounded(AsyncLaunch {agent_id, output_file: handle.output_path.clone()}, handle.task_id.clone());
                    }
                }
            }
            let (outcome, worktree) = completion.await.expect("foreground supervisor remains alive");
            ForegroundResult::Finished(outcome, worktree)
        }
        result = &mut completion => {
            guard.0 = None;
            let (outcome, worktree) = result.expect("foreground supervisor remains alive");
            ForegroundResult::Finished(outcome, worktree)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use platform_api::task_registry::TaskListFilter;
    struct ControlledSpawner {
        messages: Mutex<Vec<String>>,
        id: protocol::AgentId,
        ready: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }
    #[async_trait]
    impl SubagentSpawner for ControlledSpawner {
        async fn resume_foreground(
            &self,
            id: &protocol::AgentId,
            message: String,
        ) -> Result<(), SubagentSpawnError> {
            assert_eq!(*id, self.id);
            self.messages.lock().unwrap().push(message);
            Ok(())
        }

        async fn spawn(
            &self,
            _: SubagentSpawnRequest,
            _: SubagentInheritance,
        ) -> Result<SubagentResult, SubagentSpawnError> {
            unreachable!()
        }
        async fn spawn_with_observer(
            &self,
            _: SubagentSpawnRequest,
            _: SubagentInheritance,
            _: Option<mpsc::Sender<String>>,
            observer: Option<Arc<dyn SubagentSpawnObserver>>,
        ) -> Result<SubagentResult, SubagentSpawnError> {
            observer
                .unwrap()
                .before_start(&SubagentObservation::Allocated {
                    agent_id: self.id,
                    agent_type: "general-purpose".into(),
                    name: None,
                    model: "test".into(),
                    model_profile: None,
                    persistent: false,
                    initial_message_index: 0,
                    origin_session_id: None,
                })
                .await?;
            self.ready.notify_one();
            self.release.notified().await;
            Ok(SubagentResult::Failed {
                agent_id: self.id,
                reason: "test terminal result".into(),
                usage: Default::default(),
            })
        }
    }
    fn setup() -> (
        Arc<ControlledSpawner>,
        Arc<crate::agent_test_support::MockTaskRegistryHandle>,
        tool_api::BuiltinToolContext,
        SubagentInheritance,
    ) {
        let spawner = Arc::new(ControlledSpawner {
            messages: Default::default(),
            id: protocol::AgentId::new(),
            ready: Default::default(),
            release: Default::default(),
        });
        let registry = crate::agent_test_support::arc_mock_task_registry();
        let ctx = tool_api::test_support::ctx_for_file_tools(
            tool_api::test_support::make_dummy_fs(),
            Arc::new(telemetry::AnalyticsBus::new()),
            vec!["/tmp".into()],
        );
        let inherit = SubagentInheritance {
            tool_invoker: Arc::new(tool_api::tool_invoker_impl::RegistryToolInvoker::new(
                Arc::new(tool_api::ToolRegistry::new()),
            )),
            budget: crate::agent_test_support::arc_mock_budget(u64::MAX),
        };
        (spawner, registry, ctx, inherit)
    }
    #[tokio::test]
    async fn foreground_startup_preserves_exact_resume_request_and_inherited_capabilities() {
        let (spawner, registry, ctx, inherit) = setup();
        let request = SubagentSpawnRequest {
            subagent_type: "reviewer".into(),
            prompt: "continue the original analysis".into(),
            description: Some("original foreground task".into()),
            model: Some("original-model".into()),
            mode: Some("plan".into()),
            cwd: Some("/tmp/original-worktree".into()),
            context_paths: vec![std::path::PathBuf::from("/tmp/original-context")],
            fork_context_messages: Some(vec![]),
            fork_parent_system_prompt: Some("original inherited system prompt".into()),
            creator_agent_id: Some(protocol::AgentId::new()),
            effort: Some(serde_json::json!("high")),
            ..Default::default()
        };
        let (progress, _rx) = mpsc::channel(8);
        let call = tokio::spawn(run(
            spawner.clone(),
            request.clone(),
            inherit.clone(),
            progress,
            tool_api::test_support::fresh_tx(),
            None,
            registry.clone(),
            ctx,
        ));
        tokio::time::timeout(std::time::Duration::from_secs(5), spawner.ready.notified())
            .await
            .unwrap();
        let rows = registry.list(TaskListFilter::default()).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].owner_agent_id, Some(spawner.id.to_string()));
        let (stored, capabilities) = registry
            .resume_recipe(&rows[0].task_id)
            .expect("recipe exists before model startup");
        assert_eq!(stored, request);
        assert!(Arc::ptr_eq(
            &capabilities.tool_invoker,
            &inherit.tool_invoker
        ));
        assert!(Arc::ptr_eq(&capabilities.budget, &inherit.budget));
        spawner.release.notify_one();
        assert!(matches!(
            call.await.unwrap(),
            ForegroundResult::Finished(_, _)
        ));
    }

    #[tokio::test]
    async fn foreground_resume_recipe_failure_keeps_startup_gate_closed() {
        let (spawner, registry, ctx, inherit) = setup();
        registry.reject_resume_recipe_registration();
        // A mutant which skips recipe registration must terminate too, so the
        // assertion distinguishes the failure from an ordinary agent result.
        spawner.release.notify_one();
        let (progress, _rx) = mpsc::channel(8);
        let result = run(
            spawner,
            SubagentSpawnRequest::default(),
            inherit,
            progress,
            tool_api::test_support::fresh_tx(),
            None,
            registry.clone(),
            ctx,
        )
        .await;
        match result {
            ForegroundResult::Finished(Err(SubagentSpawnError::Internal(reason)), _) => {
                assert!(reason.contains("resume recipe rejected"))
            }
            _ => panic!("model startup proceeded after resume recipe registration failed"),
        }
        assert!(registry
            .list(TaskListFilter::default())
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn foreground_background_signal_returns_before_worker_completion_without_cancelling() {
        let (spawner, registry, ctx, inherit) = setup();
        let (progress, _rx) = mpsc::channel(8);
        let call = tokio::spawn(run(
            spawner.clone(),
            SubagentSpawnRequest::default(),
            inherit,
            progress,
            tool_api::test_support::fresh_tx(),
            None,
            registry.clone(),
            ctx,
        ));
        spawner.ready.notified().await;
        let rows = registry.list(TaskListFilter::default()).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].owner_agent_id, Some(spawner.id.to_string()));
        assert_eq!(rows[0].is_backgrounded, Some(false));
        assert!(registry.background_task(&rows[0].task_id).await);
        match tokio::time::timeout(std::time::Duration::from_secs(1), call)
            .await
            .unwrap()
            .unwrap()
        {
            ForegroundResult::Backgrounded(launch, id) => {
                assert_eq!(launch.agent_id, spawner.id);
                assert_eq!(id, rows[0].task_id);
            }
            _ => panic!("background handoff waited for completion"),
        }
        assert_eq!(
            registry
                .get(&rows[0].task_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            "running"
        );
        registry
            .send_foreground_message(&rows[0].task_id, "follow-up".into())
            .await
            .unwrap();
        assert_eq!(*spawner.messages.lock().unwrap(), ["follow-up"]);
        spawner.release.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if registry
                    .get(&rows[0].task_id)
                    .await
                    .unwrap()
                    .unwrap()
                    .status
                    == "failed"
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("detached worker publishes its actual terminal result");
    }
    #[tokio::test]
    async fn stopped_backgrounded_foreground_worker_publishes_killed_not_crashed() {
        let (spawner, registry, ctx, inherit) = setup();
        let (progress, _rx) = mpsc::channel(8);
        let call = tokio::spawn(run(
            spawner.clone(),
            SubagentSpawnRequest::default(),
            inherit,
            progress,
            tool_api::test_support::fresh_tx(),
            None,
            registry.clone(),
            ctx,
        ));
        spawner.ready.notified().await;
        let id = registry.list(TaskListFilter::default()).await.unwrap()[0]
            .task_id
            .clone();
        registry.background_task(&id).await;
        assert!(matches!(
            call.await.unwrap(),
            ForegroundResult::Backgrounded(..)
        ));
        registry.kill_foreground_worker(&id).await;
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                let status = registry.get(&id).await.unwrap().unwrap().status;
                assert_ne!(
                    status, "failed",
                    "an intentional stop is not a worker crash"
                );
                if status == "killed" {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn foreground_completion_withdraws_temporary_row() {
        let (spawner, registry, ctx, inherit) = setup();
        let (progress, _rx) = mpsc::channel(8);
        let call = tokio::spawn(run(
            spawner.clone(),
            SubagentSpawnRequest::default(),
            inherit,
            progress,
            tool_api::test_support::fresh_tx(),
            None,
            registry.clone(),
            ctx,
        ));
        spawner.ready.notified().await;
        spawner.release.notify_one();
        assert!(matches!(
            call.await.unwrap(),
            ForegroundResult::Finished(Ok(SubagentResult::Failed { .. }), _)
        ));
        assert!(registry
            .list(TaskListFilter::default())
            .await
            .unwrap()
            .is_empty());
    }
}
