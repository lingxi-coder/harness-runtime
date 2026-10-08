use crate::conversation::ConversationOrchestrator;
use hooks::attachment::HookPublicationGuard;
use hooks::events::HookEvent;
use hooks::registry::HookContext;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::{ConversationMessage, MessageId, ToolUseId};
use std::sync::Arc;
use tool_api::context::ToolUseContext;
use tool_api::ContextModifier;

/// Host-only carrier for a Native `PostToolBatch` event and the session/W1
/// generation that owns its publications. The guard never enters HookEvent or
/// transcript JSONL.
#[derive(Default)]
pub(crate) struct PostToolBatchDispatch {
    pub(crate) tool_calls: Vec<hooks::events::PostToolBatchCall>,
    pub(crate) publication_guard: Option<Arc<dyn HookPublicationGuard>>,
}

impl PostToolBatchDispatch {
    pub(crate) fn unguarded(tool_calls: Vec<hooks::events::PostToolBatchCall>) -> Self {
        Self {
            tool_calls,
            publication_guard: None,
        }
    }
}

pub(crate) struct PostToolBatchOutcome {
    pub(crate) prevent_continuation: bool,
    pub(crate) injected_messages: Vec<(ConversationMessage, ToolUseId)>,
    pub(crate) publication_guard: Option<Arc<dyn HookPublicationGuard>>,
}

/// Fire the once-per-model-response `PostToolBatch` event after all tool
/// results have been appended and persisted.
///
/// The returned boolean is the hook's stop disposition. Callers deliberately
/// ignore it when a tool result already requested end-turn: the oracle still
/// runs and records the batch hook, but does not let it re-enter the model.
pub(crate) async fn run_post_tool_batch_hooks(
    orch: &ConversationOrchestrator,
    dispatch: PostToolBatchDispatch,
) -> PostToolBatchOutcome {
    run_post_tool_batch_hooks_inner(orch, dispatch, false).await
}

/// Forced-end twin of [`run_post_tool_batch_hooks`]. Claude still executes the
/// batch hooks, but it discards block/prevent dispositions, does not synthesize
/// `hook_stopped_continuation`, and does not surface `additionalContext`.
pub(crate) async fn run_post_tool_batch_hooks_after_turn_end(
    orch: &ConversationOrchestrator,
    dispatch: PostToolBatchDispatch,
) -> PostToolBatchOutcome {
    run_post_tool_batch_hooks_inner(orch, dispatch, true).await
}

