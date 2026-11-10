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
        // Only raw snapshots can produce a delivery request. Most physical
        // requests have none, so avoid copying either history in that case.
        // Keep the session guard through candidate collection so nonempty
        // deliveries still use one captured session/provenance snapshot.
        if deliveries.is_empty() {
            return HashMap::new();
        }
        let task_delivery_envelopes = session
            .native_request_history
            .task_delivery_envelopes
            .clone();
        let session_history = session.model_context_history();
        drop(session);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
        StaticMemoryProvider,
    };
    use std::{path::PathBuf, sync::Arc};

    fn orchestrator() -> ConversationOrchestrator {
        ConversationOrchestrator::new(
            crate::OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(Vec::new())),
            Arc::new(tool_api::registry::ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            PathBuf::from("/work"),
        )
    }

    fn assistant() -> ConversationMessage {
        ConversationMessage::Assistant {
            id: MessageId::new(),
            content: Vec::new(),
            stop_reason: None,
            resumable: None,
            per_turn_effort: None,
        }
    }

    #[tokio::test]
    async fn task_deliveries_empty_candidates_ignore_envelopes_and_unrelated_snapshots() {
        let orch = orchestrator();
        let message = ConversationMessage::user(MessageId::new(), "request".into());
        let id = message.id();
        {
            let mut session = orch.session.lock().await;
            session.history.push(ConversationMessage::user(
                id,
                "retained text".repeat(1 << 16),
            ));
            session.native_request_history.task_deliveries.insert(
                MessageId::new(),
                lingxi_core::types::utf16_json::Utf16JsonProjection::parse("null").unwrap(),
            );
            session
                .native_request_history
                .task_delivery_envelopes
                .insert(id, TaskDeliveryHistoryEnvelope::default());
        }
        orch.transcript
            .model_reminder_attachments
            .lock()
            .unwrap()
            .insert(
                id,
                lingxi_core::types::utf16_json::Utf16JsonProjection::parse(
                    r#"{"type":"queued_command","commandMode":"task-notification"}"#,
                )
                .unwrap(),
            );

        assert!(orch
            .task_deliveries_for_snapshot(&[message])
            .await
            .is_empty());
    }

    #[tokio::test]
    async fn task_deliveries_queued_snapshot_overlays_native_and_keeps_original_placement() {
        use lingxi_core::types::utf16_json::Utf16JsonProjection;

        let orch = orchestrator();
        let prior = assistant();
        let human = ConversationMessage::user(MessageId::new(), "human prompt".into())
            .with_origin(Utf16JsonProjection::parse(r#"{"kind":"human"}"#).unwrap());
        let carrier = ConversationMessage::user_meta(MessageId::new(), "queued result".into());
        let carrier_id = carrier.id();
        {
            let mut session = orch.session.lock().await;
            session
                .history
                .extend([prior.clone(), human.clone(), carrier.clone()]);
            session.native_request_history.task_deliveries.insert(
                carrier_id,
                Utf16JsonProjection::from("older native snapshot"),
            );
        }
        *orch
            .transcript
            .request_history_before_attachment_projection
            .lock()
            .unwrap() = vec![prior.clone(), human.clone(), carrier.clone()];
        orch.transcript
            .model_reminder_attachments
            .lock()
            .unwrap()
            .insert(
                carrier_id,
                Utf16JsonProjection::parse(
                    r#"{"type":"queued_command","commandMode":"task-notification","taskDelivery":{"exact":"\ud800"}}"#,
                )
                .unwrap(),
            );

        let messages = [carrier, prior, human];
        let requests = orch.task_deliveries_for_snapshot(&messages).await;
        let request = &requests[&carrier_id];
        assert_eq!(requests.len(), 1);
        assert_eq!(request.snapshot.string_units("/exact"), Some(vec![0xd800]));
        assert!(!request.placement.starts_turn);
        assert!(!request.placement.in_drain_position);
        assert!(!request.placement.behind_earlier_messages);
        assert!(request.placement.in_human_turn);
        orch.session
            .lock()
            .await
            .native_request_history
            .task_deliveries
            .remove(&carrier_id);
        assert_eq!(
            orch.task_deliveries_for_snapshot(&messages).await,
            requests,
            "an attachment-only snapshot must not take the empty-candidate path"
        );
    }

    #[tokio::test]
    async fn task_deliveries_missing_original_carrier_uses_captured_session_history() {
        use lingxi_core::types::utf16_json::Utf16JsonProjection;

        let orch = orchestrator();
        let prior = ConversationMessage::user(MessageId::new(), "earlier".into());
        let carrier = ConversationMessage::user(MessageId::new(), "delivery".into());
        let carrier_id = carrier.id();
        let later = assistant();
        {
            let mut session = orch.session.lock().await;
            session
                .history
                .extend([prior.clone(), carrier.clone(), later.clone()]);
            session
                .native_request_history
                .task_deliveries
                .insert(carrier_id, Utf16JsonProjection::from("native snapshot"));
        }
        *orch
            .transcript
            .request_history_before_attachment_projection
            .lock()
            .unwrap() = vec![prior.clone(), later.clone()];

        let requests = orch
            .task_deliveries_for_snapshot(&[later, carrier, prior])
            .await;
        let request = &requests[&carrier_id];
        assert_eq!(request.snapshot.string_at(""), Some("native snapshot"));
        assert!(request.placement.starts_turn);
        assert!(request.placement.behind_earlier_messages);
        assert!(!request.placement.in_human_turn);
    }
}
