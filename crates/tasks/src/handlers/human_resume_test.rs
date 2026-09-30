use super::*;
use lingxi_core::host::subagent_spawn::{SubagentObservation, SubagentSpawnObserver};

#[derive(Default)]
struct RestoreGate {
    ids: StdMutex<Vec<AgentId>>,
    block: std::sync::atomic::AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
#[async_trait]
impl SubagentSpawnObserver for RestoreGate {
    fn on_allocated(&self, event: &SubagentObservation) {
        if let SubagentObservation::Allocated { agent_id, .. } = event {
            self.ids.lock().unwrap().push(*agent_id);
        }
    }
    async fn before_start(
        &self,
        _: &SubagentObservation,
    ) -> Result<(), lingxi_core::host::SubagentSpawnError> {
        if self.block.load(std::sync::atomic::Ordering::SeqCst) {
            self.entered.notify_one();
            self.release.notified().await;
        }
        Ok(())
    }
    async fn on_event(&self, _: SubagentObservation) {}
}
struct HumanApi {
    calls: StdMutex<Vec<String>>,
    gate: Arc<RestoreGate>,
    registry: std::sync::OnceLock<std::sync::Weak<crate::registry::TaskRegistry>>,
    block_calls: std::sync::atomic::AtomicBool,
    call_entered: tokio::sync::Notify,
    call_release: tokio::sync::Notify,
    cancelled_call: Arc<std::sync::atomic::AtomicBool>,
}
#[async_trait]
impl agent::SubagentApiClient for HumanApi {
    async fn messages_create(
        &self,
        _: &str,
        _: Option<&str>,
        messages: Vec<lingxi_core::types::ConversationMessage>,
        _: Vec<serde_json::Value>,
    ) -> Result<llm_runtime::HistoryResponse, llm_runtime::LlmError> {
        let history = serde_json::to_string(&messages).unwrap();
        let index = {
            let mut calls = self.calls.lock().unwrap();
            let index = calls.len();
            calls.push(history.clone());
            index
        };
        if index > 0 {
            let ids = self.gate.ids.lock().unwrap().clone();
            assert!(
                ids.iter().all(|id| id == &ids[0]),
                "human restore allocates the original Pool identity"
            );
            let row = self
                .registry
                .get()
                .unwrap()
                .upgrade()
                .unwrap()
                .get("ahumanalias")
                .await
                .unwrap();
            assert!(
                matches!(row, crate::state::TaskState::LocalAgent(agent) if agent.agent_id == ids[0])
            );
            assert!(
                history.contains("first answer"),
                "actual transcript history must survive stop"
            );
        }
        if index > 0 && self.block_calls.load(std::sync::atomic::Ordering::SeqCst) {
            struct InFlight {
                cancelled: Arc<std::sync::atomic::AtomicBool>,
                finished: bool,
            }
            impl Drop for InFlight {
                fn drop(&mut self) {
                    if !self.finished {
                        self.cancelled
                            .store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                }
            }
            let mut in_flight = InFlight {
                cancelled: self.cancelled_call.clone(),
                finished: false,
            };
            self.call_entered.notify_one();
            self.call_release.notified().await;
            in_flight.finished = true;
        }
        Ok(llm_runtime::HistoryResponse {
            id: "human-response".into(),
            model: "mock".into(),
            content: vec![llm_runtime::ContentBlock::Text {
                text: if index == 0 {
                    "first answer"
                } else {
                    "human answer"
                }
                .into(),
                cache_control: None,
            }],
            stop_reason: Some("end_turn".into()),
            stop_details: None,
            usage: llm_runtime::ExecutionUsage::default(),
            cost: None,
            provider_metadata: serde_json::Value::Null,
        })
    }
}
struct Fixture {
    _dir: tempfile::TempDir,
    registry: Arc<crate::registry::TaskRegistry>,
    pool: Arc<agent::StateMachinePool>,
    api: Arc<HumanApi>,
    gate: Arc<RestoreGate>,
    id: String,
    handler: Arc<LocalAgentHandler>,
}
async fn fixture() -> Fixture {
    fixture_with_worktree(None).await
}
async fn fixture_with_worktree(
    manager: Option<Arc<dyn lingxi_core::host::worktree::WorktreeManager>>,
) -> Fixture {
    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    let (dir, output) = make_output_manager(fs.clone());
    let runtime: Arc<dyn RuntimeSpawner> = Arc::new(MockRuntimeSpawner::default());
    let pool = Arc::new(agent::StateMachinePool::new(runtime.clone(), 4));
    let gate = Arc::new(RestoreGate::default());
    let api = Arc::new(HumanApi {
        calls: StdMutex::new(Vec::new()),
        gate: gate.clone(),
        registry: std::sync::OnceLock::new(),
        block_calls: std::sync::atomic::AtomicBool::new(false),
        call_entered: tokio::sync::Notify::new(),
        call_release: tokio::sync::Notify::new(),
        cancelled_call: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    });
    let spawner = Arc::new(
        agent::PoolSubagentSpawner::new(pool.clone())
            .with_api_client(api.clone())
            .with_spawn_observer(gate.clone())
            .with_hook_context(
                lingxi_core::types::SessionId::new(),
                dir.path().to_owned(),
                Some(dir.path().join("transcripts")),
            )
            .with_transcript_fs(fs.clone()),
    );
    let sink = Arc::new(crate::registry_status_sink::RegistryStatusSink::new());
    let mut handler = LocalAgentHandler::new(
        spawner.clone(),
        Arc::new(MockInvoker),
        Arc::new(MockBudget),
        output.clone(),
    )
    .with_status_sink(sink.clone())
    .with_streaming_spawner(spawner.clone());
    if let Some(manager) = manager {
        handler = handler.with_worktree_manager(manager);
    }
    let handler = Arc::new(handler);
    let mut registry = crate::registry::TaskRegistry::new(runtime, fs, output);
    registry.register_handler(TaskType::LocalAgent, handler.clone());
    let registry = Arc::new(registry);
    sink.bind(registry.clone());
    spawner.set_task_registry(registry.clone());
    api.registry.set(Arc::downgrade(&registry)).ok().unwrap();
    let id = registry
        .spawn(
            TaskType::LocalAgent,
            local_agent_input("original human test work"),
            "human test".into(),
        )
        .await
        .unwrap();
    registry.register_alias_for_test("ahumanalias", &id).await;
    wait_parked(&registry, &id).await;
    registry.kill_with_reason(&id, "user").await.unwrap();
    Fixture {
        _dir: dir,
        registry,
        pool,
        api,
        gate,
        id,
        handler,
    }
}
async fn wait_parked(registry: &crate::registry::TaskRegistry, id: &str) {
    tokio::time::timeout(std::time::Duration::from_secs(4), async {
        loop {
            if registry
                .get(id)
                .await
                .is_some_and(|state| state.is_parked())
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("real runner reaches parked boundary");
}
#[tokio::test]
async fn human_message_restores_stopped_real_pool_with_history_and_original_alias() {
    let f = fixture().await;
    use lingxi_core::host::task_registry::TaskRegistryHandle;
    TaskRegistryHandle::send_human_task_message(
        f.registry.as_ref(),
        "ahumanalias",
        "human followup",
    )
    .await
    .unwrap();
    wait_parked(&f.registry, &f.id).await;
    let calls = f.api.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 2);
    assert!(calls[1].contains("human followup"));
    f.registry.kill_with_reason(&f.id, "user").await.unwrap();
}
#[tokio::test]
async fn a_second_user_stop_invalidates_a_human_resume_before_its_startup_gate() {
    let f = fixture().await;
    f.gate
        .block
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let registry = f.registry.clone();
    let id = f.id.clone();
    let pending = tokio::spawn(async move {
        registry
            .send_human_task_message(&id, "stale typed message")
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(4), f.gate.entered.notified())
        .await
        .unwrap();
    f.registry.kill_with_reason(&f.id, "user").await.unwrap();
    assert!(pending.await.unwrap().is_err());
    f.gate
        .block
        .store(false, std::sync::atomic::Ordering::SeqCst);
    f.gate.release.notify_waiters();
    tokio::time::timeout(std::time::Duration::from_secs(4), async {
        while f.pool.slot_count().await != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        f.api.calls.lock().unwrap().len(),
        1,
        "stale epoch cannot invoke a model"
    );
    f.registry
        .send_human_task_message(&f.id, "new typed message")
        .await
        .unwrap();
    wait_parked(&f.registry, &f.id).await;
    let calls = f.api.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 2);
    assert!(!calls[1].contains("stale typed message"));
    assert!(calls[1].contains("new typed message"));
    f.registry.kill_with_reason(&f.id, "user").await.unwrap();
}

#[tokio::test]
async fn human_input_while_running_folds_without_cancelling_the_model() {
    let f = fixture().await;
    f.api
        .block_calls
        .store(true, std::sync::atomic::Ordering::SeqCst);
    f.registry
        .send_human_task_message(&f.id, "first typed resume")
        .await
        .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(4),
        f.api.call_entered.notified(),
    )
    .await
    .unwrap();
    f.registry
        .send_human_task_message(&f.id, "queued human followup")
        .await
        .unwrap();
    assert_eq!(f.api.calls.lock().unwrap().len(), 2);
    assert!(!f
        .api
        .cancelled_call
        .load(std::sync::atomic::Ordering::SeqCst));
    f.api
        .block_calls
        .store(false, std::sync::atomic::Ordering::SeqCst);
    f.api.call_release.notify_one();
    wait_parked(&f.registry, &f.id).await;
    let calls = f.api.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 3);
    assert!(calls[2].contains("queued human followup"));
    assert!(!f
        .api
        .cancelled_call
        .load(std::sync::atomic::Ordering::SeqCst));
    f.registry.kill_with_reason(&f.id, "user").await.unwrap();
}

#[tokio::test]
async fn cancelling_the_human_message_caller_does_not_cancel_accepted_resume() {
    let f = fixture().await;
    f.api
        .block_calls
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let registry = f.registry.clone();
    let id = f.id.clone();
    let request = tokio::spawn(async move {
        registry
            .send_human_task_message(&id, "caller cancelled")
            .await
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(4),
        f.api.call_entered.notified(),
    )
    .await
    .unwrap();
    request.abort();
    f.api
        .block_calls
        .store(false, std::sync::atomic::Ordering::SeqCst);
    f.api.call_release.notify_one();
    wait_parked(&f.registry, &f.id).await;
    assert!(!f
        .api
        .cancelled_call
        .load(std::sync::atomic::Ordering::SeqCst));
    assert_eq!(f.api.calls.lock().unwrap().len(), 2);
    f.registry.kill_with_reason(&f.id, "user").await.unwrap();
}

#[tokio::test]
async fn human_resume_permission_failure_keeps_the_task_stopped() {
    let f = fixture().await;
    f.handler
        .resume_recipes
        .lock()
        .unwrap()
        .get_mut(&f.id)
        .unwrap()
        .0
        .forked_skill_name = Some("restricted-skill".into());
    assert!(f
        .registry
        .send_human_task_message(&f.id, "must not bypass fork scope")
        .await
        .is_err());
    let row = f.registry.get(&f.id).await.unwrap();
    assert!(
        matches!(row,crate::state::TaskState::LocalAgent(agent) if agent.base.status==TaskStatus::Killed && agent.outcome.killed_by.as_deref()==Some("user"))
    );
    assert_eq!(f.api.calls.lock().unwrap().len(), 1);
    assert!(!f.registry.has_human_messages(&f.id));
}

#[derive(Default)]
struct WorktreeGate {
    removing: tokio::sync::Notify,
    release_remove: tokio::sync::Notify,
    creating: tokio::sync::Notify,
    release_create: tokio::sync::Notify,
    block_create: std::sync::atomic::AtomicBool,
    block_remove: std::sync::atomic::AtomicBool,
    removed: tokio::sync::Notify,
    path: StdMutex<PathBuf>,
}
#[async_trait]
impl lingxi_core::host::worktree::WorktreeManager for WorktreeGate {
    async fn create_worktree(
        &self,
        _: &str,
        base: Option<&str>,
        _: &[PathBuf],
    ) -> Result<lingxi_core::host::worktree::WorktreeHandle, lingxi_core::host::worktree::WorktreeError> {
        assert_eq!(base, Some("pinned-base"));
        self.creating.notify_one();
        if self.block_create.load(std::sync::atomic::Ordering::SeqCst) {
            self.release_create.notified().await;
        }
        let path = self.path.lock().unwrap().clone();
        std::fs::create_dir_all(&path).unwrap();
        Ok(lingxi_core::host::worktree::WorktreeHandle {
            path,
            branch_name: "restored".into(),
            base_commit: Some("pinned-base".into()),
        })
    }
    async fn remove_worktree(
        &self,
        handle: &lingxi_core::host::worktree::WorktreeHandle,
    ) -> Result<(), lingxi_core::host::worktree::WorktreeError> {
        self.removing.notify_one();
        if self.block_remove.load(std::sync::atomic::Ordering::SeqCst) {
            self.release_remove.notified().await;
        }
        let _ = std::fs::remove_dir_all(&handle.path);
        self.removed.notify_one();
        Ok(())
    }
    async fn list_worktrees(
        &self,
    ) -> Result<Vec<lingxi_core::host::worktree::WorktreeInfo>, lingxi_core::host::worktree::WorktreeError>
    {
        Ok(vec![])
    }
    async fn cleanup_stale(
        &self,
        _: std::time::Duration,
    ) -> Result<Vec<PathBuf>, lingxi_core::host::worktree::WorktreeError> {
        Ok(vec![])
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
    async fn enter_existing(
        &self,
        path: &std::path::Path,
    ) -> Result<lingxi_core::host::worktree::WorktreeHandle, lingxi_core::host::worktree::WorktreeError> {
        Ok(lingxi_core::host::worktree::WorktreeHandle {
            path: path.to_owned(),
            branch_name: "restored".into(),
            base_commit: Some("pinned-base".into()),
        })
    }
}
fn install_worktree_recipe(
    f: &Fixture,
    manager: &WorktreeGate,
) -> lingxi_core::host::worktree::WorktreeHandle {
    let path = f._dir.path().join("isolated");
    *manager.path.lock().unwrap() = path.clone();
    let handle = lingxi_core::host::worktree::WorktreeHandle {
        path: path.clone(),
        branch_name: "original".into(),
        base_commit: Some("pinned-base".into()),
    };
    let mut recipes = f.handler.resume_recipes.lock().unwrap();
    let request = &mut recipes.get_mut(&f.id).unwrap().0;
    request.worktree = Some(handle.clone());
    request.cwd = Some(path.to_string_lossy().into_owned());
    handle
}
#[tokio::test]
async fn human_resume_waits_for_old_stop_worktree_and_route_teardown() {
    let manager = Arc::new(WorktreeGate::default());
    let f = fixture_with_worktree(Some(manager.clone())).await;
    f.registry
        .send_human_task_message(&f.id, "first resume")
        .await
        .unwrap();
    wait_parked(&f.registry, &f.id).await;
    let worktree = install_worktree_recipe(&f, &manager);
    std::fs::create_dir_all(&worktree.path).unwrap();
    f.handler
        .persistent_worktrees
        .lock()
        .await
        .insert(f.id.clone(), worktree);
    manager
        .block_remove
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let registry = f.registry.clone();
    let id = f.id.clone();
    let stop = tokio::spawn(async move { registry.kill_with_reason(&id, "user").await });
    tokio::time::timeout(
        std::time::Duration::from_secs(4),
        manager.removing.notified(),
    )
    .await
    .unwrap();
    assert_eq!(
        f.pool.slot_count().await,
        0,
        "old Pool already released while cleanup is held"
    );
    let registry = f.registry.clone();
    let id = f.id.clone();
    let mut resume = tokio::spawn(async move {
        registry
            .send_human_task_message(&id, "after full teardown")
            .await
    });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(30), &mut resume)
            .await
            .is_err()
    );
    assert_eq!(f.api.calls.lock().unwrap().len(), 2);
    manager
        .block_remove
        .store(false, std::sync::atomic::Ordering::SeqCst);
    manager.release_remove.notify_one();
    stop.await.unwrap().unwrap();
    resume.await.unwrap().unwrap();
    wait_parked(&f.registry, &f.id).await;
    assert_eq!(f.api.calls.lock().unwrap().len(), 3);
    assert!(
        manager.path.lock().unwrap().is_dir(),
        "clean removed worktree was recreated from pinned base"
    );
    f.registry.kill_with_reason(&f.id, "user").await.unwrap();
}
#[tokio::test]
async fn cancelled_worktree_creation_cleans_the_late_created_directory() {
    let manager = Arc::new(WorktreeGate::default());
    let f = fixture_with_worktree(Some(manager.clone())).await;
    install_worktree_recipe(&f, &manager);
    manager
        .block_create
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let registry = f.registry.clone();
    let id = f.id.clone();
    let resume = tokio::spawn(async move {
        registry
            .send_human_task_message(&id, "cancel creation")
            .await
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(4),
        manager.creating.notified(),
    )
    .await
    .unwrap();
    f.registry.kill_with_reason(&f.id, "user").await.unwrap();
    assert!(resume.await.unwrap().is_err());
    manager.release_create.notify_one();
    tokio::time::timeout(
        std::time::Duration::from_secs(4),
        manager.removed.notified(),
    )
    .await
    .unwrap();
    assert!(!manager.path.lock().unwrap().exists());
    assert_eq!(f.api.calls.lock().unwrap().len(), 1);
}
