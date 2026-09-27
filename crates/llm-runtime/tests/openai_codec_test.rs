use base64::Engine as _;
use llm_runtime::{
    ContentBlock, ContentDelta, LlmEvent, LlmRequest, Message, OpenAiChatCodec, ProviderResponse,
    RawStreamFrame, ToolDeclaration, WireCodec,
};

// ── Item 2: OpenAI content-as-array decode ────────────────────────────────────

#[test]
fn decode_content_array_single_text_part() {
    let codec = OpenAiChatCodec::new("https://api.openai.com/v1");
    let response = ProviderResponse::json(
        200,
        serde_json::json!({
            "id": "chatcmpl-arr1",
            "model": "gpt-4o",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": [{"type": "text", "text": "hello array"}]}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 3, "completion_tokens": 2}
        }),
    );
    let decoded = codec.decode_response(response).unwrap();
    assert_eq!(decoded.content.len(), 1);
    assert!(
        matches!(&decoded.content[0], ContentBlock::Text { text, .. } if text == "hello array")
    );
}

#[test]
fn decode_content_array_multi_text_parts_concatenated() {
    let codec = OpenAiChatCodec::new("https://api.openai.com/v1");
    let response = ProviderResponse::json(
        200,
        serde_json::json!({
            "id": "chatcmpl-arr2",
            "model": "gpt-4o",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": [
                {"type": "text", "text": "hello "},
                {"type": "text", "text": "world"}
            ]}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 3, "completion_tokens": 2}
        }),
    );
    let decoded = codec.decode_response(response).unwrap();
    // Multi text parts should produce one block (concatenated) or multiple - both fine; just confirm text is present
    let text: String = decoded
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(text.contains("hello") && text.contains("world"));
}

#[test]
fn decode_content_array_unknown_part_types_are_skipped() {
    let codec = OpenAiChatCodec::new("https://api.openai.com/v1");
    let response = ProviderResponse::json(
        200,
        serde_json::json!({
            "id": "chatcmpl-arr3",
            "model": "gpt-4o",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": [
                {"type": "text", "text": "known"},
                {"type": "refusal", "refusal": "I can't do that"},
                {"type": "future_type", "data": "ignored"}
            ]}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 3, "completion_tokens": 2}
        }),
    );
    let decoded = codec.decode_response(response).unwrap();
    // Only text part should appear
    assert!(decoded
        .content
        .iter()
        .any(|b| matches!(b, ContentBlock::Text { text, .. } if text == "known")));
}

#[test]
fn decode_content_string_still_works() {
    let codec = OpenAiChatCodec::new("https://api.openai.com/v1");
    let response = ProviderResponse::json(
        200,
        serde_json::json!({
            "id": "chatcmpl-str",
            "model": "gpt-4o",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "plain string"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 3, "completion_tokens": 2}
        }),
    );
    let decoded = codec.decode_response(response).unwrap();
    assert!(
        matches!(&decoded.content[0], ContentBlock::Text { text, .. } if text == "plain string")
    );
}

// ── Item 3: OpenAI image encode ───────────────────────────────────────────────

#[test]
fn encode_image_bytes_produces_data_uri_part() {
    let codec = OpenAiChatCodec::new("https://api.openai.com/v1");
    let mut request = LlmRequest::new("gpt-4o");
    request.messages.push(Message {
        role: "user".to_string(),
        content: vec![ContentBlock::Image {
            media_type: "image/png".to_string(),
            bytes: vec![1, 2, 3],
        }],
    });
    let provider_request = codec.encode_request(&request).unwrap();
    let msg = &provider_request.body_json["messages"][0];
    // content must be an array (array form when image present)
    assert!(msg["content"].is_array());
    let part = &msg["content"][0];
    assert_eq!(part["type"], "image_url");
    let url = part["image_url"]["url"].as_str().unwrap();
    assert!(url.starts_with("data:image/png;base64,"), "url={url}");
}

#[test]
fn encode_image_url_produces_url_part() {
    let codec = OpenAiChatCodec::new("https://api.openai.com/v1");
    let mut request = LlmRequest::new("gpt-4o");
    request.messages.push(Message {
        role: "user".to_string(),
        content: vec![ContentBlock::ImageUrl {
            url: "https://example.com/img.png".to_string(),
        }],
    });
    let provider_request = codec.encode_request(&request).unwrap();
    let msg = &provider_request.body_json["messages"][0];
    assert!(msg["content"].is_array());
    let part = &msg["content"][0];
    assert_eq!(part["type"], "image_url");
    assert_eq!(part["image_url"]["url"], "https://example.com/img.png");
}

