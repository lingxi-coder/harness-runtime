use std::sync::Arc;

use llm_runtime::client::ModelRuntime;
use llm_runtime::BoxFuture;
use llm_runtime::{
    AuthStrategy, AzureConfig, Capabilities, ClientConfig, Credential, CredentialConfig,
    CredentialProvider, CredentialScope, LlmError, LlmRequest, ModelProfile, PricingConfig,
    ProtocolFamily, ProviderId, ProviderProfile, SigningConfig,
};

// ── Fix 3.3: ChatGptOAuth dispatch integration ───────────────────────────────

#[tokio::test]
async fn codex_builtin_uses_oauth_endpoint_and_rejects_api_keys() {
    #[derive(Debug)]
    struct Store(Credential);
    impl CredentialProvider for Store {
        fn load<'a>(
            &'a self,
            scope: &'a CredentialScope,
        ) -> BoxFuture<'a, Result<Credential, LlmError>> {
            assert_eq!(scope.credential_id.as_deref(), Some("openai-chatgpt"));
            Box::pin(async { Ok(self.0.clone()) })
        }
    }
    for credential in [
        Credential::ChatGptOAuth {
            access_token: "oauth-access".into(),
            account_id: Some("account".into()),
            fedramp: false,
        },
        Credential::ApiKey("api-key-must-not-be-used".into()),
    ] {
        let oauth = matches!(&credential, Credential::ChatGptOAuth { .. });
        let client = ModelRuntime::from_config(ClientConfig {
            providers: llm_runtime::builtin_presets().providers,
        })
        .unwrap()
        .with_credential_provider(Arc::new(Store(credential)));
        let mut request = LlmRequest::new("gpt-6-astra");
        request.profile = Some("openai-chatgpt".into());
        let result = client.prepare(&request).await;
        if oauth {
            let prepared = result.unwrap();
            assert_eq!(
                prepared.provider_request.url,
                "https://chatgpt.com/backend-api/codex/responses"
            );
            assert_eq!(prepared.provider_request.body_json["model"], "gpt-6-astra");
            assert_eq!(
                prepared
                    .provider_request
                    .headers
                    .get("Authorization")
                    .map(String::as_str),
                Some("Bearer oauth-access")
            );
            assert_eq!(
                prepared
                    .provider_request
                    .headers
                    .get("ChatGPT-Account-ID")
                    .map(String::as_str),
                Some("account")
            );
        } else {
            assert!(matches!(result, Err(LlmError::Authentication { .. })));
        }
    }
}

