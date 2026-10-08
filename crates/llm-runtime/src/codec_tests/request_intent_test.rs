use crate::upstream::codec_fixtures::HistoryFixture;
use crate::upstream::codec_fixtures::{
    AnthropicMessagesCodec, FixtureCodec, GeminiCodec, OpenAiChatCodec,
};

use llm_runtime::{ContentBlock, LlmError, ToolChoice, ToolDeclaration};

fn openai_codec() -> OpenAiChatCodec {
    OpenAiChatCodec::new("https://api.openai.com/v1")
}

fn gemini_codec() -> GeminiCodec {
    GeminiCodec::new("https://generativelanguage.googleapis.com/v1beta")
}

fn anthropic_codec() -> AnthropicMessagesCodec {
    AnthropicMessagesCodec::new("https://api.anthropic.com", "2023-06-01")
}

fn request_with_block(model: &str, block: ContentBlock) -> HistoryFixture {
    let mut request = HistoryFixture::new(model);
    request.messages.push(llm_runtime::Message { api_output_config: None,
        role: "user".to_string(),
        content: vec![block],
    });
    request
}

#[test]
fn anthropic_encodes_stream_true() {
    let mut request = HistoryFixture::new("claude-sonnet-4-20250514");
    request.request.stream = true;

    let provider_request = anthropic_codec().encode_request(&request).unwrap();

    assert_eq!(provider_request.body_json["stream"], true);
}

#[test]
fn openai_encodes_stream_true() {
    let mut request = HistoryFixture::new("gpt-4o");
    request.request.stream = true;

    let provider_request = openai_codec().encode_request(&request).unwrap();

    assert_eq!(provider_request.body_json["stream"], true);
}

#[test]
fn openai_encodes_response_format_variants() {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {"answer": {"type": "string"}},
        "required": ["answer"],
        "additionalProperties": false,
    });
    let cases = [
        (
            lingxi_llm_client::protocol::OutputFormat::JsonObject,
            serde_json::json!({"type": "json_object"}),
        ),
        (
            lingxi_llm_client::protocol::OutputFormat::JsonSchema {
                name: "response".into(),
                strict: true,
                schema: schema.clone(),
            },
            serde_json::json!({"type": "json_schema", "json_schema": {"name": "response", "strict": true, "schema": schema}}),
        ),
    ];

    for (response_format, expected) in cases {
        let mut request = HistoryFixture::new("gpt-4o");
        request.request.input.output_format = response_format;

        let provider_request = openai_codec().encode_request(&request).unwrap();

        assert_eq!(provider_request.body_json["response_format"], expected);
    }
}

#[test]
fn gemini_encodes_response_format_requests() {
    let schema = serde_json::json!({
        "type": "object", "properties": {"answer": {"type": "string"}}
    });
    let cases = [
        (
            lingxi_llm_client::protocol::OutputFormat::JsonObject,
            serde_json::json!({"text": {"mimeType": "application/json"}}),
        ),
        (
            lingxi_llm_client::protocol::OutputFormat::JsonSchema {
                name: "response".into(),
                strict: true,
                schema: schema.clone(),
            },
            serde_json::json!({"text": {"mimeType": "application/json", "schema": schema}}),
        ),
    ];

    for (response_format, expected) in cases {
        let mut request = HistoryFixture::new("gemini-2.0-flash");
        request.request.input.output_format = response_format;

        let encoded = gemini_codec().encode_request(&request).unwrap();
        assert_eq!(
            encoded.body_json["generationConfig"]["responseFormat"],
            expected
        );
    }
}

#[test]
fn openai_rejects_response_schemas_that_are_not_valid_for_strict_output() {
    for schema in [
        serde_json::json!({
            "type": "object", "properties": {"answer": {"type": "string"}},
            "required": ["answer"]
        }),
        serde_json::json!({
            "type": "object", "properties": {"answer": {"type": "string"}},
            "additionalProperties": false
        }),
    ] {
        let mut request = HistoryFixture::new("gpt-4o");
        request.request.input.output_format =
            lingxi_llm_client::protocol::OutputFormat::JsonSchema {
                name: "response".into(),
                strict: true,
                schema,
            };
        assert!(matches!(
            openai_codec().encode_request(&request),
            Err(LlmError::InvalidRequest { .. })
        ));
    }
}

