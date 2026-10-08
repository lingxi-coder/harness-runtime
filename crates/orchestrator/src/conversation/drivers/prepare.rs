use super::*;

/// Everything one model step needs, collected exactly once.
///
/// Every field here is expensive in the same specific way: computing it ADVANCES
/// session state — prefetches fire, compaction runs, delta trackers move,
/// consume-once sources drain. So a step prepares once and REUSES this value for
/// each rebuild of the same request (PTL retry, the non-streaming fallback, a
/// re-snapshot after compaction); `reattach_outgoing_context` is what puts the
/// ordered reminder suffix back on a snapshot rebuilt from raw history.
///
/// The durable task notifications are deliberately NOT a field. They were
/// appended to `session.history` and the JSONL during preparation, so a rebuild
/// picks them up from history on its own. Newly created model reminders also
/// live in history, but remain in the ordered suffix so rebuilding can remove
/// their history copies and restore their positions among transient reminders.
#[derive(Clone)]
pub(crate) struct PreparedTurnStep {
    pub(crate) snapshot: Vec<ConversationMessage>,
    pub(crate) model: String,
    pub(crate) model_profile: Option<String>,
    pub(crate) outgoing_history_rewriter: Option<Arc<dyn OutgoingHistoryRewriter>>,
    /// This step's full reminder suffix in request order, including the newly
    /// persisted MCP/total-token messages with their original identities.
    pub(crate) turn_reminders: Vec<ConversationMessage>,
    /// Keep async-hook producer generations through PTL/fallback snapshot
    /// rebuilds so reset can remove an old completion at model admission.
    pub(crate) guarded_async_hook_reminders:
        Vec<(MessageId, Arc<dyn hooks::attachment::HookPublicationGuard>)>,
    pub(crate) context_announcements: PreparedContextAnnouncements,
    pub(crate) wire_tools: Vec<serde_json::Value>,
    pub(crate) skip_global_cache_for_system_prompt: bool,
    pub(crate) deferred_reminder: Option<ConversationMessage>,
    pub(crate) date_change_reminder: Option<ConversationMessage>,
}

impl ConversationOrchestrator {
    /// Model-chain advancement keeps consume-once context from the same step,
    /// while model-aware preparation receives the new serving route.
    pub(crate) async fn reprepare_model_fallback(
        &self,
        system: Option<&lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput>,
        cancel: Option<&CancellationToken>,
        previous: &PreparedTurnStep,
    ) -> Result<PreparedTurnStep, OrchestratorError> {
        let display_system = system.map(
            lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput::display_text,
        );
        let call = self
            .prepare_model_call_snapshot(
                ModelCallPath::Streaming,
                display_system.as_deref(),
                cancel,
            )
            .await?;
        let mut snapshot = call.history_snapshot;
        let mut turn_reminders = previous.turn_reminders.clone();
        let mut guarded_async_hook_reminders = previous.guarded_async_hook_reminders.clone();
        crate::prompt::async_hook_response::retain_current_async_hook_reminders(
            &mut snapshot,
            &mut turn_reminders,
            &mut guarded_async_hook_reminders,
        );
        self.reattach_outgoing_context(
            &mut snapshot,
            previous.deferred_reminder.as_ref(),
            previous.date_change_reminder.as_ref(),
            &mut turn_reminders,
            &mut guarded_async_hook_reminders,
            &previous.context_announcements,
            false,
        )
        .await;
        crate::prompt::async_hook_response::retain_current_async_hook_reminders(
            &mut snapshot,
            &mut turn_reminders,
            &mut guarded_async_hook_reminders,
        );
        Ok(PreparedTurnStep {
            snapshot,
            model: call.model,
            model_profile: call.model_profile,
            outgoing_history_rewriter: call.outgoing_history_rewriter,
            turn_reminders,
            guarded_async_hook_reminders,
            context_announcements: previous.context_announcements.clone(),
            wire_tools: previous.wire_tools.clone(),
            skip_global_cache_for_system_prompt: previous.skip_global_cache_for_system_prompt,
            deferred_reminder: previous.deferred_reminder.clone(),
            date_change_reminder: previous.date_change_reminder.clone(),
        })
    }

