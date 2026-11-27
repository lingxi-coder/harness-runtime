use super::*;

/// Fix 1 end-to-end: a fake transport returns a 400 PTL envelope with counts;
/// the resulting `LlmError::ContextOverflow` carries the parsed `token_gap`.
///
/// This pins the adapter→codec→LlmError path: the adapter decodes the 400
/// PTL response through the AnthropicMessagesCodec and surfaces the variant
/// with the correct non-zero gap, so the turn-loop's `token_gap` binding is
/// non-zero instead of the old `0` sentinel.
#[tokio::test]
async fn ptl_response_surfaces_context_overflow_with_gap() {
    // The 400 PTL envelope that Anthropic returns.
    let ptl_body = serde_json::json!({
        "type": "error",
        "error": {
            "type": "invalid_request_error",
            "message": "prompt is too long: 210000 tokens > 200000 maximum"
        }
    });
    let transport = FakeTransport::always(ProviderResponse::json(400, ptl_body));
    let adapter = make_adapter(transport);

    let result = adapter
        .messages_create(crate::MessagesCreateRequest::new(
            "claude-sonnet-4-20250514",
            None,
            None,
            Vec::new(),
            Vec::new(),
        ))
        .await;

    match classified_error(&result) {
        Some(LlmError::ContextOverflow { token_gap }) => {
            assert_eq!(
                *token_gap, 10_000,
                "token_gap must be 210000 - 200000 = 10000; got {token_gap}"
            );
        }
        other => panic!("expected ContextOverflow {{ token_gap: 10000 }}, got {other:?}"),
    }
}

#[tokio::test]
async fn repeated_identical_non_stream_overflow_stops_after_one_adjustment() {
    let message = "This endpoint's maximum context length is 200000 tokens. However, you requested about 212000 tokens (150000 of text input, 30000 of tool input, 32000 in the output). Please reduce the length of either one.";
    let overflow = serde_json::json!({
        "type": "error",
        "error": {"type": "invalid_request_error", "message": message}
    });
    let transport = FakeTransport::sequence(vec![
        FakeResponse::Ok(ProviderResponse::json(400, overflow.clone())),
        FakeResponse::Ok(ProviderResponse::json(400, overflow)),
        FakeResponse::Ok(ProviderResponse::json(200, ok_response_json())),
    ]);
    let adapter = make_adapter(transport.clone())
        .with_thinking(crate::model::thinking::ThinkingConfig::Disabled);

    let mut request = crate::MessagesCreateRequest::new(
        "claude-sonnet-4-20250514",
        None,
        None,
        Vec::new(),
        Vec::new(),
    );
    request.opts.max_output_tokens = Some(32_000);
    let result = adapter.messages_create(request).await;

    assert!(matches!(
        classified_error(&result),
        Some(LlmError::InvalidRequest { .. })
    ));
    assert_eq!(transport.seen_count(), 2);
}

#[tokio::test]
async fn native_prefix_lock_partial_repair_uses_source_marker_and_retries_nonstream() {
    let model = "claude-sonnet-4-20250514";
    let original = assistant_with_thinking();
    let source_id = match &original {
        lingxi_core::types::ConversationMessage::Assistant { id, .. } => *id,
        _ => unreachable!(),
    };
    let transport = FakeTransport::sequence(vec![
        FakeResponse::Ok(prefix_lock_error_response()),
        FakeResponse::Ok(ProviderResponse::json(200, ok_response_json())),
    ]);
    let adapter = make_adapter(transport.clone());

    let response = adapter
        .messages_create(crate::MessagesCreateRequest::new(
            model,
            None,
            None,
            vec![original],
            Vec::new(),
        ))
        .await;

    assert!(
        response.is_ok(),
        "prefix retry should complete: {response:?}"
    );
    assert_eq!(transport.seen_count(), 2);
    let first = transport.seen_body(0);
    let second = transport.seen_body(1);
    assert_eq!(
        first.string_at("/messages/0/content/0/type").as_deref(),
        Some("thinking")
    );
    assert_eq!(
        second.string_at("/messages/0/content/0/text").as_deref(),
        Some("hello")
    );
    assert_eq!(
        adapter.thinking_stripped_messages().get(&source_id),
        Some(&0),
        "persist the earliest source block range, without changing transcript content"
    );
}

