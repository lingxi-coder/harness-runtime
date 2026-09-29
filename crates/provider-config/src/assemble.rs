//! Assemble the merged `ClientConfig` + pricing + chains + credential sources
//! from Anthropic state + `settings.providers` + `settings.routing` (spec §5.3).

use llm_runtime::{AuthStrategy, ClientConfig, CredentialConfig, ProviderId, ProviderProfile};

use crate::parse_providers::parse_user_providers;
use crate::parse_routing::parse_routing;
use crate::types::{AssembleInputs, Assembled, ChainEntry, CredentialKind, CredentialSource};

/// Build the Anthropic provider profile from the 3-way auth state.
fn anthropic_profile(inputs: &AssembleInputs) -> (ProviderProfile, Option<CredentialSource>) {
    let (auth, credential, cred_source) = if inputs.anthropic_has_api_key {
        (
            AuthStrategy::ApiKey,
            CredentialConfig::Static {
                id: "anthropic-api-key".to_string(),
            },
            Some(CredentialSource {
                provider_id: ProviderId::AnthropicFirstParty,
                profile_name: "anthropic".to_string(),
                credential_id: "anthropic-api-key".to_string(),
                env_var: Some("ANTHROPIC_API_KEY".to_string()),
                kind: CredentialKind::ApiKey,
            }),
        )
    } else if inputs.anthropic_has_oauth {
        (
            AuthStrategy::OAuthBearer,
            CredentialConfig::Static {
                id: "anthropic-oauth".to_string(),
            },
            Some(CredentialSource {
                provider_id: ProviderId::AnthropicFirstParty,
                profile_name: "anthropic".to_string(),
                credential_id: "anthropic-oauth".to_string(),
                env_var: None,
                kind: CredentialKind::OAuth,
            }),
        )
    } else {
        // Keep the first-party route credential-capable even when the process
        // boots without a key.  `ModelRuntime` resolves the credential on
        // every request, so a bridge that starts unauthenticated can accept a
        // key later in the same process.  Emitting `AuthStrategy::None` here
        // would make the catalog look fixable while permanently bypassing the
        // credential provider for the live route.
        (
            AuthStrategy::ApiKey,
            CredentialConfig::Static {
                id: "anthropic-api-key".to_string(),
            },
            Some(CredentialSource {
                provider_id: ProviderId::AnthropicFirstParty,
                profile_name: "anthropic".to_string(),
                credential_id: "anthropic-api-key".to_string(),
                env_var: Some("ANTHROPIC_API_KEY".to_string()),
                kind: CredentialKind::ApiKey,
            }),
        )
    };

    let mut profile =
        llm_runtime::anthropic_provider_profile(&inputs.anthropic_api_base, auth, credential);
    if let Some(preset) = llm_runtime::builtin_presets()
        .providers
        .into_iter()
        .find(|p| p.profile_name == "anthropic")
    {
        profile.regions = preset.regions;
        profile.wire_profile = preset.wire_profile;
        profile.models = preset.models;
        for model in &inputs.anthropic_models {
            if !profile
                .models
                .iter()
                .any(|existing| existing.request_model == model.request_model)
            {
                profile.models.push(model.clone());
            }
        }
    } else {
        profile.regions = vec![llm_runtime::Region::International];
        profile.models = inputs.anthropic_models.clone();
    }
    (profile, cred_source)
}

/// Locate `(provider_idx, model_idx)` for a `"profile_name/model"` target.
fn locate(providers: &[ProviderProfile], target: &str) -> Option<(usize, usize)> {
    let (prof, model) = target.split_once('/')?;
    for (pi, p) in providers.iter().enumerate() {
        if p.profile_name != prof {
            continue;
        }
        for (mi, m) in p.models.iter().enumerate() {
            if m.request_model == model || m.display_model == model || m.billing_model == model {
                return Some((pi, mi));
            }
        }
    }
    None
}

/// Assemble the full multi-provider config (spec §5.3).
//
// Spec §5.3 fixes this entry point's signature as `assemble(AssembleInputs)`
// (consumed by value; the engine hands over an owned bundle). Borrowing/cloning
// the fields rather than moving them trips `needless_pass_by_value`, so allow it
// here to keep the spec-mandated owned API.
#[must_use]
#[allow(clippy::needless_pass_by_value)]
pub fn assemble(inputs: AssembleInputs) -> Assembled {
    assemble_for_region(inputs, llm_runtime::Region::International)
}

