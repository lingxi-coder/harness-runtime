use super::*;

#[test]
fn native_custom_system_prompt_matches_hwe_utf16_line_splitting() {
    use lingxi_llm_client::providers::anthropic::system_prompt::{
        PromptText, SystemPromptInput, DYNAMIC_BOUNDARY,
    };

    let marker = DYNAMIC_BOUNDARY.encode_utf16().collect::<Vec<_>>();
    let mut exact_units = "before".encode_utf16().collect::<Vec<_>>();
    exact_units.push(0xd800);
    exact_units.push(b'\n' as u16);
    exact_units.extend("\u{feff} \u{00a0}\t".encode_utf16());
    exact_units.extend(marker.iter().copied());
    exact_units.push(b'\r' as u16);
    exact_units.push(0x2028);
    exact_units.push(b'\n' as u16);
    exact_units.extend("middle\n".encode_utf16());
    exact_units.extend(marker.iter().copied());
    exact_units.push(b'\n' as u16);
    exact_units.push(0xdc00);

    let original = PromptText::from_utf16(exact_units);
    let parsed = native_custom_system_prompt(original.clone());
    let SystemPromptInput::NativeCustomPrompt {
        text,
        source_elements,
    } = parsed
    else {
        panic!("Native customSystemPrompt must retain its parsed source elements");
    };
    assert_eq!(text, original);
    assert_eq!(source_elements.len(), 3);
    assert_eq!(
        source_elements[0].utf16_code_units(),
        [
            "before".encode_utf16().collect::<Vec<_>>(),
            vec![0xd800, b'\n' as u16]
        ]
        .concat()
    );
    assert_eq!(source_elements[1].utf16_code_units(), marker);
    let mut expected_after = vec![b'\n' as u16];
    expected_after.extend("middle\n".encode_utf16());
    expected_after.extend(marker.iter().copied());
    expected_after.push(b'\n' as u16);
    expected_after.push(0xdc00);
    assert_eq!(source_elements[2].utf16_code_units(), expected_after);

    let empty_sides = PromptText::from_string(format!("\n \t{DYNAMIC_BOUNDARY} \r\n"));
    let parsed = native_custom_system_prompt(empty_sides);
    let SystemPromptInput::NativeCustomPrompt {
        source_elements, ..
    } = parsed
    else {
        panic!("Native customSystemPrompt variant");
    };
    assert_eq!(source_elements.len(), 1);
    assert!(source_elements[0].equals_ascii(DYNAMIC_BOUNDARY));

    let no_marker = PromptText::from_utf16(vec![0xd800, b' ' as u16, b'x' as u16]);
    let parsed = native_custom_system_prompt(no_marker.clone());
    let SystemPromptInput::NativeCustomPrompt {
        text,
        source_elements,
    } = parsed
    else {
        panic!("Native customSystemPrompt variant");
    };
    assert_eq!(text, no_marker);
    assert_eq!(source_elements, vec![no_marker]);

    let through_string_entry =
        custom_system_prompt(Some(&format!("first\n{DYNAMIC_BOUNDARY}\nsecond")))
            .expect("custom string input");
    assert!(matches!(
        through_string_entry,
        SystemPromptInput::NativeCustomPrompt { source_elements, .. }
            if source_elements.len() == 3
    ));
}

#[test]
fn frozen_stream_quote_stays_out_of_serialized_provider_metadata() {
    let mut events = vec![HistoryEvent::MessageDelta {
        delta: crate::HistoryMessageDelta {
            stop_reason: Some("end_turn".into()),
            stop_details: None,
            resumable: None,
        },
        usage: Some(crate::ExecutionUsage::default()),
    }];
    attach_frozen_stream_quote(&mut events, None);
    let HistoryEvent::MessageDelta {
        usage: Some(usage), ..
    } = &events[0]
    else {
        panic!("message delta");
    };
    assert!(usage.cost_estimate.is_none());
    let mut quote = crate::CostEstimate::unestimated(crate::PricingModelRef {
        pricing_provider_id: crate::ProviderId::OpenAICompatible {
            name: "deepseek".into(),
        },
        billing_model: "deepseek-flash".into(),
        request_model: "deepseek-flash".into(),
        display_model: "deepseek-flash".into(),
    });
    quote.estimated = true;
    quote.total_cost_usd = Some(0.00075);
    attach_frozen_stream_quote(&mut events, Some(&quote));
    let HistoryEvent::MessageDelta {
        usage: Some(usage), ..
    } = &events[0]
    else {
        panic!("message delta");
    };
    assert_eq!(
        usage.cost_estimate.as_ref().unwrap().total_cost_usd,
        Some(0.00075)
    );
    assert!(serde_json::to_value(usage)
        .unwrap()
        .get("cost_estimate")
        .is_none());
}

#[test]
fn pinned_client_prices_deepseek_from_local_catalog_and_dispatch_time() {
    use lingxi_llm_client::protocol as wire;
    let profile = crate::builtin_presets()
        .providers
        .into_iter()
        .find(|profile| profile.profile_name == "deepseek")
        .unwrap()
        .wire_profile
        .unwrap();
    let display = profile
        .models
        .iter()
        .find(|model| model.request_model == "deepseek-flash")
        .unwrap()
        .display_model
        .clone();
    let frozen =
        lingxi_llm_client::FrozenPricing::capture(&profile, &display, "deepseek-flash").unwrap();
    let complete = wire::UsageReport::measured(
        wire::Usage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            ..Default::default()
        },
        wire::UsageState::Complete,
    );
    let partial = wire::UsageReport::from(complete.usage.unwrap());
    let at = |executed_at| wire::InferenceReport {
        executed_at: Some(executed_at),
        service_tier: Some(wire::ServiceTier::Standard),
        ..Default::default()
    };
    assert!(frozen
        .estimate(&partial, &at(1_790_208_000), wire::Submission::Interactive)
        .is_err());
    let off_peak = frozen
        .estimate(&complete, &at(1_790_208_000), wire::Submission::Interactive)
        .unwrap();
    let peak = frozen
        .estimate(&complete, &at(1_790_215_200), wire::Submission::Interactive)
        .unwrap();
    assert_eq!(off_peak.total_cost, 0.75);
    assert_eq!(peak.total_cost, 1.5);
}

