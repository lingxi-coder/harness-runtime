//! Deferred media Conversation continuation for a desktop-owned turn.

use client::protocol::events::ClientEvent;
use lingxi_core::host::OrchestratorHandle;
use orchestrator::{
    MediaAgentCapabilityContext, MediaConversationAttachment, OrchestratorError, RealtimeAgentEnd,
};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use tokio_util::sync::CancellationToken;
use tool_api::{MediaAgentHandoffHandle, MediaAgentHandoffRequest, ToolUseContext};

/// Connection-scoped one-shot queue shared by the desktop tool context,
/// terminal-event gate, and turn driver.
///
/// The host creates this before its output adapter so it retains the ordinary
/// terminal while the driver continues realtime under the same active turn.
#[derive(Default)]
pub struct DesktopMediaAgentHandoffQueue {
    state: StdMutex<QueueState>,
    attachment: StdMutex<Option<Arc<dyn MediaConversationAttachment>>>,
    next_claim_generation: AtomicU64,
    scheduled_terminal_deferral_suppression: std::sync::atomic::AtomicUsize,
}

#[derive(Default)]
struct QueueState {
    pending: Option<crate::media_handoff::MediaHandoffInvocation>,
    in_progress: Option<(String, u64)>,
    deferred_terminal: Option<ClientEvent>,
}

impl DesktopMediaAgentHandoffQueue {
    /// Install the host attachment selected for this connection.
    pub fn set_attachment(&self, attachment: Option<Arc<dyn MediaConversationAttachment>>) {
        *self
            .attachment
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = attachment;
    }

    fn attachment(&self) -> Option<Arc<dyn MediaConversationAttachment>> {
        self.attachment
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Take the one pending request and claim it until continuation cleanup.
    pub(crate) fn take_pending(
        &self,
    ) -> Option<(crate::media_handoff::MediaHandoffInvocation, u64)> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.in_progress.is_some() {
            return None;
        }
        let pending = state.pending.take()?;
        let generation = self
            .next_claim_generation
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        state.in_progress = Some((pending.session_id.clone(), generation));
        Some((pending, generation))
    }

    /// Clear the active claim after the realtime runner and cleanup finish.
    pub(crate) fn finish_claim(&self, session_id: &str, generation: u64) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.in_progress.as_ref() == Some(&(session_id.to_owned(), generation)) {
            state.in_progress = None;
        }
    }

    /// Release the held terminal after the handoff runner has returned.
    pub fn take_deferred_terminal(&self) -> Option<ClientEvent> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.in_progress.is_some() {
            return None;
        }
        state.deferred_terminal.take()
    }

    /// Keep the scheduled turn's terminal from being captured by an unrelated
    /// stale interactive handoff in this connection-scoped queue.
    pub fn suppress_terminal_deferral_for_scheduled_turn(
        self: &Arc<Self>,
    ) -> ScheduledTerminalDeferralGuard {
        self.scheduled_terminal_deferral_suppression
            .fetch_add(1, Ordering::AcqRel);
        ScheduledTerminalDeferralGuard {
            queue: self.clone(),
        }
    }

    /// Consume and run the pending request after the ordinary turn's tool
    /// results have drained. This is the same coordinator used by
    /// `DesktopRuntime`, exposed so host-owned streamed-turn tests can exercise
    /// the production continuation without assembling a full product runtime.
    pub async fn continue_after_turn(
        self: &Arc<Self>,
        orchestrator: Arc<orchestrator::ConversationOrchestrator>,
        service: Option<Arc<dyn media_api::MediaService>>,
        cancel: CancellationToken,
        ordinary_turn_succeeded: bool,
    ) -> Result<Option<RealtimeAgentEnd>, OrchestratorError> {
        continue_desktop_handoff(
            orchestrator,
            self.clone(),
            cancel,
            service,
            self.attachment(),
            ordinary_turn_succeeded,
        )
        .await
    }
}

