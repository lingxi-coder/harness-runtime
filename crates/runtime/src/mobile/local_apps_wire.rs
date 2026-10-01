//! The edge between the Local App service's vocabulary and the client
//! protocol.
//!
//! The service speaks the types in `local_app_contracts`; the native client
//! speaks the `UniFFI` DTOs in `client::protocol::local_apps`, whose enum
//! ordinals are locked and append-only. Neither may import the other, so every
//! crossing goes through a function here. A mapping that drifts would show up
//! first in the Swift or Kotlin client, so each pair has a test that sends the
//! same values through both and compares the JSON, which is what the page and
//! the golden snapshots see.

use async_trait::async_trait;
use client::adapter::ClientEventSink;
use client::protocol::events::ClientEvent;
use client::protocol::local_apps::{
    AppAgentProfileProposalDto, AppAuthorizationDecisionDto, AppBridgeOperationDto,
    AppBridgeRequestDto, AppBridgeResponseDto, AppBridgeStreamFrameDto, AppCapabilityKindDto,
    AppCapabilityRequestDto, AppDependencyChangeConfirmationRequestDto, AppDependencyChangeDto,
    AppDependencyChangeKindDto, AppEventDto, AppUiActionKindDto, AppUiRequestDto, AppUiTargetDto,
    AppWorkflowStateDto, LocalAppGateStatusDto, LocalAppMcpProposalApprovalRequestDto,
    LocalAppMcpToolChangeKindDto, LocalAppMcpToolDiffDto, LocalAppMcpToolFieldDto,
    LocalAppMcpToolSurfaceDto, LocalAppPluginErrorCodeDto, LocalAppVerificationStatusDto,
    LocalAppVerificationSummaryDto, ManagedLocalAppMcpServerDto, ManagedLocalAppMcpStatusDto,
    McpAppWidgetDto,
};
use local_app_contracts::approvals::{
    AgentProfileProposal, AuthorizationDecision, CapabilityKind, CapabilityRequest,
    DependencyChangeConfirmationRequest, DependencyChangeKind, DependencyChangeReview, GateStatus,
    McpProposalApprovalRequest, McpToolChangeKind, McpToolDiff, McpToolField, McpToolSurface,
    UiActionKind, UiRequest, UiTarget, VerificationStatus, VerificationSummary,
};
use local_app_contracts::bridge::{
    BridgeOperation, BridgeRequest, BridgeResponse, BridgeStreamFrame,
};
use local_app_contracts::events::{
    ManagedMcpServer, ManagedMcpStatus, McpAppWidget, PluginErrorCode, PublicationState,
};
use local_app_service::host::{HostEvent, HostEventSink};
use std::sync::Arc;

/// The operation the service knows by this DTO's name, or `None` for one this
/// build of the service does not implement (the client protocol is
/// `non_exhaustive` and may be newer).
pub(crate) fn bridge_operation_from_dto(dto: AppBridgeOperationDto) -> Option<BridgeOperation> {
    Some(match dto {
        AppBridgeOperationDto::QueryData => BridgeOperation::QueryData,
        AppBridgeOperationDto::MutateData => BridgeOperation::MutateData,
        AppBridgeOperationDto::NetworkRequest => BridgeOperation::NetworkRequest,
        AppBridgeOperationDto::RuntimeStatus => BridgeOperation::RuntimeStatus,
        AppBridgeOperationDto::CapturePhoto => BridgeOperation::CapturePhoto,
        AppBridgeOperationDto::PickImage => BridgeOperation::PickImage,
        AppBridgeOperationDto::RecordAudioStart => BridgeOperation::RecordAudioStart,
        AppBridgeOperationDto::RecordAudioStop => BridgeOperation::RecordAudioStop,
        AppBridgeOperationDto::GetLocation => BridgeOperation::GetLocation,
        AppBridgeOperationDto::TranscribeSpeech => BridgeOperation::TranscribeSpeech,
        AppBridgeOperationDto::PostNotification => BridgeOperation::PostNotification,
        AppBridgeOperationDto::ClipboardGetText => BridgeOperation::ClipboardGetText,
        AppBridgeOperationDto::ClipboardSetText => BridgeOperation::ClipboardSetText,
        AppBridgeOperationDto::Share => BridgeOperation::Share,
        AppBridgeOperationDto::SynthesizeSpeech => BridgeOperation::SynthesizeSpeech,
        AppBridgeOperationDto::FileRead => BridgeOperation::FileRead,
        AppBridgeOperationDto::FileWrite => BridgeOperation::FileWrite,
        AppBridgeOperationDto::DeviceStatus => BridgeOperation::DeviceStatus,
        AppBridgeOperationDto::Haptics => BridgeOperation::Haptics,
        AppBridgeOperationDto::DeepLink => BridgeOperation::DeepLink,
        AppBridgeOperationDto::LlmChat => BridgeOperation::LlmChat,
        AppBridgeOperationDto::LlmStream => BridgeOperation::LlmStream,
        AppBridgeOperationDto::AgentPost => BridgeOperation::AgentPost,
        AppBridgeOperationDto::AgentSessionCreate => BridgeOperation::AgentSessionCreate,
        AppBridgeOperationDto::AgentSessionList => BridgeOperation::AgentSessionList,
        AppBridgeOperationDto::AgentSessionResume => BridgeOperation::AgentSessionResume,
        AppBridgeOperationDto::AgentSessionClose => BridgeOperation::AgentSessionClose,
        AppBridgeOperationDto::AgentSend => BridgeOperation::AgentSend,
        AppBridgeOperationDto::AgentStream => BridgeOperation::AgentStream,
        AppBridgeOperationDto::AgentCancel => BridgeOperation::AgentCancel,
        AppBridgeOperationDto::AgentProfileProposeUpdate => {
            BridgeOperation::AgentProfileProposeUpdate
        }
        AppBridgeOperationDto::BackgroundSchedule => BridgeOperation::BackgroundSchedule,
        AppBridgeOperationDto::BackgroundList => BridgeOperation::BackgroundList,
        AppBridgeOperationDto::BackgroundStatus => BridgeOperation::BackgroundStatus,
        AppBridgeOperationDto::BackgroundCancel => BridgeOperation::BackgroundCancel,
        AppBridgeOperationDto::BackgroundRetry => BridgeOperation::BackgroundRetry,
        AppBridgeOperationDto::CalendarListEvents => BridgeOperation::CalendarListEvents,
        AppBridgeOperationDto::ContactsSearch => BridgeOperation::ContactsSearch,
        AppBridgeOperationDto::MediaGet => BridgeOperation::MediaGet,
        _ => return None,
    })
}

