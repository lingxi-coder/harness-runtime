//! Classifier routing reads selected credentials, never model-name heuristics.
use llm_runtime::{Credential, ModelRuntime, StaticCredentialProvider};
use serde_json::json;
use std::sync::Arc;

struct Environment(Vec<(&'static str, Option<std::ffi::OsString>)>);
impl Environment {
    fn isolated() -> Self {
        let keys = [
            "ANTHROPIC_AUTH_TOKEN",
            "_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL",
        ];
        let state = Self(
            keys.into_iter()
                .map(|key| (key, std::env::var_os(key)))
                .collect(),
        );
        for (key, _) in &state.0 {
            std::env::remove_var(key);
        }
        state
    }
}
impl Drop for Environment {
    fn drop(&mut self) {
        for (key, value) in &self.0 {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

fn runtime(auth: &str, endpoint: &str, credential: Credential) -> ModelRuntime {
    let config = serde_json::from_value(json!({"providers":[{
        "provider_id":"anthropic_first_party", "profile_name":"exact-profile",
        "protocol":"anthropic_messages", "base_url":endpoint, "auth":auth,
        "credential":{"type":"host_managed","id":"classifier-test"},
        "models":[{"display_model":"claude-sonnet-4-6","request_model":"claude-sonnet-4-6","billing_model":"claude-sonnet-4-6",
        "capabilities":{"streaming":true,"tools":true,"reasoning":true,"structured_output":false,"vision":false,"documents":false}}]
    }]})).unwrap();
    ModelRuntime::from_config(config)
        .unwrap()
        .with_credential_provider(Arc::new(StaticCredentialProvider::new(credential)))
}

#[tokio::test]
async fn classifier_facts_use_actual_selected_key_shape_and_native_url_host() {
    let _environment = Environment::isolated();
    for (auth, endpoint, credential, native_url, native_key) in [
        (
            "api_key",
            "https://api.anthropic.com",
            Credential::ApiKey("sk-ant-api-test".into()),
            true,
            true,
        ),
        (
            "api_key",
            "https://api.anthropic.com:444",
            Credential::ApiKey("sk-ant-api-test".into()),
            false,
            true,
        ),
        (
            "api_key",
            "https://proxy.example",
            Credential::ApiKey("other-key".into()),
            false,
            false,
        ),
        (
            "bearer",
            "https://api.anthropic.com",
            Credential::BearerToken("sk-ant-api-token".into()),
            true,
            false,
        ),
        (
            "o_auth_bearer",
            "https://api.anthropic.com",
            Credential::AnthropicOAuth {
                access_token: "sk-ant-api-token".into(),
                scopes: vec![],
            },
            true,
            false,
        ),
    ] {
        let facts = runtime(auth, endpoint, credential)
            .classifier_model_route_facts("claude-sonnet-4-6", Some("exact-profile"))
            .await
            .unwrap();
        assert_eq!(facts.provider.as_deref(), Some("firstParty"));
        assert_eq!(facts.native_base_url, native_url);
        assert_eq!(facts.native_api_key, native_key);
    }
    std::env::set_var("ANTHROPIC_AUTH_TOKEN", "token-present");
    std::env::set_var("_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL", "1");
    let facts = runtime(
        "api_key",
        "https://proxy.example",
        Credential::ApiKey("sk-ant-api-test".into()),
    )
    .classifier_model_route_facts("claude-sonnet-4-6", Some("exact-profile"))
    .await
    .unwrap();
    assert!(facts.native_base_url);
    assert!(!facts.native_api_key);
}