impl client::adapter::TurnTerminalDeferrer for DesktopMediaAgentHandoffQueue {
    fn defer_turn_ended(&self, event: ClientEvent) -> bool {
        if !matches!(event, ClientEvent::TurnEnded { .. }) {
            return false;
        }
        if self
            .scheduled_terminal_deferral_suppression
            .load(Ordering::Acquire)
            > 0
        {
            return false;
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.pending.is_none() && state.in_progress.is_none() {
            return false;
        }
        // Suppress duplicate terminal frames while the original is held.
        if state.deferred_terminal.is_none() {
            state.deferred_terminal = Some(event);
        }
        true
    }
}

/// Drops the scheduled-turn terminal suppression at scope exit, including
/// cancellation and errors.
pub struct ScheduledTerminalDeferralGuard {
    queue: Arc<DesktopMediaAgentHandoffQueue>,
}

impl Drop for ScheduledTerminalDeferralGuard {
    fn drop(&mut self) {
        self.queue
            .scheduled_terminal_deferral_suppression
            .fetch_sub(1, Ordering::AcqRel);
    }
}

impl MediaAgentHandoffHandle for DesktopMediaAgentHandoffQueue {
    fn supported_modes(&self) -> Vec<media_api::MediaSessionMode> {
        self.attachment()
            .map_or_else(Vec::new, |attachment| attachment.supported_modes())
    }

    fn supported_modes_for_context(
        &self,
        context: &MediaAgentCapabilityContext,
    ) -> Vec<media_api::MediaSessionMode> {
        self.attachment().map_or_else(Vec::new, |attachment| {
            attachment
                .supported_modes_for_session(context)
                .into_iter()
                .filter(|mode| attachment.supports_mode(context, *mode))
                .collect()
        })
    }

    fn supports_mode(
        &self,
        context: &MediaAgentCapabilityContext,
        mode: media_api::MediaSessionMode,
    ) -> bool {
        self.attachment()
            .is_some_and(|attachment| attachment.supports_mode(context, mode))
    }