/// The client protocol's name for a service operation. Exhaustive: adding an
/// operation to the service fails the build here until the client protocol has
/// it too.
pub(crate) fn bridge_operation_to_dto(operation: BridgeOperation) -> AppBridgeOperationDto {
    match operation {
        BridgeOperation::QueryData => AppBridgeOperationDto::QueryData,
        BridgeOperation::MutateData => AppBridgeOperationDto::MutateData,
        BridgeOperation::NetworkRequest => AppBridgeOperationDto::NetworkRequest,
        BridgeOperation::RuntimeStatus => AppBridgeOperationDto::RuntimeStatus,
        BridgeOperation::CapturePhoto => AppBridgeOperationDto::CapturePhoto,
        BridgeOperation::PickImage => AppBridgeOperationDto::PickImage,
        BridgeOperation::RecordAudioStart => AppBridgeOperationDto::RecordAudioStart,
        BridgeOperation::RecordAudioStop => AppBridgeOperationDto::RecordAudioStop,
        BridgeOperation::GetLocation => AppBridgeOperationDto::GetLocation,
        BridgeOperation::TranscribeSpeech => AppBridgeOperationDto::TranscribeSpeech,
        BridgeOperation::PostNotification => AppBridgeOperationDto::PostNotification,
        BridgeOperation::ClipboardGetText => AppBridgeOperationDto::ClipboardGetText,
        BridgeOperation::ClipboardSetText => AppBridgeOperationDto::ClipboardSetText,
        BridgeOperation::Share => AppBridgeOperationDto::Share,
        BridgeOperation::SynthesizeSpeech => AppBridgeOperationDto::SynthesizeSpeech,
        BridgeOperation::FileRead => AppBridgeOperationDto::FileRead,
        BridgeOperation::FileWrite => AppBridgeOperationDto::FileWrite,
        BridgeOperation::DeviceStatus => AppBridgeOperationDto::DeviceStatus,
        BridgeOperation::Haptics => AppBridgeOperationDto::Haptics,
        BridgeOperation::DeepLink => AppBridgeOperationDto::DeepLink,
        BridgeOperation::LlmChat => AppBridgeOperationDto::LlmChat,
        BridgeOperation::LlmStream => AppBridgeOperationDto::LlmStream,
        BridgeOperation::AgentPost => AppBridgeOperationDto::AgentPost,
        BridgeOperation::AgentSessionCreate => AppBridgeOperationDto::AgentSessionCreate,
        BridgeOperation::AgentSessionList => AppBridgeOperationDto::AgentSessionList,
        BridgeOperation::AgentSessionResume => AppBridgeOperationDto::AgentSessionResume,
        BridgeOperation::AgentSessionClose => AppBridgeOperationDto::AgentSessionClose,
        BridgeOperation::AgentSend => AppBridgeOperationDto::AgentSend,
        BridgeOperation::AgentStream => AppBridgeOperationDto::AgentStream,
        BridgeOperation::AgentCancel => AppBridgeOperationDto::AgentCancel,
        BridgeOperation::AgentProfileProposeUpdate => {
            AppBridgeOperationDto::AgentProfileProposeUpdate
        }
        BridgeOperation::BackgroundSchedule => AppBridgeOperationDto::BackgroundSchedule,
        BridgeOperation::BackgroundList => AppBridgeOperationDto::BackgroundList,
        BridgeOperation::BackgroundStatus => AppBridgeOperationDto::BackgroundStatus,
        BridgeOperation::BackgroundCancel => AppBridgeOperationDto::BackgroundCancel,
        BridgeOperation::BackgroundRetry => AppBridgeOperationDto::BackgroundRetry,
        BridgeOperation::CalendarListEvents => AppBridgeOperationDto::CalendarListEvents,
        BridgeOperation::ContactsSearch => AppBridgeOperationDto::ContactsSearch,
        BridgeOperation::MediaGet => AppBridgeOperationDto::MediaGet,
    }
}

/// The decision the client sent, in the service's terms. One this build does
/// not know is a refusal: the person has not granted what they were asked for.
pub(crate) fn authorization_decision_from_client(
    dto: AppAuthorizationDecisionDto,
) -> AuthorizationDecision {
    authorization_decision_from_dto(dto).unwrap_or(AuthorizationDecision::Deny)
}

/// A request from the client, in the service's terms. `Err` carries the
/// response to send instead when the operation is not one this build knows.
pub(crate) fn bridge_request_from_dto(
    dto: AppBridgeRequestDto,
) -> Result<BridgeRequest, BridgeResponse> {
    match bridge_operation_from_dto(dto.operation) {
        Some(operation) => Ok(BridgeRequest {
            request_id: dto.request_id,
            app_id: dto.app_id,
            operation,
            payload_json: dto.payload_json,
        }),
        None => Err(BridgeResponse {
            request_id: dto.request_id,
            app_id: dto.app_id,
            ok: false,
            result_json: None,
            error: Some("unsupported bridge operation for this engine version".into()),
            error_code: None,
        }),
    }
}

pub(crate) fn bridge_request_to_dto(request: BridgeRequest) -> AppBridgeRequestDto {
    AppBridgeRequestDto {
        request_id: request.request_id,
        app_id: request.app_id,
        operation: bridge_operation_to_dto(request.operation),
        payload_json: request.payload_json,
    }
}

pub(crate) fn bridge_response_to_dto(response: BridgeResponse) -> AppBridgeResponseDto {
    AppBridgeResponseDto {
        request_id: response.request_id,
        app_id: response.app_id,
        ok: response.ok,
        result_json: response.result_json,
        error: response.error,
        error_code: response.error_code,
    }
}

pub(crate) fn bridge_frame_to_dto(frame: BridgeStreamFrame) -> AppBridgeStreamFrameDto {
    match frame {
        BridgeStreamFrame::Started {
            app_id,
            request_id,
            stream_id,
        } => AppBridgeStreamFrameDto::Started {
            app_id,
            request_id,
            stream_id,
        },
        BridgeStreamFrame::Data {
            app_id,
            request_id,
            stream_id,
            seq,
            data_json,
        } => AppBridgeStreamFrameDto::Data {
            app_id,
            request_id,
            stream_id,
            seq,
            data_json,
        },
        BridgeStreamFrame::Completed {
            app_id,
            request_id,
            stream_id,
            seq,
        } => AppBridgeStreamFrameDto::Completed {
            app_id,
            request_id,
            stream_id,
            seq,
        },
        BridgeStreamFrame::Error {
            app_id,
            request_id,
            stream_id,
            seq,
            code,
            message,
        } => AppBridgeStreamFrameDto::Error {
            app_id,
            request_id,
            stream_id,
            seq,
            code,
            message,
        },
        BridgeStreamFrame::Cancelled {
            app_id,
            request_id,
            stream_id,
            seq,
            reason,
        } => AppBridgeStreamFrameDto::Cancelled {
            app_id,
            request_id,
            stream_id,
            seq,
            reason,
        },
    }
}

pub(crate) fn authorization_decision_to_dto(
    value: AuthorizationDecision,
) -> AppAuthorizationDecisionDto {
    match value {
        AuthorizationDecision::Deny => AppAuthorizationDecisionDto::Deny,
        AuthorizationDecision::AllowOnce => AppAuthorizationDecisionDto::AllowOnce,
        AuthorizationDecision::AllowSession => AppAuthorizationDecisionDto::AllowSession,
        AuthorizationDecision::AllowAlways => AppAuthorizationDecisionDto::AllowAlways,
    }
}

pub(crate) fn authorization_decision_from_dto(
    value: AppAuthorizationDecisionDto,
) -> Option<AuthorizationDecision> {
    Some(match value {
        AppAuthorizationDecisionDto::Deny => AuthorizationDecision::Deny,
        AppAuthorizationDecisionDto::AllowOnce => AuthorizationDecision::AllowOnce,
        AppAuthorizationDecisionDto::AllowSession => AuthorizationDecision::AllowSession,
        AppAuthorizationDecisionDto::AllowAlways => AuthorizationDecision::AllowAlways,
        _ => return None,
    })
}

pub(crate) fn capability_kind_to_dto(value: CapabilityKind) -> AppCapabilityKindDto {
    match value {
        CapabilityKind::DataMutation => AppCapabilityKindDto::DataMutation,
        CapabilityKind::UiControl => AppCapabilityKindDto::UiControl,
        CapabilityKind::NetworkDomain => AppCapabilityKindDto::NetworkDomain,
        CapabilityKind::RestoreCheckpoint => AppCapabilityKindDto::RestoreCheckpoint,
        CapabilityKind::DependencyChange => AppCapabilityKindDto::DependencyChange,
        CapabilityKind::Camera => AppCapabilityKindDto::Camera,
        CapabilityKind::PhotoLibrary => AppCapabilityKindDto::PhotoLibrary,
        CapabilityKind::Microphone => AppCapabilityKindDto::Microphone,
        CapabilityKind::Location => AppCapabilityKindDto::Location,
        CapabilityKind::Notifications => AppCapabilityKindDto::Notifications,
        CapabilityKind::Files => AppCapabilityKindDto::Files,
        CapabilityKind::Clipboard => AppCapabilityKindDto::Clipboard,
        CapabilityKind::Share => AppCapabilityKindDto::Share,
        CapabilityKind::TextToSpeech => AppCapabilityKindDto::TextToSpeech,
        CapabilityKind::DeviceStatus => AppCapabilityKindDto::DeviceStatus,
        CapabilityKind::Haptics => AppCapabilityKindDto::Haptics,
        CapabilityKind::DeepLink => AppCapabilityKindDto::DeepLink,
        CapabilityKind::Llm => AppCapabilityKindDto::Llm,
        CapabilityKind::AgentNotify => AppCapabilityKindDto::AgentNotify,
        CapabilityKind::BackgroundSchedule => AppCapabilityKindDto::BackgroundSchedule,
        CapabilityKind::Calendar => AppCapabilityKindDto::Calendar,
        CapabilityKind::Contacts => AppCapabilityKindDto::Contacts,
        CapabilityKind::Media => AppCapabilityKindDto::Media,
        CapabilityKind::FilesRead => AppCapabilityKindDto::FilesRead,
        CapabilityKind::FilesWrite => AppCapabilityKindDto::FilesWrite,
    }
}

