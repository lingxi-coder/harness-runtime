use crate::upstream::codec_fixtures::HistoryFixture;
use crate::upstream::codec_fixtures::{FixtureCodec, OpenAiResponsesCodec};
use base64::Engine as _;

use llm_runtime::{ContentBlock, Message, ToolChoice, ToolDeclaration};

const BASE_URL: &str = "https://api.openai.com/v1";

fn codec() -> OpenAiResponsesCodec {
    OpenAiResponsesCodec::new(BASE_URL)
}

// ── URL + headers ─────────────────────────────────────────────────────────────

#[test]
fn encode_request_posts_to_responses_url() {
    let provider_request = codec()
        .encode_request(&HistoryFixture::new("gpt-5").with_user_text("hi"))
        .unwrap();

    assert_eq!(provider_request.method, "POST");
    assert_eq!(provider_request.url, "https://api.openai.com/v1/responses");
}

#[test]
fn encode_request_trims_trailing_slash_on_base_url() {
    let codec = OpenAiResponsesCodec::new("https://api.openai.com/v1/");
    let provider_request = codec
        .encode_request(&HistoryFixture::new("gpt-5").with_user_text("hi"))
        .unwrap();

    assert_eq!(provider_request.url, "https://api.openai.com/v1/responses");
}

#[test]
fn encode_request_sets_content_type_header_only() {
    let provider_request = codec()
        .encode_request(&HistoryFixture::new("gpt-5").with_user_text("hi"))
        .unwrap();

    assert_eq!(
        provider_request
            .headers
            .get("content-type")
            .map(String::as_str),
        Some("application/json")
    );
    // Auth is added later by ModelRuntime::authenticate, never by the codec.
    assert_eq!(provider_request.headers.len(), 1);
}

#[test]
fn automatic_reasoning_omits_provider_override() {
    let provider_request = codec()
        .encode_request(&HistoryFixture::new("gpt-5").with_user_text("hi"))
        .unwrap();

    assert!(provider_request.body_json.get("reasoning").is_none());
}

#[test]
fn disabled_reasoning_encodes_explicit_none_effort() {
    let mut request = HistoryFixture::new("gpt-5").with_user_text("hi");
    request
        .request
        .set_effort(Some(serde_json::json!("none")))
        .unwrap();

    let provider_request = codec().encode_request(&request).unwrap();

    assert_eq!(
        provider_request.body_json["reasoning"],
        serde_json::json!({"effort": "none"})
    );
}

// ── instructions ──────────────────────────────────────────────────────────────

#[test]
fn system_blocks_join_into_instructions() {
    let mut request = HistoryFixture::new("gpt-5").with_user_text("hi");
    request.system = vec![
        llm_runtime::SystemBlock::text("a"),
        llm_runtime::SystemBlock::text("b"),
    ];

    let provider_request = codec().encode_request(&request).unwrap();

    assert_eq!(provider_request.body_json["instructions"], "a\n\nb");
}

#[test]
fn empty_system_omits_instructions() {
    let provider_request = codec()
        .encode_request(&HistoryFixture::new("gpt-5").with_user_text("hi"))
        .unwrap();

    assert!(provider_request.body_json.get("instructions").is_none());
}

// ── input items: messages ─────────────────────────────────────────────────────

#[test]
fn user_text_message_encodes_input_text_message_item() {
    let provider_request = codec()
        .encode_request(&HistoryFixture::new("gpt-5").with_user_text("hello"))
        .unwrap();

    assert_eq!(
        provider_request.body_json["input"],
        serde_json::json!([{
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "hello"}],
        }])
    );
}

#[test]
fn assistant_text_message_encodes_output_text_parts() {
    let mut request = HistoryFixture::new("gpt-5");
    request.messages.push(Message {
        role: "assistant".to_string(),
        content: vec![ContentBlock::Text {
            text: "answer".to_string(),
            cache_control: None,
        }],
    });

    let provider_request = codec().encode_request(&request).unwrap();

    assert_eq!(
        provider_request.body_json["input"],
        serde_json::json!([{
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "answer"}],
        }])
    );
}

#[test]
fn tool_call_block_encodes_top_level_function_call_item() {
    let mut request = HistoryFixture::new("gpt-5");
    request.messages.push(Message {
        role: "assistant".to_string(),
        content: vec![
            ContentBlock::Text {
                text: "running it".to_string(),
                cache_control: None,
            },
            ContentBlock::ToolCall {
                id: "call_1".to_string(),
                name: "Bash".to_string(),
                input: serde_json::json!({"command": "ls"}),
            },
        ],
    });

    let provider_request = codec().encode_request(&request).unwrap();

    let input = provider_request.body_json["input"].as_array().unwrap();
    assert_eq!(input.len(), 2);
    assert_eq!(
        input[0],
        serde_json::json!({
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "running it"}],
        })
    );
    // arguments is a JSON *string*, not an object (Responses API wire shape).
    assert_eq!(
        input[1],
        serde_json::json!({
            "type": "function_call",
            "call_id": "call_1",
            "name": "Bash",
            "arguments": "{\"command\":\"ls\"}",
        })
    );
}

#[test]
fn tool_result_block_encodes_function_call_output_item() {
    let mut request = HistoryFixture::new("gpt-5");
    request.messages.push(Message {
        role: "user".to_string(),
        content: vec![ContentBlock::ToolResult {
            tool_call_id: "call_1".to_string(),
            output: serde_json::json!("done"),
            is_error: false,
            cache_control: None,
            cache_reference: None,
        }],
    });

    let provider_request = codec().encode_request(&request).unwrap();

    assert_eq!(
        provider_request.body_json["input"],
        serde_json::json!([{
            "type": "function_call_output",
            "call_id": "call_1",
            "output": "done",
        }])
    );
}

#[test]
fn tool_result_non_string_output_is_stringified() {
    let mut request = HistoryFixture::new("gpt-5");
    request.messages.push(Message {
        role: "user".to_string(),
        content: vec![ContentBlock::ToolResult {
            tool_call_id: "call_2".to_string(),
            output: serde_json::json!({"exit_code": 0}),
            is_error: false,
            cache_control: None,
            cache_reference: None,
        }],
    });

    let provider_request = codec().encode_request(&request).unwrap();

    assert_eq!(
        provider_request.body_json["input"][0]["output"],
        "{\"exit_code\":0}"
    );
}

// ── input items: images / documents ───────────────────────────────────────────

#[test]
fn image_bytes_encode_input_image_data_uri() {
    let bytes = vec![1u8, 2, 3];
    let mut request = HistoryFixture::new("gpt-5");
    request.messages.push(Message {
        role: "user".to_string(),
        content: vec![ContentBlock::Image {
            media_type: "image/png".to_string(),
            bytes: bytes.clone(),
        }],
    });

    let provider_request = codec().encode_request(&request).unwrap();

    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    assert_eq!(
        provider_request.body_json["input"],
        serde_json::json!([{
            "type": "message",
            "role": "user",
            "content": [{
                "type": "input_image",
                "image_url": format!("data:image/png;base64,{b64}"),
            }],
        }])
    );
}

#[test]
fn image_url_encodes_input_image_url() {
    let mut request = HistoryFixture::new("gpt-5");
    request.messages.push(Message {
        role: "user".to_string(),
        content: vec![ContentBlock::ImageUrl {
            url: "https://example.com/img.png".to_string(),
        }],
    });

    let provider_request = codec().encode_request(&request).unwrap();

    assert_eq!(
        provider_request.body_json["input"][0]["content"][0],
        serde_json::json!({
            "type": "input_image",
            "image_url": "https://example.com/img.png",
        })
    );
}

#[test]
fn document_encodes_input_file_data_uri() {
    let pdf_bytes = vec![0x25u8, 0x50, 0x44, 0x46]; // %PDF
    let mut request = HistoryFixture::new("gpt-5");
    request.messages.push(Message {
        role: "user".to_string(),
        content: vec![ContentBlock::Document {
            media_type: "application/pdf".to_string(),
            bytes: pdf_bytes.clone(),
        }],
    });

    let provider_request = codec().encode_request(&request).unwrap();

    let b64 = base64::engine::general_purpose::STANDARD.encode(&pdf_bytes);
    assert_eq!(
        provider_request.body_json["input"][0]["content"][0],
        serde_json::json!({
            "type": "input_file",
            "filename": "document",
            "file_data": format!("data:application/pdf;base64,{b64}"),
        })
    );
}

