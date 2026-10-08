//! Per-turn `async_hook_response` reminder — the fold-back of completed
//! background (non-blocking / `async`) hooks.
//!
//! 1:1 with claude-code `getAsyncHookResponseAttachments`
//! (`utils/attachments.ts:3464`) + `normalizeAttachmentForAPI`'s
//! `'async_hook_response'` case (`utils/messages.ts:4026`): when an `async`
//! hook finishes in the background, its `system_message` (which already folds
//! in any `hookSpecificOutput.additionalContext`, per this engine's hook-output
//! parser) is re-injected as a meta user message wrapped in a
//! `<system-reminder>` on the NEXT turn — and delivered EXACTLY ONCE (the
//! source drains delivered responses, mirroring TS `removeDeliveredAsyncHooks`).
//!
//! Like the skill-listing / agent-listing / conditional-rules reminders, the
//! message is appended ONLY to the per-turn OUTGOING snapshot (never
//! `session.history` / JSONL), so it never accumulates. When no `async` hook
//! has completed since the last turn the reminder is `None` — byte-identical to
//! a build with no async hooks configured.

use async_trait::async_trait;
use hooks::attachment::HookPublicationGuard;
use hooks::ExactHookText;
use std::fmt;
use std::future::Future;
use std::sync::Arc;

/// One completed background hook's model-facing text and source event.
#[derive(Clone)]
pub struct AsyncHookResponse {
    pub text: ExactHookText,
    pub hook_event: Option<String>,
    /// Retain the source generation until the response is admitted to a model
    /// request. A producer-side channel send alone cannot cover later prompt
    /// preparation awaits.
    pub publication_guard: Option<Arc<dyn HookPublicationGuard>>,
}

impl fmt::Debug for AsyncHookResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AsyncHookResponse")
            .field("text", &self.text)
            .field("hook_event", &self.hook_event)
            .field("has_publication_guard", &self.publication_guard.is_some())
            .finish()
    }
}

impl PartialEq for AsyncHookResponse {
    fn eq(&self, other: &Self) -> bool {
        self.text == other.text && self.hook_event == other.hook_event
    }
}

impl Eq for AsyncHookResponse {}

/// A model-facing async-hook reminder remains associated with the generation
/// that produced it through the final prompt snapshot boundary.
#[derive(Clone)]
pub struct GuardedAsyncHookReminder {
    pub message: lingxi_core::types::ConversationMessage,
    pub publication_guard: Option<Arc<dyn HookPublicationGuard>>,
}

/// Bind accepted-prompt guards to only messages present in the final request.
/// The SDK checks this closure immediately before its logical dispatch marker.
pub fn request_dispatch_admission(
    messages: &[lingxi_core::types::ConversationMessage],
    guards: &[(lingxi_core::types::MessageId, Arc<dyn HookPublicationGuard>)],
) -> Option<llm_runtime::RequestDispatchAdmission> {
    let message_ids = messages
        .iter()
        .map(|message| message.id())
        .collect::<std::collections::HashSet<_>>();
    let guards = guards
        .iter()
        .filter(|(message_id, _)| message_ids.contains(message_id))
        .map(|(message_id, guard)| (*message_id, Arc::clone(guard)))
        .collect::<Vec<_>>();
    if guards.is_empty() {
        return None;
    }
    Some(llm_runtime::RequestDispatchAdmission::for_message_sources(
        guards.into_iter().map(|(message_id, guard)| {
            let check: Arc<dyn Fn() -> bool + Send + Sync> =
                Arc::new(move || guard.is_current());
            (message_id, check)
        }),
    ))
}

/// Run one awaited prompt-side operation while its originating executor
/// generation remains current, returning no value if reset cancels the work.
pub async fn run_hook_prompt_work<T: Send>(
    guard: Arc<dyn HookPublicationGuard>,
    future: impl Future<Output = T> + Send,
) -> Option<T> {
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    let accepted = guard
        .publish_if_current(Box::pin(async move {
            let _ = result_tx.send(future.await);
        }))
        .await;
    if accepted { result_rx.await.ok() } else { None }
}

/// Remove async-hook messages whose producer generation was reset or dropped.
/// Call after any prompt-preparation await and immediately before request
/// construction; the IDs identify only this ephemeral reminder family.
pub fn retain_current_async_hook_reminders(
    messages: &mut Vec<lingxi_core::types::ConversationMessage>,
    turn_reminders: &mut Vec<lingxi_core::types::ConversationMessage>,
    guards: &mut Vec<(lingxi_core::types::MessageId, Arc<dyn HookPublicationGuard>)>,
) {
    let stale: std::collections::HashSet<_> = guards
        .iter()
        .filter(|(_, guard)| !guard.is_current())
        .map(|(id, _)| *id)
        .collect();
    if stale.is_empty() {
        return;
    }
    messages.retain(|message| !stale.contains(&message.id()));
    turn_reminders.retain(|message| !stale.contains(&message.id()));
    guards.retain(|(id, guard)| !stale.contains(id) && guard.is_current());
}

/// Supplies the response texts of `async` (non-blocking) hooks that completed
/// in the background since the previous call.
///
/// CONSUME-ONCE: each call DRAINS the pending set — a completed hook's response
/// surfaces in exactly one turn's reminder (TS `checkForAsyncHookResponses` +
/// `removeDeliveredAsyncHooks`). The production impl (desktop) is backed by the
/// `AsyncHookRegistry` completion channel; tests inject a static fixture.
#[async_trait]
pub trait AsyncHookResponseProvider: Send + Sync {
    /// Drain + return the response text of each background hook completed since
    /// the previous call, in completion order. Each exact string is the hook's
    /// `system_message` (already including any folded `additionalContext`).
    /// Returns empty when nothing completed → no reminder this turn.
    async fn take_pending_responses(&self) -> Vec<ExactHookText>;