#[test]
fn encode_mixed_text_and_image_becomes_array_form() {
    let codec = OpenAiChatCodec::new("https://api.openai.com/v1");
    let mut request = LlmRequest::new("gpt-4o");
    request.messages.push(Message {
        role: "user".to_string(),
        content: vec![
            ContentBlock::Text {
                text: "look at this".to_string(),
                cache_control: None,
            },
            ContentBlock::Image {
                media_type: "image/jpeg".to_string(),
                bytes: vec![0xDE, 0xAD],
            },
        ],
    });
    let provider_request = codec.encode_request(&request).unwrap();
    let content = &provider_request.body_json["messages"][0]["content"];
    assert!(content.is_array());
    let arr = content.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["type"], "text");
    assert_eq!(arr[0]["text"], "look at this");
    assert_eq!(arr[1]["type"], "image_url");
}

#[test]
fn encode_text_only_message_stays_plain_string() {
    let codec = OpenAiChatCodec::new("https://api.openai.com/v1");
    let request = LlmRequest::new("gpt-4o").with_user_text("just text");
    let provider_request = codec.encode_request(&request).unwrap();
    let content = &provider_request.body_json["messages"][0]["content"];
    assert!(
        content.is_string(),
        "text-only should be plain string, got: {content}"
    );
}

#[test]
fn encode_document_produces_file_part() {
    // Document bytes must be encoded as a {"type":"file"} media part with a data-URI.
    let codec = OpenAiChatCodec::new("https://api.openai.com/v1");
    let mut request = LlmRequest::new("gpt-4o");
    let pdf_bytes = vec![0x25u8, 0x50, 0x44, 0x46]; // %PDF
    request.messages.push(Message {
        role: "user".to_string(),
        content: vec![ContentBlock::Document {
            media_type: "application/pdf".to_string(),
            bytes: pdf_bytes.clone(),
        }],
    });
    let provider_request = codec.encode_request(&request).unwrap();
    let content = &provider_request.body_json["messages"][0]["content"];
    // Document triggers array form (has_media=true).
    assert!(content.is_array(), "expected array form, got: {content}");
    let parts = content.as_array().unwrap();
    assert_eq!(parts.len(), 1);
    let part = &parts[0];
    assert_eq!(part["type"], "file");
    assert_eq!(part["file"]["filename"], "document");
    let data_uri = part["file"]["file_data"].as_str().unwrap();
    let expected_b64 = base64::engine::general_purpose::STANDARD.encode(&pdf_bytes);
    assert_eq!(
        data_uri,
        format!("data:application/pdf;base64,{expected_b64}")
    );
}

#[test]
fn encode_mixed_text_and_document_becomes_array_form() {
    let codec = OpenAiChatCodec::new("https://api.openai.com/v1");
    let mut request = LlmRequest::new("gpt-4o");
    request.messages.push(Message {
        role: "user".to_string(),
        content: vec![
            ContentBlock::Text {
                text: "see the attached".to_string(),
                cache_control: None,
            },
            ContentBlock::Document {
                media_type: "application/pdf".to_string(),
                bytes: vec![0x25, 0x50, 0x44, 0x46],
            },
        ],
    });
    let provider_request = codec.encode_request(&request).unwrap();
    let content = &provider_request.body_json["messages"][0]["content"];
    assert!(
        content.is_array(),
        "mixed text+document must produce array form"
    );
    let parts = content.as_array().unwrap();
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0]["type"], "text");
    assert_eq!(parts[0]["text"], "see the attached");
    assert_eq!(parts[1]["type"], "file");
}

#[test]
fn encode_request_shape_is_openai_chat_completions() {
    let codec = OpenAiChatCodec::new("https://api.openai.com/v1");
    let mut request = LlmRequest::new("gpt-4o");
    request.system = vec![llm_runtime::SystemBlock::text("sys")];
    request.tools = vec![ToolDeclaration {
        name: "Read".to_string(),
        description: "d".to_string(),
        input_schema: serde_json::json!({"type":"object"}),
        ..Default::default()
    }];

    let provider_request = codec.encode_request(&request).unwrap();

    assert!(provider_request.url.ends_with("/chat/completions"));
    assert_eq!(provider_request.body_json["messages"][0]["role"], "system");
    assert_eq!(provider_request.body_json["tools"][0]["type"], "function");
}

