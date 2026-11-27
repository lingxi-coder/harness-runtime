use super::*;

#[test]
fn native_context_1m_latch_key_strips_only_case_insensitive_beta_tags() {
    let request = LlmRequest::new("claude-[1M]-Opus-[2m]-Alias");
    let actual = String::from_utf16(&native_context_1m_model_key(&request)).unwrap();
    assert_eq!(actual, "claude--Opus--Alias");
}

#[test]
fn native_custom_beta_route_requires_a_represented_provider_family() {
    use crate::model::thinking_binding_controls::ProviderRoute;

    assert_eq!(
        native_custom_beta_provider_route(
            &ProviderId::BedrockClaude,
            ProtocolFamily::BedrockClaude
        ),
        Some(ProviderRoute::Bedrock)
    );
    assert_eq!(
        native_custom_beta_provider_route(&ProviderId::VertexClaude, ProtocolFamily::VertexClaude),
        Some(ProviderRoute::Vertex)
    );
    assert_eq!(
        native_custom_beta_provider_route(
            &ProviderId::FoundryClaude,
            ProtocolFamily::FoundryClaude
        ),
        Some(ProviderRoute::Foundry)
    );
    assert_eq!(
        native_custom_beta_provider_route(
            &ProviderId::Custom {
                name: "compatible".into()
            },
            ProtocolFamily::AnthropicMessages
        ),
        None
    );
    assert_eq!(
        native_custom_beta_provider_route(
            &ProviderId::BedrockClaude,
            ProtocolFamily::AnthropicMessages
        ),
        None
    );
}

#[tokio::test]
async fn vertex_custom_beta_additions_keep_native_r_s_entries_when_b6_is_true() {
    let _cache_env_lock = CACHE_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _thinking_env_lock = THINKING_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _restore = CacheEnvRestore::clear(&[
        "ANTHROPIC_VERTEX_BASE_URL",
        "CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS",
        "CLAUDE_CODE_EXTRA_BODY",
    ]);
    std::env::set_var("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS", "1");

    let model = "claude-sonnet-4-20250514";
    let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let service = make_adapter_for_protocol_with_transport(
        ProtocolFamily::VertexClaude,
        ProviderId::VertexClaude,
        "https://vertex.example.test",
        "vertex",
        model,
        transport.clone(),
    )
    .with_custom_cli_betas(vec![
        crate::model::betas::CONTEXT_1M.to_owned(),
        "not-in-native-static-list".to_owned(),
    ]);
    let mut request = LlmRequest::new(model).with_user_text("Hi");
    request.profile = Some("vertex".into());

    service
        .execute_non_stream_request(
            request,
            crate::NonStreamingRequestClass::Main,
            crate::NonStreamingRetryOptions::default(),
        )
        .await
        .expect("Vertex request succeeds");

    let headers = transport.seen_headers(0);
    assert!(request_header_has_beta(
        &headers,
        crate::model::betas::CONTEXT_1M
    ));
    assert!(!request_header_has_beta(
        &headers,
        "not-in-native-static-list"
    ));
}

#[tokio::test]
async fn bedrock_custom_beta_additions_keep_only_native_r_s_entries_when_b6_is_true() {
    let _cache_env_lock = CACHE_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _thinking_env_lock = THINKING_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _restore = CacheEnvRestore::clear(&[
        "ANTHROPIC_BEDROCK_BASE_URL",
        "CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS",
        "CLAUDE_CODE_EXTRA_BODY",
    ]);
    std::env::set_var("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS", "1");

    let model = "claude-sonnet-4-20250514";
    let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let service = make_adapter_for_protocol_with_transport(
        ProtocolFamily::BedrockClaude,
        ProviderId::BedrockClaude,
        "https://bedrock.example.test",
        "bedrock",
        model,
        transport.clone(),
    )
    .with_custom_cli_betas(vec![
        crate::model::betas::CONTEXT_1M.to_owned(),
        "not-in-native-static-list".to_owned(),
    ]);
    let mut request = LlmRequest::new(model).with_user_text("Hi");
    request.profile = Some("bedrock".into());
    let mut prepared = service.client.prepare(&request).await.expect("prepare");
    service
        .inject_headers(&mut prepared, DispatchHeaderState::default())
        .unwrap();
    let planned = prepared
        .planned_native_beta_context
        .as_ref()
        .expect("planned Native beta context is captured");
    assert!(
        planned
            .generated_body
            .iter()
            .any(|beta| beta == crate::model::betas::CONTEXT_1M),
        "filtered Bedrock CLI body beta is recorded as planned"
    );
    assert!(!planned
        .generated_body
        .iter()
        .any(|beta| beta == "not-in-native-static-list"));
    assert!(!request_header_has_beta(
        &prepared.provider_request.headers,
        crate::model::betas::CONTEXT_1M
    ));
    assert!(request_body_has_beta(
        &prepared.provider_request.body_json,
        crate::model::betas::CONTEXT_1M
    ));

    service
        .execute_non_stream_request(
            request,
            crate::NonStreamingRequestClass::Main,
            crate::NonStreamingRetryOptions::default(),
        )
        .await
        .expect("Bedrock request succeeds");

    let body = transport.seen_body(0);
    assert!(!request_header_has_beta(
        &transport.seen_headers(0),
        crate::model::betas::CONTEXT_1M
    ));
    assert!(request_body_has_beta(
        &body,
        crate::model::betas::CONTEXT_1M
    ));
    assert!(!request_body_has_beta(&body, "not-in-native-static-list"));
}

#[tokio::test]
async fn native_hq_unknown_400_retires_tbc_only_after_gb_and_jk_are_known_false() {
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
    let transport = FakeTransport::sequence(vec![
        FakeResponse::Ok(ProviderResponse::json(
            400,
            serde_json::json!({
                "type": "error",
                "error": {
                    "type": "invalid_request_error",
                    "message": "request field is not recognized"
                }
            }),
        )),
        FakeResponse::Ok(ProviderResponse::json(200, ok_response_json())),
    ]);
    let model = "claude-opus-4-7";
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

    service
        .execute_non_stream_request(
            request,
            crate::NonStreamingRequestClass::Main,
            crate::NonStreamingRetryOptions::default(),
        )
        .await
        .expect("HQ's broad QEE retries only when both native classifiers are false");

    let tbc = "thinking-binding-controls-2026-08-01";
    assert_eq!(transport.seen_count(), 2);
    assert!(request_header_has_beta(&transport.seen_headers(0), tbc));
    assert!(!request_header_has_beta(&transport.seen_headers(1), tbc));
    assert!(bedrock_tbc_state(&scope).beta_rejected);
}

#[tokio::test]
async fn native_hq_known_error_keeps_tbc_when_generic_claim_matches() {
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
    let transport = FakeTransport::sequence(vec![FakeResponse::Ok(ProviderResponse::json(
        400,
        serde_json::json!({
            "type": "error",
            "error": {
                "type": "invalid_request_error",
                "message": "the advisor tool is not available in this workspace"
            }
        }),
    ))]);
    let model = "claude-opus-4-7";
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

    let result = service
        .execute_non_stream_request(
            request,
            crate::NonStreamingRequestClass::Main,
            crate::NonStreamingRetryOptions::default(),
        )
        .await;

    assert!(result.is_err());
    assert_eq!(transport.seen_count(), 1);
    assert!(request_header_has_beta(
        &transport.seen_headers(0),
        "thinking-binding-controls-2026-08-01"
    ));
    assert!(!bedrock_tbc_state(&scope).beta_rejected);
}

#[tokio::test]
async fn native_hq_n_b_refusal_fallback_target_keeps_tbc() {
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
    let transport = FakeTransport::sequence(vec![FakeResponse::Ok(ProviderResponse::json(
        400,
        serde_json::json!({
            "type": "error",
            "error": {
                "type": "invalid_request_error",
                "message": "'claude-opus-4-7' does not support the `speed` parameter"
            }
        }),
    ))]);
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
    request.execution.refusal_fallback_context =
        Some(lingxi_core::host::refusal_driver::FallbackTargetContext {
            user_model: "claude-sonnet-4-7".into(),
            turn_override: Some(model.to_owned()),
            ..Default::default()
        });
    request.input.thinking = Some(lingxi_llm_client::protocol::ThinkingConfig {
        mode: Some(lingxi_llm_client::protocol::ThinkingMode::Adaptive),
        ..Default::default()
    });

    let result = service
        .execute_non_stream_request(
            request,
            crate::NonStreamingRequestClass::Main,
            crate::NonStreamingRetryOptions::default(),
        )
        .await;

    assert!(result.is_err());
    assert_eq!(transport.seen_count(), 1);
    assert!(request_header_has_beta(
        &transport.seen_headers(0),
        "thinking-binding-controls-2026-08-01"
    ));
    assert!(!bedrock_tbc_state(&scope).beta_rejected);
}

#[tokio::test]
async fn native_tbc_h3_fact_and_final_authentication_share_one_selected_key_snapshot() {
    let _cache_env_lock = CACHE_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _thinking_env_lock = THINKING_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _restore = CacheEnvRestore::clear(&["ADAPTER_TEST_KEY"]);
    let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let service = make_adapter_with_subscriber(transport.clone(), SubscriberState::default());
    let mut request = LlmRequest::new("claude-sonnet-4-20250514").with_user_text("Hi");
    request.profile = Some("anthropic".into());

    let mut prepared = service
        .client
        .prepare_on(&request, transport, Default::default())
        .await
        .expect("preparation captures the selected key");
    assert_eq!(prepared.native_tbc_first_party_h3, Some(false));

    std::env::set_var("ADAPTER_TEST_KEY", "rotated-test-key");
    assert_eq!(
        prepared.authenticator.captured_credential().await.unwrap(),
        Some(Credential::ApiKey("test-key".to_owned()))
    );
    service
        .client
        .seal_prepared(&mut prepared)
        .await
        .expect("final SDK authentication reuses the captured key");
    let call = prepared.wire_call.take().expect("sealed wire call");
    assert_eq!(
        call.request()
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("x-api-key"))
            .map(|(_, value)| value.as_str()),
        Some("test-key")
    );
}

#[tokio::test]
async fn model_validation_request_is_minimal_and_never_retries() {
    use lingxi_llm_client::protocol::{CacheBreakpoint, CachePosition, CacheTtl, JsonPayload};

    // Dreamy Frost adds v2d even to auxiliary first-party requests. This
    // default-off assertion must not overlap the dispatch matrix's flag writes.
    let _dispatch_flags = cedar_lattice_read();
    let _g = THINKING_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _restore = CacheEnvRestore::clear(&["CLAUDE_CODE_EXTRA_BODY"]);
    for extra in [
        r#"{"model":"inherited-model","max_tokens":4096,"system":"inherited system","tools":[],"thinking":{"type":"adaptive"},"temperature":0.5,"output_config":{"effort":"max","format":{"type":"json_schema","schema":{"type":"boolean"}}},"stop_sequences":["inherited stop"]}"#,
        r#"{"betas":{"toString":null}}"#,
    ] {
        std::env::set_var("CLAUDE_CODE_EXTRA_BODY", extra);
        let transport = FakeTransport::sequence(vec![
            FakeResponse::Ok(ProviderResponse {
                status: 429,
                headers: BTreeMap::from([("x-should-retry".into(), "true".into())]),
                body_json: provider_payload(serde_json::json!({
                    "type": "error",
                    "error": {"type": "overloaded_error", "message": "try again"}
                })),
                request_id: Some("validation-429".into()),
            }),
            FakeResponse::Ok(ProviderResponse::json(200, ok_response_json())),
        ]);
        let service = make_adapter(transport.clone());
        let mut request = LlmRequest::new("claude-sonnet-4-20250514").with_user_text("Hi");
        request.input.model_source = Some(JsonPayload::string("claude-unlisted-validation-model"));
        request.input.max_tokens = Some(1);
        request
            .input
            .prompt_cache
            .breakpoints
            .push(CacheBreakpoint {
                scope: None,
                position: CachePosition::Message { index: 0, block: 0 },
                ttl: CacheTtl::FiveMinutes,
            });

        let result = service.execute_model_validation_request(request).await;
        assert!(result.is_err(), "a retryable 429 is returned to the caller");
        assert_eq!(
            transport.seen_count(),
            1,
            "a failed probe must not be retried"
        );

        let seen = transport.seen.lock().unwrap();
        let sent = &seen[0];
        let body = request_body_value(&sent.body_json);
        assert_eq!(body["model"], "claude-unlisted-validation-model");
        assert_eq!(body["max_tokens"], 1);
        assert_eq!(body["messages"][0]["content"][0]["text"], "Hi");
        assert_eq!(
            body["messages"][0]["content"][0]["cache_control"],
            serde_json::json!({"type": "ephemeral"})
        );
        assert!(
            body.get("system").is_none(),
            "the probe has no inherited system"
        );
        assert!(body.get("tools").is_none(), "the probe has no tools");
        assert!(
            body.get("thinking").is_none(),
            "the probe has no thinking policy"
        );
        assert!(body.get("temperature").is_none());
        assert!(body.get("output_config").is_none());
        assert!(body.get("effort").is_none());
        assert!(body.get("stop_sequences").is_none());
        assert!(
            !sent.headers.contains_key(DISPATCH_ID_HEADER),
            "model-validation traffic is auxiliary"
        );
    }
}

