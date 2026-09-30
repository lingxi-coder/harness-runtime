use crate::upstream::codec_fixtures::HistoryFixture;
use crate::upstream::codec_fixtures::{AnthropicMessagesCodec, FixtureCodec};
use llm_runtime::{
    ContentBlock, HistoryContentDelta, HistoryEvent, Message, ProviderResponse, ToolChoice,
    ToolDeclaration,
};

// ── ImageUrl encode test ──────────────────────────────────────────────────────

#[test]
fn encode_image_url_block_emits_url_source() {
    let codec = AnthropicMessagesCodec::new("https://api.anthropic.com", "2023-06-01");
    let mut request = HistoryFixture::new("claude-sonnet-4-20250514");
    request.messages.push(Message {
        role: "user".to_string(),
        content: vec![ContentBlock::ImageUrl {
            url: "https://x/y.png".to_string(),
        }],
    });
    let provider_request = codec.encode_request(&request).unwrap();
    let block = &provider_request.body_json["messages"][0]["content"][0];
    assert_eq!(block["type"], "image");
    assert_eq!(block["source"]["type"], "url");
    assert_eq!(block["source"]["url"], "https://x/y.png");
}

// ── Task 1: extended block decode tests ──────────────────────────────────────

#[test]
fn decode_server_tool_use_block() {
    let codec = AnthropicMessagesCodec::new("https://api.anthropic.com", "2023-06-01");
    let response = ProviderResponse::json(
        200,
        serde_json::json!({
            "id": "msg_stu",
            "model": "claude-sonnet-4-20250514",
            "content": [{"type":"server_tool_use","id":"stu_01","name":"advisor","input":{"query":"?"}}],
            "usage": {"input_tokens": 1, "output_tokens": 1}
        }),
    );
    let decoded = codec.decode_response(response).unwrap();
    assert!(matches!(
        &decoded.content[0],
        ContentBlock::ServerToolUse { id, name, input } if id == "stu_01" && name == "advisor" && input["query"] == "?"
    ));
}

#[test]
fn decode_connector_text_block() {
    let codec = AnthropicMessagesCodec::new("https://api.anthropic.com", "2023-06-01");
    let response = ProviderResponse::json(
        200,
        serde_json::json!({
            "id": "msg_ct",
            "model": "claude-sonnet-4-20250514",
            "content": [{"type":"connector_text","connector_text":"[connector] hello","signature":"ct-sig"}],
            "usage": {"input_tokens": 1, "output_tokens": 1}
        }),
    );
    let decoded = codec.decode_response(response).unwrap();
    assert!(matches!(
        &decoded.content[0],
        ContentBlock::ConnectorText { connector_text, signature }
            if connector_text == "[connector] hello" && signature.as_deref() == Some("ct-sig")
    ));
}

#[test]
fn decode_advisor_tool_result_block() {
    let codec = AnthropicMessagesCodec::new("https://api.anthropic.com", "2023-06-01");
    let response = ProviderResponse::json(
        200,
        serde_json::json!({
            "id": "msg_atr",
            "model": "claude-sonnet-4-20250514",
            "content": [{"type":"advisor_tool_result","tool_use_id":"stu_01","content":"result text","is_error":false}],
            "usage": {"input_tokens": 1, "output_tokens": 1}
        }),
    );
    let decoded = codec.decode_response(response).unwrap();
    assert!(matches!(
        &decoded.content[0],
        ContentBlock::AdvisorToolResult { tool_use_id, content, is_error }
            if tool_use_id == "stu_01" && content == "result text" && !is_error
    ));
}

#[test]
fn stream_citations_delta_decodes() {
    let codec = AnthropicMessagesCodec::new("https://api.anthropic.com", "2023-06-01");
    let mut decoder = codec.stream_decoder();
    decoder.decode_frame(llm_runtime::RawStreamFrame::new(br#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"answer"}}"#.to_vec())).unwrap();
    let events = decoder.decode_frame(llm_runtime::RawStreamFrame::new(
        br#"{"type":"content_block_delta","index":0,"delta":{"type":"citations_delta","citation":{"url":"https://x","title":"X"}}}"#.to_vec(),
    )).unwrap();
    assert!(matches!(
        &events[0],
        HistoryEvent::ContentBlockDelta {
            index: 0,
            delta: HistoryContentDelta::CitationsDelta { .. }
        }
    ));
}

#[test]
fn stream_connector_text_delta_decodes() {
    let codec = AnthropicMessagesCodec::new("https://api.anthropic.com", "2023-06-01");
    let mut decoder = codec.stream_decoder();
    let mut events = Vec::new();
    for value in [
        serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"connector_text","connector_text":"initial","signature":"sig"}}),
        serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"connector_text_delta","connector_text":" more text"}}),
        serde_json::json!({"type":"content_block_stop","index":0}),
    ] {
        events.extend(
            decoder
                .decode_frame(llm_runtime::RawStreamFrame::new(
                    serde_json::to_vec(&value).unwrap(),
                ))
                .unwrap(),
        );
    }
    assert!(events.iter().any(|event| matches!(event, HistoryEvent::ContentBlockStart { content_block:ContentBlock::ConnectorText { connector_text, signature }, .. } if connector_text == "initial more text" && signature.as_deref() == Some("sig"))));
}

