use client::protocol::events::ClientEvent;
use client::protocol::message::MessageBlockDto;

#[test]
fn plain_text_events_deserialize_from_raw_and_value_inputs() {
    let raw = r#"{"type":"system_notice","message":"saved","is_error":true}"#;
    let from_raw: ClientEvent = serde_json::from_str(raw).expect("deserialize raw event JSON");
    let from_value: ClientEvent =
        serde_json::from_value(serde_json::from_str(raw).expect("parse ordinary JSON value"))
            .expect("deserialize event through Value");
    assert_eq!(from_raw, from_value);
    assert!(matches!(
        from_raw,
        ClientEvent::SystemNotice {
            message,
            is_error: true,
            ..
        }
            if message.code_units == "saved".encode_utf16().collect::<Vec<_>>()
    ));
}

#[test]
fn system_notice_preserves_a_lone_surrogate_through_event_roundtrip() {
    let raw = r#"{"type":"system_notice","message":"left\ud800right","is_error":false}"#;
    let event: ClientEvent = serde_json::from_str(raw).expect("deserialize exact event JSON");
    let ClientEvent::SystemNotice { message, .. } = &event else {
        panic!("expected SystemNotice");
    };
    let mut expected = "left".encode_utf16().collect::<Vec<_>>();
    expected.push(0xd800);
    expected.extend("right".encode_utf16());
    assert_eq!(message.code_units, expected);

    let encoded = serde_json::to_string(&event).expect("serialize event");
    assert!(
        encoded.contains(r#""message":"left\ud800right""#),
        "{encoded}"
    );
    let decoded: ClientEvent = serde_json::from_str(&encoded).expect("round-trip event JSON");
    assert_eq!(decoded, event);
}

#[test]
fn message_block_preserves_lone_surrogate_through_message_complete_roundtrip() {
    let raw = r#"{"type":"message_complete","message":{"role":"assistant","blocks":[{"type":"text","text":"x\ud800y"}]}}"#;
    let event: ClientEvent = serde_json::from_str(raw).expect("deserialize nested block JSON");
    let ClientEvent::MessageComplete {
        message: Some(message),
        ..
    } = &event
    else {
        panic!("expected MessageComplete");
    };
    let MessageBlockDto::Text { text } = &message.blocks[0] else {
        panic!("expected text block");
    };
    assert_eq!(
        text.code_units,
        vec![u16::from(b'x'), 0xd800, u16::from(b'y')]
    );

    let encoded = serde_json::to_string(&event).expect("serialize nested event");
    assert!(encoded.contains(r#""text":"x\ud800y""#), "{encoded}");
    let decoded: ClientEvent = serde_json::from_str(&encoded).expect("round-trip nested JSON");
    assert_eq!(decoded, event);
}

#[test]
fn message_block_plain_text_still_deserializes_from_value() {
    let value = serde_json::json!({"type": "text", "text": "ordinary text"});
    let block: MessageBlockDto = serde_json::from_value(value).expect("deserialize text block");
    assert!(matches!(
        block,
        MessageBlockDto::Text { text }
            if text.code_units == "ordinary text".encode_utf16().collect::<Vec<_>>()
    ));
}

#[test]
fn flattened_session_agent_message_preserves_nested_lone_surrogate() {
    let raw = r#"{"type":"session_agent_message","session_id":"session","agent_id":"agent","message_index":0,"message_uuid":"row","message":{"role":"assistant","blocks":[{"type":"text","text":"a\ud800b"}]}}"#;
    let event: ClientEvent = serde_json::from_str(raw).expect("deserialize flattened event JSON");
    let ClientEvent::SessionAgentMessage { row, .. } = &event else {
        panic!("expected SessionAgentMessage");
    };
    let MessageBlockDto::Text { text } = &row.message.blocks[0] else {
        panic!("expected text block");
    };
    assert_eq!(
        text.code_units,
        vec![u16::from(b'a'), 0xd800, u16::from(b'b')]
    );

    let encoded = serde_json::to_string(&event).expect("serialize flattened event");
    assert!(encoded.contains(r#""text":"a\ud800b""#), "{encoded}");
    let decoded: ClientEvent = serde_json::from_str(&encoded).expect("round-trip flattened JSON");
    assert_eq!(decoded, event);
}
