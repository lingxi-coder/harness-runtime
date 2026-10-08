//! Conversation lifecycle hooks, goal checks, and session events.

use super::mod_session_measure_sampler::{
    ModSessionMeasureReason, ModSessionMeasureRequest, ModSessionMeasureSampler,
};
use super::*;

use hooks::ExactHookText;

async fn commit_peer_queue(
    commit: lingxi_core::host::uds_inbox::PeerQueueCommit,
    text: String,
) -> bool {
    tokio::task::spawn_blocking(move || commit(text))
        .await
        .unwrap_or(false)
}

#[cfg(test)]
#[path = "hooks/idle_measure_tests.rs"]
mod idle_measure_tests;

/// The Mod's prompt admission decision before the user row is committed.
pub(super) enum ModPromptScreen {
    Admit {
        text: String,
        text_projection: lingxi_core::types::utf16_json::Utf16JsonProjection,
        context: Vec<String>,
    },
    Drop(String),
}

/// `tengu_goal_evaluated`'s `outcome`, and the `iterations` rule that rides
/// on it.
///
/// Upstream (`src_163219561.js` @4343091):
/// `outcome: u(We ?? (_e ? "error" : Qe ? "cancelled" : "absent"))` and
/// `iterations: Fe.iterations + (Je ? 1 : 0)` where
/// `Je = We==="met" || We==="not_met" || We==="impossible"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GoalEvalOutcome {
    Met,
    NotMet,
    Impossible,
    Deferred,
    Error,
    Cancelled,
    Absent,
}

impl GoalEvalOutcome {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Met => "met",
            Self::NotMet => "not_met",
            Self::Impossible => "impossible",
            Self::Deferred => "deferred",
            Self::Error => "error",
            Self::Cancelled => "cancelled",
            Self::Absent => "absent",
        }
    }

    /// Only a real verdict advances the stored and reported count.
    pub(super) fn counts_as_iteration(self) -> bool {
        matches!(self, Self::Met | Self::NotMet | Self::Impossible)
    }
}

fn accumulate_mod_turn_usage(
    facts: &mut ModTurnFacts,
    usage: &llm_runtime::ExecutionUsage,
    model: &str,
) {
    let counts = usage.counts();
    let total = facts.usage.get_or_insert_with(|| ModTurnUsage {
        tokens: lingxi_core::token::Usage::default(),
        model: model.to_owned(),
    });
    total.tokens.input_tokens = total
        .tokens
        .input_tokens
        .saturating_add(counts.input_tokens);
    total.tokens.output_tokens = total
        .tokens
        .output_tokens
        .saturating_add(counts.output_tokens);
    total.tokens.cache_read_input_tokens = total
        .tokens
        .cache_read_input_tokens
        .saturating_add(counts.cache_read_tokens);
    total.tokens.cache_creation_input_tokens = total
        .tokens
        .cache_creation_input_tokens
        .saturating_add(counts.cache_write_tokens);
    total.model = model.to_owned();
}

async fn dispatch_queued_mod_session_event(work: ModSessionEventWork) {
    match work {
        ModSessionEventWork::TurnComplete {
            context,
            input,
            original_answer,
        } => {
            dispatch_mod_turn_complete(
                context.host,
                context.session.as_ref(),
                context.output,
                input,
                original_answer,
            )
            .await;
        }
        ModSessionEventWork::Measure {
            context,
            sampler,
            request,
        } => {
            dispatch_mod_session_measure(context, sampler, request).await;
        }
        ModSessionEventWork::MeasureRequest {
            context,
            sampler,
            reason,
            snapshot,
        } => {
            if let Some(request) = sampler.enqueue(reason, snapshot) {
                dispatch_mod_session_measure(context, sampler, request).await;
            }
        }
    }
}

async fn dispatch_mod_turn_complete(
    host: Arc<hooks::mods::ModHost>,
    session: &dyn hooks::mods::ModSessionContext,
    output: Arc<dyn OutputStream>,
    input: serde_json::Value,
    original_answer: String,
) {
    let turn_id = input
        .get("turnId")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let log_output = output.clone();
    let toast_output = output.clone();
    let status_output = output.clone();
    match host
        .dispatch_with_ui_meta_at_session(
            "turn.complete",
            input,
            session,
            |event| async move {
                let mut result = serde_json::json!({"text":event["answer"]});
                if let Some(usage) = event.get("usage") {
                    result["usage"] = usage.clone();
                }
                Ok(result)
            },
            move |plugin, text| {
                let output = log_output.clone();
                async move { output.emit_mod_log(&plugin, &text).await }
            },
            move |plugin, text, timeout_ms| {
                let output = toast_output.clone();
                async move { output.emit_mod_toast(&plugin, &text, timeout_ms).await }
            },
            move |plugin, text| {
                let output = status_output.clone();
                async move { output.emit_mod_status(&plugin, text.as_deref()).await }
            },
        )
        .await
    {
        Ok(outcome) => {
            if let Some(text) = outcome
                .result
                .get("text")
                .and_then(serde_json::Value::as_str)
            {
                if text != original_answer && !text.trim().is_empty() {
                    let notice = if outcome.hooked.is_empty() {
                        text.to_owned()
                    } else {
                        format!("{}: {text}", outcome.hooked.join("+"))
                    };
                    output.emit_system_notice(&notice, false).await;
                }
            }
        }
        Err(error) => tracing::warn!(turn_id, %error, "turn.complete Mod dispatch failed"),
    }
}

async fn dispatch_mod_session_measure(
    context: ModSessionEventContext,
    sampler: Arc<ModSessionMeasureSampler>,
    mut request: ModSessionMeasureRequest,
) {
    loop {
        if let Some(input) = sampler.prepare(&request) {
            dispatch_mod_session_measure_once(
                context.host.clone(),
                context.session.as_ref(),
                context.output.clone(),
                input,
            )
            .await;
        }
        let Some(next) = sampler.finish_one() else {
            return;
        };
        request = next;
    }
}

async fn dispatch_mod_session_measure_inline(
    host: Arc<hooks::mods::ModHost>,
    session: &dyn hooks::mods::ModSessionContext,
    output: Arc<dyn OutputStream>,
    sampler: Arc<ModSessionMeasureSampler>,
    mut request: ModSessionMeasureRequest,
) {
    loop {
        if let Some(input) = sampler.prepare(&request) {
            dispatch_mod_session_measure_once(host.clone(), session, output.clone(), input).await;
        }
        let Some(next) = sampler.finish_one() else {
            return;
        };
        request = next;
    }
}

async fn dispatch_mod_session_measure_once(
    host: Arc<hooks::mods::ModHost>,
    session: &dyn hooks::mods::ModSessionContext,
    output: Arc<dyn OutputStream>,
    input: serde_json::Value,
) {
    let session_id = session.id().await;
    let log_output = output.clone();
    let toast_output = output.clone();
    let status_output = output.clone();
    if let Err(error) = host
        .dispatch_with_ui_meta_at_session(
            "session.measure",
            input,
            session,
            |event| async move {
                let changed = event.get("changed").cloned().ok_or_else(|| {
                    hooks::mods::ModError::Hook("session.measure needs changed units".into())
                })?;
                Ok(serde_json::json!({"changed":changed}))
            },
            move |plugin, text| {
                let output = log_output.clone();
                async move { output.emit_mod_log(&plugin, &text).await }
            },
            move |plugin, text, timeout_ms| {
                let output = toast_output.clone();
                async move { output.emit_mod_toast(&plugin, &text, timeout_ms).await }
            },
            move |plugin, text| {
                let output = status_output.clone();
                async move { output.emit_mod_status(&plugin, text.as_deref()).await }
            },
        )
        .await
    {
        tracing::warn!(%session_id, %error, "session.measure Mod dispatch failed");
    }
}

fn start_mod_session_event_worker() -> tokio::sync::mpsc::UnboundedSender<ModSessionEventWork> {
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(work) = receiver.recv().await {
            dispatch_queued_mod_session_event(work).await;
        }
    });
    sender
}

impl ConversationOrchestrator {
    fn enqueue_mod_session_events(&self, work: Vec<ModSessionEventWork>) {
        if work.is_empty() {
            return;
        }
        let mut slot = self
            .mod_session_event_sender
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut sender = slot
            .as_ref()
            .filter(|sender| !sender.is_closed())
            .cloned()
            .unwrap_or_else(start_mod_session_event_worker);
        let mut pending = std::collections::VecDeque::from(work);
        while let Some(item) = pending.pop_front() {
            if let Err(error) = sender.send(item) {
                tracing::warn!("Mod session-event queue stopped; restarting FIFO worker");
                sender = start_mod_session_event_worker();
                if let Err(restarted) = sender.send(error.0) {
                    tracing::error!(
                        "replacement Mod session-event worker stopped before accepting work"
                    );
                    pending.push_front(restarted.0);
                }
            }
        }
        *slot = Some(sender);
    }