#[tokio::test]
async fn ordinary_context_1m_retry_suppresses_bedrock_wire_and_latches_after_success() {
    let _thinking_env_lock = THINKING_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _restore = CacheEnvRestore::clear(&["CLAUDE_CODE_EXTRA_BODY"]);
    let transport = FakeTransport::sequence(vec![
        context_1m_refusal_response("The context-1m-2025-08-07 beta is not supported"),
        FakeResponse::Ok(ProviderResponse::json(200, ok_response_json())),
        FakeResponse::Ok(ProviderResponse::json(200, ok_response_json())),
    ]);
    let model = "claude-sonnet-4-20250514[1m]";
    let service = make_adapter_for_protocol_with_transport(
        ProtocolFamily::BedrockClaude,
        ProviderId::BedrockClaude,
        "https://bedrock.example.test",
        "bedrock",
        model,
        transport.clone(),
    );
    let scope = crate::thinking_scope::ThinkingRecoveryScope::default();
    let model_key = model.encode_utf16().collect::<Vec<_>>();
    let mut request = LlmRequest::new(model).with_user_text("Hi");
    request.profile = Some("bedrock".into());
    request.execution.thinking_recovery_scope = Some(scope.clone());

    service
        .execute_non_stream_request(
            request,
            crate::NonStreamingRequestClass::Main,
            crate::NonStreamingRetryOptions::default(),
        )
        .await
        .expect("the ordinary Native context-beta repair succeeds");
    assert!(scope.context_1m_refused(&native_foundry_model_key(&model_key)));

    let mut next_request = LlmRequest::new(model).with_user_text("Continue");
    next_request.profile = Some("bedrock".into());
    next_request.execution.thinking_recovery_scope = Some(scope.clone());
    service
        .execute_non_stream_request(
            next_request,
            crate::NonStreamingRequestClass::Main,
            crate::NonStreamingRetryOptions::default(),
        )
        .await
        .expect("the next same-session request succeeds without context-1m");

    assert_eq!(transport.seen_count(), 3);
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
async fn ordinary_context_1m_retry_ignores_custom_body_only_membership() {
    let _thinking_env_lock = THINKING_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _restore = CacheEnvRestore(vec![(
        "CLAUDE_CODE_EXTRA_BODY",
        std::env::var_os("CLAUDE_CODE_EXTRA_BODY"),
    )]);
    std::env::set_var(
        "CLAUDE_CODE_EXTRA_BODY",
        r#"{"anthropic_beta":["context-1m-2025-08-07"]}"#,
    );
    let transport = FakeTransport::sequence(vec![
        context_1m_refusal_response("The context-1m-2025-08-07 beta is not supported"),
        FakeResponse::Ok(ProviderResponse::json(200, ok_response_json())),
    ]);
    let model = "claude-sonnet-4-20250514";
    let service = make_adapter_for_protocol_with_transport(
        ProtocolFamily::BedrockClaude,
        ProviderId::BedrockClaude,
        "https://bedrock.example.test",
        "bedrock",
        model,
        transport.clone(),
    );
    let mut request = LlmRequest::new(model).with_user_text("Hi");
    request.profile = Some("bedrock".into());

    assert!(service
        .execute_non_stream_request(
            request,
            crate::NonStreamingRequestClass::Main,
            crate::NonStreamingRetryOptions::default(),
        )
        .await
        .is_err());
    assert_eq!(transport.seen_count(), 1);
    assert!(request_body_has_beta(
        &transport.seen_body(0),
        crate::model::betas::CONTEXT_1M
    ));
}

#[tokio::test]
async fn ordinary_context_1m_default_first_party_refusal_is_terminal_without_latching() {
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
    ]);
    let transport = FakeTransport::sequence(vec![
        context_1m_refusal_response("The context-1m-2025-08-07 beta is not supported"),
        FakeResponse::Ok(ProviderResponse::json(200, ok_response_json())),
    ]);
    let model = "claude-sonnet-4-20250514";
    let service = make_adapter(transport.clone()).with_custom_cli_betas(vec![
        crate::model::context_window::CONTEXT_1M_BETA_HEADER.to_string(),
    ]);
    let scope = crate::thinking_scope::ThinkingRecoveryScope::default();
    let mut request = LlmRequest::new(model).with_user_text("Hi");
    request.execution.thinking_recovery_scope = Some(scope.clone());

    assert!(service
        .execute_non_stream_request(
            request,
            crate::NonStreamingRequestClass::Main,
            crate::NonStreamingRetryOptions::default(),
        )
        .await
        .is_err());
    assert_eq!(transport.seen_count(), 1);
    assert!(request_header_has_beta(
        &transport.seen_headers(0),
        crate::model::betas::CONTEXT_1M
    ));
    assert!(!scope.context_1m_refused(&native_foundry_model_key(
        &model.encode_utf16().collect::<Vec<_>>()
    )));
}

#[tokio::test]
async fn foundry_context_1m_retry_does_not_inherit_tbc_from_bedrock() {
    let _cache_env_lock = CACHE_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _thinking_env_lock = THINKING_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _restore = CacheEnvRestore::clear(&[
        "ANTHROPIC_BASE_URL",
        "_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL",
        "ANTHROPIC_FOUNDRY_RESOURCE",
        "ANTHROPIC_FOUNDRY_BASE_URL",
        "CLAUDE_CODE_EXTRA_BODY",
        "CLAUDE_CODE_SIMULATE_PROXY_USAGE",
        "CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS",
    ]);
    let transport = FakeTransport::sequence(vec![
        context_1m_refusal_response("invalid beta flag"),
        FakeResponse::Ok(ProviderResponse::json(200, ok_response_json())),
    ]);
    let model = "claude-opus-4-7[1m]";
    let service = make_adapter_for_protocol_with_transport(
        ProtocolFamily::FoundryClaude,
        ProviderId::FoundryClaude,
        "https://resource.services.ai.azure.com/anthropic",
        "foundry",
        model,
        transport.clone(),
    );
    let scope = crate::thinking_scope::ThinkingRecoveryScope::default();
    let bedrock_transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let bedrock = make_adapter_for_protocol_with_transport(
        ProtocolFamily::BedrockClaude,
        ProviderId::BedrockClaude,
        "https://bedrock.example.test",
        "bedrock",
        model,
        bedrock_transport.clone(),
    );
    let mut previous = LlmRequest::new(model).with_user_text("Previous turn");
    previous.profile = Some("bedrock".into());
    previous.execution.thinking_recovery_scope = Some(scope.clone());
    previous.input.thinking = Some(lingxi_llm_client::protocol::ThinkingConfig {
        mode: Some(lingxi_llm_client::protocol::ThinkingMode::Adaptive),
        ..Default::default()
    });
    bedrock
        .execute_non_stream_request(
            previous,
            crate::NonStreamingRequestClass::Main,
            Default::default(),
        )
        .await
        .unwrap();
    assert!(request_header_has_beta(
        &bedrock_transport.seen_headers(0),
        "thinking-binding-controls-2026-08-01"
    ));
    let mut request = LlmRequest::new(model).with_user_text("Hi");
    request.profile = Some("foundry".into());
    request.execution.thinking_recovery_scope = Some(scope.clone());
    request.input.thinking = Some(lingxi_llm_client::protocol::ThinkingConfig {
        mode: Some(lingxi_llm_client::protocol::ThinkingMode::Adaptive),
        ..Default::default()
    });

    service
        .execute_non_stream_request(
            request,
            crate::NonStreamingRequestClass::Main,
            crate::NonStreamingRetryOptions::default(),
        )
        .await
        .expect("RZe retries context-1m because Native Tp is false on U6 routes");

    let tbc = "thinking-binding-controls-2026-08-01";
    assert_eq!(transport.seen_count(), 2);
    assert!(!request_header_has_beta(&transport.seen_headers(0), tbc));
    assert!(request_header_has_beta(
        &transport.seen_headers(0),
        crate::model::betas::CONTEXT_1M
    ));
    assert!(!request_header_has_beta(&transport.seen_headers(1), tbc));
    assert!(!request_header_has_beta(
        &transport.seen_headers(1),
        crate::model::betas::CONTEXT_1M
    ));
    assert!(scope.context_1m_refused(&native_foundry_model_key(
        &model.encode_utf16().collect::<Vec<_>>()
    )));
}

#[tokio::test]
async fn ordinary_context_1m_generic_refusal_prefers_tbc_qee_then_retires_only_tbc_for_conversation(
) {
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
    let transport = FakeTransport::sequence(vec![
        context_1m_refusal_response("invalid beta flag"),
        FakeResponse::Ok(ProviderResponse::json(200, ok_response_json())),
        FakeResponse::Ok(ProviderResponse::json(200, ok_response_json())),
        FakeResponse::Ok(ProviderResponse::json(200, ok_response_json())),
    ]);
    let model = "claude-opus-4-7[1m]";
    let service = make_adapter_for_protocol_with_transport(
        ProtocolFamily::BedrockClaude,
        ProviderId::BedrockClaude,
        "https://bedrock.example.test",
        "bedrock",
        model,
        transport.clone(),
    );
    let mut request = LlmRequest::new(model).with_user_text("Hi");
    request.profile = Some("bedrock".into());
    let scope = crate::thinking_scope::ThinkingRecoveryScope::default();
    request.execution.thinking_recovery_scope = Some(scope.clone());
    request.input.thinking = Some(lingxi_llm_client::protocol::ThinkingConfig {
        mode: Some(lingxi_llm_client::protocol::ThinkingMode::Adaptive),
        ..Default::default()
    });

    service
        .execute_non_stream_request(
            request,
            crate::NonStreamingRequestClass::Main,
            crate::NonStreamingRetryOptions::default(),
        )
        .await
        .expect("the exact TBC refusal retries without TBC");
    assert_eq!(transport.seen_count(), 2);
    let headers = transport.seen_headers(0);
    let tbc = "thinking-binding-controls-2026-08-01";
    assert!(!request_header_has_beta(
        &headers,
        crate::model::betas::CONTEXT_1M
    ));
    assert!(request_header_has_beta(&headers, tbc));
    assert!(request_body_has_beta(
        &transport.seen_body(0),
        crate::model::betas::CONTEXT_1M
    ));
    assert!(!request_header_has_beta(
        &transport.seen_headers(1),
        crate::model::betas::CONTEXT_1M
    ));
    assert!(!request_header_has_beta(&transport.seen_headers(1), tbc));
    assert!(request_body_has_beta(
        &transport.seen_body(1),
        crate::model::betas::CONTEXT_1M
    ));
    assert!(bedrock_tbc_state(&scope).beta_rejected);
    assert!(!bedrock_tbc_state(&scope).beta_sent);
    assert!(!scope.context_1m_refused(&model.encode_utf16().collect::<Vec<_>>()));

    let mut same_conversation = LlmRequest::new(model).with_user_text("Next");
    same_conversation.profile = Some("bedrock".into());
    same_conversation.execution.thinking_recovery_scope = Some(scope.clone());
    same_conversation.input.thinking = Some(lingxi_llm_client::protocol::ThinkingConfig {
        mode: Some(lingxi_llm_client::protocol::ThinkingMode::Adaptive),
        ..Default::default()
    });
    service
        .execute_non_stream_request(
            same_conversation,
            crate::NonStreamingRequestClass::Main,
            crate::NonStreamingRetryOptions::default(),
        )
        .await
        .expect("a later request in this conversation keeps the refusal latch");
    assert_eq!(transport.seen_count(), 3);
    assert!(!request_header_has_beta(&transport.seen_headers(2), tbc));

    let mut fresh_conversation = LlmRequest::new(model).with_user_text("Fresh");
    fresh_conversation.profile = Some("bedrock".into());
    fresh_conversation.execution.thinking_recovery_scope =
        Some(crate::thinking_scope::ThinkingRecoveryScope::default());
    fresh_conversation.input.thinking = Some(lingxi_llm_client::protocol::ThinkingConfig {
        mode: Some(lingxi_llm_client::protocol::ThinkingMode::Adaptive),
        ..Default::default()
    });
    service
        .execute_non_stream_request(
            fresh_conversation,
            crate::NonStreamingRequestClass::Main,
            crate::NonStreamingRetryOptions::default(),
        )
        .await
        .expect("a new conversation does not inherit a model-wide TBC refusal");
    assert_eq!(transport.seen_count(), 4);
    assert!(request_header_has_beta(&transport.seen_headers(3), tbc));
}

#[tokio::test]
async fn late_tbc_refusal_after_context_reset_does_not_poison_fresh_request() {
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
    let scope = crate::thinking_scope::ThinkingRecoveryScope::default();
    let transport = FakeTransport::sequence(vec![
        FakeResponse::ResetScopeBeforeReply {
            scope: scope.clone(),
            response: ProviderResponse::json(
                400,
                serde_json::json!({
                    "type": "error",
                    "error": {
                        "type": "invalid_request_error",
                        "message": "invalid beta flag"
                    }
                }),
            ),
        },
        FakeResponse::Ok(ProviderResponse::json(200, ok_response_json())),
        FakeResponse::Ok(ProviderResponse::json(200, ok_response_json())),
    ]);
    let model = "claude-opus-4-7[1m]";
    let service = make_adapter_for_protocol_with_transport(
        ProtocolFamily::BedrockClaude,
        ProviderId::BedrockClaude,
        "https://bedrock.example.test",
        "bedrock",
        model,
        transport.clone(),
    );
    let tbc = "thinking-binding-controls-2026-08-01";
    let mut request = LlmRequest::new(model).with_user_text("Pending");
    request.profile = Some("bedrock".into());
    request.execution.thinking_recovery_scope = Some(scope.clone());
    request.input.thinking = Some(lingxi_llm_client::protocol::ThinkingConfig {
        mode: Some(lingxi_llm_client::protocol::ThinkingMode::Adaptive),
        ..Default::default()
    });

    service
        .execute_non_stream_request(
            request,
            crate::NonStreamingRequestClass::Main,
            crate::NonStreamingRetryOptions::default(),
        )
        .await
        .expect("the in-flight request retries after its TBC refusal");
    assert_eq!(transport.seen_count(), 2);
    assert!(request_header_has_beta(&transport.seen_headers(0), tbc));
    assert!(!request_header_has_beta(&transport.seen_headers(1), tbc));
    assert!(!bedrock_tbc_state(&scope).beta_sent);
    assert!(!bedrock_tbc_state(&scope).beta_rejected);

    let mut fresh_request = LlmRequest::new(model).with_user_text("After reset");
    fresh_request.profile = Some("bedrock".into());
    fresh_request.execution.thinking_recovery_scope = Some(scope.clone());
    fresh_request.input.thinking = Some(lingxi_llm_client::protocol::ThinkingConfig {
        mode: Some(lingxi_llm_client::protocol::ThinkingMode::Adaptive),
        ..Default::default()
    });
    service
        .execute_non_stream_request(
            fresh_request,
            crate::NonStreamingRequestClass::Main,
            crate::NonStreamingRetryOptions::default(),
        )
        .await
        .expect("a fresh query can register TBC in the reset conversation");
    assert_eq!(transport.seen_count(), 3);
    assert!(request_header_has_beta(&transport.seen_headers(2), tbc));
    assert!(bedrock_tbc_state(&scope).beta_sent);
    assert!(!bedrock_tbc_state(&scope).beta_rejected);
}

#[tokio::test]
async fn ordinary_context_1m_generic_refusal_retries_when_tbc_is_absent() {
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
    ]);
    std::env::set_var("ANTHROPIC_BASE_URL", "https://proxy.example.test");
    std::env::remove_var("_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL");
    let transport = FakeTransport::sequence(vec![
        context_1m_refusal_response("invalid beta flag"),
        FakeResponse::Ok(ProviderResponse::json(200, ok_response_json())),
    ]);
    let model = "claude-sonnet-4-20250514";
    let service = make_adapter_for_protocol_with_transport(
        ProtocolFamily::AnthropicMessages,
        ProviderId::AnthropicFirstParty,
        "https://proxy.example.test",
        "proxy",
        model,
        transport.clone(),
    )
    .with_custom_cli_betas(vec![crate::model::betas::CONTEXT_1M.to_string()]);
    let mut request = LlmRequest::new(model).with_user_text("Hi");
    request.profile = Some("proxy".into());

    service
        .execute_non_stream_request(
            request,
            crate::NonStreamingRequestClass::Main,
            crate::NonStreamingRetryOptions::default(),
        )
        .await
        .expect("ambiguous refusal retries without the TBC disambiguator");
    assert_eq!(transport.seen_count(), 2);
    assert!(request_header_has_beta(
        &transport.seen_headers(0),
        crate::model::betas::CONTEXT_1M
    ));
    assert!(!request_header_has_beta(
        &transport.seen_headers(1),
        crate::model::betas::CONTEXT_1M
    ));
}

