use super::*;

#[tokio::test]
async fn model_validation_uses_host_base_url_assumption_for_context_retry_gate() {
    let _env_lock = CACHE_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _restore = CacheEnvRestore(vec![
        ("ANTHROPIC_BASE_URL", std::env::var_os("ANTHROPIC_BASE_URL")),
        (
            "_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL",
            std::env::var_os("_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL"),
        ),
    ]);
    std::env::set_var("ANTHROPIC_BASE_URL", "https://proxy.example.test");
    std::env::remove_var("_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL");
    let transport = FakeTransport::sequence(vec![
        FakeResponse::Ok(ProviderResponse::json(
            400,
            serde_json::json!({
                "type": "error",
                "error": {
                    "type": "invalid_request_error",
                    "message": "The context-1m-2025-08-07 beta is not supported"
                }
            }),
        )),
        FakeResponse::Ok(ProviderResponse::json(200, ok_response_json())),
    ]);
    let prefix = lingxi_llm_client::providers::anthropic::system_prompt::NativeSystemPrefix {
        compatibility_version: "2.1.295".into(),
        entrypoint: "cli".into(),
        host_identity: PromptText::from_string("LingXi"),
        attribution_enabled: false,
        assume_first_party_base_url: true,
    };
    let service = make_adapter_for_protocol_with_transport(
        ProtocolFamily::AnthropicMessages,
        ProviderId::AnthropicFirstParty,
        "https://proxy.example.test",
        "proxy",
        "claude-sonnet-4-20250514",
        transport.clone(),
    )
    .with_native_system_prefix(prefix)
    .with_custom_cli_betas(vec![
        crate::model::context_window::CONTEXT_1M_BETA_HEADER.to_string()
    ]);
    let mut request = LlmRequest::new("claude-sonnet-4-20250514").with_user_text("Hi");
    request.profile = Some("proxy".into());
    request.input.max_tokens = Some(1);

    assert!(service
        .execute_model_validation_request(request)
        .await
        .is_err());
    assert_eq!(transport.seen_count(), 1);
}

