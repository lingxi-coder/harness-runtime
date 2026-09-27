use llm_runtime::providers::GeminiCodec;
use llm_runtime::{
    ContentBlock, LlmEvent, LlmRequest, Message, ProviderResponse, RawStreamFrame, ToolChoice,
    ToolDeclaration, WireCodec,
};

// ── Item 4: Gemini image+document encode ──────────────────────────────────────

#[test]
fn encode_image_bytes_as_inline_data() {
    let codec = GeminiCodec::new("https://generativelanguage.googleapis.com/v1beta");
    let mut request = LlmRequest::new("gemini-2.0-flash");
    request.messages.push(Message {
        role: "user".to_string(),
        content: vec![ContentBlock::Image {
            media_type: "image/png".to_string(),
            bytes: vec![1, 2, 3],
        }],
    });
    let provider_request = codec.encode_request(&request).unwrap();
    let part = &provider_request.body_json["contents"][0]["parts"][0];
    assert_eq!(part["inlineData"]["mimeType"], "image/png");
    let b64 = part["inlineData"]["data"].as_str().unwrap();
    assert!(!b64.is_empty());
}

#[test]
fn encode_document_bytes_as_inline_data() {
    let codec = GeminiCodec::new("https://generativelanguage.googleapis.com/v1beta");
    let mut request = LlmRequest::new("gemini-2.0-flash");
    request.messages.push(Message {
        role: "user".to_string(),
        content: vec![ContentBlock::Document {
            media_type: "application/pdf".to_string(),
            bytes: vec![0x25, 0x50, 0x44, 0x46],
        }],
    });
    let provider_request = codec.encode_request(&request).unwrap();
    let part = &provider_request.body_json["contents"][0]["parts"][0];
    assert_eq!(part["inlineData"]["mimeType"], "application/pdf");
    assert!(part["inlineData"]["data"].as_str().is_some());
}

#[test]
fn encode_image_url_produces_file_data_part() {
    // ImageUrl is encoded as a file_data part; mime_type is omitted for https URIs
    // because Gemini v1beta infers it from the Content-Type served at that URL.
    let codec = GeminiCodec::new("https://generativelanguage.googleapis.com/v1beta");
    let mut request = LlmRequest::new("gemini-2.0-flash");
    request.messages.push(Message {
        role: "user".to_string(),
        content: vec![ContentBlock::ImageUrl {
            url: "https://example.com/img.png".to_string(),
        }],
    });
    let provider_request = codec.encode_request(&request).unwrap();
    let part = &provider_request.body_json["contents"][0]["parts"][0];
    assert_eq!(part["fileData"]["fileUri"], "https://example.com/img.png");
    // mime_type intentionally absent — Gemini infers it for https file_uri values.
    assert!(
        part["fileData"]["mimeType"].is_null(),
        "mime_type should be omitted"
    );
}

// ── Item 5: Gemini tool_choice ────────────────────────────────────────────────

#[test]
fn encode_tool_choice_auto() {
    let codec = GeminiCodec::new("https://generativelanguage.googleapis.com/v1beta");
    let mut request = LlmRequest::new("gemini-2.0-flash");
    request.tools.push(ToolDeclaration {
        name: "lookup".into(),
        input_schema: serde_json::json!({"type":"object"}),
        ..Default::default()
    });
    request.tool_choice = Some(ToolChoice::Auto);
    let provider_request = codec.encode_request(&request).unwrap();
    assert_eq!(
        provider_request.body_json["toolConfig"]["functionCallingConfig"]["mode"],
        "AUTO"
    );
}

#[test]
fn encode_tool_choice_none() {
    let codec = GeminiCodec::new("https://generativelanguage.googleapis.com/v1beta");
    let mut request = LlmRequest::new("gemini-2.0-flash");
    request.tools.push(ToolDeclaration {
        name: "lookup".into(),
        input_schema: serde_json::json!({"type":"object"}),
        ..Default::default()
    });
    request.tool_choice = Some(ToolChoice::None);
    let provider_request = codec.encode_request(&request).unwrap();
    assert_eq!(
        provider_request.body_json["toolConfig"]["functionCallingConfig"]["mode"],
        "NONE"
    );
}