pub(super) async fn run_post_tool_batch_hooks_inner(
    orch: &ConversationOrchestrator,
    dispatch: PostToolBatchDispatch,
    turn_already_ended: bool,
) -> PostToolBatchOutcome {
    if dispatch.tool_calls.is_empty() {
        return PostToolBatchOutcome {
            prevent_continuation: false,
            injected_messages: Vec::new(),
            publication_guard: dispatch.publication_guard,
        };
    }
    // Direct tracked-dispatch/recovery/host paths have no W1 child executor.
    // Capture the current session generation at this host-only dispatch
    // boundary; streaming callers supply their exact originating W1 fence.
    let publication_guard = dispatch.publication_guard.unwrap_or_else(|| {
        let (root, lock) = orch
            .lifecycle_runtime
            .session_tool_hook_generation
            .current();
        Arc::new(crate::autonomous_tool_scheduler::ToolDispatchPublicationFence::new(root, lock))
            as Arc<dyn HookPublicationGuard>
    });
    if !publication_guard.is_current() {
        return PostToolBatchOutcome {
            prevent_continuation: false,
            injected_messages: Vec::new(),
            publication_guard: Some(publication_guard),
        };
    }
    // Populate `transcript_path` + `permission_mode` from the same live
    // sources as the per-tool hook contexts.
    let (session_id, plan_mode) = {
        let s = orch.session.lock().await;
        (s.session_id, s.plan_mode)
    };
    let transcript_path = orch
        .transcript
        .jsonl_writer
        .as_ref()
        .map(|w| w.path().to_path_buf())
        .unwrap_or_else(|| orch.computed_transcript_path(&session_id));
    let batch_ctx = HookContext {
        prompt_transcript: Some(orch.prompt_hook_transcript().await),
        session_id,
        cwd: orch.current_cwd(),
        transcript_path,
        permission_mode: Some(if plan_mode { "plan" } else { "default" }.to_string()),
        publication_guard: Some(Arc::clone(&publication_guard)),
        ..Default::default()
    };
    let batch_agg = orch
        .hooks
        .execute(
            HookEvent::PostToolBatch {
                tool_calls: dispatch.tool_calls,
            },
            batch_ctx,
        )
        .await;
    let mut injected_messages = Vec::new();
    let identity = post_tool_batch_identity();
    let batch_id = lingxi_core::types::ToolUseId::from(identity.tool_use_id.clone());

    if !publication_guard.is_current() {
        return PostToolBatchOutcome {
            prevent_continuation: false,
            injected_messages,
            publication_guard: Some(publication_guard),
        };
    }

    if turn_already_ended {
        // `eBn` yields only `fe.message` from the hook runner, then logs
        // `blockingError` / `preventContinuation`. A blocking result is a bare
        // disposition (not a runner message), while additionalContext rides
        // `fe.additionalContexts`; neither is synthesized into history after
        // the tool result has already ended the turn. Run-outcome/system
        // messages are persisted by the hook executor's attachment sink.
        if matches!(
            batch_agg.decision,
            Some(hooks::response::HookDecision::Block)
        ) || batch_agg.prevent_continuation
        {
            tracing::debug!(
                event = "post_tool_batch_disposition_discarded",
                "PostToolBatch disposition discarded because a tool result ended the turn"
            );
        }
        return PostToolBatchOutcome {
            prevent_continuation: false,
            injected_messages: Vec::new(),
            publication_guard: Some(publication_guard),
        };
    }

    // `additionalContext` is yielded inside the per-hook loop; the stopped
    // record follows after the loop. Preserve that order when both occur.
    if !batch_agg.additional_contexts.is_empty() {
        let attachment = hooks::additional_context_attachment(
            &identity.hook_name,
            &identity.tool_use_id,
            &identity.hook_event,
            batch_agg.additional_contexts.as_slice(),
        );
        let body = hooks::ExactHookText::join(&batch_agg.additional_contexts, "\n");
        let reminder = hooks::ExactHookText::wrapped(
            "<system-reminder>\nPostToolBatch hook additional context: ",
            &body,
            "\n</system-reminder>",
        );
        let message = ConversationMessage::user_meta_js_utf16(
            MessageId::new(),
            reminder.display,
            reminder.utf16_code_units,
        );
        let committed = publication_guard
            .commit_if_current(Box::pin(async {
                orch.persist_hook_attachment_to_jsonl(attachment).await;
                orch.register_mod_persisted_attachment(
                    &message,
                    "hook_additional_context",
                    serde_json::json!({"kind":"hook","event":"PostToolBatch"}),
                )
                .await;
                orch.prompt_runtime
                    .remember_guarded_prompt_message(message.id(), Arc::clone(&publication_guard))
                    .await;
            }))
            .await;
        if committed {
            injected_messages.push((message, batch_id.clone()));
        }
    }

    let stop_reason = post_tool_batch_stop_reason(&batch_agg);
    if let Some(reason) = &stop_reason {
        let attachment = hooks::stopped_continuation_attachment(&identity, reason);
        // Ephemeral rendering of the attachment above. The attachment is the
        // sole durable transcript record (`In(...)` in the oracle).
        let mut message_units = "<system-reminder>\nPostToolBatch hook stopped continuation: "
            .encode_utf16()
            .collect::<Vec<_>>();
        message_units.extend(
            reason
                .string_units("")
                .expect("stop reason is a root string projection"),
        );
        message_units.extend("\n</system-reminder>".encode_utf16());
        let message = ConversationMessage::user_meta_js_utf16(
            MessageId::new(),
            String::from_utf16_lossy(&message_units),
            message_units,
        );
        let committed = publication_guard
            .commit_if_current(Box::pin(async {
                orch.persist_hook_attachment_to_jsonl(attachment).await;
                orch.register_mod_persisted_attachment(
                    &message,
                    "hook_stopped_continuation",
                    serde_json::json!({"kind":"hook","event":"PostToolBatch"}),
                )
                .await;
                orch.prompt_runtime
                    .remember_guarded_prompt_message(message.id(), Arc::clone(&publication_guard))
                    .await;
            }))
            .await;
        if committed {
            injected_messages.push((message, batch_id));
        }
    }
    PostToolBatchOutcome {
        prevent_continuation: stop_reason.is_some() && publication_guard.is_current(),
        injected_messages,
        publication_guard: Some(publication_guard),
    }
}

