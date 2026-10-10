//! Runtime-owned adapter from the real provider route registry to Agent's
//! value-only model-resolution context.

use std::collections::BTreeMap;
use std::sync::Arc;

use agent::model_resolution::{
    FamilyModelDefaults, ModelProviderKind, ModelResolutionContext, ModelResolutionContextProvider,
    ModelResolutionError, ModelRouteFacts,
};
use llm_runtime::{ClientConfig, ModelRuntime, ProviderId, ProviderProfile};

#[derive(Clone)]
struct ProfileFacts {
    provider: ModelProviderKind,
    endpoint: String,
    protocol: String,
    family_defaults: FamilyModelDefaults,
    model_keys: Vec<String>,
    registered_model_ids: std::collections::BTreeSet<String>,
    catalog_aliases: BTreeMap<String, Vec<String>>,
}

/// Uses the configured `ModelRuntime` route resolver and a per-profile copy of
/// endpoint/protocol/provider facts. `Agent` sees only owned facts through its
/// trait; this host adapter keeps the runtime dependency local.
pub(crate) struct RuntimeModelResolutionProvider {
    runtime: Arc<ModelRuntime>,
    profiles: BTreeMap<String, ProfileFacts>,
}

impl RuntimeModelResolutionProvider {
    pub(crate) fn new(runtime: Arc<ModelRuntime>, config: &ClientConfig) -> Self {
        let profiles = config
            .providers
            .iter()
            .map(|profile| {
                (
                    profile.profile_name.clone(),
                    ProfileFacts {
                        provider: provider_kind(&profile.provider_id),
                        endpoint: profile.base_url.clone(),
                        protocol: format!("{:?}", profile.protocol),
                        family_defaults: family_defaults(profile),
                        registered_model_ids: profile
                            .models
                            .iter()
                            .map(|model| model.request_model.clone())
                            .collect(),
                        catalog_aliases: {
                            let mut aliases: BTreeMap<String, Vec<String>> = BTreeMap::new();
                            for model in &profile.models {
                                for alias in std::iter::once(&model.display_model)
                                    .chain(model.aliases.iter())
                                {
                                    if alias != &model.request_model {
                                        let candidates =
                                            aliases.entry(alias.to_lowercase()).or_default();
                                        if !candidates.contains(&model.request_model) {
                                            candidates.push(model.request_model.clone());
                                        }
                                    }
                                }
                            }
                            aliases
                        },
                        model_keys: profile
                            .models
                            .iter()
                            .flat_map(|model| {
                                std::iter::once(model.request_model.clone())
                                    .chain(std::iter::once(model.display_model.clone()))
                                    .chain(model.aliases.iter().cloned())
                            })
                            .collect(),
                    },
                )
            })
            .collect();
        Self { runtime, profiles }
    }