pub(crate) fn capability_kind_from_dto(value: AppCapabilityKindDto) -> Option<CapabilityKind> {
    Some(match value {
        AppCapabilityKindDto::DataMutation => CapabilityKind::DataMutation,
        AppCapabilityKindDto::UiControl => CapabilityKind::UiControl,
        AppCapabilityKindDto::NetworkDomain => CapabilityKind::NetworkDomain,
        AppCapabilityKindDto::RestoreCheckpoint => CapabilityKind::RestoreCheckpoint,
        AppCapabilityKindDto::DependencyChange => CapabilityKind::DependencyChange,
        AppCapabilityKindDto::Camera => CapabilityKind::Camera,
        AppCapabilityKindDto::PhotoLibrary => CapabilityKind::PhotoLibrary,
        AppCapabilityKindDto::Microphone => CapabilityKind::Microphone,
        AppCapabilityKindDto::Location => CapabilityKind::Location,
        AppCapabilityKindDto::Notifications => CapabilityKind::Notifications,
        AppCapabilityKindDto::Files => CapabilityKind::Files,
        AppCapabilityKindDto::Clipboard => CapabilityKind::Clipboard,
        AppCapabilityKindDto::Share => CapabilityKind::Share,
        AppCapabilityKindDto::TextToSpeech => CapabilityKind::TextToSpeech,
        AppCapabilityKindDto::DeviceStatus => CapabilityKind::DeviceStatus,
        AppCapabilityKindDto::Haptics => CapabilityKind::Haptics,
        AppCapabilityKindDto::DeepLink => CapabilityKind::DeepLink,
        AppCapabilityKindDto::Llm => CapabilityKind::Llm,
        AppCapabilityKindDto::AgentNotify => CapabilityKind::AgentNotify,
        AppCapabilityKindDto::BackgroundSchedule => CapabilityKind::BackgroundSchedule,
        AppCapabilityKindDto::Calendar => CapabilityKind::Calendar,
        AppCapabilityKindDto::Contacts => CapabilityKind::Contacts,
        AppCapabilityKindDto::Media => CapabilityKind::Media,
        AppCapabilityKindDto::FilesRead => CapabilityKind::FilesRead,
        AppCapabilityKindDto::FilesWrite => CapabilityKind::FilesWrite,
        _ => return None,
    })
}

pub(crate) fn capability_request_to_dto(value: CapabilityRequest) -> AppCapabilityRequestDto {
    AppCapabilityRequestDto {
        request_id: value.request_id,
        app_id: value.app_id,
        capability: capability_kind_to_dto(value.capability),
        domain: value.domain,
        reason: value.reason,
    }
}

pub(crate) fn ui_action_kind_to_dto(value: UiActionKind) -> AppUiActionKindDto {
    match value {
        UiActionKind::Inspect => AppUiActionKindDto::Inspect,
        UiActionKind::Click => AppUiActionKindDto::Click,
        UiActionKind::Fill => AppUiActionKindDto::Fill,
        UiActionKind::Select => AppUiActionKindDto::Select,
        UiActionKind::Toggle => AppUiActionKindDto::Toggle,
        UiActionKind::Scroll => AppUiActionKindDto::Scroll,
        UiActionKind::Navigate => AppUiActionKindDto::Navigate,
        UiActionKind::Back => AppUiActionKindDto::Back,
        UiActionKind::Reload => AppUiActionKindDto::Reload,
        UiActionKind::CaptureView => AppUiActionKindDto::CaptureView,
        UiActionKind::Pointer => AppUiActionKindDto::Pointer,
        UiActionKind::Key => AppUiActionKindDto::Key,
    }
}

pub(crate) fn ui_action_kind_from_dto(value: AppUiActionKindDto) -> Option<UiActionKind> {
    Some(match value {
        AppUiActionKindDto::Inspect => UiActionKind::Inspect,
        AppUiActionKindDto::Click => UiActionKind::Click,
        AppUiActionKindDto::Fill => UiActionKind::Fill,
        AppUiActionKindDto::Select => UiActionKind::Select,
        AppUiActionKindDto::Toggle => UiActionKind::Toggle,
        AppUiActionKindDto::Scroll => UiActionKind::Scroll,
        AppUiActionKindDto::Navigate => UiActionKind::Navigate,
        AppUiActionKindDto::Back => UiActionKind::Back,
        AppUiActionKindDto::Reload => UiActionKind::Reload,
        AppUiActionKindDto::CaptureView => UiActionKind::CaptureView,
        AppUiActionKindDto::Pointer => UiActionKind::Pointer,
        AppUiActionKindDto::Key => UiActionKind::Key,
        _ => return None,
    })
}

pub(crate) fn ui_target_to_dto(value: UiTarget) -> AppUiTargetDto {
    AppUiTargetDto {
        element_id: value.element_id,
        role: value.role,
        name: value.name,
    }
}

pub(crate) fn ui_request_to_dto(value: UiRequest) -> AppUiRequestDto {
    AppUiRequestDto {
        request_id: value.request_id,
        app_id: value.app_id,
        action: ui_action_kind_to_dto(value.action),
        target: value.target.map(ui_target_to_dto),
        value: value.value,
    }
}

pub(crate) fn dependency_change_kind_to_dto(
    value: DependencyChangeKind,
) -> AppDependencyChangeKindDto {
    match value {
        DependencyChangeKind::Add => AppDependencyChangeKindDto::Add,
        DependencyChangeKind::Update => AppDependencyChangeKindDto::Update,
        DependencyChangeKind::Remove => AppDependencyChangeKindDto::Remove,
    }
}

pub(crate) fn dependency_change_kind_from_dto(
    value: AppDependencyChangeKindDto,
) -> Option<DependencyChangeKind> {
    Some(match value {
        AppDependencyChangeKindDto::Add => DependencyChangeKind::Add,
        AppDependencyChangeKindDto::Update => DependencyChangeKind::Update,
        AppDependencyChangeKindDto::Remove => DependencyChangeKind::Remove,
        _ => return None,
    })
}

pub(crate) fn dependency_change_review_to_dto(
    value: DependencyChangeReview,
) -> AppDependencyChangeDto {
    AppDependencyChangeDto {
        kind: dependency_change_kind_to_dto(value.kind),
        package: value.package,
        version: value.version,
        cache_status: value.cache_status,
        download_status: value.download_status,
    }
}

pub(crate) fn dependency_change_confirmation_request_to_dto(
    value: DependencyChangeConfirmationRequest,
) -> AppDependencyChangeConfirmationRequestDto {
    AppDependencyChangeConfirmationRequestDto {
        request_id: value.request_id,
        app_id: value.app_id,
        reason: value.reason,
        changes: value
            .changes
            .into_iter()
            .map(dependency_change_review_to_dto)
            .collect(),
        license_risk: value.license_risk,
        sbom_risk: value.sbom_risk,
        lifecycle_scripts_blocked: value.lifecycle_scripts_blocked,
        native_addons_blocked: value.native_addons_blocked,
        rollback_policy: value.rollback_policy,
    }
}

pub(crate) fn verification_status_to_dto(
    value: VerificationStatus,
) -> LocalAppVerificationStatusDto {
    match value {
        VerificationStatus::Pending => LocalAppVerificationStatusDto::Pending,
        VerificationStatus::Passed => LocalAppVerificationStatusDto::Passed,
        VerificationStatus::Failed => LocalAppVerificationStatusDto::Failed,
        VerificationStatus::Unverified => LocalAppVerificationStatusDto::Unverified,
        VerificationStatus::Unavailable => LocalAppVerificationStatusDto::Unavailable,
    }
}

pub(crate) fn verification_status_from_dto(
    value: LocalAppVerificationStatusDto,
) -> Option<VerificationStatus> {
    Some(match value {
        LocalAppVerificationStatusDto::Pending => VerificationStatus::Pending,
        LocalAppVerificationStatusDto::Passed => VerificationStatus::Passed,
        LocalAppVerificationStatusDto::Failed => VerificationStatus::Failed,
        LocalAppVerificationStatusDto::Unverified => VerificationStatus::Unverified,
        LocalAppVerificationStatusDto::Unavailable => VerificationStatus::Unavailable,
        _ => return None,
    })
}

pub(crate) fn verification_summary_to_dto(
    value: VerificationSummary,
) -> LocalAppVerificationSummaryDto {
    LocalAppVerificationSummaryDto {
        status: verification_status_to_dto(value.status),
        summary: value.summary,
        code: value.code,
    }
}