#[tokio::test]
async fn model_validation_refreshes_captured_anthropic_oauth_on_native_revocation_403() {
    let transport = FakeTransport::sequence(vec![
        FakeResponse::Ok(ProviderResponse::json(
            403,
            serde_json::json!({
                "type": "error",
                "error": {
                    "type": "authentication_error",
                    "message": "OAuth token has been revoked"
                }
            }),
        )),
        FakeResponse::Ok(ProviderResponse::json(200, ok_response_json())),
    ]);
    let provider = Arc::new(RotatingAnthropicOAuthProvider::new());
    let client = crate::ModelRuntime::from_config(ClientConfig {
        providers: vec![ProviderProfile {
            wire_profile: None,
            regions: crate::Region::all(),
            provider_id: ProviderId::AnthropicFirstParty,
            profile_name: "anthropic-oauth".into(),
            base_url: "https://api.anthropic.com".into(),
            protocol: ProtocolFamily::AnthropicMessages,
            auth: AuthStrategy::OAuthBearer,
            credential: CredentialConfig::HostManaged {
                id: "anthropic-oauth".into(),
            },
            models: vec![ModelProfile {
                display_model: "claude-sonnet-4-20250514".into(),
                request_model: "claude-sonnet-4-20250514".into(),
                billing_model: "claude-sonnet-4".into(),
                aliases: Vec::new(),
                description: None,
                metadata: Default::default(),
                capabilities: Capabilities {
                    streaming: true,
                    tools: true,
                    ..Default::default()
                },
            }],
            pricing: PricingConfig::default(),
            signing: None,
            azure: None,
            supports_websockets: false,
            supports_websocket_compression: false,
            websocket_connect_timeout_ms: None,
            vision_delegate: None,
            connection: Default::default(),
        }],
    })
    .expect("OAuth client")
    .with_credential_provider(provider.clone());
    let service = ApiService::new(
        Arc::new(client),
        transport.clone(),
        SubscriberState::default(),
        UserAgentEnv {
            user_type: Some("external".into()),
            entrypoint: Some("cli".into()),
            ..Default::default()
        },
        "0.0.0",
        None,
        None,
    );
    let mut request = LlmRequest::new("claude-sonnet-4-20250514").with_user_text("Hi");
    request.profile = Some("anthropic-oauth".into());
    request.input.max_tokens = Some(1);

    service
        .execute_model_validation_request(request)
        .await
        .expect("one rejected OAuth credential is refreshed and retried");
    assert_eq!(transport.seen_count(), 2);
    assert_eq!(
        provider.refreshes.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    let authorization = |index| {
        transport
            .seen_headers(index)
            .into_iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
            .map(|(_, value)| value)
    };
    assert_eq!(authorization(0).as_deref(), Some("Bearer rejected-token"));
    assert_eq!(authorization(1).as_deref(), Some("Bearer refreshed-token"));
}

#[tokio::test]
async fn model_validation_strips_deferred_foundry_features_and_keeps_local_search() {
    let model = "claude-opus-4-7";
    let source_units = model.encode_utf16().collect::<Vec<_>>();
    let unsupported = ProviderResponse::json(
        400,
        serde_json::json!({
            "error": {
                "message": "tool_search_server, structured_outputs not supported in your workspace"
            }
        }),
    );
    let transport = FakeTransport::sequence(vec![
        FakeResponse::Ok(unsupported),
        FakeResponse::Ok(ProviderResponse::json(200, ok_response_json())),
    ]);
    let endpoint = "https://foundry.services.ai.azure.com/anthropic";
    let wire_profile: lingxi_llm_client::protocol::ProviderProfile =
        serde_json::from_value(serde_json::json!({
            "provider_id": "foundry",
            "profile_name": "p",
            "protocol": "foundry_claude",
            "base_url": endpoint,
            "auth": "none",
            "regions": ["international"],
            "models": [{
                "display_model": model,
                "request_model": model,
                "billing_model": model,
                "foundry": {"hosting": "anthropic", "model_id": model}
            }]
        }))
        .expect("Foundry wire profile carries explicit supported deployment facts");
    let client = Arc::new(
        ModelRuntime::from_config(ClientConfig {
            providers: vec![ProviderProfile {
                wire_profile: Some(wire_profile),
                regions: crate::Region::all(),
                provider_id: ProviderId::FoundryClaude,
                profile_name: "p".to_owned(),
                base_url: endpoint.to_owned(),
                protocol: ProtocolFamily::FoundryClaude,
                auth: AuthStrategy::None,
                credential: CredentialConfig::None,
                models: vec![ModelProfile {
                    display_model: model.to_owned(),
                    request_model: model.to_owned(),
                    billing_model: model.to_owned(),
                    aliases: Vec::new(),
                    description: None,
                    metadata: Default::default(),
                    capabilities: Capabilities {
                        streaming: true,
                        tools: true,
                        reasoning: true,
                        structured_output: true,
                        ..Default::default()
                    },
                }],
                pricing: PricingConfig::default(),
                signing: None,
                azure: None,
                supports_websockets: false,
                supports_websocket_compression: false,
                websocket_connect_timeout_ms: None,
                vision_delegate: None,
                connection: Default::default(),
            }],
        })
        .expect("Foundry test client"),
    );
    let service = ApiService::new(
        client,
        transport.clone(),
        SubscriberState::default(),
        UserAgentEnv {
            user_type: Some("external".to_owned()),
            entrypoint: Some("cli".to_owned()),
            ..Default::default()
        },
        "0.0.0",
        None,
        None,
    );
    let mut request = LlmRequest::new(model).with_user_text("Hi");
    request.profile = Some("p".to_owned());
    request.input.max_tokens = Some(1);
    request.input.model_source =
        Some(lingxi_llm_client::protocol::JsonPayload::string_from_utf16_code_units(&source_units));
    let schema =
        lingxi_llm_client::protocol::JsonPayload::from_generated_value(&serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }))
        .unwrap();
    request.input.tools = vec![
        lingxi_llm_client::protocol::ToolSpec {
            name: NATIVE_TOOL_SEARCH_NAME.to_owned(),
            description: lingxi_llm_client::protocol::JsonPayload::string_from_utf16_code_units(
                &NATIVE_TOOL_SEARCH_DESCRIPTION
                    .encode_utf16()
                    .collect::<Vec<_>>(),
            ),
            input_schema: schema.clone(),
            strict: false,
            defer_loading: false,
            native_options: Vec::new(),
            tool_type: None,
            extra: Default::default(),
        },
        lingxi_llm_client::protocol::ToolSpec {
            name: NATIVE_DEFERRED_TOOL_PLACEHOLDER_NAME.to_owned(),
            description: lingxi_llm_client::protocol::JsonPayload::string_from_utf16_code_units(
                &NATIVE_DEFERRED_TOOL_PLACEHOLDER_DESCRIPTION
                    .encode_utf16()
                    .collect::<Vec<_>>(),
            ),
            input_schema: schema.clone(),
            strict: false,
            defer_loading: true,
            native_options: Vec::new(),
            tool_type: None,
            extra: Default::default(),
        },
        lingxi_llm_client::protocol::ToolSpec {
            name: "DeferredRead".to_owned(),
            description: lingxi_llm_client::protocol::JsonPayload::string_from_utf16_code_units(
                &"Read a deferred file".encode_utf16().collect::<Vec<_>>(),
            ),
            input_schema: schema.clone(),
            strict: false,
            defer_loading: true,
            native_options: Vec::new(),
            tool_type: None,
            extra: Default::default(),
        },
        lingxi_llm_client::protocol::ToolSpec {
            name: "StrictOutput".to_owned(),
            description: lingxi_llm_client::protocol::JsonPayload::string_from_utf16_code_units(
                &"Return strict output".encode_utf16().collect::<Vec<_>>(),
            ),
            input_schema: schema,
            strict: true,
            defer_loading: false,
            native_options: Vec::new(),
            tool_type: None,
            extra: Default::default(),
        },
    ];
    service
        .execute_model_validation_request(request)
        .await
        .expect("retry without the Native-rejected deferred tool");
    assert_eq!(transport.seen_count(), 2);
    let seen = transport.seen.lock().unwrap();
    let first = request_body_value(&seen[0].body_json);
    let second = request_body_value(&seen[1].body_json);
    let first_tools = first["tools"].as_array().unwrap();
    assert_eq!(first_tools.len(), 4);
    assert_eq!(first_tools[0]["name"], NATIVE_TOOL_SEARCH_NAME);
    assert_ne!(first_tools[0]["defer_loading"].as_bool(), Some(true));
    assert_eq!(
        first_tools[1]["name"],
        NATIVE_DEFERRED_TOOL_PLACEHOLDER_NAME
    );
    assert!(first_tools[1]["defer_loading"].as_bool().unwrap());
    assert_eq!(first_tools[2]["name"], "DeferredRead");
    assert!(first_tools[2]["defer_loading"].as_bool().unwrap());
    assert_eq!(first_tools[3]["name"], "StrictOutput");
    assert!(first_tools[3]["strict"].as_bool().unwrap());

    let second_tools = second["tools"].as_array().unwrap();
    assert_eq!(second_tools.len(), 3);
    assert!(second_tools
        .iter()
        .all(|tool| tool["name"] != NATIVE_DEFERRED_TOOL_PLACEHOLDER_NAME));
    let tool_search = second_tools
        .iter()
        .find(|tool| tool["name"] == NATIVE_TOOL_SEARCH_NAME)
        .expect("real ToolSearch remains after the placeholder is removed");
    assert_ne!(tool_search["defer_loading"].as_bool(), Some(true));
    let deferred_read = second_tools
        .iter()
        .find(|tool| tool["name"] == "DeferredRead")
        .expect("ordinary deferred tool remains after unsupported search is removed");
    assert!(deferred_read.get("defer_loading").is_none());
    let strict_output = second_tools
        .iter()
        .find(|tool| tool["name"] == "StrictOutput")
        .expect("strict tool remains after unsupported strict mode is removed");
    assert!(strict_output.get("strict").is_none());
}

