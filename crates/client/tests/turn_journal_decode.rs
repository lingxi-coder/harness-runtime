//! Mobile turn-envelope decoding preserves the real protocol payload and owner.

use client::protocol::events::{ClientEvent, TurnRecoverySnapshotDto, TurnRecoveryStateDto};
use client::protocol::message::{
    LoopWakeupDto, MessageBlockDto, MessageDto, MessageImageDto, VisualizationContextDto,
    VisualizationRefDto,
};
use client::protocol::text::ClientTextDto;
use client::protocol::tool_display::{ToolHeaderDto, ToolResultDisplayDto, ToolVerbDto};
use client::turn_journal::{decode_turn_event_envelope, decode_turn_journal_event};

fn complete_message() -> ClientEvent {
    ClientEvent::MessageComplete {
        stop_reason: Some("end_turn".into()),
        message: Some(MessageDto {
            transcript_uuid: Some("message-1".into()),
            role: "assistant".into(),
            blocks: vec![
                MessageBlockDto::Text {
                    text: ClientTextDto::from_code_units(vec![
                        0x4f60, 0x597d, 0xd83d, 0xde80, 0xd800, 0x61, 0xdc00,
                    ]),
                },
                MessageBlockDto::Thinking {
                    thinking: "reasoning".into(),
                    signature: Some("signature".into()),
                },
                MessageBlockDto::RedactedThinking {
                    data: "redacted".into(),
                },
                MessageBlockDto::CompactBoundary {
                    messages_before: 12,
                    messages_after: 3,
                    summary: "summary".into(),
                },
                MessageBlockDto::ToolUse {
                    id: "call-1".into(),
                    tool: "Read".into(),
                    input_json: r#"{"path":"/tmp/a"}"#.into(),
                    header: ToolHeaderDto {
                        verb: ToolVerbDto::Read,
                        icon: None,
                        label: "Read".into(),
                        primary: Some("/tmp/a".into()),
                        qualifier: None,
                        count: None,
                        sub_line: None,
                        title: "Read(/tmp/a)".into(),
                    },
                },
                MessageBlockDto::ToolResult {
                    id: "call-1".into(),
                    tool: "Read".into(),
                    result_json: r#"{"ok":true}"#.into(),
                    is_error: false,
                    display: ToolResultDisplayDto {
                        headline: None,
                        headline_kind: None,
                        headline_args: Vec::new(),
                        diff: None,
                        body: None,
                        body_lines: 0,
                        body_truncated: false,
                        collapsed: false,
                    },
                },
                MessageBlockDto::Visualization {
                    reference: Some(VisualizationRefDto {
                        id: "visual-1".into(),
                        revision: 2,
                    }),
                },
            ],
            images: vec![MessageImageDto {
                media_type: "image/png".into(),
                url: "data:image/png;base64,AA==".into(),
            }],
            loop_wakeup: Some(LoopWakeupDto {
                message: "wake".into(),
                companion: Some("quiet".into()),
                streak: 2,
                since_ms: 42,
            }),
            visualization_context: Some(VisualizationContextDto {
                id: "visual-1".into(),
                revision: 2,
                title: "Chart".into(),
            }),
            sent_at_ms: Some(123_456),
        }),
    }
}

#[test]
fn real_message_complete_keeps_all_blocks_metadata_and_exact_utf16() {
    let event = complete_message();
    let json = serde_json::to_string(&event).expect("serialize the actual journal wire form");
    assert!(json.contains(r"\ud800"), "{json}");
    assert!(json.contains(r"\udc00"), "{json}");

    let decoded = decode_turn_journal_event(json).expect("decode a complete message");
    assert_eq!(decoded, event);
    let ClientEvent::MessageComplete {
        message: Some(message),
        ..
    } = decoded
    else {
        panic!("expected complete message");
    };
    let MessageBlockDto::Text { text } = &message.blocks[0] else {
        panic!("expected text block");
    };
    assert_eq!(
        text.code_units,
        vec![0x4f60, 0x597d, 0xd83d, 0xde80, 0xd800, 0x61, 0xdc00]
    );
}

#[test]
fn legacy_optional_fields_and_direct_unicode_are_decoded_by_the_same_protocol() {
    let json = r#"{"type":"message_complete","message":{"role":"assistant","blocks":[{"type":"text","text":"你好🚀\ud800"}]}}"#;
    let ClientEvent::MessageComplete {
        stop_reason,
        message: Some(message),
    } = decode_turn_journal_event(json.into()).expect("decode legacy message")
    else {
        panic!("expected complete message");
    };
    assert_eq!(stop_reason, None);
    assert!(message.images.is_empty());
    assert!(message.loop_wakeup.is_none());
    assert!(message.visualization_context.is_none());
    assert!(message.sent_at_ms.is_none());
    let MessageBlockDto::Text { text } = &message.blocks[0] else {
        panic!("expected text block");
    };
    assert_eq!(
        text.code_units,
        vec![0x4f60, 0x597d, 0xd83d, 0xde80, 0xd800]
    );
}