    /// Preserve the hook event per completion for `prompt.attachment` origin.
    /// Providers predating this metadata continue to supply their text without
    /// inventing an event name.
    async fn take_pending_with_events(&self) -> Vec<AsyncHookResponse> {
        self.take_pending_responses()
            .await
            .into_iter()
            .map(|text| AsyncHookResponse {
                text,
                hook_event: None,
                publication_guard: None,
            })
            .collect()
    }
}

/// Render the `async_hook_response` `<system-reminder>` body from the drained
/// responses, or `None` when there is nothing to surface.
///
/// All non-empty responses are joined into ONE system-reminder (TS
/// `wrapMessagesInSystemReminder` wraps the batch of per-response meta
/// messages). Empty / whitespace-only responses are skipped so a hook that
/// produced no `system_message` contributes nothing.
#[must_use]
pub fn render_reminder(responses: &[ExactHookText]) -> Option<ExactHookText> {
    let body: Vec<ExactHookText> = responses
        .iter()
        .map(ExactHookText::trim_js)
        .filter(|text| !text.is_empty())
        .collect();
    if body.is_empty() {
        return None;
    }
    Some(ExactHookText::wrapped(
        "<system-reminder>\n",
        &ExactHookText::join(&body, "\n"),
        "\n</system-reminder>",
    ))
}

/// Build a model-facing meta message while preserving isolated UTF-16 units.
#[must_use]
pub fn user_meta_message(
    id: lingxi_core::types::MessageId,
    text: ExactHookText,
) -> lingxi_core::types::ConversationMessage {
    text.to_conversation_message(id, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_generation_guard(
        generation: lingxi_core::host::CancellationToken,
    ) -> Arc<dyn HookPublicationGuard> {
        Arc::new(crate::autonomous_tool_scheduler::ToolDispatchPublicationFence::new(
            generation,
            Arc::new(tokio::sync::Mutex::new(())),
        ))
    }

    #[test]
    fn dispatch_admission_filters_guards_by_final_message_id() {
        let active_generation = lingxi_core::host::CancellationToken::new();
        let absent_generation = lingxi_core::host::CancellationToken::new();
        let active_id = lingxi_core::types::MessageId::new();
        let absent_id = lingxi_core::types::MessageId::new();
        let guards: Vec<_> = vec![
            (active_id, test_generation_guard(active_generation)),
            (absent_id, test_generation_guard(absent_generation.clone())),
        ];
        let message = lingxi_core::types::ConversationMessage::user_meta(
            active_id,
            "accepted hook reminder".into(),
        );
        let admission = request_dispatch_admission(&[message.clone()], &guards)
            .expect("the final request contains one guarded message");
        absent_generation.cancel();
        assert!(admission.is_admitted());

        let stale_generation = lingxi_core::host::CancellationToken::new();
        let stale_guard = test_generation_guard(stale_generation.clone());
        let stale_guards = vec![(active_id, stale_guard)];
        stale_generation.cancel();
        assert!(!request_dispatch_admission(&[message], &stale_guards)
            .expect("the guarded message is present")
            .is_admitted());
        assert!(request_dispatch_admission(&[], &guards).is_none());
    }

    #[test]
    fn empty_responses_yield_no_reminder() {
        assert_eq!(render_reminder(&[]), None);
        // Whitespace-only responses are dropped → still no reminder.
        assert_eq!(render_reminder(&["   ".into(), String::new().into()]), None);
    }

    #[test]
    fn responses_wrapped_in_single_system_reminder() {
        let out = render_reminder(&["ran lints: clean".into(), "synced".into()])
            .expect("reminder");
        assert_eq!(
            out.display,
            "<system-reminder>\nran lints: clean\nsynced\n</system-reminder>"
        );
    }

    #[test]
    fn blank_entries_are_skipped_but_others_kept() {
        let out = render_reminder(&[String::new().into(), "kept".into(), "  ".into()])
            .expect("reminder");
        assert_eq!(out.display, "<system-reminder>\nkept\n</system-reminder>");
    }

    #[test]
    fn reminder_join_preserves_isolated_surrogate_units() {
        let response = ExactHookText::from_utf16(vec![u16::from(b'a'), 0xD800]);
        let rendered = render_reminder(&[response]).expect("reminder");
        let mut expected = "<system-reminder>\n".encode_utf16().collect::<Vec<_>>();
        expected.extend([u16::from(b'a'), 0xD800]);
        expected.extend("\n</system-reminder>".encode_utf16());
        assert_eq!(rendered.utf16_code_units, expected);
        assert!(rendered
            .json_projection()
            .to_json_string()
            .unwrap()
            .contains(r#"\ud800\n</system-reminder>"#));
    }

    #[test]
    fn user_meta_message_uses_exact_provider_block_when_needed() {
        let text = ExactHookText::from_utf16(vec![u16::from(b'x'), 0xD800]);
        let message = user_meta_message(lingxi_core::types::MessageId::new(), text);
        let lingxi_core::types::ConversationMessage::User { content, .. } = message else {
            panic!("user meta message");
        };
        assert!(matches!(
            content.as_slice(),
            [lingxi_core::types::ContentBlock::TextJsUtf16 {
                utf16_code_units,
                ..
            }] if utf16_code_units == &[u16::from(b'x'), 0xD800]
        ));
    }
}
