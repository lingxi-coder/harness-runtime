//! Client protocol tests for exact UTF-16 text fields and their JSON roundtrips.

use client::protocol::events::ClientEvent;
use client::protocol::message::MessageBlockDto;
use client::protocol::{commands::ClientCommand, text::ClientTextDto};

#[test]
fn send_prompt_preserves_lone_surrogate_through_typed_command_roundtrip() {
    let raw = r#"{"type":"send_prompt","text":"left\ud800right","images":[]}"#;
    let command: ClientCommand = serde_json::from_str(raw).expect("deserialize raw prompt");
    let ClientCommand::SendPrompt { text, .. } = &command else {
        panic!("expected SendPrompt");
    };
    assert_eq!(
        text.code_units,
        [
            "left".encode_utf16().collect::<Vec<_>>(),
            vec![0xd800],
            "right".encode_utf16().collect()
        ]
        .concat()
    );

    let encoded = serde_json::to_string(&command).expect("serialize source prompt");
    assert!(encoded.contains(r#""text":"left\ud800right""#), "{encoded}");
    assert_eq!(
        serde_json::from_str::<ClientCommand>(&encoded).expect("round-trip source prompt"),
        command
    );
}

#[test]
fn slash_command_preserves_surrogate_pairs_and_ordinary_typed_text() {
    let raw = r#"{"type":"run_slash_command","raw":"/deploy \ud83d\ude80","images":[]}"#;
    let command: ClientCommand = serde_json::from_str(raw).expect("deserialize raw slash command");
    let ClientCommand::RunSlashCommand { raw, .. } = &command else {
        panic!("expected RunSlashCommand");
    };
    assert_eq!(
        raw.code_units,
        [
            "/deploy ".encode_utf16().collect::<Vec<_>>(),
            vec![0xd83d, 0xde80]
        ]
        .concat()
    );
    let encoded = serde_json::to_string(&command).expect("serialize slash command");
    assert!(encoded.contains(r#""raw":"/deploy 🚀""#), "{encoded}");
    assert_eq!(
        serde_json::from_str::<ClientCommand>(&encoded).expect("round-trip slash command"),
        command
    );

    let ordinary = ClientCommand::RunSlashCommand {
        raw: ClientTextDto::from_utf8("/help"),
        images: Vec::new(),
        turn_id: None,
    };
    let ordinary_json = serde_json::to_string(&ordinary).expect("serialize ordinary command");
    assert!(
        ordinary_json.contains(r#""raw":"/help""#),
        "{ordinary_json}"
    );
    assert_eq!(
        serde_json::from_str::<ClientCommand>(&ordinary_json).expect("round-trip ordinary command"),
        ordinary
    );
    assert!(
        serde_json::from_str::<ClientCommand>(r#"{"type":"run_slash_command","raw":"/help"}"#)
            .is_err(),
        "images is required by protocol 28"
    );
}

#[test]
fn prompt_raw_members_keep_last_duplicate_and_optional_null_semantics() {
    let raw = r#"{"type":"send_prompt","text":"discarded","text":"kept\ud800","prompt_mode":null,"images":[],"turn_id":null,"visualization_context":null}"#;
    let command: ClientCommand = serde_json::from_str(raw).expect("deserialize duplicate prompt");
    let ClientCommand::SendPrompt {
        text,
        prompt_mode,
        images,
        turn_id,
        visualization_context,
    } = &command
    else {
        panic!("expected SendPrompt");
    };
    assert_eq!(
        text.code_units,
        ["kept".encode_utf16().collect::<Vec<_>>(), vec![0xd800]].concat()
    );
    assert!(prompt_mode.is_none());
    assert!(images.is_empty());
    assert!(turn_id.is_none());
    assert!(visualization_context.is_none());

    assert!(
        serde_json::from_str::<ClientCommand>(r#"{"type":"send_prompt","images":[]}"#).is_err()
    );
    assert!(serde_json::from_str::<ClientCommand>(
        r#"{"type":"send_prompt","text":null,"images":[]}"#
    )
    .is_err());
    assert!(serde_json::from_str::<ClientCommand>(r#"{"type":"send_prompt","text":"x"}"#).is_err());
}

#[test]
fn skill_admin_target_preserves_exact_units_and_plain_string_wire_shape() {
    let raw = r#"{"type":"skill_admin","command":{"action":"get_document","target":"command:re\ud800view"}}"#;
    let outer: ClientCommand = serde_json::from_str(raw).expect("deserialize skill admin");
    let ClientCommand::SkillAdmin { command } = &outer else {
        panic!("expected SkillAdmin");
    };
    assert_eq!(
        command.target.as_ref().unwrap().code_units,
        [
            "command:re".encode_utf16().collect::<Vec<_>>(),
            vec![0xd800],
            "view".encode_utf16().collect()
        ]
        .concat()
    );
    let encoded = serde_json::to_string(&outer).expect("serialize skill admin command");
    assert!(
        encoded.contains(r#""target":"command:re\ud800view""#),
        "{encoded}"
    );
    assert_eq!(
        serde_json::from_str::<ClientCommand>(&encoded).unwrap(),
        outer
    );

    let paired: ClientCommand = serde_json::from_str(
        r#"{"type":"skill_admin","command":{"action":"get_document","target":"rocket:\ud83d\ude80"}}"#,
    )
    .expect("deserialize paired skill identity");
    let ClientCommand::SkillAdmin { command } = paired else {
        panic!("expected SkillAdmin");
    };
    assert_eq!(
        command.target.unwrap().code_units,
        "rocket:🚀".encode_utf16().collect::<Vec<_>>()
    );

    let ordinary: ClientCommand = serde_json::from_str(
        r#"{"type":"skill_admin","command":{"action":"get_document","target":"/skills/review"}}"#,
    )
    .expect("deserialize ordinary skill identity");
    let ClientCommand::SkillAdmin { command } = ordinary else {
        panic!("expected SkillAdmin");
    };
    assert_eq!(
        command.target.unwrap().code_units,
        "/skills/review".encode_utf16().collect::<Vec<_>>()
    );
}

#[test]
fn skill_admin_target_keeps_optional_and_duplicate_member_policies() {
    for raw in [
        r#"{"type":"skill_admin","command":{"action":"get_catalog"}}"#,
        r#"{"type":"skill_admin","command":{"action":"get_catalog","target":null}}"#,
    ] {
        let ClientCommand::SkillAdmin { command } =
            serde_json::from_str(raw).expect("decode optional target")
        else {
            panic!("expected SkillAdmin");
        };
        assert!(command.target.is_none());
    }

    let duplicate_outer = r#"{"type":"skill_admin","command":{"action":"get_document","target":"first"},"command":{"action":"get_document","target":"last"}}"#;
    let ClientCommand::SkillAdmin { command } =
        serde_json::from_str(duplicate_outer).expect("outer envelope is last-member-wins")
    else {
        panic!("expected SkillAdmin");
    };
    assert_eq!(
        command.target.unwrap().code_units,
        "last".encode_utf16().collect::<Vec<_>>()
    );

    assert!(serde_json::from_str::<ClientCommand>(
        r#"{"type":"skill_admin","command":{"action":"get_document","target":"first","target":"last"}}"#
    )
    .is_err());
    assert!(serde_json::from_str::<ClientCommand>(
        r#"{"type":"skill_admin","command":{"action":"get_document","target":7}}"#
    )
    .is_err());
}

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
fn slash_command_catalog_preserves_lone_surrogates_through_raw_roundtrip() {
    let raw = r#"{"type":"slash_command_catalog","commands":[{"name":"re\ud800view","description":"desc\udfff","source":"plugin","aliases":["a\ud800"],"argument_hint":"hint\udfff","hidden":false}]}"#;
    let event: ClientEvent = serde_json::from_str(raw).expect("deserialize command catalog");
    let ClientEvent::SlashCommandCatalog { commands } = &event else {
        panic!("expected SlashCommandCatalog");
    };
    assert_eq!(
        commands[0].name.code_units,
        [
            "re".encode_utf16().collect::<Vec<_>>(),
            vec![0xd800],
            "view".encode_utf16().collect()
        ]
        .concat()
    );
    assert_eq!(
        commands[0].description.code_units,
        ["desc".encode_utf16().collect::<Vec<_>>(), vec![0xdfff]].concat()
    );
    assert_eq!(
        commands[0].aliases[0].code_units,
        [vec![u16::from(b'a')], vec![0xd800]].concat()
    );
    assert_eq!(
        commands[0].argument_hint.as_ref().unwrap().code_units,
        ["hint".encode_utf16().collect::<Vec<_>>(), vec![0xdfff]].concat()
    );

    let encoded = serde_json::to_string(&event).expect("serialize command catalog");
    assert!(encoded.contains(r#""name":"re\ud800view""#), "{encoded}");
    assert!(
        encoded.contains(r#""description":"desc\udfff""#),
        "{encoded}"
    );
    assert!(encoded.contains(r#""aliases":["a\ud800"]"#), "{encoded}");
    assert!(
        encoded.contains(r#""argument_hint":"hint\udfff""#),
        "{encoded}"
    );
    let decoded: ClientEvent = serde_json::from_str(&encoded).expect("round-trip command catalog");
    assert_eq!(decoded, event);
}

#[test]
fn compaction_summary_preserves_a_lone_surrogate_through_raw_roundtrip() {
    let raw = r#"{"type":"compaction_completed","messages_before":8,"messages_after":3,"bytes_saved":128,"summary":"kept\ud800context"}"#;
    let event: ClientEvent = serde_json::from_str(raw).expect("deserialize compaction event");
    let ClientEvent::CompactionCompleted { summary, .. } = &event else {
        panic!("expected CompactionCompleted");
    };
    assert_eq!(
        summary.code_units,
        [
            "kept".encode_utf16().collect::<Vec<_>>(),
            vec![0xd800],
            "context".encode_utf16().collect()
        ]
        .concat()
    );

    let encoded = serde_json::to_string(&event).expect("serialize compaction event");
    assert!(
        encoded.contains(r#""summary":"kept\ud800context""#),
        "{encoded}"
    );
    let decoded: ClientEvent = serde_json::from_str(&encoded).expect("round-trip compaction event");
    assert_eq!(decoded, event);
}

#[test]
fn session_agent_name_preserves_a_lone_surrogate_through_raw_roundtrip() {
    let raw = r#"{"type":"session_agent_list","session_id":"session","agents":[{"agent_id":"agent:1","name":"worker\udfff","agent_type":"general-purpose","status":"running","latest_activity":"work\ud800ing"}]}"#;
    let event: ClientEvent = serde_json::from_str(raw).expect("deserialize agent list");
    let ClientEvent::SessionAgentList { agents, .. } = &event else {
        panic!("expected SessionAgentList");
    };
    assert_eq!(
        agents[0].name.code_units,
        ["worker".encode_utf16().collect::<Vec<_>>(), vec![0xdfff]].concat()
    );
    assert_eq!(
        agents[0].latest_activity.as_ref().unwrap().code_units,
        [
            "work".encode_utf16().collect::<Vec<_>>(),
            vec![0xd800],
            "ing".encode_utf16().collect()
        ]
        .concat()
    );

    let encoded = serde_json::to_string(&event).expect("serialize agent list");
    assert!(encoded.contains(r#""name":"worker\udfff""#), "{encoded}");
    assert!(
        encoded.contains(r#""latest_activity":"work\ud800ing""#),
        "{encoded}"
    );
    let decoded: ClientEvent = serde_json::from_str(&encoded).expect("round-trip agent list");
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
