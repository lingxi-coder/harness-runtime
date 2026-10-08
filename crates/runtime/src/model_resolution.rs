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
    group: String,
    connection_order: u32,
    endpoint: String,
    protocol: String,
    family_defaults: FamilyModelDefaults,
    catalog_aliases: BTreeMap<String, Vec<String>>,
    registered_model_ids: std::collections::BTreeSet<String>,
    model_keys: Vec<String>,
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
                        group: profile.group().to_owned(),
                        connection_order: profile.connection.order,
                        endpoint: profile.base_url.clone(),
                        protocol: format!("{:?}", profile.protocol),
                        family_defaults: family_defaults(profile),
                        catalog_aliases: catalog_aliases(profile),
                        registered_model_ids: profile
                            .models
                            .iter()
                            .map(|model| model.request_model.clone())
                            .collect(),
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
            if is_family_alias(model)
                && self
                    .profiles
                    .get(profile)
                    .is_some_and(|facts| !registered_model(facts, model))
            {
                return Ok((profile.to_owned(), model.to_owned()));
            }
            // The SDK accepts a configured connection name or provider-group
            // selector. A group alias without a catalog entry still needs the
            // host's family default to identify an actual connection route.
            let registered_on_selector = self.profiles.iter().any(|(name, facts)| {
                (name == profile || facts.group == profile) && registered_model(facts, model)
            });
            let lookup_model = if is_family_alias(model) && !registered_on_selector {
                let candidate = self
                    .profiles
                    .iter()
                    .filter(|(_, facts)| facts.group == profile)
                    .filter(|(_, facts)| provides_alias(facts, model))
                    .min_by_key(|(name, facts)| (facts.connection_order, name.as_str()));
                candidate
                    .map(|(name, facts)| alias_default(facts, model, name))
                    .transpose()?
                    .flatten()
                    .unwrap_or(model)
            } else {
                model
            };
            return self
                .runtime
                .resolve_media_route(lookup_model, Some(profile))
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

        if is_family_alias(model)
            && !self
                .profiles
                .values()
                .any(|facts| registered_model(facts, model))
        {
            let groups: std::collections::BTreeSet<String> = self
                .profiles
                .iter()
                .filter(|(_, facts)| provides_alias(facts, model))
                .map(|(_, facts)| facts.group.clone())
                .collect();
            let profiles: Vec<String> = groups.into_iter().collect();
            return match profiles.as_slice() {
                [only] => self.selected_profile(model, Some(only)),
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
        })
    }
}

fn registered_model(facts: &ProfileFacts, model: &str) -> bool {
    let model = lingxi_core::host::effort::trim_js_whitespace(model);
    let bare = model
        .get(..model.len().saturating_sub(4))
        .filter(|_| model.to_ascii_lowercase().ends_with("[1m]"))
        .unwrap_or(model);
    facts.registered_model_ids.iter().any(|registered| {
        registered.eq_ignore_ascii_case(model) || registered.eq_ignore_ascii_case(bare)
    })
}

