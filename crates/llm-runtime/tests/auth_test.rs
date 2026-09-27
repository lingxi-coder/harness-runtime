use llm_runtime::{
    ApiKeyAuthenticator, Authenticator, BearerAuthenticator, Credential, CredentialProvider,
    CredentialScope, EnvCredentialProvider, ProviderId, ProviderRequest, StaticCredentialProvider,
};

#[tokio::test]
async fn env_credential_provider_loads_secret_for_scope() {
    std::env::set_var("LLM_CLIENT_TEST_API_KEY", "test-key");
    let provider = EnvCredentialProvider::new("LLM_CLIENT_TEST_API_KEY");

    let credential = provider
        .load(&CredentialScope::new(
            ProviderId::AnthropicFirstParty,
            "anthropic",
        ))
        .await
        .expect("credential");

    assert_eq!(credential, Credential::ApiKey("test-key".to_string()));
}

#[tokio::test]
async fn static_credential_debug_is_redacted() {
    let provider =
        StaticCredentialProvider::new(Credential::BearerToken("secret-token".to_string()));

    let debug = format!(
        "{:?}",
        provider
            .load(&CredentialScope::new(ProviderId::OpenAI, "openai"))
            .await
    );

    assert!(debug.contains("[REDACTED]"));
    assert!(!debug.contains("secret-token"));
}

#[test]
fn credential_scope_carries_optional_credential_id() {
    let scope = CredentialScope::new(ProviderId::OpenAI, "openai");
    assert_eq!(scope.credential_id, None);

    let scope = scope.with_credential_id("team-key");
    assert_eq!(scope.credential_id.as_deref(), Some("team-key"));
}

#[test]
fn api_key_authenticator_applies_header_to_provider_request() {
    let request = ProviderRequest::post_json(
        "https://api.anthropic.com/v1/messages",
        serde_json::json!({"model":"claude"}),
    );
    let auth = ApiKeyAuthenticator::new("test-key");

    let signed = auth.apply(request).expect("signed request");

    assert_eq!(
        signed.headers.get("x-api-key"),
        Some(&"test-key".to_string())
    );
    assert_eq!(signed.body_json["model"], "claude");
}

#[test]
fn api_key_authenticator_supports_provider_specific_header_names() {
    let request = ProviderRequest::post_json(
        "https://generativelanguage.googleapis.com/v1beta/models/m:generateContent",
        serde_json::json!({}),
    );
    let auth = ApiKeyAuthenticator::with_header_name("x-goog-api-key", "g-key");

    let signed = auth.apply(request).expect("signed request");

    assert_eq!(
        signed.headers.get("x-goog-api-key"),
        Some(&"g-key".to_string())
    );
}

#[test]
fn bearer_authenticator_applies_authorization_header_without_removing_existing_headers() {
    let mut request = ProviderRequest::post_json(
        "https://api.openai.com/v1/chat/completions",
        serde_json::json!({}),
    );
    request
        .headers
        .insert("content-type".to_string(), "application/json".to_string());
    let auth = BearerAuthenticator::new("test-token");

    let signed = auth.apply(request).expect("signed request");

    assert_eq!(
        signed.headers.get("Authorization"),
        Some(&"Bearer test-token".to_string())
    );
    assert_eq!(
        signed.headers.get("content-type"),
        Some(&"application/json".to_string())
    );
}