#[tokio::test]
async fn ordinary_context_1m_does_not_infer_tbc_admission_from_header_alone() {
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
    ]);
    std::env::set_var("ANTHROPIC_BASE_URL", "https://proxy.example.test");
    std::env::remove_var("_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL");
    let transport = FakeTransport::sequence(vec![
        context_1m_refusal_response("invalid beta flag"),
        FakeResponse::Ok(ProviderResponse::json(200, ok_response_json())),
    ]);
    let model = "claude-sonnet-4-20250514";
    let tbc = "thinking-binding-controls-2026-08-01";
    let service = make_adapter_for_protocol_with_transport(
        ProtocolFamily::AnthropicMessages,
        ProviderId::AnthropicFirstParty,
        "https://proxy.example.test",
        "proxy",
        model,
        transport.clone(),
    )
    .with_custom_cli_betas(vec![
        crate::model::betas::CONTEXT_1M.to_string(),
        tbc.to_string(),
    ]);
    let mut request = LlmRequest::new(model).with_user_text("Hi");
    request.profile = Some("proxy".into());
    assert_eq!(
        request
            .execution
            .native_thinking_binding_controls_beta_requested,
        None,
        "wire header alone is not Native's J_(nt,uB) admission fact"
    );

    service
        .execute_non_stream_request(
            request,
            crate::NonStreamingRequestClass::Main,
            crate::NonStreamingRetryOptions::default(),
        )
        .await
        .expect("a header token without the Native gate does not block the ctx1m repair");
    assert_eq!(transport.seen_count(), 2);
    assert!(request_header_has_beta(
        &transport.seen_headers(0),
        crate::model::betas::CONTEXT_1M
    ));
    assert!(request_header_has_beta(&transport.seen_headers(0), tbc));
    assert!(!request_header_has_beta(
        &transport.seen_headers(1),
        crate::model::betas::CONTEXT_1M
    ));
}

#[tokio::test]
async fn native_tbc_first_party_uses_separate_tf_kxt_and_sl_facts() {
    let _cache_env_lock = CACHE_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _thinking_env_lock = THINKING_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _restore = CacheEnvRestore::clear(&[
        "ANTHROPIC_BASE_URL",
        "_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL",
        "CLAUDE_CODE_SIMULATE_PROXY_USAGE",
        "CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS",
        "CLAUDE_CODE_EXTRA_BODY",
    ]);
    let model = "claude-sonnet-4-20250514";
    let tbc = "thinking-binding-controls-2026-08-01";
    let transport = FakeTransport::sequence(vec![
        FakeResponse::Ok(ProviderResponse::json(200, ok_response_json())),
        FakeResponse::Ok(ProviderResponse::json(200, ok_response_json())),
        FakeResponse::Ok(ProviderResponse::json(200, ok_response_json())),
    ]);

    let make_request = |profile: &str| {
        let mut request = LlmRequest::new(model).with_user_text("Hi");
        request.profile = Some(profile.to_owned());
        request.input.thinking = Some(lingxi_llm_client::protocol::ThinkingConfig {
            mode: Some(lingxi_llm_client::protocol::ThinkingMode::Adaptive),
            ..Default::default()
        });
        request
    };

    // A canonical process URL paired with the selected canonical route
    // takes Native KXt's local true branch.
    std::env::set_var("ANTHROPIC_BASE_URL", "https://api.anthropic.com");
    let canonical_env = make_adapter_for_protocol_with_transport(
        ProtocolFamily::AnthropicMessages,
        ProviderId::AnthropicFirstParty,
        "https://api.anthropic.com",
        "canonical-profile",
        model,
        transport.clone(),
    );
    let canonical_scope = crate::thinking_scope::ThinkingRecoveryScope::default();
    let mut canonical_request = make_request("canonical-profile");
    canonical_request.execution.thinking_recovery_scope = Some(canonical_scope.clone());
    canonical_env
        .execute_non_stream_request(
            canonical_request,
            crate::NonStreamingRequestClass::Main,
            crate::NonStreamingRetryOptions::default(),
        )
        .await
        .expect("the canonical tf/KXt path sends the planned Native beta");
    assert!(request_header_has_beta(&transport.seen_headers(0), tbc));
    assert!(
        fixture_tbc_state(
            &canonical_scope,
            ProviderId::AnthropicFirstParty,
            "canonical-profile",
            "https://api.anthropic.com",
            ProtocolFamily::AnthropicMessages
        )
        .beta_sent
    );

    // With the env absent, tf/sl are true but KXt still depends on
    // Native's bd()/h3()/profile path, which this Host has not observed.
    std::env::remove_var("ANTHROPIC_BASE_URL");
    let default_route = make_adapter_for_protocol_with_transport(
        ProtocolFamily::AnthropicMessages,
        ProviderId::AnthropicFirstParty,
        "https://api.anthropic.com",
        "native-default",
        model,
        transport.clone(),
    );
    let default_scope = crate::thinking_scope::ThinkingRecoveryScope::default();
    let mut default_request = make_request("native-default");
    default_request.execution.thinking_recovery_scope = Some(default_scope.clone());
    default_route
        .execute_non_stream_request(
            default_request,
            crate::NonStreamingRequestClass::Main,
            crate::NonStreamingRetryOptions::default(),
        )
        .await
        .expect("an unresolved KXt fact does not prevent the ordinary request");
    assert!(!request_header_has_beta(&transport.seen_headers(1), tbc));
    assert!(
        !fixture_tbc_state(
            &default_scope,
            ProviderId::AnthropicFirstParty,
            "native-default",
            "https://api.anthropic.com",
            ProtocolFamily::AnthropicMessages
        )
        .beta_sent
    );

    // The captured Native assume setting can make sl true while a custom
    // env URL makes tf/KXt false. Keeping those predicates separate avoids
    // treating the endpoint assumption as account-gate evidence.
    std::env::set_var("ANTHROPIC_BASE_URL", "https://proxy.example.test");
    let assumed_proxy = make_adapter_for_protocol_with_transport(
        ProtocolFamily::AnthropicMessages,
        ProviderId::AnthropicFirstParty,
        "https://proxy.example.test",
        "assumed-proxy",
        model,
        transport.clone(),
    )
    .with_native_system_prefix(
        lingxi_llm_client::providers::anthropic::system_prompt::NativeSystemPrefix {
            compatibility_version: "2.1.295".into(),
            entrypoint: "cli".into(),
            host_identity:
                lingxi_llm_client::providers::anthropic::system_prompt::PromptText::from_string(
                    "LingXi",
                ),
            attribution_enabled: false,
            assume_first_party_base_url: true,
        },
    );
    let assumed_scope = crate::thinking_scope::ThinkingRecoveryScope::default();
    let mut assumed_request = make_request("assumed-proxy");
    assumed_request.execution.thinking_recovery_scope = Some(assumed_scope.clone());
    assumed_proxy
        .execute_non_stream_request(
            assumed_request,
            crate::NonStreamingRequestClass::Main,
            crate::NonStreamingRetryOptions::default(),
        )
        .await
        .expect("a source endpoint assumption does not fabricate KXt");
    assert!(!request_header_has_beta(&transport.seen_headers(2), tbc));
    assert!(
        !fixture_tbc_state(
            &assumed_scope,
            ProviderId::AnthropicFirstParty,
            "assumed-proxy",
            "https://proxy.example.test",
            ProtocolFamily::AnthropicMessages
        )
        .beta_sent
    );
}

#[tokio::test]
async fn native_tbc_uses_each_selected_first_party_profile_endpoint() {
    let _cache_env_lock = CACHE_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _thinking_env_lock = THINKING_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _restore = CacheEnvRestore::clear(&[
        "ANTHROPIC_BASE_URL",
        "_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL",
        "CLAUDE_CODE_SIMULATE_PROXY_USAGE",
        "CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS",
        "CLAUDE_CODE_EXTRA_BODY",
        "R379_TBC_ROUTE_API_KEY",
    ]);
    std::env::set_var("R379_TBC_ROUTE_API_KEY", "sk-test-only");

    let model = "claude-sonnet-4-20250514";
    let mut canonical = crate::builtin_presets()
        .providers
        .into_iter()
        .find(|profile| profile.profile_name == "anthropic")
        .expect("built-in first-party profile");
    canonical.profile_name = "r379-tbc-canonical".into();
    canonical.auth = crate::AuthStrategy::ApiKey;
    canonical.credential = crate::CredentialConfig::Env {
        var: "R379_TBC_ROUTE_API_KEY".into(),
    };
    canonical.models = vec![crate::ModelProfile {
        display_model: model.into(),
        request_model: model.into(),
        billing_model: "claude-sonnet-4".into(),
        aliases: Vec::new(),
        description: None,
        metadata: Default::default(),
        capabilities: crate::Capabilities {
            streaming: true,
            tools: true,
            reasoning: true,
            ..Default::default()
        },
    }];

    let mut proxy = canonical.clone();
    proxy.profile_name = "r379-tbc-proxy".into();
    proxy.base_url = "http://127.0.0.1:31003".into();
    let client = Arc::new(
        ModelRuntime::from_config(ClientConfig {
            providers: vec![canonical, proxy],
        })
        .expect("two first-party routes with the same model"),
    );
    let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let service = ApiService::new(
        client,
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
    let tbc = "thinking-binding-controls-2026-08-01";

    for (profile, expected_tbc) in [("r379-tbc-proxy", false), ("r379-tbc-canonical", true)] {
        let mut request = LlmRequest::new(model).with_user_text("Hi");
        request.profile = Some(profile.into());
        request.input.thinking = Some(lingxi_llm_client::protocol::ThinkingConfig {
            mode: Some(lingxi_llm_client::protocol::ThinkingMode::Adaptive),
            ..Default::default()
        });
        request.execution.thinking_recovery_scope =
            Some(crate::thinking_scope::ThinkingRecoveryScope::default());
        service
            .execute_non_stream_request(
                request,
                crate::NonStreamingRequestClass::Main,
                crate::NonStreamingRetryOptions::default(),
            )
            .await
            .expect("fixture request succeeds");
        assert_eq!(
            request_header_has_beta(&transport.seen_headers(transport.seen_count() - 1), tbc),
            expected_tbc,
            "TBC admission must follow selected profile {profile} without leaking the previous route"
        );
    }
    assert_eq!(transport.seen_count(), 2);
}

#[tokio::test]
async fn ordinary_context_1m_retry_failure_does_not_commit_session_latch() {
    let _thinking_env_lock = THINKING_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _restore = CacheEnvRestore::clear(&["CLAUDE_CODE_EXTRA_BODY"]);
    let transport = FakeTransport::sequence(vec![
        context_1m_refusal_response("The context-1m-2025-08-07 beta is not supported"),
        context_1m_refusal_response("The context-1m-2025-08-07 beta is still not supported"),
    ]);
    let model = "claude-sonnet-4-20250514[1m]";
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

    assert!(service
        .execute_non_stream_request(
            request,
            crate::NonStreamingRequestClass::Main,
            crate::NonStreamingRetryOptions::default(),
        )
        .await
        .is_err());
    assert_eq!(transport.seen_count(), 2);
    assert!(!scope.context_1m_refused(&key));
}

#[tokio::test]
async fn model_validation_retries_only_the_native_context_1m_repair() {
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
    let service = make_adapter_for_protocol_with_transport(
        ProtocolFamily::AnthropicMessages,
        ProviderId::AnthropicFirstParty,
        "https://proxy.example.test",
        "proxy",
        "claude-sonnet-4-20250514",
        transport.clone(),
    )
    .with_custom_cli_betas(vec![
        crate::model::context_window::CONTEXT_1M_BETA_HEADER.to_string()
    ]);
    let mut request = LlmRequest::new("claude-sonnet-4-20250514").with_user_text("Hi");
    request.profile = Some("proxy".into());
    request.input.max_tokens = Some(1);

    service
        .execute_model_validation_request(request)
        .await
        .expect("the one Native context-beta repair succeeds");
    assert_eq!(transport.seen_count(), 2);
    let first = transport.seen_headers(0);
    let second = transport.seen_headers(1);
    let has_context_beta = |headers: &BTreeMap<String, String>| {
        headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("anthropic-beta"))
            .is_some_and(|(_, value)| {
                value
                    .split(',')
                    .map(str::trim)
                    .any(|beta| beta == crate::model::context_window::CONTEXT_1M_BETA_HEADER)
            })
    };
    assert!(has_context_beta(&first));
    assert!(!has_context_beta(&second));
    assert!(
        !second.contains_key(DISPATCH_ID_HEADER),
        "the repair remains auxiliary"
    );
}

#[tokio::test]
async fn model_validation_does_not_resend_context_1m_to_native_first_party() {
    let _env_lock = CACHE_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _restore = CacheEnvRestore::clear(&[
        "ANTHROPIC_BASE_URL",
        "_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL",
    ]);
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
    let service = make_adapter(transport.clone()).with_custom_cli_betas(vec![
        crate::model::context_window::CONTEXT_1M_BETA_HEADER.to_string(),
    ]);
    let mut request = LlmRequest::new("claude-sonnet-4-20250514").with_user_text("Hi");
    request.input.max_tokens = Some(1);

    assert!(service
        .execute_model_validation_request(request)
        .await
        .is_err());
    assert_eq!(transport.seen_count(), 1);
}

#[test]
fn build_request_metadata_threaded_when_set() {
    let _g = THINKING_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_thinking_env();
    let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let adapter = make_request_adapter(transport).with_request_metadata(crate::RequestMetadata {
        user_id: "{\"session_id\":\"s1\"}".to_string(),
    });
    let req = adapter
        .build_request(
            "claude-opus-4-8",
            None,
            None,
            vec![],
            vec![],
            false,
            None,
            false,
            PromptCacheQuerySource::Unspecified,
        )
        .expect("build_request");
    assert_eq!(
        req.input.metadata,
        provider_payload(serde_json::json!({
            "user_id": "{\"session_id\":\"s1\"}"
        }))
    );

    // Default adapter → no metadata.
    let bare = make_request_adapter(FakeTransport::always(ProviderResponse::json(
        200,
        ok_response_json(),
    )))
    .build_request(
        "claude-opus-4-8",
        None,
        None,
        vec![],
        vec![],
        false,
        None,
        false,
        PromptCacheQuerySource::Unspecified,
    )
    .expect("build_request");
    assert_eq!(bare.input.metadata.as_json_source(), "null");
    clear_thinking_env();
}