fn provider_kind(provider: &ProviderId) -> ModelProviderKind {
    match provider {
        ProviderId::AnthropicFirstParty => ModelProviderKind::FirstParty,
        // Native distinguishes `bedrock` from the separate Anthropic AWS
        // service provider. Do not collapse this to `anthropicAws`.
        ProviderId::BedrockClaude => ModelProviderKind::Bedrock,
        ProviderId::VertexClaude => ModelProviderKind::Vertex,
        ProviderId::FoundryClaude => ModelProviderKind::Foundry,
        ProviderId::OpenAI
        | ProviderId::OpenAICompatible { .. }
        | ProviderId::Gemini
        | ProviderId::VertexGemini
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

fn catalog_aliases(profile: &ProviderProfile) -> BTreeMap<String, Vec<String>> {
    let small_fast_override = is_anthropic_profile(profile)
        .then(|| std::env::var("ANTHROPIC_SMALL_FAST_MODEL").ok())
        .flatten();
    catalog_aliases_with_small_fast(profile, small_fast_override)
}

fn catalog_aliases_with_small_fast(
    profile: &ProviderProfile,
    small_fast_override: Option<String>,
) -> BTreeMap<String, Vec<String>> {
    let mut aliases = BTreeMap::<String, Vec<String>>::new();
    for model in &profile.models {
        for alias in &model.aliases {
            let key = lingxi_core::host::effort::trim_js_whitespace(alias).to_lowercase();
            let values = aliases.entry(key).or_default();
            if !values.contains(&model.request_model) {
                values.push(model.request_model.clone());
            }
        }
    }
    if is_anthropic_profile(profile) {
        if let Some(model) = small_fast_override.filter(|model| !model.is_empty()) {
            // This is provider configuration for an identified native route.
            // Consumers still validate the target against this profile's registry.
            aliases.insert("small-fast".into(), vec![model]);
        }
    }
    aliases
}

fn strategy_alias_key(model: &str) -> Option<&'static str> {
    let normalized = lingxi_core::host::effort::trim_js_whitespace(model).to_ascii_lowercase();
    match normalized.strip_suffix("[1m]").unwrap_or(&normalized) {
        "best" => Some("best"),
        "opusplan" => Some("opusplan"),
        _ => None,
    }
}

fn provides_alias(facts: &ProfileFacts, model: &str) -> bool {
    strategy_alias_key(model).is_some_and(|alias| facts.catalog_aliases.contains_key(alias))
        || family_for_alias(model).is_some_and(|family| facts.family_defaults.get(family).is_some())
}

fn alias_default<'a>(
    facts: &'a ProfileFacts,
    model: &str,
    profile: &str,
) -> Result<Option<&'a str>, ModelResolutionError> {
    if let Some(values) =
        strategy_alias_key(model).and_then(|alias| facts.catalog_aliases.get(alias))
    {
        return match values.as_slice() {
            [only] => Ok(Some(only.as_str())),
            _ => Err(ModelResolutionError::RouteUnavailable {
                model: model.into(),
                profile: Some(profile.into()),
                reason: "configured alias matches multiple models".into(),
            }),
        };
    }
    Ok(family_for_alias(model).and_then(|family| facts.family_defaults.get(family)))
}

fn family_override_or_profile(profile: &ProviderProfile, family: &str) -> Option<String> {
    let override_name = match family {
        "opus" => "ANTHROPIC_DEFAULT_OPUS_MODEL",
        "sonnet" => "ANTHROPIC_DEFAULT_SONNET_MODEL",
        "haiku" => "ANTHROPIC_DEFAULT_HAIKU_MODEL",
        "fable" => "ANTHROPIC_DEFAULT_FABLE_MODEL",
        _ => return None,
    };
    let override_value = is_anthropic_profile(profile)
        .then(|| std::env::var(override_name).ok())
        .flatten();
    family_override_or_profile_with(profile, family, override_value)
}

