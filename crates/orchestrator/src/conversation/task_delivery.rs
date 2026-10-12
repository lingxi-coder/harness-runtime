//! Request-scoped projection of durable Native task-delivery carriers.

use super::*;
use lingxi_core::host::{task_delivery::TaskDeliveryRequest, TaskDeliveryHistoryEnvelope};
use std::collections::HashMap;

impl ConversationOrchestrator {
    /// Apply only host-supplied Native row fields to one concrete user message.
    /// Hook `mod_origin` and generic `in_human_turn` state are deliberately not
    /// consulted here.
    pub(crate) async fn apply_native_user_message_source(
        &self,
        message: &mut ConversationMessage,
        source: &crate::native_user_message_source::NativeUserMessageSource,
    ) {
        let ConversationMessage::User { id, origin, .. } = message else {
            return;
        };
        if let Some(message_origin) = &source.message_origin {
            *origin = Some(message_origin.clone());
        }
        if let Some(prompt_source) = &source.prompt_source {
            let mut session = self.session.lock().await;
            let envelope = session
                .native_request_history
                .task_delivery_envelopes
                .entry(*id)
                .or_insert_with(TaskDeliveryHistoryEnvelope::default);
            envelope.prompt_source = Some(prompt_source.clone());
        }
    }

    /// Preserve only snapshots belonging to rows in this physical history.
    /// Native's producer gate does not disable replay of an existing carrier.
    /// Neither this projection nor provider consumption changes durable state.
    pub(crate) async fn task_deliveries_for_snapshot(
        &self,
        messages: &[ConversationMessage],
    ) -> HashMap<MessageId, TaskDeliveryRequest> {
        let ids: HashSet<_> = messages.iter().map(ConversationMessage::id).collect();
        let session = self.session.lock().await;
        let mut deliveries: HashMap<_, _> = session
            .native_request_history
            .task_deliveries
            .iter()
            .filter(|(id, _)| ids.contains(id))
            .map(|(id, snapshot)| (*id, snapshot.clone()))
            .collect();
        let task_delivery_envelopes = session
            .native_request_history
            .task_delivery_envelopes
            .clone();
        let session_history = session.model_context_history();
        drop(session);
        let attachments = self
            .transcript
            .model_reminder_attachments
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut task_delivery_attachments = HashMap::new();
        for (id, attachment) in attachments.iter() {
            if !ids.contains(id) || attachment.string_at("/type") != Some("queued_command") {
                continue;
            }
            task_delivery_attachments.insert(*id, attachment.clone());
            if let Ok(snapshot) = attachment.subprojection("/taskDelivery") {
                deliveries.insert(*id, snapshot);
            }
        }
        drop(attachments);
        let mut original_history = self
            .transcript
            .request_history_before_attachment_projection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let original_ids: HashSet<_> = original_history
            .iter()
            .map(ConversationMessage::id)
            .collect();
        if original_history.is_empty() || deliveries.keys().any(|id| !original_ids.contains(id)) {
            original_history = session_history;
        }
        original_history.retain(|message| ids.contains(&message.id()));
        let original_ids: HashSet<_> = original_history
            .iter()
            .map(ConversationMessage::id)
            .collect();
        original_history.extend(
            messages
                .iter()
                .filter(|message| !original_ids.contains(&message.id()))
                .cloned(),
        );
        llm_runtime::task_delivery::derive_task_delivery_requests(
            &original_history,
            &self.typed_attachment_ids_for_snapshot(messages),
            &deliveries,
            &task_delivery_envelopes,
            &task_delivery_attachments,
        )
    }
}