#[tokio::test]
async fn native_prefix_lock_partial_repair_uses_the_same_chain_on_stream_connect() {
    let model = "claude-sonnet-4-20250514";
    let transport = FakeStreamTransport::sequence(vec![
        FakeStreamResp::Status {
            status: 400,
            headers: BTreeMap::from([(
                "anthropic-thinking-prefix-mismatch".into(),
                "block=messages.0.content.0;kind=signature".into(),
            )]),
            body_json: Some(prefix_lock_error_body()),
        },
        FakeStreamResp::Status {
            status: 200,
            headers: BTreeMap::new(),
            body_json: None,
        },
    ]);
    let adapter = make_adapter(transport.clone());

    let stream = adapter
        .stream(
            model,
            None,
            None,
            vec![assistant_with_thinking()],
            Vec::new(),
            None,
            None,
        )
        .await;

    assert!(
        stream.is_ok(),
        "stream retry should open: {:?}",
        stream.as_ref().err()
    );
    assert_eq!(transport.stream_call_count(), 2);
    assert_eq!(
        transport
            .seen_body(0)
            .string_at("/messages/0/content/0/type")
            .as_deref(),
        Some("thinking")
    );
    assert_eq!(
        transport
            .seen_body(1)
            .string_at("/messages/0/content/0/text")
            .as_deref(),
        Some("hello")
    );
}

#[tokio::test]
async fn thinking_recovery_uses_captured_request_scope_after_stream_handoff() {
    use crate::thinking_scope::{scope_thinking_recovery, ThinkingRecoveryScope};
    let adapter = make_adapter(FakeTransport::always(ProviderResponse::json(
        200,
        ok_response_json(),
    )));
    let old = assistant_with_thinking();
    let owner = ThinkingRecoveryScope::default();
    let other = ThinkingRecoveryScope::default();
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let captured = recorded.clone();
    owner.set_recorder(Arc::new(move |messages| {
        let captured = captured.clone();
        Box::pin(async move {
            captured.lock().unwrap().push(messages);
        })
    }));
    let mut req = scope_thinking_recovery(owner.clone(), async {
        adapter
            .build_request(
                "claude-sonnet-4-20250514",
                None,
                None,
                vec![old.clone()],
                vec![],
                true,
                None,
                false,
                PromptCacheQuerySource::Unspecified,
            )
            .unwrap()
    })
    .await;
    // Lazy streaming retries may be polled on another task, under another
    // context, or after the caller's assembly scope has already ended.
    scope_thinking_recovery(other.clone(), async {
        assert!(adapter.handle_thinking_signature_strip(&mut req).await);
        assert!(adapter.thinking_stripped_messages().is_empty());
    })
    .await;
    assert_eq!(owner.messages().get(&old.id()), Some(&0));
    assert_eq!(recorded.lock().unwrap()[0].get(&old.id()), Some(&0));
    assert!(other.messages().is_empty());
    assert!(adapter.thinking_stripped_messages().is_empty());
    assert!(serde_json::to_value(&req)
        .unwrap()
        .get("thinking_recovery_scope")
        .is_none());
}