pub(crate) fn gate_status_to_dto(value: GateStatus) -> LocalAppGateStatusDto {
    LocalAppGateStatusDto {
        gate_id: value.gate_id,
        label: value.label,
        status: verification_status_to_dto(value.status),
        available: value.available,
        detail: value.detail,
    }
}

pub(crate) fn publication_state_to_dto(value: PublicationState) -> AppWorkflowStateDto {
    match value {
        PublicationState::Draft => AppWorkflowStateDto::Draft,
        PublicationState::PublishedUnverified => AppWorkflowStateDto::PublishedUnverified,
        PublicationState::PublishedVerified => AppWorkflowStateDto::PublishedVerified,
    }
}

pub(crate) fn publication_state_from_dto(value: AppWorkflowStateDto) -> Option<PublicationState> {
    Some(match value {
        AppWorkflowStateDto::Draft => PublicationState::Draft,
        AppWorkflowStateDto::PublishedUnverified => PublicationState::PublishedUnverified,
        AppWorkflowStateDto::PublishedVerified => PublicationState::PublishedVerified,
        _ => return None,
    })
}

pub(crate) fn mcp_tool_change_kind_to_dto(
    value: McpToolChangeKind,
) -> LocalAppMcpToolChangeKindDto {
    match value {
        McpToolChangeKind::Added => LocalAppMcpToolChangeKindDto::Added,
        McpToolChangeKind::Removed => LocalAppMcpToolChangeKindDto::Removed,
        McpToolChangeKind::Changed => LocalAppMcpToolChangeKindDto::Changed,
    }
}

pub(crate) fn mcp_tool_change_kind_from_dto(
    value: LocalAppMcpToolChangeKindDto,
) -> Option<McpToolChangeKind> {
    Some(match value {
        LocalAppMcpToolChangeKindDto::Added => McpToolChangeKind::Added,
        LocalAppMcpToolChangeKindDto::Removed => McpToolChangeKind::Removed,
        LocalAppMcpToolChangeKindDto::Changed => McpToolChangeKind::Changed,
        _ => return None,
    })
}

pub(crate) fn mcp_tool_field_to_dto(value: McpToolField) -> LocalAppMcpToolFieldDto {
    match value {
        McpToolField::Name => LocalAppMcpToolFieldDto::Name,
        McpToolField::Title => LocalAppMcpToolFieldDto::Title,
        McpToolField::Description => LocalAppMcpToolFieldDto::Description,
        McpToolField::InputSchema => LocalAppMcpToolFieldDto::InputSchema,
        McpToolField::OutputSchema => LocalAppMcpToolFieldDto::OutputSchema,
        McpToolField::Annotations => LocalAppMcpToolFieldDto::Annotations,
        McpToolField::Execution => LocalAppMcpToolFieldDto::Execution,
        McpToolField::VisibleMeta => LocalAppMcpToolFieldDto::VisibleMeta,
        McpToolField::SemanticFlow => LocalAppMcpToolFieldDto::SemanticFlow,
        McpToolField::PermissionCeiling => LocalAppMcpToolFieldDto::PermissionCeiling,
    }
}

pub(crate) fn mcp_tool_field_from_dto(value: LocalAppMcpToolFieldDto) -> Option<McpToolField> {
    Some(match value {
        LocalAppMcpToolFieldDto::Name => McpToolField::Name,
        LocalAppMcpToolFieldDto::Title => McpToolField::Title,
        LocalAppMcpToolFieldDto::Description => McpToolField::Description,
        LocalAppMcpToolFieldDto::InputSchema => McpToolField::InputSchema,
        LocalAppMcpToolFieldDto::OutputSchema => McpToolField::OutputSchema,
        LocalAppMcpToolFieldDto::Annotations => McpToolField::Annotations,
        LocalAppMcpToolFieldDto::Execution => McpToolField::Execution,
        LocalAppMcpToolFieldDto::VisibleMeta => McpToolField::VisibleMeta,
        LocalAppMcpToolFieldDto::SemanticFlow => McpToolField::SemanticFlow,
        LocalAppMcpToolFieldDto::PermissionCeiling => McpToolField::PermissionCeiling,
        _ => return None,
    })
}

pub(crate) fn mcp_tool_surface_to_dto(value: McpToolSurface) -> LocalAppMcpToolSurfaceDto {
    LocalAppMcpToolSurfaceDto {
        name: value.name,
        title: value.title,
        description: value.description,
        input_schema_json: value.input_schema_json,
        output_schema_json: value.output_schema_json,
        annotations_json: value.annotations_json,
        execution_json: value.execution_json,
        visible_meta_json: value.visible_meta_json,
        semantic_flow_json: value.semantic_flow_json,
        permission_ceiling: value.permission_ceiling,
    }
}

pub(crate) fn mcp_tool_diff_to_dto(value: McpToolDiff) -> LocalAppMcpToolDiffDto {
    LocalAppMcpToolDiffDto {
        kind: mcp_tool_change_kind_to_dto(value.kind),
        name: value.name,
        before: value.before.map(mcp_tool_surface_to_dto),
        after: value.after.map(mcp_tool_surface_to_dto),
        changed_fields: value
            .changed_fields
            .into_iter()
            .map(mcp_tool_field_to_dto)
            .collect(),
    }
}

pub(crate) fn mcp_proposal_approval_request_to_dto(
    value: McpProposalApprovalRequest,
) -> LocalAppMcpProposalApprovalRequestDto {
    LocalAppMcpProposalApprovalRequestDto {
        request_id: value.request_id,
        app_id: value.app_id,
        workflow_run_id: value.workflow_run_id,
        summary: value.summary,
        proposal_sha256: value.proposal_sha256,
        approval_contract_sha256: value.approval_contract_sha256,
        tool_surface_sha256: value.tool_surface_sha256,
        tool_diffs: value
            .tool_diffs
            .into_iter()
            .map(mcp_tool_diff_to_dto)
            .collect(),
        required_flow_changes: value.required_flow_changes,
        excluded_capabilities: value.excluded_capabilities,
        pending_gates: value
            .pending_gates
            .into_iter()
            .map(gate_status_to_dto)
            .collect(),
    }
}

pub(crate) fn agent_profile_proposal_to_dto(
    value: AgentProfileProposal,
) -> AppAgentProfileProposalDto {
    AppAgentProfileProposalDto {
        app_id: value.app_id,
        approval_token: value.approval_token,
        base_revision: value.base_revision,
        current_revision: value.current_revision,
        instructions: value.instructions,
        reason: value.reason,
    }
}

pub(crate) fn mcp_app_widget_to_dto(value: McpAppWidget) -> McpAppWidgetDto {
    McpAppWidgetDto {
        resource_uri: value.resource_uri,
        mime_type: value.mime_type,
        resource_sha256: value.resource_sha256,
    }
}

pub(crate) fn managed_mcp_status_to_dto(value: ManagedMcpStatus) -> ManagedLocalAppMcpStatusDto {
    match value {
        ManagedMcpStatus::Disabled => ManagedLocalAppMcpStatusDto::Disabled,
        ManagedMcpStatus::NeedsSetup => ManagedLocalAppMcpStatusDto::NeedsSetup,
        ManagedMcpStatus::Authoring => ManagedLocalAppMcpStatusDto::Authoring,
        ManagedMcpStatus::Enabled => ManagedLocalAppMcpStatusDto::Enabled,
        ManagedMcpStatus::NeedsRevalidation => ManagedLocalAppMcpStatusDto::NeedsRevalidation,
        ManagedMcpStatus::Error => ManagedLocalAppMcpStatusDto::Error,
    }
}

pub(crate) fn managed_mcp_status_from_dto(
    value: ManagedLocalAppMcpStatusDto,
) -> Option<ManagedMcpStatus> {
    Some(match value {
        ManagedLocalAppMcpStatusDto::Disabled => ManagedMcpStatus::Disabled,
        ManagedLocalAppMcpStatusDto::NeedsSetup => ManagedMcpStatus::NeedsSetup,
        ManagedLocalAppMcpStatusDto::Authoring => ManagedMcpStatus::Authoring,
        ManagedLocalAppMcpStatusDto::Enabled => ManagedMcpStatus::Enabled,
        ManagedLocalAppMcpStatusDto::NeedsRevalidation => ManagedMcpStatus::NeedsRevalidation,
        ManagedLocalAppMcpStatusDto::Error => ManagedMcpStatus::Error,
        _ => return None,
    })
}