#[test]
fn encode_tool_choice_required_maps_to_any() {
    let codec = GeminiCodec::new("https://generativelanguage.googleapis.com/v1beta");
    let mut request = LlmRequest::new("gemini-2.0-flash");
    request.tools.push(ToolDeclaration {
        name: "lookup".into(),
        input_schema: serde_json::json!({"type":"object"}),
        ..Default::default()
    });
    request.tool_choice = Some(ToolChoice::Required);
    let provider_request = codec.encode_request(&request).unwrap();
    assert_eq!(
        provider_request.body_json["toolConfig"]["functionCallingConfig"]["mode"],
        "ANY"
    );
}

#[test]
fn encode_tool_choice_specific_tool() {
    let codec = GeminiCodec::new("https://generativelanguage.googleapis.com/v1beta");
    let mut request = LlmRequest::new("gemini-2.0-flash");
    request.tools.push(ToolDeclaration {
        name: "lookup".into(),
        input_schema: serde_json::json!({"type":"object"}),
        ..Default::default()
    });
    request.tool_choice = Some(ToolChoice::Tool {
        name: "Bash".to_string(),
    });
    let provider_request = codec.encode_request(&request).unwrap();
    let config = &provider_request.body_json["toolConfig"]["functionCallingConfig"];
    assert_eq!(config["mode"], "ANY");
    assert_eq!(config["allowedFunctionNames"][0], "Bash");
}

#[test]
fn omit_tool_config_when_no_tool_choice() {
    let codec = GeminiCodec::new("https://generativelanguage.googleapis.com/v1beta");
    let request = LlmRequest::new("gemini-2.0-flash");
    let provider_request = codec.encode_request(&request).unwrap();
    assert!(provider_request.body_json.get("toolConfig").is_none());
}

#[test]
fn encode_request_shape_is_gemini_generate_content() {
    let codec = GeminiCodec::new("https://generativelanguage.googleapis.com/v1beta");
    let mut request = LlmRequest::new("gemini-2.0-flash");
    request.system = vec![llm_runtime::SystemBlock::text("sys")];
    request.tools = vec![ToolDeclaration {
        name: "Read".to_string(),
        description: "d".to_string(),
        input_schema: serde_json::json!({"type":"object"}),
        ..Default::default()
    }];

    let provider_request = codec.encode_request(&request).unwrap();

    assert!(provider_request.url.contains(":generateContent"));
    assert_eq!(
        provider_request.body_json["systemInstruction"]["parts"][0]["text"],
        "sys"
    );
    assert_eq!(
        provider_request.body_json["tools"][0]["functionDeclarations"][0]["name"],
        "Read"
    );
}

#[test]
fn encode_stream_request_targets_stream_generate_content() {
    let codec = GeminiCodec::new("https://generativelanguage.googleapis.com/v1beta");
    let mut request = LlmRequest::new("gemini-2.0-flash");
    request.stream = true;

    let provider_request = codec.encode_request(&request).unwrap();

    assert!(provider_request.url.contains(":streamGenerateContent"));
    assert!(provider_request.url.ends_with("alt=sse"));
}

