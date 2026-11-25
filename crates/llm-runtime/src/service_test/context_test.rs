use super::*;

#[tokio::test]
async fn token_count_projects_deferred_system_through_real_service_paths() {
    use lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput;
    let transport = FakeTransport::always(ProviderResponse::json(
        200,
        serde_json::json!({"input_tokens":2095}),
    ));
    let service = make_adapter(transport.clone());
    assert_eq!(
        service
            .count_tokens(
                "claude-sonnet-4-20250514",
                None,
                Some("count this system"),
                vec![],
                vec![]
            )
            .await
            .unwrap(),
        2095
    );
    assert_eq!(
        transport
            .seen_body(0)
            .string_at("/system/0/text")
            .as_deref(),
        Some("count this system")
    );
    service
        .count_tokens_exact(
            "claude-sonnet-4-20250514",
            None,
            Some("count exact system"),
            vec![],
            vec![],
        )
        .await
        .unwrap();
    assert_eq!(
        transport
            .seen_body(1)
            .string_at("/system/0/text")
            .as_deref(),
        Some("count exact system")
    );
    let source = SystemPromptInput::source_vector(
        vec![PromptText::from_utf16(vec![
            b'A' as u16,
            0xd800,
            b'B' as u16,
        ])],
        Some(PromptText::from_string("dynamic source")),
        None,
    );
    for stream in [false, true] {
        let (request, _) = service
            .build_main_message_request(
                MessagesCreateRequest::new(
                    "claude-sonnet-4-20250514",
                    None,
                    Some(source.clone()),
                    vec![ConversationMessage::user(
                        lingxi_core::types::MessageId::new(),
                        "hello".into(),
                    )],
                    vec![],
                ),
                stream,
            )
            .unwrap();
        let generated = service.client().prepare(&request).await.unwrap();
        let counted = service
            .client()
            .prepare_count_tokens(&request)
            .await
            .unwrap();
        let generated = &generated.provider_request.body_json;
        let text = counted.body_json.payload_at("/system/0/text").unwrap();
        assert_eq!(text, generated.payload_at("/system/0/text").unwrap());
        assert!(text.root_string_code_units().unwrap().starts_with(&[
            b'A' as u16,
            0xd800,
            b'B' as u16
        ]));
        crate::model::count_tokens::try_count_tokens_exact(
            service.client().as_ref(),
            transport.clone(),
            &request,
        )
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn non_messages_count_and_first_generation_admit_the_same_large_system() {
    use lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput;
    let (service, transport) = context_policy_fixture(100);
    let system = "large system ".repeat(200);
    let without = service
        .count_tokens("m", Some("context-policy-test"), None, vec![], vec![])
        .await
        .unwrap();
    let with = service
        .count_tokens(
            "m",
            Some("context-policy-test"),
            Some(&system),
            vec![],
            vec![],
        )
        .await
        .unwrap();
    assert!(with > without + 100);
    for stream in [false, true] {
        let source = SystemPromptInput::custom_prompt(PromptText::from_string(system.clone()));
        let (request, _) = service
            .build_main_message_request(
                MessagesCreateRequest::new(
                    "m",
                    Some("context-policy-test"),
                    Some(source.clone()),
                    vec![ConversationMessage::user(
                        lingxi_core::types::MessageId::new(),
                        "hello".into(),
                    )],
                    vec![],
                ),
                stream,
            )
            .unwrap();
        let error = if stream {
            match service.stream_request(request).await {
                Ok(_) => panic!("large system must exceed the input budget"),
                Err(error) => error,
            }
        } else {
            service
                .execute_non_stream_request(
                    request,
                    NonStreamingRequestClass::Main,
                    Default::default(),
                )
                .await
                .unwrap_err()
        };
        assert!(matches!(
            error,
            LlmError::InputBudgetExceeded {
                budget: 100,
                estimated: true,
                ..
            }
        ));
    }
    assert_eq!(transport.seen_count(), 0);
}

#[tokio::test]
async fn measured_context_rejects_a_changed_prompt_before_new_request_binding() {
    use crate::model::count_tokens::estimate_input_tokens;
    use lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput;
    let (service, transport) = context_policy_fixture(1000);
    assert_eq!(
        service
            .protocol_for_model("m", Some("context-policy-test"))
            .unwrap(),
        ProtocolFamily::OpenAiResponses,
    );
    *transport.responses.lock().unwrap() = vec![FakeResponse::Ok(ProviderResponse::json(
        200,
        serde_json::json!({
            "id":"measured", "object":"response", "model":"m", "status":"completed",
            "output":[{
                "type":"message", "id":"measured-message", "role":"assistant", "status":"completed",
                "content":[{"type":"output_text","text":"ok","annotations":[]}]
            }],
            "usage":{"input_tokens":8,"output_tokens":2,"total_tokens":10}
        }),
    ))];
    let original_history = vec![ConversationMessage::user(
        lingxi_core::types::MessageId::new(),
        "hello".into(),
    )];
    let system = SystemPromptInput::custom_prompt(PromptText::from_string("original system"));
    for stream in [false, true] {
        let build = |system: &SystemPromptInput, tools| {
            service
                .build_main_message_request(
                    MessagesCreateRequest::new(
                        "m",
                        Some("context-policy-test"),
                        Some(system.clone()),
                        original_history.clone(),
                        tools,
                    ),
                    stream,
                )
                .unwrap()
                .0
        };
        let original = build(&system, vec![]);
        let response = service
            .execute_non_stream_request(
                original,
                NonStreamingRequestClass::Main,
                Default::default(),
            )
            .await
            .unwrap();
        assert_eq!(response.usage.counts().input_tokens, 8);
        assert_eq!(response.usage.counts().output_tokens, 2);
        let measured_tokens = response.usage.context_tokens().unwrap();
        assert_eq!(measured_tokens, 10);
        let prefix: crate::context_usage::ContextUsagePrefix = serde_json::from_str(
            response
                .usage
                .provider_metadata
                .payload_at("/lingxi_context_usage_prefix")
                .unwrap()
                .as_json_source(),
        )
        .unwrap();
        let estimator =
            crate::context_usage::ContextUsageEstimator::with_prefix(move |_, _, history| {
                prefix
                    .matches_history(history)
                    .then(|| (measured_tokens, prefix.clone()))
            });
        let unchanged = estimator.scope(async { build(&system, vec![]) }).await;
        assert_eq!(estimate_input_tokens(&unchanged), measured_tokens);
        let tools = vec![crate::ToolDeclaration {
            name: "new_live_tool".into(),
            description: "schema ".repeat(1500).into(),
            input_schema: lingxi_llm_client::protocol::JsonPayload::parse("{\"type\":\"object\"}")
                .unwrap(),
            ..Default::default()
        }];
        let changed = estimator.scope(async { build(&system, tools) }).await;
        assert!(changed.execution.context_usage_estimate.is_none());
        assert!(estimate_input_tokens(&changed) > 1000);
        assert!(matches!(
            service.ensure_context_fit(&changed).await,
            Err(LlmError::InputBudgetExceeded { .. })
        ));
        let new_system = SystemPromptInput::custom_prompt(PromptText::from_string(
            "updated system ".repeat(1000),
        ));
        let changed_system = estimator.scope(async { build(&new_system, vec![]) }).await;
        assert!(changed_system.execution.context_usage_estimate.is_none());
        assert!(estimate_input_tokens(&changed_system) > 1000);
    }
}

#[tokio::test]
async fn batched_pooled_context_measurement_survives_route_selection_and_serialized_replay() {
    pooled_context_usage_survives_physical_account_selection(false).await;
}

#[tokio::test]
async fn streaming_pooled_context_measurement_survives_route_selection_and_serialized_replay() {
    pooled_context_usage_survives_physical_account_selection(true).await;
}

#[tokio::test]
async fn context_beta_refusal_isolated_between_physical_connections_nonstream() {
    let _lock = THINKING_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _restore = CacheEnvRestore::clear(&["CLAUDE_CODE_EXTRA_BODY"]);
    let transport = FakeTransport::sequence(vec![
        context_1m_refusal_response("context-1m-2025-08-07 beta is not supported"),
        FakeResponse::Ok(ProviderResponse::json(200, ok_response_json())),
        FakeResponse::Ok(ProviderResponse::json(200, ok_response_json())),
        FakeResponse::Ok(ProviderResponse::json(200, ok_response_json())),
    ]);
    let model = "claude-sonnet-4-20250514[1m]";
    let service = two_context_beta_connections(transport.clone(), model);
    let scope = crate::thinking_scope::ThinkingRecoveryScope::default();
    for profile in ["connection-a", "connection-b", "connection-a"] {
        let mut request = LlmRequest::new(model)
            .with_profile(profile)
            .with_user_text("hello");
        request.execution.thinking_recovery_scope = Some(scope.clone());
        service
            .execute_non_stream_request(request, NonStreamingRequestClass::Main, Default::default())
            .await
            .unwrap();
    }
    assert_eq!(transport.seen_count(), 4);
    for (index, expected) in [true, false, true, false].into_iter().enumerate() {
        assert_eq!(
            request_body_has_beta(&transport.seen_body(index), crate::model::betas::CONTEXT_1M),
            expected,
            "request {index}"
        );
    }
    let a = service
        .context_1m_route_key(&LlmRequest::new(model).with_profile("connection-a"))
        .unwrap();
    let b = service
        .context_1m_route_key(&LlmRequest::new(model).with_profile("connection-b"))
        .unwrap();
    assert!(scope.context_1m_refused_for(&a));
    assert!(!scope.context_1m_refused_for(&b));
    let mut other_endpoint = a.clone();
    other_endpoint.endpoint = "https://replacement.bedrock.example.test".into();
    assert!(!scope.context_1m_refused_for(&other_endpoint));
}

#[tokio::test]
async fn context_beta_refusal_isolated_between_physical_connections_stream() {
    use futures::StreamExt;
    let _lock = THINKING_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _restore = CacheEnvRestore::clear(&["CLAUDE_CODE_EXTRA_BODY"]);
    let model = "claude-sonnet-4-20250514[1m]";
    let mut responses = vec![FakeStreamResp::Status {
        status: 400,
        headers: BTreeMap::new(),
        body_json: Some(
            serde_json::json!({"error":{"type":"invalid_request_error","message":"context-1m-2025-08-07 beta is not supported"}}),
        ),
    }];
    responses.extend((0..3).map(|_| FakeStreamResp::Frames {
        status: 200,
        headers: BTreeMap::new(),
        frames: successful_bedrock_anthropic_frames(model),
    }));
    let transport = FakeStreamTransport::sequence(responses);
    let service = two_context_beta_connections(transport.clone(), model);
    let scope = crate::thinking_scope::ThinkingRecoveryScope::default();
    for profile in ["connection-a", "connection-b", "connection-a"] {
        let mut request = LlmRequest::new(model)
            .with_profile(profile)
            .with_user_text("hello");
        request.execution.thinking_recovery_scope = Some(scope.clone());
        let mut stream = service.stream_request(request).await.unwrap();
        while let Some(event) = stream.next().await {
            event.unwrap();
        }
    }
    assert_eq!(transport.stream_call_count(), 4);
    for (index, expected) in [true, false, true, false].into_iter().enumerate() {
        assert_eq!(
            request_body_has_beta(&transport.seen_body(index), crate::model::betas::CONTEXT_1M),
            expected,
            "request {index}"
        );
    }
}

#[tokio::test]
async fn context_policy_stops_oversized_custom_input_before_generation() {
    let (service, transport) = context_policy_fixture(100);
    let mut request = LlmRequest::new("m").with_user_text("x".repeat(1000));
    request.profile = Some("context-policy-test".into());
    let error = service
        .execute_non_stream_request(request, NonStreamingRequestClass::Main, Default::default())
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        LlmError::InputBudgetExceeded {
            budget: 100,
            estimated: true,
            ..
        }
    ));
    assert_eq!(transport.seen_count(), 0);
}

