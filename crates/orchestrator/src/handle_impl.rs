//! `impl OrchestratorHandle for ConversationOrchestrator`.
//!
//! M5-02 declared the trait; M5-10 lights up the implementation against the
//! real orchestrator state. The 5 original methods (`current_session_id`,
//! `clear_session`, `force_compact`, `snapshot_cost`, `switch_model`)
//! project a minimal coarse-grained view of orchestrator state. M5-10 adds
//! two new methods: `request_exit` and `open_memory_editor`.
//!
//! Behavioural notes:
//!
//! - `clear_session` wipes [`lingxi_core::SessionState::history`] and mints
//!   a fresh `SessionId`.
//! - `force_compact` drives the production `CompactionOrchestrator`, including
//!   the summarizer request, compact boundary, post-compact attachments and
//!   lifecycle hooks. An unwired compactor is an error rather than a fake
//!   zero-delta success.
//! - `request_exit` flips an `AtomicBool` on the orchestrator. The REPL
//!   (M5-13) reads this between turns and breaks out of the loop.
//! - `open_memory_editor` ensures `<config>/claude/LINGXI.md` exists, then
//!   spawns the user's `$EDITOR`. Falls back to `VISUAL`, then `vi`
//!   (Unix) / `notepad.exe` (Windows). Inherits `stdin`/`stdout`/`stderr`
//!   so TUI editors render correctly.

use crate::ConversationOrchestrator;
use async_trait::async_trait;
use hooks::mods::ModSessionContext;
use lingxi_core::host::{
    ActiveGoalSnapshot, AgentInfo, CompactionSummary, CostSnapshot, DoctorReport, ForkOutcome,
    HandleError, HookInfo, McpServerInfo, MemoryEditorOutcome, OrchestratorHandle, RecapOutcome,
    SkillInfo, StatusSnapshot,
};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::SystemTime;
use tokio::process::Command;

/// Keep the session's provider-local wire model separate from the
/// provider-qualified reference used by client pickers.
///
/// An explicit profile is useful routing evidence, but it does not make a
/// `profile/model` string a valid provider request model. When the catalog
/// proves that the reference's qualifier and the explicit profile describe the
/// same route, store only the provider-local remainder. Exact slash-bearing wire
/// ids such as OpenRouter's `openrouter/auto` do not parse as qualified refs and
/// therefore remain byte-identical.
fn normalize_session_model_ref(
    model: &str,
    explicit_profile: Option<&str>,
    listings: &[lingxi_core::host::ModelListing],
) -> (String, Option<String>) {
    let (parsed_model, parsed_profile) = lingxi_core::host::parse_model_ref(model, listings);
    match explicit_profile {
        Some(profile) if parsed_profile.as_deref() == Some(profile) => {
            (parsed_model, Some(profile.to_string()))
        }
        Some(profile) => (model.to_string(), Some(profile.to_string())),
        None => (parsed_model, parsed_profile),
    }
}

impl ConversationOrchestrator {
    async fn background_session_snapshot(
        &self,
    ) -> (
        lingxi_core::host::bg_session_forker::BgSessionSnapshot,
        String,
    ) {
        let session = self.session.lock().await;
        let attachments = self
            .transcript
            .model_reminder_attachments
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let model_reminder_attachments = session
            .history
            .iter()
            .filter_map(|message| {
                attachments
                    .get(&message.id())
                    .cloned()
                    .map(|attachment| (message.id(), attachment))
            })
            .collect();
        (
            lingxi_core::host::bg_session_forker::BgSessionSnapshot {
                history: session.history.clone(),
                model_reminder_attachments,
            },
            session.model.clone(),
        )
    }

    /// Reset state whose lifetime is one conversation rather than one process.
    /// Both `/clear` and in-place resume cross that boundary; keeping any of
    /// these caches would make the new session depend on the previously mounted
    /// transcript.
    async fn reset_session_scoped_runtime(&self) {
        // Retire session-owned async hook completions before clearing or
        // restoring the transcript. The token is cancelled before waiting for
        // already-admitted durable attachment commits, so stale producers are
        // rejected while accepted writes finish against the old session.
        self.lifecycle_runtime
            .session_tool_hook_generation
            .reset()
            .await;
        self.tools.reset_bash_precommit_prompt_session();
        // `/clear` and in-place resume can switch the session without a
        // separate SessionEnd dispatch. Apply the same state reset; it is
        // idempotent when SessionEnd already ran.
        if let Some(registry) = &self.lifecycle_runtime.hook_registry {
            if let Some(host) = registry.read().await.mod_host() {
                host.reset_session_state();
            }
        }
        self.reset_goal_interruption();
        let protocol_session_id = self.session.lock().await.session_id;
        let session_id = protocol_session_id.to_string();
        compaction::invoked_skills::clear_session(&session_id);
        session::clear_checkpoint_result_for_session(&session::checkpoint_session_key(
            protocol_session_id,
        ));
        self.compaction_runtime
            .reset_context_collapse_and_session_memory()
            .await;
        self.model_runtime.reset_cost_and_api_accounting().await;
        self.compaction_runtime.reset_token_accounting();
        self.model_runtime.reset_refusal_fallback();

        self.tools
            .deferral()
            .replace_loaded(std::iter::empty::<String>());
        self.prompt_runtime.reset_read_state();
        self.transcript.reset_session_scoped().await;
        self.orphan_forced_decisions.lock().await.clear();
        self.prompt_runtime.prepare_session_scoped_reset().await;
    }

    /// Apply a model selection with Claude Code's pre/post model-switch hook
    /// ordering. The pre hook is only valid for user/requested sources; post is
    /// best-effort because the model mutation has already happened.
    async fn switch_model_with_hook_source(
        &self,
        model: &str,
        profile: Option<&str>,
        source: &str,
    ) -> Result<(), HandleError> {
        // Unknown provenance must not become an accidental pre-hook bypass.
        // Legacy/third-party handle callers are SDK callers unless they use one
        // of the schema's explicit sources.
        let source = match source {
            "command" | "picker" | "sdk" | "auto" | "resume" => source,
            _ => "sdk",
        };
        // Preserve hook ordering between selections while allowing controls
        // during a running turn. In-flight requests keep their prepared model;
        // the next request snapshots the selection under the session lock.
        let _switch_guard = self.model_switch_gate.lock().await;
        let listings = self.api.list_model_listings();
        let (target_model, target_profile) = normalize_session_model_ref(model, profile, &listings);
        let (from_model, from_profile) = {
            let session = self.session.lock().await;
            (session.model.clone(), session.model_profile.clone())
        };
        if from_model == target_model && from_profile == target_profile {
            if matches!(source, "command" | "picker" | "sdk") {
                self.model_runtime
                    .refusal_selection
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .model_pick(&target_model);
                self.model_runtime
                    .refusal_cascade
                    .lock()
                    .await
                    .clear_target();
            }
            return Ok(());
        }

        if matches!(source, "command" | "picker" | "sdk") {
            self.model_runtime
                .refusal_selection
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .model_pick(&target_model);
            let aggregate = self
                .run_pre_model_switch_hooks(
                    &from_model,
                    &target_model,
                    Some(model),
                    target_profile.as_deref(),
                    source,
                )
                .await;
            match aggregate.decision {
                Some(hooks::HookDecision::Block) => {
                    return Err(HandleError::ActionFailed(
                        aggregate
                            .reason
                            .filter(|reason| !reason.is_empty())
                            .unwrap_or_else(|| "Model switch blocked by hook".to_string()),
                    ));
                }
                Some(hooks::HookDecision::Ask) => {
                    // OrchestratorHandle has no interactive confirmation
                    // channel. Treat an unresolved ask as a safe refusal rather
                    // than silently bypassing the hook's requested prompt.
                    return Err(HandleError::ActionFailed(
                        aggregate
                            .reason
                            .filter(|reason| !reason.is_empty())
                            .unwrap_or_else(|| "Model switch requires confirmation".to_string()),
                    ));
                }
                _ => {}
            }
        }

        if matches!(source, "command" | "picker" | "sdk") {
            self.model_runtime
                .refusal_cascade
                .lock()
                .await
                .clear_target();
        }
        {
            let mut session = self.session.lock().await;
            session.model = target_model.clone();
            session.model_profile = target_profile.clone();
        }
        self.model_runtime
            .refusal_selection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .override_model = Some(Some(target_model.clone()));
        self.run_post_model_switch_hooks(
            &from_model,
            &target_model,
            Some(model),
            target_profile.as_deref(),
            source,
        )
        .await;
        Ok(())
    }
}

enum OwnedSessionSwitch {
    Clear,
    Resume {
        session_id: lingxi_core::types::SessionId,
        history: Vec<lingxi_core::types::ConversationMessage>,
        last_jsonl_uuid: Option<String>,
        active_goal: Option<ActiveGoalSnapshot>,
        runtime: lingxi_core::host::ResumeRuntimeSnapshot,
    },
}

enum PreparedTranscriptSwitch {
    None,
    Legacy {
        writer: Arc<session::jsonl::writer::JsonlWriter>,
        path: std::path::PathBuf,
    },
    Durable {
        writer: Arc<session::jsonl::writer::JsonlWriter>,
        session_id: lingxi_core::types::SessionId,
        path: std::path::PathBuf,
        cwd: std::path::PathBuf,
        durable_lock: Arc<session::jsonl::DurableTranscriptWriter>,
    },
}

impl PreparedTranscriptSwitch {
    async fn prepare_legacy(&self) {
        if let Self::Legacy { writer, path } = self {
            writer.retarget(path.clone()).await;
        }
    }

    fn activate(self) {
        if let Self::Durable {
            writer,
            session_id,
            path,
            cwd,
            durable_lock,
        } = self
        {
            writer.activate_session_target_with_durable_lock(session_id, path, cwd, durable_lock);
        }
    }
}

impl ConversationOrchestrator {
    fn prepare_transcript_switch(
        &self,
        session_id: lingxi_core::types::SessionId,
        durable_lock: Option<Arc<session::jsonl::DurableTranscriptWriter>>,
    ) -> Result<PreparedTranscriptSwitch, HandleError> {
        let Some(writer) = self.transcript.jsonl_writer.as_ref().cloned() else {
            return Ok(PreparedTranscriptSwitch::None);
        };
        let Some(config_home) = self.config_home.as_ref() else {
            return Ok(PreparedTranscriptSwitch::None);
        };
        let cwd = self.current_cwd();
        let path = session::jsonl::path::session_path(
            config_home,
            &cwd.to_string_lossy(),
            &session_id.as_uuid().to_string(),
        );
        if writer.durable_transcript_enabled() {
            let durable_lock = durable_lock.ok_or_else(|| {
                HandleError::ActionFailed(
                    "durable session switch did not prepare its transcript authority".into(),
                )
            })?;
            Ok(PreparedTranscriptSwitch::Durable {
                writer,
                session_id,
                path,
                cwd,
                durable_lock,
            })
        } else {
            Ok(PreparedTranscriptSwitch::Legacy { writer, path })
        }
    }

    async fn run_owned_session_switch(
        &self,
        request: OwnedSessionSwitch,
    ) -> Result<(), HandleError> {
        let claim = self
            .lifecycle_runtime
            .session_switch_supervisor
            .claim()
            .map_err(HandleError::ActionFailed)?;
        let turn_guard = self.turn_gate.clone().lock_owned().await;
        let owner = self
            .lifecycle_runtime
            .session_switch_owner
            .get()
            .and_then(std::sync::Weak::upgrade);
        let Some(owner) = owner else {
            let result = self.execute_owned_session_switch(turn_guard, request).await;
            claim.complete(None);
            return result;
        };

        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            // The nested task turns a panic into a retained error while the
            // outer owner keeps the lifecycle claim until all borrowed state
            // and the turn gate have been released.
            let execution = tokio::spawn(async move {
                owner
                    .execute_owned_session_switch(turn_guard, request)
                    .await
            })
            .await;
            let result = match execution {
                Ok(result) => result,
                Err(error) => Err(HandleError::ActionFailed(format!(
                    "owned session switch failed: {error}"
                ))),
            };
            match result_tx.send(result) {
                Ok(()) => claim.complete(None),
                Err(result) => claim.complete(
                    result
                        .err()
                        .map(|error| format!("detached session switch failed: {error}")),
                ),
            }
        });
        result_rx.await.map_err(|_| {
            HandleError::ActionFailed("owned session switch result channel closed".into())
        })?
    }

    async fn execute_owned_session_switch(
        &self,
        turn_guard: tokio::sync::OwnedMutexGuard<()>,
        request: OwnedSessionSwitch,
    ) -> Result<(), HandleError> {
        // A model hook sequence must belong entirely to one session even
        // though live selections no longer acquire the turn gate.
        let _switch_guard = self.model_switch_gate.lock().await;
        match request {
            OwnedSessionSwitch::Clear => self.execute_clear_session(turn_guard).await,
            OwnedSessionSwitch::Resume {
                session_id,
                history,
                last_jsonl_uuid,
                active_goal,
                runtime,
            } => {
                self.execute_resume_session(
                    turn_guard,
                    session_id,
                    history,
                    last_jsonl_uuid,
                    active_goal,
                    runtime,
                )
                .await
            }
        }
    }

    async fn execute_clear_session(
        &self,
        _turn_guard: tokio::sync::OwnedMutexGuard<()>,
    ) -> Result<(), HandleError> {
        self.abort_startup_responses_websocket_prewarm();
        if let Err(err) = self.api.close_responses_websocket_session().await {
            tracing::warn!(error = %err, "failed to close responses websocket session during clear_session");
        }
        let new_session_id_value = lingxi_core::types::SessionId::new();
        let prepared = self
            .model_runtime
            .prepare_cost_session(new_session_id_value)
            .await
            .map_err(|error| {
                HandleError::ActionFailed(format!("Cost session preparation failed: {error}"))
            })?;
        let prepared_output = self
            .prepare_output_session(new_session_id_value)
            .await
            .map_err(|error| HandleError::ActionFailed(error.to_string()))?;
        let (prepared_cost, durable_lock, on_activated) =
            prepared.map_or((None, None, None), |prepared| {
                let (cost, durable_lock, on_activated) = prepared.into_parts();
                (Some(cost), durable_lock, on_activated)
            });
        let prepared_transcript =
            self.prepare_transcript_switch(new_session_id_value, durable_lock)?;
        prepared_transcript.prepare_legacy().await;
        self.compaction_runtime.roll_over_total_tokens_context();
        self.reset_session_scoped_runtime().await;
        let mut reports = self.main_reports.state.lock().await;
        let mut s = self.session.lock().await;
        let mut instruction_context = self.prompt_runtime.instruction_context.lock().await;
        let mut last_jsonl_uuid = self.transcript.last_jsonl_uuid.lock().await;
        let old_session_id = s.session_id;
        if let Some(registry) = &self.task_registry {
            registry.reset_agent_session_statistics(new_session_id_value, Some(old_session_id));
        }
        // All fallible and awaiting work is complete. Publish the captured
        // transcript authority, cost scope, and conversation state in one
        // synchronous critical section so caller cancellation cannot split it.
        prepared_transcript.activate();
        self.model_runtime.activate_cost_session(prepared_cost);
        self.install_output_session(prepared_output);
        s.history.clear();
        if let Some(restore) = self
            .model_runtime
            .refusal_selection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .session_transition()
        {
            s.model = restore
                .previous_app_state_model
                .flatten()
                .unwrap_or_else(|| self.config.model.clone());
            s.model_profile = restore.previous_profile;
        }
        s.compacted_user_turns = 0;
        s.virtual_user_messages.clear();
        s.transcript_only_messages.clear();
        s.compact_summary_messages.clear();
        s.model_context_excluded_messages.clear();
        s.active_goal = None;
        s.message_timing = lingxi_core::session::MessageTimingState::default();
        self.prompt_runtime
            .commit_instruction_session_reset(&mut instruction_context);
        s.session_id = new_session_id_value;
        reports.activate(new_session_id_value);
        let new_session_id = new_session_id_value.to_string();
        self.compaction_runtime
            .compaction_cumulative_dropped_tokens
            .store(0, std::sync::atomic::Ordering::Relaxed);
        *last_jsonl_uuid = None;
        drop(last_jsonl_uuid);
        drop(instruction_context);
        drop(s);
        drop(reports);
        if let Some(on_activated) = on_activated {
            on_activated();
        }
        self.notify_session_activated(old_session_id, new_session_id_value)
            .await;
        self.recover_main_reports_after_activation();
        self.invoked_skill_session_guard.replace(new_session_id);
        *self.compaction_runtime.compaction_tracking.lock().await =
            compaction::AutoCompactTrackingState::default();
        self.hooks.clear_session_hooks(old_session_id).await;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute_resume_session(
        &self,
        _turn_guard: tokio::sync::OwnedMutexGuard<()>,
        session_id: lingxi_core::types::SessionId,
        history: Vec<lingxi_core::types::ConversationMessage>,
        last_jsonl_uuid: Option<String>,
        active_goal: Option<ActiveGoalSnapshot>,
        runtime: lingxi_core::host::ResumeRuntimeSnapshot,
    ) -> Result<(), HandleError> {
        let (old_session_id, from_model, from_profile) = {
            let session = self.session.lock().await;
            (
                session.session_id,
                session.model.clone(),
                session.model_profile.clone(),
            )
        };
        let requested_model = (!runtime.model.is_empty()).then(|| runtime.model.clone());
        let (target_model, target_profile) = if runtime.model.is_empty() {
            let profile = from_profile.clone().or_else(|| {
                crate::query_model::server_session_serving_profile(&history, &from_model).flatten()
            });
            (from_model.clone(), profile)
        } else {
            let (model, mut profile) = normalize_session_model_ref(
                &runtime.model,
                runtime.model_profile.as_deref(),
                &self.api.list_model_listings(),
            );
            if profile.is_none() {
                if let Some(serving_profile) =
                    crate::query_model::server_session_serving_profile(&history, &model)
                {
                    profile = serving_profile;
                }
            }
            // Preview the existing refusal-restoration decision on a value
            // copy. A fork-neutralized fallback keeps the restored launch
            // route, and must not become the agent's parent route by accident.
            let mut refusal = self
                .model_runtime
                .refusal_selection
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            let restore = refusal.session_transition();
            let identity = lingxi_core::host::refusal_state::wire_identity;
            let adopts_model = refusal.restore(
                &model,
                &lingxi_core::host::refusal_state::frames(&history),
                false,
                Some(Some(self.config.model.clone())),
                |left, right| {
                    identity(left) == identity(right)
                        || llm_runtime::model::thinking::canonical(left)
                            == llm_runtime::model::thinking::canonical(right)
                },
                |left, right| identity(left) == identity(right),
            );
            if adopts_model {
                (model, profile)
            } else if let Some(restore) = restore {
                (
                    restore
                        .previous_app_state_model
                        .flatten()
                        .unwrap_or_else(|| self.config.model.clone()),
                    restore.previous_profile,
                )
            } else {
                (from_model.clone(), from_profile.clone())
            }
        };
        let prepared_agent = self
            .prepare_main_thread_agent_from_resume(
                runtime.main_thread_agent_type.clone(),
                runtime.main_thread_agent_definition.clone(),
                &target_model,
                target_profile.as_deref(),
            )
            .await
            .map_err(|error| {
                HandleError::ActionFailed(format!(
                    "Failed to restore main-thread agent model: {error}"
                ))
            })?;
        self.abort_startup_responses_websocket_prewarm();
        if let Err(err) = self.api.close_responses_websocket_session().await {
            tracing::warn!(error = %err, "failed to close responses websocket session during resume_session");
        }
        let prepared = self
            .model_runtime
            .prepare_cost_session(session_id)
            .await
            .map_err(|error| {
                HandleError::ActionFailed(format!("Cost session preparation failed: {error}"))
            })?;
        let prepared_output = self
            .prepare_output_session(session_id)
            .await
            .map_err(|error| HandleError::ActionFailed(error.to_string()))?;
        let (prepared_cost, durable_lock, on_activated) =
            prepared.map_or((None, None, None), |prepared| {
                let (cost, durable_lock, on_activated) = prepared.into_parts();
                (Some(cost), durable_lock, on_activated)
            });
        let prepared_transcript = self.prepare_transcript_switch(session_id, durable_lock)?;
        prepared_transcript.prepare_legacy().await;
        self.reset_session_scoped_runtime().await;
        self.restore_response_usage(runtime.current_usage);
        self.prompt_runtime
            .prompt_snapshot_resume
            .store(true, std::sync::atomic::Ordering::Release);
        *self.prompt_runtime.prompt_snapshot.lock().await = runtime.prompt_snapshot.clone();
        *self.prompt_runtime.context_rendering_hint.lock().await = runtime.context_rendering_hint;
        let mut reports = self.main_reports.state.lock().await;
        let mut s = self.session.lock().await;
        let mut instruction_context = self.prompt_runtime.instruction_context.lock().await;
        if let Some(registry) = &self.task_registry {
            registry.reset_agent_session_statistics(session_id, None);
        }
        prepared_transcript.activate();
        self.model_runtime.activate_cost_session(prepared_cost);
        self.install_output_session(prepared_output);
        s.history = history;
        if let Some(restore) = self
            .model_runtime
            .refusal_selection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .session_transition()
        {
            s.model = restore
                .previous_app_state_model
                .flatten()
                .unwrap_or_else(|| self.config.model.clone());
            s.model_profile = restore.previous_profile;
        }
        s.compacted_user_turns = runtime.compacted_user_turns;
        s.virtual_user_messages = runtime.virtual_user_message_ids.into_iter().collect();
        if !runtime.model.is_empty() {
            // `session.model` is the WIRE model id; the provider profile rides
            // beside it. A transcript can hand back a provider-QUALIFIED
            // reference either with no profile or with the matching profile
            // persisted beside the still-qualified model (an older engine,
            // another client, or a partially migrated session). Adopting that
            // verbatim ships `"deepseek/deepseek-flash"` as the wire id and
            // the provider rejects every message of the resumed session.
            // Re-split it exactly as `SetModel` does, so the resumed session
            // lands on the same (model, profile) pair a fresh pick produces.
            // `parse_model_ref` only splits a prefix a real listing claims, so
            // an unknown ref and an id whose own name contains a slash
            // (`openrouter/auto`) are both preserved.
            let listings = self.api.list_model_listings();
            let (model, mut profile) = normalize_session_model_ref(
                &runtime.model,
                runtime.model_profile.as_deref(),
                &listings,
            );
            if profile.is_none() {
                if let Some(serving_profile) =
                    crate::query_model::server_session_serving_profile(&s.history, &model)
                {
                    profile = serving_profile;
                }
            }
            if crate::query_model::restore_refusal_selection(self, Some(&model), &s.history, false)
            {
                s.model = model;
                s.model_profile = profile;
            }
        } else {
            // An empty hot-resume model snapshot means keep the live route.
            // A persisted refusal notice is not evidence for selecting its
            // fallback model; cold replay gets that from assistant rows.
            s.model.clone_from(&from_model);
            s.model_profile.clone_from(&from_profile);
            if crate::query_model::server_fallback_matches_model(&s.history, &from_model) {
                if s.model_profile.is_none() {
                    if let Some(profile) =
                        crate::query_model::server_session_serving_profile(&s.history, &from_model)
                    {
                        s.model_profile = profile;
                    }
                }
                crate::query_model::restore_refusal_selection(
                    self,
                    Some(&from_model),
                    &s.history,
                    false,
                );
            } else {
                crate::query_model::restore_refusal_selection(self, None, &s.history, false);
            }
        }
        s.transcript_only_messages = runtime.transcript_only_message_ids.into_iter().collect();
        s.compact_summary_messages = runtime.compact_summary_message_ids.into_iter().collect();
        s.model_context_excluded_messages = runtime
            .model_context_excluded_message_ids
            .into_iter()
            .collect();
        *self
            .transcript
            .post_compact_skill_attachments
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            runtime.post_compact_skill_attachments.into_iter().collect();
        *self
            .transcript
            .model_reminder_attachments
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            runtime.model_reminder_attachments.into_iter().collect();
        s.active_goal = active_goal.map(|goal| lingxi_core::session::ActiveGoalState {
            condition: goal.condition,
            set_at: goal.set_at,
            last_reason: goal.last_reason,
            iterations: goal.iterations,
            tokens_at_start: goal.tokens_at_start,
            // Adopting a goal along with a resumed session is `mon`'s case.
            origin: lingxi_core::session::GoalOrigin::Restored,
        });
        let resumed_effort = runtime.effort.clone();
        let resumed_reasoning = runtime.reasoning_selection.clone();
        let resumed_model = s.model.clone();
        let resumed_profile = s.model_profile.clone();
        self.prompt_runtime
            .commit_instruction_session_reset(&mut instruction_context);
        s.session_id = session_id;
        reports.activate(session_id);
        drop(instruction_context);
        drop(s);
        drop(reports);
        if let Some(on_activated) = on_activated {
            on_activated();
        }
        self.notify_session_activated(old_session_id, session_id)
            .await;
        self.recover_main_reports_after_activation();
        self.invoked_skill_session_guard
            .replace(session_id.to_string());
        if let Some(selection) = resumed_reasoning {
            self.restore_reasoning_selection_from_resume(
                &resumed_model,
                resumed_profile.as_deref(),
                selection,
            );
        } else if !runtime.model.is_empty() || resumed_effort.is_some() {
            self.restore_effort_from_resume(
                &resumed_model,
                resumed_profile.as_deref(),
                resumed_effort,
            );
        } else if self
            .model_runtime
            .current_effort_from_resume
            .load(std::sync::atomic::Ordering::Acquire)
        {
            // This resume carries no reasoning metadata at all — a legacy
            // transcript, or a default snapshot. Whatever is live came from an
            // EARLIER resume, so it belongs to a transcript this session is no
            // longer on and must not be inherited a second time. An application
            // default or an explicit choice is this session's own and is left
            // alone, which is the distinction `current_effort_from_resume`
            // exists to make: `current_effort_explicit` is false for both.
            self.restore_effort_from_resume(&resumed_model, resumed_profile.as_deref(), None);
            self.model_runtime
                .current_effort_from_resume
                .store(false, std::sync::atomic::Ordering::Release);
        }
        self.restore_main_thread_agent_from_resume(prepared_agent)
            .await;
        self.compaction_runtime
            .compaction_cumulative_dropped_tokens
            .store(
                runtime.cumulative_dropped_tokens,
                std::sync::atomic::Ordering::Relaxed,
            );
        *self.compaction_runtime.compaction_tracking.lock().await =
            compaction::AutoCompactTrackingState {
                compacted: runtime.compacted,
                turn_counter: runtime.turn_counter,
                turn_id: runtime.turn_id,
                consecutive_failures: runtime.consecutive_failures,
                consecutive_rapid_refills: runtime.consecutive_rapid_refills,
                ..Default::default()
            };
        self.tools
            .deferral()
            .replace_loaded(runtime.loaded_tool_names);
        *self.transcript.last_jsonl_uuid.lock().await = last_jsonl_uuid;
        // The latch resets with the session, and so does the cascade's
        // tried-models list (see `clear_session`).
        self.model_runtime
            .refusal_cascade
            .lock()
            .await
            .reset_routing();
        self.hooks.clear_session_hooks(old_session_id).await;
        let (to_model, to_profile) = {
            let session = self.session.lock().await;
            (session.model.clone(), session.model_profile.clone())
        };
        if from_model != to_model || from_profile != to_profile {
            self.run_post_model_switch_hooks(
                &from_model,
                &to_model,
                requested_model.as_deref(),
                to_profile.as_deref(),
                "resume",
            )
            .await;
        }
        self.sync_active_goal_stop_hook_for_current_state().await;
        if !runtime.deferred_tools.is_empty() {
            crate::resume::replay_deferred_tools_after_resume(self, runtime.deferred_tools)
                .await
                .map_err(|error| HandleError::ActionFailed(format!("deferred replay: {error}")))?;
        }
        Ok(())
    }

    async fn notify_session_activated(
        &self,
        previous: lingxi_core::types::SessionId,
        current: lingxi_core::types::SessionId,
    ) {
        if let Some(owner) = self.upgrade_streaming_tool_dispatch_owner() {
            owner.bind_subagent_stop_hook_owner(current);
        }
        let Some(observer) = self.lifecycle_runtime.session_activation_observer.as_ref() else {
            return;
        };
        if let Err(error) = observer.session_activated(previous, current).await {
            tracing::warn!(%error, "session committed but host presence did not follow it");
            let notice =
                format!("Session switched, but host presence could not be updated: {error}");
            let _ = tokio::time::timeout(
                std::time::Duration::from_millis(250),
                self.output.emit_system_notice(&notice, false),
            )
            .await;
        }
    }
}

