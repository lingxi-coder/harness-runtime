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
                        group: profile.group().to_owned(),
                        connection_order: profile.connection.order,
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
            match self.runtime.resolve_media_route(model, Some(profile)) {
                Ok(route) => return self.resolved_route(model, Some(profile), route.main),
                Err(_error) if is_family_alias(model) => {
                    return self.select_family_alias_profile(model, Some(profile))
                }
                Err(error) => return Err(self.route_error(model, Some(profile), error)),
            }
        }

        // A configured literal model id or catalog alias takes precedence over
        // a Native family spelling with the same text. For example, a profile
        // can literally register a model named "opus".
        match self.runtime.resolve_media_route(model, None) {
            Ok(route) => return self.resolved_route(model, None, route.main),
            Err(error) if is_family_alias(model) => {
                if error.to_string().contains("ambiguous across profiles") {
                    return Err(self.route_error(model, None, error));
                }
                return self.select_family_alias_profile(model, None);
            }
            Err(error) => return Err(self.route_error(model, None, error)),
        }
    }

    fn resolved_route(
        &self,
        requested_model: &str,
        requested_profile: Option<&str>,
        route: llm_runtime::ResolvedRoute,
    ) -> Result<(String, String), ModelResolutionError> {
        if self.profiles.contains_key(&route.profile_name) {
            Ok((route.profile_name, route.request_model))
        } else {
            Err(ModelResolutionError::RouteUnavailable {
                model: requested_model.to_owned(),
                profile: requested_profile.map(str::to_owned),
                reason: "resolved profile is absent from the host snapshot".into(),
            })
        }
    }

    fn route_error(
        &self,
        model: &str,
        profile: Option<&str>,
        error: llm_runtime::LlmError,
    ) -> ModelResolutionError {
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
                profile: profile.map(str::to_owned),
                reason,
            }
        }
    }

    fn select_family_alias_profile(
        &self,
        model: &str,
        requested_profile: Option<&str>,
    ) -> Result<(String, String), ModelResolutionError> {
        let family = family_for_alias(model).expect("recognized family alias");
        let candidates: Vec<(&String, &ProfileFacts)> = self
            .profiles
            .iter()
            .filter(|(name, facts)| {
                requested_profile
                    .is_none_or(|profile| name.as_str() == profile || facts.group == profile)
                    && facts.family_defaults.get(family).is_some()
            })
            .collect();

        let candidate_groups: std::collections::BTreeSet<&str> = candidates
            .iter()
            .map(|(_, facts)| facts.group.as_str())
            .collect();
        if let Some(profile) = requested_profile {
            if candidate_groups.is_empty() {
                return Err(ModelResolutionError::RouteUnavailable {
                    model: model.to_owned(),
                    profile: Some(profile.to_owned()),
                    reason: if self.profiles.contains_key(profile)
                        || self.profiles.values().any(|facts| facts.group == profile)
                    {
                        "configured profile does not provide this family".to_owned()
                    } else {
                        "configured profile was not found".to_owned()
                    },
                });
            }
        } else if candidate_groups.is_empty() {
            return Err(ModelResolutionError::RouteUnavailable {
                model: model.to_owned(),
                profile: None,
                reason: "no configured profile provides this family".to_string(),
            });
        }

        if candidate_groups.len() > 1 {
            return Err(ModelResolutionError::AmbiguousRoute {
                model: model.to_owned(),
                profiles: candidates.iter().map(|(name, _)| (*name).clone()).collect(),
            });
        }

        let selected = candidates
            .into_iter()
            .min_by(|(left_name, left), (right_name, right)| {
                left.connection_order
                    .cmp(&right.connection_order)
                    .then_with(|| left_name.cmp(right_name))
            })
            .expect("a matching family route exists");
        Ok((selected.0.clone(), model.to_owned()))
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
    let env_override = family_override_env_name(family).and_then(|name| std::env::var(name).ok());
    family_override_or_profile_with(profile, family, env_override)
}

