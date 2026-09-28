use crate::conversation::ConversationOrchestrator;
use hooks::events::HookEvent;
use hooks::registry::HookContext;
use protocol::{ConversationMessage, MessageId, ToolUseId};
use tool_api::context::ToolUseContext;
use tool_api::ContextModifier;

/// Fire the once-per-model-response `PostToolBatch` event after all tool
/// results have been appended and persisted.
///
/// The returned boolean is the hook's stop disposition. Callers deliberately
/// ignore it when a tool result already requested end-turn: the oracle still
/// runs and records the batch hook, but does not let it re-enter the model.
pub(crate) async fn run_post_tool_batch_hooks(
    orch: &ConversationOrchestrator,
    post_tool_batch_calls: Vec<hooks::events::PostToolBatchCall>,
) -> (bool, Vec<(ConversationMessage, ToolUseId)>) {
    run_post_tool_batch_hooks_inner(orch, post_tool_batch_calls, false).await
}

/// Forced-end twin of [`run_post_tool_batch_hooks`]. Claude still executes the
/// batch hooks, but it discards block/prevent dispositions, does not synthesize
/// `hook_stopped_continuation`, and does not surface `additionalContext`.
pub(crate) async fn run_post_tool_batch_hooks_after_turn_end(
    orch: &ConversationOrchestrator,
    post_tool_batch_calls: Vec<hooks::events::PostToolBatchCall>,
) -> Vec<(ConversationMessage, ToolUseId)> {
    run_post_tool_batch_hooks_inner(orch, post_tool_batch_calls, true)
        .await
        .1
}

pub(super) async fn run_post_tool_batch_hooks_inner(
    orch: &ConversationOrchestrator,
    post_tool_batch_calls: Vec<hooks::events::PostToolBatchCall>,
    turn_already_ended: bool,
) -> (bool, Vec<(ConversationMessage, ToolUseId)>) {
    if post_tool_batch_calls.is_empty() {
        return (false, Vec::new());
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
        ..Default::default()
    };
    let batch_agg = orch
        .hooks
        .execute(
            HookEvent::PostToolBatch {
                tool_calls: post_tool_batch_calls,
            },
            batch_ctx,
        )
        .await;
    let mut injected_messages = Vec::new();
    let identity = post_tool_batch_identity();
    let batch_id = protocol::ToolUseId::from(identity.tool_use_id.clone());

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
        return (false, Vec::new());
    }

    // `additionalContext` is yielded inside the per-hook loop; the stopped
    // record follows after the loop. Preserve that order when both occur.
    if !batch_agg.additional_contexts.is_empty() {
        orch.persist_hook_attachment_to_jsonl(hooks::additional_context_attachment(
            &identity.hook_name,
            &identity.tool_use_id,
            &identity.hook_event,
            &batch_agg.additional_contexts,
        ))
        .await;
        for ctx in &batch_agg.additional_contexts {
            injected_messages.push((
                ConversationMessage::user_meta(
                    MessageId::new(),
                    format!(
                        "<system-reminder>\nPostToolBatch hook additional context: {ctx}\n</system-reminder>"
                    ),
                ),
                batch_id.clone(),
            ));
        }
    }

    let stop_reason = post_tool_batch_stop_reason(&batch_agg);
    if let Some(reason) = &stop_reason {
        orch.persist_hook_attachment_to_jsonl(hooks::stopped_continuation_attachment(
            &identity, reason,
        ))
        .await;
        injected_messages.push((
            // Ephemeral rendering of the attachment above. The attachment is
            // the sole durable transcript record (`In(...)` in the oracle).
            ConversationMessage::user_meta(
                MessageId::new(),
                format!(
                    "<system-reminder>\nPostToolBatch hook stopped continuation: {reason}\n</system-reminder>"
                ),
            ),
            batch_id,
        ));
    }
    (stop_reason.is_some(), injected_messages)
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
        tool_use_id: format!("hook-{}", protocol::HookId::new().as_uuid()),
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
) -> Option<String> {
    let stopped = agg.prevent_continuation
        || matches!(agg.decision, Some(hooks::response::HookDecision::Block));
    if !stopped {
        return None;
    }
    Some(
        agg.reason
            .clone()
            .filter(|r| !r.is_empty())
            .unwrap_or_else(|| "Execution stopped by PostToolBatch hook".to_string()),
    )
}

/// Append tool/hook-injected messages to live history and persist only their
/// durable renderings. Hook `user_meta` messages are ephemeral views of the
/// attachment record that the hook firer already wrote, so serializing them a
/// second time would duplicate the transcript entry.
pub(crate) async fn append_tool_injected_messages(
    orch: &ConversationOrchestrator,
    messages: Vec<(ConversationMessage, ToolUseId)>,
) {
    if messages.is_empty() {
        return;
    }
    {
        let mut s = orch.session.lock().await;
        for (message, source_id) in &messages {
            s.history.push(message.clone());
            s.injected_message_sources
                .insert(message.id(), source_id.clone());
        }
    }
    for (message, _) in &messages {
        if !message.is_meta() {
            orch.persist_message_to_jsonl(message).await;
        }
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
    let (current, current_profile) = {
        let s = orch.session.lock().await;
        (s.model.clone(), s.model_profile.clone())
    };
    let resolved = modifiers
        .into_iter()
        .fold(ToolUseContext::model_seed(current.clone()), |ctx, m| m(ctx))
        .options
        .main_loop_model;
    if resolved != current {
        let listings = orch.api.list_model_listings();
        let (target_model, explicit_profile) = platform_api::parse_model_ref(&resolved, &listings);
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
            let mut s = orch.session.lock().await;
            s.model.clone_from(&target_model);
            s.model_profile.clone_from(&target_profile);
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
}
