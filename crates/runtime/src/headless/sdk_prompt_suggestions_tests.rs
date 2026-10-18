use super::*;
use lingxi_core::types::utf16_json::Utf16JsonProjection;

fn line(frame: &Utf16JsonProjection) -> String {
    frame
        .to_json_string()
        .unwrap()
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
        + "\n"
}

#[test]
fn suggestion_frame_serializes_lone_surrogate_as_json_string() {
    let mut state = SdkPromptSuggestionState::new(true);
    let generation = match state.after_result(true, false, false, false, Some("user")) {
        AfterResult::Generate(generation) => generation,
        other => panic!("expected a suggestion query, got {other:?}"),
    };
    let suggestion = Utf16JsonProjection::parse(r#""try \ud800 next""#).unwrap();
    let frame = state
        .generated(
            generation,
            suggestion,
            None,
            "suggestion-1",
            "session-1",
            10,
            false,
        )
        .expect("current generation emits a frame");
    let wire = frame.to_json_string().unwrap();
    assert_eq!(
        wire,
        r#"{"type":"prompt_suggestion","suggestion":"try \ud800 next","uuid":"suggestion-1","session_id":"session-1"}"#
    );
    assert!(!wire.contains(r#""suggestion":["#));
    assert_eq!(
        frame.string_units("/suggestion"),
        Some(
            "try "
                .encode_utf16()
                .chain([0xd800])
                .chain(" next".encode_utf16())
                .collect()
        )
    );
}

#[test]
fn pending_suggestion_feedback_keeps_source_surrogate_distinct_from_display_text() {
    let mut state = SdkPromptSuggestionState::new(true);
    let generation = match state.after_result(true, false, false, false, Some("user")) {
        AfterResult::Generate(generation) => generation,
        other => panic!("expected a suggestion query, got {other:?}"),
    };
    let suggestion = Utf16JsonProjection::parse(r#""\ud800""#).unwrap();
    assert!(state
        .generated(
            generation,
            suggestion,
            None,
            "suggestion-1",
            "session-1",
            10,
            true,
        )
        .is_none());

    let pending = state.emit_pending(11).expect("held suggestion is released");
    assert_eq!(pending.string_units("/suggestion"), Some(vec![0xd800]));
    let feedback = state
        .on_command(true, Some(&json!("�")), 12)
        .expect("prompt input records feedback");
    assert_eq!(feedback["outcome"], "ignored");
}

#[test]
fn native_sdk_controller_oracle() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/prompt_suggestion_sdk_2_1_296.json"
    ))
    .unwrap();
    assert_eq!(fixture["settleTimeoutMs"], SDK_SUGGESTION_SETTLE_TIMEOUT_MS);
    for case in fixture["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let mut state = SdkPromptSuggestionState::new(case["enabled"].as_bool().unwrap_or(true));
        let mut now = 1000;
        let mut requests = Vec::new();
        let mut frames = Vec::new();
        let mut events = Vec::new();
        let mut frame_index = 0;
        for action in case["actions"].as_array().unwrap() {
            match action["op"].as_str().unwrap() {
                "after" => match state.after_result(
                    action["shouldQuery"].as_bool().unwrap(),
                    case["shuttingDown"].as_bool().unwrap_or(false),
                    case["environmentDisabled"].as_bool().unwrap_or(false),
                    case["sable"].as_bool().unwrap_or(false),
                    case["origin"].as_str(),
                ) {
                    AfterResult::Generate(generation) => requests.push(generation),
                    AfterResult::Suppressed(reason) => events.push(json!({
                        "name":"tengu_prompt_suggestion", "payload": {
                            "source":"sdk","outcome":"suppressed","reason":reason,"prompt_id":"user_intent"
                        }
                    })),
                    AfterResult::Disabled => {},
                },
                "generated" => {
                    let generation = requests[action["index"].as_u64().unwrap() as usize];
                    if action["empty"] == true || !state.generation_is_current(generation) { continue; }
                    frame_index += 1;
                    if let Some(frame) = state.generated(
                        generation,
                        action["suggestion"].as_str().unwrap().into(),
                        action["requestId"].as_str().map(String::from),
                        &format!("frame-{frame_index}"), "session-fixture", now,
                        case["holdsResults"].as_bool().unwrap_or(false),
                    ) { frames.push(line(&frame)); }
                },
                "command" => {
                    if let Some(payload) = state.on_command(
                        action["shouldQuery"].as_bool().unwrap(),
                        (action["mode"] == "prompt").then_some(&action["content"]), now,
                    ) { events.push(json!({"name":"tengu_prompt_suggestion","payload":payload})); }
                },
                "pause" => state.set_paused(action["value"].as_bool().unwrap()),
                "enabled" => state.enabled = action["value"].as_bool().unwrap(),
                "time" => now = action["now"].as_u64().unwrap(),
                "release" => if let Some(frame) = state.emit_pending(now) {
                    frames.push(line(&frame));
                },
                "discard" => state.discard(),
                other => panic!("unknown oracle action {other}"),
            }
        }
        assert_eq!(
            json!(frames),
            case["expected"]["frames"],
            "{name}: wire bytes"
        );
        assert_eq!(
            json!(events),
            case["expected"]["events"],
            "{name}: telemetry"
        );
        assert_eq!(
            json!(requests.len()),
            case["expected"]["generations"],
            "{name}: query count"
        );
        let aborted: Vec<bool> = requests
            .iter()
            .map(|generation| !state.generation_is_current(*generation))
            .collect();
        assert_eq!(
            json!(aborted),
            case["expected"]["aborted"],
            "{name}: cancellation"
        );
    }
}

#[test]
fn initialize_and_remote_reattach_match_native_opt_in_policy() {
    let mut state = SdkPromptSuggestionState::new(false);
    state.initialize(Some(true), false);
    assert!(state.enabled);
    state.initialize(Some(false), false);
    assert!(state.enabled, "reinitialize does not opt out");
    let mut state = SdkPromptSuggestionState::new(false);
    state.initialize(None, false);
    state.initialize(Some(true), false);
    assert!(!state.enabled, "ordinary reinitialize cannot opt in");
    state.initialize(Some(true), true);
    assert!(state.enabled, "remote_cowork may opt in when reattached");
    let mut state = SdkPromptSuggestionState::new(true);
    state.initialize(Some(false), false);
    assert!(!state.enabled, "the initial SDK setting overrides argv");
    let mut state = SdkPromptSuggestionState::new(false);
    state.input_started();
    state.initialize(Some(true), false);
    assert!(
        !state.enabled,
        "initialize after a user frame is a reinitialize"
    );
}

#[test]
fn pause_validation_matches_native_no_provider_control_capture() {
    fn parse_raw(value: Option<&str>) -> Result<bool, String> {
        let Some(value) = value else {
            return parse_paused(None);
        };
        let frame = lingxi_core::types::utf16_json::Utf16JsonProjection::parse(&format!(
            "{{\"paused\":{value}}}"
        ))
        .unwrap();
        parse_paused(frame.get_property("paused"))
    }

    for (value, received) in [
        (None, "undefined"),
        (Some("null"), "null"),
        (Some("\"yes\""), "string"),
        (Some("0"), "number"),
        (Some("[]"), "array"),
        (Some("{}"), "object"),
    ] {
        assert_eq!(
            parse_raw(value).unwrap_err(),
            format!(
                "Invalid set_prompt_suggestions_paused: Invalid input: expected boolean, received {received}"
            )
        );
    }
    assert_eq!(parse_raw(Some("true")), Ok(true));
    assert_eq!(parse_raw(Some("false")), Ok(false));

    let isolated_surrogate =
        lingxi_core::types::utf16_json::Utf16JsonProjection::parse(r#"{"paused":"\ud800"}"#)
            .unwrap();
    assert_eq!(
        parse_paused(isolated_surrogate.get_property("paused")).unwrap_err(),
        "Invalid set_prompt_suggestions_paused: Invalid input: expected boolean, received string"
    );
}