#[test]
fn mixed_text_and_image_stay_one_message_item_in_order() {
    let mut request = HistoryFixture::new("gpt-5");
    request.messages.push(Message {
        role: "user".to_string(),
        content: vec![
            ContentBlock::Text {
                text: "look".to_string(),
                cache_control: None,
            },
            ContentBlock::ImageUrl {
                url: "https://example.com/a.png".to_string(),
            },
        ],
    });

    let provider_request = codec().encode_request(&request).unwrap();

    let input = provider_request.body_json["input"].as_array().unwrap();
    assert_eq!(input.len(), 1);
    let content = input[0]["content"].as_array().unwrap();
    assert_eq!(content.len(), 2);
    assert_eq!(content[0]["type"], "input_text");
    assert_eq!(content[1]["type"], "input_image");
}

// ── tools ─────────────────────────────────────────────────────────────────────

#[test]
fn tools_encode_flattened_responses_shape() {
    let mut request = HistoryFixture::new("gpt-5").with_user_text("hi");
    request.tools = vec![ToolDeclaration {
        name: "Read".to_string(),
        description: "read a file".to_string(),
        input_schema: serde_json::json!({"type": "object"}),
        ..Default::default()
    }];

    let provider_request = codec().encode_request(&request).unwrap();

    // Flattened Responses shape — NOT nested under "function" (that is Chat).
    assert_eq!(
        provider_request.body_json["tools"],
        serde_json::json!([{
            "type": "function",
            "name": "Read",
            "description": "read a file",
            "parameters": {"type": "object"},
            "strict": false,
        }])
    );
}

#[test]
fn no_tools_omits_tools_key() {
    let provider_request = codec()
        .encode_request(&HistoryFixture::new("gpt-5").with_user_text("hi"))
        .unwrap();

    assert!(provider_request.body_json.get("tools").is_none());
}

#[test]
fn default_tool_choice_is_auto_for_responses_parity() {
    let provider_request = codec()
        .encode_request(&HistoryFixture::new("gpt-5").with_user_text("hi"))
        .unwrap();

    assert!(provider_request.body_json.get("tool_choice").is_none());
}

#[test]
fn tool_choice_variants_encode() {
    let cases = [
        (ToolChoice::Auto, serde_json::json!("auto")),
        (ToolChoice::None, serde_json::json!("none")),
        (ToolChoice::Required, serde_json::json!("required")),
        (
            ToolChoice::Tool {
                name: "Read".to_string(),
            },
            serde_json::json!({"type": "function", "name": "Read"}),
        ),
    ];
    for (tool_choice, expected) in cases {
        let mut request = HistoryFixture::new("gpt-5").with_user_text("hi");
        request.tools.push(ToolDeclaration {
            name: "Read".into(),
            input_schema: serde_json::json!({"type":"object"}),
            ..Default::default()
        });
        request.request.set_tool_choice(Some(tool_choice));
        let provider_request = codec().encode_request(&request).unwrap();
        assert_eq!(provider_request.body_json["tool_choice"], expected);
    }
}

// ── sampling controls ─────────────────────────────────────────────────────────

#[test]
fn max_tokens_maps_to_max_output_tokens() {
    let mut request = HistoryFixture::new("gpt-5").with_user_text("hi");
    request.request.input.max_tokens = Some(1024);

    let provider_request = codec().encode_request(&request).unwrap();

    assert_eq!(provider_request.body_json["max_output_tokens"], 1024);
    assert!(provider_request.body_json.get("max_tokens").is_none());
}

#[test]
fn temperature_and_top_p_pass_through() {
    let mut request = HistoryFixture::new("gpt-5").with_user_text("hi");
    request.request.input.temperature = Some(0.5);
    request.request.input.controls.top_p = Some(0.9);

    let provider_request = codec().encode_request(&request).unwrap();

    assert_eq!(provider_request.body_json["temperature"], 0.5);
    assert_eq!(provider_request.body_json["top_p"], 0.9);

    let bare = codec()
        .encode_request(&HistoryFixture::new("gpt-5").with_user_text("hi"))
        .unwrap();
    assert!(bare.body_json.get("max_output_tokens").is_none());
    assert!(bare.body_json.get("temperature").is_none());
    assert!(bare.body_json.get("top_p").is_none());
}

#[test]
fn stop_sequences_are_rejected() {
    let mut request = HistoryFixture::new("gpt-5").with_user_text("hi");
    request.request.input.stop_sequences = vec!["END".to_string()];

    let err = codec().encode_request(&request).unwrap_err();

    assert!(
        matches!(err, llm_runtime::LlmError::InvalidRequest { ref message } if message.contains("stop")),
        "got: {err:?}"
    );
}

// ── reasoning effort mapping ──────────────────────────────────────────────────

#[test]
fn numeric_budget_is_rejected_instead_of_approximating_effort() {
    let mut request = HistoryFixture::new("gpt-5");
    request
        .request
        .set_reasoning(Some(llm_runtime::ReasoningConfig::Enabled {
            budget_tokens: 2048,
        }));
    assert!(matches!(
        codec().encode_request(&request),
        Err(llm_runtime::LlmError::UnsupportedCapability { .. })
    ));
}

#[test]
fn no_reasoning_config_omits_reasoning_key() {
    let provider_request = codec()
        .encode_request(&HistoryFixture::new("gpt-5").with_user_text("hi"))
        .unwrap();

    assert!(provider_request.body_json.get("reasoning").is_none());
    assert!(provider_request.body_json.get("include").is_none());
}

// ── response_format → text.format ─────────────────────────────────────────────

#[test]
fn response_format_json_object_encodes_text_format() {
    let mut request = HistoryFixture::new("gpt-5").with_user_text("hi");
    request.request.input.output_format = lingxi_llm_client::protocol::OutputFormat::JsonObject;

    let provider_request = codec().encode_request(&request).unwrap();

    assert_eq!(
        provider_request.body_json["text"],
        serde_json::json!({"format": {"type": "json_object"}})
    );
}

#[test]
fn response_format_json_schema_encodes_text_format() {
    let schema = serde_json::json!({
        "type": "object", "properties": {"x": {"type": "string"}},
        "required": ["x"], "additionalProperties": false
    });
    let mut request = HistoryFixture::new("gpt-5").with_user_text("hi");
    request.request.input.output_format = lingxi_llm_client::protocol::OutputFormat::JsonSchema {
        name: "response".into(),
        strict: true,
        schema: schema.clone(),
    };

    let provider_request = codec().encode_request(&request).unwrap();

    // Shape pinned to codex codex-api common.rs TextFormat {type, strict, schema, name}.
    assert_eq!(
        provider_request.body_json["text"],
        serde_json::json!({
            "format": {
                "type": "json_schema",
                "strict": true,
                "schema": schema,
                "name": "response",
            }
        })
    );
}

#[test]
fn no_response_format_omits_text_key() {
    let provider_request = codec()
        .encode_request(&HistoryFixture::new("gpt-5").with_user_text("hi"))
        .unwrap();

    assert!(provider_request.body_json.get("text").is_none());
}

// ── stream / store ────────────────────────────────────────────────────────────

#[test]
fn stream_true_sets_stream_in_body() {
    let mut request = HistoryFixture::new("gpt-5").with_user_text("hi");
    request.request.stream = true;

    let provider_request = codec().encode_request(&request).unwrap();

    assert_eq!(provider_request.body_json["stream"], true);
}

#[test]
fn stream_false_omits_stream_key() {
    let provider_request = codec()
        .encode_request(&HistoryFixture::new("gpt-5").with_user_text("hi"))
        .unwrap();

    assert!(provider_request.body_json.get("stream").is_none());
}

