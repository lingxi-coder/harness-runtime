//! Model registry and route identity resolution.

use crate::{
    reasoning_control_spec, Capabilities, ClientConfig, FailoverTriggers, LlmError,
    PricingModelRef, ProviderId, ReasoningControlSpec, ReasoningTarget,
};
use platform_api::{ModelBillingMode, ModelMetadata, ModelPricing};

/// Model entry exposed to model-picker and listing callers.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelListing {
    /// Explicit provider identity.
    pub provider_id: ProviderId,
    /// Configured provider profile name.
    pub profile_name: String,
    /// Provider GROUP this profile is a connection of. Equals `profile_name`
    /// for a standalone provider, so grouping is always well-defined.
    pub group: String,
    /// This profile's connection id within the group (`"default"` when the
    /// provider declares no connections).
    pub connection_id: String,
    /// Whether this connection is a spare key slot, never offered for selection.
    pub hidden: bool,
    /// Human-facing model label.
    pub display_model: String,
    /// Provider-local model value sent on the wire.
    pub request_model: String,
    /// Model key used by the pricing catalog.
    pub billing_model: String,
    /// Alternate names accepted by registry resolution.
    pub aliases: Vec<String>,
    /// Optional one-line model description (dimmed sub-line in the `/model`
    /// picker); carried through from the configured [`ModelProfile`].
    pub description: Option<String>,
    /// Model capabilities for this listing.
    pub capabilities: Capabilities,
    /// Full provider-specific metadata used by model selectors.
    pub metadata: ModelMetadata,
    /// Exact reasoning controls accepted by this route.
    pub reasoning: ReasoningControlSpec,
    /// Optional Fusion panel/analyst hints.
    pub fusion_hints: Option<platform_api::FusionModelHints>,
    /// Whether THIS ROUTE could serve as the Fusion analyst: the model claims
    /// structured output AND the owning profile's codec can actually encode a
    /// `response_format`. The two are different claims and the weaker one is
    /// not enough — see [`crate::ProtocolFamily::encodes_response_format`].
    pub fusion_analyst_capable: bool,
}

/// Resolved route identity for one requested model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRoute {
    /// Explicit provider identity.
    pub provider_id: ProviderId,
    /// Configured provider profile name.
    pub profile_name: String,
    /// Provider-local model value sent on the wire.
    pub request_model: String,
    /// Human-facing model label.
    pub display_model: String,
    /// Pricing identity for this route.
    pub pricing_model: PricingModelRef,
    /// Model capabilities for this route.
    pub capabilities: Capabilities,
    /// Sibling connections of the SAME provider group that also serve this
    /// model, in configured order, excluding the one chosen above.
    ///
    /// Empty when the caller pinned a single connection, or when the group has
    /// only one. This is the failover order the drive loop walks: every hop is a
    /// different endpoint/credential for the same logical model, so retrying
    /// along it is transparent to the caller.
    pub connection_chain: Vec<ConnectionHop>,
    /// Which failures move along [`Self::connection_chain`]. Empty unless the
    /// provider opted in, so a route with no connections is untouched.
    pub failover: FailoverTriggers,
}

/// One sibling connection to fall over to, already resolved to its profile and
/// wire model id so no second lookup is needed at failover time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionHop {
    /// Profile name of the connection to try.
    pub profile_name: String,
    /// Provider-local model value to send on that connection.
    pub request_model: String,
}

/// Resolved main route plus an optional vision delegate route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaRoute {
    /// Main route selected by the caller.
    pub main: ResolvedRoute,
    /// Vision delegate on the same profile, when the main route lacks vision.
    pub vision_delegate: Option<ResolvedRoute>,
}

/// Registry that resolves requested model names to route identities.
#[derive(Clone)]
pub struct ModelRegistry {
    resolver: std::sync::Arc<lingxi_llm_client::client::RoutingCatalog>,
    config: ClientConfig,
}

impl ModelRegistry {
    pub(crate) fn profile_pricing_config(&self, profile: &str) -> Option<crate::PricingConfig> {
        self.config
            .providers
            .iter()
            .find(|provider| provider.profile_name == profile)
            .map(|provider| provider.pricing.clone())
    }

    fn effective_metadata(
        provider: &crate::ProviderProfile,
        model: &crate::ModelProfile,
    ) -> ModelMetadata {
        let mut metadata = model.metadata.clone();
        if let Some((_, override_pricing)) = provider
            .pricing
            .overrides
            .iter()
            .find(|(id, _)| id == &model.display_model || id == &model.request_model)
        {
            if metadata
                .pricing
                .as_ref()
                .and_then(|pricing| pricing.source.as_deref())
                != Some("userOverride")
            {
                metadata.pricing = Some(ModelPricing {
                    billing_mode: ModelBillingMode::PerToken,
                    input_per_million: Some(override_pricing.input_per_million),
                    output_per_million: Some(override_pricing.output_per_million),
                    cache_read_per_million: Some(override_pricing.cache_read_per_million),
                    cache_write_per_million: Some(override_pricing.cache_write_per_million),
                    reasoning_per_million: Some(override_pricing.reasoning_per_million),
                    tiers: Vec::new(),
                    source: Some("userOverride".to_string()),
                });
            }
        } else if provider.pricing.billing_mode != ModelBillingMode::Unknown {
            let pricing = metadata.pricing.get_or_insert_with(ModelPricing::default);
            pricing.billing_mode = provider.pricing.billing_mode;
        }
        metadata
    }

