use super::*;

#[test]
fn bedrock_stream_fixture_uses_eventstream_crc_and_chunk_envelope() {
    let frame = bedrock_event_stream_frame(br#"{"bytes":"e30="}"#);
    let mut splitter = crate::EventStreamSplitter::new();
    let messages = splitter.feed(&frame).expect("valid AWS EventStream frame");
    splitter.finish().expect("complete AWS EventStream frame");
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].header(":message-type"), Some("event"));
    assert_eq!(messages[0].header(":event-type"), Some("chunk"));
    assert_eq!(messages[0].payload, br#"{"bytes":"e30="}"#);
}

#[tokio::test]
async fn native_stream_hq_unknown_400_uses_the_same_provenance_guards() {
    use futures::StreamExt;

    let _cache_env_lock = CACHE_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _thinking_env_lock = THINKING_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _restore = CacheEnvRestore::clear(&[
        "ANTHROPIC_BEDROCK_BASE_URL",
        "CLAUDE_CODE_SIMULATE_PROXY_USAGE",
        "CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS",
        "CLAUDE_CODE_EXTRA_BODY",
    ]);
    let model = "claude-opus-4-7";
    let transport = FakeStreamTransport::sequence(vec![
        FakeStreamResp::Status {
            status: 400,
            headers: BTreeMap::new(),
            body_json: Some(serde_json::json!({
                "type": "error",
                "error": {
                    "type": "invalid_request_error",
                    "message": "request field is not recognized"
                }
            })),
        },
        FakeStreamResp::Frames {
            status: 200,
            headers: BTreeMap::new(),
            frames: successful_bedrock_anthropic_frames(model),
        },
    ]);
    let service = make_adapter_for_protocol_with_transport(
        ProtocolFamily::BedrockClaude,
        ProviderId::BedrockClaude,
        "https://bedrock.example.test",
        "bedrock",
        model,
        transport.clone(),
    );
    let scope = crate::thinking_scope::ThinkingRecoveryScope::default();
    let mut request = LlmRequest::new(model).with_user_text("Hi");
    request.profile = Some("bedrock".into());
    request.execution.thinking_recovery_scope = Some(scope.clone());
    request.input.thinking = Some(lingxi_llm_client::protocol::ThinkingConfig {
        mode: Some(lingxi_llm_client::protocol::ThinkingMode::Adaptive),
        ..Default::default()
    });

    let mut stream = service
        .stream_request(request)
        .await
        .expect("HQ broad QEE recovers the unknown Bedrock refusal");
    assert_eq!(transport.stream_call_count(), 2);
    assert!(request_header_has_beta(
        &transport.seen_headers(0),
        "thinking-binding-controls-2026-08-01"
    ));
    assert!(!request_header_has_beta(
        &transport.seen_headers(1),
        "thinking-binding-controls-2026-08-01"
    ));
    while let Some(event) = stream.next().await {
        event.expect("successful retry stream");
    }
    assert!(bedrock_tbc_state(&scope).beta_rejected);
}