#[test]
fn sdk_response_inference_reports_fast_service_tier() {
    let value = serde_json::json!({
        "input_tokens": 100,
        "output_tokens": 50,
        "speed": "fast"
    });
    use lingxi_llm_client::{self as sdk, WireCodec};
    let profile = serde_json::from_value(serde_json::json!({
        "provider_id":"anthropic", "profile_name":"anthropic", "base_url":"https://api.anthropic.com",
        "protocol":sdk::protocol::ProtocolFamily::AnthropicMessages, "auth":"none", "models":[]
    })).unwrap();
    let response = sdk::HttpResponse {
        status: 200,
        headers: vec![],
        body: serde_json::to_vec(&serde_json::json!({"usage":value}))
            .unwrap()
            .into(),
    };
    let inference = sdk::AnthropicMessagesCodec.response_inference(
        &response,
        &sdk::CodecContext::new(&profile, "model", sdk::RequestMode::Complete),
    );
    assert_eq!(
        inference.service_tier,
        Some(lingxi_llm_client::protocol::ServiceTier::Fast)
    );
}

#[test]
fn usage_speed_absent_is_none() {
    let value = serde_json::json!({"input_tokens": 10, "output_tokens": 5});
    use lingxi_llm_client::{self as sdk, WireCodec};
    let profile = serde_json::from_value(serde_json::json!({
        "provider_id":"anthropic", "profile_name":"anthropic", "base_url":"https://api.anthropic.com",
        "protocol":sdk::protocol::ProtocolFamily::AnthropicMessages, "auth":"none", "models":[]
    })).unwrap();
    let response = sdk::HttpResponse {
        status: 200,
        headers: vec![],
        body: serde_json::to_vec(&serde_json::json!({"usage":value}))
            .unwrap()
            .into(),
    };
    let inference = sdk::AnthropicMessagesCodec.response_inference(
        &response,
        &sdk::CodecContext::new(&profile, "model", sdk::RequestMode::Complete),
    );
    assert!(inference.service_tier.is_none());
}

#[test]
fn server_tool_use_round_trip_encode() {
    let codec = AnthropicMessagesCodec::new("https://api.anthropic.com", "2023-06-01");
    let mut request = HistoryFixture::new("claude-sonnet-4-20250514");
    request.messages.push(Message {
        role: "assistant".to_string(),
        content: vec![ContentBlock::ServerToolUse {
            id: "stu_01".to_string(),
            name: "advisor".to_string(),
            input: serde_json::json!({"query": "?"}),
        }],
    });
    let provider_request = codec.encode_request(&request).unwrap();
    let block = &provider_request.body_json["messages"][0]["content"][0];
    assert_eq!(block["type"], "server_tool_use");
    assert_eq!(block["id"], "stu_01");
    assert_eq!(block["name"], "advisor");
    assert_eq!(block["input"]["query"], "?");
}

#[test]
fn connector_text_encode_preserves_native_content() {
    let codec = AnthropicMessagesCodec::new("https://api.anthropic.com", "2023-06-01");
    let mut request = HistoryFixture::new("claude-sonnet-4-20250514");
    request.messages.push(Message {
        role: "assistant".to_string(),
        content: vec![ContentBlock::ConnectorText {
            connector_text: "hi".to_string(),
            signature: None,
        }],
    });
    let encoded = codec.encode_request(&request).unwrap();
    assert_eq!(
        encoded.body_json["messages"][0]["content"][0]["connector_text"],
        "hi"
    );
}

#[test]
fn advisor_tool_result_encode_preserves_native_content() {
    let codec = AnthropicMessagesCodec::new("https://api.anthropic.com", "2023-06-01");
    let mut request = HistoryFixture::new("claude-sonnet-4-20250514");
    request.messages.push(Message {
        role: "user".to_string(),
        content: vec![ContentBlock::AdvisorToolResult {
            tool_use_id: "stu_01".to_string(),
            content: serde_json::json!("result"),
            is_error: false,
        }],
    });
    let encoded = codec.encode_request(&request).unwrap();
    assert_eq!(
        encoded.body_json["messages"][0]["content"][0]["content"],
        "result"
    );
}

#[test]
fn encode_request_shape_is_anthropic_messages() {
    let codec = AnthropicMessagesCodec::new("https://api.anthropic.com", "2023-06-01");
    let mut request = HistoryFixture::new("claude-sonnet-4-20250514");
    request.system = vec![llm_runtime::SystemBlock::text("sys")];
    request.messages.push(Message {
        role: "user".to_string(),
        content: vec![ContentBlock::Text {
            text: "hello".to_string(),
            cache_control: None,
        }],
    });
    request.tools.push(ToolDeclaration {
        name: "Read".to_string(),
        description: "Read a file".to_string(),
        input_schema: serde_json::json!({"type":"object"}),
        ..Default::default()
    });

    let provider_request = codec.encode_request(&request).unwrap();

    assert_eq!(provider_request.method, "POST");
    assert!(provider_request.url.ends_with("/v1/messages"));
    assert_eq!(provider_request.headers["anthropic-version"], "2023-06-01");
    assert_eq!(
        provider_request.body_json["model"],
        "claude-sonnet-4-20250514"
    );
    assert_eq!(provider_request.body_json["system"][0]["text"], "sys");
    assert_eq!(provider_request.body_json["messages"][0]["role"], "user");
    assert_eq!(
        provider_request.body_json["tools"][0]["input_schema"]["type"],
        "object"
    );
}

