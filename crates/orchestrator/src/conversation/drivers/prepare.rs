use super::*;

/// Everything one model step needs, collected exactly once.
///
/// Every field here is expensive in the same specific way: computing it ADVANCES
/// session state — prefetches fire, compaction runs, delta trackers move,
/// consume-once sources drain. So a step prepares once and REUSES this value for
/// each rebuild of the same request (PTL retry, the non-streaming fallback, a
/// re-snapshot after compaction); `reattach_outgoing_context` is what puts the
/// transient pieces back on a snapshot rebuilt from raw history.
///
/// The durable task notifications are deliberately NOT a field. They were
/// appended to `session.history` and the JSONL during preparation, so a rebuild
/// picks them up from history on its own; carrying them here too would re-append
/// them on top of that copy and send each completion twice.
pub(crate) struct PreparedTurnStep {
    pub(crate) snapshot: Vec<ConversationMessage>,
    pub(crate) model: String,
    pub(crate) model_profile: Option<String>,
    pub(crate) outgoing_history_rewriter: Option<Arc<dyn OutgoingHistoryRewriter>>,
    pub(crate) turn_reminders: Vec<ConversationMessage>,
    pub(crate) wire_tools: Vec<serde_json::Value>,
    pub(crate) deferred_reminder: Option<ConversationMessage>,
    pub(crate) date_change_reminder: Option<ConversationMessage>,
}

impl ConversationOrchestrator {
    /// The per-step preparation both turn drivers share.
    ///
    /// The three parameters are the whole of the difference between them, and
    /// each is load-bearing:
    ///
    /// * `path` — `Batched` or `Streaming`, threaded into
    ///   `prepare_model_call_snapshot`.
    /// * `in_human_turn` — batched turns pass `true` unconditionally for
    ///   task-notification provenance; streaming passes its real origin, so a
    ///   rewake or queued batch is not rendered as a human turn.
    /// * `user_cancel` — streaming passes its token INTO preparation so a cancel
    ///   lands inside the snapshot step. Batched passes `None` and is covered
    ///   instead by the outer `select!` in `try_run_turn_cancelable`, which races
    ///   this whole function. Whoever moves this must keep preparation inside
    ///   that race — `tests/turn_preparation_boundary_test.rs` parks a reminder
    ///   source mid-preparation and cancels to prove it.
    pub(crate) async fn prepare_turn_step(
        &self,
        path: ModelCallPath,
        system: Option<&str>,
        in_human_turn: bool,
        user_cancel: Option<&CancellationToken>,
    ) -> Result<PreparedTurnStep, OrchestratorError> {
        // Arm both prefetches CONCURRENTLY with this turn (claude-code `wAo` /
        // `startSkillDiscoveryPrefetch`), so their handles are ready when
        // `relevant_memory_reminder_messages` and
        // `skill_discovery_reminder_message` consume them below — both of which
        // run before the blocking-limit estimate, so their tokens are counted.
        self.start_memory_prefetch().await;
        self.start_skill_discovery_prefetch().await;
        self.maybe_extract_session_memory().await;

        self.seed_compact_cache_safe_params(system).await;
        self.maybe_compact_before_call().await;
        // 2.1.232: accepted peer inbox → user-role `<cross-session-message>`
        // before the outgoing snapshot is cloned from history.
        let _ = self.drain_peer_inbox(false).await;

        let prepared_call = self
            .prepare_model_call_snapshot(path, system, user_cancel)
            .await?;
        let mut snapshot = prepared_call.history_snapshot;

        // R-P1c/R-P1d: PREPEND the leading `additionalContext` meta message
        // (`# claudeMd` / `# userEmail` / `# currentDate`) to THIS call's
        // OUTGOING snapshot only. 1:1 with `A6n(re, userContext)`, recomputed
        // each turn so it never accumulates.
        self.prepend_leading_context(&mut snapshot).await;

        let reminders = self.collect_turn_reminders(in_human_turn).await;
        let turn_reminders = reminders.transient;
        // Durable completions first: they now live in history, so they belong
        // after the last real entry and before the transient reminders. The
        // snapshot was taken before they were appended.
        snapshot.extend(reminders.task_notifications);
        snapshot.extend(turn_reminders.iter().cloned());

        let wire_tools = self.build_wire_tools().await;
        // Carved-slate records the first eligible static prompt before the
        // request. A resumed session with no valid attachment stays live and
        // does not create a replacement snapshot.
        self.record_prompt_snapshot_if_needed(system, &wire_tools)
            .await;

        // Rebuilt on every model step: a ToolSearch result marks schemas as
        // discovered, so the immediately following request must include them
        // with `defer_loading:true`. Computing it ADVANCES the announced-set
        // tracking, so it is computed ONCE here and reattached on re-snapshot.
        let deferred_reminder = self.deferred_tools_reminder_message();
        if let Some(reminder) = deferred_reminder.clone() {
            self.prepend_transient_leading_context(&mut snapshot, reminder);
        }
        // `date_change`: prepended AFTER the deferred insert so the final order
        // is [date_change, deferred_tools_delta, …], matching the oracle batch
        // order (`Ky("date_change")` before `Ky("deferred_tools_delta")`). The
        // dedupe is committed downstream, once the request is actually sent —
        // not here, where it has only been computed.
        let date_change_reminder =
            self.date_change_reminder_message(self.session.lock().await.session_id);
        if let Some(reminder) = date_change_reminder.clone() {
            self.prepend_transient_leading_context(&mut snapshot, reminder);
        }

        Ok(PreparedTurnStep {
            snapshot,
            model: prepared_call.model,
            model_profile: prepared_call.model_profile,
            outgoing_history_rewriter: prepared_call.outgoing_history_rewriter,
            turn_reminders,
            wire_tools,
            deferred_reminder,
            date_change_reminder,
        })
    }
}