#[tokio::test]
async fn ordinary_stream_context_1m_generic_refusal_retires_tbc_before_context_1m_retry() {
    use futures::StreamExt;

    let _cache_env_lock = CACHE_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _thinking_env_lock = THINKING_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _restore = CacheEnvRestore::clear(&[
        "ANTHROPIC_BASE_URL",
        "_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL",
        "CLAUDE_CODE_EXTRA_BODY",
        "ANTHROPIC_BEDROCK_BASE_URL",
        "CLAUDE_CODE_SIMULATE_PROXY_USAGE",
        "CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS",
    ]);
    let model = "claude-opus-4-7[1m]";
    let transport = FakeStreamTransport::sequence(vec![
        FakeStreamResp::Status {
            status: 400,
            headers: BTreeMap::new(),
            body_json: Some(serde_json::json!({
                "type":"error",
                "error":{"type":"invalid_request_error","message":"invalid beta flag"}
            })),
        },
        FakeStreamResp::Frames {
            status: 200,
            headers: BTreeMap::new(),
            frames: successful_bedrock_anthropic_frames(model),
        },
    ]);
    let service = make_adapter_for_protocol_with_transport(
        ProtocolFamily::BedrockClaude,
        ProviderId::BedrockClaude,
        "https://bedrock.example.test",
        "bedrock",
        model,
        transport.clone(),
    );
    let scope = crate::thinking_scope::ThinkingRecoveryScope::default();
    let model_key = native_foundry_model_key(&model.encode_utf16().collect::<Vec<_>>());
    let mut request = LlmRequest::new(model).with_user_text("Hi");
    request.profile = Some("bedrock".into());
    request.execution.thinking_recovery_scope = Some(scope.clone());
    request.input.thinking = Some(lingxi_llm_client::protocol::ThinkingConfig {
        mode: Some(lingxi_llm_client::protocol::ThinkingMode::Adaptive),
        ..Default::default()
    });

    let mut stream = service
        .stream_request(request)
        .await
        .expect("the TBC error retries the stream without TBC");
    assert_eq!(transport.stream_call_count(), 2);
    let tbc = "thinking-binding-controls-2026-08-01";
    assert!(request_header_has_beta(&transport.seen_headers(0), tbc));
    assert!(request_body_has_beta(
        &transport.seen_body(0),
        crate::model::betas::CONTEXT_1M
    ));
    assert!(!request_header_has_beta(&transport.seen_headers(1), tbc));
    assert!(!request_header_has_beta(
        &transport.seen_headers(1),
        crate::model::betas::CONTEXT_1M
    ));
    assert!(request_body_has_beta(
        &transport.seen_body(1),
        crate::model::betas::CONTEXT_1M
    ));

    let first = stream
        .next()
        .await
        .expect("first event")
        .expect("valid stream event");
    assert!(matches!(first, HistoryEvent::MessageStart { .. }));
    while let Some(event) = stream.next().await {
        event.expect("remaining successful stream events");
    }
    assert!(bedrock_tbc_state(&scope).beta_rejected);
    assert!(!scope.context_1m_refused(&model_key));
}

#[tokio::test]
async fn ordinary_stream_context_1m_retry_latches_at_message_start_and_suppresses_next_call() {
    use futures::StreamExt;

    let _thinking_env_lock = THINKING_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _restore = CacheEnvRestore::clear(&["CLAUDE_CODE_EXTRA_BODY"]);
    let model = "claude-sonnet-4-20250514[1m]";
    let transport = FakeStreamTransport::sequence(vec![
        FakeStreamResp::Status {
            status: 400,
            headers: BTreeMap::new(),
            body_json: Some(serde_json::json!({
                "type":"error",
                "error":{"type":"invalid_request_error","message":"context-1m-2025-08-07 beta is not supported"}
            })),
        },
        FakeStreamResp::Frames {
            status: 200,
            headers: BTreeMap::new(),
            frames: successful_bedrock_anthropic_frames(model),
        },
        FakeStreamResp::Frames {
            status: 200,
            headers: BTreeMap::new(),
            frames: successful_bedrock_anthropic_frames(model),
        },
    ]);
    let service = make_adapter_for_protocol_with_transport(
        ProtocolFamily::BedrockClaude,
        ProviderId::BedrockClaude,
        "https://bedrock.example.test",
        "bedrock",
        model,
        transport.clone(),
    );
    let scope = crate::thinking_scope::ThinkingRecoveryScope::default();
    let model_key = native_foundry_model_key(&model.encode_utf16().collect::<Vec<_>>());
    let mut request = LlmRequest::new(model).with_user_text("Hi");
    request.profile = Some("bedrock".into());
    request.execution.thinking_recovery_scope = Some(scope.clone());

    let mut stream = service
        .stream_request(request)
        .await
        .expect("retry opens stream");
    assert!(
        !scope.context_1m_refused(&model_key),
        "a 2xx stream open without a projected message_start does not commit the latch"
    );
    let first = stream
        .next()
        .await
        .expect("first event")
        .expect("valid event");
    assert!(matches!(first, HistoryEvent::MessageStart { .. }));
    assert!(scope.context_1m_refused(&model_key));
    while let Some(event) = stream.next().await {
        event.expect("remaining successful stream events");
    }

    let mut next_request = LlmRequest::new(model).with_user_text("Continue");
    next_request.profile = Some("bedrock".into());
    next_request.execution.thinking_recovery_scope = Some(scope.clone());
    let mut next_stream = service
        .stream_request(next_request)
        .await
        .expect("next same-session stream");
    while let Some(event) = next_stream.next().await {
        event.expect("next successful stream events");
    }

    assert_eq!(transport.stream_call_count(), 3);
    assert!(request_body_has_beta(
        &transport.seen_body(0),
        crate::model::betas::CONTEXT_1M
    ));
    assert!(!request_body_has_beta(
        &transport.seen_body(1),
        crate::model::betas::CONTEXT_1M
    ));
    assert!(!request_body_has_beta(
        &transport.seen_body(2),
        crate::model::betas::CONTEXT_1M
    ));
}