#[test]
fn decode_text_and_function_call() {
    let codec = GeminiCodec::new("https://generativelanguage.googleapis.com/v1beta");
    let text = ProviderResponse::json(
        200,
        serde_json::json!({
            "modelVersion":"gemini-2.0-flash",
            "candidates":[{"content":{"role":"model","parts":[{"text":"hi there"}]},"finishReason":"STOP"}],
            "usageMetadata":{"promptTokenCount":9,"candidatesTokenCount":3}
        }),
    );
    let decoded = codec.decode_response(text).unwrap();
    assert_eq!(decoded.usage.billable_tokens.input, 9);
    assert_eq!(decoded.usage.billable_tokens.output, 3);
    assert!(matches!(decoded.content[0], ContentBlock::Text { .. }));
    assert_eq!(decoded.stop_reason.as_deref(), Some("end_turn"));

    let tool = ProviderResponse::json(
        200,
        serde_json::json!({
            "modelVersion":"gemini-2.0-flash",
            "candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"Bash","args":{"command":"ls"}}}]},"finishReason":"STOP"}]
        }),
    );
    let decoded = codec.decode_response(tool).unwrap();
    assert!(
        matches!(decoded.content[0], ContentBlock::ToolCall { ref name, .. } if name == "Bash")
    );
    assert_eq!(decoded.stop_reason.as_deref(), Some("tool_use"));
}

#[test]
fn encode_generation_config_from_sampling_controls() {
    let codec = GeminiCodec::new("https://generativelanguage.googleapis.com/v1beta");
    let mut request = LlmRequest::new("gemini-2.0-flash");
    request.max_tokens = Some(1024);
    request.temperature = Some(0.5);
    request.top_p = Some(0.9);
    request.stop_sequences = vec!["END".to_string()];

    let provider_request = codec.encode_request(&request).unwrap();
    let config = &provider_request.body_json["generationConfig"];

    assert_eq!(config["maxOutputTokens"], 1024);
    assert_eq!(config["temperature"], 0.5);
    assert_eq!(config["topP"], 0.9);
    assert_eq!(config["stopSequences"], serde_json::json!(["END"]));

    let bare = codec
        .encode_request(&LlmRequest::new("gemini-2.0-flash"))
        .unwrap();
    assert!(bare.body_json.get("generationConfig").is_none());
}

#[test]
fn encode_tool_result_error_uses_error_response_shape() {
    let codec = GeminiCodec::new("https://generativelanguage.googleapis.com/v1beta");
    let mut request = LlmRequest::new("gemini-2.0-flash");
    request.messages.push(llm_runtime::Message {
        role: "assistant".to_string(),
        content: vec![ContentBlock::ToolCall {
            id: "call_0".to_string(),
            name: "Bash".to_string(),
            input: serde_json::json!({"command":"ls"}),
        }],
    });
    request.messages.push(llm_runtime::Message {
        role: "user".to_string(),
        content: vec![ContentBlock::ToolResult {
            tool_call_id: "call_0".to_string(),
            output: serde_json::json!("command failed"),
            is_error: true,
            cache_control: None,
            cache_reference: None,
        }],
    });

    let provider_request = codec.encode_request(&request).unwrap();
    let response =
        &provider_request.body_json["contents"][1]["parts"][0]["functionResponse"]["response"];

    assert_eq!(response["error"], "command failed");
    assert!(response.get("result").is_none());
}

#[test]
fn encode_tool_result_uses_prior_tool_call_name() {
    let codec = GeminiCodec::new("https://generativelanguage.googleapis.com/v1beta");
    let mut request = LlmRequest::new("gemini-2.0-flash");
    request.messages.push(llm_runtime::Message {
        role: "assistant".to_string(),
        content: vec![ContentBlock::ToolCall {
            id: "call_1".to_string(),
            name: "Bash".to_string(),
            input: serde_json::json!({"command":"ls"}),
        }],
    });
    request.messages.push(llm_runtime::Message {
        role: "user".to_string(),
        content: vec![ContentBlock::ToolResult {
            tool_call_id: "call_1".to_string(),
            output: serde_json::json!("done"),
            is_error: false,
            cache_control: None,
            cache_reference: None,
        }],
    });

    let provider_request = codec.encode_request(&request).unwrap();

    assert_eq!(
        provider_request.body_json["contents"][1]["parts"][0]["functionResponse"]["name"],
        "Bash"
    );
}

