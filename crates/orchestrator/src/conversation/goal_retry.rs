//! Claude Code 2.1.270 WQn/X7n interruption streak and delayed main-turn delivery.
use super::{Arc, CancellationToken, ConversationOrchestrator, Duration};
use crate::prompt::goal_interruption::{self, GoalInterruption};
use std::time::SystemTime;

#[derive(Default)]
pub(crate) struct GoalRetryState {
    goal: Option<(SystemTime, String)>,
    retries: u32,
    announced: Option<&'static str>,
    pending: Option<CancellationToken>,
    queued_id: Option<String>,
    checkin: Option<(String, CancellationToken, (SystemTime, String))>,
}
impl Drop for GoalRetryState {
    fn drop(&mut self) {
        self.reset();
    }
}
impl GoalRetryState {
    fn cancel_pending(&mut self) {
        if let Some(token) = self.pending.take() {
            token.cancel();
        }
    }
    fn reset(&mut self) {
        self.cancel_pending();
        self.queued_id = None;
        if let Some((_, token, _)) = self.checkin.take() {
            token.cancel();
        }
        self.goal = None;
        self.retries = 0;
        self.announced = None;
    }
}

impl ConversationOrchestrator {
    /// Bind once after Arc construction; a sleeping timer never owns the session.
    pub fn enable_goal_retries(self: &Arc<Self>) {
        let _ = self
            .lifecycle_runtime
            .goal_retry_owner
            .set(Arc::downgrade(self));
    }

    pub(crate) fn reset_goal_interruption(&self) {
        self.lifecycle_runtime
            .goal_retry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .reset();
    }

    pub(crate) async fn handle_goal_interruption(&self, interruption: GoalInterruption) {
        if !self.prompt_is_interactive()
            || !telemetry::flag_bool("tengu_pewter_avocet", true)
            || crate::prompt::goal_checkin::checkin_interval_ms() == 0
        {
            self.reset_goal_interruption();
            return;
        }
        let Some(goal) = self.session.lock().await.active_goal.clone() else {
            self.reset_goal_interruption();
            return;
        };
        let notice = {
            let mut state = self
                .lifecycle_runtime
                .goal_retry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let key = (goal.set_at, goal.condition.clone());
            if state.goal.as_ref() != Some(&key) {
                state.reset();
                state.goal = Some(key);
            }
            match interruption {
                GoalInterruption::Pause(cause) => {
                    state.cancel_pending();
                    if state.announced == Some(cause.key()) {
                        return;
                    }
                    state.announced = Some(cause.key());
                    (cause.text().to_string(), false)
                }
                GoalInterruption::Retry(cause) => {
                    if state.pending.is_some() {
                        return;
                    }
                    if state.retries >= goal_interruption::MAX_RETRIES {
                        if state.announced == Some("gave_up") {
                            return;
                        }
                        state.announced = Some("gave_up");
                        (goal_interruption::gave_up_announcement(cause), true)
                    } else {
                        if self
                            .mid_turn_input
                            .get()
                            .is_none_or(|source| !source.supports_goal_retries())
                        {
                            return;
                        }
                        let Some(owner) = self.lifecycle_runtime.goal_retry_owner.get().cloned()
                        else {
                            return;
                        };
                        let attempt = state.retries;
                        // UUID entropy avoids introducing a new random-number dependency.
                        let entropy =
                            u64::try_from(uuid::Uuid::new_v4().as_u128() & ((1_u128 << 53) - 1))
                                .expect("53 bits fit in u64");
                        // Every integer in this masked range is exactly representable.
                        #[allow(clippy::cast_precision_loss)]
                        let jitter = entropy as f64 / 9_007_199_254_740_992.0;
                        let delay = goal_interruption::retry_delay_ms(attempt, jitter);
                        let cancel = CancellationToken::new();
                        state.pending = Some(cancel.clone());
                        state.retries += 1;
                        state.announced = None;
                        tokio::spawn(Self::deliver_goal_retry(
                            owner,
                            goal.clone(),
                            cause,
                            delay,
                            cancel,
                        ));
                        // WQn displays the base rung, not the jittered timer delay.
                        (
                            goal_interruption::retry_announcement(
                                cause,
                                goal_interruption::RETRY_BACKOFF_MS[attempt as usize],
                                attempt + 1,
                            ),
                            false,
                        )
                    }
                }
            }
        };
        self.output.emit_system_notice(&notice.0, notice.1).await;
    }

