//! Suggestion events remain ephemeral, session-scoped protocol messages.
use client::protocol::events::ClientEvent;
#[test]
fn prompt_suggestion_wire_roundtrip() {
    let event = ClientEvent::PromptSuggestion {
        suggestion: "运行测试".into(),
        session_id: "session-1".into(),
        uuid: "suggestion-2".into(),
    };
    let expected = serde_json::json!({"type":"prompt_suggestion","suggestion":"运行测试","session_id":"session-1","uuid":"suggestion-2"});
    assert_eq!(serde_json::to_value(&event).unwrap(), expected);
    assert_eq!(
        serde_json::to_string(&event).unwrap(),
        r#"{"type":"prompt_suggestion","suggestion":"运行测试","uuid":"suggestion-2","session_id":"session-1"}"#
    );
    assert_eq!(
        serde_json::from_value::<ClientEvent>(expected).unwrap(),
        event
    );
}

#[test]
fn prompt_suggestion_wire_preserves_unpaired_surrogate_as_string() {
    use client::protocol::text::ClientTextDto;

    let suggestion = ClientTextDto::from_code_units(
        "try "
            .encode_utf16()
            .chain([0xd800])
            .chain(" now".encode_utf16())
            .collect(),
    );
    let event = ClientEvent::PromptSuggestion {
        suggestion: suggestion.clone(),
        session_id: "session-1".into(),
        uuid: "suggestion-2".into(),
    };
    let wire = serde_json::to_string(&event).unwrap();
    assert_eq!(
        wire,
        r#"{"type":"prompt_suggestion","suggestion":"try \ud800 now","uuid":"suggestion-2","session_id":"session-1"}"#
    );
    assert!(!wire.contains(r#""suggestion":["#));

    let decoded: ClientEvent = serde_json::from_str(&wire).unwrap();
    let ClientEvent::PromptSuggestion { suggestion, .. } = decoded else {
        panic!("expected prompt suggestion event");
    };
    assert_eq!(suggestion.code_units, suggestion_source_units());

    fn suggestion_source_units() -> Vec<u16> {
        "try "
            .encode_utf16()
            .chain([0xd800])
            .chain(" now".encode_utf16())
            .collect()
    }
}

#[test]
fn composer_feedback_is_an_explicit_host_extension() {
    use client::protocol::commands::ClientCommand;
    let commands = [
        ClientCommand::RecordPromptSuggestionSubmission {
            text: "/config".into(),
        },
        ClientCommand::UpdatePromptSuggestionContext {
            session_id: "session-1".into(),
            focus: "focused".into(),
            input_empty: true,
            pending_permission: false,
            elicitation_active: false,
        },
        ClientCommand::PromptSuggestionFeedback {
            session_id: "session-1".into(),
            uuid: "suggestion-1".into(),
            action: "shown".into(),
        },
    ];
    for command in commands {
        let encoded = serde_json::to_string(&command).unwrap();
        assert_eq!(
            serde_json::from_str::<ClientCommand>(&encoded).unwrap(),
            command
        );
    }
    let state = ClientEvent::PromptSuggestionState {
        enabled: false,
        notice: None,
    };
    assert_eq!(
        serde_json::to_string(&state).unwrap(),
        r#"{"type":"prompt_suggestion_state","enabled":false,"notice":null}"#
    );
}