#[tokio::test]
async fn context_policy_measured_prefix_ignores_opaque_bytes_but_admits_new_input() {
    use crate::model::count_tokens::{approximate_tokens, estimate_input_tokens};
    use lingxi_core::types::{ContentBlock, MessageId};

    let (service, transport) = context_policy_fixture(272_000);
    let old = ConversationMessage::Assistant {
        id: MessageId::new(),
        content: vec![ContentBlock::ProviderContent {
            protocol: "open_ai_responses".into(),
            value: lingxi_core::types::utf16_json::Utf16JsonProjection::from_serde_value(
                &serde_json::json!({"type":"reasoning", "encrypted_content":"x".repeat(900_000)}),
            )
            .unwrap(),
        }],
        stop_reason: Some("end_turn".into()),
        per_turn_effort: None,
        resumable: None,
    };
    let estimator = crate::context_usage::ContextUsageEstimator::new(|model, profile, history| {
        assert_eq!((model, profile), ("m", Some("context-policy-test")));
        // Orchestrator tests cover extracting this measured baseline from
        // provider usage. Here exercise request building and admission.
        let new_tokens = history[1..]
            .iter()
            .flat_map(|message| match message {
                ConversationMessage::User { content, .. } => content.as_slice(),
                _ => &[],
            })
            .map(|block| match block {
                ContentBlock::Text { text, .. } => text.len() as u64 / 4,
                _ => 0,
            })
            .sum::<u64>();
        Some(127_967 + new_tokens)
    });
    for stream in [false, true] {
        let make_request = |text: String| {
            MessagesCreateRequest::new(
                "m",
                Some("context-policy-test"),
                None,
                vec![
                    old.clone(),
                    ConversationMessage::user(MessageId::new(), text),
                ],
                vec![],
            )
        };
        let (request, _) = estimator
            .scope(async {
                service.build_main_message_request(make_request("tailtail".into()), stream)
            })
            .await
            .unwrap();
        assert!(approximate_tokens(&request) > 272_000);
        assert_eq!(estimate_input_tokens(&request), 127_969);
        service.ensure_context_fit(&request).await.unwrap();

        let (larger, _) = estimator
            .scope(async {
                service.build_main_message_request(make_request("x".repeat(600_000)), stream)
            })
            .await
            .unwrap();
        assert_eq!(estimate_input_tokens(&larger), 277_967);
        assert!(matches!(
            service.ensure_context_fit(&larger).await,
            Err(LlmError::InputBudgetExceeded {
                input_tokens: 277_967,
                budget: 272_000,
                ..
            })
        ));

        let (unmeasured, _) = service
            .build_main_message_request(make_request("tailtail".into()), stream)
            .unwrap();
        assert!(matches!(
            service.ensure_context_fit(&unmeasured).await,
            Err(LlmError::InputBudgetExceeded { .. })
        ));
    }
    assert_eq!(
        transport.seen_count(),
        0,
        "no generation or counting calls for local admission"
    );
}