#[test]
fn store_defaults_false_for_non_azure() {
    let provider_request = codec()
        .encode_request(&HistoryFixture::new("gpt-5").with_user_text("hi"))
        .unwrap();

    assert_eq!(provider_request.body_json["store"], false);
}

#[test]
fn store_defaults_false_for_every_responses_connection() {
    let codec = OpenAiResponsesCodec::new("https://foo.openai.azure.com/openai");
    let provider_request = codec
        .encode_request(&HistoryFixture::new("gpt-5").with_user_text("hi"))
        .unwrap();

    assert_eq!(provider_request.body_json["store"], false);
}

#[test]
fn explicit_store_override_wins_over_azure_detection() {
    let codec = OpenAiResponsesCodec::new("https://foo.openai.azure.com/openai");
    let mut request = HistoryFixture::new("gpt-5").with_user_text("hi");
    request.request.input.controls.responses.store = Some(false);

    let provider_request = codec.encode_request(&request).unwrap();

    assert_eq!(provider_request.body_json["store"], false);
}

#[test]
fn codex_responses_options_encode_extra_request_fields() {
    let mut client_metadata = std::collections::BTreeMap::new();
    client_metadata.insert("session_id".to_string(), "sess_1".to_string());
    let mut request = HistoryFixture::new("gpt-5").with_user_text("hi");
    request.request.input.service_tier = Some(lingxi_llm_client::protocol::ServiceTier::Fast);
    request.request.input.controls.responses = lingxi_llm_client::protocol::ResponsesControls {
        parallel_tool_calls: Some(true),
        include: vec!["file_search_call.results".to_string()],
        prompt_cache_key: Some("cache-key".to_string()),
        client_metadata,
        store: None,
        previous_response_id: None,
        generate: None,
    };

    let provider_request = codec().encode_request(&request).unwrap();

    assert_eq!(provider_request.body_json["parallel_tool_calls"], true);
    assert_eq!(
        provider_request.body_json["include"],
        serde_json::json!(["file_search_call.results"])
    );
    assert_eq!(provider_request.body_json["service_tier"], "priority");
    assert_eq!(provider_request.body_json["prompt_cache_key"], "cache-key");
    assert_eq!(
        provider_request.body_json["client_metadata"],
        serde_json::json!({"session_id": "sess_1"})
    );
}

#[test]
fn reasoning_adds_encrypted_content_include_once() {
    let mut request = HistoryFixture::new("gpt-5").with_user_text("hi");
    request
        .request
        .set_effort(Some(serde_json::json!("medium")))
        .unwrap();
    request.request.input.controls.responses.include =
        vec!["reasoning.encrypted_content".to_string()];

    let provider_request = codec().encode_request(&request).unwrap();

    assert_eq!(
        provider_request.body_json["include"],
        serde_json::json!(["reasoning.encrypted_content"])
    );
}

// ── unsupported history blocks ────────────────────────────────────────────────

#[test]
fn reasoning_history_blocks_are_skipped_not_rejected() {
    // A resumed/continued conversation whose history carries a reasoning (or
    // redacted-thinking) block from a prior reasoning-model turn must ENCODE
    // fine — the blocks are dropped from the Responses input (we can't faithfully
    // echo them), not rejected. Regression: gpt-5.5 via Copilot failed "hello"
    // with "OpenAiResponsesCodec does not encode reasoning blocks yet".
    for block in [
        ContentBlock::Reasoning {
            text: "thought".to_string(),
            signature: None,
        },
        ContentBlock::RedactedThinking {
            data: "opaque".to_string(),
        },
    ] {
        let mut request = HistoryFixture::new("gpt-5").with_user_text("hello");
        // A prior assistant turn whose content is just a reasoning/thinking block.
        request.messages.insert(
            0,
            Message {
                role: "assistant".to_string(),
                content: vec![block],
            },
        );
        let encoded = codec()
            .encode_request(&request)
            .expect("reasoning history no longer rejected");
        let input = encoded.body_json["input"].as_array().expect("input array");
        // The reasoning/thinking block produced no input item; the user text did.
        assert!(
            !input.iter().any(|it| it["type"] == "reasoning"),
            "reasoning blocks are dropped, not encoded: {input:?}"
        );
        assert!(
            input.iter().any(|it| it["type"] == "message"),
            "the user turn still encodes: {input:?}"
        );
    }
}

#[test]
fn foreign_server_generated_blocks_are_omitted_from_replay() {
    for block in [
        ContentBlock::ServerToolUse {
            id: "srvtool_1".to_string(),
            name: "web_search".to_string(),
            input: serde_json::json!({}),
        },
        ContentBlock::ConnectorText {
            connector_text: "ct".to_string(),
            signature: None,
        },
        ContentBlock::AdvisorToolResult {
            tool_use_id: "srvtool_1".to_string(),
            content: serde_json::json!({}),
            is_error: false,
        },
    ] {
        let mut request = HistoryFixture::new("gpt-5").with_user_text("hello");
        request.messages.insert(
            0,
            Message {
                role: "assistant".to_string(),
                content: vec![block],
            },
        );
        let original = request.clone();
        let encoded = codec().encode_request(&request).unwrap();
        let input = encoded.body_json["input"].as_array().unwrap();
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["content"][0]["text"], "hello");
        assert_eq!(request, original);
    }
}

// ── full-body golden ──────────────────────────────────────────────────────────

#[test]
fn encode_request_full_body_golden() {
    let mut request = HistoryFixture::new("gpt-5").with_user_text("hello");
    request.system = vec![llm_runtime::SystemBlock::text("sys")];
    request.request.stream = true;
    request.request.input.max_tokens = Some(4096);
    request.tools = vec![ToolDeclaration {
        name: "Bash".to_string(),
        description: "run a command".to_string(),
        input_schema: serde_json::json!({"type": "object"}),
        ..Default::default()
    }];
    request.request.set_tool_choice(Some(ToolChoice::Auto));
    request
        .request
        .set_effort(Some(serde_json::json!("medium")))
        .unwrap();

    let provider_request = codec().encode_request(&request).unwrap();

    assert_eq!(
        provider_request.body_json,
        serde_json::json!({
            "model": "gpt-5",
            "instructions": "sys",
            "input": [{
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "hello"}],
            }],
            "tools": [{
                "type": "function",
                "name": "Bash",
                "description": "run a command",
                "parameters": {"type": "object"},
                "strict": false,
            }],
            "tool_choice": "auto",
            "parallel_tool_calls": false,
            "max_output_tokens": 4096,
            "reasoning": {"effort": "medium"},
            "include": ["reasoning.encrypted_content"],
            "stream": true,
            "store": false,
        })
    );
}

// ── decode_response ───────────────────────────────────────────────────────────
//
// Wire shapes pinned to the vendored Codex CLI reference:
// output items per codex-rs/protocol/src/models.rs ResponseItem
// (message{role,content:[{type:"output_text",text}]},
//  function_call{call_id,name,arguments-as-JSON-string},
//  reasoning{summary:[{type:"summary_text",text}]}),
// usage per codex-rs/codex-api/src/sse/responses.rs ResponseCompletedUsage.

fn decode(body: serde_json::Value) -> llm_runtime::HistoryResponse {
    codec()
        .decode_response(llm_runtime::ProviderResponse::json(200, body))
        .unwrap()
}

fn completed_body(output: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "id": "resp_1",
        "model": "gpt-5",
        "status": "completed",
        "output": output,
    })
}

#[test]
fn decode_response_parses_id_and_model() {
    let decoded = decode(completed_body(&serde_json::json!([])));
    assert_eq!(decoded.id, "resp_1");
    assert_eq!(decoded.model, "gpt-5");
}

#[test]
fn decode_response_message_output_text_parts_become_text_blocks() {
    let decoded = decode(completed_body(&serde_json::json!([{
        "type": "message",
        "role": "assistant",
        "content": [
            {"type": "output_text", "text": "hello"},
            {"type": "refusal", "refusal": "nope"},
            {"type": "output_text", "text": "world"},
        ],
    }])));
    assert_eq!(
        decoded.content,
        vec![
            ContentBlock::Text {
                text: "hello".to_string(),
                cache_control: None
            },
            ContentBlock::Text {
                text: "world".to_string(),
                cache_control: None
            },
        ]
    );
}