    fn request(
        &self,
        invocation: &ToolUseContext,
        capability_context: &MediaAgentCapabilityContext,
        request: MediaAgentHandoffRequest,
    ) -> Result<(), String> {
        if invocation.is_scheduled_turn {
            return Err("media conversation handoff is unavailable during scheduled turns".into());
        }
        if invocation.agent_id.is_some()
            || invocation.origin_session_id.is_some()
            || invocation.depth != 0
        {
            return Err("media conversation handoff is available only to the main Agent".into());
        }
        if invocation.session.is_none() {
            return Err("media conversation handoff has no trusted session context".into());
        }
        let assistant_message_id = invocation
            .assistant_message_id
            .clone()
            .ok_or_else(|| "media handoff is not attached to an active Agent turn".to_owned())?;
        let tool_use_id = invocation
            .tool_use_id
            .clone()
            .ok_or_else(|| "media handoff is not attached to a tool call".to_owned())?;
        let assistant_message = crate::media_handoff::capture_assistant_message(
            invocation,
            &assistant_message_id,
            &tool_use_id,
        )?;
        if !self.supports_mode(capability_context, request.mode) {
            return Err(format!(
                "media conversation mode {:?} is not supported by this host attachment",
                request.mode
            ));
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.pending.is_some()
            || state.in_progress.is_some()
            || state.deferred_terminal.is_some()
        {
            return Err("another media conversation handoff is already pending".into());
        }
        state.pending = Some(crate::media_handoff::MediaHandoffInvocation {
            session_id: capability_context.session_id.clone(),
            capability_context: capability_context.clone(),
            assistant_message_id,
            assistant_message,
            tool_use_id,
            request,
        });
        Ok(())
    }
}

/// Resume the pending media session after ordinary streamed work drains.
pub(crate) async fn continue_desktop_handoff(
    orchestrator: Arc<orchestrator::ConversationOrchestrator>,
    queue: Arc<DesktopMediaAgentHandoffQueue>,
    cancel: CancellationToken,
    service: Option<Arc<dyn media_api::MediaService>>,
    attachment: Option<Arc<dyn MediaConversationAttachment>>,
    ordinary_turn_succeeded: bool,
) -> Result<Option<RealtimeAgentEnd>, OrchestratorError> {
    let pending = queue.take_pending();
    let Some((pending, generation)) = pending else {
        return Ok(None);
    };
    let claim = HandoffClaim {
        queue,
        session_id: pending.session_id.clone(),
        generation,
    };
    let session_id = orchestrator
        .current_session_id()
        .await
        .as_uuid()
        .to_string();
    let result = crate::media_handoff::continue_media_handoff(
        orchestrator,
        pending,
        &session_id,
        cancel,
        service,
        attachment,
        ordinary_turn_succeeded,
    )
    .await;
    drop(claim);
    result
}

struct HandoffClaim {
    queue: Arc<DesktopMediaAgentHandoffQueue>,
    session_id: String,
    generation: u64,
}

impl Drop for HandoffClaim {
    fn drop(&mut self) {
        self.queue.finish_claim(&self.session_id, self.generation);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use client::adapter::TurnTerminalDeferrer;
    use client::protocol::events::{CostDto, TurnOutcomeDto};
    use lingxi_core::types::{ConversationMessage, MessageId, ToolUseId};

    fn pending_handoff() -> crate::media_handoff::MediaHandoffInvocation {
        crate::media_handoff::MediaHandoffInvocation {
            session_id: "session-a".into(),
            capability_context: MediaAgentCapabilityContext {
                session_id: "session-a".into(),
                model: "test-model".into(),
                profile_name: Some("trusted-profile".into()),
                account_scope: Some("profile:trusted-profile".into()),
            },
            assistant_message_id: MessageId::new(),
            assistant_message: ConversationMessage::Assistant {
                id: MessageId::new(),
                content: Vec::new(),
                stop_reason: Some("tool_use".into()),
                per_turn_effort: None,
                resumable: None,
            },
            tool_use_id: ToolUseId::new(),
            request: MediaAgentHandoffRequest {
                operation_id: media_api::MediaOperationId::new(1, 1),
                session: media_api::MediaSessionRef(media_api::MediaSessionId::from(
                    "media-session-a",
                )),
                mode: media_api::MediaSessionMode::Audio,
            },
        }
    }

    fn terminal() -> ClientEvent {
        ClientEvent::TurnEnded {
            outcome: TurnOutcomeDto::EndTurn,
            stop_reason: Some("end_turn".into()),
            cost: CostDto {
                total_usd: 0.0,
                input_tokens: 0,
                output_tokens: 0,
                api_calls: 0,
                session_duration_secs: 0,
                formatted: "$0.00".into(),
            },
        }
    }

    #[test]
    fn scheduled_terminal_passes_through_without_consuming_interactive_pending_handoff() {
        let queue = Arc::new(DesktopMediaAgentHandoffQueue::default());
        queue.state.lock().unwrap().pending = Some(pending_handoff());
        let event = terminal();

        {
            let _guard = queue.suppress_terminal_deferral_for_scheduled_turn();
            assert!(!queue.defer_turn_ended(event.clone()));
        }
        assert!(queue.state.lock().unwrap().pending.is_some());
        assert!(queue.take_deferred_terminal().is_none());

        assert!(queue.defer_turn_ended(event));
        assert!(matches!(
            queue.take_deferred_terminal(),
            Some(ClientEvent::TurnEnded { .. })
        ));
    }

    #[test]
    fn scheduled_handoff_callback_is_rejected_even_without_media_tool_validation() {
        let queue = DesktopMediaAgentHandoffQueue::default();
        let mut invocation = tool_api::test_support::fresh_ctx();
        invocation.is_scheduled_turn = true;
        let context = MediaAgentCapabilityContext {
            session_id: "session-a".into(),
            model: "test-model".into(),
            profile_name: Some("trusted-profile".into()),
            account_scope: Some("profile:trusted-profile".into()),
        };
        let request = MediaAgentHandoffRequest {
            operation_id: media_api::MediaOperationId::new(1, 1),
            session: media_api::MediaSessionRef(media_api::MediaSessionId::from("media-session-a")),
            mode: media_api::MediaSessionMode::Audio,
        };
        assert!(queue
            .request(&invocation, &context, request)
            .expect_err("scheduled handoffs are denied at the callback boundary")
            .contains("scheduled turns"));
    }
}