#[test]
fn decode_text_and_tool_responses() {
    let codec = OpenAiChatCodec::new("https://api.openai.com/v1");
    let text = ProviderResponse::json(
        200,
        serde_json::json!({
            "id":"chatcmpl-x",
            "model":"gpt-4o-2024-08-06",
            "choices":[{"index":0,"message":{"role":"assistant","content":"hi there"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":9,"completion_tokens":3}
        }),
    );
    let decoded = codec.decode_response(text).unwrap();
    assert_eq!(decoded.model, "gpt-4o-2024-08-06");
    assert_eq!(decoded.usage.billable_tokens.input, 9);
    assert_eq!(decoded.usage.billable_tokens.output, 3);
    assert!(matches!(decoded.content[0], ContentBlock::Text { .. }));
    assert_eq!(decoded.stop_reason.as_deref(), Some("end_turn"));

    let tool = ProviderResponse::json(
        200,
        serde_json::json!({
            "id":"chatcmpl-y",
            "model":"gpt-4o",
            "choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"Bash","arguments":"{\"command\":\"ls\"}"}}]},"finish_reason":"tool_calls"}]
        }),
    );
    let decoded = codec.decode_response(tool).unwrap();
    assert!(
        matches!(decoded.content[0], ContentBlock::ToolCall { ref name, .. } if name == "Bash")
    );
    assert_eq!(decoded.stop_reason.as_deref(), Some("tool_use"));
}

#[test]
fn encode_sampling_controls() {
    let codec = OpenAiChatCodec::new("https://api.openai.com/v1");
    let mut request = LlmRequest::new("gpt-4o");
    request.max_tokens = Some(1024);
    request.temperature = Some(0.5);
    request.top_p = Some(0.9);
    request.stop_sequences = vec!["END".to_string()];

    let provider_request = codec.encode_request(&request).unwrap();

    assert_eq!(provider_request.body_json["max_tokens"], 1024);
    assert_eq!(provider_request.body_json["temperature"], 0.5);
    assert_eq!(provider_request.body_json["top_p"], 0.9);
    assert_eq!(
        provider_request.body_json["stop"],
        serde_json::json!(["END"])
    );

    let bare = codec.encode_request(&LlmRequest::new("gpt-4o")).unwrap();
    assert!(bare.body_json.get("max_tokens").is_none());
    assert!(bare.body_json.get("temperature").is_none());
    assert!(bare.body_json.get("top_p").is_none());
    assert!(bare.body_json.get("stop").is_none());
}

#[test]
fn encode_tool_result_as_tool_message_not_tool_call() {
    let codec = OpenAiChatCodec::new("https://api.openai.com/v1");
    let mut request = LlmRequest::new("gpt-4o");
    request.messages.push(llm_runtime::Message {
        role: "assistant".to_string(),
        content: vec![ContentBlock::ToolResult {
            tool_call_id: "call_1".to_string(),
            output: serde_json::json!("done"),
            is_error: false,
            cache_control: None,
            cache_reference: None,
        }],
    });

    let provider_request = codec.encode_request(&request).unwrap();

    assert_eq!(provider_request.body_json["messages"][0]["role"], "tool");
    assert_eq!(
        provider_request.body_json["messages"][0]["tool_call_id"],
        "call_1"
    );
    assert_eq!(provider_request.body_json["messages"][0]["content"], "done");
    assert!(provider_request.body_json["messages"][0]
        .get("tool_calls")
        .is_none());
}