#[test]
fn decode_response_function_call_becomes_tool_call_with_parsed_arguments() {
    let decoded = decode(completed_body(&serde_json::json!([{
        "type": "function_call",
        "call_id": "call_7",
        "name": "Bash",
        "arguments": "{\"command\":\"ls\"}",
    }])));
    assert_eq!(
        decoded.content,
        vec![ContentBlock::ToolCall {
            id: "call_7".to_string(),
            name: "Bash".to_string(),
            input: serde_json::json!({"command": "ls"}),
        }]
    );
}

#[test]
fn malformed_tool_arguments_are_rejected_before_host_execution() {
    let body = completed_body(
        &serde_json::json!([{"type":"function_call","call_id":"c","name":"Bash","arguments":"{not json"}]),
    );
    assert!(matches!(
        codec().decode_response(llm_runtime::ProviderResponse::json(200, body)),
        Err(llm_runtime::LlmError::InvalidRequest { .. })
    ));
}

#[test]
fn decode_response_function_call_missing_call_id_errors() {
    let err = codec()
        .decode_response(llm_runtime::ProviderResponse::json(
            200,
            completed_body(&serde_json::json!([{
                "type": "function_call",
                "name": "Bash",
                "arguments": "{}",
            }])),
        ))
        .unwrap_err();
    assert!(matches!(err, llm_runtime::LlmError::InvalidRequest { .. }));
}

#[test]
fn native_reasoning_and_display_summary_are_both_preserved() {
    let native = serde_json::json!({"type":"reasoning","id":"rs_1","encrypted_content":"opaque","summary":[{"type":"summary_text","text":"first"},{"type":"summary_text","text":"second"}]});
    let response = decode(completed_body(&serde_json::json!([native.clone()])));
    assert!(
        matches!(&response.content[0],ContentBlock::ProviderContent { value, .. } if value == &native)
    );
    let texts: Vec<_> = response
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Reasoning { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, vec!["first", "second"]);
    let mut request = HistoryFixture::new("gpt-5");
    request.messages.push(Message {
        role: "assistant".into(),
        content: response.content,
    });
    let body = codec().encode_request(&request).unwrap().body_json;
    assert_eq!(body["input"], serde_json::json!([native]));
}

#[test]
fn encrypted_reasoning_without_summary_survives_replay() {
    let native = serde_json::json!({"type":"reasoning","id":"rs","encrypted_content":"opaque"});
    let response = decode(completed_body(&serde_json::json!([native.clone()])));
    assert!(
        matches!(response.content.as_slice(),[ContentBlock::ProviderContent { value, .. }] if value == &native)
    );
}

#[test]
fn decode_response_preserves_hosted_and_unknown_output_items() {
    let web_search = serde_json::json!({
        "type": "web_search_call", "id": "ws_1", "status": "completed"
    });
    let future_output = serde_json::json!({
        "type": "future_provider_item", "id": "native_1", "result": {"value": 42}
    });
    let decoded = decode(completed_body(&serde_json::json!([
        web_search.clone(),
        future_output.clone(),
        {"type": "message", "role": "assistant",
         "content": [{"type": "output_text", "text": "hi"}]},
    ])));
    assert_eq!(
        decoded.content,
        vec![
            ContentBlock::ProviderContent {
                protocol: "open_ai_responses".into(),
                value: web_search,
            },
            ContentBlock::ProviderContent {
                protocol: "open_ai_responses".into(),
                value: future_output,
            },
            ContentBlock::Text {
                text: "hi".to_string(),
                cache_control: None,
            },
        ]
    );
}

#[test]
fn decode_response_preserves_output_item_order() {
    let decoded = decode(completed_body(&serde_json::json!([
        {"type": "reasoning", "id": "rs_1",
         "summary": [{"type": "summary_text", "text": "thinking"}]},
        {"type": "message", "role": "assistant",
         "content": [{"type": "output_text", "text": "I'll run it"}]},
        {"type": "function_call", "call_id": "call_1", "name": "Bash",
         "arguments": "{}"},
    ])));
    let visible: Vec<_> = decoded
        .content
        .into_iter()
        .filter(|b| !matches!(b, ContentBlock::ProviderContent { .. }))
        .collect();
    assert_eq!(
        visible,
        vec![
            ContentBlock::Reasoning {
                text: "thinking".to_string(),
                signature: None
            },
            ContentBlock::Text {
                text: "I'll run it".to_string(),
                cache_control: None
            },
            ContentBlock::ToolCall {
                id: "call_1".to_string(),
                name: "Bash".to_string(),
                input: serde_json::json!({}),
            },
        ]
    );
}

#[test]
fn decode_response_completed_maps_end_turn() {
    let decoded = decode(completed_body(&serde_json::json!([])));
    assert_eq!(decoded.stop_reason.as_deref(), Some("end_turn"));
}

#[test]
fn decode_response_completed_with_function_call_maps_tool_use() {
    let decoded = decode(completed_body(&serde_json::json!([{
        "type": "function_call", "call_id": "call_1", "name": "Bash", "arguments": "{}",
    }])));
    assert_eq!(decoded.stop_reason.as_deref(), Some("tool_use"));
}

#[test]
fn decode_response_incomplete_max_output_tokens_maps_max_tokens() {
    let decoded = decode(serde_json::json!({
        "id": "resp_1",
        "model": "gpt-5",
        "status": "incomplete",
        "incomplete_details": {"reason": "max_output_tokens"},
        "output": [],
    }));
    assert_eq!(decoded.stop_reason.as_deref(), Some("max_tokens"));
}

#[test]
fn decode_response_incomplete_other_reason_passes_through_verbatim() {
    let decoded = decode(serde_json::json!({
        "id": "resp_1",
        "model": "gpt-5",
        "status": "incomplete",
        "incomplete_details": {"reason": "content_filter"},
        "output": [],
    }));
    assert_eq!(decoded.stop_reason.as_deref(), Some("refusal"));
}

#[test]
fn decode_response_incomplete_without_reason_passes_status_verbatim() {
    let decoded = decode(serde_json::json!({
        "id": "resp_1",
        "model": "gpt-5",
        "status": "incomplete",
        "output": [],
    }));
    assert_eq!(decoded.stop_reason.as_deref(), Some("incomplete"));
}

#[test]
fn decode_response_usage_subset_normalization() {
    let decoded = decode(serde_json::json!({
        "id": "resp_1",
        "model": "gpt-5",
        "status": "completed",
        "output": [],
        "usage": {
            "input_tokens": 100,
            "input_tokens_details": {"cached_tokens": 30},
            "output_tokens": 50,
            "output_tokens_details": {"reasoning_tokens": 20},
            "total_tokens": 150,
        },
    }));
    assert_eq!(decoded.usage.billable_tokens.input, 70);
    assert_eq!(decoded.usage.billable_tokens.cache_read, 30);
    assert_eq!(decoded.usage.billable_tokens.output, 30);
    assert_eq!(decoded.usage.billable_tokens.reasoning_output, 20);
    assert_eq!(decoded.usage.billable_tokens.cache_write, 0);
    assert_eq!(decoded.usage.provider_reported_total_tokens, Some(150));
    assert_eq!(decoded.usage.context_tokens, Some(150));
}

#[test]
fn decode_response_usage_subtraction_saturates() {
    let decoded = decode(serde_json::json!({
        "id": "resp_1",
        "model": "gpt-5",
        "status": "completed",
        "output": [],
        "usage": {
            "input_tokens": 10,
            "input_tokens_details": {"cached_tokens": 30},
            "output_tokens": 5,
            "output_tokens_details": {"reasoning_tokens": 20},
            "total_tokens": 15,
        },
    }));
    assert_eq!(decoded.usage.billable_tokens.input, 0);
    assert_eq!(decoded.usage.billable_tokens.output, 0);
    assert_eq!(decoded.usage.billable_tokens.cache_read, 30);
    assert_eq!(decoded.usage.billable_tokens.reasoning_output, 20);
}