#[tokio::test]
async fn side_query_thinking_recovery_forks_parent_ranges_without_mutating_them() {
    use crate::thinking_scope::{scope_thinking_recovery, ThinkingRecoveryScope};
    let adapter = make_adapter(FakeTransport::sequence(vec![
        FakeResponse::Ok(ProviderResponse::json(
            400,
            serde_json::json!({
                "type":"error", "error":{"type":"invalid_request_error",
                "message":"Invalid signature in thinking block"}
            }),
        )),
        FakeResponse::Ok(ProviderResponse::json(200, ok_response_json())),
    ]));
    let parent = ThinkingRecoveryScope::default();
    let old = assistant_with_thinking();
    scope_thinking_recovery(parent.clone(), async {
        adapter
            .messages_create_side_query(
                "claude-sonnet-4-20250514",
                None,
                None,
                vec![old.clone()],
                vec![],
                None,
                None,
                vec![],
                None,
                Some("summary"),
            )
            .await
            .unwrap();
        assert!(adapter.thinking_stripped_messages().is_empty());
    })
    .await;
    assert!(parent.messages().is_empty());
    assert!(adapter.thinking_stripped_messages().is_empty());
}

#[tokio::test]
async fn thinking_signature_recovery_preserves_fresh_identical_thinking() {
    let transport = FakeTransport::sequence(vec![
        FakeResponse::Ok(ProviderResponse::json(
            400,
            serde_json::json!({
                "type":"error", "error":{"type":"invalid_request_error","message":"Invalid signature in thinking block"}
            }),
        )),
        FakeResponse::Ok(ProviderResponse::json(200, ok_response_json())),
    ]);
    let adapter = make_adapter(transport);
    let old = assistant_with_thinking();
    adapter
        .messages_create(crate::MessagesCreateRequest::new(
            "claude-sonnet-4-20250514",
            None,
            None,
            vec![old.clone()],
            vec![],
        ))
        .await
        .unwrap();
    let fresh = assistant_with_thinking();
    let request = adapter
        .build_request(
            "claude-sonnet-4-20250514",
            None,
            None,
            vec![
                old,
                lingxi_core::types::ConversationMessage::user(
                    lingxi_core::types::MessageId::new(),
                    "continue".into(),
                ),
                fresh.clone(),
            ],
            vec![],
            false,
            None,
            false,
            PromptCacheQuerySource::Unspecified,
        )
        .unwrap();
    let assistants: Vec<_> = request
        .input
        .messages
        .iter()
        .filter(|message| message.role == lingxi_llm_client::protocol::MessageRole::Assistant)
        .collect();
    assert_eq!(assistants.len(), 2);
    assert!(!assistants[0].content.iter().any(|block| matches!(
        block,
        lingxi_llm_client::protocol::ContentBlock::Thinking { .. }
    )));
    assert!(assistants[1].content.iter().any(|block| matches!(
        block,
        lingxi_llm_client::protocol::ContentBlock::Thinking { .. }
    )));
    assert!(!adapter
        .thinking_stripped_messages()
        .contains_key(&fresh.id()));
    assert!(serde_json::to_value(&request)
        .unwrap()
        .get("thinking_source_message_ids")
        .is_none());
}

#[tokio::test]
async fn thinking_signature_400_strips_and_retries_on_anthropic() {
    let err = serde_json::json!({
        "type": "error",
        "error": {
            "type": "invalid_request_error",
            "message": "Invalid signature in thinking block"
        }
    });
    let transport = FakeTransport::sequence(vec![
        FakeResponse::Ok(ProviderResponse::json(400, err)),
        FakeResponse::Ok(ProviderResponse::json(200, ok_response_json())),
    ]);
    let adapter = make_adapter(transport.clone());
    let result = adapter
        .messages_create(crate::MessagesCreateRequest::new(
            "claude-sonnet-4-20250514",
            None,
            None,
            vec![assistant_with_thinking()],
            Vec::new(),
        ))
        .await;
    assert!(
        result.is_ok(),
        "Anthropic thinking-signature 400 must retry: {result:?}"
    );
    assert_eq!(transport.seen_count(), 2);
    assert!(adapter.thinking_signature_stripped());
}