#[test]
fn invalid_unknown_and_nested_delivery_events_are_rejected() {
    for raw in [
        "",
        "{",
        r#"{"type":"unknown_turn_event"}"#,
        r#"{"type":"text_delta"}"#,
        r#"{"type":"text_delta","text":17}"#,
        r#"{"type":"message_complete","message":{"role":"assistant","blocks":[{"type":"unknown"}]}}"#,
    ] {
        assert!(decode_turn_journal_event(raw.into()).is_err(), "{raw}");
    }

    let nested = [
        ClientEvent::TurnEventReplay {
            session_id: "session".into(),
            turn_id: 1,
            sequence: 1,
            event_json: r#"{"type":"text_delta","text":"x"}"#.into(),
        },
        ClientEvent::TurnEventLiveUnretained {
            session_id: "session".into(),
            turn_id: 1,
            event_json: r#"{"type":"text_delta","text":"x"}"#.into(),
        },
        ClientEvent::TurnRecoveryState {
            snapshot: TurnRecoverySnapshotDto {
                session_id: "session".into(),
                turn_id: 1,
                revision: 1,
                state: TurnRecoveryStateDto::Running,
                first_sequence: 0,
                last_sequence: 0,
                safe_to_resume: true,
                projection_uncertain: false,
                reason: None,
            },
        },
        ClientEvent::TurnAttachStarted {
            request_id: "attempt".into(),
            snapshot: TurnRecoverySnapshotDto {
                session_id: "session".into(),
                turn_id: 1,
                revision: 1,
                state: TurnRecoveryStateDto::Running,
                first_sequence: 0,
                last_sequence: 0,
                safe_to_resume: true,
                projection_uncertain: false,
                reason: None,
            },
        },
        ClientEvent::TurnAttachFinished {
            request_id: "attempt".into(),
            session_id: "session".into(),
            turn_id: 1,
        },
        ClientEvent::QueuedTurnStatus {
            session_id: "session".into(),
            turn_id: 1,
            request_id: "attempt".into(),
            status: client::protocol::events::QueuedTurnStatusDto::Pending,
        },
    ];
    for event in nested {
        let json = serde_json::to_string(&event).expect("encode nested event");
        assert!(decode_turn_journal_event(json).is_err());
    }
}

#[test]
fn envelope_keeps_owner_and_sequence_even_for_identical_payloads() {
    let json = serde_json::to_string(&complete_message()).unwrap();
    let first = decode_turn_event_envelope(ClientEvent::TurnEventReplay {
        session_id: "session-a".into(),
        turn_id: 7,
        sequence: 3,
        event_json: json.clone(),
    })
    .unwrap();
    let second = decode_turn_event_envelope(ClientEvent::TurnEventReplay {
        session_id: "session-b".into(),
        turn_id: 7,
        sequence: 9,
        event_json: json.clone(),
    })
    .unwrap();
    assert_eq!(first.event, second.event);
    assert_eq!(
        (first.session_id.as_str(), first.turn_id, first.sequence),
        ("session-a", 7, Some(3))
    );
    assert_eq!(
        (second.session_id.as_str(), second.turn_id, second.sequence),
        ("session-b", 7, Some(9))
    );

    let unretained = ClientEvent::TurnEventLiveUnretained {
        session_id: "session-b".into(),
        turn_id: 8,
        event_json: json,
    };
    let wire = serde_json::to_string(&unretained).unwrap();
    let back: ClientEvent = serde_json::from_str(&wire).unwrap();
    assert_eq!(back, unretained);
    let decoded = decode_turn_event_envelope(back).unwrap();
    assert_eq!(
        (
            decoded.session_id.as_str(),
            decoded.turn_id,
            decoded.sequence
        ),
        ("session-b", 8, None)
    );
    assert_eq!(decoded.event, first.event);
}

#[test]
fn envelope_rejects_missing_identity_zero_sequence_and_non_envelopes() {
    for envelope in [
        ClientEvent::TurnEventReplay {
            session_id: "".into(),
            turn_id: 1,
            sequence: 1,
            event_json: r#"{"type":"text_delta","text":"x"}"#.into(),
        },
        ClientEvent::TurnEventReplay {
            session_id: "session".into(),
            turn_id: 1,
            sequence: 0,
            event_json: r#"{"type":"text_delta","text":"x"}"#.into(),
        },
        ClientEvent::TextDelta { text: "x".into() },
    ] {
        assert!(decode_turn_event_envelope(envelope).is_err());
    }
}
