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
    /// the previous call, in completion order. Each string is the hook's
    /// `system_message` (already including any folded `additionalContext`).
    /// Returns empty when nothing completed → no reminder this turn.
    async fn take_pending_responses(&self) -> Vec<String>;
}

/// Render the `async_hook_response` `<system-reminder>` body from the drained
/// responses, or `None` when there is nothing to surface.
///
/// All non-empty responses are joined into ONE system-reminder (TS
/// `wrapMessagesInSystemReminder` wraps the batch of per-response meta
/// messages). Empty / whitespace-only responses are skipped so a hook that
/// produced no `system_message` contributes nothing.
#[must_use]
pub fn render_reminder(responses: &[String]) -> Option<String> {
    let body: Vec<&str> = responses
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    if body.is_empty() {
        return None;
    }
    Some(format!(
        "<system-reminder>\n{}\n</system-reminder>",
        body.join("\n")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_responses_yield_no_reminder() {
        assert_eq!(render_reminder(&[]), None);
        // Whitespace-only responses are dropped → still no reminder.
        assert_eq!(render_reminder(&["   ".to_string(), String::new()]), None);
    }

    #[test]
    fn responses_wrapped_in_single_system_reminder() {
        let out = render_reminder(&["ran lints: clean".to_string(), "synced".to_string()])
            .expect("reminder");
        assert_eq!(
            out,
            "<system-reminder>\nran lints: clean\nsynced\n</system-reminder>"
        );
    }

    #[test]
    fn blank_entries_are_skipped_but_others_kept() {
        let out = render_reminder(&[String::new(), "kept".to_string(), "  ".to_string()])
            .expect("reminder");
        assert_eq!(out, "<system-reminder>\nkept\n</system-reminder>");
    }
}