pub(crate) fn managed_mcp_server_to_dto(value: ManagedMcpServer) -> ManagedLocalAppMcpServerDto {
    ManagedLocalAppMcpServerDto {
        server_name: value.server_name,
        app_id: value.app_id,
        app_name: value.app_name,
        enabled: value.enabled,
        status: managed_mcp_status_to_dto(value.status),
        settings_revision: value.settings_revision,
        enabled_tools: value.enabled_tools,
        pinned_to_current_conversation: value.pinned_to_current_conversation,
        build_id: value.build_id,
        catalog_sha256: value.catalog_sha256,
        tool_surface_sha256: value.tool_surface_sha256,
        tool_count: value.tool_count,
        authoring_revision: value.authoring_revision,
        publication_state: publication_state_to_dto(value.publication_state),
        mcp_verification: verification_summary_to_dto(value.mcp_verification),
        ui_verification: verification_summary_to_dto(value.ui_verification),
        widget: value.widget.map(mcp_app_widget_to_dto),
        tools: value
            .tools
            .into_iter()
            .map(mcp_tool_surface_to_dto)
            .collect(),
    }
}

pub(crate) fn plugin_error_code_to_dto(value: PluginErrorCode) -> LocalAppPluginErrorCodeDto {
    match value {
        PluginErrorCode::PluginDisabled => LocalAppPluginErrorCodeDto::PluginDisabled,
        PluginErrorCode::BuiltinBundleUnavailable => {
            LocalAppPluginErrorCodeDto::BuiltinBundleUnavailable
        }
        PluginErrorCode::TemplateUnavailable => LocalAppPluginErrorCodeDto::TemplateUnavailable,
        PluginErrorCode::ProposalInvalid => LocalAppPluginErrorCodeDto::ProposalInvalid,
        PluginErrorCode::CatalogStale => LocalAppPluginErrorCodeDto::CatalogStale,
        PluginErrorCode::ActiveStateCorrupt => LocalAppPluginErrorCodeDto::ActiveStateCorrupt,
        PluginErrorCode::RevisionConflict => LocalAppPluginErrorCodeDto::RevisionConflict,
        PluginErrorCode::InvalidMcpSettings => LocalAppPluginErrorCodeDto::InvalidMcpSettings,
        PluginErrorCode::McpAuthoringRequired => LocalAppPluginErrorCodeDto::McpAuthoringRequired,
        PluginErrorCode::RepairBudgetExhausted => LocalAppPluginErrorCodeDto::RepairBudgetExhausted,
        PluginErrorCode::ExposureCapacityReached => {
            LocalAppPluginErrorCodeDto::ExposureCapacityReached
        }
    }
}

pub(crate) fn plugin_error_code_from_dto(
    value: LocalAppPluginErrorCodeDto,
) -> Option<PluginErrorCode> {
    Some(match value {
        LocalAppPluginErrorCodeDto::PluginDisabled => PluginErrorCode::PluginDisabled,
        LocalAppPluginErrorCodeDto::BuiltinBundleUnavailable => {
            PluginErrorCode::BuiltinBundleUnavailable
        }
        LocalAppPluginErrorCodeDto::TemplateUnavailable => PluginErrorCode::TemplateUnavailable,
        LocalAppPluginErrorCodeDto::ProposalInvalid => PluginErrorCode::ProposalInvalid,
        LocalAppPluginErrorCodeDto::CatalogStale => PluginErrorCode::CatalogStale,
        LocalAppPluginErrorCodeDto::ActiveStateCorrupt => PluginErrorCode::ActiveStateCorrupt,
        LocalAppPluginErrorCodeDto::RevisionConflict => PluginErrorCode::RevisionConflict,
        LocalAppPluginErrorCodeDto::InvalidMcpSettings => PluginErrorCode::InvalidMcpSettings,
        LocalAppPluginErrorCodeDto::McpAuthoringRequired => PluginErrorCode::McpAuthoringRequired,
        LocalAppPluginErrorCodeDto::RepairBudgetExhausted => PluginErrorCode::RepairBudgetExhausted,
        LocalAppPluginErrorCodeDto::ExposureCapacityReached => {
            PluginErrorCode::ExposureCapacityReached
        }
        _ => return None,
    })
}

/// What the native client is told for an event the service emitted.
pub(crate) fn host_event_to_client(event: HostEvent) -> ClientEvent {
    let event = match event {
        HostEvent::BridgeResponse(response) => AppEventDto::AppBridgeResponse {
            response: bridge_response_to_dto(response),
        },
        HostEvent::BridgeStreamFrame(frame) => {
            // FFI clients deliver a frame to the WebView as the camelCase JSON
            // the page expects, so it is serialized once here.
            let frame_json = serde_json::to_string(&frame).unwrap_or_else(|_| "{}".into());
            AppEventDto::AppBridgeStreamFrame {
                frame: bridge_frame_to_dto(frame),
                frame_json,
            }
        }
        HostEvent::CapabilityRequested(request) => AppEventDto::AppCapabilityRequested {
            request: capability_request_to_dto(request),
        },
        HostEvent::UiRequest(request) => AppEventDto::AppUiRequest {
            request: ui_request_to_dto(request),
        },
        HostEvent::DependencyChangeConfirmationRequested(request) => {
            AppEventDto::AppDependencyChangeConfirmationRequested {
                request: dependency_change_confirmation_request_to_dto(request),
            }
        }
        HostEvent::McpProposalApprovalRequested(request) => {
            AppEventDto::McpProposalApprovalRequested {
                request: mcp_proposal_approval_request_to_dto(request),
            }
        }
        HostEvent::ProfileProposal(proposal) => AppEventDto::AppProfileProposal {
            proposal: agent_profile_proposal_to_dto(proposal),
        },
        HostEvent::LlmActivityChanged { app_id, active } => {
            AppEventDto::AppLlmActivityChanged { app_id, active }
        }
        HostEvent::AgentEventPosted {
            app_id,
            seq,
            topic,
            created_at_ms,
        } => AppEventDto::AppAgentEventPosted {
            app_id,
            seq,
            topic,
            created_at_ms,
        },
        HostEvent::BackgroundTaskChanged {
            app_id,
            task_id,
            status,
            result_json,
            error,
            retryable,
        } => AppEventDto::AppBackgroundTaskChanged {
            app_id,
            task_id,
            status,
            result_json,
            error,
            retryable,
        },
        HostEvent::ManagedMcpInventoryChanged { servers } => {
            AppEventDto::ManagedMcpInventoryChanged {
                servers: servers.into_iter().map(managed_mcp_server_to_dto).collect(),
            }
        }
        HostEvent::VerificationSummaryChanged {
            app_id,
            publication_state,
            mcp_verification,
            ui_verification,
        } => AppEventDto::VerificationSummaryChanged {
            app_id,
            publication_state: publication_state_to_dto(publication_state),
            mcp_verification: verification_summary_to_dto(mcp_verification),
            ui_verification: verification_summary_to_dto(ui_verification),
        },
        HostEvent::AppOperationFailed {
            app_id,
            code,
            message,
            request_id,
        } => {
            return ClientEvent::AppOperationFailed {
                app_id,
                code: crate::mobile::local_apps_bridge::lower_error_code(code),
                message,
                request_id,
            }
        }
        HostEvent::PluginOperationFailed {
            app_id,
            code,
            message,
            request_id,
        } => AppEventDto::LocalAppOperationFailed {
            app_id,
            code: plugin_error_code_to_dto(code),
            message,
            request_id,
        },
    };
    ClientEvent::AppEvent { event }
}
/// Delivers the service's events to the native client as
/// `ClientEvent::AppEvent`, the way every Local App event has always reached it.
pub(crate) struct ClientSinkAdapter {
    sink: Arc<dyn ClientEventSink>,
}

impl ClientSinkAdapter {
    pub(crate) fn new(sink: Arc<dyn ClientEventSink>) -> Arc<dyn HostEventSink> {
        Arc::new(Self { sink })
    }
}

#[async_trait]
impl HostEventSink for ClientSinkAdapter {
    async fn emit(&self, event: HostEvent) {
        self.sink.emit(host_event_to_client(event)).await;
    }
}

/// A broker whose events reach `sink` the way the native client receives them.
/// The tests that watch what a client would see build their broker this way.
#[cfg(test)]
pub(crate) fn broker_with_client_sink(
    root: std::path::PathBuf,
    sink: Arc<dyn ClientEventSink>,
    mobile_linux: Option<Arc<dyn mobile_linux_api::MobileLinuxRuntime>>,
    full_runtime: bool,
    runtime_root: Option<std::path::PathBuf>,
) -> Arc<crate::mobile::local_apps_host::LocalAppsHostBroker> {
    crate::mobile::local_apps_host::LocalAppsHostBroker::new(
        root,
        ClientSinkAdapter::new(sink),
        mobile_linux,
        full_runtime,
        runtime_root,
    )
}