#[tokio::test]
async fn ordinary_stream_context_1m_retry_without_message_start_does_not_latch() {
    use futures::StreamExt;

    let _thinking_env_lock = THINKING_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _restore = CacheEnvRestore::clear(&["CLAUDE_CODE_EXTRA_BODY"]);
    let model = "claude-sonnet-4-20250514[1m]";
    let transport = FakeStreamTransport::sequence(vec![
        FakeStreamResp::Status {
            status: 400,
            headers: BTreeMap::new(),
            body_json: Some(serde_json::json!({
                "type":"error",
                "error":{"type":"invalid_request_error","message":"context-1m-2025-08-07 beta is not supported"}
            })),
        },
        FakeStreamResp::Frames {
            status: 200,
            headers: BTreeMap::new(),
            frames: Vec::new(),
        },
    ]);
    let service = make_adapter_for_protocol_with_transport(
        ProtocolFamily::BedrockClaude,
        ProviderId::BedrockClaude,
        "https://bedrock.example.test",
        "bedrock",
        model,
        transport.clone(),
    );
    let scope = crate::thinking_scope::ThinkingRecoveryScope::default();
    let key = native_foundry_model_key(&model.encode_utf16().collect::<Vec<_>>());
    let mut request = LlmRequest::new(model).with_user_text("Hi");
    request.profile = Some("bedrock".into());
    request.execution.thinking_recovery_scope = Some(scope.clone());

    let mut stream = service
        .stream_request(request)
        .await
        .expect("retry opens stream");
    while let Some(event) = stream.next().await {
        let _ = event;
    }

    assert_eq!(transport.stream_call_count(), 2);
    assert!(!scope.context_1m_refused(&key));
}