#[tokio::test]
async fn context_policy_fresh_provider_count_overrides_measured_prefix_estimate() {
    let (service, transport, mut request) = context_policy_count_fixture(FakeResponse::Ok(
        ProviderResponse::json(200, serde_json::json!({"input_tokens":9000})),
    ));
    request.execution.context_usage_estimate =
        crate::context_usage::ContextUsageEstimate::bind(&request, 7400);
    assert!(matches!(
        service.ensure_context_fit(&request).await,
        Err(LlmError::ContextOverflow { .. })
    ));
    assert_eq!(transport.seen_count(), 1);
}

#[tokio::test]
async fn context_policy_missing_count_endpoint_still_dispatches_generation() {
    for status in [404, 405, 501] {
        let (service, transport, request) = context_policy_count_fixture(FakeResponse::Ok(
            ProviderResponse::json(
                status,
                serde_json::json!({"error":{"type":"not_found_error","message":"count_tokens endpoint is not implemented"}}),
            ),
        ));
        service
            .execute_non_stream_request(request, NonStreamingRequestClass::Main, Default::default())
            .await
            .unwrap();
        let seen = transport.seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "HTTP {status}");
        assert!(url::Url::parse(&seen[0].url)
            .unwrap()
            .path()
            .ends_with("/messages/count_tokens"));
        assert!(url::Url::parse(&seen[1].url)
            .unwrap()
            .path()
            .ends_with("/messages"));
        assert_eq!(
            seen[1].body_json.unsigned_integer_at("/max_tokens"),
            Some(512)
        );
    }
}

