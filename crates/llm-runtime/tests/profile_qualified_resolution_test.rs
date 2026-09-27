//! Profile-qualified resolution: a shared id (gpt-5.2 on openai + github-copilot)
//! resolves by profile; unqualified stays ambiguous (decided behaviour).
use llm_runtime::{builtin_presets, ClientConfig, LlmError, ModelRegistry};

fn registry() -> ModelRegistry {
    ModelRegistry::from_config(ClientConfig {
        providers: builtin_presets().providers,
    })
    .expect("registry")
}

#[test]
fn shared_id_resolves_by_profile() {
    let reg = registry();
    assert_eq!(
        reg.resolve_in("gpt-5.2", Some("openai"))
            .expect("openai")
            .profile_name,
        "openai"
    );
    assert_eq!(
        reg.resolve_in("gpt-5.2", Some("github-copilot"))
            .expect("copilot")
            .profile_name,
        "github-copilot"
    );
}

#[test]
fn unqualified_shared_id_still_ambiguous() {
    match registry().resolve_in("gpt-5.2", None) {
        Err(LlmError::InvalidRequest { message }) => {
            assert!(message.contains("ambiguous"), "got {message}");
            assert!(
                message.contains("openai/gpt-5.2"),
                "error should suggest the qualified form, got {message}"
            );
        }
        other => panic!("expected ambiguous error, got {other:?}"),
    }
}

#[test]
fn qualified_absent_model_is_unavailable() {
    assert!(matches!(
        registry().resolve_in("gpt-5.2", Some("zai")),
        Err(LlmError::ModelUnavailable)
    ));
    assert!(matches!(
        registry().resolve_in("gpt-5.2", Some("nope")),
        Err(LlmError::ModelUnavailable)
    ));
}

#[test]
fn unique_unqualified_still_resolves() {
    assert_eq!(
        registry()
            .resolve_in("gpt-4.1-mini", None)
            .expect("unique")
            .profile_name,
        "openai"
    );
    assert_eq!(
        registry()
            .resolve("gpt-4.1-mini")
            .expect("unique")
            .profile_name,
        "openai"
    );
}

#[derive(Debug)]
struct ApiKeyStub;
impl llm_runtime::CredentialProvider for ApiKeyStub {
    fn load<'a>(
        &'a self,
        _s: &'a llm_runtime::CredentialScope,
    ) -> llm_runtime::BoxFuture<'a, Result<llm_runtime::Credential, LlmError>> {
        Box::pin(async { Ok(llm_runtime::Credential::ApiKey("k".into())) })
    }
}

/// Build a minimal two-profile config for gpt-5.2:
///   - "openai"          -> <https://api.openai.com/v1> (`OpenAiResponses`)
///   - "github-copilot"  -> <https://api.githubcopilot.com> (`OpenAiChat`)
///
/// Both use `HostManaged` credentials so `ApiKeyStub` is consulted.
fn two_profile_client() -> llm_runtime::client::DefaultLlmClient {
    use llm_runtime::{
        AuthStrategy, Capabilities, ClientConfig, CredentialConfig, ModelProfile, PricingConfig,
        ProtocolFamily, ProviderId, ProviderProfile,
    };
    use std::sync::Arc;

    let shared_model = ModelProfile {
        display_model: "gpt-5.2".to_string(),
        request_model: "gpt-5.2".to_string(),
        billing_model: "gpt-5.2".to_string(),
        aliases: vec![],
        description: None,
        metadata: Default::default(),
        capabilities: Capabilities {
            streaming: true,
            tools: true,
            ..Default::default()
        },
    };

    let config = ClientConfig {
        providers: vec![
            ProviderProfile {
                wire_profile: None,
                regions: lingxi_llm_client::protocol::Region::all(),
                provider_id: ProviderId::OpenAI,
                profile_name: "openai".to_string(),
                base_url: "https://api.openai.com/v1".to_string(),
                protocol: ProtocolFamily::OpenAiResponses,
                auth: AuthStrategy::Bearer,
                credential: CredentialConfig::HostManaged {
                    id: "openai".to_string(),
                },
                models: vec![shared_model.clone()],
                pricing: PricingConfig::default(),
                signing: None,
                azure: None,
                supports_websockets: false,
                supports_websocket_compression: false,
                websocket_connect_timeout_ms: None,
                vision_delegate: None,
                connection: Default::default(),
            },
            ProviderProfile {
                wire_profile: None,
                regions: lingxi_llm_client::protocol::Region::all(),
                provider_id: ProviderId::OpenAICompatible {
                    name: "github-copilot".to_string(),
                },
                profile_name: "github-copilot".to_string(),
                base_url: "https://api.githubcopilot.com".to_string(),
                protocol: ProtocolFamily::OpenAiChat,
                auth: AuthStrategy::CopilotBearer,
                credential: CredentialConfig::HostManaged {
                    id: "github-copilot".to_string(),
                },
                models: vec![shared_model],
                pricing: PricingConfig::default(),
                signing: None,
                azure: None,
                supports_websockets: false,
                supports_websocket_compression: false,
                websocket_connect_timeout_ms: None,
                vision_delegate: None,
                connection: Default::default(),
            },
        ],
    };

    llm_runtime::client::DefaultLlmClient::from_config(config)
        .expect("client")
        .with_credential_provider(Arc::new(ApiKeyStub))
}

#[tokio::test]
async fn prepare_routes_shared_id_by_profile() {
    use llm_runtime::LlmRequest;

    let client = two_profile_client();

    let p = client
        .prepare(&LlmRequest::new("gpt-5.2").with_profile("openai"))
        .await
        .expect("openai");
    assert!(
        p.provider_request
            .url
            .starts_with("https://api.openai.com/"),
        "got {}",
        p.provider_request.url
    );

    let p = client
        .prepare(&LlmRequest::new("gpt-5.2").with_profile("github-copilot"))
        .await
        .expect("copilot");
    assert!(
        p.provider_request
            .url
            .starts_with("https://api.githubcopilot.com"),
        "got {}",
        p.provider_request.url
    );
}
