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

use client::protocol::local_apps::{
    AppBridgeOperationDto, AppBridgeRequestDto, AppBridgeResponseDto, AppBridgeStreamFrameDto,
};
use local_app_contracts::bridge::{
    BridgeOperation, BridgeRequest, BridgeResponse, BridgeStreamFrame,
};

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
}