#[test]
fn decode_response_usage_missing_details_objects_zero_tolerantly() {
    let decoded = decode(serde_json::json!({
        "id": "resp_1",
        "model": "gpt-5",
        "status": "completed",
        "output": [],
        "usage": {
            "input_tokens": 100,
            "output_tokens": 50,
            "total_tokens": 150,
        },
    }));
    assert_eq!(decoded.usage.billable_tokens.input, 100);
    assert_eq!(decoded.usage.billable_tokens.cache_read, 0);
    assert_eq!(decoded.usage.billable_tokens.output, 50);
    assert_eq!(decoded.usage.billable_tokens.reasoning_output, 0);
}

#[test]
fn decode_response_missing_usage_defaults_to_zero() {
    let decoded = decode(completed_body(&serde_json::json!([])));
    assert_eq!(decoded.usage, llm_runtime::Usage::default());
}

#[test]
fn decode_response_cost_is_none_and_metadata_is_body() {
    let body = serde_json::json!({
        "id": "resp_1",
        "model": "gpt-5",
        "status": "completed",
        "output": [],
        "vendor_extra": {"k": "v"},
    });
    let decoded = decode(body.clone());
    assert!(decoded.cost.is_none());
    assert_eq!(decoded.provider_metadata, body);
}

#[test]
fn missing_response_id_does_not_invent_a_continuation_id() {
    let response = decode(serde_json::json!({"model":"gpt-5","status":"completed","output":[]}));
    assert!(response.id.is_empty());
}

#[test]
fn decode_response_http_429_maps_rate_limited() {
    let err = codec()
        .decode_response(llm_runtime::ProviderResponse::json(
            429,
            serde_json::json!({"error": {"message": "slow down", "type": "rate_limit_error"}}),
        ))
        .unwrap_err();
    assert!(matches!(err, llm_runtime::LlmError::RateLimited { .. }));
}

#[test]
fn decode_response_http_401_maps_authentication() {
    let err = codec()
        .decode_response(llm_runtime::ProviderResponse::json(
            401,
            serde_json::json!({"error": {"message": "bad key", "code": "invalid_api_key"}}),
        ))
        .unwrap_err();
    assert!(matches!(err, llm_runtime::LlmError::Authentication { .. }));
}

#[test]
fn decode_response_insufficient_quota_maps_quota_exceeded() {
    let err = codec()
        .decode_response(llm_runtime::ProviderResponse::json(
            429,
            serde_json::json!({"error": {"message": "quota", "code": "insufficient_quota"}}),
        ))
        .unwrap_err();
    assert!(matches!(err, llm_runtime::LlmError::QuotaExceeded));
}

#[test]
fn decode_response_http_400_maps_invalid_request_with_message() {
    let err = codec()
        .decode_response(llm_runtime::ProviderResponse::json(
            400,
            serde_json::json!({"error": {"message": "bad input", "type": "invalid_request_error"}}),
        ))
        .unwrap_err();
    // The stored text is the SDK's `makeMessage` form — `${status} ${body}` —
    // not the bare `error.message` this used to assert. The status prefix is how
    // claude-code carries HTTP status provenance, and dropping it is what left
    // the port unable to tell a 422 from a 400 downstream.
    match err {
        llm_runtime::LlmError::InvalidRequest { message } => assert_eq!(message, "400 bad input"),
        other => panic!("expected InvalidRequest, got {other:?}"),
    }
}

// ── stream decoder ────────────────────────────────────────────────────────────

use llm_runtime::{
    HistoryContentDelta, HistoryEvent, HistoryMessageDelta, HistoryResponse, RawStreamFrame,
    TokenUsage, Usage,
};

/// Feed SSE data payloads (already de-framed) through a fresh stream decoder.
fn decode_stream(frames: &[serde_json::Value]) -> Vec<HistoryEvent> {
    let mut decoder = codec().stream_decoder();
    let mut events = Vec::new();
    for frame in frames {
        events.extend(
            decoder
                .decode_frame(RawStreamFrame::new(frame.to_string().into_bytes()))
                .unwrap(),
        );
    }
    events
}

fn decode_stream_with_metadata(
    frames: &[serde_json::Value],
    metadata: serde_json::Value,
) -> Vec<HistoryEvent> {
    let mut decoder = codec().stream_decoder();
    decoder.set_provider_metadata(metadata);
    let mut events = Vec::new();
    for frame in frames {
        events.extend(
            decoder
                .decode_frame(RawStreamFrame::new(frame.to_string().into_bytes()))
                .unwrap(),
        );
    }
    events
}

fn created_frame() -> serde_json::Value {
    serde_json::json!({
        "type": "response.created",
        "response": {"id": "resp_1", "model": "gpt-5", "status": "in_progress"},
    })
}

#[test]
fn stream_provider_metadata_is_retained_on_start_and_terminal_usage() {
    let metadata = serde_json::json!({
        "openai-model": "gpt-5-2026-06-01",
        "x-models-etag": "etag-1",
        "x-codex-turn-state": "turn-state",
        "x-ratelimit-limit-requests": "1000",
    });
    let usage_json = serde_json::json!({
        "input_tokens": 2,
        "output_tokens": 3,
        "total_tokens": 5,
    });
    let events = decode_stream_with_metadata(
        &[
            created_frame(),
            serde_json::json!({
                "type": "response.completed",
                "response": {
                    "id": "resp_1",
                    "model": "gpt-5",
                    "status": "completed",
                    "usage": usage_json,
                },
            }),
        ],
        metadata.clone(),
    );

    assert!(matches!(
        &events[0],
        HistoryEvent::MessageStart { response }
            if response.provider_metadata == metadata
    ));
    assert!(matches!(
        &events[1],
        HistoryEvent::MessageDelta { usage: Some(usage), .. }
            if usage.provider_metadata["stream"] == metadata && usage.billable_tokens.input == 2 && usage.billable_tokens.output == 3
    ));
}

#[test]
fn stream_created_emits_message_start_snapshot() {
    let events = decode_stream(&[created_frame()]);

    assert_eq!(
        events,
        vec![HistoryEvent::MessageStart {
            response: Box::new(HistoryResponse {
                id: "resp_1".to_string(),
                model: "gpt-5".to_string(),
                content: Vec::new(),
                stop_reason: None,
                stop_details: None,
                usage: Usage::default(),
                cost: None,
                provider_metadata: serde_json::Value::Null,
            }),
        }]
    );
}

#[test]
fn stream_text_delta_opens_block_once_then_deltas() {
    let events = decode_stream(&[
        created_frame(),
        serde_json::json!({
            "type": "response.output_text.delta",
            "item_id": "msg_1", "output_index": 0, "content_index": 0,
            "delta": "Hel",
        }),
        serde_json::json!({
            "type": "response.output_text.delta",
            "item_id": "msg_1", "output_index": 0, "content_index": 0,
            "delta": "lo",
        }),
    ]);

    assert_eq!(events.len(), 4);
    assert_eq!(
        events[1],
        HistoryEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlock::Text {
                text: String::new(),
                cache_control: None,
            },
        }
    );
    assert_eq!(
        events[2],
        HistoryEvent::ContentBlockDelta {
            index: 0,
            delta: HistoryContentDelta::TextDelta {
                text: "Hel".to_string()
            },
        }
    );
    assert_eq!(
        events[3],
        HistoryEvent::ContentBlockDelta {
            index: 0,
            delta: HistoryContentDelta::TextDelta {
                text: "lo".to_string()
            },
        }
    );
}

#[test]
fn stream_function_call_added_starts_tool_block_with_empty_input() {
    let events = decode_stream(&[
        created_frame(),
        serde_json::json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "Bash", "arguments": ""},
        }),
    ]);

    assert_eq!(
        events[1],
        HistoryEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlock::ToolCall {
                id: "call_1".to_string(),
                name: "Bash".to_string(),
                input: serde_json::Value::Object(serde_json::Map::new()),
            },
        }
    );
}

