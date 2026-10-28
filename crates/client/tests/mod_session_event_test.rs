//! Session-owned Mod output uses the shared, lossless native protocol.

use client::protocol::events::ClientEvent;
use client::protocol::mod_session_event::ModSessionEventPayload;
use client::protocol::text::ClientTextDto;
use client::turn_journal::decode_turn_journal_event;

#[test]
fn all_mod_payloads_round_trip_exact_utf16_and_original_identity() {
    let text = ClientTextDto::from_code_units(vec![0x4f60, 0xd83d, 0xde80, 0xd800, 0x0061, 0xdfff]);
    let payloads = [
        ModSessionEventPayload::Notice {
            message: text.clone(),
            is_error: true,
        },
        ModSessionEventPayload::Log {
            plugin: "review".into(),
            text: text.clone(),
        },
        ModSessionEventPayload::Toast {
            plugin: "review".into(),
            text: text.clone(),
            timeout_ms: 4_000,
        },
        ModSessionEventPayload::Status {
            plugin: "review".into(),
            text: Some(text.clone()),
        },
        ModSessionEventPayload::Notify {
            plugin: "review".into(),
            text,
        },
    ];
    for payload in payloads {
        let event = ClientEvent::ModSessionEvent {
            session_id: "session-a".into(),
            mod_turn_id: Some("mod-turn-1".into()),
            payload,
        };
        let wire = serde_json::to_string(&event).expect("serialize Mod event");
        assert!(wire.contains(r"\ud800"), "{wire}");
        assert!(wire.contains(r"\udfff"), "{wire}");
        let decoded: ClientEvent = serde_json::from_str(&wire).expect("decode Mod event");
        assert_eq!(decoded, event);
        assert!(decode_turn_journal_event(wire).is_err());
    }
}

#[test]
fn optional_fields_use_explicit_null_and_missing_fields_remain_readable() {
    let event = ClientEvent::ModSessionEvent {
        session_id: "session-a".into(),
        mod_turn_id: None,
        payload: ModSessionEventPayload::Status {
            plugin: "review".into(),
            text: None,
        },
    };
    let wire = serde_json::to_string(&event).unwrap();
    assert!(wire.contains(r#""mod_turn_id":null"#), "{wire}");
    assert!(wire.contains(r#""text":null"#), "{wire}");
    let decoded: ClientEvent = serde_json::from_str(&wire).unwrap();
    assert_eq!(decoded, event);

    let missing = r#"{"type":"mod_session_event","session_id":"session-a","payload":{"type":"status","plugin":"review"}}"#;
    assert_eq!(serde_json::from_str::<ClientEvent>(missing).unwrap(), event);
}

#[test]
fn unknown_or_malformed_mod_payload_is_rejected() {
    for wire in [
        r#"{"type":"mod_session_event","session_id":"s","mod_turn_id":null,"payload":{"type":"future"}}"#,
        r#"{"type":"mod_session_event","session_id":"s","mod_turn_id":null,"payload":{"type":"notice","is_error":false}}"#,
        r#"{"type":"mod_session_event","session_id":"s","mod_turn_id":null,"payload":{"type":"log","plugin":"p","text":7}}"#,
        r#"{"type":"mod_session_event","session_id":"s","mod_turn_id":null,"payload":{"type":"status","plugin":"p","text":7}}"#,
        r#"{"type":"mod_session_event","session_id":"s","mod_turn_id":null,"payload":{"type":"toast","plugin":"p","text":"ok","timeout_ms":-1}}"#,
    ] {
        assert!(serde_json::from_str::<ClientEvent>(wire).is_err(), "{wire}");
    }
}