fn family_override_or_profile_with(
    profile: &ProviderProfile,
    family: &str,
    env_override: Option<String>,
) -> Option<String> {
    family_override_env_name(family)?;
    // Native's getDefaultOpusModel/getDefaultSonnetModel/getDefaultHaikuModel
    // return a defined environment override before consulting route defaults.
    // In particular, an empty-but-defined value stays distinct from no value.
    if let Some(value) = env_override {
        return Some(value);
    }

    let alias_matches: Vec<&str> = profile
        .models
        .iter()
        .filter(|model| {
            std::iter::once(&model.display_model)
                .chain(model.aliases.iter())
                .any(|alias| alias.eq_ignore_ascii_case(family))
        })
        .map(|model| model.request_model.as_str())
        .collect();
    if !alias_matches.is_empty() {
        return unique_value(&alias_matches).map(str::to_owned);
    }

    let provider = provider_kind(&profile.provider_id);
    let baseline = host_family_baseline(provider, family)?;
    profile
        .models
        .iter()
        .find(|model| model.request_model.eq_ignore_ascii_case(baseline))
        .map(|model| model.request_model.clone())
}

fn family_override_env_name(family: &str) -> Option<&'static str> {
    let name = match family {
        "opus" => "ANTHROPIC_DEFAULT_OPUS_MODEL",
        "sonnet" => "ANTHROPIC_DEFAULT_SONNET_MODEL",
        "haiku" => "ANTHROPIC_DEFAULT_HAIKU_MODEL",
        "fable" => "ANTHROPIC_DEFAULT_FABLE_MODEL",
        _ => return None,
    };
    Some(name)
}

fn unique_value<'a>(values: &[&'a str]) -> Option<&'a str> {
    let first = *values.first()?;
    values.iter().all(|value| *value == first).then_some(first)
}

fn host_family_baseline(provider: ModelProviderKind, family: &str) -> Option<&'static str> {
    match family {
        "opus" => match provider {
            ModelProviderKind::FirstParty => Some("claude-opus-5-5"),
            // Native 2.1.295's provider alias table points Bedrock to its
            // provider-qualified cross-region model id and Vertex to the
            // canonical 5.5 id. These are distinct route request ids.
            ModelProviderKind::Bedrock => Some("us.anthropic.claude-opus-5-5"),
            ModelProviderKind::Vertex => Some("claude-opus-5-5"),
            ModelProviderKind::Foundry => Some("claude-opus-4-6"),
            _ => None,
        },
        "sonnet" => match provider {
            ModelProviderKind::FirstParty => Some("claude-sonnet-5"),
            ModelProviderKind::Bedrock | ModelProviderKind::Vertex | ModelProviderKind::Foundry => {
                Some("claude-sonnet-4-5-20250929")
            }
            _ => None,
        },
        "haiku" => match provider {
            ModelProviderKind::FirstParty
            | ModelProviderKind::Bedrock
            | ModelProviderKind::Vertex
            | ModelProviderKind::Foundry => Some("claude-haiku-4-5"),
            _ => None,
        },
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
    fn small_fast_environment_setting_is_not_a_family_alias() {
        assert!(!is_family_alias("small-fast"));
        assert_eq!(family_override_env_name("small-fast"), None);
    }

    #[test]
    fn native_environment_family_override_precedes_route_catalog_on_other_provider() {
        let profile = profile(ProviderId::OpenAI, "openai", "gpt-4o");
        for (family, override_model) in [
            ("opus", "custom-opus"),
            ("sonnet", "custom-sonnet"),
            ("haiku", "custom-haiku"),
            ("fable", "custom-fable"),
        ] {
            assert_eq!(
                family_override_or_profile_with(&profile, family, Some(override_model.into())),
                Some(override_model.into()),
                "Native environment defaults apply before provider route defaults for {family}"
            );
        }
        assert_eq!(
            family_override_or_profile_with(&profile, "opus", Some(String::new())),
            Some(String::new()),
            "a defined empty environment default remains distinct from no override"
        );
    }

    #[test]
    fn native_environment_family_override_precedes_a_configured_other_provider_alias() {
        let mut profile = profile(ProviderId::OpenAI, "openai", "gpt-4o");
        profile.models[0].aliases.push("sonnet".into());
        assert_eq!(
            family_override_or_profile_with(&profile, "sonnet", Some("configured-sonnet".into())),
            Some("configured-sonnet".into()),
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
    fn native_opus_family_defaults_follow_provider_catalog_aliases() {
        let cases = [
            (ModelProviderKind::FirstParty, "claude-opus-5-5"),
            (ModelProviderKind::Bedrock, "us.anthropic.claude-opus-5-5"),
            (ModelProviderKind::Vertex, "claude-opus-5-5"),
            (ModelProviderKind::Foundry, "claude-opus-4-6"),
        ];
        for (provider, expected) in cases {
            assert_eq!(host_family_baseline(provider, "opus"), Some(expected));
        }
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