#[test]
fn build_api_metadata_user_id_shapes_and_orders() {
    // Serialize env access (CLAUDE_CODE_EXTRA_METADATA) with the other
    // env-mutating tests in this module.
    let _g = THINKING_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("CLAUDE_CODE_EXTRA_METADATA");
    // No extra → exactly the three canonical keys, in order, compact JSON.
    assert_eq!(
        ApiService::build_api_metadata_user_id("dev123", "acct-9", "sess-1", None),
        r#"{"device_id":"dev123","account_uuid":"acct-9","session_id":"sess-1"}"#
    );
    // Empty account_uuid (the `?? ''` branch) still emits the key.
    assert_eq!(
        ApiService::build_api_metadata_user_id("d", "", "s", None),
        r#"{"device_id":"d","account_uuid":"","session_id":"s"}"#
    );
    // Valid extra object is spread FIRST; a colliding key keeps its first
    // position but takes the canonical value (JS `{...extra, device_id,…}`).
    std::env::set_var(
        "CLAUDE_CODE_EXTRA_METADATA",
        r#"{"team":"core","device_id":"override"}"#,
    );
    assert_eq!(
        ApiService::build_api_metadata_user_id("dev", "acct", "sess", None),
        r#"{"team":"core","device_id":"dev","account_uuid":"acct","session_id":"sess"}"#
    );
    // Invalid extra (not a JSON object) is ignored.
    std::env::set_var("CLAUDE_CODE_EXTRA_METADATA", "not json");
    assert_eq!(
        ApiService::build_api_metadata_user_id("d", "a", "s", None),
        r#"{"device_id":"d","account_uuid":"a","session_id":"s"}"#
    );
    assert_eq!(
        ApiService::build_api_metadata_user_id("d", "a", "s", Some("parent")),
        r#"{"device_id":"d","account_uuid":"a","session_id":"s","parent_session_id":"parent"}"#
    );
    std::env::remove_var("CLAUDE_CODE_EXTRA_METADATA");
}

