//! Tests for `service.rs`, extracted from inline `#[cfg(test)]` blocks. Included via `#[path] mod service_test;`.

pub use super::*;

// Shared across all service test modules: flags are process-global, so retry
// fixtures must exclude the dispatch-header writers in sibling modules too.
static DISPATCH_FLAG_LOCK: std::sync::RwLock<()> = std::sync::RwLock::new(());

fn cedar_lattice_write() -> std::sync::RwLockWriteGuard<'static, ()> {
    DISPATCH_FLAG_LOCK
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(super) fn cedar_lattice_read() -> std::sync::RwLockReadGuard<'static, ()> {
    DISPATCH_FLAG_LOCK
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_llm_client::providers::anthropic::system_prompt::PromptText;

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

    // Ordinary structural assertions may use serde_json as a test projection;
    // source and exact-byte assertions below stay on JsonPayload/sealed bytes.
    fn request_body_value(body: &lingxi_llm_client::protocol::JsonPayload) -> serde_json::Value {
        serde_json::from_str(body.as_json_source()).expect("request body is valid JSON")
    }

    fn provider_payload(value: serde_json::Value) -> lingxi_llm_client::protocol::JsonPayload {
        lingxi_llm_client::protocol::JsonPayload::from_generated_value(&value)
            .expect("test provider response is valid JSON")
    }

    fn context_1m_refusal_response(message: &str) -> FakeResponse {
        FakeResponse::Ok(ProviderResponse::json(
            400,
            serde_json::json!({
                "type": "error",
                "error": {
                    "type": "invalid_request_error",
                    "message": message
                }
            }),
        ))
    }

    fn request_header_has_beta(headers: &BTreeMap<String, String>, beta: &str) -> bool {
        headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("anthropic-beta"))
            .is_some_and(|(_, value)| value.split(',').map(str::trim).any(|item| item == beta))
    }

    fn request_body_has_beta(body: &lingxi_llm_client::protocol::JsonPayload, beta: &str) -> bool {
        request_body_value(body)
            .get("anthropic_beta")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|items| items.iter().any(|item| item.as_str() == Some(beta)))
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
            native_custom_beta_provider_route(
                &ProviderId::VertexClaude,
                ProtocolFamily::VertexClaude
            ),
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
        assert!(scope.beta_rejected(
            lingxi_llm_client::providers::anthropic::beta_repair::Beta::ThinkingBindingControls
        ));
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
        assert!(!scope.beta_rejected(
            lingxi_llm_client::providers::anthropic::beta_repair::Beta::ThinkingBindingControls
        ));
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
        assert!(!scope.beta_rejected(
            lingxi_llm_client::providers::anthropic::beta_repair::Beta::ThinkingBindingControls
        ));
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
            .prepare_on(&request, transport)
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

    fn classified_error<T>(result: &Result<T, LlmError>) -> Option<&LlmError> {
        result.as_ref().err().map(LlmError::classification_source)
    }

    fn core_projection(
        value: &serde_json::Value,
    ) -> lingxi_core::types::utf16_json::Utf16JsonProjection {
        lingxi_core::types::utf16_json::Utf16JsonProjection::from_serde_value(value)
            .expect("generated test fixture is valid JSON")
    }

    fn source_tool_declaration(
        name: &str,
        description: &str,
        schema: &str,
    ) -> crate::ToolDeclaration {
        crate::convert::to_tool_declarations(vec![lingxi_core::host::SourceToolWireDefinition {
            name: name.to_owned(),
            description: lingxi_core::types::utf16_json::Utf16JsonProjection::from(description),
            input_schema: lingxi_core::types::utf16_json::Utf16JsonProjection::parse(schema)
                .unwrap(),
            strict: false,
            defer_loading: false,
        }])
        .unwrap()
        .pop()
        .unwrap()
    }

    fn prompt_source_elements(request: &LlmRequest) -> Vec<&PromptText> {
        use lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput;

        let system = request
            .execution
            .prompt_cache
            .as_ref()
            .and_then(|context| context.system.as_ref())
            .expect("the source system prompt is carried to SDK projection");
        match system {
            SystemPromptInput::SourceVector {
                elements,
                dynamic_context,
                ..
            } => {
                let mut result = elements.iter().collect::<Vec<_>>();
                if let Some(dynamic_context) =
                    dynamic_context.as_ref().filter(|text| !text.is_empty())
                {
                    result.push(dynamic_context);
                }
                result
            }
            SystemPromptInput::CustomPrompt { text } => vec![text],
            SystemPromptInput::NativeCustomPrompt {
                source_elements, ..
            } => source_elements.iter().collect(),
        }
    }

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
            lingxi_llm_client::FrozenPricing::capture(&profile, &display, "deepseek-flash")
                .unwrap();
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
    use crate::model::retry::DEFAULT_MAX_RETRIES;
    use crate::{
        AuthStrategy, BoxFuture, Capabilities, ClientConfig, Credential, CredentialConfig,
        CredentialProvider, CredentialScope, LlmError, ModelProfile, PricingConfig, ProtocolFamily,
        ProviderId, ProviderProfile, ProviderRequest, ProviderResponse, StreamingResponse,
    };
    use std::collections::BTreeMap;
    use std::sync::{atomic::AtomicUsize, Mutex};

    // These SDK projection fixtures exercise Native's exact brand-member
    // classifier. An arbitrary opaque header is an ordinary source member,
    // so use the current Native identity while testing its block/cache shape.
    const HEADER: &str = "You are Claude Code, Anthropic's official CLI for Claude.";
    const SECTION_SEP: &str = "\n\n";

    // ── FakeTransport ─────────────────────────────────────────────────────────

    /// A fake Transport that returns a scripted sequence of responses (or errors).
    struct FakeTransport {
        /// Pre-recorded responses returned in order; cycles to last entry.
        responses: Mutex<Vec<FakeResponse>>,
        /// All requests received, in order.
        seen: Mutex<Vec<ProviderRequest>>,
    }

    #[derive(Clone)]
    #[allow(dead_code)]
    enum FakeResponse {
        Ok(ProviderResponse),
        ResetScopeBeforeReply {
            scope: crate::thinking_scope::ThinkingRecoveryScope,
            response: ProviderResponse,
        },
        Err(LlmError),
    }

    impl FakeTransport {
        /// Return the same response on every call.
        fn always(resp: ProviderResponse) -> Arc<Self> {
            Arc::new(Self {
                responses: Mutex::new(vec![FakeResponse::Ok(resp)]),
                seen: Mutex::new(vec![]),
            })
        }

        /// Return each response in sequence; the last is repeated forever.
        fn sequence(resps: Vec<FakeResponse>) -> Arc<Self> {
            Arc::new(Self {
                responses: Mutex::new(resps),
                seen: Mutex::new(vec![]),
            })
        }

        fn seen_count(&self) -> usize {
            self.seen.lock().unwrap().len()
        }

        fn seen_headers(&self, idx: usize) -> BTreeMap<String, String> {
            self.seen.lock().unwrap()[idx].headers.clone()
        }

        fn seen_body(&self, idx: usize) -> lingxi_llm_client::protocol::JsonPayload {
            self.seen.lock().unwrap()[idx].body_json.clone()
        }

        /// Return the `"model"` field from the JSON body of the `idx`-th request.
        /// Useful for asserting chain-walk model sequences.
        fn seen_body_model(&self, idx: usize) -> Option<String> {
            request_body_value(&self.seen.lock().unwrap()[idx].body_json)
                .get("model")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        }
    }

    #[derive(Debug)]
    struct RotatingAnthropicOAuthProvider {
        token: Mutex<String>,
        refreshes: AtomicUsize,
    }

    impl RotatingAnthropicOAuthProvider {
        fn new() -> Self {
            Self {
                token: Mutex::new("rejected-token".into()),
                refreshes: AtomicUsize::new(0),
            }
        }
    }

    impl CredentialProvider for RotatingAnthropicOAuthProvider {
        fn load<'a>(
            &'a self,
            _scope: &'a CredentialScope,
        ) -> BoxFuture<'a, Result<Credential, LlmError>> {
            Box::pin(async move {
                Ok(Credential::AnthropicOAuth {
                    access_token: self.token.lock().unwrap().clone(),
                    scopes: vec!["user:inference".into()],
                })
            })
        }

        fn refresh<'a>(
            &'a self,
            _scope: &'a CredentialScope,
            rejected: &'a Credential,
        ) -> BoxFuture<'a, Result<Option<Credential>, LlmError>> {
            Box::pin(async move {
                let Credential::AnthropicOAuth {
                    access_token,
                    scopes,
                } = rejected
                else {
                    return Ok(None);
                };
                if access_token != "rejected-token" {
                    return Ok(None);
                }
                self.refreshes
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                *self.token.lock().unwrap() = "refreshed-token".into();
                Ok(Some(Credential::AnthropicOAuth {
                    access_token: "refreshed-token".into(),
                    scopes: scopes.clone(),
                }))
            })
        }
    }

    impl llm_runtime::test_support::FixtureTransport for FakeTransport {
        fn execute<'a>(
            &'a self,
            request: &'a ProviderRequest,
        ) -> BoxFuture<'a, Result<ProviderResponse, LlmError>> {
            let mut seen = self.seen.lock().unwrap();
            seen.push(request.clone());
            let idx = (seen.len() - 1).min({
                let resps = self.responses.lock().unwrap();
                resps.len().saturating_sub(1)
            });
            let resp = {
                let resps = self.responses.lock().unwrap();
                resps[idx].clone()
            };
            Box::pin(async move {
                match resp {
                    FakeResponse::Ok(r) => Ok(r),
                    FakeResponse::ResetScopeBeforeReply { scope, response } => {
                        scope.reset_beta_rejections();
                        Ok(response)
                    }
                    FakeResponse::Err(e) => Err(e),
                }
            })
        }

        fn open_stream<'a>(
            &'a self,
            _request: &'a ProviderRequest,
        ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>> {
            Box::pin(async move {
                Err(LlmError::Transport {
                    message: "open_stream not scripted".to_string(),
                })
            })
        }
    }
    llm_runtime::impl_fixture_transport!(FakeTransport);

    // ── Test helpers ──────────────────────────────────────────────────────────

    #[derive(Default)]
    struct AttemptProbe {
        events: Mutex<Vec<&'static str>>,
        observations: Mutex<Vec<(crate::ExecutionUsage, crate::ModelAttemptUsageCompleteness)>>,
        fail_settlement: bool,
        admission_delay: Duration,
    }

    struct ProbeLease {
        probe: Arc<AttemptProbe>,
        finished: bool,
    }

    impl Drop for ProbeLease {
        fn drop(&mut self) {
            if !self.finished {
                self.probe.events.lock().unwrap().push("drop-owned");
            }
        }
    }

    struct ProbeSettlement(Arc<AttemptProbe>);

    #[async_trait::async_trait]
    impl crate::ModelAttemptHooks for Arc<AttemptProbe> {
        async fn begin(
            &self,
            _: &lingxi_core::host::ModelAttemptContext,
            request: &LlmRequest,
            prepared: &crate::PreparedLlmCall,
        ) -> Result<Box<dyn crate::ModelAttemptLease>, LlmError> {
            assert!(request.execution.model_attempt.is_some());
            if !self.admission_delay.is_zero() {
                tokio::time::sleep(self.admission_delay).await;
            }
            let _ = prepared;
            self.events.lock().unwrap().push("begin");
            Ok(Box::new(ProbeLease {
                probe: self.clone(),
                finished: false,
            }))
        }
    }

    impl crate::ModelAttemptLease for ProbeLease {
        fn mark_dispatched(&mut self) -> Result<(), LlmError> {
            self.probe.events.lock().unwrap().push("dispatch");
            Ok(())
        }
        fn observe_usage(
            &mut self,
            usage: &crate::ExecutionUsage,
            completeness: crate::ModelAttemptUsageCompleteness,
        ) {
            self.probe
                .observations
                .lock()
                .unwrap()
                .push((usage.clone(), completeness));
        }
        fn mark_no_provider_response(&mut self) {
            self.probe
                .events
                .lock()
                .unwrap()
                .push("no-provider-response");
        }
        fn finish(mut self: Box<Self>) -> Box<dyn crate::ModelAttemptSettlement> {
            self.finished = true;
            self.probe.events.lock().unwrap().push("finish-owned");
            Box::new(ProbeSettlement(self.probe.clone()))
        }
    }

    #[async_trait::async_trait]
    impl crate::ModelAttemptSettlement for ProbeSettlement {
        async fn wait(self: Box<Self>) -> Result<(), LlmError> {
            self.0.events.lock().unwrap().push("settled");
            if self.0.fail_settlement {
                return Err(LlmError::Transport {
                    message: "fake persistence failed".into(),
                });
            }
            Ok(())
        }
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
        request.opts.request_dispatch_admission =
            Some(crate::RequestDispatchAdmission::new(|| false));
        let dispatch_observations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = dispatch_observations.clone();
        request.opts.request_dispatch_observer =
            Some(crate::RequestDispatchObserver::new(move || {
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

    struct IncompleteResponseTransport {
        status: u16,
    }
    #[async_trait::async_trait]
    impl Transport for IncompleteResponseTransport {
        async fn send(
            &self,
            _: lingxi_llm_client::HttpRequest,
        ) -> Result<lingxi_llm_client::StreamResponse, lingxi_llm_client::protocol::LlmError>
        {
            use futures::StreamExt;
            let body = if self.status >= 400 {
                futures::stream::pending().boxed()
            } else {
                futures::stream::iter(vec![
                    Ok(
                        r#"{"id":"msg","content":[{"type":"text","text":"generated"}],"usage":"#
                            .into(),
                    ),
                    Err(lingxi_llm_client::protocol::LlmError::StreamInterrupted {
                        message: "lost body after generation".into(),
                    }),
                ])
                .boxed()
            };
            Ok(lingxi_llm_client::StreamResponse {
                status: self.status,
                headers: vec![],
                body,
            })
        }
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
            let service = make_adapter_with_retries(
                Arc::new(IncompleteResponseTransport { status }),
                Some(3),
            )
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

    struct StalledNonStreamTransport {
        body: bool,
    }
    #[async_trait::async_trait]
    impl Transport for StalledNonStreamTransport {
        async fn send(
            &self,
            _: lingxi_llm_client::HttpRequest,
        ) -> Result<lingxi_llm_client::StreamResponse, lingxi_llm_client::protocol::LlmError>
        {
            use futures::StreamExt;
            if !self.body {
                return std::future::pending().await;
            }
            Ok(lingxi_llm_client::StreamResponse {
                status: 200,
                headers: vec![],
                body: futures::stream::pending().boxed(),
            })
        }
    }
    #[tokio::test]
    async fn model_validation_request_is_minimal_and_never_retries() {
        use lingxi_llm_client::protocol::{CacheBreakpoint, CachePosition, CacheTtl, JsonPayload};

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
    async fn ordinary_context_1m_generic_refusal_retries_when_sticky_tbc_is_on_u6_route() {
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
        // This conversation has prior Native `sent` membership from an earlier
        // eligible route. Foundry is in U6 but cannot newly register via sl;
        // it still preserves `kc=J_(nt,uB)` for this request.
        let scope = crate::thinking_scope::ThinkingRecoveryScope::default();
        scope.mark_native_thinking_binding_controls_beta_sent();
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
        assert!(request_header_has_beta(&transport.seen_headers(0), tbc));
        assert!(request_header_has_beta(
            &transport.seen_headers(0),
            crate::model::betas::CONTEXT_1M
        ));
        assert!(request_header_has_beta(&transport.seen_headers(1), tbc));
        assert!(!request_header_has_beta(
            &transport.seen_headers(1),
            crate::model::betas::CONTEXT_1M
        ));
        assert!(!scope.beta_rejected(
            lingxi_llm_client::providers::anthropic::beta_repair::Beta::ThinkingBindingControls
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
        assert!(scope.beta_rejected(
            lingxi_llm_client::providers::anthropic::beta_repair::Beta::ThinkingBindingControls
        ));
        assert!(!scope.native_thinking_binding_controls_state().beta_sent);
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
        assert!(!scope.native_thinking_binding_controls_state().beta_sent);
        assert!(!scope.beta_rejected(
            lingxi_llm_client::providers::anthropic::beta_repair::Beta::ThinkingBindingControls
        ));

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
        assert!(scope.native_thinking_binding_controls_state().beta_sent);
        assert!(!scope.beta_rejected(
            lingxi_llm_client::providers::anthropic::beta_repair::Beta::ThinkingBindingControls
        ));
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

        // An explicitly configured canonical URL takes Native KXt's local
        // true branch. Its provider route remains first-party even when the
        // configured profile itself has a different URL.
        std::env::set_var("ANTHROPIC_BASE_URL", "https://api.anthropic.com");
        let canonical_env = make_adapter_for_protocol_with_transport(
            ProtocolFamily::AnthropicMessages,
            ProviderId::AnthropicFirstParty,
            "https://proxy.example.test",
            "proxy-profile",
            model,
            transport.clone(),
        );
        let canonical_scope = crate::thinking_scope::ThinkingRecoveryScope::default();
        let mut canonical_request = make_request("proxy-profile");
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
            canonical_scope
                .native_thinking_binding_controls_state()
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
            !default_scope
                .native_thinking_binding_controls_state()
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
            !assumed_scope
                .native_thinking_binding_controls_state()
                .beta_sent
        );
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
        request.input.model_source = Some(
            lingxi_llm_client::protocol::JsonPayload::string_from_utf16_code_units(&source_units),
        );
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
        assert!(native_v_oauth_revocation(
            403,
            Some("OAuth token has been revoked for this account")
        ));
        assert!(native_v_oauth_revocation(
            401,
            Some("OAuth access token has been revoked")
        ));
        assert!(!native_v_oauth_revocation(
            403,
            Some("OAuth access token has been revoked")
        ));
        assert!(!native_v_oauth_revocation(
            401,
            Some("oauth access token has been revoked")
        ));
        assert!(!native_v_oauth_revocation(403, None));
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
            service.fast_availability.generation()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn nonstream_deadline_bounds_headers_and_body_and_settles_once() {
        for body in [false, true] {
            let service =
                make_adapter_with_retries(Arc::new(StalledNonStreamTransport { body }), Some(0));
            let probe = Arc::new(AttemptProbe::default());
            service.set_model_attempt_hooks(Arc::new(probe.clone()));
            let result = tokio::time::timeout(
                crate::model::request_timeout::non_stream_timeout().unwrap()
                    + Duration::from_secs(1),
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

    fn registered_request() -> LlmRequest {
        let mut request =
            LlmRequest::new("claude-sonnet-4-20250514").with_user_text("fake request");
        request.input.max_tokens = Some(100);
        request.execution.model_attempt = Some(
            lingxi_core::host::ModelAttemptRun::new(Arc::new(()))
                .context(lingxi_core::host::ModelAttemptStage::Panel, Some(0))
                .unwrap(),
        );
        request
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

    struct HttpOnlyProbeTransport {
        frames: Vec<Vec<u8>>,
        http_calls: std::sync::atomic::AtomicUsize,
        ws_calls: std::sync::atomic::AtomicUsize,
    }

    impl llm_runtime::test_support::FixtureTransport for HttpOnlyProbeTransport {
        fn execute<'a>(
            &'a self,
            _: &'a ProviderRequest,
        ) -> BoxFuture<'a, Result<ProviderResponse, LlmError>> {
            Box::pin(async { panic!("registered stream must not become nonstream fallback") })
        }
        fn open_stream<'a>(
            &'a self,
            request: &'a ProviderRequest,
        ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>> {
            assert_eq!(
                request.stream_transport,
                crate::ProviderStreamTransport::Http
            );
            self.http_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let frames = self.frames.clone();
            Box::pin(async move {
                Ok(StreamingResponse {
                    status: 200,
                    headers: BTreeMap::new(),
                    frames: Box::new(ScriptedFrames::new(frames)),
                })
            })
        }
        fn open_responses_websocket_session<'a>(
            &'a self,
            _: &'a ProviderRequest,
        ) -> BoxFuture<'a, Result<Box<dyn crate::ResponsesWebSocketTransportSession>, LlmError>>
        {
            self.ws_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let frames = self.frames.clone();
            Box::pin(async move {
                Ok(Box::new(ProbeWsSession { frames })
                    as Box<dyn crate::ResponsesWebSocketTransportSession>)
            })
        }
    }
    llm_runtime::impl_fixture_transport!(HttpOnlyProbeTransport);

    struct ProbeWsSession {
        frames: Vec<Vec<u8>>,
    }

    impl crate::ResponsesWebSocketTransportSession for ProbeWsSession {
        fn send<'a>(
            &'a mut self,
            _: &'a ProviderRequest,
        ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>> {
            let frames = self.frames.clone();
            Box::pin(async move {
                Ok(StreamingResponse {
                    status: 101,
                    headers: BTreeMap::new(),
                    frames: Box::new(ScriptedFrames::new(frames)),
                })
            })
        }

        fn close(&mut self) -> BoxFuture<'_, Result<(), LlmError>> {
            Box::pin(async { Ok(()) })
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
            assert!(probe
                .observations
                .lock()
                .unwrap()
                .iter()
                .all(|(_, completeness)| *completeness
                    == crate::ModelAttemptUsageCompleteness::Partial));
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

    struct HookRetirementProbe {
        service: std::sync::Weak<ApiService>,
        unlocked_drop: Arc<std::sync::atomic::AtomicBool>,
    }

    #[async_trait::async_trait]
    impl crate::ModelAttemptHooks for HookRetirementProbe {
        async fn begin(
            &self,
            _: &lingxi_core::host::ModelAttemptContext,
            _: &LlmRequest,
            _: &crate::PreparedLlmCall,
        ) -> Result<Box<dyn crate::ModelAttemptLease>, LlmError> {
            panic!("retired hooks must never receive a request")
        }
    }

    impl Drop for HookRetirementProbe {
        fn drop(&mut self) {
            let unlocked = self
                .service
                .upgrade()
                .is_some_and(|service| service.model_attempt_hooks.try_write().is_ok());
            self.unlocked_drop
                .store(unlocked, std::sync::atomic::Ordering::SeqCst);
        }
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
    async fn registered_complete_usage_requires_provider_input_and_output_but_accepts_explicit_zero(
    ) {
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

    fn ok_response_json() -> serde_json::Value {
        serde_json::json!({
            "id": "msg_test",
            "model": "claude-sonnet-4-20250514",
            "content": [{"type": "text", "text": "hello"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 5, "output_tokens": 2}
        })
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

    fn make_adapter_with_subscriber(
        transport: Arc<dyn Transport>,
        subscriber: SubscriberState,
    ) -> ApiService {
        std::env::set_var("ADAPTER_TEST_KEY", "test-key");
        let client = Arc::new(
            ModelRuntime::from_config(ClientConfig {
                providers: vec![ProviderProfile {
                    wire_profile: None,
                    regions: lingxi_llm_client::protocol::Region::all(),
                    provider_id: ProviderId::AnthropicFirstParty,
                    profile_name: "anthropic".to_string(),
                    base_url: "https://api.anthropic.com".to_string(),
                    protocol: ProtocolFamily::AnthropicMessages,
                    auth: AuthStrategy::ApiKey,
                    credential: CredentialConfig::Env {
                        var: "ADAPTER_TEST_KEY".to_string(),
                    },
                    models: vec![ModelProfile {
                        display_model: "claude-sonnet-4-20250514".to_string(),
                        request_model: "claude-sonnet-4-20250514".to_string(),
                        billing_model: "claude-sonnet-4".to_string(),
                        aliases: vec!["claude".to_string()],
                        description: None,
                        metadata: Default::default(),
                        capabilities: Capabilities {
                            streaming: true,
                            tools: true,
                            reasoning: true,
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
            .expect("client"),
        );
        ApiService::new(
            client,
            transport,
            subscriber,
            UserAgentEnv {
                user_type: Some("external".to_string()),
                entrypoint: Some("cli".to_string()),
                ..Default::default()
            },
            "0.0.0",
            None,
            None,
        )
        .with_fast_policy_source(Arc::new(|| {
            Ok(crate::model::fast_admission::Policy {
                cached_org_enabled: true,
                ..Default::default()
            })
        }))
    }

    /// `max_retries` bounds the retry ladder; `None` keeps the production
    /// default, which walks ten rungs before giving up.
    fn make_adapter_with_retries(
        transport: Arc<dyn Transport>,
        max_retries: Option<u32>,
    ) -> ApiService {
        std::env::set_var("ADAPTER_TEST_KEY", "test-key");
        let client = Arc::new(
            ModelRuntime::from_config(ClientConfig {
                providers: vec![ProviderProfile {
                    wire_profile: None,
                    regions: lingxi_llm_client::protocol::Region::all(),
                    provider_id: ProviderId::AnthropicFirstParty,
                    profile_name: "anthropic".to_string(),
                    base_url: "https://api.anthropic.com".to_string(),
                    protocol: ProtocolFamily::AnthropicMessages,
                    auth: AuthStrategy::ApiKey,
                    credential: CredentialConfig::Env {
                        var: "ADAPTER_TEST_KEY".to_string(),
                    },
                    models: vec![ModelProfile {
                        display_model: "claude-sonnet-4-20250514".to_string(),
                        request_model: "claude-sonnet-4-20250514".to_string(),
                        billing_model: "claude-sonnet-4".to_string(),
                        aliases: vec!["claude".to_string()],
                        description: None,
                        metadata: Default::default(),
                        capabilities: Capabilities {
                            streaming: true,
                            tools: true,
                            reasoning: true,
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
            .expect("client"),
        );
        ApiService::new_with_routing(
            client,
            transport,
            SubscriberState::default(),
            UserAgentEnv {
                user_type: Some("external".to_string()),
                entrypoint: Some("cli".to_string()),
                ..Default::default()
            },
            "0.0.0",
            None,
            None,
            None,
            std::collections::BTreeMap::new(),
            max_retries,
            None,
        )
        .with_fast_policy_source(Arc::new(|| {
            Ok(crate::model::fast_admission::Policy {
                cached_org_enabled: true,
                ..Default::default()
            })
        }))
    }

    fn make_adapter(transport: Arc<dyn Transport>) -> ApiService {
        make_adapter_with_retries(transport, None)
    }

    #[test]
    fn api_service_interactivity_is_session_local() {
        let prior = lingxi_core::host::session_flags::is_non_interactive_session();
        lingxi_core::host::session_flags::set_non_interactive_session(false);
        let transport = FakeTransport::always(ProviderResponse {
            status: 200,
            headers: BTreeMap::new(),
            body_json: provider_payload(ok_response_json()),
            request_id: None,
        });

        let headless = make_adapter(transport.clone()).with_interactive_session(false);
        let interactive = make_adapter(transport).with_interactive_session(true);

        assert!(!headless.interactive_session_for_test());
        assert!(interactive.interactive_session_for_test());
        lingxi_core::host::session_flags::set_non_interactive_session(prior);
    }

    fn make_adapter_for_protocol(
        protocol: ProtocolFamily,
        provider_id: ProviderId,
        base_url: &str,
    ) -> ApiService {
        make_adapter_for_protocol_with_transport(
            protocol,
            provider_id,
            base_url,
            "p",
            "model",
            FakeTransport::always(ProviderResponse::json(200, ok_response_json())),
        )
    }

    fn make_adapter_for_protocol_with_transport(
        protocol: ProtocolFamily,
        provider_id: ProviderId,
        base_url: &str,
        profile_name: &str,
        model: &str,
        transport: Arc<dyn Transport>,
    ) -> ApiService {
        make_adapter_for_protocol_with_websocket(
            protocol,
            provider_id,
            base_url,
            profile_name,
            model,
            transport,
            false,
        )
    }

    fn make_adapter_for_protocol_with_websocket(
        protocol: ProtocolFamily,
        provider_id: ProviderId,
        base_url: &str,
        profile_name: &str,
        model: &str,
        transport: Arc<dyn Transport>,
        supports_websockets: bool,
    ) -> ApiService {
        let azure = if matches!(protocol, ProtocolFamily::AzureOpenAi) {
            Some(crate::AzureConfig {
                api_version: "2024-02-01".to_string(),
            })
        } else {
            None
        };
        let client = Arc::new(
            ModelRuntime::from_config(ClientConfig {
                providers: vec![ProviderProfile {
                    wire_profile: None,
                    regions: crate::Region::all(),
                    provider_id,
                    profile_name: profile_name.to_string(),
                    base_url: base_url.to_string(),
                    protocol,
                    auth: AuthStrategy::None,
                    credential: CredentialConfig::None,
                    models: vec![ModelProfile {
                        display_model: model.to_string(),
                        request_model: model.to_string(),
                        billing_model: model.to_string(),
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
                    azure,
                    supports_websockets,
                    supports_websocket_compression: false,
                    websocket_connect_timeout_ms: None,
                    vision_delegate: None,
                    connection: Default::default(),
                }],
            })
            .expect("client"),
        );
        ApiService::new(
            client,
            transport,
            SubscriberState::default(),
            UserAgentEnv {
                user_type: Some("external".to_string()),
                entrypoint: Some("cli".to_string()),
                ..Default::default()
            },
            "0.0.0",
            None,
            None,
        )
    }

    async fn headers_after_inject_for_protocol(
        protocol: ProtocolFamily,
        provider_id: ProviderId,
        base_url: &str,
    ) -> BTreeMap<String, String> {
        let adapter = make_adapter_for_protocol(protocol, provider_id, base_url);
        let mut request = LlmRequest::new("model").with_user_text("hi");
        request.input.max_tokens = Some(4096);
        let mut prepared = adapter.client.prepare(&request).await.expect("prepare");
        adapter
            .inject_headers(&mut prepared, DispatchHeaderState::default())
            .unwrap();
        prepared.provider_request.headers
    }

    // ── Prompt-cache breakpoints (CACHE.1) ──────────────────────────────────

    // Serializes the two prompt-cache tests: one mutates DISABLE_PROMPT_CACHING
    // (process-global), so the default-on assertion in the other must not run
    // concurrently. Lock poison is benign here — recover the guard.
    static CACHE_ENV_LOCK: Mutex<()> = Mutex::new(());

    struct CacheEnvRestore(Vec<(&'static str, Option<std::ffi::OsString>)>);

    impl CacheEnvRestore {
        fn clear(names: &[&'static str]) -> Self {
            let saved = names
                .iter()
                .map(|name| (*name, std::env::var_os(name)))
                .collect();
            for name in names {
                std::env::remove_var(name);
            }
            Self(saved)
        }
    }

    impl Drop for CacheEnvRestore {
        fn drop(&mut self) {
            for (name, value) in &self.0 {
                if let Some(value) = value {
                    std::env::set_var(name, value);
                } else {
                    std::env::remove_var(name);
                }
            }
        }
    }

    fn native_capability_profile(
        provider_id: ProviderId,
        profile_name: &str,
        protocol: ProtocolFamily,
        base_url: &str,
        model: &str,
        aliases: Vec<String>,
    ) -> ProviderProfile {
        ProviderProfile {
            wire_profile: None,
            regions: lingxi_llm_client::protocol::Region::all(),
            provider_id,
            profile_name: profile_name.to_owned(),
            base_url: base_url.to_owned(),
            protocol,
            auth: AuthStrategy::ApiKey,
            credential: CredentialConfig::Env {
                var: "ADAPTER_TEST_KEY".to_owned(),
            },
            models: vec![ModelProfile {
                display_model: model.to_owned(),
                request_model: model.to_owned(),
                billing_model: model.to_owned(),
                aliases,
                description: None,
                metadata: Default::default(),
                capabilities: Capabilities {
                    streaming: true,
                    tools: true,
                    reasoning: true,
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
        }
    }

    fn native_capability_adapter(profiles: Vec<ProviderProfile>) -> ApiService {
        let client = Arc::new(
            ModelRuntime::from_config(ClientConfig {
                providers: profiles,
            })
            .expect("client"),
        );
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        ApiService::new(
            client,
            transport,
            SubscriberState::default(),
            UserAgentEnv {
                user_type: Some("external".to_owned()),
                entrypoint: Some("cli".to_owned()),
                ..Default::default()
            },
            "0.0.0",
            None,
            None,
        )
    }

    #[test]
    fn native_mid_conversation_system_memoizes_per_service_and_raw_model() {
        let _guard = CACHE_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let _env = CacheEnvRestore::clear(&[
            "CLAUDE_CODE_FORCE_MID_CONVERSATION_SYSTEM",
            "CLAUDE_CODE_MODEL_CAPABILITIES",
            "ANTHROPIC_DEFAULT_FABLE_MODEL",
            "ANTHROPIC_DEFAULT_FABLE_MODEL_SUPPORTED_CAPABILITIES",
            "ANTHROPIC_DEFAULT_OPUS_MODEL",
            "ANTHROPIC_DEFAULT_OPUS_MODEL_SUPPORTED_CAPABILITIES",
            "ANTHROPIC_DEFAULT_SONNET_MODEL",
            "ANTHROPIC_DEFAULT_SONNET_MODEL_SUPPORTED_CAPABILITIES",
            "ANTHROPIC_DEFAULT_HAIKU_MODEL",
            "ANTHROPIC_DEFAULT_HAIKU_MODEL_SUPPORTED_CAPABILITIES",
            "ANTHROPIC_CUSTOM_MODEL_OPTION",
            "ANTHROPIC_CUSTOM_MODEL_OPTION_SUPPORTED_CAPABILITIES",
        ]);
        let model = "claude-sonnet-4-6";
        let profile = native_capability_profile(
            ProviderId::FoundryClaude,
            "foundry",
            ProtocolFamily::FoundryClaude,
            "https://resource.services.ai.azure.com/anthropic",
            model,
            vec!["sonnet-route-alias".to_owned()],
        );
        let first_host = native_capability_adapter(vec![profile.clone()]);

        // QTe's explicit false wins before the model-age floor and is memoized
        // by the raw requested model string in this host/profile.
        std::env::set_var("ANTHROPIC_DEFAULT_SONNET_MODEL", model);
        std::env::set_var("ANTHROPIC_DEFAULT_SONNET_MODEL_SUPPORTED_CAPABILITIES", "");
        assert!(!first_host.supports_native_mid_conversation_system(model, Some("foundry")));

        // The Native hQ/ Ta result stays stable for the same host and raw key
        // after process configuration changes.
        std::env::set_var(
            "ANTHROPIC_DEFAULT_SONNET_MODEL_SUPPORTED_CAPABILITIES",
            "mid_conversation_system",
        );
        assert!(!first_host.supports_native_mid_conversation_system(model, Some("foundry")));

        // A new raw key that resolves to the same route gets its own QTe
        // lookup, and a new ApiService gets an independent host cache.
        std::env::set_var("ANTHROPIC_DEFAULT_SONNET_MODEL", "sonnet-route-alias");
        assert!(first_host
            .supports_native_mid_conversation_system("sonnet-route-alias", Some("foundry")));
        std::env::set_var("ANTHROPIC_DEFAULT_SONNET_MODEL", model);
        let second_host = native_capability_adapter(vec![profile]);
        assert!(second_host.supports_native_mid_conversation_system(model, Some("foundry")));
    }

    #[test]
    fn native_mid_conversation_system_cache_keeps_provider_profiles_separate() {
        let _guard = CACHE_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let _env = CacheEnvRestore::clear(&[
            "CLAUDE_CODE_FORCE_MID_CONVERSATION_SYSTEM",
            "CLAUDE_CODE_MODEL_CAPABILITIES",
            "ANTHROPIC_DEFAULT_FABLE_MODEL",
            "ANTHROPIC_DEFAULT_FABLE_MODEL_SUPPORTED_CAPABILITIES",
            "ANTHROPIC_DEFAULT_OPUS_MODEL",
            "ANTHROPIC_DEFAULT_OPUS_MODEL_SUPPORTED_CAPABILITIES",
            "ANTHROPIC_DEFAULT_SONNET_MODEL",
            "ANTHROPIC_DEFAULT_SONNET_MODEL_SUPPORTED_CAPABILITIES",
            "ANTHROPIC_DEFAULT_HAIKU_MODEL",
            "ANTHROPIC_DEFAULT_HAIKU_MODEL_SUPPORTED_CAPABILITIES",
            "ANTHROPIC_CUSTOM_MODEL_OPTION",
            "ANTHROPIC_CUSTOM_MODEL_OPTION_SUPPORTED_CAPABILITIES",
        ]);
        let model = "claude-sonnet-4-6";
        std::env::set_var("ANTHROPIC_DEFAULT_SONNET_MODEL", model);
        std::env::set_var(
            "ANTHROPIC_DEFAULT_SONNET_MODEL_SUPPORTED_CAPABILITIES",
            "mid_conversation_system",
        );
        let adapter = native_capability_adapter(vec![
            native_capability_profile(
                ProviderId::OpenAI,
                "openai",
                ProtocolFamily::OpenAiChat,
                "https://api.openai.com/v1",
                model,
                Vec::new(),
            ),
            native_capability_profile(
                ProviderId::AnthropicFirstParty,
                "anthropic",
                ProtocolFamily::AnthropicMessages,
                "https://api.anthropic.com",
                model,
                Vec::new(),
            ),
            native_capability_profile(
                ProviderId::FoundryClaude,
                "foundry",
                ProtocolFamily::FoundryClaude,
                "https://resource.services.ai.azure.com/anthropic",
                model,
                Vec::new(),
            ),
        ]);

        // A non-Anthropic route returns before entering the cache. First-party
        // routes disallow QTe model-option overrides; Foundry allows them. The
        // shared raw model string must retain each route's actual result.
        assert!(!adapter.supports_native_mid_conversation_system(model, Some("openai")));
        assert!(!adapter.supports_native_mid_conversation_system(model, Some("anthropic")));
        assert!(adapter.supports_native_mid_conversation_system(model, Some("foundry")));

        let reverse_order = native_capability_adapter(vec![
            native_capability_profile(
                ProviderId::AnthropicFirstParty,
                "anthropic",
                ProtocolFamily::AnthropicMessages,
                "https://api.anthropic.com",
                model,
                Vec::new(),
            ),
            native_capability_profile(
                ProviderId::FoundryClaude,
                "foundry",
                ProtocolFamily::FoundryClaude,
                "https://resource.services.ai.azure.com/anthropic",
                model,
                Vec::new(),
            ),
        ]);
        assert!(reverse_order.supports_native_mid_conversation_system(model, Some("foundry")));
        assert!(!reverse_order.supports_native_mid_conversation_system(model, Some("anthropic")));
    }

    fn text_user_msg(s: &str) -> ConversationMessage {
        ConversationMessage::User {
            api_message_override: None,
            id: lingxi_core::types::MessageId::new(),
            content: vec![ContentBlock::Text {
                text: s.to_string(),
                citations: None,
            }],
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
            turn_companion: None,
            usage_limit_note: None,
            origin: None,
            turn_origin: None,
        }
    }

    /// `last_request_id()` captures the selected provider's server-side request
    /// id (origin = Server), uses a sent SDK client UUID when present (origin =
    /// Client), and clears when neither exists. Host telemetry IDs are not
    /// provider request IDs and must not appear in this persisted field.
    #[test]
    fn last_request_id_captures_request_id_header() {
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let adapter = make_adapter(transport);
        assert_eq!(adapter.last_request_id(), None);
        assert_eq!(adapter.last_request_id_origin(), None);

        let mut h = std::collections::BTreeMap::new();
        h.insert("request-id".to_string(), "req_011abc".to_string());
        adapter.record_rate_limit_from_headers_for_route(
            &h,
            Some("client_uuid"),
            lingxi_llm_client::protocol::ProtocolFamily::AnthropicMessages,
            "anthropic",
        );
        assert_eq!(adapter.last_request_id(), Some("req_011abc".to_string()));
        // Server header present → server origin (client id ignored).
        assert_eq!(
            adapter.last_request_id_origin(),
            Some(RequestIdOrigin::Server)
        );

        // `x-request-id` remains a provider response fallback header → server origin.
        let mut h2 = std::collections::BTreeMap::new();
        h2.insert("x-request-id".to_string(), "req_xfallback".to_string());
        adapter.record_rate_limit_from_headers_for_route(
            &h2,
            Some("client_uuid"),
            lingxi_llm_client::protocol::ProtocolFamily::AnthropicMessages,
            "anthropic",
        );
        assert_eq!(adapter.last_request_id(), Some("req_xfallback".to_string()));
        assert_eq!(
            adapter.last_request_id_origin(),
            Some(RequestIdOrigin::Server)
        );

        // No server id header → use only a client ID that was actually sent.
        adapter.record_rate_limit_from_headers_for_route(
            &std::collections::BTreeMap::new(),
            Some("client_uuid"),
            lingxi_llm_client::protocol::ProtocolFamily::AnthropicMessages,
            "anthropic",
        );
        assert_eq!(adapter.last_request_id(), Some("client_uuid".to_string()));
        assert_eq!(
            adapter.last_request_id_origin(),
            Some(RequestIdOrigin::Client)
        );

        // Neither a server header nor a sent SDK client ID → no old Host ID fallback.
        adapter.record_rate_limit_from_headers_for_route(
            &std::collections::BTreeMap::new(),
            None,
            lingxi_llm_client::protocol::ProtocolFamily::AnthropicMessages,
            "anthropic",
        );
        assert_eq!(adapter.last_request_id(), None);
        assert_eq!(adapter.last_request_id_origin(), None);
    }

    #[test]
    fn build_request_keeps_system_source_for_sdk_projection() {
        let _guard = CACHE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("DISABLE_PROMPT_CACHING");
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let adapter = make_adapter(transport);
        // The main getSystemPrompt source vector keeps the banner separate
        // from its body. Custom --system-prompt input has a distinct role.
        let system = lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput::source_vector(
            vec![
                PromptText::from_string(HEADER),
                PromptText::from_string("rest body here"),
            ],
            None,
            None,
        );
        let req = adapter
            .build_request(
                "claude-sonnet-4-20250514",
                None,
                Some(&system),
                vec![text_user_msg("hello")],
                vec![],
                false,
                Some(1024),
                false,
                PromptCacheQuerySource::Unspecified,
            )
            .expect("build_request");
        // The Host keeps system source in its execution context. The SDK
        // projects provider blocks and cache markers after route selection.
        assert_eq!(
            prompt_source_elements(&req)
                .iter()
                .map(|text| text.display_text())
                .collect::<Vec<_>>(),
            vec![HEADER, "rest body here"]
        );
        assert!(req.input.system.is_empty());
        assert!(req.input.prompt_cache.breakpoints.is_empty());
    }

    #[test]
    fn build_request_samples_cache_ttl_settings_into_execution_policy() {
        use lingxi_core::settings::schema::{
            PromptCacheTtl as HostTtl, PromptCacheTtlSettings as HostSettings,
        };
        use lingxi_llm_client::providers::anthropic::system_prompt::PromptCacheTtl;

        let _guard = CACHE_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let _env = CacheEnvRestore::clear(&[
            "DISABLE_PROMPT_CACHING",
            "FORCE_PROMPT_CACHING_5M",
            "CLAUDE_CODE_PROMPT_CACHE_TTL",
            "CLAUDE_CODE_SUBAGENT_PROMPT_CACHE_TTL",
            "ENABLE_PROMPT_CACHING_1H",
            "ENABLE_PROMPT_CACHING_1H_BEDROCK",
            "CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS",
            "LINGXI_GLOBAL_CACHE_SCOPE",
        ]);
        let settings = Arc::new(std::sync::Mutex::new(HostSettings {
            main: Some(HostTtl::OneHour),
            subagent: Some(HostTtl::FiveMinutes),
        }));
        let source_settings = settings.clone();
        let adapter = make_adapter(FakeTransport::always(ProviderResponse::json(
            200,
            ok_response_json(),
        )))
        .with_prompt_cache_ttl_settings_source(Arc::new(move || {
            Ok(*source_settings.lock().unwrap())
        }));
        let system_text = format!("{HEADER}{SECTION_SEP}retained settings prompt");
        let system = custom_system_prompt(Some(&system_text));

        let main = adapter
            .build_request(
                "claude-sonnet-4-20250514",
                None,
                system.as_ref(),
                vec![text_user_msg("main")],
                vec![],
                false,
                None,
                false,
                PromptCacheQuerySource::Named("sdk"),
            )
            .expect("main request");
        let main_policy = &main
            .execution
            .prompt_cache
            .as_ref()
            .expect("request retains the SDK prompt context")
            .policy;
        assert_eq!(
            main_policy.prompt_cache_ttl_settings.prompt_cache_ttl,
            Some(PromptCacheTtl::OneHour)
        );

        *settings.lock().unwrap() = HostSettings {
            main: Some(HostTtl::FiveMinutes),
            subagent: Some(HostTtl::OneHour),
        };
        let subagent = adapter
            .build_request(
                "claude-sonnet-4-20250514",
                None,
                system.as_ref(),
                vec![text_user_msg("subagent")],
                vec![],
                false,
                None,
                false,
                PromptCacheQuerySource::Subagent,
            )
            .expect("subagent request");
        let subagent_policy = &subagent
            .execution
            .prompt_cache
            .as_ref()
            .expect("request retains the SDK prompt context")
            .policy;
        assert_eq!(
            subagent_policy
                .prompt_cache_ttl_settings
                .subagent_prompt_cache_ttl,
            Some(PromptCacheTtl::OneHour)
        );
    }

    #[tokio::test]
    async fn build_request_projects_sampled_cache_ttl_settings_to_wire() {
        use lingxi_core::settings::schema::{
            PromptCacheTtl as HostTtl, PromptCacheTtlSettings as HostSettings,
        };

        let _guard = CACHE_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let _env = CacheEnvRestore::clear(&[
            "DISABLE_PROMPT_CACHING",
            "FORCE_PROMPT_CACHING_5M",
            "CLAUDE_CODE_PROMPT_CACHE_TTL",
            "CLAUDE_CODE_SUBAGENT_PROMPT_CACHE_TTL",
            "ENABLE_PROMPT_CACHING_1H",
            "ENABLE_PROMPT_CACHING_1H_BEDROCK",
            "CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS",
            "LINGXI_GLOBAL_CACHE_SCOPE",
        ]);
        let settings = Arc::new(std::sync::Mutex::new(HostSettings {
            main: Some(HostTtl::OneHour),
            subagent: Some(HostTtl::FiveMinutes),
        }));
        let source_settings = settings.clone();
        let adapter = make_adapter(FakeTransport::always(ProviderResponse::json(
            200,
            ok_response_json(),
        )))
        .with_prompt_cache_ttl_settings_source(Arc::new(move || {
            Ok(*source_settings.lock().unwrap())
        }));
        let system_text = format!("{HEADER}{SECTION_SEP}retained settings prompt");
        let system = custom_system_prompt(Some(&system_text));

        let main = adapter
            .build_request(
                "claude-sonnet-4-20250514",
                None,
                system.as_ref(),
                vec![text_user_msg("main")],
                vec![],
                false,
                None,
                false,
                PromptCacheQuerySource::Named("sdk"),
            )
            .expect("main request");
        fn system_cache_ttl(body: &serde_json::Value) -> &str {
            body["system"]
                .as_array()
                .expect("wire system array")
                .iter()
                .find_map(|block| block["cache_control"]["ttl"].as_str())
                .expect("wire system cache breakpoint")
        }
        let main_prepared = adapter.client.prepare(&main).await.expect("prepare main");
        let main_body = request_body_value(&main_prepared.provider_request.body_json);
        assert_eq!(system_cache_ttl(&main_body), "1h");

        *settings.lock().unwrap() = HostSettings {
            main: Some(HostTtl::FiveMinutes),
            subagent: Some(HostTtl::OneHour),
        };
        let subagent = adapter
            .build_request(
                "claude-sonnet-4-20250514",
                None,
                system.as_ref(),
                vec![text_user_msg("subagent")],
                vec![],
                false,
                None,
                false,
                PromptCacheQuerySource::Subagent,
            )
            .expect("subagent request");
        let subagent_prepared = adapter
            .client
            .prepare(&subagent)
            .await
            .expect("prepare subagent");
        let subagent_body = request_body_value(&subagent_prepared.provider_request.body_json);
        assert_eq!(system_cache_ttl(&subagent_body), "1h");
    }

    /// The `tool_reference` normalization branch is selected by the SESSION-scoped
    /// tool-search gate (Claude Code `$U()`), NOT by whether the request's own
    /// toolset carries a `ToolSearch` declaration. This is the compaction /
    /// side-query shape: the request is assembled with an EMPTY toolset, so the
    /// old code (`tools contains "ToolSearch"`) inferred disabled and emitted the
    /// wrong placeholder. With the fix the branch follows the published session
    /// flag, matching the oracle's `if(!$U())W=j6s(W);else W=xPy(W,a)`.
    #[test]
    fn build_request_tool_reference_branch_follows_session_gate_not_toolset() {
        // A user tool_result carrying a tool_reference to a tool absent from the
        // (empty) availability set, paired with its assistant tool_use so
        // `ensure_tool_result_pairing` is a strict no-op.
        let convo = || {
            vec![
                ConversationMessage::Assistant {
                    per_turn_effort: None,
                    id: lingxi_core::types::MessageId::new(),
                    content: vec![ContentBlock::ToolUse {
                        id: lingxi_core::types::ToolUseId::from("toolu_ref"),
                        name: "ToolSearch".to_string(),
                        input: lingxi_core::types::utf16_json::Utf16JsonProjection::parse("{}")
                            .unwrap(),
                        provider_id: Some("toolu_ref".to_string()),
                    }],
                    stop_reason: None,
                    resumable: None,
                },
                ConversationMessage::User {
                    api_message_override: None,
                    id: lingxi_core::types::MessageId::new(),
                    content: vec![ContentBlock::ToolResult {
                        tool_use_id: lingxi_core::types::ToolUseId::from("toolu_ref"),
                        content: String::new().into(),
                        is_error: Some(false),
                        provider_tool_use_id: Some("toolu_ref".to_string()),
                        content_blocks: Some(
                            lingxi_core::types::utf16_json::Utf16JsonProjection::from_serde_value(
                                &serde_json::json!([{
                                "type": "tool_reference",
                                "tool_name": "mcp__gone__write"
                                    }]),
                            )
                            .expect("tool reference fixture is valid JSON"),
                        ),
                    }],
                    is_meta: false,
                    is_compact_summary: false,
                    is_visible_in_transcript_only: false,
                    turn_companion: None,
                    usage_limit_note: None,
                    origin: None,
                    turn_origin: None,
                },
            ]
        };
        // Read the placeholder text the converted tool_result carries.
        let placeholder = |req: &LlmRequest| -> String {
            for m in &req.input.messages {
                for b in &m.content {
                    if let lingxi_llm_client::protocol::ContentBlock::ToolResult {
                        blocks, ..
                    } = b
                    {
                        if let Some(text) = blocks
                            .as_ref()
                            .and_then(|blocks| blocks.array_item(0))
                            .and_then(|item| item.string_at("/text"))
                        {
                            return text;
                        }
                    }
                }
            }
            panic!("no tool_result placeholder found in {req:?}");
        };

        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let adapter = make_adapter(transport);
        let prior = lingxi_core::host::session_flags::tool_search_enabled();

        // Session gate ON, EMPTY toolset (no `ToolSearch` declaration present):
        // the ENABLED branch filters the unavailable reference against the empty
        // availability set and emits the "tools no longer available" placeholder.
        lingxi_core::host::session_flags::set_tool_search_enabled(true);
        let req = adapter
            .build_request(
                "claude-sonnet-4-20250514",
                None,
                None,
                convo(),
                vec![],
                false,
                Some(1024),
                false,
                PromptCacheQuerySource::Unspecified,
            )
            .expect("build_request");
        assert_eq!(
            placeholder(&req),
            "[Tool references removed - tools no longer available]",
            "empty toolset in a tool-search-enabled session must take the enabled branch"
        );

        // Session gate OFF: the DISABLED branch strips every reference with the
        // "tool search not enabled" placeholder, regardless of the toolset.
        lingxi_core::host::session_flags::set_tool_search_enabled(false);
        let req = adapter
            .build_request(
                "claude-sonnet-4-20250514",
                None,
                None,
                convo(),
                vec![],
                false,
                Some(1024),
                false,
                PromptCacheQuerySource::Unspecified,
            )
            .expect("build_request");
        assert_eq!(
            placeholder(&req),
            "[Tool references removed - tool search not enabled]",
            "a tool-search-disabled session must take the disabled branch"
        );

        lingxi_core::host::session_flags::set_tool_search_enabled(prior);
    }

    #[test]
    fn thinking_is_provider_aware() {
        use crate::model::thinking::ThinkingConfig;
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let adapter = make_adapter(transport);
        let build = |model: &str| {
            adapter
                .build_request(
                    model,
                    None,
                    None,
                    vec![text_user_msg("hi")],
                    vec![],
                    false,
                    None,
                    false,
                    PromptCacheQuerySource::Unspecified,
                )
                .expect("build_request")
                .input
                .thinking
        };

        // Claude with the default (Adaptive) thinking → Adaptive, byte-faithful.
        assert_eq!(
            build("claude-opus-4-8-20260115"),
            Some(lingxi_llm_client::protocol::ThinkingConfig {
                mode: Some(lingxi_llm_client::protocol::ThinkingMode::Adaptive),
                ..Default::default()
            })
        );
        // Non-Claude with the default (Adaptive) → NO reasoning field (provider
        // applies its own default instead of a forced high-effort / budget-0).
        assert_eq!(build("gpt-5"), None);
        assert_eq!(build("gemini-2.5-pro"), None);
        assert_eq!(build("deepseek-reasoner"), None);

        // An EXPLICIT fixed budget on a non-Claude model is still honored.
        let adapter_fixed = make_adapter(FakeTransport::always(ProviderResponse::json(
            200,
            ok_response_json(),
        )))
        .with_thinking(ThinkingConfig::Enabled {
            budget_tokens: 4096,
        });
        let r = adapter_fixed
            .build_request(
                "gpt-5",
                None,
                None,
                vec![text_user_msg("hi")],
                vec![],
                false,
                None,
                false,
                PromptCacheQuerySource::Unspecified,
            )
            .expect("build_request")
            .input
            .thinking;
        assert_eq!(
            r,
            Some(lingxi_llm_client::protocol::ThinkingConfig {
                mode: Some(lingxi_llm_client::protocol::ThinkingMode::Enabled),
                budget: Some(lingxi_llm_client::protocol::ThinkingBudget::Tokens(4096)),
                effort: None
            })
        );
    }

    /// `None` must mean the model-aware request default, not a codec default.
    ///
    /// This is what makes `messages_create_side_query`'s `Option<u32>` worth
    /// having. A caller with no real reason to pick a number used to be forced
    /// to invent a ceiling, and an invented ceiling is sized against the ANSWER
    /// while the provider bills THINKING to the same number. The app
    /// questionnaire stage picked 4096, and on `deepseek-v4-pro` a 3123-token
    /// reasoning pass hit `finish_reason: length` at 4098, cutting the tool call
    /// mid-JSON.
    #[tokio::test]
    async fn a_side_query_without_a_ceiling_uses_the_model_request_default() {
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let adapter = make_adapter(transport);
        let model = "claude-sonnet-4-20250514";
        let build = |max_tokens| {
            adapter
                .build_request(
                    model,
                    None,
                    None,
                    vec![text_user_msg("hello")],
                    vec![],
                    false,
                    max_tokens,
                    false,
                    PromptCacheQuerySource::Unspecified,
                )
                .expect("build_request")
        };

        let model_default =
            u32::try_from(crate::model::context_window::default_output_tokens_for_model(model))
                .expect("the model default fits u32");
        let automatic = build(None);
        assert_eq!(automatic.input.max_tokens, None);
        let prepared = adapter
            .client
            .prepare(&automatic)
            .await
            .expect("SDK resolves the current model-aware application default");
        let default_raw = model_default.to_string();
        assert_eq!(
            prepared
                .provider_request
                .body_json
                .raw_source_at("/max_tokens"),
            Some(default_raw.clone()),
            "an absent ceiling reaches the wire as the current model-aware default"
        );
        assert!(
            model_default > 4096,
            "this model must have headroom above the figure the app stages \
             used to invent, or the test cannot tell the two apart"
        );
        let explicit = build(Some(4096));
        assert_eq!(
            explicit.input.max_tokens,
            Some(4096),
            "an explicit ceiling must still be honored verbatim"
        );
        let prepared = adapter
            .client
            .prepare(&explicit)
            .await
            .expect("the explicit ceiling prepares");
        assert_eq!(
            prepared
                .provider_request
                .body_json
                .raw_source_at("/max_tokens"),
            Some("4096".to_owned()),
            "an explicit ceiling remains unchanged on the wire"
        );
    }

    #[test]
    fn build_request_keeps_system_source_when_cache_is_disabled() {
        let _guard = CACHE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("DISABLE_PROMPT_CACHING", "1");
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let adapter = make_adapter(transport);
        let system = lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput::source_vector(
            vec![
                PromptText::from_string(HEADER),
                PromptText::from_string("rest body here"),
            ],
            None,
            None,
        );
        let req = adapter
            .build_request(
                "claude-sonnet-4-20250514",
                None,
                Some(&system),
                vec![text_user_msg("hello")],
                vec![],
                false,
                Some(1024),
                false,
                PromptCacheQuerySource::Unspecified,
            )
            .expect("build_request");
        std::env::remove_var("DISABLE_PROMPT_CACHING");
        assert_eq!(
            prompt_source_elements(&req)
                .iter()
                .map(|text| text.display_text())
                .collect::<Vec<_>>(),
            vec![HEADER, "rest body here"]
        );
        assert!(req.input.system.is_empty());
        assert!(req.input.prompt_cache.breakpoints.is_empty());
    }

    #[test]
    fn build_request_global_cache_gate_respects_experimental_beta_disable() {
        // Native suppresses the source boundary when experimental betas are
        // disabled, independent of subscription or local opt-in state.
        use lingxi_llm_client::providers::anthropic::system_prompt::DYNAMIC_BOUNDARY as SYSTEM_PROMPT_DYNAMIC_BOUNDARY;
        let _guard = CACHE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("DISABLE_PROMPT_CACHING");
        std::env::set_var("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS", "1");
        std::env::remove_var("LINGXI_GLOBAL_CACHE_SCOPE");
        std::env::remove_var("ANTHROPIC_BASE_URL");
        std::env::remove_var("_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL");
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let adapter = make_adapter(transport);
        let sections = [
            lingxi_llm_client::providers::anthropic::system_prompt::SourceSection {
                text: "shared".into(),
                scope: lingxi_llm_client::providers::anthropic::system_prompt::SectionScope::Shared,
            },
            lingxi_llm_client::providers::anthropic::system_prompt::SourceSection {
                text: "session".into(),
                scope:
                    lingxi_llm_client::providers::anthropic::system_prompt::SectionScope::Session,
            },
        ];
        assert_eq!(
            adapter.prompt_snapshot_source_vector("claude-sonnet-4-20250514", None, &sections),
            vec![
                PromptText::from_string("shared"),
                PromptText::from_string("session")
            ]
        );
        let system = lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput::source_vector(
            vec![
                PromptText::from_string(HEADER),
                PromptText::from_string("static"),
                PromptText::from_string(SYSTEM_PROMPT_DYNAMIC_BOUNDARY),
                PromptText::from_string("dynamic"),
            ],
            None,
            None,
        );
        let req = adapter
            .build_request(
                "claude-sonnet-4-20250514",
                None,
                Some(&system),
                vec![text_user_msg("hi")],
                vec![],
                false,
                Some(1024),
                false,
                PromptCacheQuerySource::Unspecified,
            )
            .expect("build_request");
        // The source vector and resolved kill switch stay in the request
        // context until SDK egress selects and projects the provider route.
        assert_eq!(
            prompt_source_elements(&req)
                .iter()
                .map(|text| text.display_text())
                .collect::<Vec<_>>(),
            vec![HEADER, "static", SYSTEM_PROMPT_DYNAMIC_BOUNDARY, "dynamic"]
        );
        assert!(
            req.execution
                .prompt_cache
                .as_ref()
                .expect("request retains the SDK prompt context")
                .policy
                .experimental_betas_disabled
        );
        assert!(req.input.system.is_empty());
        assert!(req.input.prompt_cache.breakpoints.is_empty());
        std::env::remove_var("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS");
    }

    #[test]
    fn build_request_global_cache_gate_uses_native_default_without_subscription_or_opt_in() {
        // With an untainted host and Native's default beta state, the source
        // vector reaches SDK projection without a subscriber or local opt-in.
        use lingxi_llm_client::providers::anthropic::system_prompt::DYNAMIC_BOUNDARY as SYSTEM_PROMPT_DYNAMIC_BOUNDARY;
        let _guard = CACHE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("DISABLE_PROMPT_CACHING");
        std::env::remove_var("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS");
        std::env::remove_var("LINGXI_GLOBAL_CACHE_SCOPE");
        std::env::remove_var("ANTHROPIC_BASE_URL");
        std::env::remove_var("_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL");
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let adapter = make_adapter(transport);
        let sections = [
            lingxi_llm_client::providers::anthropic::system_prompt::SourceSection {
                text: "shared".into(),
                scope: lingxi_llm_client::providers::anthropic::system_prompt::SectionScope::Shared,
            },
            lingxi_llm_client::providers::anthropic::system_prompt::SourceSection {
                text: "session".into(),
                scope:
                    lingxi_llm_client::providers::anthropic::system_prompt::SectionScope::Session,
            },
        ];
        assert_eq!(
            adapter.prompt_snapshot_source_vector("claude-sonnet-4-20250514", None, &sections),
            vec![
                PromptText::from_string("shared"),
                PromptText::from_string(
                    lingxi_llm_client::providers::anthropic::system_prompt::DYNAMIC_BOUNDARY,
                ),
                PromptText::from_string("session")
            ]
        );
        let system = lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput::source_vector(
            vec![
                PromptText::from_string(HEADER),
                PromptText::from_string("static"),
                PromptText::from_string(SYSTEM_PROMPT_DYNAMIC_BOUNDARY),
                PromptText::from_string("dynamic"),
            ],
            None,
            None,
        );
        let req = adapter
            .build_request(
                "claude-sonnet-4-20250514",
                None,
                Some(&system),
                vec![text_user_msg("hi")],
                vec![],
                false,
                Some(1024),
                false,
                PromptCacheQuerySource::Unspecified,
            )
            .expect("build_request");
        assert_eq!(
            prompt_source_elements(&req)
                .iter()
                .map(|text| text.display_text())
                .collect::<Vec<_>>(),
            vec![HEADER, "static", SYSTEM_PROMPT_DYNAMIC_BOUNDARY, "dynamic"]
        );
        assert!(
            !req.execution
                .prompt_cache
                .as_ref()
                .expect("request retains the SDK prompt context")
                .policy
                .experimental_betas_disabled
        );
        assert!(req.input.system.is_empty());
        assert!(req.input.prompt_cache.breakpoints.is_empty());
    }

    #[tokio::test]
    async fn sdk_request_body_emits_native_global_scope_without_subscription_or_opt_in() {
        use lingxi_llm_client::providers::anthropic::system_prompt::DYNAMIC_BOUNDARY;

        let _guard = CACHE_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        std::env::remove_var("DISABLE_PROMPT_CACHING");
        std::env::remove_var("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS");
        std::env::remove_var("LINGXI_GLOBAL_CACHE_SCOPE");
        std::env::remove_var("ANTHROPIC_BASE_URL");
        std::env::remove_var("_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL");
        let adapter = make_adapter(FakeTransport::always(ProviderResponse::json(
            200,
            ok_response_json(),
        )));
        let system = lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput::source_vector(
            vec![
                PromptText::from_string(HEADER),
                PromptText::from_string("static"),
                PromptText::from_string(DYNAMIC_BOUNDARY),
                PromptText::from_string("dynamic"),
            ],
            None,
            None,
        );
        let request = adapter
            .build_request(
                "claude-sonnet-4-20250514",
                None,
                Some(&system),
                vec![text_user_msg("hi")],
                vec![],
                false,
                Some(1024),
                false,
                PromptCacheQuerySource::Unspecified,
            )
            .expect("global-cache request");
        let prepared = adapter.client.prepare(&request).await.expect("SDK prepare");
        let body = request_body_value(&prepared.provider_request.body_json);
        assert_eq!(body["system"].as_array().unwrap().len(), 3);
        assert_eq!(body["system"][0]["text"], HEADER);
        assert_eq!(body["system"][1]["text"], "static");
        assert_eq!(body["system"][2]["text"], "dynamic");
        assert_eq!(body["system"][1]["cache_control"]["scope"], "global");
        assert!(!prepared
            .provider_request
            .body_json
            .as_json_source()
            .contains(DYNAMIC_BOUNDARY));
    }

    #[tokio::test]
    async fn sdk_request_body_honors_native_process_base_url_gate() {
        use lingxi_llm_client::providers::anthropic::system_prompt::DYNAMIC_BOUNDARY;

        let _guard = CACHE_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        std::env::remove_var("DISABLE_PROMPT_CACHING");
        std::env::remove_var("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS");
        std::env::remove_var("LINGXI_GLOBAL_CACHE_SCOPE");
        std::env::set_var("ANTHROPIC_BASE_URL", "https://proxy.example.test");
        std::env::remove_var("_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL");
        let adapter = make_adapter(FakeTransport::always(ProviderResponse::json(
            200,
            ok_response_json(),
        )));
        let system = lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput::source_vector(
            vec![
                PromptText::from_string(HEADER),
                PromptText::from_string("static"),
                PromptText::from_string(DYNAMIC_BOUNDARY),
                PromptText::from_string("dynamic"),
            ],
            None,
            None,
        );
        let request = adapter
            .build_request(
                "claude-sonnet-4-20250514",
                None,
                Some(&system),
                vec![text_user_msg("hi")],
                vec![],
                false,
                Some(1024),
                false,
                PromptCacheQuerySource::Unspecified,
            )
            .expect("proxy-gated request");
        let prepared = adapter.client.prepare(&request).await.expect("SDK prepare");
        let body = request_body_value(&prepared.provider_request.body_json);
        assert_eq!(body["system"].as_array().unwrap().len(), 2);
        assert_eq!(body["system"][0]["text"], HEADER);
        assert_eq!(body["system"][1]["text"], "static\n\ndynamic");
        assert!(body["system"][1].get("cache_control").is_some());
        assert_eq!(
            body["system"][1]
                .get("cache_control")
                .and_then(|value| value.get("scope")),
            None
        );
        assert!(!prepared
            .provider_request
            .body_json
            .as_json_source()
            .contains(DYNAMIC_BOUNDARY));
        std::env::remove_var("ANTHROPIC_BASE_URL");
    }

    // ── 1P cache-EDITING (cache_edits / cache_reference, RESIDUAL 4) ───────────

    /// A multi-message conversation: an assistant tool_use, a user tool_result,
    /// an assistant text, then a trailing user text. Only the trailing message
    /// carries the cache_control marker, so the tool_result (in an earlier user
    /// message) is strictly within the cached prefix.
    fn tool_result_conversation() -> Vec<ConversationMessage> {
        use lingxi_core::types::{
            ContentBlock as PB, ConversationMessage as CM, MessageId, ToolUseId,
        };
        let tool_id = ToolUseId::new();
        vec![
            CM::Assistant {
                per_turn_effort: None,
                id: MessageId::new(),
                content: vec![PB::ToolUse {
                    id: tool_id.clone(),
                    name: "Read".to_string(),
                    input: core_projection(&serde_json::json!({"path": "/x"})),
                    provider_id: Some("toolu_abc".to_string()),
                }],
                stop_reason: None,
                resumable: None,
            },
            CM::User {
                api_message_override: None,
                id: MessageId::new(),
                content: vec![PB::ToolResult {
                    tool_use_id: tool_id,
                    content: "file body".to_string().into(),
                    is_error: Some(false),
                    provider_tool_use_id: Some("toolu_abc".to_string()),
                    content_blocks: None,
                }],
                is_meta: false,
                is_compact_summary: false,
                is_visible_in_transcript_only: false,
                turn_companion: None,
                usage_limit_note: None,
                origin: None,
                turn_origin: None,
            },
            CM::Assistant {
                per_turn_effort: None,
                id: MessageId::new(),
                content: vec![PB::Text {
                    text: "ok".to_string(),
                    citations: None,
                }],
                stop_reason: None,
                resumable: None,
            },
            text_user_msg("continue"),
        ]
    }

    #[test]
    fn build_request_cache_editing_dormant_by_default() {
        // Gate OFF (default): no cache_reference on tool_results, no cache_edits
        // block — byte-identical to the pre-feature request.
        let _guard = CACHE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("DISABLE_PROMPT_CACHING");
        std::env::remove_var("LINGXI_CACHE_EDITING");
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        // Even a subscriber + injected edits must stay inert without the env.
        let adapter = make_adapter_with_subscriber(
            transport,
            SubscriberState {
                is_subscriber: true,
                is_enterprise: false,
            },
        )
        .with_cache_editing_inputs(CacheEditingInputs {
            new_edits: vec![crate::CacheEdit::Delete {
                cache_reference: "toolu_zzz".to_string(),
            }],
            pinned: vec![],
        });
        let req = adapter
            .build_request(
                "claude-sonnet-4-20250514",
                None,
                custom_system_prompt(Some("sys")).as_ref(),
                tool_result_conversation(),
                vec![],
                false,
                Some(1024),
                false,
                PromptCacheQuerySource::Unspecified,
            )
            .expect("build_request");
        // No cache_edits block anywhere.
        let encoded = serde_json::to_value(&req.input).unwrap().to_string();
        assert!(
            !encoded.contains("cache_edits"),
            "cache editing remains disabled"
        );
        assert!(
            !encoded.contains("cache_reference"),
            "cache references remain absent"
        );
    }

    #[test]
    fn build_request_cache_editing_opt_in_remains_dormant_without_protocol() {
        // Even with the old opt-in env set, the path stays fail-closed until
        // the cache-editing beta/session latch and cross-call pinned state are
        // implemented together.
        use crate::CacheEdit;
        let _guard = CACHE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("DISABLE_PROMPT_CACHING");
        std::env::remove_var("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS");
        std::env::set_var("LINGXI_CACHE_EDITING", "1");
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        // pinned (pos 1, the tool_result user msg) deletes ref "dup" + "p1";
        // new (last user msg) deletes "dup" (collapsed by dedup) + "n1".
        let adapter = make_adapter_with_subscriber(
            transport,
            SubscriberState {
                is_subscriber: true,
                is_enterprise: false,
            },
        )
        .with_cache_editing_inputs(CacheEditingInputs {
            new_edits: vec![
                CacheEdit::Delete {
                    cache_reference: "dup".to_string(),
                },
                CacheEdit::Delete {
                    cache_reference: "n1".to_string(),
                },
            ],
            pinned: vec![PinnedCacheEdits {
                user_message_index: 1,
                edits: vec![
                    CacheEdit::Delete {
                        cache_reference: "dup".to_string(),
                    },
                    CacheEdit::Delete {
                        cache_reference: "p1".to_string(),
                    },
                ],
            }],
        });
        let req = adapter
            .build_request(
                "claude-sonnet-4-20250514",
                None,
                custom_system_prompt(Some("sys")).as_ref(),
                tool_result_conversation(),
                vec![],
                false,
                Some(1024),
                false,
                PromptCacheQuerySource::Unspecified,
            )
            .expect("build_request");
        std::env::remove_var("LINGXI_CACHE_EDITING");

        // No cache_reference stamping and no cache_edits insertion anywhere.
        let encoded = serde_json::to_value(&req.input).unwrap().to_string();
        assert!(
            !encoded.contains("cache_edits"),
            "cache editing remains disabled"
        );
        assert!(
            !encoded.contains("cache_reference"),
            "cache references remain absent"
        );
    }

    // ── build_request profile threading (Unit B Task 5) ──────────────────────

    #[test]
    fn build_request_sets_profile_when_provided() {
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let adapter = make_adapter(transport);
        let req = adapter
            .build_request(
                "gpt-5.2",
                Some("github-copilot"),
                None,
                vec![],
                vec![],
                false,
                None,
                false,
                PromptCacheQuerySource::Unspecified,
            )
            .expect("build_request with profile");
        assert_eq!(
            req.input.model, "gpt-5.2",
            "model must be preserved verbatim"
        );
        assert_eq!(
            req.profile.as_deref(),
            Some("github-copilot"),
            "profile must be threaded into LlmRequest"
        );
    }

    #[test]
    fn build_request_leaves_profile_none_when_not_provided() {
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let adapter = make_adapter(transport);
        let req = adapter
            .build_request(
                "claude-opus-4-7",
                None,
                None,
                vec![],
                vec![],
                false,
                None,
                false,
                PromptCacheQuerySource::Unspecified,
            )
            .expect("build_request without profile");
        assert_eq!(
            req.input.model, "claude-opus-4-7",
            "model must be preserved verbatim"
        );
        assert!(
            req.profile.is_none(),
            "profile must be None when not passed"
        );
    }

    #[tokio::test]
    async fn mod_request_effort_applies_only_inside_its_task_scope() {
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let service = make_adapter(transport);
        let ordinary = service
            .build_request(
                "claude-opus-4-7",
                None,
                None,
                vec![],
                vec![],
                false,
                None,
                false,
                PromptCacheQuerySource::Unspecified,
            )
            .unwrap();
        let rewritten = with_mod_request_effort("low", async {
            service.build_request(
                "claude-opus-4-7",
                None,
                None,
                vec![],
                vec![],
                false,
                None,
                false,
                PromptCacheQuerySource::Unspecified,
            )
        })
        .await
        .unwrap();
        let side_query = with_mod_request_effort("low", async {
            service.build_side_query_request_with_thinking(
                "claude-opus-4-7",
                None,
                None,
                None,
                false,
                vec![],
                vec![],
                None,
                None,
                vec![],
                None,
                None,
                None,
                Some("mod_side_query"),
                std::collections::HashSet::new(),
                std::collections::HashMap::new(),
            )
        })
        .await
        .unwrap();
        let again = service
            .build_request(
                "claude-opus-4-7",
                None,
                None,
                vec![],
                vec![],
                false,
                None,
                false,
                PromptCacheQuerySource::Unspecified,
            )
            .unwrap();
        let effort = |request: &LlmRequest| {
            serde_json::to_value(request).unwrap()["input"]["thinking"]["effort"].clone()
        };
        assert_eq!(effort(&rewritten), serde_json::json!("low"));
        assert!(effort(&side_query).is_null());
        assert_eq!(effort(&ordinary), effort(&again));
    }

    #[test]
    fn side_query_builder_preserves_all_canonical_request_controls() {
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let adapter = make_adapter(transport);
        let req = adapter
            .build_side_query_request_with_thinking(
                "claude-sonnet-4-20250514",
                Some("anthropic"),
                Some(
                    &lingxi_core::types::utf16_json::Utf16JsonProjection::parse("\"system\"")
                        .unwrap(),
                ),
                None,
                false,
                vec![text_user_msg("payload")],
                vec![],
                Some(321),
                None,
                vec!["STOP".into()],
                None,
                Some(serde_json::json!("high")),
                Some(0.2),
                Some("fusion_analyst"),
                std::collections::HashSet::new(),
                std::collections::HashMap::new(),
            )
            .expect("side-query request builder");
        assert_eq!(req.input.model, "claude-sonnet-4-20250514");
        assert_eq!(req.profile.as_deref(), Some("anthropic"));
        let expected_system_units = "system".encode_utf16().collect::<Vec<_>>();
        assert_eq!(
            prompt_source_elements(&req)[0].utf16_code_units(),
            expected_system_units.as_slice()
        );
        assert_eq!(req.input.messages.len(), 1);
        assert_eq!(req.input.max_tokens, Some(321));
        assert_eq!(req.input.stop_sequences, vec!["STOP"]);
        assert!(
            req.input
                .temperature
                .is_some_and(|temperature| temperature == 0.2_f32),
            "the f32 caller value must remain exact in canonical input"
        );
        assert_eq!(
            req.input
                .thinking
                .as_ref()
                .and_then(|thinking| thinking.effort),
            Some(lingxi_llm_client::protocol::ReasoningEffort::High)
        );
        assert_eq!(
            req.execution.query_source.as_deref(),
            Some("fusion_analyst")
        );
        assert!(req.execution.capture_retry_count);
    }

    #[test]
    fn side_query_builder_preserves_source_system_utf16_units() {
        let adapter = make_adapter(FakeTransport::always(ProviderResponse::json(
            200,
            ok_response_json(),
        )));
        let source =
            lingxi_core::types::utf16_json::Utf16JsonProjection::parse(r#""a\ud800b""#).unwrap();
        let request = adapter
            .build_side_query_request_with_thinking(
                "claude-sonnet-4-20250514",
                Some("anthropic"),
                Some(&source),
                None,
                false,
                vec![text_user_msg("payload")],
                vec![],
                Some(321),
                None,
                vec![],
                None,
                None,
                None,
                Some("permission_classifier"),
                std::collections::HashSet::new(),
                std::collections::HashMap::new(),
            )
            .unwrap();
        assert_eq!(
            prompt_source_elements(&request)[0].utf16_code_units(),
            &[b'a' as u16, 0xd800, b'b' as u16]
        );
    }

    #[test]
    fn side_query_builder_prefers_the_captured_provider_system_structure() {
        let adapter = make_adapter(FakeTransport::always(ProviderResponse::json(
            200,
            ok_response_json(),
        )));
        let captured = lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput::source_vector(
            vec![lingxi_llm_client::providers::anthropic::system_prompt::PromptText::from_utf16(
                vec![b'a' as u16, 0xd800],
            )],
            Some(
                lingxi_llm_client::providers::anthropic::system_prompt::PromptText::from_string(
                    "dynamic",
                ),
            ),
            None,
        );
        let request = adapter
            .build_side_query_request_with_thinking(
                "claude-sonnet-4-20250514",
                Some("anthropic"),
                None,
                Some(&captured),
                false,
                vec![text_user_msg("payload")],
                vec![],
                Some(321),
                None,
                vec![],
                None,
                None,
                None,
                Some("permission_classifier"),
                std::collections::HashSet::new(),
                std::collections::HashMap::new(),
            )
            .unwrap();

        let elements = prompt_source_elements(&request);
        assert_eq!(elements.len(), 2);
        assert_eq!(elements[0].utf16_code_units(), &[b'a' as u16, 0xd800]);
        assert_eq!(
            elements[1].utf16_code_units(),
            "dynamic".encode_utf16().collect::<Vec<_>>()
        );
    }

    #[test]
    fn side_query_builder_rejects_non_string_source_system() {
        let adapter = make_adapter(FakeTransport::always(ProviderResponse::json(
            200,
            ok_response_json(),
        )));
        let source = lingxi_core::types::utf16_json::Utf16JsonProjection::parse("null").unwrap();
        let error = adapter
            .build_side_query_request_with_thinking(
                "claude-sonnet-4-20250514",
                Some("anthropic"),
                Some(&source),
                None,
                false,
                vec![],
                vec![],
                Some(321),
                None,
                vec![],
                None,
                None,
                None,
                Some("permission_classifier"),
                std::collections::HashSet::new(),
                std::collections::HashMap::new(),
            )
            .unwrap_err();
        assert!(matches!(
            error,
            LlmError::InvalidRequest { message }
                if message == "side-query system prompt must be a root JSON string"
        ));
    }

    #[test]
    fn strict_side_query_builder_uses_json_schema_without_parent_tools() {
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let adapter = make_adapter(transport);
        let schema = serde_json::json!({
            "type": "object",
            "properties": {"ok": {"type": "boolean"}},
            "required": ["ok"]
        });
        let req = adapter
            .build_json_schema_request_with_thinking(
                "claude-sonnet-4-20250514",
                Some("anthropic"),
                Some(
                    &lingxi_core::types::utf16_json::Utf16JsonProjection::parse("\"judge\"")
                        .unwrap(),
                ),
                None,
                vec![text_user_msg("payload")],
                schema.clone(),
                Some(256),
                None,
                None,
                Some(0.0),
                Some("fusion_analyst"),
            )
            .expect("strict request builder");
        assert!(req.stream);
        assert!(req.input.tools.is_empty());
        assert!(req.input.tool_choice == lingxi_llm_client::protocol::ToolChoice::Auto);
        assert_eq!(req.input.max_tokens, Some(256));
        assert_eq!(
            req.input.output_format,
            lingxi_llm_client::protocol::OutputFormat::JsonSchema {
                name: "response".into(),
                schema: provider_payload(schema),
                strict: true
            }
        );
        assert_eq!(
            req.execution.query_source.as_deref(),
            Some("fusion_analyst")
        );
    }

    // ── build_request thinking / temperature / max_tokens (DIV-1/3/4) ────────

    // Serialize the env-touching thinking tests: they mutate process-global
    // LINGXI_DISABLE_THINKING. A module-level mutex keeps them from racing
    // each other (and is poison-tolerant).
    static THINKING_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn clear_thinking_env() {
        std::env::remove_var("LINGXI_DISABLE_THINKING");
        std::env::remove_var("LINGXI_DISABLE_ADAPTIVE_THINKING");
    }

    #[test]
    fn build_request_adaptive_models_preserve_automatic_output_budget() {
        let _g = THINKING_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_thinking_env();
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let adapter = make_adapter(transport); // default ThinkingConfig::Adaptive

        // Runtime preserves automatic intent; the SDK resolves each route budget.
        for model in [
            "claude-opus-4-6",
            "claude-opus-4-8",
            "claude-opus-5",
            "claude-sonnet-4-6",
            "claude-sonnet-5",
            "claude-fable-5-1",
            "claude-mythos-5-1",
        ] {
            let req = adapter
                .build_request(
                    model,
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
                req.input
                    .thinking
                    .as_ref()
                    .and_then(|thinking| thinking.mode),
                Some(lingxi_llm_client::protocol::ThinkingMode::Adaptive),
                "{model} → adaptive"
            );
            assert!(
                req.input.temperature.is_none(),
                "{model} → no temperature when thinking on"
            );
            assert_eq!(
                req.input.max_tokens, None,
                "{model} → preserve automatic budget intent"
            );
        }
        clear_thinking_env();
    }

    #[test]
    fn build_request_non_adaptive_model_gets_fixed_budget() {
        let _g = THINKING_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_thinking_env();
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let adapter = make_adapter(transport); // default Adaptive intent, but model disallows adaptive

        // haiku-4-5 supports thinking but NOT adaptive → Enabled{upperLimit-1}.
        // The 64k output ceiling also permits the model's 63_999 thinking cap.
        let req = adapter
            .build_request(
                "claude-haiku-4-5",
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
        assert_eq!(req.input.max_tokens, None);
        assert_eq!(
            req.input
                .thinking
                .as_ref()
                .and_then(|thinking| thinking.budget),
            Some(lingxi_llm_client::protocol::ThinkingBudget::Tokens(31_999)),
            "haiku-4-5 → fixed budget clamped to max_tokens-1"
        );
        assert!(
            req.input.temperature.is_none(),
            "thinking on → no temperature"
        );
        clear_thinking_env();
    }

    /// cc 2.1.198 "Subagents + compaction inherit extended thinking config" —
    /// the SUBAGENT seam half. `ProviderApiAdapter`'s `agent::SubagentApiClient`
    /// impl delegates 1:1 to `ApiService::messages_create` / `stream`, which
    /// route every request through the SAME `build_request` and thus the SAME
    /// session `self.thinking` (binary: the child session's options carry
    /// `thinkingConfig: sDi(n.options.thinkingConfig, …)` @215628753). Lock
    /// that the subagent entry point issues a wire body whose `thinking`
    /// field matches the session config exactly like the main loop's.
    #[tokio::test]
    async fn subagent_entry_point_inherits_session_thinking_config() {
        let _g = THINKING_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_thinking_env();
        let model = "claude-sonnet-4-20250514";

        // Default session config (Adaptive intent): the issued body carries the
        // SAME thinking the shared main-loop builder computes for this model.
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let adapter = make_adapter(transport.clone());
        let expected = match adapter
            .build_request(
                model,
                None,
                None,
                vec![],
                vec![],
                false,
                None,
                false,
                PromptCacheQuerySource::Unspecified,
            )
            .expect("build_request")
            .input
            .thinking
        {
            Some(lingxi_llm_client::protocol::ThinkingConfig {
                mode: Some(lingxi_llm_client::protocol::ThinkingMode::Adaptive),
                ..
            }) => serde_json::json!({"type": "adaptive", "display": "updates"}),
            Some(lingxi_llm_client::protocol::ThinkingConfig {
                mode: Some(lingxi_llm_client::protocol::ThinkingMode::Enabled),
                budget: Some(lingxi_llm_client::protocol::ThinkingBudget::Tokens(budget_tokens)),
                ..
            }) => {
                serde_json::json!({"type": "enabled", "budget_tokens": budget_tokens, "display": "updates"})
            }
            None => serde_json::Value::Null,
            other => panic!("unexpected thinking config: {other:?}"),
        };
        assert_ne!(
            expected,
            serde_json::Value::Null,
            "session thinking is ON by default"
        );
        adapter
            .messages_create(crate::MessagesCreateRequest::new(
                model,
                None,
                None,
                vec![],
                vec![],
            ))
            .await
            .expect("messages_create");
        let body = request_body_value(&transport.seen.lock().unwrap()[0].body_json);
        assert_eq!(
            body.get("thinking")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
            expected,
            "subagent seam body inherits the session thinking config"
        );

        // Explicit session config (fixed budget): the subagent seam carries it too.
        let transport2 = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let adapter2 = make_adapter(transport2.clone()).with_thinking(
            crate::model::thinking::ThinkingConfig::Enabled {
                budget_tokens: 2_048,
            },
        );
        adapter2
            .messages_create(crate::MessagesCreateRequest::new(
                model,
                None,
                None,
                vec![],
                vec![],
            ))
            .await
            .expect("messages_create");
        let body2 = request_body_value(&transport2.seen.lock().unwrap()[0].body_json);
        assert_eq!(
            body2["thinking"],
            serde_json::json!({"type": "enabled", "budget_tokens": 2_048, "display": "updates"}),
            "an explicit session budget rides on the subagent seam"
        );
        clear_thinking_env();
    }

    #[test]
    fn live_thinking_update_applies_to_the_next_request() {
        let _g = THINKING_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_thinking_env();
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let service = make_adapter(transport);

        service.set_thinking(crate::model::thinking::ThinkingConfig::Enabled {
            budget_tokens: 4_096,
        });
        let enabled = service
            .build_request(
                "claude-sonnet-4-20250514",
                None,
                None,
                vec![],
                vec![],
                false,
                None,
                false,
                PromptCacheQuerySource::Unspecified,
            )
            .expect("enabled request");
        assert_eq!(
            enabled
                .input
                .thinking
                .as_ref()
                .and_then(|thinking| thinking.budget),
            Some(lingxi_llm_client::protocol::ThinkingBudget::Tokens(4_096))
        );

        service.set_thinking(crate::model::thinking::ThinkingConfig::Disabled);
        let disabled = service
            .build_request(
                "claude-sonnet-4-20250514",
                None,
                None,
                vec![],
                vec![],
                false,
                None,
                false,
                PromptCacheQuerySource::Unspecified,
            )
            .expect("disabled request");
        assert!(disabled.input.thinking.is_none());
        clear_thinking_env();
    }

    #[test]
    fn build_request_disable_thinking_env_drops_reasoning_sets_temperature() {
        let _g = THINKING_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_thinking_env();
        std::env::set_var("LINGXI_DISABLE_THINKING", "1");
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let adapter = make_adapter(transport);

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
        assert!(
            req.input.thinking.is_none(),
            "thinking disabled → no reasoning"
        );
        // opus-4-8 is not in the `rhn` temperature-gate set → no temperature even
        // when thinking is env-disabled.
        assert!(
            req.input.temperature.is_none(),
            "opus-4-8 thinking-disabled → no temperature (not in rhn set)"
        );
        // max_tokens still uses the model limit even when thinking is disabled.
        assert_eq!(req.input.max_tokens, None);
        clear_thinking_env();
    }

    #[test]
    fn build_request_explicit_max_tokens_override_wins() {
        let _g = THINKING_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_thinking_env();
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let adapter = make_adapter(transport);
        // Escalation override (Some) honored verbatim.
        let req = adapter
            .build_request(
                "claude-opus-4-8",
                None,
                None,
                vec![],
                vec![],
                false,
                Some(7_777),
                false,
                PromptCacheQuerySource::Unspecified,
            )
            .expect("build_request");
        assert_eq!(req.input.max_tokens, Some(7_777));
        clear_thinking_env();
    }

    #[test]
    fn build_request_uses_selected_glm_ceiling_and_enforces_hard_output_limit() {
        let model = "z-ai/glm-5.2:free-build-request-test";
        let mut provider = crate::builtin_presets()
            .providers
            .into_iter()
            .find(|provider| provider.profile_name == "openrouter")
            .unwrap();
        provider.wire_profile = None;
        provider.connection = Default::default();
        provider.models.truncate(1);
        let entry = &mut provider.models[0];
        entry.display_model = model.into();
        entry.request_model = model.into();
        entry.billing_model = model.into();
        entry.metadata.context_window_tokens = Some(256_000);
        entry.metadata.max_output_tokens = Some(230_400);
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let mut adapter = make_adapter(transport);
        adapter.client = Arc::new(
            ModelRuntime::from_config(ClientConfig {
                providers: vec![provider],
            })
            .unwrap(),
        );

        let ordinary = adapter
            .build_request(
                model,
                Some("openrouter"),
                None,
                vec![text_user_msg("hello")],
                vec![],
                false,
                None,
                false,
                PromptCacheQuerySource::Unspecified,
            )
            .expect("ordinary GLM request");
        assert_eq!(ordinary.input.max_tokens, None);

        let oversized_override = adapter
            .build_request(
                model,
                Some("openrouter"),
                None,
                vec![text_user_msg("hello")],
                vec![],
                false,
                Some(u32::MAX),
                false,
                PromptCacheQuerySource::Unspecified,
            )
            .expect("explicit GLM request");
        assert_eq!(oversized_override.input.max_tokens, Some(u32::MAX));
    }

    #[test]
    fn build_request_preserves_explicit_limit_for_unknown_custom_model() {
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let adapter = make_adapter(transport);
        let request = adapter
            .build_request(
                "unknown-custom-long-output-model",
                None,
                None,
                Vec::new(),
                Vec::new(),
                false,
                Some(150_000),
                false,
                PromptCacheQuerySource::Unspecified,
            )
            .expect("custom model request");

        assert_eq!(request.input.max_tokens, Some(150_000));
    }

    #[test]
    fn build_request_catalog_routes_preserve_automatic_budget_intent() {
        let _g = THINKING_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_thinking_env();
        let mut adapter = make_adapter(FakeTransport::always(ProviderResponse::json(
            200,
            ok_response_json(),
        )));
        adapter.client = Arc::new(
            ModelRuntime::from_config(ClientConfig {
                providers: crate::builtin_presets().providers,
            })
            .unwrap(),
        );
        let mut checked = 0;
        for listing in adapter.client.available_models() {
            let limits = adapter
                .client
                .output_token_limits_in(&listing.display_model, Some(&listing.profile_name))
                .unwrap();
            if limits.max_output_tokens.is_none() {
                continue;
            }
            let req = adapter
                .build_request(
                    &listing.display_model,
                    Some(&listing.profile_name),
                    None,
                    vec![text_user_msg("hello")],
                    vec![],
                    true,
                    None,
                    false,
                    PromptCacheQuerySource::Unspecified,
                )
                .unwrap();
            assert_eq!(
                req.input.max_tokens, None,
                "{}/{}",
                listing.profile_name, listing.request_model
            );
            checked += 1;
        }
        assert!(checked > 100, "checked {checked} routes");
        clear_thinking_env();
    }

    #[test]
    fn build_request_thinking_disabled_config_drops_reasoning() {
        let _g = THINKING_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_thinking_env();
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let adapter =
            make_adapter(transport).with_thinking(crate::model::thinking::ThinkingConfig::Disabled);
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
        assert!(
            req.input.thinking.is_none(),
            "ThinkingConfig::Disabled → no reasoning"
        );
        // opus-4-8 is NOT in the `rhn` temperature-gate set → field omitted even
        // with thinking disabled (binary @205866168: `!xs && rhn(u) ? … : void 0`).
        assert!(
            req.input.temperature.is_none(),
            "opus-4-8 thinking-disabled → no temperature (not in rhn set)"
        );
        clear_thinking_env();
    }

    #[test]
    fn build_request_thinking_disabled_temperature_only_for_rhn_models() {
        let _g = THINKING_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_thinking_env();
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let adapter =
            make_adapter(transport).with_thinking(crate::model::thinking::ThinkingConfig::Disabled);
        // claude-sonnet-4-5 IS in the `rhn` set → temperature:1 is sent.
        let req = adapter
            .build_request(
                "claude-sonnet-4-5",
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
        assert!(
            req.input.thinking.is_none(),
            "thinking disabled → no reasoning"
        );
        assert_eq!(
            req.input.temperature,
            Some(1.0),
            "sonnet-4-5 thinking-disabled → temperature:1 (in rhn set)"
        );
        clear_thinking_env();
    }

    #[test]
    fn build_request_metadata_threaded_when_set() {
        let _g = THINKING_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_thinking_env();
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let adapter = make_adapter(transport).with_request_metadata(crate::RequestMetadata {
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
        let bare = make_adapter(FakeTransport::always(ProviderResponse::json(
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

    // ── CLAUDE_CODE_EXTRA_BODY merge (claude-code B0t; parity 2.1.207) ────────

    /// Prepare `request` on `adapter` and run the header injectors (which now also
    /// run the `CLAUDE_CODE_EXTRA_BODY` merge), returning the outgoing body.
    async fn body_after_inject(
        adapter: &ApiService,
        request: &LlmRequest,
    ) -> lingxi_llm_client::protocol::JsonPayload {
        let mut request = request.clone();
        request.execution.resolve_native_effort = true;
        let mut prepared = adapter.client.prepare(&request).await.expect("prepare");
        adapter
            .inject_headers(&mut prepared, DispatchHeaderState::default())
            .unwrap();
        prepared.provider_request.body_json
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

        // Header policy places main requests in the SDK's Native top-level order.
        let unset = body_after_inject(&adapter, &request).await;
        assert_eq!(
            unset.as_json_source(),
            r#"{"model":"claude-sonnet-4-20250514","messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}],"tools":[],"max_tokens":32000}"#,
            "unset extra body retains the Native main-template source"
        );

        // (1) valid object → new key appended at the tail; (2) a colliding key
        // (`max_tokens`) keeps its original body position but takes the extra value.
        std::env::set_var("CLAUDE_CODE_EXTRA_BODY", r#"{"foo":"bar","max_tokens":99}"#);
        let merged = body_after_inject(&adapter, &request).await;
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
        prepared.anthropic_request_kind = lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::SideQuery;
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
                crate::structured_output::bool_environment(
                    branding::DISABLE_STRUCTURED_OUTPUTS_ENV
                ),
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
        let mut prepared = adapter
            .client
            .prepare(&request)
            .await
            .expect("extra-body format is caller-authored");
        adapter
            .inject_headers(&mut prepared, DispatchHeaderState::default())
            .unwrap();
        assert_eq!(
            prepared
                .provider_request
                .body_json
                .string_at("/output_config/format/name")
                .as_deref(),
            Some("explicit"),
            "Native B6 does not delete an explicit extra-body format"
        );
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
            serde_json::from_str(include_str!("../tests/fixtures/extra_body_2_1_287.json"))
                .unwrap();
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

    // ── effective_subscriber (batch-5 Task 3: live SharedSubscription) ───────

    fn shared_slot(
        snap: Option<lingxi_core::host::subscription::SubscriptionSnapshot>,
    ) -> lingxi_core::host::subscription::SharedSubscription {
        Arc::new(std::sync::RwLock::new(snap))
    }

    #[test]
    fn effective_subscriber_prefers_live_snapshot() {
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let adapter = make_adapter_with_subscriber(transport, SubscriberState::default())
            .with_subscription(shared_slot(Some(
                lingxi_core::host::subscription::SubscriptionSnapshot {
                    is_subscriber: true,
                    subscription_type: Some("enterprise".to_string()),
                    ..Default::default()
                },
            )));
        let sub = adapter.effective_subscriber();
        assert!(sub.is_subscriber);
        assert!(sub.is_enterprise);
    }

    #[test]
    fn effective_subscriber_falls_back_when_slot_empty_or_absent() {
        let static_state = SubscriberState {
            is_subscriber: true,
            is_enterprise: false,
        };

        // No slot attached → static build-time state.
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let adapter = make_adapter_with_subscriber(transport, static_state);
        let sub = adapter.effective_subscriber();
        assert!(sub.is_subscriber);
        assert!(!sub.is_enterprise);

        // Slot attached but unresolved (None) → static build-time state.
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let adapter = make_adapter_with_subscriber(transport, static_state)
            .with_subscription(shared_slot(None));
        let sub = adapter.effective_subscriber();
        assert!(sub.is_subscriber);
        assert!(!sub.is_enterprise);
    }

    #[test]
    fn effective_subscriber_non_enterprise_tier_is_not_enterprise() {
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let adapter = make_adapter_with_subscriber(transport, SubscriberState::default())
            .with_subscription(shared_slot(Some(
                lingxi_core::host::subscription::SubscriptionSnapshot {
                    is_subscriber: true,
                    subscription_type: Some("team".to_string()),
                    ..Default::default()
                },
            )));
        let sub = adapter.effective_subscriber();
        assert!(sub.is_subscriber);
        assert!(!sub.is_enterprise);
    }

    // ── Previously-ignored tests (un-ignored, ported to FakeTransport) ────────

    #[tokio::test]
    async fn bridge_resolves_and_forwards_local_model() {
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let adapter = make_adapter(transport.clone());
        let resp = adapter
            .messages_create(crate::MessagesCreateRequest::new(
                "claude-sonnet-4-20250514",
                None,
                custom_system_prompt(Some("sys")),
                Vec::new(),
                Vec::new(),
            ))
            .await
            .expect("ok");
        assert_eq!(resp.model, "claude-sonnet-4-20250514");
        assert_eq!(resp.stop_reason.as_deref(), Some("end_turn"));
    }

    #[test]
    fn available_models_non_empty() {
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let adapter = make_adapter(transport);
        let models = adapter.available_models();
        assert!(
            !models.is_empty(),
            "available_models must return at least one entry"
        );
    }

    #[tokio::test]
    async fn bridge_forwards_batched_tools() {
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let adapter = make_adapter(transport.clone());
        let tools = vec![source_tool_declaration(
            "Read",
            "read a file",
            r#"{"type":"object"}"#,
        )];
        let _ = adapter
            .messages_create(crate::MessagesCreateRequest::new(
                "claude-sonnet-4-20250514",
                None,
                custom_system_prompt(Some("sys")),
                Vec::new(),
                tools.into_iter().map(Into::into).collect(),
            ))
            .await
            .expect("ok");
        assert_eq!(transport.seen_count(), 1);
    }

    #[tokio::test]
    async fn messages_create_with_tools_on_tool_capable_model_succeeds() {
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let adapter = make_adapter(transport.clone());
        let tools = vec![source_tool_declaration(
            "Read",
            "read",
            r#"{"type":"object"}"#,
        )];
        let result = adapter
            .messages_create(crate::MessagesCreateRequest::new(
                "claude-sonnet-4-20250514",
                None,
                None,
                Vec::new(),
                tools.into_iter().map(Into::into).collect(),
            ))
            .await;
        assert!(result.is_ok(), "tool-capable model should accept tools");
    }

    #[tokio::test]
    async fn image_to_vision_model_is_allowed() {
        use lingxi_core::types::{ContentBlock, ConversationMessage, ImageSource, MessageId};
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let adapter = make_adapter(transport);
        let msgs = vec![ConversationMessage::User {
            api_message_override: None,
            id: MessageId::new(),
            content: vec![ContentBlock::Image {
                source: ImageSource::Url {
                    url: "https://x/y.png".to_string(),
                },
            }],
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
            turn_companion: None,
            usage_limit_note: None,
            origin: None,
            turn_origin: None,
        }];
        // The capability check is in ModelRuntime.validate_capabilities; since
        // FakeTransport doesn't inspect the body, this exercises the whole path.
        let _ = adapter
            .messages_create(crate::MessagesCreateRequest::new(
                "claude-sonnet-4-20250514",
                None,
                None,
                msgs,
                Vec::new(),
            ))
            .await;
    }

    // ── New plan-named tests ──────────────────────────────────────────────────

    /// Plan test: 429 with `retry-after` triggers one sleep then succeeds.
    #[tokio::test]
    async fn live_path_surfaces_rate_limit_headers() {
        let mut rate_limit_headers = BTreeMap::new();
        rate_limit_headers.insert("retry-after".to_string(), "1".to_string());

        let transport = FakeTransport::sequence(vec![
            FakeResponse::Ok(ProviderResponse {
                status: 429,
                headers: rate_limit_headers,
                body_json: provider_payload(serde_json::json!({
                    "type": "error",
                    "error": {"type": "rate_limit_error", "message": "rate limited"}
                })),
                request_id: None,
            }),
            FakeResponse::Ok(ProviderResponse::json(200, ok_response_json())),
        ]);
        let adapter = make_adapter(transport.clone());
        let resp = adapter
            .messages_create(crate::MessagesCreateRequest::new(
                "claude-sonnet-4-20250514",
                None,
                None,
                Vec::new(),
                Vec::new(),
            ))
            .await
            .expect("ok after retry");
        assert_eq!(resp.stop_reason.as_deref(), Some("end_turn"));
        // Two executions: the 429 then the 200.
        assert_eq!(transport.seen_count(), 2);
    }

    /// Plan test: betas and User-Agent are injected post-prepare.
    #[tokio::test]
    async fn betas_and_user_agent_applied_post_prepare() {
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let adapter = make_adapter(transport.clone());
        let _ = adapter
            .messages_create(crate::MessagesCreateRequest::new(
                "claude-sonnet-4-20250514",
                None,
                None,
                Vec::new(),
                Vec::new(),
            ))
            .await
            .expect("ok");
        let headers = transport.seen_headers(0);
        assert!(
            headers.contains_key("anthropic-beta"),
            "anthropic-beta must be present; got: {headers:?}"
        );
        let ua = headers.get("user-agent").cloned().unwrap_or_default();
        assert!(
            ua.starts_with("claude-cli/"),
            "user-agent must start with claude-cli/; got: {ua}"
        );
    }

    // ── (cc 2.1.219) `anthropic-dispatch-id: v2s` opt-in ────────────────────

    /// Guards the process-global `tengu_cedar_lattice` override (and
    /// `_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL`).
    ///
    /// `telemetry::test_set_flag` writes a PROCESS-GLOBAL map, so serializing
    /// only the tests that flip it is not enough: while the flip is live, every
    /// *concurrently running* test in this binary reads the flipped value too.
    /// A leaked opt-in makes first-party attempts carry `anthropic-dispatch-id`,
    /// and [`ApiService::note_dispatch_header_failure`] then inserts one
    /// extra, budget-free "strip and retry" attempt on the first 5xx — which
    /// silently shifts every attempt-count and fallback-position assertion in
    /// the drive-loop tests (they consumed one more canned response than they
    /// expected and never reached the fallback entry).
    ///
    /// So: a test that FLIPS the flag takes [`cedar_lattice_write`]; a test
    /// whose expectations depend on the header being ABSENT takes
    /// [`cedar_lattice_read`]. Readers exclude writers, which is the whole
    /// invariant — concurrent readers of an unflipped flag are fine.
    /// Turns `tengu_cedar_lattice` on for the duration of a test and clears it
    /// on drop, so a panicking test cannot leak the opt-in into its neighbours.
    struct CedarLatticeOn;

    impl CedarLatticeOn {
        fn set() -> Self {
            ::telemetry::test_set_flag("tengu_cedar_lattice", true);
            Self
        }
    }

    impl Drop for CedarLatticeOn {
        fn drop(&mut self) {
            ::telemetry::test_clear_flag("tengu_cedar_lattice");
        }
    }

    #[tokio::test]
    async fn dispatch_header_absent_by_default() {
        // Reader: asserts the DEFAULT (unflipped) value, so it only has to
        // exclude the flippers, not its fellow readers.
        let _g = cedar_lattice_read();
        let headers = headers_after_inject_for_protocol(
            ProtocolFamily::AnthropicMessages,
            ProviderId::AnthropicFirstParty,
            "https://api.anthropic.com",
        )
        .await;
        assert!(
            !headers.contains_key("anthropic-dispatch-id"),
            "default-off: `tengu_cedar_lattice` defaults false — {headers:?}"
        );
    }

    /// `Mg` is `oMl({}, null)` (2.1.220 @226176806), so
    /// `Mg.CLAUDE_CODE_DISPATCH_V2S` is ALWAYS `undefined` and the nullish `??`
    /// always falls through to the flag: the env var can neither enable nor
    /// disable the header in the oracle. Regression guard for the port having
    /// treated a set env var as authoritative in both directions.
    #[tokio::test]
    async fn dispatch_header_ignores_the_inert_env_var() {
        let _g = cedar_lattice_write();
        std::env::set_var("CLAUDE_CODE_DISPATCH_V2S", "1");
        let flag_off = headers_after_inject_for_protocol(
            ProtocolFamily::AnthropicMessages,
            ProviderId::AnthropicFirstParty,
            "https://api.anthropic.com",
        )
        .await;
        std::env::set_var("CLAUDE_CODE_DISPATCH_V2S", "0");
        let flag_on = {
            let _on = CedarLatticeOn::set();
            headers_after_inject_for_protocol(
                ProtocolFamily::AnthropicMessages,
                ProviderId::AnthropicFirstParty,
                "https://api.anthropic.com",
            )
            .await
        };
        std::env::remove_var("CLAUDE_CODE_DISPATCH_V2S");
        assert!(
            !flag_off.contains_key("anthropic-dispatch-id"),
            "a truthy env var must NOT enable what the oracle cannot enable"
        );
        assert_eq!(
            flag_on.get("anthropic-dispatch-id").map(String::as_str),
            Some("v2s"),
            "env \"0\" must NOT disable a true flag (`??` is nullish-only)"
        );
    }

    /// `Ooe() = xn()==="firstParty" && Yd()` (2.1.220 @227683488): BOTH halves
    /// gate the header. `Yd()`/`d6r()`/`T1e()` require the base URL to resolve
    /// to host `api.anthropic.com` unless
    /// `_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL` is truthy.
    #[tokio::test]
    async fn dispatch_header_needs_both_halves_of_ooe() {
        let _g = cedar_lattice_write();
        let _on = CedarLatticeOn::set();
        let first_party = headers_after_inject_for_protocol(
            ProtocolFamily::AnthropicMessages,
            ProviderId::AnthropicFirstParty,
            "https://api.anthropic.com",
        )
        .await;
        // Anthropic-wire but NOT first-party: fails the `xn()` half.
        let compatible = headers_after_inject_for_protocol(
            ProtocolFamily::AnthropicMessages,
            ProviderId::OpenAICompatible {
                name: "proxy".to_string(),
            },
            "https://proxy.example.com",
        )
        .await;
        // First-party id pointed at an enterprise gateway: fails the `Yd()`
        // half, which the port used not to model at all.
        let gateway_base = headers_after_inject_for_protocol(
            ProtocolFamily::AnthropicMessages,
            ProviderId::AnthropicFirstParty,
            "https://proxy.example.com",
        )
        .await;
        // `URL.host` keeps a non-default port, so this is NOT the allow-listed
        // host either.
        let ported_host = headers_after_inject_for_protocol(
            ProtocolFamily::AnthropicMessages,
            ProviderId::AnthropicFirstParty,
            "https://api.anthropic.com:8443",
        )
        .await;
        std::env::set_var("_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL", "1");
        let assumed = headers_after_inject_for_protocol(
            ProtocolFamily::AnthropicMessages,
            ProviderId::AnthropicFirstParty,
            "https://proxy.example.com",
        )
        .await;
        std::env::remove_var("_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL");

        assert_eq!(
            first_party.get("anthropic-dispatch-id").map(String::as_str),
            Some("v2s")
        );
        assert!(!compatible.contains_key("anthropic-dispatch-id"));
        assert!(
            !gateway_base.contains_key("anthropic-dispatch-id"),
            "Yd(): a custom API base must get no dispatch header"
        );
        assert!(!ported_host.contains_key("anthropic-dispatch-id"));
        assert_eq!(
            assumed.get("anthropic-dispatch-id").map(String::as_str),
            Some("v2s"),
            "_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL short-circuits Yd()"
        );
    }

    /// `fB(i.querySource)!=="auxiliary"` (`fB` @227888239): utility queries —
    /// the side-query driver's compaction / recap / title-generation traffic —
    /// never carry the header, while an unclassified query (`fB(void 0)` ⇒
    /// `undefined`) does.
    #[tokio::test]
    async fn dispatch_header_skips_auxiliary_queries() {
        let _g = cedar_lattice_write();
        let _on = CedarLatticeOn::set();
        let adapter = make_adapter_for_protocol(
            ProtocolFamily::AnthropicMessages,
            ProviderId::AnthropicFirstParty,
            "https://api.anthropic.com",
        );
        let mut request = LlmRequest::new("model").with_user_text("hi");
        request.input.max_tokens = Some(4096);
        let mut aux = adapter.client.prepare(&request).await.expect("prepare");
        adapter
            .inject_headers(&mut aux, DispatchHeaderState::AUXILIARY)
            .unwrap();
        let mut main = adapter.client.prepare(&request).await.expect("prepare");
        adapter
            .inject_headers(&mut main, DispatchHeaderState::default())
            .unwrap();
        assert!(
            !aux.provider_request
                .headers
                .contains_key("anthropic-dispatch-id"),
            "auxiliary (side-query) traffic must not carry the header"
        );
        assert!(main
            .provider_request
            .headers
            .contains_key("anthropic-dispatch-id"));
    }

    struct DispatchTrafficTransport {
        requests: Mutex<Vec<ProviderRequest>>,
        fail_first: bool,
    }

    impl llm_runtime::test_support::FixtureTransport for DispatchTrafficTransport {
        fn execute<'a>(
            &'a self,
            request: &'a ProviderRequest,
        ) -> BoxFuture<'a, Result<ProviderResponse, LlmError>> {
            let mut requests = self.requests.lock().unwrap();
            requests.push(request.clone());
            let fail = self.fail_first && requests.len() == 1;
            Box::pin(async move {
                if fail {
                    Err(LlmError::ProviderInternal)
                } else {
                    Ok(ProviderResponse::json(200, ok_response_json()))
                }
            })
        }

        fn open_stream<'a>(
            &'a self,
            request: &'a ProviderRequest,
        ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>> {
            let mut requests = self.requests.lock().unwrap();
            requests.push(request.clone());
            let fail = self.fail_first && requests.len() == 1;
            Box::pin(async move {
                if fail {
                    Err(LlmError::ProviderInternal)
                } else {
                    Ok(StreamingResponse {
                        status: 200,
                        headers: BTreeMap::new(),
                        frames: Box::new(ScriptedFrames::new(
                            ScriptedStreamTransport::anthropic_success().frames.clone(),
                        )),
                    })
                }
            })
        }
    }
    llm_runtime::impl_fixture_transport!(DispatchTrafficTransport);

    struct DispatchRecoveryFrames(VecDeque<Result<crate::RawStreamFrame, LlmError>>);

    impl crate::FrameStream for DispatchRecoveryFrames {
        fn next_frame(&mut self) -> BoxFuture<'_, Result<Option<crate::RawStreamFrame>, LlmError>> {
            let result = self.0.pop_front().transpose();
            Box::pin(async move { result })
        }
    }

    #[derive(Clone, Copy)]
    enum DispatchFault {
        Http(u16, bool),
        Connection,
        Timeout,
        Body(bool),
    }

    struct DispatchRecoveryTransport {
        requests: Mutex<Vec<ProviderRequest>>,
        fault: DispatchFault,
        failures: usize,
    }

    impl DispatchRecoveryTransport {
        fn record(&self, request: &ProviderRequest) -> bool {
            let mut requests = self.requests.lock().unwrap();
            requests.push(request.clone());
            requests.len() <= self.failures
        }

        fn response(&self, failed: bool) -> Result<ProviderResponse, LlmError> {
            if !failed {
                return Ok(ProviderResponse::json(200, ok_response_json()));
            }
            match self.fault {
                DispatchFault::Http(status, declined) => Ok(ProviderResponse {
                    status,
                    headers: BTreeMap::from([
                        ("request-id".into(), "dispatch-request".into()),
                        (
                            "x-should-retry".into(),
                            if declined { "false" } else { "true" }.into(),
                        ),
                    ]),
                    request_id: Some("dispatch-request".into()),
                    body_json: provider_payload(
                        serde_json::json!({"type":"error","error":{"type":"api_error","message":"service unavailable"}}),
                    ),
                }),
                DispatchFault::Timeout => Err(LlmError::TransportTimeout {
                    message: "HTTP request timed out".into(),
                }),
                DispatchFault::Connection | DispatchFault::Body(_) => Err(LlmError::Transport {
                    message: "connection reset".into(),
                }),
            }
        }

        fn headers(&self) -> Vec<Option<String>> {
            self.requests
                .lock()
                .unwrap()
                .iter()
                .map(|request| request.headers.get(DISPATCH_ID_HEADER).cloned())
                .collect()
        }
    }

    impl llm_runtime::test_support::FixtureTransport for DispatchRecoveryTransport {
        fn execute<'a>(
            &'a self,
            request: &'a ProviderRequest,
        ) -> BoxFuture<'a, Result<ProviderResponse, LlmError>> {
            let result = self.response(self.record(request));
            Box::pin(async move { result })
        }

        fn open_stream<'a>(
            &'a self,
            request: &'a ProviderRequest,
        ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>> {
            let failed = self.record(request);
            let result = if failed && matches!(self.fault, DispatchFault::Body(_)) {
                let mut frames = Vec::new();
                if matches!(self.fault, DispatchFault::Body(true)) {
                    frames.push(Ok(crate::RawStreamFrame::new(serde_json::to_vec(&serde_json::json!({"type":"message_start","message":{"id":"msg_dispatch","model":"claude-sonnet-4-20250514","content":[],"usage":{"input_tokens":1,"output_tokens":0}}})).unwrap())));
                }
                frames.push(Err(LlmError::Transport {
                    message: "connection reset".into(),
                }));
                Ok(StreamingResponse {
                    status: 200,
                    headers: BTreeMap::new(),
                    frames: Box::new(DispatchRecoveryFrames(frames.into())),
                })
            } else {
                self.response(failed).map(|response| {
                    let frames = if response.status >= 400 {
                        vec![Ok(crate::RawStreamFrame::new(
                            serde_json::to_vec(&response.body_json).unwrap(),
                        ))]
                    } else {
                        ScriptedStreamTransport::anthropic_success()
                            .frames
                            .iter()
                            .cloned()
                            .map(|bytes| Ok(crate::RawStreamFrame::new(bytes)))
                            .collect()
                    };
                    StreamingResponse {
                        status: response.status,
                        headers: response.headers,
                        frames: Box::new(DispatchRecoveryFrames(frames.into())),
                    }
                })
            };
            Box::pin(async move { result })
        }
    }
    llm_runtime::impl_fixture_transport!(DispatchRecoveryTransport);

    async fn dispatch_result(
        service: &ApiService,
        source: &str,
        streaming: bool,
        side_query: bool,
    ) -> Result<(), LlmError> {
        use futures::StreamExt;
        let model = "claude-sonnet-4-20250514";
        let mut request = LlmRequest::new(model).with_user_text("hi");
        request.input.max_tokens = Some(100);
        request.execution.query_source = Some(source.into());
        if streaming {
            let mut stream = service.stream_request(request).await?;
            while let Some(event) = stream.next().await {
                event?;
            }
        } else if side_query {
            service.execute_side_query_request(request).await?;
        } else {
            let mut request = crate::MessagesCreateRequest::new(
                model,
                None,
                None,
                vec![ConversationMessage::user(
                    lingxi_core::types::MessageId::new(),
                    "hi".into(),
                )],
                Vec::new(),
            );
            request.opts.max_output_tokens = Some(100);
            request.opts.query_source = Some(source.into());
            service.messages_create(request).await?;
        }
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn physical_dispatch_287_http_recovery_and_query_reset() {
        let _lock = cedar_lattice_write();
        for cedar in [false, true] {
            let _cedar = cedar.then(CedarLatticeOn::set);
            for dreamy in [false, true] {
                let _dreamy = DreamyFrostGuard::set(dreamy);
                for source in ["sdk", "hook_prompt"] {
                    for (streaming, side_query) in [(false, false), (true, false), (false, true)] {
                        for declined in [false, true] {
                            let transport = Arc::new(DispatchRecoveryTransport {
                                requests: Mutex::new(Vec::new()),
                                fault: DispatchFault::Http(503, declined),
                                failures: 1,
                            });
                            let mut service = make_adapter_with_retries(transport.clone(), Some(0));
                            service.set_thinking(crate::model::thinking::ThinkingConfig::Disabled);
                            let sink = Arc::new(::telemetry::InMemorySink::new());
                            let bus = Arc::new(::telemetry::AnalyticsBus::new());
                            bus.attach_sink(sink.clone()).await;
                            service.analytics = Some(bus);
                            let first = if dreamy {
                                Some("v2d")
                            } else if cedar && source == "sdk" {
                                Some("v2s")
                            } else {
                                None
                            };
                            let start = tokio::time::Instant::now();
                            dispatch_result(&service, source, streaming, side_query)
                                .await
                                .unwrap();
                            let elapsed = start.elapsed();
                            assert_eq!(
                                transport.headers(),
                                [first.map(str::to_owned), Some("v2p".into())],
                                "{cedar}/{dreamy}/{source}/{streaming}/{side_query}/{declined}"
                            );
                            assert_eq!(
                                service.last_retry_count(),
                                0,
                                "dispatch recovery is outside the ordinary retry budget"
                            );
                            if first.is_none() && !declined {
                                assert!((Duration::from_millis(500)..=Duration::from_millis(625))
                                    .contains(&elapsed));
                            } else {
                                assert_eq!(elapsed, Duration::ZERO);
                            }
                            let events = sink.events().await;
                            let events: Vec<_> = events
                                .iter()
                                .filter(|event| event.name == "tengu_dispatch_header_fallback")
                                .collect();
                            assert_eq!(events.len(), 1);
                            let metadata = &events[0].metadata;
                            for (key, value) in [
                                ("dispatch", first.unwrap_or("none")),
                                ("resend_dispatch", "v2p"),
                                ("query_source", source),
                                ("request_id", "dispatch-request"),
                            ] {
                                assert_eq!(
                                    serde_json::to_value(&metadata[key]).unwrap(),
                                    serde_json::Value::from(value)
                                );
                            }
                            assert_eq!(
                                serde_json::to_value(&metadata["status"]).unwrap(),
                                serde_json::Value::from(503)
                            );
                            let reason = if first.is_some() {
                                "5xx"
                            } else if declined {
                                "headerless_decline"
                            } else {
                                "headerless_retryable_503"
                            };
                            assert_eq!(
                                serde_json::to_value(&metadata["reason"]).unwrap(),
                                serde_json::Value::from(reason)
                            );
                            dispatch_result(&service, source, streaming, side_query)
                                .await
                                .unwrap();
                            assert_eq!(
                                transport.headers()[2].as_deref(),
                                first,
                                "the next query must reset its recovery state"
                            );
                        }
                    }
                }
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn physical_dispatch_287_recovery_latch_is_once_per_query() {
        let _lock = cedar_lattice_write();
        let _dreamy = DreamyFrostGuard::set(true);
        for fault in [
            DispatchFault::Http(500, false),
            DispatchFault::Connection,
            DispatchFault::Timeout,
            DispatchFault::Body(false),
        ] {
            for streaming in [false, true] {
                let transport = Arc::new(DispatchRecoveryTransport {
                    requests: Mutex::new(Vec::new()),
                    fault,
                    failures: 2,
                });
                let service = make_adapter_with_retries(transport.clone(), Some(0));
                assert!(dispatch_result(&service, "sdk", streaming, false)
                    .await
                    .is_err());
                assert_eq!(
                    transport.headers(),
                    [Some("v2d".into()), Some("v2p".into())]
                );
            }
        }
        let transport = Arc::new(DispatchRecoveryTransport {
            requests: Mutex::new(Vec::new()),
            fault: DispatchFault::Body(true),
            failures: 1,
        });
        let service = make_adapter_with_retries(transport.clone(), Some(0));
        assert!(dispatch_result(&service, "sdk", true, false).await.is_err());
        assert_eq!(
            transport.headers(),
            [Some("v2d".into())],
            "forwarded events suppress the body-phase resend"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn dispatch_287_does_not_add_recovery_to_other_provider_routes() {
        let _lock = cedar_lattice_write();
        let _dreamy = DreamyFrostGuard::set(true);
        for (protocol, provider, base) in [
            (
                ProtocolFamily::AnthropicMessages,
                ProviderId::AnthropicFirstParty,
                "https://gateway.example.com",
            ),
            (
                ProtocolFamily::AnthropicMessages,
                ProviderId::OpenAICompatible {
                    name: "anthropic-compatible".into(),
                },
                "https://api.anthropic.com",
            ),
            (
                ProtocolFamily::OpenAiChat,
                ProviderId::OpenAICompatible {
                    name: "custom".into(),
                },
                "https://custom.example.com",
            ),
            (
                ProtocolFamily::OpenAiResponses,
                ProviderId::OpenAICompatible {
                    name: "responses".into(),
                },
                "https://custom.example.com",
            ),
        ] {
            for streaming in [false, true] {
                let transport = Arc::new(DispatchRecoveryTransport {
                    requests: Mutex::new(Vec::new()),
                    fault: DispatchFault::Http(503, false),
                    failures: 2,
                });
                let mut service = make_adapter_for_protocol_with_transport(
                    protocol,
                    provider.clone(),
                    base,
                    "p",
                    "claude-sonnet-4-20250514",
                    transport.clone(),
                );
                service.settings_max_retries = Some(0);
                assert!(dispatch_result(&service, "sdk", streaming, false)
                    .await
                    .is_err());
                assert_eq!(
                    transport.headers(),
                    [None],
                    "{protocol:?}/{provider:?}/{streaming}"
                );
            }
        }
    }

    struct DreamyFrostGuard;
    impl DreamyFrostGuard {
        fn set(enabled: bool) -> Self {
            ::telemetry::test_set_flag("tengu_dreamy_frost", enabled);
            Self
        }
    }
    impl Drop for DreamyFrostGuard {
        fn drop(&mut self) {
            ::telemetry::test_clear_flag("tengu_dreamy_frost");
        }
    }

    async fn run_dispatch_traffic(service: &ApiService, source: Option<&str>, streaming: bool) {
        use futures::StreamExt;

        let model = "claude-sonnet-4-20250514";
        if streaming {
            let mut request = LlmRequest::new(model).with_user_text("hi");
            request.input.max_tokens = Some(100);
            request.execution.query_source = source.map(str::to_owned);
            let mut stream = service.stream_request(request).await.unwrap();
            while let Some(event) = stream.next().await {
                event.unwrap();
            }
        } else {
            let mut request = crate::MessagesCreateRequest::new(
                model,
                None,
                None,
                vec![ConversationMessage::user(
                    lingxi_core::types::MessageId::new(),
                    "hi".into(),
                )],
                Vec::new(),
            );
            request.opts.max_output_tokens = Some(100);
            request.opts.query_source = source.map(str::to_owned);
            service.messages_create(request).await.unwrap();
        }
    }

    #[tokio::test]
    async fn physical_stream_and_owned_main_dispatch_follow_native_query_source() {
        let _lock = cedar_lattice_write();
        let _cedar = CedarLatticeOn::set();
        for dreamy in [false, true] {
            let _dreamy = DreamyFrostGuard::set(dreamy);
            for (source, ordinary_v2s) in [
                (None, true),
                (Some("repl_main_thread"), true),
                (Some("repl_main_thread:resume"), true),
                (Some("sdk"), true),
                (Some("agent:worker"), true),
                (Some("hook_agent"), true),
                (Some("hook_prompt"), false),
                (Some("session_memory"), false),
            ] {
                for streaming in [false, true] {
                    let transport = Arc::new(DispatchTrafficTransport {
                        requests: Mutex::new(Vec::new()),
                        fail_first: false,
                    });
                    let service = make_adapter_with_retries(transport.clone(), Some(0));
                    service.set_thinking(crate::model::thinking::ThinkingConfig::Disabled);
                    run_dispatch_traffic(&service, source, streaming).await;
                    let requests = transport.requests.lock().unwrap();
                    assert_eq!(requests.len(), 1);
                    let expected = if dreamy {
                        Some("v2d")
                    } else if ordinary_v2s {
                        Some("v2s")
                    } else {
                        None
                    };
                    assert_eq!(
                        requests[0]
                            .headers
                            .get("anthropic-dispatch-id")
                            .map(String::as_str),
                        expected,
                        "source={source:?}, streaming={streaming}, dreamy={dreamy}"
                    );
                }
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn auxiliary_stream_and_owned_main_error_use_normal_retry_budget() {
        let _lock = cedar_lattice_write();
        let _cedar = CedarLatticeOn::set();
        let _dreamy = DreamyFrostGuard::set(false);
        for streaming in [false, true] {
            let transport = Arc::new(DispatchTrafficTransport {
                requests: Mutex::new(Vec::new()),
                fail_first: true,
            });
            let service = make_adapter_with_retries(transport.clone(), Some(1));
            service.set_thinking(crate::model::thinking::ThinkingConfig::Disabled);
            run_dispatch_traffic(&service, Some("hook_prompt"), streaming).await;
            let requests = transport.requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            assert!(requests
                .iter()
                .all(|request| !request.headers.contains_key("anthropic-dispatch-id")));
            assert_eq!(
                service.last_retry_count(),
                1,
                "auxiliary errors must consume the normal retry budget, without a free header-strip replay"
            );
        }
    }

    #[tokio::test]
    async fn dispatch_recovery_resends_v2p_only_for_this_query() {
        let _g = cedar_lattice_write();
        let _on = CedarLatticeOn::set();
        let adapter = make_adapter_for_protocol(
            ProtocolFamily::AnthropicMessages,
            ProviderId::AnthropicFirstParty,
            "https://api.anthropic.com",
        );
        let mut request = LlmRequest::new("model").with_user_text("hi");
        request.input.max_tokens = Some(4096);
        let mut query = DispatchHeaderState::default();
        let mut prepared = adapter.client.prepare(&request).await.unwrap();
        let (attempt, _) = adapter.inject_headers(&mut prepared, query).unwrap();
        assert_eq!(attempt.value.as_deref(), Some("v2s"));
        assert!(ApiService::note_dispatch_header_failure(
            &mut query,
            &attempt,
            &LlmError::RateLimited {
                retry_after: None,
                scope: None
            },
            Some((429, false)),
        )
        .is_none());
        let fallback = ApiService::note_dispatch_header_failure(
            &mut query,
            &attempt,
            &LlmError::ProviderInternal,
            Some((500, false)),
        )
        .unwrap();
        assert_eq!(fallback.reason, "5xx");
        assert_eq!(fallback.status, Some(500));
        assert_eq!(fallback.previous.as_deref(), Some("v2s"));
        assert!(!fallback.wait);
        let mut retry = adapter.client.prepare(&request).await.unwrap();
        let (retry_attempt, _) = adapter.inject_headers(&mut retry, query).unwrap();
        assert_eq!(
            retry
                .provider_request
                .headers
                .get(DISPATCH_ID_HEADER)
                .map(String::as_str),
            Some("v2p")
        );
        assert!(ApiService::note_dispatch_header_failure(
            &mut query,
            &retry_attempt,
            &LlmError::ProviderInternal,
            Some((503, false)),
        )
        .is_none());
        let mut next = adapter.client.prepare(&request).await.unwrap();
        adapter
            .inject_headers(&mut next, DispatchHeaderState::default())
            .unwrap();
        assert_eq!(
            next.provider_request
                .headers
                .get(DISPATCH_ID_HEADER)
                .map(String::as_str),
            Some("v2s")
        );
    }

    /// A `FrameStream` whose body drops with a connection error before any
    /// event — the oracle's `Bs!==null && !oc`.
    struct ConnErrOnFirstFrame;

    impl crate::FrameStream for ConnErrOnFirstFrame {
        fn next_frame(&mut self) -> BoxFuture<'_, Result<Option<crate::RawStreamFrame>, LlmError>> {
            Box::pin(async {
                Err(LlmError::Transport {
                    message: "ECONNRESET".to_string(),
                })
            })
        }
    }

    /// Connect succeeds on both attempts; the FIRST stream body drops before
    /// the first event, the second is a clean anthropic stream. Records the
    /// `anthropic-dispatch-id` value seen per attempt.
    struct BodyPhaseDropThenOk {
        attempts: Mutex<Vec<Option<String>>>,
    }

    impl llm_runtime::test_support::FixtureTransport for BodyPhaseDropThenOk {
        fn execute<'a>(
            &'a self,
            _request: &'a ProviderRequest,
        ) -> BoxFuture<'a, Result<ProviderResponse, LlmError>> {
            Box::pin(async {
                Err(LlmError::Transport {
                    message: "execute not used".to_string(),
                })
            })
        }

        fn open_stream<'a>(
            &'a self,
            request: &'a ProviderRequest,
        ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>> {
            let attempt = {
                let mut seen = self
                    .attempts
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                seen.push(request.headers.get("anthropic-dispatch-id").cloned());
                seen.len()
            };
            Box::pin(async move {
                let frames: Box<dyn crate::FrameStream> = if attempt == 1 {
                    Box::new(ConnErrOnFirstFrame)
                } else {
                    Box::new(ScriptedFrames::new(vec![
                        br#"{"type":"message_start","message":{"id":"msg_t","model":"claude-sonnet-4-20250514","usage":{"input_tokens":1,"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}"#.to_vec(),
                        br#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}"#.to_vec(),
                        br#"{"type":"message_stop"}"#.to_vec(),
                    ]))
                };
                Ok(StreamingResponse {
                    status: 200,
                    headers: BTreeMap::new(),
                    frames,
                })
            })
        }
    }
    llm_runtime::impl_fixture_transport!(BodyPhaseDropThenOk);

    /// The drive loop's one-frame lookahead must resend v2p, emit `tengu_dispatch_header_fallback{reason:"body_phase",
    /// status:"none"}` and re-open — and the seeded frame must flow through the
    /// unfold untouched on the retry that succeeds.
    #[tokio::test]
    async fn dispatch_body_phase_drop_resends_v2p_and_re_opens() {
        use ::telemetry::AnalyticsValue;
        use futures::StreamExt as _;

        let _g = cedar_lattice_write();
        let _on = CedarLatticeOn::set();
        let transport = Arc::new(BodyPhaseDropThenOk {
            attempts: Mutex::new(Vec::new()),
        });
        let (adapter, sink) = make_stream_adapter_with_bus(transport.clone()).await;

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
        let mut yielded = 0_usize;
        while let Some(item) = stream.next().await {
            assert!(item.is_ok(), "retry stream must decode cleanly: {item:?}");
            yielded += 1;
        }
        assert!(yielded > 0, "the seeded retry stream must yield events");

        let seen = transport
            .attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert_eq!(
            seen,
            vec![Some("v2s".to_string()), Some("v2p".to_string())],
            "the body-phase retry must resend v2p"
        );

        let events = sink.events().await;
        let fallback = events
            .iter()
            .find(|e| e.name == "tengu_dispatch_header_fallback")
            .expect("body-phase fallback event must be emitted");
        assert!(
            matches!(&fallback.metadata["reason"], AnalyticsValue::String(s) if s == "body_phase"),
            "reason must be body_phase; got {:?}",
            fallback.metadata["reason"]
        );
        assert!(
            matches!(&fallback.metadata["status"], AnalyticsValue::String(s) if s == "none"),
            "status must stringify to none; got {:?}",
            fallback.metadata["status"]
        );
    }

    #[tokio::test]
    async fn anthropic_beta_header_is_anthropic_family_and_provider_scoped() {
        let anthropic_headers = headers_after_inject_for_protocol(
            ProtocolFamily::AnthropicMessages,
            ProviderId::AnthropicFirstParty,
            "https://api.anthropic.com",
        )
        .await;
        assert!(anthropic_headers.contains_key("anthropic-beta"));

        let gateway_headers = headers_after_inject_for_protocol(
            ProtocolFamily::AnthropicMessages,
            ProviderId::AnthropicFirstParty,
            "https://gateway.example",
        )
        .await;
        assert!(
            gateway_headers.contains_key("anthropic-beta"),
            "a first-party provider keeps its beta policy behind a gateway"
        );
        let compatible_headers = headers_after_inject_for_protocol(
            ProtocolFamily::AnthropicMessages,
            ProviderId::Custom {
                name: "compatible".into(),
            },
            "https://gateway.example",
        )
        .await;
        assert!(
            !compatible_headers.contains_key("anthropic-beta"),
            "an Anthropic-wire custom endpoint must not inherit first-party betas"
        );

        for (protocol, base_url, name) in [
            (
                ProtocolFamily::FoundryClaude,
                "https://example.services.ai.azure.com/anthropic",
                "foundry-claude",
            ),
            (
                ProtocolFamily::VertexClaude,
                "https://us-central1-aiplatform.googleapis.com/v1/projects/p/locations/us-central1",
                "vertex-claude",
            ),
            (
                ProtocolFamily::BedrockClaude,
                "https://bedrock-runtime.us-east-1.amazonaws.com",
                "bedrock-claude",
            ),
        ] {
            let headers = headers_after_inject_for_protocol(
                protocol,
                ProviderId::OpenAICompatible {
                    name: name.to_string(),
                },
                base_url,
            )
            .await;
            assert!(
                headers.contains_key("anthropic-beta"),
                "{name} must receive its provider-specific beta header: {headers:?}"
            );
        }

        let routes = [
            (
                ProtocolFamily::AnthropicMessages,
                "https://gateway.example",
                "anthropic-compatible",
            ),
            (
                ProtocolFamily::OpenAiChat,
                "https://api.openai.com/v1",
                "openai",
            ),
            (
                ProtocolFamily::OpenAiResponses,
                "https://api.openai.com/v1",
                "openai-responses",
            ),
            (
                ProtocolFamily::GeminiGenerateContent,
                "https://generativelanguage.googleapis.com/v1beta",
                "gemini",
            ),
            (
                ProtocolFamily::AzureOpenAi,
                "https://example.openai.azure.com/openai/deployments/model",
                "azure-openai",
            ),
            (
                ProtocolFamily::VertexGemini,
                "https://us-central1-aiplatform.googleapis.com/v1/projects/p/locations/us-central1/publishers/google/models/model:generateContent",
                "vertex-gemini",
            ),
        ];

        for (protocol, base_url, name) in routes {
            let headers = headers_after_inject_for_protocol(
                protocol,
                ProviderId::OpenAICompatible {
                    name: name.to_string(),
                },
                base_url,
            )
            .await;
            assert!(
                !headers.contains_key("anthropic-beta"),
                "{name} must not receive anthropic-beta: {headers:?}"
            );
            assert!(headers.contains_key("user-agent"));
            assert!(
                !headers.contains_key("x-request-id"),
                "Host must not synthesize a provider request-id for {name}"
            );
            assert!(
                !headers.contains_key("x-client-request-id"),
                "the SDK's selected-route request-id policy is outside this Host-only injector"
            );
        }
    }

    #[tokio::test]
    async fn user_agent_is_provider_aware() {
        // Anthropic-family routes keep the byte-faithful claude-cli UA.
        for (proto, pid, url) in [
            (
                ProtocolFamily::AnthropicMessages,
                ProviderId::AnthropicFirstParty,
                "https://api.anthropic.com",
            ),
            (
                ProtocolFamily::BedrockClaude,
                ProviderId::BedrockClaude,
                "https://bedrock-runtime.us-east-1.amazonaws.com",
            ),
        ] {
            let h = headers_after_inject_for_protocol(proto, pid, url).await;
            let ua = h.get("user-agent").expect("ua");
            assert!(ua.starts_with("claude-cli/"), "anthropic-family UA: {ua}");
        }

        // Non-Anthropic routes get a neutral UA, never claude-cli.
        let h = headers_after_inject_for_protocol(
            ProtocolFamily::OpenAiChat,
            ProviderId::OpenAICompatible {
                name: "openai".to_string(),
            },
            "https://api.openai.com/v1",
        )
        .await;
        let ua = h.get("user-agent").expect("ua");
        assert!(ua.starts_with("LingXi-Code/"), "neutral UA expected: {ua}");
        assert!(!ua.contains("claude-cli"), "must not leak claude-cli: {ua}");
    }

    #[tokio::test]
    async fn anthropic_compatibility_version_keeps_neutral_product_identity() {
        for (protocol, provider, url, expected) in [
            (
                ProtocolFamily::AnthropicMessages,
                ProviderId::AnthropicFirstParty,
                "https://api.anthropic.com",
                "claude-cli/2.1.293 (external, sdk-cli)",
            ),
            (
                ProtocolFamily::OpenAiChat,
                ProviderId::OpenAICompatible {
                    name: "openai".into(),
                },
                "https://api.openai.com/v1",
                "LingXi-Code/0.12.0",
            ),
        ] {
            let mut adapter = make_adapter_for_protocol(protocol, provider, url)
                .with_anthropic_compatible_version("2.1.293")
                .with_anthropic_client_metadata(lingxi_llm_client::providers::anthropic::request_policy::AnthropicClientMetadata::claude_code_2_1_293("darwin", "aarch64"));
            if protocol == ProtocolFamily::AnthropicMessages {
                adapter = adapter.with_native_anthropic_endpoint(url);
            }
            adapter.ua = UserAgentEnv {
                user_type: Some("external".into()),
                entrypoint: Some("sdk-cli".into()),
                ..Default::default()
            };
            adapter.version = "0.12.0".into();
            let mut request = LlmRequest::new("model").with_user_text("hi");
            if protocol == ProtocolFamily::AnthropicMessages {
                request.input.max_tokens = Some(4096);
            }
            let mut prepared = adapter.client.prepare(&request).await.unwrap();
            adapter.apply_user_agent(&mut prepared);
            assert_eq!(prepared.provider_request.headers["user-agent"], expected);
            if protocol == ProtocolFamily::AnthropicMessages {
                assert_eq!(
                    prepared.provider_request.headers["x-stainless-package-version"],
                    "0.128.0"
                );
                assert_eq!(
                    prepared.provider_request.headers["x-stainless-runtime-version"],
                    "v26.3.0"
                );
                assert_eq!(prepared.provider_request.headers["x-stainless-os"], "MacOS");
                assert_eq!(
                    prepared.provider_request.headers["x-stainless-arch"],
                    "arm64"
                );
            } else {
                assert!(!prepared
                    .provider_request
                    .headers
                    .keys()
                    .any(|name| name.to_ascii_lowercase().starts_with("x-stainless-")));
            }
        }
    }

    #[test]
    fn explicit_api_identity_metadata_does_not_read_process_metadata() {
        let extra = serde_json::json!({"scope":"embedding","device_id":"stale"})
            .as_object()
            .unwrap()
            .clone();
        assert_eq!(
            ApiService::build_api_metadata_user_id_with_extra(
                "device",
                "account",
                "session",
                Some("parent"),
                Some(extra)
            ),
            r#"{"scope":"embedding","device_id":"device","account_uuid":"account","session_id":"session","parent_session_id":"parent"}"#
        );
        assert_eq!(
            ApiService::build_api_metadata_user_id_with_extra(
                "device", "account", "session", None, None
            ),
            r#"{"device_id":"device","account_uuid":"account","session_id":"session"}"#
        );
    }

    #[tokio::test]
    async fn request_session_snapshot_survives_clear_and_new_admissions_refresh() {
        let original = lingxi_core::types::SessionId::new().as_uuid().to_string();
        let replacement = lingxi_core::types::SessionId::new().as_uuid().to_string();
        let live = Arc::new(std::sync::Mutex::new(original.clone()));
        let metadata = ApiService::build_api_metadata_user_id_with_extra(
            "device", "account", "boot", None, None,
        );
        let adapter = make_adapter_for_protocol(
            ProtocolFamily::AnthropicMessages,
            ProviderId::AnthropicFirstParty,
            "https://api.anthropic.com",
        )
        .with_request_metadata(crate::RequestMetadata { user_id: metadata.clone() })
        .with_anthropic_client_metadata(
            lingxi_llm_client::providers::anthropic::request_policy::AnthropicClientMetadata::claude_code_2_1_293("darwin", "aarch64"),
        )
        .with_native_anthropic_endpoint("https://api.anthropic.com");
        let owner = live.clone();
        adapter.set_request_session_id_source(Arc::new(move || {
            let owner = owner.clone();
            Box::pin(async move { Some(owner.lock().unwrap().clone()) })
        }));
        let mut admitted = LlmRequest::new("model").with_user_text("hi");
        admitted.input.max_tokens = Some(4096);
        admitted.input.metadata = lingxi_llm_client::protocol::JsonPayload::from_generated_value(
            &serde_json::json!({"user_id":metadata}),
        )
        .unwrap();
        adapter.capture_request_session_id(&mut admitted).await;
        *live.lock().unwrap() = replacement.clone();
        adapter.capture_request_session_id(&mut admitted).await;
        assert_eq!(
            admitted.execution.request_session_id.as_deref(),
            Some(original.as_str())
        );
        let mut prepared = adapter.client.prepare(&admitted).await.unwrap();
        adapter.apply_user_agent(&mut prepared);
        assert_eq!(
            prepared.provider_request.headers["x-claude-code-session-id"],
            original
        );
        let user_id = admitted
            .input
            .metadata
            .string_at("/user_id")
            .expect("request metadata retains the host user_id");
        let identity: serde_json::Value = serde_json::from_str(&user_id).unwrap();
        assert_eq!(identity["session_id"], original);
        let mut next = LlmRequest::new("model").with_user_text("next");
        adapter.capture_request_session_id(&mut next).await;
        assert_eq!(
            next.execution.request_session_id.as_deref(),
            Some(replacement.as_str())
        );
    }

    #[tokio::test]
    async fn request_parent_scope_takes_precedence_over_current_session_and_payload() {
        let parent = lingxi_core::types::SessionId::new();
        let current = lingxi_core::types::SessionId::new().as_uuid().to_string();
        let adapter = make_adapter_for_protocol(
            ProtocolFamily::AnthropicMessages,
            ProviderId::AnthropicFirstParty,
            "https://api.anthropic.com",
        );
        adapter.set_request_session_id_source(Arc::new(move || {
            let current = current.clone();
            Box::pin(async move { Some(current) })
        }));
        let mut request = LlmRequest::new("model").with_user_text("child");
        request.input.metadata = lingxi_llm_client::protocol::JsonPayload::parse(
            r#"{"session_id":"provider-authored-id"}"#,
        )
        .unwrap();
        lingxi_core::host::session_flags::scope_request_session_id(parent, async {
            adapter.capture_request_session_id(&mut request).await;
        })
        .await;
        assert_eq!(
            request.execution.request_session_id,
            Some(parent.as_uuid().to_string())
        );
    }

    struct RegisteredRequestSession(lingxi_core::types::SessionId);
    #[async_trait::async_trait]
    impl crate::ModelAttemptHooks for RegisteredRequestSession {
        fn request_session_id(
            &self,
            _: &lingxi_core::host::ModelAttemptContext,
        ) -> Option<lingxi_core::types::SessionId> {
            Some(self.0)
        }
        async fn begin(
            &self,
            _: &lingxi_core::host::ModelAttemptContext,
            _: &LlmRequest,
            _: &crate::PreparedLlmCall,
        ) -> Result<Box<dyn crate::ModelAttemptLease>, LlmError> {
            unreachable!("identity lookup does not dispatch")
        }
    }
    #[tokio::test]
    async fn registered_request_origin_precedes_live_and_unrelated_task_scope() {
        let origin = lingxi_core::types::SessionId::new();
        let current = lingxi_core::types::SessionId::new();
        let adapter = make_adapter_for_protocol(
            ProtocolFamily::AnthropicMessages,
            ProviderId::AnthropicFirstParty,
            "https://api.anthropic.com",
        );
        adapter.set_model_attempt_hooks(Arc::new(RegisteredRequestSession(origin)));
        adapter.set_request_session_id_source(Arc::new(|| {
            panic!("registered requests never consult current owner")
        }));
        let run = lingxi_core::host::ModelAttemptRun::new(Arc::new(()));
        let mut request = LlmRequest::new("model");
        request.execution.model_attempt = Some(
            run.context(lingxi_core::host::ModelAttemptStage::Panel, Some(0))
                .unwrap(),
        );
        lingxi_core::host::session_flags::scope_request_session_id(current, async {
            adapter.capture_request_session_id(&mut request).await;
        })
        .await;
        assert_eq!(
            request.execution.request_session_id,
            Some(origin.as_uuid().to_string())
        );
    }

    #[tokio::test]
    async fn authenticator_user_agent_is_not_duplicated() {
        // Simulate the Copilot authenticator having set `User-Agent` during
        // prepare(): inject_headers must NOT add a second lowercase `user-agent`.
        let adapter = make_adapter_for_protocol(
            ProtocolFamily::OpenAiChat,
            ProviderId::OpenAICompatible {
                name: "github-copilot".to_string(),
            },
            "https://api.githubcopilot.com",
        );
        let request = LlmRequest::new("model").with_user_text("hi");
        let mut prepared = adapter.client.prepare(&request).await.expect("prepare");
        prepared
            .provider_request
            .headers
            .insert("User-Agent".to_string(), "LingXi-Code".to_string());
        adapter
            .inject_headers(&mut prepared, DispatchHeaderState::default())
            .unwrap();
        let h = &prepared.provider_request.headers;
        assert_eq!(h.get("User-Agent").map(String::as_str), Some("LingXi-Code"));
        assert!(
            !h.contains_key("user-agent"),
            "no duplicate lowercase user-agent: {h:?}"
        );
    }

    /// Plan test: budget terminates after DEFAULT_MAX_RETRIES + 1 executions.
    #[tokio::test]
    async fn retry_terminal_after_budget() {
        // The drive loop's attempt budget is only stable while the dispatch
        // opt-in stays OFF: a leaked `tengu_cedar_lattice` adds one extra,
        // budget-free "strip and retry" attempt on the first 5xx.
        let _dispatch_off = cedar_lattice_read();
        // All responses are 500 — should retry DEFAULT_MAX_RETRIES times then fail.
        let transport = FakeTransport::sequence(vec![FakeResponse::Ok(ProviderResponse::json(
            500,
            serde_json::json!({"type": "error", "error": {"type": "api_error", "message": "internal"}}),
        ))]);
        let adapter = make_adapter_with_routing(
            transport.clone(),
            std::collections::BTreeMap::new(),
            None,
            Some(0),
        );
        let result = adapter
            .messages_create(crate::MessagesCreateRequest::new(
                "claude-sonnet-4-20250514",
                None,
                None,
                Vec::new(),
                Vec::new(),
            ))
            .await;
        assert!(result.is_err(), "must fail after exhausting budget");
        // Should have tried DEFAULT_MAX_RETRIES + 1 = 11 times.
        assert_eq!(
            u32::try_from(transport.seen_count()).unwrap(),
            DEFAULT_MAX_RETRIES + 1,
            "expected {} executions, got {}",
            DEFAULT_MAX_RETRIES + 1,
            transport.seen_count()
        );
    }

    /// Native headerless 503 decline gets one v2p retry, then is terminal.
    #[tokio::test]
    async fn headerless_503_decline_retries_v2p_once_then_is_terminal() {
        let _flags = cedar_lattice_read();
        let mut headers = BTreeMap::new();
        headers.insert("x-should-retry".to_string(), "false".to_string());

        let transport = FakeTransport::always(ProviderResponse {
            status: 503,
            headers,
            body_json: provider_payload(serde_json::json!({
                "type": "error",
                "error": {"type": "api_error", "message": "service unavailable"}
            })),
            request_id: None,
        });
        let adapter = make_adapter(transport.clone());
        let result = adapter
            .messages_create(crate::MessagesCreateRequest::new(
                "claude-sonnet-4-20250514",
                None,
                None,
                Vec::new(),
                Vec::new(),
            ))
            .await;
        assert!(result.is_err(), "x-should-retry:false must be terminal");
        assert_eq!(
            transport.seen_count(),
            2,
            "one native v2p resend, then terminal"
        );
        assert!(!transport.seen_headers(0).contains_key(DISPATCH_ID_HEADER));
        assert_eq!(
            transport
                .seen_headers(1)
                .get(DISPATCH_ID_HEADER)
                .map(String::as_str),
            Some("v2p")
        );
    }

    /// Plan test: tool_use id round-trips through the adapter without mangling.
    #[tokio::test]
    async fn tool_use_id_round_trip_within_turn() {
        let response_json = serde_json::json!({
            "id": "msg_tool",
            "model": "claude-sonnet-4-20250514",
            "content": [
                {"type": "tool_use", "id": "toolu_abc", "name": "Read", "input": {"path": "/x"}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 10, "output_tokens": 5}
        });
        let transport = FakeTransport::always(ProviderResponse::json(200, response_json));
        let adapter = make_adapter(transport);
        let resp = adapter
            .messages_create(crate::MessagesCreateRequest::new(
                "claude-sonnet-4-20250514",
                None,
                None,
                Vec::new(),
                Vec::new(),
            ))
            .await
            .expect("ok");
        match resp.content.as_slice() {
            [crate::ContentBlock::ToolCall { id, name, .. }] => {
                assert_eq!(id, "toolu_abc", "tool_use id must round-trip verbatim");
                assert_eq!(name, "Read");
            }
            other => panic!("expected single ToolCall, got: {other:?}"),
        }
    }

    // ---- MULTIMODAL.6: per-request media cap (stripExcessMediaItems) ----

    fn img(n: usize) -> lingxi_core::types::ContentBlock {
        lingxi_core::types::ContentBlock::Image {
            source: lingxi_core::types::ImageSource::Base64 {
                media_type: "image/png".to_string(),
                data: format!("img{n}"),
            },
        }
    }

    fn user_with_imgs(range: std::ops::Range<usize>) -> ConversationMessage {
        let mut content = vec![ContentBlock::Text {
            text: "hi".to_string(),
            citations: None,
        }];
        content.extend(range.map(img));
        ConversationMessage::User {
            api_message_override: None,
            id: lingxi_core::types::MessageId::new(),
            content,
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
            turn_companion: None,
            usage_limit_note: None,
            origin: None,
            turn_origin: None,
        }
    }

    fn image_data_in_order(msgs: &[ConversationMessage]) -> Vec<String> {
        let mut out = Vec::new();
        for m in msgs {
            if let ConversationMessage::User { content, .. }
            | ConversationMessage::Assistant { content, .. } = m
            {
                for b in content {
                    if let ContentBlock::Image {
                        source: lingxi_core::types::ImageSource::Base64 { data, .. },
                    } = b
                    {
                        out.push(data.clone());
                    }
                }
            }
        }
        out
    }

    #[test]
    fn count_media_counts_top_level_images_across_messages() {
        let msgs = vec![user_with_imgs(0..3), user_with_imgs(3..5)];
        assert_eq!(count_media(&msgs), 5);
    }

    #[test]
    fn tool_result_string_content_contributes_no_media() {
        let msgs = vec![ConversationMessage::User {
            api_message_override: None,
            id: lingxi_core::types::MessageId::new(),
            content: vec![
                ContentBlock::ToolResult {
                    tool_use_id: lingxi_core::types::ToolUseId::new(),
                    content: "lots of text, no media".to_string().into(),
                    is_error: Some(false),
                    provider_tool_use_id: None,
                    content_blocks: None,
                },
                img(0),
            ],
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
            turn_companion: None,
            usage_limit_note: None,
            origin: None,
            turn_origin: None,
        }];
        assert_eq!(count_media(&msgs), 1);
    }

    #[test]
    fn count_media_includes_nested_tool_result_media() {
        // An MCP image result populates `content_blocks` with `{"type":"image"}`
        // values; these MUST count toward the media cap (claude.ts:965-969), or an
        // image-heavy MCP transcript silently exceeds the API limit and 400s.
        let msgs = vec![ConversationMessage::User {
            api_message_override: None,
            id: lingxi_core::types::MessageId::new(),
            content: vec![ContentBlock::ToolResult {
                tool_use_id: lingxi_core::types::ToolUseId::new(),
                content: "see images".to_string().into(),
                is_error: Some(false),
                provider_tool_use_id: None,
                content_blocks: Some(core_projection(&serde_json::json!([
                    serde_json::json!({"type": "text", "text": "x"}),
                    serde_json::json!({"type": "image", "source": {"data": "AAA"}}),
                    serde_json::json!({"type": "image", "source": {"data": "BBB"}}),
                ]))),
            }],
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
            turn_companion: None,
            usage_limit_note: None,
            origin: None,
            turn_origin: None,
        }];
        assert_eq!(count_media(&msgs), 2, "two nested image blocks must count");
    }

    #[test]
    fn count_media_includes_nested_openai_image_url_media() {
        let msgs = vec![ConversationMessage::User {
            api_message_override: None,
            id: lingxi_core::types::MessageId::new(),
            content: vec![ContentBlock::ToolResult {
                tool_use_id: lingxi_core::types::ToolUseId::new(),
                content: "see image".to_string().into(),
                is_error: Some(false),
                provider_tool_use_id: None,
                content_blocks: Some(core_projection(&serde_json::json!([{
                    "type": "image_url",
                    "image_url": {"url": "https://example.com/image.png"}
                }]))),
            }],
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
            turn_companion: None,
            usage_limit_note: None,
            origin: None,
            turn_origin: None,
        }];
        assert_eq!(count_media(&msgs), 1);
    }

    #[test]
    fn strip_excess_media_strips_nested_tool_result_media() {
        // Over the cap, nested tool_result media is stripped oldest-first
        // (claude.ts:982-999), leaving the text + the most-recent nested image.
        let msgs = vec![ConversationMessage::User {
            api_message_override: None,
            id: lingxi_core::types::MessageId::new(),
            content: vec![ContentBlock::ToolResult {
                tool_use_id: lingxi_core::types::ToolUseId::new(),
                content: "imgs".to_string().into(),
                is_error: Some(false),
                provider_tool_use_id: None,
                content_blocks: Some(core_projection(&serde_json::json!([
                    serde_json::json!({"type": "image", "source": {"data": "a"}}),
                    serde_json::json!({"type": "image", "source": {"data": "b"}}),
                    serde_json::json!({"type": "image", "source": {"data": "c"}}),
                    serde_json::json!({"type": "text", "text": "keep"}),
                ]))),
            }],
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
            turn_companion: None,
            usage_limit_note: None,
            origin: None,
            turn_origin: None,
        }];
        let stripped = strip_excess_media(msgs, 1);
        assert_eq!(
            count_media(&stripped),
            1,
            "nested media trimmed to the limit"
        );
        // The text block and the newest image survive.
        let ConversationMessage::User { content, .. } = &stripped[0] else {
            panic!("user message");
        };
        let ContentBlock::ToolResult {
            content_blocks: Some(blocks),
            ..
        } = &content[0]
        else {
            panic!("tool_result with content_blocks");
        };
        assert_eq!(
            blocks.array_len("").unwrap(),
            2,
            "one image + the text remain; got {blocks:?}"
        );
        assert!((0..blocks.array_len("").unwrap()).any(|index| {
            blocks
                .at_pointer(&format!("/{index}/type"))
                .and_then(lingxi_core::types::utf16_json::JsonValueView::as_str)
                == Some("text")
        }));
    }

    #[test]
    fn strip_excess_media_trims_oldest_to_limit_without_touching_history() {
        let stored = vec![user_with_imgs(0..60), user_with_imgs(60..102)];
        assert_eq!(count_media(&stored), 102);

        let to_send = stored.clone();
        let trimmed = strip_excess_media(to_send, MAX_MEDIA_PER_REQUEST);

        assert_eq!(count_media(&trimmed), 100);
        let remaining = image_data_in_order(&trimmed);
        assert_eq!(remaining.len(), 100);
        assert_eq!(remaining.first().unwrap(), "img2");
        assert_eq!(remaining.last().unwrap(), "img101");
        assert!(!remaining.contains(&"img0".to_string()));
        assert!(!remaining.contains(&"img1".to_string()));

        assert_eq!(count_media(&stored), 102);
        assert_eq!(image_data_in_order(&stored).first().unwrap(), "img0");
    }

    #[test]
    fn strip_excess_media_leaves_within_limit_messages_unchanged() {
        let msgs = vec![user_with_imgs(0..50), user_with_imgs(50..100)];
        let before = msgs.clone();
        let out = strip_excess_media(msgs, MAX_MEDIA_PER_REQUEST);
        assert_eq!(out, before, "exactly 100 media → no stripping");
        assert_eq!(count_media(&out), 100);
    }

    #[test]
    fn strip_excess_media_no_images_is_noop() {
        let msgs = vec![ConversationMessage::User {
            api_message_override: None,
            id: lingxi_core::types::MessageId::new(),
            content: vec![ContentBlock::Text {
                text: "no media here".to_string(),
                citations: None,
            }],
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
            turn_companion: None,
            usage_limit_note: None,
            origin: None,
            turn_origin: None,
        }];
        let before = msgs.clone();
        let out = strip_excess_media(msgs, MAX_MEDIA_PER_REQUEST);
        assert_eq!(out, before);
    }

    // ── Fix 2: error_kind label-parity test ──────────────────────────────────

    /// Labels must be locked to api-client originals (Fix 2).
    ///
    /// Ensures that re-naming a label here triggers a test failure so the
    /// telemetry schema change is explicit.
    #[test]
    fn error_kind_labels_match_api_client_originals() {
        // api-client: Unauthorized → "unauthorized"
        assert_eq!(
            ApiService::error_kind(&LlmError::Authentication {
                message: String::new()
            }),
            "unauthorized"
        );
        assert_eq!(
            ApiService::error_kind(&LlmError::PermissionDenied {
                message: String::new()
            }),
            "unauthorized"
        );
        // api-client: Server → "server"
        assert_eq!(
            ApiService::error_kind(&LlmError::ProviderInternal),
            "server"
        );
        // api-client: Http → "http"
        assert_eq!(
            ApiService::error_kind(&LlmError::Transport {
                message: "t".into()
            }),
            "http"
        );
        // api-client: MalformedStream → "malformed_stream"
        assert_eq!(
            ApiService::error_kind(&LlmError::StreamInterrupted {
                message: "s".into()
            }),
            "malformed_stream"
        );
        // api-client: Overloaded → "overloaded"
        assert_eq!(
            ApiService::error_kind(&LlmError::Overloaded { repeated: false }),
            "overloaded"
        );
        // api-client: RateLimited → "rate_limited"
        assert_eq!(
            ApiService::error_kind(&LlmError::RateLimited {
                retry_after: None,
                scope: None
            }),
            "rate_limited"
        );
        // api-client: PromptTooLong → "prompt_too_long"
        assert_eq!(
            ApiService::error_kind(&LlmError::ContextOverflow { token_gap: 0 }),
            "prompt_too_long"
        );
        // 2.1.212 413 split: RequestTooLarge → "request_too_large" (distinct
        // from the context-window "prompt_too_long").
        assert_eq!(
            ApiService::error_kind(&LlmError::RequestTooLarge),
            "request_too_large"
        );
    }

    // ── Task 8: subscriber 429 gate end-to-end through the adapter ───────────

    /// Task 8 gate: subscriber non-enterprise 429 → terminal immediately (no retry).
    ///
    /// Wire: `SubscriberState { is_subscriber: true, is_enterprise: false }` +
    /// a transport that always returns 429 → the adapter returns an error without
    /// making a second request.
    #[tokio::test]
    async fn subscriber_429_is_terminal_in_adapter() {
        let transport = FakeTransport::always(ProviderResponse {
            status: 429,
            headers: BTreeMap::new(),
            body_json: provider_payload(serde_json::json!({
                "type": "error",
                "error": {"type": "rate_limit_error", "message": "You have reached your usage limit"}
            })),
            request_id: None,
        });
        let adapter = make_adapter_with_subscriber(
            transport.clone(),
            SubscriberState {
                is_subscriber: true,
                is_enterprise: false,
            },
        );
        let result = adapter
            .messages_create(crate::MessagesCreateRequest::new(
                "claude-sonnet-4-20250514",
                None,
                None,
                Vec::new(),
                Vec::new(),
            ))
            .await;
        assert!(result.is_err(), "subscriber 429 must be terminal");
        // Only ONE execution — no retries.
        assert_eq!(
            transport.seen_count(),
            1,
            "subscriber 429 must not retry (seen_count should be 1)"
        );
    }

    /// Directly guards [`ApiService::clear_pending_429`]: a staged 429
    /// snapshot must be discarded so a subsequent promote writes NOTHING. This
    /// goes RED iff `clear_pending_429`'s body is emptied (a no-op clear leaves
    /// the slot `Some`, so promote would copy A's `rejected` snapshot into
    /// `last_rate_limit`). The active cross-drive isolation is the per-attempt
    /// record stage-or-clear; this test is the credibility guard for the
    /// defensive drive-entry / success backstop.
    #[test]
    fn clear_pending_429_discards_staged_snapshot() {
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let adapter = make_adapter(transport);

        // Stage: a 429 carrying a representative-claim → pending = Some(info).
        let headers = {
            let mut h = std::collections::BTreeMap::new();
            h.insert(
                "anthropic-ratelimit-unified-representative-claim".to_string(),
                "seven_day".to_string(),
            );
            h.insert(
                "anthropic-ratelimit-unified-status".to_string(),
                "rejected".to_string(),
            );
            h
        };
        adapter.record_rate_limit_from_429(&headers, None, "claude-sonnet-4-20250514");
        // Sanity: the slot is genuinely staged before we clear it.
        assert!(
            adapter.pending_429.lock().unwrap().is_some(),
            "precondition: record_rate_limit_from_429 must stage a snapshot"
        );

        // Clear, then a terminal promote: with the slot emptied, promote is a
        // no-op and `last_rate_limit` stays None.
        adapter.clear_pending_429();
        adapter.promote_pending_429(None);

        assert_eq!(
            adapter.last_rate_limit_info(),
            None,
            "clear_pending_429 must discard the staged snapshot so promote writes nothing"
        );
    }

    #[test]
    fn prompt_cache_overage_is_scoped_epoch_guarded_and_wait_published() {
        let adapter = make_adapter(FakeTransport::always(ProviderResponse::json(
            200,
            ok_response_json(),
        )));
        let scope =
            crate::CredentialScope::new(crate::ProviderId::AnthropicFirstParty, "account-profile")
                .with_credential_id("account-credential");
        let prepared = crate::client::PreparedPromptCacheContext {
            scope: scope.clone(),
            account_epoch: 0,
            account_epoch_stale: false,
            is_subscriber: true,
            pending_overage: Arc::new(Mutex::new(None)),
        };
        let header_set = |status: &str, overage_status: &str| {
            BTreeMap::from([
                (
                    "anthropic-ratelimit-unified-status".to_owned(),
                    status.to_owned(),
                ),
                (
                    "anthropic-ratelimit-unified-overage-status".to_owned(),
                    overage_status.to_owned(),
                ),
            ])
        };

        let using_overage = header_set("rejected", "allowed");
        adapter.record_prompt_cache_overage_from_headers(&using_overage, Some(&prepared));
        assert!(adapter.prompt_cache_overage.is_using_overage(&scope, 0));

        let stale_clear = header_set("allowed", "rejected");
        assert!(!adapter.prompt_cache_overage.record(&scope, 0, false, 0));
        assert!(
            adapter.prompt_cache_overage.is_using_overage(&scope, 0),
            "older observations cannot clear the current overage state"
        );

        adapter.record_rate_limit_from_429(&stale_clear, None, "claude-sonnet-4-20250514");
        adapter.stage_prompt_cache_overage_from_429(&stale_clear, Some(&prepared));
        assert!(prepared.pending_overage.lock().unwrap().is_some());
        assert!(
            adapter.prompt_cache_overage.is_using_overage(&scope, 0),
            "retryable 429 does not change the current state before terminal promotion"
        );
        adapter.clear_pending_429();
        adapter.promote_pending_429(Some(&prepared));
        assert!(!adapter.prompt_cache_overage.is_using_overage(&scope, 0));

        let other_scope =
            crate::CredentialScope::new(crate::ProviderId::AnthropicFirstParty, "another-profile");
        assert!(!adapter
            .prompt_cache_overage
            .is_using_overage(&other_scope, 0));

        let reset = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 30;
        let retry_headers = BTreeMap::from([
            (
                "anthropic-ratelimit-unified-status".to_owned(),
                "rejected".to_owned(),
            ),
            (
                "anthropic-ratelimit-unified-overage-status".to_owned(),
                "allowed_warning".to_owned(),
            ),
            (
                "anthropic-ratelimit-unified-reset".to_owned(),
                reset.to_string(),
            ),
        ]);
        adapter.record_prompt_cache_overage_from_quota_wait(
            429,
            &retry_headers,
            false,
            Some(&prepared),
        );
        assert!(!adapter.prompt_cache_overage.is_using_overage(&scope, 0));
        adapter.record_prompt_cache_overage_from_quota_wait(
            429,
            &retry_headers,
            true,
            Some(&prepared),
        );
        assert!(adapter.prompt_cache_overage.is_using_overage(&scope, 0));

        adapter.account_change_observer().account_changed();
        assert_eq!(adapter.prompt_cache_overage.account_epoch(), 1);
        // A response that arrives after the credential owner advanced the
        // account epoch is a late observation for the old request. It must not
        // seed the new account's scope cache.
        adapter.record_prompt_cache_overage_from_headers(&using_overage, Some(&prepared));
        assert!(!adapter.prompt_cache_overage.record(
            &scope,
            prepared.account_epoch,
            true,
            u128::MAX
        ));
        assert!(!adapter.prompt_cache_overage.is_using_overage(&scope, 1));
    }

    /// Task 6 (batch 5): a terminal 429 whose response carries the unified
    /// headers records the forced-`rejected` snapshot AND the composed
    /// limits copy (byte-pinned: no reset header → no ` · resets …` clause,
    /// `rateLimitMessages.ts:149` + `:333-344`).
    #[tokio::test]
    async fn terminal_429_with_unified_headers_records_limits_copy() {
        let transport = FakeTransport::always(ProviderResponse {
            status: 429,
            headers: {
                let mut h = BTreeMap::new();
                h.insert(
                    "anthropic-ratelimit-unified-representative-claim".to_string(),
                    "seven_day".to_string(),
                );
                h.insert(
                    "anthropic-ratelimit-unified-status".to_string(),
                    "rejected".to_string(),
                );
                h
            },
            body_json: provider_payload(serde_json::json!({
                "type": "error",
                "error": {"type": "rate_limit_error", "message": "You have reached your usage limit"}
            })),
            request_id: None,
        });
        // Subscriber (non-enterprise) → the 429 is terminal on the first try.
        let adapter = make_adapter_with_subscriber(
            transport.clone(),
            SubscriberState {
                is_subscriber: true,
                is_enterprise: false,
            },
        );
        let result = adapter
            .messages_create(crate::MessagesCreateRequest::new(
                "claude-sonnet-4-20250514",
                None,
                None,
                Vec::new(),
                Vec::new(),
            ))
            .await;
        assert!(
            matches!(
                classified_error(&result),
                Some(LlmError::RateLimited { .. })
            ),
            "got {result:?}"
        );

        // The composed copy is cached for the orchestrator's terminal re-map.
        assert_eq!(
            adapter.last_rate_limit_error_message().as_deref(),
            Some("You've hit your weekly limit"),
            "seven_day → formatLimitReachedText('weekly limit', '') verbatim"
        );
        // errors.ts:482-516 — the limits snapshot is updated from the error's
        // headers with status FORCED 'rejected'.
        let info = adapter.last_rate_limit_info().expect("snapshot");
        assert_eq!(info.status.as_deref(), Some("rejected"));
        assert_eq!(info.rate_limit_type.as_deref(), Some("seven_day"));
    }

    // ── M12: rate-limit warning flicker (2.1.196 monotonic guard) ────────────

    fn unified_headers(status: &str) -> BTreeMap<String, String> {
        let mut h = BTreeMap::new();
        h.insert(
            "anthropic-ratelimit-unified-representative-claim".to_string(),
            "seven_day".to_string(),
        );
        h.insert(
            "anthropic-ratelimit-unified-status".to_string(),
            status.to_string(),
        );
        h
    }

    fn near_limit_headers(
        utilization: &str,
        reset_secs: u64,
        overage_status: Option<&str>,
    ) -> BTreeMap<String, String> {
        let mut h = BTreeMap::new();
        h.insert(
            "anthropic-ratelimit-unified-representative-claim".to_string(),
            "five_hour".to_string(),
        );
        h.insert(
            "anthropic-ratelimit-unified-status".to_string(),
            "allowed".to_string(),
        );
        h.insert(
            "anthropic-ratelimit-unified-5h-utilization".to_string(),
            utilization.to_string(),
        );
        h.insert(
            "anthropic-ratelimit-unified-5h-reset".to_string(),
            reset_secs.to_string(),
        );
        if let Some(overage_status) = overage_status {
            h.insert(
                "anthropic-ratelimit-unified-overage-status".to_string(),
                overage_status.to_string(),
            );
        }
        h
    }

    fn reset_in(seconds: u64) -> u64 {
        u64::try_from(ApiService::now_ms() / 1000)
            .unwrap_or(u64::MAX)
            .saturating_add(seconds)
    }

    /// The `Bha`/`Nha` monotonic guard: an out-of-order (older-timestamp)
    /// response must NOT overwrite a newer at-limit snapshot, so the warning
    /// cannot flicker off while still at the limit. A genuinely NEWER response
    /// still updates.
    #[test]
    fn stale_parallel_response_does_not_flip_rate_limit_warning_off() {
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let adapter = make_adapter(transport);

        // t=1000: the account is at its weekly limit (rejected).
        adapter.record_rate_limit_from_headers_at(&unified_headers("rejected"), "", 1000);
        assert_eq!(
            adapter.last_rate_limit_info().and_then(|i| i.status),
            Some("rejected".to_string()),
        );

        // t=500: a STALE parallel response reporting 'allowed' arrives late —
        // it must be DROPPED (no flicker): the warning stays 'rejected'.
        adapter.record_rate_limit_from_headers_at(&unified_headers("allowed"), "", 500);
        assert_eq!(
            adapter.last_rate_limit_info().and_then(|i| i.status),
            Some("rejected".to_string()),
            "a stale (older-timestamp) response must not flip the warning off"
        );

        // t=2000: a genuinely NEWER 'allowed' response clears the limit.
        adapter.record_rate_limit_from_headers_at(&unified_headers("allowed"), "", 2000);
        assert_eq!(
            adapter.last_rate_limit_info().and_then(|i| i.status),
            Some("allowed".to_string()),
            "a fresher response must update the snapshot"
        );
    }

    /// The guard is inert under normal monotonic operation: equal-or-increasing
    /// timestamps always record (the production path uses wall-clock `now_ms`).
    #[test]
    fn equal_or_increasing_timestamps_always_record() {
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let adapter = make_adapter(transport);
        adapter.record_rate_limit_from_headers_at(&unified_headers("allowed"), "", 5000);
        adapter.record_rate_limit_from_headers_at(&unified_headers("rejected"), "", 5000);
        assert_eq!(
            adapter.last_rate_limit_info().and_then(|i| i.status),
            Some("rejected".to_string()),
            "an equal-timestamp record is not stale and updates"
        );
    }

    #[test]
    fn near_limit_wrap_up_hint_consumes_once_per_window() {
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let adapter = make_adapter(transport);
        let reset = reset_in(24 * 60 * 60);

        adapter.record_rate_limit_from_headers_at(
            &near_limit_headers("0.95", reset, None),
            "",
            1_000_000,
        );

        assert!(adapter.consume_pending_near_limit_wrap_up_hint());
        assert!(!adapter.consume_pending_near_limit_wrap_up_hint());
    }

    #[test]
    fn near_limit_wrap_up_hint_rearms_only_after_window_changes() {
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let adapter = make_adapter(transport);
        let first_reset = reset_in(60 * 60);
        let second_reset = reset_in(2 * 60 * 60);

        adapter.record_rate_limit_from_headers_at(
            &near_limit_headers("0.96", first_reset, None),
            "",
            1_000_000,
        );
        assert!(adapter.consume_pending_near_limit_wrap_up_hint());

        adapter.record_rate_limit_from_headers_at(
            &near_limit_headers("0.99", first_reset, None),
            "",
            1_010_000,
        );
        assert!(
            !adapter.consume_pending_near_limit_wrap_up_hint(),
            "same five-hour reset must not re-arm after consumption"
        );

        adapter.record_rate_limit_from_headers_at(
            &near_limit_headers("0.96", second_reset, None),
            "",
            1_020_000,
        );
        assert!(
            adapter.consume_pending_near_limit_wrap_up_hint(),
            "a later five-hour reset opens a new hint window"
        );
    }

    #[test]
    fn stale_parallel_response_does_not_clear_pending_near_limit_wrap_up() {
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let adapter = make_adapter(transport);
        let reset = reset_in(60 * 60);

        adapter.record_rate_limit_from_headers_at(
            &near_limit_headers("0.96", reset, None),
            "",
            2_000,
        );
        adapter.record_rate_limit_from_headers_at(
            &near_limit_headers("0.10", reset, Some("allowed_warning")),
            "",
            1_000,
        );

        assert!(
            adapter.consume_pending_near_limit_wrap_up_hint(),
            "stale snapshots must not clear the armed near-limit hint"
        );
    }

    #[test]
    fn near_limit_wrap_up_overage_warning_clears_pending_hint() {
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let adapter = make_adapter(transport);
        let reset = reset_in(60 * 60);

        adapter.record_rate_limit_from_headers_at(
            &near_limit_headers("0.96", reset, None),
            "",
            1_000_000,
        );
        adapter.record_rate_limit_from_headers_at(
            &near_limit_headers("0.96", reset, Some("allowed_warning")),
            "",
            1_001_000,
        );

        assert!(
            !adapter.consume_pending_near_limit_wrap_up_hint(),
            "allowed_warning must clear the pending near-limit hint"
        );
        adapter.record_rate_limit_from_headers_at(
            &near_limit_headers("0.99", reset, None),
            "",
            1_002_000,
        );
        assert!(
            !adapter.consume_pending_near_limit_wrap_up_hint(),
            "overage clearing preserves the reset key, so the same window cannot re-arm"
        );
    }

    #[test]
    fn near_limit_wrap_up_threshold_uses_max20x_tier() {
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let adapter = make_adapter_with_subscriber(transport, SubscriberState::default())
            .with_subscription(shared_slot(Some(
                lingxi_core::host::subscription::SubscriptionSnapshot {
                    is_subscriber: true,
                    subscription_type: Some("max".to_string()),
                    rate_limit_tier: Some("default_claude_max_20x".to_string()),
                    ..Default::default()
                },
            )));
        let reset = reset_in(60 * 60);

        adapter.record_rate_limit_from_headers_at(
            &near_limit_headers("0.99", reset, None),
            "",
            1_000_000,
        );
        assert!(
            !adapter.consume_pending_near_limit_wrap_up_hint(),
            "max20x must not arm at the looser subscriber threshold"
        );

        adapter.record_rate_limit_from_headers_at(
            &near_limit_headers("0.9975", reset, None),
            "",
            1_001_000,
        );
        assert!(
            adapter.consume_pending_near_limit_wrap_up_hint(),
            "max20x must arm at the stricter 99.75% threshold"
        );
    }

    #[test]
    fn near_limit_wrap_up_uses_future_reset_not_a_five_hour_countdown() {
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let adapter = make_adapter(transport);
        let now = u64::try_from(ApiService::now_ms() / 1000).unwrap_or(u64::MAX);

        adapter.record_rate_limit_from_headers_at(
            &near_limit_headers("1", now.saturating_sub(1), None),
            "",
            1_000,
        );
        assert!(!adapter.consume_pending_near_limit_wrap_up_hint());

        adapter.record_rate_limit_from_headers_at(
            &near_limit_headers("0.95", now.saturating_add(24 * 60 * 60), None),
            "",
            2_000,
        );
        assert!(
            adapter.consume_pending_near_limit_wrap_up_hint(),
            "the five-hour bucket may reset more than five hours from now"
        );
    }

    #[test]
    fn near_limit_wrap_up_threshold_uses_exact_rate_limit_tier() {
        let reset = reset_in(60 * 60);

        let ordinary_subscriber = make_adapter_with_subscriber(
            FakeTransport::always(ProviderResponse::json(200, ok_response_json())),
            SubscriberState {
                is_subscriber: true,
                is_enterprise: false,
            },
        );
        ordinary_subscriber.record_rate_limit_from_headers_at(
            &near_limit_headers("0.95", reset, None),
            "",
            1_000,
        );
        assert!(
            ordinary_subscriber.consume_pending_near_limit_wrap_up_hint(),
            "subscriber status alone does not select the max-5x threshold"
        );

        let max5 = make_adapter_with_subscriber(
            FakeTransport::always(ProviderResponse::json(200, ok_response_json())),
            SubscriberState::default(),
        )
        .with_subscription(shared_slot(Some(
            lingxi_core::host::subscription::SubscriptionSnapshot {
                rate_limit_tier: Some("default_claude_max_5x".to_string()),
                ..Default::default()
            },
        )));
        max5.record_rate_limit_from_headers_at(
            &near_limit_headers("0.989", reset, None),
            "",
            1_000,
        );
        assert!(!max5.consume_pending_near_limit_wrap_up_hint());
        max5.record_rate_limit_from_headers_at(&near_limit_headers("0.99", reset, None), "", 2_000);
        assert!(max5.consume_pending_near_limit_wrap_up_hint());
    }

    // ── B6-T1: terminal-only 429 state promotion (pending slot) ──────────────
    //
    // claude-code updates the limits/raw module state ONLY in the terminal
    // catch handler `extractQuotaStatusFromError` (claudeAiLimits.ts:487),
    // never on a retried attempt that later recovers. The Rust seam stages
    // each 429 attempt's snapshot in a `pending_429` slot and promotes it into
    // the live caches only when the retry loop declares the error TERMINAL —
    // and discards it on drive-entry and on any subsequent success.

    /// A 429 carrying BOTH the unified limits headers (gate passes) AND the
    /// per-window quartet (raw non-empty) that exhausts the retry budget
    /// promotes BOTH: the forced-`rejected` limits snapshot
    /// (`last_rate_limit_full`) and the raw per-window utilization
    /// (`last_raw_utilization`) — `extractRawUtilization` runs on the SAME
    /// error headers (claudeAiLimits.ts:500).
    #[tokio::test]
    async fn terminal_429_promotes_snapshot_and_raw() {
        let headers = {
            let mut h = BTreeMap::new();
            // Limits gate (from_429_error_headers → Some).
            h.insert(
                "anthropic-ratelimit-unified-representative-claim".to_string(),
                "seven_day".to_string(),
            );
            // Per-window quartet → raw non-empty.
            h.insert(
                "anthropic-ratelimit-unified-5h-utilization".to_string(),
                "0.42".to_string(),
            );
            h.insert(
                "anthropic-ratelimit-unified-5h-reset".to_string(),
                "1750000005".to_string(),
            );
            h.insert(
                "anthropic-ratelimit-unified-7d-utilization".to_string(),
                "0.77".to_string(),
            );
            h.insert(
                "anthropic-ratelimit-unified-7d-reset".to_string(),
                "1750000007".to_string(),
            );
            h
        };
        let transport = FakeTransport::always(ProviderResponse {
            status: 429,
            headers,
            body_json: provider_payload(serde_json::json!({
                "type": "error",
                "error": {"type": "rate_limit_error", "message": "rate limited"}
            })),
            request_id: None,
        });
        // Subscriber (non-enterprise) → the 429 is terminal on the first try.
        let adapter = make_adapter_with_subscriber(
            transport.clone(),
            SubscriberState {
                is_subscriber: true,
                is_enterprise: false,
            },
        );
        let result = adapter
            .messages_create(crate::MessagesCreateRequest::new(
                "claude-sonnet-4-20250514",
                None,
                None,
                Vec::new(),
                Vec::new(),
            ))
            .await;
        assert!(
            matches!(
                classified_error(&result),
                Some(LlmError::RateLimited { .. })
            ),
            "got {result:?}"
        );

        // Promoted at the terminal: rejected limits snapshot.
        let info = adapter.last_rate_limit_info().expect("snapshot");
        assert_eq!(info.status.as_deref(), Some("rejected"));
        assert_eq!(info.rate_limit_type.as_deref(), Some("seven_day"));

        // Promoted at the terminal: raw per-window utilization from the SAME
        // error headers (extractRawUtilization, ts:500).
        let raw = adapter.last_raw_utilization().expect("raw");
        assert_eq!(raw.five_hour.map(|w| w.utilization), Some(0.42));
        assert_eq!(raw.five_hour.map(|w| w.resets_at), Some(1_750_000_005));
        assert_eq!(raw.seven_day.map(|w| w.utilization), Some(0.77));
        assert_eq!(raw.seven_day.map(|w| w.resets_at), Some(1_750_000_007));
    }

    /// A terminal 429 with NO per-window quartet must still overwrite the raw
    /// snapshot with the EMPTY parse so stale statusline state can be cleared.
    #[test]
    fn terminal_429_without_raw_headers_clears_raw_snapshot() {
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let adapter = make_adapter(transport);

        adapter.record_rate_limit_from_headers(
            &BTreeMap::from([
                (
                    "anthropic-ratelimit-unified-5h-utilization".to_string(),
                    "0.42".to_string(),
                ),
                (
                    "anthropic-ratelimit-unified-5h-reset".to_string(),
                    "1750000005".to_string(),
                ),
            ]),
            "",
        );
        assert_ne!(
            adapter.last_raw_utilization(),
            Some(RawUtilization::default()),
            "precondition: seed a non-empty raw snapshot first"
        );

        adapter.record_rate_limit_from_429(&BTreeMap::new(), None, "claude-sonnet-4-20250514");
        adapter.promote_pending_429(None);

        assert_eq!(
            adapter.last_raw_utilization(),
            Some(RawUtilization::default()),
            "terminal 429 must preserve the EMPTY raw snapshot to clear stale state"
        );
    }

    /// A 429-with-headers that is RETRIED and then RECOVERS on a 200 must NOT
    /// leave the rejected snapshot behind. This verifies the SUCCESS PATH:
    /// after recovery, `last_rate_limit` reflects the 200 (here headerless →
    /// `None`), NOT the retried 429 — because the limits cache is only written
    /// at the terminal promote, which never fires on a recovered turn. (The
    /// success-clear of the pending SLOT itself is guarded directly by
    /// `clear_pending_429_discards_staged_snapshot`, not here.)
    #[tokio::test]
    async fn retried_429_does_not_update_limits_snapshot() {
        let headers_429 = {
            let mut h = BTreeMap::new();
            h.insert(
                "anthropic-ratelimit-unified-representative-claim".to_string(),
                "seven_day".to_string(),
            );
            h.insert(
                "anthropic-ratelimit-unified-status".to_string(),
                "rejected".to_string(),
            );
            // retry-after 0 → no real sleep.
            h.insert("retry-after".to_string(), "0".to_string());
            h
        };
        let transport = FakeTransport::sequence(vec![
            FakeResponse::Ok(ProviderResponse {
                status: 429,
                headers: headers_429,
                body_json: provider_payload(serde_json::json!({
                    "type": "error",
                    "error": {"type": "rate_limit_error", "message": "rate limited"}
                })),
                request_id: None,
            }),
            // Recovery: a 200 with NO unified headers.
            FakeResponse::Ok(ProviderResponse::json(200, ok_response_json())),
        ]);
        // Enterprise subscriber → the 429 is retried, then the 200 recovers.
        let adapter = make_adapter_with_subscriber(
            transport.clone(),
            SubscriberState {
                is_subscriber: true,
                is_enterprise: true,
            },
        );
        let result = adapter
            .messages_create(crate::MessagesCreateRequest::new(
                "claude-sonnet-4-20250514",
                None,
                None,
                Vec::new(),
                Vec::new(),
            ))
            .await;
        assert!(result.is_ok(), "429 then 200 must recover: {result:?}");

        // The success seam cleared the limits cache (the 200 carried no unified
        // headers); the retried 429's rejected snapshot was NEVER promoted.
        assert_eq!(
            adapter.last_rate_limit_info(),
            None,
            "retried-then-recovered 429 must not plant a rejected snapshot"
        );
        // The 429 message copy was likewise cleared on success.
        assert_eq!(adapter.last_rate_limit_error_message(), None,);
    }

    /// Cross-drive isolation, asserted on the OBSERVABLE promoted snapshot: a
    /// pending 429 staged by an EARLIER drive must never survive into a LATER
    /// drive's terminal promote. Drive A = `[429-with-headers(seven_day),
    /// 400-invalid-request terminal]` — A stages a `seven_day` snapshot but the
    /// 400 is non-RateLimited, so it never promotes and the slot is orphaned.
    /// Drive B ends on a TERMINAL 429 carrying its OWN fresh headers
    /// (`five_hour`, DIFFERENT from A's) → B promotes B's snapshot. The promoted
    /// `last_rate_limit_full()` must read `five_hour` (DRIVE B), proving A's
    /// orphaned `seven_day` slot did NOT leak in.
    ///
    /// The ACTIVE isolation mechanism this asserts is the per-attempt record
    /// stage-or-clear in `record_rate_limit_from_429` (drive B's first attempt
    /// overwrites the slot with B's snapshot before the terminal promote runs).
    /// The drive-entry reset is a defensive backstop, not what this test
    /// exercises — `clear_pending_429_discards_staged_snapshot` guards that
    /// directly. This test is non-vacuous: it fails if promotion ever reads a
    /// stale slot (B's snapshot would be wrong, or `seven_day` would surface).
    #[tokio::test]
    async fn stale_pending_429_not_promoted_across_drives() {
        // Drive A's 429: representative-claim = seven_day. Staged then orphaned.
        let resp_429_a = ProviderResponse {
            status: 429,
            headers: {
                let mut h = BTreeMap::new();
                h.insert(
                    "anthropic-ratelimit-unified-representative-claim".to_string(),
                    "seven_day".to_string(),
                );
                h.insert(
                    "anthropic-ratelimit-unified-status".to_string(),
                    "rejected".to_string(),
                );
                h.insert("retry-after".to_string(), "0".to_string());
                h
            },
            body_json: provider_payload(serde_json::json!({
                "type": "error",
                "error": {"type": "rate_limit_error", "message": "rate limited"}
            })),
            request_id: None,
        };
        // A plain 400 invalid_request → terminal, NON-rate-limited (no promote).
        let resp_400 = ProviderResponse {
            status: 400,
            headers: BTreeMap::new(),
            body_json: provider_payload(serde_json::json!({
                "type": "error",
                "error": {"type": "invalid_request_error", "message": "bad request"}
            })),
            request_id: None,
        };
        // Drive B's 429: representative-claim = five_hour (DIFFERENT from A).
        // Terminal here → B promotes B's OWN fresh snapshot.
        let resp_429_b = ProviderResponse {
            status: 429,
            headers: {
                let mut h = BTreeMap::new();
                h.insert(
                    "anthropic-ratelimit-unified-representative-claim".to_string(),
                    "five_hour".to_string(),
                );
                h.insert(
                    "anthropic-ratelimit-unified-status".to_string(),
                    "rejected".to_string(),
                );
                h.insert("retry-after".to_string(), "0".to_string());
                h
            },
            body_json: provider_payload(serde_json::json!({
                "type": "error",
                "error": {"type": "rate_limit_error", "message": "rate limited"}
            })),
            request_id: None,
        };
        // Drive A consumes idx 0,1; Drive B consumes idx 2,3 (global cursor).
        let transport = FakeTransport::sequence(vec![
            FakeResponse::Ok(resp_429_a),
            FakeResponse::Ok(resp_400),
            FakeResponse::Ok(resp_429_b.clone()),
            FakeResponse::Ok(resp_429_b),
        ]);
        // Non-subscriber (429 is retryable), settings_max_retries=1 so each
        // drive retries exactly once then terminates; backoff 0 → no sleeps.
        let adapter = make_adapter_with_routing(
            transport.clone(),
            std::collections::BTreeMap::new(),
            Some(1),
            Some(0),
        );

        // Drive A: 429(seven_day) (retried, pending set) → 400 (terminal,
        // non-RateLimited → no promotion). Pending lingers with A's snapshot.
        let a = adapter
            .messages_create(crate::MessagesCreateRequest::new(
                "claude-haiku-4-20250307",
                None,
                None,
                Vec::new(),
                Vec::new(),
            ))
            .await;
        assert!(
            matches!(classified_error(&a), Some(LlmError::InvalidRequest { .. })),
            "drive A must die on the 400, got {a:?}"
        );

        // Drive B: 429(five_hour) (retried, pending OVERWRITTEN with B's
        // snapshot) → 429(five_hour) terminal → promotes B's snapshot.
        let b = adapter
            .messages_create(crate::MessagesCreateRequest::new(
                "claude-haiku-4-20250307",
                None,
                None,
                Vec::new(),
                Vec::new(),
            ))
            .await;
        assert!(
            matches!(classified_error(&b), Some(LlmError::RateLimited { .. })),
            "drive B must die on its terminal 429, got {b:?}"
        );

        // The promoted snapshot must be DRIVE B's (five_hour), proving drive A's
        // orphaned seven_day slot did not survive into B's promote.
        let info = adapter
            .last_rate_limit_info()
            .expect("drive B promotes its own snapshot");
        assert_eq!(
            info.rate_limit_type.as_deref(),
            Some("five_hour"),
            "promoted snapshot must reflect DRIVE B (five_hour), not A's stale seven_day"
        );
        assert_eq!(info.status.as_deref(), Some("rejected"));
    }

    /// Task 6 (batch 5): a 429 WITHOUT unified headers fails the
    /// `if (rateLimitType || overageStatus)` gate (errors.ts:480) — no copy
    /// is composed, and a copy from an earlier 429 is superseded (the slot
    /// reflects the most recent 429), so the generic surface applies.
    #[tokio::test]
    async fn terminal_429_without_unified_headers_records_no_copy() {
        let transport = FakeTransport::always(ProviderResponse {
            status: 429,
            headers: BTreeMap::new(),
            body_json: provider_payload(serde_json::json!({
                "type": "error",
                "error": {"type": "rate_limit_error", "message": "rate limited"}
            })),
            request_id: None,
        });
        let adapter = make_adapter_with_subscriber(
            transport.clone(),
            SubscriberState {
                is_subscriber: true,
                is_enterprise: false,
            },
        );
        let result = adapter
            .messages_create(crate::MessagesCreateRequest::new(
                "claude-sonnet-4-20250514",
                None,
                None,
                Vec::new(),
                Vec::new(),
            ))
            .await;
        assert!(result.is_err());
        assert_eq!(
            adapter.last_rate_limit_error_message(),
            None,
            "no unified headers on the 429 → no limits copy"
        );
    }

    /// Task 6 (batch 5): the live subscription slot's `pro` plan flips the
    /// `seven_day_sonnet` wording to "weekly limit"
    /// (`rateLimitMessages.ts:176-181`).
    #[tokio::test]
    async fn terminal_429_sonnet_copy_uses_pro_subscription_wording() {
        let resp_429 = |claim: &str| ProviderResponse {
            status: 429,
            headers: {
                let mut h = BTreeMap::new();
                h.insert(
                    "anthropic-ratelimit-unified-representative-claim".to_string(),
                    claim.to_string(),
                );
                h
            },
            body_json: provider_payload(serde_json::json!({
                "type": "error",
                "error": {"type": "rate_limit_error", "message": "rate limited"}
            })),
            request_id: None,
        };

        // Without a pro/enterprise snapshot → "Sonnet limit".
        let adapter = make_adapter_with_subscriber(
            FakeTransport::always(resp_429("seven_day_sonnet")),
            SubscriberState {
                is_subscriber: true,
                is_enterprise: false,
            },
        );
        let _ = adapter
            .messages_create(crate::MessagesCreateRequest::new(
                "claude-sonnet-4-20250514",
                None,
                None,
                Vec::new(),
                Vec::new(),
            ))
            .await;
        assert_eq!(
            adapter.last_rate_limit_error_message().as_deref(),
            Some("You've hit your Sonnet limit")
        );

        // With a live `pro` snapshot → "weekly limit".
        let pro = make_adapter_with_subscriber(
            FakeTransport::always(resp_429("seven_day_sonnet")),
            SubscriberState {
                is_subscriber: true,
                is_enterprise: false,
            },
        )
        .with_subscription(shared_slot(Some(
            lingxi_core::host::subscription::SubscriptionSnapshot {
                is_subscriber: true,
                subscription_type: Some("pro".to_string()),
                ..Default::default()
            },
        )));
        let _ = pro
            .messages_create(crate::MessagesCreateRequest::new(
                "claude-sonnet-4-20250514",
                None,
                None,
                Vec::new(),
                Vec::new(),
            ))
            .await;
        assert_eq!(
            pro.last_rate_limit_error_message().as_deref(),
            Some("You've hit your weekly limit")
        );
    }

    /// Task 8 gate: enterprise subscriber 429 → retries through the full budget.
    ///
    /// Wire: `SubscriberState { is_subscriber: true, is_enterprise: true }` +
    /// a transport that returns 429 then 200 → the adapter retries and succeeds.
    #[tokio::test]
    async fn enterprise_subscriber_429_retries_in_adapter() {
        let transport = FakeTransport::sequence(vec![
            FakeResponse::Ok(ProviderResponse {
                status: 429,
                headers: {
                    let mut h = BTreeMap::new();
                    h.insert("retry-after".to_string(), "0".to_string());
                    h
                },
                body_json: provider_payload(serde_json::json!({
                    "type": "error",
                    "error": {"type": "rate_limit_error", "message": "rate limited"}
                })),
                request_id: None,
            }),
            FakeResponse::Ok(ProviderResponse::json(200, ok_response_json())),
        ]);
        let adapter = make_adapter_with_subscriber(
            transport.clone(),
            SubscriberState {
                is_subscriber: true,
                is_enterprise: true,
            },
        );
        let result = adapter
            .messages_create(crate::MessagesCreateRequest::new(
                "claude-sonnet-4-20250514",
                None,
                None,
                Vec::new(),
                Vec::new(),
            ))
            .await;
        assert!(
            result.is_ok(),
            "enterprise subscriber 429 must retry and succeed"
        );
        assert_eq!(
            transport.seen_count(),
            2,
            "should have made 2 requests (429 then 200)"
        );
    }

    /// #5 (main-loop parity): the adapter surfaces the most recent drive's
    /// budget-consuming retry count via `last_retry_count()`, so the cost path
    /// can record the REAL retry count (not the previous hardcoded `0`). A 429
    /// followed by a 200 is exactly one retry.
    #[tokio::test]
    async fn last_retry_count_reflects_429_retry() {
        let transport = FakeTransport::sequence(vec![
            FakeResponse::Ok(ProviderResponse {
                status: 429,
                headers: {
                    let mut h = BTreeMap::new();
                    h.insert("retry-after".to_string(), "0".to_string());
                    h
                },
                body_json: provider_payload(serde_json::json!({
                    "type": "error",
                    "error": {"type": "rate_limit_error", "message": "rate limited"}
                })),
                request_id: None,
            }),
            FakeResponse::Ok(ProviderResponse::json(200, ok_response_json())),
        ]);
        let adapter = make_adapter_with_subscriber(
            transport.clone(),
            SubscriberState {
                is_subscriber: true,
                is_enterprise: true,
            },
        );
        // No drive yet ⇒ zero.
        assert_eq!(adapter.last_retry_count(), 0);
        let result = adapter
            .messages_create(crate::MessagesCreateRequest::new(
                "claude-sonnet-4-20250514",
                None,
                None,
                Vec::new(),
                Vec::new(),
            ))
            .await;
        assert!(result.is_ok(), "429→200 must succeed");
        // Exactly one budget-consuming retry was performed.
        assert_eq!(
            adapter.last_retry_count(),
            1,
            "one 429 retry ⇒ last_retry_count() == 1"
        );
    }

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

    fn assistant_with_thinking() -> lingxi_core::types::ConversationMessage {
        lingxi_core::types::ConversationMessage::Assistant {
            per_turn_effort: None,
            id: lingxi_core::types::MessageId::new(),
            content: vec![
                lingxi_core::types::ContentBlock::Thinking {
                    thinking: "secret".into(),
                    signature: Some("sig".into()),
                },
                lingxi_core::types::ContentBlock::Text {
                    text: "hello".into(),
                    citations: None,
                },
            ],
            stop_reason: Some("end_turn".into()),
            resumable: None,
        }
    }

    fn prefix_lock_error_body() -> serde_json::Value {
        serde_json::json!({
            "type": "error",
            "error": {
                "type": "invalid_request_error",
                "message": "This message is not created in this conversation"
            }
        })
    }

    fn prefix_lock_error_response() -> ProviderResponse {
        ProviderResponse {
            status: 400,
            headers: BTreeMap::from([(
                "anthropic-thinking-prefix-mismatch".into(),
                "block=messages.0.content.0;kind=signature".into(),
            )]),
            body_json: provider_payload(prefix_lock_error_body()),
            request_id: None,
        }
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

    // ── 3c-T1: streaming 429 + retry-after header drives correct delay ────────

    struct RawBodyFrames(Option<Vec<u8>>);
    impl crate::FrameStream for RawBodyFrames {
        fn next_frame(&mut self) -> BoxFuture<'_, Result<Option<crate::RawStreamFrame>, LlmError>> {
            let frame = self.0.take().map(crate::RawStreamFrame::new);
            Box::pin(async move { Ok(frame) })
        }
    }

    fn bedrock_event_stream_frame(payload: &[u8]) -> Vec<u8> {
        let headers = [
            (":message-type", "event"),
            (":event-type", "chunk"),
            (":content-type", "application/json"),
        ];
        let mut encoded_headers = Vec::new();
        for (name, value) in headers {
            encoded_headers.push(name.len() as u8);
            encoded_headers.extend_from_slice(name.as_bytes());
            encoded_headers.push(7); // AWS event-stream string header.
            encoded_headers.extend_from_slice(&(value.len() as u16).to_be_bytes());
            encoded_headers.extend_from_slice(value.as_bytes());
        }

        let total_len = 12 + encoded_headers.len() + payload.len() + 4;
        let mut frame = Vec::with_capacity(total_len);
        frame.extend_from_slice(&(total_len as u32).to_be_bytes());
        frame.extend_from_slice(&(encoded_headers.len() as u32).to_be_bytes());
        frame.extend_from_slice(&crate::crc32(&frame).to_be_bytes());
        frame.extend_from_slice(&encoded_headers);
        frame.extend_from_slice(payload);
        let message_crc = crate::crc32(&frame);
        frame.extend_from_slice(&message_crc.to_be_bytes());
        frame
    }

    fn successful_bedrock_anthropic_frames(model: &str) -> Vec<Vec<u8>> {
        use base64::Engine as _;

        let events = vec![
            serde_json::to_vec(&serde_json::json!({
                "type": "message_start",
                "message": {
                    "id": "msg_context_1m",
                    "model": model,
                    "usage": {
                        "input_tokens": 1,
                        "output_tokens": 0,
                        "cache_read_input_tokens": 0,
                        "cache_creation_input_tokens": 0
                    }
                }
            }))
            .unwrap(),
            br#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}"#.to_vec(),
            br#"{"type":"message_stop"}"#.to_vec(),
        ];
        events
            .into_iter()
            .map(|event| {
                let body = serde_json::to_vec(&serde_json::json!({
                    "bytes": base64::engine::general_purpose::STANDARD.encode(event)
                }))
                .expect("event-stream chunk envelope");
                bedrock_event_stream_frame(&body)
            })
            .collect()
    }

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

    /// A Transport that sequences execute responses AND can return scripted
    /// streaming (`open_stream`) responses.
    struct FakeStreamTransport {
        /// Sequence of `open_stream` results.
        responses: Mutex<Vec<FakeStreamResp>>,
        call_count: Mutex<usize>,
        requests: Mutex<Vec<ProviderRequest>>,
    }

    #[allow(dead_code)]
    enum FakeStreamResp {
        /// Streaming response with given status + headers + no frames.
        Status {
            status: u16,
            headers: BTreeMap<String, String>,
            body_json: Option<serde_json::Value>,
        },
        Frames {
            status: u16,
            headers: BTreeMap<String, String>,
            frames: Vec<Vec<u8>>,
        },
        /// Terminal transport error (e.g. connection failure).
        Err(LlmError),
    }

    impl FakeStreamTransport {
        fn sequence(stream_resps: Vec<FakeStreamResp>) -> Arc<Self> {
            Arc::new(Self {
                responses: Mutex::new(stream_resps),
                call_count: Mutex::new(0),
                requests: Mutex::new(Vec::new()),
            })
        }

        fn stream_call_count(&self) -> usize {
            *self.call_count.lock().unwrap()
        }

        fn request_max_tokens(&self, index: usize) -> Option<u64> {
            request_body_value(&self.requests.lock().unwrap()[index].body_json)
                .get("max_tokens")
                .and_then(serde_json::Value::as_u64)
        }

        fn seen_headers(&self, index: usize) -> BTreeMap<String, String> {
            self.requests.lock().unwrap()[index].headers.clone()
        }

        fn seen_body(&self, index: usize) -> lingxi_llm_client::protocol::JsonPayload {
            self.requests.lock().unwrap()[index].body_json.clone()
        }
    }

    impl llm_runtime::test_support::FixtureTransport for FakeStreamTransport {
        fn execute<'a>(
            &'a self,
            _request: &'a ProviderRequest,
        ) -> BoxFuture<'a, Result<ProviderResponse, LlmError>> {
            Box::pin(async move {
                Err(LlmError::Transport {
                    message: "execute not scripted in FakeStreamTransport".to_string(),
                })
            })
        }

        fn open_stream<'a>(
            &'a self,
            request: &'a ProviderRequest,
        ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>> {
            self.requests.lock().unwrap().push(request.clone());
            let mut count = self.call_count.lock().unwrap();
            let idx = (*count).min(self.responses.lock().unwrap().len().saturating_sub(1));
            *count += 1;
            drop(count);
            let resp = {
                let resps = self.responses.lock().unwrap();
                match &resps[idx] {
                    FakeStreamResp::Status {
                        status,
                        headers,
                        body_json,
                    } => {
                        let body = body_json
                            .as_ref()
                            .map(|value| serde_json::to_vec(value).expect("scripted JSON body"));
                        Ok(StreamingResponse {
                            status: *status,
                            headers: headers.clone(),
                            frames: Box::new(RawBodyFrames(body)),
                        })
                    }
                    FakeStreamResp::Frames {
                        status,
                        headers,
                        frames,
                    } => Ok(StreamingResponse {
                        status: *status,
                        headers: headers.clone(),
                        frames: Box::new(ScriptedFrames::new(frames.clone())),
                    }),
                    FakeStreamResp::Err(e) => Err(e.clone()),
                }
            };
            Box::pin(async move { resp })
        }
    }
    llm_runtime::impl_fixture_transport!(FakeStreamTransport);

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
        assert!(scope.beta_rejected(
            lingxi_llm_client::providers::anthropic::beta_repair::Beta::ThinkingBindingControls
        ));
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
        assert!(scope.beta_rejected(
            lingxi_llm_client::providers::anthropic::beta_repair::Beta::ThinkingBindingControls
        ));
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

    /// A transport whose connect/header phase never resolves.
    struct HangingOpenTransport {
        calls: Mutex<usize>,
    }

    impl HangingOpenTransport {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(0),
            })
        }

        fn call_count(&self) -> usize {
            *self.calls.lock().unwrap()
        }
    }

    impl llm_runtime::test_support::FixtureTransport for HangingOpenTransport {
        fn execute<'a>(
            &'a self,
            _request: &'a ProviderRequest,
        ) -> BoxFuture<'a, Result<ProviderResponse, LlmError>> {
            Box::pin(async {
                Err(LlmError::Transport {
                    message: "execute not scripted".to_string(),
                })
            })
        }

        fn open_stream<'a>(
            &'a self,
            _request: &'a ProviderRequest,
        ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>> {
            *self.calls.lock().unwrap() += 1;
            Box::pin(std::future::pending())
        }
    }
    llm_runtime::impl_fixture_transport!(HangingOpenTransport);

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

    // ── M12: streaming idle watchdog (cc 2.1.196 default-on) ─────────────────

    /// A frame stream that NEVER produces a frame nor completes — models a
    /// hung connection so the idle watchdog must fire.
    struct HangingFrames;
    impl crate::FrameStream for HangingFrames {
        fn next_frame(&mut self) -> BoxFuture<'_, Result<Option<crate::RawStreamFrame>, LlmError>> {
            Box::pin(std::future::pending())
        }
    }

    /// Transport that opens a 200 stream whose frames hang forever.
    struct HangingStreamTransport;
    impl llm_runtime::test_support::FixtureTransport for HangingStreamTransport {
        fn execute<'a>(
            &'a self,
            _request: &'a ProviderRequest,
        ) -> BoxFuture<'a, Result<ProviderResponse, LlmError>> {
            Box::pin(async move {
                Err(LlmError::Transport {
                    message: "execute not scripted".to_string(),
                })
            })
        }
        fn open_stream<'a>(
            &'a self,
            _request: &'a ProviderRequest,
        ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>> {
            Box::pin(async move {
                Ok(StreamingResponse {
                    status: 200,
                    headers: BTreeMap::new(),
                    frames: Box::new(HangingFrames),
                })
            })
        }
    }
    llm_runtime::impl_fixture_transport!(HangingStreamTransport);

    /// The watchdog is ON by default and aborts a stream that produces no
    /// event within the idle timeout, surfacing a detectable idle-timeout
    /// error. `start_paused` auto-advances the mock clock to the deadline.
    #[tokio::test(start_paused = true)]
    async fn streaming_idle_watchdog_aborts_hung_stream() {
        use futures::StreamExt;
        let adapter = make_adapter(Arc::new(HangingStreamTransport) as Arc<dyn Transport>)
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
    }

    // ── Task 1: routing.fallback/retry adapter tests ──────────────────────────

    /// Build an adapter with routing overrides for per-model fallback and retry.
    fn make_adapter_with_routing(
        transport: Arc<dyn Transport>,
        fallback_overrides: std::collections::BTreeMap<String, Vec<String>>,
        settings_max_retries: Option<u32>,
        settings_backoff_ms: Option<u64>,
    ) -> ApiService {
        #[allow(deprecated)]
        std::env::set_var("ROUTING_TEST_KEY", "test-key");
        let client = Arc::new(
            ModelRuntime::from_config(ClientConfig {
                providers: vec![ProviderProfile {
                    wire_profile: None,
                    regions: lingxi_llm_client::protocol::Region::all(),
                    provider_id: ProviderId::AnthropicFirstParty,
                    profile_name: "anthropic".to_string(),
                    base_url: "https://api.anthropic.com".to_string(),
                    protocol: ProtocolFamily::AnthropicMessages,
                    auth: AuthStrategy::ApiKey,
                    credential: CredentialConfig::Env {
                        var: "ROUTING_TEST_KEY".to_string(),
                    },
                    models: vec![
                        ModelProfile {
                            display_model: "claude-opus-4-6".to_string(),
                            request_model: "claude-opus-4-6".to_string(),
                            billing_model: "claude-opus-4-6".to_string(),
                            aliases: vec![],
                            description: None,
                            metadata: Default::default(),
                            capabilities: Capabilities {
                                streaming: true,
                                tools: true,
                                reasoning: true,
                                ..Default::default()
                            },
                        },
                        ModelProfile {
                            display_model: "claude-haiku-4-20250307".to_string(),
                            request_model: "claude-haiku-4-20250307".to_string(),
                            billing_model: "claude-haiku-4".to_string(),
                            aliases: vec![],
                            description: None,
                            metadata: Default::default(),
                            capabilities: Capabilities {
                                streaming: true,
                                tools: true,
                                reasoning: true,
                                ..Default::default()
                            },
                        },
                        ModelProfile {
                            display_model: "claude-sonnet-4-20250514".to_string(),
                            request_model: "claude-sonnet-4-20250514".to_string(),
                            billing_model: "claude-sonnet-4".to_string(),
                            aliases: vec!["claude".to_string()],
                            description: None,
                            metadata: Default::default(),
                            capabilities: Capabilities {
                                streaming: true,
                                tools: true,
                                reasoning: true,
                                ..Default::default()
                            },
                        },
                    ],
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
            .expect("client"),
        );
        ApiService::new_with_routing(
            client,
            transport,
            SubscriberState::default(),
            UserAgentEnv {
                user_type: Some("external".to_string()),
                entrypoint: Some("cli".to_string()),
                ..Default::default()
            },
            "0.0.0",
            None,
            None,
            None,
            fallback_overrides,
            settings_max_retries,
            settings_backoff_ms,
        )
    }

    fn routing_ok_response_json() -> serde_json::Value {
        serde_json::json!({
            "id": "msg_routing",
            "model": "claude-sonnet-4-20250514",
            "content": [{"type": "text", "text": "hello"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 5, "output_tokens": 2}
        })
    }

    /// Per-model fallback entry wins over global fallback_model.
    ///
    /// Uses `claude-opus-4-6` as primary (is_non_custom_opus = true → allow_fallback
    /// activates naturally for a non-subscriber, no process env mutation needed).
    /// After 3 consecutive 529s the per-model fallback to haiku fires.
    #[tokio::test]
    async fn per_model_fallback_wins_over_global() {
        let haiku_ok = serde_json::json!({
            "id": "msg_haiku",
            "model": "claude-haiku-4-20250307",
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 5, "output_tokens": 2}
        });
        let overloaded_body = serde_json::json!({
            "type": "error",
            "error": {"type": "overloaded_error", "message": "Overloaded"}
        });
        let transport = FakeTransport::sequence(vec![
            FakeResponse::Ok(ProviderResponse::json(529, overloaded_body.clone())),
            FakeResponse::Ok(ProviderResponse::json(529, overloaded_body.clone())),
            FakeResponse::Ok(ProviderResponse::json(529, overloaded_body.clone())),
            FakeResponse::Ok(ProviderResponse::json(200, haiku_ok)),
        ]);

        let mut fallback_overrides = std::collections::BTreeMap::new();
        // Per-model: opus → haiku (single-entry chain).
        fallback_overrides.insert(
            "claude-opus-4-6".to_string(),
            vec!["claude-haiku-4-20250307".to_string()],
        );

        // No env var needed: claude-opus-4-6 is_non_custom_opus=true → allow_fallback=true
        // for non-subscriber (default SubscriberState).
        let mut adapter =
            make_adapter_with_routing(transport.clone(), fallback_overrides, None, Some(0));
        // Global fallback also points somewhere — per-model must win.
        adapter.fallback_models = vec!["claude-sonnet-4-20250514".to_string()];

        let result = adapter
            .messages_create({
                let mut request = crate::MessagesCreateRequest::new(
                    "claude-opus-4-6",
                    None,
                    None,
                    Vec::new(),
                    Vec::new(),
                );
                request.opts.fallback = match None {
                    Some(models) => crate::FallbackPolicy::from_models_csv(models),
                    None => crate::FallbackPolicy::Configured,
                };
                request
            })
            .await;

        // Should succeed — 3 × 529 then haiku 200.
        assert!(
            result.is_ok(),
            "per-model fallback should route to haiku and succeed: {result:?}"
        );
        assert_eq!(
            transport.seen_count(),
            4,
            "expected 4 requests: 3 × 529 + 1 × 200"
        );
    }

    /// Global fallback is used when no per-model entry is present.
    #[tokio::test]
    async fn global_fallback_used_when_no_per_model_entry() {
        let haiku_ok = serde_json::json!({
            "id": "msg_haiku2",
            "model": "claude-haiku-4-20250307",
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 5, "output_tokens": 2}
        });
        let overloaded_body = serde_json::json!({
            "type": "error",
            "error": {"type": "overloaded_error", "message": "Overloaded"}
        });
        let transport = FakeTransport::sequence(vec![
            FakeResponse::Ok(ProviderResponse::json(529, overloaded_body.clone())),
            FakeResponse::Ok(ProviderResponse::json(529, overloaded_body.clone())),
            FakeResponse::Ok(ProviderResponse::json(529, overloaded_body.clone())),
            FakeResponse::Ok(ProviderResponse::json(200, haiku_ok)),
        ]);

        // No per-model overrides.
        let mut adapter = make_adapter_with_routing(
            transport.clone(),
            std::collections::BTreeMap::new(),
            None,
            Some(0),
        );
        // Global fallback: opus → haiku.
        adapter.fallback_models = vec!["claude-haiku-4-20250307".to_string()];

        // claude-opus-4-6 is_non_custom_opus=true → allow_fallback=true for non-subscriber.
        let result = adapter
            .messages_create({
                let mut request = crate::MessagesCreateRequest::new(
                    "claude-opus-4-6",
                    None,
                    None,
                    Vec::new(),
                    Vec::new(),
                );
                request.opts.fallback = match None {
                    Some(models) => crate::FallbackPolicy::from_models_csv(models),
                    None => crate::FallbackPolicy::Configured,
                };
                request
            })
            .await;

        assert!(result.is_ok(), "global fallback should work: {result:?}");
        assert_eq!(transport.seen_count(), 4);
    }

    /// Per-model fallback fires even when the request uses an ALIAS of the
    /// primary model.
    ///
    /// `fallback_overrides` keys are keyed by the display model; if the request
    /// arrives as an alias (e.g. `"claude"` instead of `"claude-sonnet-4-20250514"`)
    /// the lookup must normalize via `alias_to_display` before probing the map.
    ///
    /// RED on the old code (raw model probe skips the per-model entry when an
    /// alias is used); GREEN after the alias normalization fix.
    #[tokio::test]
    async fn per_model_fallback_fires_via_alias() {
        // Three 529s then a success on the fallback (haiku).
        let overloaded_json = serde_json::json!({
            "type": "error",
            "error": {"type": "overloaded_error", "message": "overloaded"}
        });
        let transport = FakeTransport::sequence(vec![
            FakeResponse::Ok(ProviderResponse::json(529, overloaded_json.clone())),
            FakeResponse::Ok(ProviderResponse::json(529, overloaded_json.clone())),
            FakeResponse::Ok(ProviderResponse::json(529, overloaded_json.clone())),
            FakeResponse::Ok(ProviderResponse::json(200, routing_ok_response_json())),
        ]);

        // Per-model fallback: display "claude-sonnet-4-20250514" → "claude-haiku-4-20250307"
        // (single-entry chain).
        let mut fallback_overrides = std::collections::BTreeMap::new();
        fallback_overrides.insert(
            "claude-sonnet-4-20250514".to_string(),
            vec!["claude-haiku-4-20250307".to_string()],
        );

        let adapter =
            make_adapter_with_routing(transport.clone(), fallback_overrides, None, Some(0));

        // Request via ALIAS — the alias_to_display map must normalize this to
        // "claude-sonnet-4-20250514" before the fallback_overrides lookup.
        let result = adapter
            .messages_create({
                let mut request = crate::MessagesCreateRequest::new(
                    "claude",
                    // alias of "claude-sonnet-4-20250514"
                    None,
                    custom_system_prompt(Some("sys")),
                    Vec::new(),
                    Vec::new(),
                );
                request.opts.fallback = match None {
                    Some(models) => crate::FallbackPolicy::from_models_csv(models),
                    None => crate::FallbackPolicy::Configured,
                };
                request
            })
            .await;

        assert!(
            result.is_ok(),
            "alias-keyed per-model fallback should fire and succeed; got: {result:?}"
        );
        // 3 primary 529s + 1 fallback success = 4 transport calls.
        assert_eq!(
            transport.seen_count(),
            4,
            "expected 3 failing primary calls + 1 successful fallback call"
        );
    }

    // ── Task 5: fallback chain walk tests ────────────────────────────────────

    #[test]
    fn fallback_csv_is_ordered_trimmed_and_deduplicated() {
        assert_eq!(
            super::parse_fallback_chain(" sonnet,haiku,sonnet "),
            vec!["sonnet".to_string(), "haiku".to_string()]
        );
    }

    /// 2-entry chain: primary → chain[0] → chain[1] when all 529s.
    ///
    /// Scripted: 3×529 on primary, 3×529 on chain[0], then 200 on chain[1].
    /// Asserts the model sequence primary→c0→c1 via captured request bodies.
    #[tokio::test]
    async fn two_entry_chain_walks_both_entries() {
        // The drive loop's attempt budget is only stable while the dispatch
        // opt-in stays OFF: a leaked `tengu_cedar_lattice` adds one extra,
        // budget-free "strip and retry" attempt on the first 5xx.
        let _dispatch_off = cedar_lattice_read();
        let overloaded = serde_json::json!({
            "type": "error",
            "error": {"type": "overloaded_error", "message": "Overloaded"}
        });
        let c1_ok = serde_json::json!({
            "id": "msg_c1",
            "model": "claude-haiku-4-20250307",
            "content": [{"type": "text", "text": "c1 ok"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 5, "output_tokens": 2}
        });
        // 3 × 529 on primary (claude-opus-4-6)
        // 3 × 529 on chain[0] (claude-sonnet-4-20250514)
        // 1 × 200 on chain[1] (claude-haiku-4-20250307)
        let transport = FakeTransport::sequence(vec![
            FakeResponse::Ok(ProviderResponse::json(529, overloaded.clone())),
            FakeResponse::Ok(ProviderResponse::json(529, overloaded.clone())),
            FakeResponse::Ok(ProviderResponse::json(529, overloaded.clone())),
            FakeResponse::Ok(ProviderResponse::json(529, overloaded.clone())),
            FakeResponse::Ok(ProviderResponse::json(529, overloaded.clone())),
            FakeResponse::Ok(ProviderResponse::json(529, overloaded.clone())),
            FakeResponse::Ok(ProviderResponse::json(200, c1_ok)),
        ]);

        let mut fallback_overrides = std::collections::BTreeMap::new();
        // 2-entry chain: opus-4-6 → sonnet-4-20250514 → haiku-4-20250307
        fallback_overrides.insert(
            "claude-opus-4-6".to_string(),
            vec![
                "claude-sonnet-4-20250514".to_string(),
                "claude-haiku-4-20250307".to_string(),
            ],
        );

        let adapter =
            make_adapter_with_routing(transport.clone(), fallback_overrides, None, Some(0));

        let result = adapter
            .messages_create({
                let mut request = crate::MessagesCreateRequest::new(
                    "claude-opus-4-6",
                    None,
                    None,
                    Vec::new(),
                    Vec::new(),
                );
                request.opts.fallback = match None {
                    Some(models) => crate::FallbackPolicy::from_models_csv(models),
                    None => crate::FallbackPolicy::Configured,
                };
                request
            })
            .await;

        assert!(
            result.is_ok(),
            "chain walk must succeed on chain[1]: {result:?}"
        );
        assert_eq!(
            transport.seen_count(),
            7,
            "3 primary + 3 chain[0] + 1 chain[1]"
        );

        // Assert the model sequence: first 3 requests use primary, next 3 use chain[0],
        // last 1 uses chain[1].
        let primary = "claude-opus-4-6";
        let c0 = "claude-sonnet-4-20250514";
        let c1 = "claude-haiku-4-20250307";
        for i in 0..3 {
            assert_eq!(
                transport.seen_body_model(i).as_deref(),
                Some(primary),
                "request {i} must use primary model"
            );
        }
        for i in 3..6 {
            assert_eq!(
                transport.seen_body_model(i).as_deref(),
                Some(c0),
                "request {i} must use chain[0]"
            );
        }
        assert_eq!(
            transport.seen_body_model(6).as_deref(),
            Some(c1),
            "request 6 must use chain[1]"
        );
    }

    /// Chain exhaustion: when all entries are overloaded, the call is terminal.
    ///
    /// Single-entry chain: primary 3×529 → chain[0] persistent 529 → terminal error.
    #[tokio::test]
    async fn chain_exhausted_gives_terminal_error() {
        let overloaded = serde_json::json!({
            "type": "error",
            "error": {"type": "overloaded_error", "message": "Overloaded"}
        });
        // primary: 3 × 529 → Fallback
        // chain[0]: persistent 529 → RepeatedOverloaded (is_external=true in make_adapter_with_routing)
        let transport = FakeTransport::always(ProviderResponse::json(529, overloaded));

        let mut fallback_overrides = std::collections::BTreeMap::new();
        fallback_overrides.insert(
            "claude-opus-4-6".to_string(),
            vec!["claude-haiku-4-20250307".to_string()],
        );

        let adapter =
            make_adapter_with_routing(transport.clone(), fallback_overrides, None, Some(0));

        let result = adapter
            .messages_create({
                let mut request = crate::MessagesCreateRequest::new(
                    "claude-opus-4-6",
                    None,
                    None,
                    Vec::new(),
                    Vec::new(),
                );
                request.opts.fallback = match None {
                    Some(models) => crate::FallbackPolicy::from_models_csv(models),
                    None => crate::FallbackPolicy::Configured,
                };
                request
            })
            .await;

        assert!(
            result.is_err(),
            "exhausted chain must produce terminal error"
        );
        // The error must be Overloaded (either repeated=true from external path or
        // plain Overloaded — either variant indicates the chain was walked and terminated).
        assert!(
            matches!(classified_error(&result), Some(LlmError::Overloaded { .. })),
            "terminal error must be LlmError::Overloaded"
        );
    }

    /// Single-entry chain behaves like batch-1 (exactly one fallback hop).
    ///
    /// Uses the existing `per_model_fallback_wins_over_global` scenario but
    /// verifies via `seen_body_model` that the model sequence is correct.
    #[tokio::test]
    async fn single_entry_chain_behaves_like_batch1() {
        // The drive loop's attempt budget is only stable while the dispatch
        // opt-in stays OFF: a leaked `tengu_cedar_lattice` adds one extra,
        // budget-free "strip and retry" attempt on the first 5xx.
        let _dispatch_off = cedar_lattice_read();
        let overloaded = serde_json::json!({
            "type": "error",
            "error": {"type": "overloaded_error", "message": "Overloaded"}
        });
        let haiku_ok = serde_json::json!({
            "id": "msg_haiku",
            "model": "claude-haiku-4-20250307",
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 5, "output_tokens": 2}
        });
        let transport = FakeTransport::sequence(vec![
            FakeResponse::Ok(ProviderResponse::json(529, overloaded.clone())),
            FakeResponse::Ok(ProviderResponse::json(529, overloaded.clone())),
            FakeResponse::Ok(ProviderResponse::json(529, overloaded.clone())),
            FakeResponse::Ok(ProviderResponse::json(200, haiku_ok)),
        ]);

        let mut fallback_overrides = std::collections::BTreeMap::new();
        fallback_overrides.insert(
            "claude-opus-4-6".to_string(),
            vec!["claude-haiku-4-20250307".to_string()],
        );
        let adapter =
            make_adapter_with_routing(transport.clone(), fallback_overrides, None, Some(0));

        let result = adapter
            .messages_create({
                let mut request = crate::MessagesCreateRequest::new(
                    "claude-opus-4-6",
                    None,
                    None,
                    Vec::new(),
                    Vec::new(),
                );
                request.opts.fallback = match None {
                    Some(models) => crate::FallbackPolicy::from_models_csv(models),
                    None => crate::FallbackPolicy::Configured,
                };
                request
            })
            .await;

        assert!(
            result.is_ok(),
            "single-entry chain must succeed: {result:?}"
        );
        assert_eq!(transport.seen_count(), 4, "3 primary 529s + 1 fallback 200");
        // First 3 requests: primary model.
        for i in 0..3 {
            assert_eq!(
                transport.seen_body_model(i).as_deref(),
                Some("claude-opus-4-6"),
                "request {i} must use primary"
            );
        }
        // 4th request: fallback model.
        assert_eq!(
            transport.seen_body_model(3).as_deref(),
            Some("claude-haiku-4-20250307"),
            "request 3 must use chain[0]"
        );
    }

    /// Global fallback_model still works when no per-model chain is configured.
    ///
    /// The global `fallback_model` is wrapped into a single-entry chain and walks
    /// the same code path; this test guards that wiring.
    #[tokio::test]
    async fn global_fallback_model_works_without_chain_entry() {
        // The drive loop's attempt budget is only stable while the dispatch
        // opt-in stays OFF: a leaked `tengu_cedar_lattice` adds one extra,
        // budget-free "strip and retry" attempt on the first 5xx.
        let _dispatch_off = cedar_lattice_read();
        let overloaded = serde_json::json!({
            "type": "error",
            "error": {"type": "overloaded_error", "message": "Overloaded"}
        });
        let sonnet_ok = serde_json::json!({
            "id": "msg_sonnet",
            "model": "claude-sonnet-4-20250514",
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 5, "output_tokens": 2}
        });
        let transport = FakeTransport::sequence(vec![
            FakeResponse::Ok(ProviderResponse::json(529, overloaded.clone())),
            FakeResponse::Ok(ProviderResponse::json(529, overloaded.clone())),
            FakeResponse::Ok(ProviderResponse::json(529, overloaded.clone())),
            FakeResponse::Ok(ProviderResponse::json(200, sonnet_ok)),
        ]);

        let mut adapter = make_adapter_with_routing(
            transport.clone(),
            std::collections::BTreeMap::new(),
            None,
            Some(0),
        );
        // Set global fallback only (no per-model chain).
        adapter.fallback_models = vec!["claude-sonnet-4-20250514".to_string()];

        let result = adapter
            .messages_create({
                let mut request = crate::MessagesCreateRequest::new(
                    "claude-opus-4-6",
                    None,
                    None,
                    Vec::new(),
                    Vec::new(),
                );
                request.opts.fallback = match None {
                    Some(models) => crate::FallbackPolicy::from_models_csv(models),
                    None => crate::FallbackPolicy::Configured,
                };
                request
            })
            .await;

        assert!(result.is_ok(), "global fallback must work: {result:?}");
        assert_eq!(transport.seen_count(), 4);
        // Request 3 must use the global fallback model.
        assert_eq!(
            transport.seen_body_model(3).as_deref(),
            Some("claude-sonnet-4-20250514"),
            "request 3 must use global fallback model"
        );
    }

    /// `settings_max_retries=2` causes terminal after 3 executions (not 11).
    ///
    /// The adapter reads `LINGXI_MAX_RETRIES` from the process env in
    /// `messages_create`.  To avoid interference with parallel tests we verify
    /// via the retry.rs layer (which is injected, not process-env) rather than
    /// through the adapter's env path.  The adapter's `settings_max_retries`
    /// field is directly observable via the resolve_retry_control_with_settings
    /// call: when env var is absent it uses `settings_max_retries` as the
    /// effective limit.  We temporarily clear the env var then restore it.
    #[tokio::test]
    async fn settings_max_retries_beats_default() {
        // The drive loop's attempt budget is only stable while the dispatch
        // opt-in stays OFF: a leaked `tengu_cedar_lattice` adds one extra,
        // budget-free "strip and retry" attempt on the first 5xx.
        let _dispatch_off = cedar_lattice_read();
        // We can test the settings path directly: when the env var is absent
        // the settings_max_retries field applies.  We control LINGXI_MAX_RETRIES
        // for the duration of this test — accept minor isolation risk since the
        // pre-existing test suite also does this.
        let transport = FakeTransport::sequence(vec![FakeResponse::Ok(ProviderResponse::json(
            500,
            serde_json::json!({"type": "error", "error": {"type": "api_error", "message": "internal"}}),
        ))]);

        let adapter = make_adapter_with_routing(
            transport.clone(),
            std::collections::BTreeMap::new(),
            Some(2), // settings says max 2 retries
            None,
        );

        // Temporarily unset LINGXI_MAX_RETRIES so settings value wins.
        let saved = std::env::var("LINGXI_MAX_RETRIES").ok();
        #[allow(deprecated)]
        std::env::remove_var("LINGXI_MAX_RETRIES");

        let result = adapter
            .messages_create(crate::MessagesCreateRequest::new(
                "claude-sonnet-4-20250514",
                None,
                None,
                Vec::new(),
                Vec::new(),
            ))
            .await;

        // Restore.
        if let Some(v) = saved {
            #[allow(deprecated)]
            std::env::set_var("LINGXI_MAX_RETRIES", v);
        }

        assert!(result.is_err(), "must fail after exhausting budget");
        // max_retries=2 → 3 executions (2 sleeps + 1 final).
        assert_eq!(
            transport.seen_count(),
            3,
            "settings_max_retries=2 should give 3 executions (2 retries + 1 initial)"
        );
    }

    /// Env `LINGXI_MAX_RETRIES` beats `settings_max_retries`.
    ///
    /// Proven via `resolve_retry_control_with_settings` unit tests in retry.rs;
    /// this adapter-level test verifies the wiring by injecting through
    /// `ResolveRetryEnv` directly rather than mutating the process env.
    ///
    /// We use `crate::model::retry::resolve_retry_control_with_settings` to
    /// build the expected `RetryControl` and compare `max_retries`.
    #[test]
    fn env_max_retries_beats_settings_via_resolve() {
        use crate::model::retry::{
            resolve_retry_control_with_settings, ResolveRetryEnv, DEFAULT_MAX_RETRIES,
        };

        // env=Some("1") + settings=Some(8) → max_retries=1 (env wins).
        let env_with_1 = ResolveRetryEnv {
            max_retries: Some("1".to_string()),
            ..ResolveRetryEnv::default()
        };
        let ctl = resolve_retry_control_with_settings(
            "claude-sonnet-4-20250514",
            None,
            false,
            &env_with_1,
            Some(8),
        );
        assert_eq!(ctl.max_retries, 1, "env(1) must beat settings(8)");

        // env=None + settings=Some(7) → max_retries=7 (settings wins).
        let env_absent = ResolveRetryEnv::default();
        let ctl2 = resolve_retry_control_with_settings(
            "claude-sonnet-4-20250514",
            None,
            false,
            &env_absent,
            Some(7),
        );
        assert_eq!(ctl2.max_retries, 7, "settings(7) must beat default(10)");

        // env=None + settings=None → DEFAULT.
        let ctl3 = resolve_retry_control_with_settings(
            "claude-sonnet-4-20250514",
            None,
            false,
            &env_absent,
            None,
        );
        assert_eq!(ctl3.max_retries, DEFAULT_MAX_RETRIES);
    }

    /// `settings_backoff_ms=1000` doubles the jitter ladder base.
    ///
    /// With `backoff_ms=1000` and time paused, we verify the first retry delay
    /// is ≥ 800 ms (= 1000 × 0.8 lower-jitter-bound).  Without the setting the
    /// base would be 500 ms (lower bound 400 ms) — so 800 ms is above the
    /// un-scaled upper bound (600 ms) which proves scaling is active.
    #[tokio::test(start_paused = true)]
    async fn backoff_ms_scales_jitter_base() {
        // The drive loop's attempt budget is only stable while the dispatch
        // opt-in stays OFF: a leaked `tengu_cedar_lattice` adds one extra,
        // budget-free "strip and retry" attempt on the first 5xx.
        let _dispatch_off = cedar_lattice_read();
        let transport = FakeTransport::sequence(vec![
            FakeResponse::Ok(ProviderResponse::json(
                500,
                serde_json::json!({"type": "error", "error": {"type": "api_error", "message": "err"}}),
            )),
            FakeResponse::Ok(ProviderResponse::json(200, routing_ok_response_json())),
        ]);

        let adapter = make_adapter_with_routing(
            transport.clone(),
            std::collections::BTreeMap::new(),
            None,
            Some(1000), // backoff_ms = 1000 → first rung 1000, additive jitter [1000, 1250)
        );

        let before = tokio::time::Instant::now();
        let result = adapter
            .messages_create(crate::MessagesCreateRequest::new(
                "claude-sonnet-4-20250514",
                None,
                None,
                Vec::new(),
                Vec::new(),
            ))
            .await;
        let elapsed = before.elapsed();

        assert!(result.is_ok(), "should succeed after retry: {result:?}");
        // Additive jitter (binary `sle`: base + rand(0,0.25)·base) → base 1000
        // gives [1000, 1250). Default base 500 → [500, 625), upper 625 < 800.
        // So ≥ 800 ms proves the 1000 ms base (not the default 500) is in effect.
        assert!(
            elapsed >= std::time::Duration::from_millis(800),
            "backoff_ms=1000 should produce ≥ 800 ms delay; elapsed={elapsed:?}"
        );
        // Must be < 1250 ms (upper additive-jitter bound: 1000 + 0.25·1000).
        assert!(
            elapsed < std::time::Duration::from_millis(1250),
            "backoff_ms=1000 delay should be < 1250 ms; elapsed={elapsed:?}"
        );
    }

    // ── T3 Step 2: 2xx rate-limit header feed ────────────────────────────────

    /// A 2xx response with `anthropic-ratelimit-unified-overage-status: rejected`
    /// must be stored in `last_rate_limit_info` and trigger a `tracing::warn!`.
    ///
    /// We only assert the data is stored; the warn fires on a live-log subscriber
    /// which we do not attach in tests — the absence of a panic is the assertion.
    #[tokio::test]
    async fn rate_limit_info_stored_from_2xx_response() {
        let mut headers = BTreeMap::new();
        headers.insert(
            "anthropic-ratelimit-unified-representative-claim".to_string(),
            "five_hour".to_string(),
        );
        headers.insert(
            "anthropic-ratelimit-unified-overage-status".to_string(),
            "rejected".to_string(),
        );

        let transport = FakeTransport::always(ProviderResponse {
            status: 200,
            headers,
            body_json: provider_payload(ok_response_json()),
            request_id: None,
        });
        let adapter = make_adapter(transport);
        let _ = adapter
            .messages_create(crate::MessagesCreateRequest::new(
                "claude-sonnet-4-20250514",
                None,
                None,
                Vec::new(),
                Vec::new(),
            ))
            .await
            .expect("ok");

        let info = adapter
            .last_rate_limit_info()
            .expect("last_rate_limit_info must be Some after a 2xx with unified headers");
        assert_eq!(
            info.rate_limit_type.as_deref(),
            Some("five_hour"),
            "rate_limit_type must be five_hour"
        );
        assert_eq!(
            info.overage_status.as_deref(),
            Some("rejected"),
            "overage_status must be rejected"
        );
    }

    /// Without unified rate-limit headers the cached info stays `None`.
    #[tokio::test]
    async fn rate_limit_info_none_when_no_unified_headers() {
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let adapter = make_adapter(transport);
        let _ = adapter
            .messages_create(crate::MessagesCreateRequest::new(
                "claude-sonnet-4-20250514",
                None,
                None,
                Vec::new(),
                Vec::new(),
            ))
            .await
            .expect("ok");

        // No unified headers → None still.
        assert!(
            adapter.last_rate_limit_info().is_none(),
            "last_rate_limit_info must be None when no unified headers are present"
        );
    }

    // ── count_tokens (inherent ApiService) ──────────────────────────────────────

    /// The adapter override drives the real `/v1/messages/count_tokens` endpoint
    /// on an Anthropic route: it sends one request to the count_tokens URL
    /// carrying the `count_tokens` beta header and returns the decoded
    /// `input_tokens` from the response.
    #[tokio::test]
    async fn count_tokens_through_adapter_hits_anthropic_endpoint_with_beta() {
        let transport = FakeTransport::always(ProviderResponse {
            status: 200,
            headers: BTreeMap::new(),
            body_json: provider_payload(serde_json::json!({ "input_tokens": 2095 })),
            request_id: None,
        });
        let adapter = make_adapter(transport.clone());

        let count = adapter
            .count_tokens(
                "claude-sonnet-4-20250514",
                None,
                Some("you are helpful"),
                Vec::new(),
                Vec::new(),
            )
            .await
            .expect("count_tokens ok");

        assert_eq!(
            count, 2095,
            "decoded input_tokens from the count_tokens response"
        );
        assert_eq!(
            transport.seen_count(),
            1,
            "exactly one count_tokens request sent"
        );

        let url = transport.seen.lock().unwrap()[0].url.clone();
        assert!(
            url.ends_with("/v1/messages/count_tokens?beta=true"),
            "must route to the count_tokens endpoint; url={url}"
        );
        let expected_beta = crate::model::betas::assemble_beta_header(
            crate::model::betas::Provider::Anthropic,
            crate::model::betas::Endpoint::CountTokens,
            &crate::model::betas::BetaContext::for_model("claude-sonnet-4-20250514"),
        );
        let expected_beta = format!(
            "{expected_beta},{}",
            lingxi_llm_client::providers::anthropic::request_policy::TOKEN_COUNTING
        );
        assert_eq!(
            transport
                .seen_headers(0)
                .get("anthropic-beta")
                .map(String::as_str),
            Some(expected_beta.as_str()),
            "countTokens appends the SDK token-counting beta after model betas"
        );
    }

    // ── T3 Step 3: stream telemetry twins ────────────────────────────────────

    /// A `FrameStream` that yields scripted raw SSE frames (encoded as
    /// JSON byte sequences) then terminates. Used to drive the stream decoder
    /// with a controlled event sequence so the `drive_stream` unfold
    /// emits telemetry at the right points.
    struct ScriptedFrames {
        frames: Vec<Vec<u8>>,
        idx: usize,
    }

    impl ScriptedFrames {
        fn new(frames: Vec<Vec<u8>>) -> Self {
            Self { frames, idx: 0 }
        }
    }

    impl crate::FrameStream for ScriptedFrames {
        fn next_frame(&mut self) -> BoxFuture<'_, Result<Option<crate::RawStreamFrame>, LlmError>> {
            let result = if self.idx < self.frames.len() {
                let bytes = self.frames[self.idx].clone();
                self.idx += 1;
                Ok(Some(crate::RawStreamFrame::new(bytes)))
            } else {
                Ok(None)
            };
            Box::pin(async move { result })
        }
    }

    /// A transport that delivers a fixed successful stream ending in `message_stop`.
    ///
    /// Builds valid Anthropic SSE frames (as raw JSON lines) so the `AnthropicMessages`
    /// codec can decode them. The stream ends with `message_stop` which is the
    /// terminal event → should trigger `emit_succeeded` once.
    struct ScriptedStreamTransport {
        frames: Vec<Vec<u8>>,
        headers: BTreeMap<String, String>,
        status: u16,
    }

    impl ScriptedStreamTransport {
        /// Build a transport that delivers a minimal valid anthropic stream:
        /// `message_start` → `message_delta(stop_reason=end_turn)` → `message_stop`.
        fn anthropic_success() -> Arc<Self> {
            let frames = vec![
                br#"{"type":"message_start","message":{"id":"msg_t","model":"claude-sonnet-4-20250514","usage":{"input_tokens":1,"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}"#.to_vec(),
                br#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}"#.to_vec(),
                br#"{"type":"message_stop"}"#.to_vec(),
            ];
            Arc::new(Self {
                frames,
                headers: BTreeMap::new(),
                status: 200,
            })
        }

        /// Build a transport that delivers a single malformed frame → decoder error.
        fn malformed_frame() -> Arc<Self> {
            let frames = vec![b"not-valid-json".to_vec()];
            Arc::new(Self {
                frames,
                headers: BTreeMap::new(),
                status: 200,
            })
        }
    }

    impl llm_runtime::test_support::FixtureTransport for ScriptedStreamTransport {
        fn execute<'a>(
            &'a self,
            _request: &'a ProviderRequest,
        ) -> BoxFuture<'a, Result<ProviderResponse, LlmError>> {
            Box::pin(async move {
                Err(LlmError::Transport {
                    message: "execute not used in ScriptedStreamTransport".into(),
                })
            })
        }

        fn open_stream<'a>(
            &'a self,
            _request: &'a ProviderRequest,
        ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>> {
            let frames: Vec<Vec<u8>> = self.frames.clone();
            let headers = self.headers.clone();
            let status = self.status;
            Box::pin(async move {
                Ok(StreamingResponse {
                    status,
                    headers,
                    frames: Box::new(ScriptedFrames::new(frames)),
                })
            })
        }
    }
    llm_runtime::impl_fixture_transport!(ScriptedStreamTransport);

    /// Returns a scripted sequence of HTTP stream responses and records the
    /// exact beta headers of each physical request.
    struct NativeAfkStreamRetryTransport {
        responses: Mutex<std::collections::VecDeque<(u16, BTreeMap<String, String>, Vec<Vec<u8>>)>>,
        seen: Mutex<Vec<ProviderRequest>>,
    }

    impl llm_runtime::test_support::FixtureTransport for NativeAfkStreamRetryTransport {
        fn execute<'a>(
            &'a self,
            _request: &'a ProviderRequest,
        ) -> BoxFuture<'a, Result<ProviderResponse, LlmError>> {
            Box::pin(async {
                Err(LlmError::Transport {
                    message: "execute is not used in Native AFK stream retry test".into(),
                })
            })
        }

        fn open_stream<'a>(
            &'a self,
            request: &'a ProviderRequest,
        ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>> {
            self.seen.lock().unwrap().push(request.clone());
            let next = self.responses.lock().unwrap().pop_front();
            Box::pin(async move {
                let (status, headers, frames) = next.ok_or_else(|| LlmError::Transport {
                    message: "Native AFK stream retry sequence exhausted".into(),
                })?;
                Ok(StreamingResponse {
                    status,
                    headers,
                    frames: Box::new(ScriptedFrames::new(frames)),
                })
            })
        }
    }
    llm_runtime::impl_fixture_transport!(NativeAfkStreamRetryTransport);

    /// Build a streaming adapter with an attached analytics bus.
    async fn make_stream_adapter_with_bus(
        transport: Arc<dyn Transport>,
    ) -> (ApiService, Arc<::telemetry::InMemorySink>) {
        use ::telemetry::{AnalyticsBus, InMemorySink};

        #[allow(deprecated)]
        std::env::set_var("STREAM_TELEM_TEST_KEY", "test-key");
        let client = Arc::new(
            ModelRuntime::from_config(ClientConfig {
                providers: vec![ProviderProfile {
                    wire_profile: None,
                    regions: lingxi_llm_client::protocol::Region::all(),
                    provider_id: ProviderId::AnthropicFirstParty,
                    profile_name: "anthropic".to_string(),
                    base_url: "https://api.anthropic.com".to_string(),
                    protocol: ProtocolFamily::AnthropicMessages,
                    auth: AuthStrategy::ApiKey,
                    credential: CredentialConfig::Env {
                        var: "STREAM_TELEM_TEST_KEY".to_string(),
                    },
                    models: vec![ModelProfile {
                        display_model: "claude-sonnet-4-20250514".to_string(),
                        request_model: "claude-sonnet-4-20250514".to_string(),
                        billing_model: "claude-sonnet-4".to_string(),
                        aliases: vec![],
                        description: None,
                        metadata: Default::default(),
                        capabilities: Capabilities {
                            streaming: true,
                            tools: true,
                            reasoning: true,
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
            .expect("client"),
        );

        let bus = Arc::new(AnalyticsBus::new());
        let sink = Arc::new(InMemorySink::new());
        bus.attach_sink(sink.clone()).await;

        let adapter = ApiService::new(
            client,
            transport,
            SubscriberState::default(),
            UserAgentEnv {
                user_type: Some("external".to_string()),
                entrypoint: Some("cli".to_string()),
                ..Default::default()
            },
            "0.0.0",
            Some(bus),
            None,
        );
        (adapter, sink)
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

    // ── AWS auth refresh trigger (2.1.198 V_c/G_c/s_f + Ygf) ─────────────────

    /// Counting stand-in for the `ZBd` driver — the drive loop only needs the
    /// object-safe `AwsAuthRefresh` seam.
    #[derive(Debug, Default)]
    struct CountingAwsRefresh {
        calls: std::sync::atomic::AtomicU32,
    }

    impl crate::auth::external_aws::AwsAuthRefresh for CountingAwsRefresh {
        fn refresh(&self) -> BoxFuture<'_, bool> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async { true })
        }
    }

    /// Bedrock-provider adapter with the AWS auth-refresh seam attached.
    fn make_bedrock_adapter_with_aws(
        transport: Arc<dyn Transport>,
        aws: Arc<CountingAwsRefresh>,
    ) -> ApiService {
        let client = Arc::new(
            ModelRuntime::from_config(ClientConfig {
                providers: vec![ProviderProfile {
                    wire_profile: None,
                    regions: lingxi_llm_client::protocol::Region::all(),
                    provider_id: ProviderId::BedrockClaude,
                    profile_name: "bedrock".to_string(),
                    base_url: "https://bedrock-runtime.us-east-1.amazonaws.com".to_string(),
                    protocol: ProtocolFamily::BedrockClaude,
                    // Auth None so prepare() succeeds without SigV4 material —
                    // the refresh trigger keys off the RESPONSE error + the
                    // route's provider_id, not the auth strategy.
                    auth: AuthStrategy::None,
                    credential: CredentialConfig::None,
                    models: vec![ModelProfile {
                        display_model: "model".to_string(),
                        request_model: "model".to_string(),
                        billing_model: "model".to_string(),
                        aliases: Vec::new(),
                        description: None,
                        metadata: Default::default(),
                        capabilities: Capabilities {
                            streaming: true,
                            tools: true,
                            reasoning: true,
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
            .expect("client"),
        );
        ApiService::new(
            client,
            transport,
            SubscriberState::default(),
            UserAgentEnv {
                user_type: Some("external".to_string()),
                entrypoint: Some("cli".to_string()),
                ..Default::default()
            },
            "0.0.0",
            None,
            None,
        )
        .with_aws_auth(aws)
    }

    /// 401 `authentication_error` body (decodes to `LlmError::Authentication`
    /// through the Bedrock codec's Anthropic-shape error decode) — the
    /// expired-STS terminal the 2.1.198 trigger classifies via `V_c`.
    fn expired_sts_response() -> ProviderResponse {
        ProviderResponse::json(
            401,
            serde_json::json!({
                "type": "error",
                "error": {
                    "type": "authentication_error",
                    "message": "The security token included in the request is expired"
                }
            }),
        )
    }

    #[tokio::test]
    async fn aws_auth_error_refreshes_and_retries_once() {
        // 401 (expired STS) then 200: the hook must run ZBd once and the retry
        // must succeed — the 2.1.198 behavior replacing the "/login" dead end.
        let transport = FakeTransport::sequence(vec![
            FakeResponse::Ok(expired_sts_response()),
            FakeResponse::Ok(ProviderResponse::json(200, ok_response_json())),
        ]);
        let aws = Arc::new(CountingAwsRefresh::default());
        let adapter = make_bedrock_adapter_with_aws(transport.clone(), aws.clone());

        let mut request =
            crate::MessagesCreateRequest::new("model", None, None, Vec::new(), Vec::new());
        request.opts.max_output_tokens = Some(4096);
        let out = adapter.messages_create(request).await;
        assert!(out.is_ok(), "retry after refresh must succeed: {out:?}");
        assert_eq!(
            aws.calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "exactly one refresh"
        );
        assert_eq!(transport.seen_count(), 2, "original + one retry");
    }

    #[tokio::test]
    async fn aws_auth_retries_bounded_at_ygf_two() {
        // Every attempt 401s: the hook may fire at most AWS_AUTH_MAX_ATTEMPTS
        // (Ygf=2) times, then the error goes terminal (the binary's
        // `api_request_aws_auth_exhausted` throw).
        let transport = FakeTransport::sequence(vec![FakeResponse::Ok(expired_sts_response())]);
        let aws = Arc::new(CountingAwsRefresh::default());
        let adapter = make_bedrock_adapter_with_aws(transport.clone(), aws.clone());

        let mut request =
            crate::MessagesCreateRequest::new("model", None, None, Vec::new(), Vec::new());
        request.opts.max_output_tokens = Some(4096);
        let out = adapter.messages_create(request).await;
        assert!(
            matches!(
                classified_error(&out),
                Some(LlmError::Authentication { .. })
            ),
            "exhausted refresh budget surfaces the auth error: {out:?}"
        );
        assert_eq!(
            aws.calls.load(std::sync::atomic::Ordering::SeqCst),
            crate::auth::external_aws::AWS_AUTH_MAX_ATTEMPTS,
            "refresh bounded at Ygf=2"
        );
        assert_eq!(
            transport.seen_count(),
            3,
            "initial attempt + 2 refresh retries"
        );
    }

    #[tokio::test]
    async fn non_aws_provider_never_triggers_refresh() {
        // Provider gate (multi-provider structure is sacrosanct): the SAME 401
        // on the Anthropic first-party provider must NOT touch the refresher.
        let transport = FakeTransport::sequence(vec![FakeResponse::Ok(expired_sts_response())]);
        let aws = Arc::new(CountingAwsRefresh::default());
        let adapter = make_adapter(transport.clone());
        let adapter = adapter.with_aws_auth(aws.clone());

        let out = adapter
            .messages_create(crate::MessagesCreateRequest::new(
                "claude-sonnet-4-20250514",
                None,
                None,
                Vec::new(),
                Vec::new(),
            ))
            .await;
        assert!(matches!(
            classified_error(&out),
            Some(LlmError::Authentication { .. })
        ));
        assert_eq!(
            aws.calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "refresh must never run for a non-AWS provider"
        );
        assert_eq!(transport.seen_count(), 1, "401 stays terminal, no retry");
    }

    #[test]
    fn custom_context_beta_preserves_automatic_output_budget_intent() {
        // 520k ASCII bytes are below the default 200k input window under the
        // conservative request estimator, but leave too little headroom for a
        // full 32k output. With the 1M beta the full output allowance fits.
        let messages = vec![text_user_msg(&"x".repeat(520_000))];
        let without = make_adapter(FakeTransport::always(ProviderResponse::json(
            200,
            ok_response_json(),
        )));
        let default_req = without
            .build_request(
                "claude-sonnet-4-20250514",
                None,
                None,
                messages.clone(),
                Vec::new(),
                false,
                None,
                false,
                PromptCacheQuerySource::Unspecified,
            )
            .expect("default request");
        assert_eq!(default_req.input.max_tokens, None);

        let with_beta = make_adapter(FakeTransport::always(ProviderResponse::json(
            200,
            ok_response_json(),
        )))
        .with_custom_cli_betas(vec![
            crate::model::context_window::CONTEXT_1M_BETA_HEADER.to_string(),
        ]);
        assert_eq!(
            with_beta.active_custom_betas(),
            [crate::model::context_window::CONTEXT_1M_BETA_HEADER]
        );
        let beta_req = with_beta
            .build_request(
                "claude-sonnet-4-20250514",
                None,
                None,
                messages,
                Vec::new(),
                false,
                None,
                false,
                PromptCacheQuerySource::Unspecified,
            )
            .expect("1m request");
        assert_eq!(beta_req.input.max_tokens, None);
    }

    mod request_session_capture_tests {
        use super::*;

        fn service_with_metadata() -> ApiService {
            let mut metadata =
                lingxi_llm_client::providers::anthropic::request_policy::AnthropicClientMetadata::
                    claude_code_2_1_293("darwin", "arm64");
            metadata.session_id = Some("stale-builder-session".into());
            make_adapter(FakeTransport::always(ProviderResponse::json(
                200,
                ok_response_json(),
            )))
            .with_anthropic_client_metadata(metadata)
        }

        #[tokio::test]
        async fn live_request_source_is_captured_per_request_and_none_does_not_fall_back() {
            let service = service_with_metadata();
            let source: RequestSessionIdSource =
                Arc::new(|| Box::pin(async { Some("live-session".to_owned()) }));
            service.set_request_session_id_source(source);

            let mut request = LlmRequest::new("claude-sonnet-4-5");
            service.capture_request_session_id(&mut request).await;
            assert_eq!(
                request.execution.request_session_id.as_deref(),
                Some("live-session")
            );

            let source: RequestSessionIdSource = Arc::new(|| Box::pin(async { None }));
            service.set_request_session_id_source(source);
            let mut request = LlmRequest::new("claude-sonnet-4-5");
            service.capture_request_session_id(&mut request).await;
            assert_eq!(request.execution.request_session_id, None);
        }

        #[tokio::test]
        async fn task_local_session_precedes_the_live_request_source() {
            let service = service_with_metadata();
            let source: RequestSessionIdSource =
                Arc::new(|| Box::pin(async { Some("wrong-live-session".to_owned()) }));
            service.set_request_session_id_source(source);
            let expected = lingxi_core::types::SessionId::new();

            let captured =
                lingxi_core::host::session_flags::scope_request_session_id(expected, async {
                    let mut request = LlmRequest::new("claude-sonnet-4-5");
                    service.capture_request_session_id(&mut request).await;
                    request.execution.request_session_id
                })
                .await;

            assert_eq!(captured, Some(expected.as_uuid().to_string()));
        }
    }

    #[tokio::test]
    async fn headless_admission_preserves_native_effort_context_and_text_display_with_base_override(
    ) {
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
            "../tests/fixtures/native_per_turn_control_2_1_295_print_44.json"
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
                ..Default::default()
            });
        other_model_service
            .messages_create(other_model_request)
            .await
            .expect("different model remains eligible");
        assert!(other_model_transport.seen_headers(0)["anthropic-beta"].contains(mctc));

        // Model support is provider-route scoped as well as canonical-model
        // scoped in this multi-provider runtime. A direct Anthropic refusal
        // must not suppress the same model on Foundry.
        let foundry_transport =
            FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
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
        foundry_request.opts.native_anthropic_request_context =
            Some(NativeAnthropicRequestContext {
                mid_conversation_tool_changes: true,
                ..Default::default()
            });
        foundry_service
            .messages_create(foundry_request)
            .await
            .expect("same canonical model remains eligible on another provider route");
        assert!(foundry_transport.seen_headers(0)["anthropic-beta"].contains(mctc));
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

    async fn run_bedrock_afk_stream_retry(
        model: &str,
        first_error: &str,
        second_error: &str,
    ) -> Arc<FakeStreamTransport> {
        use futures::StreamExt;
        use lingxi_llm_client::NativeAnthropicRequestContext;

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
        assert!(
            !lingxi_core::host::compliance_taints::is_tainted("hipaa"),
            "this fixture requires the initialized-empty compliance taint state"
        );
        let error_response = |message: &str| FakeStreamResp::Status {
            status: 400,
            headers: BTreeMap::new(),
            body_json: Some(serde_json::json!({
                "type": "error",
                "error": {"type": "invalid_request_error", "message": message}
            })),
        };
        let transport = FakeStreamTransport::sequence(vec![
            error_response(first_error),
            error_response(second_error),
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
        request.execution.thinking_recovery_scope = Some(scope);
        request.execution.native_anthropic_request_context = Some(NativeAnthropicRequestContext {
            afk_mode: true,
            ..Default::default()
        });
        request.input.thinking = Some(lingxi_llm_client::protocol::ThinkingConfig {
            mode: Some(lingxi_llm_client::protocol::ThinkingMode::Adaptive),
            ..Default::default()
        });

        let mut stream = service
            .stream_request(request)
            .await
            .expect("the Native AFK/TBC request is admitted");
        while let Some(event) = stream.next().await {
            event.expect("the final retry stream decodes");
        }
        transport
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
            "../tests/fixtures/native_per_turn_control_2_1_295_tty_mainrequest_44.json"
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
            let mctc_route = matches!(input["provider"].as_str(), Some("firstParty" | "foundry"));
            let mctc_model = matches!(model, "claude-sonnet-5-5" | "claude-fable-5-1");
            let afk_route = match input["provider"].as_str() {
                // Native Gyo requires vh() for direct Anthropic, including its
                // experimental-beta gate.
                Some("firstParty") => !experimental_betas_disabled,
                // Native's broader MUn() branch admits Foundry independently
                // of vh()'s experimental-beta gate.
                Some("foundry") => true,
                other => panic!("unexpected AFK provider {other:?}"),
            };
            let classifier_source =
                host_inputs["server_classifier_context"]["payload_json"].as_str();
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
                    // In 2.1.295 V$n() defaults true. These recorded models
                    // pass the Native model capability gate; vh() still
                    // excludes disabled experimental betas and HIPAA routes.
                    mid_conversation_tool_changes: mctc_route
                        && mctc_model
                        && !experimental_betas_disabled,
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
            assert_eq!(
                actual_beta_line.as_deref(),
                Some(expected_beta_line.as_bytes()),
                "row {matrix_row}: exact Native beta header bytes"
            );
        }
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
        let second_response = service
            .messages_create(crate::MessagesCreateRequest::new(
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
            ))
            .await
            .expect("second Foundry main response");
        assert_eq!(
            second_response
                .per_turn_effort()
                .and_then(|effort| effort.string_at("").map(str::to_owned)),
            Some("medium".into())
        );

        let seen = transport.seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        let second_body = request_body_value(&seen[1].body_json);
        assert_eq!(second_body["output_config"]["effort"], "medium");
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
                (4, "system".to_owned(), "medium".to_owned()),
            ],
            "successful response effort stays attached to its historical turn"
        );
    }

    #[test]
    fn main_request_retains_exact_model_source_utf16_units() {
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let service = make_adapter(transport);
        let source =
            lingxi_core::types::utf16_json::Utf16JsonProjection::parse(r#""model-\ud800""#)
                .unwrap();
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
}

#[cfg(test)]
mod tool_result_reminder_suffix_tests {
    use super::*;
    use lingxi_core::types::utf16_json::Utf16JsonProjection;
    use lingxi_core::types::{ContentBlock, ConversationMessage, MessageId, ToolUseId};

    #[test]
    fn only_anthropic_messages_moves_the_typed_reminder_into_its_tool_result() {
        let reminder_id = MessageId::new();
        let target_id = MessageId::new();
        let reminder_text = Utf16JsonProjection::from("<system-reminder>tokens</system-reminder>");
        let make_messages = || {
            vec![
                ConversationMessage::User {
                    id: target_id,
                    content: vec![ContentBlock::ToolResult {
                        tool_use_id: ToolUseId::from("toolu_target"),
                        content: Utf16JsonProjection::from("tool output"),
                        is_error: None,
                        provider_tool_use_id: None,
                        content_blocks: None,
                    }],
                    is_meta: false,
                    is_compact_summary: false,
                    is_visible_in_transcript_only: false,
                    turn_companion: None,
                    usage_limit_note: None,
                    origin: None,
                    turn_origin: None,
                    api_message_override: None,
                },
                ConversationMessage::user_meta(
                    reminder_id,
                    reminder_text.string_at("").unwrap().to_owned(),
                ),
            ]
        };
        let suffix = crate::convert::AnthropicToolResultReminderSuffix {
            tool_use_id: ToolUseId::from("toolu_target"),
            reminder_message_id: reminder_id,
            text: reminder_text.clone(),
        };

        let mut openai_messages = make_messages();
        let original_len = openai_messages.len();
        apply_anthropic_tool_result_reminder_suffixes(
            lingxi_llm_client::protocol::ProtocolFamily::OpenAiChat,
            &mut openai_messages,
            std::slice::from_ref(&suffix),
        );
        assert_eq!(openai_messages.len(), original_len);
        assert_eq!(openai_messages[1].id(), reminder_id);
        let ConversationMessage::User { content, .. } = &openai_messages[0] else {
            panic!("expected original tool result");
        };
        let ContentBlock::ToolResult { content, .. } = &content[0] else {
            panic!("expected tool result block");
        };
        assert_eq!(content.string_at(""), Some("tool output"));

        let mut anthropic_messages = make_messages();
        apply_anthropic_tool_result_reminder_suffixes(
            lingxi_llm_client::protocol::ProtocolFamily::AnthropicMessages,
            &mut anthropic_messages,
            std::slice::from_ref(&suffix),
        );
        assert_eq!(anthropic_messages.len(), 1);
        let ConversationMessage::User { content, .. } = &anthropic_messages[0] else {
            panic!("expected decorated tool result");
        };
        let ContentBlock::ToolResult { content, .. } = &content[0] else {
            panic!("expected decorated tool result block");
        };
        assert_eq!(
            content.string_at(""),
            Some("tool output\n\n<system-reminder>tokens</system-reminder>")
        );
    }
}

#[cfg(test)]
mod subscription_rate_limit_tests {
    /// REGRESSION: a ChatGPT-login 429 burned the full retry ladder.
    ///
    /// The rule "a subscription's 429 is terminal" already existed, but only in
    /// Claude.ai vocabulary (`RetryState::is_subscriber`, fed from a snapshot
    /// whose `subscription_type` is an Anthropic concept). An OpenAI plan can
    /// never set it, so the ChatGPT profile fell through to the API-key path and
    /// retried a quota that resets on the plan's clock, not ours.
    #[test]
    fn a_subscription_profile_fails_fast_while_an_api_key_retries() {
        use super::super::rate_limit_cannot_clear as cannot_clear;
        // The credential that motivated this: ChatGPT OAuth, a plan quota.
        assert!(cannot_clear(Some("openai-chatgpt"), "gpt-6-astra"));
        // The same MODEL on an API key keeps the ladder — the model is not the
        // discriminator, the credential is.
        assert!(!cannot_clear(Some("openai"), "gpt-6-astra"));
        // Unchanged: OpenRouter's free tier, matched on the model suffix.
        assert!(cannot_clear(Some("openrouter"), "z-ai/glm-4.6:free"));
        assert!(!cannot_clear(Some("openrouter"), "z-ai/glm-4.6"));
        // Anthropic is untouched here on purpose: its subscription is already
        // handled by the parity subscriber gate.
        assert!(!cannot_clear(Some("anthropic"), "claude-opus-5"));
        assert!(!cannot_clear(None, "claude-opus-5"));
    }
}