#[tokio::test]
async fn a_transport_failure_keeps_unknown_execution_outcome() {
    let transport = FakeTransport::sequence(vec![FakeResponse::Err(LlmError::Transport {
        message: "connection refused".into(),
    })]);
    // One attempt, no backoff ladder: this test is about how a dispatched
    // attempt settles, not about the retry policy.
    let service = make_adapter_with_retries(transport.clone(), Some(0));
    let probe = Arc::new(AttemptProbe::default());
    service.set_model_attempt_hooks(Arc::new(probe.clone()));
    assert!(service
        .execute_side_query_request(registered_request())
        .await
        .is_err());
    let events = probe.events.lock().unwrap().clone();
    assert!(
        !events.contains(&"no-provider-response"),
        "missing usage must leave the dispatched attempt unknown: {events:?}"
    );
    assert!(
        probe.observations.lock().unwrap().is_empty(),
        "nothing was observed, so nothing may be priced"
    );
}

#[tokio::test]
async fn rejected_dispatch_admission_never_sends_or_marks_the_attempt() {
    let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let service = make_adapter_with_retries(transport.clone(), Some(0));
    let probe = Arc::new(AttemptProbe::default());
    service.set_model_attempt_hooks(Arc::new(probe.clone()));
    let mut request = crate::MessagesCreateRequest::new(
        "claude-sonnet-4-20250514",
        None,
        None,
        vec![lingxi_core::types::ConversationMessage::user(
            lingxi_core::types::MessageId::new(),
            "fake request".into(),
        )],
        Vec::new(),
    );
    request.opts.max_output_tokens = Some(100);
    request.opts.model_attempt = registered_request().execution.model_attempt;
    request.opts.request_dispatch_admission = Some(crate::RequestDispatchAdmission::new(|| false));
    let dispatch_observations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = dispatch_observations.clone();
    request.opts.request_dispatch_observer = Some(crate::RequestDispatchObserver::new(move || {
        observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }));
    let capture = crate::prompt_cache::RequestCapture::default();

    let result = capture.scope(service.messages_create(request)).await;

    assert!(matches!(
        result,
        Err(LlmError::RequestDispatchRejected {
            prior_dispatch: false
        })
    ));
    assert_eq!(
        transport.seen_count(),
        0,
        "rejected prompts never reach SDK transport"
    );
    assert_eq!(
        dispatch_observations.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "rejected prompts are not observed as dispatched"
    );
    assert_eq!(
        *probe.events.lock().unwrap(),
        vec!["begin", "finish-owned", "settled"],
        "rejection settles the reserved attempt as not dispatched"
    );
    assert!(
        capture.take().is_none(),
        "unadmitted prompts are not recorded as sent"
    );
}

#[tokio::test(start_paused = true)]
async fn retry_rejection_reports_prior_dispatch_and_never_replays_again() {
    let transport = FakeTransport::sequence(vec![FakeResponse::Err(LlmError::Transport {
        message: "connection refused".into(),
    })]);
    let service = make_adapter_with_retries(transport.clone(), Some(1));
    let probe = Arc::new(AttemptProbe::default());
    service.set_model_attempt_hooks(Arc::new(probe.clone()));
    let admissions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let check = admissions.clone();
    let dispatch_observations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = dispatch_observations.clone();
    let mut request = registered_request();
    request.execution.request_dispatch_admission =
        Some(crate::RequestDispatchAdmission::new(move || {
            check.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0
        }));
    request.execution.request_dispatch_observer =
        Some(crate::RequestDispatchObserver::new(move || {
            observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }));

    let result = service
        .execute_non_stream_request(
            request,
            crate::NonStreamingRequestClass::Main,
            crate::NonStreamingRetryOptions::default(),
        )
        .await;

    assert!(matches!(
        result,
        Err(LlmError::RequestDispatchRejected {
            prior_dispatch: true
        })
    ));
    assert_eq!(
        transport.seen_count(),
        1,
        "the rejected retry is never replayed"
    );
    assert_eq!(
        dispatch_observations.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the observer reports one admitted physical dispatch across retries"
    );
    assert_eq!(
        *probe.events.lock().unwrap(),
        vec![
            "begin",
            "dispatch",
            "finish-owned",
            "settled",
            "begin",
            "finish-owned",
            "settled",
        ]
    );
}