#[test]
fn encode_request_hosted_computer_use_tool_passthrough_and_beta_header() {
    // codex `liter-llm` parity: a hosted `computer_use_20250124` tool is
    // emitted as a typed passthrough (`type`/`name` + extra wire fields) and
    // the request carries `anthropic-beta: computer-use-2025-01-24`.
    let codec = AnthropicMessagesCodec::new("https://api.anthropic.com", "2023-06-01");
    let mut request = HistoryFixture::new("claude-sonnet-4-20250514");
    request.messages.push(Message {
        role: "user".to_string(),
        content: vec![ContentBlock::Text {
            text: "use the computer".to_string(),
            cache_control: None,
        }],
    });
    let mut extra = serde_json::Map::new();
    extra.insert("display_width_px".to_string(), serde_json::json!(1024));
    extra.insert("display_height_px".to_string(), serde_json::json!(768));
    request.tools.push(ToolDeclaration {
        name: "computer".to_string(),
        description: String::new(),
        input_schema: serde_json::Value::Null,
        tool_type: Some("computer_use_20250124".to_string()),
        extra,
        strict: false,
        defer_loading: false,
    });

    let provider_request = codec.encode_request(&request).unwrap();

    let tool = &provider_request.body_json["tools"][0];
    assert_eq!(tool["type"], "computer_use_20250124");
    assert_eq!(tool["name"], "computer");
    assert_eq!(tool["display_width_px"], 1024);
    assert_eq!(tool["display_height_px"], 768);
    // Hosted-tool passthrough does not emit a caller `input_schema`.
    assert!(tool.get("input_schema").is_none());
    assert_eq!(
        provider_request.headers["anthropic-beta"],
        "computer-use-2025-01-24"
    );
}

#[test]
fn encode_request_no_beta_header_without_hosted_tool() {
    // A caller-defined tool must not trigger the computer-use beta header.
    let codec = AnthropicMessagesCodec::new("https://api.anthropic.com", "2023-06-01");
    let mut request = HistoryFixture::new("claude-sonnet-4-20250514");
    request.messages.push(Message {
        role: "user".to_string(),
        content: vec![ContentBlock::Text {
            text: "hi".to_string(),
            cache_control: None,
        }],
    });
    request.tools.push(ToolDeclaration {
        name: "Read".to_string(),
        description: "Read a file".to_string(),
        input_schema: serde_json::json!({"type":"object"}),
        ..Default::default()
    });

    let provider_request = codec.encode_request(&request).unwrap();
    assert!(provider_request.headers.get("anthropic-beta").is_none());
}

#[test]
fn encode_request_maps_image_and_tool_result_blocks() {
    let codec = AnthropicMessagesCodec::new("https://api.anthropic.com", "2023-06-01");
    let mut request = HistoryFixture::new("claude-sonnet-4-20250514");
    request.messages.push(Message {
        role: "user".to_string(),
        content: vec![
            ContentBlock::Image {
                media_type: "image/png".to_string(),
                bytes: vec![1, 2, 3],
            },
            ContentBlock::ToolResult {
                tool_call_id: "tool-1".to_string(),
                output: serde_json::json!({"ok": true}),
                is_error: false,
                cache_control: None,
                cache_reference: None,
            },
        ],
    });

    let provider_request = codec.encode_request(&request).unwrap();

    assert_eq!(
        provider_request.body_json["messages"][0]["content"][0]["type"],
        "image"
    );
    assert_eq!(
        provider_request.body_json["messages"][0]["content"][0]["source"]["type"],
        "base64"
    );
    assert_eq!(
        provider_request.body_json["messages"][0]["content"][0]["source"]["media_type"],
        "image/png"
    );
    assert_eq!(
        provider_request.body_json["messages"][0]["content"][0]["source"]["data"],
        "AQID"
    );
    assert_eq!(
        provider_request.body_json["messages"][0]["content"][1]["type"],
        "tool_result"
    );
    assert_eq!(
        provider_request.body_json["messages"][0]["content"][1]["tool_use_id"],
        "tool-1"
    );
    assert_eq!(
        provider_request.body_json["messages"][0]["content"][1]["content"],
        "{\"ok\":true}"
    );
}

#[test]
fn encode_request_passes_tool_result_content_block_array_verbatim() {
    // An MCP result whose `output` is a content-block ARRAY (e.g. text + image)
    // is sent VERBATIM as the Anthropic `tool_result.content` — claude-code
    // passes the MCP content array directly (images stay viewable), NOT
    // stringified. (An object output, above, still stringifies.)
    let codec = AnthropicMessagesCodec::new("https://api.anthropic.com", "2023-06-01");
    let mut request = HistoryFixture::new("claude-sonnet-4-20250514");
    request.messages.push(Message {
        role: "user".to_string(),
        content: vec![ContentBlock::ToolResult {
            tool_call_id: "tool-1".to_string(),
            output: serde_json::json!([
                { "type": "text", "text": "see image:" },
                { "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": "AQID" } },
            ]),
            is_error: false,
            cache_control: None,
            cache_reference: None,
        }],
    });
    let req = codec.encode_request(&request).unwrap();
    let content = &req.body_json["messages"][0]["content"][0]["content"];
    assert!(
        content.is_array(),
        "array output must stay an array, got {content:?}"
    );
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[0]["text"], "see image:");
    assert_eq!(content[1]["type"], "image");
    assert_eq!(content[1]["source"]["data"], "AQID");
}