#[test]
fn build_api_metadata_user_id_reloads_extra_metadata_each_call() {
    let _g = THINKING_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("CLAUDE_CODE_EXTRA_METADATA", r#"{"team":"first"}"#);
    assert_eq!(
        ApiService::build_api_metadata_user_id("dev", "acct", "sess", None),
        r#"{"team":"first","device_id":"dev","account_uuid":"acct","session_id":"sess"}"#
    );

    std::env::set_var("CLAUDE_CODE_EXTRA_METADATA", r#"{"team":"second"}"#);
    assert_eq!(
        ApiService::build_api_metadata_user_id("dev", "acct", "sess", None),
        r#"{"team":"second","device_id":"dev","account_uuid":"acct","session_id":"sess"}"#
    );

    std::env::remove_var("CLAUDE_CODE_EXTRA_METADATA");
}

#[test]
fn fallback_signature_stripping_only_changes_assistant_authenticated_blocks() {
    let text = || crate::ContentBlock::Text {
        text: "keep".to_string(),
        cache_control: None,
        citations: None,
    };
    let messages = vec![
        crate::Message {
            api_output_config: None,
            role: "assistant".to_string(),
            content: vec![
                text(),
                crate::ContentBlock::Reasoning {
                    text: "secret".to_string(),
                    signature: Some("sig".to_string()),
                },
                crate::ContentBlock::RedactedThinking {
                    data: "opaque".to_string(),
                },
                crate::ContentBlock::ConnectorText {
                    connector_text: "connector".to_string(),
                    signature: Some("sig".to_string()),
                },
            ],
        },
        crate::Message {
            api_output_config: None,
            role: "user".to_string(),
            content: vec![crate::ContentBlock::Reasoning {
                text: "leave non-assistant untouched".to_string(),
                signature: Some("sig".to_string()),
            }],
        },
    ];

    let (canonical, _) = crate::convert::history_input(
        "model",
        &messages,
        &[],
        &[],
        lingxi_llm_client::protocol::ProtocolFamily::AnthropicMessages,
    )
    .unwrap();
    let mut messages = canonical.messages;
    strip_signature_blocks(&mut messages);

    assert_eq!(
        messages[0].content,
        vec![lingxi_llm_client::protocol::ContentBlock::Text {
            text: "keep".into(),
            thought_signature: None,
            citations: None
        }]
    );
    assert!(matches!(
        messages[1].content.as_slice(),
        [lingxi_llm_client::protocol::ContentBlock::Thinking { .. }]
    ));
}

#[tokio::test]
async fn extra_body_merges_into_anthropic_body() {
    let _g = THINKING_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_thinking_env();
    std::env::remove_var("CLAUDE_CODE_EXTRA_BODY");

    let adapter = make_adapter(FakeTransport::always(ProviderResponse::json(
        200,
        ok_response_json(),
    )));
    let request = LlmRequest::new("claude-sonnet-4-20250514").with_user_text("hi");

    // Header policy retains Native top-level order; the public service's
    // conversation session adds its initial thread-create field at seal.
    let unset = body_after_inject(&adapter, &request).await;
    assert_eq!(
        unset.as_json_source(),
        r#"{"model":"claude-sonnet-4-20250514","messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}],"tools":[],"max_tokens":32000,"thread":{"type":"create"}}"#,
        "unset extra body retains the Native main-template source"
    );

    // Caller fields cross the public dispatch boundary before the session
    // appends its thread field, retaining Native's complete source order.
    std::env::set_var("CLAUDE_CODE_EXTRA_BODY", r#"{"foo":"bar"}"#);
    let dispatched = body_after_inject(&adapter, &request).await;
    assert_eq!(
        dispatched.as_json_source(),
        r#"{"model":"claude-sonnet-4-20250514","messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}],"tools":[],"max_tokens":32000,"foo":"bar","thread":{"type":"create"}}"#,
        "extra fields retain the public Native main-template source"
    );

    // A different output ceiling is a pre-seal merge-policy check: sealing
    // separately enforces the SDK's already resolved output-budget fence.
    // The collision must replace the value without moving its original key.
    std::env::set_var("CLAUDE_CODE_EXTRA_BODY", r#"{"foo":"bar","max_tokens":99}"#);
    let mut captured = request.clone();
    captured.execution.resolve_native_effort = true;
    crate::anthropic_extra_body::capture(&mut captured).unwrap();
    let mut prepared = adapter.client.prepare(&captured).await.unwrap();
    assert_eq!(
        request_body_value(&prepared.provider_request.body_json)["max_tokens"],
        serde_json::json!(32000)
    );
    adapter
        .inject_headers(&mut prepared, DispatchHeaderState::default())
        .unwrap();
    let merged = prepared.provider_request.body_json;
    assert_eq!(
        merged.as_json_source(),
        r#"{"model":"claude-sonnet-4-20250514","messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}],"tools":[],"max_tokens":99,"foo":"bar"}"#,
        "the pre-seal override retains the complete Native source order"
    );
    let merged_value = request_body_value(&merged);
    assert_eq!(merged_value["foo"], serde_json::json!("bar"));
    assert_eq!(merged_value["max_tokens"], serde_json::json!(99));
    let keys: Vec<&String> = merged_value.as_object().unwrap().keys().collect();
    let mt = keys
        .iter()
        .position(|k| *k == "max_tokens")
        .expect("max_tokens");
    let foo = keys.iter().position(|k| *k == "foo").expect("foo");
    assert!(
        mt < foo,
        "colliding key keeps its position; new key appends at the tail: {keys:?}"
    );

    std::env::remove_var("CLAUDE_CODE_EXTRA_BODY");
    clear_thinking_env();
}

#[tokio::test]
async fn extra_body_reloads_each_call() {
    let _g = THINKING_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_thinking_env();

    let adapter = make_adapter(FakeTransport::always(ProviderResponse::json(
        200,
        ok_response_json(),
    )));
    let request = LlmRequest::new("claude-sonnet-4-20250514").with_user_text("hi");

    std::env::set_var("CLAUDE_CODE_EXTRA_BODY", r#"{"foo":"first"}"#);
    let first = body_after_inject(&adapter, &request).await;
    assert_eq!(
        request_body_value(&first)["foo"],
        serde_json::json!("first")
    );

    std::env::set_var("CLAUDE_CODE_EXTRA_BODY", r#"{"foo":"second"}"#);
    let second = body_after_inject(&adapter, &request).await;
    assert_eq!(
        request_body_value(&second)["foo"],
        serde_json::json!("second")
    );

    std::env::remove_var("CLAUDE_CODE_EXTRA_BODY");
    clear_thinking_env();
}

#[tokio::test]
async fn extra_body_non_object_or_invalid_is_ignored() {
    let _g = THINKING_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_thinking_env();
    std::env::remove_var("CLAUDE_CODE_EXTRA_BODY");

    let adapter = make_adapter(FakeTransport::always(ProviderResponse::json(
        200,
        ok_response_json(),
    )));
    let request = LlmRequest::new("claude-sonnet-4-20250514").with_user_text("hi");
    let baseline = body_after_inject(&adapter, &request).await;

    // (3) non-object JSON values are ignored (error logged, body unchanged).
    for val in ["[1]", "\"x\"", "5", "true", "null"] {
        std::env::set_var("CLAUDE_CODE_EXTRA_BODY", val);
        let body = body_after_inject(&adapter, &request).await;
        assert_eq!(body, baseline, "non-object extra body ignored: {val}");
    }
    // (4) unparseable JSON is ignored (parse-error logged, body unchanged).
    std::env::set_var("CLAUDE_CODE_EXTRA_BODY", "{not json");
    let body = body_after_inject(&adapter, &request).await;
    assert_eq!(body, baseline, "invalid JSON ignored");
    // Empty string is falsy → silent no-op.
    std::env::set_var("CLAUDE_CODE_EXTRA_BODY", "");
    let body = body_after_inject(&adapter, &request).await;
    assert_eq!(body, baseline, "empty env value ignored");

    std::env::remove_var("CLAUDE_CODE_EXTRA_BODY");
    clear_thinking_env();
}

#[tokio::test]
async fn side_effort_beta_observes_resolution_snapshot_instead_of_stale_wire_field() {
    let adapter = make_adapter(FakeTransport::always(ProviderResponse::json(
        200,
        ok_response_json(),
    )));
    let request = LlmRequest::new("claude-sonnet-4-20250514").with_user_text("hi");
    let mut prepared = adapter.client.prepare(&request).await.unwrap();
    prepared.anthropic_request_kind =
        lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::SideQuery;
    let output_config =
        lingxi_llm_client::protocol::JsonPayload::parse(r#"{"effort":"high"}"#).unwrap();
    prepared.provider_request.body_json = prepared
        .provider_request
        .body_json
        .with_object_field("output_config", &output_config)
        .unwrap();
    prepared.effort_policy = Some(
        lingxi_llm_client::providers::anthropic::request_policy::AnthropicEffortPolicy {
            supported: true,
            value: None,
        },
    );
    assert!(
        !adapter.beta_context(&prepared).effort,
        "native side query without hook effort does not call YMe, even when an SDK base contains effort"
    );
    prepared.effort_policy.as_mut().unwrap().value =
        Some(lingxi_llm_client::protocol::JsonPayload::parse("7").unwrap());
    assert!(
        !adapter.beta_context(&prepared).effort,
        "native numeric resolution also emits no effort beta"
    );
    prepared.effort_policy.as_mut().unwrap().value =
        Some(lingxi_llm_client::protocol::JsonPayload::string("low"));
    assert!(adapter.beta_context(&prepared).effort);
}

#[tokio::test]
async fn main_extra_body_output_config_removes_unsupported_effort() {
    let _g = THINKING_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_thinking_env();
    std::env::remove_var("CLAUDE_CODE_EXTRA_BODY");

    let adapter = make_adapter(FakeTransport::always(ProviderResponse::json(
        200,
        ok_response_json(),
    )));
    // The encoder emits a computed `output_config.effort` from request.effort.
    let mut request = LlmRequest::new("claude-sonnet-4-20250514").with_user_text("hi");
    request.set_effort(Some(serde_json::json!("high"))).unwrap();

    // Native YMe deletes effort before inspecting explicit presence when
    // the selected model is unsupported, while retaining other fields.
    std::env::set_var(
        "CLAUDE_CODE_EXTRA_BODY",
        r#"{"output_config":{"effort":"low","format":"json"}}"#,
    );
    let body = body_after_inject(&adapter, &request).await;
    assert!(body.raw_source_at("/output_config/effort").is_none());
    assert_eq!(
        body.string_at("/output_config/format").as_deref(),
        Some("json"),
        "extra-body-only output_config keys are merged in"
    );

    std::env::remove_var("CLAUDE_CODE_EXTRA_BODY");
    clear_thinking_env();
}

#[tokio::test]
async fn native_structured_output_hun_obeys_typed_disable_gates_and_preserves_extra_format() {
    let _cache_env_lock = CACHE_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _thinking_env_lock = THINKING_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _restore = CacheEnvRestore::clear(&[
        "ANTHROPIC_BASE_URL",
        "CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS",
        "CLAUDE_CODE_DISABLE_STRUCTURED_OUTPUTS",
        "CLAUDE_CODE_EXTRA_BODY",
    ]);

    let model = "claude-sonnet-4-5-20250929";
    let adapter = make_adapter_for_protocol_with_transport(
        ProtocolFamily::AnthropicMessages,
        ProviderId::AnthropicFirstParty,
        "https://api.anthropic.com",
        "native-hun",
        model,
        FakeTransport::always(ProviderResponse::json(200, ok_response_json())),
    )
    .with_native_anthropic_endpoint("https://api.anthropic.com");
    let make_schema_request = || {
        let mut request = LlmRequest::new(model).with_user_text("Return JSON.");
        request.input.output_format = lingxi_llm_client::protocol::OutputFormat::JsonSchema {
            name: "response".into(),
            schema: provider_payload(serde_json::json!({
                "type": "object",
                "properties": {"ok": {"type": "boolean"}},
                "additionalProperties": false
            })),
            strict: true,
        };
        request
    };

    std::env::remove_var("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS");
    for (raw_disabled, schema_enabled) in [
        ("0", true),
        ("false", true),
        ("", true),
        ("wat", true),
        ("true", false),
        ("1", false),
        ("yes", false),
        ("on", false),
    ] {
        std::env::set_var(branding::DISABLE_STRUCTURED_OUTPUTS_ENV, raw_disabled);
        assert_eq!(
            crate::structured_output::bool_environment(branding::DISABLE_STRUCTURED_OUTPUTS_ENV),
            !schema_enabled,
            "both Native HUn and the existing typed helper parse {raw_disabled:?} the same way"
        );
        let mut prepared = adapter
            .client
            .prepare(&make_schema_request())
            .await
            .expect("HUn applies the Native M.bool gate before schema validation");
        assert_eq!(
            prepared
                .provider_request
                .body_json
                .raw_source_at("/output_config/format")
                .is_some(),
            schema_enabled,
            "schema admission follows Native M.bool for {raw_disabled:?}"
        );
        adapter
            .inject_headers(&mut prepared, DispatchHeaderState::default())
            .unwrap();
        assert_eq!(
            request_header_has_beta(
                &prepared.provider_request.headers,
                crate::model::betas::STRUCTURED_OUTPUTS,
            ),
            schema_enabled,
            "the late beta gate follows the same Native HUn result for {raw_disabled:?}"
        );
    }

    std::env::remove_var(branding::DISABLE_STRUCTURED_OUTPUTS_ENV);
    let mut prepared = adapter
        .client
        .prepare(&make_schema_request())
        .await
        .expect("an unset structured-output disable variable admits the schema");
    assert!(prepared
        .provider_request
        .body_json
        .raw_source_at("/output_config/format")
        .is_some());
    adapter
        .inject_headers(&mut prepared, DispatchHeaderState::default())
        .unwrap();
    assert!(request_header_has_beta(
        &prepared.provider_request.headers,
        crate::model::betas::STRUCTURED_OUTPUTS,
    ));

    for raw_enabled in ["false", "0", "wat", ""] {
        std::env::set_var("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS", raw_enabled);
        let mut prepared = adapter
            .client
            .prepare(&make_schema_request())
            .await
            .expect("falsey or unrecognized M.bool values leave HUn enabled");
        assert!(prepared
            .provider_request
            .body_json
            .raw_source_at("/output_config/format")
            .is_some());
        adapter
            .inject_headers(&mut prepared, DispatchHeaderState::default())
            .unwrap();
        assert!(
            request_header_has_beta(
                &prepared.provider_request.headers,
                crate::model::betas::STRUCTURED_OUTPUTS,
            ),
            "Native M.bool leaves the schema beta enabled for {raw_enabled:?}"
        );
    }

    for raw_disabled in ["true", "1", "yes", "on"] {
        std::env::set_var("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS", raw_disabled);
        let mut prepared = adapter
            .client
            .prepare(&make_schema_request())
            .await
            .expect("Native M.bool B6 suppresses schema before SDK validation");
        assert!(prepared
            .provider_request
            .body_json
            .raw_source_at("/output_config/format")
            .is_none());
        adapter
            .inject_headers(&mut prepared, DispatchHeaderState::default())
            .unwrap();
        assert!(!request_header_has_beta(
            &prepared.provider_request.headers,
            crate::model::betas::STRUCTURED_OUTPUTS,
        ));
    }

    std::env::remove_var("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS");
    let mut prepared = adapter
        .client
        .prepare(&make_schema_request())
        .await
        .expect("schema is admitted before the late Host policy runs");
    assert!(prepared
        .provider_request
        .body_json
        .raw_source_at("/output_config/format")
        .is_some());
    std::env::set_var("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS", "0");
    adapter
        .inject_headers(&mut prepared, DispatchHeaderState::default())
        .unwrap();
    assert!(prepared
        .provider_request
        .body_json
        .raw_source_at("/output_config/format")
        .is_some());
    assert!(
        request_header_has_beta(
            &prepared.provider_request.headers,
            crate::model::betas::STRUCTURED_OUTPUTS,
        ),
        "M.bool treats late value 0 as false, so HUn adds the beta"
    );

    let mut prepared = adapter
        .client
        .prepare(&make_schema_request())
        .await
        .expect("schema is admitted before the late Host policy runs");
    assert!(prepared
        .provider_request
        .body_json
        .raw_source_at("/output_config/format")
        .is_some());
    std::env::set_var("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS", "true");
    adapter
        .inject_headers(&mut prepared, DispatchHeaderState::default())
        .unwrap();
    assert!(prepared
        .provider_request
        .body_json
        .raw_source_at("/output_config/format")
        .is_some());
    assert!(
        !request_header_has_beta(
            &prepared.provider_request.headers,
            crate::model::betas::STRUCTURED_OUTPUTS,
        ),
        "a true value added after preparation still suppresses the late beta"
    );

    std::env::set_var("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS", "false");
    std::env::set_var(
        "CLAUDE_CODE_EXTRA_BODY",
        r#"{"output_config":{"format":{"type":"json_schema","name":"explicit","schema":{"type":"object"},"strict":true}}}"#,
    );
    let request = LlmRequest::new(model).with_user_text("Keep explicit format.");
    let sent = request_after_service_dispatch(&adapter, &request).await;
    assert_eq!(
        sent.body_json
            .string_at("/output_config/format/name")
            .as_deref(),
        Some("explicit"),
        "Native B6 does not delete an explicit extra-body format"
    );
    assert!(!request_header_has_beta(
        &sent.headers,
        crate::model::betas::STRUCTURED_OUTPUTS,
    ));

    std::env::set_var("CLAUDE_CODE_EXTRA_BODY", r#"{"betas":["caller-only"]}"#);
    let mut captured = adapter.build_json_schema_request_with_thinking(
        model,
        None,
        None,
        None,
        vec![text_user_msg("Return JSON.")],
        serde_json::json!({"type":"object","properties":{"ok":{"type":"boolean"}},"additionalProperties":false}),
        Some(8192),
        None,
        Some(crate::model::thinking::ThinkingConfig::Disabled),
        None,
        None,
    ).unwrap();
    // Inspect the main-only pre-spread candidate snapshot using the
    // public builder's saved body, without rereading the environment.
    captured.execution.anthropic_request_kind =
        lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::Main;
    crate::anthropic_extra_body::capture(&mut captured).unwrap();
    let mut prepared = adapter
        .client
        .prepare(&captured)
        .await
        .expect("Native output format is present before the caller beta spread");
    adapter
        .inject_headers(&mut prepared, DispatchHeaderState::default())
        .unwrap();
    assert!(
        prepared
            .native_beta_rejection_candidates
            .iter()
            .any(|beta| beta == crate::model::betas::STRUCTURED_OUTPUTS),
        "structured beta is in Native's pre-spread Ww/oT candidate snapshot"
    );
    assert!(!prepared
        .native_beta_rejection_candidates
        .iter()
        .any(|beta| beta == "caller-only"));
    assert!(!request_header_has_beta(
        &prepared.provider_request.headers,
        crate::model::betas::STRUCTURED_OUTPUTS,
    ));
}

#[tokio::test]
async fn extra_body_speed_computed_wins() {
    let _g = THINKING_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_thinking_env();
    std::env::remove_var("CLAUDE_CODE_EXTRA_BODY");

    let adapter = make_adapter(FakeTransport::always(ProviderResponse::json(
        200,
        ok_response_json(),
    )));
    // The encoder emits a computed top-level `speed` from request.speed
    // (fast mode). claude-code spreads the extra body (`...Vs`) BEFORE the
    // computed `...{speed:ze}`, so the computed speed wins over any extra one.
    let request = LlmRequest::new("claude-sonnet-4-20250514").with_user_text("hi");
    let mut prepared = adapter.client.prepare(&request).await.expect("prepare");
    let computed_speed = lingxi_llm_client::protocol::JsonPayload::string("fast");
    prepared.provider_request.body_json = prepared
        .provider_request
        .body_json
        .with_object_field("speed", &computed_speed)
        .unwrap();

    std::env::set_var("CLAUDE_CODE_EXTRA_BODY", r#"{"speed":"slow"}"#);
    adapter.merge_extra_body(&mut prepared).unwrap();
    let body = prepared.provider_request.body_json;
    assert_eq!(
        body.string_at("/speed").as_deref(),
        Some("fast"),
        "computed speed wins over the extra body's speed"
    );
    prepared.provider_request.body_json = body;
    adapter.enforce_fast_route(&mut prepared).unwrap();
    assert!(
        prepared
            .provider_request
            .body_json
            .raw_source_at("/speed")
            .is_none(),
        "the final capability gate strips speed from unsupported models"
    );

    std::env::remove_var("CLAUDE_CODE_EXTRA_BODY");
    clear_thinking_env();
}

#[tokio::test]
async fn extra_body_cannot_reintroduce_fast_mode_on_a_custom_route() {
    let _g = THINKING_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("CLAUDE_CODE_EXTRA_BODY", r#"{"speed":"fast"}"#);
    let adapter = make_adapter_for_protocol(
        ProtocolFamily::AnthropicMessages,
        ProviderId::AnthropicFirstParty,
        "https://gateway.example",
    );
    let mut request = LlmRequest::new("model").with_user_text("hi");
    request.input.max_tokens = Some(4096);
    let body = body_after_inject(&adapter, &request).await;
    assert!(body.raw_source_at("/speed").is_none());
    std::env::remove_var("CLAUDE_CODE_EXTRA_BODY");
}

#[tokio::test]
async fn insecure_anthropic_host_is_not_a_first_party_fast_route() {
    let adapter = make_adapter_for_protocol(
        ProtocolFamily::AnthropicMessages,
        ProviderId::AnthropicFirstParty,
        "http://api.anthropic.com",
    );
    let mut request = LlmRequest::new("model").with_user_text("hi");
    request.input.max_tokens = Some(4096);
    let prepared = adapter.client.prepare(&request).await.expect("prepare");
    assert!(!ApiService::direct_anthropic_api_route(&prepared));
}

#[test]
fn extra_body_anthropic_beta_append_dedupe() {
    let _g = THINKING_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("CLAUDE_CODE_EXTRA_BODY");

    // (6) pre-existing array → keeps its entries, appends only missing betas.
    std::env::set_var(
        "CLAUDE_CODE_EXTRA_BODY",
        r#"{"anthropic_beta":["keep-1","dup"]}"#,
    );
    let r = ApiService::parse_extra_body(&["dup".to_string(), "new-1".to_string()]);
    assert_eq!(
        r["anthropic_beta"],
        serde_json::json!(["keep-1", "dup", "new-1"]),
        "existing entries kept in order; only missing betas appended"
    );

    // No pre-existing array → set to the beta list; other keys untouched.
    std::env::set_var("CLAUDE_CODE_EXTRA_BODY", r#"{"x":1}"#);
    let r2 = ApiService::parse_extra_body(&["a".to_string(), "b".to_string()]);
    assert_eq!(r2["anthropic_beta"], serde_json::json!(["a", "b"]));
    assert_eq!(r2["x"], serde_json::json!(1));

    // Empty betas arg → anthropic_beta is never synthesized.
    let r3 = ApiService::parse_extra_body(&[]);
    assert!(r3.get("anthropic_beta").is_none());

    std::env::remove_var("CLAUDE_CODE_EXTRA_BODY");
}

#[test]
fn extra_body_raw_parser_and_cleanup_match_current_native() {
    let _g = THINKING_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let original = std::env::var_os("CLAUDE_CODE_EXTRA_BODY");
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("../../tests/fixtures/extra_body_2_1_287.json")).unwrap();
    for case in fixture["rawCases"].as_array().unwrap() {
        std::env::set_var("CLAUDE_CODE_EXTRA_BODY", case["raw"].as_str().unwrap());
        assert_eq!(
            request_body_value(&extra_body_object().unwrap().unwrap_or_else(|| {
                lingxi_llm_client::protocol::JsonPayload::parse("{}")
                    .expect("empty object is valid JSON")
            }),),
            case["expected"],
            "{case}"
        );
    }
    std::env::set_var("CLAUDE_CODE_EXTRA_BODY", r#"{"betas":{"toString":null}}"#);
    assert!(matches!(
        extra_body_object(),
        Err(LlmError::InvalidRequest { .. })
    ));
    match original {
        Some(value) => std::env::set_var("CLAUDE_CODE_EXTRA_BODY", value),
        None => std::env::remove_var("CLAUDE_CODE_EXTRA_BODY"),
    }
}

#[tokio::test]
async fn bedrock_tool_search_beta_is_in_request_body() {
    if lingxi_core::host::env::is_env_truthy(
        std::env::var("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS")
            .ok()
            .as_deref(),
    ) {
        return;
    }
    let _g = THINKING_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("CLAUDE_CODE_EXTRA_BODY");
    let adapter = make_adapter_for_protocol(
        ProtocolFamily::BedrockClaude,
        ProviderId::OpenAICompatible {
            name: "bedrock-claude".to_string(),
        },
        "https://bedrock-runtime.us-east-1.amazonaws.com",
    );
    let mut request = LlmRequest::new("model").with_user_text("hi");
    request.input.max_tokens = Some(4096);
    request.input.tools.push(serde_json::from_value(serde_json::json!({"name":"ToolSearch", "description":"discover tools", "input_schema":{"type":"object"}})).unwrap());
    let body = body_after_inject(&adapter, &request).await;
    assert!(body.array_len("/anthropic_beta").is_some_and(|len| {
        (0..len).any(|index| {
            body.array_item_at("/anthropic_beta", index)
                .and_then(|beta| beta.string_at(""))
                .as_deref()
                == Some(crate::model::betas::TOOL_SEARCH_TOOL_3P)
        })
    }));
    assert!(body.raw_source_at("/anthropic-beta").is_none());
}

#[tokio::test]
async fn extra_body_not_applied_to_non_anthropic_route() {
    let _g = THINKING_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_thinking_env();
    std::env::set_var("CLAUDE_CODE_EXTRA_BODY", r#"{"foo":"bar"}"#);

    // (7) OpenAI route: the body must be untouched by the extra-body merge.
    let adapter = make_adapter_for_protocol(
        ProtocolFamily::OpenAiChat,
        ProviderId::OpenAICompatible {
            name: "openai".to_string(),
        },
        "https://api.openai.com/v1",
    );
    let request = LlmRequest::new("model").with_user_text("hi");
    let body = body_after_inject(&adapter, &request).await;
    assert!(
        body.raw_source_at("/foo").is_none(),
        "extra body must not touch non-Anthropic routes: {}",
        body.as_json_source()
    );

    std::env::remove_var("CLAUDE_CODE_EXTRA_BODY");
    clear_thinking_env();
}

#[tokio::test]
async fn headless_admission_preserves_native_effort_context_and_text_display_with_base_override() {
    let _display_env = CacheEnvRestore::clear(&[branding::THINKING_DISPLAY_UPDATES_ENV]);
    let adapter = make_adapter_for_protocol_with_transport(
        ProtocolFamily::AnthropicMessages,
        ProviderId::AnthropicFirstParty,
        "http://configured-anthropic-base.test",
        "native",
        "claude-sonnet-5-5",
        FakeTransport::always(ProviderResponse::json(200, ok_response_json())),
    )
    .with_anthropic_client_metadata(
        lingxi_llm_client::providers::anthropic::request_policy::AnthropicClientMetadata::
            claude_code_2_1_293("darwin", "arm64"),
    )
    .with_native_anthropic_endpoint("http://configured-anthropic-base.test")
    .with_native_thinking_display(
        lingxi_llm_client::providers::anthropic::thinking_display::ThinkingDisplayPolicy {
            omit_default: true,
            ..Default::default()
        },
    )
    .with_interactive_session(false);
    let request = adapter
        .build_request(
            "claude-sonnet-5-5",
            None,
            None,
            vec![text_user_msg("Return marker.")],
            vec![],
            true,
            None,
            false,
            PromptCacheQuerySource::Unspecified,
        )
        .unwrap();
    assert!(request.execution.resolve_native_effort);
    assert!(adapter
        .is_first_party_route("claude-sonnet-5-5", None)
        .unwrap());
    assert!(
        !adapter
            .client
            .fast_account_identity("claude-sonnet-5-5", None)
            .unwrap()
            .first_party
    );
    let mut prepared = adapter.client.prepare(&request).await.unwrap();
    adapter
        .inject_headers(&mut prepared, DispatchHeaderState::default())
        .unwrap();
    let body = request_body_value(&prepared.provider_request.body_json);
    assert_eq!(body["thinking"]["display"], "omitted");
    assert_eq!(body["output_config"]["effort"], "medium");
    assert_eq!(
        body["messages"][1],
        serde_json::json!({"role":"system","content":[],"output_config":{"effort":"medium"}})
    );
    let betas: Vec<_> = prepared.provider_request.headers["anthropic-beta"]
        .split(',')
        .collect();
    assert!(
        betas
            .iter()
            .position(|beta| *beta == crate::model::betas::MID_CONVERSATION_SYSTEM)
            .unwrap()
            < betas
                .iter()
                .position(|beta| *beta == crate::model::betas::PER_TURN_CONTROL)
                .unwrap()
    );
    assert_eq!(
        body["context_management"],
        serde_json::json!({"edits":[{"type":"clear_thinking_20251015","keep":"all"}]})
    );

    assert!(!prepared.provider_request.headers["anthropic-beta"]
        .contains("thinking-display-updates-2026-08-18"));
}

#[tokio::test]
async fn native_295_per_turn_effort_matches_prepared_first_party_and_foundry_requests() {
    let _guard = CACHE_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _env = CacheEnvRestore::clear(&[
        "CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS",
        "DISABLE_INTERLEAVED_THINKING",
        "CLAUDE_CODE_FORCE_MID_CONVERSATION_SYSTEM",
        "CLAUDE_CODE_SIMULATE_PROXY_USAGE",
        "CLAUDE_CODE_EXTRA_BODY",
        branding::EFFORT_LEVEL_ENV,
        branding::THINKING_DISPLAY_UPDATES_ENV,
        "DISABLE_PROMPT_CACHING",
    ]);
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/native_per_turn_control_2_1_295_print_44.json"
    ))
    .expect("captured Native fixture parses");

    // One default and one beta-disabled row per provider and model. This
    // exercises the actual host-built PreparedLlmCall consumed by the
    // protocol codec, rather than testing only the beta-list helper.
    for matrix_row in [264_u64, 266, 308, 310, 660, 662, 704, 706] {
        let case = fixture["cases"]
            .as_array()
            .expect("capture cases")
            .iter()
            .find(|case| case["matrix_row"].as_u64() == Some(matrix_row))
            .unwrap_or_else(|| panic!("missing Native capture row {matrix_row}"));
        let input = &case["input"];
        for key in [
            "CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS",
            "DISABLE_INTERLEAVED_THINKING",
            "CLAUDE_CODE_FORCE_MID_CONVERSATION_SYSTEM",
        ] {
            if let Some(value) = input["env"][key].as_str() {
                std::env::set_var(key, value);
            } else {
                std::env::remove_var(key);
            }
        }

        let model = input["model"].as_str().expect("request model");
        let first_party = input["provider"] == "firstParty";
        let (protocol, provider_id, profile_name, base_url) = if first_party {
            (
                ProtocolFamily::AnthropicMessages,
                ProviderId::AnthropicFirstParty,
                "native",
                "http://configured-anthropic-base.test",
            )
        } else {
            (
                ProtocolFamily::FoundryClaude,
                ProviderId::OpenAICompatible {
                    name: "foundry-claude".to_owned(),
                },
                "foundry-claude",
                "https://example.services.ai.azure.com/anthropic",
            )
        };
        let mut adapter = make_adapter_for_protocol_with_transport(
            protocol,
            provider_id,
            base_url,
            profile_name,
            model,
            FakeTransport::always(ProviderResponse::json(200, ok_response_json())),
        )
        .with_interactive_session(false);
        if first_party {
            adapter = adapter.with_native_anthropic_endpoint(base_url);
        }
        let mut request = adapter
            .build_request(
                model,
                None,
                None,
                vec![text_user_msg("Return marker.")],
                vec![],
                false,
                None,
                false,
                PromptCacheQuerySource::Unspecified,
            )
            .expect("build captured main request");
        // The real non-stream and stream drivers make this admission
        // immediately before ModelRuntime::prepare for every provider.
        request.execution.resolve_native_effort = true;
        let mut prepared = adapter
            .client
            .prepare(&request)
            .await
            .expect("prepare actual provider request");
        adapter
            .inject_headers(&mut prepared, DispatchHeaderState::default())
            .expect("inject production headers");

        let body = request_body_value(&prepared.provider_request.body_json);
        let actual_top_effort = body.pointer("/output_config/effort").cloned();
        let actual_message_effort = body["messages"]
            .as_array()
            .expect("encoded messages")
            .iter()
            .enumerate()
            .filter_map(|(index, message)| {
                let effort = message.pointer("/output_config/effort")?;
                Some(serde_json::json!({
                    "message_index": index,
                    "role": message["role"],
                    "effort": effort,
                }))
            })
            .collect::<Vec<_>>();
        let actual_header = prepared
            .provider_request
            .headers
            .get("anthropic-beta")
            .map(|value| format!("anthropic-beta: {value}\r\n"));
        let captured = &case["captured"];
        let expected_thinking: serde_json::Value = serde_json::from_str(
            captured["raw_body_fragments"]["thinking_utf8"]
                .as_str()
                .expect("captured thinking source"),
        )
        .expect("captured thinking JSON");
        assert_eq!(
            body["thinking"], expected_thinking,
            "row {matrix_row}: thinking display projection"
        );
        assert_eq!(
            actual_top_effort
                .as_ref()
                .and_then(serde_json::Value::as_str),
            captured["top_level_output_config_effort"].as_str(),
            "row {matrix_row}: top-level effort"
        );
        assert_eq!(
            &actual_message_effort,
            captured["per_message_effort_markers"]
                .as_array()
                .expect("captured markers"),
            "row {matrix_row}: per-message effort markers"
        );
        assert_eq!(
            actual_header.as_deref(),
            captured["beta_header_line_utf8"].as_str(),
            "row {matrix_row}: exact provider header line"
        );
    }
}

#[tokio::test]
async fn native_295_mctc_model_unsupported_retries_and_latches_only_canonical_model() {
    let _guard = CACHE_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _env = CacheEnvRestore::clear(&[
        "CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS",
        "DISABLE_INTERLEAVED_THINKING",
    ]);
    use lingxi_llm_client::providers::anthropic::beta_repair::Beta;
    use lingxi_llm_client::NativeAnthropicRequestContext;

    let first_model = "claude-opus-4-8-r30-model-unsupported-20261011";
    let transport = FakeTransport::sequence(vec![
        FakeResponse::Ok(ProviderResponse::json(
            400,
            serde_json::json!({
                "type": "error",
                "error": {
                    "type": "invalid_request_error",
                    "message": "The tool_addition block is not supported on this model"
                }
            }),
        )),
        FakeResponse::Ok(ProviderResponse::json(200, ok_response_json())),
    ]);
    let service = make_adapter_for_protocol_with_transport(
        ProtocolFamily::AnthropicMessages,
        ProviderId::AnthropicFirstParty,
        "https://api.anthropic.com",
        "native",
        first_model,
        transport.clone(),
    )
    .with_native_anthropic_endpoint("https://api.anthropic.com");
    let mut request = crate::MessagesCreateRequest::new(
        first_model,
        None,
        None,
        vec![text_user_msg("Return marker.")],
        vec![],
    );
    request.opts.max_output_tokens = Some(4096);
    request.opts.native_anthropic_request_context = Some(NativeAnthropicRequestContext {
        mid_conversation_tool_changes: true,
        mid_conversation_tool_changes_feature_gate: true,
        ..Default::default()
    });
    request.opts.fallback = crate::FallbackPolicy::Disabled;
    service
        .messages_create(request)
        .await
        .expect("model-unsupported MCTC response retries without the beta");

    let mctc = Beta::MidConversationToolChanges.header();
    assert_eq!(transport.seen_count(), 2);
    assert!(transport.seen_headers(0)["anthropic-beta"].contains(mctc));
    assert!(!transport.seen_headers(1)["anthropic-beta"].contains(mctc));

    // The Native request latch uses We(model), which canonicalizes model
    // family aliases/suffixes before its host-level model-only lookup.
    let same_model_transport =
        FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let same_model_alias = "claude-opus-4-8-r30-model-unsupported-alias";
    let same_model_service = make_adapter_for_protocol_with_transport(
        ProtocolFamily::AnthropicMessages,
        ProviderId::AnthropicFirstParty,
        "https://api.anthropic.com",
        "native",
        same_model_alias,
        same_model_transport.clone(),
    )
    .with_native_anthropic_endpoint("https://api.anthropic.com");
    let mut same_model_request = crate::MessagesCreateRequest::new(
        same_model_alias,
        None,
        None,
        vec![text_user_msg("Return marker.")],
        vec![],
    );
    same_model_request.opts.max_output_tokens = Some(4096);
    same_model_request.opts.native_anthropic_request_context =
        Some(NativeAnthropicRequestContext {
            mid_conversation_tool_changes: true,
            mid_conversation_tool_changes_feature_gate: true,
            ..Default::default()
        });
    same_model_service
        .messages_create(same_model_request)
        .await
        .expect("same canonical model request succeeds");
    assert!(!same_model_transport.seen_headers(0)["anthropic-beta"].contains(mctc));

    let other_model_transport =
        FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let other_model = "claude-sonnet-5-5-r30-distinct-model";
    let other_model_service = make_adapter_for_protocol_with_transport(
        ProtocolFamily::AnthropicMessages,
        ProviderId::AnthropicFirstParty,
        "https://api.anthropic.com",
        "native",
        other_model,
        other_model_transport.clone(),
    )
    .with_native_anthropic_endpoint("https://api.anthropic.com");
    let mut other_model_request = crate::MessagesCreateRequest::new(
        other_model,
        None,
        None,
        vec![text_user_msg("Return marker.")],
        vec![],
    );
    other_model_request.opts.max_output_tokens = Some(4096);
    other_model_request.opts.native_anthropic_request_context =
        Some(NativeAnthropicRequestContext {
            mid_conversation_tool_changes: true,
            mid_conversation_tool_changes_feature_gate: true,
            ..Default::default()
        });
    other_model_service
        .messages_create(other_model_request)
        .await
        .expect("different model remains eligible");
    assert!(other_model_transport.seen_headers(0)["anthropic-beta"].contains(mctc));

    // Foundry cannot encode tool-change rows even with explicit host feature gates.
    let foundry_transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let foundry_service = make_adapter_for_protocol_with_transport(
        ProtocolFamily::FoundryClaude,
        ProviderId::OpenAICompatible {
            name: "foundry-claude".to_owned(),
        },
        "https://example.services.ai.azure.com/anthropic",
        "foundry-claude",
        same_model_alias,
        foundry_transport.clone(),
    );
    let mut foundry_request = crate::MessagesCreateRequest::new(
        same_model_alias,
        None,
        None,
        vec![text_user_msg("Return marker.")],
        vec![],
    );
    foundry_request.opts.max_output_tokens = Some(4096);
    foundry_request.opts.native_anthropic_request_context = Some(NativeAnthropicRequestContext {
        mid_conversation_tool_changes: true,
        mid_conversation_tool_changes_feature_gate: true,
        ..Default::default()
    });
    foundry_service
        .messages_create(foundry_request)
        .await
        .expect("Foundry accepts the request without tool-change betas");
    assert!(!foundry_transport.seen_headers(0)["anthropic-beta"].contains(mctc));
}

#[tokio::test]
async fn native_295_mctc_and_inline_beta_gates_are_independent_in_main_headers() {
    use lingxi_llm_client::providers::anthropic::beta_repair::Beta;
    use lingxi_llm_client::NativeAnthropicRequestContext;

    let _guard = CACHE_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    // CLAUDE_CODE_EXTRA_BODY writers hold THINKING_ENV_LOCK; a sibling's
    // `{"betas":{"toString":null}}` must not reach this capture.
    let _thinking_env = THINKING_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _env = CacheEnvRestore::clear(&[
        "CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS",
        "CLAUDE_CODE_EXTRA_BODY",
        "CLAUDE_CODE_FORCE_MID_CONVERSATION_SYSTEM",
        "DISABLE_INTERLEAVED_THINKING",
    ]);
    assert!(
        !lingxi_core::host::compliance_taints::is_tainted("hipaa"),
        "this fixture requires the initialized-empty compliance taint state"
    );

    let mctc = Beta::MidConversationToolChanges.header();
    let inline_tools = "inline-tools-2026-09-15";
    for (profile, mctc_gate, inline_gate, expect_mctc, expect_inline) in [
        ("native-gate-off", false, false, false, false),
        ("native-mctc-only", true, false, true, false),
        ("native-inline-on", true, true, true, true),
    ] {
        let model = "claude-sonnet-5-5-r62-feature-gate";
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let service = make_adapter_for_protocol_with_transport(
            ProtocolFamily::AnthropicMessages,
            ProviderId::AnthropicFirstParty,
            "https://api.anthropic.com",
            profile,
            model,
            transport.clone(),
        )
        .with_native_anthropic_endpoint("https://api.anthropic.com");
        let mut request = crate::MessagesCreateRequest::new(
            model,
            Some(profile),
            None,
            vec![text_user_msg("Return marker.")],
            vec![],
        );
        request.opts.max_output_tokens = Some(4096);
        request.opts.native_anthropic_request_context = Some(NativeAnthropicRequestContext {
            mid_conversation_tool_changes: true,
            mid_conversation_tool_changes_feature_gate: mctc_gate,
            inline_tools_feature_gate: inline_gate,
            ..Default::default()
        });

        service
            .messages_create(request)
            .await
            .expect("the selected Native feature gate does not prevent dispatch");

        let header = &transport.seen_headers(0)["anthropic-beta"];
        assert_eq!(header.contains(mctc), expect_mctc, "{profile}: {header}");
        assert_eq!(
            header.contains(inline_tools),
            expect_inline,
            "{profile}: {header}"
        );
    }
}

#[tokio::test]
async fn foundry_main_route_excludes_tool_changes_and_inline_tools() {
    use lingxi_llm_client::providers::anthropic::beta_repair::Beta;
    use lingxi_llm_client::NativeAnthropicRequestContext;

    let _guard = CACHE_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    // CLAUDE_CODE_EXTRA_BODY writers hold THINKING_ENV_LOCK; a sibling's
    // `{"betas":{"toString":null}}` must not reach this capture.
    let _thinking_env = THINKING_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _env = CacheEnvRestore::clear(&[
        "CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS",
        "CLAUDE_CODE_EXTRA_BODY",
        "CLAUDE_CODE_FORCE_MID_CONVERSATION_SYSTEM",
        "DISABLE_INTERLEAVED_THINKING",
    ]);
    assert!(
        !lingxi_core::host::compliance_taints::is_tainted("hipaa"),
        "this fixture requires the initialized-empty compliance taint state"
    );

    let mctc = Beta::MidConversationToolChanges.header();
    let inline_tools = "inline-tools-2026-09-15";
    for (profile, inline_gate) in [("foundry-mctc-only", false), ("foundry-inline-on", true)] {
        let model = "claude-sonnet-5-5-r62-foundry-feature-gate";
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let service = make_adapter_for_protocol_with_transport(
            ProtocolFamily::FoundryClaude,
            ProviderId::OpenAICompatible {
                name: "foundry-claude".to_owned(),
            },
            "https://example.services.ai.azure.com/anthropic",
            profile,
            model,
            transport.clone(),
        );
        assert!(!service.supports_native_mid_conversation_system(model, Some(profile)));
        assert_eq!(
            service.native_mid_conversation_tool_change_supported(model, Some(profile)),
            Some(false)
        );
        let mut request = crate::MessagesCreateRequest::new(
            model,
            Some(profile),
            None,
            vec![text_user_msg("Return marker.")],
            vec![],
        );
        request.opts.max_output_tokens = Some(4096);
        request.opts.native_anthropic_request_context = Some(NativeAnthropicRequestContext {
            mid_conversation_tool_changes: true,
            mid_conversation_tool_changes_feature_gate: true,
            inline_tools_feature_gate: inline_gate,
            ..Default::default()
        });

        service
            .messages_create(request)
            .await
            .expect("the selected Foundry feature gates do not prevent dispatch");

        let header = &transport.seen_headers(0)["anthropic-beta"];
        assert!(!header.contains(mctc), "{profile}: {header}");
        assert!(!header.contains(inline_tools), "{profile}: {header}");
    }
}

#[tokio::test]
async fn native_295_nonstream_bedrock_afk_error_does_not_use_stream_kie_retry() {
    let _guard = CACHE_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _env = CacheEnvRestore::clear(&[
        "CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS",
        "CLAUDE_CODE_SIMULATE_PROXY_USAGE",
        "ANTHROPIC_BEDROCK_BASE_URL",
    ]);
    use lingxi_llm_client::providers::anthropic::beta_repair::Beta;
    use lingxi_llm_client::NativeAnthropicRequestContext;

    let transport = FakeTransport::always(ProviderResponse::json(
        400,
        serde_json::json!({
            "type": "error",
            "error": {
                "type": "invalid_request_error",
                "message": "afk-mode-2026-01-31 is unavailable for this endpoint"
            }
        }),
    ));
    let service = make_adapter_for_protocol_with_transport(
        ProtocolFamily::BedrockClaude,
        ProviderId::BedrockClaude,
        "https://bedrock-runtime.us-east-1.amazonaws.com",
        "bedrock",
        "claude-opus-4-8-r30-afk",
        transport.clone(),
    );
    let mut request = crate::MessagesCreateRequest::new(
        "claude-opus-4-8-r30-afk",
        None,
        None,
        vec![text_user_msg("Return marker.")],
        vec![],
    );
    request.opts.max_output_tokens = Some(4096);
    request.opts.native_anthropic_request_context = Some(NativeAnthropicRequestContext {
        afk_mode: true,
        ..Default::default()
    });
    request.opts.fallback = crate::FallbackPolicy::Disabled;
    assert!(
        !lingxi_core::host::compliance_taints::is_tainted("hipaa"),
        "this fixture requires the initialized-empty compliance taint state"
    );
    assert!(
        service.messages_create(request).await.is_err(),
        "the HQ TBC-only retry still ends on the repeated AFK refusal"
    );

    let afk = Beta::AfkMode.header();
    let tbc = Beta::ThinkingBindingControls.header();
    assert_eq!(
        transport.seen_count(),
        2,
        "the AFK text is an unknown 400 to HQ, so the sent TBC beta is retried once"
    );
    for attempt in 0..2 {
        assert!(
            request_body_has_beta(&transport.seen_body(attempt), afk),
            "AFK remains in Bedrock body across the TBC-only retry (attempt {attempt})"
        );
        assert!(
            !request_header_has_beta(&transport.seen_headers(attempt), afk),
            "Bedrock carries AFK in body, not the anthropic-beta header"
        );
    }
    assert!(request_header_has_beta(&transport.seen_headers(0), tbc));
    assert!(
        !request_header_has_beta(&transport.seen_headers(1), tbc),
        "HQ's unknown-400 retry strips TBC but does not trip the AFK/Kie circuit"
    );
}

#[tokio::test]
async fn native_295_stream_afk_kie_retries_only_afk_before_mctc_model_latch() {
    use futures::StreamExt;
    use lingxi_llm_client::providers::anthropic::beta_repair::Beta;
    use lingxi_llm_client::NativeAnthropicRequestContext;

    let _guard = CACHE_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _env = CacheEnvRestore::clear(&[
        "CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS",
        "DISABLE_INTERLEAVED_THINKING",
    ]);

    let model = "claude-sonnet-5-5-r30-afk-mctc-stream";
    let body = |message: &str| {
        serde_json::to_vec(&serde_json::json!({
            "type": "error",
            "error": {"type": "invalid_request_error", "message": message}
        }))
        .unwrap()
    };
    let error_headers = BTreeMap::from([("content-type".into(), "application/json".into())]);
    let success_frames = ScriptedStreamTransport::anthropic_success().frames.clone();
    let transport = Arc::new(NativeAfkStreamRetryTransport {
        responses: Mutex::new(std::collections::VecDeque::from(vec![
            (
                400,
                error_headers.clone(),
                vec![body(
                    "anthropic-beta afk-mode-2026-01-31 was rejected; tool_addition is not supported on this model",
                )],
            ),
            (
                400,
                error_headers,
                vec![body(
                    "The tool_addition block is not supported on this model",
                )],
            ),
            (
                200,
                BTreeMap::from([("content-type".into(), "text/event-stream".into())]),
                success_frames,
            ),
        ])),
        seen: Mutex::new(Vec::new()),
    });
    let service = make_adapter_for_protocol_with_transport(
        ProtocolFamily::AnthropicMessages,
        ProviderId::AnthropicFirstParty,
        "https://api.anthropic.com",
        // The process model-support latch is route-scoped. Keep this
        // synthetic refusal on its own profile so it cannot pre-retire
        // MCTC for the public TTY fixture's `native` route in this test
        // binary.
        "native-afk-mctc-latch-test",
        model,
        transport.clone(),
    )
    .with_native_anthropic_endpoint("https://api.anthropic.com");
    let mut request = LlmRequest::new(model).with_user_text("Return marker.");
    request.input.max_tokens = Some(4096);
    request.execution.native_anthropic_request_context = Some(NativeAnthropicRequestContext {
        afk_mode: true,
        mid_conversation_tool_changes: true,
        mid_conversation_tool_changes_feature_gate: true,
        ..Default::default()
    });
    let mut stream = service
        .stream_request(request)
        .await
        .expect("AFK and MCTC refusals are repaired in Native order");
    while let Some(event) = stream.next().await {
        event.expect("successful third stream decodes");
    }

    let afk = Beta::AfkMode.header();
    let mctc = Beta::MidConversationToolChanges.header();
    let seen = transport.seen.lock().unwrap();
    assert_eq!(seen.len(), 3);
    let first = seen[0].headers["anthropic-beta"].as_str();
    let second = seen[1].headers["anthropic-beta"].as_str();
    let third = seen[2].headers["anthropic-beta"].as_str();
    assert!(first.contains(afk) && first.contains(mctc));
    assert!(!second.contains(afk) && second.contains(mctc));
    assert!(!third.contains(afk) && !third.contains(mctc));
}

#[tokio::test]
async fn native_stream_afk_kie_precedes_context_1m_retry() {
    use lingxi_llm_client::providers::anthropic::beta_repair::Beta;

    let model = "claude-opus-4-7[1m]";
    let afk = Beta::AfkMode.header();
    let context_1m = crate::model::betas::CONTEXT_1M;
    let tbc = Beta::ThinkingBindingControls.header();
    let transport = run_bedrock_afk_stream_retry(
        model,
        "anthropic-beta afk-mode-2026-01-31 was rejected; context-1m-2025-08-07 beta is not supported",
        "context-1m-2025-08-07 beta is not supported",
    )
    .await;

    assert_eq!(transport.stream_call_count(), 3);
    for attempt in 0..3 {
        assert!(request_header_has_beta(
            &transport.seen_headers(attempt),
            tbc
        ));
    }
    assert!(request_body_has_beta(&transport.seen_body(0), afk));
    assert!(request_body_has_beta(&transport.seen_body(0), context_1m));
    assert!(
        !request_body_has_beta(&transport.seen_body(1), afk),
        "the first retry removes only AFK"
    );
    assert!(request_body_has_beta(&transport.seen_body(1), context_1m));
    assert!(!request_body_has_beta(&transport.seen_body(2), afk));
    assert!(
        !request_body_has_beta(&transport.seen_body(2), context_1m),
        "context-1m is removed only after its later refusal"
    );
}

#[tokio::test]
async fn native_stream_afk_kie_precedes_explicit_tbc_retry() {
    use lingxi_llm_client::providers::anthropic::beta_repair::Beta;

    let model = "claude-opus-4-7";
    let afk = Beta::AfkMode.header();
    let tbc = Beta::ThinkingBindingControls.header();
    let transport = run_bedrock_afk_stream_retry(
        model,
        "anthropic-beta afk-mode-2026-01-31 was rejected; thinking-binding-controls-2026-08-01 is unsupported",
        "thinking-binding-controls-2026-08-01 is unsupported",
    )
    .await;

    assert_eq!(transport.stream_call_count(), 3);
    assert!(request_header_has_beta(&transport.seen_headers(0), tbc));
    assert!(request_header_has_beta(&transport.seen_headers(1), tbc));
    assert!(
        !request_header_has_beta(&transport.seen_headers(2), tbc),
        "TBC remains until the retry reports its own refusal"
    );
    assert!(request_body_has_beta(&transport.seen_body(0), afk));
    assert!(
        !request_body_has_beta(&transport.seen_body(1), afk),
        "the first retry removes AFK while retaining TBC"
    );
    assert!(!request_body_has_beta(&transport.seen_body(2), afk));
}

#[tokio::test]
async fn native_295_tty_main_request_projections_match_prepared_host_calls() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/native_per_turn_control_2_1_295_tty_mainrequest_44.json"
    ))
    .expect("captured Native TTY main-request fixture parses");
    assert_eq!(fixture["native"]["version"], "2.1.295");
    let cases = fixture["cases"].as_array().expect("TTY cases");
    assert_eq!(
        cases.len(),
        44,
        "all available main requests are represented"
    );
    assert_eq!(fixture["lifecycle_summary"]["complete_tty_lifecycles"], 42);
    assert_eq!(
        fixture["lifecycle_summary"]["qualified_incomplete_rows"],
        serde_json::json!([265, 267])
    );

    const CHILD_ENV: &str = "LINGXI_NATIVE_295_TTY_ROW_CHILD";
    const ROW_ENV: &str = "LINGXI_NATIVE_295_TTY_MATRIX_ROW";
    if std::env::var_os(CHILD_ENV).is_none() {
        // Each Native capture came from a fresh CLI process. Reusing one
        // process here shares model beta latches and the display-probe
        // budget across rows, which changes later provider requests.
        // Re-run this same test once per captured row in a clean process;
        // each child selects exactly one immutable fixture input below.
        let executable = std::env::current_exe().expect("current test binary path");
        let mut failures = Vec::new();
        for case in cases {
            let matrix_row = case["matrix_row"].as_u64().expect("matrix row");
            let output = std::process::Command::new(&executable)
                .arg("native_295_tty_main_request_projections_match_prepared_host_calls")
                .arg("--nocapture")
                .env_clear()
                .env(CHILD_ENV, "1")
                .env(ROW_ENV, matrix_row.to_string())
                .env("RUST_TEST_THREADS", "1")
                .output()
                .unwrap_or_else(|error| {
                    panic!("row {matrix_row}: launch isolated test process: {error}")
                });
            if !output.status.success() {
                failures.push(format!(
                    "row {matrix_row} failed\nstdout:\n{}\nstderr:\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                ));
            }
        }
        assert!(
            failures.is_empty(),
            "independent Native row processes failed:\n{}",
            failures.join("\n\n")
        );
        return;
    }
    let selected_row = std::env::var(ROW_ENV)
        .expect("isolated child has a selected matrix row")
        .parse::<u64>()
        .expect("selected matrix row is numeric");
    assert_eq!(
        cases
            .iter()
            .filter(|case| case["matrix_row"].as_u64() == Some(selected_row))
            .count(),
        1,
        "the isolated child selects exactly one recorded row"
    );

    let _guard = CACHE_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _env = CacheEnvRestore::clear(&[
        "CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS",
        "DISABLE_INTERLEAVED_THINKING",
        "CLAUDE_CODE_FORCE_MID_CONVERSATION_SYSTEM",
        "CLAUDE_CODE_ALWAYS_ENABLE_EFFORT",
        branding::EFFORT_LEVEL_ENV,
        "DISABLE_PROMPT_CACHING",
    ]);

    let environment_keys = [
        "CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS",
        "DISABLE_INTERLEAVED_THINKING",
        "CLAUDE_CODE_FORCE_MID_CONVERSATION_SYSTEM",
        "CLAUDE_CODE_ALWAYS_ENABLE_EFFORT",
    ];
    for case in cases
        .iter()
        .filter(|case| case["matrix_row"].as_u64() == Some(selected_row))
    {
        let matrix_row = case["matrix_row"].as_u64().expect("matrix row");
        let input = &case["input"];
        assert_eq!(input["interactive"], true, "row {matrix_row}");
        for key in environment_keys {
            if let Some(value) = input["env"].get(key).and_then(serde_json::Value::as_str) {
                std::env::set_var(key, value);
            } else {
                std::env::remove_var(key);
            }
        }
        let capture_complete = case["capture_complete"]
            .as_bool()
            .expect("capture lifecycle status");
        assert_eq!(
            capture_complete,
            case["lifecycle_status"] == "complete",
            "row {matrix_row}: keep capture lifecycle status explicit"
        );

        let model = input["model"].as_str().expect("request model");
        let (protocol, provider_id, profile_name, base_url, first_party) =
            match input["provider"].as_str().expect("provider") {
                "firstParty" => (
                    ProtocolFamily::AnthropicMessages,
                    ProviderId::AnthropicFirstParty,
                    "native",
                    "http://configured-anthropic-base.test",
                    true,
                ),
                "foundry" => (
                    ProtocolFamily::FoundryClaude,
                    ProviderId::OpenAICompatible {
                        name: "foundry-claude".to_owned(),
                    },
                    "foundry-claude",
                    "https://example.services.ai.azure.com/anthropic",
                    false,
                ),
                other => panic!("unexpected captured provider {other:?}"),
            };
        let mut adapter = make_adapter_for_protocol_with_transport(
            protocol,
            provider_id,
            base_url,
            profile_name,
            model,
            FakeTransport::always(ProviderResponse::json(200, ok_response_json())),
        )
        .with_interactive_session(true);
        if first_party {
            adapter = adapter.with_native_anthropic_endpoint(base_url);
        }
        let stream = case["observed"]["stream"]
            .as_bool()
            .expect("captured main request stream flag");
        let mut request = adapter
            .build_request(
                model,
                None,
                None,
                vec![text_user_msg("Return marker.")],
                vec![],
                stream,
                None,
                false,
                PromptCacheQuerySource::Unspecified,
            )
            .expect("build current Host main request");
        request.execution.resolve_native_effort = true;
        let host_inputs = &case["host_inputs"];
        let permission_mode = host_inputs["permission_mode"]["value"]
            .as_str()
            .expect("captured host permission mode");
        assert_eq!(permission_mode, "auto", "row {matrix_row}");
        let permission_source = host_inputs["permission_mode"]["source_path"]
            .as_str()
            .expect("captured session JSONL path");
        let permission_source_hash = host_inputs["permission_mode"]["source_sha256"]
            .as_str()
            .expect("captured session JSONL hash");
        assert!(permission_source.ends_with(".jsonl"), "row {matrix_row}");
        assert_eq!(permission_source_hash.len(), 64, "row {matrix_row}");
        assert_eq!(
            host_inputs["server_classifier_context"]["source_body_path"],
            case["main_request"]["source_body_path"],
            "row {matrix_row}: classifier source is the captured main request"
        );
        assert_eq!(
            host_inputs["server_classifier_context"]["source_body_sha256"],
            case["main_request"]["body_sha256"],
            "row {matrix_row}: classifier source hash matches the captured body"
        );
        let experimental_betas_disabled = input["env"]
            .get("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS")
            .and_then(serde_json::Value::as_str)
            .is_some_and(crate::structured_output::bool_value);
        let mctc_route = first_party;
        let afk_route = match input["provider"].as_str() {
            // Native Gyo requires vh() for direct Anthropic, including its
            // experimental-beta gate.
            Some("firstParty") => !experimental_betas_disabled,
            // Native's broader MUn() branch admits Foundry independently
            // of vh()'s experimental-beta gate.
            Some("foundry") => true,
            other => panic!("unexpected AFK provider {other:?}"),
        };
        let classifier_source = host_inputs["server_classifier_context"]["payload_json"].as_str();
        let server_classifier = classifier_source.map(|source| {
            use sha2::Digest as _;

            let payload_hash = format!("{:x}", sha2::Sha256::digest(source.as_bytes()));
            assert_eq!(
                Some(payload_hash.as_str()),
                host_inputs["server_classifier_context"]["payload_sha256"].as_str(),
                "row {matrix_row}: classifier input retains its exact captured JSON source"
            );
            assert_eq!(
                host_inputs["server_classifier_context"]["json_pointer"],
                "/safeguards/0/classifier_context",
                "row {matrix_row}: classifier input comes from the captured safeguards"
            );
            let payload = lingxi_llm_client::protocol::JsonPayload::parse(source)
                .expect("captured classifier JSON source parses");
            lingxi_llm_client::NativeAnthropicServerClassifierContext::new(payload)
                .expect("captured classifier context is a JSON object")
        });
        request.execution.native_anthropic_request_context =
            Some(lingxi_llm_client::NativeAnthropicRequestContext {
                server_classifier,
                resume_incomplete_thinking: false,
                afk_mode: permission_mode == "auto" && afk_route,
                // Route and B6 remain explicit request facts; model
                // capability and refusal state are checked by the service.
                mid_conversation_tool_changes: mctc_route,
                mid_conversation_tool_changes_feature_gate: mctc_route
                    && !experimental_betas_disabled,
                inline_tools_feature_gate: false,
            });
        let mut prepared = adapter
            .client
            .prepare(&request)
            .await
            .expect("prepare current Host request");
        adapter
            .inject_headers(&mut prepared, DispatchHeaderState::default())
            .expect("inject current Host headers");

        let body = request_body_value(&prepared.provider_request.body_json);
        assert_eq!(
            body["model"].as_str(),
            case["observed"]["body_model"].as_str(),
            "row {matrix_row}: model"
        );
        assert_eq!(
            body["stream"].as_bool(),
            Some(stream),
            "row {matrix_row}: stream flag"
        );
        let actual_top_effort = body.pointer("/output_config/effort").cloned();
        let expected_top_effort = case["observed"]["top_level_output_config_effort"]
            .as_str()
            .map(|effort| serde_json::Value::String(effort.to_owned()));
        assert_eq!(
            actual_top_effort, expected_top_effort,
            "row {matrix_row}: top-level effort"
        );
        let actual_message_effort = body["messages"]
            .as_array()
            .expect("encoded messages")
            .iter()
            .enumerate()
            .filter_map(|(index, message)| {
                let effort = message.pointer("/output_config/effort")?;
                Some(serde_json::json!({
                    "message_index": index,
                    "role": message["role"],
                    "effort": effort,
                }))
            })
            .collect::<Vec<_>>();
        let expected_message_effort = case["observed"]["per_message_effort_markers"]
            .as_array()
            .expect("captured per-message markers")
            .clone();
        assert_eq!(
            actual_message_effort, expected_message_effort,
            "row {matrix_row}: per-message effort markers"
        );

        let actual_beta_line = prepared
            .provider_request
            .headers
            .get("anthropic-beta")
            .map(|value| format!("anthropic-beta: {value}\r\n").into_bytes());
        let expected_beta_line = case["observed"]["anthropic_beta_header_line"]
            .as_str()
            .expect("exact captured beta header line");
        // The Foundry codec accepts only effort-only system rows. All other
        // recorded beta tokens and their order still match the Native request.
        let expected_beta_line = if first_party {
            expected_beta_line.to_owned()
        } else {
            expected_beta_line.replace("mid-conversation-tool-changes-2026-07-01,", "")
        };
        assert_eq!(
            actual_beta_line.as_deref(),
            Some(expected_beta_line.as_bytes()),
            "row {matrix_row}: exact Native beta header bytes"
        );
    }
}

#[tokio::test]
async fn resumed_system_effort_preserves_text_and_options_on_success() {
    use lingxi_llm_client::protocol::{ConversationMessage as WireMessage, MessageRole};
    use lingxi_llm_client::providers::anthropic::types::{
        AnthropicClearAt, AnthropicMessageEffort, AnthropicMessageOptions,
    };

    let _guard = CACHE_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _env = CacheEnvRestore::clear(&[
        "CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS",
        branding::EFFORT_LEVEL_ENV,
    ]);
    let model = "claude-sonnet-5-5";
    let url = "https://api.anthropic.com";
    let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let service = make_adapter_for_protocol_with_transport(
        ProtocolFamily::AnthropicMessages,
        ProviderId::AnthropicFirstParty,
        url,
        "anthropic",
        model,
        transport.clone(),
    );
    let mut request = service
        .build_request(
            model,
            Some("anthropic"),
            None,
            vec![text_user_msg("Continue")],
            vec![],
            false,
            None,
            false,
            PromptCacheQuerySource::Unspecified,
        )
        .unwrap();
    let text = request.input.messages[0].content.clone();
    request.input.messages.truncate(1);
    request.input.messages.push(
        WireMessage {
            role: MessageRole::System,
            content: text,
            native_options: Vec::new(),
        }
        .with_anthropic_options(AnthropicMessageOptions {
            clear_at: Some(AnthropicClearAt::Never),
            effort: Some(AnthropicMessageEffort::High),
        }),
    );
    request.set_effort(Some(serde_json::json!("low"))).unwrap();
    let response = service
        .execute_non_stream_request(
            request,
            crate::NonStreamingRequestClass::Main,
            Default::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.per_turn_effort().unwrap().string_at(""),
        Some("low")
    );
    let body = request_body_value(&transport.seen_body(0));
    let rows = body["messages"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1]["content"][0]["text"], "Continue");
    assert_eq!(rows[1]["clear_at"], "never");
    assert_eq!(rows[1]["output_config"]["effort"], "low");
    assert_eq!(body["output_config"]["effort"], "low");
}

#[tokio::test]
async fn foundry_effort_from_successful_response_replays_as_history_marker() {
    let _guard = CACHE_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _env = CacheEnvRestore::clear(&[
        "CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS",
        "DISABLE_INTERLEAVED_THINKING",
        "CLAUDE_CODE_FORCE_MID_CONVERSATION_SYSTEM",
        branding::EFFORT_LEVEL_ENV,
        "DISABLE_PROMPT_CACHING",
    ]);
    let transport = FakeTransport::sequence(vec![
        FakeResponse::Ok(ProviderResponse::json(200, ok_response_json())),
        FakeResponse::Ok(ProviderResponse::json(200, ok_response_json())),
    ]);
    let service = make_adapter_for_protocol_with_transport(
        ProtocolFamily::FoundryClaude,
        ProviderId::OpenAICompatible {
            name: "foundry-claude".to_owned(),
        },
        "https://example.services.ai.azure.com/anthropic",
        "foundry-claude",
        "claude-sonnet-5-5",
        transport.clone(),
    );
    service.set_session_effort(lingxi_core::host::effort_table::SessionEffort::Level(
        serde_json::json!("high"),
    ));
    let first_user = lingxi_core::types::ConversationMessage::user(
        lingxi_core::types::MessageId::new(),
        "first turn".into(),
    );
    let first_response = service
        .messages_create(crate::MessagesCreateRequest::new(
            "claude-sonnet-5-5",
            Some("foundry-claude"),
            None,
            vec![first_user.clone()],
            vec![],
        ))
        .await
        .expect("first Foundry main response");
    let first_effort = first_response
        .per_turn_effort()
        .expect("successful Foundry response records its admitted effort");
    assert_eq!(first_effort.string_at(""), Some("high"));
    let first_body = request_body_value(&transport.seen.lock().unwrap()[0].body_json);
    assert!(first_body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|message| {
            message["role"] == "system" && message["output_config"]["effort"] == "high"
        }));

    // Seed the next API-history batch from the actual response, including
    // its response-owned effort tag, then select a different current level.
    let assistant_content = first_response
        .content
        .iter()
        .filter_map(|block| match block {
            crate::ContentBlock::Text { text, .. } => {
                Some(lingxi_core::types::ContentBlock::Text {
                    text: text.clone(),
                    citations: None,
                })
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        !assistant_content.is_empty(),
        "fixture returns assistant text"
    );
    let prior_assistant = lingxi_core::types::ConversationMessage::Assistant {
        id: lingxi_core::types::MessageId::new(),
        content: assistant_content,
        stop_reason: first_response.stop_reason.clone(),
        resumable: None,
        per_turn_effort: Some(first_effort),
    };
    service.set_session_effort(lingxi_core::host::effort_table::SessionEffort::Level(
        serde_json::json!("medium"),
    ));
    let mut second = crate::MessagesCreateRequest::new(
        "claude-sonnet-5-5",
        Some("foundry-claude"),
        None,
        vec![
            first_user,
            prior_assistant,
            lingxi_core::types::ConversationMessage::user(
                lingxi_core::types::MessageId::new(),
                "second turn".into(),
            ),
        ],
        vec![],
    );
    second.opts.effort = Some(serde_json::json!("low"));
    let second_response = service
        .messages_create(second)
        .await
        .expect("second Foundry main response");
    assert_eq!(
        second_response
            .per_turn_effort()
            .and_then(|effort| effort.string_at("").map(str::to_owned)),
        Some("low".into())
    );

    let seen = transport.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    let second_body = request_body_value(&seen[1].body_json);
    assert_eq!(second_body["output_config"]["effort"], "low");
    let markers = second_body["messages"]
        .as_array()
        .expect("encoded Foundry message history")
        .iter()
        .enumerate()
        .filter_map(|(index, message)| {
            let effort = message.pointer("/output_config/effort")?.as_str()?;
            Some((
                index,
                message["role"].as_str()?.to_owned(),
                effort.to_owned(),
            ))
        })
        .collect::<Vec<_>>();
    assert_eq!(
        markers,
        vec![
            (1, "system".to_owned(), "high".to_owned()),
            (4, "system".to_owned(), "low".to_owned()),
        ],
        "successful response effort stays attached to its historical turn"
    );
}

#[test]
fn main_request_retains_exact_model_source_utf16_units() {
    let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let service = make_adapter(transport);
    let source =
        lingxi_core::types::utf16_json::Utf16JsonProjection::parse(r#""model-\ud800""#).unwrap();
    let mut request = crate::MessagesCreateRequest::new(
        "claude-sonnet-4-20250514",
        Some("anthropic"),
        None,
        Vec::new(),
        Vec::new(),
    );
    request.opts.model_source = Some(source);

    let (request, _) = service
        .build_main_message_request(request, false)
        .expect("valid route plus source string projection");
    assert_eq!(
        request
            .input
            .model_source
            .as_ref()
            .expect("source carrier reaches SDK request")
            .as_json_source(),
        r#""model-\ud800""#
    );
}