    /// Build a registry from validated client config.
    pub fn from_config(config: ClientConfig) -> Result<Self, LlmError> {
        if config
            .providers
            .iter()
            .any(|provider| provider.models.is_empty())
        {
            return Err(LlmError::InvalidRequest {
                message: "provider profile must declare at least one model".to_string(),
            });
        }

        for provider in &config.providers {
            if let Some(delegate_model) = provider.vision_delegate.as_deref() {
                let delegate = provider
                    .models
                    .iter()
                    .find(|model| {
                        model.display_model == delegate_model
                            || model.request_model == delegate_model
                            || model.aliases.iter().any(|alias| alias == delegate_model)
                    })
                    .ok_or_else(|| LlmError::InvalidRequest {
                        message: format!(
                            "provider profile '{}' sets visionDelegate to {:?}, but that model is not declared on the same profile",
                            provider.profile_name, delegate_model
                        ),
                    })?;
                if provider.models.len() == 1 {
                    return Err(LlmError::InvalidRequest {
                        message: format!(
                            "provider profile '{}' sets visionDelegate to {:?}, but a profile's only model cannot delegate to itself",
                            provider.profile_name, delegate_model
                        ),
                    });
                }
                if !delegate.capabilities.vision {
                    return Err(LlmError::InvalidRequest {
                        message: format!(
                            "provider profile '{}' sets visionDelegate to {:?}, but that model does not advertise vision capability",
                            provider.profile_name, delegate_model
                        ),
                    });
                }
            }
        }

        let profiles = config
            .providers
            .iter()
            .map(|profile| {
                let mut projected = crate::upstream::profile(profile)?;
                // Availability was selected by the host's region-aware assembler.
                projected.regions = lingxi_llm_client::protocol::Region::all();
                Ok(projected)
            })
            .collect::<Result<Vec<_>, LlmError>>()?;
        let resolver = std::sync::Arc::new(lingxi_llm_client::client::RoutingCatalog::new(
            profiles,
            lingxi_llm_client::protocol::Region::International,
        ));
        Ok(Self { resolver, config })
    }

    /// Return configured profile/model pairs.
    #[must_use]
    pub fn available_models(&self) -> Vec<ModelListing> {
        self.config
            .providers
            .iter()
            // Extra key slots of one connection exist only to be failed over
            // onto; listing them would show the same model several times and
            // let a user "pick" a spare credential.
            .filter(|provider| !provider.connection.hidden)
            .flat_map(|provider| {
                provider.models.iter().map(|model| ModelListing {
                    provider_id: provider.provider_id.clone(),
                    profile_name: provider.profile_name.clone(),
                    group: provider.group().to_string(),
                    connection_id: provider.connection_id().to_string(),
                    hidden: provider.connection.hidden,
                    display_model: model.display_model.clone(),
                    request_model: model.request_model.clone(),
                    billing_model: model.billing_model.clone(),
                    aliases: model.aliases.clone(),
                    description: model.description.clone(),
                    capabilities: model.capabilities,
                    metadata: Self::effective_metadata(provider, model),
                    reasoning: reasoning_control_spec(ReasoningTarget {
                        profile_name: Some(provider.profile_name.as_str()),
                        protocol: &provider.protocol,
                        base_url: &provider.base_url,
                        model: &model.request_model,
                    }),
                    fusion_hints: crate::fusion_hints::hints_for(
                        &provider.profile_name,
                        &model.request_model,
                    ),
                    fusion_analyst_capable: model.capabilities.structured_output
                        && provider.protocol.encodes_response_format(),
                })
            })
            .collect()
    }