#[test]
fn encode_request_emits_cache_edits_and_cache_reference() {
    // 1P experimental cache-editing wire shape (claude.ts:3052-3055, 3201-3203):
    // a tool_result carrying cache_reference + a cache_edits delete block.
    let codec = AnthropicMessagesCodec::new("https://api.anthropic.com", "2023-06-01");
    let mut request = HistoryFixture::new("claude-sonnet-4-20250514");
    request.messages.push(Message {
        role: "user".to_string(),
        content: vec![
            ContentBlock::ToolResult {
                tool_call_id: "toolu_abc".to_string(),
                output: serde_json::json!("body"),
                is_error: false,
                cache_control: None,
                cache_reference: Some("toolu_abc".to_string()),
            },
            ContentBlock::CacheEdits {
                edits: vec![llm_runtime::CacheEdit::Delete {
                    cache_reference: "toolu_xyz".to_string(),
                }],
            },
        ],
    });

    let provider_request = codec.encode_request(&request).unwrap();
    let content = &provider_request.body_json["messages"][0]["content"];

    // tool_result carries cache_reference.
    assert_eq!(content[0]["type"], "tool_result");
    assert_eq!(content[0]["cache_reference"], "toolu_abc");

    // cache_edits block: {type:'cache_edits', edits:[{type:'delete', cache_reference}]}.
    assert_eq!(
        content[1],
        serde_json::json!({
            "type": "cache_edits",
            "edits": [{"type": "delete", "cache_reference": "toolu_xyz"}],
        })
    );
}

#[test]
fn encode_request_omits_cache_reference_when_absent() {
    // Default path: cache_reference None → the key is absent (byte-unchanged).
    let codec = AnthropicMessagesCodec::new("https://api.anthropic.com", "2023-06-01");
    let mut request = HistoryFixture::new("claude-sonnet-4-20250514");
    request.messages.push(Message {
        role: "user".to_string(),
        content: vec![ContentBlock::ToolResult {
            tool_call_id: "toolu_abc".to_string(),
            output: serde_json::json!("body"),
            is_error: false,
            cache_control: None,
            cache_reference: None,
        }],
    });
    let provider_request = codec.encode_request(&request).unwrap();
    let tr = &provider_request.body_json["messages"][0]["content"][0];
    assert_eq!(tr["type"], "tool_result");
    assert!(
        tr.get("cache_reference").is_none(),
        "no cache_reference key by default"
    );
}

#[test]
fn encode_request_maps_supported_tool_choice_variants() {
    let codec = AnthropicMessagesCodec::new("https://api.anthropic.com", "2023-06-01");

    let cases = [
        (ToolChoice::Auto, "auto", None::<&str>),
        (ToolChoice::None, "none", None::<&str>),
        (ToolChoice::Required, "any", None::<&str>),
        (
            ToolChoice::Tool {
                name: "Read".to_string(),
            },
            "tool",
            Some("Read"),
        ),
    ];

    for (choice, expected_type, expected_name) in cases {
        let mut request = HistoryFixture::new("claude-sonnet-4-20250514");
        request.tools.push(ToolDeclaration {
            name: "Read".into(),
            input_schema: serde_json::json!({"type":"object"}),
            ..Default::default()
        });
        request.request.set_tool_choice(Some(choice));

        let provider_request = codec.encode_request(&request).unwrap();

        assert_eq!(
            provider_request.body_json["tool_choice"]["type"],
            expected_type
        );
        if let Some(name) = expected_name {
            assert_eq!(provider_request.body_json["tool_choice"]["name"], name);
        }
    }
}

#[test]
fn encode_request_rejects_response_format_explicitly() {
    let codec = AnthropicMessagesCodec::new("https://api.anthropic.com", "2023-06-01");

    let mut response_format_request = HistoryFixture::new("claude-sonnet-4-20250514");
    response_format_request.request.input.output_format =
        lingxi_llm_client::protocol::OutputFormat::JsonObject;

    let response_format_err = codec.encode_request(&response_format_request).unwrap_err();
    assert!(matches!(
        response_format_err,
        llm_runtime::LlmError::UnsupportedCapability { .. }
    ));
}

#[test]
fn encode_omits_empty_tools_array() {
    let codec = AnthropicMessagesCodec::new("https://api.anthropic.com", "2023-06-01");

    let bare = codec
        .encode_request(&HistoryFixture::new("claude-sonnet-4-20250514"))
        .unwrap();
    assert!(bare.body_json.get("tools").is_none());

    let mut with_tools = HistoryFixture::new("claude-sonnet-4-20250514");
    with_tools.tools.push(ToolDeclaration {
        name: "Read".to_string(),
        description: "d".to_string(),
        input_schema: serde_json::json!({"type":"object"}),
        ..Default::default()
    });
    let encoded = codec.encode_request(&with_tools).unwrap();
    assert_eq!(encoded.body_json["tools"][0]["name"], "Read");
    // Default (strict == false) omits the `strict` field entirely — wire bytes
    // are the plain `{name, description, input_schema}` object.
    assert!(encoded.body_json["tools"][0].get("strict").is_none());
}

#[test]
fn encode_strict_tool_sends_converted_schema_and_strict_flag() {
    let codec = AnthropicMessagesCodec::new("https://api.anthropic.com", "2023-06-01");
    let mut request = HistoryFixture::new("claude-sonnet-4-20250514");
    request.tools.push(ToolDeclaration {
        name: "StructuredOutput".to_string(),
        description: "emit".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": { "answer": { "type": "string" } },
            "required": ["answer"]
        }),
        strict: true,
        ..Default::default()
    });
    let tool = &codec.encode_request(&request).unwrap().body_json["tools"][0];
    assert_eq!(tool["strict"], serde_json::json!(true));
    // The converted schema closes the object with additionalProperties:false.
    assert_eq!(
        tool["input_schema"]["additionalProperties"],
        serde_json::json!(false)
    );
}