#[test]
fn stream_function_call_arguments_delta_emits_input_json_delta() {
    let events = decode_stream(&[
        created_frame(),
        serde_json::json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "Bash", "arguments": ""},
        }),
        serde_json::json!({
            "type": "response.function_call_arguments.delta",
            "item_id": "fc_1", "output_index": 0,
            "delta": "{\"command\":",
        }),
        serde_json::json!({
            "type": "response.function_call_arguments.delta",
            "item_id": "fc_1", "output_index": 0,
            "delta": "\"ls\"}",
        }),
    ]);

    assert_eq!(
        &events[2..],
        &[
            HistoryEvent::ContentBlockDelta {
                index: 0,
                delta: HistoryContentDelta::InputJsonDelta {
                    partial_json: "{\"command\":".to_string(),
                },
            },
            HistoryEvent::ContentBlockDelta {
                index: 0,
                delta: HistoryContentDelta::InputJsonDelta {
                    partial_json: "\"ls\"}".to_string(),
                },
            },
        ]
    );
}

#[test]
fn stream_reasoning_text_delta_opens_reasoning_block_once() {
    let events = decode_stream(&[
        created_frame(),
        serde_json::json!({
            "type": "response.reasoning_text.delta",
            "item_id": "rs_1", "output_index": 0, "content_index": 0,
            "delta": "thinking ",
        }),
        serde_json::json!({
            "type": "response.reasoning_text.delta",
            "item_id": "rs_1", "output_index": 0, "content_index": 0,
            "delta": "hard",
        }),
    ]);

    assert_eq!(events.len(), 4);
    assert_eq!(
        events[1],
        HistoryEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlock::Reasoning {
                text: String::new(),
                signature: None,
            },
        }
    );
    assert_eq!(
        events[2],
        HistoryEvent::ContentBlockDelta {
            index: 0,
            delta: HistoryContentDelta::ThinkingDelta {
                thinking: "thinking ".to_string(),
            },
        }
    );
}

#[test]
fn stream_reasoning_summary_delta_shares_reasoning_block() {
    // Both reasoning_text.delta and reasoning_summary_text.delta target the
    // same item's single Reasoning block: one start, two thinking deltas.
    let events = decode_stream(&[
        created_frame(),
        serde_json::json!({
            "type": "response.reasoning_summary_text.delta",
            "item_id": "rs_1", "output_index": 0, "summary_index": 0,
            "delta": "summary ",
        }),
        serde_json::json!({
            "type": "response.reasoning_text.delta",
            "item_id": "rs_1", "output_index": 0, "content_index": 0,
            "delta": "raw",
        }),
    ]);

    let starts = events
        .iter()
        .filter(|event| matches!(event, HistoryEvent::ContentBlockStart { .. }))
        .count();
    assert_eq!(starts, 1);
    let thinking: String = events
        .iter()
        .filter_map(|event| match event {
            HistoryEvent::ContentBlockDelta {
                delta: HistoryContentDelta::ThinkingDelta { thinking },
                ..
            } => Some(thinking.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(thinking, "summary raw");
}

#[test]
fn stream_output_item_done_emits_content_block_stop() {
    let events = decode_stream(&[
        created_frame(),
        serde_json::json!({
            "type": "response.output_text.delta",
            "item_id": "msg_1", "output_index": 0, "content_index": 0,
            "delta": "hi",
        }),
        serde_json::json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {"type": "message", "id": "msg_1", "role": "assistant"},
        }),
    ]);

    assert_eq!(
        events.last(),
        Some(&HistoryEvent::ContentBlockStop { index: 0 })
    );
}

#[test]
fn stream_output_index_keys_block_mapping() {
    // Interleaved deltas for two wire items route to two sequential block
    // indices keyed by output_index.
    let events = decode_stream(&[
        created_frame(),
        serde_json::json!({
            "type": "response.reasoning_text.delta",
            "item_id": "rs_1", "output_index": 0, "content_index": 0,
            "delta": "think",
        }),
        serde_json::json!({
            "type": "response.output_text.delta",
            "item_id": "msg_1", "output_index": 1, "content_index": 0,
            "delta": "answer",
        }),
        serde_json::json!({
            "type": "response.reasoning_text.delta",
            "item_id": "rs_1", "output_index": 0, "content_index": 0,
            "delta": " more",
        }),
    ]);

    assert_eq!(
        events[1],
        HistoryEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlock::Reasoning {
                text: String::new(),
                signature: None,
            },
        }
    );
    assert_eq!(
        events[3],
        HistoryEvent::ContentBlockStart {
            index: 1,
            content_block: ContentBlock::Text {
                text: String::new(),
                cache_control: None,
            },
        }
    );
    assert_eq!(
        events[5],
        HistoryEvent::ContentBlockDelta {
            index: 0,
            delta: HistoryContentDelta::ThinkingDelta {
                thinking: " more".to_string(),
            },
        }
    );
}

#[test]
fn stream_completed_closes_open_blocks_then_message_delta_and_stop() {
    let usage_json = serde_json::json!({
        "input_tokens": 100,
        "input_tokens_details": {"cached_tokens": 40},
        "output_tokens": 50,
        "output_tokens_details": {"reasoning_tokens": 30},
        "total_tokens": 150,
    });
    let events = decode_stream(&[
        created_frame(),
        serde_json::json!({
            "type": "response.output_text.delta",
            "item_id": "msg_1", "output_index": 0, "content_index": 0,
            "delta": "hi",
        }),
        serde_json::json!({
            "type": "response.completed",
            "response": {
                "id": "resp_1", "model": "gpt-5", "status": "completed",
                "usage": usage_json,
            },
        }),
    ]);

    // The still-open text block is closed before the terminal events.
    assert_eq!(
        &events[3..],
        &[
            HistoryEvent::ContentBlockStop { index: 0 },
            HistoryEvent::MessageDelta {
                delta: HistoryMessageDelta {
                    stop_reason: Some("end_turn".to_string()),
                    stop_details: None,
                },
                usage: Some(Usage {
                    billable_tokens: TokenUsage {
                        input: 60,
                        output: 20,
                        cache_read: 40,
                        reasoning_output: 30,
                        ..Default::default()
                    },
                    context_tokens: Some(150),
                    provider_reported_total_tokens: Some(150),
                    provider_metadata: serde_json::json!({"upstreamUsageState":"complete","input_tokens":60,"output_tokens":50,"cache_creation_input_tokens":0,"cache_read_input_tokens":40,"cache_creation":{"ephemeral_1h_input_tokens":0}}),
                    ..Default::default()
                }),
            },
            HistoryEvent::MessageStop,
        ]
    );
}

#[test]
fn stream_completed_after_function_call_maps_tool_use() {
    let events = decode_stream(&[
        created_frame(),
        serde_json::json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "Bash", "arguments": ""},
        }),
        serde_json::json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "Bash", "arguments": "{}"},
        }),
        serde_json::json!({
            "type": "response.completed",
            "response": {"id": "resp_1", "model": "gpt-5", "status": "completed"},
        }),
    ]);

    let stop_reason = events
        .iter()
        .find_map(|event| match event {
            HistoryEvent::MessageDelta { delta, .. } => delta.stop_reason.clone(),
            _ => None,
        })
        .expect("terminal stop reason");
    assert_eq!(stop_reason, "tool_use");
}

#[test]
fn stream_incomplete_max_output_tokens_maps_max_tokens() {
    let events = decode_stream(&[
        created_frame(),
        serde_json::json!({
            "type": "response.incomplete",
            "response": {
                "id": "resp_1", "model": "gpt-5", "status": "incomplete",
                "incomplete_details": {"reason": "max_output_tokens"},
            },
        }),
    ]);

    assert_eq!(
        &events[1..],
        &[
            HistoryEvent::MessageDelta {
                delta: HistoryMessageDelta {
                    stop_reason: Some("max_tokens".to_string()),
                    stop_details: None,
                },
                usage: None,
            },
            HistoryEvent::MessageStop,
        ]
    );
}