#[test]
fn native_v_oauth_revocation_uses_status_specific_exact_substrings() {
    assert!(oauth_token_revoked(
        403,
        Some("OAuth token has been revoked for this account")
    ));
    assert!(oauth_token_revoked(
        401,
        Some("OAuth access token has been revoked")
    ));
    assert!(!oauth_token_revoked(
        403,
        Some("OAuth access token has been revoked")
    ));
    assert!(!oauth_token_revoked(
        401,
        Some("oauth access token has been revoked")
    ));
    assert!(!oauth_token_revoked(403, None));
}

#[test]
fn foundry_tool_search_cache_is_negative_only_and_route_scoped() {
    let model_units = "model".encode_utf16().collect::<Vec<_>>();
    let formatted_model_units = "model[1m][2M]-[1m]".encode_utf16().collect::<Vec<_>>();
    assert_eq!(
        native_foundry_model_key(&formatted_model_units),
        "model-".encode_utf16().collect::<Vec<_>>()
    );
    assert_eq!(
        foundry_unsupported_feature_names(
            "tool_search_server, tool_search not supported in your workspace"
        ),
        Some(vec!["tool_search_server".into(), "tool_search".into()])
    );
    assert_eq!(
        foundry_unsupported_feature_names(
            "Features are not available for Azure AI Foundry workspaces: tool_search_server and tool_search"
        ),
        Some(vec!["tool_search_server".into(), "tool_search".into()])
    );
    assert_eq!(
        foundry_unsupported_feature_names(
            "TOOL_SEARCH_SERVER, tool_search not supported in your workspace"
        ),
        None,
        "Jg rejects the whole captured list when any token is not lowercase ASCII"
    );
    assert_eq!(
        foundry_unsupported_feature_names(
            "features are not available for Azure AI Foundry workspaces: TOOL_SEARCH_SERVER and tool_search"
        ),
        Some(vec!["tool_search".into()]),
        "Xg drops invalid tokens individually after its different split rule"
    );
    assert_eq!(
        foundry_unsupported_feature_names(
            "Server-side web search is not available in this environment"
        ),
        Some(vec!["web_search".into()])
    );
    assert_eq!(foundry_unsupported_feature_names("unrelated 400"), None);

    let service = make_adapter_for_protocol(
        ProtocolFamily::FoundryClaude,
        ProviderId::FoundryClaude,
        "https://foundry.example/anthropic/",
    );
    assert_eq!(
        service.foundry_tool_search_supported("model", Some("p"), Some(&model_units)),
        Some(true),
        "an unknown deployment is allowed"
    );
    let no_observation = provider_payload(serde_json::json!({
        "error": {"message": "unrelated 400"}
    }));
    service.record_foundry_capability_observation(
        "model",
        &model_units,
        "p",
        ProtocolFamily::FoundryClaude,
        &ProviderId::FoundryClaude,
        400,
        &no_observation,
        None,
    );
    assert_eq!(
        service.foundry_tool_search_supported("model", Some("p"), Some(&model_units)),
        Some(true),
        "unrelated 400s do not poison the capability cache"
    );
    let other_source_units = "custom-unlisted-model".encode_utf16().collect::<Vec<_>>();
    assert_eq!(
        service.foundry_tool_search_supported("model", Some("p"), Some(&other_source_units)),
        Some(true),
        "the route anchor alone does not inherit another Native model source's cache"
    );
    let unsupported = provider_payload(serde_json::json!({
        "error": {"message": "tool_search_server not supported in your workspace"}
    }));
    service.record_foundry_capability_observation(
        "model",
        &model_units,
        "p",
        ProtocolFamily::FoundryClaude,
        &ProviderId::FoundryClaude,
        403,
        &unsupported,
        None,
    );
    assert_eq!(
        service.foundry_tool_search_supported("model", Some("p"), Some(&model_units)),
        Some(true),
        "only source-qualified HTTP 400 observations count"
    );
    service.record_foundry_capability_observation(
        "model",
        &model_units,
        "p",
        ProtocolFamily::FoundryClaude,
        &ProviderId::FoundryClaude,
        400,
        &unsupported,
        None,
    );
    assert_eq!(
        service.foundry_tool_search_supported("model", Some("p"), Some(&model_units)),
        Some(false)
    );
    assert_eq!(
        service.foundry_tool_search_supported("model", Some("p"), Some(&other_source_units)),
        Some(true),
        "an unsupported observation is keyed by the exact Native model source"
    );
    let other_deployment = make_adapter_for_protocol(
        ProtocolFamily::FoundryClaude,
        ProviderId::FoundryClaude,
        "https://other-foundry.example/anthropic/",
    );
    assert_eq!(
        other_deployment.foundry_tool_search_supported("model", Some("p"), Some(&model_units)),
        Some(true),
        "negative capability observations are deployment scoped"
    );
    assert_eq!(
        service.model_validation_account_generation(),
        service.anthropic.fast_availability.generation()
    );
}