/// Build an execution catalog for one explicit product usage region.
/// Configuration and credentials in other regions remain stored by the host.
pub fn assemble_for_region(inputs: AssembleInputs, region: llm_runtime::Region) -> Assembled {
    let mut warnings = Vec::new();
    let mut providers: Vec<ProviderProfile> = Vec::new();
    let mut credential_sources: Vec<CredentialSource> = Vec::new();

    // 1. Anthropic profile.
    let (anthropic, anthropic_cred) = anthropic_profile(&inputs);
    providers.push(anthropic);
    if let Some(cs) = anthropic_cred {
        credential_sources.push(cs);
    }

    // 2. Built-in presets: rewrite Env -> Static{id = profile_name}.
    let catalog = llm_runtime::builtin_presets();
    for mut preset in catalog.providers {
        if preset.profile_name == "anthropic" {
            continue;
        }
        let env_var = match &preset.credential {
            CredentialConfig::Env { var } => Some(var.clone()),
            _ => None,
        };
        let id = preset.profile_name.clone();
        preset.credential = CredentialConfig::Static { id: id.clone() };
        credential_sources.push(CredentialSource {
            provider_id: preset.provider_id.clone(),
            profile_name: preset.profile_name.clone(),
            credential_id: id,
            env_var,
            kind: CredentialKind::ApiKey,
        });
        providers.push(preset);
    }

    // 3. User providers (only routable ones — must declare >= 1 model).
    let (parsed_users, user_warns) = parse_user_providers(&inputs.user_providers);
    warnings.extend(user_warns);
    for pu in parsed_users {
        if pu.profile.models.is_empty() {
            warnings.push(format!(
                "provider {:?}: declares no models; dropped (not routable — declare \"models\" or a routing alias)",
                pu.profile.profile_name
            ));
            continue;
        }
        let id = pu.profile.profile_name.clone();
        let mut profile = pu.profile;
        let credential_id = match &profile.credential {
            CredentialConfig::Static { id } | CredentialConfig::HostManaged { id } => id.clone(),
            CredentialConfig::Env { var } => var.clone(),
            CredentialConfig::None => {
                profile.credential = CredentialConfig::Static { id: id.clone() };
                id.clone()
            }
        };
        // A settings entry named like a built-in preset REPLACES it. Appending
        // beside it instead produces two profiles with one name, and
        // `ModelRuntime::from_config` rejects a repeated `profile_name` for
        // the WHOLE config — so a single `"deepseek": {…}` in settings.json took
        // every other provider down with it, not just that one.
        let superseded = providers
            .iter()
            .position(|existing| existing.profile_name == profile.profile_name);
        if superseded.is_some() {
            warnings.push(format!(
                "provider {:?}: overrides the built-in profile of the same name",
                profile.profile_name
            ));
            // The replaced profile must not keep claiming a credential slot.
            credential_sources.retain(|source| source.profile_name != profile.profile_name);
        }
        credential_sources.push(CredentialSource {
            provider_id: profile.provider_id.clone(),
            profile_name: profile.profile_name.clone(),
            credential_id,
            env_var: pu.env_var,
            kind: CredentialKind::ApiKey,
        });
        match superseded {
            // Replaced in place: the provider order a picker shows is stable
            // whether or not the user overrode a preset.
            Some(pos) => providers[pos] = profile,
            None => providers.push(profile),
        }
    }

    providers.retain(|profile| profile.regions.contains(&region));

    // 4. Routing: aliases (fold) + fallback (validate).
    let (mut chains, raw_fallback, routing_warns) = parse_routing(inputs.routing.as_ref());
    warnings.extend(routing_warns);

    for (alias, target) in &chains.aliases {
        match locate(&providers, target) {
            Some((pi, mi)) => {
                let aliases = &mut providers[pi].models[mi].aliases;
                if !aliases.iter().any(|a| a == alias) {
                    aliases.push(alias.clone());
                }
            }
            None => warnings.push(format!(
                "routing.aliases[{alias:?}]: target {target:?} matches no registered provider/model; skipped"
            )),
        }
    }

    // 5. Validate fallback chains into ChainEntry lists.
    for (key, targets) in raw_fallback {
        let mut entries: Vec<ChainEntry> = Vec::new();
        for target in &targets {
            match locate(&providers, target) {
                Some((pi, mi)) => entries.push(ChainEntry {
                    provider_id: providers[pi].provider_id.clone(),
                    model: providers[pi].models[mi].request_model.clone(),
                }),
                None => warnings.push(format!(
                    "routing.fallback[{key:?}]: entry {target:?} matches no registered provider/model; skipped"
                )),
            }
        }
        if entries.is_empty() {
            warnings.push(format!(
                "routing.fallback[{key:?}]: no valid entries; chain dropped"
            ));
        } else {
            chains.chains.insert(key, entries);
        }
    }

    // 6. Pricing: reference tiers + a default-unknown row for every non-Anthropic
    //    profile model the reference catalog does not already price.
    let pricing = crate::cost_translate::pricing_for(&providers);

    Assembled {
        client_config: ClientConfig { providers },
        pricing,
        chains,
        credential_sources,
        warnings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_runtime::ModelProfile;
    use serde_json::json;
    use std::collections::BTreeMap;

    #[test]
    fn region_filters_builtin_and_custom_routes_without_losing_saved_configuration() {
        let mut inputs = anthropic_only_inputs();
        inputs.user_providers.insert(
            "mainland-only".into(),
            serde_json::json!({
                "baseUrl":"https://example.test/v1", "type":"openai", "regions":["china_mainland"],
                "models":["custom-model"]
            }),
        );
        let saved = inputs.user_providers.clone();
        for region in [
            llm_runtime::Region::International,
            llm_runtime::Region::ChinaMainland,
        ] {
            let assembled = assemble_for_region(inputs.clone(), region);
            assert!(assembled
                .client_config
                .providers
                .iter()
                .all(|p| p.regions.contains(&region)));
            assert_eq!(
                assembled
                    .client_config
                    .providers
                    .iter()
                    .any(|p| p.profile_name == "openai"),
                region == llm_runtime::Region::International
            );
            assert_eq!(
                assembled
                    .client_config
                    .providers
                    .iter()
                    .any(|p| p.profile_name == "mainland-only"),
                region == llm_runtime::Region::ChinaMainland
            );
        }
        assert_eq!(inputs.user_providers, saved);
    }

    fn anthropic_only_inputs() -> AssembleInputs {
        AssembleInputs {
            anthropic_api_base: "https://api.anthropic.com".to_string(),
            anthropic_models: vec![ModelProfile {
                display_model: "claude-opus-4-6".to_string(),
                request_model: "claude-opus-4-6".to_string(),
                billing_model: "claude-opus-4-6".to_string(),
                aliases: Vec::new(),
                description: None,
                metadata: Default::default(),
                capabilities: llm_runtime::Capabilities::default(),
            }],
            anthropic_has_api_key: true,
            anthropic_has_oauth: false,
            user_providers: BTreeMap::new(),
            routing: None,
        }
    }

    #[test]
    fn merges_anthropic_and_presets() {
        let out = assemble(anthropic_only_inputs());
        // anthropic + 10 built-in presets (incl. Kimi Open Platform and Code).
        assert!(out.client_config.providers.len() > 11);
        let names: Vec<&str> = out
            .client_config
            .providers
            .iter()
            .map(|p| p.profile_name.as_str())
            .collect();
        assert!(names.contains(&"anthropic"));
        assert!(names.contains(&"openrouter"));
        assert!(names.contains(&"deepseek"));
        assert!(names.contains(&"kimi-intl"));
        assert!(names.contains(&"kimi-code"));
        assert!(!names.contains(&"glm-coding"));
        assert!(names.contains(&"zai"));
        assert!(names.contains(&"openai"));
        assert!(names.contains(&"openai-chatgpt"));
        assert!(names.contains(&"github-copilot"));
    }

    #[test]
    fn presets_env_rewritten_to_static_with_profile_name() {
        let out = assemble(anthropic_only_inputs());
        let openrouter = out
            .client_config
            .providers
            .iter()
            .find(|p| p.profile_name == "openrouter")
            .unwrap();
        assert_eq!(
            openrouter.credential,
            CredentialConfig::Static {
                id: "openrouter".to_string()
            }
        );
        let cs = out
            .credential_sources
            .iter()
            .find(|c| c.credential_id == "openrouter")
            .unwrap();
        assert_eq!(cs.profile_name, "openrouter");
        assert_eq!(cs.env_var.as_deref(), Some("OPENROUTER_API_KEY"));
        assert_eq!(cs.kind, CredentialKind::ApiKey);

        let kimi = out
            .client_config
            .providers
            .iter()
            .find(|p| p.profile_name == "kimi-intl")
            .unwrap();
        assert_eq!(kimi.base_url, "https://api.moonshot.ai/v1");
        assert_eq!(kimi.protocol, llm_runtime::ProtocolFamily::OpenAiChat);
        assert_eq!(
            kimi.credential,
            CredentialConfig::Static {
                id: "kimi-intl".to_string()
            }
        );
        let kimi_source = out
            .credential_sources
            .iter()
            .find(|c| c.credential_id == "kimi-intl")
            .unwrap();
        assert_eq!(
            kimi_source.env_var.as_deref(),
            Some("MOONSHOT_INTL_API_KEY")
        );
        assert_eq!(kimi_source.kind, CredentialKind::ApiKey);

        let kimi_code = out
            .client_config
            .providers
            .iter()
            .find(|p| p.profile_name == "kimi-code")
            .unwrap();
        assert_eq!(kimi_code.base_url, "https://api.kimi.com/coding/v1");
        assert_eq!(
            kimi_code.credential,
            CredentialConfig::Static {
                id: "kimi-code".to_string()
            }
        );
        let kimi_code_source = out
            .credential_sources
            .iter()
            .find(|c| c.credential_id == "kimi-code")
            .unwrap();
        assert_eq!(kimi_code_source.env_var.as_deref(), Some("KIMI_API_KEY"));
    }

    #[test]
    fn anthropic_api_key_credential_source() {
        let out = assemble(anthropic_only_inputs());
        let cs = out
            .credential_sources
            .iter()
            .find(|c| c.credential_id == "anthropic-api-key")
            .unwrap();
        assert_eq!(cs.profile_name, "anthropic");
        assert_eq!(cs.env_var.as_deref(), Some("ANTHROPIC_API_KEY"));
        assert_eq!(cs.kind, CredentialKind::ApiKey);
        let anthropic = &out.client_config.providers[0];
        assert_eq!(anthropic.auth, AuthStrategy::ApiKey);
        assert_eq!(
            anthropic.credential,
            CredentialConfig::Static {
                id: "anthropic-api-key".to_string()
            }
        );
    }

    #[test]
    fn anthropic_oauth_path() {
        let mut inp = anthropic_only_inputs();
        inp.anthropic_has_api_key = false;
        inp.anthropic_has_oauth = true;
        let out = assemble(inp);
        let anthropic = &out.client_config.providers[0];
        assert_eq!(anthropic.auth, AuthStrategy::OAuthBearer);
        assert_eq!(
            anthropic.credential,
            CredentialConfig::Static {
                id: "anthropic-oauth".to_string()
            }
        );
        let cs = out
            .credential_sources
            .iter()
            .find(|c| c.credential_id == "anthropic-oauth")
            .unwrap();
        assert_eq!(cs.kind, CredentialKind::OAuth);
        assert!(cs.env_var.is_none());
    }

    #[test]
    fn anthropic_without_boot_credential_keeps_a_live_api_key_route() {
        let mut inp = anthropic_only_inputs();
        inp.anthropic_has_api_key = false;
        inp.anthropic_has_oauth = false;
        let out = assemble(inp);
        let anthropic = &out.client_config.providers[0];
        assert_eq!(anthropic.auth, AuthStrategy::ApiKey);
        assert_eq!(
            anthropic.credential,
            CredentialConfig::Static {
                id: "anthropic-api-key".to_string()
            }
        );
        let source = out
            .credential_sources
            .iter()
            .find(|c| c.credential_id == "anthropic-api-key")
            .expect("cold-start Anthropic route must retain a credential source");
        assert_eq!(source.profile_name, "anthropic");
        assert_eq!(source.env_var.as_deref(), Some("ANTHROPIC_API_KEY"));
    }

    #[test]
    fn user_provider_added_with_static_credential() {
        let mut inp = anthropic_only_inputs();
        inp.user_providers.insert(
            "groq".to_string(),
            json!({ "type": "openai", "baseUrl": "https://api.groq.com/openai/v1", "apiKeyEnv": "GROQ_API_KEY", "models": ["llama-3.3-70b"] }),
        );
        let out = assemble(inp);
        let groq = out
            .client_config
            .providers
            .iter()
            .find(|p| p.profile_name == "groq")
            .unwrap();
        assert_eq!(
            groq.credential,
            CredentialConfig::Static {
                id: "groq".to_string()
            }
        );
        let cs = out
            .credential_sources
            .iter()
            .find(|c| c.credential_id == "groq")
            .unwrap();
        assert_eq!(cs.env_var.as_deref(), Some("GROQ_API_KEY"));
    }

    #[test]
    fn model_less_user_provider_dropped_with_warning() {
        let mut inp = anthropic_only_inputs();
        inp.user_providers.insert(
            "listingonly".to_string(),
            json!({ "type": "openai", "baseUrl": "https://x", "apiKeyEnv": "LISTING_ONLY_KEY" }),
        );
        let out = assemble(inp);
        assert!(out
            .client_config
            .providers
            .iter()
            .all(|p| p.profile_name != "listingonly"));
        assert!(out
            .warnings
            .iter()
            .any(|w| w.contains("listingonly") && w.contains("no models")));
    }

    #[test]
    fn alias_folded_into_target_model() {
        let mut inp = anthropic_only_inputs();
        inp.routing = Some(json!({ "aliases": { "boss": "anthropic/claude-opus-4-6" } }));
        let out = assemble(inp);
        let anthropic = &out.client_config.providers[0];
        assert!(anthropic
            .models
            .iter()
            .find(|m| m.request_model == "claude-opus-4-6")
            .unwrap()
            .aliases
            .iter()
            .any(|a| a == "boss"));
    }

    #[test]
    fn unknown_alias_target_warns() {
        let mut inp = anthropic_only_inputs();
        inp.routing = Some(json!({ "aliases": { "x": "nope/missing" } }));
        let out = assemble(inp);
        assert!(out
            .warnings
            .iter()
            .any(|w| w.contains('x') && w.contains("nope/missing")));
    }

    #[test]
    fn fallback_validated_into_chain() {
        let mut inp = anthropic_only_inputs();
        inp.user_providers.insert(
            "deepseek-user".to_string(),
            json!({ "type": "openai", "baseUrl": "https://api.deepseek.com", "apiKeyEnv": "DEEPSEEK_API_KEY", "models": ["deepseek-chat"] }),
        );
        inp.routing = Some(json!({
            "fallback": { "primary": ["anthropic/claude-opus-4-6", "deepseek-user/deepseek-chat"] }
        }));
        let out = assemble(inp);
        let chain = out.chains.chains.get("primary").unwrap();
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0].provider_id, ProviderId::AnthropicFirstParty);
        assert_eq!(chain[0].model, "claude-opus-4-6");
        assert_eq!(
            chain[1].provider_id,
            ProviderId::OpenAICompatible {
                name: "deepseek-user".to_string()
            }
        );
        assert_eq!(chain[1].model, "deepseek-chat");
    }

    #[test]
    fn fallback_unknown_entry_skipped_chain_kept() {
        let mut inp = anthropic_only_inputs();
        inp.routing =
            Some(json!({ "fallback": { "primary": ["anthropic/claude-opus-4-6", "ghost/none"] } }));
        let out = assemble(inp);
        let chain = out.chains.chains.get("primary").unwrap();
        assert_eq!(chain.len(), 1);
        assert!(out.warnings.iter().any(|w| w.contains("ghost/none")));
    }

    #[test]
    fn fallback_all_unknown_chain_dropped() {
        let mut inp = anthropic_only_inputs();
        inp.routing = Some(json!({ "fallback": { "primary": ["ghost/a", "ghost/b"] } }));
        let out = assemble(inp);
        assert!(!out.chains.chains.contains_key("primary"));
        assert!(out.warnings.iter().any(|w| w.contains("chain dropped")));
    }

    #[test]
    fn anthropic_builtins_priced_through_assembled_catalog() {
        // The assembled pricing is a cost::PricingCatalog with Anthropic tiers.
        let out = assemble(anthropic_only_inputs());
        let mr = cost::ModelRef {
            provider: cost::pricing::ProviderId::Anthropic,
            model: "claude-opus-4-6".to_string(),
        };
        assert!(out.pricing.resolve(&mr).is_ok());
    }
}