    /// Resolve a model id, optionally scoped to one provider profile or group.
    ///
    /// `profile = Some(p)` matches only connections whose profile name OR group
    /// is `p` (absent model or unknown profile → `ModelUnavailable`);
    /// `None` matches across all providers.
    ///
    /// Several matches inside ONE provider group are not ambiguous: they are the
    /// group's connections (a domestic and an international endpoint, or two API
    /// keys), which all serve the same model. The first by
    /// [`ProviderProfile::connection_sort_key`] is chosen and the rest are
    /// returned as [`ResolvedRoute::connection_chain`]. Matches spanning several
    /// groups stay ambiguous — that is a genuine "which provider did you mean".
    pub fn resolve_in(
        &self,
        requested: &str,
        profile: Option<&str>,
    ) -> Result<ResolvedRoute, LlmError> {
        let resolved=self.resolver.resolve_in_prefer_native(requested,profile).map_err(|error|match error {
            lingxi_llm_client::ResolveError::UnknownModel {..}=>LlmError::ModelUnavailable,
            lingxi_llm_client::ResolveError::AmbiguousAcrossGroups {model,groups}=>LlmError::InvalidRequest {message:format!("model reference '{model}' is ambiguous across profiles: {} — qualify it, e.g. {}",groups.join(", "),groups.iter().map(|g|format!("{g}/{model}")).collect::<Vec<_>>().join(", "))},
            other=>LlmError::InvalidRequest {message:other.to_string()},
        })?;
        let provider = self
            .config
            .providers
            .iter()
            .find(|p| p.profile_name == resolved.profile_name)
            .ok_or(LlmError::ModelUnavailable)?;
        let model = provider
            .models
            .iter()
            .find(|m| {
                m.display_model == resolved.display_model
                    && m.request_model == resolved.request_model
            })
            .ok_or(LlmError::ModelUnavailable)?;
        Ok(ResolvedRoute {
            provider_id: provider.provider_id.clone(),
            profile_name: resolved.profile_name,
            request_model: resolved.request_model.clone(),
            display_model: resolved.display_model.clone(),
            pricing_model: PricingModelRef {
                pricing_provider_id: crate::pricing_provider_id_for_profile(
                    &provider.profile_name,
                    &provider.provider_id,
                ),
                billing_model: resolved.pricing_model.billing_model,
                request_model: resolved.request_model,
                display_model: resolved.display_model,
            },
            capabilities: model.capabilities,
            connection_chain: resolved
                .connection_chain
                .into_iter()
                .map(|hop| ConnectionHop {
                    profile_name: hop.profile_name,
                    request_model: hop.request_model,
                })
                .collect(),
            failover: provider.connection.failover,
        })
    }

    /// Resolve across all providers (unscoped). Ambiguous ids error.
    pub fn resolve(&self, requested: &str) -> Result<ResolvedRoute, LlmError> {
        self.resolve_in(requested, None)
    }

    /// Resolve a route and, when needed, its same-profile vision delegate.
    pub fn resolve_media_route_in(
        &self,
        requested: &str,
        profile: Option<&str>,
    ) -> Result<MediaRoute, LlmError> {
        let main = self.resolve_in(requested, profile)?;
        let provider = self
            .config
            .providers
            .iter()
            .find(|provider| provider.profile_name == main.profile_name)
            .ok_or(LlmError::ModelUnavailable)?;
        let vision_delegate = if main.capabilities.vision {
            None
        } else {
            provider
                .vision_delegate
                .as_deref()
                .map(|delegate| self.resolve_in(delegate, Some(provider.profile_name.as_str())))
                .transpose()?
        };
        Ok(MediaRoute {
            main,
            vision_delegate,
        })
    }

    /// Resolve a route and optional same-profile vision delegate unscoped.
    pub fn resolve_media_route(&self, requested: &str) -> Result<MediaRoute, LlmError> {
        self.resolve_media_route_in(requested, None)
    }
}

#[cfg(test)]
mod pricing_policy_tests {
    use super::*;

    #[test]
    fn metered_and_subscription_connections_keep_separate_price_identities() {
        let profiles = crate::builtin_presets()
            .providers
            .into_iter()
            .filter(|profile| matches!(profile.profile_name.as_str(), "zai" | "zai-coding"))
            .collect();
        let registry = ModelRegistry::from_config(crate::ClientConfig {
            providers: profiles,
        })
        .expect("two connection profiles");
        let metered = registry.resolve_in("glm-4.7", Some("zai")).unwrap();
        let subscription = registry.resolve_in("glm-4.7", Some("zai-coding")).unwrap();
        assert_eq!(metered.provider_id, subscription.provider_id);
        assert_ne!(
            metered.pricing_model.pricing_provider_id,
            subscription.pricing_model.pricing_provider_id
        );
    }

    #[test]
    fn profile_pricing_policy_retains_explicit_overrides_without_metadata_inference() {
        let mut provider = crate::builtin_presets()
            .providers
            .into_iter()
            .next()
            .unwrap();
        provider.profile_name = "captured-profile".into();
        provider.pricing.overrides = vec![(
            "billing-only-alias".into(),
            crate::TokenPricing::input_output(3.0, 7.0),
        )];
        let expected = provider.pricing.clone();
        let registry = ModelRegistry::from_config(crate::ClientConfig {
            providers: vec![provider],
        })
        .unwrap();
        let mut captured = registry.profile_pricing_config("captured-profile").unwrap();
        assert_eq!(captured, expected);
        captured.overrides.clear();
        assert_eq!(
            registry.profile_pricing_config("captured-profile"),
            Some(expected)
        );
        assert!(registry.profile_pricing_config("other-profile").is_none());
    }
}

impl std::fmt::Debug for ModelRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelRegistry")
            .field("profiles", &self.config.providers.len())
            .finish_non_exhaustive()
    }
}
