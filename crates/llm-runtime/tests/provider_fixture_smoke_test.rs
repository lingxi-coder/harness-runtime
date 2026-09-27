use llm_runtime::{
    providers::GeminiCodec, AnthropicMessagesCodec, LlmEvent, OpenAiChatCodec, ProviderResponse,
    RawStreamFrame, WireCodec,
};

fn decode_jsonl_stream<C: WireCodec>(codec_name: &str, codec: &C, fixture: &str) -> Vec<LlmEvent> {
    let mut decoder = codec.stream_decoder();
    let mut events = Vec::new();

    for (frame_index, frame) in fixture
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .enumerate()
    {
        let frame_events = decoder
            .decode_frame(RawStreamFrame::new(frame.as_bytes().to_vec()))
            .unwrap_or_else(|err| {
                panic!("{codec_name} frame {frame_index} decode failed: {err:?}")
            });
        events.extend(frame_events);
    }

    events.extend(
        decoder
            .finish()
            .unwrap_or_else(|err| panic!("{codec_name} fixture finish failed: {err:?}")),
    );
    events
}

fn assert_single_message_stop(events: &[LlmEvent]) {
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, LlmEvent::MessageStop))
            .count(),
        1
    );
    assert!(matches!(events.last(), Some(LlmEvent::MessageStop)));
}

fn assert_openai_events(events: &[LlmEvent]) {
    assert!(matches!(events[0], LlmEvent::MessageStart { .. }));
    assert!(matches!(
        events[1],
        LlmEvent::ContentBlockStart {
            index: 0,
            content_block: llm_runtime::ContentBlock::Text { .. },
        }
    ));
    assert!(matches!(
        events[2],
        LlmEvent::ContentBlockDelta {
            index: 0,
            delta: llm_runtime::ContentDelta::TextDelta { ref text },
        } if text == "On it. "
    ));
    assert!(matches!(
        events[3],
        LlmEvent::ContentBlockStart {
            index: 1,
            content_block: llm_runtime::ContentBlock::ToolCall { ref name, .. },
        } if name == "Bash"
    ));
    assert!(matches!(
        events[4],
        LlmEvent::ContentBlockDelta {
            index: 1,
            delta: llm_runtime::ContentDelta::InputJsonDelta { ref partial_json },
        } if partial_json == "{\"command\":"
    ));
    assert!(matches!(
        events[5],
        LlmEvent::ContentBlockDelta {
            index: 1,
            delta: llm_runtime::ContentDelta::InputJsonDelta { ref partial_json },
        } if partial_json == "\"ls\"}"
    ));
    assert!(matches!(events[6], LlmEvent::ContentBlockStop { index: 0 }));
    assert!(matches!(events[7], LlmEvent::ContentBlockStop { index: 1 }));
    assert!(matches!(
        events[8],
        LlmEvent::MessageDelta {
            delta: llm_runtime::MessageDeltaPayload { stop_reason: Some(ref reason), stop_details: None },
            ..
        } if reason == "tool_use"
    ));
    assert_single_message_stop(events);
}

fn assert_gemini_events(events: &[LlmEvent]) {
    assert!(matches!(events[0], LlmEvent::MessageStart { .. }));
    assert!(matches!(
        events[1],
        LlmEvent::ContentBlockStart {
            index: 0,
            content_block: llm_runtime::ContentBlock::Text { .. },
        }
    ));
    assert!(matches!(
        events[2],
        LlmEvent::ContentBlockDelta {
            index: 0,
            delta: llm_runtime::ContentDelta::TextDelta { ref text },
        } if text == "On it."
    ));
    assert!(matches!(
        events[3],
        LlmEvent::ContentBlockStart {
            index: 1,
            content_block: llm_runtime::ContentBlock::ToolCall { ref name, .. },
        } if name == "Bash"
    ));
    assert!(matches!(
        events[4],
        LlmEvent::ContentBlockDelta {
            index: 1,
            delta: llm_runtime::ContentDelta::InputJsonDelta { ref partial_json },
        } if partial_json == "{\"command\":\"ls\"}"
    ));
    assert!(matches!(events[5], LlmEvent::ContentBlockStop { index: 1 }));
    assert!(matches!(events[6], LlmEvent::ContentBlockStop { index: 0 }));
    assert!(matches!(
        events[7],
        LlmEvent::MessageDelta {
            delta: llm_runtime::MessageDeltaPayload { stop_reason: Some(ref reason), stop_details: None },
            ..
        } if reason == "tool_use"
    ));
    assert_single_message_stop(events);
}

#[test]
fn provider_fixtures_smoke_test() {
    let openai_fixture = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/openai_stream_text_tool.jsonl"
    ));
    let gemini_fixture = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/gemini_stream_text_tool.jsonl"
    ));
    let anthropic_fixture = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/anthropic_text_response.json"
    ));

    let openai_events = decode_jsonl_stream(
        "OpenAI",
        &OpenAiChatCodec::new("https://api.openai.com/v1"),
        openai_fixture,
    );
    assert_openai_events(&openai_events);

    let gemini_events = decode_jsonl_stream(
        "Gemini",
        &GeminiCodec::new("https://generativelanguage.googleapis.com/v1beta"),
        gemini_fixture,
    );
    assert_gemini_events(&gemini_events);

    let anthropic = AnthropicMessagesCodec::new("https://api.anthropic.com", "2023-06-01");
    let decoded = anthropic
        .decode_response(ProviderResponse::json(
            200,
            serde_json::from_str(anthropic_fixture).expect("Anthropic fixture JSON should parse"),
        ))
        .unwrap();

    assert!(matches!(
        decoded.content.as_slice(),
        [llm_runtime::ContentBlock::Text { text, .. }] if text == "hi"
    ));
    assert_eq!(decoded.stop_reason.as_deref(), Some("end_turn"));
    assert_eq!(
        decoded.provider_metadata["stop_reason"],
        serde_json::json!("end_turn")
    );
    assert_eq!(decoded.usage.billable_tokens.input, 9);
    assert_eq!(decoded.usage.billable_tokens.output, 3);
}
