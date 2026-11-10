//! Claude Code 2.1.270 WQn/X7n interruption streak and delayed main-turn delivery.
use super::{Arc, CancellationToken, ConversationOrchestrator, Duration};
use crate::prompt::goal_interruption::{self, GoalInterruption};
use std::time::SystemTime;

type GoalIdentity = (
    SystemTime,
    lingxi_core::types::utf16_json::Utf16JsonProjection,
);

#[derive(Default)]
pub(crate) struct GoalRetryState {
    goal: Option<GoalIdentity>,
    retries: u32,
    announced: Option<&'static str>,
    pending: Option<CancellationToken>,
    queued_id: Option<String>,
    checkin: Option<(String, CancellationToken, GoalIdentity)>,
}
impl Drop for GoalRetryState {
    fn drop(&mut self) {
        self.reset();
    }
}
impl GoalRetryState {
    fn retry_is_valid(&self, id: &str, current: Option<&GoalIdentity>) -> bool {
        if let Some((_, token, goal)) = self.checkin.as_ref().filter(|(queued, _, _)| queued == id)
        {
            return !token.is_cancelled() && current == Some(goal);
        }
        current.is_some()
            && current == self.goal.as_ref()
            && self.queued_id.as_deref() == Some(id)
            && self
                .pending
                .as_ref()
                .is_some_and(|token| !token.is_cancelled())
    }

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
        self.handle_goal_interruption_for_owner(interruption, None)
            .await;
    }

    pub(super) async fn handle_goal_hook_interruption(
        &self,
        interruption: GoalInterruption,
        session_id: lingxi_core::types::SessionId,
        hook_id: lingxi_core::types::HookId,
        goal: &lingxi_core::session::ActiveGoalState,
    ) -> bool {
        self.handle_goal_interruption_for_owner(interruption, Some((session_id, hook_id, goal)))
            .await
    }

    async fn handle_goal_interruption_for_owner(
        &self,
        interruption: GoalInterruption,
        expected: Option<(
            lingxi_core::types::SessionId,
            lingxi_core::types::HookId,
            &lingxi_core::session::ActiveGoalState,
        )>,
    ) -> bool {
        let notice = {
            // Set/Clear publish state, metadata and the runtime hook role in
            // this order under one transition owner. An old evaluator cannot
            // act on the intermediate replacement state.
            let _transition = self.lifecycle_runtime.goal_transition.lock().await;
            let session = self.session.lock().await;
            if let Some((session_id, hook_id, goal)) = expected {
                if session.session_id != session_id
                    || !session.active_goal.as_ref().is_some_and(|current| {
                        current.set_at == goal.set_at && current.condition == goal.condition
                    })
                    || self
                        .hooks
                        .get_session_named_hook(session_id, super::GOAL_STOP_HOOK_NAME)
                        .await
                        .is_none_or(|hook| hook.id != hook_id)
                {
                    return false;
                }
            }
            if !self.prompt_is_interactive()
                || !telemetry::flag_bool("tengu_pewter_avocet", true)
                || crate::prompt::goal_checkin::checkin_interval_ms() == 0
            {
                self.reset_goal_interruption();
                return true;
            }
            let Some(goal) = session.active_goal.clone() else {
                self.reset_goal_interruption();
                return true;
            };
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
                        return true;
                    }
                    state.announced = Some(cause.key());
                    (cause.text().into(), false)
                }
                GoalInterruption::Retry(cause) => {
                    if state.pending.is_some() {
                        return true;
                    }
                    if state.retries >= goal_interruption::MAX_RETRIES {
                        if state.announced == Some("gave_up") {
                            return true;
                        }
                        state.announced = Some("gave_up");
                        (goal_interruption::gave_up_announcement(cause).into(), true)
                    } else {
                        if self
                            .mid_turn_input
                            .get()
                            .is_none_or(|source| !source.supports_goal_retries())
                        {
                            return true;
                        }
                        let Some(owner) = self.lifecycle_runtime.goal_retry_owner.get().cloned()
                        else {
                            return true;
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
                            )
                            .into(),
                            false,
                        )
                    }
                }
            }
        };
        self.output.emit_system_notice(&notice.0, notice.1).await;
        true
    }

    /// Queue an idle deferral check-in without consuming the interruption retry budget.
    pub(crate) async fn queue_goal_checkin(
        &self,
        body: lingxi_core::types::utf16_json::Utf16JsonProjection,
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

    fn goal_retry_enabled(&self, id: &str) -> bool {
        self.prompt_is_interactive()
            && (id.starts_with("goal-retry-checkin-")
                || telemetry::flag_bool("tengu_pewter_avocet", true))
            && crate::prompt::goal_checkin::checkin_interval_ms() != 0
    }

    /// Inspect a retry without consuming it before profile persistence.
    pub(crate) async fn goal_retry_is_valid(&self, id: &str) -> bool {
        if !self.goal_retry_enabled(id) {
            return false;
        }
        let session = self.session.lock().await;
        let current = session
            .active_goal
            .as_ref()
            .map(|goal| (goal.set_at, goal.condition.clone()));
        let state = self
            .lifecycle_runtime
            .goal_retry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.retry_is_valid(id, current.as_ref())
    }

    /// A discarded stale check-in must not block future check-ins. Recheck
    /// under the state lock and report an entry that has become valid again.
    pub(crate) async fn recheck_goal_retry_or_discard_stale(&self, id: &str) -> bool {
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
        let enabled = self.goal_retry_enabled(id);
        if enabled && state.retry_is_valid(id, current.as_ref()) {
            return true;
        }
        if state
            .checkin
            .as_ref()
            .is_some_and(|(queued, _, _)| queued == id)
        {
            let (_, token, _) = state.checkin.take().expect("checked checkin");
            token.cancel();
        } else if !enabled && state.queued_id.as_deref() == Some(id) {
            state.queued_id = None;
            state.cancel_pending();
        }
        false
    }

    /// Drop invalidated queued retries at the actual turn gate, including races
    /// where the host already dequeued one before a human message or goal clear.
    pub(crate) async fn admit_goal_retry(&self, id: &str) -> bool {
        if !self.goal_retry_enabled(id) {
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
            let valid = state.retry_is_valid(id, current.as_ref());
            let (_, token, _) = state.checkin.take().expect("checked checkin");
            token.cancel();
            return valid;
        }
        if !state.retry_is_valid(id, current.as_ref()) {
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

    fn source_text(text: &str) -> lingxi_core::types::utf16_json::Utf16JsonProjection {
        lingxi_core::types::utf16_json::Utf16JsonProjection::root_string(
            text.to_owned(),
            text.encode_utf16().collect(),
        )
        .unwrap()
    }

    #[derive(Default)]
    struct Queue(
        std::sync::Mutex<
            Vec<(
                String,
                lingxi_core::types::utf16_json::Utf16JsonProjection,
                CancellationToken,
            )>,
        >,
    );
    #[async_trait::async_trait]
    impl crate::prompt::mid_turn_input::MidTurnInputSource for Queue {
        fn supports_goal_retries(&self) -> bool {
            true
        }
        async fn enqueue_goal_retry(
            &self,
            id: String,
            body: lingxi_core::types::utf16_json::Utf16JsonProjection,
            cancel: CancellationToken,
        ) {
            self.0.lock().unwrap().push((id, body, cancel));
        }
        async fn take_mid_turn_input(
            &self,
        ) -> Option<lingxi_core::types::utf16_json::Utf16JsonProjection> {
            None
        }
    }

    fn fixture_config(
        interactive: bool,
        wire: bool,
        writer: Option<(
            lingxi_core::types::SessionId,
            Arc<session::jsonl::JsonlWriter>,
        )>,
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
        let orch = ConversationOrchestrator::new_with_streaming(
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
        );
        let orch = if let Some((id, writer)) = writer {
            orch.with_session_id(id).with_jsonl_writer(writer)
        } else {
            orch
        };
        let orch = ConversationOrchestrator::into_shared(orch);
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
        fixture_config(true, true, None)
    }

    #[tokio::test]
    async fn retries_require_interactive_host_delivery() {
        use lingxi_core::host::OrchestratorHandle;
        for (interactive, wire) in [(false, true), (true, false)] {
            let (orch, _, queue) = fixture_config(interactive, wire, None);
            orch.set_active_goal(&lingxi_core::types::utf16_json::Utf16JsonProjection::from(
                "finish",
            ))
            .await;
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
        use lingxi_core::host::OrchestratorHandle;
        orch.set_active_goal(&lingxi_core::types::utf16_json::Utf16JsonProjection::from(
            "finish",
        ))
        .await;
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
        assert!(calls[0].messages.iter().any(|m| {
            m.is_meta()
                && m.text_content()
                    .contains("The last turn ended before the goal could be evaluated")
        }));
        orch.set_active_goal(&lingxi_core::types::utf16_json::Utf16JsonProjection::from(
            "replacement",
        ))
        .await;
        orch.run_queued_prompt_batch(
            vec![crate::QueuedPromptInput {
                goal_retry_id: Some(id),
                text: body.clone(),
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
        use lingxi_core::host::OrchestratorHandle;
        orch.set_active_goal(&lingxi_core::types::utf16_json::Utf16JsonProjection::from(
            "finish",
        ))
        .await;
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
        use lingxi_core::host::OrchestratorHandle;
        orch.set_active_goal(&lingxi_core::types::utf16_json::Utf16JsonProjection::from(
            "finish",
        ))
        .await;
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
        orch.set_active_goal(&lingxi_core::types::utf16_json::Utf16JsonProjection::from(
            "replacement",
        ))
        .await;
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
        use lingxi_core::host::OrchestratorHandle;
        orch.set_active_goal(&lingxi_core::types::utf16_json::Utf16JsonProjection::from(
            "finish",
        ))
        .await;
        let goal = orch.session.lock().await.active_goal.clone().unwrap();
        assert!(orch.queue_goal_checkin("same body".into(), &goal).await);
        let (old_id, body, token) = queue.0.lock().unwrap().pop().unwrap();
        orch.run_queued_prompt_batch(
            vec![
                crate::QueuedPromptInput {
                    text: source_text("human instruction"),
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
        let body_display = body.string_at("").unwrap().to_owned();
        assert!(!calls[0]
            .messages
            .iter()
            .any(|m| m.text_content() == body_display));
        assert!(orch.queue_goal_checkin(body, &goal).await);
        let (new_id, _, _) = queue.0.lock().unwrap().pop().unwrap();
        assert!(!orch.admit_goal_retry(&old_id).await);
        assert!(orch.admit_goal_retry(&new_id).await);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn invalid_only_retry_preserves_pending_profile_and_skill_epoch() {
        use lingxi_core::host::session_context::ConversationMode;
        use lingxi_core::host::OrchestratorHandle;
        use lingxi_core::types::{ConversationMessage, MessageId};

        let _registry_guard = compaction::invoked_skills::TEST_LOCK.lock().unwrap();
        let (orch, api, queue) = fixture();
        orch.set_active_goal(&source_text("finish")).await;
        let goal = orch.session.lock().await.active_goal.clone().unwrap();
        assert!(orch.queue_goal_checkin("continue".into(), &goal).await);
        let (valid_id, body, token) = queue.0.lock().unwrap().pop().unwrap();

        let session_id = orch.current_session_id().await.to_string();
        let scope = compaction::invoked_skills::InvokedSkillScopeRef::new(Some(&session_id), None);
        let skill_id = MessageId::new();
        let skill = ConversationMessage::user_meta(skill_id, "old skill guidance".into())
            .with_origin(compaction::invoked_skills::skill_activation_origin(0));
        orch.session.lock().await.history.push(skill);
        compaction::invoked_skills::register_scoped_for_message(
            &source_text("keep"),
            std::path::Path::new("/skills/keep"),
            &source_text("old skill guidance"),
            scope,
            skill_id,
        );
        assert!(orch
            .ensure_conversation_mode(ConversationMode::Chat)
            .await
            .is_err());

        let outcome = orch
            .run_queued_prompt_batch(
                vec![crate::QueuedPromptInput {
                    goal_retry_id: Some("goal-retry-checkin-stale".into()),
                    text: "stale".into(),
                    is_meta: true,
                    ..Default::default()
                }],
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(matches!(outcome, crate::conversation::TurnOutcome::EndTurn));
        let state = orch.conversation_mode().await;
        assert_eq!(state.mode, ConversationMode::Code);
        assert_eq!(orch.profile_epoch().await, 0);
        assert_eq!(compaction::invoked_skills::context_epoch(scope), 0);
        assert_eq!(compaction::invoked_skills::filter_for_scope(scope).len(), 1);
        assert!(orch
            .session
            .lock()
            .await
            .model_context_history()
            .iter()
            .any(|row| row.id() == skill_id));
        assert!(api.captured_calls().await.is_empty());
        assert!(!token.is_cancelled());
        assert!(orch.goal_retry_is_valid(&valid_id).await);
        assert!(orch.recheck_goal_retry_or_discard_stale(&valid_id).await);
        assert!(!token.is_cancelled());

        orch.run_queued_prompt_batch(
            vec![crate::QueuedPromptInput {
                goal_retry_id: Some(valid_id),
                text: body,
                is_meta: true,
                ..Default::default()
            }],
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(api.captured_calls().await.len(), 1);
        assert!(token.is_cancelled());
        assert_eq!(
            (
                orch.conversation_mode().await.mode,
                orch.profile_epoch().await
            ),
            (ConversationMode::Code, 0)
        );
        assert_eq!(compaction::invoked_skills::filter_for_scope(scope).len(), 1);
        compaction::invoked_skills::clear_session(&session_id);
    }

    #[tokio::test]
    async fn goal_retry_invalidated_during_storage_wait_does_not_admit_profile() {
        use lingxi_core::host::session_context::ConversationMode;
        use lingxi_core::host::OrchestratorHandle;

        let (orch, api, queue) = fixture();
        orch.set_active_goal(&source_text("finish")).await;
        let goal = orch.session.lock().await.active_goal.clone().unwrap();
        assert!(orch.queue_goal_checkin("continue".into(), &goal).await);
        let (id, body, token) = queue.0.lock().unwrap().pop().unwrap();
        assert!(orch
            .ensure_conversation_mode(ConversationMode::Chat)
            .await
            .is_err());

        let storage = orch.transcript.storage_pauses.lock().await;
        let mut retry = orch.run_queued_prompt_batch(
            vec![crate::QueuedPromptInput {
                goal_retry_id: Some(id),
                text: body,
                is_meta: true,
                ..Default::default()
            }],
            CancellationToken::new(),
        );
        assert!(futures::poll!(retry.as_mut()).is_pending());
        assert!(orch.turn_gate.try_lock().is_err());
        token.cancel();
        drop(storage);
        assert!(matches!(
            retry.await.unwrap(),
            crate::conversation::TurnOutcome::EndTurn
        ));
        let state = orch.conversation_mode().await;
        assert_eq!(state.mode, ConversationMode::Code);
        assert_eq!(orch.profile_epoch().await, 0);
        assert!(api.captured_calls().await.is_empty());
    }

    #[tokio::test]
    async fn valid_goal_retry_survives_failed_profile_persistence() {
        use lingxi_core::host::session_context::ConversationMode;
        use lingxi_core::host::OrchestratorHandle;

        let root = tempfile::tempdir().unwrap();
        let id = lingxi_core::types::SessionId::new();
        let path = root.path().join(format!("{}.jsonl", id.as_uuid()));
        let fs: Arc<dyn lingxi_core::host::FileSystem> = Arc::new(
            platform_posix::fs::PosixFileSystem::new(root.path().to_path_buf()),
        );
        let writer = Arc::new(session::jsonl::JsonlWriter::new(path.clone(), fs));
        let (orch, api, queue) = fixture_config(true, true, Some((id, writer)));
        orch.initialize_conversation_mode().await.unwrap();
        orch.set_active_goal(&source_text("finish")).await;
        let goal = orch.session.lock().await.active_goal.clone().unwrap();
        assert!(orch.queue_goal_checkin("continue".into(), &goal).await);
        let (retry_id, body, token) = queue.0.lock().unwrap().pop().unwrap();
        assert!(orch
            .ensure_conversation_mode(ConversationMode::Chat)
            .await
            .is_err());

        let backup = path.with_extension("backup");
        std::fs::rename(&path, &backup).unwrap();
        std::fs::create_dir(&path).unwrap();
        let retry = || {
            vec![crate::QueuedPromptInput {
                goal_retry_id: Some(retry_id.clone()),
                text: body.clone(),
                is_meta: true,
                ..Default::default()
            }]
        };
        assert!(orch.initialize_conversation_mode().await.is_err());
        assert!(!token.is_cancelled());
        assert!(orch.goal_retry_is_valid(&retry_id).await);
        assert!(api.captured_calls().await.is_empty());

        std::fs::remove_dir(&path).unwrap();
        std::fs::rename(&backup, &path).unwrap();
        orch.initialize_conversation_mode().await.unwrap();
        orch.run_queued_prompt_batch(retry(), CancellationToken::new())
            .await
            .unwrap();
        assert!(token.is_cancelled());
        assert_eq!(api.captured_calls().await.len(), 1);
        assert_eq!(
            (
                orch.conversation_mode().await.mode,
                orch.profile_epoch().await
            ),
            (ConversationMode::Code, 0)
        );
    }

    #[tokio::test]
    async fn discarded_cancelled_checkin_does_not_block_a_later_checkin() {
        use lingxi_core::host::OrchestratorHandle;
        let (orch, api, queue) = fixture();
        orch.set_active_goal(&source_text("finish")).await;
        let goal = orch.session.lock().await.active_goal.clone().unwrap();
        assert!(orch.queue_goal_checkin("continue".into(), &goal).await);
        let (id, body, token) = queue.0.lock().unwrap().pop().unwrap();
        token.cancel();
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
        assert!(api.captured_calls().await.is_empty());
        assert!(orch.queue_goal_checkin("next".into(), &goal).await);
    }
}