#[test]
fn decode_cached_content_token_count_maps_to_cache_read_usage() {
    let codec = GeminiCodec::new("https://generativelanguage.googleapis.com/v1beta");
    let response = ProviderResponse::json(
        200,
        serde_json::json!({
            "modelVersion":"gemini-2.0-flash",
            "candidates":[{"content":{"role":"model","parts":[{"text":"hi"}]},"finishReason":"STOP"}],
            "usageMetadata":{"promptTokenCount":9,"candidatesTokenCount":3,"cachedContentTokenCount":7}
        }),
    );

    let decoded = codec.decode_response(response).unwrap();

    assert_eq!(decoded.usage.billable_tokens.cache_read, 7);
}

#[test]
fn sse_stream_reassembles_text_then_thought_then_function_call() {
    let frames = [
        r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"On it."}]}}]}"#,
        r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"thinking","thought":true}]}}]}"#,
        r#"{"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"Bash","args":{"command":"ls"}}}]} ,"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":2}}"#,
    ];
    let mut decoder =
        GeminiCodec::new("https://generativelanguage.googleapis.com/v1beta").stream_decoder();
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
    assert!(matches!(
        events[1],
        LlmEvent::ContentBlockStart {
            content_block: ContentBlock::Text { .. },
            ..
        }
    ));
    assert!(
        matches!(events[2], LlmEvent::ContentBlockDelta { delta: llm_runtime::ContentDelta::TextDelta { ref text }, .. } if text == "On it.")
    );
    assert!(matches!(
        events[3],
        LlmEvent::ContentBlockStart {
            content_block: ContentBlock::Reasoning { .. },
            ..
        }
    ));
    assert!(
        matches!(events[4], LlmEvent::ContentBlockDelta { delta: llm_runtime::ContentDelta::ThinkingDelta { ref thinking }, .. } if thinking == "thinking")
    );
    assert!(
        matches!(events[5], LlmEvent::ContentBlockStart { content_block: ContentBlock::ToolCall { ref name, .. }, .. } if name == "Bash")
    );
    assert!(
        matches!(events[6], LlmEvent::ContentBlockDelta { delta: llm_runtime::ContentDelta::InputJsonDelta { ref partial_json }, .. } if partial_json == "{\"command\":\"ls\"}")
    );
    assert!(matches!(events[7], LlmEvent::ContentBlockStop { .. }));
    assert!(matches!(events[8], LlmEvent::ContentBlockStop { .. }));
    assert!(matches!(events[9], LlmEvent::ContentBlockStop { .. }));
    assert!(
        matches!(events[10], LlmEvent::MessageDelta { delta: llm_runtime::MessageDeltaPayload { stop_reason: Some(ref reason), stop_details: None }, .. } if reason == "tool_use")
    );
    assert!(matches!(events.last(), Some(LlmEvent::MessageStop)));
}

#[test]
fn decode_usage_excludes_cached_tokens_from_input_and_maps_thoughts() {
    let codec = GeminiCodec::new("https://generativelanguage.googleapis.com/v1beta");
    let response = ProviderResponse::json(
        200,
        serde_json::json!({
            "modelVersion":"gemini-2.0-flash",
            "candidates":[{"content":{"role":"model","parts":[{"text":"hi"}]},"finishReason":"STOP"}],
            "usageMetadata":{"promptTokenCount":100,"candidatesTokenCount":10,"cachedContentTokenCount":40,"thoughtsTokenCount":5,"totalTokenCount":115}
        }),
    );

    let decoded = codec.decode_response(response).unwrap();

    assert_eq!(decoded.usage.billable_tokens.input, 60);
    assert_eq!(decoded.usage.billable_tokens.cache_read, 40);
    assert_eq!(decoded.usage.billable_tokens.output, 10);
    assert_eq!(decoded.usage.billable_tokens.reasoning_output, 5);
    assert_eq!(decoded.usage.provider_reported_total_tokens, Some(115));
}