#[test]
fn encode_strict_tool_with_bad_schema_falls_back_to_non_strict() {
    let codec = AnthropicMessagesCodec::new("https://api.anthropic.com", "2023-06-01");
    let mut request = HistoryFixture::new("claude-sonnet-4-20250514");
    // A root that is not an object cannot be made strict → non-strict fallback.
    let schema = serde_json::json!({ "type": "string" });
    request.tools.push(ToolDeclaration {
        name: "Bad".to_string(),
        description: "d".to_string(),
        input_schema: schema.clone(),
        strict: true,
        ..Default::default()
    });
    let tool = &codec.encode_request(&request).unwrap().body_json["tools"][0];
    assert!(tool.get("strict").is_none());
    assert_eq!(tool["input_schema"], schema);
}

#[test]
fn encode_request_pins_default_max_tokens_to_4096() {
    let codec = AnthropicMessagesCodec::new("https://api.anthropic.com", "2023-06-01");
    let request = HistoryFixture::new("claude-sonnet-4-20250514");

    let provider_request = codec.encode_request(&request).unwrap();

    assert_eq!(provider_request.body_json["max_tokens"], 4096);
}

#[test]
fn decode_text_response_maps_usage_and_stop_reason() {
    let codec = AnthropicMessagesCodec::new("https://api.anthropic.com", "2023-06-01");
    let response = ProviderResponse::json(
        200,
        serde_json::json!({
            "id": "msg_1",
            "model": "claude-sonnet-4-20250514",
            "content": [{"type":"text","text":"hi"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 9, "output_tokens": 3}
        }),
    );

    let decoded = codec.decode_response(response).unwrap();

    assert_eq!(decoded.id, "msg_1");
    assert_eq!(decoded.usage.counts().input_tokens, 9);
    assert_eq!(decoded.usage.counts().output_tokens, 3);
    assert_eq!(decoded.stop_reason.as_deref(), Some("end_turn"));
}

#[test]
fn encode_thinking_round_trip_requires_signature() {
    let codec = AnthropicMessagesCodec::new("https://api.anthropic.com", "2023-06-01");

    let mut signed = HistoryFixture::new("claude-sonnet-4-20250514");
    signed.messages.push(Message {
        role: "assistant".to_string(),
        content: vec![ContentBlock::Reasoning {
            text: "pondering".to_string(),
            signature: Some("sig_abc".to_string()),
        }],
    });
    let provider_request = codec.encode_request(&signed).unwrap();
    let block = &provider_request.body_json["messages"][0]["content"][0];
    assert_eq!(block["type"], "thinking");
    assert_eq!(block["thinking"], "pondering");
    assert_eq!(block["signature"], "sig_abc");

    let mut unsigned = HistoryFixture::new("claude-sonnet-4-20250514");
    unsigned.messages.push(Message {
        role: "assistant".to_string(),
        content: vec![ContentBlock::Reasoning {
            text: "pondering".to_string(),
            signature: None,
        }],
    });
    assert!(matches!(
        codec.encode_request(&unsigned).unwrap_err(),
        llm_runtime::LlmError::InvalidRequest { .. }
    ));
}

#[test]
fn redacted_thinking_round_trips() {
    let codec = AnthropicMessagesCodec::new("https://api.anthropic.com", "2023-06-01");
    let response = ProviderResponse::json(
        200,
        serde_json::json!({
            "id": "msg_5",
            "model": "claude-sonnet-4-20250514",
            "content": [{"type":"redacted_thinking","data":"opaque-bytes"}],
            "usage": {"input_tokens": 1, "output_tokens": 1}
        }),
    );

    let decoded = codec.decode_response(response).unwrap();
    assert!(matches!(
        decoded.content.as_slice(),
        [ContentBlock::RedactedThinking { data }] if data == "opaque-bytes"
    ));

    let mut request = HistoryFixture::new("claude-sonnet-4-20250514");
    request.messages.push(Message {
        role: "assistant".to_string(),
        content: decoded.content,
    });
    let provider_request = codec.encode_request(&request).unwrap();
    let block = &provider_request.body_json["messages"][0]["content"][0];
    assert_eq!(block["type"], "redacted_thinking");
    assert_eq!(block["data"], "opaque-bytes");
}

#[test]
fn stream_signature_delta_maps_to_signature_delta() {
    let codec = AnthropicMessagesCodec::new("https://api.anthropic.com", "2023-06-01");
    let mut decoder = codec.stream_decoder();

    let events = decoder.decode_frame(llm_runtime::RawStreamFrame::new(
        br#"{"type":"content_block_delta","index":1,"delta":{"type":"signature_delta","signature":"sig_abc"}}"#.to_vec(),
    )).unwrap();

    assert!(matches!(
        events.last().unwrap(),
        HistoryEvent::ContentBlockDelta {
            index: 1,
            delta: HistoryContentDelta::SignatureDelta { signature },
        } if signature == "sig_abc"
    ));
}

#[test]
fn encode_tool_result_error_flag() {
    let codec = AnthropicMessagesCodec::new("https://api.anthropic.com", "2023-06-01");
    let mut request = HistoryFixture::new("claude-sonnet-4-20250514");
    request.messages.push(Message {
        role: "user".to_string(),
        content: vec![
            ContentBlock::ToolResult {
                tool_call_id: "tool-1".to_string(),
                output: serde_json::json!("boom"),
                is_error: true,
                cache_control: None,
                cache_reference: None,
            },
            ContentBlock::ToolResult {
                tool_call_id: "tool-2".to_string(),
                output: serde_json::json!("fine"),
                is_error: false,
                cache_control: None,
                cache_reference: None,
            },
        ],
    });

    let provider_request = codec.encode_request(&request).unwrap();
    let content = &provider_request.body_json["messages"][0]["content"];

    assert_eq!(content[0]["is_error"], true);
    assert!(content[1].get("is_error").is_none());
}

#[test]
fn encode_sampling_controls() {
    let codec = AnthropicMessagesCodec::new("https://api.anthropic.com", "2023-06-01");
    let mut request = HistoryFixture::new("claude-sonnet-4-20250514");
    request.request.input.max_tokens = Some(1024);
    request.request.input.temperature = Some(0.5);
    request.request.input.controls.top_p = Some(0.9);
    request.request.input.stop_sequences = vec!["END".to_string()];

    let provider_request = codec.encode_request(&request).unwrap();

    assert_eq!(provider_request.body_json["max_tokens"], 1024);
    assert_eq!(provider_request.body_json["temperature"], 0.5);
    assert_eq!(provider_request.body_json["top_p"], 0.9);
    assert_eq!(
        provider_request.body_json["stop_sequences"],
        serde_json::json!(["END"])
    );

    let bare = codec
        .encode_request(&HistoryFixture::new("claude-sonnet-4-20250514"))
        .unwrap();
    assert!(bare.body_json.get("temperature").is_none());
    assert!(bare.body_json.get("top_p").is_none());
    assert!(bare.body_json.get("stop_sequences").is_none());
}

#[test]
fn decode_tool_use_response_maps_tool_call_block() {
    let codec = AnthropicMessagesCodec::new("https://api.anthropic.com", "2023-06-01");
    let response = ProviderResponse::json(
        200,
        serde_json::json!({
            "id": "msg_2",
            "model": "claude-sonnet-4-20250514",
            "content": [{"type":"tool_use","id":"tool_1","name":"Read","input":{"path":"foo.txt"}}],
            "usage": {"input_tokens": 1, "output_tokens": 1}
        }),
    );

    let decoded = codec.decode_response(response).unwrap();

    assert!(
        matches!(decoded.content[0], ContentBlock::ToolCall { ref id, ref name, .. } if id == "tool_1" && name == "Read")
    );
}

#[test]
fn stream_decoder_maps_text_delta_and_rejects_garbage() {
    let codec = AnthropicMessagesCodec::new("https://api.anthropic.com", "2023-06-01");
    let mut decoder = codec.stream_decoder();

    let events = decoder.decode_frame(llm_runtime::RawStreamFrame::new(
        br#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#.to_vec(),
    )).unwrap();

    assert!(matches!(
        events.last().unwrap(),
        HistoryEvent::ContentBlockDelta { index: 0, delta: HistoryContentDelta::TextDelta { text } } if text == "hi"
    ));
    assert!(decoder
        .decode_frame(llm_runtime::RawStreamFrame::new(b"not json".to_vec()))
        .is_err());
}

#[test]
fn stream_decoder_covers_required_event_paths_and_reasoning_blocks() {
    let codec = AnthropicMessagesCodec::new("https://api.anthropic.com", "2023-06-01");
    let mut decoder = codec.stream_decoder();
    let mut events = Vec::new();
    for value in [
        serde_json::json!({"type":"message_start","message":{"id":"msg","model":"claude","usage":{"input_tokens":2,"output_tokens":0}}}),
        serde_json::json!({"type":"content_block_start","index":1,"content_block":{"type":"thinking","thinking":""}}),
        serde_json::json!({"type":"content_block_delta","index":1,"delta":{"type":"thinking_delta","thinking":"ponder"}}),
        serde_json::json!({"type":"content_block_delta","index":1,"delta":{"type":"signature_delta","signature":"sig"}}),
        serde_json::json!({"type":"content_block_stop","index":1}),
        serde_json::json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}),
        serde_json::json!({"type":"message_stop"}),
    ] {
        events.extend(
            decoder
                .decode_frame(llm_runtime::RawStreamFrame::new(
                    serde_json::to_vec(&value).unwrap(),
                ))
                .unwrap(),
        );
    }
    assert!(
        matches!(events.first(),Some(HistoryEvent::MessageStart { response }) if response.id == "msg")
    );
    assert!(events.iter().any(|e| matches!(e,HistoryEvent::ContentBlockDelta { delta:HistoryContentDelta::ThinkingDelta { thinking }, .. } if thinking == "ponder")));
    assert!(events.iter().any(|e| matches!(e,HistoryEvent::ContentBlockDelta { delta:HistoryContentDelta::SignatureDelta { signature }, .. } if signature == "sig")));
    assert!(matches!(events.last(), Some(HistoryEvent::MessageStop)));
    assert!(decoder.finish().unwrap().is_empty());
    assert_eq!(
        decoder.observed_usage().unwrap().1,
        llm_runtime::ModelAttemptUsageCompleteness::Complete
    );
}