    /// Queue an idle deferral check-in without consuming the interruption retry budget.
    pub(crate) async fn queue_goal_checkin(
        &self,
        body: String,
        goal: &lingxi_core::session::ActiveGoalState,
    ) -> bool {
        if !self.prompt_is_interactive() || crate::prompt::goal_checkin::checkin_interval_ms() == 0
        {
            return false;
        }
        let Some(source) = self
            .mid_turn_input
            .get()
            .filter(|s| s.supports_goal_retries())
        else {
            return false;
        };
        if source.has_queued_goal_work().await {
            return false;
        }
        let current = self.session.lock().await.active_goal.clone();
        if !current.is_some_and(|g| g.set_at == goal.set_at && g.condition == goal.condition) {
            return false;
        }
        let id = format!("goal-retry-checkin-{}", uuid::Uuid::new_v4());
        let cancel = CancellationToken::new();
        {
            let mut state = self
                .lifecycle_runtime
                .goal_retry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.checkin.is_some() {
                return false;
            }
            state.checkin = Some((
                id.clone(),
                cancel.clone(),
                (goal.set_at, goal.condition.clone()),
            ));
        }
        source.enqueue_goal_retry(id, body, cancel).await;
        true
    }

    /// Drop invalidated queued retries at the actual turn gate, including races
    /// where the host already dequeued one before a human message or goal clear.
    pub(crate) async fn admit_goal_retry(&self, id: &str) -> bool {
        if !self.prompt_is_interactive()
            || (!id.starts_with("goal-retry-checkin-")
                && !telemetry::flag_bool("tengu_pewter_avocet", true))
            || crate::prompt::goal_checkin::checkin_interval_ms() == 0
        {
            self.reset_goal_interruption();
            return false;
        }
        let session = self.session.lock().await;
        let current = session
            .active_goal
            .as_ref()
            .map(|goal| (goal.set_at, goal.condition.clone()));
        let mut state = self
            .lifecycle_runtime
            .goal_retry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state
            .checkin
            .as_ref()
            .is_some_and(|(queued, _, _)| queued == id)
        {
            let (_, token, goal) = state.checkin.take().expect("checked checkin");
            let valid = !token.is_cancelled() && current.as_ref() == Some(&goal);
            token.cancel();
            return valid;
        }
        if current != state.goal
            || current.is_none()
            || state.queued_id.as_deref() != Some(id)
            || state
                .pending
                .as_ref()
                .is_none_or(CancellationToken::is_cancelled)
        {
            return false;
        }
        state.queued_id = None;
        state.cancel_pending();
        true
    }