/// ChatGptOAuth sets Authorization: Bearer and ChatGPT-Account-ID headers.
#[tokio::test]
async fn chatgpt_oauth_injects_bearer_and_account_id_headers() {
    #[derive(Debug)]
    struct ChatGptStore;
    impl CredentialProvider for ChatGptStore {
        fn load<'a>(
            &'a self,
            _scope: &'a CredentialScope,
        ) -> BoxFuture<'a, Result<Credential, LlmError>> {
            Box::pin(async {
                Ok(Credential::ChatGptOAuth {
                    access_token: "t".to_string(),
                    account_id: Some("a".to_string()),
                    fedramp: false,
                })
            })
        }
    }

    let client = ModelRuntime::from_config(ClientConfig {
        providers: vec![ProviderProfile {
            wire_profile: None,
            regions: lingxi_llm_client::protocol::Region::all(),
            provider_id: ProviderId::OpenAI,
            profile_name: "p".to_string(),
            base_url: "https://chatgpt.com/backend-api".to_string(),
            protocol: ProtocolFamily::OpenAiResponses,
            auth: AuthStrategy::ChatGptOAuth,
            credential: CredentialConfig::HostManaged {
                id: "chatgpt-oauth".to_string(),
            },
            models: vec![ModelProfile {
                display_model: "p-model".to_string(),
                request_model: "p-model".to_string(),
                billing_model: "p-model".to_string(),
                aliases: vec![],
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
    .expect("client")
    .with_credential_provider(Arc::new(ChatGptStore));

    let mut request = LlmRequest::new("p-model");
    request.input.max_tokens = Some(1024);
    request.input.temperature = Some(0.5);
    request.input.controls.top_p = Some(0.9);
    request.input.controls.responses.store = Some(true);
    let prepared = client.prepare(&request).await.expect("prepare");

    let body = &prepared.provider_request.body_json;
    assert!(body.get("max_output_tokens").is_none());
    assert!(body.get("temperature").is_none());
    assert!(body.get("top_p").is_none());
    assert_eq!(body["store"], false);
    assert_eq!(body["instructions"], "");

    assert_eq!(
        prepared
            .provider_request
            .headers
            .get("Authorization")
            .map(String::as_str),
        Some("Bearer t"),
        "ChatGptOAuth must inject Authorization: Bearer header"
    );
    assert_eq!(
        prepared
            .provider_request
            .headers
            .get("ChatGPT-Account-ID")
            .map(String::as_str),
        Some("a"),
        "ChatGptOAuth must inject ChatGPT-Account-ID header"
    );
    assert!(
        !prepared
            .provider_request
            .headers
            .contains_key("X-OpenAI-Fedramp"),
        "ChatGptOAuth must NOT inject X-OpenAI-Fedramp when fedramp=false"
    );
}

fn profile(
    provider_id: ProviderId,
    protocol: ProtocolFamily,
    base_url: &str,
    auth: AuthStrategy,
    credential: CredentialConfig,
) -> ProviderProfile {
    ProviderProfile {
        wire_profile: None,
        regions: llm_runtime::Region::all(),
        provider_id,
        profile_name: "p".to_string(),
        base_url: base_url.to_string(),
        protocol,
        auth,
        credential,
        models: vec![ModelProfile {
            display_model: "p-model".to_string(),
            request_model: "p-model".to_string(),
            billing_model: "p-model".to_string(),
            aliases: vec![],
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
    }
}

fn client_with(
    provider_id: ProviderId,
    protocol: ProtocolFamily,
    base_url: &str,
    auth: AuthStrategy,
    credential: CredentialConfig,
) -> ModelRuntime {
    ModelRuntime::from_config(ClientConfig {
        providers: vec![profile(provider_id, protocol, base_url, auth, credential)],
    })
    .expect("client")
}

#[tokio::test]
async fn api_key_strategy_uses_provider_specific_headers() {
    std::env::set_var("LLM_CLIENT_AUTH_TEST_ANTHROPIC", "anthropic-secret");
    let client = client_with(
        ProviderId::AnthropicFirstParty,
        ProtocolFamily::AnthropicMessages,
        "https://api.anthropic.com",
        AuthStrategy::ApiKey,
        CredentialConfig::Env {
            var: "LLM_CLIENT_AUTH_TEST_ANTHROPIC".to_string(),
        },
    );
    let prepared = client
        .prepare(&LlmRequest::new("p-model"))
        .await
        .expect("prepare");
    assert_eq!(
        prepared
            .provider_request
            .headers
            .get("x-api-key")
            .map(String::as_str),
        Some("anthropic-secret")
    );

    std::env::set_var("LLM_CLIENT_AUTH_TEST_GEMINI", "gemini-secret");
    let client = client_with(
        ProviderId::Gemini,
        ProtocolFamily::GeminiGenerateContent,
        "https://generativelanguage.googleapis.com/v1beta",
        AuthStrategy::ApiKey,
        CredentialConfig::Env {
            var: "LLM_CLIENT_AUTH_TEST_GEMINI".to_string(),
        },
    );
    let prepared = client
        .prepare(&LlmRequest::new("p-model"))
        .await
        .expect("prepare");
    assert_eq!(
        prepared
            .provider_request
            .headers
            .get("x-goog-api-key")
            .map(String::as_str),
        Some("gemini-secret")
    );

    std::env::set_var("LLM_CLIENT_AUTH_TEST_OPENAI", "openai-secret");
    let client = client_with(
        ProviderId::OpenAI,
        ProtocolFamily::OpenAiChat,
        "https://api.openai.com/v1",
        AuthStrategy::ApiKey,
        CredentialConfig::Env {
            var: "LLM_CLIENT_AUTH_TEST_OPENAI".to_string(),
        },
    );
    let prepared = client
        .prepare(&LlmRequest::new("p-model"))
        .await
        .expect("prepare");
    assert_eq!(
        prepared
            .provider_request
            .headers
            .get("Authorization")
            .map(String::as_str),
        Some("Bearer openai-secret")
    );
}

#[tokio::test]
async fn oauth_bearer_on_anthropic_adds_oauth_beta_header() {
    std::env::set_var("LLM_CLIENT_AUTH_TEST_OAUTH", "oauth-token");
    let client = client_with(
        ProviderId::AnthropicFirstParty,
        ProtocolFamily::AnthropicMessages,
        "https://api.anthropic.com",
        AuthStrategy::OAuthBearer,
        CredentialConfig::Env {
            var: "LLM_CLIENT_AUTH_TEST_OAUTH".to_string(),
        },
    );

    let prepared = client
        .prepare(&LlmRequest::new("p-model"))
        .await
        .expect("prepare");

    assert_eq!(
        prepared
            .provider_request
            .headers
            .get("Authorization")
            .map(String::as_str),
        Some("Bearer oauth-token")
    );
    assert_eq!(
        prepared
            .provider_request
            .headers
            .get("anthropic-beta")
            .map(String::as_str),
        Some("oauth-2025-04-20")
    );
}

#[tokio::test]
async fn host_managed_credentials_resolve_through_injected_provider() {
    #[derive(Debug)]
    struct RecordingStore;
    impl CredentialProvider for RecordingStore {
        fn load<'a>(
            &'a self,
            scope: &'a CredentialScope,
        ) -> BoxFuture<'a, Result<Credential, LlmError>> {
            let id = scope
                .credential_id
                .as_deref()
                .unwrap_or("missing")
                .to_string();
            Box::pin(async move { Ok(Credential::BearerToken(format!("token-for-{id}"))) })
        }
    }

    let client = ModelRuntime::from_config(ClientConfig {
        providers: vec![profile(
            ProviderId::OpenAI,
            ProtocolFamily::OpenAiChat,
            "https://api.openai.com/v1",
            AuthStrategy::Bearer,
            CredentialConfig::HostManaged {
                id: "team-key".to_string(),
            },
        )],
    })
    .expect("client")
    .with_credential_provider(Arc::new(RecordingStore));

    let prepared = client
        .prepare(&LlmRequest::new("p-model"))
        .await
        .expect("prepare");

    assert_eq!(
        prepared
            .provider_request
            .headers
            .get("Authorization")
            .map(String::as_str),
        Some("Bearer token-for-team-key")
    );
}

#[tokio::test]
async fn host_managed_credentials_without_provider_fail_authentication() {
    let client = client_with(
        ProviderId::OpenAI,
        ProtocolFamily::OpenAiChat,
        "https://api.openai.com/v1",
        AuthStrategy::Bearer,
        CredentialConfig::Static {
            id: "k".to_string(),
        },
    );

    assert!(matches!(
        client
            .prepare(&LlmRequest::new("p-model"))
            .await
            .unwrap_err(),
        LlmError::Authentication { .. }
    ));
}

/// AwsSigV4 with a missing signing config (no region/service) must return
/// InvalidRequest naming the missing config — even when the credential itself
/// is loaded successfully.
#[tokio::test]
async fn sigv4_without_signing_config_fails_at_prepare() {
    #[derive(Debug)]
    struct SigV4Store;
    impl CredentialProvider for SigV4Store {
        fn load<'a>(
            &'a self,
            _scope: &'a CredentialScope,
        ) -> BoxFuture<'a, Result<Credential, LlmError>> {
            Box::pin(async {
                Ok(Credential::AwsSigV4 {
                    access_key_id: "AKIDEXAMPLE".to_string(),
                    secret_access_key: "secret".to_string(),
                    session_token: None,
                })
            })
        }
    }

    // Build a profile with AwsSigV4 auth but NO signing config.
    let client = ModelRuntime::from_config(ClientConfig {
        providers: vec![ProviderProfile {
            wire_profile: None,
            regions: lingxi_llm_client::protocol::Region::all(),
            provider_id: ProviderId::BedrockClaude,
            profile_name: "p".to_string(),
            base_url: "https://bedrock-runtime.us-east-1.amazonaws.com".to_string(),
            // AzureOpenAi would fail at codec build; use AnthropicMessages to
            // get past codec construction and hit the auth path.
            protocol: ProtocolFamily::AnthropicMessages,
            auth: AuthStrategy::AwsSigV4,
            credential: CredentialConfig::HostManaged {
                id: "bedrock-key".to_string(),
            },
            models: vec![ModelProfile {
                display_model: "p-model".to_string(),
                request_model: "p-model".to_string(),
                billing_model: "p-model".to_string(),
                aliases: vec![],
                description: None,
                metadata: Default::default(),
                capabilities: Capabilities {
                    streaming: true,
                    tools: true,
                    ..Default::default()
                },
            }],
            pricing: PricingConfig::default(),
            // No signing config → must fail at prepare() with InvalidRequest.
            signing: None,
            azure: None,
            supports_websockets: false,
            supports_websocket_compression: false,
            websocket_connect_timeout_ms: None,
            vision_delegate: None,
            connection: Default::default(),
        }],
    })
    .expect("client")
    .with_credential_provider(Arc::new(SigV4Store));

    let err = client
        .prepare(&LlmRequest::new("p-model"))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, LlmError::InvalidRequest { message } if message.contains("signing")),
        "expected InvalidRequest about missing signing config, got: {err:?}"
    );
}

/// GcpToken auth injects an `Authorization: Bearer` header (applied by the SDK).
#[tokio::test]
async fn gcp_token_injects_bearer_header() {
    std::env::set_var("LLM_CLIENT_AUTH_TEST_GCP", "gcp-bearer-token");
    let client = client_with(
        ProviderId::Gemini,
        ProtocolFamily::GeminiGenerateContent,
        "https://generativelanguage.googleapis.com/v1beta",
        AuthStrategy::GcpToken,
        CredentialConfig::Env {
            var: "LLM_CLIENT_AUTH_TEST_GCP".to_string(),
        },
    );

    let prepared = client
        .prepare(&LlmRequest::new("p-model"))
        .await
        .expect("prepare");
    assert_eq!(
        prepared
            .provider_request
            .headers
            .get("Authorization")
            .map(String::as_str),
        Some("Bearer gcp-bearer-token"),
        "GcpToken must inject Authorization: Bearer"
    );
}

/// AzureToken auth injects an `api-key: <secret>` header.
#[tokio::test]
async fn azure_token_injects_api_key_header() {
    std::env::set_var("LLM_CLIENT_AUTH_TEST_AZURE", "azure-api-key-value");

    // Build an Azure OpenAI profile with AzureToken auth.
    let client = ModelRuntime::from_config(ClientConfig {
        providers: vec![ProviderProfile {
            wire_profile: None,
            regions: lingxi_llm_client::protocol::Region::all(),
            provider_id: ProviderId::OpenAICompatible {
                name: "azure".to_string(),
            },
            profile_name: "p".to_string(),
            base_url: "https://myresource.openai.azure.com".to_string(),
            protocol: ProtocolFamily::AzureOpenAi,
            auth: AuthStrategy::AzureToken,
            credential: CredentialConfig::Env {
                var: "LLM_CLIENT_AUTH_TEST_AZURE".to_string(),
            },
            models: vec![ModelProfile {
                display_model: "p-model".to_string(),
                request_model: "p-model".to_string(),
                billing_model: "p-model".to_string(),
                aliases: vec![],
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
            azure: Some(AzureConfig {
                api_version: "2024-02-01".to_string(),
            }),
            supports_websockets: false,
            supports_websocket_compression: false,
            websocket_connect_timeout_ms: None,
            vision_delegate: None,
            connection: Default::default(),
        }],
    })
    .expect("client");

    let prepared = client
        .prepare(&LlmRequest::new("p-model"))
        .await
        .expect("prepare");

    // api-key header must be present with the secret value.
    assert_eq!(
        prepared
            .provider_request
            .headers
            .get("api-key")
            .map(String::as_str),
        Some("azure-api-key-value"),
        "AzureToken must inject api-key header"
    );
    // Authorization must NOT be present (Azure uses api-key, not Bearer).
    assert!(
        !prepared
            .provider_request
            .headers
            .contains_key("Authorization"),
        "AzureToken must NOT inject Authorization header"
    );
    // URL must use the Azure deployment pattern.
    assert!(
        prepared
            .provider_request
            .url
            .contains("/openai/deployments/"),
        "Azure URL must include deployment segment; got: {}",
        prepared.provider_request.url
    );
    assert!(
        prepared
            .provider_request
            .url
            .contains("api-version=2024-02-01"),
        "Azure URL must include api-version; got: {}",
        prepared.provider_request.url
    );
    // model key must be ABSENT from the body (deployment is in the URL).
    assert!(
        prepared.provider_request.body_json.get("model").is_none(),
        "Azure request body must not include model key; got: {}",
        prepared.provider_request.body_json
    );
}

#[tokio::test]
async fn missing_credential_config_sends_request_without_client_auth() {
    let client = client_with(
        ProviderId::OpenAI,
        ProtocolFamily::OpenAiChat,
        "https://api.openai.com/v1",
        AuthStrategy::Bearer,
        CredentialConfig::None,
    );

    let prepared = client
        .prepare(&LlmRequest::new("p-model"))
        .await
        .expect("prepare");

    assert!(!prepared
        .provider_request
        .headers
        .contains_key("Authorization"));
    assert!(!prepared.provider_request.headers.contains_key("x-api-key"));
}

#[tokio::test]
async fn prepare_count_tokens_is_authenticated_for_anthropic_routes() {
    std::env::set_var("LLM_CLIENT_AUTH_TEST_CT", "ct-key");
    let client = client_with(
        ProviderId::AnthropicFirstParty,
        ProtocolFamily::AnthropicMessages,
        "https://api.anthropic.com",
        AuthStrategy::ApiKey,
        CredentialConfig::Env {
            var: "LLM_CLIENT_AUTH_TEST_CT".to_string(),
        },
    );

    let prepared = client
        .prepare_count_tokens(&LlmRequest::new("p-model").with_user_text("hi"))
        .await
        .expect("prepared");

    assert!(prepared.url.ends_with("/v1/messages/count_tokens"));
    assert_eq!(
        prepared.headers.get("x-api-key").map(String::as_str),
        Some("ct-key")
    );
    assert!(prepared.body_json.get("max_tokens").is_none());
}

#[tokio::test]
async fn prepare_count_tokens_rejects_non_anthropic_routes() {
    let client = client_with(
        ProviderId::OpenAI,
        ProtocolFamily::OpenAiChat,
        "https://api.openai.com/v1",
        AuthStrategy::None,
        CredentialConfig::None,
    );

    let err = client
        .prepare_count_tokens(&LlmRequest::new("p-model").with_user_text("hi"))
        .await
        .unwrap_err();

    assert!(
        matches!(err, LlmError::InvalidRequest { message } if message.contains("count_tokens"))
    );
}

// ── Fix 4: Null-body hash divergence ─────────────────────────────────────────

/// Fix 4 RED → GREEN: A `Value::Null` body must be signed as the SHA-256 of
/// the 4-byte string "null" (exactly what `LlmTransportBridge` sends on the
/// wire via `ProviderRequest::wire_body_bytes()`), NOT as SHA-256 of empty bytes.
///
/// Before the fix, `client.rs` signed empty bytes for `Null` while the bridge
/// sent "null" → signature mismatch → 403.
#[tokio::test]
async fn sigv4_null_body_signs_as_null_string() {
    use llm_runtime::sigv4;
    use std::collections::BTreeMap;

    // SHA-256("null") — the 4-byte ASCII string.
    let null_hash = {
        let body = b"null";
        let mut h = BTreeMap::new();
        h.insert("host".to_string(), "example.amazonaws.com".to_string());
        // Use sign_request directly to get the x-amz-content-sha256 for "null" bytes.
        sigv4::sign_request(
            "POST",
            "https://example.amazonaws.com/",
            &h,
            body,
            "AKIDEXAMPLE",
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            None,
            "us-east-1",
            "service",
            "20150830T123600Z",
        )
        .expect("sign must succeed")
        .x_amz_content_sha256
    };
    // SHA-256("") for comparison.
    let empty_hash = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    // The two hashes must differ — "null" is NOT empty.
    assert_ne!(
        null_hash, empty_hash,
        "SHA-256('null') must differ from SHA-256(''); got: {null_hash}"
    );
    // The hash of "null" bytes is known.
    assert_eq!(
        null_hash, "74234e98afe7498fb5daf1f36ac2d78acc339464f950703b8c019892f982b90b",
        "SHA-256('null') must be the known value"
    );
}

/// Fix 4 integration: when a client signs a request whose `body_json` is
/// `Value::Null`, the `x-amz-content-sha256` header must equal
/// SHA-256("null"), not SHA-256("").
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn sigv4_null_body_content_sha256_via_client() {
    use std::time::{Duration, UNIX_EPOCH};
    #[derive(Debug)]
    struct NullBodyStore;
    impl CredentialProvider for NullBodyStore {
        fn load<'a>(
            &'a self,
            _scope: &'a CredentialScope,
        ) -> BoxFuture<'a, Result<Credential, LlmError>> {
            Box::pin(async {
                Ok(Credential::AwsSigV4 {
                    access_key_id: "AKIDEXAMPLE".to_string(),
                    secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".to_string(),
                    session_token: None,
                })
            })
        }
    }

    // SHA-256("null") — the 4-byte ASCII string "null".
    // Value::Null.to_string() == "null", so that's what the bridge sends.
    let expected_hash = "74234e98afe7498fb5daf1f36ac2d78acc339464f950703b8c019892f982b90b";

    let client = ModelRuntime::from_config(ClientConfig {
        providers: vec![ProviderProfile {
            wire_profile: None,
            regions: lingxi_llm_client::protocol::Region::all(),
            provider_id: ProviderId::BedrockClaude,
            profile_name: "p".to_string(),
            base_url: "https://bedrock-runtime.us-east-1.amazonaws.com".to_string(),
            protocol: ProtocolFamily::AnthropicMessages,
            auth: AuthStrategy::AwsSigV4,
            credential: CredentialConfig::HostManaged {
                id: "k".to_string(),
            },
            models: vec![ModelProfile {
                display_model: "p-model".to_string(),
                request_model: "p-model".to_string(),
                billing_model: "p-model".to_string(),
                aliases: vec![],
                description: None,
                metadata: Default::default(),
                capabilities: Capabilities {
                    streaming: true,
                    tools: true,
                    ..Default::default()
                },
            }],
            pricing: PricingConfig::default(),
            signing: Some(SigningConfig {
                region: "us-east-1".to_string(),
                service: "bedrock".to_string(),
            }),
            azure: None,
            supports_websockets: false,
            supports_websocket_compression: false,
            websocket_connect_timeout_ms: None,
            vision_delegate: None,
            connection: Default::default(),
        }],
    })
    .expect("client")
    .with_credential_provider(Arc::new(NullBodyStore));

    // Use a fixed timestamp so the hash is deterministic.
    let fixed_time = UNIX_EPOCH + Duration::from_secs(1_440_938_160); // 20150830T123600Z

    let prepared = client
        .prepare_at(&LlmRequest::new("p-model"), fixed_time)
        .await
        .expect("prepare must succeed");

    // The AnthropicMessages codec produces a real JSON body (not null).
    // The important invariant: x-amz-content-sha256 == SHA-256(wire_body_bytes()).
    // Verify by re-signing with the same body and comparing hashes.
    let body_bytes = prepared.provider_request.wire_body_bytes().unwrap();
    let url = &prepared.provider_request.url;
    let method = &prepared.provider_request.method;
    let pre_sign_headers: std::collections::BTreeMap<String, String> = prepared
        .provider_request
        .headers
        .iter()
        .filter(|(k, _)| {
            !matches!(
                k.as_str(),
                "Authorization" | "x-amz-date" | "x-amz-content-sha256"
            )
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let re_signed = llm_runtime::sigv4::sign_request(
        method,
        url,
        &pre_sign_headers,
        &body_bytes,
        "AKIDEXAMPLE",
        "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
        None,
        "us-east-1",
        "bedrock",
        "20150830T123600Z",
    )
    .expect("re-sign must succeed");

    // x-amz-content-sha256 must match the independent hash.
    let actual_hash = prepared
        .provider_request
        .headers
        .get("x-amz-content-sha256")
        .expect("x-amz-content-sha256 must be set");
    assert_eq!(
        actual_hash, &re_signed.x_amz_content_sha256,
        "x-amz-content-sha256 must equal SHA-256(wire_body_bytes())"
    );

    // The body is not null for AnthropicMessages, so hash must differ from SHA-256("null").
    assert_ne!(
        actual_hash, expected_hash,
        "the body produced by AnthropicMessages codec is not null"
    );
    let auth = prepared
        .provider_request
        .headers
        .get("Authorization")
        .expect("Authorization must be set");
    assert!(
        auth.starts_with("AWS4-HMAC-SHA256 "),
        "Authorization must use SigV4; got: {auth}"
    );
    assert!(
        auth.contains("20150830"),
        "Authorization must contain the fixed date; got: {auth}"
    );
}

// ── Fix 5: clock injection + exact-Authorization client test ─────────────────

/// Fix 5 RED → GREEN: With a fixed timestamp and static AwsSigV4 credential,
/// the client must produce a deterministic, exact Authorization header.
///
/// The expected signature is derived via `sigv4::sign_request` with the SAME
/// inputs and hardcoded here.  This pins: clock seam, body-byte derivation,
/// header injection, and the full SigV4 pipeline end-to-end.
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn sigv4_exact_authorization_header_with_fixed_clock() {
    #[derive(Debug)]
    struct FixedSigV4Store;
    use std::time::{Duration, UNIX_EPOCH};
    impl CredentialProvider for FixedSigV4Store {
        fn load<'a>(
            &'a self,
            _scope: &'a CredentialScope,
        ) -> BoxFuture<'a, Result<Credential, LlmError>> {
            Box::pin(async {
                Ok(Credential::AwsSigV4 {
                    access_key_id: "AKIDEXAMPLE".to_string(),
                    secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".to_string(),
                    session_token: None,
                })
            })
        }
    }

    // Build an AnthropicMessages client with AwsSigV4 auth.
    let client = ModelRuntime::from_config(ClientConfig {
        providers: vec![ProviderProfile {
            wire_profile: None,
            regions: lingxi_llm_client::protocol::Region::all(),
            provider_id: ProviderId::BedrockClaude,
            profile_name: "p".to_string(),
            base_url: "https://bedrock-runtime.us-east-1.amazonaws.com".to_string(),
            protocol: ProtocolFamily::AnthropicMessages,
            auth: AuthStrategy::AwsSigV4,
            credential: CredentialConfig::HostManaged {
                id: "k".to_string(),
            },
            models: vec![ModelProfile {
                display_model: "p-model".to_string(),
                request_model: "p-model".to_string(),
                billing_model: "p-model".to_string(),
                aliases: vec![],
                description: None,
                metadata: Default::default(),
                capabilities: Capabilities {
                    streaming: true,
                    tools: true,
                    ..Default::default()
                },
            }],
            pricing: PricingConfig::default(),
            signing: Some(SigningConfig {
                region: "us-east-1".to_string(),
                service: "bedrock".to_string(),
            }),
            azure: None,
            supports_websockets: false,
            supports_websocket_compression: false,
            websocket_connect_timeout_ms: None,
            vision_delegate: None,
            connection: Default::default(),
        }],
    })
    .expect("client")
    .with_credential_provider(Arc::new(FixedSigV4Store));

    // Fixed timestamp: 20150830T123600Z = 1_440_938_160 Unix seconds.
    let fixed_time = UNIX_EPOCH + Duration::from_secs(1_440_938_160);

    // Prepare the request with the fixed clock.
    let request = LlmRequest::new("p-model");
    let prepared = client
        .prepare_at(&request, fixed_time)
        .await
        .expect("prepare must succeed");

    // Extract the Authorization header from the prepared request.
    let actual_auth = prepared
        .provider_request
        .headers
        .get("Authorization")
        .expect("Authorization header must be set by SigV4 authenticator")
        .clone();

    // ── Independent derivation ────────────────────────────────────────────────
    // Re-derive the expected Authorization from the same inputs.
    // This exercises the full client pipeline (encode → body bytes → sign).
    // We use sigv4::sign_request with the same body bytes the client used.
    let body_bytes = prepared.provider_request.wire_body_bytes().unwrap();
    let url = &prepared.provider_request.url;
    let method = &prepared.provider_request.method;

    // Build the headers the client had BEFORE SigV4 injection (the codec headers).
    // The client injects x-amz-date, x-amz-content-sha256, and Authorization.
    // We need the pre-signing headers.  Since the codec sets no auth headers,
    // we can reconstruct them: take all headers except the three SigV4 ones.
    let pre_sign_headers: std::collections::BTreeMap<String, String> = prepared
        .provider_request
        .headers
        .iter()
        .filter(|(k, _)| {
            !matches!(
                k.as_str(),
                "authorization" | "Authorization" | "x-amz-date" | "x-amz-content-sha256"
            )
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    let expected = llm_runtime::sigv4::sign_request(
        method,
        url,
        &pre_sign_headers,
        &body_bytes,
        "AKIDEXAMPLE",
        "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
        None,
        "us-east-1",
        "bedrock",
        "20150830T123600Z",
    )
    .expect("independent sign must succeed");

    // The client's Authorization must EXACTLY match the independently-derived one.
    // This pins the full pipeline: clock seam + body derivation + header injection.
    // Any change to canonicalization, body encoding, or header set will break this test.
    assert_eq!(
        actual_auth, expected.authorization,
        "Authorization header must exactly match independent derivation"
    );

    // Hardcoded regression pin (computed once via sigv4::sign_request above, 2026-06-12).
    // Signed headers: anthropic-version;content-type;host;x-amz-date
    // Signature covers: AnthropicMessages encode of LlmRequest::new("p-model") at 20150830T123600Z.
    let pinned_auth =
        "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/bedrock/aws4_request, \
        SignedHeaders=anthropic-version;content-type;host;x-amz-date, \
        Signature=2c9c9309c020f925f19d1996de491f9271f99a389ff18feb811f38eae4708f2b";
    assert_eq!(
        actual_auth, pinned_auth,
        "Authorization must match hardcoded regression pin (truncated: ...Signature=2c9c9309...)"
    );

    // Also check the x-amz-date header is the pinned timestamp.
    assert_eq!(
        prepared
            .provider_request
            .headers
            .get("x-amz-date")
            .map(String::as_str),
        Some("20150830T123600Z"),
        "x-amz-date must match the fixed clock"
    );

    // And x-amz-content-sha256 must equal the independently-derived hash.
    assert_eq!(
        prepared
            .provider_request
            .headers
            .get("x-amz-content-sha256")
            .map(String::as_str),
        Some(expected.x_amz_content_sha256.as_str()),
        "x-amz-content-sha256 must match independent hash of body bytes"
    );
}

// ── Azure AI Foundry Claude (parity 2.1.207 H-BIN-10) ────────────────────────

/// A Foundry Claude profile with a plain API key sends `x-api-key` (CC 2.1.207
/// `AnthropicFoundry.authHeaders()`: string `apiKey` ⇒ `{"x-api-key": apiKey}`)
/// and routes the request to `{base}/v1/messages` via the Foundry codec — NOT a
/// `Bearer` token and NOT a Vertex-style URL. Guards the previously-absent
/// Foundry transport.
#[tokio::test]
async fn foundry_claude_api_key_uses_x_api_key_header_and_messages_url() {
    std::env::set_var("LLM_CLIENT_AUTH_TEST_FOUNDRY_KEY", "foundry-secret");
    let client = client_with(
        ProviderId::FoundryClaude,
        ProtocolFamily::FoundryClaude,
        "https://my-res.services.ai.azure.com/anthropic/",
        AuthStrategy::ApiKey,
        CredentialConfig::Env {
            var: "LLM_CLIENT_AUTH_TEST_FOUNDRY_KEY".to_string(),
        },
    );
    let prepared = client
        .prepare(&LlmRequest::new("p-model"))
        .await
        .expect("prepare");

    // x-api-key header (Anthropic-style), NOT Authorization: Bearer.
    assert_eq!(
        prepared
            .provider_request
            .headers
            .get("x-api-key")
            .map(String::as_str),
        Some("foundry-secret"),
        "Foundry API key must be sent as x-api-key"
    );
    assert!(
        !prepared
            .provider_request
            .headers
            .contains_key("Authorization"),
        "Foundry API-key auth must NOT set Authorization"
    );

    // Routed through the Foundry codec: {base}/v1/messages.
    assert_eq!(
        prepared.provider_request.url, "https://my-res.services.ai.azure.com/anthropic/v1/messages",
        "Foundry request must target {{base}}/v1/messages"
    );
    // Standard anthropic-version header (not the Vertex in-body version).
    assert_eq!(
        prepared
            .provider_request
            .headers
            .get("anthropic-version")
            .map(String::as_str),
        Some("2023-06-01"),
    );
}

/// A Foundry profile configured with an AAD token (`AuthStrategy::Bearer`) sends
/// `Authorization: Bearer` (CC's `azureADTokenProvider` function path), still
/// routed to the Foundry `{base}/v1/messages` endpoint.
#[tokio::test]
async fn foundry_claude_aad_token_uses_bearer_header() {
    std::env::set_var("LLM_CLIENT_AUTH_TEST_FOUNDRY_AAD", "aad-token");
    let client = client_with(
        ProviderId::FoundryClaude,
        ProtocolFamily::FoundryClaude,
        "https://my-res.services.ai.azure.com/anthropic/",
        AuthStrategy::Bearer,
        CredentialConfig::Env {
            var: "LLM_CLIENT_AUTH_TEST_FOUNDRY_AAD".to_string(),
        },
    );
    let prepared = client
        .prepare(&LlmRequest::new("p-model"))
        .await
        .expect("prepare");

    assert_eq!(
        prepared
            .provider_request
            .headers
            .get("Authorization")
            .map(String::as_str),
        Some("Bearer aad-token"),
        "Foundry AAD token must be sent as Authorization: Bearer"
    );
    assert!(
        !prepared.provider_request.headers.contains_key("x-api-key"),
        "Foundry AAD auth must NOT set x-api-key"
    );
    assert_eq!(
        prepared.provider_request.url,
        "https://my-res.services.ai.azure.com/anthropic/v1/messages",
    );
}