/// Reminders collected once for a single model step.
///
/// Transient reminders are re-appended when the same step rebuilds its request.
/// Task notifications are durable conversation events: they are persisted here
/// and returned separately so the caller can add them to a snapshot that was
/// captured before persistence without duplicating them on retry.
pub(crate) struct TurnReminders {
    pub(crate) transient: Vec<ConversationMessage>,
    pub(crate) task_notifications: Vec<ConversationMessage>,
}

impl ConversationOrchestrator {
    /// Collect reminder producers in their model-facing order exactly once.
    pub(crate) async fn collect_turn_reminders(&self, in_human_turn: bool) -> TurnReminders {
        let mut transient = Vec::new();

        if let Some(reminder) = self.brief_mode_reminder_message() {
            transient.push(reminder);
        }
        if let Some(reminder) = self.output_style_reminder_message().await {
            transient.push(reminder);
        }

        transient.extend(self.plan_mode_turn_messages().await);
        if let Some(reminder) = self.plan_mode_exit_message().await {
            transient.push(reminder);
        }
        if let Some(reminder) = self.skill_listing_reminder_message().await {
            transient.push(reminder);
        }
        if let Some(reminder) = self.conditional_rules_reminder_message().await {
            transient.push(reminder);
        }
        // Nested memory runs directly AFTER conditional rules, and the order is
        // load-bearing, not cosmetic: both consult the same
        // `sent_conditional_rules` set, and a `paths:`-gated rule already
        // claimed by the conditional-rules producer is skipped here. Swapping
        // the two changes WHICH mechanism reports such a rule, and therefore
        // the bytes the model sees.
        if let Some(reminder) = self.nested_memory_reminder_message().await {
            transient.push(reminder);
        }
        if let Some(reminder) = self.new_diagnostics_reminder_message().await {
            transient.push(reminder);
        }
        if let Some(reminder) = self.agent_listing_reminder_message().await {
            transient.push(reminder);
        }
        // DIVERGENCE (position, deliberate): the oracle's attachment fan-out
        // (@296520120) emits `changed_files` immediately after
        // `agent_listing_delta` and immediately BEFORE `nested_memory`. This
        // port injects `nested_memory` above — it has to, to read the
        // `sent_conditional_rules` claims noted there — so `changed_files`
        // sits directly after `agent_listing_delta` instead, which preserves
        // its order relative to everything downstream. `crate::prompt::changed_files`
        // carries the same note from the renderer's side.
        transient.extend(self.changed_files_reminder_messages().await);

        // `todo_reminder_message` already returns a system-reminder envelope.
        let todo_reminder = self.todo_reminder_message().await;
        let todo_reminder_fired = todo_reminder.is_some();
        if let Some(reminder) = todo_reminder {
            transient.push(reminder);
        }
        if let Some(reminder) = self
            .tool_search_usage_reminder_message(todo_reminder_fired)
            .await
        {
            transient.push(reminder);
        }
        if let Some(reminder) = self.async_hook_response_reminder_message().await {
            transient.push(reminder);
        }

        let task_notifications = self
            .task_notification_reminder_messages_in_turn(in_human_turn)
            .await;
        for notification in &task_notifications {
            {
                let mut session = self.session.lock().await;
                session.history.push(notification.clone());
            }
            self.persist_message_to_jsonl(notification).await;
        }

        // Task completion can enqueue memory updates, so this must follow the
        // notification drain and persistence above.
        transient.extend(self.memory_update_reminder_messages().await);
        transient.extend(self.relevant_memory_reminder_messages().await);
        if let Some(reminder) = self.skill_discovery_reminder_message().await {
            transient.push(reminder);
        }
        if let Some(reminder) = self.silent_turn_reminder_message().await {
            transient.push(reminder);
        }
        if let Some(reminder) = self.total_tokens_reminder_message().await {
            transient.push(reminder);
        }

        TurnReminders {
            transient,
            task_notifications,
        }
    }
}