    /// Capture the step's prompt origin before preparation can compact history
    /// or append durable attachments. Meta messages and tool-injected prompts
    /// do not turn a continuation into a regular user submission.
    pub(crate) async fn regular_user_prompt_for_model_step(&self) -> bool {
        let session = self.session.lock().await;
        session
            .history
            .iter()
            .rev()
            .find_map(|message| match message {
                ConversationMessage::User {
                    id,
                    content,
                    is_meta,
                    is_compact_summary,
                    ..
                } => {
                    if *is_meta || session.injected_message_sources.contains_key(id) {
                        return None;
                    }
                    Some(
                        !*is_compact_summary
                            && !content
                                .iter()
                                .any(|block| matches!(block, ContentBlock::ToolResult { .. })),
                    )
                }
                ConversationMessage::Assistant { .. } => Some(false),
                ConversationMessage::System { subtype, .. }
                    if subtype.as_deref() == Some("compact_boundary") =>
                {
                    Some(false)
                }
                ConversationMessage::System { .. } => None,
            })
            .unwrap_or(false)
    }

    /// The per-step preparation both turn drivers share.
    ///
    /// The path, prompt origin and cancellation parameters preserve the
    /// differences between the drivers:
    ///
    /// * `path` — `Batched` or `Streaming`, threaded into
    ///   `prepare_model_call_snapshot`.
    /// * `in_human_turn` — batched turns pass `true` unconditionally for
    ///   task-notification provenance; streaming passes its real origin, so a
    ///   rewake or queued batch is not rendered as a human turn.
    /// * `is_regular_user_prompt` — captured at step ingress, before compaction
    ///   or notification persistence can change the history tail.
    /// * `user_cancel` — streaming passes its token INTO preparation so a cancel
    ///   lands inside the snapshot step. Batched passes `None` and is covered
    ///   instead by the outer `select!` in `try_run_turn_cancelable`, which races
    ///   this whole function. Whoever moves this must keep preparation inside
    ///   that race — `tests/turn_preparation_boundary_test.rs` parks a reminder
    ///   source mid-preparation and cancels to prove it.
    pub(crate) async fn prepare_turn_step(
        &self,
        path: ModelCallPath,
        system: Option<&lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput>,
        in_human_turn: bool,
        is_regular_user_prompt: bool,
        user_cancel: Option<&CancellationToken>,
    ) -> Result<PreparedTurnStep, OrchestratorError> {
        // Peer reports are model output, with no human prompt processing or
        // authority. Keep their queue claims retryable until durable history
        // admission has succeeded, before any request snapshot is captured.
        self.consume_main_reports().await?;
        // Arm both prefetches CONCURRENTLY with this turn (claude-code `wAo` /
        // `startSkillDiscoveryPrefetch`), so their handles are ready when
        // `relevant_memory_reminder_messages` and
        // `skill_discovery_reminder_message` consume them below — both of which
        // run before the blocking-limit estimate, so their tokens are counted.
        self.start_memory_prefetch().await;
        self.start_skill_discovery_prefetch().await;
        self.maybe_extract_session_memory().await;

        // Native Or freezes the host context before the query's automatic
        // compaction; Gr and the later Br projection reuse that same snapshot.
        let (instruction_key, instruction_load) = self.main_instruction_load().await;
        let scalar_context = self
            .additional_context_message_from_load(instruction_key, &instruction_load)
            .await
            .map_err(OrchestratorError::Internal)?;
        let inline_context = if self.uses_announced_context().await {
            None
        } else {
            scalar_context
        };
        let instruction_reason = self.frozen_instruction_refresh_reason();

        let display_system = system.map(
            lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput::display_text,
        );
        self.seed_compact_cache_safe_params(display_system.as_deref())
            .await;
        self.maybe_compact_before_call().await;
        // 2.1.232: accepted peer inbox → user-role `<cross-session-message>`
        // before the outgoing snapshot is cloned from history.
        let _ = self.drain_peer_inbox(false).await;

        let prepared_call = self
            .prepare_model_call_snapshot(path, display_system.as_deref(), user_cancel)
            .await?;
        let mut snapshot = prepared_call.history_snapshot;

        let mut context_announcements = PreparedContextAnnouncements {
            messages: self.context_announcements_from_frozen_snapshot().await,
            before_task_notification: None,
            refresh_reason: instruction_reason.as_str(),
            inline_context,
        };
        snapshot.extend(context_announcements.messages.iter().cloned());

        // Inline routing uses the native outgoing userContext prefix. Normal
        // routing has already appended durable typed announcements above.
        self.prepend_leading_context(&mut snapshot, &context_announcements)
            .await;

        let reminders = self
            .collect_turn_reminders(in_human_turn, is_regular_user_prompt, user_cancel)
            .await;
        let mut guarded_async_hook_reminders = reminders.guarded_async_hook_reminders;
        let transient_reminders = reminders.transient;
        context_announcements.before_task_notification = reminders
            .task_notifications
            .first()
            .map(ConversationMessage::id);
        // Durable completions first: they now live in history, so they belong
        // after the last real entry and before the transient reminders. The
        // snapshot was taken before they were appended.
        snapshot.extend(reminders.task_notifications);
        let mut turn_reminders =
            Vec::with_capacity(transient_reminders.len() + reminders.model_reminders.len());
        let mut model_reminders = reminders.model_reminders.into_iter().peekable();
        for index in 0..=transient_reminders.len() {
            while model_reminders
                .peek()
                .is_some_and(|(position, _)| *position == index)
            {
                turn_reminders.push(model_reminders.next().expect("peeked reminder").1);
            }
            if let Some(reminder) = transient_reminders.get(index) {
                turn_reminders.push(reminder.clone());
            }
        }
        snapshot.extend(turn_reminders.iter().cloned());

        let (wire_tools, skip_global_cache_for_system_prompt) = self.build_wire_tools().await;
        // Carved-slate records the first eligible static prompt before the
        // request. A resumed session with no valid attachment stays live and
        // does not create a replacement snapshot.
        self.record_prompt_snapshot_if_needed(system, &wire_tools)
            .await;

        // Rebuilt on every model step: a ToolSearch result marks schemas as
        // discovered, so the immediately following request must include them
        // with `defer_loading:true`. Computing it ADVANCES the announced-set
        // tracking, so it is computed ONCE here and reattached on re-snapshot.
        let deferred_reminder = if let Some(reminder) = self.deferred_tools_reminder_message() {
            self.mod_prompt_attachment(
                "deferred_tools_delta",
                reminder,
                serde_json::json!({"kind":"engine"}),
            )
            .await
        } else {
            None
        };
        if let Some(reminder) = deferred_reminder.clone() {
            self.prepend_transient_leading_context(&mut snapshot, reminder);
        }
        // Inline `date_change`: prepended AFTER the deferred insert so the final order
        // is [date_change, deferred_tools_delta, …], matching the oracle batch
        // order (`Ky("date_change")` before `Ky("deferred_tools_delta")`). The
        // dedupe is committed downstream, once the request is actually sent —
        // not here, where it has only been computed.
        let date_change_reminder = if self.uses_announced_context().await {
            None
        } else {
            let session_id = self.session.lock().await.session_id;
            if let Some(reminder) = self.date_change_reminder_message(session_id) {
                self.mod_prompt_attachment(
                    "date_change",
                    reminder,
                    serde_json::json!({"kind":"engine"}),
                )
                .await
            } else {
                None
            }
        };
        if let Some(reminder) = date_change_reminder.clone() {
            self.prepend_transient_leading_context(&mut snapshot, reminder);
        }
        self.screen_mod_persisted_attachments(&mut snapshot).await;
        crate::prompt::async_hook_response::retain_current_async_hook_reminders(
            &mut snapshot,
            &mut turn_reminders,
            &mut guarded_async_hook_reminders,
        );

        Ok(PreparedTurnStep {
            snapshot,
            model: prepared_call.model,
            model_profile: prepared_call.model_profile,
            outgoing_history_rewriter: prepared_call.outgoing_history_rewriter,
            turn_reminders,
            guarded_async_hook_reminders,
            context_announcements,
            wire_tools,
            skip_global_cache_for_system_prompt,
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
/// Model reminders retain their insertion positions among transient reminders;
/// preparation merges them into the reusable ordered request suffix.
pub(crate) struct TurnReminders {
    pub(crate) transient: Vec<ConversationMessage>,
    pub(crate) task_notifications: Vec<ConversationMessage>,
    pub(crate) model_reminders: Vec<(usize, ConversationMessage)>,
    pub(crate) guarded_async_hook_reminders:
        Vec<(MessageId, Arc<dyn hooks::attachment::HookPublicationGuard>)>,
}

impl ConversationOrchestrator {
    fn push_engine_attachment<'a>(
        &'a self,
        output: &'a mut Vec<ConversationMessage>,
        kind: &'a str,
        message: ConversationMessage,
    ) -> futures::future::BoxFuture<'a, ()> {
        Box::pin(async move {
            if let Some(message) = self
                .mod_prompt_attachment(kind, message, serde_json::json!({"kind":"engine"}))
                .await
            {
                output.push(message);
            }
        })
    }

    /// Collect reminder producers in their model-facing order exactly once.
    pub(crate) fn collect_turn_reminders<'a>(
        &'a self,
        in_human_turn: bool,
        is_regular_user_prompt: bool,
        user_cancel: Option<&'a CancellationToken>,
    ) -> futures::future::BoxFuture<'a, TurnReminders> {
        // Reminder fan-out has a large state machine; allocate it here so
        // preparing a turn does not embed or copy that state through its callers.
        Box::pin(async move {
            let mut transient = Vec::new();
            let mut model_reminders = Vec::new();
            let mut guarded_async_hook_reminders = Vec::new();

            if let Some(reminder) = self.brief_mode_reminder_message() {
                transient.push(reminder);
            }
            if let Some(reminder) = self.agent_listing_reminder_message().await {
                self.push_engine_attachment(&mut transient, "agent_listing_delta", reminder)
                    .await;
            }
            for reminder in self.changed_files_reminder_messages(user_cancel).await {
                self.push_engine_attachment(&mut transient, "edited_text_file", reminder)
                    .await;
            }
            for reminder in self.nested_memory_reminder_messages().await {
                if let Some(reminder) = self
                    .mod_prompt_attachment(
                        "nested_memory",
                        reminder,
                        serde_json::json!({"kind":"engine"}),
                    )
                    .await
                {
                    model_reminders.push((transient.len(), reminder));
                }
            }
            if let Some(reminder) = self.skill_discovery_reminder_message().await {
                self.push_engine_attachment(&mut transient, "skill_discovery", reminder)
                    .await;
            }
            if let Some(reminder) = self.skill_listing_reminder_message().await {
                self.push_engine_attachment(&mut transient, "skill_listing", reminder)
                    .await;
            }
            let mcp_instructions_position = transient.len();
            // DIVERGENCE (position, deliberate): the oracle's attachment fan-out
            // (@296520120) emits `changed_files` immediately after
            // `agent_listing_delta` and immediately BEFORE `nested_memory`. This
            // port injects `nested_memory` above — it has to, to read the
            // ReadState claims made above — so `changed_files`
            // sits directly after `agent_listing_delta` instead, which preserves
            // its order relative to everything downstream. `crate::prompt::changed_files`
            // carries the same note from the renderer's side.
            transient.extend(self.changed_files_reminder_messages(user_cancel).await);

            let plan_reminders = self.plan_mode_turn_messages().await;
            let has_reentry = plan_reminders.len() > 1;
            let plan_detail = if plan_reminders.is_empty() {
                None
            } else {
                let session_id = self.session.lock().await.session_id;
                let plan_file_path = self.session_plan_file_path(&session_id);
                let has_plan = std::path::Path::new(&plan_file_path).exists();
                let emitted = self
                    .prompt_runtime
                    .plan_reminder_cadence
                    .lock()
                    .await
                    .attachments_emitted;
                let reminder = if emitted % PLAN_FULL_REMINDER_EVERY_N_ATTACHMENTS == 1 {
                    "full"
                } else {
                    "sparse"
                };
                Some((plan_file_path, has_plan, reminder))
            };
            for (index, reminder) in plan_reminders.into_iter().enumerate() {
                let kind = if has_reentry && index == 0 {
                    "plan_mode_reentry"
                } else {
                    "plan_mode"
                };
                let detail = plan_detail.as_ref().map(|(path, exists, cadence)| {
                    if kind == "plan_mode_reentry" {
                        serde_json::json!({"planFilePath":path})
                    } else {
                        serde_json::json!({
                            "reminder":cadence,
                            "planFilePath":path,
                            "hasPlan":exists
                        })
                    }
                });
                if let Some(reminder) = self
                    .mod_prompt_attachment_with_detail(
                        kind,
                        reminder,
                        serde_json::json!({"kind":"engine"}),
                        detail,
                    )
                    .await
                {
                    transient.push(reminder);
                }
            }
            if let Some(reminder) = self.plan_mode_exit_message().await {
                let session_id = self.session.lock().await.session_id;
                let plan_file_path = self.session_plan_file_path(&session_id);
                let has_plan = std::path::Path::new(&plan_file_path).exists();
                if let Some(reminder) = self
                    .mod_prompt_attachment_with_detail(
                        "plan_mode_exit",
                        reminder,
                        serde_json::json!({"kind":"engine"}),
                        Some(serde_json::json!({"planFilePath":plan_file_path,"hasPlan":has_plan})),
                    )
                    .await
                {
                    transient.push(reminder);
                }
            }
            // `todo_reminder_message` already returns a system-reminder envelope.
            let todo_reminder = self.todo_reminder_message().await;
            let todo_reminder_fired = todo_reminder.is_some();
            if let Some(reminder) = todo_reminder {
                self.push_engine_attachment(&mut transient, "todo_reminder", reminder)
                    .await;
            }
            if let Some(reminder) = self
                .tool_search_usage_reminder_message(todo_reminder_fired)
                .await
            {
                self.push_engine_attachment(&mut transient, "tool_search_usage_reminder", reminder)
                    .await;
            }
            if let Some(reminder) = self.silent_turn_reminder_message().await {
                self.push_engine_attachment(&mut transient, "silent_turn_reminder", reminder)
                    .await;
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

            for reminder in self.async_hook_response_mod_messages().await {
                if let Some(guard) = reminder.publication_guard {
                    guarded_async_hook_reminders.push((reminder.message.id(), guard));
                }
                transient.push(reminder.message);
            }
            // PostToolBatch additionalContext is already present in durable
            // session history. Carry only its private generation authority through
            // the same final request-admission filter as async hook reminders.
            guarded_async_hook_reminders.extend(
                self.prompt_runtime
                    .take_guarded_prompt_message_guards()
                    .await,
            );

            // Task completion can enqueue memory updates, so this must follow the
            // notification drain and persistence above.
            for reminder in self.memory_update_reminder_messages().await {
                self.push_engine_attachment(&mut transient, "memory_update", reminder)
                    .await;
            }
            for reminder in self.relevant_memory_reminder_messages().await {
                self.push_engine_attachment(&mut transient, "relevant_memories", reminder)
                    .await;
            }
            if let Some(reminder) = self.output_style_reminder_message().await {
                self.push_engine_attachment(&mut transient, "output_style", reminder)
                    .await;
            }
            if let Some(reminder) = self.new_diagnostics_reminder_message().await {
                self.push_engine_attachment(&mut transient, "diagnostics", reminder)
                    .await;
            }
            let total_tokens = self
                .total_tokens_reminder_message(is_regular_user_prompt)
                .await;
            if let Some(reminder) = self.mcp_instructions_reminder_message().await {
                model_reminders.push((mcp_instructions_position, reminder));
            }
            if let Some(reminder) = total_tokens {
                let content = reminder.text_content();
                let text = content
                    .strip_prefix("<system-reminder>\n")
                    .and_then(|text| text.strip_suffix("\n</system-reminder>"))
                    .unwrap_or(&content);
                let attachment = serde_json::json!({"type":"total_tokens_reminder", "text":text});
                model_reminders.push((
                    transient.len(),
                    self.persist_model_reminder(reminder, attachment).await,
                ));
            }

            TurnReminders {
                transient,
                task_notifications,
                model_reminders,
                guarded_async_hook_reminders,
            }
        })
    }
}