#[async_trait]
impl OrchestratorHandle for ConversationOrchestrator {
    async fn current_session_id(&self) -> lingxi_core::types::SessionId {
        self.session.lock().await.session_id
    }

    async fn clear_session(&self) -> Result<(), HandleError> {
        self.run_owned_session_switch(OwnedSessionSwitch::Clear)
            .await
    }

    /// Adopt a replayed session IN PLACE — the symmetric twin of
    /// [`Self::clear_session`]. Where `clear_session` wipes the history and
    /// MINTS a fresh `SessionId`, `resume_session` ADOPTS the named on-disk
    /// session: it swaps in the replayed `history`, adopts the named
    /// `session_id` (so the running orchestrator IS the resumed session), and
    /// seeds the JSONL parent-uuid chain to `last_jsonl_uuid` so any future
    /// append chains via `parent_uuid` off the resumed tail (the same field
    /// `with_resume` overrides at construction time, here applied to a live
    /// orchestrator).
    ///
    /// When the replay carries a resolved model, it replaces `s.model` and its
    /// provider-profile hint. Legacy/default callers leave the live values
    /// unchanged by passing an empty model.
    async fn resume_session(
        &self,
        session_id: lingxi_core::types::SessionId,
        history: Vec<lingxi_core::types::ConversationMessage>,
        last_jsonl_uuid: Option<String>,
        active_goal: Option<ActiveGoalSnapshot>,
        runtime: lingxi_core::host::ResumeRuntimeSnapshot,
    ) -> Result<(), HandleError> {
        self.run_owned_session_switch(OwnedSessionSwitch::Resume {
            session_id,
            history,
            last_jsonl_uuid,
            active_goal,
            runtime,
        })
        .await
    }

    async fn force_compact(&self) -> Result<CompactionSummary, HandleError> {
        // M6-08: dispatch to the cancelable inherent method with a fresh
        // (un-cancelled) token. The REPL/TUI can call
        // `force_compact_with_cancel` directly to provide a Ctrl-C token.
        self.force_compact_with_cancel(tokio_util::sync::CancellationToken::new())
            .await
    }