#[tokio::test]
async fn context_policy_count_failures_preserve_auth_and_transport_errors() {
    for (status, kind) in [
        (401, "authentication_error"),
        (403, "permission_error"),
        (429, "rate_limit_error"),
        (500, "api_error"),
    ] {
        let (service, transport, request) =
            context_policy_count_fixture(FakeResponse::Ok(ProviderResponse::json(
                status,
                serde_json::json!({"error":{"type":kind,"message":"count rejected"}}),
            )));
        let error = service.ensure_context_fit(&request).await.unwrap_err();
        assert_eq!(error.http_status(), Some(status));
        assert_eq!(transport.seen_count(), 1);
    }
    for error in [
        LlmError::tls_cert("CERT_HAS_EXPIRED"),
        LlmError::TransportTimeout {
            message: "count timed out".into(),
        },
    ] {
        let (service, transport, request) =
            context_policy_count_fixture(FakeResponse::Err(error.clone()));
        let actual = service.ensure_context_fit(&request).await.unwrap_err();
        // FixtureTransport projects host errors through the SDK transport,
        // which may enrich diagnostic text; their failure class must survive.
        assert_eq!(
            std::mem::discriminant(actual.classification_source()),
            std::mem::discriminant(&error)
        );
        assert_eq!(transport.seen_count(), 1);
    }
}