/// [`broker_with_client_sink`] with the device's physical memory stated.
#[cfg(test)]
pub(crate) fn broker_with_client_sink_and_memory(
    root: std::path::PathBuf,
    sink: Arc<dyn ClientEventSink>,
    mobile_linux: Option<Arc<dyn mobile_linux_api::MobileLinuxRuntime>>,
    full_runtime: bool,
    runtime_root: Option<std::path::PathBuf>,
    physical_memory_bytes: u64,
) -> Arc<crate::mobile::local_apps_host::LocalAppsHostBroker> {
    crate::mobile::local_apps_host::LocalAppsHostBroker::new_with_physical_memory(
        root,
        ClientSinkAdapter::new(sink),
        mobile_linux,
        full_runtime,
        runtime_root,
        physical_memory_bytes,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::to_value;

    fn every_operation() -> Vec<BridgeOperation> {
        // The list is checked for completeness by the exhaustive match below:
        // a new operation fails to compile there until it is added here too.
        let all = vec![
            BridgeOperation::QueryData,
            BridgeOperation::MutateData,
            BridgeOperation::NetworkRequest,
            BridgeOperation::RuntimeStatus,
            BridgeOperation::CapturePhoto,
            BridgeOperation::PickImage,
            BridgeOperation::RecordAudioStart,
            BridgeOperation::RecordAudioStop,
            BridgeOperation::GetLocation,
            BridgeOperation::TranscribeSpeech,
            BridgeOperation::PostNotification,
            BridgeOperation::ClipboardGetText,
            BridgeOperation::ClipboardSetText,
            BridgeOperation::Share,
            BridgeOperation::SynthesizeSpeech,
            BridgeOperation::FileRead,
            BridgeOperation::FileWrite,
            BridgeOperation::DeviceStatus,
            BridgeOperation::Haptics,
            BridgeOperation::DeepLink,
            BridgeOperation::LlmChat,
            BridgeOperation::LlmStream,
            BridgeOperation::AgentPost,
            BridgeOperation::AgentSessionCreate,
            BridgeOperation::AgentSessionList,
            BridgeOperation::AgentSessionResume,
            BridgeOperation::AgentSessionClose,
            BridgeOperation::AgentSend,
            BridgeOperation::AgentStream,
            BridgeOperation::AgentCancel,
            BridgeOperation::AgentProfileProposeUpdate,
            BridgeOperation::BackgroundSchedule,
            BridgeOperation::BackgroundList,
            BridgeOperation::BackgroundStatus,
            BridgeOperation::BackgroundCancel,
            BridgeOperation::BackgroundRetry,
            BridgeOperation::CalendarListEvents,
            BridgeOperation::ContactsSearch,
            BridgeOperation::MediaGet,
        ];
        for operation in &all {
            match operation {
                BridgeOperation::QueryData
                | BridgeOperation::MutateData
                | BridgeOperation::NetworkRequest
                | BridgeOperation::RuntimeStatus
                | BridgeOperation::CapturePhoto
                | BridgeOperation::PickImage
                | BridgeOperation::RecordAudioStart
                | BridgeOperation::RecordAudioStop
                | BridgeOperation::GetLocation
                | BridgeOperation::TranscribeSpeech
                | BridgeOperation::PostNotification
                | BridgeOperation::ClipboardGetText
                | BridgeOperation::ClipboardSetText
                | BridgeOperation::Share
                | BridgeOperation::SynthesizeSpeech
                | BridgeOperation::FileRead
                | BridgeOperation::FileWrite
                | BridgeOperation::DeviceStatus
                | BridgeOperation::Haptics
                | BridgeOperation::DeepLink
                | BridgeOperation::LlmChat
                | BridgeOperation::LlmStream
                | BridgeOperation::AgentPost
                | BridgeOperation::AgentSessionCreate
                | BridgeOperation::AgentSessionList
                | BridgeOperation::AgentSessionResume
                | BridgeOperation::AgentSessionClose
                | BridgeOperation::AgentSend
                | BridgeOperation::AgentStream
                | BridgeOperation::AgentCancel
                | BridgeOperation::AgentProfileProposeUpdate
                | BridgeOperation::BackgroundSchedule
                | BridgeOperation::BackgroundList
                | BridgeOperation::BackgroundStatus
                | BridgeOperation::BackgroundCancel
                | BridgeOperation::BackgroundRetry
                | BridgeOperation::CalendarListEvents
                | BridgeOperation::ContactsSearch
                | BridgeOperation::MediaGet => {}
            }
        }
        all
    }

    #[test]
    fn every_operation_survives_the_round_trip_under_the_same_wire_name() {
        for operation in every_operation() {
            let dto = bridge_operation_to_dto(operation);
            assert_eq!(
                to_value(dto).unwrap(),
                to_value(operation).unwrap(),
                "{operation:?} must keep its wire name"
            );
            assert_eq!(bridge_operation_from_dto(dto), Some(operation));
        }
    }

    #[test]
    fn requests_and_responses_serialize_identically_on_both_sides() {
        for payload_json in [None, Some("{\"q\":1}".to_string())] {
            let request = BridgeRequest {
                request_id: "r1".into(),
                app_id: "notes".into(),
                operation: BridgeOperation::QueryData,
                payload_json,
            };
            let dto = bridge_request_to_dto(request.clone());
            assert_eq!(to_value(&dto).unwrap(), to_value(&request).unwrap());
            assert_eq!(bridge_request_from_dto(dto).unwrap(), request);
        }
        for response in [
            BridgeResponse {
                request_id: "r1".into(),
                app_id: "notes".into(),
                ok: true,
                result_json: Some("{}".into()),
                error: None,
                error_code: None,
            },
            BridgeResponse {
                request_id: "r2".into(),
                app_id: "notes".into(),
                ok: false,
                result_json: None,
                error: Some("denied".into()),
                error_code: Some("permission_denied".into()),
            },
        ] {
            let dto = bridge_response_to_dto(response.clone());
            assert_eq!(to_value(&dto).unwrap(), to_value(&response).unwrap());
        }
    }

    #[test]
    fn stream_frames_serialize_identically_on_both_sides() {
        let (app_id, request_id, stream_id) =
            ("notes".to_string(), "r1".to_string(), "s1".to_string());
        let frames = vec![
            BridgeStreamFrame::Started {
                app_id: app_id.clone(),
                request_id: request_id.clone(),
                stream_id: stream_id.clone(),
            },
            BridgeStreamFrame::Data {
                app_id: app_id.clone(),
                request_id: request_id.clone(),
                stream_id: stream_id.clone(),
                seq: 1,
                data_json: "\"hi\"".into(),
            },
            BridgeStreamFrame::Completed {
                app_id: app_id.clone(),
                request_id: request_id.clone(),
                stream_id: stream_id.clone(),
                seq: 2,
            },
            BridgeStreamFrame::Error {
                app_id: app_id.clone(),
                request_id: request_id.clone(),
                stream_id: stream_id.clone(),
                seq: 3,
                code: "llm_busy".into(),
                message: "busy".into(),
            },
            BridgeStreamFrame::Cancelled {
                app_id,
                request_id,
                stream_id,
                seq: 4,
                reason: "user".into(),
            },
        ];
        for frame in frames {
            let dto = bridge_frame_to_dto(frame.clone());
            assert_eq!(
                to_value(&dto).unwrap(),
                to_value(&frame).unwrap(),
                "{frame:?}"
            );
        }
    }

    #[test]
    fn authorization_decision_matches_the_client_protocol_for_every_variant() {
        for name in ["deny", "allow_once", "allow_session", "allow_always"] {
            let sample = serde_json::Value::String(name.to_string());
            let value: AuthorizationDecision = serde_json::from_value(sample.clone()).expect(name);
            let dto = authorization_decision_to_dto(value);
            assert_eq!(to_value(dto).unwrap(), sample, "{name}");
            assert_eq!(authorization_decision_from_dto(dto), Some(value));
        }
    }

    #[test]
    fn capability_kind_matches_the_client_protocol_for_every_variant() {
        for name in [
            "data_mutation",
            "ui_control",
            "network_domain",
            "restore_checkpoint",
            "dependency_change",
            "camera",
            "photo_library",
            "microphone",
            "location",
            "notifications",
            "files",
            "clipboard",
            "share",
            "text_to_speech",
            "device_status",
            "haptics",
            "deep_link",
            "llm",
            "agent_notify",
            "background_schedule",
            "calendar",
            "contacts",
            "media",
            "files_read",
            "files_write",
        ] {
            let sample = serde_json::Value::String(name.to_string());
            let value: CapabilityKind = serde_json::from_value(sample.clone()).expect(name);
            let dto = capability_kind_to_dto(value);
            assert_eq!(to_value(dto).unwrap(), sample, "{name}");
            assert_eq!(capability_kind_from_dto(dto), Some(value));
        }
    }

    #[test]
    fn capability_request_matches_the_client_protocol() {
        for sample in [
            r#"{"request_id": "s", "app_id": "s", "capability": "data_mutation", "domain": "s", "reason": "s"}"#,
            r#"{"request_id": "s", "app_id": "s", "capability": "data_mutation", "reason": "s"}"#,
        ] {
            let sample: serde_json::Value = serde_json::from_str(sample).unwrap();
            let value: CapabilityRequest = serde_json::from_value(sample.clone()).unwrap();
            assert_eq!(to_value(&value).unwrap(), sample);
            assert_eq!(to_value(capability_request_to_dto(value)).unwrap(), sample);
        }
    }

    #[test]
    fn ui_action_kind_matches_the_client_protocol_for_every_variant() {
        for name in [
            "inspect",
            "click",
            "fill",
            "select",
            "toggle",
            "scroll",
            "navigate",
            "back",
            "reload",
            "capture_view",
            "pointer",
            "key",
        ] {
            let sample = serde_json::Value::String(name.to_string());
            let value: UiActionKind = serde_json::from_value(sample.clone()).expect(name);
            let dto = ui_action_kind_to_dto(value);
            assert_eq!(to_value(dto).unwrap(), sample, "{name}");
            assert_eq!(ui_action_kind_from_dto(dto), Some(value));
        }
    }

    #[test]
    fn ui_target_matches_the_client_protocol() {
        for sample in [r#"{"element_id": "s", "role": "s", "name": "s"}"#, r#"{}"#] {
            let sample: serde_json::Value = serde_json::from_str(sample).unwrap();
            let value: UiTarget = serde_json::from_value(sample.clone()).unwrap();
            assert_eq!(to_value(&value).unwrap(), sample);
            assert_eq!(to_value(ui_target_to_dto(value)).unwrap(), sample);
        }
    }

    #[test]
    fn ui_request_matches_the_client_protocol() {
        for sample in [
            r#"{"request_id": "s", "app_id": "s", "action": "inspect", "target": {"element_id": "s", "role": "s", "name": "s"}, "value": "s"}"#,
            r#"{"request_id": "s", "app_id": "s", "action": "inspect"}"#,
        ] {
            let sample: serde_json::Value = serde_json::from_str(sample).unwrap();
            let value: UiRequest = serde_json::from_value(sample.clone()).unwrap();
            assert_eq!(to_value(&value).unwrap(), sample);
            assert_eq!(to_value(ui_request_to_dto(value)).unwrap(), sample);
        }
    }

    #[test]
    fn dependency_change_kind_matches_the_client_protocol_for_every_variant() {
        for name in ["add", "update", "remove"] {
            let sample = serde_json::Value::String(name.to_string());
            let value: DependencyChangeKind = serde_json::from_value(sample.clone()).expect(name);
            let dto = dependency_change_kind_to_dto(value);
            assert_eq!(to_value(dto).unwrap(), sample, "{name}");
            assert_eq!(dependency_change_kind_from_dto(dto), Some(value));
        }
    }

    #[test]
    fn dependency_change_review_matches_the_client_protocol() {
        for sample in [
            r#"{"kind": "add", "package": "s", "version": "s", "cacheStatus": "s", "downloadStatus": "s"}"#,
            r#"{"kind": "add", "package": "s", "cacheStatus": "s", "downloadStatus": "s"}"#,
        ] {
            let sample: serde_json::Value = serde_json::from_str(sample).unwrap();
            let value: DependencyChangeReview = serde_json::from_value(sample.clone()).unwrap();
            assert_eq!(to_value(&value).unwrap(), sample);
            assert_eq!(
                to_value(dependency_change_review_to_dto(value)).unwrap(),
                sample
            );
        }
    }

    #[test]
    fn dependency_change_confirmation_request_matches_the_client_protocol() {
        for sample in [
            r#"{"requestId": "s", "appId": "s", "reason": "s", "changes": [{"kind": "add", "package": "s", "version": "s", "cacheStatus": "s", "downloadStatus": "s"}], "licenseRisk": "s", "sbomRisk": "s", "lifecycleScriptsBlocked": true, "nativeAddonsBlocked": true, "rollbackPolicy": "s"}"#,
            r#"{"requestId": "s", "appId": "s", "reason": "s", "changes": [], "licenseRisk": "s", "sbomRisk": "s", "lifecycleScriptsBlocked": true, "nativeAddonsBlocked": true, "rollbackPolicy": "s"}"#,
        ] {
            let sample: serde_json::Value = serde_json::from_str(sample).unwrap();
            let value: DependencyChangeConfirmationRequest =
                serde_json::from_value(sample.clone()).unwrap();
            assert_eq!(to_value(&value).unwrap(), sample);
            assert_eq!(
                to_value(dependency_change_confirmation_request_to_dto(value)).unwrap(),
                sample
            );
        }
    }

    #[test]
    fn verification_status_matches_the_client_protocol_for_every_variant() {
        for name in ["pending", "passed", "failed", "unverified", "unavailable"] {
            let sample = serde_json::Value::String(name.to_string());
            let value: VerificationStatus = serde_json::from_value(sample.clone()).expect(name);
            let dto = verification_status_to_dto(value);
            assert_eq!(to_value(dto).unwrap(), sample, "{name}");
            assert_eq!(verification_status_from_dto(dto), Some(value));
        }
    }

    #[test]
    fn verification_summary_matches_the_client_protocol() {
        for sample in [
            r#"{"status": "pending", "summary": "s", "code": "s"}"#,
            r#"{"status": "pending", "summary": "s"}"#,
        ] {
            let sample: serde_json::Value = serde_json::from_str(sample).unwrap();
            let value: VerificationSummary = serde_json::from_value(sample.clone()).unwrap();
            assert_eq!(to_value(&value).unwrap(), sample);
            assert_eq!(
                to_value(verification_summary_to_dto(value)).unwrap(),
                sample
            );
        }
    }

    #[test]
    fn gate_status_matches_the_client_protocol() {
        for sample in [
            r#"{"gateId": "s", "label": "s", "status": "pending", "available": true, "detail": "s"}"#,
            r#"{"gateId": "s", "label": "s", "status": "pending", "available": true}"#,
        ] {
            let sample: serde_json::Value = serde_json::from_str(sample).unwrap();
            let value: GateStatus = serde_json::from_value(sample.clone()).unwrap();
            assert_eq!(to_value(&value).unwrap(), sample);
            assert_eq!(to_value(gate_status_to_dto(value)).unwrap(), sample);
        }
    }

    #[test]
    fn publication_state_matches_the_client_protocol_for_every_variant() {
        for name in ["draft", "published_unverified", "published_verified"] {
            let sample = serde_json::Value::String(name.to_string());
            let value: PublicationState = serde_json::from_value(sample.clone()).expect(name);
            let dto = publication_state_to_dto(value);
            assert_eq!(to_value(dto).unwrap(), sample, "{name}");
            assert_eq!(publication_state_from_dto(dto), Some(value));
        }
    }

    #[test]
    fn mcp_tool_change_kind_matches_the_client_protocol_for_every_variant() {
        for name in ["added", "removed", "changed"] {
            let sample = serde_json::Value::String(name.to_string());
            let value: McpToolChangeKind = serde_json::from_value(sample.clone()).expect(name);
            let dto = mcp_tool_change_kind_to_dto(value);
            assert_eq!(to_value(dto).unwrap(), sample, "{name}");
            assert_eq!(mcp_tool_change_kind_from_dto(dto), Some(value));
        }
    }

    #[test]
    fn mcp_tool_field_matches_the_client_protocol_for_every_variant() {
        for name in [
            "name",
            "title",
            "description",
            "input_schema",
            "output_schema",
            "annotations",
            "execution",
            "visible_meta",
            "semantic_flow",
            "permission_ceiling",
        ] {
            let sample = serde_json::Value::String(name.to_string());
            let value: McpToolField = serde_json::from_value(sample.clone()).expect(name);
            let dto = mcp_tool_field_to_dto(value);
            assert_eq!(to_value(dto).unwrap(), sample, "{name}");
            assert_eq!(mcp_tool_field_from_dto(dto), Some(value));
        }
    }

    #[test]
    fn mcp_tool_surface_matches_the_client_protocol() {
        for sample in [
            r#"{"name": "s", "title": "s", "description": "s", "inputSchemaJson": "s", "outputSchemaJson": "s", "annotationsJson": "s", "executionJson": "s", "visibleMetaJson": "s", "semanticFlowJson": "s", "permissionCeiling": "s"}"#,
            r#"{"name": "s", "inputSchemaJson": "s", "semanticFlowJson": "s", "permissionCeiling": "s"}"#,
        ] {
            let sample: serde_json::Value = serde_json::from_str(sample).unwrap();
            let value: McpToolSurface = serde_json::from_value(sample.clone()).unwrap();
            assert_eq!(to_value(&value).unwrap(), sample);
            assert_eq!(to_value(mcp_tool_surface_to_dto(value)).unwrap(), sample);
        }
    }

    #[test]
    fn mcp_tool_diff_matches_the_client_protocol() {
        for sample in [
            r#"{"kind": "added", "name": "s", "before": {"name": "s", "title": "s", "description": "s", "inputSchemaJson": "s", "outputSchemaJson": "s", "annotationsJson": "s", "executionJson": "s", "visibleMetaJson": "s", "semanticFlowJson": "s", "permissionCeiling": "s"}, "after": {"name": "s", "title": "s", "description": "s", "inputSchemaJson": "s", "outputSchemaJson": "s", "annotationsJson": "s", "executionJson": "s", "visibleMetaJson": "s", "semanticFlowJson": "s", "permissionCeiling": "s"}, "changedFields": ["name"]}"#,
            r#"{"kind": "added", "name": "s"}"#,
        ] {
            let sample: serde_json::Value = serde_json::from_str(sample).unwrap();
            let value: McpToolDiff = serde_json::from_value(sample.clone()).unwrap();
            assert_eq!(to_value(&value).unwrap(), sample);
            assert_eq!(to_value(mcp_tool_diff_to_dto(value)).unwrap(), sample);
        }
    }

    #[test]
    fn mcp_proposal_approval_request_matches_the_client_protocol() {
        for sample in [
            r#"{"requestId": "s", "appId": "s", "workflowRunId": "s", "summary": "s", "proposalSha256": "s", "approvalContractSha256": "s", "toolSurfaceSha256": "s", "toolDiffs": [{"kind": "added", "name": "s", "before": {"name": "s", "title": "s", "description": "s", "inputSchemaJson": "s", "outputSchemaJson": "s", "annotationsJson": "s", "executionJson": "s", "visibleMetaJson": "s", "semanticFlowJson": "s", "permissionCeiling": "s"}, "after": {"name": "s", "title": "s", "description": "s", "inputSchemaJson": "s", "outputSchemaJson": "s", "annotationsJson": "s", "executionJson": "s", "visibleMetaJson": "s", "semanticFlowJson": "s", "permissionCeiling": "s"}, "changedFields": ["name"]}], "requiredFlowChanges": ["s"], "excludedCapabilities": ["s"], "pendingGates": [{"gateId": "s", "label": "s", "status": "pending", "available": true, "detail": "s"}]}"#,
            r#"{"requestId": "s", "appId": "s", "workflowRunId": "s", "summary": "s", "proposalSha256": "s", "approvalContractSha256": "s", "toolSurfaceSha256": "s"}"#,
        ] {
            let sample: serde_json::Value = serde_json::from_str(sample).unwrap();
            let value: McpProposalApprovalRequest = serde_json::from_value(sample.clone()).unwrap();
            assert_eq!(to_value(&value).unwrap(), sample);
            assert_eq!(
                to_value(mcp_proposal_approval_request_to_dto(value)).unwrap(),
                sample
            );
        }
    }

    #[test]
    fn agent_profile_proposal_matches_the_client_protocol() {
        for sample in [
            r#"{"appId": "s", "approvalToken": "s", "baseRevision": 1, "currentRevision": 1, "instructions": "s", "reason": "s"}"#,
            r#"{"appId": "s", "approvalToken": "s", "baseRevision": 1, "currentRevision": 1, "instructions": "s", "reason": "s"}"#,
        ] {
            let sample: serde_json::Value = serde_json::from_str(sample).unwrap();
            let value: AgentProfileProposal = serde_json::from_value(sample.clone()).unwrap();
            assert_eq!(to_value(&value).unwrap(), sample);
            assert_eq!(
                to_value(agent_profile_proposal_to_dto(value)).unwrap(),
                sample
            );
        }
    }

    #[test]
    fn mcp_app_widget_matches_the_client_protocol() {
        for sample in [
            r#"{"resourceUri": "s", "mimeType": "s", "resourceSha256": "s"}"#,
            r#"{"resourceUri": "s", "mimeType": "s", "resourceSha256": "s"}"#,
        ] {
            let sample: serde_json::Value = serde_json::from_str(sample).unwrap();
            let value: McpAppWidget = serde_json::from_value(sample.clone()).unwrap();
            assert_eq!(to_value(&value).unwrap(), sample);
            assert_eq!(to_value(mcp_app_widget_to_dto(value)).unwrap(), sample);
        }
    }

    #[test]
    fn managed_mcp_status_matches_the_client_protocol_for_every_variant() {
        for name in [
            "disabled",
            "needs_setup",
            "authoring",
            "enabled",
            "needs_revalidation",
            "error",
        ] {
            let sample = serde_json::Value::String(name.to_string());
            let value: ManagedMcpStatus = serde_json::from_value(sample.clone()).expect(name);
            let dto = managed_mcp_status_to_dto(value);
            assert_eq!(to_value(dto).unwrap(), sample, "{name}");
            assert_eq!(managed_mcp_status_from_dto(dto), Some(value));
        }
    }

    #[test]
    fn managed_mcp_server_matches_the_client_protocol() {
        for sample in [
            r#"{"serverName": "s", "appId": "s", "appName": "s", "enabled": true, "status": "disabled", "settingsRevision": 1, "enabledTools": ["s"], "pinnedToCurrentConversation": true, "buildId": "s", "catalogSha256": "s", "toolSurfaceSha256": "s", "toolCount": 1, "authoringRevision": 1, "publicationState": "draft", "mcpVerification": {"status": "pending", "summary": "s", "code": "s"}, "uiVerification": {"status": "pending", "summary": "s", "code": "s"}, "widget": {"resourceUri": "s", "mimeType": "s", "resourceSha256": "s"}, "tools": [{"name": "s", "title": "s", "description": "s", "inputSchemaJson": "s", "outputSchemaJson": "s", "annotationsJson": "s", "executionJson": "s", "visibleMetaJson": "s", "semanticFlowJson": "s", "permissionCeiling": "s"}]}"#,
            r#"{"serverName": "s", "appId": "s", "appName": "s", "enabled": true, "status": "disabled", "settingsRevision": 1, "pinnedToCurrentConversation": true, "buildId": "s", "catalogSha256": "s", "toolSurfaceSha256": "s", "toolCount": 1, "authoringRevision": 1, "publicationState": "draft", "mcpVerification": {"status": "pending", "summary": "s"}, "uiVerification": {"status": "pending", "summary": "s"}}"#,
        ] {
            let sample: serde_json::Value = serde_json::from_str(sample).unwrap();
            let value: ManagedMcpServer = serde_json::from_value(sample.clone()).unwrap();
            assert_eq!(to_value(&value).unwrap(), sample);
            assert_eq!(to_value(managed_mcp_server_to_dto(value)).unwrap(), sample);
        }
    }

    #[test]
    fn operation_error_code_matches_the_client_protocol_for_every_variant() {
        for name in [
            "plugin_disabled",
            "builtin_bundle_unavailable",
            "template_unavailable",
            "proposal_invalid",
            "catalog_stale",
            "active_state_corrupt",
            "revision_conflict",
            "invalid_mcp_settings",
            "mcp_authoring_required",
            "repair_budget_exhausted",
            "exposure_capacity_reached",
        ] {
            let sample = serde_json::Value::String(name.to_string());
            let value: PluginErrorCode = serde_json::from_value(sample.clone()).expect(name);
            let dto = plugin_error_code_to_dto(value);
            assert_eq!(to_value(dto).unwrap(), sample, "{name}");
            assert_eq!(plugin_error_code_from_dto(dto), Some(value));
        }
    }
}
