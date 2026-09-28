//! M8-P13 — serde round-trip + JSON-shape locks for the bridge wire types.
//! Catches accidental rename / field-reorder before M9's wire code lands.

#![allow(clippy::unwrap_used)]

use bridge::{
    AuthChallenge, AuthResponse, BridgeRequest, BridgeResponse, BridgeWireError, Capabilities,
    ClientHello, Frame, ServerHello, BRIDGE_PROTOCOL_VERSION,
};
use client::protocol::commands::ClientCommand;
use client::protocol::computer_access::{
    AccessTierDto, ComputerAccessRequestDto, ComputerAccessResponseDto, RequestedAppDto,
};
use client::protocol::events::{ClientEvent, ErrorKindDto};
use client::protocol::permission::{PermissionKindDto, PermissionRequest};
use client::protocol::version::CLIENT_PROTOCOL_VERSION;

fn roundtrip<T>(v: &T) -> T
where
    T: serde::Serialize + serde::de::DeserializeOwned,
{
    let json = serde_json::to_string(v).unwrap();
    serde_json::from_str(&json).unwrap()
}

#[test]
fn capabilities_default_shape() {
    let c = Capabilities::default();
    let json = serde_json::to_string(&c).unwrap();
    assert_eq!(
        json,
        // Key ORDER is what this locks; the version tracks the constant so a
        // reviewed §0.10 bump cannot break an unrelated shape assertion.
        format!(
            r#"{{"supports_streaming":true,"supports_tools":true,"supports_skills":true,"supports_commands":true,"client_protocol_version":"{CLIENT_PROTOCOL_VERSION}"}}"#
        )
    );
}

#[test]
fn client_hello_roundtrip() {
    let h = ClientHello {
        protocol_version: "0.1.0".into(),
        client_name: "lingxi-ios/0.9.0".into(),
        capabilities: Capabilities::default(),
    };
    assert_eq!(roundtrip(&h), h);
}

#[test]
fn server_hello_roundtrip() {
    let h = ServerHello {
        protocol_version: "0.1.0".into(),
        server_name: "lingxi-engine-desktop/0.9.0".into(),
        capabilities: Capabilities::default(),
    };
    assert_eq!(roundtrip(&h), h);
}

#[test]
fn bridge_request_roundtrip() {
    let r = BridgeRequest {
        id: 7,
        method: "run_turn".into(),
        params: serde_json::json!({ "text": "hi" }),
    };
    assert_eq!(roundtrip(&r), r);
}

#[test]
fn bridge_response_roundtrip() {
    let ok = BridgeResponse {
        id: 7,
        result: Some(serde_json::json!({ "ok": true })),
        error: None,
    };
    assert_eq!(roundtrip(&ok), ok);
    let err = BridgeResponse {
        id: 8,
        result: None,
        error: Some(BridgeWireError {
            code: -32000,
            message: "boom".into(),
        }),
    };
    assert_eq!(roundtrip(&err), err);
}

#[test]
fn auth_roundtrip() {
    let c = AuthChallenge {
        nonce: "abc".into(),
    };
    assert_eq!(roundtrip(&c), c);
    let r = AuthResponse {
        token: "xyz".into(),
    };
    assert_eq!(roundtrip(&r), r);
}

// ── F2-02: wire v0.2 — tagged `Frame` + version carry ────────────────────────

#[test]
fn bridge_protocol_version_is_0_2_0() {
    assert_eq!(BRIDGE_PROTOCOL_VERSION, "0.2.0");
}

#[test]
fn frame_request_round_trips() {
    let f = Frame::Request(BridgeRequest {
        id: 11,
        method: "submit".into(),
        params: serde_json::json!({ "type": "send_prompt", "text": "hi" }),
    });
    assert_eq!(roundtrip(&f), f);
    // Externally tagged on `type`, snake_case.
    let json = serde_json::to_string(&f).unwrap();
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["type"], "request");
}

#[test]
fn frame_response_round_trips() {
    let ok = Frame::Response(BridgeResponse {
        id: 11,
        result: Some(serde_json::json!({ "ok": true })),
        error: None,
    });
    assert_eq!(roundtrip(&ok), ok);
    let err = Frame::Response(BridgeResponse {
        id: 12,
        result: None,
        error: Some(BridgeWireError {
            code: -32000,
            message: "boom".into(),
        }),
    });
    assert_eq!(roundtrip(&err), err);
    let json = serde_json::to_string(&ok).unwrap();
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["type"], "response");
}

#[test]
fn frame_event_round_trips() {
    // Events carry NO id — they are unsolicited server pushes.
    let f = Frame::Event(ClientEvent::TextDelta {
        text: "hello".into(),
    });
    assert_eq!(roundtrip(&f), f);
    let json = serde_json::to_string(&f).unwrap();
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["type"], "event");
    assert!(
        v.get("id").is_none(),
        "events must not carry a correlation id"
    );
}