#[test]
fn output_recovery_grows_explicit_budget_without_crossing_route_limit() {
    let mut provider = crate::builtin_presets()
        .providers
        .into_iter()
        .find(|p| p.profile_name == "deepseek")
        .unwrap();
    provider.connection = Default::default();
    provider.auth = crate::AuthStrategy::None;
    provider.credential = crate::CredentialConfig::None;
    provider.models.truncate(1);
    let model = provider.models[0].request_model.clone();
    provider.models[0].metadata.max_output_tokens = Some(16000);
    provider.models[0].metadata.context_window_tokens = Some(1000000);
    let client = crate::ModelRuntime::from_config(crate::ClientConfig {
        providers: vec![provider],
    })
    .unwrap();
    let service = ApiService::new(
        Arc::new(client),
        FakeTransport::always(ProviderResponse::json(200, serde_json::json!({}))),
        SubscriberState::default(),
        UserAgentEnv::default(),
        "test",
        None,
        None,
    );
    let mut usage = crate::ExecutionUsage::from_counts(lingxi_llm_client::protocol::Usage {
        input_tokens: 1000,
        output_tokens: 8000,
        ..Default::default()
    });
    usage.provider_metadata = provider_payload(serde_json::json!({"llm_client": {
        "executed_profile":"deepseek", "output_budget":{"source":"explicit", "value":8000}
    }}));
    assert_eq!(
        service
            .next_output_budget(&model, Some("deepseek"), &usage)
            .unwrap(),
        Some(16000)
    );
    usage.provider_metadata = provider_payload(serde_json::json!({"llm_client": {
        "executed_profile":"deepseek", "output_budget":{"source":"explicit", "value":16000}
    }}));
    assert_eq!(
        service
            .next_output_budget(&model, Some("deepseek"), &usage)
            .unwrap(),
        None
    );
    usage.provider_metadata = provider_payload(serde_json::json!({"llm_client": {
        "executed_profile":"another-route", "output_budget":{"source":"explicit", "value":8000}
    }}));
    assert_eq!(
        service
            .next_output_budget(&model, Some("deepseek"), &usage)
            .unwrap(),
        None
    );
}

#[test]
fn native_model_option_overrides_follow_gd_provider_gate() {
    assert!(!ApiService::native_model_option_overrides_allowed(
        &crate::ProviderId::AnthropicFirstParty
    ));
    assert!(!ApiService::native_model_option_overrides_allowed(
        &crate::ProviderId::Custom {
            name: "gateway".into(),
        }
    ));
    assert!(!ApiService::native_model_option_overrides_allowed(
        &crate::ProviderId::Custom {
            name: "anthropicAws".into(),
        }
    ));
    assert!(!ApiService::native_model_option_overrides_allowed(
        &crate::ProviderId::OpenAICompatible {
            name: "anthropicGoogleCloud".into(),
        }
    ));
    for provider_id in [
        crate::ProviderId::FoundryClaude,
        crate::ProviderId::VertexClaude,
        crate::ProviderId::BedrockClaude,
    ] {
        assert!(ApiService::native_model_option_overrides_allowed(
            &provider_id
        ));
    }
}

