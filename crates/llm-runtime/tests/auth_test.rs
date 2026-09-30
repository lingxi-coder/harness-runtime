//! Regression tests for auth test.

use llm_runtime::{
    Credential, CredentialProvider, CredentialScope, EnvCredentialProvider, ProviderId,
    StaticCredentialProvider,
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

fn authenticate(
    auth: &str,
    protocol: &str,
    extra: serde_json::Value,
    token: &str,
    headers: Vec<(String, String)>,
) -> lingxi_llm_client::HttpRequest {
    use lingxi_llm_client::auth::{apply_credential, ClientIdentity, CredentialRef};
    let profile = serde_json::from_value(serde_json::json!({
        "provider_id":"test", "profile_name":"test", "protocol":protocol,
        "auth":auth, "base_url":"https://example.test", "models":[], "extra":extra
    }))
    .unwrap();
    let mut request = lingxi_llm_client::HttpRequest {
        method: "POST".into(),
        url: "https://example.test/messages".into(),
        headers,
        body: br#"{"model":"claude"}"#.to_vec().into(),
        timeout: None,
    };
    apply_credential(
        &mut request,
        &profile,
        CredentialRef::Token(token),
        ClientIdentity {
            user_agent: "test",
            editor_version: "test/1",
            plugin_version: "test/1",
        },
        std::time::SystemTime::UNIX_EPOCH,
    )
    .unwrap();
    request
}

#[test]
fn api_key_authentication_preserves_provider_body() {
    let signed = authenticate(
        "api_key",
        "anthropic_messages",
        serde_json::json!({}),
        "test-key",
        vec![],
    );
    assert!(signed
        .headers
        .iter()
        .any(|(name, value)| name == "x-api-key" && value == "test-key"));
    assert_eq!(&signed.body[..], br#"{"model":"claude"}"#);
}

#[test]
fn api_key_authentication_supports_provider_specific_header_names() {
    let signed = authenticate(
        "api_key",
        "gemini_generate_content",
        serde_json::json!({"credential_header":"x-goog-api-key"}),
        "g-key",
        vec![],
    );
    assert!(signed
        .headers
        .iter()
        .any(|(name, value)| name == "x-goog-api-key" && value == "g-key"));
}

#[test]
fn bearer_authentication_preserves_unrelated_headers() {
    let signed = authenticate(
        "bearer",
        "open_ai_chat",
        serde_json::json!({}),
        "test-token",
        vec![("content-type".into(), "application/json".into())],
    );
    assert!(signed
        .headers
        .iter()
        .any(|(name, value)| name.eq_ignore_ascii_case("authorization")
            && value == "Bearer test-token"));
    assert!(signed
        .headers
        .iter()
        .any(|(name, value)| name == "content-type" && value == "application/json"));
}
