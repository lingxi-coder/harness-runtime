//! First-party auth can change between requests without changing the engine.
use super::*;
use crate::{BoxFuture, PromptCacheRequestContext, RequestCredentials};
use lingxi_llm_client::providers::anthropic::system_prompt::{
    CachePolicy, PromptCacheTtlInputs, PromptText, SystemPromptInput,
};

#[derive(Debug)]
struct LiveCredential(Mutex<Credential>);
impl CredentialProvider for LiveCredential {
    fn load<'a>(
        &'a self,
        scope: &'a CredentialScope,
    ) -> BoxFuture<'a, Result<Credential, LlmError>> {
        assert!(scope.selects_anthropic_auth());
        Box::pin(async { Ok(self.0.lock().unwrap().clone()) })
    }
}

fn config(auth: AuthStrategy, base_url: &str) -> ClientConfig {
    serde_json::from_value(serde_json::json!({"providers":[{
        "provider_id":"anthropic_first_party", "profile_name":"anthropic",
        "base_url":base_url, "protocol":"anthropic_messages",
        "auth":auth, "credential":{"type":"host_managed","id": if auth == AuthStrategy::ApiKey {"anthropic-api-key"} else {"anthropic-oauth"}},
        "models":[{"display_model":"claude-sonnet-4-6","request_model":"claude-sonnet-4-6","billing_model":"claude-sonnet-4-6"}]
    }]})).unwrap()
}

fn runtime(auth: AuthStrategy, store: Arc<LiveCredential>) -> ModelRuntime {
    ModelRuntime::from_config(config(auth, "https://api.anthropic.com"))
        .unwrap()
        .with_credential_provider(store)
}

#[tokio::test]
async fn first_party_oauth_snapshot_cannot_be_reused_at_a_custom_endpoint() {
    let official = runtime(
        AuthStrategy::OAuthBearer,
        Arc::new(LiveCredential(Mutex::new(oauth()))),
    );
    let mut in_flight = request();
    in_flight.execution.request_credentials = Some(RequestCredentials::default());
    assert_auth(&official.prepare(&in_flight).await.unwrap(), None);
    let custom = ModelRuntime::from_config(config(AuthStrategy::ApiKey, "https://custom.example"))
        .unwrap()
        .with_credential_provider(Arc::new(crate::StaticCredentialProvider::new(
            Credential::ApiKey("custom-key".into()),
        )));
    assert!(matches!(
        custom.prepare(&in_flight).await,
        Err(LlmError::Authentication { .. })
    ));
    let prepared = custom.prepare(&request()).await.unwrap();
    assert_eq!(
        prepared
            .provider_request
            .headers
            .get("x-api-key")
            .map(String::as_str),
        Some("custom-key")
    );
}

#[tokio::test]
async fn classifier_facts_use_loaded_material_without_a_separate_source_query() {
    #[derive(Debug)]
    struct ChangedSource;
    impl CredentialProvider for ChangedSource {
        fn source<'a>(
            &'a self,
            _: &'a CredentialScope,
        ) -> BoxFuture<'a, Result<crate::CredentialSource, LlmError>> {
            panic!("source may describe the credential before a concurrent rotation");
        }
        fn load<'a>(
            &'a self,
            _: &'a CredentialScope,
        ) -> BoxFuture<'a, Result<Credential, LlmError>> {
            Box::pin(async { Ok(Credential::ApiKey("sk-ant-api03-current-key".into())) })
        }
    }
    let client = ModelRuntime::from_config(config(
        AuthStrategy::OAuthBearer,
        "https://api.anthropic.com",
    ))
    .unwrap()
    .with_credential_provider(Arc::new(ChangedSource));
    let facts = client
        .classifier_model_route_facts("claude-sonnet-4-6", Some("anthropic"))
        .await
        .unwrap();
    assert!(facts.native_api_key);
    assert_eq!(
        client
            .prepare(&request())
            .await
            .unwrap()
            .provider_request
            .headers
            .get("x-api-key")
            .map(String::as_str),
        Some("sk-ant-api03-current-key")
    );
}