/// Identity for the once-per-batch `PostToolBatch` records.
///
/// `hookName`/`hookEvent` are the bare literal `PostToolBatch` — NOT suffixed
/// with a tool name the way `PostToolUse:${t.name}` is (@234726414), because the
/// event covers the whole batch rather than one call.
///
/// `toolUseID` is the oracle's `rt`, bound at the top of the batch block as
/// ``rt = `hook-${f.uuid()}` `` (@233159400) — a SYNTHETIC id, not any real
/// tool's. The `Stop`-hook site builds its id the same way
/// (`conversation.rs`), so the two stay consistent.
pub(super) fn post_tool_batch_identity() -> hooks::HookAttachmentIdentity {
    hooks::HookAttachmentIdentity {
        hook_name: "PostToolBatch".to_string(),
        hook_event: "PostToolBatch".to_string(),
        tool_use_id: format!("hook-{}", lingxi_core::types::HookId::new().as_uuid()),
    }
}

/// The stop reason a `PostToolBatch` aggregate implies, or `None` to continue.
///
/// Ports the oracle's `Mr`/`Qn` pair (@233161375):
///
/// ```js
/// if(Mn.blockingError)Mr=!0,Qn??=Mn.blockingError.blockingError;
/// if(Mn.preventContinuation)Mr=!0,Qn??=Mn.stopReason
/// …
/// if(Mr)… message:Qn||"Execution stopped by PostToolBatch hook" …
/// ```
///
/// Two details worth keeping:
///
/// - `Mr` is set by EITHER a blocking error or `preventContinuation`. Gating on
///   `prevent_continuation` alone would let a blocking batch hook run on.
/// - `Qn` is `??=` (first-wins) and falls back to the literal below when empty.
///   The aggregate's `reason` already freezes at the first blocker
///   (`executor.rs`), so reading it here preserves that ordering.
pub(super) fn post_tool_batch_stop_reason(
    agg: &hooks::response::AggregateHookResult,
) -> Option<Utf16JsonProjection> {
    let stopped = agg.prevent_continuation
        || matches!(agg.decision, Some(hooks::response::HookDecision::Block));
    if !stopped {
        return None;
    }
    let reason = agg
        .reason
        .clone()
        .filter(|reason| !reason.is_empty())
        .unwrap_or_else(|| {
            hooks::ExactHookText::from_text("Execution stopped by PostToolBatch hook")
        });
    Some(reason.into_projection())
}

/// Append tool/hook-injected messages to live history and persist only their
/// durable renderings. Hook `user_meta` messages are ephemeral views of the
/// attachment record that the hook firer already wrote, so serializing them a
/// second time would duplicate the transcript entry.
pub(crate) async fn append_tool_injected_messages(
    orch: &ConversationOrchestrator,
    messages: Vec<(ConversationMessage, ToolUseId)>,
    publication_guard: Option<Arc<dyn HookPublicationGuard>>,
) {
    if messages.is_empty() {
        return;
    }
    for (message, source_id) in messages {
        let guard = match publication_guard.as_ref() {
            Some(guard) => Some(Arc::clone(guard)),
            None => {
                orch.prompt_runtime
                    .guarded_prompt_message_guard(message.id())
                    .await
            }
        };
        if let Some(guard) = guard {
            orch.append_guarded_injected_message(&message, source_id, guard)
                .await;
            continue;
        }
        let append = async {
            {
                let mut session = orch.session.lock().await;
                session.history.push(message.clone());
                session
                    .injected_message_sources
                    .insert(message.id(), source_id);
            }
            if !message.is_meta() {
                orch.persist_message_to_jsonl(&message).await;
            }
        };
        append.await;
    }
}

