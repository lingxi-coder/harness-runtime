//! Shared continuation mechanics for host-owned media Conversation sessions.

use lingxi_core::types::{ContentBlock, ConversationMessage, MessageId, ToolUseId};
use orchestrator::{
    ConversationOrchestrator, MediaAgentCapabilityContext, MediaConversationAttachment,
    OrchestratorError, RealtimeAgentEnd, RealtimeAgentSessionIo,
};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tool_api::MediaAgentHandoffRequest;

/// Trusted invocation identity retained until the streamed tool result commits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MediaHandoffInvocation {
    pub session_id: String,
    pub capability_context: MediaAgentCapabilityContext,
    pub assistant_message_id: MessageId,
    pub assistant_message: ConversationMessage,
    pub tool_use_id: ToolUseId,
    pub request: MediaAgentHandoffRequest,
}

/// Snapshot the exact assistant tool-use row that admitted a handoff. Streaming
/// dispatches retain a per-block row ID here, while the committed Agent history
/// later contains the merged assistant message under a different ID.
pub(crate) fn capture_assistant_message(
    invocation: &tool_api::ToolUseContext,
    assistant_message_id: &MessageId,
    tool_use_id: &ToolUseId,
) -> Result<ConversationMessage, String> {
    let assistant_message = invocation
        .assistant_message
        .as_ref()
        .ok_or_else(|| "media handoff has no trusted assistant tool-use row".to_owned())?;
    if assistant_message.id() != *assistant_message_id {
        return Err(
            "media handoff assistant row identity does not match its tool invocation".into(),
        );
    }
    let contains_requested_tool = matches!(
        assistant_message,
        ConversationMessage::Assistant { content, .. }
            if content.iter().any(|block| matches!(
                block,
                ContentBlock::ToolUse { id, name, .. }
                    if id == tool_use_id && name == "media_session_start"
            ))
    );
    if !contains_requested_tool {
        return Err("media handoff is not attached to its media_session_start tool row".into());
    }
    Ok(assistant_message.clone())
}

static NEXT_CLEANUP_GENERATION: AtomicU64 = AtomicU64::new(1);
pub(crate) const CLEANUP_BUDGET: std::time::Duration = std::time::Duration::from_secs(3);

pub(crate) fn media_operation_context(
    service: &dyn media_api::MediaService,
    owner: media_api::MediaOwner,
) -> media_api::MediaOperationContext {
    let capabilities = service.capabilities_for(&owner);
    media_api::MediaOperationContext::new(
        media_api::MediaOperationId::new(
            NEXT_CLEANUP_GENERATION
                .fetch_add(1, Ordering::Relaxed)
                .max(1),
            capabilities.service_epoch,
        ),
        owner,
    )
    .with_timeout_budget_ms(u64::try_from(CLEANUP_BUDGET.as_millis()).unwrap_or(u64::MAX))
    .with_max_payload_bytes(capabilities.max_payload_bytes)
}

pub(crate) async fn stop_media_session(
    service: &Arc<dyn media_api::MediaService>,
    session: &media_api::MediaSessionRef,
    owner: media_api::MediaOwner,
) {
    if !service
        .capabilities_for(&owner)
        .supports(media_api::MediaOperationKind::Stop)
    {
        return;
    }
    let context = media_operation_context(service.as_ref(), owner);
    let _ = tokio::time::timeout(
        CLEANUP_BUDGET,
        service.execute(
            context,
            media_api::MediaOperation::Stop {
                target: media_api::MediaResourceRef::Session(session.clone()),
            },
        ),
    )
    .await;
}