#[test]
fn decode_preserves_unknown_content_block_types() {
    let codec = AnthropicMessagesCodec::new("https://api.anthropic.com", "2023-06-01");
    let response = ProviderResponse::json(
        200,
        serde_json::json!({
            "id": "msg_4",
            "model": "claude-sonnet-4-20250514",
            "content": [
                {"type":"some_future_block_type","data":"opaque"},
                {"type":"text","text":"hi"}
            ],
            "usage": {"input_tokens": 1, "output_tokens": 1}
        }),
    );

    let decoded = codec.decode_response(response).unwrap();

    assert!(
        matches!(decoded.content.as_slice(), [ContentBlock::ProviderContent { value, .. }, ContentBlock::Text { text, .. }] if text == "hi" && value["data"] == "opaque")
    );
    assert_eq!(
        decoded.provider_metadata["content"][0]["type"],
        "some_future_block_type"
    );
}

#[test]
fn stream_decoder_preserves_hosted_observations_without_fabricating_deltas() {
    let codec = AnthropicMessagesCodec::new("https://api.anthropic.com", "2023-06-01");
    let mut decoder = codec.stream_decoder();

    let unknown_event = decoder
        .decode_frame(llm_runtime::RawStreamFrame::new(
            br#"{"type":"some_future_event","payload":{}}"#.to_vec(),
        ))
        .unwrap();
    assert!(unknown_event.is_empty());

    let unknown_block_start = decoder.decode_frame(llm_runtime::RawStreamFrame::new(
        br#"{"type":"content_block_start","index":0,"content_block":{"type":"web_search_tool_result","content":[]}}"#.to_vec(),
    )).unwrap();
    assert!(
        unknown_block_start.iter().any(|event| matches!(event,
            llm_runtime::HistoryEvent::WebSearch { result }
                if result.metadata["events"][0]["content_block"]["type"] == "web_search_tool_result"
        )),
        "{unknown_block_start:?}"
    );
    assert!(!unknown_block_start.iter().any(|event| matches!(
        event,
        llm_runtime::HistoryEvent::ContentBlockStart {
            content_block: ContentBlock::ToolCall { .. },
            ..
        }
    )));

    let unknown_delta = decoder.decode_frame(llm_runtime::RawStreamFrame::new(
        br#"{"type":"content_block_delta","index":0,"delta":{"type":"some_future_delta_type","data":"x"}}"#.to_vec(),
    )).unwrap();
    assert!(
        unknown_delta
            .iter()
            .all(|event| matches!(event, llm_runtime::HistoryEvent::WebSearch { .. })),
        "{unknown_delta:?}"
    );
    assert!(
        unknown_delta.iter().any(
            |event| matches!(event, llm_runtime::HistoryEvent::WebSearch { result }
                if result.metadata["events"][0]["delta"]["type"] == "some_future_delta_type"
            )
        ),
        "{unknown_delta:?}"
    );
}