#[test]
fn openai_encodes_tool_choice_variants() {
    let cases = [
        (ToolChoice::Auto, serde_json::json!("auto")),
        (ToolChoice::None, serde_json::json!("none")),
        (ToolChoice::Required, serde_json::json!("required")),
        (
            ToolChoice::Tool {
                name: "Read".to_string(),
            },
            serde_json::json!({"type": "function", "function": {"name": "Read"}}),
        ),
    ];

    for (tool_choice, expected) in cases {
        let mut request = HistoryFixture::new("gpt-4o");
        request.request.set_tool_choice(Some(tool_choice));
        request.tools = vec![ToolDeclaration {
            name: "Read".to_string(),
            description: "d".to_string(),
            input_schema: serde_json::json!({"type": "object"}),
            ..Default::default()
        }];

        let provider_request = openai_codec().encode_request(&request).unwrap();

        assert_eq!(provider_request.body_json["tool_choice"], expected);
    }
}

#[test]
fn gemini_encodes_tool_choice_variants() {
    // tool_choice is now supported for Gemini — verify it succeeds and emits toolConfig.
    let cases = [
        ToolChoice::Auto,
        ToolChoice::None,
        ToolChoice::Required,
        ToolChoice::Tool {
            name: "Read".to_string(),
        },
    ];

    for tool_choice in cases {
        let mut request = HistoryFixture::new("gemini-2.0-flash");
        request.request.set_tool_choice(Some(tool_choice));
        request.tools = vec![ToolDeclaration {
            name: "Read".to_string(),
            description: "d".to_string(),
            input_schema: serde_json::json!({"type": "object"}),
            ..Default::default()
        }];

        let provider_request = gemini_codec().encode_request(&request).unwrap();
        assert!(provider_request.body_json.get("toolConfig").is_some());
    }
}

#[test]
fn openai_skips_reasoning_blocks_instead_of_rejecting() {
    // Image, ImageUrl, and Document are supported; Reasoning blocks (emitted
    // into history by the stream decoder) are intentionally SKIPPED on
    // re-encode — chat-completions has no assistant-reasoning input slot — so
    // encoding succeeds and the block is simply omitted from the wire body.
    // See `providers/openai.rs` (`ContentBlock::Reasoning` skip arm).
    let block = ContentBlock::Reasoning {
        text: "thought".to_string(),
        signature: None,
    };
    let request = request_with_block("gpt-4o", block);
    let encoded = openai_codec()
        .encode_request(&request)
        .expect("reasoning block skipped");
    assert!(!encoded.body_json.to_string().contains("thought"));
}

#[test]
fn openai_now_accepts_image_image_url_and_document_blocks() {
    // Image, ImageUrl, and Document are all supported.
    for block in [
        ContentBlock::Image {
            media_type: "image/png".to_string(),
            bytes: vec![1, 2, 3],
        },
        ContentBlock::ImageUrl {
            url: "https://example.com/img.png".to_string(),
        },
        ContentBlock::Document {
            media_type: "application/pdf".to_string(),
            bytes: vec![0x25, 0x50, 0x44, 0x46],
        },
    ] {
        let request = request_with_block("gpt-4o", block);
        openai_codec()
            .encode_request(&request)
            .expect("image/imageurl/document should be accepted");
    }
}

#[test]
fn gemini_preserves_reasoning_content() {
    let request = request_with_block(
        "gemini-2.0-flash",
        ContentBlock::Reasoning {
            text: "thought".into(),
            signature: None,
        },
    );
    let encoded = gemini_codec().encode_request(&request).unwrap();
    assert_eq!(
        encoded.body_json["contents"][0]["parts"][0]["thought"],
        true
    );
    assert_eq!(
        encoded.body_json["contents"][0]["parts"][0]["text"],
        "thought"
    );
}

#[test]
fn gemini_now_accepts_image_document_and_image_url_blocks() {
    // Image, Document, and ImageUrl are all supported.
    for block in [
        ContentBlock::Image {
            media_type: "image/png".to_string(),
            bytes: vec![1, 2, 3],
        },
        ContentBlock::Document {
            media_type: "application/pdf".to_string(),
            bytes: vec![0x25, 0x50, 0x44, 0x46],
        },
        ContentBlock::ImageUrl {
            url: "https://example.com/img.png".to_string(),
        },
    ] {
        let request = request_with_block("gemini-2.0-flash", block);
        gemini_codec()
            .encode_request(&request)
            .expect("image/document/imageurl should be accepted");
    }
}