/// Stop every active, owner-visible resource for one retired conversation
/// session. The deadline bounds the entire enumeration and stop sequence, so a
/// session with many resources cannot hold retarget or disposal indefinitely.
#[cfg(feature = "mobile")]
pub(crate) async fn stop_media_owner_resources(
    service: &Arc<dyn media_api::MediaService>,
    owner: media_api::MediaOwner,
) -> usize {
    let capabilities = service.capabilities_for(&owner);
    if !capabilities.supports(media_api::MediaOperationKind::Status)
        || !capabilities.supports(media_api::MediaOperationKind::Stop)
    {
        return 0;
    }

    let deadline = tokio::time::Instant::now() + CLEANUP_BUDGET;
    let status_context = media_operation_context(service.as_ref(), owner.clone());
    let resources = match tokio::time::timeout_at(
        deadline,
        service.execute(
            status_context,
            media_api::MediaOperation::Status { target: None },
        ),
    )
    .await
    {
        Ok(Ok(media_api::MediaResult::Status { resources })) => resources,
        _ => return 0,
    };

    let mut stopped = 0;
    for resource in resources.into_iter().filter(|resource| {
        matches!(
            resource.state,
            media_api::MediaResourceState::Active | media_api::MediaResourceState::Stopping
        ) && matches!(
            &resource.resource,
            media_api::MediaResourceRef::Operation { .. }
                | media_api::MediaResourceRef::Recording(_)
                | media_api::MediaResourceRef::Session(_)
        )
    }) {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let context = media_operation_context(service.as_ref(), owner.clone())
            .with_timeout_budget_ms(u64::try_from(remaining.as_millis()).unwrap_or(u64::MAX));
        if matches!(
            tokio::time::timeout_at(
                deadline,
                service.execute(
                    context,
                    media_api::MediaOperation::Stop {
                        target: resource.resource,
                    },
                ),
            )
            .await,
            Ok(Ok(_))
        ) {
            stopped += 1;
        }
    }
    stopped
}

pub(crate) async fn open_attachment(
    attachment: &dyn MediaConversationAttachment,
    request: MediaAgentHandoffRequest,
    prepared: orchestrator::RealtimeAgentContext,
    audio: Arc<dyn media_api::MediaAudioChannel>,
) -> Result<RealtimeAgentSessionIo, String> {
    attachment.open(request, prepared, audio).await
}

