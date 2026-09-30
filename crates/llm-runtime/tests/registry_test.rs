//! Regression tests for registry test.

use llm_runtime::{
    AuthStrategy, Capabilities, ClientConfig, CredentialConfig, ModelProfile, ModelRegistry,
    PricingConfig, ProtocolFamily, ProviderId, ProviderProfile,
};

fn test_config() -> ClientConfig {
    ClientConfig {
        providers: vec![ProviderProfile {
            wire_profile: None,
            regions: lingxi_llm_client::protocol::Region::all(),
            provider_id: ProviderId::OpenAICompatible {
                name: "openrouter".to_string(),
            },
            profile_name: "openrouter".to_string(),
            base_url: "https://openrouter.ai/api/v1".to_string(),
            protocol: ProtocolFamily::OpenAiChat,
            auth: AuthStrategy::Bearer,
            credential: CredentialConfig::Env {
                var: "OPENROUTER_API_KEY".to_string(),
            },
            models: vec![ModelProfile {
                display_model: "Claude via OpenRouter".to_string(),
                request_model: "anthropic/claude-sonnet-4".to_string(),
                billing_model: "claude-sonnet-4".to_string(),
                aliases: vec!["or-sonnet".to_string()],
                description: None,
                metadata: Default::default(),
                capabilities: Capabilities {
                    streaming: true,
                    tools: true,
                    vision: false,
                    documents: false,
                    reasoning: false,
                    structured_output: true,
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
    }
}

#[test]
fn available_models_lists_configured_profile_models_and_aliases() {
    let registry = ModelRegistry::from_config(test_config()).expect("registry");

    let listings = registry.available_models();

    assert_eq!(listings.len(), 1);
    assert_eq!(listings[0].profile_name, "openrouter");
    assert_eq!(listings[0].display_model, "Claude via OpenRouter");
    assert_eq!(listings[0].request_model, "anthropic/claude-sonnet-4");
    assert_eq!(listings[0].billing_model, "claude-sonnet-4");
    assert_eq!(listings[0].aliases, vec!["or-sonnet"]);
    assert!(listings[0].capabilities.streaming);
}

#[test]
fn resolve_rejects_ambiguous_model_references() {
    let mut config = test_config();
    let mut second = config.providers[0].clone();
    second.profile_name = "fallback-gateway".to_string();
    second.models[0].aliases = vec![];
    config.providers.push(second);
    let registry = ModelRegistry::from_config(config).expect("registry");

    let err = registry
        .resolve("anthropic/claude-sonnet-4")
        .expect_err("shared request_model must be ambiguous");
    assert!(matches!(
        err,
        llm_runtime::LlmError::InvalidRequest { message }
            if message.contains("openrouter") && message.contains("fallback-gateway")
    ));

    let route = registry
        .resolve("or-sonnet")
        .expect("unique alias still resolves");
    assert_eq!(route.profile_name, "openrouter");
}

#[test]
fn resolve_uses_alias_without_model_string_provider_guessing() {
    let registry = ModelRegistry::from_config(test_config()).expect("registry");

    let route = registry.resolve("or-sonnet").expect("route");

    assert_eq!(route.profile_name, "openrouter");
    assert_eq!(route.request_model, "anthropic/claude-sonnet-4");
    assert_eq!(route.pricing_model.billing_model, "claude-sonnet-4");
    assert_eq!(
        route.pricing_model.pricing_provider_id,
        ProviderId::OpenAICompatible {
            name: "openrouter".to_string(),
        }
    );
}

#[test]
fn resolve_media_route_uses_same_profile_delegate_for_non_vision_main() {
    let mut config = test_config();
    config.providers[0].models.push(ModelProfile {
        display_model: "Claude Vision".to_string(),
        request_model: "anthropic/claude-sonnet-4-vision".to_string(),
        billing_model: "claude-sonnet-4-vision".to_string(),
        aliases: vec![],
        description: None,
        metadata: Default::default(),
        capabilities: Capabilities {
            streaming: true,
            tools: false,
            vision: true,
            documents: false,
            reasoning: false,
            structured_output: false,
        },
    });
    config.providers[0].vision_delegate = Some("Claude Vision".to_string());
    let registry = ModelRegistry::from_config(config).expect("registry");

    let route = registry
        .resolve_media_route("Claude via OpenRouter")
        .expect("media route");
    assert_eq!(route.main.profile_name, "openrouter");
    assert_eq!(
        route
            .vision_delegate
            .as_ref()
            .map(|route| route.request_model.as_str()),
        Some("anthropic/claude-sonnet-4-vision")
    );
}

#[test]
fn resolve_media_route_skips_delegate_for_native_vision_model() {
    let mut config = test_config();
    config.providers[0].models[0].capabilities.vision = true;
    config.providers[0].models.push(ModelProfile {
        display_model: "Fallback Vision".to_string(),
        request_model: "fallback-vision".to_string(),
        billing_model: "fallback-vision".to_string(),
        aliases: vec![],
        description: None,
        metadata: Default::default(),
        capabilities: Capabilities {
            streaming: true,
            tools: false,
            vision: true,
            documents: false,
            reasoning: false,
            structured_output: false,
        },
    });
    config.providers[0].vision_delegate = Some("Claude via OpenRouter".to_string());
    let registry = ModelRegistry::from_config(config).expect("registry");

    let route = registry
        .resolve_media_route("Claude via OpenRouter")
        .expect("media route");
    assert!(route.vision_delegate.is_none());
}

#[test]
fn invalid_vision_delegate_is_rejected_at_registry_build() {
    let mut config = test_config();
    config.providers[0].vision_delegate = Some("missing-model".to_string());
    let err = ModelRegistry::from_config(config).expect_err("invalid delegate");
    assert!(matches!(
        err,
        llm_runtime::LlmError::InvalidRequest { message }
            if message.contains("visionDelegate")
    ));
}

#[test]
fn non_vision_delegate_is_rejected_at_registry_build() {
    let mut config = test_config();
    config.providers[0].models.push(ModelProfile {
        display_model: "Also Text Only".to_string(),
        request_model: "text-only".to_string(),
        billing_model: "text-only".to_string(),
        aliases: vec![],
        description: None,
        metadata: Default::default(),
        capabilities: Capabilities {
            streaming: true,
            tools: false,
            vision: false,
            documents: false,
            reasoning: false,
            structured_output: false,
        },
    });
    config.providers[0].vision_delegate = Some("Also Text Only".to_string());
    let err = ModelRegistry::from_config(config).expect_err("non-vision delegate");
    assert!(matches!(
        err,
        llm_runtime::LlmError::InvalidRequest { message }
            if message.contains("visionDelegate") && message.contains("vision capability")
    ));
}

#[test]
fn vision_capable_self_delegate_is_rejected_at_registry_build() {
    let mut config = test_config();
    config.providers[0].models[0].capabilities.vision = true;
    config.providers[0].vision_delegate = Some("Claude via OpenRouter".to_string());
    let err = ModelRegistry::from_config(config).expect_err("vision-capable self delegate");
    assert!(matches!(
        err,
        llm_runtime::LlmError::InvalidRequest { message }
            if message.contains("visionDelegate") && message.contains("cannot delegate to itself")
    ));
}