#[tokio::test]
async fn incomplete_nonstream_response_keeps_unknown_attempt() {
    let service = make_adapter_with_retries(
        Arc::new(IncompleteResponseTransport { status: 200 }),
        Some(0),
    );
    let probe = Arc::new(AttemptProbe::default());
    service.set_model_attempt_hooks(Arc::new(probe.clone()));
    assert!(service
        .execute_side_query_request(registered_request())
        .await
        .is_err());
    assert_eq!(
        *probe.events.lock().unwrap(),
        vec!["begin", "dispatch", "finish-owned", "settled"]
    );
    assert!(probe.observations.lock().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn stalled_stream_error_body_times_out_and_settles_once() {
    for status in [429, 500] {
        let service =
            make_adapter_with_retries(Arc::new(IncompleteResponseTransport { status }), Some(3))
                .with_stream_idle_timeout_override(Some(Duration::from_secs(5)));
        let probe = Arc::new(AttemptProbe::default());
        service.set_model_attempt_hooks(Arc::new(probe.clone()));
        let result = tokio::time::timeout(
            Duration::from_secs(6),
            service.stream_request(registered_request()),
        )
        .await;
        assert!(matches!(result, Ok(Err(LlmError::TransportTimeout { .. }))));
        assert_eq!(
            *probe.events.lock().unwrap(),
            vec!["begin", "dispatch", "finish-owned", "settled"]
        );
    }
}

#[tokio::test]
async fn owned_main_request_requires_and_settles_its_registered_attempt() {
    for bind_hooks in [false, true] {
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let service = make_adapter_with_retries(transport.clone(), Some(0));
        service.set_thinking(crate::model::thinking::ThinkingConfig::Disabled);
        let probe = Arc::new(AttemptProbe::default());
        if bind_hooks {
            service.set_model_attempt_hooks(Arc::new(probe.clone()));
        }
        let registered = registered_request();
        let mut request = crate::MessagesCreateRequest::new(
            &registered.input.model,
            None,
            None,
            vec![lingxi_core::types::ConversationMessage::user(
                lingxi_core::types::MessageId::new(),
                "fake request".into(),
            )],
            Vec::new(),
        );
        request.opts.max_output_tokens = Some(100);
        request.opts.model_attempt = registered.execution.model_attempt;
        request.opts.query_source = Some("registered-main-test".into());
        let result = service.messages_create(request).await;
        if !bind_hooks {
            assert!(
                matches!(result, Err(LlmError::InvalidRequest { message }) if message.contains("requires host accounting hooks"))
            );
            assert_eq!(
                transport.seen_count(),
                0,
                "unowned admission cannot dispatch"
            );
            assert!(probe.events.lock().unwrap().is_empty());
            continue;
        }
        result.unwrap();
        assert_eq!(transport.seen_count(), 1);
        assert_eq!(
            *probe.events.lock().unwrap(),
            ["begin", "dispatch", "finish-owned", "settled"]
        );
        let seen = transport.seen.lock().unwrap();
        let body = request_body_value(&seen[0].body_json);
        assert_eq!(body["max_tokens"], 100);
        assert!(body.get("model_attempt").is_none());
        assert!(body.get("query_source").is_none());
        let observations = probe.observations.lock().unwrap();
        assert_eq!(observations.len(), 1);
        assert_eq!(observations[0].0.counts().input_tokens, 5);
        assert_eq!(
            observations[0].1,
            crate::ModelAttemptUsageCompleteness::Complete
        );
    }
}

#[tokio::test]
async fn scope_quota_rechecks_revocation_after_awaited_host_admission() {
    for streaming in [false, true] {
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let quota = scope_quota(1);
        let service = Arc::new(
            make_adapter_with_retries(transport.clone(), Some(0))
                .with_scope_output_quota(Some(quota.clone())),
        );
        let waiting = Arc::new(ScopeWaitingAdmission {
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
            probe: Arc::new(AttemptProbe::default()),
        });
        service.set_model_attempt_hooks(Arc::new(waiting.clone()));
        let run = tokio::spawn(async move {
            if streaming {
                match service.stream_request(registered_request()).await {
                    Err(error) => error,
                    Ok(_) => panic!("revoked scope opened a stream"),
                }
            } else {
                service
                    .execute_side_query_request(registered_request())
                    .await
                    .unwrap_err()
            }
        });
        tokio::time::timeout(Duration::from_secs(5), waiting.entered.notified())
            .await
            .unwrap();
        quota.revoke();
        waiting.release.notify_one();
        assert!(matches!(
            run.await.unwrap(),
            LlmError::RequestDispatchRejected {
                prior_dispatch: false
            }
        ));
        assert_eq!(quota.remaining_dispatches(), 1);
        assert_eq!(transport.seen_count(), 0);
        assert!(!waiting.probe.events.lock().unwrap().contains(&"dispatch"));
    }
}

#[tokio::test]
async fn scope_quota_covers_responses_websocket_and_disables_prewarm() {
    use futures::StreamExt;
    let transport = Arc::new(HttpOnlyProbeTransport {
        frames: vec![
            br#"{"type":"response.created","response":{"id":"r1","model":"model","output":[]}}"#.to_vec(),
            br#"{"type":"response.completed","response":{"id":"r1","model":"model","status":"completed","output":[],"usage":{"input_tokens":4,"output_tokens":3,"total_tokens":7}}}"#.to_vec(),
        ], http_calls: Default::default(), ws_calls: Default::default(),
    });
    let quota = scope_quota(2);
    let service = make_adapter_for_protocol_with_websocket(
        ProtocolFamily::OpenAiResponses,
        ProviderId::OpenAI,
        "https://api.openai.com/v1",
        "test",
        "model",
        transport.clone(),
        true,
    )
    .with_scope_output_quota(Some(quota.clone()));
    service
        .prewarm_responses_websocket("model", None, None, Vec::new(), Vec::new(), false)
        .await
        .unwrap();
    assert_eq!(quota.remaining_dispatches(), 2);
    assert_eq!(
        transport.ws_calls.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    for _ in 0..2 {
        let mut request = LlmRequest::new("model").with_user_text("hello");
        request.input.max_tokens = Some(u32::MAX);
        let mut stream = service.stream_request(request).await.unwrap();
        while let Some(event) = stream.next().await {
            event.unwrap();
        }
    }
    assert_eq!(quota.remaining_dispatches(), 0);
    assert!(matches!(
        service
            .stream_request(LlmRequest::new("model").with_user_text("last"))
            .await,
        Err(LlmError::ApiAttemptsExhausted)
    ));
    assert_eq!(
        transport
            .http_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    assert_eq!(
        transport.ws_calls.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
}

#[tokio::test]
async fn scope_quota_checks_final_codec_fields_and_rejects_generation_multipliers() {
    for (protocol, extra) in [
        (ProtocolFamily::OpenAiChat, serde_json::json!({"n": 2})),
        (ProtocolFamily::OpenAiChat, serde_json::json!({"n": "1"})),
        (
            ProtocolFamily::OpenAiChat,
            serde_json::json!({"best_of": 3}),
        ),
    ] {
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let quota = scope_quota(1);
        let service = scope_wire_service(protocol, extra, AuthStrategy::None, transport.clone())
            .with_scope_output_quota(Some(quota.clone()));
        let error = service
            .execute_side_query_request(LlmRequest::new("model").with_user_text("hello"))
            .await
            .expect_err("multiple outputs cannot share a single-output allowance");
        assert!(
            matches!(
                error,
                LlmError::UnsupportedCapability { .. } | LlmError::InvalidRequest { .. }
            ),
            "{protocol:?}: {error}"
        );
        assert_eq!(transport.seen_count(), 0, "{protocol:?}");
        assert_eq!(quota.remaining_dispatches(), 1);
    }
}

#[tokio::test]
async fn scope_quota_validates_real_sdk_wire_limits_for_supported_protocols() {
    for protocol in [
        ProtocolFamily::OpenAiChat,
        ProtocolFamily::OpenAiResponses,
        ProtocolFamily::GeminiGenerateContent,
        ProtocolFamily::GeminiInteractions,
    ] {
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let quota = scope_quota(1);
        let service = scope_wire_service(
            protocol,
            serde_json::json!({}),
            if protocol == ProtocolFamily::GeminiInteractions {
                AuthStrategy::ApiKey
            } else {
                AuthStrategy::None
            },
            transport,
        )
        .with_scope_output_quota(Some(quota));
        let mut request = LlmRequest::new("model").with_user_text("hello");
        service.clamp_scope_output(&mut request, false).unwrap();
        let mut prepared = service
            .client
            .prepare_on(&request, service.transport.clone())
            .await
            .unwrap();
        service.client.seal_prepared(&mut prepared).await.unwrap();
        service.validate_scope_output(&prepared).unwrap();
    }
}

#[tokio::test]
async fn scope_quota_never_drops_the_limit_for_unsupported_subscription_routes() {
    for auth in [AuthStrategy::ChatGptOAuth, AuthStrategy::ChatGptPlan] {
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let quota = scope_quota(1);
        let service = scope_wire_service(
            ProtocolFamily::OpenAiResponses,
            serde_json::json!({}),
            auth,
            transport.clone(),
        )
        .with_scope_output_quota(Some(quota.clone()));
        let mut request = LlmRequest::new("model").with_user_text("hello");
        request.stream = true;
        request.input.controls.responses.store = Some(false);
        // Prove this is a valid subscription fixture before adding the cap.
        let mut uncapped = service
            .client
            .prepare_on(&request, service.transport.clone())
            .await
            .unwrap();
        service.client.seal_prepared(&mut uncapped).await.unwrap();
        service.clamp_scope_output(&mut request, false).unwrap();
        let result = async {
            let mut prepared = service
                .client
                .prepare_on(&request, service.transport.clone())
                .await?;
            service.client.seal_prepared(&mut prepared).await?;
            service.validate_scope_output(&prepared)
        }
        .await;
        assert!(
            matches!(&result, Err(LlmError::UnsupportedCapability { .. }))
                || matches!(&result, Err(LlmError::InvalidRequest { message }) if message.contains("max_output_tokens")),
            "{auth:?}: {result:?}"
        );
        assert_eq!(transport.seen_count(), 0);
        assert_eq!(quota.remaining_dispatches(), 1);
    }
}

/// P0-9: a registered attempt on a WebSocket-capable route keeps the
/// WebSocket. It used to be forced onto HTTP, because the prepared-WS path
/// never marked dispatch and metering would have been lost.
#[tokio::test]
async fn a_registered_attempt_keeps_the_websocket_and_is_still_metered() {
    use futures::StreamExt;
    let transport = Arc::new(HttpOnlyProbeTransport {
        frames: vec![
            br#"{"type":"response.created","response":{"id":"r1","model":"model","output":[]}}"#.to_vec(),
            br#"{"type":"response.completed","response":{"id":"r1","model":"model","status":"completed","output":[],"usage":{"input_tokens":4,"output_tokens":3,"total_tokens":7}}}"#.to_vec(),
        ], http_calls: Default::default(), ws_calls: Default::default(),
    });
    let service = make_adapter_for_protocol_with_websocket(
        ProtocolFamily::OpenAiResponses,
        ProviderId::OpenAI,
        "https://api.openai.com/v1",
        "test",
        "model",
        transport.clone(),
        true,
    );
    let mut request = registered_request();
    request.input.model = "model".into();
    request.stream = true;
    assert_eq!(
        service
            .client
            .prepare(&request)
            .await
            .unwrap()
            .provider_request
            .stream_transport,
        crate::ProviderStreamTransport::ResponsesWebSocket
    );
    let probe = Arc::new(AttemptProbe::default());
    service.set_model_attempt_hooks(Arc::new(probe.clone()));
    // Each drive opens a fresh WebSocket session while sharing one connection.
    for _ in 0..2 {
        let mut stream = service.stream_request(request.clone()).await.unwrap();
        while let Some(event) = stream.next().await {
            event.unwrap();
        }
    }
    assert_eq!(
        transport
            .http_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        0,
        "a WebSocket-capable route must not be pushed onto HTTP"
    );
    assert_eq!(
        transport.ws_calls.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    assert_eq!(
        *probe.events.lock().unwrap(),
        ["begin", "dispatch", "finish-owned", "settled"].repeat(2),
        "the WebSocket send is metered exactly like an HTTP one"
    );
    assert_eq!(
        probe.observations.lock().unwrap().last().unwrap().1,
        crate::ModelAttemptUsageCompleteness::Complete
    );
}

#[tokio::test]
async fn registered_gemini_partial_usage_is_owned_before_content_and_not_promoted_by_eof() {
    use futures::StreamExt;
    for drop_early in [false, true] {
        let transport = Arc::new(HttpOnlyProbeTransport {
            frames: vec![br#"{"candidates":[{"content":{"parts":[{"text":"partial"}]}}],"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":4,"thoughtsTokenCount":6,"totalTokenCount":15}}"#.to_vec()],
            http_calls: Default::default(), ws_calls: Default::default(),
        });
        let service = make_adapter_for_protocol_with_transport(
            ProtocolFamily::GeminiGenerateContent,
            ProviderId::Gemini,
            "https://generativelanguage.googleapis.com/v1beta",
            "test",
            "model",
            transport,
        );
        let probe = Arc::new(AttemptProbe::default());
        service.set_model_attempt_hooks(Arc::new(probe.clone()));
        let mut request = registered_request();
        request.input.model = "model".into();
        let mut stream = service.stream_request(request).await.unwrap();
        stream.next().await.unwrap().unwrap();
        {
            let observations = probe.observations.lock().unwrap();
            let (usage, completeness) = observations.last().unwrap();
            assert_eq!(usage.counts().output_tokens, 10);
            assert_eq!(usage.counts().reasoning_tokens, 6);
            assert_eq!(*completeness, crate::ModelAttemptUsageCompleteness::Partial);
        }
        if !drop_early {
            let mut interrupted = false;
            while let Some(event) = stream.next().await {
                match event {
                    Ok(_) => {}
                    Err(LlmError::StreamInterrupted { .. }) => interrupted = true,
                    Err(error) => panic!("unexpected stream error: {error:?}"),
                }
            }
            assert!(
                interrupted,
                "EOF without a terminal frame must report interruption"
            );
        }
        drop(stream);
        assert!(probe.observations.lock().unwrap().iter().all(
            |(_, completeness)| *completeness == crate::ModelAttemptUsageCompleteness::Partial
        ));
        assert_eq!(
            probe.events.lock().unwrap().last().copied(),
            Some(if drop_early { "drop-owned" } else { "settled" })
        );
    }
}

#[tokio::test(start_paused = true)]
async fn registered_dispatch_header_retry_preserves_one_settlement_per_send() {
    let _guard = cedar_lattice_write();
    let _enabled = CedarLatticeOn::set();
    let transport = FakeTransport::sequence(vec![
        FakeResponse::Ok(ProviderResponse::json(
            500,
            serde_json::json!({"type":"error","error":{"type":"api_error","message":"retry"}}),
        )),
        FakeResponse::Ok(ProviderResponse::json(200, ok_response_json())),
    ]);
    let service = make_adapter(transport.clone());
    let probe = Arc::new(AttemptProbe::default());
    service.set_model_attempt_hooks(Arc::new(probe.clone()));
    let request = registered_request();
    let control = resolve_retry_control_with_settings(
        &request.input.model,
        None,
        false,
        &ResolveRetryEnv::from_process_env(),
        Some(0),
    );
    service
        .drive_non_stream(request, control, DispatchHeaderState::default())
        .await
        .unwrap();
    assert_eq!(transport.seen_count(), 2);
    assert_eq!(
        transport
            .seen_headers(0)
            .get(DISPATCH_ID_HEADER)
            .map(String::as_str),
        Some("v2s")
    );
    assert_eq!(
        transport
            .seen_headers(1)
            .get(DISPATCH_ID_HEADER)
            .map(String::as_str),
        Some("v2p")
    );
    assert_eq!(
        *probe.events.lock().unwrap(),
        vec![
            "begin",
            "dispatch",
            // Missing usage keeps the first physical attempt unknown.
            "finish-owned",
            "settled",
            "begin",
            "dispatch",
            "finish-owned",
            "settled"
        ]
    );
}

#[tokio::test]
async fn registered_absent_hooks_and_direct_backend_reject_before_wire() {
    let transport = FakeTransport::always(ProviderResponse {
        status: 200,
        headers: BTreeMap::new(),
        body_json: provider_payload(ok_response_json()),
        request_id: None,
    });
    let adapter = make_adapter(transport.clone());
    let request = registered_request();
    assert!(adapter
        .execute_side_query_request(request.clone())
        .await
        .is_err());
    assert!(adapter.stream_request(request.clone()).await.is_err());
    assert!(adapter
        .client
        .execute(&request, transport.clone())
        .await
        .is_err());
    assert_eq!(transport.seen_count(), 0);
}

#[tokio::test]
async fn registered_hook_retirement_drops_outside_lock_and_rejects_late_wire() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let transport = FakeTransport::always(ProviderResponse {
        status: 200,
        headers: BTreeMap::new(),
        body_json: provider_payload(ok_response_json()),
        request_id: None,
    });
    let service = Arc::new(make_adapter(transport.clone()));
    for replacement in [true, false] {
        let unlocked = Arc::new(AtomicBool::new(false));
        service.set_model_attempt_hooks(Arc::new(HookRetirementProbe {
            service: Arc::downgrade(&service),
            unlocked_drop: unlocked.clone(),
        }));
        if replacement {
            service.set_model_attempt_hooks(Arc::new(Arc::new(AttemptProbe::default())));
        } else {
            service.clear_model_attempt_hooks();
        }
        assert!(
            unlocked.load(Ordering::SeqCst),
            "hook destruction must run outside the write lock"
        );
    }
    service.clear_model_attempt_hooks(); // idempotent
    let error = service
        .execute_side_query_request(registered_request())
        .await
        .unwrap_err();
    assert!(
        matches!(error, LlmError::InvalidRequest { message } if message.contains("requires host accounting hooks"))
    );
    assert!(service.stream_request(registered_request()).await.is_err());
    assert_eq!(transport.seen_count(), 0);
    let mut ordinary = registered_request();
    ordinary.execution.model_attempt = None;
    service.execute_side_query_request(ordinary).await.unwrap();
    assert_eq!(
        transport.seen_count(),
        1,
        "ordinary calls remain unaffected"
    );
}

#[tokio::test(start_paused = true)]
async fn registered_physical_retries_settle_each_attempt_before_next_send() {
    let transport = FakeTransport::sequence(vec![
        FakeResponse::Err(LlmError::Transport {
            message: "fake connect error".into(),
        }),
        FakeResponse::Ok(ProviderResponse {
            status: 200,
            headers: BTreeMap::new(),
            body_json: provider_payload(ok_response_json()),
            request_id: None,
        }),
    ]);
    let adapter = make_adapter(transport.clone());
    let probe = Arc::new(AttemptProbe::default());
    adapter.set_model_attempt_hooks(Arc::new(probe.clone()));
    adapter
        .execute_side_query_request(registered_request())
        .await
        .unwrap();
    assert_eq!(transport.seen_count(), 2);
    assert_eq!(
        *probe.events.lock().unwrap(),
        vec![
            "begin",
            "dispatch",
            // Missing usage keeps the first physical attempt unknown.
            "finish-owned",
            "settled",
            "begin",
            "dispatch",
            "finish-owned",
            "settled"
        ]
    );
    let observations = probe.observations.lock().unwrap();
    assert_eq!(observations.len(), 1);
    assert_eq!(observations[0].0.counts().input_tokens, 5);
    assert_eq!(
        observations[0].1,
        crate::ModelAttemptUsageCompleteness::Complete
    );
}

#[tokio::test]
async fn registered_settlement_failure_is_terminal_and_never_transport_retried() {
    let transport = FakeTransport::always(ProviderResponse {
        status: 200,
        headers: BTreeMap::new(),
        body_json: provider_payload(ok_response_json()),
        request_id: None,
    });
    let adapter = make_adapter(transport.clone());
    let probe = Arc::new(AttemptProbe {
        fail_settlement: true,
        ..AttemptProbe::default()
    });
    adapter.set_model_attempt_hooks(Arc::new(probe));
    assert!(matches!(
        adapter
            .execute_side_query_request(registered_request())
            .await,
        Err(LlmError::CostUnavailable { .. })
    ));
    assert_eq!(transport.seen_count(), 1);
}

#[tokio::test]
async fn registered_stream_observes_before_yield_and_drop_retains_partial_usage() {
    use futures::StreamExt;
    let adapter = make_adapter(ScriptedStreamTransport::anthropic_success());
    let probe = Arc::new(AttemptProbe::default());
    adapter.set_model_attempt_hooks(Arc::new(probe.clone()));
    let mut stream = adapter.stream_request(registered_request()).await.unwrap();
    assert!(matches!(
        stream.next().await.unwrap().unwrap(),
        HistoryEvent::MessageStart { .. }
    ));
    {
        let seen = probe.observations.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0.counts().input_tokens, 1);
        assert_eq!(seen[0].1, crate::ModelAttemptUsageCompleteness::Partial);
    }
    drop(stream);
    assert_eq!(
        *probe.events.lock().unwrap(),
        vec!["begin", "dispatch", "drop-owned"]
    );
}

#[tokio::test]
async fn registered_stream_final_usage_merges_cumulative_counters_once() {
    use futures::StreamExt;
    let adapter = make_adapter(ScriptedStreamTransport::anthropic_success());
    let probe = Arc::new(AttemptProbe::default());
    adapter.set_model_attempt_hooks(Arc::new(probe.clone()));
    let mut stream = adapter.stream_request(registered_request()).await.unwrap();
    while let Some(event) = stream.next().await {
        event.unwrap();
    }
    let seen = probe.observations.lock().unwrap();
    let (usage, completeness) = seen.last().unwrap();
    assert_eq!(usage.counts().input_tokens, 1);
    assert_eq!(usage.counts().output_tokens, 1);
    assert_eq!(
        *completeness,
        crate::ModelAttemptUsageCompleteness::Complete
    );
    assert_eq!(
        *probe.events.lock().unwrap(),
        vec!["begin", "dispatch", "finish-owned", "settled"]
    );
}

#[tokio::test]
async fn registered_success_without_provider_usage_remains_incomplete() {
    let mut body = ok_response_json();
    body.as_object_mut().unwrap().remove("usage");
    let adapter = make_adapter(FakeTransport::always(ProviderResponse {
        status: 200,
        headers: BTreeMap::new(),
        body_json: provider_payload(body),
        request_id: None,
    }));
    let probe = Arc::new(AttemptProbe::default());
    adapter.set_model_attempt_hooks(Arc::new(probe.clone()));
    adapter
        .execute_side_query_request(registered_request())
        .await
        .unwrap();
    assert_eq!(
        probe.observations.lock().unwrap()[0].1,
        crate::ModelAttemptUsageCompleteness::Partial
    );
}

#[tokio::test]
async fn registered_http_content_decode_error_preserves_independently_extracted_usage() {
    let mut body = ok_response_json();
    body["content"] = serde_json::json!([{"type":"tool_use","id":"tool1","input":{}}]);
    let transport = FakeTransport::always(ProviderResponse::json(200, body));
    let service = make_adapter(transport.clone());
    let probe = Arc::new(AttemptProbe::default());
    service.set_model_attempt_hooks(Arc::new(probe.clone()));
    assert!(service
        .execute_side_query_request(registered_request())
        .await
        .is_err());
    assert_eq!(transport.seen_count(), 1);
    let seen = probe.observations.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0.counts().input_tokens, 5);
    assert_eq!(seen[0].0.counts().output_tokens, 2);
    assert_eq!(seen[0].1, crate::ModelAttemptUsageCompleteness::Complete);
    assert_eq!(
        probe.events.lock().unwrap().last().copied(),
        Some("settled")
    );
}

#[tokio::test]
async fn registered_responses_failed_event_retains_usage_as_partial_before_error() {
    use futures::StreamExt;
    let transport = Arc::new(HttpOnlyProbeTransport {
        frames: vec![br#"{"type":"response.failed","response":{"id":"r1","status":"failed","error":{"code":"server_error","message":"failed"},"usage":{"input_tokens":10,"output_tokens":7,"output_tokens_details":{"reasoning_tokens":3}}}}"#.to_vec()],
        http_calls: Default::default(), ws_calls: Default::default(),
    });
    let service = make_adapter_for_protocol_with_transport(
        ProtocolFamily::OpenAiResponses,
        ProviderId::OpenAI,
        "https://api.openai.com/v1",
        "test",
        "model",
        transport,
    );
    let probe = Arc::new(AttemptProbe::default());
    service.set_model_attempt_hooks(Arc::new(probe.clone()));
    let mut request = registered_request();
    request.input.model = "model".into();
    let mut stream = service.stream_request(request).await.unwrap();
    assert!(stream.next().await.unwrap().is_err());
    let seen = probe.observations.lock().unwrap();
    let (usage, completeness) = seen.last().unwrap();
    assert_eq!(usage.counts().input_tokens, 10);
    assert_eq!(usage.counts().output_tokens, 7);
    assert_eq!(usage.counts().reasoning_tokens, 3);
    assert_eq!(*completeness, crate::ModelAttemptUsageCompleteness::Partial);
    assert_eq!(
        probe.events.lock().unwrap().last().copied(),
        Some("settled")
    );
}

#[tokio::test]
async fn registered_anthropic_output_deltas_retain_cache_ttl_without_double_counting() {
    use futures::StreamExt;
    let transport = Arc::new(ScriptedStreamTransport {
        frames: vec![
            br#"{"type":"message_start","message":{"id":"m","model":"claude-sonnet-4-20250514","usage":{"input_tokens":2,"output_tokens":0,"cache_creation_input_tokens":100,"cache_creation":{"ephemeral_1h_input_tokens":100}}}}"#.to_vec(),
            br#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}"#.to_vec(),
            br#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}"#.to_vec(),
            br#"{"type":"message_stop"}"#.to_vec(),
        ], status: 200, headers: BTreeMap::new(),
    });
    let service = make_adapter(transport);
    let probe = Arc::new(AttemptProbe::default());
    service.set_model_attempt_hooks(Arc::new(probe.clone()));
    let mut stream = service.stream_request(registered_request()).await.unwrap();
    while let Some(event) = stream.next().await {
        event.unwrap();
    }
    let seen = probe.observations.lock().unwrap();
    let (usage, completeness) = seen.last().unwrap();
    assert_eq!(usage.counts().cache_write_tokens, 100);
    assert_eq!(usage.counts().output_tokens, 5);
    assert_eq!(
        usage
            .provider_metadata
            .raw_source_at("/cache_creation/ephemeral_1h_input_tokens")
            .as_deref(),
        Some("100")
    );
    assert_eq!(
        *completeness,
        crate::ModelAttemptUsageCompleteness::Complete
    );
    // The shared stream may omit repeated snapshots with no new facts.
    // The contract is cumulative replacement, never adding the repeated
    // output/cache counters a second time.
    assert!(seen.iter().all(|(usage, _)| {
        usage.counts().output_tokens <= 5 && usage.counts().cache_write_tokens <= 100
    }));
}

#[tokio::test]
async fn registered_complete_usage_requires_provider_input_and_output_but_accepts_explicit_zero() {
    for (protocol, provider, input_key, output_key, usage_key, mut body) in [
        (
            ProtocolFamily::AnthropicMessages,
            ProviderId::AnthropicFirstParty,
            "input_tokens",
            "output_tokens",
            "usage",
            ok_response_json(),
        ),
        (
            ProtocolFamily::OpenAiChat,
            ProviderId::OpenAI,
            "prompt_tokens",
            "completion_tokens",
            "usage",
            serde_json::json!({"id":"r","model":"model","choices":[{"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}),
        ),
        (
            ProtocolFamily::OpenAiResponses,
            ProviderId::OpenAI,
            "input_tokens",
            "output_tokens",
            "usage",
            serde_json::json!({"id":"r","model":"model","status":"completed","output":[]}),
        ),
        (
            ProtocolFamily::GeminiGenerateContent,
            ProviderId::Gemini,
            "promptTokenCount",
            "candidatesTokenCount",
            "usageMetadata",
            serde_json::json!({"responseId":"r","modelVersion":"model","candidates":[{"content":{"parts":[{"text":"ok"}]},"finishReason":"STOP"}]}),
        ),
    ] {
        for shape in 0..3 {
            let mut usage = serde_json::json!({"total_tokens": 10});
            if shape > 0 {
                usage[input_key] = serde_json::json!(0);
            }
            if shape == 2 {
                usage[output_key] = serde_json::json!(0);
                usage["total_tokens"] = serde_json::json!(0);
            }
            body[usage_key] = usage;
            let transport = FakeTransport::always(ProviderResponse::json(200, body.clone()));
            let service = make_adapter_for_protocol_with_transport(
                protocol,
                provider.clone(),
                "https://example.test/v1",
                "test",
                "model",
                transport.clone(),
            );
            let probe = Arc::new(AttemptProbe::default());
            service.set_model_attempt_hooks(Arc::new(probe.clone()));
            let mut request = registered_request();
            request.input.model = "model".into();
            service.execute_side_query_request(request).await.unwrap();
            assert_eq!(transport.seen_count(), 1);
            assert_eq!(
                probe.observations.lock().unwrap().last().unwrap().1,
                if shape == 2 {
                    crate::ModelAttemptUsageCompleteness::Complete
                } else {
                    crate::ModelAttemptUsageCompleteness::Partial
                },
                "protocol {protocol:?}, shape {shape}"
            );
        }
    }
}

#[tokio::test]
async fn registered_contradictory_totals_and_incomplete_cache_splits_stay_partial() {
    for (metadata, complete) in [
        (
            serde_json::json!({"input_tokens":0,"output_tokens":0,"total_tokens":10}),
            false,
        ),
        (
            serde_json::json!({"input_tokens":0,"output_tokens":0,"total_tokens":0}),
            true,
        ),
        (
            serde_json::json!({"input_tokens":2,"output_tokens":5,"cache_creation_input_tokens":100,"cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":50}}),
            false,
        ),
        (
            serde_json::json!({"input_tokens":2,"output_tokens":5,"cache_creation_input_tokens":100,"cache_creation":{"ephemeral_5m_input_tokens":50,"ephemeral_1h_input_tokens":50}}),
            true,
        ),
        (
            serde_json::json!({"input_tokens":2,"output_tokens":5,"cache_creation_input_tokens":100,"cache_creation":{"ephemeral_1h_input_tokens":50}}),
            true,
        ),
        (
            serde_json::json!({"input_tokens":2,"output_tokens":5,"cache_creation_input_tokens":100,"cache_creation":{"ephemeral_5m_input_tokens":50}}),
            false,
        ),
        (
            serde_json::json!({"input_tokens":2,"output_tokens":5,"cache_creation_input_tokens":100,"cache_creation":{"ephemeral_5m_input_tokens":100}}),
            true,
        ),
    ] {
        let mut body = ok_response_json();
        body["usage"] = metadata;
        let transport = FakeTransport::always(ProviderResponse::json(200, body));
        let service = make_adapter(transport.clone());
        let probe = Arc::new(AttemptProbe::default());
        service.set_model_attempt_hooks(Arc::new(probe.clone()));
        service
            .execute_side_query_request(registered_request())
            .await
            .unwrap();
        assert_eq!(transport.seen_count(), 1);
        assert_eq!(
            probe.observations.lock().unwrap().last().unwrap().1,
            if complete {
                crate::ModelAttemptUsageCompleteness::Complete
            } else {
                crate::ModelAttemptUsageCompleteness::Partial
            }
        );
    }
}

#[tokio::test]
async fn registered_malformed_optional_billing_and_kimi_cache_overflow_stay_partial() {
    for key in [
        "cache_creation_input_tokens",
        "cache_read_input_tokens",
        "reasoning_output_tokens",
    ] {
        let mut body = ok_response_json();
        body["usage"][key] = serde_json::json!("malformed");
        let service = make_adapter(FakeTransport::always(ProviderResponse::json(200, body)));
        let probe = Arc::new(AttemptProbe::default());
        service.set_model_attempt_hooks(Arc::new(probe.clone()));
        service
            .execute_side_query_request(registered_request())
            .await
            .unwrap();
        assert_eq!(
            probe.observations.lock().unwrap().last().unwrap().1,
            crate::ModelAttemptUsageCompleteness::Partial,
            "{key}"
        );
    }
    for (nested, complete) in [(None, false), (Some(2), true)] {
        let mut usage =
            serde_json::json!({"prompt_tokens":5,"completion_tokens":1,"cached_tokens":9});
        if let Some(nested) = nested {
            usage["prompt_tokens_details"] = serde_json::json!({"cached_tokens":nested});
        }
        let body = serde_json::json!({"id":"r","model":"model","choices":[{"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":usage});
        let service = make_adapter_for_protocol_with_transport(
            ProtocolFamily::OpenAiChat,
            ProviderId::OpenAI,
            "https://example.test/v1",
            "test",
            "model",
            FakeTransport::always(ProviderResponse::json(200, body)),
        );
        let probe = Arc::new(AttemptProbe::default());
        service.set_model_attempt_hooks(Arc::new(probe.clone()));
        let mut request = registered_request();
        request.input.model = "model".into();
        service.execute_side_query_request(request).await.unwrap();
        let seen = probe.observations.lock().unwrap();
        assert_eq!(
            seen.last().unwrap().1,
            if complete {
                crate::ModelAttemptUsageCompleteness::Complete
            } else {
                crate::ModelAttemptUsageCompleteness::Partial
            }
        );
        if complete {
            assert_eq!(seen.last().unwrap().0.counts().cache_read_tokens, 2);
        }
    }
}