fn family_override_or_profile_with(
    profile: &ProviderProfile,
    family: &str,
    override_value: Option<String>,
) -> Option<String> {
    if is_anthropic_profile(profile) {
        if let Some(value) = override_value {
            // Keep the provider's defined-empty distinction. These overrides
            // are scoped to Anthropic routes; other providers may define their
            // own family aliases explicitly in the configured model catalog.
            return Some(value);
        }
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

    if !is_anthropic_profile(profile) {
        return None;
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

fn is_anthropic_profile(profile: &ProviderProfile) -> bool {
    matches!(
        profile.provider_id,
        ProviderId::AnthropicFirstParty
            | ProviderId::BedrockClaude
            | ProviderId::VertexClaude
            | ProviderId::FoundryClaude
    )
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

#[cfg(test)]
mod tests {
    use super::*;
    use llm_runtime::{
        AuthStrategy, CredentialConfig, ModelProfile, PricingConfig, ProtocolFamily,
    };

    fn profile(provider_id: ProviderId, name: &str, model: &str) -> ProviderProfile {
        let protocol = if provider_id == ProviderId::AnthropicFirstParty {
            ProtocolFamily::AnthropicMessages
        } else {
            ProtocolFamily::OpenAiChat
        };
        ProviderProfile {
            wire_profile: None,
            regions: llm_runtime::Region::all(),
            provider_id,
            profile_name: name.into(),
            base_url: "https://example.invalid".into(),
            protocol,
            auth: AuthStrategy::None,
            credential: CredentialConfig::None,
            models: vec![ModelProfile {
                display_model: model.into(),
                request_model: model.into(),
                billing_model: model.into(),
                aliases: Vec::new(),
                description: None,
                metadata: Default::default(),
                capabilities: Default::default(),
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

    #[test]
    fn small_fast_override_is_scoped_to_native_provider_facts() {
        let native = profile(ProviderId::AnthropicFirstParty, "custom-native-name", "fast-id");
        let aliases = catalog_aliases_with_small_fast(&native, Some("fast-id".into()));
        assert_eq!(aliases.get("small-fast"), Some(&vec!["fast-id".to_string()]));
        assert!(!catalog_aliases_with_small_fast(&native, Some(String::new())).contains_key("small-fast"));
        let foreign = profile(ProviderId::OpenAI, "anthropic", "claude-shaped-id");
        assert!(!catalog_aliases_with_small_fast(&foreign, Some("fast-id".into())).contains_key("small-fast"));
    }

    #[test]
    fn anthropic_overrides_do_not_create_other_provider_family_aliases() {
        for provider in [
            ProviderId::OpenAI,
            ProviderId::AzureOpenAI,
            ProviderId::Gemini,
            ProviderId::VertexGemini,
            ProviderId::OpenAICompatible {
                name: "gateway".into(),
            },
            ProviderId::Custom {
                name: "custom".into(),
            },
        ] {
            let profile = profile(provider, "non-anthropic", "gpt-4o");
            for family in ["opus", "sonnet", "haiku", "fable"] {
                assert_eq!(
                    family_override_or_profile_with(
                        &profile,
                        family,
                        Some(format!("claude-{family}-configured")),
                    ),
                    None,
                    "Anthropic environment defaults cannot introduce {family} on {:?}",
                    profile.provider_id,
                );
            }
        }
    }

    #[test]
    fn configured_non_anthropic_family_alias_wins_over_anthropic_override() {
        let mut profile = profile(ProviderId::OpenAI, "openai", "gpt-4o");
        profile.models[0].aliases.push("sonnet".into());
        assert_eq!(
            family_override_or_profile_with(&profile, "sonnet", Some("claude-sonnet-other".into())),
            Some("gpt-4o".into()),
        );
    }

    #[test]
    fn non_anthropic_catalog_names_do_not_implicitly_create_claude_aliases() {
        let profile = profile(ProviderId::OpenAI, "gateway", "claude-sonnet-4-5");
        assert_eq!(
            family_override_or_profile_with(&profile, "sonnet", None),
            None
        );
    }

    #[test]
    fn anthropic_routes_keep_scoped_and_defined_empty_overrides() {
        for provider in [
            ProviderId::AnthropicFirstParty,
            ProviderId::BedrockClaude,
            ProviderId::VertexClaude,
            ProviderId::FoundryClaude,
        ] {
            let profile = profile(provider, "anthropic-route", "claude-sonnet-4-5");
            assert_eq!(
                family_override_or_profile_with(
                    &profile,
                    "sonnet",
                    Some("configured-sonnet".into())
                ),
                Some("configured-sonnet".into()),
            );
            assert_eq!(
                family_override_or_profile_with(&profile, "sonnet", Some(String::new())),
                Some(String::new()),
            );
        }
    }

    fn resolution_provider(config: ClientConfig) -> RuntimeModelResolutionProvider {
        let runtime = Arc::new(ModelRuntime::from_config(config.clone()).unwrap());
        let mut provider = RuntimeModelResolutionProvider::new(runtime, &config);
        // Use explicit test facts without depending on the process environment.
        for profile in &config.providers {
            provider
                .profiles
                .get_mut(&profile.profile_name)
                .unwrap()
                .family_defaults = FamilyModelDefaults {
                opus: family_override_or_profile_with(profile, "opus", None),
                sonnet: family_override_or_profile_with(profile, "sonnet", None),
                haiku: family_override_or_profile_with(profile, "haiku", None),
                fable: family_override_or_profile_with(profile, "fable", None),
            };
        }
        provider
    }

    #[test]
    fn literal_native_alias_ids_resolve_through_the_registered_runtime_route() {
        for wire_model in ["opus", "sonnet", "haiku", "fable", "best", "opusplan"] {
            let provider = resolution_provider(ClientConfig {
                providers: vec![profile(ProviderId::OpenAI, "custom", wire_model)],
            });
            let parent = ModelResolutionContext::default();
            let qualified = format!("custom/{wire_model}");
            for (model, profile) in [
                (wire_model, None),
                (wire_model, Some("custom")),
                (qualified.as_str(), None),
            ] {
                let selected = agent::model_resolution::resolve_user_model_selection(
                    model, profile, &parent, &provider,
                )
                .unwrap();
                assert_eq!(selected.model, wire_model);
                assert_eq!(selected.model_profile.as_deref(), Some("custom"));
                assert_eq!(
                    selected.model_resolution_context.route.provider,
                    Some(ModelProviderKind::Other)
                );
            }
            assert!(agent::model_resolution::resolve_user_model_selection(
                "missing",
                Some("custom"),
                &parent,
                &provider,
            )
            .is_err());
        }
    }

    #[test]
    fn duplicate_literal_native_alias_ids_require_an_explicit_profile() {
        let provider = resolution_provider(ClientConfig {
            providers: vec![
                profile(ProviderId::OpenAI, "one", "opus"),
                profile(ProviderId::Gemini, "two", "opus"),
            ],
        });
        let parent = ModelResolutionContext::default();
        assert!(matches!(
            agent::model_resolution::resolve_user_model_selection("opus", None, &parent, &provider),
            Err(ModelResolutionError::AmbiguousRoute { .. })
        ));
        for name in ["one", "two"] {
            let selected = agent::model_resolution::resolve_user_model_selection(
                "opus",
                Some(name),
                &parent,
                &provider,
            )
            .unwrap();
            assert_eq!(selected.model, "opus");
            assert_eq!(selected.model_profile.as_deref(), Some(name));
        }
    }

    #[test]
    fn literal_model_id_wins_over_a_conflicting_profile_family_default() {
        let mut configured = profile(ProviderId::OpenAI, "custom", "opus");
        let mut alias_target = configured.models[0].clone();
        alias_target.request_model = "different-model".into();
        alias_target.display_model = "different-model".into();
        alias_target.billing_model = "different-model".into();
        alias_target.aliases.clear();
        configured.models.push(alias_target);
        let mut provider = resolution_provider(ClientConfig {
            providers: vec![configured],
        });
        provider
            .profiles
            .get_mut("custom")
            .unwrap()
            .family_defaults
            .opus = Some("different-model".into());
        let context = provider.context_for_route("opus", Some("custom")).unwrap();
        assert_eq!(
            context.family_defaults.opus.as_deref(),
            Some("different-model")
        );
        assert_eq!(context.route.model, "opus");
        let selected = agent::model_resolution::resolve_user_model_selection(
            "opus",
            Some("custom"),
            &ModelResolutionContext::default(),
            &provider,
        )
        .unwrap();
        assert_eq!(selected.model, "opus");
        assert_eq!(selected.model_profile.as_deref(), Some("custom"));
    }

    #[test]
    fn qualified_main_agent_model_selects_the_target_provider_profile() {
        let provider = resolution_provider(ClientConfig {
            providers: vec![
                profile(
                    ProviderId::AnthropicFirstParty,
                    "anthropic",
                    "claude-opus-4-8",
                ),
                profile(ProviderId::OpenAI, "openai", "gpt-4o"),
            ],
        });
        let parent = provider
            .context_for_route("claude-opus-4-8", Some("anthropic"))
            .unwrap();
        let selected = agent::model_resolution::resolve_user_model_selection(
            "openai/gpt-4o",
            None,
            &parent,
            &provider,
        )
        .unwrap();
        assert_eq!(selected.model, "gpt-4o");
        assert_eq!(selected.model_profile.as_deref(), Some("openai"));
    }

    #[test]
    fn relative_alias_keeps_selected_profile_when_model_exists_in_multiple_profiles() {
        let mut first = profile(ProviderId::AnthropicFirstParty, "selected", "shared-sonnet");
        first.models[0].aliases.push("sonnet".into());
        let mut second = first.clone();
        second.profile_name = "other".into();
        let provider = resolution_provider(ClientConfig {
            providers: vec![first, second],
        });
        let parent = provider
            .context_for_route("shared-sonnet", Some("selected"))
            .unwrap();
        let selected = agent::model_resolution::resolve_user_model_selection(
            "sonnet", None, &parent, &provider,
        )
        .unwrap();
        assert_eq!(selected.model, "shared-sonnet");
        assert_eq!(selected.model_profile.as_deref(), Some("selected"));
    }

    #[test]
    fn explicit_provider_group_selects_concrete_model_connection() {
        let mut first = profile(ProviderId::OpenAI, "openai-primary", "gpt-4o");
        first.connection.group = Some("openai-group".into());
        first.connection.connection_id = Some("primary".into());
        first.connection.order = 1;
        let mut second = first.clone();
        second.profile_name = "openai-backup".into();
        second.connection.connection_id = Some("backup".into());
        second.connection.order = 2;
        let provider = resolution_provider(ClientConfig {
            providers: vec![second, first],
        });
        let parent = provider
            .context_for_route("gpt-4o", Some("openai-backup"))
            .unwrap();
        let selected = agent::model_resolution::resolve_user_model_selection(
            "gpt-4o",
            Some("openai-group"),
            &parent,
            &provider,
        )
        .unwrap();
        assert_eq!(selected.model, "gpt-4o");
        assert_eq!(selected.model_profile.as_deref(), Some("openai-primary"));
    }

    #[test]
    fn explicit_provider_group_resolves_family_default_on_group_head() {
        let mut first = profile(
            ProviderId::AnthropicFirstParty,
            "claude-primary",
            "claude-sonnet-5",
        );
        first.connection.group = Some("claude-group".into());
        first.connection.connection_id = Some("primary".into());
        first.connection.order = 1;
        let mut second = first.clone();
        second.profile_name = "claude-backup".into();
        second.connection.connection_id = Some("backup".into());
        second.connection.order = 2;
        let provider = resolution_provider(ClientConfig {
            providers: vec![second, first],
        });
        let unqualified = provider.context_for_route("sonnet", None).unwrap();
        assert_eq!(unqualified.route.profile.as_deref(), Some("claude-primary"));
        let parent = provider
            .context_for_route("claude-sonnet-5", Some("claude-backup"))
            .unwrap();
        let selected = agent::model_resolution::resolve_user_model_selection(
            "sonnet",
            Some("claude-group"),
            &parent,
            &provider,
        )
        .unwrap();
        assert_eq!(selected.model, "claude-sonnet-5");
        assert_eq!(selected.model_profile.as_deref(), Some("claude-primary"));
    }

    #[test]
    fn configured_best_and_opusplan_aliases_use_the_selected_openai_catalog() {
        for alias in ["best", "opusplan"] {
            let mut openai = profile(ProviderId::OpenAI, "openai-primary", "gpt-4o");
            openai.models[0].aliases.push(alias.into());
            openai.connection.group = Some("openai-group".into());
            openai.connection.connection_id = Some("primary".into());
            let provider = resolution_provider(ClientConfig {
                providers: vec![openai],
            });
            let parent = provider
                .context_for_route("gpt-4o", Some("openai-primary"))
                .unwrap();
            assert_eq!(parent.family_defaults.opus, None);
            assert_eq!(parent.family_defaults.sonnet, None);
            for explicit_profile in [None, Some("openai-primary"), Some("openai-group")] {
                let selected = agent::model_resolution::resolve_user_model_selection(
                    alias,
                    explicit_profile,
                    &parent,
                    &provider,
                )
                .unwrap();
                assert_eq!(selected.model, "gpt-4o");
                assert_eq!(selected.model_profile.as_deref(), Some("openai-primary"));
            }
            let discovered = provider.context_for_route(alias, None).unwrap();
            assert_eq!(discovered.route.profile.as_deref(), Some("openai-primary"));
            let plan_model = agent::resolve_agent_model_with_context(
                &agent::AgentModel::Inherit,
                "gpt-4o",
                permission::PermissionMode::Plan,
                Some(alias),
                &parent,
            )
            .unwrap();
            assert_eq!(plan_model, "gpt-4o");
        }
    }

    #[test]
    fn duplicate_configured_strategy_alias_fails_instead_of_selecting_a_family_default() {
        let mut first = profile(ProviderId::OpenAI, "openai", "gpt-4o");
        first.models[0].aliases.push("best".into());
        first.connection.group = Some("openai-group".into());
        first.connection.connection_id = Some("primary".into());
        let mut second_model = first.models[0].clone();
        second_model.request_model = "gpt-4.1".into();
        second_model.display_model = "gpt-4.1".into();
        second_model.billing_model = "gpt-4.1".into();
        first.models.push(second_model);
        let provider = resolution_provider(ClientConfig {
            providers: vec![first],
        });
        let parent = provider
            .context_for_route("gpt-4o", Some("openai"))
            .unwrap();
        let error =
            agent::model_resolution::resolve_user_model_selection("best", None, &parent, &provider)
                .expect_err("ambiguous configured aliases must be rejected");
        assert!(matches!(
            error,
            ModelResolutionError::RouteUnavailable { model, profile, .. }
                if model == "best" && profile.as_deref() == Some("openai")
        ));
        assert!(provider.context_for_route("best", None).is_err());
    }
}