#[tokio::test(start_paused = true)]
async fn classifier_attempt_deadline_uses_trusted_override_and_preserves_default() {
    for body in [false, true] {
        for override_ms in [None, Some(17)] {
            let service =
                make_adapter_with_retries(Arc::new(StalledNonStreamTransport { body }), Some(0));
            let probe = Arc::new(AttemptProbe::default());
            service.set_model_attempt_hooks(Arc::new(probe.clone()));
            let mut request = registered_request();
            request.execution.classifier_attempt_timeout_ms = override_ms;
            let expected = override_ms
                .map(Duration::from_millis)
                .unwrap_or_else(|| crate::model::request_timeout::non_stream_timeout().unwrap());
            let started = tokio::time::Instant::now();
            let result = tokio::time::timeout(
                expected + Duration::from_secs(1),
                service.execute_classifier_request(request, 0),
            )
            .await;
            assert!(
                matches!(result, Ok(Err(LlmError::TransportTimeout { .. }))),
                "{result:?}"
            );
            assert_eq!(started.elapsed(), expected);
            assert_eq!(
                *probe.events.lock().unwrap(),
                vec!["begin", "dispatch", "finish-owned", "settled"]
            );
        }
    }
}

#[tokio::test(start_paused = true)]
async fn classifier_stream_deadline_bounds_headers_body_heartbeats_and_error_body() {
    use futures::StreamExt;
    for protocol in [
        ProtocolFamily::AnthropicMessages,
        ProtocolFamily::OpenAiResponses,
    ] {
        for phase in 0..4 {
            let mut service = make_adapter_for_protocol_with_transport(
                protocol,
                ProviderId::Custom {
                    name: "classifier-test".into(),
                },
                "https://example.test",
                "p",
                "model",
                Arc::new(ClassifierStreamTransport { phase }),
            );
            service.settings_max_retries = Some(0);
            let probe = Arc::new(AttemptProbe::default());
            service.set_model_attempt_hooks(Arc::new(probe.clone()));
            let mut request = registered_request();
            request.input.model = "model".into();
            request.profile = Some("p".into());
            request.execution.isolated_query = true;
            request.execution.query_source = Some("auto_mode".into());
            request.execution.classifier_attempt_timeout_ms = Some(17);
            let started = tokio::time::Instant::now();
            let result = tokio::time::timeout(Duration::from_millis(100), async {
                let mut stream = service.stream_request(request).await?;
                while let Some(event) = stream.next().await {
                    event?;
                }
                Ok::<(), LlmError>(())
            })
            .await;
            assert!(
                matches!(result, Ok(Err(LlmError::TransportTimeout { .. }))),
                "{protocol:?} phase={phase}: {result:?}"
            );
            assert_eq!(
                started.elapsed(),
                Duration::from_millis(17),
                "{protocol:?} phase={phase}"
            );
            assert_eq!(
                *probe.events.lock().unwrap(),
                vec!["begin", "dispatch", "finish-owned", "settled"]
            );
        }
    }
}