#[tokio::test]
async fn main_request_preserves_source_thinking_type_before_wire_projection() {
    use crate::model::thinking::ThinkingConfig;

    for (thinking, expected) in [
        (ThinkingConfig::Disabled, Some(true)),
        (ThinkingConfig::Adaptive, Some(false)),
        // Automatic has no explicit Native thinking type. Applying Main
        // policy preserves that absence while retaining the base snapshot.
        (ThinkingConfig::Automatic, None),
    ] {
        let service = make_adapter_for_protocol_with_transport(
            ProtocolFamily::AnthropicMessages,
            ProviderId::AnthropicFirstParty,
            "https://api.anthropic.com",
            "native",
            "claude-opus-4-6",
            FakeTransport::always(ProviderResponse::json(200, ok_response_json())),
        )
        .with_thinking(thinking);
        let mut main = MessagesCreateRequest::new(
            "claude-opus-4-6",
            None,
            None,
            vec![text_user_msg("hello")],
            vec![],
        );
        main.opts.query_source = Some("sdk".into());
        let mut request = service
            .build_main_message_request(main, false)
            .expect("build a StandardMain request from the captured session config")
            .0;
        assert_eq!(
            request
                .execution
                .native_thinking_state_snapshot
                .map(|state| state.base()),
            Some(thinking),
            "the main request captures the session base before native preparation"
        );
        service
            .apply_native_main_thinking_policy(&mut request)
            .expect("apply the Native Main policy before SDK preparation");
        assert_eq!(
            request.execution.native_thinking_request_type_disabled,
            expected
        );
        assert_eq!(
            request.execution.native_thinking_mechanically_disabled,
            Some(false)
        );
        let prepared = service
            .client
            .prepare(&request)
            .await
            .expect("prepare the request without losing source thinking type");
        assert_eq!(prepared.native_thinking_request_type_disabled, expected);
        assert_eq!(prepared.native_thinking_mechanically_disabled, Some(false));

        let utility_request = service
            .build_request(
                "claude-opus-4-6",
                None,
                None,
                vec![text_user_msg("utility query")],
                vec![],
                false,
                None,
                false,
                PromptCacheQuerySource::Unspecified,
            )
            .expect("build a utility request without claiming StandardMain provenance");
        assert_eq!(
            utility_request
                .execution
                .native_thinking_request_type_disabled,
            None
        );
        assert_eq!(
            utility_request
                .execution
                .native_thinking_mechanically_disabled,
            None
        );

        let scheduled_request = service
            .build_scheduled_request(
                MessagesCreateRequest::new(
                    "claude-opus-4-6",
                    None,
                    None,
                    vec![text_user_msg("scheduled turn")],
                    vec![],
                ),
                ThinkingConfig::Disabled,
                None,
            )
            .expect("scheduled requests use their own thinking source");
        assert_eq!(
            scheduled_request
                .execution
                .native_thinking_request_type_disabled,
            None
        );
        assert_eq!(
            scheduled_request
                .execution
                .native_thinking_mechanically_disabled,
            None
        );
    }
}

#[test]
fn main_request_retains_scalar_shape_sources_through_adjacent_user_merge() {
    use lingxi_core::types::MessageId;
    use std::collections::HashSet;

    let service = make_adapter_for_protocol_with_transport(
        ProtocolFamily::AnthropicMessages,
        ProviderId::AnthropicFirstParty,
        "https://api.anthropic.com",
        "native",
        "claude-opus-4-6",
        FakeTransport::always(ProviderResponse::json(200, ok_response_json())),
    );
    let scalar = MessageId::new();
    let array = MessageId::new();
    let merged_scalar = MessageId::new();
    let merged_array = MessageId::new();
    let assistant = |text: &str| ConversationMessage::Assistant {
        per_turn_effort: None,
        id: MessageId::new(),
        content: vec![lingxi_core::types::ContentBlock::Text {
            text: text.to_owned(),
            citations: None,
        }],
        stop_reason: Some("end_turn".into()),
        resumable: None,
    };
    let mut main = MessagesCreateRequest::new(
        "claude-opus-4-6",
        None,
        None,
        vec![
            ConversationMessage::user(scalar, "same text".into()),
            assistant("first response"),
            ConversationMessage::user(array, "same text".into()),
            assistant("second response"),
            ConversationMessage::user(merged_scalar, "scalar source".into()),
            ConversationMessage::user(merged_array, "array source".into()),
        ],
        vec![],
    );
    main.opts.scalar_user_content_ids = HashSet::from([scalar, merged_scalar]);
    let request = service.build_main_message_request(main, false).unwrap().0;
    assert_eq!(
        request.execution.scalar_user_content_ids,
        HashSet::from([scalar, merged_scalar])
    );
    assert_eq!(request.input.messages.len(), 5);
    assert_eq!(
        request.execution.request_content_source_ids[0],
        vec![HashSet::from([scalar])]
    );
    assert_eq!(
        request.execution.request_content_source_ids[2],
        vec![HashSet::from([array])]
    );
    assert_eq!(request.input.messages[4].content.len(), 2);
    assert_eq!(
        request.execution.request_content_source_ids[4],
        vec![
            HashSet::from([merged_scalar]),
            HashSet::from([merged_array])
        ]
    );
}