    fn selected_profile(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> Result<(String, String), ModelResolutionError> {
        if let Some(profile) = profile {
            if !self.profiles.contains_key(profile) {
                return Err(ModelResolutionError::RouteUnavailable {
                    model: model.to_owned(),
                    profile: Some(profile.to_owned()),
                    reason: "configured profile was not found".to_string(),
                });
            }
            if is_family_alias(model) {
                return Ok((profile.to_owned(), model.to_owned()));
            }
            return self
                .runtime
                .resolve_media_route(model, Some(profile))
                .map(|route| (route.main.profile_name, route.main.request_model))
                .map_err(|error| ModelResolutionError::RouteUnavailable {
                    model: model.to_owned(),
                    profile: Some(profile.to_owned()),
                    reason: error.to_string(),
                })
                .and_then(|(resolved_profile, request_model)| {
                    if self.profiles.contains_key(&resolved_profile) {
                        Ok((resolved_profile, request_model))
                    } else {
                        Err(ModelResolutionError::RouteUnavailable {
                            model: model.to_owned(),
                            profile: Some(profile.to_owned()),
                            reason: "resolved profile is absent from the host snapshot".into(),
                        })
                    }
                });
        }

        if is_family_alias(model) {
            let family = family_for_alias(model).expect("recognized family alias");
            let profiles: Vec<String> = self
                .profiles
                .iter()
                .filter(|(_, facts)| facts.family_defaults.get(family).is_some())
                .map(|(name, _)| name.clone())
                .collect();
            return match profiles.as_slice() {
                [only] => Ok((only.clone(), model.to_owned())),
                [] => Err(ModelResolutionError::RouteUnavailable {
                    model: model.to_owned(),
                    profile: None,
                    reason: "no configured profile provides this family".to_string(),
                }),
                _ => Err(ModelResolutionError::AmbiguousRoute {
                    model: model.to_owned(),
                    profiles,
                }),
            };
        }

        self.runtime
            .resolve_media_route(model, None)
            .map(|route| (route.main.profile_name, route.main.request_model))
            .map_err(|error| {
                let reason = error.to_string();
                if reason.contains("ambiguous across profiles") {
                    let profiles = self
                        .profiles
                        .iter()
                        .filter(|(_, facts)| {
                            facts
                                .model_keys
                                .iter()
                                .any(|key| key.eq_ignore_ascii_case(model))
                        })
                        .map(|(name, _)| name.clone())
                        .collect();
                    ModelResolutionError::AmbiguousRoute {
                        model: model.to_owned(),
                        profiles,
                    }
                } else {
                    ModelResolutionError::RouteUnavailable {
                        model: model.to_owned(),
                        profile: None,
                        reason,
                    }
                }
            })
    }
}

impl ModelResolutionContextProvider for RuntimeModelResolutionProvider {
    fn context_for_route(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> Result<ModelResolutionContext, ModelResolutionError> {
        let (profile_name, request_model) = self.selected_profile(model, profile)?;
        let facts = self.profiles.get(&profile_name).ok_or_else(|| {
            ModelResolutionError::RouteUnavailable {
                model: model.to_owned(),
                profile: Some(profile_name.clone()),
                reason: "resolved profile is absent from the host snapshot".to_string(),
            }
        })?;
        Ok(ModelResolutionContext {
            route: ModelRouteFacts {
                model: request_model,
                profile: Some(profile_name),
                provider: Some(facts.provider),
                endpoint: Some(facts.endpoint.clone()),
                protocol: Some(facts.protocol.clone()),
            },
            family_defaults: facts.family_defaults.clone(),
            registered_model_ids: facts.registered_model_ids.clone(),
            catalog_aliases: facts.catalog_aliases.clone(),
            // Native's live best/catalog, Fable entitlement, 1M capability,
            // and broader entitlement resolver have no trusted host ingress.
            best_model: None,
            fable_strategy_available: None,
            native_1m: None,
            entitlement_allowed: None,
            disable_1m_context: env_truthy(branding::DISABLE_1M_CONTEXT_ENV),
            disable_legacy_model_remap: env_truthy(branding::DISABLE_LEGACY_MODEL_REMAP_ENV),
        })
    }
}

fn provider_kind(provider: &ProviderId) -> ModelProviderKind {
    match provider {
        ProviderId::AnthropicFirstParty => ModelProviderKind::FirstParty,
        // Native distinguishes `bedrock` from the separate Anthropic AWS
        // service provider. Do not collapse this to `anthropicAws`.
        ProviderId::BedrockClaude => ModelProviderKind::Bedrock,
        ProviderId::VertexClaude | ProviderId::VertexGemini => ModelProviderKind::Vertex,
        ProviderId::FoundryClaude => ModelProviderKind::Foundry,
        ProviderId::OpenAI
        | ProviderId::OpenAICompatible { .. }
        | ProviderId::Gemini
        | ProviderId::AzureOpenAI
        | ProviderId::Custom { .. } => ModelProviderKind::Other,
    }
}

fn family_defaults(profile: &ProviderProfile) -> FamilyModelDefaults {
    FamilyModelDefaults {
        opus: family_override_or_profile(profile, "opus"),
        sonnet: family_override_or_profile(profile, "sonnet"),
        haiku: family_override_or_profile(profile, "haiku"),
        fable: family_override_or_profile(profile, "fable"),
    }
}

fn family_override_or_profile(profile: &ProviderProfile, family: &str) -> Option<String> {
    let override_name = match family {
        "opus" => "ANTHROPIC_DEFAULT_OPUS_MODEL",
        "sonnet" => "ANTHROPIC_DEFAULT_SONNET_MODEL",
        "haiku" => "ANTHROPIC_DEFAULT_HAIKU_MODEL",
        "fable" => "ANTHROPIC_DEFAULT_FABLE_MODEL",
        _ => return None,
    };
    if let Ok(value) = std::env::var(override_name) {
        // Keep Native's defined-empty distinction.
        return Some(value);
    }

    let alias_matches: Vec<&str> = profile
        .models
        .iter()
        .filter(|model| {
            model
                .aliases
                .iter()
                .any(|alias| alias.eq_ignore_ascii_case(family))
        })
        .map(|model| model.request_model.as_str())
        .collect();
    if !alias_matches.is_empty() {
        return unique_value(&alias_matches).map(str::to_owned);
    }

    let provider = provider_kind(&profile.provider_id);
    let baseline = host_family_baseline(provider, family)?;
    let exact = profile
        .models
        .iter()
        .find(|model| model.request_model == baseline)
        .map(|model| model.request_model.clone());
    if exact.is_some() {
        return exact;
    }

    let needle = format!("claude-{family}");
    let family_matches: Vec<&str> = profile
        .models
        .iter()
        .filter(|model| {
            model.request_model.to_ascii_lowercase().contains(&needle)
                || model.display_model.to_ascii_lowercase().contains(&needle)
        })
        .map(|model| model.request_model.as_str())
        .collect();
    unique_value(&family_matches).map(str::to_owned)
}

fn unique_value<'a>(values: &[&'a str]) -> Option<&'a str> {
    let first = *values.first()?;
    values.iter().all(|value| *value == first).then_some(first)
}

fn host_family_baseline(provider: ModelProviderKind, family: &str) -> Option<&'static str> {
    match family {
        "opus" => Some(if provider == ModelProviderKind::Foundry {
            "claude-opus-4-6"
        } else {
            "claude-opus-4-8"
        }),
        "sonnet" => Some(if provider == ModelProviderKind::FirstParty {
            "claude-sonnet-5"
        } else {
            "claude-sonnet-4-5-20250929"
        }),
        "haiku" => Some("claude-haiku-4-5"),
        "fable" if provider == ModelProviderKind::FirstParty => Some("claude-fable-5-1"),
        _ => None,
    }
}

fn family_for_alias(model: &str) -> Option<&'static str> {
    let normalized = model.trim().to_ascii_lowercase();
    let bare = normalized.strip_suffix("[1m]").unwrap_or(&normalized);
    match bare {
        "opus" => Some("opus"),
        "sonnet" | "opusplan" => Some("sonnet"),
        "haiku" => Some("haiku"),
        "fable" => Some("fable"),
        "best" => Some("opus"),
        _ => None,
    }
}

fn is_family_alias(model: &str) -> bool {
    family_for_alias(model).is_some()
}

fn env_truthy(name: &str) -> bool {
    std::env::var(name).is_ok_and(|value| !value.is_empty())
}