#[tokio::test]
async fn stream_dispatch_admission_guards_the_actual_sdk_send_boundary() {
    let transport = FakeStreamTransport::sequence(vec![FakeStreamResp::Status {
        status: 200,
        headers: BTreeMap::new(),
        body_json: None,
    }]);
    let service = make_adapter_with_retries(transport.clone(), Some(0));
    let capture = crate::prompt_cache::RequestCapture::default();
    let rejected_observations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let rejected = rejected_observations.clone();
    let result = capture
        .scope(service.stream_with_system_prompt(
            "claude-sonnet-4-20250514",
            None,
            None,
            Vec::new(),
            Vec::new(),
            None,
            None,
            None,
            false,
            Some("main"),
            Some(crate::RequestDispatchAdmission::new(|| false)),
            Vec::new(),
            Some(crate::RequestDispatchObserver::new(move || {
                rejected.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            })),
            None,
            std::collections::HashSet::new(),
            std::collections::HashMap::new(),
            None,
            std::collections::HashSet::new(),
        ))
        .await;

    assert!(matches!(
        result,
        Err(LlmError::RequestDispatchRejected {
            prior_dispatch: false
        })
    ));
    assert_eq!(transport.stream_call_count(), 0);
    assert_eq!(
        rejected_observations.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    assert!(
        capture.take().is_none(),
        "rejected stream is not a dispatch snapshot"
    );

    let accepted = FakeStreamTransport::sequence(vec![FakeStreamResp::Status {
        status: 200,
        headers: BTreeMap::new(),
        body_json: None,
    }]);
    let service = make_adapter_with_retries(accepted.clone(), Some(0));
    let accepted_observations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = accepted_observations.clone();
    let _stream = service
        .stream_with_system_prompt(
            "claude-sonnet-4-20250514",
            None,
            None,
            Vec::new(),
            Vec::new(),
            None,
            None,
            None,
            false,
            Some("main"),
            Some(crate::RequestDispatchAdmission::new(|| true)),
            Vec::new(),
            Some(crate::RequestDispatchObserver::new(move || {
                observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            })),
            None,
            std::collections::HashSet::new(),
            std::collections::HashMap::new(),
            None,
            std::collections::HashSet::new(),
        )
        .await
        .expect("positive control reaches transport");
    assert_eq!(accepted.stream_call_count(), 1);
    assert_eq!(
        accepted_observations.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
}

#[tokio::test(start_paused = true)]
async fn stream_retry_rejection_reports_prior_dispatch_without_replay() {
    let transport = FakeStreamTransport::sequence(vec![
        FakeStreamResp::Err(LlmError::Transport {
            message: "first admitted connect failed".into(),
        }),
        FakeStreamResp::Status {
            status: 200,
            headers: BTreeMap::new(),
            body_json: None,
        },
    ]);
    let service = make_adapter_with_retries(transport.clone(), Some(1));
    let admission_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let calls = admission_calls.clone();
    let dispatch_observations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = dispatch_observations.clone();

    let result = service
        .stream_with_system_prompt(
            "claude-sonnet-4-20250514",
            None,
            None,
            vec![lingxi_core::types::ConversationMessage::user(
                lingxi_core::types::MessageId::new(),
                "hello".into(),
            )],
            Vec::new(),
            None,
            None,
            None,
            false,
            Some("main"),
            Some(crate::RequestDispatchAdmission::new(move || {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0
            })),
            Vec::new(),
            Some(crate::RequestDispatchObserver::new(move || {
                observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            })),
            None,
            std::collections::HashSet::new(),
            std::collections::HashMap::new(),
            None,
            std::collections::HashSet::new(),
        )
        .await;

    assert!(matches!(
        result,
        Err(LlmError::RequestDispatchRejected {
            prior_dispatch: true
        })
    ));
    assert_eq!(admission_calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(
        transport.stream_call_count(),
        1,
        "the stale retry must be rejected before a second transport send"
    );
    assert_eq!(
        dispatch_observations.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the observer reports one admitted physical dispatch across retries"
    );
}

/// 2.1.245/246 first-byte parity: a request that never receives response
/// headers retries once, then terminates with the exact user-facing copy.
#[tokio::test(start_paused = true)]
async fn streaming_first_byte_timeout_retries_once_then_stops() {
    let transport = HangingOpenTransport::new();
    let adapter = make_adapter(Arc::clone(&transport) as Arc<dyn Transport>)
        .with_stream_first_byte_timeout_override(std::time::Duration::from_millis(50));

    let error = match adapter
        .stream(
            "claude-sonnet-4-20250514",
            None,
            None,
            Vec::new(),
            Vec::new(),
            None,
            None,
        )
        .await
    {
        Ok(_) => panic!("a permanently silent connect phase must fail"),
        Err(error) => error,
    };

    assert!(
        crate::model::stream_watchdog::is_stream_no_response(&error),
        "expected StreamNoResponse marker, got {error:?}"
    );
    assert_eq!(
        crate::error::error_display_text(&error),
        "No response from API"
    );
    assert_eq!(transport.call_count(), 2, "must retry exactly once");
}

/// 3c-T1 pin: a connect-phase 429 with `retry-after: 7` on the streaming
/// path must drive a 7 s `RetryAfter` delay (not the 1 s fallback).
///
/// We use `tokio::time::pause()` so the test completes instantly; the
/// `drive_stream` loop sleeps via `tokio::time::sleep` which respects the
/// paused clock.  After the first 429 the test advances time past 7 s and
/// the second (200) attempt is served, confirming the delay was honoured.
#[tokio::test(start_paused = true)]
async fn streaming_429_with_retry_after_header_drives_7s_not_1s() {
    // Attempt 1: 429 with retry-after: 7.
    let mut headers_429 = BTreeMap::new();
    headers_429.insert("retry-after".to_string(), "7".to_string());

    // Attempt 2: 200 with an empty body (the codec will produce
    // StreamInterrupted on an empty frame-stream, but that is terminal and
    // proves two calls were made — what we care about).
    let stream_transport = FakeStreamTransport::sequence(vec![
        FakeStreamResp::Status {
            status: 429,
            headers: headers_429,
            body_json: None,
        },
        FakeStreamResp::Status {
            status: 200,
            headers: BTreeMap::new(),
            body_json: None,
        },
    ]);

    let adapter = make_adapter(Arc::clone(&stream_transport) as Arc<dyn Transport>);

    // Record the instant before calling drive_stream.
    let before = tokio::time::Instant::now();

    // drive_stream is private; call it through the StreamingApiClient trait.
    // The result will be an error (empty frame-stream on attempt 2) or Ok
    // depending on the codec — we only care that two open_stream calls were
    // made and that the elapsed time is ≥ 7 s (the retry-after delay).
    let _result = adapter
        .stream(
            "claude-sonnet-4-20250514",
            None,
            None,
            Vec::new(),
            Vec::new(),
            None,
            None,
        )
        .await;

    let elapsed = before.elapsed();
    // The sleep was for exactly 7 s (retry-after value).  With time paused
    // the sleep advances the mock clock, so elapsed reports ≥ 7 s.
    assert!(
        elapsed >= std::time::Duration::from_secs(7),
        "retry-after:7 must drive a ≥7 s delay; elapsed={elapsed:?}"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(8),
        "delay must be close to 7 s (not jittered / not 1 s fallback); elapsed={elapsed:?}"
    );
    // Two open_stream calls: 429 then 200.
    assert_eq!(
        stream_transport.stream_call_count(),
        2,
        "must retry exactly once (429 → 200)"
    );
}

#[tokio::test]
async fn streaming_openrouter_free_429_surfaces_provider_detail_without_retrying() {
    let model = "z-ai/glm-5.2:free";
    let alias = "free-router-model";
    let stream_transport = FakeStreamTransport::sequence(vec![FakeStreamResp::Status {
        status: 429,
        headers: BTreeMap::new(),
        body_json: Some(serde_json::json!({
            "error": {
                "code": "rate_limit_exceeded",
                "message": "Free model daily request limit exceeded"
            }
        })),
    }]);
    let mut adapter = make_adapter_for_protocol_with_transport(
        ProtocolFamily::OpenAiChat,
        ProviderId::OpenAICompatible {
            name: "openrouter".to_string(),
        },
        "https://openrouter.ai/api/v1",
        "openrouter",
        model,
        Arc::clone(&stream_transport) as Arc<dyn Transport>,
    );
    let mut config = adapter.client.configuration();
    config.providers[0].models[0].aliases.push(alias.to_owned());
    adapter.client = Arc::new(adapter.client.reconfigured(config).unwrap());

    let result = adapter
        .stream_with_attempt_opts(
            alias,
            Some("openrouter"),
            None,
            Vec::new(),
            Vec::new(),
            None,
            None,
            Some(32_000),
            None,
            None,
        )
        .await;

    assert!(
        matches!(
            classified_error(&result),
            Some(LlmError::RateLimited { .. })
        ),
        "OpenRouter's 429 must keep the typed rate-limit error"
    );
    assert_eq!(
        stream_transport.stream_call_count(),
        1,
        "a free-tier 429 must still fail fast"
    );
    assert_eq!(
        adapter.last_rate_limit_error_message().as_deref(),
        Some(
            "OpenRouter free-model rate limit reached: Free model daily request limit exceeded. Try another free model or retry later."
        ),
        "the terminal surface must retain OpenRouter's useful error detail"
    );
}

#[tokio::test]
async fn streaming_openrouter_overflow_retries_with_server_derived_output_limit() {
    use crate::model::model_limits::{register, ModelLimits};

    let model = "z-ai/glm-5.2:free-stream-test";
    register(
        model,
        ModelLimits {
            context_window: 256_000,
            max_output_tokens: 230_400,
        },
    );
    let overflow = serde_json::json!({
        "error": {
            "message": "This endpoint's maximum context length is 256000 tokens. However, you requested about 272000 tokens (200000 of text input, 40000 of tool input, 32000 in the output). Please reduce the length of either one.",
            "code": 400,
            "metadata": {"provider_name": null}
        }
    });
    let stream_transport = FakeStreamTransport::sequence(vec![
        FakeStreamResp::Status {
            status: 400,
            headers: BTreeMap::new(),
            body_json: Some(overflow),
        },
        FakeStreamResp::Status {
            status: 200,
            headers: BTreeMap::new(),
            body_json: None,
        },
    ]);
    let adapter = make_adapter_for_protocol_with_transport(
        ProtocolFamily::OpenAiChat,
        ProviderId::OpenAICompatible {
            name: "openrouter".to_string(),
        },
        "https://openrouter.ai/api/v1",
        "openrouter",
        model,
        Arc::clone(&stream_transport) as Arc<dyn Transport>,
    );

    let result = adapter
        .stream_with_attempt_opts(
            model,
            Some("openrouter"),
            None,
            Vec::new(),
            Vec::new(),
            None,
            None,
            Some(32_000),
            None,
            None,
        )
        .await;

    assert!(result.is_ok(), "second stream connection should open");
    assert_eq!(stream_transport.stream_call_count(), 2);
    assert_eq!(stream_transport.request_max_tokens(0), Some(32_000));
    assert_eq!(stream_transport.request_max_tokens(1), Some(15_000));
}

#[tokio::test]
async fn repeated_identical_stream_overflow_stops_after_one_adjustment() {
    use crate::model::model_limits::{register, ModelLimits};

    let model = "z-ai/glm-5.2:free-repeated-stream-test";
    register(
        model,
        ModelLimits {
            context_window: 256_000,
            max_output_tokens: 230_400,
        },
    );
    let message = "This endpoint's maximum context length is 256000 tokens. However, you requested about 272000 tokens (200000 of text input, 40000 of tool input, 32000 in the output). Please reduce the length of either one.";
    let overflow = serde_json::json!({
        "error": {"message": message, "code": 400, "metadata": {"provider_name": null}}
    });
    let stream_transport = FakeStreamTransport::sequence(vec![
        FakeStreamResp::Status {
            status: 400,
            headers: BTreeMap::new(),
            body_json: Some(overflow.clone()),
        },
        FakeStreamResp::Status {
            status: 400,
            headers: BTreeMap::new(),
            body_json: Some(overflow),
        },
        FakeStreamResp::Status {
            status: 200,
            headers: BTreeMap::new(),
            body_json: None,
        },
    ]);
    let adapter = make_adapter_for_protocol_with_transport(
        ProtocolFamily::OpenAiChat,
        ProviderId::OpenAICompatible {
            name: "openrouter".to_string(),
        },
        "https://openrouter.ai/api/v1",
        "openrouter",
        model,
        Arc::clone(&stream_transport) as Arc<dyn Transport>,
    );

    let result = adapter
        .stream_with_attempt_opts(
            model,
            Some("openrouter"),
            None,
            Vec::new(),
            Vec::new(),
            None,
            None,
            Some(32_000),
            None,
            None,
        )
        .await;

    assert!(matches!(
        classified_error(&result),
        Some(LlmError::InvalidRequest { .. })
    ));
    assert_eq!(stream_transport.stream_call_count(), 2);
}

/// The watchdog is ON by default and aborts a stream that produces no
/// event within the idle timeout, surfacing a detectable idle-timeout
/// error. `start_paused` auto-advances the mock clock to the deadline.
#[tokio::test(start_paused = true)]
async fn streaming_idle_watchdog_aborts_hung_stream() {
    use futures::StreamExt;
    let (body_dropped, observed) = tokio::sync::oneshot::channel();
    struct PendingBody(Option<tokio::sync::oneshot::Sender<()>>);
    impl futures::Stream for PendingBody {
        type Item = Result<bytes::Bytes, lingxi_llm_client::protocol::LlmError>;
        fn poll_next(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Self::Item>> {
            std::task::Poll::Pending
        }
    }
    impl Drop for PendingBody {
        fn drop(&mut self) {
            let _ = self.0.take().unwrap().send(());
        }
    }
    struct PendingTransport(Mutex<Option<tokio::sync::oneshot::Sender<()>>>);
    #[async_trait::async_trait]
    impl Transport for PendingTransport {
        async fn send(
            &self,
            _: lingxi_llm_client::HttpRequest,
        ) -> Result<lingxi_llm_client::StreamResponse, lingxi_llm_client::protocol::LlmError>
        {
            Ok(lingxi_llm_client::StreamResponse {
                status: 200,
                headers: Vec::new(),
                body: Box::pin(PendingBody(self.0.lock().unwrap().take())),
            })
        }
    }
    let adapter = make_adapter(Arc::new(PendingTransport(Mutex::new(Some(body_dropped)))))
        .with_stream_idle_timeout_override(Some(std::time::Duration::from_millis(50)));
    let mut stream = adapter
        .stream(
            "claude-sonnet-4-20250514",
            None,
            None,
            Vec::new(),
            Vec::new(),
            None,
            None,
        )
        .await
        .expect("connect-phase 200 opens the stream");
    let first = stream
        .next()
        .await
        .expect("the watchdog yields an error item");
    let err = first.expect_err("a hung stream must abort with an idle-timeout error");
    assert!(
        crate::model::stream_watchdog::is_stream_idle_timeout(&err),
        "expected a watchdog idle-timeout abort, got {err:?}"
    );
    let mut observed = observed;
    assert_eq!(
        observed.try_recv(),
        Ok(()),
        "timeout must release the live body before yielding"
    );
    assert!(stream.next().await.is_none());
}

/// Stream telemetry twin — succeed path:
/// a clean stream (message_stop at the end) must emit:
/// 1. `tengu_api_request_started` (stream=true)
/// 2. `tengu_api_request_succeeded` (from the unfold's terminal-event arm)
#[tokio::test]
async fn stream_emit_succeeded_fires_on_message_stop() {
    use ::telemetry::AnalyticsValue;
    use futures::StreamExt as _;

    let transport = ScriptedStreamTransport::anthropic_success();
    let (adapter, sink) = make_stream_adapter_with_bus(transport).await;

    let mut stream = adapter
        .stream(
            "claude-sonnet-4-20250514",
            None,
            None,
            Vec::new(),
            Vec::new(),
            None,
            None,
        )
        .await
        .expect("stream open ok");

    // Drain all events.
    while stream.next().await.is_some() {}

    let events = sink.events().await;
    let names: Vec<&str> = events.iter().map(|e| e.name.as_str()).collect();

    assert!(
        names.contains(&"tengu_api_request_started"),
        "started must fire; got {names:?}"
    );
    assert!(
        names.contains(&"tengu_api_request_succeeded"),
        "succeeded must fire on message_stop; got {names:?}"
    );

    // Must NOT fire multiple times.
    let succeeded_count = names
        .iter()
        .filter(|&&n| n == "tengu_api_request_succeeded")
        .count();
    assert_eq!(
        succeeded_count, 1,
        "succeeded must fire exactly once; got {succeeded_count}"
    );

    // Verify stream=true on the started event.
    let started = events
        .iter()
        .find(|e| e.name == "tengu_api_request_started")
        .unwrap();
    assert!(
        matches!(&started.metadata["stream"], AnalyticsValue::Bool(true)),
        "stream must be true on the started event"
    );
}

/// Stream telemetry twin — fail path:
/// a stream that produces a decode error must emit `tengu_api_request_failed`
/// and NOT emit `tengu_api_request_succeeded`.
#[tokio::test]
async fn stream_emit_failed_fires_on_decode_error() {
    use futures::StreamExt as _;

    let transport = ScriptedStreamTransport::malformed_frame();
    let (adapter, sink) = make_stream_adapter_with_bus(transport).await;

    let stream_result = adapter
        .stream(
            "claude-sonnet-4-20250514",
            None,
            None,
            Vec::new(),
            Vec::new(),
            None,
            None,
        )
        .await;

    // May error at open or during drain.
    if let Ok(mut stream) = stream_result {
        while let Some(item) = stream.next().await {
            // consume until error or end
            let _ = item;
        }
    }

    let events = sink.events().await;
    let names: Vec<&str> = events.iter().map(|e| e.name.as_str()).collect();

    // failed must fire (decode error or stream-interrupted)
    // succeeded must NOT fire
    assert!(
        names.contains(&"tengu_api_request_failed"),
        "failed must fire on decode error; got {names:?}"
    );
    assert!(
        !names.contains(&"tengu_api_request_succeeded"),
        "succeeded must NOT fire on error; got {names:?}"
    );
}