#[test]
fn stream_failed_maps_error_taxonomy() {
    let mut decoder = codec().stream_decoder();
    decoder
        .decode_frame(RawStreamFrame::new(
            created_frame().to_string().into_bytes(),
        ))
        .unwrap();
    let err = decoder
        .decode_frame(RawStreamFrame::new(
            serde_json::json!({
                "type": "response.failed",
                "response": {
                    "id": "resp_1", "status": "failed",
                    "error": {"code": "context_length_exceeded", "message": "too long"},
                },
            })
            .to_string()
            .into_bytes(),
        ))
        .unwrap_err();

    assert!(matches!(err, llm_runtime::LlmError::ContextOverflow { .. }));
}

#[test]
fn stream_failed_quota_maps_quota_exceeded() {
    let mut decoder = codec().stream_decoder();
    let err = decoder
        .decode_frame(RawStreamFrame::new(
            serde_json::json!({
                "type": "response.failed",
                "response": {
                    "id": "resp_1", "status": "failed",
                    "error": {"code": "insufficient_quota", "message": "quota gone"},
                },
            })
            .to_string()
            .into_bytes(),
        ))
        .unwrap_err();

    assert!(matches!(err, llm_runtime::LlmError::QuotaExceeded));
}

#[test]
fn stream_failed_rate_limit_exceeded_maps_rate_limited() {
    // Wire fixture pinned to codex codex-api/src/sse/responses.rs
    // error_when_error_event: a response.failed carrying rate_limit_exceeded
    // must map to RateLimited (retryable), with the retry-after delay parsed
    // out of the "Please try again in 11.054s" message text.
    let mut decoder = codec().stream_decoder();
    let err = decoder
        .decode_frame(RawStreamFrame::new(
            serde_json::json!({
                "type": "response.failed",
                "response": {
                    "id": "resp_1", "status": "failed",
                    "error": {
                        "code": "rate_limit_exceeded",
                        "message": "Rate limit reached for gpt-5.1 in organization org-AAA on tokens per min (TPM): Limit 30000, Used 22999, Requested 12528. Please try again in 11.054s. Visit https://platform.openai.com/account/rate-limits to learn more.",
                    },
                },
            })
            .to_string()
            .into_bytes(),
        ))
        .unwrap_err();

    match err {
        llm_runtime::LlmError::RateLimited { retry_after, scope } => {
            assert_eq!(
                retry_after,
                Some(std::time::Duration::from_secs_f64(11.054))
            );
            assert_eq!(scope, None);
        }
        other => panic!("expected RateLimited, got: {other:?}"),
    }
}

#[test]
fn stream_failed_rate_limit_exceeded_without_delay_text_still_maps_rate_limited() {
    let mut decoder = codec().stream_decoder();
    let err = decoder
        .decode_frame(RawStreamFrame::new(
            serde_json::json!({
                "type": "response.failed",
                "response": {
                    "id": "resp_1", "status": "failed",
                    "error": {"code": "rate_limit_exceeded", "message": "Rate limit reached."},
                },
            })
            .to_string()
            .into_bytes(),
        ))
        .unwrap_err();

    assert!(
        matches!(
            err,
            llm_runtime::LlmError::RateLimited {
                retry_after: None,
                ..
            }
        ),
        "got: {err:?}"
    );
}

#[test]
fn websocket_wrapped_rate_limit_error_maps_through_openai_taxonomy() {
    let mut decoder = codec().stream_decoder();
    let err = decoder
        .decode_frame(RawStreamFrame::new(
            serde_json::json!({
                "type": "error",
                "status": 429,
                "error": {"type": "rate_limit_error", "message": "slow down"},
                "headers": {"retry-after": "7", "x-request-id": "req_ws"},
            })
            .to_string()
            .into_bytes(),
        ))
        .unwrap_err();

    match err {
        llm_runtime::LlmError::RateLimited { retry_after, scope } => {
            assert_eq!(retry_after, Some(std::time::Duration::from_secs(7)));
            assert_eq!(scope, None);
        }
        other => panic!("expected RateLimited, got: {other:?}"),
    }
}

#[test]
fn websocket_connection_limit_reached_is_retryable_rate_limit() {
    let mut decoder = codec().stream_decoder();
    let err = decoder
        .decode_frame(RawStreamFrame::new(
            serde_json::json!({
                "type": "error",
                "status": 429,
                "error": {
                    "code": "websocket_connection_limit_reached",
                    "message": "too many websocket connections",
                },
            })
            .to_string()
            .into_bytes(),
        ))
        .unwrap_err();

    assert!(matches!(
        err,
        llm_runtime::LlmError::RateLimited {
            retry_after: None,
            scope: None
        }
    ));
}

#[test]
fn stream_failed_invalid_prompt_maps_invalid_request() {
    // codex responses.rs is_invalid_prompt_error: code "invalid_prompt" is a
    // non-retryable invalid request, not a generic 5xx fallthrough.
    let mut decoder = codec().stream_decoder();
    let err = decoder
        .decode_frame(RawStreamFrame::new(
            serde_json::json!({
                "type": "response.failed",
                "response": {
                    "id": "resp_1", "status": "failed",
                    "error": {"code": "invalid_prompt", "message": "prompt was flagged"},
                },
            })
            .to_string()
            .into_bytes(),
        ))
        .unwrap_err();

    assert!(
        matches!(
            err,
            llm_runtime::LlmError::InvalidRequest { ref message } if message.contains("prompt was flagged")
        ),
        "got: {err:?}"
    );
}

#[test]
fn stream_failed_unknown_code_falls_back_to_provider_internal() {
    // Genuinely unknown codes keep the generic 5xx (retryable) fallthrough.
    let mut decoder = codec().stream_decoder();
    let err = decoder
        .decode_frame(RawStreamFrame::new(
            serde_json::json!({
                "type": "response.failed",
                "response": {
                    "id": "resp_1", "status": "failed",
                    "error": {"code": "some_future_code", "message": "??"},
                },
            })
            .to_string()
            .into_bytes(),
        ))
        .unwrap_err();

    assert!(
        matches!(err, llm_runtime::LlmError::ProviderInternal),
        "got: {err:?}"
    );
}

#[test]
fn anonymous_tool_arguments_are_rejected() {
    let mut decoder = codec().stream_decoder();
    let frame = serde_json::json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":"{}"});
    assert!(matches!(
        decoder.decode_frame(RawStreamFrame::new(serde_json::to_vec(&frame).unwrap())),
        Err(llm_runtime::LlmError::InvalidRequest { .. })
    ));
}

#[test]
fn stream_done_sentinel_is_tolerated() {
    // The Responses API ends after response.completed without a [DONE]
    // sentinel, but OpenAI-compatible gateways may append one: it must not
    // error and must not duplicate the terminal events.
    let mut decoder = codec().stream_decoder();
    let mut events = Vec::new();
    for frame in [
        created_frame(),
        serde_json::json!({
            "type": "response.completed",
            "response": {"id": "resp_1", "model": "gpt-5", "status": "completed"},
        }),
    ] {
        events.extend(
            decoder
                .decode_frame(RawStreamFrame::new(frame.to_string().into_bytes()))
                .unwrap(),
        );
    }
    assert_eq!(events.last(), Some(&HistoryEvent::MessageStop));
    let terminal_count = events.len();

    let extra = decoder
        .decode_frame(RawStreamFrame::new(b"[DONE]".to_vec()))
        .unwrap();

    assert_eq!(extra, Vec::new());
    assert_eq!(events.len(), terminal_count);
}

#[test]
fn stream_content_delta_before_created_emits_synthetic_message_start() {
    // Documented tolerance (ensure_started): content events arriving before
    // response.created synthesize an empty MessageStart snapshot first,
    // mirroring OpenAiChatCodec.
    let events = decode_stream(&[serde_json::json!({
        "type": "response.output_text.delta",
        "item_id": "msg_1", "output_index": 0, "content_index": 0,
        "delta": "hi",
    })]);

    assert_eq!(
        &events[..2],
        &[
            HistoryEvent::MessageStart {
                response: Box::new(HistoryResponse {
                    id: String::new(),
                    model: String::new(),
                    content: Vec::new(),
                    stop_reason: None,
                    stop_details: None,
                    usage: Usage::default(),
                    cost: None,
                    provider_metadata: serde_json::Value::Null,
                }),
            },
            HistoryEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlock::Text {
                    text: String::new(),
                    cache_control: None,
                },
            },
        ]
    );
}

