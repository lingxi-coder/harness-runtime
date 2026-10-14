//! Main-thread Or / yi / Br announcements use durable typed attachment rows.
//! The raw sidecar, in visible history order, is the announcement cursor.

use super::*;
use lingxi_core::host::instruction_announcements::{
    context_attachments, render_instruction_attachment,
};
use lingxi_core::host::instructions::InstructionRendering;

#[derive(Clone)]
pub(crate) struct PreparedContextAnnouncements {
    pub(crate) messages: Vec<ConversationMessage>,
    pub(crate) refresh_reason: &'static str,
    /// Or/pi's outgoing-only scalar envelope captured before compaction.
    pub(crate) inline_context: Option<ConversationMessage>,
}

impl Default for PreparedContextAnnouncements {
    fn default() -> Self {
        Self {
            messages: Vec::new(),
            refresh_reason: "session_start",
            inline_context: None,
        }
    }
}

impl PreparedContextAnnouncements {
    pub(crate) fn reattach(&self, history: &mut Vec<ConversationMessage>) {
        let ids = self
            .messages
            .iter()
            .map(ConversationMessage::id)
            .collect::<HashSet<_>>();
        history.retain(|message| !ids.contains(&message.id()));
        history.extend(self.messages.iter().cloned());
    }
}

pub(crate) fn context_attachment_projection(
    id: MessageId,
    attachment: &lingxi_core::types::utf16_json::Utf16JsonProjection,
) -> ConversationMessage {
    match render_instruction_attachment(attachment) {
        Some(body) => {
            let units = body
                .string_units("")
                .expect("rendered attachment is a string");
            ConversationMessage::user_meta_js_utf16(id, String::from_utf16_lossy(&units), units)
        }
        None => ConversationMessage::System {
            api_system: None,
            id,
            content: "".into(),
            subtype: Some("model_reminder_attachment".into()),
            compact_metadata: None,
            model_fallback: None,
            refusal_fallback: None,
        },
    }
}

impl ConversationOrchestrator {
    pub(crate) async fn current_context_rendering(&self) -> InstructionRendering {
        if self.config.context_rendering == InstructionRendering::Inline {
            return InstructionRendering::Inline;
        }
        if telemetry::feature_flags::flag_bool("tengu_foamy_spring", true) {
            if self.prompt_snapshot_resume() {
                return self
                    .prompt_runtime
                    .context_rendering_hint
                    .lock()
                    .await
                    .unwrap_or(InstructionRendering::Announced);
            }
            if let Some(rendering) = self
                .prompt_runtime
                .prompt_snapshot
                .lock()
                .await
                .as_ref()
                .and_then(|snapshot| snapshot.context_rendering)
            {
                return rendering;
            }
        }
        self.config.context_rendering
    }

    pub(crate) async fn uses_announced_context(&self) -> bool {
        self.current_context_rendering().await == InstructionRendering::Announced
    }

    #[cfg(test)]
    pub(crate) async fn context_announcement_messages(&self) -> Vec<ConversationMessage> {
        // This loads the host's eager descriptor snapshot and all existing
        // userContext producers once; the scalar projection is for inline only.
        self.additional_context_message()
            .await
            .expect("settings source must resolve before rendering context announcements");
        self.context_announcements_from_frozen_snapshot().await
    }

    /// PTL returns to the native outer query loop: Br runs again on the new
    /// history, while Or's eager file snapshot remains frozen for that query.
    #[cfg(test)]
    pub(crate) async fn context_announcements_from_frozen_snapshot(
        &self,
    ) -> Vec<ConversationMessage> {
        self.context_announcements_from_frozen_with_reason(
            self.frozen_instruction_refresh_reason().as_str(),
        )
        .await
    }

    pub(crate) async fn context_announcements_from_frozen_with_reason(
        &self,
        reason: &str,
    ) -> Vec<ConversationMessage> {
        let history = self.session.lock().await.model_context_history();
        let mut messages = Vec::new();
        for (projection, attachment) in self
            .context_announcement_rows_for_history(&history, reason)
            .await
        {
            messages.push(self.persist_model_reminder(projection, attachment).await);
        }
        messages
    }

    pub(crate) fn context_attachment_history(
        &self,
        history: &[ConversationMessage],
    ) -> Vec<lingxi_core::types::utf16_json::Utf16JsonProjection> {
        let attachments = self
            .transcript
            .model_reminder_attachments
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        history
            .iter()
            .filter_map(|message| attachments.get(&message.id()).cloned())
            .collect()
    }

    pub(crate) async fn context_announcement_rows_for_history(
        &self,
        history: &[ConversationMessage],
        reason: &str,
    ) -> Vec<(
        ConversationMessage,
        lingxi_core::types::utf16_json::Utf16JsonProjection,
    )> {
        if self.config.bare {
            return Vec::new();
        }
        let mut context = self.instruction_context_snapshot().await;
        context.announcement_history = self.context_attachment_history(history);
        if context.rendering == InstructionRendering::Inline {
            return Vec::new();
        }
        // Jn freezes the git snapshot already announced in this history chain.
        // A new chain uses the existing host git-status seam.
        let prior_context = context
            .announcement_history
            .iter()
            .rev()
            .find(|attachment| attachment.string_at("/type") == Some("session_context"))
            .filter(|attachment| {
                lingxi_core::host::instruction_announcements::is_valid_context_attachment(
                    attachment,
                )
            })
            .and_then(|attachment| attachment.at_pointer("/context"));
        let git = if self.config.exclude_dynamic_system_prompt_sections {
            None
        } else if let Some(prior_context) = prior_context {
            prior_context
                .get_property("gitStatus")
                .and_then(|value| value.code_units())
                .map(|units| {
                    lingxi_core::types::utf16_json::Utf16JsonProjection::root_string(
                        String::from_utf16_lossy(units),
                        units.to_vec(),
                    )
                    .expect("git status display matches source units")
                })
        } else {
            let probe_cwd = self.prompt_probe_cwd(&self.session_cwd.cwd());
            self.cached_git_status(&probe_cwd).await.1.map(|block| {
                let text = block
                    .strip_prefix("gitStatus: ")
                    .unwrap_or(&block)
                    .to_owned();
                lingxi_core::types::utf16_json::Utf16JsonProjection::from(text)
            })
        };
        context
            .remove_user_context_text("gitStatus")
            .expect("userContext remains an object");
        if let Some(git) = git {
            context
                .set_user_context_text("gitStatus", git)
                .expect("userContext remains an object");
        }
        let date = crate::prompt::env_meta::current_date_string();
        context
            .set_user_context_text("currentDate", format!("Today's date is {date}."))
            .expect("userContext remains an object");
        // Capture the exact request context for a subsequent explicit fork.
        if let Some(stored) = self
            .prompt_runtime
            .instruction_context
            .lock()
            .await
            .as_mut()
        {
            stored.user_context.clone_from(&context.user_context);
        }
        context_attachments(&context, &date, reason)
            .into_iter()
            .map(|attachment| {
                (
                    context_attachment_projection(MessageId::new(), &attachment),
                    attachment,
                )
            })
            .collect()
    }
}