    /// Observe a main-loop turn immediately before its first model request.
    /// Mods may inspect this event, but their answer cannot change the turn.
    pub(super) async fn fire_mod_turn_start(&self, text: &str, turn_id: &str) {
        *self
            .mod_turn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ModTurnFacts {
            id: turn_id.to_owned(),
            started_at: std::time::Instant::now(),
            answer: String::new(),
            usage: None,
            refusal: None,
            error: false,
        });
        let Some(registry) = &self.lifecycle_runtime.hook_registry else {
            return;
        };
        let Some(host) = registry.read().await.mod_host() else {
            return;
        };
        let input = serde_json::json!({"text":text,"turnId":turn_id});
        let log_output = self.output.clone();
        let toast_output = self.output.clone();
        let status_output = self.output.clone();
        let turn_id = turn_id.to_owned();
        if let Err(error) = host
            .dispatch_with_ui_at_session(
                "turn.start",
                input,
                self,
                move |_| {
                    let turn_id = turn_id.clone();
                    async move { Ok(serde_json::json!({"turnId":turn_id})) }
                },
                move |plugin, text| {
                    let output = log_output.clone();
                    async move { output.emit_mod_log(&plugin, &text).await }
                },
                move |plugin, text, timeout_ms| {
                    let output = toast_output.clone();
                    async move { output.emit_mod_toast(&plugin, &text, timeout_ms).await }
                },
                move |plugin, text| {
                    let output = status_output.clone();
                    async move { output.emit_mod_status(&plugin, text.as_deref()).await }
                },
            )
            .await
        {
            tracing::warn!(%error, "turn.start Mod dispatch failed");
        }
    }

    /// Accumulate one provider response for the eventual `turn.complete`.
    pub(crate) fn record_mod_turn_response(
        &self,
        message: &ConversationMessage,
        usage: Option<&llm_runtime::ExecutionUsage>,
        model: &str,
        stop_reason: Option<&str>,
        stop_details: Option<&llm_runtime::HistoryStopDetails>,
    ) {
        let ConversationMessage::Assistant { content, .. } = message else {
            return;
        };
        let answer = content
            .iter()
            .filter_map(lingxi_core::types::ContentBlock::visible_text)
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_owned();
        let mut held = self
            .mod_turn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(facts) = held.as_mut() else { return };
        facts.answer = answer;
        facts.error = false;
        facts.refusal = (stop_reason == Some("refusal")).then(|| {
            serde_json::json!({
                "category":stop_details.and_then(|details| details.category.as_deref()),
                "explanation":stop_details.and_then(|details| details.explanation.as_deref()),
            })
        });
        if let Some(usage) = usage {
            accumulate_mod_turn_usage(facts, usage, model);
        }
    }

    /// Count a completed physical Mod request immediately, even if the outer
    /// stream later fails before producing an assistant message.
    pub(crate) fn record_mod_turn_provider_usage(
        &self,
        usage: &llm_runtime::ExecutionUsage,
        model: &str,
    ) {
        if let Some(facts) = self
            .mod_turn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_mut()
        {
            accumulate_mod_turn_usage(facts, usage, model);
        }
    }

    pub(crate) fn mark_mod_turn_error(&self) {
        if let Some(facts) = self
            .mod_turn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_mut()
        {
            facts.error = true;
        }
    }

    /// Sample `session.measure` after this process observes a rate-limit state
    /// or raw utilization snapshot change. There is no polling fallback: API
    /// adapters without an update simply leave the turn-settled path intact.
    pub(crate) async fn notify_mod_session_measure_rate_limits_changed(&self) {
        let Some(registry) = &self.lifecycle_runtime.hook_registry else {
            return;
        };
        let Some(host) = registry.read().await.mod_host() else {
            return;
        };
        if !host.has_event("session.measure") {
            return;
        }

        let snapshot = self.mod_session_measure_snapshot().await;
        let sampler = self.mod_session_measure_sampler.clone();
        let Some(request) = sampler.enqueue(ModSessionMeasureReason::Limits, snapshot) else {
            return;
        };
        if let Some(session) = host.bound_session() {
            self.enqueue_mod_session_events(vec![ModSessionEventWork::Measure {
                context: ModSessionEventContext {
                    host,
                    session,
                    output: self.output.clone(),
                },
                sampler,
                request,
            }]);
        } else {
            dispatch_mod_session_measure_inline(host, self, self.output.clone(), sampler, request)
                .await;
        }
    }

    /// Deliver the settled main-loop turn to Mods. A changed nonblank result
    /// becomes a UI notice while the recorded assistant answer stays intact.
    pub(super) async fn fire_mod_turn_complete(&self, is_aborted: bool, has_error: bool) {
        // Resolve the host before consuming the lifecycle record so canceling
        // this future while it waits for the registry cannot lose a settled
        // turn.
        let host = if let Some(registry) = &self.lifecycle_runtime.hook_registry {
            registry.read().await.mod_host()
        } else {
            None
        };
        let session = host.as_ref().and_then(|host| host.bound_session());
        let facts = self
            .mod_turn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let Some(facts) = facts else { return };
        let Some(host) = host else { return };
        let reason = if is_aborted {
            "aborted"
        } else if facts.refusal.is_some() {
            "refusal"
        } else if has_error || facts.error {
            "error"
        } else {
            "answer"
        };
        let mut input = serde_json::json!({
            "answer":facts.answer,
            "durationMs":u64::try_from(facts.started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
            "isAborted":is_aborted,
            "turnId":facts.id,
            "reason":reason,
        });
        if let Some(refusal) = facts.refusal {
            input["refusal"] = refusal;
        }
        if let Some(usage) = facts.usage {
            input["usage"] = serde_json::json!({
                "input_tokens":usage.tokens.input_tokens,
                "output_tokens":usage.tokens.output_tokens,
                "cache_read_input_tokens":usage.tokens.cache_read_input_tokens,
                "cache_creation_input_tokens":usage.tokens.cache_creation_input_tokens,
                "model":usage.model,
            });
        }
        let original = input["answer"].as_str().unwrap_or_default().to_owned();
        let measure_snapshot = if host.has_event("session.measure") {
            Some(self.mod_session_measure_snapshot().await)
        } else {
            None
        };
        let sampler = self.mod_session_measure_sampler.clone();
        if let Some(session) = session {
            let context = ModSessionEventContext {
                host,
                session,
                output: self.output.clone(),
            };
            let mut work = vec![ModSessionEventWork::TurnComplete {
                context: context.clone(),
                input,
                original_answer: original,
            }];
            if let Some(snapshot) = measure_snapshot {
                work.push(ModSessionEventWork::MeasureRequest {
                    context,
                    sampler,
                    reason: ModSessionMeasureReason::Turn,
                    snapshot,
                });
            }
            self.enqueue_mod_session_events(work);
        } else {
            // Embedded callers may not have an owning session Arc. Preserve
            // their request-scoped dispatch behavior in that case.
            let measure_host = host.clone();
            dispatch_mod_turn_complete(host, self, self.output.clone(), input, original).await;
            if let Some(snapshot) = measure_snapshot {
                if let Some(request) = sampler.enqueue(ModSessionMeasureReason::Turn, snapshot) {
                    dispatch_mod_session_measure_inline(
                        measure_host,
                        self,
                        self.output.clone(),
                        sampler,
                        request,
                    )
                    .await;
                }
            }
        }
    }

    pub(super) async fn screen_mod_prompt_submit(
        &self,
        text: &str,
        images: &[lingxi_core::types::ImageSource],
        origin_override: Option<serde_json::Value>,
    ) -> ModPromptScreen {
        self.screen_mod_prompt_submit_projected(
            &lingxi_core::types::utf16_json::Utf16JsonProjection::plain(serde_json::json!(text)),
            images,
            origin_override,
        )
        .await
    }

    pub(super) async fn screen_mod_prompt_submit_projected(
        &self,
        text_projection: &lingxi_core::types::utf16_json::Utf16JsonProjection,
        images: &[lingxi_core::types::ImageSource],
        origin_override: Option<serde_json::Value>,
    ) -> ModPromptScreen {
        let text = text_projection
            .value
            .as_str()
            .expect("prompt text projection is a string");
        let origin = origin_override.unwrap_or_else(crate::mod_prompt_origin::current);
        let attachments: Vec<_> = images
            .iter()
            .map(|image| match image {
                lingxi_core::types::ImageSource::Base64 { media_type, .. } => {
                    serde_json::json!({"type":"image","mediaType":media_type})
                }
                lingxi_core::types::ImageSource::Url { .. } => {
                    serde_json::json!({"type":"image"})
                }
            })
            .collect();
        let mut input = serde_json::json!({"text":text,"wait":false,"origin":origin});
        if !attachments.is_empty() {
            input["attachments"] = serde_json::Value::Array(attachments);
        }
        let mut projected_input = lingxi_core::types::utf16_json::Utf16JsonProjection::plain(input);
        projected_input
            .set_field("text", text_projection.clone())
            .expect("valid prompt projection");
        let mut fallback = lingxi_core::types::utf16_json::Utf16JsonProjection::plain(
            serde_json::json!({"origin":origin}),
        );
        fallback
            .set_field("text", text_projection.clone())
            .expect("valid prompt projection");
        let result = if let Some(registry) = &self.lifecycle_runtime.hook_registry {
            if let Some(host) = registry.read().await.mod_host() {
                let log_output = self.output.clone();
                let toast_output = self.output.clone();
                let status_output = self.output.clone();
                host.dispatch_with_utf16_at_context(
                    "prompt.submit",
                    hooks::mods::ModUtf16ValueProjection::from_core_projection(projected_input)
                        .expect("valid prompt projection"),
                    &self.current_cwd(),
                    Some(self),
                    None,
                    None,
                    None,
                    lingxi_core::host::task_registry::FieldPresence::Missing,
                    |event| async move {
                        let event = event.into_core_projection()?;
                        let answer = event
                            .pick_object_fields(&["text", "origin", "context"])
                            .map_err(|error| hooks::mods::ModError::Hook(error.to_string()))?;
                        hooks::mods::ModUtf16ValueProjection::from_core_projection(answer)
                    },
                    move |plugin, text| {
                        let output = log_output.clone();
                        async move { output.emit_mod_log(&plugin, &text).await }
                    },
                    move |plugin, text, timeout_ms| {
                        let output = toast_output.clone();
                        async move { output.emit_mod_toast(&plugin, &text, timeout_ms).await }
                    },
                    move |plugin, text| {
                        let output = status_output.clone();
                        async move { output.emit_mod_status(&plugin, text.as_deref()).await }
                    },
                )
                .await
                .and_then(|outcome| {
                    hooks::mods::ModUtf16ValueProjection {
                        value: outcome.result,
                        strings: outcome.result_utf16_strings,
                        keys: outcome.result_utf16_keys,
                    }
                    .into_core_projection()
                })
                .unwrap_or_else(|error| {
                    tracing::warn!(%error, "prompt.submit Mod dispatch failed");
                    fallback.clone()
                })
            } else {
                fallback.clone()
            }
        } else {
            fallback.clone()
        };
        if let Some(reason) = result.value.get("drop").and_then(serde_json::Value::as_str) {
            return ModPromptScreen::Drop(reason.to_owned());
        }
        let text_projection = result
            .subprojection("/text")
            .ok()
            .filter(|projection| projection.value.is_string())
            .unwrap_or_else(|| text_projection.clone());
        let text = text_projection
            .value
            .as_str()
            .expect("prompt text projection is a string")
            .to_owned();
        let context = result
            .value
            .get("context")
            .and_then(serde_json::Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        ModPromptScreen::Admit {
            text,
            text_projection,
            context,
        }
    }

    /// Screen an accepted external delivery before it becomes a transcript
    /// message. The first `next(e)` to reach core wins, as in native `JDt`;
    /// a hook answer without `next` consumes the delivery even if it returns
    /// `{ text }`.
    pub(super) async fn screen_mod_session_receive(&self, text: &str, origin_kind: &str) -> bool {
        self.resolve_mod_session_receive(text, origin_kind, true)
            .await
            .is_some()
    }

    async fn screen_mod_uds_receive(
        &self,
        text: &str,
        commit: lingxi_core::host::uds_inbox::PeerQueueCommit,
    ) {
        let _ = self
            .resolve_mod_session_receive_with_queue(text, "peer", false, Some(commit))
            .await;
    }

    /// Screen a host-stamped external prompt while it is still in the input
    /// queue. The existing turn driver will persist the admitted text once.
    pub(super) async fn screen_mod_queued_receive(
        &self,
        text: &str,
        origin_kind: &str,
    ) -> Option<String> {
        self.resolve_mod_session_receive(text, origin_kind, false)
            .await
    }

    async fn resolve_mod_session_receive(
        &self,
        text: &str,
        origin_kind: &str,
        persist_at_core: bool,
    ) -> Option<String> {
        self.resolve_mod_session_receive_with_queue(text, origin_kind, persist_at_core, None)
            .await
    }

    async fn resolve_mod_session_receive_with_queue(
        &self,
        text: &str,
        origin_kind: &str,
        persist_at_core: bool,
        queue_commit: Option<lingxi_core::host::uds_inbox::PeerQueueCommit>,
    ) -> Option<String> {
        let host = if let Some(registry) = &self.lifecycle_runtime.hook_registry {
            registry.read().await.mod_host()
        } else {
            None
        };
        let Some(host) = host.filter(|host| host.has_event("session.receive")) else {
            if let Some(commit) = queue_commit {
                let _ = commit_peer_queue(commit, text.to_owned()).await;
            } else if persist_at_core {
                self.inject_user_text(text, true).await;
            }
            return Some(text.to_owned());
        };
        let origin = serde_json::json!({"kind":origin_kind});
        let input = serde_json::json!({"origin":origin,"text":text});
        // 0 = no core arrival, 1 = first core arrival, 2 = answered without
        // core. Closing state 0 when the hook answers prevents a late,
        // unawaited next(e) from injecting a supposedly consumed delivery.
        let state = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0));
        let state_for_core = state.clone();
        let admitted = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
        let admitted_for_core = admitted.clone();
        let commit_for_core = queue_commit.clone();
        let (done_tx, mut done_rx) = tokio::sync::watch::channel(false);
        let pinned_origin = origin.clone();
        let log_output = self.output.clone();
        let toast_output = self.output.clone();
        let status_output = self.output.clone();
        let result = host
            .dispatch_with_ui_at_session(
                "session.receive",
                input,
                self,
                move |forwarded| {
                    let pinned_origin = pinned_origin.clone();
                    let state = state_for_core.clone();
                    let admitted = admitted_for_core.clone();
                    let commit = commit_for_core.clone();
                    let done_tx = done_tx.clone();
                    async move {
                        if forwarded.get("origin") != Some(&pinned_origin)
                            || forwarded.get("event").is_some()
                            || forwarded.get("agentId").is_some()
                        {
                            return Err(hooks::mods::ModError::Hook(
                                "session.receive origin and event are pinned".into(),
                            ));
                        }
                        let rewritten = forwarded
                            .get("text")
                            .and_then(serde_json::Value::as_str)
                            .ok_or_else(|| {
                            hooks::mods::ModError::Hook(
                                "session.receive text must be a string".into(),
                            )
                        })?;
                        if state
                            .compare_exchange(
                                0,
                                1,
                                std::sync::atomic::Ordering::AcqRel,
                                std::sync::atomic::Ordering::Acquire,
                            )
                            .is_ok()
                        {
                            *admitted
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                Some(rewritten.to_owned());
                            let queued = if let Some(commit) = commit {
                                commit_peer_queue(commit, rewritten.to_owned()).await
                            } else {
                                true
                            };
                            if persist_at_core && queued {
                                self.inject_user_text(rewritten, true).await;
                            }
                            done_tx.send_replace(true);
                            if !queued {
                                return Err(hooks::mods::ModError::Hook(
                                    "session.receive delivery was not queued by inbound policy"
                                        .into(),
                                ));
                            }
                        } else if state.load(std::sync::atomic::Ordering::Acquire) == 2 {
                            return Err(hooks::mods::ModError::Hook(
                                "session.receive delivery was consumed before next(e)".into(),
                            ));
                        }
                        Ok(serde_json::json!({"text":rewritten}))
                    }
                },
                move |plugin, text| {
                    let output = log_output.clone();
                    async move { output.emit_mod_log(&plugin, &text).await }
                },
                move |plugin, text, timeout_ms| {
                    let output = toast_output.clone();
                    async move { output.emit_mod_toast(&plugin, &text, timeout_ms).await }
                },
                move |plugin, text| {
                    let output = status_output.clone();
                    async move { output.emit_mod_status(&plugin, text.as_deref()).await }
                },
            )
            .await;
        let reached_core = state
            .compare_exchange(
                0,
                2,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_err();
        if !reached_core {
            if let Err(error) = result {
                tracing::warn!(%error, "session.receive Mod dispatch failed");
                if persist_at_core {
                    self.inject_user_text(text, true).await;
                } else if let Some(commit) = queue_commit {
                    let _ = commit_peer_queue(commit, text.to_owned()).await;
                }
                return Some(text.to_owned());
            }
            return None;
        }
        if !*done_rx.borrow() && done_rx.changed().await.is_err() {
            tracing::warn!("session.receive core ended before confirming transcript persistence");
        }
        if let Err(error) = result {
            tracing::warn!(%error, "session.receive Mod dispatch failed after core arrival");
        }
        let answer = admitted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        answer
    }

    pub(super) async fn append_mod_prompt_context(&self, context: &[String]) {
        if context.is_empty() {
            return;
        }
        let exact_context: Vec<_> = context
            .iter()
            .cloned()
            .map(|text| hooks::ExactHookText::from_text(text))
            .collect();
        let tool_use_id = format!("hook-{}", lingxi_core::types::HookId::new().as_uuid());
        let attachment = hooks::additional_context_attachment(
            "prompt.submit",
            &tool_use_id,
            "UserPromptSubmit",
            &exact_context,
        );
        self.persist_exact_hook_attachment(attachment).await;
        let body = hooks::ExactHookText::join(&exact_context, "\n");
        let message = hooks::ExactHookText::wrapped(
            "<system-reminder>\nprompt.submit hook additional context: ",
            &body,
            "\n</system-reminder>",
        )
        .to_conversation_message(MessageId::new(), true);
        self.register_mod_persisted_attachment(
            &message,
            "hook_additional_context",
            serde_json::json!({"kind":"plugin","event":"prompt.submit"}),
        )
        .await;
        self.session.lock().await.history.push(message);
    }

    async fn persist_exact_hook_attachment(
        &self,
        projection: lingxi_core::types::utf16_json::Utf16JsonProjection,
    ) {
        let overrides = projection.string_overrides();
        self.persist_hook_attachment_to_jsonl(projection.value, overrides)
            .await;
    }

    pub(super) async fn emit_mod_prompt_drop(&self, reason: &str) {
        self.output
            .emit_text(&format!("Prompt dropped by a hook: {reason}"), None)
            .await;
    }
    pub(crate) async fn upsert_active_goal_stop_hook_for_session(
        &self,
        session_id: SessionId,
        condition: &str,
    ) {
        let hook = hooks::HookDefinition {
            id: lingxi_core::types::HookId::new(),
            name: GOAL_STOP_HOOK_NAME.to_string(),
            events: vec![hooks::HookEventType::Stop],
            if_condition: None,
            executor: hooks::HookExecutor::Prompt {
                prompt: condition.to_string(),
                model: None,
                continue_on_block: true,
            },
            source: hooks::HookSource::Session,
            blocking: true,
            timeout: Some(Duration::from_secs(GOAL_PROMPT_TIMEOUT_SECS)),
            priority: GOAL_STOP_HOOK_PRIORITY,
            once: false,
            status_message: None,
            async_rewake: false,
            async_timeout: None,
            rewake_message: None,
        };
        let _ = self
            .hooks
            .upsert_session_named_hook(session_id, GOAL_STOP_HOOK_NAME.to_string(), hook)
            .await;
    }

    pub(crate) async fn remove_active_goal_stop_hook_for_session(
        &self,
        session_id: SessionId,
    ) -> bool {
        self.hooks
            .remove_session_named_hook(session_id, GOAL_STOP_HOOK_NAME)
            .await
            .is_some()
    }

    pub async fn sync_active_goal_stop_hook_for_current_state(&self) {
        let (session_id, active_goal) = {
            let s = self.session.lock().await;
            (s.session_id, s.active_goal.clone())
        };
        match active_goal {
            Some(goal) => {
                self.upsert_active_goal_stop_hook_for_session(session_id, &goal.condition)
                    .await;
            }
            None => {
                let _ = self
                    .remove_active_goal_stop_hook_for_session(session_id)
                    .await;
            }
        }
        self.sync_goal_checkin_idle_task().await;
    }

    /// Tear the active goal down, reporting `reason` on `tengu_goal_cleared`.
    ///
    /// The reason is a parameter because upstream's is: `kB(goal, reason)`
    /// (`src_161508826.js`) is reached with six different values, and a
    /// context-limit teardown that reports `user_clear` is indistinguishable in
    /// the metric from somebody typing `/goal clear`.
    pub(crate) async fn clear_active_goal_state_and_hook(
        &self,
        reason: lingxi_core::host::GoalClearedReason,
    ) -> Option<lingxi_core::host::ActiveGoalSnapshot> {
        self.finish_active_goal_state_and_hook(
            lingxi_core::host::GoalStatusKind::Cleared,
            Some(reason),
        )
        .await
    }

    async fn finish_active_goal_state_and_hook(
        &self,
        status: lingxi_core::host::GoalStatusKind,
        cleared_reason: Option<lingxi_core::host::GoalClearedReason>,
    ) -> Option<lingxi_core::host::ActiveGoalSnapshot> {
        self.reset_goal_interruption();
        let (session_id, goal, cleared) = {
            let mut s = self.session.lock().await;
            let goal = s.active_goal.take();
            let cleared = goal
                .as_ref()
                .map(|goal| lingxi_core::host::ActiveGoalSnapshot {
                    condition: goal.condition.clone(),
                    set_at: goal.set_at,
                    last_reason: goal.last_reason.clone(),
                    iterations: goal.iterations,
                    tokens_at_start: goal.tokens_at_start,
                });
            (s.session_id, goal, cleared)
        };
        if let Some(goal) = goal.as_ref() {
            self.persist_goal_status_attachment(status, Some(goal))
                .await;
            self.fire_goal_terminal_event(status, goal, cleared_reason)
                .await;
        }
        let _ = self
            .remove_active_goal_stop_hook_for_session(session_id)
            .await;
        cleared
    }

    /// Internal turn driver (no telemetry — wrapped by `run_turn`).
    /// Build the lifecycle `HookContext` for this conversation (hooks B4),
    /// mirroring the `PreToolUse` context construction in `turn_loop.rs` plus
    /// the B4 additive fields.
    pub(super) async fn lifecycle_hook_ctx(&self, stop_hook_active: bool) -> HookContext {
        // FIX 2: populate `transcript_path` + `permission_mode` on the lifecycle
        // hook context, matching claude-code `createBaseHookInput` (always sets
        // `transcript_path`, utils/hooks.ts:322) and the live permission mode.
        // The lifecycle hooks (Stop /
        // UserPromptSubmit / SessionStart / …) thus carry a non-empty
        // `transcript_path` like the tool-use hooks do, instead of serializing `""`.
        //
        // FIX A: the path is the live JSONL writer's path when one is wired
        // (preserves the writer-backed tests) ELSE the deterministically-computed
        // `<config_home>/projects/<sanitize(cwd)>/<uuid>.jsonl` — because in
        // PRODUCTION no writer is wired, so the prior `unwrap_or_default()` left
        // `transcript_path` EMPTY for every lifecycle hook. `computed_transcript_path`
        // is itself `""` only when neither a writer nor a `config_home` is present
        // (library/test builds), preserving the old behavior there.
        let (session_id, plan_mode, last_assistant_message, model_selection) = {
            let s = self.session.lock().await;
            (
                s.session_id,
                s.plan_mode,
                Self::last_assistant_message_for_hooks(&s.history),
                hooks::HookModelSelection {
                    model: s.model.clone(),
                    model_profile: s.model_profile.clone(),
                },
            )
        };
        let transcript_path = self
            .transcript
            .jsonl_writer
            .as_ref()
            .map(|w| w.path().to_path_buf())
            .unwrap_or_else(|| self.computed_transcript_path(&session_id));
        // LingXi keeps `/plan` as session state, while Shift+Tab/control
        // requests mutate the enforcing permission gate. Prefer the explicit
        // plan state when active; otherwise report the gate's authoritative
        // live wire id instead of collapsing every non-plan mode to `default`.
        let permission_mode = if plan_mode {
            "plan".to_string()
        } else {
            self.permission_mode()
                .unwrap_or_else(|| "default".to_string())
        };
        // Main-thread lifecycle hooks (SessionStart / UserPromptSubmit / Stop /
        // expansion) carry the adopted `--agent`'s `agentType` — claude-code's
        // base hook-input builder `wf` uses `r?.agentType ?? MB()`, and these
        // firings have no tool-use context `r`, so they fall through to `MB()`
        // (the main-thread agent type). `None` when no `--agent` was applied.
        let agent_type = self.main_thread_agent_type().await;
        // `prompt_id` on the shared hook-input base (oracle `createBaseHookInput`
        // / minified `c_`: `prompt_id:Vut()??void 0`). `Vut()` is the process-wide
        // current prompt id — the same value stamped on the JSONL `user` lines and
        // on the OTel `prompt.id` attribute — and is `undefined` until the first
        // user input, which `current_prompt_id` reproduces exactly.
        let prompt_id = self.prompt_runtime.current_prompt_id.lock().await.clone();
        HookContext {
            model_safety_observer: self.model_safety_observer().await,
            prompt_transcript: Some(self.prompt_hook_transcript().await),
            model_selection: Some(model_selection),
            inherit: self.hook_agent_inheritance.clone(),
            agent_depth: Some(0),
            session_id,
            cwd: self.current_cwd(),
            transcript_path,
            prompt_id,
            permission_mode: Some(permission_mode),
            stop_hook_active,
            last_assistant_message,
            agent_type,
            ..Default::default()
        }
    }

    /// Snapshot at dispatch, rather than racing the asynchronous JSONL writer.
    pub(crate) async fn prompt_hook_transcript(&self) -> hooks::PromptHookTranscript {
        let (messages, message_grouping) = {
            let session = self.session.lock().await;
            (
                session.history.clone(),
                session.hook_message_grouping.clone(),
            )
        };
        let input = self
            .compaction_runtime
            .last_response_input_tokens
            .load(std::sync::atomic::Ordering::Relaxed);
        let output = self
            .compaction_runtime
            .last_response_output_tokens
            .load(std::sync::atomic::Ordering::Relaxed);
        hooks::PromptHookTranscript {
            messages,
            message_grouping,
            last_usage_tokens: usize::try_from(input.saturating_add(output)).unwrap_or(usize::MAX),
        }
    }

    fn last_assistant_message_for_hooks(
        history: &[lingxi_core::types::ConversationMessage],
    ) -> Option<String> {
        let assistant = history.iter().rev().find_map(|message| match message {
            lingxi_core::types::ConversationMessage::Assistant { content, .. } => Some(content),
            _ => None,
        })?;
        let joined = assistant
            .iter()
            .filter_map(lingxi_core::types::ContentBlock::visible_text)
            .collect::<Vec<_>>()
            .join("\n");
        let trimmed = joined.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    }

    /// Stamp the `Stop` / `SubagentStop` `background_tasks` + `session_crons`
    /// snapshot onto a lifecycle [`HookContext`] (claude-code's `m = s ? {
    /// background_tasks: Lic(s.taskRegistry.all()), session_crons: Mic() } :
    /// undefined`, spread `...m` after `last_assistant_message`).
    ///
    /// Called ONLY at the `Stop` / `SubagentStop` firings — never on other
    /// lifecycle hooks (`UserPromptSubmit`, expansion, …) — mirroring claude's
    /// gate on the tool-use context `s`. When a snapshot provider is wired BOTH
    /// fields become `Some(vec)` (possibly empty `[]`, which claude STILL emits
    /// when `s` is present); when no provider is wired both stay `None` and the
    /// executor omits the keys (claude `m = undefined`), keeping no-registry
    /// builds byte-identical.
    pub(crate) async fn populate_stop_hook_snapshot(
        &self,
        ctx: &mut HookContext,
    ) -> std::collections::HashMap<String, u64> {
        if let Some(provider) = self.lifecycle_runtime.stop_hook_snapshot.as_ref() {
            let (tasks, start_times) = provider.background_tasks_with_start_times().await;
            ctx.background_tasks = Some(tasks);
            ctx.session_crons = Some(provider.session_crons().await);
            return start_times;
        }
        std::collections::HashMap::new()
    }

    /// Public lifecycle [`HookContext`] for collaborators that fire engine
    /// hooks OUTSIDE the turn loop — notably the slash-command dispatcher,
    /// which fires `UserPromptExpansion` (#39) at command expansion and needs
    /// this conversation's `session_id` + `cwd` to populate the base hook input
    /// (claude-code minified `vd`). Same shape as the in-loop lifecycle hooks
    /// (`stop_hook_active = false`), so the dispatcher's payload matches the
    /// orchestrator's own lifecycle firings byte-for-byte on the shared fields.
    pub async fn expansion_hook_context(&self) -> HookContext {
        self.lifecycle_hook_ctx(false).await
    }

    /// Fire the `UserPromptSubmit` lifecycle hooks at prompt ingress (hooks B4,
    /// TS `executeUserPromptSubmitHooks` / `query.ts` prompt path). Returns
    /// `true` when a hook returned a `Block` decision, signalling the caller to
    /// ABORT the turn before any API call. Strict no-op (returns `false`) when
    /// no matching hook is registered, so existing flows are unaffected.
    ///
    /// Two `hookSpecificOutput` fields are applied here (P2-04), matching the
    /// binary's prompt-hook switch:
    /// - `sessionTitle` renames the session via the same custom-title write
    ///   path as `/rename` (claude `kje(title,"hook")`), applied for any
    ///   outcome (not only on block).
    /// - On a `Block`, a warning message is rendered to the output stream —
    ///   claude `dPs`/`Tc(...,"warning",void 0,!0)`:
    ///   `"UserPromptSubmit operation blocked by hook:\n{reason}"`, and unless
    ///   the hook set `suppressOriginalPrompt`, `"\n\nOriginal prompt: {prompt}"`
    ///   is appended. The reason defaults to `"Blocked by hook"` when the hook
    ///   omits one (`e.reason||"Blocked by hook"`). The message is display-only
    ///   (isMeta warning) — the turn still aborts (`shouldQuery:!1`).
    pub(super) async fn fire_user_prompt_submit(
        &self,
        prompt: &str,
        message_id: MessageId,
    ) -> bool {
        self.fire_user_prompt_submit_projected(
            &lingxi_core::types::utf16_json::Utf16JsonProjection::plain(serde_json::json!(prompt)),
            message_id,
        )
        .await
    }

    pub(super) async fn fire_user_prompt_submit_projected(
        &self,
        projection: &lingxi_core::types::utf16_json::Utf16JsonProjection,
        message_id: MessageId,
    ) -> bool {
        let prompt = projection
            .value
            .as_str()
            .expect("prompt projection is a string");
        // The JSONL append immediately before this seam minted the stable
        // per-turn prompt id. Emit once for all batched/streaming/cancelable
        // prompt paths before hooks can block the API call.
        let prompt_id = self
            .prompt_runtime
            .current_prompt_id
            .lock()
            .await
            .clone()
            .unwrap_or_else(|| message_id.as_uuid().to_string());
        telemetry::otel::emit_user_prompt_log(
            prompt,
            &prompt_id,
            &message_id.as_uuid().to_string(),
        );
        self.append_ultracode_attachments(prompt).await;
        let ctx = self.lifecycle_hook_ctx(false).await;
        let agg = self
            .hooks
            .execute(
                HookEvent::UserPromptSubmit {
                    prompt: prompt.to_string(),
                    prompt_projection: Some(projection.clone()),
                },
                ctx,
            )
            .await;

        // `hookSpecificOutput.sessionTitle` — rename the session (claude
        // applies it regardless of the block outcome). Best-effort: no writer
        // wired (library/test callers) ⇒ silent no-op, an empty title is
        // ignored.
        if let Some(title) = agg.session_title.as_deref() {
            if !title.is_empty() {
                if let Some(writer) = self.transcript.jsonl_writer.as_ref() {
                    let session_id = self.session.lock().await.session_id;
                    if let Err(error) = writer
                        .append_custom_title(&session_id.as_uuid().to_string(), title)
                        .await
                    {
                        self.record_transcript_append_failure(
                            &session_id.to_string(),
                            "hook_custom_title",
                            &error,
                        )
                        .await;
                    }
                }
            }
        }

        let blocked = matches!(agg.decision, Some(hooks::response::HookDecision::Block));
        if blocked {
            let reason = agg
                .reason
                .clone()
                .unwrap_or_else(|| "Blocked by hook".to_string());
            let base = format!("UserPromptSubmit operation blocked by hook:\n{reason}");
            let warning = if agg.suppress_original_prompt {
                base
            } else {
                format!("{base}\n\nOriginal prompt: {prompt}")
            };
            self.output.emit_text(&warning, None).await;
        }
        blocked
    }

    /// Fire the `MessageDisplay` hooks at the BEGIN of an assistant-message
    /// stream — the orchestrator twin of claude-code's stream-display state
    /// machine `begin(d)` (BIN off 208862320), which, on each new assistant
    /// message, checks the `MessageDisplay` gate and then initializes the
    /// per-message flush state `o={apiMessageId:d, messageId:randomUUID(),
    /// turnId:r, index:0, ...}`. The hook payload is built by `aAt` (BIN off
    /// 205705090): `{hook_event_name:"MessageDisplay", turn_id:e.turnId,
    /// message_id:e.messageId, index:e.index, final:e.final, delta:e.delta}`.
    ///
    /// LingXi's streaming pump emits text deltas through the `OutputStream`
    /// rather than re-entering a hook-aware flush loop, so we fire ONCE per
    /// assistant message at its begin (the single hook-reachable point that
    /// owns the executor + the pre-allocated assistant id), mirroring the
    /// `begin(d)` initialization with `index:0`, `final:false`, and an empty
    /// `delta` (no delta text has streamed yet at begin). `turn_id` is a fresh
    /// per-turn UUID minted here (claude-code `newTurn(){…; r=randomUUID()}`),
    /// and `message_id` is the assistant message's bare UUID (no `msg:` prefix,
    /// matching the binary's bare `randomUUID()` display id and the inner
    /// Anthropic `message.id` formatting used by `persist_assistant_per_block`).
    ///
    /// Best-effort: the aggregate is discarded so a failing / blocking
    /// `MessageDisplay` hook never affects the turn, and firing is a strict
    /// no-op when no `MessageDisplay` hook is registered.
    pub(super) async fn fire_message_display(&self, turn_id: &str, assistant_id: MessageId) {
        let ctx = self.lifecycle_hook_ctx(false).await;
        let _ = self
            .hooks
            .execute(
                HookEvent::MessageDisplay {
                    turn_id: turn_id.to_string(),
                    message_id: assistant_id.as_uuid().to_string(),
                    index: 0,
                    is_final: false,
                    delta: String::new(),
                },
                ctx,
            )
            .await;
    }

    /// Fire the `MessageDisplay` completed-message pass and return the hook's
    /// display override, if any — the orchestrator twin of claude-code's `Qff`
    /// (BIN off 229876575). After an assistant message's text blocks finalize,
    /// claude-code fires `MessageDisplay` once with `{turnId, messageId:
    /// randomUUID(), index:0, final:!0, delta:<joined text>}` and folds the last
    /// `displayContent` any hook returns into the message's separate
    /// `displayedMessageContent` (the stored `message.content` is NEVER touched —
    /// "Display-only"). On a hook exception it logs
    /// `MessageDisplay hook failed for completed message; emitting original text:`
    /// and displays the original.
    ///
    /// `message_id` is a fresh per-pass UUID (claude-code `zfn.randomUUID()`),
    /// distinct from the assistant message's own id. Returns `Some(text)` when a
    /// hook supplied `displayContent`, else `None` (caller displays the original).
    /// Best-effort: a failing / blocking hook yields `None`, never affecting the
    /// stored message or the turn.
    pub(super) async fn fire_message_display_completed(
        &self,
        turn_id: &str,
        text: &str,
    ) -> Option<String> {
        let ctx = self.lifecycle_hook_ctx(false).await;
        let agg = self
            .hooks
            .execute(
                HookEvent::MessageDisplay {
                    turn_id: turn_id.to_string(),
                    message_id: uuid::Uuid::new_v4().to_string(),
                    index: 0,
                    is_final: true,
                    delta: text.to_string(),
                },
                ctx,
            )
            .await;
        // claude-code's `try { … } catch(c) { log; return original }` — a
        // `MessageDisplay` hook that errored (non-zero exit / transport failure /
        // timeout) never overrides the display: log the byte-exact fallback and
        // fall through to the original text.
        if agg.display_content.is_none()
            && agg.all_results.iter().any(|(_, r)| {
                matches!(
                    r.outcome,
                    hooks::response::HookOutcome::Error | hooks::response::HookOutcome::Timeout
                )
            })
        {
            tracing::warn!(
                "MessageDisplay hook failed for completed message; emitting original text: {text}"
            );
        }
        agg.display_content
    }

    /// Fire the `Stop` lifecycle hooks at end-of-turn and classify the result
    /// (hooks B4, TS `handleStopHooks` + `query.ts:1267-1306`).
    ///
    /// `stop_hook_active` carries the re-entry flag into the hook payload AND
    /// gates the continuation guard: a `Block` when already active becomes
    /// [`StopHookDisposition::Pass`] so a perpetually-blocking Stop hook cannot
    /// loop forever. `prevent_continuation` (`continue:false`) always wins →
    /// [`StopHookDisposition::Prevent`]. Strict no-op (`Pass`) when no Stop hook
    /// is registered.
    async fn fire_stop_hooks(
        &self,
        reason: &str,
        stop_hook_active: bool,
        parent_aborted: bool,
    ) -> StopHookDisposition {
        let evaluation_started = std::time::Instant::now();
        tracing::debug!(event = "hook_stop_started", reason, stop_hook_active);
        let mut ctx = self.lifecycle_hook_ctx(stop_hook_active).await;
        let goal_hook_id = self
            .hooks
            .get_session_named_hook(ctx.session_id, GOAL_STOP_HOOK_NAME)
            .await
            .map(|hook| hook.id);
        // claude-code stamps `background_tasks` + `session_crons` onto the Stop
        // payload whenever the tool-use context is present (`...m`). The
        // orchestrator's main-loop Stop firing always runs inside a tool-use
        // context, so populate the snapshot here (and ONLY here / SubagentStop).
        let start_times = self.populate_stop_hook_snapshot(&mut ctx).await;
        // 2.1.270 $Qn removes the goal hook while background work runs.
        // Exclude it before dispatch so neither an evaluator API call nor its
        // blocking result can leak through the generic Stop aggregate.
        let goal_before = { self.session.lock().await.active_goal.clone() };
        let mut deferring_tasks = Self::build_deferring_goal_checkin_tasks(
            ctx.background_tasks.as_deref().unwrap_or(&[]),
        );
        for task in &mut deferring_tasks {
            task.start_time_ms = start_times.get(&task.id).copied();
        }
        let (goal_deferred, checkin_emitted) = self.goal_checkin_pass_tasks(&deferring_tasks).await;
        let agg = self
            .hooks
            .execute_excluding_hook(
                HookEvent::Stop {
                    reason: reason.to_string(),
                },
                ctx,
                goal_hook_id.filter(|_| goal_deferred),
            )
            .await;
        let (goal_disposition, goal_outcome) = if goal_deferred {
            (
                checkin_emitted.then(|| StopHookDisposition::Continue(String::new())),
                GoalEvalOutcome::Deferred,
            )
        } else {
            self.goal_stop_hook_disposition(goal_hook_id, &agg, parent_aborted)
                .await
        };
        if matches!(
            goal_outcome,
            GoalEvalOutcome::Met
                | GoalEvalOutcome::NotMet
                | GoalEvalOutcome::Impossible
                | GoalEvalOutcome::Deferred
        ) {
            self.reset_goal_interruption();
        }
        if let Some(goal) = goal_before.as_ref() {
            self.fire_goal_evaluated(
                goal,
                goal_outcome,
                parent_aborted,
                &deferring_tasks,
                evaluation_started.elapsed(),
            )
            .await;
        }
        let disposition = if agg.prevent_continuation {
            // FIX C: carry the hook's `stopReason` (parsed into `agg.reason`,
            // `hook_payload.rs:1113`) so `handle_stop_at_end` can persist the
            // `hook_stopped_continuation` meta message. Default matches TS
            // `query/stopHooks.ts:271` (`result.stopReason || 'Stop hook prevented
            // continuation'`).
            let reason = agg
                .reason
                .clone()
                .unwrap_or_else(|| "Stop hook prevented continuation".to_string());
            StopHookDisposition::Prevent(reason)
        } else if let Some(disposition) = goal_disposition {
            disposition
        } else if matches!(agg.decision, Some(hooks::response::HookDecision::Block)) {
            // #2: ANY Block yields Continue — the consecutive-block CAP is no
            // longer the old `!stop_hook_active` boolean (which let a blocking
            // hook drive exactly ONE extra turn). The binary carries a
            // `stopHookBlockingCount` and only ends after it exceeds
            // `LINGXI_STOP_HOOK_BLOCK_CAP` (default 8); that counter cap is
            // enforced in `handle_stop_at_end`, not here. `stop_hook_active` is
            // still threaded into the hook CONTEXT above so the hook can read it
            // and return success while it is true (the documented escape hatch).
            //
            // Source the continuation from the hook's blocking REASON
            // (`blockingError.blockingError`), NOT the transcript-only
            // `system_messages`. `agg.reason` is `None` when the hook omits a
            // reason, so apply claude-code's default (`utils/hooks.ts:533`,
            // `json.reason || 'Blocked by hook'`).
            let reason = agg
                .reason
                .clone()
                .unwrap_or_else(|| "Blocked by hook".to_string());
            StopHookDisposition::Continue(reason)
        } else {
            StopHookDisposition::Pass
        };
        tracing::debug!(
            event = "hook_stop_completed",
            prevent_continuation = agg.prevent_continuation,
            blocked = matches!(agg.decision, Some(hooks::response::HookDecision::Block)),
        );
        disposition
    }

    /// REM-09 — the goal-deferral pass at end of turn, and the goal check-in
    /// interstitial it emits.
    ///
    /// Returns `true` when the goal's Stop-hook evaluation must be SKIPPED this
    /// turn because background work is still running (oracle @292174788:
    /// `if(U.length>0){…sessionHooksRegistry.remove(zt(),"Stop",y)…}`).
    ///
    /// * DEFERRING SET — `_qf(taskRegistry.all())` @292181184, applied to the
    ///   `background_tasks` snapshot the Stop payload already carries (which is
    ///   itself `Lic(taskRegistry.all())`, i.e. the same registry, already
    ///   filtered to `running`/`pending` and already type-labelled). Kept: the
    ///   labels in [`crate::prompt::goal_checkin::DEFERRING_TASK_LABELS`], minus
    ///   the `main-session` subagent the oracle excludes by name.
    /// * NOTHING DEFERRING — the `else if(L.deferredSince!==void 0){…}` arm:
    ///   drop the deferral bookkeeping and evaluate the goal normally.
    /// * CHECK-IN — [`crate::prompt::goal_checkin::GoalDeferralState::advance`]
    ///   (`wzf`/`Tzf`); the body is appended as a plain `isMeta` user message
    ///   (`kn({content:Z,isMeta:!0})`), NOT `<system-reminder>`-wrapped — the
    ///   interstitial is not part of the attachment family.
    ///
    /// LIVE by default: the upstream gate `tengu_saffron_wren` defaults TRUE and
    /// the port has no GrowthBook, so only `CLAUDE_CODE_GOAL_CHECKIN_MINUTES=0`
    /// turns it off. It is still a strict no-op in any session with no active
    /// goal, which is the overwhelmingly common case.
    fn build_deferring_goal_checkin_tasks(
        background_tasks: &[hooks::HookBackgroundTask],
    ) -> Vec<crate::prompt::goal_checkin::DeferringTask> {
        background_tasks
            .iter()
            .filter(|t| {
                crate::prompt::goal_checkin::DEFERRING_TASK_LABELS.contains(&t.r#type.as_str())
                    && t.agent_type.as_deref() != Some("main-session")
                    && !(t.r#type == "teammate" && t.is_idle)
            })
            .map(|t| crate::prompt::goal_checkin::DeferringTask {
                id: t.id.clone(),
                start_time_ms: None,
                label: t.r#type.clone(),
                detail: t
                    .command
                    .clone()
                    .filter(|c| !c.is_empty())
                    .unwrap_or_else(|| t.description.clone()),
            })
            .collect()
    }

    #[cfg(test)]
    pub(super) async fn goal_checkin_pass(
        &self,
        background_tasks: &[hooks::HookBackgroundTask],
    ) -> bool {
        let deferring = Self::build_deferring_goal_checkin_tasks(background_tasks);
        self.goal_checkin_pass_tasks(&deferring).await.0
    }

    async fn goal_checkin_pass_tasks(
        &self,
        deferring: &[crate::prompt::goal_checkin::DeferringTask],
    ) -> (bool, bool) {
        let Some(goal) = ({
            let session = self.session.lock().await;
            session.active_goal.clone()
        }) else {
            self.lifecycle_runtime
                .goal_checkin
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clear();
            self.sync_goal_checkin_idle_task().await;
            return (false, false);
        };

        if deferring.is_empty() {
            self.lifecycle_runtime
                .goal_checkin
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clear();
            self.sync_goal_checkin_idle_task().await;
            return (false, false);
        }

        // Captured BEFORE `advance`, which resets `deferred_since` to now when
        // it fires — `Jer`'s `deferredMs` is the stretch that just ended.
        let deferred_since_before = self
            .lifecycle_runtime
            .goal_checkin
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .deferred_since;
        let checkin = Self::advance_goal_checkin_state(
            &self.lifecycle_runtime.goal_checkin,
            &goal.condition,
            deferring,
            crate::prompt::goal_checkin::checkin_interval_ms(),
        );
        self.sync_goal_checkin_idle_task().await;
        let emitted = checkin.is_some();
        if let Some(body) = checkin {
            let (checkin_count, idle_checkin_count) = {
                let state = self
                    .lifecycle_runtime
                    .goal_checkin
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                (state.checkin_count, state.idle_checkin_count)
            };
            let now_ms = tool_api::read_file_state::mtime_ms_floor(std::time::SystemTime::now());
            Self::fire_goal_checkin_injected(
                self.model_runtime.analytics_bus.as_ref(),
                "turn_end",
                deferred_since_before.map_or(0, |since| now_ms.saturating_sub(since)),
                deferring,
                checkin_count,
                idle_checkin_count,
            )
            .await;
            let msg = ConversationMessage::user_meta(MessageId::new(), body);
            {
                let mut session = self.session.lock().await;
                session.history.push(msg.clone());
            }
            self.persist_message_to_jsonl(&msg).await;
        }
        (true, emitted)
    }

    /// The terminal `tengu_goal_*` analytics events (2.1.266):
    ///
    /// * achieved — `i("tengu_goal_achieved",{promptLength,iterations,durationMs,tokens,origin})`
    ///   (@4211700);
    /// * impossible — `i("tengu_goal_failed",{promptLength,reasonLength,iterations,durationMs,tokens,origin})`
    ///   (@4211500);
    /// * cleared — `eB(goal, reason)` = `i("tengu_goal_cleared",{reason,iterations,durationMs,origin})`
    ///   (`src_160454523.js`), whose `reason` discriminates `user_clear` from
    ///   `superseded` / `context_limit` / `api_error`.
    ///
    /// ## Divergence (reason)
    ///
    /// `origin` comes off the goal. Upstream resolves it in `y(e,t)`
    /// (`src_161508826.js`), which returns a `queuedGoalOrigin` staged by
    /// `ProposeGoal` and otherwise the literal `"user"` — so it is never absent.
    /// `ProposeGoal`'s two spellings are unreachable here (LingXi does not ship
    /// that tool; see the accepted-divergence register), but `"restored"` is
    /// not: `mon` stamps it on every goal recovered by a resume.
    pub(crate) async fn fire_goal_terminal_event(
        &self,
        status: lingxi_core::host::GoalStatusKind,
        goal: &lingxi_core::session::ActiveGoalState,
        cleared_reason: Option<lingxi_core::host::GoalClearedReason>,
    ) {
        let Some(bus) = self.model_runtime.analytics_bus.as_ref() else {
            return;
        };
        let (event, reason) = match status {
            lingxi_core::host::GoalStatusKind::Achieved => ("tengu_goal_achieved", None),
            lingxi_core::host::GoalStatusKind::Failed => ("tengu_goal_failed", None),
            lingxi_core::host::GoalStatusKind::Cleared => (
                "tengu_goal_cleared",
                // `kB`'s second argument. A `Cleared` teardown always carries
                // one; the fallback keeps this total rather than silently
                // dropping `reason` from the event.
                Some(
                    cleared_reason
                        .unwrap_or(lingxi_core::host::GoalClearedReason::UserClear)
                        .as_str(),
                ),
            ),
            // `Set` never tears a goal down, and `NotMet` leaves it running.
            lingxi_core::host::GoalStatusKind::Set | lingxi_core::host::GoalStatusKind::NotMet => {
                return;
            }
        };
        let duration_ms = std::time::SystemTime::now()
            .duration_since(goal.set_at)
            .unwrap_or_default()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64;
        let mut metadata = telemetry::LogEventMetadata::new();
        if let Some(reason) = reason {
            metadata.insert(
                "reason".into(),
                telemetry::AnalyticsValue::String(reason.to_string()),
            );
        } else {
            metadata.insert(
                "promptLength".into(),
                telemetry::AnalyticsValue::Int(goal.condition.encode_utf16().count() as i64),
            );
            let tokens = self
                .snapshot_cost_real()
                .await
                .total_tokens
                .saturating_sub(goal.tokens_at_start);
            metadata.insert(
                "tokens".into(),
                telemetry::AnalyticsValue::Int(tokens as i64),
            );
            if event == "tengu_goal_failed" {
                metadata.insert(
                    "reasonLength".into(),
                    telemetry::AnalyticsValue::Int(
                        goal.last_reason
                            .as_ref()
                            .map_or(0, |reason| reason.encode_utf16().count())
                            as i64,
                    ),
                );
            }
        }
        metadata.insert(
            "iterations".into(),
            telemetry::AnalyticsValue::Int(goal.iterations as i64),
        );
        metadata.insert(
            "durationMs".into(),
            telemetry::AnalyticsValue::Int(duration_ms as i64),
        );
        metadata.insert(
            "origin".into(),
            telemetry::AnalyticsValue::String(goal.origin.as_str().to_string()),
        );
        bus.log_event(event, metadata).await;
    }

    /// `i("tengu_goal_evaluated", …)` — the evaluator's `finally`
    /// (`src_163219561.js` @4343091).
    ///
    /// ```js
    /// if (Fe) { let Qe = p.abortController.signal.aborted,
    ///           Je = We==="met"||We==="not_met"||We==="impossible";
    ///   i("tengu_goal_evaluated",{ outcome:u(We ?? (_e?"error":Qe?"cancelled":"absent")),
    ///     durationMs:Date.now()-D, iterations:Fe.iterations+(Je?1:0),
    ///     parentAborted:Qe, origin:we(Fe.origin), ...Xe }) }
    /// ```
    ///
    /// `...Xe` is NOT a general spread: it is set in exactly one branch — the
    /// deferred one — as `{activeAgents, activeShells}`, and contributes nothing
    /// anywhere else. The split is the same one
    /// [`Self::fire_goal_checkin_injected`] already makes off
    /// [`crate::prompt::goal_checkin::DeferringTask::label`].
    ///
    /// 2.1.270 `$Qn` starts `D` at Stop-handler entry. Model-query latency
    /// is excluded from this duration.
    ///
    /// Upstream captures the goal as `Fe = p.agentId ? void 0 : activeGoal`, so
    /// a subagent never emits this event. That gate is vacuous here: this
    /// orchestrator does not model subagent identity (see
    /// [`Self::fire_stop_hook_block_count`]) and only ever runs the main thread.
    ///
    /// `origin` is the goal's, for the reason recorded on
    /// [`Self::fire_goal_terminal_event`].
    pub(super) async fn fire_goal_evaluated(
        &self,
        goal: &lingxi_core::session::ActiveGoalState,
        outcome: GoalEvalOutcome,
        parent_aborted: bool,
        deferring_tasks: &[crate::prompt::goal_checkin::DeferringTask],
        duration: Duration,
    ) {
        let Some(bus) = self.model_runtime.analytics_bus.as_ref() else {
            return;
        };
        let mut metadata = telemetry::LogEventMetadata::new();
        metadata.insert(
            "outcome".into(),
            telemetry::AnalyticsValue::String(outcome.as_str().to_string()),
        );
        metadata.insert(
            "durationMs".into(),
            telemetry::AnalyticsValue::Int(duration.as_millis().min(i64::MAX as u128) as i64),
        );
        metadata.insert(
            "iterations".into(),
            telemetry::AnalyticsValue::Int(
                goal.iterations
                    .saturating_add(u64::from(outcome.counts_as_iteration()))
                    as i64,
            ),
        );
        metadata.insert(
            "parentAborted".into(),
            telemetry::AnalyticsValue::Bool(parent_aborted),
        );
        metadata.insert(
            "origin".into(),
            telemetry::AnalyticsValue::String(goal.origin.as_str().to_string()),
        );
        // `...Xe` — present ONLY on the deferred branch, where upstream sets
        // `Xe = {activeAgents: j(An, A9t), activeShells: j(An, v9t)}`. Both
        // counts run over the SAME already-filtered list (`Ofr`), split by two
        // disjoint predicates: `v9t` is `type === "local_bash"` (this port's
        // `"shell"`) and `A9t` is the four agent-ish types. Counted by predicate
        // rather than as `len - shells`, so a label neither side claims shows up
        // as a missing count instead of silently inflating `activeAgents`.
        if outcome == GoalEvalOutcome::Deferred {
            let shells = deferring_tasks
                .iter()
                .filter(|t| t.label == "shell")
                .count();
            let agents = deferring_tasks
                .iter()
                .filter(|t| {
                    crate::prompt::goal_checkin::GOAL_EVAL_AGENT_TASK_LABELS
                        .contains(&t.label.as_str())
                })
                .count();
            metadata.insert(
                "activeShells".into(),
                telemetry::AnalyticsValue::Int(shells as i64),
            );
            metadata.insert(
                "activeAgents".into(),
                telemetry::AnalyticsValue::Int(agents as i64),
            );
        }
        bus.log_event("tengu_goal_evaluated", metadata).await;
    }

    /// `Jer(trigger, deferredMs, goal, tasks)` (2.1.266 @4168800) —
    /// `i("tengu_goal_checkin_injected",{trigger,deferredMs,activeShells,activeAgents,checkinCount,idleCheckinCount})`.
    ///
    /// `activeShells` is upstream's `W(tasks, E4t)` (the `local_bash` half),
    /// `activeAgents` the remainder — here read off the already-projected
    /// [`crate::prompt::goal_checkin::DeferringTask::label`].
    pub(super) async fn fire_goal_checkin_injected(
        bus: Option<&Arc<telemetry::AnalyticsBus>>,
        trigger: &'static str,
        deferred_ms: i64,
        tasks: &[crate::prompt::goal_checkin::DeferringTask],
        checkin_count: u32,
        idle_checkin_count: u32,
    ) {
        let Some(bus) = bus else {
            return;
        };
        let active_shells = tasks.iter().filter(|t| t.label == "shell").count();
        let mut metadata = telemetry::LogEventMetadata::new();
        metadata.insert(
            "trigger".into(),
            telemetry::AnalyticsValue::String(trigger.to_string()),
        );
        metadata.insert(
            "deferredMs".into(),
            telemetry::AnalyticsValue::Int(deferred_ms),
        );
        metadata.insert(
            "activeShells".into(),
            telemetry::AnalyticsValue::Int(active_shells as i64),
        );
        metadata.insert(
            "activeAgents".into(),
            telemetry::AnalyticsValue::Int((tasks.len() - active_shells) as i64),
        );
        metadata.insert(
            "checkinCount".into(),
            telemetry::AnalyticsValue::Int(i64::from(checkin_count)),
        );
        metadata.insert(
            "idleCheckinCount".into(),
            telemetry::AnalyticsValue::Int(i64::from(idle_checkin_count)),
        );
        bus.log_event("tengu_goal_checkin_injected", metadata).await;
    }

    fn advance_goal_checkin_state(
        goal_checkin: &Arc<std::sync::Mutex<crate::prompt::goal_checkin::GoalDeferralState>>,
        condition: &str,
        deferring: &[crate::prompt::goal_checkin::DeferringTask],
        interval_ms: i64,
    ) -> Option<String> {
        let now_ms = tool_api::read_file_state::mtime_ms_floor(std::time::SystemTime::now());
        goal_checkin
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .advance(condition, deferring, now_ms, interval_ms)
    }

    pub(super) async fn run_goal_checkin_idle_loop(
        provider: Arc<dyn crate::stop_hook_snapshot::StopHookSnapshotProvider>,
        owner: Option<std::sync::Weak<ConversationOrchestrator>>,
        session: Arc<Mutex<SessionState>>,
        turn_gate: Arc<Mutex<()>>,
        goal_checkin: Arc<std::sync::Mutex<crate::prompt::goal_checkin::GoalDeferralState>>,
        running: Arc<std::sync::atomic::AtomicBool>,
        generation_counter: Arc<std::sync::atomic::AtomicU64>,
        generation: u64,
        analytics_bus: Option<Arc<telemetry::AnalyticsBus>>,
    ) {
        struct RunningGuard {
            running: Arc<std::sync::atomic::AtomicBool>,
            generation_counter: Arc<std::sync::atomic::AtomicU64>,
            generation: u64,
        }

        impl Drop for RunningGuard {
            fn drop(&mut self) {
                if self
                    .generation_counter
                    .load(std::sync::atomic::Ordering::Acquire)
                    == self.generation
                {
                    self.running
                        .store(false, std::sync::atomic::Ordering::Release);
                }
            }
        }

        let _guard = RunningGuard {
            running,
            generation_counter,
            generation,
        };

        loop {
            let base_interval_ms = crate::prompt::goal_checkin::checkin_interval_ms();
            if base_interval_ms <= 0 {
                return;
            }
            // 2.1.266 `HZ`: `min($S, max(qmt, backoff - elapsed))`. The 60s
            // floor is what stops a stretch that has already outrun its
            // interval from spinning the timer, and once the idle cap is
            // reached the elapsed term drops out so the re-arm is a full
            // interval ahead.
            let sleep_ms = {
                let state = goal_checkin
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let Some(deferred_since) = state.deferred_since else {
                    return;
                };
                let now_ms =
                    tool_api::read_file_state::mtime_ms_floor(std::time::SystemTime::now());
                crate::prompt::goal_checkin::next_idle_delay_ms(
                    base_interval_ms,
                    state.checkin_count,
                    state.idle_checkin_count,
                    now_ms,
                    deferred_since,
                )
            };
            tokio::time::sleep(Duration::from_millis(sleep_ms.max(1) as u64)).await;

            // `if(RUe(C)){t("[goal] idle check-in cap reached — tick re-armed
            // only"),HZ({...}); return}` — read AFTER the sleep, because a user
            // message during it clears the budget (`vSt`) and re-enables
            // delivery. Upstream also skips a tick when a passive
            // task-notification is already queued (`Ins`/`rnr`); LingXi's idle
            // path has no message queue to peek at, so that skip has no home.
            if crate::prompt::goal_checkin::idle_cap_reached(
                goal_checkin
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .idle_checkin_count,
            ) {
                tracing::debug!("[goal] idle check-in cap reached \u{2014} tick re-armed only");
                continue;
            }

            // Serialize idle emission with every public turn. Besides keeping
            // the history order deterministic, this prevents the background
            // append and a foreground append from reading the same JSONL parent
            // and creating a split chain.
            let Ok(_turn_guard) = turn_gate.try_lock() else {
                continue;
            };

            let Some(goal) = ({
                let locked = session.lock().await;
                locked.active_goal.clone()
            }) else {
                goal_checkin
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clear();
                return;
            };
            let (tasks, start_times) = provider.background_tasks_with_start_times().await;
            let mut deferring = Self::build_deferring_goal_checkin_tasks(&tasks);
            for task in &mut deferring {
                task.start_time_ms = start_times.get(&task.id).copied();
            }
            let Some(orch) = owner.as_ref().and_then(std::sync::Weak::upgrade) else {
                if deferring.is_empty() {
                    goal_checkin
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .clear();
                }
                return;
            };
            // If admission is busy, retry without spending an idle check-in.
            let state_before = goal_checkin
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            // Captured before `advance` resets the stretch — see the
            // turn-end twin.
            let deferred_since_before = goal_checkin
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .deferred_since;
            let body = if deferring.is_empty() {
                {
                    let mut state = goal_checkin
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    state.checkin_count = state.checkin_count.saturating_add(1);
                }
                Some(crate::prompt::goal_checkin::build_checkin_body(
                    &goal.condition,
                    tool_api::read_file_state::mtime_ms_floor(std::time::SystemTime::now())
                        .saturating_sub(deferred_since_before.unwrap_or(0)),
                    &deferring,
                ))
            } else {
                Self::advance_goal_checkin_state(
                    &goal_checkin,
                    &goal.condition,
                    &deferring,
                    base_interval_ms,
                )
            };
            if let Some(body) = body {
                // `p(RUe(V)?Mns(re):re)` — `V` is the goal AFTER the increment,
                // so the delivery that REACHES the cap is the one that carries
                // the pause suffix, and no further delivery follows it.
                let idle_checkin_count = {
                    let mut state = goal_checkin
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    state.idle_checkin_count = state.idle_checkin_count.saturating_add(1);
                    state.idle_checkin_count
                };
                let body = if crate::prompt::goal_checkin::idle_cap_reached(idle_checkin_count) {
                    crate::prompt::goal_checkin::with_idle_pause_suffix(&body)
                } else {
                    body
                };
                let checkin_count = goal_checkin
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .checkin_count;
                let now_ms =
                    tool_api::read_file_state::mtime_ms_floor(std::time::SystemTime::now());
                if !orch.queue_goal_checkin(body, &goal).await {
                    let current = session.lock().await;
                    if !current.active_goal.as_ref().is_some_and(|active| {
                        active.set_at == goal.set_at && active.condition == goal.condition
                    }) {
                        return;
                    }
                    *goal_checkin
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = state_before;
                    continue;
                }
                Self::fire_goal_checkin_injected(
                    analytics_bus.as_ref(),
                    "idle_timer",
                    deferred_since_before.map_or(0, |since| now_ms.saturating_sub(since)),
                    &deferring,
                    checkin_count,
                    idle_checkin_count,
                )
                .await;
                if deferring.is_empty() {
                    goal_checkin
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .clear();
                    return;
                }
            }
        }
    }

    async fn goal_stop_hook_disposition(
        &self,
        goal_hook_id: Option<HookId>,
        agg: &hooks::AggregateHookResult,
        parent_aborted: bool,
    ) -> (Option<StopHookDisposition>, GoalEvalOutcome) {
        let Some(goal_hook_id) = goal_hook_id else {
            // No verdict at all: upstream's `We ?? (…)` fallback.
            return (
                None,
                if parent_aborted {
                    GoalEvalOutcome::Cancelled
                } else {
                    GoalEvalOutcome::Absent
                },
            );
        };
        let (session_id, goal) = {
            let session = self.session.lock().await;
            (session.session_id, session.active_goal.clone())
        };
        let Some(goal) = goal else {
            return (None, GoalEvalOutcome::Absent);
        };
        // A user may clear or replace a goal while its evaluator is running.
        // Never apply the old hook's verdict to the replacement condition.
        if self
            .hooks
            .get_session_named_hook(session_id, GOAL_STOP_HOOK_NAME)
            .await
            .is_none_or(|hook| hook.id != goal_hook_id)
        {
            return (None, GoalEvalOutcome::Absent);
        }
        let Some((_, result)) = agg
            .all_results
            .iter()
            .find(|(hook_id, _)| *hook_id == goal_hook_id)
        else {
            return (
                None,
                if parent_aborted {
                    GoalEvalOutcome::Cancelled
                } else {
                    GoalEvalOutcome::Absent
                },
            );
        };

        match result.outcome {
            hooks::HookOutcome::Success => {
                if let Some(response) = &result.response {
                    if matches!(response.decision, Some(hooks::HookDecision::Block)) {
                        let reason = response
                            .reason
                            .as_deref()
                            .map(|reason| {
                                reason
                                    .strip_prefix(&format!("[{}]: ", goal.condition))
                                    .unwrap_or(reason)
                                    .to_string()
                            })
                            .filter(|reason| !reason.is_empty())
                            .unwrap_or_else(|| "Goal not met yet".to_string());
                        self.record_goal_evaluation(Some(reason.clone()), true)
                            .await;
                        return (
                            Some(StopHookDisposition::GoalContinue(
                                response.reason.clone().unwrap_or(reason),
                            )),
                            GoalEvalOutcome::NotMet,
                        );
                    }
                }
                if let Some(response) = result.response.as_ref().filter(|r| r.impossible) {
                    // 2.1.263 `xY`: an impossible prompt condition clears the
                    // matching session goal as failed, never as achieved.
                    self.record_goal_evaluation(response.reason.clone(), false)
                        .await;
                    let _ = self
                        .finish_active_goal_state_and_hook(
                            lingxi_core::host::GoalStatusKind::Failed,
                            None,
                        )
                        .await;
                    return (None, GoalEvalOutcome::Impossible);
                }
                // The terminal `achieved` attachment below carries the updated
                // iteration count; avoid writing a redundant intermediate
                // `set` attachment for the same successful evaluation.
                // Successful prompt hooks do not contribute a generic hook
                // decision/reason. Keep the evaluator's evidence on the goal only.
                let reason = serde_json::from_str::<serde_json::Value>(&result.stdout)
                    .ok()
                    .and_then(|value| {
                        value
                            .get("reason")
                            .and_then(|r| r.as_str())
                            .map(str::to_string)
                    });
                self.record_goal_evaluation(reason, false).await;
                let _ = self
                    .finish_active_goal_state_and_hook(
                        lingxi_core::host::GoalStatusKind::Achieved,
                        None,
                    )
                    .await;
                (None, GoalEvalOutcome::Met)
            }
            hooks::HookOutcome::Timeout | hooks::HookOutcome::Cancelled => {
                if !parent_aborted {
                    self.handle_goal_interruption(
                        crate::prompt::goal_interruption::GoalInterruption::Pause(
                            crate::prompt::goal_interruption::PauseCause::GoalCheckTimeout,
                        ),
                    )
                    .await;
                }
                (None, GoalEvalOutcome::Cancelled)
            }
            hooks::HookOutcome::Error => {
                if !parent_aborted {
                    self.handle_goal_interruption(
                        crate::prompt::goal_interruption::GoalInterruption::Retry(
                            crate::prompt::goal_interruption::RetryCause::GoalCheck,
                        ),
                    )
                    .await;
                }
                (None, GoalEvalOutcome::Error)
            }
        }
    }

    async fn record_goal_evaluation(&self, last_reason: Option<String>, persist_active: bool) {
        let snapshot = {
            let mut session = self.session.lock().await;
            let Some(goal) = session.active_goal.as_mut() else {
                return;
            };
            goal.iterations = goal.iterations.saturating_add(1);
            goal.last_reason = last_reason;
            goal.clone()
        };
        if persist_active {
            // 2.1.266 @4212300: a blocking evaluation yields
            // `{met:false, condition, reason}` — the goal stays active with its
            // iteration count and reason refreshed, so this is NOT the `Set`
            // sentinel the port used to write here.
            self.persist_goal_status_attachment(
                lingxi_core::host::GoalStatusKind::NotMet,
                Some(&snapshot),
            )
            .await;
        }
    }

    /// Fire the `StopFailure` lifecycle hooks when a turn ends on an API error
    /// (RECOV.2, TS `executeStopFailureHooks`, `query.ts:1174/1181/1263`).
    ///
    /// Distinct from [`Self::fire_stop_hooks`]: the model never produced a real
    /// response, so the normal `Stop` hooks are skipped (they would create a
    /// death spiral — error → hook blocking → retry → error → …) and the
    /// `StopFailure` event fires instead. Fire-and-forget + best-effort exactly
    /// like the other lifecycle fires: the aggregate is discarded (TS calls
    /// `executeStopFailureHooks` as `void` — a `StopFailure` hook can neither
    /// block nor continue the turn). Strict no-op when no `StopFailure` hook is
    /// registered. `error` is the api-error discriminator carried verbatim into
    /// the wire payload's `error` field (TS `lastMessage.error`).
    async fn fire_stop_failure(&self, error: &str) {
        tracing::debug!(event = "hook_stop_failure_started", error);
        let ctx = self.lifecycle_hook_ctx(false).await;
        let _ = self
            .hooks
            .execute(
                HookEvent::StopFailure {
                    error: error.to_string(),
                },
                ctx,
            )
            .await;
    }

    /// Fire Stop hooks at a natural end-of-turn arm and translate the
    /// disposition into a driver control-flow directive (hooks B4). Shared by
    /// all three turn drivers. Skips firing (returns `FallThrough`) on the
    /// `prompt_too_long` error surface — the port of the skip-on-API-error guard
    /// (`query.ts:1262`). On `Prevent` it emits the end-turn before terminating
    /// so the cost/UI bookkeeping still fires.
    pub(super) async fn handle_stop_at_end(
        &self,
        stop_reason: &str,
        stop_hook_active: &mut bool,
        stop_hook_blocking_count: &mut u32,
        turn_count: u32,
        final_message_id: MessageId,
        parent_aborted: bool,
    ) -> StopHookFlow {
        if matches!(
            stop_reason,
            "prompt_too_long" | "blocking_limit" | "rapid_refill_breaker"
        ) {
            // RECOV.2: this turn ended on an API error — the model never produced
            // a real response, so fire the `StopFailure` hooks (NOT the `Stop`
            // hooks) before ending. 1:1 with TS `query.ts:1262-1264` (the
            // api-error skip guard runs `executeStopFailureHooks(lastMessage)` then
            // returns) and `query.ts:1174/1181` (PTL recovery exhausted). Running
            // the normal `Stop` hooks here would risk the death-spiral TS warns
            // against (error → hook blocking → retry → error → …). The wire
            // `error` is `"invalid_request"` for ALL THREE — the proactive
            // blocking-limit preempt and the rapid-refill breaker both surface an
            // assistant message whose api-error field is `invalid_request`
            // (binary `Ol({...,error:"invalid_request"})`), even though their
            // TERMINAL reasons (`blocking_limit` / `rapid_refill_breaker`) differ
            // from the reactive `prompt_too_long`. Matching TS
            // `createAssistantAPIErrorMessage({ …, error: 'invalid_request' })`
            // (`query.ts:642-644`).
            self.fire_stop_failure("invalid_request").await;
            return StopHookFlow::FallThrough;
        }
        let disposition = self
            .fire_stop_hooks(stop_reason, *stop_hook_active, parent_aborted)
            .await;
        let goal_blocked = matches!(&disposition, StopHookDisposition::GoalContinue(_));
        match disposition {
            StopHookDisposition::Prevent(reason) => {
                if !parent_aborted {
                    self.handle_goal_interruption(
                        crate::prompt::goal_interruption::GoalInterruption::Pause(
                            crate::prompt::goal_interruption::PauseCause::HookStopped,
                        ),
                    )
                    .await;
                }
                // FIX C (Stop hook_stopped_continuation): persist the stop-reason
                // meta message (claude `query/stopHooks.ts:269-280`, an isMeta
                // `hook_stopped_continuation` attachment) BEFORE terminating so the
                // transcript records why the Stop hook halted continuation.
                self.append_stop_hook_stopped_continuation(&reason).await;
                let cost = self.snapshot_cost_real().await;
                self.output.emit_end_turn(stop_reason, &cost).await;
                StopHookFlow::Terminate(ConversationOutcome::StopHookPrevented {
                    turn_count,
                    final_message_id,
                })
            }
            StopHookDisposition::Continue(reason) | StopHookDisposition::GoalContinue(reason) => {
                // #2/#4 consecutive-block cap (binary `let ar=Z+1; if(bo>0&&ar>bo)
                // …return {reason:"completed"}`). `Z` is the carried
                // `stopHookBlockingCount`; `ar` the would-be next count.
                let next_count = stop_hook_blocking_count.saturating_add(1);
                // Binary blocking-branch max-turns check, evaluated BEFORE the
                // block cap (`let dt=ie+1,nn=te+1; if(c&&dt>c) return
                // G("tengu_stop_hook_block_count",{count:nn,hit_max_turns:!0,
                // hit_cap:!1}), yield ei({type:"max_turns_reached",maxTurns:c,
                // turnCount:dt},…), {reason:"max_turns",turnCount:dt}`). `c` is
                // `maxTurns`; `ie` is this turn's count = our (already-incremented)
                // `turn_count`, so the binary's `ie+1>c` is exactly
                // `turn_count >= max_turns`. The port represents the max-turns
                // terminal as `OrchestratorError::MaxTurnsReached`
                // (→ `TurnOutcome::MaxTurns`) rather than a discrete stream event,
                // so we route there via `TerminateMaxTurns` — but we still fire the
                // `hit_max_turns:true` block-count event the binary emits here, and
                // (like the binary, which returns before appending) we end WITHOUT
                // adding the stop-hook feedback message the `LoopAgain` path would.
                if self.config.max_turns != 0 && turn_count >= self.config.max_turns {
                    self.fire_stop_hook_block_count(next_count, true, false)
                        .await;
                    return StopHookFlow::TerminateMaxTurns;
                }
                // `parseInt(process.env.LINGXI_STOP_HOOK_BLOCK_CAP??"",10)`
                // with `Number.isNaN(jr)?8:jr` ⇒ unset / non-numeric → 8. A
                // `cap <= 0` disables the cap (binary `if(bo>0&&…)`), letting a
                // blocking hook drive until the max_turns top-of-loop guard ends it.
                let cap: i64 = std::env::var("LINGXI_STOP_HOOK_BLOCK_CAP")
                    .or_else(|_| std::env::var("CLAUDE_CODE_STOP_HOOK_BLOCK_CAP"))
                    .ok()
                    .and_then(|v| v.trim().parse::<i64>().ok())
                    .unwrap_or(8);
                if cap > 0 && i64::from(next_count) > cap {
                    // Cap exceeded: log `tengu_stop_hook_block_count{hit_cap:true}`,
                    // surface the byte-exact override warning (em-dash U+2014;
                    // binary `yield Dc(…,"warning")`), and END the turn (binary
                    // returns `{reason:"completed"}` ⇒ our `FallThrough` runs the
                    // normal end-of-turn tail). NOTE: `is_subagent` is hard-coded
                    // `false` — the orchestrator does not thread subagent identity
                    // (`Boolean(N.agentId)`); the main-loop value is `false`.
                    self.fire_stop_hook_block_count(next_count, false, true)
                        .await;
                    let warning = format!(
                        "A hook blocked the turn from ending {next_count} consecutive times — overriding and ending turn. For Stop/SubagentStop hooks, check stop_hook_active in the input and return success while it's true. Set LINGXI_STOP_HOOK_BLOCK_CAP to raise this limit."
                    );
                    self.output.emit_text(&warning, None).await;
                    if goal_blocked {
                        self.handle_goal_interruption(
                            crate::prompt::goal_interruption::GoalInterruption::Pause(
                                crate::prompt::goal_interruption::PauseCause::GoalCheckCapped,
                            ),
                        )
                        .await;
                    }
                    return StopHookFlow::FallThrough;
                }
                // Empty feedback means the goal check-in was already appended.
                if !reason.is_empty() {
                    self.append_stop_hook_feedback(&reason).await;
                }
                *stop_hook_active = true;
                *stop_hook_blocking_count = next_count;
                StopHookFlow::LoopAgain
            }
            StopHookDisposition::Pass => {
                // #4: a previously-blocking Stop hook finally let the turn end —
                // log the final consecutive-block count (binary
                // `if(Z>0&&Un.blockingErrors.length===0) W("tengu_stop_hook_block_count",
                // {count:Z,hit_max_turns:!1,hit_cap:!1})`). No-op when the hook
                // never blocked this turn (`count == 0`).
                if *stop_hook_blocking_count > 0 {
                    self.fire_stop_hook_block_count(*stop_hook_blocking_count, false, false)
                        .await;
                }
                StopHookFlow::FallThrough
            }
        }
    }

    /// Fire Stop hooks for a turn ended by `ToolResult.endsTurn` or MCP
    /// `claude/endTurn`, discarding every block/prevent disposition.
    ///
    /// Claude Code's forced end path deliberately runs the hooks for their
    /// messages and side effects, then logs any attempted block and completes
    /// without another model call. It never appends stop-hook feedback, applies
    /// the block cap, or returns `StopHookPrevented` on this path.
    pub(super) async fn fire_tool_result_end_stop_hooks(
        &self,
        stop_reason: &str,
        stop_hook_active: bool,
        parent_aborted: bool,
    ) {
        let disposition = self
            .fire_stop_hooks(stop_reason, stop_hook_active, parent_aborted)
            .await;
        if !matches!(disposition, StopHookDisposition::Pass) {
            tracing::debug!(
                event = "hook_stop_disposition_discarded",
                reason = stop_reason,
                "Stop hook disposition discarded because a tool result ended the turn"
            );
        }
    }

    /// Fire the `tengu_stop_hook_block_count` analytics event (hooks B4, binary
    /// `bin/claude.exe` offset ~208046100). Emitted in three forms: on the
    /// block-cap end (`hit_cap:true`), on the max-turns-via-stop-hook end
    /// (`hit_max_turns:true`), and when a previously-blocking hook finally lets
    /// the turn end (both `false`). `is_subagent` is hard-coded `false` — the
    /// orchestrator does not model subagent identity (`Boolean(N.agentId)`).
    /// Strict no-op when no analytics bus is wired.
    async fn fire_stop_hook_block_count(&self, count: u32, hit_max_turns: bool, hit_cap: bool) {
        let Some(bus) = self.model_runtime.analytics_bus.as_ref() else {
            return;
        };
        let mut metadata = telemetry::LogEventMetadata::new();
        metadata.insert(
            "count".into(),
            telemetry::AnalyticsValue::Int(i64::from(count)),
        );
        metadata.insert("is_subagent".into(), telemetry::AnalyticsValue::Bool(false));
        metadata.insert(
            "hit_max_turns".into(),
            telemetry::AnalyticsValue::Bool(hit_max_turns),
        );
        metadata.insert("hit_cap".into(), telemetry::AnalyticsValue::Bool(hit_cap));
        bus.log_event("tengu_stop_hook_block_count", metadata).await;
    }

    /// Fire the `SessionStart` lifecycle hooks at session startup (hooks
    /// session lifecycle, TS `SessionStart` event fired by `executeSetupHooks` /
    /// the `SessionStart` path at session startup — `utils/hooks.ts:3876-3881`).
    ///
    /// `source` is the TS `SessionStart` `source` discriminator — one of
    /// `startup` / `resume` / `clear` / `compact`. The host composition root
    /// (`engine-desktop` / `engine-mobile`) owns the session lifecycle (it
    /// constructs the orchestrator), so it calls this ONCE immediately after
    /// `build()` returns a fully-wired runtime, passing `"startup"` for a fresh
    /// session.
    ///
    /// Best-effort, exactly like [`Self::fire_pre_compact`]: the aggregate is
    /// discarded so a failing `SessionStart` hook never breaks boot. Strict no-op
    /// when no `SessionStart` hook is registered. The hook executor reads
    /// `session_id` / `cwd` from the lifecycle [`HookContext`]; the variant's
    /// `session_id` field is filled from the live session for symmetry.
    /// Fire the `SessionStart` hooks once and return the folded aggregate.
    async fn run_session_start_hooks(&self, source: &str) -> hooks::response::AggregateHookResult {
        let session_id = { self.session.lock().await.session_id };
        let ctx = self.lifecycle_hook_ctx(false).await;
        self.hooks
            .execute(
                HookEvent::SessionStart {
                    session_id,
                    source: source.to_string(),
                },
                ctx,
            )
            .await
    }

    /// Build the cache metadata shared by `PreModelSwitch` and
    /// `PostModelSwitch`. Context is the last main-thread response's complete
    /// usage (input already includes cache read/write, plus output). Cache warmth
    /// describes the existing prompt prefix before the switch: a recent
    /// successful call is likely still inside its configured TTL even though
    /// the target model will need a new provider-local cache entry.
    async fn model_switch_cache_metadata(
        &self,
        to_model: &str,
        to_profile: Option<&str>,
    ) -> (u64, bool, String, f64, String) {
        let context_tokens = self
            .compaction_runtime
            .last_response_input_tokens
            .load(std::sync::atomic::Ordering::Relaxed)
            .saturating_add(
                self.compaction_runtime
                    .last_response_output_tokens
                    .load(std::sync::atomic::Ordering::Relaxed),
            );
        let fallback_ttl = if lingxi_core::host::env::is_env_truthy(
            std::env::var("ENABLE_PROMPT_CACHING_1H").ok().as_deref(),
        ) {
            cost::prompt_cache_ledger::CacheTtl::OneHour
        } else {
            cost::prompt_cache_ledger::CacheTtl::FiveMinutes
        };
        let now_ms = u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(0);
        let (prompt_cache_warm, ttl) = self
            .current_prompt_cache_ledger()
            .await
            .model_switch_cache_state(now_ms, fallback_ttl);
        let ttl_1h = ttl == cost::prompt_cache_ledger::CacheTtl::OneHour;
        let cache_ttl = ttl.as_str();
        let (estimated_cache_write_usd, pricing) =
            self.model_switch_cache_write_estimate(to_model, to_profile, context_tokens, ttl_1h);
        (
            context_tokens,
            prompt_cache_warm,
            cache_ttl.to_string(),
            estimated_cache_write_usd,
            pricing,
        )
    }

    /// Estimate the target route's cache-write cost from the same assembled
    /// multi-provider catalog used for real accounting. Display metadata is
    /// consulted for explicit free/subscription routes and user overrides;
    /// unknown routes use the tracker's documented default tier rather than a
    /// misleading zero.
    #[allow(clippy::cast_precision_loss)]
    fn model_switch_cache_write_estimate(
        &self,
        to_model: &str,
        to_profile: Option<&str>,
        context_tokens: u64,
        ttl_1h: bool,
    ) -> (f64, String) {
        let listings = self.api.list_model_listings();
        let listing = listings.iter().find(|listing| {
            listing.request_model == to_model
                && to_profile.map_or_else(
                    || {
                        listings
                            .iter()
                            .filter(|candidate| candidate.request_model == to_model)
                            .count()
                            == 1
                    },
                    |profile| listing.provider_id == profile,
                )
        });
        let display_pricing = listing.and_then(|listing| listing.metadata.pricing.as_ref());
        if display_pricing.is_some_and(|pricing| {
            matches!(
                pricing.billing_mode,
                lingxi_core::host::ModelBillingMode::Subscription
                    | lingxi_core::host::ModelBillingMode::Free
            )
        }) {
            return (0.0, "catalog".to_string());
        }

        let model_ref = crate::cost_wiring::model_ref_from_string(to_model, to_profile);
        let (catalog_pricing, resolution) = self.model_runtime.cost_tracker.as_ref().map_or_else(
            || {
                (
                    cost::pricing::PricingCatalog::default_unknown_pricing(&model_ref),
                    cost::pricing::PricingResolution::UnpricedModel {
                        requested: model_ref.clone(),
                    },
                )
            },
            |tracker| tracker.resolve_pricing_with_default(&model_ref),
        );
        let used_default = matches!(
            &resolution,
            cost::pricing::PricingResolution::UnpricedModel { .. }
        );

        let direct_rate = display_pricing.and_then(|pricing| {
            pricing
                .tiers
                .iter()
                .filter(|tier| tier.context_threshold_tokens <= context_tokens)
                .max_by_key(|tier| tier.context_threshold_tokens)
                .and_then(|tier| tier.cache_write_per_million)
                .or(pricing.cache_write_per_million)
        });
        let catalog_rate = |class: cost::pricing::TokenClass| {
            catalog_pricing
                .token_rates
                .get(&class)
                .map(|rate| rate.nano_usd_per_token as f64 / 1_000.0)
        };
        let rate_per_million = direct_rate.map_or_else(
            || {
                if ttl_1h {
                    catalog_rate(cost::pricing::TokenClass::CacheWrite1h)
                } else {
                    None
                }
                .or_else(|| catalog_rate(cost::pricing::TokenClass::CacheWrite))
                .unwrap_or(0.0)
            },
            |rate| {
                if !ttl_1h {
                    return rate;
                }
                let base = catalog_rate(cost::pricing::TokenClass::CacheWrite);
                let hourly = catalog_rate(cost::pricing::TokenClass::CacheWrite1h);
                match (base, hourly) {
                    _ if used_default => rate,
                    (Some(base), Some(hourly)) if base > 0.0 => rate * hourly / base,
                    _ => rate,
                }
            },
        );
        let estimate = ((context_tokens as f64 * rate_per_million / 1_000_000.0) * 10_000.0)
            .round()
            / 10_000.0;
        let pricing = if display_pricing.and_then(|pricing| pricing.source.as_deref())
            == Some("userOverride")
        {
            "configured"
        } else if used_default {
            "default"
        } else {
            "catalog"
        };
        (estimate, pricing.to_string())
    }

    /// Fire the blocking `PreModelSwitch` hooks before mutating the live model.
    /// Only callers that provide a `command`, `picker`, or `sdk` source should
    /// invoke this method; `auto` and `resume` are Post-only upstream paths.
    pub(crate) async fn run_pre_model_switch_hooks(
        &self,
        from_model: &str,
        to_model: &str,
        requested_model: Option<&str>,
        to_profile: Option<&str>,
        source: &str,
    ) -> hooks::response::AggregateHookResult {
        if !self
            .hooks
            .has_hooks_for(&hooks::events::HookEventType::PreModelSwitch)
            .await
        {
            return hooks::response::AggregateHookResult::default();
        }
        let (context_tokens, prompt_cache_warm, cache_ttl, estimated_cache_write_usd, pricing) =
            self.model_switch_cache_metadata(to_model, to_profile).await;
        let ctx = self.lifecycle_hook_ctx(false).await;
        self.hooks
            .execute(
                HookEvent::PreModelSwitch {
                    from_model: from_model.to_string(),
                    to_model: to_model.to_string(),
                    requested_model: requested_model.map(str::to_string),
                    source: source.to_string(),
                    context_tokens,
                    prompt_cache_warm,
                    cache_ttl,
                    estimated_cache_write_usd,
                    pricing,
                },
                ctx,
            )
            .await
    }

    /// Fire `PostModelSwitch` after the live model changes. Post hooks are
    /// best-effort: their decisions never roll back an already-applied switch.
    /// Additional context is persisted as one attachment and kept as a meta
    /// message in the live history for the next model request.
    pub(crate) async fn run_post_model_switch_hooks(
        &self,
        from_model: &str,
        to_model: &str,
        requested_model: Option<&str>,
        to_profile: Option<&str>,
        source: &str,
    ) -> hooks::response::AggregateHookResult {
        if !self
            .hooks
            .has_hooks_for(&hooks::events::HookEventType::PostModelSwitch)
            .await
        {
            return hooks::response::AggregateHookResult::default();
        }
        let (context_tokens, prompt_cache_warm, cache_ttl, estimated_cache_write_usd, pricing) =
            self.model_switch_cache_metadata(to_model, to_profile).await;
        let ctx = self.lifecycle_hook_ctx(false).await;
        let aggregate = self
            .hooks
            .execute(
                HookEvent::PostModelSwitch {
                    from_model: from_model.to_string(),
                    to_model: to_model.to_string(),
                    requested_model: requested_model.map(str::to_string),
                    source: source.to_string(),
                    context_tokens,
                    prompt_cache_warm,
                    cache_ttl,
                    estimated_cache_write_usd,
                    pricing,
                },
                ctx,
            )
            .await;

        if !aggregate.additional_contexts.is_empty() {
            let tool_use_id = format!("hook-{}", lingxi_core::types::HookId::new().as_uuid());
            let attachment = hooks::additional_context_attachment(
                "PostModelSwitch",
                &tool_use_id,
                "PostModelSwitch",
                &aggregate.additional_contexts,
            );
            self.persist_exact_hook_attachment(attachment).await;
            let body = ExactHookText::join(&aggregate.additional_contexts, "\n");
            let message_text = ExactHookText::wrapped(
                "<system-reminder>\nPostModelSwitch hook additional context: ",
                &body,
                "\n</system-reminder>",
            );
            let message = message_text.to_conversation_message(MessageId::new(), true);
            self.register_mod_persisted_attachment(
                &message,
                "hook_additional_context",
                serde_json::json!({"kind":"hook","event":"PostModelSwitch"}),
            )
            .await;
            self.session.lock().await.history.push(message);
        }
        aggregate
    }

    pub(super) async fn collect_session_start_messages(
        &self,
        source: &str,
    ) -> Vec<ConversationMessage> {
        let agg = self.run_session_start_hooks(source).await;
        let messages = Self::session_start_context_messages(&agg);
        for message in &messages {
            self.register_mod_persisted_attachment(
                message,
                "hook_additional_context",
                serde_json::json!({"kind":"hook","event":"SessionStart"}),
            )
            .await;
        }
        messages
    }

    /// Build the model-facing `hook_additional_context` meta message(s) from a
    /// folded `SessionStart` aggregate (empty when no hook emitted context).
    fn session_start_context_messages(
        agg: &hooks::response::AggregateHookResult,
    ) -> Vec<ConversationMessage> {
        // SESSIONSTART.CTX: a `SessionStart` hook's
        // `hookSpecificOutput.additionalContext` becomes a persistent
        // `hook_additional_context` attachment in the conversation —
        // claude-code's `processSessionStartHooks` collects every hook's
        // `additionalContext` and, when non-empty, emits a single
        // `createAttachmentMessage({type:'hook_additional_context', content:
        // additionalContexts, hookName:'SessionStart'})` (`utils/sessionStart.ts:163-172`),
        // rendered by `messages.ts:4117-4128` as a meta user message
        // `wrapInSystemReminder(`${hookName} hook additional context: ${content.join('\n')}`)`
        // with `isMeta:true`.
        //
        // Unlike the per-turn date / output-style reminders (regenerated each
        // turn and never stored), claude-code adds THIS message ONCE at session
        // start and keeps it in the conversation array, so it rides EVERY
        // subsequent turn. We mirror that by pushing it into `s.history`, which
        // is cloned into each turn's outgoing snapshot (`turn_loop.rs` —
        // `s.history.clone()`). `user_meta` (isMeta) is sent to the wire but not
        // persisted to JSONL (matching `createUserMessage({isMeta:true})`); a
        // `resume` re-fires `SessionStart`, so the context is re-added rather
        // than relying on transcript persistence.
        //
        // Strict no-op when no hook emitted `additionalContext` — the aggregate
        // is otherwise discarded exactly as before, so a failing / silent
        // `SessionStart` hook never affects boot.
        if agg.additional_contexts.is_empty() {
            return Vec::new();
        }
        let body = ExactHookText::join(&agg.additional_contexts, "\n");
        vec![
            ExactHookText::wrapped(
                "<system-reminder>\nSessionStart hook additional context: ",
                &body,
                "\n</system-reminder>",
            )
            .to_conversation_message(MessageId::new(), true),
        ]
    }

    /// Fire SessionStart and append its model-facing additional context, then
    /// honor a `SessionStart` hook's `initialUserMessage` (injected as a non-meta
    /// user prompt — claude-code `if(p.initialUserMessage)$os=p.initialUserMessage`)
    /// and return the folded hook aggregate so the composition root can apply
    /// host-owned follow-up actions such as `reloadSkills`.
    async fn fire_mod_session_start(&self) {
        let Some(registry) = &self.lifecycle_runtime.hook_registry else {
            return;
        };
        let Some(host) = registry.read().await.mod_host() else {
            return;
        };
        let interactive = self.prompt_is_interactive();
        let surface = if interactive
            && self
                .config
                .query_source
                .starts_with(crate::config::QUERY_SOURCE_REPL_MAIN_THREAD)
        {
            Some("terminal")
        } else {
            None
        };
        let input = serde_json::json!({
            "cwd":self.current_cwd().to_string_lossy(),
            "surface":surface,
            "isInteractive":interactive,
        });
        let log_output = self.output.clone();
        let toast_output = self.output.clone();
        let status_output = self.output.clone();
        let _ = host
            .dispatch_with_ui_at_session(
                "session.start",
                input,
                self,
                |event| async move {
                    let cwd = event
                        .get("cwd")
                        .and_then(serde_json::Value::as_str)
                        .ok_or_else(|| {
                            hooks::mods::ModError::Hook("session.start needs cwd".into())
                        })?;
                    Ok(serde_json::json!({"cwd":cwd}))
                },
                move |plugin, text| {
                    let output = log_output.clone();
                    async move { output.emit_mod_log(&plugin, &text).await }
                },
                move |plugin, text, timeout_ms| {
                    let output = toast_output.clone();
                    async move { output.emit_mod_toast(&plugin, &text, timeout_ms).await }
                },
                move |plugin, text| {
                    let output = status_output.clone();
                    async move { output.emit_mod_status(&plugin, text.as_deref()).await }
                },
            )
            .await;
    }

    pub async fn fire_session_start(&self, source: &str) -> hooks::response::AggregateHookResult {
        self.fire_mod_session_start().await;
        let agg = self.run_session_start_hooks(source).await;
        let messages = Self::session_start_context_messages(&agg);
        if !messages.is_empty() {
            for message in &messages {
                self.register_mod_persisted_attachment(
                    message,
                    "hook_additional_context",
                    serde_json::json!({"kind":"hook","event":"SessionStart"}),
                )
                .await;
            }
            // O3: the PERSISTED record is a `hook_additional_context`
            // ATTACHMENT line (2.1.220 BIN off 232675554). Note the three
            // LITERALS at that site — `hookName:"SessionStart"` (BARE, NOT
            // `SessionStart:{source}` as the hook-RUN attachments use) and
            // `toolUseID:"SessionStart"` (a literal, NOT a uuid). Confirmed by
            // 129 real 2.1.220 records.
            //
            // The `user_meta` message below is the EPHEMERAL model rendering
            // (`zr({content: Ww(…), isMeta:true})`, renderer BIN off
            // 238107100); it only enters `history` and is never persisted from
            // here, so this attachment is the sole on-disk record.
            let attachment = hooks::additional_context_attachment(
                "SessionStart",
                "SessionStart",
                "SessionStart",
                &agg.additional_contexts,
            );
            self.persist_exact_hook_attachment(attachment).await;
            self.session.lock().await.history.extend(messages);
        }
        // `initialUserMessage` → seed the initial user prompt (claude-code `$os`).
        if let Some(initial) = &agg.initial_user_message {
            self.inject_user_text_exact(initial, false).await;
        }
        agg
    }

    /// Fire the `InstructionsLoaded` hooks once per loaded instruction file at
    /// session startup (hooks lifecycle, TS `executeInstructionsLoadedHooks`
    /// dispatched from the eager `getMemoryFiles` pass — `utils/claudemd.ts:1054-1071`,
    /// `utils/hooks.ts:4335-4369`).
    ///
    /// claude-code fires this fire-and-forget hook for **each** LINGXI.md /
    /// `LINGXI.local.md` it splices into context, carrying that file's `file_path`,
    /// `memory_type` (`User` / `Project` / `Local` / `Managed`), and `load_reason`.
    /// The eager session-start pass reports `load_reason: 'session_start'` for every
    /// top-level (parent-less) file (`eagerLoadReason`). The orchestrator's
    /// [`crate::prompt::MemoryHierarchyProvider`] loads the full Managed/User/
    /// Project/Local hierarchy and tags each file with its
    /// [`memory::lingxi_md::LingxiMdTier`]; `memory_type` is taken directly from
    /// that tier (so an enterprise-`Managed` file is reported as `Managed`).
    ///
    /// Conditional rule-directory projections are filtered at acquisition.
    /// Named files retain globs, and all retained imports carry their direct
    /// parent path and report `load_reason = include`. The eager pass omits the
    /// lazy trigger path.
    ///
    /// The orchestrator already owns the memory provider AND the hook registry, so
    /// it loads memory ONCE here (the same `memory.load(&cwd)` the system-prompt
    /// assembler uses) and fires from a single point — no cross-crate seam. The
    /// host composition root calls this ONCE immediately after [`Self::fire_session_start`].
    ///
    /// Best-effort, exactly like [`Self::fire_session_start`]: each hook aggregate
    /// is discarded so a failing `InstructionsLoaded` hook never breaks boot, and
    /// it is a strict no-op when no `InstructionsLoaded` hook is registered.
    pub async fn fire_instructions_loaded(&self) {
        self.fire_instructions_loaded_with_reason(
            hooks::events::InstructionsLoadReason::SessionStart,
        )
        .await;
    }

    pub(super) async fn fire_instructions_loaded_with_reason(
        &self,
        load_reason: hooks::events::InstructionsLoadReason,
    ) {
        let memory_files = match self.instruction_file_snapshot().await {
            Ok(files) => files,
            Err(error) => {
                tracing::warn!(%error, "eager instruction reload failed");
                return;
            }
        };
        if self
            .prompt_runtime
            .instruction_cache
            .consume_eager_load_reason()
            .is_none()
        {
            return;
        }
        if memory_files.is_empty() {
            return;
        }
        // The eager acquisition already filtered conditional rule-directory
        // projections. Named files and their imports keep glob/parent metadata
        // and participate in both eager seeding and InstructionsLoaded.
        self.seed_memory_read_state(&memory_files).await;
        for file in memory_files.iter().cloned() {
            // `memory_type` is taken straight from the file's tier (claude-code
            // fires `file.type`, claudemd.ts:1058-1062), so the Managed tier is
            // reported faithfully rather than misclassified as Project. The
            // two used to be separate enums with a four-arm translation here;
            // they are now the same type, so there is nothing left to mistype.
            let memory_type = file.tier;
            // A fresh per-file `HookContext` (the executor reads `session_id` /
            // `cwd` from it); `lifecycle_hook_ctx` re-locks the session each call,
            // matching the other lifecycle fires.
            let ctx = self.lifecycle_hook_ctx(false).await;
            let _ = self
                .hooks
                .execute(
                    HookEvent::InstructionsLoaded {
                        file_path: file.path,
                        memory_type,
                        load_reason: if file.parent.is_some() {
                            hooks::events::InstructionsLoadReason::Include
                        } else {
                            load_reason
                        },
                        globs: file.globs,
                        trigger_file_path: None,
                        parent_file_path: file.parent,
                    },
                    ctx,
                )
                .await;
        }
    }

    /// Fire the `SessionEnd` lifecycle hooks at session teardown (hooks session
    /// lifecycle, TS `executeSessionEndHooks` — `utils/hooks.ts:4097-4117`).
    ///
    /// `reason` is the TS `SessionEnd` `reason` (`ExitReason`) discriminator
    /// (e.g. `clear` / `logout` / `prompt_input_exit` / `other`). The host
    /// composition root owns teardown; it calls this at an explicit session-end
    /// seam when one exists. Best-effort like [`Self::fire_session_start`] — a
    /// failing hook never breaks teardown, and it is a strict no-op when no
    /// `SessionEnd` hook is registered.
    async fn fire_mod_session_end(&self, reason: &str, session_id: &str) {
        let Some(registry) = &self.lifecycle_runtime.hook_registry else {
            return;
        };
        let Some(host) = registry.read().await.mod_host() else {
            return;
        };
        struct ResetModStateOnDrop(std::sync::Arc<hooks::mods::ModHost>);
        impl Drop for ResetModStateOnDrop {
            fn drop(&mut self) {
                self.0.reset_session_state();
            }
        }
        let _reset_state = ResetModStateOnDrop(host.clone());
        let input = serde_json::json!({
            "reason": reason,
            "sessionId": session_id,
            "resume": { "id": session_id },
        });
        let log_output = self.output.clone();
        let toast_output = self.output.clone();
        let status_output = self.output.clone();
        let pinned_session_id = session_id.to_owned();
        let _ = host
            .dispatch_with_ui_at_session(
                "session.end",
                input,
                self,
                move |_| {
                    let session_id = pinned_session_id.clone();
                    async move { Ok(serde_json::json!({"sessionId":session_id})) }
                },
                move |plugin, text| {
                    let output = log_output.clone();
                    async move { output.emit_mod_log(&plugin, &text).await }
                },
                move |plugin, text, timeout_ms| {
                    let output = toast_output.clone();
                    async move { output.emit_mod_toast(&plugin, &text, timeout_ms).await }
                },
                move |plugin, text| {
                    let output = status_output.clone();
                    async move { output.emit_mod_status(&plugin, text.as_deref()).await }
                },
            )
            .await;
    }

    pub async fn fire_session_end(&self, reason: &str) {
        let session_id = { self.session.lock().await.session_id };
        let mut ended = self.session_end_fired.lock().await;
        let session_id_text = session_id.to_string();
        if ended.contains(&session_id_text) {
            return;
        }
        self.fire_mod_session_end(reason, &session_id_text).await;
        let ctx = self.lifecycle_hook_ctx(false).await;
        // Route through the SessionEnd *batch-deadline* path (claude-code `lje`
        // → `cH({signal: AbortSignal.timeout(Wqt())})`): the whole SessionEnd
        // hook batch is capped by a single shutdown budget
        // (`LINGXI_SESSIONEND_HOOKS_TIMEOUT_MS`, else `max(1500,
        // min(maxPerHookMs, 60000))`), so a slow / hung teardown hook cannot
        // stall session exit — NOT the generic 10-minute per-hook `execute`.
        let _ = self
            .hooks
            .execute_session_end(
                HookEvent::SessionEnd {
                    session_id,
                    reason: reason.to_string(),
                },
                ctx,
            )
            .await;
        compaction::invoked_skills::clear_session(&session_id.to_string());
        ended.insert(session_id_text);
    }

    /// Fire the `Notification` lifecycle hooks (hooks runtime lifecycle, TS
    /// `sendNotification` → `executeNotificationHooks`). `message` is the wire
    /// `NotificationPayload.message`; `notification_type` is the byte-faithful
    /// `notification_type` discriminator (e.g. `idle_prompt`) — it FEEDS the
    /// `HookEvent::Notification { kind }` field, which the executor copies
    /// verbatim into `NotificationPayload.notification_type`
    /// (`hooks/executor.rs` Notification arm).
    ///
    /// The canonical caller is the REPL idle watcher: claude-code fires
    /// `sendNotification({ message: "Claude is waiting for your input",
    /// notificationType: "idle_prompt" })` once the repl has been idle for
    /// `messageIdleNotifThresholdMs` after the last response
    /// (`screens/REPL.tsx:3930-3940`).
    ///
    /// Best-effort, exactly like [`Self::fire_session_end`]: the aggregate is
    /// discarded so a failing or blocking `Notification` hook can NEVER affect
    /// the caller (the repl input loop), and it is a strict no-op when no
    /// `Notification` hook is registered (the `execute` matcher returns an
    /// empty set → default aggregate, no process spawned).
    pub async fn fire_notification(&self, message: &str, notification_type: &str) {
        let ctx = self.lifecycle_hook_ctx(false).await;
        let _ = self
            .hooks
            .execute(
                HookEvent::Notification {
                    message: message.to_string(),
                    kind: notification_type.to_string(),
                },
                ctx,
            )
            .await;
    }

    /// Fire the `ConfigChange` hooks when a settings / skills file mutated on
    /// disk (hooks runtime lifecycle, TS `executeConfigChangeHooks` —
    /// `utils/hooks.ts:4214`, dispatched from the settings watcher
    /// `utils/settings/changeDetector.ts:285-297`).
    ///
    /// claude-code's `changeDetector` watches the settings files via
    /// `fs.watchFile` polling and, on every detected change, fires this hook
    /// with the `source` (which settings layer / skills) and the changed
    /// `file_path` BEFORE applying the change to the live session. The host
    /// composition root (`engine-desktop`) owns the watcher; this method is the
    /// single fire seam it calls per change event — the live settings RELOAD is
    /// a separate concern handled (or not) by the composition root.
    ///
    /// `source` maps 1:1 onto claude-code's `ConfigChangeSource`
    /// (`user_settings` / `project_settings` / `local_settings` /
    /// `policy_settings` / `skills`); `file_path` is the absolute path that
    /// changed (TS always passes the path, so this is `Some` in production —
    /// the field stays `Option` because the wire schema marks it `.optional()`).
    ///
    /// Best-effort, exactly like [`Self::fire_session_start`]: the aggregate is
    /// discarded so a failing or blocking `ConfigChange` hook never breaks the
    /// watcher loop, and it is a strict no-op when no `ConfigChange` hook is
    /// registered (the common case). The hook executor reads `session_id` /
    /// `cwd` from the lifecycle [`HookContext`].
    pub async fn fire_config_change(
        &self,
        source: hooks::events::ConfigChangeSource,
        file_path: Option<std::path::PathBuf>,
    ) {
        let ctx = self.lifecycle_hook_ctx(false).await;
        let _ = self
            .hooks
            .execute(HookEvent::ConfigChange { source, file_path }, ctx)
            .await;
    }

    /// Fire the `DirectoryAdded` hook after a working directory is added
    /// mid-session (2.1.219).
    ///
    /// `source` is both a payload field and the MATCHER QUERY — claude-code
    /// `a$t` dispatches with `matchQuery: t`, so a hook `matcher` is tested
    /// against `slash_command` / `register_repo_root`, not against the path. A
    /// matcher written against the directory would silently never fire.
    ///
    /// Best-effort, like the other lifecycle fires: a failing or absent hook
    /// must not undo a directory the user successfully added.
    pub async fn fire_directory_added(
        &self,
        directory: &str,
        source: &str,
    ) -> lingxi_core::host::DirectoryAddedHookSummary {
        self.refresh_instruction_context(
            lingxi_core::host::instructions::InstructionRefreshReason::DirectoryAdded,
        );
        let ctx = self.lifecycle_hook_ctx(false).await;
        let aggregate = self
            .hooks
            .execute(
                HookEvent::DirectoryAdded {
                    directory: directory.to_string(),
                    source: source.to_string(),
                },
                ctx,
            )
            .await;

        let mut failure_count = 0u32;
        for (hook_id, result) in &aggregate.all_results {
            if result.outcome != hooks::response::HookOutcome::Success {
                failure_count = failure_count.saturating_add(1);
                tracing::error!(
                    hook_id = %hook_id,
                    outcome = ?result.outcome,
                    stdout = %result.stdout,
                    stderr = %result.stderr,
                    "DirectoryAdded hook failed"
                );
            }
        }

        const PER_MESSAGE_LIMIT: usize = 4 * 1024;
        const TOTAL_LIMIT: usize = 16 * 1024;
        let mut remaining = TOTAL_LIMIT;
        let mut context_messages = Vec::new();
        let mut exact_context_messages = Vec::new();
        for message in aggregate
            .system_messages
            .iter()
            .chain(aggregate.additional_contexts.iter())
        {
            if remaining == 0 {
                break;
            }
            let cap = PER_MESSAGE_LIMIT.min(remaining);
            let bounded = message.truncate_utf8_bytes(cap);
            remaining = remaining.saturating_sub(bounded.encoded_utf8_len());
            if !bounded.is_empty() {
                context_messages.push(bounded.display.clone());
                exact_context_messages.push(bounded);
            }
        }
        if failure_count > 0 {
            let failure = format!(
                "{failure_count} DirectoryAdded hook(s) failed; output is in the debug log, not shown here"
            );
            context_messages.push(failure.clone());
            exact_context_messages.push(failure.into());
        }

        if !exact_context_messages.is_empty() {
            let body = exact_context_messages
                .iter()
                .map(|message| ExactHookText::wrapped("DirectoryAdded hook: ", message, ""))
                .collect::<Vec<_>>();
            let body = ExactHookText::join(&body, "\n");
            let message =
                ExactHookText::wrapped("<system-reminder>\n", &body, "\n</system-reminder>")
                    .to_conversation_message(MessageId::new(), true);
            self.session.lock().await.history.push(message.clone());
            self.persist_message_to_jsonl(&message).await;
        }

        lingxi_core::host::DirectoryAddedHookSummary {
            failure_count,
            context_messages,
        }
    }

    /// Register a canonical additional repository root for the live session.
    ///
    /// The trusted-root cell is updated before MCP/sandbox consumers and before
    /// `DirectoryAdded` hooks, so a hook observing the event cannot race the
    /// permission boundary it was told had changed.
    pub async fn register_repo_root(
        &self,
        request: lingxi_core::host::RegisterRepoRootRequest,
    ) -> Result<lingxi_core::host::RegisterRepoRootOutcome, lingxi_core::host::HandleError> {
        let current = self.session_cwd.cwd();
        let raw = std::path::PathBuf::from(request.path.trim());
        let candidate = if raw.is_absolute() {
            raw
        } else {
            current.join(raw)
        };
        let canonical = std::fs::canonicalize(&candidate).map_err(|_| {
            lingxi_core::host::HandleError::ActionFailed(
                "register_repo_root: target is not a directory".into(),
            )
        })?;
        if !canonical.is_dir() {
            return Err(lingxi_core::host::HandleError::ActionFailed(
                "register_repo_root: target is not a directory".into(),
            ));
        }

        let current_scope = current
            .parent()
            .and_then(|path| std::fs::canonicalize(path).ok())
            .unwrap_or_else(|| current.clone());
        let home_scope = dirs::home_dir().and_then(|path| std::fs::canonicalize(path).ok());
        if !canonical.starts_with(&current_scope)
            && !home_scope
                .as_ref()
                .is_some_and(|home| canonical.starts_with(home))
        {
            return Err(lingxi_core::host::HandleError::ActionFailed(
                "register_repo_root: target is outside the allowed registration scope".into(),
            ));
        }

        // Sandbox/file permission refresh FIRST.
        if !self.session_cwd.add_trusted_dir(canonical.clone()) {
            return Err(lingxi_core::host::HandleError::ActionFailed(
                "register_repo_root: target is already a registered working directory".into(),
            ));
        }
        // Then refresh MCP roots; hooks run only after both live consumers see
        // the new directory.
        if let Some(registry) = &self.mcp_registry {
            registry.add_root(canonical.clone());
            registry.notify_roots_list_changed_all().await;
        }

        let directory = canonical.to_string_lossy().into_owned();
        let hooks = self
            .fire_directory_added(&directory, "register_repo_root")
            .await;

        if request.reload_claude_md {
            // Native register_repo_root clears the eager cache; it does not
            // synthesize a nested traversal InstructionsLoaded event.
            self.clear_instruction_files();
        }
        let reload = if request.reload_skills || request.reload_plugins {
            if let Some(reloader) = &self.repo_root_reloader {
                reloader
                    .reload(lingxi_core::host::RepoRootReloadRequest {
                        root: canonical.clone(),
                        reload_skills: request.reload_skills,
                        reload_plugins: request.reload_plugins,
                    })
                    .await
            } else {
                lingxi_core::host::RepoRootReloadOutcome {
                    errors: vec![
                        "catalog reload unavailable in this runtime; repository root was registered"
                            .to_string(),
                    ],
                    ..lingxi_core::host::RepoRootReloadOutcome::default()
                }
            }
        } else {
            lingxi_core::host::RepoRootReloadOutcome::default()
        };
        for error in &reload.errors {
            tracing::warn!(%error, root = %canonical.display(), "register_repo_root reload failed");
        }

        Ok(lingxi_core::host::RegisterRepoRootOutcome {
            directory: canonical,
            added: true,
            hooks,
            skills_reloaded: reload.skills_reloaded,
            plugins_reloaded: reload.plugins_reloaded,
            reload_errors: reload.errors,
        })
    }

    /// Append a Stop hook's blocking reason as a *meta* user message so the
    /// model sees the hook feedback on the continued turn. Mirrors claude-code
    /// `query/stopHooks.ts:257-262`: each blocking error becomes
    /// `createUserMessage({ content: getStopHookMessage(blockingError), isMeta: true })`,
    /// where `getStopHookMessage` = `Stop hook feedback:\n${blockingError}`
    /// (`utils/hooks.ts:1895`). The `isMeta` flag hides it from the user-facing
    /// UI while keeping it in the API stream. Best-effort persist, like the
    /// other meta appends.
    async fn append_stop_hook_feedback(&self, reason: &str) {
        let content = format!("Stop hook feedback:\n{reason}");
        let msg = ConversationMessage::user_meta(MessageId::new(), content);
        {
            let mut s = self.session.lock().await;
            s.history.push(msg.clone());
        }
        self.persist_message_to_jsonl(&msg).await;
    }

    /// Append a Stop hook's `preventContinuation` stop-reason as a *meta* user
    /// message so the transcript records why continuation was halted. Mirrors
    /// claude-code `query/stopHooks.ts:269-280`: a `hook_stopped_continuation`
    /// attachment (hookName `Stop`) rendered as
    /// `<system-reminder>\nStop hook stopped continuation: {stopReason}\n</system-reminder>`
    /// (isMeta, `utils/messages.ts:4130-4137`). Best-effort persist, exactly like
    /// [`Self::append_stop_hook_feedback`].
    async fn append_stop_hook_stopped_continuation(&self, reason: &str) {
        // O2: the PERSISTED record is a `hook_stopped_continuation` attachment
        // (BIN off 233101239): `Va({type:"hook_stopped_continuation",
        // message:N, hookName:"Stop", toolUseID:H, hookEvent:"Stop"})`, where
        // `N = B.stopReason||"Stop hook prevented continuation"` and `H` is the
        // Stop dispatch's `hook-${randomUUID()}` — the SAME id shape the
        // dispatch's `hook_additional_context` record uses (BIN off 233240830).
        //
        // The derived meta message is kept in live API history but is not written
        // as a second JSONL row. Cold resume performs the same normalization from
        // this attachment, so the on-disk transcript has one source of truth.
        self.persist_hook_attachment_to_jsonl(
            hooks::stopped_continuation_attachment(
                &hooks::HookAttachmentIdentity {
                    hook_name: "Stop".to_string(),
                    hook_event: "Stop".to_string(),
                    tool_use_id: format!("hook-{}", lingxi_core::types::HookId::new().as_uuid()),
                },
                reason,
            ),
            Default::default(),
        )
        .await;
        let content = format!(
            "<system-reminder>\nStop hook stopped continuation: {reason}\n</system-reminder>"
        );
        let msg = ConversationMessage::user_meta(MessageId::new(), content);
        self.register_mod_persisted_attachment(
            &msg,
            "hook_stopped_continuation",
            serde_json::json!({"kind":"hook","event":"Stop"}),
        )
        .await;
        {
            let mut s = self.session.lock().await;
            s.history.push(msg);
        }
    }
}

#[async_trait::async_trait]
impl lingxi_core::host::uds_inbox::PeerReceiveGate for ConversationOrchestrator {
    async fn receive(
        &self,
        destination_session_id: &str,
        text: &str,
        commit: lingxi_core::host::uds_inbox::PeerQueueCommit,
    ) -> bool {
        let current = lingxi_core::host::OrchestratorHandle::current_session_id(self).await;
        if lingxi_core::types::SessionId::parse_prefixed(destination_session_id) != Some(current) {
            return false;
        }
        self.screen_mod_uds_receive(text, commit).await;
        true
    }
}

#[cfg(test)]
mod model_switch_metadata_tests {
    use super::*;
    use crate::test_support::{
        MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
        noop_hook_executor,
    };

    #[tokio::test]
    async fn metadata_counts_output_and_uses_target_profile_pricing() {
        let api = Arc::new(MockApiClient::new(Vec::new()));
        api.set_model_listings(vec![lingxi_core::host::ModelListing {
            display_model: "Example".to_string(),
            request_model: "shared-model".to_string(),
            provider_id: "example".to_string(),
            provider_label: "Example".to_string(),
            metadata: lingxi_core::host::ModelMetadata {
                pricing: Some(lingxi_core::host::ModelPricing {
                    billing_mode: lingxi_core::host::ModelBillingMode::PerToken,
                    cache_write_per_million: Some(2.5),
                    source: Some("userOverride".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        }]);
        let orch = ConversationOrchestrator::new(
            crate::OrchestratorConfig::default(),
            api,
            Arc::new(tool_api::ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            std::env::temp_dir(),
        );
        orch.compaction_runtime
            .last_response_input_tokens
            .store(1_000, std::sync::atomic::Ordering::Relaxed);
        orch.compaction_runtime
            .last_response_output_tokens
            .store(100, std::sync::atomic::Ordering::Relaxed);

        let (tokens, warm, ttl, estimate, pricing) = orch
            .model_switch_cache_metadata("shared-model", Some("example"))
            .await;

        assert_eq!(tokens, 1_100);
        assert!(!warm);
        assert!(matches!(ttl.as_str(), "5m" | "1h"));
        assert_eq!(estimate, 0.0028);
        assert_eq!(pricing, "configured");
    }
}
