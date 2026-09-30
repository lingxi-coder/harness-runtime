//! Regression tests for audio contract test.

use client::protocol::audio::{
    AudioCapabilitySnapshotDto, AudioErrorDto, AudioErrorKindDto, AudioInitiatorDto,
    AudioOperationDto, AudioOperationIdDto, AudioOperationKindDto, AudioOperationReadinessDto,
    AudioOperationRequestDto, AudioOperationResultDto, AudioOwnerDto, AudioReadinessStateDto,
    AudioStatusDto,
};
use serde_json::json;

#[test]
fn audio_request_uses_stable_owner_and_skew_safe_timeout_budget() {
    let request = AudioOperationRequestDto {
        identity: AudioOperationIdDto {
            id: "00000000-0000-4000-8000-000000000001".into(),
            generation: 4,
            service_epoch: 7,
        },
        owner: AudioOwnerDto::Session {
            session_id: "session-1".into(),
        },
        initiator: Some(AudioInitiatorDto {
            agent_id: Some("agent-2".into()),
            tool_use_id: Some("tool-3".into()),
            request_id: None,
        }),
        timeout_budget_ms: Some(30_000),
        max_payload_bytes: 12_533_760,
        operation: AudioOperationDto::Listen { language: None },
    };

    let value = serde_json::to_value(&request).unwrap();
    assert_eq!(
        value["identity"]["id"],
        "00000000-0000-4000-8000-000000000001"
    );
    assert_eq!(value["identity"]["service_epoch"], 7);
    assert_eq!(
        value["owner"],
        json!({"type":"session","session_id":"session-1"})
    );
    assert_eq!(value["operation"], json!({"type":"listen"}));
    assert_eq!(value["timeout_budget_ms"], 30_000);
    assert_eq!(value["max_payload_bytes"], 12_533_760);
    assert_eq!(value["initiator"]["tool_use_id"], "tool-3");
    assert!(value["initiator"].get("request_id").is_none());
}

#[test]
fn audio_result_preserves_structured_failure_and_playback_completion() {
    let failed = AudioOperationResultDto::Failed {
        error: AudioErrorDto {
            kind: AudioErrorKindDto::MediaTooLarge,
            message: "audio exceeds the device transfer limit".into(),
        },
    };
    assert_eq!(
        serde_json::to_value(failed).unwrap(),
        json!({
            "type":"failed",
            "error":{"kind":"media_too_large","message":"audio exceeds the device transfer limit"}
        })
    );

    let completed = AudioOperationResultDto::PlaybackCompleted { duration_ms: 412 };
    assert_eq!(
        serde_json::to_value(completed).unwrap(),
        json!({"type":"playback_completed","duration_ms":412})
    );
}

#[test]
fn audio_status_and_owner_teardown_are_wire_operations() {
    for (operation, expected) in [
        (
            AudioOperationDto::Status {
                handle: Some("recording-1".into()),
            },
            json!({"type":"status","handle":"recording-1"}),
        ),
        (AudioOperationDto::EndOwner, json!({"type":"end_owner"})),
    ] {
        assert_eq!(serde_json::to_value(operation).unwrap(), expected);
    }

    assert_eq!(
        serde_json::to_value(AudioOperationResultDto::Status {
            status: AudioStatusDto {
                recording: true,
                playing: false,
            },
        })
        .unwrap(),
        json!({"type":"status","status":{"recording":true,"playing":false}})
    );
}

#[test]
fn supported_operations_are_independent_of_transient_readiness() {
    let capabilities = AudioCapabilitySnapshotDto {
        service_epoch: 9,
        support_revision: 3,
        supported_operations: vec![AudioOperationKindDto::Listen, AudioOperationKindDto::Speak],
        readiness: vec![AudioOperationReadinessDto {
            operation: AudioOperationKindDto::Listen,
            state: AudioReadinessStateDto::NeedsPermission,
        }],
        max_payload_bytes: 12_533_760,
    };

    let value = serde_json::to_value(capabilities).unwrap();
    assert_eq!(value["service_epoch"], 9);
    assert_eq!(value["supported_operations"], json!(["listen", "speak"]));
    assert_eq!(value["readiness"][0]["state"], "needs_permission");
}