/// SKILLEXEC.3 (model scope): fold a tool batch's `context_modifier`s over a
/// seed context carrying the live `session.model`, then persist the resolved
/// model back to `session.model` when it changed (TS `contextModifier` sets
/// `options.mainLoopModel` for the rest of the session).
///
/// Called POST-BATCH by BOTH drivers (the batched [`execute_one_turn`] and the
/// streaming `try_run_turn_streaming`) at the same point they append injected
/// `new_messages`. Applying after the whole batch — rather than per tool —
/// gives the concurrent streaming dispatch a SINGLE application point, so there
/// is no race on `session.model` between concurrently-dispatched tools.
///
/// Empty `modifiers` (every existing tool + skills WITHOUT a `model:`
/// frontmatter) → an early return that never touches the session lock →
/// `session.model` is unchanged → byte-identical. The model override then
/// persists: subsequent turns read the new `session.model` (TS sets
/// `options.mainLoopModel` for the rest of the session).
pub(crate) async fn apply_model_context_modifiers(
    orch: &ConversationOrchestrator,
    modifiers: Vec<ContextModifier>,
) {
    if modifiers.is_empty() {
        return;
    }
    let current = orch.session.lock().await.model.clone();
    let resolved = modifiers
        .into_iter()
        .fold(ToolUseContext::model_seed(current), |ctx, modifier| {
            modifier(ctx)
        })
        .options
        .main_loop_model;
    apply_resolved_model_context(orch, resolved).await;
}

/// Apply the model layer already folded by the owned streaming scheduler. The
/// one-shot callbacks have been consumed at Native's unsafe/ended-run barrier;
/// this final projection preserves the same profile selection and post-switch
/// hooks without replaying a callback.
pub(crate) async fn apply_model_context_state(
    orch: &ConversationOrchestrator,
    state: lingxi_core::host::tool_invoker::ToolInvocationContextState,
) -> Result<(), crate::error::OrchestratorError> {
    let context = state.downcast_arc::<ToolUseContext>().map_err(|error| {
        crate::error::OrchestratorError::StreamingProtocol(format!(
            "streaming tool context state is invalid: {error}"
        ))
    })?;
    apply_resolved_model_context(orch, context.options.main_loop_model.clone()).await;
    Ok(())
}

async fn apply_resolved_model_context(orch: &ConversationOrchestrator, resolved: String) {
    let (current, current_profile) = {
        let session = orch.session.lock().await;
        (session.model.clone(), session.model_profile.clone())
    };
    if resolved == current {
        return;
    }
    let listings = orch.api.list_model_listings();
    let (target_model, explicit_profile) = lingxi_core::host::parse_model_ref(&resolved, &listings);
    let target_profile = explicit_profile.or_else(|| {
        current_profile
            .as_ref()
            .filter(|profile| {
                listings.iter().any(|listing| {
                    listing.provider_id.as_str() == profile.as_str()
                        && listing.request_model == target_model
                })
            })
            .cloned()
            .or_else(|| {
                let mut matches = listings
                    .iter()
                    .filter(|listing| listing.request_model == target_model);
                let first = matches.next()?;
                matches.next().is_none().then(|| first.provider_id.clone())
            })
    });
    {
        let mut session = orch.session.lock().await;
        session.model.clone_from(&target_model);
        session.model_profile.clone_from(&target_profile);
    }
    orch.run_post_model_switch_hooks(
        &current,
        &target_model,
        None,
        target_profile.as_deref(),
        "auto",
    )
    .await;
}
