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
    assert_eq!(serde_json::to_string(&event).unwrap(), r#"{"type":"prompt_suggestion","suggestion":"运行测试","uuid":"suggestion-2","session_id":"session-1"}"#);
    assert_eq!(
        serde_json::from_value::<ClientEvent>(expected).unwrap(),
        event
    );
}


#[test]
fn composer_feedback_is_an_explicit_host_extension() {
    use client::protocol::commands::ClientCommand;
    let commands = [
        ClientCommand::RecordPromptSuggestionSubmission { text: "/config".into() },
        ClientCommand::UpdatePromptSuggestionContext {
            session_id: "session-1".into(), focus: "focused".into(), input_empty: true,
            pending_permission: false, elicitation_active: false,
        },
        ClientCommand::PromptSuggestionFeedback {
            session_id: "session-1".into(), uuid: "suggestion-1".into(), action: "shown".into(),
        },
    ];
    for command in commands {
        let encoded = serde_json::to_string(&command).unwrap();
        assert_eq!(serde_json::from_str::<ClientCommand>(&encoded).unwrap(), command);
    }
    let state = ClientEvent::PromptSuggestionState { enabled: false, notice: None };
    assert_eq!(serde_json::to_string(&state).unwrap(), r#"{"type":"prompt_suggestion_state","enabled":false,"notice":null}"#);
}
