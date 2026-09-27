//! `computer` tool `request_access` DTO round-trip + exact-wire-shape tests.
//!
//! Mirrors `tests/permission_test.rs`: each type gets a serialize →
//! assert-shape → deserialize → assert-eq round-trip. Additionally, since these
//! DTOs are a NEW wire surface shared with a TypeScript track, several tests
//! assert the serialized JSON is BYTE-IDENTICAL to the frozen example strings
//! from the wire contract (not just structurally equivalent) — this is the
//! strongest guarantee the Rust and TypeScript sides agree on field order, tag
//! spelling, and the `tcc_state`-omitted-when-absent convention.

use client_protocol::commands::ClientCommand;
use client_protocol::computer_access::{
    AccessTierDto, ComputerAccessRequestDto, ComputerAccessResponseDto, RequestedAppDto,
    TccStateDto,
};

/// `AccessTierDto` is a bare wire STRING (not internally tagged like this
/// crate's other enums) — byte-identical to
/// `permission::computer_access::AccessTier::as_str()`.
#[test]
fn access_tier_serializes_as_bare_string() {
    let cases = [
        (AccessTierDto::Read, "\"read\""),
        (AccessTierDto::Click, "\"click\""),
        (AccessTierDto::Full, "\"full\""),
    ];
    for (tier, expected) in cases {
        let json = serde_json::to_string(&tier).expect("serialize AccessTierDto");
        assert_eq!(json, expected, "AccessTierDto::{tier:?} wire form");
        let back: AccessTierDto = serde_json::from_str(&json).expect("deserialize AccessTierDto");
        assert_eq!(back, tier);
    }
}

/// `ComputerAccessRequestDto` with `tcc_state` PRESENT — the exact wire
/// contract example (`request_id: 42, "automate chat"`, Slack+Chrome, tier
/// full, TCC accessibility granted / screen recording missing).
#[test]
fn request_with_tcc_state_matches_exact_wire_shape() {
    let req = ComputerAccessRequestDto {
        request_id: 42,
        reason: "automate chat".to_string(),
        apps: vec![
            RequestedAppDto {
                label: "Slack".to_string(),
            },
            RequestedAppDto {
                label: "Chrome".to_string(),
            },
        ],
        tier: AccessTierDto::Full,
        clipboard_read: false,
        clipboard_write: false,
        system_key_combos: false,
        tcc_state: Some(TccStateDto {
            accessibility: true,
            screen_recording: false,
        }),
    };
    let json = serde_json::to_string(&req).expect("serialize ComputerAccessRequestDto");
    assert_eq!(
        json,
        r#"{"request_id":42,"reason":"automate chat","apps":[{"label":"Slack"},{"label":"Chrome"}],"tier":"full","clipboard_read":false,"clipboard_write":false,"system_key_combos":false,"tcc_state":{"accessibility":true,"screen_recording":false}}"#
    );
    let back: ComputerAccessRequestDto =
        serde_json::from_str(&json).expect("deserialize ComputerAccessRequestDto");
    assert_eq!(back, req);
}

/// `ComputerAccessRequestDto` with `tcc_state` ABSENT — the key is OMITTED
/// entirely from the wire (not `null`), matching this crate's
/// `#[serde(skip_serializing_if = "Option::is_none")]` convention.
#[test]
fn request_without_tcc_state_omits_the_key() {
    let req = ComputerAccessRequestDto {
        request_id: 7,
        reason: "read the clipboard".to_string(),
        apps: vec![RequestedAppDto {
            label: "Notes".to_string(),
        }],
        tier: AccessTierDto::Read,
        clipboard_read: true,
        clipboard_write: false,
        system_key_combos: false,
        tcc_state: None,
    };
    let json = serde_json::to_string(&req).expect("serialize ComputerAccessRequestDto");
    assert_eq!(
        json,
        r#"{"request_id":7,"reason":"read the clipboard","apps":[{"label":"Notes"}],"tier":"read","clipboard_read":true,"clipboard_write":false,"system_key_combos":false}"#
    );
    assert!(
        !json.contains("tcc_state"),
        "an absent tcc_state must be OMITTED, not serialized as null"
    );
    let back: ComputerAccessRequestDto =
        serde_json::from_str(&json).expect("deserialize ComputerAccessRequestDto");
    assert_eq!(back, req);

    // A frame omitting `tcc_state` entirely still deserializes (the `#[serde(default)]`
    // half of the convention), independent of what THIS instance serialized.
    let minimal = r#"{"request_id":1,"reason":"r","apps":[],"tier":"click","clipboard_read":false,"clipboard_write":false,"system_key_combos":false}"#;
    let from_minimal: ComputerAccessRequestDto =
        serde_json::from_str(minimal).expect("deserialize without tcc_state key");
    assert_eq!(from_minimal.tcc_state, None);
}

/// `ComputerAccessResponseDto` — the inbound grant. `Default` is fully denied
/// (mirrors the source `permission::computer_access::ComputerAccessResponse`).
#[test]
fn response_default_is_fully_denied_and_round_trips() {
    let denied = ComputerAccessResponseDto::default();
    assert!(denied.granted_apps.is_empty());
    assert!(!denied.clipboard_read);
    assert!(!denied.clipboard_write);
    assert!(!denied.system_key_combos);

    let granted = ComputerAccessResponseDto {
        granted_apps: vec!["Slack".to_string()],
        clipboard_read: false,
        clipboard_write: false,
        system_key_combos: false,
    };
    let json = serde_json::to_string(&granted).expect("serialize ComputerAccessResponseDto");
    assert_eq!(
        json,
        r#"{"granted_apps":["Slack"],"clipboard_read":false,"clipboard_write":false,"system_key_combos":false}"#
    );
    let back: ComputerAccessResponseDto =
        serde_json::from_str(&json).expect("deserialize ComputerAccessResponseDto");
    assert_eq!(back, granted);
}

/// `ClientCommand::ApproveComputerAccess` — the exact wire contract example.
#[test]
fn approve_computer_access_matches_exact_wire_shape() {
    let cmd = ClientCommand::ApproveComputerAccess {
        request_id: 42,
        response: ComputerAccessResponseDto {
            granted_apps: vec!["Slack".to_string()],
            clipboard_read: false,
            clipboard_write: false,
            system_key_combos: false,
        },
    };
    let json = serde_json::to_string(&cmd).expect("serialize ApproveComputerAccess");
    assert_eq!(
        json,
        r#"{"type":"approve_computer_access","request_id":42,"response":{"granted_apps":["Slack"],"clipboard_read":false,"clipboard_write":false,"system_key_combos":false}}"#
    );
    let back: ClientCommand =
        serde_json::from_str(&json).expect("deserialize ApproveComputerAccess");
    assert_eq!(back, cmd);
}

/// `ClientCommand::DenyComputerAccess` — the exact wire contract example.
#[test]
fn deny_computer_access_matches_exact_wire_shape() {
    let cmd = ClientCommand::DenyComputerAccess { request_id: 42 };
    let json = serde_json::to_string(&cmd).expect("serialize DenyComputerAccess");
    assert_eq!(json, r#"{"type":"deny_computer_access","request_id":42}"#);
    let back: ClientCommand = serde_json::from_str(&json).expect("deserialize DenyComputerAccess");
    assert_eq!(back, cmd);
}