#[tokio::test]
async fn thinking_signature_400_strips_and_retries_on_openai_chat() {
    let err = serde_json::json!({
        "error": {
            "message": "Invalid signature in thinking block",
            "type": "invalid_request_error"
        }
    });
    let transport = FakeTransport::sequence(vec![
        FakeResponse::Ok(ProviderResponse::json(400, err)),
        FakeResponse::Ok(ProviderResponse::json(
            200,
            serde_json::json!({
                "id": "chatcmpl_test",
                "model": "gpt-4o",
                "choices": [{
                    "message": {"role": "assistant", "content": "hello"},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7}
            }),
        )),
    ]);
    let adapter = make_adapter_for_protocol_with_transport(
        ProtocolFamily::OpenAiChat,
        ProviderId::OpenAI,
        "https://api.openai.com",
        "openai",
        "gpt-4o",
        transport.clone(),
    );
    let result = adapter
        .messages_create(crate::MessagesCreateRequest::new(
            "gpt-4o",
            Some("openai"),
            None,
            vec![assistant_with_thinking()],
            Vec::new(),
        ))
        .await;
    assert!(
        result.is_ok(),
        "OpenAI-compat thinking models must heal thinking-signature 400: {result:?}"
    );
    assert_eq!(transport.seen_count(), 2);
    assert!(adapter.thinking_signature_stripped());
}

#[tokio::test]
async fn latched_thinking_strip_lets_gemini_encode_after_anthropic_thinking() {
    let transport = FakeTransport::always(ProviderResponse::json(
        200,
        serde_json::json!({
            "responseId": "resp_test",
            "modelVersion": "gemini-2.5-flash",
            "candidates": [{
                "content": {"role": "model", "parts": [{"text": "hello"}]},
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 5,
                "candidatesTokenCount": 2,
                "totalTokenCount": 7
            }
        }),
    ));
    let adapter = make_adapter_for_protocol_with_transport(
        ProtocolFamily::GeminiGenerateContent,
        ProviderId::Gemini,
        "https://generativelanguage.googleapis.com/v1beta",
        "gemini",
        "gemini-2.5-flash",
        transport.clone(),
    );
    adapter.set_thinking_signature_stripped(true);
    let result = adapter
        .messages_create(crate::MessagesCreateRequest::new(
            "gemini-2.5-flash",
            Some("gemini"),
            None,
            vec![assistant_with_thinking()],
            Vec::new(),
        ))
        .await;
    assert!(
        result.is_ok(),
        "latched strip must drop thinking before Gemini encode: {result:?}"
    );
    assert_eq!(transport.seen_count(), 1);
}

#[tokio::test]
async fn latched_thinking_strip_preserves_deepseek_reasoning_round_trip() {
    let transport = FakeTransport::always(ProviderResponse::json(
        200,
        serde_json::json!({
            "id": "chatcmpl_test",
            "model": "deepseek-reasoner",
            "choices": [{
                "message": {"role": "assistant", "content": "ok"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7}
        }),
    ));
    let adapter = make_adapter_for_protocol_with_transport(
        ProtocolFamily::OpenAiChat,
        ProviderId::OpenAICompatible {
            name: "deepseek".into(),
        },
        "https://api.deepseek.com",
        "deepseek",
        "deepseek-reasoner",
        transport.clone(),
    );
    adapter.set_thinking_signature_stripped(true);
    let result = adapter
        .messages_create(crate::MessagesCreateRequest::new(
            "deepseek-reasoner",
            Some("deepseek"),
            None,
            vec![assistant_with_thinking()],
            Vec::new(),
        ))
        .await;
    assert!(
        result.is_ok(),
        "DeepSeek later turn must still encode: {result:?}"
    );
    let body = request_body_value(&transport.seen.lock().unwrap()[0].body_json);
    let messages = body["messages"].as_array().expect("messages");
    let has_reasoning = messages.iter().any(|message| {
        message
            .get("reasoning_content")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|text| !text.is_empty())
    });
    assert!(
        has_reasoning,
        "DeepSeek must keep reasoning_content under the latch: {body}"
    );
}