#[test]
fn stream_usage_only_final_frame_is_not_lost() {
    let frames = [
        r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"hi"}]},"finishReason":"STOP"}]}"#,
        r#"{"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":2,"totalTokenCount":7}}"#,
    ];
    let mut decoder =
        GeminiCodec::new("https://generativelanguage.googleapis.com/v1beta").stream_decoder();
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
        .expect("terminal usage should survive a usage-only final frame");

    assert_eq!(usage.billable_tokens.input, 5);
    assert_eq!(usage.billable_tokens.output, 2);
}

#[test]
fn decode_synthesizes_unique_tool_call_ids() {
    let codec = GeminiCodec::new("https://generativelanguage.googleapis.com/v1beta");
    let response = ProviderResponse::json(
        200,
        serde_json::json!({
            "modelVersion":"gemini-2.0-flash",
            "candidates":[{"content":{"role":"model","parts":[
                {"functionCall":{"name":"Bash","args":{"command":"ls"}}},
                {"functionCall":{"name":"Read","args":{"path":"a.txt"}}}
            ]},"finishReason":"STOP"}]
        }),
    );

    let decoded = codec.decode_response(response).unwrap();

    let ids: Vec<&str> = decoded
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolCall { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(ids.len(), 2);
    assert!(ids.iter().all(|id| !id.is_empty()));
    assert_ne!(ids[0], ids[1]);
}

#[test]
fn stream_synthesizes_unique_tool_call_ids() {
    let frame = r#"{"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"Bash","args":{"command":"ls"}}},{"functionCall":{"name":"Read","args":{"path":"a.txt"}}}]},"finishReason":"STOP"}]}"#;
    let mut decoder =
        GeminiCodec::new("https://generativelanguage.googleapis.com/v1beta").stream_decoder();
    let mut events = decoder
        .decode_frame(RawStreamFrame::new(frame.as_bytes().to_vec()))
        .unwrap();
    events.extend(decoder.finish().unwrap());

    let ids: Vec<String> = events
        .iter()
        .filter_map(|event| match event {
            LlmEvent::ContentBlockStart {
                content_block: ContentBlock::ToolCall { id, .. },
                ..
            } => Some(id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(ids.len(), 2);
    assert!(ids.iter().all(|id| !id.is_empty()));
    assert_ne!(ids[0], ids[1]);
}

#[test]
fn encode_tool_result_with_unknown_call_id_is_rejected() {
    let codec = GeminiCodec::new("https://generativelanguage.googleapis.com/v1beta");
    let mut request = LlmRequest::new("gemini-2.0-flash");
    request.messages.push(llm_runtime::Message {
        role: "user".to_string(),
        content: vec![ContentBlock::ToolResult {
            tool_call_id: "call_unseen".to_string(),
            output: serde_json::json!("done"),
            is_error: false,
            cache_control: None,
            cache_reference: None,
        }],
    });

    let err = codec.encode_request(&request).unwrap_err();

    assert!(
        matches!(err, llm_runtime::LlmError::InvalidRequest { message } if message.contains("call_unseen"))
    );
}

#[test]
fn decode_blocked_prompt_reports_block_reason() {
    let codec = GeminiCodec::new("https://generativelanguage.googleapis.com/v1beta");
    let response = ProviderResponse::json(
        200,
        serde_json::json!({
            "promptFeedback":{"blockReason":"SAFETY"}
        }),
    );

    let response = codec.decode_response(response).unwrap();

    assert_eq!(response.stop_reason.as_deref(), Some("refusal"));
    assert_eq!(
        response.provider_metadata["promptFeedback"]["blockReason"],
        "SAFETY"
    );
}

#[test]
fn encode_reasoning_budget_as_thinking_config() {
    let codec = GeminiCodec::new("https://generativelanguage.googleapis.com/v1beta");
    let mut request = LlmRequest::new("gemini-2.0-flash");
    request.reasoning = Some(llm_runtime::ReasoningConfig::Enabled {
        budget_tokens: 2048,
    });

    let provider_request = codec.encode_request(&request).unwrap();

    assert_eq!(
        provider_request.body_json["generationConfig"]["thinkingConfig"]["thinkingBudget"],
        2048
    );
}

#[test]
fn system_blocks_become_system_instruction_parts() {
    let codec = GeminiCodec::new("https://generativelanguage.googleapis.com/v1beta");
    let mut request = LlmRequest::new("gemini-2.0-flash");
    request.system = vec![
        llm_runtime::SystemBlock {
            text: "a".to_string(),
            cache_control: None,
        },
        llm_runtime::SystemBlock {
            text: "b".to_string(),
            cache_control: Some(llm_runtime::CacheControl::Ephemeral),
        },
    ];

    let provider_request = codec.encode_request(&request).unwrap();

    assert_eq!(
        provider_request.body_json["systemInstruction"]["parts"][0]["text"],
        "a"
    );
    assert_eq!(
        provider_request.body_json["systemInstruction"]["parts"][1]["text"],
        "b"
    );
}

#[tokio::test]
async fn signed_text_and_tools_survive_both_response_paths_and_replay_once() {
    use futures::StreamExt;
    let codec = GeminiCodec::new("https://generativelanguage.googleapis.com/v1beta");
    let body = serde_json::json!({
        "modelVersion":"gemini-3.1-pro-preview", "responseId":"response",
        "candidates":[{"content":{"role":"model","parts":[
            {"text":"visible", "thoughtSignature":"text-signature"},
            {"functionCall":{"id":"provider-call","name":"lookup","args":{"q":"x"}}, "thoughtSignature":"tool-signature"}
        ]},"finishReason":"STOP"}],
        "usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":5,"totalTokenCount":10}
    });
    for streamed in [false, true] {
        let response = if streamed {
            let mut decoder = codec.stream_decoder();
            let mut events = decoder
                .decode_frame(RawStreamFrame::new(serde_json::to_vec(&body).unwrap()))
                .unwrap();
            events.extend(decoder.finish().unwrap());
            llm_runtime::stream_accumulator::accumulate_stream_salvaging(
                futures::stream::iter(events.into_iter().map(Ok)).boxed(),
            )
            .await
            .unwrap()
        } else {
            codec
                .decode_response(ProviderResponse::json(200, body.clone()))
                .unwrap()
        };
        assert!(
            response
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::ProviderContent { value, .. } => Some(value),
                    _ => None,
                })
                .all(|value| value["type"] == "lingxi_replay_metadata"),
            "partial output must not expose a metadata companion as another tool_use"
        );
        let calls: Vec<_> = response
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolCall { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            calls.len(),
            1,
            "metadata must not become a second executable call"
        );
        let mut request = LlmRequest::new("gemini-3.1-pro-preview");
        request.messages.push(Message {
            role: "assistant".into(),
            content: response.content,
        });
        request.messages.push(Message {
            role: "user".into(),
            content: vec![ContentBlock::ToolResult {
                tool_call_id: calls[0].clone(),
                output: serde_json::json!({"ok":true}),
                is_error: false,
                cache_control: None,
                cache_reference: None,
            }],
        });
        let encoded = codec.encode_request(&request).unwrap().body_json;
        let parts = encoded["contents"][0]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["text"], "visible");
        assert_eq!(parts[0]["thoughtSignature"], "text-signature");
        assert_eq!(parts[1]["thoughtSignature"], "tool-signature");
        assert_eq!(parts[1]["functionCall"]["id"], "provider-call");
        assert_eq!(
            encoded["contents"][1]["parts"][0]["functionResponse"]["id"],
            "provider-call"
        );
    }
}