#[test]
fn stream_error_events_map_to_error_taxonomy() {
    let codec = AnthropicMessagesCodec::new("https://api.anthropic.com", "2023-06-01");
    let mut decoder = codec.stream_decoder();
    assert!(matches!(
        decoder.decode_frame(llm_runtime::RawStreamFrame::new(
            br#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#
                .to_vec(),
        )),
        Err(llm_runtime::LlmError::Overloaded { .. })
    ));
    let mut decoder = codec.stream_decoder();
    assert!(matches!(
        decoder.decode_frame(llm_runtime::RawStreamFrame::new(
            br#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#
                .to_vec(),
        )),
        Err(llm_runtime::LlmError::RateLimited { .. })
    ));
    let mut decoder = codec.stream_decoder();
    assert!(matches!(
        decoder.decode_frame(llm_runtime::RawStreamFrame::new(
            br#"{"type":"error","error":{"type":"authentication_error","message":"bad key"}}"#
                .to_vec(),
        )),
        Err(llm_runtime::LlmError::Authentication { .. })
    ));
    let mut decoder = codec.stream_decoder();
    assert!(matches!(
        decoder.decode_frame(llm_runtime::RawStreamFrame::new(
            br#"{"type":"error","error":{"type":"invalid_request_error","message":"bad request"}}"#.to_vec(),
        )),
        Err(llm_runtime::LlmError::InvalidRequest { message }) if message.contains("bad request")
    ));
}

#[test]
fn encode_reasoning_budget_as_thinking() {
    let codec = AnthropicMessagesCodec::new("https://api.anthropic.com", "2023-06-01");
    let mut request = HistoryFixture::new("claude-sonnet-4-20250514");
    request
        .request
        .set_reasoning(Some(llm_runtime::ReasoningConfig::Enabled {
            budget_tokens: 2048,
        }));

    let provider_request = codec.encode_request(&request).unwrap();

    assert_eq!(provider_request.body_json["thinking"]["type"], "enabled");
    assert_eq!(
        provider_request.body_json["thinking"]["budget_tokens"],
        2048
    );

    let bare = codec
        .encode_request(&HistoryFixture::new("claude-sonnet-4-20250514"))
        .unwrap();
    assert!(bare.body_json.get("thinking").is_none());
}

#[test]
fn encode_adaptive_reasoning_as_thinking() {
    let codec = AnthropicMessagesCodec::new("https://api.anthropic.com", "2023-06-01");
    let mut request = HistoryFixture::new("claude-opus-4-8");
    request
        .request
        .set_reasoning(Some(llm_runtime::ReasoningConfig::Adaptive));

    let provider_request = codec.encode_request(&request).unwrap();

    assert_eq!(provider_request.body_json["thinking"]["type"], "adaptive");
    assert!(provider_request.body_json["thinking"]
        .get("budget_tokens")
        .is_none());
}

#[test]
fn encode_metadata_user_id() {
    let codec = AnthropicMessagesCodec::new("https://api.anthropic.com", "2023-06-01");
    let mut request = HistoryFixture::new("claude-opus-4-8");
    request.request.input.metadata = serde_json::json!({
        "user_id": "{\"device_id\":\"abc\",\"session_id\":\"s1\"}",
    });

    let provider_request = codec.encode_request(&request).unwrap();

    assert_eq!(
        provider_request.body_json["metadata"]["user_id"],
        "{\"device_id\":\"abc\",\"session_id\":\"s1\"}"
    );

    // Absent metadata → no key.
    let bare = codec
        .encode_request(&HistoryFixture::new("claude-opus-4-8"))
        .unwrap();
    assert!(bare.body_json.get("metadata").is_none());
}