#[test]
fn stream_unknown_event_types_ignored() {
    let events = decode_stream(&[
        created_frame(),
        serde_json::json!({"type": "response.in_progress", "response": {"id": "resp_1"}}),
        serde_json::json!({"type": "response.output_text.done", "item_id": "msg_1", "output_index": 0, "text": "hi"}),
        serde_json::json!({"type": "response.some_future_event"}),
    ]);

    assert_eq!(events.len(), 1);
    assert!(matches!(events[0], HistoryEvent::MessageStart { .. }));
}

#[test]
fn truncated_stream_does_not_report_success() {
    let mut decoder = codec().stream_decoder();
    let frame =
        serde_json::json!({"type":"response.output_text.delta","output_index":0,"delta":"partial"});
    decoder
        .decode_frame(RawStreamFrame::new(serde_json::to_vec(&frame).unwrap()))
        .unwrap();
    assert!(matches!(
        decoder.finish(),
        Err(llm_runtime::LlmError::StreamInterrupted { .. })
    ));
    assert!(decoder.finish().unwrap().is_empty());
}

#[test]
fn stream_delta_without_output_index_defaults_to_slot_zero() {
    // Mirror the OpenAiChatCodec tolerance: a single-item stream that omits
    // output_index routes to slot zero instead of dropping the fragment.
    let events = decode_stream(&[
        created_frame(),
        serde_json::json!({"type": "response.output_text.delta", "item_id": "msg_1", "delta": "a"}),
        serde_json::json!({"type": "response.output_text.delta", "item_id": "msg_1", "delta": "b"}),
    ]);

    let starts = events
        .iter()
        .filter(|event| matches!(event, HistoryEvent::ContentBlockStart { .. }))
        .count();
    assert_eq!(starts, 1);
    let text: String = events
        .iter()
        .filter_map(|event| match event {
            HistoryEvent::ContentBlockDelta {
                delta: HistoryContentDelta::TextDelta { text },
                ..
            } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "ab");
}

#[test]
#[allow(clippy::too_many_lines)] // full-transcript pin: every event in sequence
fn stream_happy_path_exact_event_sequence() {
    let usage_json = serde_json::json!({
        "input_tokens": 100,
        "input_tokens_details": {"cached_tokens": 40},
        "output_tokens": 50,
        "output_tokens_details": {"reasoning_tokens": 30},
        "total_tokens": 150,
    });
    let events = decode_stream(&[
        created_frame(),
        serde_json::json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {"type": "message", "id": "msg_1", "role": "assistant"},
        }),
        serde_json::json!({
            "type": "response.output_text.delta",
            "item_id": "msg_1", "output_index": 0, "content_index": 0,
            "delta": "On ",
        }),
        serde_json::json!({
            "type": "response.output_text.delta",
            "item_id": "msg_1", "output_index": 0, "content_index": 0,
            "delta": "it.",
        }),
        serde_json::json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {"type": "message", "id": "msg_1", "role": "assistant"},
        }),
        serde_json::json!({
            "type": "response.output_item.added",
            "output_index": 1,
            "item": {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "Bash", "arguments": ""},
        }),
        serde_json::json!({
            "type": "response.function_call_arguments.delta",
            "item_id": "fc_1", "output_index": 1,
            "delta": "{\"command\":",
        }),
        serde_json::json!({
            "type": "response.function_call_arguments.delta",
            "item_id": "fc_1", "output_index": 1,
            "delta": "\"ls\"}",
        }),
        serde_json::json!({
            "type": "response.output_item.done",
            "output_index": 1,
            "item": {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "Bash", "arguments": "{\"command\":\"ls\"}"},
        }),
        serde_json::json!({
            "type": "response.completed",
            "response": {
                "id": "resp_1", "model": "gpt-5", "status": "completed",
                "usage": usage_json,
            },
        }),
    ]);

    assert_eq!(
        events,
        vec![
            HistoryEvent::MessageStart {
                response: Box::new(HistoryResponse {
                    id: "resp_1".to_string(),
                    model: "gpt-5".to_string(),
                    content: Vec::new(),
                    stop_reason: None,
                    stop_details: None,
                    usage: Usage::default(),
                    cost: None,
                    provider_metadata: serde_json::Value::Null,
                }),
            },
            HistoryEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlock::Text {
                    text: String::new(),
                    cache_control: None,
                },
            },
            HistoryEvent::ContentBlockDelta {
                index: 0,
                delta: HistoryContentDelta::TextDelta {
                    text: "On ".to_string()
                },
            },
            HistoryEvent::ContentBlockDelta {
                index: 0,
                delta: HistoryContentDelta::TextDelta {
                    text: "it.".to_string()
                },
            },
            HistoryEvent::ContentBlockStop { index: 0 },
            HistoryEvent::ContentBlockStart {
                index: 1,
                content_block: ContentBlock::ToolCall {
                    id: "call_1".to_string(),
                    name: "Bash".to_string(),
                    input: serde_json::Value::Object(serde_json::Map::new()),
                },
            },
            HistoryEvent::ContentBlockDelta {
                index: 1,
                delta: HistoryContentDelta::InputJsonDelta {
                    partial_json: "{\"command\":".to_string(),
                },
            },
            HistoryEvent::ContentBlockDelta {
                index: 1,
                delta: HistoryContentDelta::InputJsonDelta {
                    partial_json: "\"ls\"}".to_string(),
                },
            },
            HistoryEvent::ContentBlockStop { index: 1 },
            HistoryEvent::MessageDelta {
                delta: HistoryMessageDelta {
                    stop_reason: Some("tool_use".to_string()),
                    stop_details: None,
                },
                usage: Some(Usage {
                    billable_tokens: TokenUsage {
                        input: 60,
                        output: 20,
                        cache_read: 40,
                        reasoning_output: 30,
                        ..Default::default()
                    },
                    context_tokens: Some(150),
                    provider_reported_total_tokens: Some(150),
                    provider_metadata: serde_json::json!({"upstreamUsageState":"complete","input_tokens":60,"output_tokens":50,"cache_creation_input_tokens":0,"cache_read_input_tokens":40,"cache_creation":{"ephemeral_1h_input_tokens":0}}),
                    ..Default::default()
                }),
            },
            HistoryEvent::MessageStop,
        ]
    );
}

#[tokio::test]
async fn native_reasoning_received_at_completion_keeps_its_replay_position() {
    use futures::StreamExt;
    let native =
        serde_json::json!({"type":"reasoning","id":"rs","encrypted_content":"opaque","summary":[]});
    let call =
        serde_json::json!({"type":"function_call","call_id":"call","name":"Bash","arguments":"{}"});
    let events = decode_stream(&[
        created_frame(),
        serde_json::json!({"type":"response.reasoning_summary_text.delta","output_index":0,"delta":"thinking"}),
        serde_json::json!({"type":"response.output_item.added","output_index":1,"item":call}),
        serde_json::json!({"type":"response.function_call_arguments.delta","output_index":1,"delta":"{}"}),
        serde_json::json!({"type":"response.output_item.done","output_index":1,"item":call}),
        serde_json::json!({"type":"response.completed","response":{"id":"resp_1","status":"completed","output":[native.clone(),call]}}),
    ]);
    let response = llm_runtime::stream_accumulator::accumulate_stream_salvaging(
        futures::stream::iter(events.into_iter().map(Ok)).boxed(),
    )
    .await
    .unwrap();
    let mut request = HistoryFixture::new("gpt-5");
    request.messages.push(Message {
        role: "assistant".into(),
        content: response.content,
    });
    let encoded = codec().encode_request(&request).unwrap();
    assert_eq!(encoded.body_json["input"][0], native);
    assert_eq!(encoded.body_json["input"][1]["type"], "function_call");
}
