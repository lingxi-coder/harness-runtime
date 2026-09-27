use crate::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use crate::ConversationOrchestrator;
use crate::OrchestratorConfig;
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
struct IdleQueue(std::sync::Mutex<Vec<String>>);
#[async_trait::async_trait]
impl crate::prompt::mid_turn_input::MidTurnInputSource for IdleQueue {
    fn supports_goal_retries(&self) -> bool {
        true
    }
    async fn enqueue_goal_retry(
        &self,
        _: String,
        body: String,
        _: tokio_util::sync::CancellationToken,
    ) {
        self.0.lock().unwrap().push(body);
    }
    async fn take_mid_turn_input(&self) -> Option<String> {
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
        assert!(queued[0].contains(if finished {
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
            protocol::MessageId::new(),
            false,
        )
        .await;
    assert!(matches!(flow, crate::conversation::StopHookFlow::LoopAgain));
    let history = orch.session.lock().await.history.clone();
    assert_eq!(history.len(), 1);
    assert!(history[0].text_content().starts_with("Goal check-in:"));
}