#[test]
fn encode_system_blocks_and_cache_control() {
    let codec = AnthropicMessagesCodec::new("https://api.anthropic.com", "2023-06-01");
    let mut request = HistoryFixture::new("claude-sonnet-4-20250514");
    request.system = vec![
        llm_runtime::SystemBlock {
            text: "stable prefix".to_string(),
            cache_control: Some(llm_runtime::CacheControl::Ephemeral),
        },
        llm_runtime::SystemBlock {
            text: "tail".to_string(),
            cache_control: None,
        },
    ];
    request.messages.push(Message {
        role: "user".to_string(),
        content: vec![
            ContentBlock::Text {
                text: "hello".to_string(),
                cache_control: Some(llm_runtime::CacheControl::Ephemeral),
            },
            ContentBlock::ToolResult {
                tool_call_id: "tool-1".to_string(),
                output: serde_json::json!("ok"),
                is_error: false,
                cache_control: Some(llm_runtime::CacheControl::Ephemeral),
                cache_reference: None,
            },
        ],
    });

    let provider_request = codec.encode_request(&request).unwrap();
    let body = &provider_request.body_json;

    assert_eq!(body["system"][0]["type"], "text");
    assert_eq!(body["system"][0]["text"], "stable prefix");
    assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
    assert!(body["system"][1].get("cache_control").is_none());
    assert_eq!(
        body["messages"][0]["content"][0]["cache_control"]["type"],
        "ephemeral"
    );
    assert_eq!(
        body["messages"][0]["content"][1]["cache_control"]["type"],
        "ephemeral"
    );

    let bare = codec
        .encode_request(&HistoryFixture::new("claude-sonnet-4-20250514").with_user_text("hi"))
        .unwrap();
    assert!(bare.body_json.get("system").is_none());
    assert!(bare.body_json["messages"][0]["content"][0]
        .get("cache_control")
        .is_none());
}

#[test]
fn encode_cache_control_scope_and_ttl() {
    // getCacheControl parity: scope:'global' + ttl:'1h' serialize as extra keys
    // on {"type":"ephemeral"}; the plain Ephemeral emits neither.
    let codec = AnthropicMessagesCodec::new("https://api.anthropic.com", "2023-06-01");
    let mut request = HistoryFixture::new("claude-sonnet-4-20250514").with_user_text("hi");
    request.system = vec![
        llm_runtime::SystemBlock {
            text: "global static".to_string(),
            cache_control: Some(llm_runtime::CacheControl::EphemeralScoped {
                scope: Some(llm_runtime::CacheScope::Global),
                ttl_1h: true,
            }),
        },
        llm_runtime::SystemBlock {
            text: "ttl only".to_string(),
            cache_control: Some(llm_runtime::CacheControl::EphemeralScoped {
                scope: None,
                ttl_1h: true,
            }),
        },
        llm_runtime::SystemBlock {
            text: "plain org".to_string(),
            cache_control: Some(llm_runtime::CacheControl::Ephemeral),
        },
    ];

    let body = codec.encode_request(&request).unwrap().body_json;

    // Block 0: type+ttl+scope.
    assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
    assert_eq!(body["system"][0]["cache_control"]["ttl"], "1h");
    assert_eq!(body["system"][0]["cache_control"]["scope"], "global");
    // Block 1: type+ttl, no scope key.
    assert_eq!(body["system"][1]["cache_control"]["type"], "ephemeral");
    assert_eq!(body["system"][1]["cache_control"]["ttl"], "1h");
    assert!(body["system"][1]["cache_control"].get("scope").is_none());
    // Block 2: plain ephemeral, neither extra key.
    assert_eq!(body["system"][2]["cache_control"]["type"], "ephemeral");
    assert!(body["system"][2]["cache_control"].get("ttl").is_none());
    assert!(body["system"][2]["cache_control"].get("scope").is_none());
}

#[test]
fn count_tokens_request_and_response_round_trip() {
    let codec = AnthropicMessagesCodec::new("https://api.anthropic.com", "2023-06-01");
    let mut request = HistoryFixture::new("claude-sonnet-4-20250514").with_user_text("hello");
    request.system = vec![llm_runtime::SystemBlock::text("sys")];

    let provider_request = codec.encode_count_tokens_request(&request).unwrap();

    assert!(provider_request.url.ends_with("/v1/messages/count_tokens"));
    assert_eq!(
        provider_request.body_json["model"],
        "claude-sonnet-4-20250514"
    );
    assert_eq!(provider_request.body_json["messages"][0]["role"], "user");
    assert_eq!(provider_request.body_json["system"][0]["text"], "sys");
    assert!(provider_request.body_json.get("max_tokens").is_none());
    assert!(provider_request.body_json.get("stream").is_none());

    let count = codec
        .decode_count_tokens_response(&ProviderResponse::json(
            200,
            serde_json::json!({"input_tokens": 2095}),
        ))
        .unwrap();
    assert_eq!(count, 2095);
}

#[test]
fn count_tokens_error_status_maps_through_taxonomy() {
    let codec = AnthropicMessagesCodec::new("https://api.anthropic.com", "2023-06-01");

    let err = codec
        .decode_count_tokens_response(&ProviderResponse::json(
            401,
            serde_json::json!({
                "type": "error",
                "error": {"type": "authentication_error", "message": "bad key"}
            }),
        ))
        .unwrap_err();

    assert!(matches!(err, llm_runtime::LlmError::Authentication { .. }));
}