#[test]
fn frame_event_carries_error_client_event() {
    let f = Frame::Event(ClientEvent::Error {
        kind: ErrorKindDto::Transport,
        message: "lost connection".into(),
    });
    assert_eq!(roundtrip(&f), f);
}

#[test]
fn frame_permission_request_round_trips() {
    // F2-06: `PermissionRequest` is NOT a `ClientEvent` variant (it is a
    // standalone frozen DTO in `client::protocol::permission`), so it cannot ride
    // on `Frame::Event(ClientEvent)`. The bridge wire carries it on its OWN
    // additive `Frame::PermissionRequest` arm (the `Frame` enum is
    // `#[non_exhaustive]`, so this is additive — no `client-protocol` snapshot
    // change). Like `Frame::Event` it carries NO correlation `id`: it is an
    // unsolicited server push that the client answers with a SEPARATE
    // `Frame::Request(ApprovePermission/DenyPermission)` correlated by
    // `request_id`.
    let f = Frame::PermissionRequest(PermissionRequest {
        request_id: 42,
        kind: PermissionKindDto::ToolUseConfirm {
            tool_name: "Bash".into(),
            tool_input_json: r#"{"command":"ls"}"#.into(),
            default_allow: false,
        },
        worker: None,
        owner: None,
        suppress_always_allow_rule: false,
        auto_mode_prompt: None,
    });
    assert_eq!(roundtrip(&f), f);
    let json = serde_json::to_string(&f).unwrap();
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["type"], "permission_request");
    assert!(
        v.get("id").is_none(),
        "permission requests are unsolicited pushes — no correlation id"
    );
    // The inner request_id is the correlator (echoed by the inbound
    // ApprovePermission/DenyPermission command), carried under `payload`.
    assert_eq!(v["payload"]["request_id"], 42);
}

#[test]
fn frame_computer_access_request_round_trips() {
    // Mirrors `frame_permission_request_round_trips`: the `computer` tool's
    // `request_access` prompt is a SEPARATE additive `Frame` arm (no `id`,
    // no own `type` tag on the inner DTO — the tag lives on `Frame`).
    let f = Frame::ComputerAccessRequest(ComputerAccessRequestDto {
        request_id: 42,
        reason: "automate chat".into(),
        apps: vec![RequestedAppDto {
            label: "Slack".into(),
        }],
        tier: AccessTierDto::Full,
        clipboard_read: false,
        clipboard_write: false,
        system_key_combos: false,
        tcc_state: None,
    });
    assert_eq!(roundtrip(&f), f);
    let json = serde_json::to_string(&f).unwrap();
    assert_eq!(
        json,
        r#"{"type":"computer_access_request","payload":{"request_id":42,"reason":"automate chat","apps":[{"label":"Slack"}],"tier":"full","clipboard_read":false,"clipboard_write":false,"system_key_combos":false}}"#,
        "the computer_access_request wire envelope must match the frozen wire contract example byte-for-byte"
    );
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["type"], "computer_access_request");
    assert!(
        v.get("id").is_none(),
        "computer-access requests are unsolicited pushes — no correlation id"
    );
    assert_eq!(v["payload"]["request_id"], 42);
}

#[test]
fn approve_and_deny_computer_access_commands_match_exact_wire_shape() {
    let approve = ClientCommand::ApproveComputerAccess {
        request_id: 42,
        response: ComputerAccessResponseDto {
            granted_apps: vec!["Slack".to_string()],
            clipboard_read: false,
            clipboard_write: false,
            system_key_combos: false,
        },
    };
    assert_eq!(roundtrip(&approve), approve);
    let json = serde_json::to_string(&approve).unwrap();
    assert_eq!(
        json,
        r#"{"type":"approve_computer_access","request_id":42,"response":{"granted_apps":["Slack"],"clipboard_read":false,"clipboard_write":false,"system_key_combos":false}}"#
    );

    let deny = ClientCommand::DenyComputerAccess { request_id: 42 };
    assert_eq!(roundtrip(&deny), deny);
    let json = serde_json::to_string(&deny).unwrap();
    assert_eq!(json, r#"{"type":"deny_computer_access","request_id":42}"#);

    // Both travel as a client→server `Frame::Request` (a `BridgeRequest` whose
    // `params` is the serialized `ClientCommand`), the SAME envelope
    // `ApprovePermission`/`DenyPermission` use.
    let framed = Frame::Request(BridgeRequest {
        id: 1,
        method: "submit".into(),
        params: serde_json::to_value(&approve).unwrap(),
    });
    assert_eq!(roundtrip(&framed), framed);
}

#[test]
fn capabilities_carry_client_protocol_version() {
    let c = Capabilities::default();
    assert_eq!(c.client_protocol_version, CLIENT_PROTOCOL_VERSION);
    assert_eq!(roundtrip(&c), c);
}
