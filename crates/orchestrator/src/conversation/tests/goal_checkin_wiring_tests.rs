use crate::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use crate::ConversationOrchestrator;
use crate::OrchestratorConfig;
use lingxi_core::host::OrchestratorHandle;
use std::path::PathBuf;
use std::sync::{atomic::Ordering, Arc};
use tool_api::registry::ToolRegistry;

fn orch() -> ConversationOrchestrator {
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

fn task(id: &str, kind: &str, agent_type: Option<&str>) -> hooks::HookBackgroundTask {
    hooks::HookBackgroundTask {
        is_idle: false,
        id: id.into(),
        r#type: kind.into(),
        status: "running".into(),
        description: "doing things".into(),
        command: None,
        agent_type: agent_type.map(str::to_string),
        server: None,
        tool: None,
        name: None,
    }
}

async fn set_goal(orch: &ConversationOrchestrator, condition: &str) {
    let mut s = orch.session.lock().await;
    s.active_goal = Some(lingxi_core::session::ActiveGoalState {
        condition: condition.into(),
        set_at: std::time::SystemTime::now(),
        last_reason: None,
        iterations: 0,
        tokens_at_start: 0,
        origin: lingxi_core::session::GoalOrigin::User,
    });
}

struct GoalStopSnapshot;

#[async_trait::async_trait]
impl crate::stop_hook_snapshot::StopHookSnapshotProvider for GoalStopSnapshot {
    async fn background_tasks(&self) -> Vec<hooks::HookBackgroundTask> {
        vec![task("b1", "shell", None)]
    }

    async fn session_crons(&self) -> Vec<hooks::HookSessionCron> {
        Vec::new()
    }
}

struct EmptyGoalStopSnapshot;

#[async_trait::async_trait]
impl crate::stop_hook_snapshot::StopHookSnapshotProvider for EmptyGoalStopSnapshot {
    async fn background_tasks(&self) -> Vec<hooks::HookBackgroundTask> {
        Vec::new()
    }

    async fn session_crons(&self) -> Vec<hooks::HookSessionCron> {
        Vec::new()
    }
}

#[tokio::test]
async fn no_active_goal_never_defers() {
    let orch = orch();
    assert!(!orch.goal_checkin_pass(&[task("b1", "shell", None)]).await);
}

#[tokio::test]
async fn a_running_shell_defers_the_goal_evaluation_and_arms_the_clock() {
    let orch = orch();
    set_goal(&orch, "ship it").await;
    assert!(
        orch.goal_checkin_pass(&[task("b1", "shell", None)]).await,
        "background work ⇒ the goal is NOT evaluated this turn"
    );
    assert!(orch
        .lifecycle_runtime
        .goal_checkin
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .deferred_since
        .is_some());
    // The first pass only starts the clock — no interstitial yet.
    assert!(orch.session.lock().await.history.is_empty());
}

/// `_qf` keeps only agent-ish + shell tasks, and never the `main-session`
/// agent; a monitor / dream / MCP task must NOT defer the goal.
#[tokio::test]
async fn non_deferring_task_kinds_leave_the_goal_evaluable() {
    let orch = orch();
    set_goal(&orch, "ship it").await;
    assert!(
        !orch
            .goal_checkin_pass(&[
                task("m1", "monitor", None),
                task("d1", "dream", None),
                task("x1", "MCP task", None),
                task("a1", "subagent", Some("main-session")),
            ])
            .await,
        "none of these defer a goal"
    );
}

/// Once the deferral has run past the interval, the interstitial is appended
/// as a plain meta user message — NOT `<system-reminder>`-wrapped.
#[tokio::test]
async fn past_the_interval_the_interstitial_is_appended_unwrapped() {
    let orch = orch();
    set_goal(&orch, "ship it").await;
    // Pretend the stretch started an hour ago.
    {
        let mut state = orch
            .lifecycle_runtime
            .goal_checkin
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.deferred_since = Some(0);
        state.last_deferral_pass_at = Some(0);
        state.last_deferring_ids = vec!["b1".into()];
    }
    assert!(orch.goal_checkin_pass(&[task("b1", "shell", None)]).await);

    let history = orch.session.lock().await.history.clone();
    assert_eq!(history.len(), 1);
    let text = history[0].text_content();
    assert!(
        text.starts_with("Goal check-in: \u{ab}ship it\u{bb} is still active, and evaluation has been deferred for "),
        "got: {text}"
    );
    assert!(
        !text.contains("<system-reminder>"),
        "the interstitial is a bare meta message; got: {text}"
    );
    assert!(
        text.contains("- b1 \u{b7} shell \u{b7} doing things"),
        "got: {text}"
    );
}

/// The `else if(L.deferredSince!==void 0)` arm: background work finished ⇒
/// the deferral bookkeeping is dropped and the goal is evaluated again.
#[tokio::test]
async fn an_empty_task_set_clears_the_deferral_state() {
    let orch = orch();
    set_goal(&orch, "ship it").await;
    assert!(orch.goal_checkin_pass(&[task("b1", "shell", None)]).await);
    assert!(!orch.goal_checkin_pass(&[]).await);
    assert_eq!(
        *orch
            .lifecycle_runtime
            .goal_checkin
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        crate::prompt::goal_checkin::GoalDeferralState::default()
    );
}

#[tokio::test]
async fn deferral_arms_the_idle_timer_and_clear_cancels_it() {
    let orch = orch().with_stop_hook_snapshot(Arc::new(GoalStopSnapshot));
    set_goal(&orch, "ship it").await;

    assert!(orch.goal_checkin_pass(&[task("b1", "shell", None)]).await);
    assert!(
        orch.lifecycle_runtime
            .goal_checkin_idle_running
            .load(Ordering::SeqCst),
        "deferral should arm the idle loop"
    );
    assert!(orch
        .lifecycle_runtime
        .goal_checkin_idle_task
        .lock()
        .expect("goal checkin idle task")
        .is_some());

    assert!(!orch.goal_checkin_pass(&[]).await);
    assert!(
        !orch
            .lifecycle_runtime
            .goal_checkin_idle_running
            .load(Ordering::SeqCst),
        "clearing the stretch should drop the running marker"
    );
    assert!(orch
        .lifecycle_runtime
        .goal_checkin_idle_task
        .lock()
        .expect("goal checkin idle task")
        .is_none());
}

/// 2.1.266 `HZ` floors the idle re-arm at `qmt` (60 s), so this test can no
/// longer observe the loop by waiting in real time — it runs on the paused
/// clock, which auto-advances to each sleep's deadline while the runtime is
/// idle.
#[tokio::test(start_paused = true)]
async fn idle_loop_exit_allows_a_new_deferral_stretch_to_rearm() {
    let mut orch = orch().with_stop_hook_snapshot(Arc::new(EmptyGoalStopSnapshot));
    set_goal(&orch, "ship it").await;
    {
        let mut state = orch
            .lifecycle_runtime
            .goal_checkin
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.deferred_since = Some(0);
    }

    let first_generation = orch
        .lifecycle_runtime
        .goal_checkin_idle_generation
        .load(Ordering::SeqCst);
    orch.sync_goal_checkin_idle_task().await;
    // Virtual seconds: the loop's first sleep is a full 60 s re-arm floor.
    tokio::time::timeout(std::time::Duration::from_secs(600), async {
        while orch
            .lifecycle_runtime
            .goal_checkin_idle_running
            .load(Ordering::SeqCst)
        {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("the first idle loop should observe the empty task set");
    assert!(
        !orch
            .lifecycle_runtime
            .goal_checkin_idle_running
            .load(Ordering::SeqCst),
        "the first idle loop should exit after seeing no deferring tasks"
    );
    assert!(
        orch.lifecycle_runtime
            .goal_checkin_idle_generation
            .load(Ordering::SeqCst)
            > first_generation,
        "arming should advance the loop generation"
    );

    {
        let mut state = orch
            .lifecycle_runtime
            .goal_checkin
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.deferred_since = Some(0);
    }
    orch.lifecycle_runtime.stop_hook_snapshot = Some(Arc::new(GoalStopSnapshot));
    let second_generation = orch
        .lifecycle_runtime
        .goal_checkin_idle_generation
        .load(Ordering::SeqCst);
    orch.sync_goal_checkin_idle_task().await;
    assert!(orch
        .lifecycle_runtime
        .goal_checkin_idle_running
        .load(Ordering::SeqCst));
    assert!(
        orch.lifecycle_runtime
            .goal_checkin_idle_generation
            .load(Ordering::SeqCst)
            > second_generation,
        "a new stretch should arm a fresh idle loop"
    );
}

#[tokio::test]
async fn an_idle_teammate_does_not_defer_but_a_working_teammate_does() {
    let orch = orch();
    set_goal(&orch, "ship it").await;
    let mut teammate = task("t1", "teammate", None);
    teammate.is_idle = true;
    assert!(!orch.goal_checkin_pass(&[teammate.clone()]).await);
    teammate.is_idle = false;
    assert!(orch.goal_checkin_pass(&[teammate]).await);
}

#[derive(Default)]
struct IdleQueue(std::sync::Mutex<Vec<lingxi_core::types::utf16_json::Utf16JsonProjection>>);
#[async_trait::async_trait]
impl crate::prompt::mid_turn_input::MidTurnInputSource for IdleQueue {
    fn supports_goal_retries(&self) -> bool {
        true
    }
    async fn enqueue_goal_retry(
        &self,
        _: String,
        body: lingxi_core::types::utf16_json::Utf16JsonProjection,
        _: tokio_util::sync::CancellationToken,
    ) {
        self.0.lock().unwrap().push(body);
    }
    async fn take_mid_turn_input(
        &self,
    ) -> Option<lingxi_core::types::utf16_json::Utf16JsonProjection> {
        None
    }
}

#[tokio::test(start_paused = true)]
async fn idle_checkin_queues_a_turn_including_when_background_work_has_finished() {
    for finished in [false, true] {
        let provider: Arc<dyn crate::StopHookSnapshotProvider> = if finished {
            Arc::new(EmptyGoalStopSnapshot)
        } else {
            Arc::new(GoalStopSnapshot)
        };
        let mut base = orch().with_stop_hook_snapshot(provider);
        base.config.interactive_session = true;
        let orch = Arc::new(base);
        orch.enable_goal_retries();
        let queue = Arc::new(IdleQueue::default());
        orch.set_mid_turn_input(queue.clone());
        set_goal(&orch, "ship it").await;
        orch.lifecycle_runtime
            .goal_checkin
            .lock()
            .unwrap()
            .deferred_since = Some(0);
        orch.sync_goal_checkin_idle_task().await;
        tokio::time::timeout(std::time::Duration::from_secs(61), async {
            while queue.0.lock().unwrap().is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("idle check-in must wake the host queue");
        let queued = queue.0.lock().unwrap().clone();
        assert_eq!(queued.len(), 1);
        assert!(queued[0].string_at("").unwrap().contains(if finished {
            "no longer running"
        } else {
            "background work is still running"
        }));
        assert!(
            orch.session.lock().await.history.is_empty(),
            "the host must admit and persist the queued turn exactly once"
        );
    }
}

#[tokio::test]
async fn turn_end_checkin_drives_another_round_without_duplicate_feedback() {
    let orch = orch().with_stop_hook_snapshot(Arc::new(GoalStopSnapshot));
    set_goal(&orch, "ship it").await;
    orch.lifecycle_runtime
        .goal_checkin
        .lock()
        .unwrap()
        .deferred_since = Some(0);
    let mut active = false;
    let mut count = 0;
    let flow = orch
        .handle_stop_at_end(
            "end_turn",
            &mut active,
            &mut count,
            1,
            lingxi_core::types::MessageId::new(),
            false,
        )
        .await;
    assert!(matches!(flow, crate::conversation::StopHookFlow::LoopAgain));
    let history = orch.session.lock().await.history.clone();
    assert_eq!(history.len(), 1);
    assert!(history[0].text_content().starts_with("Goal check-in:"));
}

struct HeldGoalEvaluator {
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

fn stop_evaluator_condition(request: &hooks::PromptHookRequest) -> &str {
    let prompt = request.prompt.string_at("").expect("Stop evaluator text");
    let condition_and_arguments = prompt
        .strip_prefix(
            "Based on the conversation transcript above, has the following stopping condition been satisfied? Answer based on transcript evidence only.\n\nCondition: ",
        )
        .expect("the real Stop pipeline wraps the condition for transcript evaluation");
    let (condition, arguments) = condition_and_arguments
        .split_once("\n\nARGUMENTS: ")
        .expect("the real Stop pipeline appends its event payload");
    let payload = lingxi_core::types::utf16_json::Utf16JsonProjection::parse(arguments)
        .expect("the Stop event payload is valid JSON");
    assert_eq!(payload.string_at("/hook_event_name"), Some("Stop"));
    condition
}

#[async_trait::async_trait]
impl hooks::HookPromptRunner for HeldGoalEvaluator {
    async fn run(
        &self,
        request: hooks::PromptHookRequest,
    ) -> Result<lingxi_core::types::utf16_json::Utf16JsonProjection, hooks::PromptHookError> {
        if stop_evaluator_condition(&request) == "old condition" {
            self.started.notify_one();
            self.release.notified().await;
            Ok(r#"{"ok":false,"reason":"retired goal feedback"}"#.into())
        } else {
            assert_eq!(
                stop_evaluator_condition(&request),
                "independent ordinary condition",
            );
            Ok(r#"{"ok":false,"reason":"ordinary stop feedback"}"#.into())
        }
    }
}

#[tokio::test]
async fn retired_goal_results_cannot_drive_stop_but_independent_blocks_survive() {
    use crate::conversation::{StopHookFlow, GOAL_STOP_HOOK_NAME};
    use lingxi_core::types::{HookId, MessageId};

    for replacement in [None, Some("replacement condition"), Some("old condition")] {
        for ordinary_block in [false, true] {
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
            let orch = ConversationOrchestrator::into_shared(ConversationOrchestrator::new(
                OrchestratorConfig::default(),
                Arc::new(MockApiClient::new(vec![])),
                Arc::new(ToolRegistry::new()),
                hooks,
                Arc::new(NoOpPermissionGate),
                Arc::new(MockOutputStream::new()),
                Arc::new(StaticMemoryProvider::empty()),
                std::env::temp_dir(),
            ));
            orch.set_active_goal(&"old condition".into()).await;
            if ordinary_block {
                let session_id = orch.session.lock().await.session_id;
                let mut ordinary = orch
                    .hooks
                    .get_session_named_hook(session_id, GOAL_STOP_HOOK_NAME)
                    .await
                    .unwrap();
                ordinary.id = HookId::new();
                ordinary.name = "ordinary-stop".into();
                ordinary.executor = hooks::HookExecutor::Prompt {
                    prompt: "independent ordinary condition".into(),
                    model: None,
                    continue_on_block: true,
                };
                orch.hooks
                    .upsert_session_named_hook(session_id, ordinary.name.clone(), ordinary)
                    .await;
            }
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
            if let Some(condition) = replacement {
                orch.set_active_goal(&condition.into()).await;
            } else {
                orch.clear_active_goal().await;
            }
            evaluator.release.notify_one();
            let disposition = tokio::time::timeout(std::time::Duration::from_secs(2), stop)
                .await
                .unwrap()
                .unwrap();
            if ordinary_block {
                assert!(matches!(disposition, StopHookFlow::LoopAgain));
                assert!(orch
                    .session
                    .lock()
                    .await
                    .history
                    .iter()
                    .any(|message| message.text_content().contains("ordinary stop feedback")));
            } else {
                assert!(matches!(disposition, StopHookFlow::FallThrough));
            }
            assert!(!orch
                .session
                .lock()
                .await
                .history
                .iter()
                .any(|message| message.text_content().contains("retired goal feedback")));
            let active = orch.get_active_goal().await;
            assert_eq!(
                active
                    .as_ref()
                    .and_then(|goal| goal.condition.string_at("")),
                replacement,
            );
            assert!(active.is_none_or(|goal| goal.iterations == 0));
        }
    }
}

#[tokio::test]
async fn clear_and_replacement_retire_deferral_timing_and_idle_timer() {
    for clear_first in [false, true] {
        let orch = orch().with_stop_hook_snapshot(Arc::new(GoalStopSnapshot));
        orch.set_active_goal(&"old condition".into()).await;
        assert!(orch.goal_checkin_pass(&[task("b1", "shell", None)]).await);
        {
            let mut state = orch.lifecycle_runtime.goal_checkin.lock().unwrap();
            state.deferred_since = Some(1);
            state.checkin_count = 7;
            state.idle_checkin_count = 3;
            state.last_deferral_pass_at = Some(2);
            state.last_deferring_ids = vec!["old-shell".into()];
        }
        assert!(orch
            .lifecycle_runtime
            .goal_checkin_idle_running
            .load(Ordering::Acquire));
        if clear_first {
            orch.clear_active_goal().await;
        }
        orch.set_active_goal(&"replacement condition".into()).await;
        assert_eq!(
            *orch.lifecycle_runtime.goal_checkin.lock().unwrap(),
            crate::prompt::goal_checkin::GoalDeferralState::default(),
        );
        assert!(!orch
            .lifecycle_runtime
            .goal_checkin_idle_running
            .load(Ordering::Acquire));
        assert!(orch
            .lifecycle_runtime
            .goal_checkin_idle_task
            .lock()
            .unwrap()
            .is_none());
        assert!(orch.goal_checkin_pass(&[task("b1", "shell", None)]).await);
        assert_eq!(
            orch.lifecycle_runtime
                .goal_checkin
                .lock()
                .unwrap()
                .checkin_count,
            0,
        );
    }
}

struct LiveGoalHookPolicy {
    key: &'static str,
    restricted: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl hooks::mods::ModSettingsReader for LiveGoalHookPolicy {
    async fn read(
        &self,
        _: lingxi_core::types::utf16_json::Utf16JsonProjection,
    ) -> Result<lingxi_core::types::utf16_json::Utf16JsonProjection, hooks::mods::ModError> {
        Ok(
            lingxi_core::types::utf16_json::Utf16JsonProjection::parse(&format!(
                "{{\"{}\":{}}}",
                self.key,
                self.restricted.load(Ordering::Acquire),
            ))
            .expect("fixed hook-policy key and boolean produce valid JSON"),
        )
    }
}

#[derive(Default)]
struct CountingGoalEvaluator(std::sync::Mutex<Vec<String>>);

#[async_trait::async_trait]
impl hooks::HookPromptRunner for CountingGoalEvaluator {
    async fn run(
        &self,
        request: hooks::PromptHookRequest,
    ) -> Result<lingxi_core::types::utf16_json::Utf16JsonProjection, hooks::PromptHookError> {
        let condition = stop_evaluator_condition(&request).to_owned();
        self.0.lock().unwrap().push(condition);
        Ok(r#"{"ok":false,"reason":"condition is not yet met"}"#.into())
    }
}

#[tokio::test]
async fn live_hook_restrictions_skip_only_goal_evaluation_and_reenable_without_reset() {
    use crate::conversation::{StopHookFlow, GOAL_STOP_HOOK_NAME};
    use lingxi_core::types::{HookId, MessageId};

    for key in ["disableAllHooks", "allowManagedHooksOnly"] {
        for ordinary_hook in [false, true] {
            let policy = Arc::new(LiveGoalHookPolicy {
                key,
                restricted: std::sync::atomic::AtomicBool::new(false),
            });
            let evaluator = Arc::new(CountingGoalEvaluator::default());
            let hooks = Arc::new(
                Arc::try_unwrap(noop_hook_executor())
                    .ok()
                    .unwrap()
                    .with_prompt_runner(evaluator.clone()),
            );
            let orch = ConversationOrchestrator::new(
                OrchestratorConfig::default(),
                Arc::new(MockApiClient::new(vec![])),
                Arc::new(ToolRegistry::new()),
                hooks,
                Arc::new(NoOpPermissionGate),
                Arc::new(MockOutputStream::new()),
                Arc::new(StaticMemoryProvider::empty()),
                std::env::temp_dir(),
            )
            .with_mod_settings_reader(policy.clone());
            orch.set_active_goal(&"existing condition".into()).await;
            let session_id = orch.session.lock().await.session_id;
            let goal_hook = orch
                .hooks
                .get_session_named_hook(session_id, GOAL_STOP_HOOK_NAME)
                .await
                .unwrap();
            if ordinary_hook {
                let mut ordinary = goal_hook.clone();
                ordinary.id = HookId::new();
                ordinary.name = "ordinary-stop".into();
                ordinary.executor = hooks::HookExecutor::Prompt {
                    prompt: "ordinary condition".into(),
                    model: None,
                    continue_on_block: true,
                };
                orch.hooks
                    .upsert_session_named_hook(session_id, ordinary.name.clone(), ordinary)
                    .await;
            }
            policy.restricted.store(true, Ordering::Release);
            let mut active = false;
            let mut blocks = 0;
            let flow = orch
                .handle_stop_at_end(
                    "end_turn",
                    &mut active,
                    &mut blocks,
                    1,
                    MessageId::new(),
                    false,
                )
                .await;
            if ordinary_hook {
                assert!(matches!(flow, StopHookFlow::LoopAgain));
            } else {
                assert!(matches!(flow, StopHookFlow::FallThrough));
            }
            {
                let requests = evaluator.0.lock().unwrap();
                assert_eq!(requests.len(), usize::from(ordinary_hook));
                assert!(requests
                    .iter()
                    .all(|prompt| prompt.starts_with("ordinary condition")));
            }
            let goal = orch.get_active_goal().await.unwrap();
            assert_eq!(goal.condition.string_at(""), Some("existing condition"));
            assert_eq!(goal.iterations, 0);
            assert!(goal.last_reason.is_none());
            assert_eq!(
                orch.hooks
                    .get_session_named_hook(session_id, GOAL_STOP_HOOK_NAME)
                    .await
                    .unwrap()
                    .id,
                goal_hook.id,
            );
            policy.restricted.store(false, Ordering::Release);
            let flow = orch
                .handle_stop_at_end(
                    "end_turn",
                    &mut active,
                    &mut blocks,
                    2,
                    MessageId::new(),
                    false,
                )
                .await;
            assert!(matches!(flow, StopHookFlow::LoopAgain));
            assert_eq!(orch.get_active_goal().await.unwrap().iterations, 1);
            assert_eq!(
                evaluator
                    .0
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|prompt| prompt.starts_with("existing condition"))
                    .count(),
                1,
            );
        }
    }
}