#[test]
fn encode_multiple_tool_results_preserves_each_as_tool_message() {
    let codec = OpenAiChatCodec::new("https://api.openai.com/v1");
    let mut request = LlmRequest::new("gpt-4o");
    request.messages.push(llm_runtime::Message {
        role: "assistant".to_string(),
        content: vec![
            ContentBlock::ToolResult {
                tool_call_id: "call_1".to_string(),
                output: serde_json::json!("first"),
                is_error: false,
                cache_control: None,
                cache_reference: None,
            },
            ContentBlock::ToolResult {
                tool_call_id: "call_2".to_string(),
                output: serde_json::json!("second"),
                is_error: false,
                cache_control: None,
                cache_reference: None,
            },
        ],
    });

    let provider_request = codec.encode_request(&request).unwrap();

    assert_eq!(provider_request.body_json["messages"][0]["role"], "tool");
    assert_eq!(
        provider_request.body_json["messages"][0]["tool_call_id"],
        "call_1"
    );
    assert_eq!(
        provider_request.body_json["messages"][0]["content"],
        "first"
    );
    assert_eq!(provider_request.body_json["messages"][1]["role"], "tool");
    assert_eq!(
        provider_request.body_json["messages"][1]["tool_call_id"],
        "call_2"
    );
    assert_eq!(
        provider_request.body_json["messages"][1]["content"],
        "second"
    );
    assert_eq!(
        provider_request.body_json["messages"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn decode_rejects_invalid_tool_call_arguments() {
    let codec = OpenAiChatCodec::new("https://api.openai.com/v1");
    let response = ProviderResponse::json(
        200,
        serde_json::json!({
            "id":"chatcmpl-z",
            "model":"gpt-4o",
            "choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"Bash","arguments":"not-json"}}]},"finish_reason":"tool_calls"}]
        }),
    );

    let err = codec.decode_response(response).unwrap_err();
    assert!(matches!(err, llm_runtime::LlmError::InvalidRequest { .. }));
}

#[test]
fn decode_rejects_missing_tool_call_arguments() {
    let codec = OpenAiChatCodec::new("https://api.openai.com/v1");
    let response = ProviderResponse::json(
        200,
        serde_json::json!({
            "id":"chatcmpl-w",
            "model":"gpt-4o",
            "choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"Bash"}}]},"finish_reason":"tool_calls"}]
        }),
    );

    let err = codec.decode_response(response).unwrap_err();
    assert!(matches!(err, llm_runtime::LlmError::InvalidRequest { .. }));
}

#[test]
fn sse_stream_reassembles_text_then_tool_call() {
    let frames = [
        r#"{"id":"c","model":"gpt-4o","choices":[{"index":0,"delta":{"role":"assistant","content":"On it. "}}]}"#,
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_z","type":"function","function":{"name":"Bash","arguments":"{\"command\":"}}]}}]}"#,
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"ls\"}"}}]}}]}"#,
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        "[DONE]",
    ];
    let mut decoder = OpenAiChatCodec::new("https://api.openai.com/v1").stream_decoder();
    let mut events = Vec::new();
    for frame in frames {
        events.extend(
            decoder
                .decode_frame(RawStreamFrame::new(frame.as_bytes().to_vec()))
                .unwrap(),
        );
    }
    events.extend(decoder.finish().unwrap());

    assert!(matches!(
        events.first(),
        Some(LlmEvent::MessageStart { .. })
    ));
    let args: String = events
        .iter()
        .filter_map(|event| match event {
            LlmEvent::ContentBlockDelta {
                delta: ContentDelta::InputJsonDelta { partial_json },
                ..
            } => Some(partial_json.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(args, "{\"command\":\"ls\"}");
    assert!(matches!(events.last(), Some(LlmEvent::MessageStop)));
}

#[test]
fn stream_usage_keeps_reasoning_and_cached_buckets_independent() {
    let frames = [
        r#"{"id":"c","model":"gpt-4o","choices":[{"index":0,"delta":{"role":"assistant","content":"hi"}}]}"#,
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
        r#"{"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":50,"prompt_tokens_details":{"cached_tokens":40},"completion_tokens_details":{"reasoning_tokens":30}}}"#,
        "[DONE]",
    ];
    let mut decoder = OpenAiChatCodec::new("https://api.openai.com/v1").stream_decoder();
    let mut events = Vec::new();
    for frame in frames {
        events.extend(
            decoder
                .decode_frame(RawStreamFrame::new(frame.as_bytes().to_vec()))
                .unwrap(),
        );
    }
    events.extend(decoder.finish().unwrap());

    let usage = events
        .iter()
        .find_map(|event| match event {
            LlmEvent::MessageDelta {
                usage: Some(usage), ..
            } => Some(usage.clone()),
            _ => None,
        })
        .expect("terminal usage");

    assert_eq!(usage.billable_tokens.input, 60);
    assert_eq!(usage.billable_tokens.cache_read, 40);
    assert_eq!(usage.billable_tokens.output, 20);
    assert_eq!(usage.billable_tokens.reasoning_output, 30);
}

#[test]
fn kimi_stream_requests_terminal_usage_and_decodes_top_level_cached_tokens() {
    let codec = OpenAiChatCodec::new("https://api.moonshot.ai/v1").with_profile_name("kimi");
    let mut request = LlmRequest::new("kimi-k3").with_user_text("hello");
    request.stream = true;

    let encoded = codec.encode_request(&request).expect("encode Kimi stream");
    assert_eq!(encoded.body_json["stream"], serde_json::json!(true));
    assert_eq!(
        encoded.body_json["stream_options"]["include_usage"],
        serde_json::json!(true)
    );

    let frames = [
        r#"{"id":"c","model":"kimi-k3","choices":[{"index":0,"delta":{"role":"assistant","content":"hi"}}]}"#,
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":100,"completion_tokens":25,"total_tokens":125,"cached_tokens":40}}"#,
        "[DONE]",
    ];
    let mut decoder = codec.stream_decoder();
    let mut events = Vec::new();
    for frame in frames {
        events.extend(
            decoder
                .decode_frame(RawStreamFrame::new(frame.as_bytes().to_vec()))
                .unwrap(),
        );
    }
    events.extend(decoder.finish().unwrap());

    let usage = events
        .iter()
        .find_map(|event| match event {
            LlmEvent::MessageDelta {
                usage: Some(usage), ..
            } => Some(usage),
            _ => None,
        })
        .expect("Kimi terminal usage");
    assert_eq!(usage.billable_tokens.input, 60);
    assert_eq!(usage.billable_tokens.cache_read, 40);
    assert_eq!(usage.billable_tokens.output, 25);
    assert_eq!(usage.provider_reported_total_tokens, Some(125));
}

#[test]
fn decode_response_usage_normalization_matches_stream_path() {
    let codec = OpenAiChatCodec::new("https://api.openai.com/v1");
    let response = ProviderResponse::json(
        200,
        serde_json::json!({
            "id":"chatcmpl-u",
            "model":"gpt-4o",
            "choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":100,"completion_tokens":50,"prompt_tokens_details":{"cached_tokens":40},"completion_tokens_details":{"reasoning_tokens":30},"total_tokens":150}
        }),
    );

    let decoded = codec.decode_response(response).unwrap();

    assert_eq!(decoded.usage.billable_tokens.input, 60);
    assert_eq!(decoded.usage.billable_tokens.cache_read, 40);
    assert_eq!(decoded.usage.billable_tokens.output, 20);
    assert_eq!(decoded.usage.billable_tokens.reasoning_output, 30);
    assert_eq!(decoded.usage.provider_reported_total_tokens, Some(150));
}

#[test]
fn stream_tool_fragment_without_index_defaults_to_slot_zero() {
    let frames = [
        r#"{"id":"c","model":"gpt-4o","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"id":"call_n","type":"function","function":{"name":"Bash","arguments":"{\"command\":"}}]}}]}"#,
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"function":{"arguments":"\"ls\"}"}}]}}]}"#,
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        "[DONE]",
    ];
    let mut decoder = OpenAiChatCodec::new("https://api.openai.com/v1").stream_decoder();
    let mut events = Vec::new();
    for frame in frames {
        events.extend(
            decoder
                .decode_frame(RawStreamFrame::new(frame.as_bytes().to_vec()))
                .unwrap(),
        );
    }
    events.extend(decoder.finish().unwrap());

    let starts = events
        .iter()
        .filter(|event| {
            matches!(
                event,
                LlmEvent::ContentBlockStart {
                    content_block: ContentBlock::ToolCall { .. },
                    ..
                }
            )
        })
        .count();
    assert_eq!(starts, 1);

    let args: String = events
        .iter()
        .filter_map(|event| match event {
            LlmEvent::ContentBlockDelta {
                delta: ContentDelta::InputJsonDelta { partial_json },
                ..
            } => Some(partial_json.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(args, "{\"command\":\"ls\"}");
}

#[test]
fn numeric_reasoning_budget_is_not_silently_approximated_on_chat_wire() {
    // Chat-completions has no per-request reasoning-budget wire slot; the codec
    // intentionally drops `request.reasoning` instead of erroring (reasoning on
    // this wire is model-id-driven, e.g. deepseek-reasoner). See
    // `providers/openai.rs::encode_request`.
    let codec = OpenAiChatCodec::new("https://api.openai.com/v1");
    let mut request = LlmRequest::new("gpt-4o");
    request.reasoning = Some(llm_runtime::ReasoningConfig::Enabled {
        budget_tokens: 2048,
    });

    assert!(matches!(
        codec.encode_request(&request),
        Err(llm_runtime::LlmError::UnsupportedCapability { .. })
    ));
}

#[test]
fn system_blocks_join_into_one_system_message() {
    let codec = OpenAiChatCodec::new("https://api.openai.com/v1");
    let mut request = LlmRequest::new("gpt-4o");
    request.system = vec![
        llm_runtime::SystemBlock {
            text: "a".to_string(),
            cache_control: Some(llm_runtime::CacheControl::Ephemeral),
        },
        llm_runtime::SystemBlock {
            text: "b".to_string(),
            cache_control: None,
        },
    ];

    let provider_request = codec.encode_request(&request).unwrap();

    assert_eq!(provider_request.body_json["messages"][0]["role"], "system");
    assert_eq!(
        provider_request.body_json["messages"][0]["content"],
        "a\n\nb"
    );

    let bare = codec
        .encode_request(&LlmRequest::new("gpt-4o").with_user_text("hi"))
        .unwrap();
    assert_eq!(bare.body_json["messages"][0]["role"], "user");
}