#[tokio::test(start_paused = true)]
async fn classifier_stream_retries_bodies_and_headers_with_one_shared_budget() {
    use std::sync::atomic::Ordering;
    for failures in [
        vec![0],
        vec![1],
        vec![2],
        vec![3],
        vec![4],
        vec![0, 1],
        vec![1, 0],
        vec![0, 3],
        vec![3, 0],
    ] {
        let transport = Arc::new(ClassifierRetryTransport {
            failures: failures.clone(),
            sent: Default::default(),
        });
        let mut service = make_adapter_for_protocol_with_transport(
            ProtocolFamily::OpenAiResponses,
            ProviderId::Custom {
                name: "classifier-test".into(),
            },
            "https://example.test",
            "p",
            "model",
            transport.clone(),
        );
        // Classifier-owned retry control wins over an ordinary zero-retry setting.
        service.settings_max_retries = Some(0);
        let probe = Arc::new(AttemptProbe::default());
        service.set_model_attempt_hooks(Arc::new(probe.clone()));
        let mut request = registered_request();
        request.input.model = "model".into();
        request.profile = Some("p".into());
        request.execution.query_source = Some("auto_mode".into());
        request.execution.classifier_attempt_timeout_ms = Some(17);
        let response = tokio::time::timeout(
            Duration::from_secs(10),
            service.execute_classifier_stream_request(request, 1),
        )
        .await
        .unwrap();
        assert_eq!(transport.sent.load(Ordering::SeqCst), 2, "{failures:?}");
        if failures.len() == 1 {
            let response = response.unwrap();
            let text: String = response
                .content
                .iter()
                .filter_map(|block| match block {
                    crate::history::ContentBlock::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(
                text, "<block>no",
                "failed partial output must not leak into the retry"
            );
        } else {
            assert!(
                matches!(response, Err(LlmError::TransportTimeout { .. })),
                "{failures:?}: {response:?}"
            );
        }
        let events = probe.events.lock().unwrap();
        assert_eq!(
            events.iter().filter(|event| **event == "dispatch").count(),
            2
        );
        assert_eq!(
            events.iter().filter(|event| **event == "settled").count(),
            2
        );
    }
}

#[tokio::test(start_paused = true)]
async fn classifier_stream_preserves_terminal_header_retry_decisions() {
    use std::sync::atomic::Ordering;
    for phase in [5, 6] {
        let transport = Arc::new(ClassifierRetryTransport {
            failures: vec![phase],
            sent: Default::default(),
        });
        let service = make_adapter_for_protocol_with_transport(
            ProtocolFamily::OpenAiResponses,
            ProviderId::Custom {
                name: "classifier-test".into(),
            },
            "https://example.test",
            "p",
            "model",
            transport.clone(),
        );
        let mut request = LlmRequest::new("model").with_user_text("classify");
        request.profile = Some("p".into());
        request.input.max_tokens = Some(100);
        request.execution.query_source = Some("auto_mode".into());
        request.execution.classifier_attempt_timeout_ms = Some(17);
        let error = service
            .execute_classifier_stream_request(request, 3)
            .await
            .unwrap_err();
        if phase == 5 {
            assert!(
                matches!(error.classification_source(), LlmError::ProviderInternal),
                "{error:?}"
            );
        } else {
            assert!(
                matches!(error, LlmError::TransportTimeout { .. }),
                "{error:?}"
            );
        }
        assert_eq!(transport.sent.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test(start_paused = true)]
async fn nonstream_deadline_bounds_headers_and_body_and_settles_once() {
    for body in [false, true] {
        let service =
            make_adapter_with_retries(Arc::new(StalledNonStreamTransport { body }), Some(0));
        let probe = Arc::new(AttemptProbe::default());
        service.set_model_attempt_hooks(Arc::new(probe.clone()));
        let result = tokio::time::timeout(
            crate::model::request_timeout::non_stream_timeout().unwrap() + Duration::from_secs(1),
            service.execute_side_query_request(registered_request()),
        )
        .await;
        assert!(
            matches!(result, Ok(Err(LlmError::TransportTimeout { .. }))),
            "{result:?}"
        );
        assert_eq!(
            *probe.events.lock().unwrap(),
            vec!["begin", "dispatch", "finish-owned", "settled"]
        );
    }
}

#[tokio::test(start_paused = true)]
async fn nonstream_deadline_excludes_host_admission() {
    let transport = FakeTransport::always(ProviderResponse {
        status: 200,
        headers: BTreeMap::new(),
        body_json: provider_payload(ok_response_json()),
        request_id: None,
    });
    let service = make_adapter_with_retries(transport, Some(0));
    let probe = Arc::new(AttemptProbe {
        admission_delay: crate::model::request_timeout::non_stream_timeout().unwrap()
            + Duration::from_secs(1),
        ..Default::default()
    });
    service.set_model_attempt_hooks(Arc::new(probe.clone()));
    service
        .execute_side_query_request(registered_request())
        .await
        .unwrap();
    assert_eq!(
        *probe.events.lock().unwrap(),
        vec!["begin", "dispatch", "finish-owned", "settled"]
    );
}
