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
    /// Optional Anthropic-only request suffix tied to this step's current tool
    /// result. It is applied to the API clone and never persisted in history.
    pub(crate) tool_result_reminder_suffixes:
        Vec<llm_runtime::convert::AnthropicToolResultReminderSuffix>,
    /// Trusted Native classifier facts captured from the live permission gate
    /// for this main request; kept outside conversation history.
    pub(crate) native_anthropic_request_context:
        Option<lingxi_llm_client::NativeAnthropicRequestContext>,
    /// Host gate snapshot used only if this model response reaches max_tokens.
    pub(crate) thinking_resumption_host_facts:
        crate::conversation::NativeThinkingResumptionHostFacts,
    /// Keep async-hook producer generations through PTL/fallback snapshot
    /// rebuilds so reset can remove an old completion at model admission.
    pub(crate) guarded_async_hook_reminders:
        Vec<(MessageId, Arc<dyn hooks::attachment::HookPublicationGuard>)>,
    pub(crate) context_announcements: PreparedContextAnnouncements,
    pub(crate) deferred_context_announcement_rows: Vec<(
        ConversationMessage,
        lingxi_core::types::utf16_json::Utf16JsonProjection,
    )>,
    /// The static prompt snapshot is consumed and persisted only after this
    /// prepared step passes the cancellation boundary.
    pub(crate) deferred_system_prompt:
        Option<lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput>,
    pub(crate) wire_tools: Vec<lingxi_core::host::SourceToolWireDefinition>,
    /// Exact canonical names and aliases captured from the same filtered tool
    /// vector that produced this request's wire tools.
    pub(crate) available_tool_names:
        Option<Vec<lingxi_core::types::utf16_json::Utf16JsonProjection>>,
    pub(crate) skip_global_cache_for_system_prompt: bool,
    pub(crate) deferred_reminder: Option<ConversationMessage>,
    pub(crate) date_change_reminder: Option<ConversationMessage>,
}

