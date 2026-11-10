use super::*;
use crate::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use lingxi_core::host::OrchestratorHandle;
use std::sync::Arc;

struct HeldGoalEvaluator {
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl hooks::HookPromptRunner for HeldGoalEvaluator {
    async fn run(
        &self,
        request: hooks::PromptHookRequest,
    ) -> Result<lingxi_core::types::utf16_json::Utf16JsonProjection, hooks::PromptHookError> {
        // Stop prompts contain the Native condition framing and serialized
        // event arguments, rather than the unprocessed template alone.
        let condition = request
            .prompt
            .string_at("")
            .and_then(|prompt| prompt.split_once("\n\nARGUMENTS: "))
            .and_then(|(framed, _)| framed.rsplit_once("\n\nCondition: "))
            .map(|(_, condition)| condition);
        if condition == Some("ordinary") {
            return Ok(r#"{"ok":false,"reason":"ordinary policy"}"#.into());
        }
        self.started.notify_one();
        self.release.notified().await;
        Ok(r#"{"ok":false,"reason":"old condition is not met"}"#.into())
    }
}

fn fixture_base(hooks: Arc<hooks::HookExecutorImpl>) -> ConversationOrchestrator {
    ConversationOrchestrator::new(
        crate::OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(tool_api::ToolRegistry::new()),
        hooks,
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    )
}

fn fixture(hooks: Arc<hooks::HookExecutorImpl>) -> Arc<ConversationOrchestrator> {
    ConversationOrchestrator::into_shared(fixture_base(hooks))
}

#[tokio::test]
async fn cleared_goal_block_must_not_reenter_the_generic_stop_block_branch() {
    let evaluator = Arc::new(HeldGoalEvaluator {
        started: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let hooks = Arc::new(
        Arc::try_unwrap(noop_hook_executor())
            .ok()
            .unwrap()
            .with_prompt_runner(evaluator.clone()),
    );
    let orch = fixture(hooks);
    orch.set_active_goal(&"old condition".into()).await;
    let running = orch.clone();
    let stop = tokio::spawn(async move {
        let mut active = false;
        let mut blocks = 0;
        running
            .handle_stop_at_end(
                "end_turn",
                &mut active,
                &mut blocks,
                1,
                MessageId::new(),
                false,
            )
            .await
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        evaluator.started.notified(),
    )
    .await
    .unwrap();
    orch.clear_active_goal().await;
    evaluator.release.notify_one();
    let flow = tokio::time::timeout(std::time::Duration::from_secs(2), stop)
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(flow, StopHookFlow::FallThrough),
        "a cleared evaluator must not drive LoopAgain through agg.decision"
    );
    assert!(!orch
        .session
        .lock()
        .await
        .history
        .iter()
        .any(|message| message.text_content().contains("old condition is not met")));
}

#[tokio::test]
async fn cleared_goal_must_release_its_deferral_bookkeeping() {
    let orch = fixture(noop_hook_executor());
    orch.set_active_goal(&"old condition".into()).await;
    {
        let mut state = orch.lifecycle_runtime.goal_checkin.lock().unwrap();
        state.deferred_since = Some(1);
        state.checkin_count = 7;
        state.idle_checkin_count = 3;
        state.last_deferral_pass_at = Some(2);
        state.last_deferring_ids = vec!["old-shell".into()];
    }
    orch.clear_active_goal().await;
    orch.set_active_goal(&"replacement condition".into()).await;
    assert_eq!(
        *orch.lifecycle_runtime.goal_checkin.lock().unwrap(),
        crate::prompt::goal_checkin::GoalDeferralState::default()
    );
}

#[tokio::test]
async fn replacing_goal_releases_its_deferral_bookkeeping_and_timer() {
    let orch = fixture(noop_hook_executor());
    orch.set_active_goal(&"old condition".into()).await;
    {
        let mut state = orch.lifecycle_runtime.goal_checkin.lock().unwrap();
        state.deferred_since = Some(1);
        state.checkin_count = 7;
        state.idle_checkin_count = 3;
    }
    let generation = orch
        .lifecycle_runtime
        .goal_checkin_idle_generation
        .load(std::sync::atomic::Ordering::Acquire);
    orch.set_active_goal(&"replacement condition".into()).await;
    assert_eq!(
        *orch.lifecycle_runtime.goal_checkin.lock().unwrap(),
        crate::prompt::goal_checkin::GoalDeferralState::default()
    );
    assert!(
        orch.lifecycle_runtime
            .goal_checkin_idle_generation
            .load(std::sync::atomic::Ordering::Acquire)
            > generation
    );
    assert!(!orch
        .lifecycle_runtime
        .goal_checkin_idle_running
        .load(std::sync::atomic::Ordering::Acquire));
}

#[tokio::test]
async fn cleared_goal_keeps_an_independent_ordinary_stop_block() {
    let evaluator = Arc::new(HeldGoalEvaluator {
        started: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let hooks = Arc::new(
        Arc::try_unwrap(noop_hook_executor())
            .ok()
            .unwrap()
            .with_prompt_runner(evaluator.clone()),
    );
    let orch = fixture(hooks);
    orch.set_active_goal(&"old condition".into()).await;
    let session_id = orch.session.lock().await.session_id;
    let mut ordinary = orch
        .hooks
        .get_session_named_hook(session_id, GOAL_STOP_HOOK_NAME)
        .await
        .unwrap();
    ordinary.id = HookId::new();
    ordinary.name = "ordinary".into();
    ordinary.priority = 0;
    ordinary.executor = hooks::HookExecutor::Prompt {
        prompt: "ordinary".into(),
        model: None,
        continue_on_block: false,
    };
    orch.hooks
        .upsert_session_named_hook(session_id, "ordinary".into(), ordinary)
        .await;
    let running = orch.clone();
    let stop = tokio::spawn(async move { running.fire_stop_hooks("end_turn", false, false).await });
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        evaluator.started.notified(),
    )
    .await
    .unwrap();
    orch.clear_active_goal().await;
    evaluator.release.notify_one();
    let disposition = stop.await.unwrap();
    assert!(
        matches!(disposition, StopHookDisposition::Continue(reason) if reason.string_at("") == Some("[ordinary]: ordinary policy"))
    );
}

struct HeldIdleTaskSnapshot {
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
    tasks: Vec<hooks::HookBackgroundTask>,
}

#[async_trait::async_trait]
impl crate::stop_hook_snapshot::StopHookSnapshotProvider for HeldIdleTaskSnapshot {
    async fn background_tasks(&self) -> Vec<hooks::HookBackgroundTask> {
        self.started.notify_one();
        self.release.notified().await;
        self.tasks.clone()
    }
    async fn session_crons(&self) -> Vec<hooks::HookSessionCron> {
        Vec::new()
    }
}

#[tokio::test(start_paused = true)]
async fn retired_idle_provider_cannot_repopulate_cleared_or_replaced_goal_counters() {
    for replace in [false, true] {
        let orch = fixture(noop_hook_executor());
        orch.set_active_goal(&"old condition".into()).await;
        orch.lifecycle_runtime
            .goal_checkin
            .lock()
            .unwrap()
            .deferred_since = Some(0);
        let provider = Arc::new(HeldIdleTaskSnapshot {
            started: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
            tasks: vec![hooks::HookBackgroundTask {
                id: "old-shell".into(),
                r#type: "shell".into(),
                status: "running".into(),
                description: "old work".into(),
                command: None,
                agent_type: None,
                server: None,
                tool: None,
                name: None,
                is_idle: false,
            }],
        });
        let generation = orch
            .lifecycle_runtime
            .goal_checkin_idle_generation
            .load(std::sync::atomic::Ordering::Acquire);
        // Keep this task outside the stored abort handle to prove that owner
        // validation protects even a task already executing a ready provider.
        let running = orch.clone();
        let snapshot = provider.clone();
        let idle = tokio::spawn(async move {
            ConversationOrchestrator::run_goal_checkin_idle_loop(
                snapshot,
                Some(Arc::downgrade(&running)),
                running.session.clone(),
                running.turn_gate.clone(),
                running.lifecycle_runtime.goal_checkin.clone(),
                running.lifecycle_runtime.goal_checkin_idle_running.clone(),
                running
                    .lifecycle_runtime
                    .goal_checkin_idle_generation
                    .clone(),
                generation,
                None,
            )
            .await;
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(61),
            provider.started.notified(),
        )
        .await
        .unwrap();
        if replace {
            orch.set_active_goal(&"replacement condition".into()).await;
        } else {
            orch.clear_active_goal().await;
        }
        provider.release.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(1), idle)
            .await
            .expect("a retired provider continuation must stop")
            .unwrap();
        assert_eq!(
            *orch.lifecycle_runtime.goal_checkin.lock().unwrap(),
            crate::prompt::goal_checkin::GoalDeferralState::default()
        );
    }
}

#[tokio::test]
async fn rematched_replacement_goal_block_stays_out_of_the_ordinary_stop_channel() {
    let evaluator = Arc::new(HeldGoalEvaluator {
        started: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let hooks = Arc::new(
        Arc::try_unwrap(noop_hook_executor())
            .ok()
            .unwrap()
            .with_prompt_runner(evaluator.clone()),
    );
    let provider = Arc::new(HeldIdleTaskSnapshot {
        started: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
        tasks: Vec::new(),
    });
    let orch = ConversationOrchestrator::into_shared(
        fixture_base(hooks).with_stop_hook_snapshot(provider.clone()),
    );
    orch.set_active_goal(&"goal A".into()).await;
    let running = orch.clone();
    let stop = tokio::spawn(async move { running.fire_stop_hooks("end_turn", false, false).await });
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        provider.started.notified(),
    )
    .await
    .unwrap();
    orch.set_active_goal(&"goal B".into()).await;
    provider.release.notify_one();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        evaluator.started.notified(),
    )
    .await
    .unwrap();
    orch.clear_active_goal().await;
    evaluator.release.notify_one();
    let disposition = tokio::time::timeout(std::time::Duration::from_secs(2), stop)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(disposition, StopHookDisposition::Pass));
}

struct HeldCheckinTelemetry {
    event: &'static str,
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl telemetry::AnalyticsSink for HeldCheckinTelemetry {
    async fn log_event(&self, name: &str, _: telemetry::LogEventMetadata) {
        if name == self.event {
            self.started.notify_one();
            self.release.notified().await;
        }
    }
    async fn log_event_async(&self, name: &str, metadata: telemetry::LogEventMetadata) {
        self.log_event(name, metadata).await;
    }
    fn name(&self) -> &str {
        "held-checkin"
    }
}

#[tokio::test]
async fn retired_turn_end_checkin_does_not_publish_after_awaited_telemetry() {
    for replace in [false, true] {
        let bus = Arc::new(telemetry::AnalyticsBus::new());
        let telemetry = Arc::new(HeldCheckinTelemetry {
            event: "tengu_goal_checkin_injected",
            started: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        bus.attach_sink(telemetry.clone()).await;
        let orch = ConversationOrchestrator::into_shared(
            fixture_base(noop_hook_executor()).with_analytics_bus(bus),
        );
        orch.set_active_goal(&"old condition".into()).await;
        orch.lifecycle_runtime
            .goal_checkin
            .lock()
            .unwrap()
            .deferred_since = Some(0);
        let running = orch.clone();
        let pass = tokio::spawn(async move {
            running
                .goal_checkin_pass_tasks(&[crate::prompt::goal_checkin::DeferringTask {
                    id: "old-shell".into(),
                    start_time_ms: None,
                    label: "shell".into(),
                    detail: "old work".into(),
                }])
                .await
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            telemetry.started.notified(),
        )
        .await
        .unwrap();
        if replace {
            orch.set_active_goal(&"replacement condition".into()).await;
        } else {
            orch.clear_active_goal().await;
        }
        telemetry.release.notify_one();
        let (deferred, emitted, owner) =
            tokio::time::timeout(std::time::Duration::from_secs(2), pass)
                .await
                .unwrap()
                .unwrap();
        assert!(!deferred && !emitted && owner.is_none());
        assert!(!orch
            .session
            .lock()
            .await
            .history
            .iter()
            .any(|message| message.text_content().contains("old condition")));
        assert_eq!(
            *orch.lifecycle_runtime.goal_checkin.lock().unwrap(),
            crate::prompt::goal_checkin::GoalDeferralState::default()
        );
    }
}

#[tokio::test]
async fn replacement_goal_metadata_follows_a_pending_clear_and_restores_the_live_goal() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("goal-ordering.jsonl");
    let fs = Arc::new(platform_posix::fs::PosixFileSystem::new(
        root.path().to_path_buf(),
    ));
    let writer = Arc::new(session::jsonl::JsonlWriter::new(path.clone(), fs));
    let orch = ConversationOrchestrator::into_shared(
        fixture_base(noop_hook_executor()).with_jsonl_writer(writer),
    );
    orch.set_active_goal(&"old condition".into()).await;
    let gate = Arc::new(crate::request_context::PromptIngressAppendTestGate::default());
    let running = orch.clone();
    let pending = gate.clone();
    let clear = tokio::spawn(async move {
        crate::request_context::PROMPT_INGRESS_APPEND_TEST_GATE
            .scope(pending, running.clear_active_goal())
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), gate.wait_until_entered())
        .await
        .unwrap();
    let replacement =
        lingxi_core::types::utf16_json::Utf16JsonProjection::from("replacement condition");
    let set = orch.set_active_goal(&replacement);
    tokio::pin!(set);
    assert!(matches!(
        futures::poll!(set.as_mut()),
        std::task::Poll::Pending
    ));
    assert!(
        orch.get_active_goal().await.is_none(),
        "replacement waits for the old metadata write"
    );
    gate.resume();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        let _ = tokio::join!(clear, set);
    })
    .await
    .unwrap();
    let rows = std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str::<session::JsonlMessage>(line).ok())
        .collect::<Vec<_>>();
    let session_id = orch.session.lock().await.session_id;
    let restored = crate::resume::state_from_messages(session_id.as_uuid(), &rows);
    assert_eq!(
        restored.active_goal.unwrap().condition.string_at(""),
        Some("replacement condition")
    );
    let hook = orch
        .hooks
        .get_session_named_hook(session_id, GOAL_STOP_HOOK_NAME)
        .await
        .unwrap();
    assert!(
        matches!(hook.executor, hooks::HookExecutor::Prompt { prompt, .. } if prompt.string_at("") == Some("replacement condition"))
    );
}

#[tokio::test]
async fn retired_goal_control_is_rechecked_after_evaluation_telemetry() {
    let bus = Arc::new(telemetry::AnalyticsBus::new());
    let telemetry = Arc::new(HeldCheckinTelemetry {
        event: "tengu_goal_evaluated",
        started: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    bus.attach_sink(telemetry.clone()).await;
    let evaluator = Arc::new(HeldGoalEvaluator {
        started: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let hooks = Arc::new(
        Arc::try_unwrap(noop_hook_executor())
            .ok()
            .unwrap()
            .with_prompt_runner(evaluator.clone()),
    );
    let orch = ConversationOrchestrator::into_shared(fixture_base(hooks).with_analytics_bus(bus));
    orch.set_active_goal(&"old condition".into()).await;
    let running = orch.clone();
    let stop = tokio::spawn(async move { running.fire_stop_hooks("end_turn", false, false).await });
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        evaluator.started.notified(),
    )
    .await
    .unwrap();
    evaluator.release.notify_one();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        telemetry.started.notified(),
    )
    .await
    .unwrap();
    orch.clear_active_goal().await;
    telemetry.release.notify_one();
    let disposition = tokio::time::timeout(std::time::Duration::from_secs(2), stop)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(disposition, StopHookDisposition::Pass));
}

#[tokio::test]
async fn retired_deferred_goal_control_is_rechecked_after_evaluation_telemetry() {
    let bus = Arc::new(telemetry::AnalyticsBus::new());
    let telemetry = Arc::new(HeldCheckinTelemetry {
        event: "tengu_goal_evaluated",
        started: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    bus.attach_sink(telemetry.clone()).await;
    let provider = Arc::new(HeldIdleTaskSnapshot {
        started: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
        tasks: vec![hooks::HookBackgroundTask {
            id: "old-shell".into(),
            r#type: "shell".into(),
            status: "running".into(),
            description: "old work".into(),
            command: None,
            agent_type: None,
            server: None,
            tool: None,
            name: None,
            is_idle: false,
        }],
    });
    provider.release.notify_one();
    let orch = ConversationOrchestrator::into_shared(
        fixture_base(noop_hook_executor())
            .with_stop_hook_snapshot(provider)
            .with_analytics_bus(bus),
    );
    orch.set_active_goal(&"old condition".into()).await;
    orch.lifecycle_runtime
        .goal_checkin
        .lock()
        .unwrap()
        .deferred_since = Some(0);
    let running = orch.clone();
    let stop = tokio::spawn(async move { running.fire_stop_hooks("end_turn", false, false).await });
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        telemetry.started.notified(),
    )
    .await
    .unwrap();
    orch.clear_active_goal().await;
    telemetry.release.notify_one();
    let disposition = tokio::time::timeout(std::time::Duration::from_secs(2), stop)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(disposition, StopHookDisposition::Pass));
}

struct HeldFeedbackFileSystem {
    inner: Arc<dyn lingxi_core::host::FileSystem>,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    claimed: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl lingxi_core::host::FileSystem for HeldFeedbackFileSystem {
    async fn read_file(
        &self,
        path: &str,
        offset: Option<u64>,
        limit: Option<u64>,
    ) -> Result<lingxi_core::host::filesystem::FileContent, lingxi_core::host::FsError> {
        self.inner.read_file(path, offset, limit).await
    }
    async fn write_file(
        &self,
        path: &str,
        content: &str,
    ) -> Result<(), lingxi_core::host::FsError> {
        self.inner.write_file(path, content).await
    }
    fn is_within_workspace(&self, path: &str) -> bool {
        self.inner.is_within_workspace(path)
    }
    async fn watch(
        &self,
        path: &str,
    ) -> Result<
        std::pin::Pin<
            Box<dyn futures::Stream<Item = lingxi_core::host::filesystem::FileEvent> + Send>,
        >,
        lingxi_core::host::FsError,
    > {
        self.inner.watch(path).await
    }
    async fn append_file(
        &self,
        path: &str,
        content: &str,
    ) -> Result<(), lingxi_core::host::FsError> {
        self.inner.append_file(path, content).await?;
        // JsonlWriter::new uses append_file_with_mode, whose default dispatches
        // through append_file. Pause the real post-write boundary as well as
        // the rooted adapter below.
        if content.contains("Stop hook feedback:")
            && !self.claimed.swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            self.entered.notify_one();
            self.release.notified().await;
        }
        Ok(())
    }
    async fn append_file_rooted_staged(
        &self,
        root: &std::path::Path,
        relative: &std::path::Path,
        content: &str,
        expected: Option<&lingxi_core::host::rooted_fs::RootIdentity>,
    ) -> Result<(), lingxi_core::host::filesystem::FileAppendError> {
        self.inner
            .append_file_rooted_staged(root, relative, content, expected)
            .await?;
        if content.contains("Stop hook feedback:")
            && !self.claimed.swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            self.entered.notify_one();
            self.release.notified().await;
        }
        Ok(())
    }
    async fn truncate(&self, path: &str, len: u64) -> Result<(), lingxi_core::host::FsError> {
        self.inner.truncate(path, len).await
    }
    async fn file_mtime(
        &self,
        path: &str,
    ) -> Result<std::time::SystemTime, lingxi_core::host::FsError> {
        self.inner.file_mtime(path).await
    }
    async fn file_size(&self, path: &str) -> Result<u64, lingxi_core::host::FsError> {
        self.inner.file_size(path).await
    }
    async fn delete_file(&self, path: &str) -> Result<(), lingxi_core::host::FsError> {
        self.inner.delete_file(path).await
    }
    async fn symlink(&self, target: &str, link: &str) -> Result<(), lingxi_core::host::FsError> {
        self.inner.symlink(target, link).await
    }
    async fn flock_exclusive(
        &self,
        path: &str,
    ) -> Result<Box<dyn lingxi_core::host::filesystem::FlockGuard>, lingxi_core::host::FsError>
    {
        self.inner.flock_exclusive(path).await
    }
    async fn fsync(&self, path: &str) -> Result<(), lingxi_core::host::FsError> {
        self.inner.fsync(path).await
    }
}

#[tokio::test]
async fn late_goal_retirement_keeps_an_ordinary_block_across_feedback_persistence() {
    let root = tempfile::tempdir().unwrap();
    let fs = Arc::new(HeldFeedbackFileSystem {
        inner: Arc::new(platform_posix::fs::PosixFileSystem::new(
            root.path().to_path_buf(),
        )),
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
        claimed: std::sync::atomic::AtomicBool::new(false),
    });
    let writer = Arc::new(session::jsonl::JsonlWriter::new(
        root.path().join("feedback.jsonl"),
        fs.clone(),
    ));
    let evaluator = Arc::new(HeldGoalEvaluator {
        started: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let hooks = Arc::new(
        Arc::try_unwrap(noop_hook_executor())
            .ok()
            .unwrap()
            .with_prompt_runner(evaluator.clone()),
    );
    let orch = ConversationOrchestrator::into_shared(fixture_base(hooks).with_jsonl_writer(writer));
    orch.set_active_goal(&"old condition".into()).await;
    let session_id = orch.session.lock().await.session_id;
    let mut ordinary = orch
        .hooks
        .get_session_named_hook(session_id, GOAL_STOP_HOOK_NAME)
        .await
        .unwrap();
    ordinary.id = HookId::new();
    ordinary.name = "ordinary".into();
    ordinary.priority = 0;
    ordinary.executor = hooks::HookExecutor::Prompt {
        prompt: "ordinary".into(),
        model: None,
        continue_on_block: false,
    };
    orch.hooks
        .upsert_session_named_hook(session_id, "ordinary".into(), ordinary)
        .await;
    let running = orch.clone();
    let stop = tokio::spawn(async move {
        let mut active = false;
        let mut blocks = 0;
        running
            .handle_stop_at_end(
                "end_turn",
                &mut active,
                &mut blocks,
                1,
                MessageId::new(),
                false,
            )
            .await
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        evaluator.started.notified(),
    )
    .await
    .unwrap();
    evaluator.release.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(2), fs.entered.notified())
        .await
        .unwrap();
    let running = orch.clone();
    let clear = tokio::spawn(async move { running.clear_active_goal().await });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while orch.get_active_goal().await.is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    fs.release.notify_one();
    let flow = tokio::time::timeout(std::time::Duration::from_secs(2), stop)
        .await
        .unwrap()
        .unwrap();
    clear.await.unwrap();
    assert!(matches!(flow, StopHookFlow::LoopAgain));
    assert!(orch
        .session
        .lock()
        .await
        .history
        .iter()
        .any(|message| message
            .text_content()
            .contains("Stop hook feedback:\n[ordinary]: ordinary policy")));
}

struct FixedGoalVerdict(&'static str);
#[async_trait::async_trait]
impl hooks::HookPromptRunner for FixedGoalVerdict {
    async fn run(
        &self,
        _: hooks::PromptHookRequest,
    ) -> Result<lingxi_core::types::utf16_json::Utf16JsonProjection, hooks::PromptHookError> {
        Ok(self.0.into())
    }
}

#[tokio::test]
async fn old_matched_goal_verdict_cannot_commit_during_replacement_role_publication() {
    for verdict in [r#"{"ok":false,"reason":"old verdict"}"#, r#"{"ok":true}"#] {
        let root = tempfile::tempdir().unwrap();
        let fs = Arc::new(platform_posix::fs::PosixFileSystem::new(
            root.path().to_path_buf(),
        ));
        let writer = Arc::new(session::jsonl::JsonlWriter::new(
            root.path().join("role-ordering.jsonl"),
            fs,
        ));
        let hooks = Arc::new(
            Arc::try_unwrap(noop_hook_executor())
                .ok()
                .unwrap()
                .with_prompt_runner(Arc::new(FixedGoalVerdict(verdict))),
        );
        let orch =
            ConversationOrchestrator::into_shared(fixture_base(hooks).with_jsonl_writer(writer));
        orch.set_active_goal(&"goal B".into()).await;
        let aggregate = orch
            .hooks
            .execute(
                hooks::HookEvent::Stop {
                    reason: "end_turn".into(),
                },
                orch.lifecycle_hook_ctx(false).await,
            )
            .await;
        let old_hook = aggregate.session_hook_ids.get(GOAL_STOP_HOOK_NAME).copied();
        let gate = Arc::new(crate::request_context::PromptIngressAppendTestGate::default());
        let pending = gate.clone();
        let running = orch.clone();
        let replacement = tokio::spawn(async move {
            crate::request_context::PROMPT_INGRESS_APPEND_TEST_GATE
                .scope(pending, running.set_active_goal(&"goal C".into()))
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), gate.wait_until_entered())
            .await
            .unwrap();
        // State is C while the old role B remains installed until C's
        // metadata append finishes. Poll exactly through the uncontended
        // session/registry reads to the held transition lock.
        let apply = orch.goal_stop_hook_disposition(old_hook, &aggregate, false);
        tokio::pin!(apply);
        assert!(matches!(
            futures::poll!(apply.as_mut()),
            std::task::Poll::Pending
        ));
        gate.resume();
        replacement.await.unwrap();
        let (control, outcome) = tokio::time::timeout(std::time::Duration::from_secs(2), apply)
            .await
            .unwrap();
        assert!(control.is_none());
        assert!(matches!(outcome, GoalEvalOutcome::Absent));
        let active = orch.get_active_goal().await.unwrap();
        assert_eq!(active.condition.string_at(""), Some("goal C"));
        assert_eq!(active.iterations, 0);
        assert!(active.last_reason.is_none());
    }
}

struct GoalInterruptionQueue;
#[async_trait::async_trait]
impl crate::prompt::mid_turn_input::MidTurnInputSource for GoalInterruptionQueue {
    fn supports_goal_retries(&self) -> bool {
        true
    }

    async fn take_mid_turn_input(
        &self,
    ) -> Option<lingxi_core::types::utf16_json::Utf16JsonProjection> {
        None
    }
}

#[tokio::test]
async fn retired_goal_interruption_cannot_act_during_replacement_role_publication() {
    for failed_outcome in [
        hooks::HookOutcome::Timeout,
        hooks::HookOutcome::Cancelled,
        hooks::HookOutcome::Error,
    ] {
        let root = tempfile::tempdir().unwrap();
        let writer = Arc::new(session::jsonl::JsonlWriter::new(
            root.path().join("interruption-role-ordering.jsonl"),
            Arc::new(platform_posix::fs::PosixFileSystem::new(
                root.path().to_path_buf(),
            )),
        ));
        let hooks = Arc::new(
            Arc::try_unwrap(noop_hook_executor())
                .ok()
                .unwrap()
                .with_prompt_runner(Arc::new(FixedGoalVerdict(r#"{"ok":true}"#))),
        );
        let output = Arc::new(MockOutputStream::new());
        let mut base = fixture_base(hooks)
            .with_jsonl_writer(writer)
            .with_mid_turn_input(Arc::new(GoalInterruptionQueue));
        base.config.interactive_session = true;
        base.output = output.clone();
        let orch = ConversationOrchestrator::into_shared(base);
        orch.enable_goal_retries();
        orch.set_active_goal(&"goal B".into()).await;
        let mut aggregate = orch
            .hooks
            .execute(
                hooks::HookEvent::Stop {
                    reason: "end_turn".into(),
                },
                orch.lifecycle_hook_ctx(false).await,
            )
            .await;
        let old_hook = aggregate.session_hook_ids.get(GOAL_STOP_HOOK_NAME).copied();
        let result = &mut aggregate
            .all_results
            .iter_mut()
            .find(|(id, _)| Some(*id) == old_hook)
            .unwrap()
            .1;
        result.outcome = failed_outcome;
        result.response = None;
        let gate = Arc::new(crate::request_context::PromptIngressAppendTestGate::default());
        let pending = gate.clone();
        let running = orch.clone();
        let replacement = tokio::spawn(async move {
            crate::request_context::PROMPT_INGRESS_APPEND_TEST_GATE
                .scope(pending, running.set_active_goal(&"goal C".into()))
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), gate.wait_until_entered())
            .await
            .unwrap();
        // C is already active, but the registry still names B while its
        // successor's durable publication is held. Interruption admission must
        // wait for that same transition and then reject the actual old role.
        let apply = orch.goal_stop_hook_disposition(old_hook, &aggregate, false);
        tokio::pin!(apply);
        assert!(matches!(
            futures::poll!(apply.as_mut()),
            std::task::Poll::Pending
        ));
        gate.resume();
        replacement.await.unwrap();
        let (control, outcome) = apply.await;
        assert!(control.is_none());
        assert_eq!(outcome, GoalEvalOutcome::Absent);
        assert!(!output
            .snapshot()
            .await
            .iter()
            .any(|event| matches!(event, lingxi_core::host::OutputEvent::SystemNotice { .. })));
        let active = orch.get_active_goal().await.unwrap();
        assert_eq!(active.condition.string_at(""), Some("goal C"));
        assert_eq!(active.iterations, 0);

        // A current evaluator failure still keeps its ordinary recovery path.
        let mut current = orch
            .hooks
            .execute(
                hooks::HookEvent::Stop {
                    reason: "end_turn".into(),
                },
                orch.lifecycle_hook_ctx(false).await,
            )
            .await;
        let current_hook = current.session_hook_ids.get(GOAL_STOP_HOOK_NAME).copied();
        let result = &mut current
            .all_results
            .iter_mut()
            .find(|(id, _)| Some(*id) == current_hook)
            .unwrap()
            .1;
        result.outcome = failed_outcome;
        result.response = None;
        let (_, current_outcome) = orch
            .goal_stop_hook_disposition(current_hook, &current, false)
            .await;
        assert_eq!(
            current_outcome,
            if failed_outcome == hooks::HookOutcome::Error {
                GoalEvalOutcome::Error
            } else {
                GoalEvalOutcome::Cancelled
            }
        );
        assert_eq!(
            output
                .snapshot()
                .await
                .iter()
                .filter(|event| matches!(
                    event,
                    lingxi_core::host::OutputEvent::SystemNotice { .. }
                ))
                .count(),
            1
        );
        orch.clear_active_goal().await;
    }
}
