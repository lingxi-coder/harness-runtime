//! No embedded catalog copy: provider facts are supplied by the pinned client.
use crate::{
    Capabilities, CredentialConfig, ModelProfile, PricingCatalog, ProviderId, ProviderProfile,
};
use lingxi_core::host::{ModelMetadata, ModelPricing};
use lingxi_llm_client::protocol as wire;

#[derive(Debug, Clone)]
pub struct BuiltinCatalog {
    pub providers: Vec<ProviderProfile>,
    pub pricing: PricingCatalog,
}

pub fn builtin_presets() -> BuiltinCatalog {
    let mut pricing = PricingCatalog::empty();
    let mut providers: Vec<ProviderProfile> = lingxi_llm_client::builtin_providers()
        .expect("pinned provider catalog must parse")
        .into_iter()
        .map(|profile| {
            let provider_id = match profile.protocol {
                wire::ProtocolFamily::AnthropicMessages
                    if profile.provider_id.as_str() == "anthropic" =>
                {
                    ProviderId::AnthropicFirstParty
                }
                wire::ProtocolFamily::OpenAiChat | wire::ProtocolFamily::OpenAiResponses
                    if profile.provider_id.as_str() == "openai" =>
                {
                    ProviderId::OpenAI
                }
                wire::ProtocolFamily::GeminiGenerateContent | wire::ProtocolFamily::GeminiInteractions => ProviderId::Gemini,
                wire::ProtocolFamily::VertexGemini => ProviderId::VertexGemini,
                wire::ProtocolFamily::VertexClaude => ProviderId::VertexClaude,
                wire::ProtocolFamily::BedrockClaude => ProviderId::BedrockClaude,
                wire::ProtocolFamily::FoundryClaude => ProviderId::FoundryClaude,
                wire::ProtocolFamily::AzureOpenAi => ProviderId::AzureOpenAI,
                _ => ProviderId::OpenAICompatible {
                    name: profile.provider_id.as_str().into(),
                },
            };
            let models = profile
                .models
                .iter()
                .filter(|m| {
                    !m.metadata
                        .output_modalities
                        .iter()
                        .any(|modality| modality == "image")
                })
                .map(|model| {
                    let mut metadata: ModelMetadata = serde_json::from_value(
                        serde_json::to_value(&model.metadata).expect("metadata serializes"),
                    )
                    .expect("model metadata contracts agree");
                    let billing = model.billing_mode_on(&profile.pricing);
                    let billing_mode = billing_mode(billing);
                    let rates = model.pricing.as_ref();
                    // The existing UI displays USD only. Non-USD and subscription prices
                    // remain explicitly unknown here; the upstream estimator retains them.
                    let usd = billing == wire::BillingMode::PerToken
                        && rates.is_some_and(|p| p.currency.as_deref().unwrap_or("USD") == "USD");
                    metadata.pricing = Some(ModelPricing {
                        billing_mode,
                        input_per_million: usd
                            .then(|| rates.and_then(|p| p.input_per_million))
                            .flatten(),
                        output_per_million: usd
                            .then(|| rates.and_then(|p| p.output_per_million))
                            .flatten(),
                        cache_read_per_million: usd
                            .then(|| rates.and_then(|p| p.cache_read_per_million))
                            .flatten(),
                        cache_write_per_million: usd
                            .then(|| rates.and_then(|p| p.cache_write_per_million))
                            .flatten(),
                        reasoning_per_million: usd
                            .then(|| rates.and_then(|p| p.reasoning_per_million))
                            .flatten(),
                        source: rates.and_then(|p| p.source.clone()),
                        ..Default::default()
                    });
                    if let Some(rates) = rates.filter(|_| usd) {
                        pricing = std::mem::take(&mut pricing).with_price(
                            provider_id.clone(),
                            &model.billing_model,
                            rates.clone(),
                        );
                    }
                    if let (Some(context_window), Some(max_output)) =
                        (metadata.context_window_tokens, metadata.max_output_tokens)
                    {
                        if context_window > 0 {
                            let limits = crate::model::model_limits::ModelLimits {
                                context_window,
                                max_output_tokens: max_output,
                            };
                            crate::model::model_limits::register(&model.request_model, limits);
                            crate::model::model_limits::register(&model.display_model, limits);
                        }
                    }
                    let support = model.capability_support.unwrap_or_default();
                    let supported = |value| value == wire::CapabilitySupport::Supported;
                    ModelProfile {
                        display_model: model.display_model.clone(),
                        request_model: model.request_model.clone(),
                        billing_model: model.billing_model.clone(),
                        aliases: model.aliases.clone(),
                        description: model.description.clone(),
                        metadata,
                        capabilities: Capabilities {
                            streaming: supported(support.streaming),
                            tools: supported(support.tools),
                            vision: supported(support.vision),
                            documents: supported(support.documents),
                            reasoning: supported(support.reasoning),
                            structured_output: supported(support.structured_output),
                        },
                    }
                })
                .collect();
            ProviderProfile {
                wire_profile: Some(profile.clone()),
                regions: profile.regions.clone(),
                provider_id,
                profile_name: profile.profile_name.clone(),
                base_url: profile.base_url.clone(),
                protocol: serde_json::from_value(serde_json::to_value(profile.protocol).unwrap())
                    .expect("protocol families agree"),
                auth: serde_json::from_value(serde_json::to_value(profile.auth).unwrap())
                    .expect("auth strategies agree"),
                credential: match &profile.credential {
                    wire::CredentialConfig::Env { var } => {
                        CredentialConfig::Env { var: var.clone() }
                    }
                    _ => CredentialConfig::HostManaged {
                        id: profile.profile_name.clone(),
                    },
                },
                models,
                pricing: crate::PricingConfig {
                    // This mode is an observed value from the SDK catalog, not
                    // a user override. Materialize it for host consumers while
                    // preserving the same source in `wire_profile`.
                    billing_mode: Some(billing_mode(profile.pricing.billing_mode)),
                    ..Default::default()
                },
                signing: profile.signing.as_ref().and_then(|s| {
                    Some(crate::SigningConfig {
                        region: s.region.clone()?,
                        service: s.service.clone()?,
                    })
                }),
                azure: profile.azure.as_ref().and_then(|a| {
                    a.api_version.as_ref().map(|v| crate::AzureConfig {
                        api_version: v.clone(),
                    })
                }),
                supports_websockets: profile.supports_websockets,
                supports_websocket_compression: profile.supports_websocket_compression,
                websocket_connect_timeout_ms: profile.websocket_connect_timeout_ms,
                vision_delegate: None,
                connection: serde_json::from_value(
                    serde_json::to_value(&profile.connection).unwrap(),
                )
                .expect("connection policies agree"),
            }
        })
        .filter(|profile| !profile.models.is_empty())
        .collect();
    // Account login is a host-owned connection; model facts still come from upstream.
    if let Some(mut chatgpt) = providers
        .iter()
        .find(|p| p.profile_name == "openai")
        .cloned()
    {
        chatgpt.profile_name = "openai-chatgpt".into();
        chatgpt.provider_id = ProviderId::OpenAICompatible {
            name: chatgpt.profile_name.clone(),
        };
        chatgpt.base_url = "https://chatgpt.com/backend-api/codex".into();
        chatgpt.auth = crate::AuthStrategy::ChatGptOAuth;
        chatgpt.credential = CredentialConfig::HostManaged {
            id: chatgpt.profile_name.clone(),
        };
        chatgpt.supports_websockets = true;
        chatgpt.pricing.billing_mode = Some(lingxi_core::host::ModelBillingMode::Subscription);
        // Served to ChatGPT sign-in, in OpenAI's recommended order (newest
        // first; the picker keeps catalog order). GPT-5.6 stays routable while
        // OpenAI keeps serving it during the GPT-6 rollout.
        const CHATGPT_SIGN_IN_MODELS: &[&str] = &[
            "gpt-6-astra",
            "gpt-6.1-sol",
            "gpt-6-sol",
            "gpt-6-luna",
            "gpt-5.6-sol",
            "gpt-5.6-terra",
            "gpt-5.6-luna",
        ];
        chatgpt
            .models
            .retain(|m| CHATGPT_SIGN_IN_MODELS.contains(&m.request_model.as_str()));
        chatgpt.models.sort_by_key(|m| {
            CHATGPT_SIGN_IN_MODELS
                .iter()
                .position(|id| *id == m.request_model)
        });
        for model in &mut chatgpt.models {
            model.metadata.pricing = Some(ModelPricing {
                billing_mode: lingxi_core::host::ModelBillingMode::Subscription,
                ..Default::default()
            });
        }
        if let Some(source) = &mut chatgpt.wire_profile {
            source.pricing.billing_mode = wire::BillingMode::Subscription;
            for model in &mut source.models {
                model.pricing = None;
                model.billing_mode = Some(wire::BillingMode::Subscription);
            }
        }
        providers.push(chatgpt);
    }
    BuiltinCatalog { providers, pricing }
}

fn billing_mode(mode: wire::BillingMode) -> lingxi_core::host::ModelBillingMode {
    match mode {
        wire::BillingMode::PerToken => lingxi_core::host::ModelBillingMode::PerToken,
        wire::BillingMode::Subscription => lingxi_core::host::ModelBillingMode::Subscription,
        wire::BillingMode::Free => lingxi_core::host::ModelBillingMode::Free,
        wire::BillingMode::Unknown => lingxi_core::host::ModelBillingMode::Unknown,
    }
}