fn classify_prompt_ingress_history(
    history: &[ConversationMessage],
    mut is_injected: impl FnMut(&lingxi_core::types::MessageId) -> bool,
) -> crate::request_context::PromptIngressKind {
    use crate::request_context::PromptIngressKind::{LocalSkill, Other, RegularUser};

    for message in history.iter().rev() {
        match message {
            ConversationMessage::User {
                id,
                content,
                is_meta,
                is_compact_summary,
                turn_companion,
                ..
            } => {
                if content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
                {
                    return Other;
                }
                // A native Skill body is marked by trusted turnCompanion
                // metadata. Never infer this class from its command header text.
                if *turn_companion == Some(Some(true)) {
                    return LocalSkill;
                }
                if *is_meta {
                    continue;
                }
                if is_injected(id) || *is_compact_summary {
                    return Other;
                }
                return RegularUser;
            }
            ConversationMessage::Assistant { .. } => return Other,
            ConversationMessage::System { subtype, .. }
                if subtype.as_deref() == Some("compact_boundary") =>
            {
                return Other;
            }
            ConversationMessage::System { .. } => {}
        }
    }
    Other
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
        let call = self
            .prepare_model_call_snapshot(ModelCallPath::Streaming, system, cancel)
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
        let tool_result_reminder_suffixes =
            self.tool_result_reminder_suffixes_for_snapshot(&snapshot);
        Ok(PreparedTurnStep {
            snapshot,
            model: call.model,
            model_profile: call.model_profile,
            outgoing_history_rewriter: call.outgoing_history_rewriter,
            turn_reminders,
            tool_result_reminder_suffixes,
            native_anthropic_request_context: previous.native_anthropic_request_context.clone(),
            thinking_resumption_host_facts: previous.thinking_resumption_host_facts.clone(),
            guarded_async_hook_reminders,
            context_announcements: previous.context_announcements.clone(),
            deferred_context_announcement_rows: Vec::new(),
            deferred_system_prompt: None,
            wire_tools: previous.wire_tools.clone(),
            available_tool_names: previous.available_tool_names.clone(),
            skip_global_cache_for_system_prompt: previous.skip_global_cache_for_system_prompt,
            deferred_reminder: previous.deferred_reminder.clone(),
            date_change_reminder: previous.date_change_reminder.clone(),
        })
    }

    /// Capture the step's prompt origin before preparation can compact history
    /// or append durable attachments. The companion bit is trusted structured
    /// history metadata: it makes a LocalSkill step non-regular even when the
    /// preceding command header is an ordinary user row.
    pub(crate) async fn regular_user_prompt_for_model_step(&self) -> bool {
        if matches!(
            crate::request_context::pending_model_step_ingress_kind(),
            Some(
                crate::request_context::PromptIngressKind::DirectBash
                    | crate::request_context::PromptIngressKind::LocalSkill
            )
        ) || crate::request_context::unknown_command_fallback_pending()
        {
            return false;
        }
        self.current_prompt_ingress_kind_from_history().await
            == crate::request_context::PromptIngressKind::RegularUser
    }

    async fn current_prompt_ingress_kind_from_history(
        &self,
    ) -> crate::request_context::PromptIngressKind {
        let session = self.session.lock().await;
        classify_prompt_ingress_history(&session.history, |id| {
            session.injected_message_sources.contains_key(id)
        })
    }

    async fn prompt_ingress_kind_for_model_step(
        &self,
        captured_regular_user_prompt: bool,
    ) -> crate::request_context::PromptIngressKind {
        use crate::request_context::PromptIngressKind::{LocalSkill, Other, RegularUser};

        if let Some(kind) = crate::request_context::take_model_step_ingress_kind() {
            return kind;
        }
        let detected = self.current_prompt_ingress_kind_from_history().await;
        if detected == LocalSkill {
            return LocalSkill;
        }
        if crate::request_context::unknown_command_fallback_pending() {
            return Other;
        }
        if captured_regular_user_prompt {
            RegularUser
        } else {
            Other
        }
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
    /// * `user_cancel` — both turn drivers pass their token into preparation.
    ///   The initial attachment batch checks it before the first durable append;
    ///   once an append starts, callers let preparation finish the batch so the
    ///   transcript tip and the rest of the rows stay in sync. The request
    ///   itself remains cancellation-gated by its caller.
    pub(crate) async fn prepare_turn_step(
        &self,
        path: ModelCallPath,
        system: Option<&lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput>,
        in_human_turn: bool,
        is_regular_user_prompt: bool,
        user_cancel: Option<&CancellationToken>,
    ) -> Result<PreparedTurnStep, OrchestratorError> {
        let prompt_ingress_kind = self
            .prompt_ingress_kind_for_model_step(is_regular_user_prompt)
            .await;
        let is_regular_user_prompt =
            prompt_ingress_kind == crate::request_context::PromptIngressKind::RegularUser;
        let native_nar_ingress = crate::request_context::is_native_nar_ingress(prompt_ingress_kind);
        if !is_regular_user_prompt {
            crate::request_context::clear_turn_projection();
        }
        // Native runs its next-turn MCP redial scheduler once at committed
        // regular user-turn start, before deferred status snapshots are read.
        // Cache an empty producer result before entering the stateful registry
        // call so a cancelled preparation cannot run the scheduler twice.
        if is_regular_user_prompt
            && crate::request_context::prompt_ingress_producer_rows("mcp_turn_started").is_none()
            && crate::request_context::cache_prompt_ingress_producer_rows_pending(
                self,
                "mcp_turn_started",
                Vec::new(),
            )
            .await
            .is_some()
        {
            if let Some(registry) = &self.mcp_registry {
                registry.turn_started().await;
            }
            let _ = crate::request_context::complete_prompt_ingress_producer_rows(
                self,
                "mcp_turn_started",
                Vec::new(),
            )
            .await;
        }
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
            .await?;
        let inline_context = if self.uses_announced_context().await {
            None
        } else {
            scalar_context
        };
        let instruction_reason = self.frozen_instruction_refresh_reason();

        self.seed_compact_cache_safe_params(system).await?;
        self.maybe_compact_before_call(user_cancel).await;
        // 2.1.232: accepted peer inbox → user-role `<cross-session-message>`
        // before the outgoing snapshot is cloned from history.
        let _ = self.drain_peer_inbox(false).await;

        let prepared_call = self
            .prepare_model_call_snapshot(path, system, user_cancel)
            .await?;
        let first_party_anthropic = self
            .api
            .is_first_party_route(&prepared_call.model, prepared_call.model_profile.as_deref());
        let protocol = self
            .api
            .protocol_for_model(&prepared_call.model, prepared_call.model_profile.as_deref());
        let native_classifier_route = protocol.as_ref().is_some_and(|protocol| {
            super::super::native_classifier_context::supports_native_classifier_route(
                protocol,
                first_party_anthropic,
            )
        });
        let native_request_route = protocol.as_ref().is_some_and(
            super::super::native_classifier_context::supports_native_request_context_route,
        );
        let mid_conversation_tool_changes_route = protocol.as_ref().is_some_and(|protocol| {
            super::super::native_classifier_context::supports_native_mid_conversation_tool_changes_route(
                protocol,
                first_party_anthropic,
            )
        });
        let (mid_conversation_tool_changes_feature_gate, inline_tools_feature_gate) =
            if mid_conversation_tool_changes_route {
                self.api.native_tool_change_feature_gates()
            } else {
                (false, false)
            };
        let mut snapshot = prepared_call.history_snapshot;
        // The current Native NAr batch remains task-local until accepted request
        // assembly. Keep its identities out of this earlier history snapshot;
        // the accepted finalizer below re-reads only committed rows.
        if native_nar_ingress {
            if let Some(rows) = crate::request_context::staged_prompt_ingress_attachments() {
                let ingress_ids = rows
                    .iter()
                    .map(|row| row.message.id())
                    .collect::<std::collections::HashSet<_>>();
                snapshot.retain(|row| !ingress_ids.contains(&row.id()));
            }
        }

        let history = self.session.lock().await.model_context_history();
        let deferred_context_announcement_rows = self
            .context_announcement_rows_for_history(&history, instruction_reason.as_str())
            .await;
        let collector_context_attachments = deferred_context_announcement_rows
            .iter()
            .map(|(_, attachment)| attachment.clone())
            .collect::<Vec<_>>();
        let context_announcements = PreparedContextAnnouncements {
            messages: deferred_context_announcement_rows
                .iter()
                .map(|(message, _)| message.clone())
                .collect(),
            refresh_reason: instruction_reason.as_str(),
            inline_context,
        };
        // Sgn runs in Native's first NAr fan-out, before deferred/tool and
        // reminder rows. Produce its settings deltas now so they join the
        // staged turn batch before the collector snapshots that batch.
        if native_nar_ingress
            && !crate::request_context::prompt_ingress_batch_finalized()
            && crate::request_context::staged_prompt_ingress_attachments().is_some()
        {
            let prior_history =
                crate::request_context::prior_history_for_current_prompt(self).await;
            let _ = crate::request_context::prompt_ingress_producer_rows_or_else(
                self,
                "native_sandbox",
                || async {
                    self.native_sandbox_prompt_ingress_attachments(&prior_history)
                        .await
                },
            )
            .await;
            let _ = crate::request_context::prompt_ingress_producer_rows_or_else(
                self,
                "native_settings",
                || async {
                    self.native_settings_prompt_ingress_attachments(&prior_history)
                        .await
                },
            )
            .await;
            let _ = crate::request_context::prompt_ingress_producer_rows_or_else(
                self,
                "native_policy",
                || async {
                    self.native_policy_prompt_ingress_attachments(&prior_history)
                        .await
                },
            )
            .await;
        }
        // Refresh the live pool before producing the `skill_listing` NAr row.
        // The common post-user staging path runs before the current request's
        // final tool filters; if it did not produce a row, retry the typed
        // producer now against the exact pool used for this request.
        let (
            wire_tools,
            skip_global_cache_for_system_prompt,
            complete_wire_tools,
            available_tool_names,
        ) = self.build_wire_tools_with_complete().await?;
        if native_nar_ingress
            && !crate::request_context::prompt_ingress_batch_finalized()
            && crate::request_context::staged_prompt_ingress_attachments().is_some()
            && crate::request_context::staged_prompt_ingress_attachment("skill_listing").is_none()
        {
            crate::request_context::prompt_ingress_producer_rows_or_else_fallible(
                self,
                "skill_listing",
                || async {
                    Ok(self
                        .skill_listing_prompt_ingress_attachment()
                        .await?
                        .into_iter()
                        .collect())
                },
            )
            .await?;
        }
        let reminders = self
            .collect_turn_reminders_for_prompt_ingress(
                in_human_turn,
                prompt_ingress_kind,
                user_cancel,
                &collector_context_attachments,
            )
            .await?;
        let mut guarded_async_hook_reminders = reminders.guarded_async_hook_reminders;
        let transient_reminders = reminders.transient;
        // Durable completions first: they now live in history, so they belong
        // after the last real entry and before the transient reminders. The
        // snapshot was taken before they were appended.
        let task_notification_ids = reminders
            .task_notifications
            .iter()
            .map(ConversationMessage::id)
            .collect::<Vec<_>>();
        snapshot.extend(reminders.task_notifications);
        let mut turn_reminders =
            Vec::with_capacity(transient_reminders.len() + reminders.model_reminders.len());
        // Collectors can insert a later-produced attachment at an earlier
        // source position (for example, MCP instructions after a token row).
        // Stable sorting restores Native NAr order while retaining producer
        // order for rows that share the same insertion point.
        let mut positioned_model_reminders = reminders.model_reminders;
        positioned_model_reminders.sort_by_key(|(position, _)| *position);
        let mut model_reminders = positioned_model_reminders.into_iter().peekable();
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
        // Native NAr/rle attaches the turn's reminders before the recursive
        // query emits Wi context announcements. Persistence and retries keep
        // this order; API text packing is a separate projection.
        snapshot.extend(context_announcements.messages.iter().cloned());

        // Rebuilt on every model step: a ToolSearch result marks schemas as
        // discovered, so the immediately following request must include them
        // with `defer_loading:true`. Computing it ADVANCES the announced-set
        // tracking, so it is computed ONCE here and reattached on re-snapshot.
        let ingress_batch_finalized = crate::request_context::prompt_ingress_batch_finalized();
        let mut deferred_wire_tool_names = self
            .tools
            .tool_search_view()
            .entries()
            .into_iter()
            .map(|entry| entry.name)
            .collect::<Vec<_>>();
        deferred_wire_tool_names.sort();
        deferred_wire_tool_names.dedup();
        let mut current_wire_tool_names = wire_tools
            .iter()
            .map(|tool| tool.name.clone())
            .collect::<Vec<_>>();
        // The final wire excludes undiscovered deferred schemas. NAr receives
        // the active tool pool before that projection, so reconstruct its
        // identity set from the exact shared deferred registry view.
        current_wire_tool_names.extend(deferred_wire_tool_names.iter().cloned());
        current_wire_tool_names.sort();
        current_wire_tool_names.dedup();
        let cached_deferred_attachment = native_nar_ingress
            .then(|| {
                ingress_batch_finalized
                    .then(|| {
                        crate::request_context::staged_prompt_ingress_attachment(
                            "deferred_tools_delta",
                        )
                    })
                    .flatten()
            })
            .flatten();
        let deferred_rows: Vec<crate::request_context::PromptIngressAttachment> =
            if let Some(row) = cached_deferred_attachment {
                vec![row]
            } else if native_nar_ingress && ingress_batch_finalized {
                Vec::new()
            } else if native_nar_ingress {
                let rows: Vec<crate::request_context::PromptIngressAttachment> = if let Some(rows) =
                    crate::request_context::prompt_ingress_producer_rows("deferred_tools_delta")
                {
                    rows
                } else {
                    let rows: Vec<crate::request_context::PromptIngressAttachment> = self
                        .deferred_tools_prompt_ingress_attachment(
                            &current_wire_tool_names,
                            self.native_tool_pool_refresh_callback_available(),
                        )
                        .await
                        .into_iter()
                        .collect();
                    crate::request_context::cache_prompt_ingress_producer_rows_pending(
                        self,
                        "deferred_tools_delta",
                        rows.clone(),
                    )
                    .await
                    .unwrap_or(rows)
                };
                rows
            } else {
                self.deferred_tools_prompt_ingress_attachment(&current_wire_tool_names, None)
                    .await
                    .into_iter()
                    .collect()
            };
        let native_tool_change_api_system_rows = if native_nar_ingress {
            let scope = self.native_tool_search_scope_snapshot();
            let latches = scope.native_tool_search_state();
            let beta_rejections = scope.beta_rejections();
            let mctc_beta = lingxi_llm_client::providers::anthropic::beta_repair::Beta::MidConversationToolChanges;
            let mcs_beta =
                lingxi_llm_client::providers::anthropic::beta_repair::Beta::MidConversationSystem;
            let mctc_supported = self.api.native_mid_conversation_tool_change_supported(
                &prepared_call.model,
                prepared_call.model_profile.as_deref(),
            ) == Some(true);
            crate::request_context::prompt_ingress_api_system_rows_or_else(
                self,
                "native_tool_changes",
                || {
                    if !mctc_supported
                        || !mid_conversation_tool_changes_feature_gate
                        || beta_rejections.rejected(mctc_beta)
                        || beta_rejections.rejected(mcs_beta)
                        || latches.tool_change_header_refused
                    {
                        return Vec::new();
                    }
                    deferred_rows
                        .iter()
                        .filter_map(|row| {
                            super::super::reminders_impl::native_tool_change_api_system_message(
                                row,
                                if inline_tools_feature_gate {
                                    &complete_wire_tools
                                } else {
                                    &[]
                                },
                            )
                        })
                        .collect()
                },
            )
            .await
        } else {
            Vec::new()
        };
        let projection_scope = self.native_tool_search_scope_snapshot();
        let projection_latches = projection_scope.native_tool_search_state();
        let projection_beta_rejections = projection_scope.beta_rejections();
        let mctc_beta =
            lingxi_llm_client::providers::anthropic::beta_repair::Beta::MidConversationToolChanges;
        let mcs_beta =
            lingxi_llm_client::providers::anthropic::beta_repair::Beta::MidConversationSystem;
        let mctc_supported = self.api.native_mid_conversation_tool_change_supported(
            &prepared_call.model,
            prepared_call.model_profile.as_deref(),
        ) == Some(true);
        let surface_late_tool_additions = mctc_supported
            && mid_conversation_tool_changes_feature_gate
            && !projection_beta_rejections.rejected(mctc_beta)
            && !projection_beta_rejections.rejected(mcs_beta)
            && !projection_latches.tool_change_header_refused
            && self.tools.deferral().keeps_deferred_tools();
        let by_value_tool_definition_names = |message: &ConversationMessage| {
            let ConversationMessage::System {
                api_system: Some(api_system),
                ..
            } = message
            else {
                return Vec::new();
            };
            api_system
                .content
                .iter()
                .filter_map(|block| {
                    let lingxi_core::types::ContentBlock::ProviderContent { protocol, value } =
                        block
                    else {
                        return None;
                    };
                    (protocol == "anthropic_messages"
                        && value.string_at("/type") == Some("tool_addition")
                        && value.string_at("/tool/type") == Some("tool_definition"))
                    .then(|| value.string_at("/tool/definition/name").map(str::to_owned))
                    .flatten()
                })
                .collect::<Vec<_>>()
        };
        let mut inline_tool_definition_names = snapshot
            .iter()
            .chain(turn_reminders.iter())
            .chain(context_announcements.messages.iter())
            .chain(native_tool_change_api_system_rows.iter())
            .flat_map(by_value_tool_definition_names)
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        inline_tool_definition_names.sort();
        let inline_tool_definition_names =
            (!inline_tool_definition_names.is_empty()).then_some(inline_tool_definition_names);
        let deferred_tools_projection_facts =
            crate::request_context::DeferredToolsProjectionFacts {
                surface_late_tool_additions,
                inline_tool_definition_names,
                deferred_wire_tool_names,
            };
        crate::request_context::set_deferred_tools_projection_facts(
            deferred_tools_projection_facts.clone(),
        );
        if native_nar_ingress {
            let scope = self.native_tool_search_scope_snapshot();
            let latches = scope.native_tool_search_state();
            let beta_rejections = scope.beta_rejections();
            let mctc_beta = lingxi_llm_client::providers::anthropic::beta_repair::Beta::MidConversationToolChanges;
            let mcs_beta =
                lingxi_llm_client::providers::anthropic::beta_repair::Beta::MidConversationSystem;
            let mctc_supported = self.api.native_mid_conversation_tool_change_supported(
                &prepared_call.model,
                prepared_call.model_profile.as_deref(),
            ) == Some(true);
            let use_tool_search = self.tools.deferral().request_supported()
                && wire_tools.iter().any(|tool| tool.name == "ToolSearch");
            let mctc_blocks_eligible = mctc_supported
                && mid_conversation_tool_changes_feature_gate
                && !beta_rejections.rejected(mctc_beta)
                && !beta_rejections.rejected(mcs_beta)
                && !latches.tool_change_header_refused
                && self.tools.deferral().keeps_deferred_tools();
            let inline_tool_names = wire_tools
                .iter()
                .filter(|tool| !tool.defer_loading)
                .map(|tool| tool.name.clone())
                .collect::<std::collections::HashSet<_>>();
            let _name_only_rows =
                crate::request_context::prompt_ingress_producer_rows_or_else(
                    self,
                    "deferred_tools_record",
                    || async {
                        if !use_tool_search {
                            return Vec::new();
                        }
                        let addition_names =
                            super::super::reminders_impl::native_tool_change_addition_names(
                                &native_tool_change_api_system_rows,
                            );
                        deferred_rows
                            .iter()
                            .filter_map(|row| {
                                super::super::reminders_impl::deferred_tools_name_only_record_attachment(
                                    row,
                                    self.tools.deferral(),
                                    use_tool_search,
                                    mctc_blocks_eligible,
                                    &inline_tool_names,
                                    &addition_names,
                                )
                            })
                            .collect::<Vec<_>>()
                    },
                )
                .await;
        }
        if native_nar_ingress
            && !ingress_batch_finalized
            && deferred_rows.is_empty()
            && !crate::request_context::prompt_ingress_producer_is_complete("deferred_tools_delta")
        {
            let _ = crate::request_context::complete_prompt_ingress_producer_rows(
                self,
                "deferred_tools_delta",
                Vec::new(),
            )
            .await;
        }
        let deferred_attachment = deferred_rows.into_iter().next();
        let deferred_reminder = if let Some(mut row) = deferred_attachment {
            let pending_initial_producer = native_nar_ingress
                && !ingress_batch_finalized
                && !crate::request_context::prompt_ingress_producer_is_complete(
                    "deferred_tools_delta",
                );
            let has_rendered_reminder = row.rendered.array_len("").is_some_and(|length| length > 0);
            let mut keep = true;
            if pending_initial_producer && has_rendered_reminder {
                if let Some(message) = self
                    .mod_prompt_attachment(
                        "deferred_tools_delta",
                        row.message.clone(),
                        serde_json::json!({"kind":"engine"}),
                    )
                    .await
                {
                    row.message = message;
                    if let Some(rendered) =
                        crate::request_context::rendered_system_message(&row.message)
                    {
                        row.rendered = rendered;
                    } else {
                        keep = false;
                    }
                } else {
                    keep = false;
                }
                let completed_rows = keep.then_some(row.clone()).into_iter().collect();
                let _ = crate::request_context::complete_prompt_ingress_producer_rows(
                    self,
                    "deferred_tools_delta",
                    completed_rows,
                )
                .await;
            } else if pending_initial_producer {
                let _ = crate::request_context::complete_prompt_ingress_producer_rows(
                    self,
                    "deferred_tools_delta",
                    vec![row.clone()],
                )
                .await;
            } else if !native_nar_ingress && has_rendered_reminder {
                if let Some(message) = self
                    .mod_prompt_attachment(
                        "deferred_tools_delta",
                        row.message.clone(),
                        serde_json::json!({"kind":"engine"}),
                    )
                    .await
                {
                    row.message = message;
                    if let Some(rendered) =
                        crate::request_context::rendered_system_message(&row.message)
                    {
                        row.rendered = rendered;
                    } else {
                        keep = false;
                    }
                } else {
                    keep = false;
                }
                if keep {
                    self.persist_staged_prompt_ingress_reminder(&row).await;
                }
            } else if !native_nar_ingress {
                // Native can persist a status-clearing attachment whose API
                // renderer returns `[]`; it still carries typed replay state.
                self.persist_staged_prompt_ingress_reminder(&row).await;
            }
            if keep {
                let cached_rendered_count = row.rendered.array_len("").unwrap_or_default();
                crate::request_context::render_deferred_tools_delta_for_request(
                    row.message.id(),
                    &row.payload,
                    (cached_rendered_count > 0).then_some(&row.message),
                    cached_rendered_count,
                    deferred_tools_projection_facts.clone(),
                    crate::request_context::staged_name_only_announcement_uuids()
                        .contains(&row.message.id().to_string()),
                )
            } else {
                None
            }
        } else {
            None
        };
        if native_nar_ingress {
            crate::request_context::mark_native_request_assembly_accepted();
            crate::request_context::finalize_prompt_ingress_batch(self, user_cancel).await?;

            // The request history snapshot predates accepted NAr publication.
            // Rebuild its current batch from the task-local identities that the
            // finalizer actually committed, so no pre-acceptance row can leak
            // into the request and no accepted row is lost from this request.
            let accepted_ingress =
                crate::request_context::staged_prompt_ingress_attachments().unwrap_or_default();
            let staged_ingress_ids = accepted_ingress
                .iter()
                .map(|row| row.message.id())
                .collect::<std::collections::HashSet<_>>();
            snapshot.retain(|row| !staged_ingress_ids.contains(&row.id()));
            if crate::request_context::prompt_ingress_batch_finalized() {
                let model_visible_ingress_ids = accepted_ingress
                    .iter()
                    .filter(|row| row.rendered.array_len("").is_some_and(|length| length > 0))
                    .map(|row| row.message.id())
                    .collect::<std::collections::HashSet<_>>();
                let deferred_ingress_ids = accepted_ingress
                    .iter()
                    .filter(|row| row.payload.string_at("/type") == Some("deferred_tools_delta"))
                    .map(|row| row.message.id())
                    .collect::<std::collections::HashSet<_>>();
                let request_history_ids = model_visible_ingress_ids
                    .union(&deferred_ingress_ids)
                    .copied()
                    .collect::<std::collections::HashSet<_>>();
                let accepted_history = {
                    let session = self.session.lock().await;
                    session
                        .model_context_history()
                        .into_iter()
                        .filter(|row| request_history_ids.contains(&row.id()))
                        .map(|row| (row.id(), row))
                        .collect::<std::collections::HashMap<_, _>>()
                };
                let accepted_ids = accepted_history
                    .keys()
                    .copied()
                    .collect::<std::collections::HashSet<_>>();
                turn_reminders.retain(|row| {
                    !staged_ingress_ids.contains(&row.id()) || accepted_ids.contains(&row.id())
                });
                let mut accepted_history = accepted_history;
                for attachment in accepted_ingress {
                    if attachment
                        .rendered
                        .array_len("")
                        .is_some_and(|length| length > 0)
                        || attachment.payload.string_at("/type") == Some("deferred_tools_delta")
                    {
                        if let Some(message) = accepted_history.remove(&attachment.message.id()) {
                            snapshot.push(message);
                        }
                    }
                }
            } else {
                // A cancellation at the commit boundary leaves the cached NAr
                // batch retryable, but this prepared step must not retain it.
                turn_reminders.retain(|row| !staged_ingress_ids.contains(&row.id()));
            }

            // This snapshot predates the NAr finalizer, which flushes the
            // current UPE and following prompt.submit rows into live history.
            // Copy only those exact buffered IDs from model-visible history so
            // the request projection can place the complete tail after NAr.
            // The finalizer remains the sole durable/live append path.
            let mut buffered_tail_ids = self.prompt_runtime.current_prompt_upe_attachment_ids();
            buffered_tail_ids
                .extend(crate::request_context::finalized_buffered_prompt_submit_row_ids());
            if !buffered_tail_ids.is_empty() {
                let snapshot_ids = snapshot
                    .iter()
                    .map(ConversationMessage::id)
                    .collect::<std::collections::HashSet<_>>();
                let missing_tail_rows = {
                    let session = self.session.lock().await;
                    session
                        .model_context_history()
                        .into_iter()
                        .filter(|message| {
                            let id = message.id();
                            buffered_tail_ids.contains(&id) && !snapshot_ids.contains(&id)
                        })
                        .collect::<Vec<_>>()
                };
                snapshot.extend(missing_tail_rows);
            }
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
        // The one-shot unknown-command fallback has no regular-user ingress,
        // but Native still places its request-owned row after the initial NAr
        // reminders and before shared context. Later tool steps keep the
        // durable history order instead of moving that row again.
        if is_regular_user_prompt
            || (native_nar_ingress
                && prompt_ingress_kind == crate::request_context::PromptIngressKind::Other)
        {
            crate::request_context::set_turn_projection(
                task_notification_ids
                    .into_iter()
                    .chain(turn_reminders.iter().map(ConversationMessage::id))
                    .collect(),
                context_announcements
                    .messages
                    .iter()
                    .map(ConversationMessage::id)
                    .collect(),
            );
        } else {
            crate::request_context::clear_turn_projection();
        }
        crate::request_context::attach(&mut snapshot);
        self.project_deferred_tools_request_view(&mut snapshot, deferred_tools_projection_facts);
        self.project_typed_attachment_order(
            &mut snapshot,
            &turn_reminders,
            &context_announcements.messages,
            self.api.supports_native_mid_conversation_system(
                &prepared_call.model,
                prepared_call.model_profile.as_deref(),
            ),
        )
        .await;
        if !native_tool_change_api_system_rows.is_empty() {
            let insertion_index = snapshot
                .iter()
                .rposition(|message| {
                    matches!(
                        message,
                        ConversationMessage::User {
                            is_meta: false,
                            is_compact_summary: false,
                            ..
                        }
                    )
                })
                .unwrap_or(snapshot.len());
            snapshot.splice(
                insertion_index..insertion_index,
                native_tool_change_api_system_rows.iter().cloned(),
            );
        }
        // Fixed host/scalar context is an API prefix outside Native's typed
        // attachment grouping, so add it after the full-history projection.
        self.prepend_leading_context(&mut snapshot, &context_announcements)
            .await;
        if let Some(reminder) = deferred_reminder.clone() {
            if !snapshot.iter().any(|message| message.id() == reminder.id()) {
                self.prepend_transient_leading_context(&mut snapshot, reminder);
            }
        }
        if let Some(reminder) = date_change_reminder.clone() {
            self.prepend_transient_leading_context(&mut snapshot, reminder);
        }
        self.screen_mod_persisted_attachments(&mut snapshot).await;
        let tool_result_reminder_suffixes =
            self.tool_result_reminder_suffixes_for_snapshot(&snapshot);
        // Native's server classifier is attached only to the selected
        // first-party Anthropic or Foundry Claude route. Resolve provider
        // identity instead of inferring eligibility from the model string.
        let native_anthropic_request_context = if native_request_route {
            super::super::native_classifier_context::request_context(
                self.perms.permission_policy_snapshot(),
                native_classifier_route,
                mid_conversation_tool_changes_route,
                mid_conversation_tool_changes_feature_gate,
                inline_tools_feature_gate,
                &self.config.query_source,
            )
            .await
        } else {
            None
        };
        let mut thinking_resumption_host_facts = self
            .native_policy_host_facts_provider
            .as_ref()
            .map(|provider| provider().thinking_resumption)
            .unwrap_or_default();
        let native_resume_route_allowed = crate::output_recovery::native_route_supports_resumption(
            self.api.as_ref(),
            &prepared_call.model,
            prepared_call.model_profile.as_deref(),
        );
        thinking_resumption_host_facts.provider_route_allowed = Some(native_resume_route_allowed);
        let model_capability = if native_resume_route_allowed {
            self.api
                .native_model_option_capability_for_request(
                    &prepared_call.model,
                    prepared_call.model_profile.as_deref(),
                    "interleaved_thinking",
                )
                .or_else(|| {
                    crate::output_recovery::native_interleaved_thinking_default(
                        self.api.as_ref(),
                        &prepared_call.model,
                        prepared_call.model_profile.as_deref(),
                    )
                })
        } else {
            Some(false)
        };
        thinking_resumption_host_facts.model_capability_allowed =
            model_capability.or(thinking_resumption_host_facts.model_capability_allowed);
        thinking_resumption_host_facts.thinking_resumption_refused = self
            .api
            .native_thinking_resumption_refused()
            .or(thinking_resumption_host_facts.thinking_resumption_refused);
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
            tool_result_reminder_suffixes,
            native_anthropic_request_context,
            thinking_resumption_host_facts,
            guarded_async_hook_reminders,
            context_announcements,
            deferred_context_announcement_rows,
            deferred_system_prompt: system.cloned(),
            wire_tools,
            available_tool_names: Some(available_tool_names),
            skip_global_cache_for_system_prompt,
            deferred_reminder,
            date_change_reminder,
        })
    }

    /// Persist later-prepared attachments only when the request has not been
    /// cancelled. The initial prompt-ingress batch has its own commit boundary;
    /// these shared-context rows and the static prompt snapshot stay behind the
    /// post-prepare cancellation check.
    pub(crate) async fn persist_prepared_turn_attachments(&self, prepared: &mut PreparedTurnStep) {
        for (message, attachment) in
            std::mem::take(&mut prepared.deferred_context_announcement_rows)
        {
            self.persist_model_reminder(message, attachment).await;
        }
        if let Some(system) = prepared.deferred_system_prompt.take() {
            self.record_prompt_snapshot_if_needed(Some(&system), &prepared.wire_tools)
                .await;
        }
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

    /// Bool adapter retained only for existing unit tests that exercise the
    /// non-Native reminder collector directly. Production preparation passes
    /// the source-classified per-step kind below.
    #[cfg(test)]
    pub(crate) fn collect_turn_reminders<'a>(
        &'a self,
        in_human_turn: bool,
        is_regular_user_prompt: bool,
        user_cancel: Option<&'a CancellationToken>,
        context_attachments: &'a [lingxi_core::types::utf16_json::Utf16JsonProjection],
    ) -> futures::future::BoxFuture<'a, Result<TurnReminders, lingxi_core::settings::SettingsError>>
    {
        let kind = if is_regular_user_prompt {
            crate::request_context::PromptIngressKind::RegularUser
        } else {
            crate::request_context::PromptIngressKind::Other
        };
        self.collect_turn_reminders_for_prompt_ingress(
            in_human_turn,
            kind,
            user_cancel,
            context_attachments,
        )
    }

    /// Collect reminder producers in their model-facing order exactly once
    /// using the current model step's source-derived ingress class.
    pub(crate) fn collect_turn_reminders_for_prompt_ingress<'a>(
        &'a self,
        in_human_turn: bool,
        prompt_ingress_kind: crate::request_context::PromptIngressKind,
        user_cancel: Option<&'a CancellationToken>,
        context_attachments: &'a [lingxi_core::types::utf16_json::Utf16JsonProjection],
    ) -> futures::future::BoxFuture<'a, Result<TurnReminders, lingxi_core::settings::SettingsError>>
    {
        // Reminder fan-out has a large state machine; allocate it here so
        // preparing a turn does not embed or copy that state through its callers.
        Box::pin(async move {
            let is_regular_user_prompt =
                prompt_ingress_kind == crate::request_context::PromptIngressKind::RegularUser;
            let native_nar_ingress =
                crate::request_context::is_native_nar_ingress(prompt_ingress_kind);
            // Native 2.1.293 SIMPLE returns only the queued task attachments
            // before entering the ordinary reminder producer fan-out.
            if self.config.bare {
                let notification_rows =
                    crate::request_context::prompt_ingress_producer_rows_or_else(
                        self,
                        "task_notifications",
                        || async {
                            self.task_notification_reminder_messages_in_turn(in_human_turn)
                                .await
                        },
                    )
                    .await;
                let staged_batch = native_nar_ingress
                    && crate::request_context::staged_prompt_ingress_attachments().is_some();
                if !staged_batch {
                    for notification in &notification_rows {
                        self.persist_prompt_ingress_reminder(notification).await;
                    }
                }
                let task_notifications = notification_rows
                    .into_iter()
                    .map(|row| row.message)
                    .collect();
                return Ok(TurnReminders {
                    transient: Vec::new(),
                    task_notifications,
                    model_reminders: Vec::new(),
                    guarded_async_hook_reminders: Vec::new(),
                });
            }
            let mut transient = Vec::new();
            let mut model_reminders = Vec::new();
            let mut guarded_async_hook_reminders = Vec::new();
            // Pending NAr rows are request-local until the finalizer accepts
            // them. Once finalized, keep the historical live-ID filter so a
            // later re-snapshot does not resurrect rows removed by compaction.
            let mut staged_ingress = if native_nar_ingress {
                crate::request_context::staged_prompt_ingress_attachments()
            } else {
                None
            };
            if crate::request_context::prompt_ingress_batch_finalized() {
                if let Some(rows) = staged_ingress.as_mut() {
                    let live_ids = self
                        .session
                        .lock()
                        .await
                        .model_context_history()
                        .iter()
                        .map(ConversationMessage::id)
                        .collect::<std::collections::HashSet<_>>();
                    rows.retain(|row| live_ids.contains(&row.message.id()));
                }
            }
            let staged = |kind| {
                staged_ingress
                    .as_ref()
                    .and_then(|rows| {
                        rows.iter()
                            .find(|row| row.payload.string_at("/type") == Some(kind))
                    })
                    .map(|row| row.message.clone())
            };

            for kind in [
                "environment",
                "model",
                "output_style_instructions",
                "language",
            ] {
                if let Some(rows) = &staged_ingress {
                    transient.extend(
                        rows.iter()
                            .filter(|row| row.payload.string_at("/type") == Some(kind))
                            .map(|row| row.message.clone()),
                    );
                }
            }

            if let Some(reminder) = self.brief_mode_reminder_message() {
                transient.push(reminder);
            }
            if let Some(reminder) = staged("agent_listing_delta") {
                transient.push(reminder);
            } else if staged_ingress.is_none() {
                if let Some(reminder) = self.agent_listing_reminder_message().await {
                    self.push_engine_attachment(&mut transient, "agent_listing_delta", reminder)
                        .await;
                }
            }
            let changed_file_rows = crate::request_context::prompt_ingress_producer_rows_or_else(
                self,
                "changed_files",
                || async { self.changed_files_reminder_messages(user_cancel).await },
            )
            .await;
            if staged_ingress.is_none() {
                for row in &changed_file_rows {
                    self.persist_prompt_ingress_reminder(row).await;
                }
            }
            for row in changed_file_rows {
                transient.push(row.message);
            }
            for reminder in self
                .nested_memory_reminder_messages(context_attachments)
                .await
            {
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
            if let Some(reminder) = staged("skill_listing") {
                transient.push(reminder);
            } else if staged_ingress.is_none() {
                if let Some(reminder) = self.skill_listing_reminder_message().await? {
                    self.push_engine_attachment(&mut transient, "skill_listing", reminder)
                        .await;
                }
            }
            let mcp_instructions_position = transient.len();

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
            // NAr's primary-model policy producer is staged with the complete
            // ingress batch, but unlike the individual staged reminder lanes it
            // has no transient collector. Reattach its current typed rows after
            // plan-mode rows and before Native's todo/reminder tail.
            for kind in ["proactivity", "auto_mode", "auto_mode_exit"] {
                if let Some(reminder) = staged(kind) {
                    model_reminders.push((transient.len(), reminder));
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
            let notification_rows = crate::request_context::prompt_ingress_producer_rows_or_else(
                self,
                "task_notifications",
                || async {
                    self.task_notification_reminder_messages_in_turn(in_human_turn)
                        .await
                },
            )
            .await;
            if staged_ingress.is_none() {
                for notification in &notification_rows {
                    self.persist_prompt_ingress_reminder(notification).await;
                }
            }
            let task_notifications = notification_rows
                .into_iter()
                .map(|row| row.message)
                .collect();

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
            let total_tokens = if staged_ingress.is_none() {
                self.total_tokens_reminder_message(is_regular_user_prompt)
                    .await
            } else {
                None
            };
            if let Some(reminder) = staged("total_tokens_reminder") {
                model_reminders.push((transient.len(), reminder));
            } else if let Some(reminder) = total_tokens {
                let content = reminder.text_content();
                let text = content
                    .strip_prefix("<system-reminder>\n")
                    .and_then(|text| text.strip_suffix("\n</system-reminder>"))
                    .unwrap_or(&content);
                let attachment = serde_json::json!({"type":"total_tokens_reminder", "text":text});
                let attachment =
                    lingxi_core::types::utf16_json::Utf16JsonProjection::from_serde_value(
                        &attachment,
                    )
                    .expect("generated total-token reminder attachment is valid JSON");
                model_reminders.push((
                    transient.len(),
                    self.persist_model_reminder(reminder, attachment).await,
                ));
            }
            if let Some(reminder) = staged("budget_usd") {
                model_reminders.push((transient.len(), reminder));
            }

            if let Some(reminder) = staged("mcp_instructions_delta") {
                model_reminders.push((mcp_instructions_position, reminder));
            } else if native_nar_ingress
                && !crate::request_context::prompt_ingress_batch_finalized()
            {
                let rows = crate::request_context::prompt_ingress_producer_rows_or_else(
                    self,
                    "mcp_instructions_delta",
                    || async {
                        self.mcp_instructions_prompt_ingress_attachment()
                            .await
                            .into_iter()
                            .collect()
                    },
                )
                .await;
                if let Some(row) = rows.into_iter().next() {
                    let message = row.message.clone();
                    if staged_ingress.is_none() {
                        self.persist_prompt_ingress_reminder(&row).await;
                    }
                    model_reminders.push((mcp_instructions_position, message));
                }
            } else if native_nar_ingress {
                if let Some(row) =
                    crate::request_context::prompt_ingress_producer_rows("mcp_instructions_delta")
                        .and_then(|rows| rows.into_iter().next())
                {
                    model_reminders.push((mcp_instructions_position, row.message));
                }
            } else if !native_nar_ingress {
                if let Some(reminder) = self.mcp_instructions_reminder_message().await {
                    model_reminders.push((mcp_instructions_position, reminder));
                }
            }

            if let Some(reminder) = staged("mcp_dropped_tools_delta") {
                model_reminders.push((mcp_instructions_position, reminder));
            } else if native_nar_ingress
                && !crate::request_context::prompt_ingress_batch_finalized()
            {
                let rows = crate::request_context::prompt_ingress_producer_rows_or_else(
                    self,
                    "mcp_dropped_tools_delta",
                    || async {
                        self.mcp_dropped_tools_prompt_ingress_attachment()
                            .await
                            .into_iter()
                            .collect()
                    },
                )
                .await;
                if let Some(row) = rows.into_iter().next() {
                    if staged_ingress.is_none() {
                        self.persist_prompt_ingress_reminder(&row).await;
                    }
                    model_reminders.push((mcp_instructions_position, row.message));
                }
            } else if native_nar_ingress {
                if let Some(row) =
                    crate::request_context::prompt_ingress_producer_rows("mcp_dropped_tools_delta")
                        .and_then(|rows| rows.into_iter().next())
                {
                    model_reminders.push((mcp_instructions_position, row.message));
                }
            } else if let Some(row) = self.mcp_dropped_tools_prompt_ingress_attachment().await {
                model_reminders.push((mcp_instructions_position, row.message.clone()));
                self.persist_prompt_ingress_reminder(&row).await;
            }

            Ok(TurnReminders {
                transient,
                task_notifications,
                model_reminders,
                guarded_async_hook_reminders,
            })
        })
    }

    pub(crate) fn tool_result_reminder_suffixes_for_snapshot(
        &self,
        snapshot: &[ConversationMessage],
    ) -> Vec<llm_runtime::convert::AnthropicToolResultReminderSuffix> {
        let attachments = self
            .transcript
            .model_reminder_attachments
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        total_token_reminder_suffixes_from_history(snapshot, &attachments)
    }
}

fn total_token_reminder_suffixes_from_history(
    snapshot: &[ConversationMessage],
    attachments: &std::collections::HashMap<
        MessageId,
        lingxi_core::types::utf16_json::Utf16JsonProjection,
    >,
) -> Vec<llm_runtime::convert::AnthropicToolResultReminderSuffix> {
    use lingxi_core::types::ContentBlock;

    let mut current_tool_result = None;
    let mut suffixes = Vec::new();
    for message in snapshot {
        match message {
            ConversationMessage::User {
                id, is_meta: true, ..
            } => {
                let Some(attachment) = attachments.get(id) else {
                    continue;
                };
                if attachment.string_at("/type") != Some("total_tokens_reminder") {
                    continue;
                }
                let Some(tool_use_id) = current_tool_result.clone() else {
                    continue;
                };
                if let Some(text) = total_token_reminder_source(message) {
                    suffixes.push(llm_runtime::convert::AnthropicToolResultReminderSuffix {
                        tool_use_id,
                        reminder_message_id: *id,
                        text,
                    });
                }
            }
            ConversationMessage::User {
                content,
                is_meta: false,
                ..
            } => {
                current_tool_result = if !content.is_empty()
                    && content
                        .iter()
                        .all(|block| matches!(block, ContentBlock::ToolResult { .. }))
                {
                    content.iter().rev().find_map(|block| match block {
                        ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.clone()),
                        _ => None,
                    })
                } else {
                    None
                };
            }
            ConversationMessage::Assistant { .. } | ConversationMessage::System { .. } => {
                current_tool_result = None;
            }
        }
    }
    suffixes
}

fn total_token_reminder_source(
    message: &ConversationMessage,
) -> Option<lingxi_core::types::utf16_json::Utf16JsonProjection> {
    let ConversationMessage::User { content, .. } = message else {
        return None;
    };
    if content.len() != 1 {
        return None;
    }
    match content.first()? {
        lingxi_core::types::ContentBlock::Text { text, .. } => Some(
            lingxi_core::types::utf16_json::Utf16JsonProjection::from(text.as_str()),
        ),
        lingxi_core::types::ContentBlock::TextJsUtf16 {
            text,
            utf16_code_units,
            ..
        } => lingxi_core::types::utf16_json::Utf16JsonProjection::root_string(
            text.clone(),
            utf16_code_units.clone(),
        )
        .ok(),
        _ => None,
    }
}

#[cfg(test)]
mod prompt_ingress_classifier_tests {
    use super::*;
    use crate::request_context::PromptIngressKind::{DirectBash, LocalSkill, Other, RegularUser};
    use lingxi_core::types::{ContentBlock, MessageId};

    fn user(text: &str) -> ConversationMessage {
        ConversationMessage::user(MessageId::new(), text.to_owned())
    }

    fn meta(text: &str) -> ConversationMessage {
        ConversationMessage::user_meta(MessageId::new(), text.to_owned())
    }

    fn skill_body() -> ConversationMessage {
        let mut body = meta("expanded skill body");
        if let ConversationMessage::User { turn_companion, .. } = &mut body {
            *turn_companion = Some(Some(true));
        }
        body
    }

    fn assistant() -> ConversationMessage {
        ConversationMessage::Assistant {
            per_turn_effort: None,
            id: MessageId::new(),
            content: vec![ContentBlock::Text {
                text: "skill response".into(),
                citations: None,
            }],
            stop_reason: Some("end_turn".into()),
            resumable: None,
        }
    }

    fn tool_result() -> ConversationMessage {
        ConversationMessage::User {
            api_message_override: None,
            interrupted_by_shutdown: None,
            refused_message_id: None,
            id: MessageId::new(),
            content: vec![ContentBlock::ToolResult {
                tool_use_id: lingxi_core::types::ToolUseId::from("toolu_skill"),
                content: "done".into(),
                is_error: Some(false),
                provider_tool_use_id: None,
                content_blocks: None,
            }],
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
            turn_companion: None,
            usage_limit_note: None,
            origin: None,
            turn_origin: None,
        }
    }

    #[test]
    fn direct_bash_header_text_alone_does_not_change_history_classification() {
        assert_eq!(
            classify_prompt_ingress_history(&[user("<bash-input>echo hi</bash-input>")], |_| false),
            RegularUser,
            "only the host-stamped queue source marks DirectBash; XML-like text is not authority"
        );
        assert_ne!(DirectBash, RegularUser);
    }

    #[test]
    fn trusted_companion_body_classifies_typed_and_cold_skill_rows_as_nonregular() {
        let rows = vec![
            user("/review"),
            skill_body(),
            meta("UserPromptExpansion additional context"),
            meta("environment metadata"),
        ];
        assert_eq!(
            classify_prompt_ingress_history(&rows, |_| false),
            LocalSkill,
            "metadata rows are skipped back to the trusted companion body, not its header text"
        );
        assert_eq!(
            classify_prompt_ingress_history(&rows, |_| false),
            LocalSkill,
            "the same persisted row shape is sufficient after cold reconstruction"
        );
    }

    #[test]
    fn assistant_tool_result_and_new_ordinary_user_end_the_skill_classifier_window() {
        let prefix = vec![user("/review"), skill_body()];
        let mut after_assistant = prefix.clone();
        after_assistant.push(assistant());
        assert_eq!(
            classify_prompt_ingress_history(&after_assistant, |_| false),
            Other
        );

        let mut after_tool_result = prefix.clone();
        after_tool_result.push(tool_result());
        assert_eq!(
            classify_prompt_ingress_history(&after_tool_result, |_| false),
            Other
        );

        let mut after_new_user = prefix;
        after_new_user.push(user("new human prompt"));
        assert_eq!(
            classify_prompt_ingress_history(&after_new_user, |_| false),
            RegularUser
        );
    }

    #[test]
    fn an_injected_user_row_stops_instead_of_inheriting_an_old_companion() {
        let mut rows = vec![user("/review"), skill_body()];
        let injected = user("tool supplied prompt");
        let injected_id = injected.id();
        rows.push(injected);
        assert_eq!(
            classify_prompt_ingress_history(&rows, |id| *id == injected_id),
            Other
        );
    }
}

#[cfg(test)]
#[path = "prepare_tool_result_reminder_tests.rs"]
mod tool_result_reminder_tests;

#[cfg(test)]
#[path = "deferred_tools_projection_tests.rs"]
mod deferred_tools_projection_tests;
