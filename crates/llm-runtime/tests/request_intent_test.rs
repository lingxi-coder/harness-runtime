use llm_runtime::providers::{GeminiCodec, OpenAiChatCodec};
use llm_runtime::{
    AnthropicMessagesCodec, ContentBlock, LlmError, LlmRequest, ResponseFormat, ToolChoice,
    ToolDeclaration, WireCodec,
};

fn openai_codec() -> OpenAiChatCodec {
    OpenAiChatCodec::new("https://api.openai.com/v1")
}

fn gemini_codec() -> GeminiCodec {
    GeminiCodec::new("https://generativelanguage.googleapis.com/v1beta")
}

fn anthropic_codec() -> AnthropicMessagesCodec {
    AnthropicMessagesCodec::new("https://api.anthropic.com", "2023-06-01")
}

fn request_with_block(model: &str, block: ContentBlock) -> LlmRequest {
    let mut request = LlmRequest::new(model);
    request.messages.push(llm_runtime::Message {
        role: "user".to_string(),
        content: vec![block],
    });
    request
}

#[test]
fn anthropic_encodes_stream_true() {
    let mut request = LlmRequest::new("claude-sonnet-4-20250514");
    request.stream = true;

    let provider_request = anthropic_codec().encode_request(&request).unwrap();

    assert_eq!(provider_request.body_json["stream"], true);
}

#[test]
fn openai_encodes_stream_true() {
    let mut request = LlmRequest::new("gpt-4o");
    request.stream = true;

    let provider_request = openai_codec().encode_request(&request).unwrap();

    assert_eq!(provider_request.body_json["stream"], true);
}

#[test]
fn openai_encodes_response_format_variants() {
    let cases = [
        (
            ResponseFormat::JsonObject,
            serde_json::json!({"type": "json_object"}),
        ),
        (
            ResponseFormat::JsonSchema {
                schema: serde_json::json!({"type": "object", "properties": {"answer": {"type": "string"}}}),
            },
            serde_json::json!({"type": "json_schema", "json_schema": {"name": "response", "strict": true, "schema": {"type": "object", "properties": {"answer": {"type": "string"}}}}}),
        ),
    ];

    for (response_format, expected) in cases {
        let mut request = LlmRequest::new("gpt-4o");
        request.response_format = Some(response_format);

        let provider_request = openai_codec().encode_request(&request).unwrap();

        assert_eq!(provider_request.body_json["response_format"], expected);
    }
}

#[test]
fn gemini_encodes_response_format_requests() {
    let cases = [
        ResponseFormat::JsonObject,
        ResponseFormat::JsonSchema {
            schema: serde_json::json!({"type": "object"}),
        },
    ];

    for response_format in cases {
        let mut request = LlmRequest::new("gemini-2.0-flash");
        request.response_format = Some(response_format);

        let encoded = gemini_codec().encode_request(&request).unwrap();
        assert_eq!(
            encoded.body_json["generationConfig"]["responseMimeType"],
            "application/json"
        );
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
        let mut request = LlmRequest::new("gpt-4o");
        request.tool_choice = Some(tool_choice);
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
        let mut request = LlmRequest::new("gemini-2.0-flash");
        request.tool_choice = Some(tool_choice);
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