fn oauth() -> Credential {
    Credential::AnthropicOAuth {
        access_token: "synthetic-oauth".into(),
        scopes: vec!["user:inference".into()],
    }
}

fn request() -> LlmRequest {
    let mut request = LlmRequest::new("claude-sonnet-4-6").with_user_text("retain this message");
    request.execution.prompt_cache = Some(PromptCacheRequestContext {
        native_system_prefix: None,
        system: Some(SystemPromptInput::source_vector(
            vec![PromptText::from_string("retained system text")],
            None,
            None,
        )),
        policy: CachePolicy {
            query_source: Some("repl_main_thread".into()),
            prompt_cache_ttl_inputs: PromptCacheTtlInputs {
                subscriber_allowlist: Arc::new(|| vec!["repl_main_thread".into()]),
                ..Default::default()
            },
            ..Default::default()
        },
        current_account_epoch: Arc::new(|| 1),
        native_bare_mode: false,
        native_unix_socket: false,
        overage_for_scope: Arc::new(|_, _| false),
        pending_overage: Arc::new(Mutex::new(None)),
    });
    request
}

fn assert_auth(prepared: &PreparedLlmCall, key: Option<&str>) {
    let headers = &prepared.provider_request.headers;
    match key {
        Some(key) => {
            assert_eq!(headers.get("x-api-key").map(String::as_str), Some(key));
            assert!(!headers
                .keys()
                .any(|name| name.eq_ignore_ascii_case("authorization")));
            assert!(!prepared.prompt_cache.as_ref().unwrap().is_subscriber);
        }
        None => {
            assert_eq!(
                headers.get("Authorization").map(String::as_str),
                Some("Bearer synthetic-oauth")
            );
            assert!(!headers
                .keys()
                .any(|name| name.eq_ignore_ascii_case("x-api-key")));
            assert!(headers
                .get("anthropic-beta")
                .unwrap()
                .contains("oauth-2025-04-20"));
            assert!(prepared.prompt_cache.as_ref().unwrap().is_subscriber);
        }
    }
    assert_eq!(
        prepared
            .provider_request
            .body_json
            .string_at("/model")
            .as_deref(),
        Some("claude-sonnet-4-6")
    );
    assert_eq!(
        prepared
            .provider_request
            .body_json
            .string_at("/messages/0/content/0/text")
            .as_deref(),
        Some("retain this message")
    );
    assert!(prepared
        .provider_request
        .body_json
        .string_at("/system/0/text")
        .unwrap()
        .contains("retained system text"));
}

#[tokio::test]
async fn live_anthropic_auth_updates_headers_subscriber_and_body_policy_between_requests() {
    for boot_auth in [AuthStrategy::ApiKey, AuthStrategy::OAuthBearer] {
        let store = Arc::new(LiveCredential(Mutex::new(oauth())));
        let client = runtime(boot_auth, store.clone());
        let mut in_flight = request();
        in_flight.execution.request_credentials = Some(RequestCredentials::default());
        let first = client.prepare(&in_flight).await.unwrap();
        assert_auth(&first, None);

        *store.0.lock().unwrap() = Credential::ApiKey("key-one".into());
        let key_one = client.prepare(&request()).await.unwrap();
        assert_auth(&key_one, Some("key-one"));
        // A retry belonging to the older logical call retains its credential
        // AND subscriber policy, even after a later request uses an API key.
        assert_auth(&client.prepare(&in_flight).await.unwrap(), None);

        *store.0.lock().unwrap() = Credential::ApiKey("key-two".into());
        assert_auth(&client.prepare(&request()).await.unwrap(), Some("key-two"));
        assert_auth(&key_one, Some("key-one"));

        *store.0.lock().unwrap() = oauth();
        assert_auth(&client.prepare(&request()).await.unwrap(), None);
        let count = client.prepare_count_tokens(&request()).await.unwrap();
        assert_eq!(
            count.headers.get("Authorization").map(String::as_str),
            Some("Bearer synthetic-oauth")
        );
    }
}
