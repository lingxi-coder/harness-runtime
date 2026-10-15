use crate::upstream::codec_fixtures::HistoryFixture;
use crate::upstream::codec_fixtures::{
    AnthropicMessagesCodec, BedrockClaudeCodec, FixtureCodec, OpenAiChatCodec, OpenAiResponsesCodec,
};
// Cross-provider replay and exact tool-result projection regressions.
use llm_runtime::{CacheControl, ContentBlock, Message, ProviderResponse};
use serde_json::json;

#[test]
fn switching_from_responses_to_chat_keeps_answer_and_original_replay_state() {
    let responses = OpenAiResponsesCodec::new("https://api.openai.com/v1");
    let response = responses
        .decode_response(ProviderResponse::json(
            200,
            json!({
                "id":"response-1", "model":"reasoning-model", "status":"completed",
                "output":[
                    {"type":"reasoning", "id":"reasoning-1", "encrypted_content":"opaque",
                     "summary":[{"type":"summary_text", "text":"summary"}]},
                    {"type":"message", "role":"assistant",
                     "content":[{"type":"output_text", "text":"answer"}]}
                ],
                "usage":{"input_tokens":1,"output_tokens":2}
            }),
        ))
        .unwrap();
    let mut request = HistoryFixture::new("chat-model");
    request.request.input.max_tokens = Some(4096);
    request.messages.push(Message {
        api_output_config: None,
        role: "assistant".into(),
        content: response.content,
    });
    let original = request.clone();
    let chat = OpenAiChatCodec::new("https://example.test/v1")
        .encode_request(&request)
        .unwrap();
    let sent = chat.body_json.as_json_source();
    assert!(sent.contains("answer"));
    assert!(!sent.contains("opaque"));
    assert!(!sent.contains("encrypted_content"));
    assert_eq!(
        request, original,
        "projection must not mutate transcript data"
    );

    let resumed = responses.encode_request(&request).unwrap();
    assert_eq!(
        super::json_payload_value(&resumed.body_json)["input"][0]["encrypted_content"],
        "opaque"
    );

    request.messages[0].content.push(ContentBlock::TextJsUtf16 {
        text: "B�".into(),
        utf16_code_units: vec![66, 0xd83d],
        cache_control: None,
        citations: None,
    });
    let original = request.clone();
    let anthropic = AnthropicMessagesCodec::new("https://api.anthropic.com", "2023-06-01")
        .encode_request(&request)
        .unwrap();
    let sent = String::from_utf8(anthropic.wire_body_bytes().unwrap()).unwrap();
    assert!(sent.contains("answer"));
    assert!(sent.contains(r#""text":"B\ud83d""#));
    assert!(!sent.contains("summary"));
    assert!(!sent.contains("opaque"));
    assert_eq!(request, original);
}

#[test]
fn switching_away_from_anthropic_keeps_native_text_without_hosted_tool_state() {
    let mut request = HistoryFixture::new("chat-model");
    request.messages.push(Message { api_output_config: None,
        role: "assistant".into(),
        content: vec![
            ContentBlock::ServerToolUse {
                id: "search-1".into(),
                name: "web_search".into(),
                input: super::json_payload(json!({"query":"something"})),
            },
            ContentBlock::ProviderContent {
                protocol: "anthropic_messages".into(),
                value: super::json_payload(json!({"type":"text", "text":"cited answer", "citations":[{"type":"web_search_result_location"}]})),
            },
        ],
    });
    let original = request.clone();
    let projected = OpenAiChatCodec::new("https://example.test/v1")
        .encode_request(&request)
        .unwrap();
    let sent = projected.body_json.as_json_source();
    assert!(sent.contains("cited answer"));
    assert!(!sent.contains("server_tool_use"));
    assert!(!sent.contains("citations"));
    assert_eq!(request, original);
}

fn exact_tool_result(cache_control: Option<CacheControl>) -> ContentBlock {
    ContentBlock::ToolResult {
        tool_call_id: "call-1".into(),
        output: lingxi_llm_client::protocol::JsonPayload::string_from_utf16_code_units(&[
            65, 0xd83d, 10,
        ]),
        is_error: Some(false),
        cache_control,
        cache_reference: None,
    }
}

#[test]
fn tool_result_sidecars_use_exact_sdk_strings_after_replay_filtering() {
    let codecs: Vec<Box<dyn FixtureCodec>> = vec![
        Box::new(AnthropicMessagesCodec::new(
            "https://api.anthropic.com",
            "2023-06-01",
        )),
        Box::new(BedrockClaudeCodec::new(
            "https://bedrock-runtime.us-east-1.amazonaws.com",
        )),
    ];
    for codec in codecs {
        for control in [None, Some(CacheControl::Ephemeral)] {
            let mut request = HistoryFixture::new("claude-sonnet-4-6");
            request.messages.push(Message {
                api_output_config: None,
                role: "assistant".into(),
                content: vec![
                    ContentBlock::ProviderContent {
                        protocol: "open_ai_responses".into(),
                        value: super::json_payload(
                            json!({"type":"reasoning", "encrypted_content":"foreign"}),
                        ),
                    },
                    ContentBlock::Reasoning {
                        text: "foreign summary".into(),
                        signature: None,
                    },
                ],
            });
            request.messages.push(Message {
                api_output_config: None,
                role: "user".into(),
                content: vec![
                    ContentBlock::ProviderContent {
                        protocol: "open_ai_responses".into(),
                        value: super::json_payload(
                            json!({"type":"reasoning", "encrypted_content":"foreign"}),
                        ),
                    },
                    exact_tool_result(control),
                    ContentBlock::TextJsUtf16 {
                        text: "B�".into(),
                        utf16_code_units: vec![66, 0xd83d],
                        cache_control: None,
                        citations: None,
                    },
                ],
            });
            let original = request.clone();
            let encoded = codec.encode_request(&request).unwrap();
            let sent = String::from_utf8(encoded.wire_body_bytes().unwrap()).unwrap();
            assert!(sent.contains(r#""content":"A\ud83d\n""#), "{sent}");
            assert!(sent.contains(r#""text":"B\ud83d""#), "{sent}");
            assert!(!sent.contains("lingxi_tool_result_string_utf16"));
            assert!(!sent.contains("foreign"));
            assert_eq!(
                super::json_payload_projection(&encoded.body_json).array_len("/messages/0/content"),
                Some(2)
            );
            assert_eq!(request, original);
        }
    }
}

#[test]
fn tool_result_sidecars_become_display_text_on_chat_wire() {
    let mut request = HistoryFixture::new("chat-model");
    request.messages.push(Message {
        api_output_config: None,
        role: "user".into(),
        content: vec![exact_tool_result(None)],
    });
    let encoded = OpenAiChatCodec::new("https://example.test/v1")
        .encode_request(&request)
        .unwrap();
    let projected = super::json_payload_projection(&encoded.body_json);
    assert_eq!(projected.string_at("/messages/0/content"), Some("A�\n"));
    assert_eq!(
        projected.string_units("/messages/0/content").as_deref(),
        Some(&[65, 0xd83d, 10][..])
    );
    assert!(!encoded
        .body_json
        .as_json_source()
        .contains("lingxi_tool_result_string_utf16"));
}