    fn deliver_goal_retry(
        owner: std::sync::Weak<Self>,
        goal: lingxi_core::session::ActiveGoalState,
        cause: goal_interruption::RetryCause,
        delay: i64,
        cancel: CancellationToken,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        Box::pin(async move {
            let mut delay = delay;
            loop {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => return,
                    () = tokio::time::sleep(Duration::from_millis(u64::try_from(delay).unwrap_or_default())) => {}
                }
                let Some(orch) = owner.upgrade() else {
                    return;
                };
                let Ok(gate) = orch.turn_gate.try_lock() else {
                    delay = 60_000;
                    continue;
                };
                let current = orch.session.lock().await.active_goal.clone();
                if cancel.is_cancelled()
                    || !current
                        .is_some_and(|g| g.set_at == goal.set_at && g.condition == goal.condition)
                    || !telemetry::flag_bool("tengu_pewter_avocet", true)
                {
                    return;
                }
                let body = goal_interruption::retry_body(&goal.condition, cause);
                let Some(source) = orch.mid_turn_input.get() else {
                    return;
                };
                if crate::prompt::goal_checkin::checkin_interval_ms() == 0 {
                    return;
                }
                if source.has_queued_goal_work().await {
                    delay = 60_000;
                    continue;
                }
                let id = format!("goal-retry-{}", uuid::Uuid::new_v4());
                orch.lifecycle_runtime
                    .goal_retry
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .queued_id = Some(id.clone());
                // The host queue owns admission, cancellation, UI and error finalization.
                // Keep pending until admission so repeated failures cannot queue duplicates.
                source.enqueue_goal_retry(id, body, cancel).await;
                drop(gate);
                return;
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt::goal_interruption::RetryCause;
    use crate::test_support::*;

    #[derive(Default)]
    struct Queue(std::sync::Mutex<Vec<(String, String, CancellationToken)>>);
    #[async_trait::async_trait]
    impl crate::prompt::mid_turn_input::MidTurnInputSource for Queue {
        fn supports_goal_retries(&self) -> bool {
            true
        }
        async fn enqueue_goal_retry(&self, id: String, body: String, cancel: CancellationToken) {
            self.0.lock().unwrap().push((id, body, cancel));
        }
        async fn take_mid_turn_input(&self) -> Option<String> {
            None
        }
    }

    fn fixture_config(
        interactive: bool,
        wire: bool,
    ) -> (
        Arc<ConversationOrchestrator>,
        Arc<MockStreamingApiClient>,
        Arc<Queue>,
    ) {
        let api = Arc::new(MockStreamingApiClient::with_turns(vec![vec![
            message_start("retry", "claude-opus-4-7"),
            content_block_start_text(0),
            text_delta(0, "continued"),
            content_block_stop(0),
            message_delta_stop("end_turn"),
            message_stop(),
        ]]));
        let orch = Arc::new(ConversationOrchestrator::new_with_streaming(
            crate::OrchestratorConfig {
                interactive_session: interactive,
                ..Default::default()
            },
            Arc::new(MockApiClient::new(vec![])),
            api.clone(),
            Arc::new(tool_api::ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            std::env::temp_dir(),
        ));
        orch.enable_goal_retries();
        let queue = Arc::new(Queue::default());
        if wire {
            orch.set_mid_turn_input(queue.clone());
        }
        (orch, api, queue)
    }

    fn fixture() -> (
        Arc<ConversationOrchestrator>,
        Arc<MockStreamingApiClient>,
        Arc<Queue>,
    ) {
        fixture_config(true, true)
    }

    #[tokio::test]
    async fn retries_require_interactive_host_delivery() {
        use platform_api::OrchestratorHandle;
        for (interactive, wire) in [(false, true), (true, false)] {
            let (orch, _, queue) = fixture_config(interactive, wire);
            orch.set_active_goal("finish").await;
            orch.handle_goal_interruption(GoalInterruption::Retry(RetryCause::ApiUnavailable))
                .await;
            let state = orch.lifecycle_runtime.goal_retry.lock().unwrap();
            assert_eq!(state.retries, 0);
            assert!(state.pending.is_none());
            assert!(queue.0.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn retry_delivers_a_real_meta_turn_and_stale_goal_does_not() {
        let (orch, api, queue) = fixture();
        use platform_api::OrchestratorHandle;
        orch.set_active_goal("finish").await;
        let goal = orch.session.lock().await.active_goal.clone().unwrap();
        let cancel = CancellationToken::new();
        {
            let mut state = orch.lifecycle_runtime.goal_retry.lock().unwrap();
            state.pending = Some(cancel.clone());
            state.goal = Some((goal.set_at, goal.condition.clone()));
        }
        ConversationOrchestrator::deliver_goal_retry(
            Arc::downgrade(&orch),
            goal.clone(),
            RetryCause::ApiUnavailable,
            0,
            cancel,
        )
        .await;
        let (id, body, _) = queue.0.lock().unwrap().pop().unwrap();
        orch.run_queued_prompt_batch(
            vec![crate::QueuedPromptInput {
                goal_retry_id: Some(id.clone()),
                text: body.clone(),
                is_meta: true,
                ..Default::default()
            }],
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let calls = api.captured_calls().await;
        assert_eq!(calls.len(), 1);
        assert!(calls[0].messages.iter().any(|m| m.is_meta()
            && m.text_content()
                .contains("The last turn ended before the goal could be evaluated")));
        orch.set_active_goal("replacement").await;
        orch.run_queued_prompt_batch(
            vec![crate::QueuedPromptInput {
                goal_retry_id: Some(id),
                text: body,
                is_meta: true,
                ..Default::default()
            }],
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(api.captured_calls().await.len(), 1);
    }

    #[tokio::test]
    async fn retry_streak_dedupes_caps_and_resets() {
        let (orch, _, _) = fixture();
        use platform_api::OrchestratorHandle;
        orch.set_active_goal("finish").await;
        for expected in 1..=3 {
            orch.handle_goal_interruption(GoalInterruption::Retry(RetryCause::ApiUnavailable))
                .await;
            orch.handle_goal_interruption(GoalInterruption::Retry(RetryCause::ApiUnavailable))
                .await;
            let mut state = orch.lifecycle_runtime.goal_retry.lock().unwrap();
            assert_eq!(state.retries, expected);
            state.cancel_pending();
        }
        orch.handle_goal_interruption(GoalInterruption::Retry(RetryCause::ApiUnavailable))
            .await;
        assert_eq!(
            orch.lifecycle_runtime.goal_retry.lock().unwrap().announced,
            Some("gave_up")
        );
        orch.reset_goal_interruption();
        let state = orch.lifecycle_runtime.goal_retry.lock().unwrap();
        assert_eq!(state.retries, 0);
        assert!(state.pending.is_none());
    }
    #[tokio::test]
    async fn checkin_queues_separately_and_is_cancelled_by_new_goal() {
        let (orch, _, queue) = fixture();
        use platform_api::OrchestratorHandle;
        orch.set_active_goal("finish").await;
        let goal = orch.session.lock().await.active_goal.clone().unwrap();
        assert!(
            orch.queue_goal_checkin("check progress".into(), &goal)
                .await
        );
        assert!(
            !orch
                .queue_goal_checkin("check progress".into(), &goal)
                .await
        );
        assert_eq!(orch.lifecycle_runtime.goal_retry.lock().unwrap().retries, 0);
        let (id, _, token) = queue.0.lock().unwrap().pop().unwrap();
        orch.set_active_goal("replacement").await;
        assert!(token.is_cancelled());
        assert!(!orch.admit_goal_retry(&id).await);
    }

    #[test]
    fn dropping_retry_state_cancels_queued_work() {
        let token = CancellationToken::new();
        let mut state = GoalRetryState::default();
        state.pending = Some(token.clone());
        drop(state);
        assert!(token.is_cancelled());
    }
    #[tokio::test]
    async fn human_input_invalidates_dequeued_retry_before_admission() {
        let (orch, api, queue) = fixture();
        use platform_api::OrchestratorHandle;
        orch.set_active_goal("finish").await;
        let goal = orch.session.lock().await.active_goal.clone().unwrap();
        assert!(orch.queue_goal_checkin("same body".into(), &goal).await);
        let (old_id, body, token) = queue.0.lock().unwrap().pop().unwrap();
        orch.run_queued_prompt_batch(
            vec![
                crate::QueuedPromptInput {
                    text: "human instruction".into(),
                    ..Default::default()
                },
                crate::QueuedPromptInput {
                    goal_retry_id: Some(old_id.clone()),
                    text: body.clone(),
                    is_meta: true,
                    ..Default::default()
                },
            ],
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(token.is_cancelled());
        let calls = api.captured_calls().await;
        assert!(!calls[0].messages.iter().any(|m| m.text_content() == body));
        assert!(orch.queue_goal_checkin(body, &goal).await);
        let (new_id, _, _) = queue.0.lock().unwrap().pop().unwrap();
        assert!(!orch.admit_goal_retry(&old_id).await);
        assert!(orch.admit_goal_retry(&new_id).await);
    }
}
