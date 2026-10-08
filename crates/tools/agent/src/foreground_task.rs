//! Foreground registration and the non-cancelling Ctrl+B handoff (`fln/s9/mln`).
use async_trait::async_trait;
use lingxi_core::host::subagent_spawn::{AsyncLaunch, SubagentObservation, SubagentSpawnObserver};
use lingxi_core::host::task_registry::{
    AgentRunUsage, AgentTerminalOutcome, ForegroundAgentHandle, ForegroundAgentRegistration,
    TaskBackgrounder, TaskKiller, TaskRegistryHandle,
};
use lingxi_core::host::{
    SubagentInheritance, SubagentResult, SubagentSpawnError, SubagentSpawnRequest, SubagentSpawner,
};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot, watch};

/// The real child identity and selected wire model, sent only after the
/// spawner released its startup gate. `agent.spawn` resolves at this boundary
/// while the foreground Agent tool continues to own the terminal result.
pub(super) type StartReceipt = (lingxi_core::types::AgentId, String);

pub(super) enum ForegroundResult {
    Finished(
        Result<SubagentResult, SubagentSpawnError>,
        Option<(String, String)>,
        bool,
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
    allocated_agent_id: Mutex<Option<lingxi_core::types::AgentId>>,
    allocated_agent_type: Mutex<Option<String>>,
    signal: watch::Sender<bool>,
    hint_progress: tool_api::ToolProgressSender,
    tool_use_id: Option<lingxi_core::types::ToolUseId>,
    start_receipt: Mutex<Option<oneshot::Sender<StartReceipt>>>,
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
impl lingxi_core::host::task_registry::TaskMessageReceiver for TaskControl {
    async fn send_peer(
        &self,
        envelope: lingxi_core::host::handback::HandbackEnvelope,
    ) -> Result<(), lingxi_core::host::task_registry::TaskRegistryError> {
        use lingxi_core::host::handback::HandbackRecipient;
        use lingxi_core::host::task_registry::TaskRegistryError;
        let control = self
            .0
            .upgrade()
            .ok_or_else(|| TaskRegistryError::Internal("agent loop ended".into()))?;
        let id = (*control.allocated_agent_id.lock().unwrap())
            .ok_or_else(|| TaskRegistryError::Internal("agent not allocated".into()))?;
        if !envelope.validate()
            || !matches!(envelope.receipt.recipient, HandbackRecipient::Agent { agent_id, .. } if agent_id == id)
        {
            return Err(TaskRegistryError::Internal(
                "peer report recipient does not match this agent".into(),
            ));
        }
        let task_id = control
            .handle
            .lock()
            .await
            .as_ref()
            .map(|handle| handle.task_id.clone())
            .ok_or_else(|| TaskRegistryError::Internal("agent task not registered".into()))?;
        let record = control
            .registry
            .get(&task_id)
            .await?
            .ok_or_else(|| TaskRegistryError::NotFound(task_id.clone()))?;
        if record.status != "running" && !(record.status == "completed" && record.is_parked) {
            return Err(TaskRegistryError::NotFound(task_id));
        }
        control
            .spawner
            .resume_foreground_peer(&id, envelope)
            .await
            .map_err(|error| TaskRegistryError::Internal(error.to_string()))
    }

    async fn send(
        &self,
        message: String,
    ) -> Result<(), lingxi_core::host::task_registry::TaskRegistryError> {
        let control = self.0.upgrade().ok_or_else(|| {
            lingxi_core::host::task_registry::TaskRegistryError::Internal("agent loop ended".into())
        })?;
        let id = (*control.allocated_agent_id.lock().unwrap()).ok_or_else(|| {
            lingxi_core::host::task_registry::TaskRegistryError::Internal(
                "agent not allocated".into(),
            )
        })?;
        let task_id = control
            .handle
            .lock()
            .await
            .as_ref()
            .map(|handle| handle.task_id.clone())
            .ok_or_else(|| {
                lingxi_core::host::task_registry::TaskRegistryError::Internal(
                    "agent task not registered".into(),
                )
            })?;
        let record = control.registry.get(&task_id).await?.ok_or_else(|| {
            lingxi_core::host::task_registry::TaskRegistryError::NotFound(task_id.clone())
        })?;
        if record.status != "running" && !record.is_parked {
            return Err(lingxi_core::host::task_registry::TaskRegistryError::NotFound(task_id));
        }
        if control
            .registry
            .set_status(&task_id, "running")
            .await?
            .status
            != "running"
        {
            return Err(lingxi_core::host::task_registry::TaskRegistryError::NotFound(task_id));
        }
        if let Err(error) = control.spawner.resume_foreground(&id, message).await {
            // A closed pool channel cannot leave a task advertising active work.
            let _ = control.registry.set_status(&task_id, "failed").await;
            return Err(
                lingxi_core::host::task_registry::TaskRegistryError::Internal(error.to_string()),
            );
        }
        Ok(())
    }
}
struct Observer(Arc<Control>);
#[async_trait]
impl SubagentSpawnObserver for Observer {
    fn on_started(&self, event: &SubagentObservation) {
        if let SubagentObservation::Allocated {
            agent_id, model, ..
        } = event
        {
            if let Some(sender) = self.0.start_receipt.lock().unwrap().take() {
                let _ = sender.send((*agent_id, model.clone()));
            }
        }
    }

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
            *self.0.allocated_agent_type.lock().unwrap() = Some(agent_type.clone());
            let registration = ForegroundAgentRegistration {
                agent_id,
                agent_type,
                description: request.description.clone().unwrap_or_default(),
                prompt: request.prompt.clone(),
                tool_use_id: request.tool_use_id.clone(),
                creator_agent_id: request.creator_agent_id,
                creator_teammate_name: request.creator_teammate_name.clone(),
                creator_team_name: request.creator_team_name.clone(),
                agent_spawn_provenance: request.agent_spawn_provenance.clone(),
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
                    if let Some(token) = request.agent_spawn_token.clone() {
                        self.0.registry.bind_agent_spawn_token(&id, token);
                    }
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
                    )));
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
    Option<lingxi_core::host::agent_statistics::AgentSpawnToken>,
);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            self.1.store(true, std::sync::atomic::Ordering::SeqCst);
            if let Some(token) = &self.2 { token.cancelled_after_completion(); }
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
    tool_use_id: Option<lingxi_core::types::ToolUseId>,
    registry: Arc<dyn TaskRegistryHandle>,
    ctx: tool_api::BuiltinToolContext,
    start_receipt: Option<oneshot::Sender<StartReceipt>>,
) -> ForegroundResult {
    let worktree = request.worktree.clone();
    let spawner_owns_stop =
        spawner.owns_subagent_stop_hooks(false, request.origin_session_id, request.stop_hook_scope);
    let stop_firer = (!spawner_owns_stop
        && request.stop_hook_scope
            == lingxi_core::host::subagent_spawn::SubagentStopScope::Session)
        .then(|| request.origin_session_id)
        .flatten()
        .and_then(|session_id| {
            ctx.task_lifecycle_hooks
                .as_ref()
                .and_then(|hooks| hooks.subagent_stop_firer(session_id))
        });
    let terminal_hooks_owned = spawner_owns_stop || stop_firer.is_some();
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
        allocated_agent_type: Default::default(),
        signal,
        hint_progress,
        tool_use_id,
        start_receipt: Mutex::new(start_receipt),
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
    let mut guard = CancelOnDrop(Some(worker.abort_handle()), control.stop_requested.clone(), control.request.agent_spawn_token.clone());
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
        let retains_owned_work = match &outcome {
            Ok(SubagentResult::Completed {
                agent_id,
                handback: Some(state),
                ..
            }) if state.active => registry.agent_waiting_on_owned_work(*agent_id).await,
            _ => false,
        };
        if let Some(firer) = stop_firer.as_ref() {
            let terminal = match &outcome {
                Ok(SubagentResult::Completed { agent_id, .. }) => Some((
                    *agent_id,
                    lingxi_core::host::subagent_spawn::SubagentStopStatus::Completed,
                )),
                Ok(SubagentResult::Failed { agent_id, .. }) => Some((
                    *agent_id,
                    lingxi_core::host::subagent_spawn::SubagentStopStatus::Failed,
                )),
                _ => None,
            };
            if let Some((agent_id, status)) = terminal {
                let agent_type = completion_control
                    .allocated_agent_type
                    .lock()
                    .unwrap()
                    .clone()
                    .unwrap_or_else(|| completion_control.request.subagent_type.clone());
                firer.fire(agent_id, &agent_type, status).await;
            }
        }
        let worktree_result = match (worktree, retains_owned_work) {
            (Some(handle), false) => {
                lingxi_core::host::worktree::agent_worktree_result(ctx.worktree.as_ref(), &handle)
                    .await
            }
            _ => None,
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
                        handback,
                        content,
                        total_tokens,
                        total_tool_use_count,
                        total_duration_ms,
                        ..
                    }) => {
                        terminal.handback = handback.clone();
                        let sender_name = completion_control
                            .request
                            .name
                            .clone()
                            .or_else(|| {
                                completion_control
                                    .allocated_agent_type
                                    .lock()
                                    .unwrap()
                                    .clone()
                            })
                            .unwrap_or_else(|| completion_control.request.subagent_type.clone());
                        terminal.result = Some(
                            super::handback_completion_texts(
                                handback.as_ref(),
                                content,
                                &sender_name,
                                retains_owned_work,
                                true,
                            )
                            .join("\n"),
                        );
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
        (outcome, worktree_result, terminal_hooks_owned)
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
            let (outcome, worktree, hooks_owned) = completion.await.expect("foreground supervisor remains alive");
            ForegroundResult::Finished(outcome, worktree, hooks_owned)
        }
        result = &mut completion => {
            guard.0 = None;
            let (outcome, worktree, hooks_owned) = result.expect("foreground supervisor remains alive");
            ForegroundResult::Finished(outcome, worktree, hooks_owned)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::task_registry::TaskListFilter;
    struct ControlledSpawner {
        messages: Mutex<Vec<String>>,
        id: lingxi_core::types::AgentId,
        ready: tokio::sync::Notify,
        release: tokio::sync::Notify,
        terminal: Mutex<Option<SubagentResult>>,
    }
    #[async_trait]
    impl SubagentSpawner for ControlledSpawner {
        async fn resume_foreground(
            &self,
            id: &lingxi_core::types::AgentId,
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
            let observer = observer.unwrap();
            let allocation = SubagentObservation::Allocated {
                agent_id: self.id,
                agent_type: "general-purpose".into(),
                name: None,
                model: "test".into(),
                model_profile: None,
                persistent: false,
                initial_message_index: 0,
                origin_session_id: None,
            };
            observer.before_start(&allocation).await?;
            observer.on_started(&allocation);
            self.ready.notify_one();
            self.release.notified().await;
            Ok(self
                .terminal
                .lock()
                .unwrap()
                .take()
                .unwrap_or(SubagentResult::Failed {
                    agent_id: self.id,
                    reason: "test terminal result".into(),
                    usage: Default::default(),
                }))
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
            id: lingxi_core::types::AgentId::new(),
            ready: Default::default(),
            release: Default::default(),
            terminal: Default::default(),
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
            instruction_context: None,
            fork_parent_system_prompt: Some("original inherited system prompt".into()),
            creator_agent_id: Some(lingxi_core::types::AgentId::new()),
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
            None,
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
            ForegroundResult::Finished(_, _, _)
        ));
    }

    #[tokio::test]
    async fn start_receipt_precedes_foreground_terminal_result() {
        let (spawner, registry, ctx, inherit) = setup();
        let (progress, _rx) = mpsc::channel(8);
        let (started, receipt) = oneshot::channel();
        let call = tokio::spawn(run(
            spawner.clone(),
            SubagentSpawnRequest::default(),
            inherit,
            progress,
            tool_api::test_support::fresh_tx(),
            None,
            registry,
            ctx,
            Some(started),
        ));
        let (agent_id, model) = tokio::time::timeout(std::time::Duration::from_secs(5), receipt)
            .await
            .expect("start receipt arrives before terminal")
            .expect("startup gate opened");
        assert_eq!(agent_id, spawner.id);
        assert_eq!(model, "test");
        assert!(
            !call.is_finished(),
            "the terminal result is still owned by Agent"
        );
        spawner.release.notify_one();
        assert!(matches!(
            call.await.unwrap(),
            ForegroundResult::Finished(..)
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
        let (started, receipt) = oneshot::channel();
        let result = run(
            spawner,
            SubagentSpawnRequest::default(),
            inherit,
            progress,
            tool_api::test_support::fresh_tx(),
            None,
            registry.clone(),
            ctx,
            Some(started),
        )
        .await;
        assert!(
            receipt.await.is_err(),
            "a rejected startup has no start receipt"
        );
        match result {
            ForegroundResult::Finished(Err(SubagentSpawnError::Internal(reason)), _, _) => {
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
        let (spawner, registry, mut ctx, inherit) = setup();
        let (session, firer) = bind_stop_owner(&mut ctx);
        let (progress, _rx) = mpsc::channel(8);
        let call = tokio::spawn(run(
            spawner.clone(),
            SubagentSpawnRequest {
                origin_session_id: Some(session),
                stop_hook_scope: lingxi_core::host::subagent_spawn::SubagentStopScope::Session,
                ..Default::default()
            },
            inherit,
            progress,
            tool_api::test_support::fresh_tx(),
            None,
            registry.clone(),
            ctx,
            None,
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
        assert!(
            firer.seen.lock().unwrap().is_empty(),
            "handoff is not terminal"
        );
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
        assert_eq!(
            *firer.seen.lock().unwrap(),
            vec![(
                spawner.id,
                lingxi_core::host::subagent_spawn::SubagentStopStatus::Failed
            )]
        );
    }
    #[tokio::test]
    async fn stopped_backgrounded_foreground_worker_publishes_killed_not_crashed() {
        let (spawner, registry, mut ctx, inherit) = setup();
        let (session, firer) = bind_stop_owner(&mut ctx);
        let (progress, _rx) = mpsc::channel(8);
        let call = tokio::spawn(run(
            spawner.clone(),
            SubagentSpawnRequest {
                origin_session_id: Some(session),
                stop_hook_scope: lingxi_core::host::subagent_spawn::SubagentStopScope::Session,
                ..Default::default()
            },
            inherit,
            progress,
            tool_api::test_support::fresh_tx(),
            None,
            registry.clone(),
            ctx,
            None,
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
        assert!(
            firer.seen.lock().unwrap().is_empty(),
            "Killed never fires Stop"
        );
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
            None,
        ));
        spawner.ready.notified().await;
        spawner.release.notify_one();
        assert!(matches!(
            call.await.unwrap(),
            ForegroundResult::Finished(Ok(SubagentResult::Failed { .. }), _, _)
        ));
        assert!(registry
            .list(TaskListFilter::default())
            .await
            .unwrap()
            .is_empty());
    }
    struct RecordingStopOwner {
        epoch: lingxi_core::types::MessageId,
        current: std::sync::atomic::AtomicBool,
        seen: Mutex<
            Vec<(
                lingxi_core::types::AgentId,
                lingxi_core::host::subagent_spawn::SubagentStopStatus,
            )>,
        >,
        must_exist: Option<std::path::PathBuf>,
    }
    #[async_trait]
    impl lingxi_core::host::subagent_spawn::SubagentStopHookFirer for RecordingStopOwner {
        fn epoch_id(&self) -> lingxi_core::types::MessageId {
            self.epoch
        }
        fn retire(&self) {
            self.current
                .store(false, std::sync::atomic::Ordering::SeqCst);
        }
        fn is_current(&self) -> bool {
            self.current.load(std::sync::atomic::Ordering::SeqCst)
        }
        async fn fire(
            &self,
            id: lingxi_core::types::AgentId,
            _: &str,
            status: lingxi_core::host::subagent_spawn::SubagentStopStatus,
        ) {
            if !self.is_current() {
                return;
            }
            if let Some(path) = &self.must_exist {
                assert!(path.exists(), "Stop command cwd must still exist");
            }
            self.seen.lock().unwrap().push((id, status));
        }
    }
    struct StopCapability {
        session: lingxi_core::types::SessionId,
        firer: Arc<RecordingStopOwner>,
    }
    #[async_trait]
    impl tool_api::TaskLifecycleHookFirer for StopCapability {
        fn subagent_stop_firer(
            &self,
            session: lingxi_core::types::SessionId,
        ) -> Option<Arc<dyn lingxi_core::host::subagent_spawn::SubagentStopHookFirer>> {
            (session == self.session).then(|| {
                self.firer.clone()
                    as Arc<dyn lingxi_core::host::subagent_spawn::SubagentStopHookFirer>
            })
        }
    }
    fn bind_stop_owner(
        ctx: &mut tool_api::BuiltinToolContext,
    ) -> (lingxi_core::types::SessionId, Arc<RecordingStopOwner>) {
        let session = lingxi_core::types::SessionId::new();
        let firer = Arc::new(RecordingStopOwner {
            epoch: lingxi_core::types::MessageId::new(),
            current: std::sync::atomic::AtomicBool::new(true),
            seen: Mutex::new(Vec::new()),
            must_exist: None,
        });
        ctx.task_lifecycle_hooks = Some(Arc::new(StopCapability {
            session,
            firer: firer.clone(),
        }));
        (session, firer)
    }
    fn completed(id: lingxi_core::types::AgentId) -> SubagentResult {
        SubagentResult::Completed {
            agent_id: id,
            handback: None,
            content: serde_json::json!("done"),
            usage: Default::default(),
            cumulative_usage: Default::default(),
            usage_complete: true,
            total_tokens: 0,
            total_tool_use_count: 0,
            total_duration_ms: 0,
            assistant_message_count: 1,
            response_char_count: 1,
            last_request_id: None,
        }
    }
    #[tokio::test]
    async fn bound_foreground_completion_and_failure_each_fire_once_and_return_owned_receipt() {
        for success in [true, false] {
            let (spawner, registry, mut ctx, inherit) = setup();
            let (session, firer) = bind_stop_owner(&mut ctx);
            if success {
                *spawner.terminal.lock().unwrap() = Some(completed(spawner.id));
            }
            let (progress, _rx) = mpsc::channel(8);
            let call = tokio::spawn(run(
                spawner.clone(),
                SubagentSpawnRequest {
                    origin_session_id: Some(session),
                    stop_hook_scope: lingxi_core::host::subagent_spawn::SubagentStopScope::Session,
                    ..Default::default()
                },
                inherit,
                progress,
                tool_api::test_support::fresh_tx(),
                None,
                registry,
                ctx,
                None,
            ));
            spawner.ready.notified().await;
            assert!(firer.seen.lock().unwrap().is_empty());
            spawner.release.notify_one();
            assert!(matches!(
                call.await.unwrap(),
                ForegroundResult::Finished(Ok(_), _, true)
            ));
            let status = if success {
                lingxi_core::host::subagent_spawn::SubagentStopStatus::Completed
            } else {
                lingxi_core::host::subagent_spawn::SubagentStopStatus::Failed
            };
            assert_eq!(*firer.seen.lock().unwrap(), vec![(spawner.id, status)]);
        }
    }
    struct CleanWorktree(Arc<RecordingStopOwner>);
    #[async_trait]
    impl lingxi_core::host::worktree::WorktreeManager for CleanWorktree {
        async fn create_worktree(
            &self,
            _: &str,
            _: Option<&str>,
            _: &[std::path::PathBuf],
        ) -> Result<
            lingxi_core::host::worktree::WorktreeHandle,
            lingxi_core::host::worktree::WorktreeError,
        > {
            Err(lingxi_core::host::worktree::WorktreeError::Unsupported)
        }
        async fn remove_worktree(
            &self,
            handle: &lingxi_core::host::worktree::WorktreeHandle,
        ) -> Result<(), lingxi_core::host::worktree::WorktreeError> {
            assert_eq!(
                self.0.seen.lock().unwrap().len(),
                1,
                "Stop precedes clean worktree removal"
            );
            std::fs::remove_dir(&handle.path).unwrap();
            Ok(())
        }
        async fn list_worktrees(
            &self,
        ) -> Result<
            Vec<lingxi_core::host::worktree::WorktreeInfo>,
            lingxi_core::host::worktree::WorktreeError,
        > {
            Ok(Vec::new())
        }
        async fn cleanup_stale(
            &self,
            _: std::time::Duration,
        ) -> Result<Vec<std::path::PathBuf>, lingxi_core::host::worktree::WorktreeError> {
            Ok(Vec::new())
        }
        fn is_supported(&self) -> bool {
            true
        }
        async fn worktree_change_summary(
            &self,
            _: &lingxi_core::host::worktree::WorktreeHandle,
        ) -> Result<
            Option<lingxi_core::host::worktree::WorktreeChangeSummary>,
            lingxi_core::host::worktree::WorktreeError,
        > {
            Ok(Some(lingxi_core::host::worktree::WorktreeChangeSummary {
                changed_files: 0,
                commits: 0,
            }))
        }
    }
    #[tokio::test]
    async fn owned_stop_finishes_before_clean_worktree_cleanup() {
        let (spawner, registry, mut ctx, inherit) = setup();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("child-cwd");
        std::fs::create_dir(&path).unwrap();
        let (session, mut firer) = bind_stop_owner(&mut ctx);
        // Replace the binding with a callback that checks its command cwd.
        let checker = Arc::new(RecordingStopOwner {
            epoch: firer.epoch,
            current: std::sync::atomic::AtomicBool::new(true),
            seen: Mutex::new(Vec::new()),
            must_exist: Some(path.clone()),
        });
        firer = checker;
        ctx.task_lifecycle_hooks = Some(Arc::new(StopCapability {
            session,
            firer: firer.clone(),
        }));
        ctx.worktree = Arc::new(CleanWorktree(firer.clone()));
        let (progress, _rx) = mpsc::channel(8);
        let call = tokio::spawn(run(
            spawner.clone(),
            SubagentSpawnRequest {
                origin_session_id: Some(session),
                stop_hook_scope: lingxi_core::host::subagent_spawn::SubagentStopScope::Session,
                worktree: Some(lingxi_core::host::worktree::WorktreeHandle {
                    path: path.clone(),
                    branch_name: "child-branch".into(),
                    base_commit: Some("base".into()),
                }),
                ..Default::default()
            },
            inherit,
            progress,
            tool_api::test_support::fresh_tx(),
            None,
            registry,
            ctx,
            None,
        ));
        spawner.ready.notified().await;
        spawner.release.notify_one();
        assert!(matches!(
            call.await.unwrap(),
            ForegroundResult::Finished(Ok(_), None, true)
        ));
        assert_eq!(firer.seen.lock().unwrap().len(), 1);
        assert!(!path.exists());
    }
}