    async fn force_compact_with_instructions(
        &self,
        custom_instructions: &str,
    ) -> Result<CompactionSummary, HandleError> {
        self.force_compact_with_instructions_and_cancel(
            Some(custom_instructions).filter(|s| !s.trim().is_empty()),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
    }

    async fn force_compact_with_instructions_and_cancel(
        &self,
        custom_instructions: Option<&str>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<CompactionSummary, HandleError> {
        ConversationOrchestrator::force_compact_with_instructions_and_cancel(
            self,
            custom_instructions,
            cancel,
        )
        .await
    }

    async fn summarize_at(
        &self,
        message_uuid: &str,
        user_context: Option<&str>,
        direction: lingxi_core::host::SummarizeDirection,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<CompactionSummary, HandleError> {
        ConversationOrchestrator::summarize_at(self, message_uuid, user_context, direction, cancel)
            .await
    }

    async fn get_active_goal(&self) -> Option<ActiveGoalSnapshot> {
        let s = self.session.lock().await;
        s.active_goal.as_ref().map(|goal| ActiveGoalSnapshot {
            condition: goal.condition.clone(),
            set_at: goal.set_at,
            last_reason: goal.last_reason.clone(),
            iterations: goal.iterations,
            tokens_at_start: goal.tokens_at_start,
        })
    }

    async fn set_active_goal(&self, condition: &str) {
        let tokens_at_start = self.snapshot_cost_real().await.total_tokens;
        // `let l = t.getAppState().activeGoal; if (l !== void 0) kB(l,"superseded")`
        // (`src_161508826.js`) — a goal replaced by a newer one is torn down as
        // surely as one that was cleared, and upstream accounts for it before
        // the overwrite so `iterations` / `durationMs` still describe the OLD
        // goal. Only the event fires here: the Stop hook is not removed, because
        // `sync_active_goal_stop_hook_for_current_state` below re-points it at
        // the replacement.
        let superseded = {
            let s = self.session.lock().await;
            s.active_goal.clone()
        };
        if let Some(superseded) = superseded.as_ref() {
            self.fire_goal_terminal_event(
                lingxi_core::host::GoalStatusKind::Cleared,
                superseded,
                Some(lingxi_core::host::GoalClearedReason::Superseded),
            )
            .await;
        }
        let mut s = self.session.lock().await;
        self.reset_goal_interruption();
        s.active_goal = Some(lingxi_core::session::ActiveGoalState {
            condition: condition.to_string(),
            set_at: SystemTime::now(),
            last_reason: None,
            iterations: 0,
            tokens_at_start,
            // `fxe`'s `origin: o`, where `y()` returned its `"user"` fallback.
            origin: lingxi_core::session::GoalOrigin::User,
        });
        let snapshot = s.active_goal.clone();
        drop(s);
        self.persist_active_goal_state_to_jsonl(snapshot.as_ref())
            .await;
        // `rRe`: `i("tengu_stop_hook_added",{promptLength,via:_("goal"),origin})`
        // (`src_160454523.js`). `origin` is always `"user"` here — see
        // `fire_goal_terminal_event`.
        if let Some(bus) = self.model_runtime.analytics_bus.as_ref() {
            let mut metadata = telemetry::LogEventMetadata::new();
            metadata.insert(
                "promptLength".into(),
                telemetry::AnalyticsValue::Int(condition.encode_utf16().count() as i64),
            );
            metadata.insert(
                "via".into(),
                telemetry::AnalyticsValue::String("goal".to_string()),
            );
            metadata.insert(
                "origin".into(),
                telemetry::AnalyticsValue::String("user".to_string()),
            );
            bus.log_event("tengu_stop_hook_added", metadata).await;
        }
        self.sync_active_goal_stop_hook_for_current_state().await;
    }

    async fn clear_active_goal(&self) -> Option<ActiveGoalSnapshot> {
        // The `/goal clear` arm — upstream's `kB(n,"user_clear")`.
        self.clear_active_goal_state_and_hook(lingxi_core::host::GoalClearedReason::UserClear)
            .await
    }

    async fn set_active_goal_last_reason(&self, reason: Option<String>) {
        let mut s = self.session.lock().await;
        if let Some(goal) = s.active_goal.as_mut() {
            goal.last_reason = reason;
        }
        let snapshot = s.active_goal.clone();
        drop(s);
        if snapshot.is_some() {
            self.persist_active_goal_state_to_jsonl(snapshot.as_ref())
                .await;
        }
    }

    async fn workspace_trusted(&self) -> bool {
        self.workspace_trusted
    }

    async fn hooks_restricted(&self) -> bool {
        self.hooks_restricted
    }

    /// `/fork` — spawn a DETACHED background agent that inherits the
    /// conversation. Reads the transcript tail (last assistant message) to build
    /// the cache-safe fork prefix, then dispatches through the wired
    /// `SubagentSpawner::spawn_async` (`run_in_background = true`) and returns
    /// immediately with the spawned agent's synthesized name + id. No turn-loop
    /// entanglement.
    ///
    /// Requires both a wired fork spawner (composition root's
    /// `BackgroundAgentSpawner`) and a budget enforcer; either missing ⇒ a clear
    /// `ActionFailed` (the `/fork` handler renders "Could not fork
    /// conversation: …"). The handler already gates the "no first turn yet"
    /// case, but this also checks defensively.
    async fn fork_conversation(&self, directive: &str) -> Result<ForkOutcome, HandleError> {
        let spawner = self
            .fork_spawner
            .as_ref()
            .ok_or_else(|| HandleError::ActionFailed("fork: no spawner wired".into()))?;
        let budget = self
            .fork_budget
            .clone()
            .ok_or_else(|| HandleError::ActionFailed("fork: no budget wired".into()))?;

        // Keep the complete parent history, including lazy instruction bodies.
        let (history, origin_session_id) = {
            let s = self.session.lock().await;
            (s.history.clone(), s.session_id)
        };
        if !history.iter().any(|message| {
            matches!(
                message,
                lingxi_core::types::ConversationMessage::Assistant { .. }
            )
        }) {
            return Err(HandleError::ActionFailed(
                "fork: no assistant turn to fork from".into(),
            ));
        }

        let fork_msgs = lingxi_core::host::fork_subagent::build_forked_context(directive, &history);
        // The parent's rendered system-prompt bytes for a cache-identical child
        // prefix (`None` until the first successful turn).
        let parent_sys = self.current_turn_system_prompt().await;

        // Synthesize a short, stable display codename. Reused for BOTH the spawn
        // request's `name` (so the background spawner registers it for
        // `SendMessage` routing) and the returned `ForkOutcome.name` (rendered
        // as "⑂ forked {name} ({id-tail})"). AsyncLaunch carries no name.
        let codename = format!("fork-{}", &uuid::Uuid::new_v4().simple().to_string()[..4]);

        let request = lingxi_core::host::subagent_spawn::SubagentSpawnRequest {
            agent_spawn_token: None,
            stop_hook_scope: lingxi_core::host::subagent_spawn::SubagentStopScope::Session,
            agent_spawn_provenance: Default::default(),
            teammate_color: None,
            subagent_type: lingxi_core::host::fork_subagent::FORK_SUBAGENT_TYPE.to_string(),
            origin_session_id: Some(origin_session_id),
            // BackgroundAgentSpawner carries the complete request into
            // LocalAgent; the runner seeds the fork prefix below. Keep the
            // directive as task metadata beside that model-facing prefix.
            prompt: directive.to_string(),
            observer: None,
            context_paths: Vec::new(),
            description: Some(directive.to_string()),
            model: None,
            model_profile: None,
            run_in_background: true,
            name: Some(codename.clone()),
            team_name: None,
            creator_teammate_name: None,
            creator_team_name: None,
            creator_agent_id: None,
            mode: None,
            isolation: None,
            cwd: None,
            worktree: None,
            fork_context_messages: Some(fork_msgs),
            instruction_context: Some(self.instruction_context_snapshot().await),
            fork_parent_system_prompt: parent_sys,
            schema: None,
            structured_output_mode: Default::default(),
            structured_output_parse_retries: 0,
            effort: None,
            tool_use_id: None,
            system_prompt_override: None,
            system_prompt_addendum: None,
            additional_disallowed_tools: Vec::new(),
            depth: 0,
            parent_model_override: None,
            parent_model_profile_override: None,
            forked_skill_name: None,
            forked_skill_attribution: None,
            forked_skill_effort: None,
            frozen_command_denies: Vec::new(),
            resumed_history: None,
            max_turns_override: None,
            max_output_tokens_per_turn: None,
            max_input_bytes_per_turn: None,
            query_source_label: None,
            correlation_id: None,
            model_attempt: None,
            // Detached /fork inherits a conversation and is outside the
            // ordinary Agent handback opt-in, even under an auto parent.
            handback_opt_in: false,
            parent_permission_mode: self.permission_mode(),
            handback_enabled: None,
            handback_ends_turn_enabled: None,
            restored_handback_state: None,
            restored_handback_history: Vec::new(),
            restore_handback_start: None,
        };

        let invoker: Arc<dyn lingxi_core::host::tool_invoker::ToolInvoker> = Arc::new(
            tool_api::tool_invoker_impl::RegistryToolInvoker::new(self.tools.clone()),
        );
        let inherit = lingxi_core::host::subagent_spawn::SubagentInheritance {
            tool_invoker: invoker,
            budget,
        };

        let launch = spawner
            .spawn_async(request, inherit)
            .await
            .map_err(|e| HandleError::ActionFailed(e.to_string()))?;

        Ok(ForkOutcome {
            name: codename,
            agent_id: launch.agent_id.to_string(),
        })
    }

    /// `/fork` (2.1.212 `vAd`) — copy the CURRENT conversation into a NEW
    /// BACKGROUND session and keep the interactive session live. Reads the live
    /// history + the parent's rendered system prompt and hands them to the
    /// injected [`lingxi_core::host::bg_session_forker::BgSessionForker`] seam (the CLI
    /// composition root's `CliBgSessionForker`), which snapshots the copy into
    /// the new session's transcript and dispatches a detached daemon worker that
    /// resumes it. Returns the system line for the live session (the seam owns
    /// the exact text — it mints the new short id). No forker wired (tests /
    /// non-desktop roots) ⇒ a clear `ActionFailed`.
    async fn fork_to_background_session(&self, prompt: &str) -> Result<String, HandleError> {
        let forker = self.bg_session_forker.as_ref().ok_or_else(|| {
            HandleError::ActionFailed("fork: no background-session forker wired".into())
        })?;
        // Snapshot the live conversation + capture the parent's CURRENT active
        // model. The clone releases the session lock before the (potentially
        // slow) dispatch. The model is threaded into the snapshot so the forked
        // background session resumes on the parent's model (e.g. after `/model
        // sonnet`, or a cross-provider model) instead of the `DEFAULT_MODEL`
        // seed — the copied assistant lines otherwise carry no `model` for
        // `state_from_messages` to restore.
        let (mut conversation, mut model) = self.background_session_snapshot().await;
        lingxi_core::host::refusal_state::neutralize_fork(&mut conversation.history);
        if let Some(latch) = self
            .model_runtime
            .refusal_selection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .live_latch()
        {
            model = latch
                .previous_app_state_model
                .clone()
                .flatten()
                .unwrap_or_else(|| self.config.model.clone());
        }
        // The parent's rendered system-prompt bytes (`None` until the first
        // successful turn) so the copy carries a cache-identical prefix.
        let system_prompt = self
            .current_turn_system_prompt()
            .await
            .map(|s| Arc::from(s.as_str()));
        forker
            .fork_to_background(&conversation, system_prompt, prompt, &model)
            .await
            .map_err(|e| HandleError::ActionFailed(e.to_string()))
    }

    async fn can_background_conversation_on_exit(&self) -> bool {
        if self.bg_session_forker.is_none()
            || !lingxi_core::host::agent_view::is_enabled()
            || std::env::var("LINGXI_DISABLE_ADOPT").is_ok_and(|value| !value.is_empty())
        {
            return false;
        }
        self.session.lock().await.history.iter().any(|message| {
            matches!(
                message,
                lingxi_core::types::ConversationMessage::User { .. }
            ) && !message.is_meta()
                && !message.text_content().trim().is_empty()
        })
    }

    async fn background_conversation(
        &self,
        snapshot: lingxi_core::host::BackgroundingSnapshot,
    ) -> Result<String, HandleError> {
        let forker = self.bg_session_forker.as_ref().ok_or_else(|| {
            HandleError::ActionFailed(
                "Cannot open agents — session persistence is disabled, so this conversation cannot be backgrounded."
                    .into(),
            )
        })?;

        let (mut conversation, model) = self.background_session_snapshot().await;
        let partial = snapshot.partial_text();
        if !partial.is_empty() {
            conversation
                .history
                .push(lingxi_core::types::ConversationMessage::Assistant { per_turn_effort: None,
                    id: lingxi_core::types::MessageId::new(),
                    content: vec![lingxi_core::types::ContentBlock::Text {
                        text: partial.to_string(),
                        citations: None,
                    }],
                    stop_reason: Some("background_requested".to_string()),
                });
        }

        let system_prompt = self
            .current_turn_system_prompt()
            .await
            .map(|prompt| Arc::from(prompt.as_str()));
        let continuation = if matches!(
            snapshot,
            lingxi_core::host::BackgroundingSnapshot::Idle { .. }
        ) {
            ""
        } else {
            "Continue the interrupted turn from the backgrounding boundary. Preserve the user's intent and safely restart any interrupted work."
        };
        forker
            .background_conversation(
                &conversation,
                system_prompt,
                continuation,
                &model,
                &snapshot,
            )
            .await
            .map_err(|error| HandleError::ActionFailed(error.to_string()))
    }

    /// `/resume`-as-background (2.1.212, G06) — launch an EXISTING on-disk
    /// session (one enumerated by [`Self::list_resumable_sessions`]) as a NEW
    /// background session, via the same injected `BgSessionForker` seam. Unlike
    /// [`Self::fork_to_background_session`] there is no live conversation to
    /// snapshot — the session already exists, so the seam dispatches a detached
    /// worker that resumes it directly. No forker wired ⇒ a clear `ActionFailed`.
    async fn resume_to_background_session(&self, session_id: &str) -> Result<String, HandleError> {
        let forker = self.bg_session_forker.as_ref().ok_or_else(|| {
            HandleError::ActionFailed("resume: no background-session forker wired".into())
        })?;
        forker
            .resume_to_background(session_id)
            .await
            .map_err(|e| HandleError::ActionFailed(e.to_string()))
    }

    /// `/recap` — delegate to the history-inert inherent
    /// [`ConversationOrchestrator::generate_recap_query`] with a fresh
    /// (un-cancelled) token. A cancel-carrying caller (future Ctrl-C wiring in
    /// command dispatch) can call `generate_recap_query` directly. This mirrors
    /// `force_compact`'s fresh-token delegation to `force_compact_with_cancel`.
    async fn generate_recap(&self) -> Result<RecapOutcome, HandleError> {
        self.generate_recap_query(tokio_util::sync::CancellationToken::new())
            .await
    }

    async fn generate_session_name(&self) -> Result<Option<String>, HandleError> {
        self.generate_session_name_query(tokio_util::sync::CancellationToken::new())
            .await
    }

    async fn rewind_rows(&self) -> Vec<lingxi_core::host::RewindRowData> {
        let Some(fh) = self.file_history.as_ref() else {
            return Vec::new();
        };
        let history = self.session.lock().await.history.clone();
        let mut rows = Vec::new();
        let mut turn = 0u32;
        for msg in &history {
            let uuid = msg.id().as_uuid();
            // Only turns with a checkpoint are restore points (make_snapshot runs
            // once per real user turn, so meta/assistant messages are excluded).
            if !fh.can_restore(uuid) {
                continue;
            }
            turn += 1;
            let preview: String = msg
                .text_content()
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(80)
                .collect();
            let has_code_changes = fh.has_any_changes(uuid).await;
            rows.push(lingxi_core::host::RewindRowData {
                message_uuid: uuid,
                preview,
                timestamp_label: format!("turn {turn}"),
                has_code_changes,
            });
        }
        rows
    }

    /// `/btw` — delegate to the history-inert inherent
    /// [`ConversationOrchestrator::answer_side_question_query`] with a fresh
    /// (un-cancelled) token, mirroring `generate_recap`'s delegation.
    async fn answer_side_question(&self, question: &str) -> Result<RecapOutcome, HandleError> {
        self.answer_side_question_query(question, tokio_util::sync::CancellationToken::new())
            .await
    }

    /// `/rename <name>` — append a user-set `custom-title` line to this
    /// session's transcript (1:1 with claude-code `saveCustomTitle`). The
    /// `sessionId` field is the BARE uuid (the `<uuid>.jsonl` stem the loader
    /// keys `custom_titles` by). No-op Ok when no writer is wired.
    async fn rename_session(&self, name: String) -> Result<(), HandleError> {
        let Some(writer) = self.transcript.jsonl_writer.as_ref() else {
            return Ok(());
        };
        let session_id = self.session.lock().await.session_id;
        writer
            .append_custom_title(&session_id.as_uuid().to_string(), &name)
            .await
            .map_err(|e| HandleError::ActionFailed(e.to_string()))
    }

    async fn snapshot_cost(&self) -> CostSnapshot {
        // M6-06: delegate to the inherent helper that reads from the wired
        // CostTracker. Returns zero-valued snapshot if no tracker is wired
        // (library callers — production CLI always wires one via init.rs).
        self.snapshot_cost_real().await
    }

    /// Oracle `htf()` — recomputed on demand, against the LIVE memory set and
    /// the LIVE model.
    ///
    /// Reads through `self.memory`, the same provider the system prompt's
    /// memory block is built from, so every membership gate (the
    /// `LINGXI_DISABLE_LINGXI_MDS` kill switch, the Managed tier, `@import`
    /// expansion, the 4 MiB skip) belongs to the provider rather than being
    /// restated here.
    async fn large_memory_warnings(&self) -> Option<Vec<String>> {
        let (model, profile, cwd) = {
            let session = self.session.lock().await;
            (
                session.model.clone(),
                session.model_profile.clone(),
                self.session_cwd.cwd(),
            )
        };
        let files = self.instruction_file_snapshot().await.ok()?;
        if files.is_empty() {
            return Some(Vec::new());
        }
        let model = match self.model_resolution_context_provider.as_ref() {
            Some(provider) => {
                let selection = provider
                    .context_for_route(&model, profile.as_deref())
                    .and_then(|context| {
                        agent::model_resolution::resolve_user_model_selection(
                            &model,
                            profile.as_deref(),
                            &context,
                            provider.as_ref(),
                        )
                    });
                match selection {
                    Ok(selection) => selection.model,
                    Err(error) => {
                        tracing::warn!(%model, ?profile, %error, "memory warning model route could not be resolved");
                        return None;
                    }
                }
            }
            None if agent::model_resolution::is_relative_model_alias(&model) => {
                tracing::warn!(%model, ?profile, "memory warning model alias has no configured route resolver");
                return None;
            }
            None => model,
        };
        let active_betas = self.api.active_betas();
        Some(crate::prompt::large_memory_warning_rows(
            &files,
            &cwd,
            dirs::home_dir().as_deref(),
            &model,
            &active_betas,
        ))
    }

    async fn active_betas(&self) -> Vec<String> {
        self.api.active_betas()
    }

    async fn switch_model(&self, model: &str, profile: Option<&str>) -> Result<(), HandleError> {
        self.switch_model_with_hook_source(model, profile, "sdk")
            .await
    }

    async fn switch_model_with_source(
        &self,
        model: &str,
        profile: Option<&str>,
        source: &str,
    ) -> Result<(), HandleError> {
        self.switch_model_with_hook_source(model, profile, source)
            .await
    }

    async fn fast_mode(&self) -> bool {
        self.fast_mode.load(Ordering::SeqCst)
    }

    async fn set_fast_mode(&self, on: bool) -> Result<(), HandleError> {
        if on {
            let (model, profile) = {
                let session = self.session.lock().await;
                (session.model.clone(), session.model_profile.clone())
            };
            self.api
                .validate_fast_enable(&model, profile.as_deref())
                .await
                .map_err(|error| {
                    HandleError::ActionFailed(llm_runtime::error::error_display_text(&error))
                })?;
            lingxi_core::host::fast_mode::ModelRejections::for_process().rearm();
        }
        self.fast_mode.store(on, Ordering::SeqCst);
        Ok(())
    }

    fn set_mod_ui_selection(&self, selection: Option<lingxi_core::host::ModUiSelection>) {
        *self
            .mod_ui_selection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = selection;
    }

    async fn mod_describe_command(&self, input: serde_json::Value) -> serde_json::Value {
        self.apply_mod_command_description(input).await
    }

    async fn mod_ui_render(
        &self,
        input: serde_json::Value,
        ui_revision: u64,
    ) -> Result<serde_json::Value, HandleError> {
        let Some(registry) = &self.lifecycle_runtime.hook_registry else {
            return Ok(serde_json::Value::Null);
        };
        let Some(host) = registry.read().await.mod_host() else {
            return Ok(serde_json::Value::Null);
        };
        let session_id = self.id().await;
        let registration_revision = host
            .begin_mod_ui_render(&session_id, &input, ui_revision)
            .map_err(|error| HandleError::ActionFailed(error.to_string()))?;
        let log_output = self.output.clone();
        let toast_output = self.output.clone();
        let status_output = self.output.clone();
        let tree = host.dispatch_with_ui_at_session(
            "ui.render",
            input.clone(),
            self,
            |event| async move {
                Ok(serde_json::json!({
                    "type":"engine",
                    "ref":format!("{}:{}:{}", event["surface"], event["component"], event["requestId"])
                }))
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
        .map_err(|error| HandleError::ActionFailed(error.to_string()))?;
        host.finish_mod_ui_render(
            &session_id,
            &input,
            ui_revision,
            registration_revision,
            &tree,
        )
        .map_err(|error| HandleError::ActionFailed(error.to_string()))?;
        Ok(tree)
    }

    async fn mod_ui_control(
        &self,
        request: lingxi_core::types::utf16_json::Utf16JsonProjection,
    ) -> Result<lingxi_core::host::orchestrator::ModUiControlOutcome, HandleError> {
        let request = hooks::mods::normalize_mod_ui_control_request(request)
            .map_err(|error| HandleError::ActionFailed(error.to_string()))?;
        let host = match &self.lifecycle_runtime.hook_registry {
            Some(registry) => registry.read().await.mod_host(),
            None => None,
        };
        let Some(host) = host else {
            let response = match request
                .value
                .get("subtype")
                .and_then(serde_json::Value::as_str)
            {
                Some("ui_render") => {
                    let mut response = lingxi_core::types::utf16_json::Utf16JsonProjection::plain(
                        serde_json::json!({
                            "tree": null,
                            "rewritten": false,
                            "hooked": false
                        }),
                    );
                    let props = request.subprojection("/props").unwrap_or_else(|_| {
                        lingxi_core::types::utf16_json::Utf16JsonProjection::plain(
                            serde_json::Value::Null,
                        )
                    });
                    response
                        .set_field("props", props)
                        .map_err(|error| HandleError::ActionFailed(error.to_string()))?;
                    response
                }
                Some("ui_client_module") => {
                    lingxi_core::types::utf16_json::Utf16JsonProjection::plain(
                        serde_json::Value::Null,
                    )
                }
                Some(
                    "ui_message" | "ui_client_fault" | "ui_client_press" | "ui_press" | "ui_input"
                    | "ui_select",
                ) => lingxi_core::types::utf16_json::Utf16JsonProjection::plain(
                    serde_json::json!({"handled": false}),
                ),
                _ => return Err(HandleError::ActionFailed("Unknown Mod UI control".into())),
            };
            return Ok(lingxi_core::host::orchestrator::ModUiControlOutcome {
                response,
                render_revision: None,
                client_runtime_epochs: std::collections::BTreeMap::new(),
                client_state_token: None,
            });
        };
        host.dispatch_client_ui_control(request, self)
            .await
            .map_err(|error| HandleError::ActionFailed(error.to_string()))
    }

    async fn mod_ui_client_operation(
        &self,
        operation: lingxi_core::types::utf16_json::Utf16JsonProjection,
    ) -> Result<lingxi_core::types::utf16_json::Utf16JsonProjection, HandleError> {
        let host = match &self.lifecycle_runtime.hook_registry {
            Some(registry) => registry.read().await.mod_host(),
            None => None,
        }
        .ok_or_else(|| HandleError::ActionFailed("The session has no Mod Client runtime".into()))?;
        host.dispatch_client_ui_operation(operation, self)
            .await
            .map_err(|error| HandleError::ActionFailed(error.to_string()))
    }

    async fn mod_ui_press(
        &self,
        input: serde_json::Value,
        ui_revision: u64,
    ) -> Result<serde_json::Value, HandleError> {
        let Some(registry) = &self.lifecycle_runtime.hook_registry else {
            return Ok(serde_json::json!({"handled":false}));
        };
        let Some(host) = registry.read().await.mod_host() else {
            return Ok(serde_json::json!({"handled":false}));
        };
        let log_output = self.output.clone();
        let toast_output = self.output.clone();
        let status_output = self.output.clone();
        host.dispatch_ui_press_at_session(
            input,
            ui_revision,
            self,
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
        .map_err(|error| HandleError::ActionFailed(error.to_string()))
    }

    async fn mod_ui_clear_press_actions(&self, ui_revision: u64) -> Result<(), HandleError> {
        let Some(registry) = &self.lifecycle_runtime.hook_registry else {
            return Ok(());
        };
        let Some(host) = registry.read().await.mod_host() else {
            return Ok(());
        };
        host.clear_mod_ui_press_actions(&self.id().await, ui_revision)
            .map_err(|error| HandleError::ActionFailed(error.to_string()))
    }

    async fn mod_ui_render_generation(&self) -> u64 {
        let Some(registry) = &self.lifecycle_runtime.hook_registry else {
            return 0;
        };
        registry
            .read()
            .await
            .mod_host()
            .map_or(0, |host| host.ui_render_generation())
    }

    async fn plan_mode(&self) -> bool {
        self.session.lock().await.plan_mode
    }

    async fn set_plan_mode(&self, on: bool) -> Result<(), HandleError> {
        let mut s = self.session.lock().await;
        s.plan_mode = on;
        // Entering plan mode replays the FULL (206 `LU_`) reminder: reset the
        // full-vs-sparse tracker so the next turn injects `full` before
        // switching to `sparse` (206 `reminderType`). Leaving plan mode need not
        // touch it (it's re-armed on the next entry).
        if on {
            s.plan_reminder_shown = false;
        }
        Ok(())
    }

    async fn current_plan(&self) -> Result<Option<lingxi_core::host::PlanSnapshot>, HandleError> {
        let session_id = self.session.lock().await.session_id;
        let path = std::path::PathBuf::from(ConversationOrchestrator::plan_file_path(
            &session_id,
            &self.cwd,
            self.config.plans_directory.as_deref(),
        ));
        match tokio::fs::read_to_string(&path).await {
            Ok(content) => Ok(Some(lingxi_core::host::PlanSnapshot { path, content })),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(HandleError::ActionFailed(format!(
                "could not read plan {}: {error}",
                path.display()
            ))),
        }
    }

    async fn open_plan_editor(&self) -> Result<MemoryEditorOutcome, HandleError> {
        let session_id = self.session.lock().await.session_id;
        let path = std::path::PathBuf::from(ConversationOrchestrator::plan_file_path(
            &session_id,
            &self.cwd,
            self.config.plans_directory.as_deref(),
        ));
        spawn_editor_on(path, "").await
    }

    async fn current_effort(&self) -> Option<String> {
        self.model_runtime
            .current_effort
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    async fn effort_command_snapshot(
        &self,
    ) -> Result<Option<lingxi_core::host::effort::EffortCommandSnapshot>, HandleError> {
        let session = self.session.lock().await;
        let mut snapshot = self
            .api
            .effort_command_snapshot(&session.model, session.model_profile.as_deref())
            .map_err(|error| HandleError::ActionFailed(error.to_string()))?;
        if let Some(snapshot) = &mut snapshot {
            snapshot.save_default = self.prompt_is_interactive();
            if snapshot.user_settings_path.is_none() {
                snapshot.user_settings_path = self
                    .config_home
                    .as_ref()
                    .map(|home| home.join("settings.json"));
            }
        }
        Ok(snapshot)
    }

    async fn ultracode_enabled(&self) -> bool {
        self.model_runtime
            .ultracode_enabled
            .load(std::sync::atomic::Ordering::Acquire)
    }
    async fn set_ultracode_enabled(&self, enabled: bool) -> Result<(), HandleError> {
        self.model_runtime
            .ultracode_enabled
            .store(enabled, std::sync::atomic::Ordering::Release);
        Ok(())
    }

    async fn dynamic_workflows_enabled(&self) -> bool {
        self.dynamic_workflows_gate.enabled()
    }

    async fn dynamic_workflows_managed(&self) -> bool {
        self.dynamic_workflows_gate.managed()
    }

    async fn workflow_size_guideline(&self) -> String {
        self.workflow_size_guideline.value().to_string()
    }

    async fn workflow_size_guideline_managed(&self) -> bool {
        self.workflow_size_guideline.managed()
    }

    async fn workflow_size_guideline_state(
        &self,
    ) -> lingxi_core::host::session_flags::WorkflowSizeGuidelineSnapshot {
        self.workflow_size_guideline.snapshot()
    }

    async fn workflow_size_guideline_is_default(&self) -> bool {
        self.workflow_size_guideline.is_default()
    }

    async fn set_dynamic_workflows_enabled(
        &self,
        enabled: bool,
        managed: bool,
    ) -> Result<(), HandleError> {
        self.dynamic_workflows_gate.set(enabled, managed);
        Ok(())
    }

    async fn set_workflow_size_guideline(
        &self,
        value: String,
        managed: bool,
        is_default: bool,
    ) -> Result<(), HandleError> {
        if !self
            .workflow_size_guideline
            .set_with_source(&value, managed, is_default)
        {
            return Err(HandleError::ActionFailed(format!(
                "invalid workflowSizeGuideline: {value}"
            )));
        }
        Ok(())
    }

    async fn set_session_effort(
        &self,
        effort: lingxi_core::host::effort_table::SessionEffort,
    ) -> Result<(), HandleError> {
        use lingxi_core::host::effort_table::SessionEffort;
        let _session = self.session.lock().await;
        self.model_runtime.current_effort_explicit.store(
            !matches!(effort, SessionEffort::Inherit),
            std::sync::atomic::Ordering::Release,
        );
        let level = match &effort {
            SessionEffort::Level(serde_json::Value::String(level)) => Some(level.clone()),
            _ => None,
        };
        *self
            .model_runtime
            .current_effort
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = level.clone();
        *self
            .model_runtime
            .current_reasoning_selection
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = level
            .map(|id| lingxi_core::host::ReasoningSelection::Level { id })
            .unwrap_or(lingxi_core::host::ReasoningSelection::Automatic);
        self.api.set_effort(effort);
        Ok(())
    }

    async fn reasoning_default_settings_path(&self) -> Option<std::path::PathBuf> {
        self.config_home
            .as_ref()
            .map(|home| home.join("settings.json"))
    }

    async fn output_styles(&self) -> Option<lingxi_core::host::OutputStyleListing> {
        let mut styles: Vec<(String, Option<String>)> = vec![(
            // Upstream's style table maps `default` to `null`, so the listing
            // renders it with no description. Keeping that shape here means the
            // rendered line matches without the renderer special-casing a name.
            outputstyles::DEFAULT_OUTPUT_STYLE_NAME.to_string(),
            None,
        )];
        for builtin in outputstyles::BUILTIN_OUTPUT_STYLES {
            styles.push((
                builtin.name.to_string(),
                Some(builtin.description.to_string()),
            ));
        }
        for dir in &self.config.output_style_dirs {
            for style in outputstyles::load_output_styles_from_dir(dir) {
                if styles.iter().any(|(name, _)| *name == style.name) {
                    continue;
                }
                let description = (!style.description.is_empty()).then_some(style.description);
                styles.push((style.name, description));
            }
        }
        if let Some(registry) = &self.prompt_runtime.output_style_registry {
            for name in registry.read().await.names() {
                if styles.iter().any(|(existing, _)| *existing == name) {
                    continue;
                }
                styles.push((name, None));
            }
        }
        let current = self
            .active_output_style_setting()
            .await
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| outputstyles::DEFAULT_OUTPUT_STYLE_NAME.to_string());
        Some(lingxi_core::host::OutputStyleListing { current, styles })
    }

    async fn set_output_style(&self, name: &str) -> Result<(), HandleError> {
        let listing = self
            .output_styles()
            .await
            .ok_or_else(|| HandleError::ActionFailed("no output styles are available".into()))?;
        // Case-INSENSITIVE, like upstream's `l.find(e => e.toLowerCase() === a)`,
        // but the CANONICAL spelling is what gets stored: the resolvers look
        // names up case-sensitively, so storing what the user typed would
        // select nothing.
        let canonical = listing
            .styles
            .iter()
            .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
            .map(|(candidate, _)| candidate.clone())
            .ok_or_else(|| HandleError::ActionFailed(format!("unknown output style {name:?}")))?;
        *self.prompt_runtime.live_output_style.lock().await = Some(canonical);
        Ok(())
    }

    async fn conversation_controls(&self) -> Option<lingxi_core::host::ConversationControls> {
        let state = self.session.lock().await;
        Some(self.conversation_controls_for_model(&state.model, state.model_profile.as_deref()))
    }

    async fn set_reasoning_selection(
        &self,
        selection: lingxi_core::host::ReasoningSelection,
    ) -> Result<(), HandleError> {
        let state = self.session.lock().await;
        self.set_reasoning_selection_for_model(
            &state.model,
            state.model_profile.as_deref(),
            selection,
        );
        Ok(())
    }

    async fn permission_mode(&self) -> Option<String> {
        ConversationOrchestrator::permission_mode(self)
    }

    async fn set_permission_mode(&self, mode: &str) -> Result<(), HandleError> {
        ConversationOrchestrator::set_permission_mode(self, mode)
            .await
            .map_err(HandleError::ActionFailed)
    }

    async fn request_exit(&self) {
        self.abort_startup_responses_websocket_prewarm();
        if let Err(err) = self.api.close_responses_websocket_session().await {
            tracing::warn!(error = %err, "failed to close responses websocket session during request_exit");
        }
        let session_id = self.session.lock().await.session_id.to_string();
        compaction::invoked_skills::clear_session(&session_id);
        self.should_exit.store(true, Ordering::SeqCst);
    }

    async fn current_should_exit(&self) -> bool {
        self.should_exit.load(Ordering::SeqCst)
    }

    async fn open_memory_editor(&self) -> Result<MemoryEditorOutcome, HandleError> {
        // `/memory` edits the USER-tier LINGXI.md — the SAME file the system-prompt
        // hierarchy loads (memory::lingxi_md::user_config_dir): `$LINGXI_CONFIG_DIR`
        // when set, else `~/.lingxi/LINGXI.md`. (Previously this targeted
        // `dirs::config_dir()/claude/LINGXI.md` — a different, never-loaded path that
        // also ignored `$LINGXI_CONFIG_DIR`.)
        let home = dirs::home_dir().ok_or_else(|| {
            HandleError::ActionFailed("home dir unavailable on this platform".into())
        })?;
        let target = memory::lingxi_md::user_config_dir(&home).join(branding::MEMORY_FILE);
        spawn_editor_on(target, "").await
    }

    // M5-11 additions:

    async fn ide_status(&self) -> lingxi_core::host::IdeStatus {
        match self.ide_handle.as_ref() {
            Some(ide) => ide.status().await,
            None => lingxi_core::host::IdeStatus::default(),
        }
    }

    async fn ide_connect(
        &self,
        endpoint_id: &str,
    ) -> Result<lingxi_core::host::IdeStatus, HandleError> {
        let Some(ide) = self.ide_handle.as_ref() else {
            return Err(HandleError::Unimplemented("ide_connect".into()));
        };
        ide.connect(endpoint_id)
            .await
            .map_err(HandleError::ActionFailed)
    }

    async fn ide_disconnect(&self) -> Result<lingxi_core::host::IdeStatus, HandleError> {
        let Some(ide) = self.ide_handle.as_ref() else {
            return Err(HandleError::Unimplemented("ide_disconnect".into()));
        };
        ide.disconnect().await.map_err(HandleError::ActionFailed)
    }

    async fn ide_open(&self) -> Result<String, HandleError> {
        let Some(ide) = self.ide_handle.as_ref() else {
            return Err(HandleError::Unimplemented("ide_open".into()));
        };
        // Read the session CWD at invocation time. Worktree enter/exit and
        // `/cd` mutate this exact cell, so an open request never uses boot cwd.
        ide.open(self.session_cwd.cwd())
            .await
            .map_err(HandleError::ActionFailed)
    }

    async fn ide_auto_connect_if_single(&self) -> Result<bool, HandleError> {
        let Some(ide) = self.ide_handle.as_ref() else {
            return Ok(false);
        };
        ide.auto_connect_if_single()
            .await
            .map_err(HandleError::ActionFailed)
    }

    async fn list_mcp_servers(&self) -> Vec<McpServerInfo> {
        // M6-07: read the wired McpRegistry (Task 7); falls back to
        // `vec![]` when no registry was attached so unit tests / library
        // callers remain unaffected.
        let Some(reg) = self.mcp_registry.as_ref() else {
            return Vec::new();
        };
        reg.snapshot().await
    }

    async fn list_skills(&self) -> Vec<SkillInfo> {
        // Desktop Skills settings view (Task 7): re-scan the project, user,
        // and managed tiers via `skill_api::load_file_skill_sections_with_roots`,
        // rather than a Rust-side `SkillRegistry` — the composition root's
        // registry is a residual empty instance with no turn-loop consumer,
        // so it would report zero skills unconditionally.
        //
        // `managed_dir` mirrors the composition root's
        // `SkillsHandler::with_all_roots(.., Some(managed_settings_dir()), ..)`
        // (`runtime/src/desktop/mod.rs:3351-3354`) exactly — it is the
        // SAME pure, env/platform-derived function (`lingxi_core::host::live_sessions::
        // managed_settings_dir`), not a second computation; the loader
        // treats a non-existent managed dir as an empty tier, so this is
        // harmless when no managed policy is installed.
        //
        // `additional_skill_dirs` is deliberately `&[]` (fix round 2): a
        // first attempt read it from `self.session_cwd.trusted_dirs()`, but
        // that set is seeded at boot with `cwd` plus every settings-tier
        // `permissions.additionalDirectories` entry plus `--add-dir`
        // (`runtime/src/desktop/mod.rs:7538-7568`, `:8666-8688`) — a much
        // wider set than what the sibling slash commands actually scan. The
        // desktop's own `SkillsHandler` (`/skills`,
        // `runtime/src/desktop/mod.rs:3351-3354`) is constructed with
        // `additional_skill_dirs: Vec::new()` HARDCODED — it never sees any
        // additional directory. Its `ReloadSkillsHandler` counterpart
        // (`/reload-skills`) is wired to `DesktopRepoRootReloader
        // .registered_roots` (`runtime/src/desktop/mod.rs:4919`,
        // `:4964-4977`), which starts empty and grows only through a live
        // `register_repo_root` call with no desktop-side call site outside
        // `apps/cli/src/run.rs` — so on the desktop it is always empty too.
        // Using `trusted_dirs()` therefore made this listing show skills
        // NEITHER sibling command reports for any project configuring
        // `additionalDirectories` — the mirror image of the defect this
        // parameter previously had. If either construction site above ever
        // starts supplying real roots, this must be revisited together with
        // them; until then, pass `&[]` here, not `trusted_dirs()` or any
        // other implicit "additional directories" source.
        //
        // Falls back to `vec![]` when no config home is wired, mirroring
        // `list_mcp_servers`'s "no registry" default.
        let Some(config_home) = self.config_home.as_ref() else {
            return Vec::new();
        };
        let managed_dir = lingxi_core::host::live_sessions::managed_settings_dir();
        skill_api::load_file_skill_sections_with_roots(
            &self.cwd,
            config_home,
            Some(&managed_dir),
            &[],
        )
        .into_iter()
        .flat_map(|section| {
            section.rows.into_iter().map(|row| SkillInfo {
                name: row.name,
                source_dir: row.source_dir,
            })
        })
        .collect()
    }

    async fn reconnect_mcp_servers(
        &self,
        name: Option<&str>,
    ) -> (Vec<String>, Vec<(String, String)>) {
        let Some(reg) = self.mcp_registry.as_ref() else {
            return (Vec::new(), Vec::new());
        };
        // `None` / "all" → every registered server; else the single named one
        // (reported as a failure when unknown).
        let targets: Vec<String> = match name {
            None | Some("all") => reg.server_names().await,
            Some(n) => vec![n.to_string()],
        };
        let (mut ok, mut failed) = (Vec::new(), Vec::new());
        for server in targets {
            match reg.reconnect(&server).await {
                Ok(()) => ok.push(server),
                Err(e) => failed.push((server, e.to_string())),
            }
        }
        (ok, failed)
    }

    async fn mcp_server_states(&self) -> Vec<(String, lingxi_core::host::McpActionState)> {
        match self.mcp_registry.as_ref() {
            Some(reg) => reg.action_states().await,
            None => Vec::new(),
        }
    }

    async fn set_mcp_servers_disabled(
        &self,
        server: Option<&str>,
        disabled: bool,
    ) -> Result<Vec<lingxi_core::host::McpToggleOutcome>, String> {
        let Some(path) = migrations::global_config::global_config_path() else {
            return Err("global config path is unavailable".to_string());
        };
        let Some(registry) = self.mcp_registry.as_ref() else {
            return Ok(Vec::new());
        };
        let known = registry.server_names().await;
        // `None`/"all" → every registered server; else just the named one.
        let targets: Vec<String> = match server {
            None | Some("all") => known,
            Some(n) if known.iter().any(|known| known == n) => vec![n.to_string()],
            Some(n) => return Err(format!("no MCP server named \"{n}\"")),
        };
        if targets.is_empty() {
            return Ok(Vec::new());
        }
        // Read-modify-write `projects[<cwd>].disabledMcpjsonServers` — the list
        // `mcp::apply_project_server_gate` reads at startup. This persistence is
        // a side effect; the returned outcome is driven by the live toggles
        // below (claude's `p`/`allSettled` set), not by config membership.
        let key = migrations::global_config::project_path_for_config(&self.cwd);
        migrations::global_config::save_project_config(&path, &key, |mut proj| {
            let list: Vec<String> = proj
                .get("disabledMcpjsonServers")
                .and_then(serde_json::Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let (next, _changed) = apply_mcp_disabled(list, &targets, disabled);
            proj.insert(
                "disabledMcpjsonServers".to_string(),
                serde_json::json!(next),
            );
            proj
        })
        .map_err(|e| e.to_string())?;
        // Toggle each target live — claude's `Promise.allSettled(p.map(u))`.
        // Only servers that were NOT already in the requested state (claude's
        // `p` filter) yield an outcome: `Ok(None)` means "already there" and is
        // skipped; `Ok(Some(state))` is a settled outcome carrying the post-
        // toggle state; `Err` is a rejected op ("couldn't be changed — may have
        // been removed"), recorded as `state: None`. A failed enable *connect*
        // is NOT an `Err` (see `McpRegistry::set_disabled`), so it lands here as
        // a settled `Failed`, letting the handler emit the "…but it isn't
        // connected yet." variant instead of a hard error.
        let mut outcomes: Vec<lingxi_core::host::McpToggleOutcome> = Vec::new();
        for name in &targets {
            let outcome = match registry.set_disabled(name, disabled).await {
                Ok(None) => continue,
                Ok(state @ Some(_)) => lingxi_core::host::McpToggleOutcome {
                    name: name.clone(),
                    state,
                },
                Err(_) => lingxi_core::host::McpToggleOutcome {
                    name: name.clone(),
                    state: None,
                },
            };
            outcomes.push(outcome);
        }
        Ok(outcomes)
    }

    async fn fire_directory_added(
        &self,
        directory: &str,
        source: &str,
    ) -> lingxi_core::host::DirectoryAddedHookSummary {
        ConversationOrchestrator::fire_directory_added(self, directory, source).await
    }

    async fn register_repo_root(
        &self,
        request: lingxi_core::host::RegisterRepoRootRequest,
    ) -> Result<lingxi_core::host::RegisterRepoRootOutcome, lingxi_core::host::HandleError> {
        ConversationOrchestrator::register_repo_root(self, request).await
    }

    async fn list_hooks(&self) -> Vec<HookInfo> {
        // M6-07: read the wired HookRegistry (Task 7).
        let Some(reg) = self.lifecycle_runtime.hook_registry.as_ref() else {
            return Vec::new();
        };
        let g = reg.read().await;
        let mut out: Vec<HookInfo> = g
            .all_hooks()
            .into_iter()
            .map(|h| {
                let (hook_type, content) = hook_executor_type_and_content(&h.executor);
                HookInfo {
                    name: h.name.clone(),
                    event: h.events.first().map_or("Unknown", event_str).to_string(),
                    matcher: h.if_condition.as_ref().map(|c| c.pattern.clone()),
                    timeout_ms: h.timeout.map_or(60_000_u64, |d| {
                        u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
                    }),
                    hook_type,
                    source: hook_source_description(h.source),
                    content,
                    status_message: h.status_message.clone(),
                    blocking: h.blocking,
                    priority: h.priority,
                    async_rewake: h.async_rewake,
                    async_timeout_ms: h
                        .async_timeout
                        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)),
                    if_condition: h.if_pattern().map(ToOwned::to_owned),
                }
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    async fn list_agents(&self) -> Vec<AgentInfo> {
        // M6-07: read the wired agent catalog (Task 7).
        let Some(cat) = self.lifecycle_runtime.agent_catalog.as_ref() else {
            return Vec::new();
        };
        let g = cat.read().await;
        let mut out: Vec<AgentInfo> = g
            .iter()
            .map(|a| AgentInfo {
                name: a.agent_type.clone(),
                description: a.when_to_use.clone(),
                tools_allowed: a.allowed_tools.clone(),
                wildcard_tools: matches!(a.tools, agent::AgentToolPolicy::All { .. }),
                source_group: agent_source_group_label(a.source).to_string(),
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    async fn run_doctor_checks(&self) -> DoctorReport {
        // Probe the real config-home tree: `$LINGXI_CONFIG_DIR` when set, else
        // `~/.claude` (claude-code `tr()` — the SAME dir `/memory`, settings, and
        // the TUI doctor screen use). NOT `dirs::config_dir()` (≈ `~/Library/
        // Application Support` on macOS), which is a different, never-used tree.
        let config_dir = dirs::home_dir().map_or_else(
            || std::path::PathBuf::from("."),
            |h| memory::lingxi_md::user_config_dir(&h),
        );
        // Also diagnose the global config `~/.lingxi.json` (the one path NOT under
        // `tr()`: `($LINGXI_CONFIG_DIR || $HOME)/.lingxi.json`), which claude-code's
        // doctor probes alongside the tree.
        crate::diagnostics::run_all(
            &config_dir,
            migrations::global_config::global_config_path().as_deref(),
        )
        .await
    }

    async fn append_meta_user_message(&self, text: &str) -> Result<(), HandleError> {
        let msg = lingxi_core::types::ConversationMessage::user_meta(
            lingxi_core::types::MessageId::new(),
            text.to_string(),
        );
        {
            let mut s = self.session.lock().await;
            s.history.push(msg.clone());
        }
        self.persist_message_to_jsonl(&msg).await;
        Ok(())
    }

    async fn append_slash_command_transcript(
        &self,
        raw: &str,
        display: &str,
    ) -> Result<(), HandleError> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Ok(());
        }

        let mut messages = vec![lingxi_core::types::ConversationMessage::User { api_message_override: None,
            id: lingxi_core::types::MessageId::new(),
            content: vec![lingxi_core::types::ContentBlock::Text {
                text: raw.to_string(),
                citations: None,
            }],
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
        }];
        if !display.trim().is_empty() {
            messages.push(lingxi_core::types::ConversationMessage::Assistant { per_turn_effort: None,
                id: lingxi_core::types::MessageId::new(),
                content: vec![lingxi_core::types::ContentBlock::Text {
                    text: display.to_string(),
                    citations: None,
                }],
                stop_reason: None,
            });
        }

        let _turn_guard = self.turn_gate.lock().await;
        let target = self.session.lock().await.session_id;
        for message in messages {
            let persisted_uuid = self
                .persist_model_excluded_meta_to_session(target, &message)
                .await?;
            {
                let mut session = self.session.lock().await;
                session.model_context_excluded_messages.insert(message.id());
                session.history.push(message);
            }
            if let Some(uuid) = persisted_uuid {
                *self.transcript.last_jsonl_uuid.lock().await = Some(uuid);
            }
        }
        Ok(())
    }

    async fn append_fusion_command_message(
        &self,
        session_id: &str,
        text: &str,
        is_user: bool,
    ) -> Result<(), HandleError> {
        let target =
            lingxi_core::types::SessionId::parse_prefixed(session_id).ok_or_else(|| {
                HandleError::ActionFailed(format!("invalid target session id {session_id:?}"))
            })?;
        let _turn_guard = self.turn_gate.lock().await;
        let message = if is_user {
            lingxi_core::types::ConversationMessage::user(
                lingxi_core::types::MessageId::new(),
                text.to_string(),
            )
        } else {
            lingxi_core::types::ConversationMessage::Assistant { per_turn_effort: None,
                id: lingxi_core::types::MessageId::new(),
                content: vec![lingxi_core::types::ContentBlock::Text {
                    text: text.to_string(),
                    citations: None,
                }],
                stop_reason: None,
            }
        };
        let uuid = self
            .persist_conversation_message_to_session(target, &message, false)
            .await?
            .ok_or_else(|| {
                HandleError::ActionFailed("Fusion requires session persistence".into())
            })?;
        let mut session = self.session.lock().await;
        if session.session_id == target {
            session.history.push(message);
            drop(session);
            *self.transcript.last_jsonl_uuid.lock().await = Some(uuid);
        }
        Ok(())
    }

    async fn append_meta_user_message_to_session(
        &self,
        session_id: &str,
        text: &str,
    ) -> Result<(), HandleError> {
        let target =
            lingxi_core::types::SessionId::parse_prefixed(session_id).ok_or_else(|| {
                HandleError::ActionFailed(format!("invalid target session id {session_id:?}"))
            })?;
        // Serialize with turns and hot-resume/clear so the target check,
        // transcript parent lookup, append, and live-history update all land on
        // one side of a session transition.
        let _turn_guard = self.turn_gate.lock().await;
        let current = self.session.lock().await.session_id;
        let msg = lingxi_core::types::ConversationMessage::user_meta(
            lingxi_core::types::MessageId::new(),
            text.to_string(),
        );
        let persisted_uuid = self
            .persist_model_excluded_meta_to_session(target, &msg)
            .await?;

        if current == target {
            let mut session = self.session.lock().await;
            session.model_context_excluded_messages.insert(msg.id());
            session.history.push(msg);
            drop(session);
            if let Some(uuid) = persisted_uuid {
                *self.transcript.last_jsonl_uuid.lock().await = Some(uuid);
            }
            return Ok(());
        }

        if persisted_uuid.is_none() {
            return Err(HandleError::ActionFailed(format!(
                "target session {session_id} is not active and persistence is disabled"
            )));
        }
        Ok(())
    }

    async fn emit_background_system_notice(&self, body: &str) {
        self.output.emit_system_notice(body, false).await;
    }

    async fn get_status_snapshot(&self) -> StatusSnapshot {
        // Read the durable session ledger rather than manufacturing zero totals
        // on every status request (including the first request after resume).
        let cost = self.snapshot_cost_real().await;
        let s = self.session.lock().await;
        let cost = if cost.session_id == s.session_id {
            cost
        } else {
            CostSnapshot {
                session_id: s.session_id,
                ..CostSnapshot::default()
            }
        };
        StatusSnapshot {
            session_id: s.session_id.to_string(),
            model: s.model.clone(),
            model_profile: s.model_profile.clone(),
            n_messages: u32::try_from(s.history.len()).unwrap_or(u32::MAX),
            total_cost_usd: cost.total_usd,
            input_tokens: cost.input_tokens,
            output_tokens: cost.output_tokens,
            n_mcp_connected: 0,
            n_mcp_total: 0,
            setting_sources: setting_sources_for(&self.cwd),
            n_hooks: 0,
            n_agents: 0,
            started_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            cwd: self.cwd.clone(),
            // This handle is not coordinator-wired; the count is 0 (T21).
            active_workers: 0,
        }
    }

    async fn edit_config_file(&self) -> Result<MemoryEditorOutcome, HandleError> {
        // claude-code has no standalone `config.json`: `/config` reads/writes
        // `<config-home>/settings.json` (`$LINGXI_CONFIG_DIR` else `~/.claude`).
        // Edit that real file, not a phantom under `dirs::config_dir()`.
        let home = dirs::home_dir().ok_or_else(|| {
            HandleError::ActionFailed("home dir unavailable on this platform".into())
        })?;
        let target = memory::lingxi_md::user_config_dir(&home).join("settings.json");
        spawn_editor_on(target, "{}\n").await
    }

    async fn edit_permissions_file(&self) -> Result<MemoryEditorOutcome, HandleError> {
        // claude-code has no `permissions.json`: tool permissions live under the
        // `permissions` key of `<config-home>/settings.json`. Open that file.
        let home = dirs::home_dir().ok_or_else(|| {
            HandleError::ActionFailed("home dir unavailable on this platform".into())
        })?;
        let target = memory::lingxi_md::user_config_dir(&home).join("settings.json");
        spawn_editor_on(target, "{}\n").await
    }

    /// Model names shown by the no-arg `/model` display and the picker's "live"
    /// column. Surfaces the real configured profiles + aliases via the API-client
    /// seam (`ProviderApiAdapter` → `ModelRouter::available_models`, emitting
    /// `provider/model` ids and `@aliases`).
    ///
    /// There is intentionally NO provider-specific fallback here. This may be
    /// empty for library / test / no-streaming callers that wire no routing
    /// client; the grouped picker merges it with the static catalog
    /// (`list_model_listings`), which now enumerates EVERY provider — including
    /// first-party Anthropic — so Claude no longer needs a hardcoded list. A real
    /// session always has a live config (`provider_config::assemble` injects the
    /// Anthropic profile plus every preset), so this is non-empty in production.
    /// `switch_model` still accepts any string; actual availability depends on the
    /// profile's credential (see `docs/LLM_PROVIDERS.md`).
    async fn list_available_models(&self) -> Vec<String> {
        self.api.available_models()
    }

    /// Richer catalog listing for the grouped `/model` picker. Delegates to the
    /// api client's [`OrchestratorApiClient::list_model_listings`], which the
    /// production `ProviderApiAdapter` sources from the llm-runtime catalog.
    async fn list_model_listings(&self) -> Vec<lingxi_core::host::orchestrator::ModelListing> {
        self.api.list_model_listings()
    }

    /// Return the most recently observed provider rate-limit header snapshot.
    ///
    /// Delegates to [`OrchestratorApiClient::last_rate_limit_info`] on the
    /// `api` field.  `ProviderApiAdapter` overrides the default (None) to
    /// return the cached 2xx header snapshot from `last_rate_limit`.
    ///
    /// TUI note: this surface is available for polling (e.g. from a ticker).
    /// Wiring it into `RenderedMessage::RateLimit` requires a new protocol
    /// event or a dedicated status-poll channel — both outside this task's
    /// scope (frozen protocol guard).  See the trait doc for details.
    async fn last_rate_limit_info(&self) -> Option<lingxi_core::host::RateLimitSnapshot> {
        self.api.last_rate_limit_info()
    }

    async fn run_turn_streaming_with_cancel(
        &self,
        prompt: &str,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<lingxi_core::host::TurnOutcome, HandleError> {
        // Delegate to the inherent method on `ConversationOrchestrator`
        // (M6-03 T1). Disambiguate via fully-qualified call syntax since
        // the trait method has the same name.
        match crate::ConversationOrchestrator::run_turn_streaming_with_cancel(self, prompt, cancel)
            .await
        {
            Ok(crate::conversation::TurnOutcome::EndTurn) => {
                Ok(lingxi_core::host::TurnOutcome::EndTurn)
            }
            Ok(crate::conversation::TurnOutcome::MaxTurns) => {
                Ok(lingxi_core::host::TurnOutcome::MaxTurns)
            }
            Ok(crate::conversation::TurnOutcome::Cancelled) => {
                Ok(lingxi_core::host::TurnOutcome::Cancelled)
            }
            Err(e) => Err(HandleError::ActionFailed(e.to_string())),
        }
    }

    async fn run_queued_turn_streaming(
        &self,
        prompt: &str,
        cancel: tokio_util::sync::CancellationToken,
        in_human_turn: bool,
    ) -> Result<lingxi_core::host::TurnOutcome, HandleError> {
        self.run_turn_streaming_with_origin(prompt, Vec::new(), cancel, None, in_human_turn)
            .await
            .map(|outcome| match outcome {
                crate::conversation::TurnOutcome::EndTurn => {
                    lingxi_core::host::TurnOutcome::EndTurn
                }
                crate::conversation::TurnOutcome::Cancelled => {
                    lingxi_core::host::TurnOutcome::Cancelled
                }
                crate::conversation::TurnOutcome::MaxTurns => {
                    lingxi_core::host::TurnOutcome::MaxTurns
                }
            })
            .map_err(|error| HandleError::ActionFailed(error.to_string()))
    }

    async fn run_queued_turn_streaming_with_row_token(
        &self,
        prompt: &str,
        cancel: tokio_util::sync::CancellationToken,
        in_human_turn: bool,
        row_token: String,
    ) -> Result<lingxi_core::host::TurnOutcome, HandleError> {
        self.run_turn_streaming_with_origin_and_row_token(
            prompt,
            Vec::new(),
            cancel,
            None,
            in_human_turn,
            Some(row_token),
        )
        .await
        .map(|outcome| match outcome {
            crate::conversation::TurnOutcome::EndTurn => lingxi_core::host::TurnOutcome::EndTurn,
            crate::conversation::TurnOutcome::Cancelled => {
                lingxi_core::host::TurnOutcome::Cancelled
            }
            crate::conversation::TurnOutcome::MaxTurns => lingxi_core::host::TurnOutcome::MaxTurns,
        })
        .map_err(|error| HandleError::ActionFailed(error.to_string()))
    }

    async fn run_turn_streaming_with_images(
        &self,
        prompt: &str,
        image_paths: &[std::path::PathBuf],
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<lingxi_core::host::TurnOutcome, HandleError> {
        // Delegate to the inherent image-aware streaming entry point.
        match crate::ConversationOrchestrator::run_turn_streaming_with_cancel_images(
            self,
            prompt,
            image_paths,
            cancel,
        )
        .await
        {
            Ok(crate::conversation::TurnOutcome::EndTurn) => {
                Ok(lingxi_core::host::TurnOutcome::EndTurn)
            }
            Ok(crate::conversation::TurnOutcome::MaxTurns) => {
                Ok(lingxi_core::host::TurnOutcome::MaxTurns)
            }
            Ok(crate::conversation::TurnOutcome::Cancelled) => {
                Ok(lingxi_core::host::TurnOutcome::Cancelled)
            }
            Err(e) => Err(HandleError::ActionFailed(e.to_string())),
        }
    }

    async fn run_turn_streaming_with_images_and_row_token(
        &self,
        prompt: &str,
        image_paths: &[std::path::PathBuf],
        cancel: tokio_util::sync::CancellationToken,
        row_token: String,
    ) -> Result<lingxi_core::host::TurnOutcome, HandleError> {
        match crate::ConversationOrchestrator::run_turn_streaming_with_cancel_images_and_row_token(
            self,
            prompt,
            image_paths,
            cancel,
            row_token,
        )
        .await
        {
            Ok(crate::conversation::TurnOutcome::EndTurn) => {
                Ok(lingxi_core::host::TurnOutcome::EndTurn)
            }
            Ok(crate::conversation::TurnOutcome::MaxTurns) => {
                Ok(lingxi_core::host::TurnOutcome::MaxTurns)
            }
            Ok(crate::conversation::TurnOutcome::Cancelled) => {
                Ok(lingxi_core::host::TurnOutcome::Cancelled)
            }
            Err(error) => Err(HandleError::ActionFailed(error.to_string())),
        }
    }

    async fn run_async_hook_rewake(
        &self,
        generation_cancel: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<lingxi_core::host::TurnOutcome, HandleError> {
        match crate::ConversationOrchestrator::run_async_hook_rewake(self, generation_cancel).await
        {
            Ok(crate::conversation::TurnOutcome::EndTurn) => {
                Ok(lingxi_core::host::TurnOutcome::EndTurn)
            }
            Ok(crate::conversation::TurnOutcome::MaxTurns) => {
                Ok(lingxi_core::host::TurnOutcome::MaxTurns)
            }
            Ok(crate::conversation::TurnOutcome::Cancelled) => {
                Ok(lingxi_core::host::TurnOutcome::Cancelled)
            }
            Err(error) => Err(HandleError::ActionFailed(error.to_string())),
        }
    }

    // engine-data-commands additions:

    async fn conversation_transcript(&self) -> Vec<lingxi_core::types::ConversationMessage> {
        // Clone of the live, ordered session history. Backs `/export`
        // (transcript → file) and underpins `/summary` + `/diff`.
        self.session.lock().await.history.clone()
    }

    async fn files_in_context(&self) -> Vec<PathBuf> {
        // Read the ONE shared read-file-state registry (TS
        // `context.readFileState`), populated by the file tools' own
        // `readFileState.set` (Read/Edit/Write/MultiEdit/NotebookEdit) over the
        // `Arc` the composition root shares into `BuiltinToolContext`. Keys are
        // the MODEL-VISIBLE live-cwd absolutized paths in MRU→LRU order — 1:1
        // with TS `cacheKeys(context.readFileState)` (`Array.from(cache.keys())`),
        // which `/files` renders via `relative(getCwd(), f)`. Host-seeded
        // snapshots stay in the shared cache for staleness/dedup but are
        // intentionally filtered here because the model never saw them. An
        // empty visible cache still renders the locked "No files in context"
        // branch.
        self.prompt_runtime
            .read_state_map
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .model_context_keys()
    }

    async fn context_usage_snapshot(&self) -> lingxi_core::host::ContextUsageSnapshot {
        use lingxi_core::host::{ContextUsageCategory, ContextUsageCategoryKind as Kind};

        // Snapshot the live model and history together. The history component
        // uses the exact estimator auto-compaction uses, so replacing history
        // with a compact summary immediately lowers the Messages row.
        let (model, history) = {
            let session = self.session.lock().await;
            (session.model.clone(), session.history.clone())
        };
        let active_betas = self.api.active_betas();
        let max_context_tokens =
            compaction::context_window::context_window_for_model(&model, &active_betas);

        // The system prompt and tool schemas are the bytes the next main-loop
        // request will advertise. Keep MCP definitions separate from ordinary
        // tools so both renderers can expose the same category split.
        let system_prompt = self.effective_system_prompt().await;
        let wire_tools = self.build_wire_tools().await.0;
        let (mcp_tools, system_tools): (Vec<_>, Vec<_>) =
            wire_tools.into_iter().partition(is_mcp_wire_tool);

        let memory_files = self.main_instruction_files().await.unwrap_or_else(|error| {
            tracing::warn!(%error, "instruction context usage snapshot failed");
            Arc::new(Vec::new())
        });
        let memory_tokens =
            estimate_bytes_as_tokens(crate::prompt::memory_block::format(&memory_files).len());
        let system_prompt_tokens = estimate_bytes_as_tokens(system_prompt.len());
        let system_tool_tokens = estimate_json_tokens(&system_tools);
        let mcp_tool_tokens = estimate_json_tokens(&mcp_tools);
        let message_tokens = compaction::grouping::estimate_tokens_for_range(&history);

        // Invoked skill bodies are already represented by their persisted
        // messages. Keep the explicit Skills row stable without double-counting.
        let skills_tokens = 0;
        let live_context_tokens = system_prompt_tokens
            .saturating_add(system_tool_tokens)
            .saturating_add(mcp_tool_tokens)
            .saturating_add(memory_tokens)
            .saturating_add(skills_tokens)
            .saturating_add(message_tokens);
        let autocompact_buffer = compaction::thresholds::AUTOCOMPACT_BUFFER_TOKENS
            .min(max_context_tokens.saturating_sub(live_context_tokens));
        let free_tokens = max_context_tokens
            .saturating_sub(live_context_tokens)
            .saturating_sub(autocompact_buffer);

        lingxi_core::host::ContextUsageSnapshot {
            live_context_tokens,
            max_context_tokens,
            breakdown: vec![
                ContextUsageCategory::new(Kind::SystemPrompt, system_prompt_tokens),
                ContextUsageCategory::new(Kind::SystemTools, system_tool_tokens),
                ContextUsageCategory::new(Kind::McpTools, mcp_tool_tokens),
                ContextUsageCategory::new(Kind::MemoryFiles, memory_tokens),
                ContextUsageCategory::new(Kind::Skills, skills_tokens),
                ContextUsageCategory::new(Kind::Messages, message_tokens),
                ContextUsageCategory::new(Kind::AutocompactBuffer, autocompact_buffer),
                ContextUsageCategory::new(Kind::FreeSpace, free_tokens),
            ],
            cumulative_cost: self.snapshot_cost().await,
        }
    }

    async fn context_window_usage(&self) -> (u64, u64) {
        let snapshot = self.context_usage_snapshot().await;
        (snapshot.live_context_tokens, snapshot.max_context_tokens)
    }

    async fn list_resumable_sessions(&self) -> Vec<(String, String)> {
        // Enumerate the on-disk JSONL session store the resume path reads
        // from: `<config-home>/projects/<project_dir_name(cwd)>/<uuid>.jsonl`.
        // `config-home` is `$LINGXI_CONFIG_DIR` (else `~/.claude`) — the SAME
        // env-aware resolver the CLI loader (`run::lingxi_home_dir`) uses, so the
        // picker lists exactly what `--resume` can load. Each entry maps to
        // `(session_id, label)`; label is the id (the first-prompt label +
        // interactive picker are deferred). Newest-first by mtime. Returns an
        // empty Vec when the store is absent.
        let Some(home) = dirs::home_dir() else {
            return Vec::new();
        };
        let cwd = self.cwd.to_string_lossy();
        let project_dir = memory::lingxi_md::user_config_dir(&home)
            .join("projects")
            .join(session::project_dir_name(&cwd));
        let Ok(entries) = std::fs::read_dir(&project_dir) else {
            return Vec::new();
        };
        let mut found: Vec<(std::time::SystemTime, String)> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let mtime = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            found.push((mtime, stem.to_string()));
        }
        // Newest-first by modification time.
        found.sort_by(|a, b| b.0.cmp(&a.0));
        found.into_iter().map(|(_, id)| (id.clone(), id)).collect()
    }
}

fn estimate_bytes_as_tokens(bytes: usize) -> u64 {
    let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
    bytes.saturating_add(crate::model::count_tokens::APPROX_CHARS_PER_TOKEN - 1)
        / crate::model::count_tokens::APPROX_CHARS_PER_TOKEN
}

fn estimate_json_tokens(values: &[lingxi_core::types::utf16_json::Utf16JsonProjection]) -> u64 {
    values
        .iter()
        .map(|value| value.to_json_string().map_or(0, |raw| raw.len()))
        .map(estimate_bytes_as_tokens)
        .fold(0, u64::saturating_add)
}

fn is_mcp_wire_tool(value: &lingxi_core::types::utf16_json::Utf16JsonProjection) -> bool {
    value
        .get("name")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|name| {
            name == "MCP"
                || name == "McpAuth"
                || name == "ListMcpResources"
                || name == "ReadMcpResource"
                || name.starts_with("mcp__")
        })
}

/// Stable string label for a `HookEventType`, used by [`list_hooks`] to
/// populate [`lingxi_core::host::HookInfo::event`]. Avoids `Debug` derive
/// drift — the locked names are part of the M6-07 surface and the
/// claude-code parity. (M6-07)
fn event_str(et: &hooks::events::HookEventType) -> &'static str {
    use hooks::events::HookEventType as E;
    match et {
        E::AgentSpawn => "AgentSpawn",
        E::PreToolUse => "PreToolUse",
        E::PostToolUse => "PostToolUse",
        E::PostToolUseFailure => "PostToolUseFailure",
        E::SessionStart => "SessionStart",
        E::SessionEnd => "SessionEnd",
        E::Setup => "Setup",
        E::UserPromptSubmit => "UserPromptSubmit",
        E::Stop => "Stop",
        E::StopFailure => "StopFailure",
        E::SubagentStart => "SubagentStart",
        E::SubagentStop => "SubagentStop",
        E::PreCompact => "PreCompact",
        E::PostCompact => "PostCompact",
        E::PreModelSwitch => "PreModelSwitch",
        E::PostModelSwitch => "PostModelSwitch",
        E::PermissionRequest => "PermissionRequest",
        E::PermissionDenied => "PermissionDenied",
        E::TeammateIdle => "TeammateIdle",
        E::TaskCreated => "TaskCreated",
        E::TaskCompleted => "TaskCompleted",
        E::Elicitation => "Elicitation",
        E::ElicitationResult => "ElicitationResult",
        E::ConfigChange => "ConfigChange",
        E::WorktreeCreate => "WorktreeCreate",
        E::WorktreeRemove => "WorktreeRemove",
        E::InstructionsLoaded => "InstructionsLoaded",
        E::CwdChanged => "CwdChanged",
        E::FileChanged => "FileChanged",
        E::Notification => "Notification",
        E::PostToolBatch => "PostToolBatch",
        E::UserPromptExpansion => "UserPromptExpansion",
        E::MessageDisplay => "MessageDisplay",
        E::DirectoryAdded => "DirectoryAdded",
    }
}

/// (hooks-detail-fields-divergent) `config.type` + the primary content field
/// (claude-code `getContentFieldLabel`/`getContentFieldValue`,
/// `ViewHookMode.tsx`). `Builtin` has no TS analogue (an in-process Rust
/// handler) — surfaced as `"builtin"` with the handler id as its content.
fn hook_executor_type_and_content(executor: &hooks::HookExecutor) -> (String, String) {
    use hooks::HookExecutor as Ex;
    match executor {
        Ex::Command { command, args, .. } => {
            let content = if args.is_empty() {
                command.clone()
            } else {
                format!("{command} {}", args.join(" "))
            };
            ("command".to_string(), content)
        }
        Ex::Http { url, .. } => ("http".to_string(), url.clone()),
        // ⛔ Never surface the hook BODY here: this feeds user-facing hook
        // listings, and a function hook's source is plugin-authored text of
        // arbitrary size. The budget is the useful, safe detail.
        Ex::Function { budget_ms, .. } => (
            "function".to_string(),
            budget_ms.map_or_else(
                || "sandboxed JS".to_string(),
                |ms| format!("sandboxed JS ({ms}ms budget)"),
            ),
        ),
        Ex::Agent { prompt, .. } => ("agent".to_string(), prompt.clone()),
        Ex::Prompt { prompt, .. } => ("prompt".to_string(), prompt.clone()),
        // claude-code's content field for an `mcp_tool` hook is server, slash,
        // tool — `y2e` in the 2.1.238 binary at offset 296901596
        // (`case"mcp_tool"` → the server/tool pair). Its label there is
        // "MCP tool" (`Cug`, offset 301222424); the label column is the
        // divergent `/hooks` display template, so only the content matters here.
        Ex::McpTool { server, tool, .. } => ("mcp_tool".to_string(), format!("{server}/{tool}")),
        Ex::Builtin { handler_id } => ("builtin".to_string(), handler_id.clone()),
    }
}

/// Apply an enable/disable to a `disabledMcpjsonServers` list. `disabled=true`
/// adds each target that isn't already present; `disabled=false` removes each
/// that is. Returns `(new_list, affected)` where `affected` is the servers whose
/// membership actually changed (so an already-disabled server re-disabled is a
/// no-op and reported as unaffected). Idempotent.
fn apply_mcp_disabled(
    mut list: Vec<String>,
    targets: &[String],
    disabled: bool,
) -> (Vec<String>, Vec<String>) {
    let mut affected = Vec::new();
    for t in targets {
        let present = list.iter().any(|s| s == t);
        if disabled && !present {
            list.push(t.clone());
            affected.push(t.clone());
        } else if !disabled && present {
            list.retain(|s| s != t);
            affected.push(t.clone());
        }
    }
    (list, affected)
}

/// (hooks-detail-fields-divergent) claude-code
/// `hookSourceDescriptionDisplayString` (`utils/hooks/hooksSettings.ts`).
/// `Managed`/`FrontMatter`/`Skill` have no TS analogue (LingXi-only source
/// kinds) — given a parallel, sensible description rather than the TS
/// fallback (`source as string`, the bare enum name).
fn hook_source_description(source: hooks::HookSource) -> String {
    use hooks::HookSource as S;
    use lingxi_core::types::SettingsScope as T;
    match source {
        S::Settings(T::User) => "User settings (~/.lingxi/settings.json)",
        S::Settings(T::Project) => "Project settings (.lingxi/settings.json)",
        S::Settings(T::Local) => "Local settings (.lingxi/settings.local.json)",
        S::Settings(T::Managed) => "Managed settings (enterprise policy)",
        S::Plugin => "Plugin hooks (~/.lingxi/plugins/*/hooks/hooks.json)",
        S::FrontMatter => "Agent front matter",
        S::Session => "Session hooks (in-memory, temporary)",
        S::Skill => "Skill bundle",
    }
    .to_string()
}

/// (agents-08) claude-code `AGENT_SOURCE_GROUPS` label for an
/// [`agent::AgentSource`] (`tools/AgentTool/agentDisplay.ts:24-32` ×
/// `getSettingSourceName`). `UserDefined`→User, `Project`→Project,
/// `PolicySettings`→Managed, `Plugin`→Plugin, `Flag`→CLI arg,
/// `AdditionalDirectory`→User, `BuiltIn`→Built-in.
///
/// The local tier now exists on the shared ladder but no loader produces a
/// local agent, so it is named here rather than caught by `_`.
fn agent_source_group_label(source: agent::AgentSource) -> &'static str {
    use agent::AgentSource as S;
    use lingxi_core::types::SettingsScope as T;
    match source {
        S::Settings(T::User) => "User agents",
        S::Settings(T::Project) => "Project agents",
        S::Settings(T::Local) => "Local agents",
        S::Settings(T::Managed) => "Managed agents",
        S::Plugin => "Plugin agents",
        S::Flag => "CLI arg agents",
        S::AdditionalDirectory => "User agents",
        S::BuiltIn => "Built-in agents",
    }
}

/// (settings-status-missing-mcp-and-setting-sources) claude-code
/// `buildSettingSourcesProperties`: one display string per settings-file tier
/// that currently exists on disk (`sourcesWithSettings`'s "actually have
/// settings loaded" filter — approximated here as plain file existence,
/// since LingXi's settings loader does no separate enterprise-policy/managed
/// tier). Project, then User, in splice order.
fn setting_sources_for(cwd: &std::path::Path) -> Vec<String> {
    let mut out = Vec::new();
    if lingxi_core::settings::loader::project_settings_path(cwd).is_file() {
        out.push("Project settings (.lingxi/settings.json)".to_string());
    }
    if lingxi_core::settings::loader::user_settings_path().is_some_and(|p| p.is_file()) {
        out.push("User settings (~/.lingxi/settings.json)".to_string());
    }
    out
}

/// Touch + spawn an editor on `target`. If the target does not yet exist,
/// it is created with `default_body` as its initial content.
async fn spawn_editor_on(
    target: PathBuf,
    default_body: &str,
) -> Result<MemoryEditorOutcome, HandleError> {
    if let Some(parent) = target.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| HandleError::ActionFailed(format!("mkdir {parent:?}: {e}")))?;
    }
    if !target.exists() {
        tokio::fs::write(&target, default_body)
            .await
            .map_err(|e| HandleError::ActionFailed(format!("touch {target:?}: {e}")))?;
    }
    let editor = resolve_editor();
    let status = Command::new(&editor)
        .arg(&target)
        .status()
        .await
        .map_err(|e| HandleError::ActionFailed(format!("spawn {editor}: {e}")))?;
    Ok(MemoryEditorOutcome {
        edited_path: target,
        exit_code: status.code().unwrap_or(-1),
    })
}

#[cfg(unix)]
fn default_editor() -> String {
    "vi".to_string()
}
#[cfg(windows)]
fn default_editor() -> String {
    "notepad.exe".to_string()
}
#[cfg(not(any(unix, windows)))]
fn default_editor() -> String {
    "vi".to_string()
}

fn resolve_editor() -> String {
    use std::env;
    if let Some(v) = env::var_os("EDITOR") {
        if !v.is_empty() {
            return v.to_string_lossy().into_owned();
        }
    }
    if let Some(v) = env::var_os("VISUAL") {
        if !v.is_empty() {
            return v.to_string_lossy().into_owned();
        }
    }
    default_editor()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
        StaticMemoryProvider,
    };
    use std::sync::Arc;

    fn memory_warning_orchestrator() -> ConversationOrchestrator {
        let file = crate::prompt::MemoryFile {
            parent: None,
            source_content: None,
            path: PathBuf::from("/work/repo").join(branding::MEMORY_FILE),
            body: "x".repeat(52_310),
            is_local_override: false,
            tier: crate::prompt::LingxiMdTier::Project,
            globs: None,
            raw_content: String::new(),
            content_differs_from_disk: false,
        };
        ConversationOrchestrator::new(
            crate::OrchestratorConfig {
                model: "initial-wire-model".into(),
                ..Default::default()
            },
            Arc::new(MockApiClient::new(Vec::new())),
            Arc::new(tool_api::registry::ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::with_files(vec![file])),
            PathBuf::from("/work/repo"),
        )
    }

    #[tokio::test]
    async fn large_memory_warnings_resolve_live_alias_and_profile() {
        use agent::model_resolution::{
            FamilyModelDefaults, ModelProviderKind, ModelResolutionContext, ModelResolutionError,
            ModelRouteFacts,
        };

        let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let calls = observed.clone();
        let orch = memory_warning_orchestrator().with_model_resolution_context_provider(Arc::new(
            move |model: &str, profile: Option<&str>| {
                calls
                    .lock()
                    .unwrap()
                    .push((model.to_string(), profile.map(str::to_string)));
                let opus = match profile {
                    Some("wide") => Some("claude-opus-4-8"),
                    Some("small") => Some("claude-opus-4-1-20250805"),
                    None if model == "initial-wire-model" => None,
                    _ => {
                        return Err(ModelResolutionError::RouteUnavailable {
                            model: model.into(),
                            profile: profile.map(str::to_string),
                            reason: "fixture has no selected route".into(),
                        });
                    }
                };
                if model != "opus" && opus.is_some_and(|wire| model != wire) {
                    return Err(ModelResolutionError::RouteUnavailable {
                        model: model.into(),
                        profile: profile.map(str::to_string),
                        reason: "model is unavailable on the selected fixture route".into(),
                    });
                }
                Ok(ModelResolutionContext {
                    route: ModelRouteFacts {
                        model: model.into(),
                        profile: profile.map(str::to_string),
                        provider: opus.map(|_| ModelProviderKind::FirstParty),
                        ..Default::default()
                    },
                    family_defaults: FamilyModelDefaults {
                        opus: opus.map(str::to_string),
                        ..Default::default()
                    },
                    ..Default::default()
                })
            },
        ));
        observed.lock().unwrap().clear();

        for (model, profile, expected_warnings) in [
            ("opus", "wide", 0),
            ("claude-opus-4-8", "wide", 0),
            ("opus", "small", 1),
        ] {
            {
                let mut session = orch.session.lock().await;
                session.model = model.into();
                session.model_profile = Some(profile.into());
            }
            let rows = orch
                .large_memory_warnings()
                .await
                .expect("configured warning route resolves");
            assert_eq!(rows.len(), expected_warnings, "{model}/{profile}: {rows:?}");
            if expected_warnings != 0 {
                assert!(rows[0].contains("52.3k chars > 40.0k"), "{rows:?}");
            }
        }

        let calls = observed.lock().unwrap();
        for profile in ["wide", "small"] {
            assert!(calls.contains(&("opus".into(), Some(profile.into()))));
        }
        assert!(calls.iter().all(|(_, profile)| profile.is_some()));
    }

    #[tokio::test]
    async fn large_memory_warnings_skip_unresolved_routes_without_guessing() {
        let orch = memory_warning_orchestrator().with_model_resolution_context_provider(Arc::new(
            |model: &str, profile: Option<&str>| {
                Err(
                    agent::model_resolution::ModelResolutionError::RouteUnavailable {
                        model: model.into(),
                        profile: profile.map(str::to_string),
                        reason: "fixture route is unavailable".into(),
                    },
                )
            },
        ));
        {
            let mut session = orch.session.lock().await;
            session.model = "unavailable-model".into();
            session.model_profile = Some("unavailable-profile".into());
        }
        assert!(orch.large_memory_warnings().await.is_none());

        let unwired = memory_warning_orchestrator();
        unwired.session.lock().await.model = "opus".into();
        assert!(unwired.large_memory_warnings().await.is_none());
    }

    struct InvokedSkillRegistryGuard(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);

    impl InvokedSkillRegistryGuard {
        fn acquire() -> Self {
            let guard = compaction::invoked_skills::TEST_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            compaction::invoked_skills::reset_for_test();
            Self(guard)
        }
    }

    impl Drop for InvokedSkillRegistryGuard {
        fn drop(&mut self) {
            compaction::invoked_skills::reset_for_test();
        }
    }

    struct SessionMemoryTestRuntime;

    #[async_trait::async_trait]
    impl lingxi_core::host::RuntimeSpawner for SessionMemoryTestRuntime {
        async fn spawn(
            &self,
            _name: &str,
            _task: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
        ) -> Result<lingxi_core::host::BackgroundTaskHandle, lingxi_core::host::RuntimeError>
        {
            Err(lingxi_core::host::RuntimeError::Internal(
                "unused session-memory test runtime".to_string(),
            ))
        }

        async fn sleep(&self, _duration: std::time::Duration) {}

        async fn cancel(
            &self,
            _handle: &lingxi_core::host::BackgroundTaskHandle,
        ) -> Result<(), lingxi_core::host::RuntimeError> {
            Ok(())
        }
    }

    struct RecordingIdeHandle {
        opened: Arc<std::sync::Mutex<Vec<std::path::PathBuf>>>,
    }

    #[async_trait::async_trait]
    impl lingxi_core::host::IdeHandle for RecordingIdeHandle {
        async fn status(&self) -> lingxi_core::host::IdeStatus {
            lingxi_core::host::IdeStatus::default()
        }

        async fn connect(
            &self,
            _endpoint_id: &str,
        ) -> Result<lingxi_core::host::IdeStatus, String> {
            Ok(lingxi_core::host::IdeStatus::default())
        }

        async fn disconnect(&self) -> Result<lingxi_core::host::IdeStatus, String> {
            Ok(lingxi_core::host::IdeStatus::default())
        }

        async fn open(&self, cwd: std::path::PathBuf) -> Result<String, String> {
            self.opened.lock().unwrap().push(cwd);
            Ok("opened".to_string())
        }

        async fn auto_connect_if_single(&self) -> Result<bool, String> {
            Ok(false)
        }
    }

    #[tokio::test]
    async fn ide_open_uses_the_live_session_cwd() {
        let boot_cwd = std::path::PathBuf::from("/workspace/boot");
        let live_cwd = std::path::PathBuf::from("/workspace/worktree");
        let session_cwd = tool_api::SessionCwd::new(boot_cwd.clone(), vec![boot_cwd.clone()]);
        let opened = Arc::new(std::sync::Mutex::new(Vec::new()));
        let orch = crate::ConversationOrchestrator::new(
            crate::OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(Vec::new())),
            Arc::new(tool_api::registry::ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            boot_cwd,
        )
        .with_session_cwd(session_cwd.clone())
        .with_ide_handle(Arc::new(RecordingIdeHandle {
            opened: opened.clone(),
        }));

        session_cwd.change_cwd(live_cwd.clone());
        assert_eq!(
            lingxi_core::host::OrchestratorHandle::ide_open(&orch).await,
            Ok("opened".into())
        );
        let opened = opened.lock().unwrap();
        assert_eq!(opened.len(), 1);
        assert_eq!(opened[0], live_cwd);
    }

    #[tokio::test]
    async fn clear_invalidates_stale_session_memory_tasks_and_resets_progress() {
        let handle = Arc::new(crate::conversation::SessionMemoryHandle {
            extractor: tokio::sync::Mutex::new(
                memory::session_memory::SessionMemoryExtractor::new(
                    memory::session_memory::SessionMemoryConfig {
                        enabled: true,
                        minimum_message_tokens_to_init: 0,
                        minimum_tokens_between_update: 0,
                        tool_calls_between_updates: 3,
                    },
                ),
            ),
            runner: Arc::new(sidequery::ForkedAgentRunner::new()),
            config_home: std::env::temp_dir(),
            runtime: Arc::new(SessionMemoryTestRuntime),
            in_flight: std::sync::atomic::AtomicBool::new(true),
            generation: std::sync::atomic::AtomicU64::new(7),
        });
        {
            let mut extractor = handle.extractor.lock().await;
            extractor.mark_extracted_through(Some(lingxi_core::types::MessageId::new()));
            extractor.record_compaction_boundary(Some(lingxi_core::types::MessageId::new()), 2);
        }
        let captured_generation = handle.generation.load(Ordering::Acquire);
        let orch = crate::ConversationOrchestrator::new(
            crate::OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(Vec::new())),
            Arc::new(tool_api::registry::ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            std::env::temp_dir(),
        )
        .with_session_memory(handle.clone());
        let attachment = serde_json::json!({
            "type": "nested_memory", "path": "/old-session/nested.md",
            "content": {"path":"/old-session/nested.md", "type":"Project",
                "content":"old session guidance", "contentDiffersFromDisk":false},
            "displayPath": "nested.md",
        });
        let message = crate::ConversationOrchestrator::nested_memory_attachment_projection(
            lingxi_core::types::MessageId::new(),
            &attachment,
        )
        .unwrap();
        orch.persist_model_reminder(message, attachment).await;
        {
            let mut session = orch.session.lock().await;
            session.compacted_user_turns = 4;
            session
                .virtual_user_messages
                .insert(lingxi_core::types::MessageId::new());
        }

        lingxi_core::host::OrchestratorHandle::clear_session(&orch)
            .await
            .expect("clear session");
        {
            let session = orch.session.lock().await;
            assert_eq!(session.compacted_user_turns, 0);
            assert!(session.virtual_user_messages.is_empty());
        }

        assert_ne!(
            handle.generation.load(Ordering::Acquire),
            captured_generation,
            "a task captured before clear must fail its generation check"
        );
        let extractor = handle.extractor.lock().await;
        assert!(!extractor.is_initialized());
        assert_eq!(extractor.pending_tool_calls(), 0);
        drop(extractor);
        assert!(
            orch.nested_memory_history().await.nested.is_empty(),
            "clear must not carry old nested-memory history into the new session"
        );
        assert!(
            handle.in_flight.load(Ordering::Acquire),
            "the stale task owns releasing its reservation; clear must not open a concurrent slot"
        );
    }

    #[tokio::test]
    async fn request_exit_clears_only_the_current_sessions_invoked_skills() {
        let _registry = InvokedSkillRegistryGuard::acquire();
        let orch = crate::ConversationOrchestrator::new(
            crate::OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(Vec::new())),
            Arc::new(tool_api::registry::ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            std::env::temp_dir(),
        );
        let session_id = orch.session.lock().await.session_id.to_string();
        let other_session_id = lingxi_core::types::SessionId::new().to_string();
        let current_scope =
            compaction::invoked_skills::InvokedSkillScopeRef::new(Some(&session_id), None);
        let other_scope =
            compaction::invoked_skills::InvokedSkillScopeRef::new(Some(&other_session_id), None);
        compaction::invoked_skills::register_scoped(
            "current",
            std::path::Path::new("/current"),
            "current body",
            current_scope,
        );
        compaction::invoked_skills::register_scoped(
            "other",
            std::path::Path::new("/other"),
            "other body",
            other_scope,
        );

        lingxi_core::host::OrchestratorHandle::request_exit(&orch).await;

        assert!(compaction::invoked_skills::filter_for_scope(current_scope).is_empty());
        assert_eq!(
            compaction::invoked_skills::filter_for_scope(other_scope).len(),
            1,
            "exiting one session must not clear another session's registry rows"
        );
    }

    #[tokio::test]
    async fn session_end_teardown_clears_the_current_sessions_invoked_skills() {
        let _registry = InvokedSkillRegistryGuard::acquire();
        let orch = crate::ConversationOrchestrator::new(
            crate::OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(Vec::new())),
            Arc::new(tool_api::registry::ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            std::env::temp_dir(),
        );
        let session_id = orch.session.lock().await.session_id.to_string();
        let scope = compaction::invoked_skills::InvokedSkillScopeRef::new(
            Some(&session_id),
            Some("agent:child"),
        );
        compaction::invoked_skills::register_scoped(
            "child",
            std::path::Path::new("/child"),
            "child body",
            scope,
        );

        orch.fire_session_end("prompt_input_exit").await;

        assert!(compaction::invoked_skills::filter_for_scope(scope).is_empty());
    }

    #[tokio::test]
    async fn live_model_switch_does_not_wait_for_the_running_turn() {
        let orch = crate::ConversationOrchestrator::new(
            crate::OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(Vec::new())),
            Arc::new(tool_api::registry::ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            std::env::temp_dir(),
        );
        let _running_turn = orch.turn_gate.lock().await;
        let prepared_before = orch
            .prepare_model_call_snapshot(crate::conversation::ModelCallPath::Streaming, None, None)
            .await
            .expect("prepare in-flight request");
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            orch.switch_model("live-model", Some("live-provider")),
        )
        .await
        .expect("model controls must complete while the turn is active")
        .expect("model switch");
        let session = orch.session.lock().await;
        assert_eq!(session.model, "live-model");
        assert_eq!(session.model_profile.as_deref(), Some("live-provider"));
        drop(session);
        let prepared_after = orch
            .prepare_model_call_snapshot(crate::conversation::ModelCallPath::Streaming, None, None)
            .await
            .expect("prepare next request");
        assert_ne!(prepared_before.model, "live-model");
        assert_eq!(prepared_after.model, "live-model");
        assert_eq!(
            prepared_after.model_profile.as_deref(),
            Some("live-provider")
        );
    }

    /// A transcript can carry a PROVIDER-QUALIFIED model reference with no
    /// `modelProfile` alongside it — written by an older engine, by another
    /// client, or by any session whose profile was never persisted. Adopting it
    /// verbatim makes `session.model` the whole `"deepseek/deepseek-flash"`
    /// string, which is then sent as the WIRE model id: the provider 404s and
    /// the turn reports `model unavailable: the provider does not serve
    /// 'deepseek/deepseek-flash'` for every message, with no way out of that
    /// session short of re-picking a model.
    #[tokio::test]
    async fn hot_resume_splits_a_qualified_model_ref_with_no_profile() {
        let tools = Arc::new(tool_api::registry::ToolRegistry::new());
        let api = Arc::new(MockApiClient::new(Vec::new()));
        api.set_model_listings(vec![lingxi_core::host::ModelListing {
            display_model: "deepseek-flash".to_string(),
            request_model: "deepseek-flash".to_string(),
            provider_id: "deepseek".to_string(),
            provider_label: "DeepSeek".to_string(),
            description: None,
            metadata: Default::default(),
            capabilities: Default::default(),
            reasoning: Default::default(),
            supports_reasoning: true,
            fusion_analyst_capable: false,
            connection: Default::default(),
        }]);
        let orch = crate::ConversationOrchestrator::new(
            crate::OrchestratorConfig::default(),
            api,
            tools,
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            std::env::temp_dir(),
        );

        lingxi_core::host::OrchestratorHandle::resume_session(
            &orch,
            lingxi_core::types::SessionId::new(),
            Vec::new(),
            None,
            None,
            lingxi_core::host::ResumeRuntimeSnapshot {
                model: "deepseek/deepseek-flash".to_string(),
                model_profile: None,
                ..Default::default()
            },
        )
        .await
        .expect("hot resume");

        let session = orch.session.lock().await;
        assert_eq!(
            session.model, "deepseek-flash",
            "the resumed model must be the BARE wire id, never the qualified ref"
        );
        assert_eq!(session.model_profile.as_deref(), Some("deepseek"));
    }

    /// A persisted profile does not make a qualified model reference safe to
    /// use as the wire id. Some older/mobile session paths recorded BOTH
    /// `model = "deepseek/deepseek-flash"` and
    /// `modelProfile = "deepseek"`; trusting the latter left the qualified
    /// UI reference in `session.model` and every subsequent request failed
    /// model resolution. Normalize this shape exactly like the profile-less
    /// legacy row above.
    #[tokio::test]
    async fn hot_resume_splits_a_qualified_model_ref_with_matching_profile() {
        let api = Arc::new(MockApiClient::new(Vec::new()));
        api.set_model_listings(vec![lingxi_core::host::ModelListing {
            display_model: "deepseek-flash".to_string(),
            request_model: "deepseek-flash".to_string(),
            provider_id: "deepseek".to_string(),
            provider_label: "DeepSeek".to_string(),
            description: None,
            metadata: Default::default(),
            capabilities: Default::default(),
            reasoning: Default::default(),
            supports_reasoning: true,
            fusion_analyst_capable: false,
            connection: Default::default(),
        }]);
        let orch = crate::ConversationOrchestrator::new(
            crate::OrchestratorConfig::default(),
            api,
            Arc::new(tool_api::registry::ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            std::env::temp_dir(),
        );

        lingxi_core::host::OrchestratorHandle::resume_session(
            &orch,
            lingxi_core::types::SessionId::new(),
            Vec::new(),
            None,
            None,
            lingxi_core::host::ResumeRuntimeSnapshot {
                model: "deepseek/deepseek-flash".to_string(),
                model_profile: Some("deepseek".to_string()),
                ..Default::default()
            },
        )
        .await
        .expect("hot resume");

        let session = orch.session.lock().await;
        assert_eq!(session.model, "deepseek-flash");
        assert_eq!(session.model_profile.as_deref(), Some("deepseek"));
        drop(session);

        // The lower-level switch seam is also used outside the mobile command
        // adapter. It must uphold the same invariant even if a caller passes
        // the UI reference and explicit profile together.
        lingxi_core::host::OrchestratorHandle::switch_model(
            &orch,
            "deepseek/deepseek-flash",
            Some("deepseek"),
        )
        .await
        .expect("switch model");
        let session = orch.session.lock().await;
        assert_eq!(session.model, "deepseek-flash");
        assert_eq!(session.model_profile.as_deref(), Some("deepseek"));
    }

    /// The split must not corrupt a reference that is legitimately un-splittable:
    /// an OpenRouter wire id contains a slash of its own, and a bare id that no
    /// listing claims stays exactly as recorded.
    #[tokio::test]
    async fn hot_resume_leaves_unqualified_and_unknown_model_refs_alone() {
        let listings = vec![lingxi_core::host::ModelListing {
            display_model: "openrouter/auto".to_string(),
            request_model: "openrouter/auto".to_string(),
            provider_id: "openrouter".to_string(),
            provider_label: "OpenRouter".to_string(),
            description: None,
            metadata: Default::default(),
            capabilities: Default::default(),
            reasoning: Default::default(),
            supports_reasoning: false,
            fusion_analyst_capable: false,
            connection: Default::default(),
        }];
        for (recorded, want_model, want_profile) in [
            // An openrouter wire id whose OWN name contains a slash: the
            // qualified form splits to the full wire id, not to "auto".
            (
                "openrouter/openrouter/auto",
                "openrouter/auto",
                Some("openrouter"),
            ),
            // No listing claims this pair ⇒ keep the string verbatim.
            ("someproxy/some-model", "someproxy/some-model", None),
            ("claude-sonnet-5", "claude-sonnet-5", None),
        ] {
            let api = Arc::new(MockApiClient::new(Vec::new()));
            api.set_model_listings(listings.clone());
            let orch = crate::ConversationOrchestrator::new(
                crate::OrchestratorConfig::default(),
                api,
                Arc::new(tool_api::registry::ToolRegistry::new()),
                noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                Arc::new(MockOutputStream::new()),
                Arc::new(StaticMemoryProvider::empty()),
                std::env::temp_dir(),
            );
            lingxi_core::host::OrchestratorHandle::resume_session(
                &orch,
                lingxi_core::types::SessionId::new(),
                Vec::new(),
                None,
                None,
                lingxi_core::host::ResumeRuntimeSnapshot {
                    model: recorded.to_string(),
                    model_profile: None,
                    ..Default::default()
                },
            )
            .await
            .expect("hot resume");
            let session = orch.session.lock().await;
            assert_eq!(session.model, want_model, "recorded {recorded:?}");
            assert_eq!(
                session.model_profile.as_deref(),
                want_profile,
                "recorded {recorded:?}"
            );
        }
    }

    #[tokio::test]
    async fn hot_resume_restores_compaction_visibility_and_deferred_tools() {
        let tools = Arc::new(tool_api::registry::ToolRegistry::new());
        let orch = crate::ConversationOrchestrator::new(
            crate::OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(Vec::new())),
            tools.clone(),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            std::env::temp_dir(),
        );
        let transcript_only = lingxi_core::types::MessageId::new();
        let compact_summary = lingxi_core::types::MessageId::new();
        let virtual_user = lingxi_core::types::MessageId::new();
        let skill_message = lingxi_core::types::MessageId::new();
        tools.deferral().mark_loaded(["StaleTool"]);
        orch.prompt_runtime
            .sent_skill_names
            .lock()
            .await
            .insert("stale-skill".to_string());
        orch.compaction_runtime
            .last_response_input_tokens
            .store(99, std::sync::atomic::Ordering::Relaxed);
        orch.compaction_runtime
            .last_response_output_tokens
            .store(88, std::sync::atomic::Ordering::Relaxed);

        lingxi_core::host::OrchestratorHandle::resume_session(
            &orch,
            lingxi_core::types::SessionId::new(),
            Vec::new(),
            None,
            None,
            lingxi_core::host::ResumeRuntimeSnapshot {
                current_usage: Some(lingxi_core::host::CurrentUsageSnapshot {
                    input_tokens: 100,
                    output_tokens: 40,
                    cache_read_input_tokens: 200,
                    cache_creation_input_tokens: 300,
                }),
                model: "claude-opus-4-8".to_string(),
                model_profile: Some("anthropic".to_string()),
                effort: Some("high".to_string()),
                reasoning_selection: None,
                main_thread_agent_type: None,
                main_thread_agent_definition: None,
                transcript_only_message_ids: vec![transcript_only],
                compact_summary_message_ids: vec![compact_summary],
                virtual_user_message_ids: vec![virtual_user],
                compacted_user_turns: 7,
                model_context_excluded_message_ids: Vec::new(),
                loaded_tool_names: vec!["DeferredTool".to_string()],
                post_compact_skill_attachments: vec![(
                    skill_message,
                    vec!["body with\n\n---\n\na legal separator".to_string()],
                )],
                cumulative_dropped_tokens: 4_321,
                model_reminder_attachments: Vec::new(),
                compacted: true,
                turn_counter: 3,
                turn_id: "turn-after-compact".to_string(),
                consecutive_failures: 2,
                consecutive_rapid_refills: 1,
                deferred_tools: Vec::new(),
                prompt_snapshot: None,
                context_rendering_hint: None,
            },
        )
        .await
        .expect("hot resume");

        let session = orch.session.lock().await;
        assert_eq!(session.model, "claude-opus-4-8");
        assert_eq!(session.model_profile.as_deref(), Some("anthropic"));
        assert!(session.transcript_only_messages.contains(&transcript_only));
        assert!(session.compact_summary_messages.contains(&compact_summary));
        assert!(session.virtual_user_messages.contains(&virtual_user));
        assert_eq!(session.compacted_user_turns, 7);
        drop(session);
        assert_eq!(
            orch.compaction_runtime
                .compaction_cumulative_dropped_tokens
                .load(std::sync::atomic::Ordering::Relaxed),
            4_321
        );
        let tracking = orch.compaction_runtime.compaction_tracking.lock().await;
        assert!(tracking.compacted);
        assert_eq!(tracking.turn_counter, 3);
        assert_eq!(tracking.turn_id, "turn-after-compact");
        assert_eq!(tracking.consecutive_failures, 2);
        assert_eq!(tracking.consecutive_rapid_refills, 1);
        drop(tracking);
        assert!(tools.deferral().is_loaded("DeferredTool"));
        assert!(!tools.deferral().is_loaded("StaleTool"));
        assert!(orch.prompt_runtime.sent_skill_names.lock().await.is_empty());
        assert_eq!(
            orch.compaction_runtime
                .last_response_input_tokens
                .load(std::sync::atomic::Ordering::Relaxed),
            600
        );
        assert_eq!(
            orch.compaction_runtime
                .last_response_output_tokens
                .load(std::sync::atomic::Ordering::Relaxed),
            40
        );
        assert_eq!(
            orch.transcript
                .post_compact_skill_attachments
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&skill_message)
                .cloned(),
            Some(vec!["body with\n\n---\n\na legal separator".to_string()]),
            "hot resume restores structured skill attachment identity"
        );
        assert_eq!(
            orch.model_runtime
                .current_effort
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_deref(),
            Some("high"),
            "an unpinned runtime inherits transcript effort"
        );

        lingxi_core::host::OrchestratorHandle::resume_session(
            &orch,
            lingxi_core::types::SessionId::new(),
            Vec::new(),
            None,
            None,
            lingxi_core::host::ResumeRuntimeSnapshot::default(),
        )
        .await
        .expect("second hot resume");
        assert_eq!(
            *orch
                .model_runtime
                .current_effort
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            None,
            "an unpinned legacy transcript clears inherited effort"
        );
    }

    #[tokio::test]
    async fn files_in_context_excludes_host_seed_entries() {
        let orch = crate::ConversationOrchestrator::new(
            crate::OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(Vec::new())),
            Arc::new(tool_api::registry::ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            std::env::temp_dir(),
        );
        let visible = std::env::temp_dir().join("visible.txt");
        let seeded = std::env::temp_dir().join("seeded.txt");
        tool_api::read_file_state::set(
            &orch.prompt_runtime.read_state_map,
            visible.clone(),
            tool_api::read_file_state::ReadFileEntry {
                content: "visible".into(),
                mtime_ms: 1,
                offset: None,
                limit: None,
                from_read: true,
                seeded_from_context: false,
                is_partial_view: false,
            },
        );
        tool_api::read_file_state::set_with_model_context(
            &orch.prompt_runtime.read_state_map,
            seeded,
            tool_api::read_file_state::ReadFileEntry {
                content: "seeded".into(),
                mtime_ms: 1,
                offset: None,
                limit: None,
                from_read: false,
                seeded_from_context: false,
                is_partial_view: false,
            },
            false,
        );

        assert_eq!(
            lingxi_core::host::OrchestratorHandle::files_in_context(&orch).await,
            vec![visible]
        );
    }

    #[tokio::test]
    async fn hot_resume_preserves_explicit_launch_effort() {
        let orch = crate::ConversationOrchestrator::new(
            crate::OrchestratorConfig {
                effort: Some("low".to_string()),
                ..crate::OrchestratorConfig::default()
            },
            Arc::new(MockApiClient::new(Vec::new())),
            Arc::new(tool_api::registry::ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            std::env::temp_dir(),
        );

        lingxi_core::host::OrchestratorHandle::resume_session(
            &orch,
            lingxi_core::types::SessionId::new(),
            Vec::new(),
            None,
            None,
            lingxi_core::host::ResumeRuntimeSnapshot {
                effort: Some("high".to_string()),
                ..lingxi_core::host::ResumeRuntimeSnapshot::default()
            },
        )
        .await
        .expect("hot resume");

        assert_eq!(
            orch.model_runtime
                .current_effort
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_deref(),
            Some("low"),
            "explicit launch effort outranks transcript resume metadata"
        );
    }

    #[tokio::test]
    async fn hot_resume_replaces_main_thread_agent_and_its_hooks() {
        use crate::test_support::{
            noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
            StaticMemoryProvider,
        };
        use std::collections::HashMap;
        use std::sync::Arc;

        let orch = crate::ConversationOrchestrator::new(
            crate::OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(Vec::new())),
            Arc::new(tool_api::registry::ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            std::env::temp_dir(),
        );
        let definition = agent::AgentDefinition {
            omit_instructions: false,
            cache_ttl: None,
            agent_type: "reviewer".to_string(),
            when_to_use: "review code".to_string(),
            tools: agent::AgentToolPolicy::Explicit(vec!["Read".to_string()]),
            max_turns: 4,
            model: agent::AgentModel::Inherit,
            permission_mode: agent::AgentPermissionMode::Bubble,
            source: agent::AgentSource::Settings(lingxi_core::types::SettingsScope::User),
            base_dir: std::env::temp_dir(),
            system_prompt: Some("review carefully".to_string()),
            mcp_servers: Vec::new(),
            frontmatter_hooks: vec![hooks::HookDefinition {
                id: lingxi_core::types::HookId::new(),
                name: "resumed-agent-stop".to_string(),
                events: vec![hooks::HookEventType::Stop],
                if_condition: None,
                executor: hooks::HookExecutor::Command {
                    command: "true".to_string(),
                    args: Vec::new(),
                    env: HashMap::new(),
                    cwd: None,
                    shell: None,
                },
                source: hooks::HookSource::Settings(lingxi_core::types::SettingsScope::User),
                blocking: true,
                timeout: None,
                priority: 0,
                once: false,
                status_message: None,
                async_rewake: false,
                async_timeout: None,
                rewake_message: None,
            }],
            icon: None,
            allowed_tools: Vec::new(),
            worktree_requirement: None,
            disallowed_tools: vec!["Write".to_string()],
            skills: Vec::new(),
            required_mcp_servers: Vec::new(),
            background: false,
            isolation: None,
            memory: None,
            effort: None,
            initial_prompt: None,
            color: None,
            observer: None,
            offer_provider: None,
        };

        lingxi_core::host::OrchestratorHandle::resume_session(
            &orch,
            lingxi_core::types::SessionId::new(),
            Vec::new(),
            None,
            None,
            lingxi_core::host::ResumeRuntimeSnapshot {
                main_thread_agent_type: Some("reviewer".to_string()),
                main_thread_agent_definition: Some(
                    serde_json::to_value(definition).expect("serialize agent snapshot"),
                ),
                ..lingxi_core::host::ResumeRuntimeSnapshot::default()
            },
        )
        .await
        .expect("resume agent session");

        {
            let restored = orch.lifecycle_runtime.main_thread_agent.read().await;
            let restored = restored.as_ref().expect("resumed main-thread agent");
            assert_eq!(restored.agent_type, "reviewer");
            assert_eq!(restored.system_prompt.as_deref(), Some("review carefully"));
        }
        assert!(orch
            .lifecycle_runtime
            .main_thread_agent_hook_id
            .lock()
            .await
            .is_some());
        assert!(orch.hooks.has_hooks_for(&hooks::HookEventType::Stop).await);

        lingxi_core::host::OrchestratorHandle::resume_session(
            &orch,
            lingxi_core::types::SessionId::new(),
            Vec::new(),
            None,
            None,
            lingxi_core::host::ResumeRuntimeSnapshot::default(),
        )
        .await
        .expect("resume default-agent session");

        assert!(orch
            .lifecycle_runtime
            .main_thread_agent
            .read()
            .await
            .is_none());
        assert!(orch
            .lifecycle_runtime
            .main_thread_agent_hook_id
            .lock()
            .await
            .is_none());
        assert!(!orch.hooks.has_hooks_for(&hooks::HookEventType::Stop).await);
    }

    /// gap218 #43 — a minimal resumed agent definition carrying `model`.
    #[cfg(test)]
    fn resume_definition_with_model(model: agent::AgentModel) -> agent::AgentDefinition {
        agent::AgentDefinition {
            omit_instructions: false,
            cache_ttl: None,
            agent_type: "modelful".to_string(),
            when_to_use: "does things".to_string(),
            tools: agent::AgentToolPolicy::Explicit(Vec::new()),
            max_turns: 4,
            model,
            permission_mode: agent::AgentPermissionMode::Bubble,
            source: agent::AgentSource::Settings(lingxi_core::types::SettingsScope::User),
            base_dir: std::env::temp_dir(),
            system_prompt: Some("do it".to_string()),
            mcp_servers: Vec::new(),
            frontmatter_hooks: Vec::new(),
            icon: None,
            allowed_tools: Vec::new(),
            worktree_requirement: None,
            disallowed_tools: Vec::new(),
            skills: Vec::new(),
            required_mcp_servers: Vec::new(),
            background: false,
            isolation: None,
            memory: None,
            effort: None,
            initial_prompt: None,
            color: None,
            observer: None,
            offer_provider: None,
        }
    }

    /// gap218 #43 (cc 2.1.218 `NQe`) — the in-place resume adopts the resumed
    /// agent's frontmatter `model` (resolved alias → wire id) when the user did
    /// NOT pass `--model` (`apply_resumed_agent_model == true`). Locks the
    /// resolved id against the SAME resolver (no invented bytes).
    #[tokio::test]
    async fn hot_resume_adopts_agent_frontmatter_model_when_no_user_model() {
        use crate::test_support::{
            noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
            StaticMemoryProvider,
        };
        use std::sync::Arc;

        let orch = crate::ConversationOrchestrator::new(
            crate::OrchestratorConfig {
                apply_resumed_agent_model: true,
                ..crate::OrchestratorConfig::default()
            },
            Arc::new(MockApiClient::new(Vec::new())),
            Arc::new(tool_api::registry::ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            std::env::temp_dir(),
        )
        .with_model_resolution_context_provider(Arc::new(
            |model: &str, profile: Option<&str>| {
                // Installing the provider also refreshes the constructor's
                // initial route, which has no selected provider profile yet.
                // The resumed frontmatter alias and its resolved route must
                // both receive the profile already selected for the session.
                if matches!(model, "opus" | "configured-resume-opus") {
                    assert_eq!(profile, Some("resume-profile"));
                }
                Ok(agent::model_resolution::ModelResolutionContext {
                    route: agent::model_resolution::ModelRouteFacts {
                        model: model.to_owned(),
                        profile: profile.map(str::to_owned),
                        ..Default::default()
                    },
                    family_defaults: agent::model_resolution::FamilyModelDefaults {
                        opus: Some("configured-resume-opus".into()),
                        ..Default::default()
                    },
                    ..Default::default()
                })
            },
        ));
        let sentinel = "sentinel-session-model".to_string();
        {
            let mut session = orch.session.lock().await;
            session.model = sentinel.clone();
            session.model_profile = Some("resume-profile".into());
        }

        let def = resume_definition_with_model(agent::AgentModel::Alias("opus".to_string()));
        orch.restore_main_thread_agent_for_test(
            Some("modelful".to_string()),
            Some(serde_json::to_value(&def).expect("serialize agent snapshot")),
        )
        .await
        .expect("resume main-thread agent route resolves");

        let expected = "configured-resume-opus";
        let got = orch.session.lock().await.model.clone();
        assert_eq!(
            got, expected,
            "resumed agent's frontmatter model replaces the session model"
        );
        assert_ne!(got, sentinel, "the session model actually changed");
        assert_eq!(
            orch.session.lock().await.model_profile.as_deref(),
            Some("resume-profile"),
            "the resolved model keeps the profile used to select its route"
        );
    }

    #[tokio::test]
    async fn hot_resume_keeps_the_resolved_empty_or_missing_profile() {
        use crate::test_support::{
            noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
            StaticMemoryProvider,
        };

        for selected_profile in [None, Some(String::new())] {
            let expected_profile = selected_profile.clone();
            let orch = crate::ConversationOrchestrator::new(
                crate::OrchestratorConfig {
                    model: "initial-wire-model".into(),
                    apply_resumed_agent_model: true,
                    ..crate::OrchestratorConfig::default()
                },
                Arc::new(MockApiClient::new(Vec::new())),
                Arc::new(tool_api::registry::ToolRegistry::new()),
                noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                Arc::new(MockOutputStream::new()),
                Arc::new(StaticMemoryProvider::empty()),
                std::env::temp_dir(),
            )
            .with_model_resolution_context_provider(Arc::new(
                move |model: &str, profile: Option<&str>| {
                    if model == "opus" {
                        // Alias discovery starts from the resumed profile
                        // hint. The host's resolved None/empty route then
                        // owns the canonical wire-model lookup below.
                        assert_eq!(profile, Some("old-profile"));
                    } else if model == "shared-wire-model" {
                        assert_eq!(profile, selected_profile.as_deref());
                    }
                    Ok(agent::model_resolution::ModelResolutionContext {
                        route: agent::model_resolution::ModelRouteFacts {
                            model: model.to_owned(),
                            profile: selected_profile.clone(),
                            ..Default::default()
                        },
                        family_defaults: agent::model_resolution::FamilyModelDefaults {
                            opus: Some("shared-wire-model".into()),
                            ..Default::default()
                        },
                        ..Default::default()
                    })
                },
            ));
            orch.session.lock().await.model_profile = Some("old-profile".into());
            let definition = resume_definition_with_model(agent::AgentModel::Alias("opus".into()));
            orch.restore_main_thread_agent_for_test(
                Some("modelful".into()),
                Some(serde_json::to_value(definition).unwrap()),
            )
            .await
            .expect("resume main-thread agent route resolves");
            let session = orch.session.lock().await;
            assert_eq!(session.model, "shared-wire-model");
            assert_eq!(session.model_profile, expected_profile);
        }
    }

    #[tokio::test]
    async fn hot_resume_qualified_agent_model_changes_provider_profile() {
        use crate::test_support::{
            noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
            StaticMemoryProvider,
        };

        let orch = crate::ConversationOrchestrator::new(
            crate::OrchestratorConfig {
                model: "claude-parent".into(),
                apply_resumed_agent_model: true,
                ..Default::default()
            },
            Arc::new(MockApiClient::new(Vec::new())),
            Arc::new(tool_api::registry::ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            std::env::temp_dir(),
        )
        .with_model_resolution_context_provider(Arc::new(
            |model: &str, profile: Option<&str>| {
                let (model, profile) = match model {
                    "openai/gpt-4o" => {
                        if profile == Some("anthropic") {
                            return Err(
                                agent::model_resolution::ModelResolutionError::RouteUnavailable {
                                    model: model.into(),
                                    profile: profile.map(str::to_owned),
                                    reason: "model is unavailable on the parent's profile".into(),
                                },
                            );
                        }
                        assert_eq!(
                            profile, None,
                            "qualified models own their provider selection"
                        );
                        ("gpt-4o", Some("openai"))
                    }
                    "gpt-4o" => {
                        assert_eq!(profile, Some("openai"));
                        (model, profile)
                    }
                    _ => (model, Some("anthropic")),
                };
                Ok(agent::model_resolution::ModelResolutionContext {
                    route: agent::model_resolution::ModelRouteFacts {
                        model: model.into(),
                        profile: profile.map(str::to_owned),
                        ..Default::default()
                    },
                    ..Default::default()
                })
            },
        ));
        orch.session.lock().await.model_profile = Some("anthropic".into());
        let definition =
            resume_definition_with_model(agent::AgentModel::Explicit("openai/gpt-4o".into()));
        orch.restore_main_thread_agent_for_test(
            Some("modelful".into()),
            Some(serde_json::to_value(definition).unwrap()),
        )
        .await
        .expect("qualified resumed model has a configured route");
        let session = orch.session.lock().await;
        assert_eq!(session.model, "gpt-4o");
        assert_eq!(session.model_profile.as_deref(), Some("openai"));
    }

    #[tokio::test]
    async fn hot_resume_model_route_error_preserves_live_agent_and_model() {
        use crate::test_support::{
            noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
            StaticMemoryProvider,
        };

        struct OutputAccount {
            session: lingxi_core::types::SessionId,
            generation: lingxi_core::types::MessageId,
        }
        impl lingxi_core::host::WorkflowOutputAccount for OutputAccount {
            fn session_id(&self) -> lingxi_core::types::SessionId {
                self.session
            }
            fn generation_id(&self) -> lingxi_core::types::MessageId {
                self.generation
            }
            fn spent(&self) -> u64 {
                0
            }
            fn record_legacy(
                &self,
                _: lingxi_core::host::WorkflowOutputEventId,
                _: u64,
            ) -> Result<(), lingxi_core::host::BudgetError> {
                Ok(())
            }
        }
        struct OutputScopes {
            old: lingxi_core::host::WorkflowOutputScope,
            prepared: std::sync::Mutex<Vec<lingxi_core::types::SessionId>>,
        }
        #[async_trait::async_trait]
        impl lingxi_core::host::WorkflowOutputScopes for OutputScopes {
            async fn ensure_current(
                &self,
                session: lingxi_core::types::SessionId,
                _: lingxi_core::types::MessageId,
                _: Option<u64>,
            ) -> Result<lingxi_core::host::WorkflowOutputScope, lingxi_core::host::BudgetError>
            {
                self.prepared.lock().unwrap().push(session);
                self.capture(session)
            }
            async fn begin_turn(
                &self,
                session: lingxi_core::types::SessionId,
                _: lingxi_core::types::MessageId,
                _: Option<u64>,
            ) -> Result<lingxi_core::host::WorkflowOutputScope, lingxi_core::host::BudgetError>
            {
                self.capture(session)
            }
            fn capture(
                &self,
                session: lingxi_core::types::SessionId,
            ) -> Result<lingxi_core::host::WorkflowOutputScope, lingxi_core::host::BudgetError>
            {
                if session == self.old.session_id() {
                    Ok(self.old.clone())
                } else {
                    Err(lingxi_core::host::BudgetError::Internal(
                        "output preparation ran before model preflight".into(),
                    ))
                }
            }
        }

        let temp = tempfile::tempdir().unwrap();
        let session_a = lingxi_core::types::SessionId::new();
        let session_b = lingxi_core::types::SessionId::new();
        let (persist_tx, _persist_rx) = tokio::sync::mpsc::channel(8);
        let tracker = Arc::new(cost::CostTracker::new(
            session_a,
            Arc::new(cost::PricingCatalog::empty()),
            persist_tx,
        ));
        let cwd = temp.path().join("workspace");
        std::fs::create_dir_all(&cwd).unwrap();
        let authority = temp.path().join("authority");
        std::fs::create_dir_all(&authority).unwrap();
        let lock = Arc::new(session::jsonl::DurableTranscriptWriter::open(&authority).unwrap());
        let path_a = session::jsonl::path::session_path(
            temp.path(),
            &cwd.to_string_lossy(),
            &session_a.as_uuid().to_string(),
        );
        let fs: Arc<dyn lingxi_core::host::FileSystem> = Arc::new(
            platform_posix::fs::PosixFileSystem::new(temp.path().to_path_buf()),
        );
        let writer =
            Arc::new(session::jsonl::JsonlWriter::new(path_a.clone(), fs).with_durable_lock(lock));
        writer
            .activate_session_target(session_a, path_a.clone(), cwd.clone())
            .unwrap();
        let output_scopes = Arc::new(OutputScopes {
            old: lingxi_core::host::WorkflowOutputScope::new(Arc::new(OutputAccount {
                session: session_a,
                generation: lingxi_core::types::MessageId::new(),
            })),
            prepared: std::sync::Mutex::new(Vec::new()),
        });
        let observer = Arc::new(RecordingSessionActivationObserver::default());
        let api = Arc::new(MockApiClient::new(Vec::new()));
        let orch = crate::ConversationOrchestrator::new(
            crate::OrchestratorConfig {
                model: "live-model".into(),
                apply_resumed_agent_model: true,
                ..Default::default()
            },
            api.clone(),
            Arc::new(tool_api::registry::ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            cwd,
        )
        .with_session_id(session_a)
        .with_jsonl_writer(writer.clone())
        .with_config_home(temp.path().to_path_buf())
        .with_cost_tracker(tracker.clone())
        .with_workflow_output_scopes(output_scopes.clone())
        .with_session_activation_observer(observer.clone())
        .with_model_resolution_context_provider(Arc::new(
            |model: &str, profile: Option<&str>| {
                if model == "resumed-model" {
                    assert_eq!(profile, Some("resumed-profile"));
                }
                if model == "live-model" {
                    assert!(
                        profile.is_none(),
                        "resume preflight must use the target session route"
                    );
                }
                if model == "openai/unavailable" {
                    return Err(
                        agent::model_resolution::ModelResolutionError::RouteUnavailable {
                            model: model.into(),
                            profile: profile.map(str::to_owned),
                            reason: "configured model is unavailable".into(),
                        },
                    );
                }
                Ok(agent::model_resolution::ModelResolutionContext {
                    route: agent::model_resolution::ModelRouteFacts {
                        model: model.into(),
                        profile: profile.map(str::to_owned),
                        ..Default::default()
                    },
                    ..Default::default()
                })
            },
        ));
        let old_message = lingxi_core::types::ConversationMessage::user_meta(
            lingxi_core::types::MessageId::new(),
            "old conversation".into(),
        );
        {
            let mut session = orch.session.lock().await;
            session.model_profile = Some("live-profile".into());
            session.history.push(old_message.clone());
        }
        let old_output = orch.prepare_output_session(session_a).await.unwrap();
        orch.install_output_session(old_output);
        output_scopes.prepared.lock().unwrap().clear();
        orch.set_main_thread_agent(
            "previous-agent".into(),
            Some("previous prompt".into()),
            agent::AgentToolPolicy::All {
                use_exact_tools: false,
            },
            Vec::new(),
            None,
        )
        .await;
        let previous_hook = hooks::HookDefinition {
            id: lingxi_core::types::HookId::new(),
            name: "previous-agent-stop".into(),
            events: vec![hooks::HookEventType::Stop],
            if_condition: None,
            executor: hooks::HookExecutor::Command {
                command: "true".into(),
                args: Vec::new(),
                env: Default::default(),
                cwd: None,
                shell: None,
            },
            source: hooks::HookSource::Settings(lingxi_core::types::SettingsScope::User),
            blocking: true,
            timeout: None,
            priority: 0,
            once: false,
            status_message: None,
            async_rewake: false,
            async_timeout: None,
            rewake_message: None,
        };
        orch.replace_main_thread_agent_hooks(&[previous_hook]).await;
        let previous_hook_id = *orch
            .lifecycle_runtime
            .main_thread_agent_hook_id
            .lock()
            .await;
        assert!(previous_hook_id.is_some());
        orch.compaction_runtime
            .compaction_cumulative_dropped_tokens
            .store(123, Ordering::Relaxed);
        let old_refusal = orch.model_runtime.refusal_selection.lock().unwrap().clone();
        let definition =
            resume_definition_with_model(agent::AgentModel::Explicit("openai/unavailable".into()));
        let error = lingxi_core::host::OrchestratorHandle::resume_session(
            &orch,
            session_b,
            vec![lingxi_core::types::ConversationMessage::user_meta(
                lingxi_core::types::MessageId::new(),
                "resumed conversation".into(),
            )],
            None,
            None,
            lingxi_core::host::ResumeRuntimeSnapshot {
                model: "resumed-model".into(),
                model_profile: Some("resumed-profile".into()),
                main_thread_agent_type: Some("modelful".into()),
                main_thread_agent_definition: Some(serde_json::to_value(definition).unwrap()),
                cumulative_dropped_tokens: 999,
                ..Default::default()
            },
        )
        .await
        .expect_err("invalid resumed routes must surface their error");
        assert!(error
            .to_string()
            .contains("configured model is unavailable"));
        assert_eq!(
            orch.main_thread_agent_type().await.as_deref(),
            Some("previous-agent")
        );
        let session = orch.session.lock().await;
        assert_eq!(session.session_id, session_a);
        assert_eq!(session.history.len(), 1);
        assert_eq!(session.history[0].id(), old_message.id());
        assert_eq!(session.model, "live-model");
        assert_eq!(session.model_profile.as_deref(), Some("live-profile"));
        drop(session);
        assert_eq!(tracker.session_id().await, session_a);
        assert_eq!(writer.session_target_path(session_a), Some(path_a));
        assert!(writer.session_target_path(session_b).is_none());
        assert!(output_scopes.prepared.lock().unwrap().is_empty());
        assert!(orch.capture_main_output().await.unwrap().is_some());
        assert_eq!(
            *orch
                .lifecycle_runtime
                .main_thread_agent_hook_id
                .lock()
                .await,
            previous_hook_id
        );
        assert!(observer.activations.lock().unwrap().is_empty());
        assert_eq!(api.close_responses_ws_count().await, 0);
        assert_eq!(
            orch.compaction_runtime
                .compaction_cumulative_dropped_tokens
                .load(Ordering::Relaxed),
            123
        );
        assert_eq!(
            *orch.model_runtime.refusal_selection.lock().unwrap(),
            old_refusal
        );
    }

    /// gap218 #43 — the gate. When the user DID pass `--model`
    /// (`apply_resumed_agent_model == false`, the parity default), the resumed
    /// agent's frontmatter model must NOT override the session model; and an
    /// `Inherit` agent never overrides even with the gate open.
    #[tokio::test]
    async fn hot_resume_keeps_session_model_when_gated_or_inherit() {
        use crate::test_support::{
            noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
            StaticMemoryProvider,
        };
        use std::sync::Arc;

        // (1) Gate CLOSED (explicit --model): an aliased agent model is ignored.
        let orch = crate::ConversationOrchestrator::new(
            crate::OrchestratorConfig::default(), // apply_resumed_agent_model == false
            Arc::new(MockApiClient::new(Vec::new())),
            Arc::new(tool_api::registry::ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            std::env::temp_dir(),
        );
        let user_model = "user-picked-model".to_string();
        orch.session.lock().await.model = user_model.clone();
        let def = resume_definition_with_model(agent::AgentModel::Alias("opus".to_string()));
        orch.restore_main_thread_agent_for_test(
            Some("modelful".to_string()),
            Some(serde_json::to_value(&def).expect("serialize agent snapshot")),
        )
        .await
        .expect("resume main-thread agent route resolves");
        assert_eq!(
            orch.session.lock().await.model,
            user_model,
            "an explicit --model is never overridden by agent frontmatter"
        );

        // (2) Gate OPEN but agent inherits: still no override.
        let orch2 = crate::ConversationOrchestrator::new(
            crate::OrchestratorConfig {
                apply_resumed_agent_model: true,
                ..crate::OrchestratorConfig::default()
            },
            Arc::new(MockApiClient::new(Vec::new())),
            Arc::new(tool_api::registry::ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            std::env::temp_dir(),
        );
        let inherited = "inherited-session-model".to_string();
        orch2.session.lock().await.model = inherited.clone();
        let def2 = resume_definition_with_model(agent::AgentModel::Inherit);
        orch2
            .restore_main_thread_agent_for_test(
                Some("modelful".to_string()),
                Some(serde_json::to_value(&def2).expect("serialize agent snapshot")),
            )
            .await
            .expect("resume main-thread agent route resolves");
        assert_eq!(
            orch2.session.lock().await.model,
            inherited,
            "AgentModel::Inherit keeps the session model"
        );
    }

    #[test]
    fn apply_mcp_disabled_adds_removes_and_is_idempotent() {
        let s = |v: &[&str]| v.iter().map(|x| (*x).to_string()).collect::<Vec<_>>();

        // Disable adds absent targets (dedup) and reports them affected.
        let (list, aff) = apply_mcp_disabled(s(&["a"]), &s(&["b", "c"]), true);
        assert_eq!(list, s(&["a", "b", "c"]));
        assert_eq!(aff, s(&["b", "c"]));

        // Re-disabling an already-disabled server is a no-op (not affected).
        let (list, aff) = apply_mcp_disabled(s(&["a", "b"]), &s(&["b"]), true);
        assert_eq!(list, s(&["a", "b"]));
        assert!(aff.is_empty());

        // Enable removes present targets; absent ones are no-ops.
        let (list, aff) = apply_mcp_disabled(s(&["a", "b", "c"]), &s(&["b", "z"]), false);
        assert_eq!(list, s(&["a", "c"]));
        assert_eq!(aff, s(&["b"]));

        // Enabling on an empty list is a clean no-op.
        let (list, aff) = apply_mcp_disabled(Vec::new(), &s(&["x"]), false);
        assert!(list.is_empty() && aff.is_empty());
    }

    /// `/context` follows the live model and history rather than cumulative
    /// provider usage (which does not shrink after compaction).
    #[tokio::test]
    async fn context_window_usage_is_live_and_model_aware() {
        use crate::test_support::{
            noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
            StaticMemoryProvider,
        };
        use std::sync::Arc;

        let orch = crate::ConversationOrchestrator::new(
            crate::OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(Vec::new())),
            Arc::new(tool_api::registry::ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            std::env::temp_dir(),
        );
        {
            let mut s = orch.session.lock().await;
            s.model = "claude-opus-5".to_string();
            s.usage.add(&lingxi_core::token::Usage {
                input_tokens: 1_200,
                output_tokens: 345,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
            });
            s.history
                .push(lingxi_core::types::ConversationMessage::user(
                    lingxi_core::types::MessageId::new(),
                    "live context after compact".repeat(100),
                ));
        }
        let expected_messages = {
            let s = orch.session.lock().await;
            compaction::grouping::estimate_tokens_for_range(&s.history)
        };
        let snapshot = orch.context_usage_snapshot().await;
        let used = snapshot.live_context_tokens;
        let max = snapshot.max_context_tokens;
        let messages = snapshot
            .breakdown
            .iter()
            .find(|row| row.kind == lingxi_core::host::ContextUsageCategoryKind::Messages)
            .map_or(0, |row| row.tokens);
        assert_eq!(messages, expected_messages, "history estimator must match");
        assert_ne!(used, 1_545, "cumulative provider usage must not leak in");
        assert_eq!(max, 1_000_000, "Opus 5 has a native 1M window");
        assert_eq!(
            snapshot.breakdown.iter().map(|row| row.tokens).sum::<u64>(),
            max,
            "used + reserved buffer + free space must cover the window"
        );
    }

    #[tokio::test]
    async fn ultracode_control_and_effort_selection_remain_independent() {
        use crate::test_support::{
            noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
            StaticMemoryProvider,
        };
        use lingxi_core::host::effort_table::SessionEffort;
        use lingxi_core::host::OrchestratorHandle;
        let orch = crate::ConversationOrchestrator::new(
            crate::OrchestratorConfig {
                effort: Some("low".into()),
                ultracode: true,
                ..Default::default()
            },
            std::sync::Arc::new(MockApiClient::new(Vec::new())),
            std::sync::Arc::new(tool_api::registry::ToolRegistry::new()),
            noop_hook_executor(),
            std::sync::Arc::new(NoOpPermissionGate),
            std::sync::Arc::new(MockOutputStream::new()),
            std::sync::Arc::new(StaticMemoryProvider::empty()),
            std::env::temp_dir(),
        );
        assert!(orch.ultracode_enabled().await);
        assert_eq!(orch.current_effort().await.as_deref(), Some("low"));
        orch.set_session_effort(SessionEffort::Level(serde_json::json!("high")))
            .await
            .unwrap();
        assert!(orch.ultracode_enabled().await);
        orch.set_ultracode_enabled(false).await.unwrap();
        assert_eq!(orch.current_effort().await.as_deref(), Some("high"));
        orch.set_session_effort(SessionEffort::Default)
            .await
            .unwrap();
        assert!(!orch.ultracode_enabled().await);
        assert_eq!(orch.current_effort().await, None);
    }

    #[tokio::test]
    async fn handle_reads_the_real_plan_store_and_updates_live_effort() {
        use crate::test_support::{
            noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
            StaticMemoryProvider,
        };
        use std::sync::Arc;

        let root = std::env::temp_dir().join(format!(
            "lingxi-handle-plan-{}-{}",
            std::process::id(),
            lingxi_core::types::SessionId::new().as_uuid()
        ));
        std::fs::create_dir_all(&root).expect("create plan root");
        let orch = crate::ConversationOrchestrator::new(
            crate::OrchestratorConfig {
                plans_directory: Some(root.to_string_lossy().into_owned()),
                ..crate::OrchestratorConfig::default()
            },
            Arc::new(MockApiClient::new(Vec::new())),
            Arc::new(tool_api::registry::ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            std::env::temp_dir(),
        );

        assert!(lingxi_core::host::OrchestratorHandle::current_plan(&orch)
            .await
            .expect("read absent plan")
            .is_none());
        let session_id = orch.session.lock().await.session_id;
        let path = ConversationOrchestrator::plan_file_path(
            &session_id,
            &orch.cwd,
            orch.config.plans_directory.as_deref(),
        );
        std::fs::write(&path, "# Plan\n\n- ship it\n").expect("write plan");
        let plan = lingxi_core::host::OrchestratorHandle::current_plan(&orch)
            .await
            .expect("read plan")
            .expect("plan exists");
        assert_eq!(plan.path, std::path::PathBuf::from(path));
        assert_eq!(plan.content, "# Plan\n\n- ship it\n");

        lingxi_core::host::OrchestratorHandle::set_session_effort(
            &orch,
            lingxi_core::host::effort_table::SessionEffort::Level(serde_json::json!("xhigh")),
        )
        .await
        .expect("set effort");
        assert_eq!(
            lingxi_core::host::OrchestratorHandle::current_effort(&orch)
                .await
                .as_deref(),
            Some("xhigh")
        );
        std::fs::remove_dir_all(root).ok();
    }

    struct BlockingSessionPreparer {
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
        destination: std::sync::Mutex<Option<lingxi_core::types::SessionId>>,
        transcript_lock: Arc<session::jsonl::DurableTranscriptWriter>,
    }

    #[async_trait::async_trait]
    impl crate::conversation::CostSessionSwitcher for BlockingSessionPreparer {
        async fn prepare_session(
            &self,
            tracker: Arc<cost::CostTracker>,
            session_id: lingxi_core::types::SessionId,
        ) -> Result<crate::conversation::PreparedSessionSwitch, cost::CostPersistError> {
            let cost = tracker.prepare_session(session_id).await?;
            *self
                .destination
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(session_id);
            self.entered.notify_one();
            self.release.notified().await;
            Ok(crate::conversation::PreparedSessionSwitch::new(
                cost,
                Some(self.transcript_lock.clone()),
            ))
        }
    }

    #[derive(Default)]
    struct RecordingSessionActivationObserver {
        activations:
            std::sync::Mutex<Vec<(lingxi_core::types::SessionId, lingxi_core::types::SessionId)>>,
        changed: tokio::sync::Notify,
    }

    #[async_trait::async_trait]
    impl crate::conversation::SessionActivationObserver for RecordingSessionActivationObserver {
        async fn session_activated(
            &self,
            previous: lingxi_core::types::SessionId,
            current: lingxi_core::types::SessionId,
        ) -> Result<(), String> {
            self.activations
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((previous, current));
            self.changed.notify_waiters();
            Ok(())
        }
    }

    #[tokio::test]
    async fn dropped_hot_clear_keeps_a_coherent_until_owned_b_commit_finishes() {
        let temp = tempfile::tempdir().unwrap();
        let session_a = lingxi_core::types::SessionId::new();
        let (persist_tx, _persist_rx) = tokio::sync::mpsc::channel(8);
        let tracker = Arc::new(cost::CostTracker::new(
            session_a,
            Arc::new(cost::PricingCatalog::empty()),
            persist_tx,
        ));
        let root_a = temp.path().join("authority-a");
        let root_b = temp.path().join("authority-b");
        std::fs::create_dir_all(&root_a).unwrap();
        std::fs::create_dir_all(&root_b).unwrap();
        let lock_a = Arc::new(session::jsonl::DurableTranscriptWriter::open(&root_a).unwrap());
        let lock_b = Arc::new(session::jsonl::DurableTranscriptWriter::open(&root_b).unwrap());
        let cwd = temp.path().join("workspace");
        std::fs::create_dir_all(&cwd).unwrap();
        let path_a = session::jsonl::path::session_path(
            temp.path(),
            &cwd.to_string_lossy(),
            &session_a.as_uuid().to_string(),
        );
        let fs: Arc<dyn lingxi_core::host::FileSystem> = Arc::new(
            platform_posix::fs::PosixFileSystem::new(temp.path().to_path_buf()),
        );
        let writer = Arc::new(
            session::jsonl::JsonlWriter::new(path_a.clone(), fs).with_durable_lock(lock_a),
        );
        writer
            .activate_session_target(session_a, path_a.clone(), cwd.clone())
            .unwrap();
        let preparer = Arc::new(BlockingSessionPreparer {
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
            destination: std::sync::Mutex::new(None),
            transcript_lock: lock_b,
        });
        let orchestrator = Arc::new(
            crate::ConversationOrchestrator::new(
                crate::OrchestratorConfig::default(),
                Arc::new(MockApiClient::new(Vec::new())),
                Arc::new(tool_api::registry::ToolRegistry::new()),
                noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                Arc::new(MockOutputStream::new()),
                Arc::new(StaticMemoryProvider::empty()),
                cwd.clone(),
            )
            .with_session_id(session_a)
            .with_jsonl_writer(writer.clone())
            .with_config_home(temp.path().to_path_buf())
            .with_cost_tracker(tracker.clone())
            .with_cost_session_switcher(preparer.clone()),
        );
        orchestrator.attach_owned_session_switches();

        let clearing = {
            let orchestrator = orchestrator.clone();
            tokio::spawn(async move {
                lingxi_core::host::OrchestratorHandle::clear_session(orchestrator.as_ref()).await
            })
        };
        preparer.entered.notified().await;
        let session_b = preparer
            .destination
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .expect("prepared destination");
        clearing.abort();
        let _ = clearing.await;

        assert_eq!(orchestrator.current_session_id().await, session_a);
        assert_eq!(tracker.session_id().await, session_a);
        assert_eq!(writer.session_target_path(session_a), Some(path_a));
        assert!(writer.session_target_path(session_b).is_none());

        preparer.release.notify_one();
        let errors = orchestrator.close_and_drain_session_switches().await;
        assert!(errors.is_empty(), "owned switch errors: {errors:?}");
        assert_eq!(orchestrator.current_session_id().await, session_b);
        assert_eq!(tracker.session_id().await, session_b);
        assert_eq!(
            writer.session_target_path(session_b),
            Some(session::jsonl::path::session_path(
                temp.path(),
                &cwd.to_string_lossy(),
                &session_b.as_uuid().to_string(),
            ))
        );
    }

    #[tokio::test]
    async fn dropped_prepared_resume_is_owned_through_host_activation_and_shutdown() {
        let temp = tempfile::tempdir().unwrap();
        let session_a = lingxi_core::types::SessionId::new();
        let session_b = lingxi_core::types::SessionId::new();
        let (persist_tx, _persist_rx) = tokio::sync::mpsc::channel(8);
        let tracker = Arc::new(cost::CostTracker::new(
            session_a,
            Arc::new(cost::PricingCatalog::empty()),
            persist_tx,
        ));
        let root_a = temp.path().join("resume-authority-a");
        let root_b = temp.path().join("resume-authority-b");
        std::fs::create_dir_all(&root_a).unwrap();
        std::fs::create_dir_all(&root_b).unwrap();
        let lock_a = Arc::new(session::jsonl::DurableTranscriptWriter::open(&root_a).unwrap());
        let lock_b = Arc::new(session::jsonl::DurableTranscriptWriter::open(&root_b).unwrap());
        let cwd = temp.path().join("resume-workspace");
        std::fs::create_dir_all(&cwd).unwrap();
        let path_a = session::jsonl::path::session_path(
            temp.path(),
            &cwd.to_string_lossy(),
            &session_a.as_uuid().to_string(),
        );
        let fs: Arc<dyn lingxi_core::host::FileSystem> = Arc::new(
            platform_posix::fs::PosixFileSystem::new(temp.path().to_path_buf()),
        );
        let writer = Arc::new(
            session::jsonl::JsonlWriter::new(path_a.clone(), fs).with_durable_lock(lock_a),
        );
        writer
            .activate_session_target(session_a, path_a.clone(), cwd.clone())
            .unwrap();
        let preparer = Arc::new(BlockingSessionPreparer {
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
            destination: std::sync::Mutex::new(None),
            transcript_lock: lock_b,
        });
        let observer = Arc::new(RecordingSessionActivationObserver::default());
        let orchestrator = Arc::new(
            crate::ConversationOrchestrator::new(
                crate::OrchestratorConfig::default(),
                Arc::new(MockApiClient::new(Vec::new())),
                Arc::new(tool_api::registry::ToolRegistry::new()),
                noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                Arc::new(MockOutputStream::new()),
                Arc::new(StaticMemoryProvider::empty()),
                cwd.clone(),
            )
            .with_session_id(session_a)
            .with_jsonl_writer(writer.clone())
            .with_config_home(temp.path().to_path_buf())
            .with_cost_tracker(tracker.clone())
            .with_cost_session_switcher(preparer.clone())
            .with_session_activation_observer(observer.clone()),
        );
        orchestrator.attach_owned_session_switches();

        let resuming = {
            let orchestrator = orchestrator.clone();
            tokio::spawn(async move {
                lingxi_core::host::OrchestratorHandle::resume_session(
                    orchestrator.as_ref(),
                    session_b,
                    Vec::new(),
                    None,
                    None,
                    lingxi_core::host::ResumeRuntimeSnapshot::default(),
                )
                .await
            })
        };
        preparer.entered.notified().await;
        assert_eq!(
            *preparer
                .destination
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            Some(session_b)
        );
        // The destination cost/transcript authorities are prepared. Hold the
        // conversation lock that reset/publication must acquire, then let the
        // owner cross out of preparation and cancel only its public waiter.
        let mounted_a = orchestrator.session.clone().lock_owned().await;
        preparer.release.notify_one();
        tokio::task::yield_now().await;
        resuming.abort();
        let _ = resuming.await;

        assert_eq!(mounted_a.session_id, session_a);
        assert_eq!(tracker.session_id().await, session_a);
        assert_eq!(writer.session_target_path(session_a), Some(path_a));
        assert!(writer.session_target_path(session_b).is_none());

        let draining = {
            let orchestrator = orchestrator.clone();
            tokio::spawn(async move { orchestrator.close_and_drain_session_switches().await })
        };
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), async {
                while !draining.is_finished() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .is_err(),
            "shutdown must wait for the accepted resume owner"
        );
        drop(mounted_a);
        let errors = tokio::time::timeout(std::time::Duration::from_secs(2), draining)
            .await
            .expect("owned resume drains")
            .expect("drain task joins");
        assert!(errors.is_empty(), "owned switch errors: {errors:?}");
        assert_eq!(orchestrator.current_session_id().await, session_b);
        assert_eq!(tracker.session_id().await, session_b);
        assert_eq!(
            writer.session_target_path(session_b),
            Some(session::jsonl::path::session_path(
                temp.path(),
                &cwd.to_string_lossy(),
                &session_b.as_uuid().to_string(),
            ))
        );
        assert_eq!(
            observer
                .activations
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_slice(),
            &[(session_a, session_b)]
        );
    }

    #[test]
    fn resolve_editor_returns_non_empty() {
        // Smoke test: helper must always return *some* non-empty editor.
        let s = resolve_editor();
        assert!(!s.is_empty(), "resolve_editor returned empty string");
    }

    #[cfg(unix)]
    #[test]
    fn default_editor_unix_is_vi() {
        assert_eq!(default_editor(), "vi");
    }

    #[cfg(windows)]
    #[test]
    fn default_editor_windows_is_notepad() {
        assert_eq!(default_editor(), "notepad.exe");
    }
}