/// Run a pending attachment after the caller has drained ordinary streamed
/// work. This deliberately does not reserve or announce another turn.
pub(crate) async fn continue_media_handoff(
    orchestrator: Arc<ConversationOrchestrator>,
    pending: MediaHandoffInvocation,
    current_session_id: &str,
    cancel: CancellationToken,
    service: Option<Arc<dyn media_api::MediaService>>,
    attachment: Option<Arc<dyn MediaConversationAttachment>>,
    ordinary_turn_succeeded: bool,
) -> Result<Option<RealtimeAgentEnd>, OrchestratorError> {
    let owner = media_api::MediaOwner::Session {
        session_id: pending.session_id.clone(),
    };
    let Some(service) = service else {
        return Err(OrchestratorError::Internal(
            "media conversation handoff lost its app media service".into(),
        ));
    };
    if service.attachment_id() != pending.capability_context.attachment_id {
        return Err(OrchestratorError::Internal(
            "media conversation handoff belongs to a different engine attachment".into(),
        ));
    }
    let cleanup = || stop_media_session(&service, &pending.request.session, owner.clone());
    if !ordinary_turn_succeeded {
        cleanup().await;
        return Ok(None);
    }
    if pending.session_id != current_session_id {
        cleanup().await;
        return Err(OrchestratorError::Internal(
            "media conversation handoff belongs to a different active session".into(),
        ));
    }
    if pending.request.mode == media_api::MediaSessionMode::Video {
        cleanup().await;
        return Err(OrchestratorError::Internal(
            "video-only media conversations require a video Agent input lane that this attachment does not provide".into(),
        ));
    }
    if cancel.is_cancelled() {
        cleanup().await;
        return Ok(Some(RealtimeAgentEnd::Cancelled));
    }
    let capabilities = service.capabilities_for(&owner);
    if pending.request.operation_id.service_epoch != capabilities.service_epoch {
        cleanup().await;
        return Err(OrchestratorError::Internal(
            "media conversation handoff belongs to a stale service epoch".into(),
        ));
    }
    let result_committed = orchestrator
        .media_handoff_result_committed(
            &pending.session_id,
            &pending.assistant_message_id,
            &pending.assistant_message,
            &pending.tool_use_id,
            &pending.request.session.0 .0,
        )
        .await;
    if !result_committed {
        cleanup().await;
        return Err(OrchestratorError::Internal(
            "media conversation handoff has no committed successful tool result".into(),
        ));
    }
    let Some(attachment) = attachment else {
        cleanup().await;
        return Err(OrchestratorError::Internal(
            "this host has no native Agent realtime attachment".into(),
        ));
    };
    if !attachment.supports_mode(&pending.capability_context, pending.request.mode) {
        cleanup().await;
        return Err(OrchestratorError::Internal(
            "the current Agent provider no longer supports this media conversation mode".into(),
        ));
    }

    let session_audio = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            cleanup().await;
            return Ok(Some(RealtimeAgentEnd::Cancelled));
        }
        result = tokio::time::timeout(
            CLEANUP_BUDGET,
            service.session_audio(
                media_operation_context(service.as_ref(), owner.clone()),
                pending.request.session.clone(),
            ),
        ) => match result {
            Ok(Ok(audio)) => audio,
            Ok(Err(error)) => {
                cleanup().await;
                return Err(OrchestratorError::Internal(format!(
                    "media conversation has no owner-bound audio channel: {error}"
                )));
            }
            Err(_) => {
                cleanup().await;
                return Err(OrchestratorError::Internal(
                    "media conversation audio channel timed out".into(),
                ));
            }
        }
    };
    let prepared = match orchestrator.prepare_realtime_agent().await {
        Ok(mut prepared)
            if prepared.session_id == pending.session_id
                && prepared.profile_name == pending.capability_context.profile_name
                && prepared.account_scope == pending.capability_context.account_scope
                && prepared.attachment_id.as_ref().is_none_or(|id| {
                    Some(id) == pending.capability_context.attachment_id.as_ref()
                }) =>
        {
            prepared.attachment_id = pending.capability_context.attachment_id.clone();
            prepared
        }
        Ok(_) => {
            cleanup().await;
            return Err(OrchestratorError::Internal(
                "the active Agent profile changed before media handoff".into(),
            ));
        }
        Err(error) => {
            cleanup().await;
            return Err(error);
        }
    };
    let io = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            cleanup().await;
            return Ok(Some(RealtimeAgentEnd::Cancelled));
        }
        opened = open_attachment(
            attachment.as_ref(),
            pending.request.clone(),
            prepared.clone(),
            session_audio,
        ) => match opened {
            Ok(io) => io,
            Err(error) => {
                cleanup().await;
                return Err(OrchestratorError::Internal(format!(
                    "could not open native Agent realtime: {error}"
                )));
            }
        }
    };
    if cancel.is_cancelled() {
        let _ = io
            .control
            .abort(llm_runtime::services::sdk::realtime::RealtimeClose {
                code: 1000,
                reason: "media Agent handoff cancelled".into(),
            })
            .await;
        cleanup().await;
        return Ok(Some(RealtimeAgentEnd::Cancelled));
    }
    if service.capabilities_for(&owner).service_epoch != pending.request.operation_id.service_epoch
    {
        let _ = io
            .control
            .abort(llm_runtime::services::sdk::realtime::RealtimeClose {
                code: 1000,
                reason: "media service epoch changed".into(),
            })
            .await;
        cleanup().await;
        return Err(OrchestratorError::Internal(
            "media service epoch changed before Agent realtime started".into(),
        ));
    }
    let result = orchestrator
        .run_realtime_agent(
            prepared,
            io.control,
            io.events,
            io.inputs,
            io.output,
            io.limits,
            io.image_input_supported,
            cancel,
        )
        .await;
    cleanup().await;
    result.map(Some)
}
