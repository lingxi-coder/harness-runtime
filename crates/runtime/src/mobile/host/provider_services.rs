use client::protocol::listings::{ModelDetailsDto, ProviderModelCatalogEntryDto};
use llm_runtime::CredentialConfig;

use super::{lower_model_details, MobileConfig};

/// Credential-free metadata used by mobile settings to render the same
/// provider choices the engine can actually assemble.
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderCatalogEntryDto {
    pub profile_id: String,
    pub display_name: String,
    pub base_url: String,
    pub protocol: String,
    pub auth: String,
    pub credential_env: Option<String>,
    pub models: Vec<String>,
    pub model_details: Vec<ModelDetailsDto>,
}

pub(super) fn provider_catalog_from_profiles(
    providers: Vec<llm_runtime::ProviderProfile>,
) -> Vec<ProviderCatalogEntryDto> {
    let to_entry = |provider: llm_runtime::ProviderProfile| {
        let display_name = provider
            .wire_profile
            .as_ref()
            .and_then(|p| p.info.display_name.clone())
            .unwrap_or_else(|| provider.profile_name.clone());
        let credential_env = match &provider.credential {
            CredentialConfig::Env { var } => Some(var.clone()),
            _ => None,
        };
        let listings = model_listings(std::slice::from_ref(&provider));
        let curated = listings
            .into_iter()
            .filter(|listing| {
                !matches!(
                    listing.metadata.status.as_deref(),
                    Some("disabled" | "deprecated")
                ) && (listing.metadata.catalog_offered
                    || lingxi_core::host::is_curated_model(
                        &provider.profile_name,
                        &listing.request_model,
                    )
                    || !lingxi_core::host::provider_has_curated_list(&provider.profile_name))
            })
            .collect::<Vec<_>>();
        ProviderCatalogEntryDto {
            profile_id: provider.profile_name.clone(),
            display_name,
            base_url: provider.base_url,
            protocol: serde_json::to_value(provider.protocol)
                .unwrap()
                .as_str()
                .unwrap()
                .into(),
            auth: serde_json::to_value(provider.auth)
                .unwrap()
                .as_str()
                .unwrap()
                .into(),
            credential_env,
            models: curated
                .iter()
                .map(|listing| listing.request_model.clone())
                .collect(),
            model_details: curated.iter().map(lower_model_details).collect(),
        }
    };

    providers
        .into_iter()
        .filter(|p| !p.connection.hidden)
        .map(to_entry)
        .collect()
}

pub(super) fn builtin_provider_catalog() -> Vec<ProviderCatalogEntryDto> {
    provider_catalog_from_profiles(llm_runtime::builtin_presets().providers)
}

pub(super) fn provider_model_catalog_from_listings(
    listings: &[lingxi_core::host::ModelListing],
) -> Vec<ProviderModelCatalogEntryDto> {
    lingxi_core::host::provider_model_catalog(listings)
        .iter()
        .map(client::adapter::lowering::lower_provider_model_catalog_entry)
        .collect()
}

pub(super) fn anthropic_models(default_model: &str) -> Vec<llm_runtime::ModelProfile> {
    let caps = llm_runtime::Capabilities {
        streaming: true,
        tools: true,
        vision: true,
        documents: true,
        reasoning: true,
        structured_output: true,
    };
    // Every id `lingxi_core::host::is_curated_model` lists under the "anthropic" arm must
    // appear here, otherwise the client picker's ANTHROPIC section renders only
    // the subset this registry happens to route (the section used to show just
    // Sonnet 4.6 + Haiku 4.5 while Sonnet 5 / Opus 4.8 / Fable 5 were curated
    // but unroutable), plus the extra Opus routes the host may still request.
    let mut ids: Vec<String> = vec![
        "claude-opus-5".to_string(),
        "claude-opus-4-8".to_string(),
        "claude-opus-4-6".to_string(),
        "claude-sonnet-5".to_string(),
        "claude-sonnet-4-6".to_string(),
        "claude-haiku-4-5".to_string(),
        "claude-fable-5-1".to_string(),
    ];
    // The configured default, when it routes here — see `anthropic_route_id`.
    ids.extend(anthropic_route_id(default_model));
    // Env-configured small-fast / haiku model a `prompt` hook may resolve to
    // (matching `hook_prompt_runner::resolve_model`'s precedence:
    // `ANTHROPIC_SMALL_FAST_MODEL` > `ANTHROPIC_DEFAULT_HAIKU_MODEL` > default
    // Haiku), so such a request resolves instead of failing `ModelUnavailable`.
    for var in [
        "ANTHROPIC_SMALL_FAST_MODEL",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    ] {
        if let Ok(m) = std::env::var(var) {
            // Same routing rule as the configured default above. Pushing the
            // raw env value bypassed the guard entirely — `ANTHROPIC_SMALL_
            // FAST_MODEL=anthropic/claude-haiku-4-5` registered the qualified
            // string as a model id, and a foreign ref leaked a foreign model
            // into this registry by the very path the guard exists to close.
            ids.extend(anthropic_route_id(&m));
        }
    }
    ids.sort();
    ids.dedup();
    ids.into_iter()
        .map(|id| llm_runtime::ModelProfile {
            display_model: id.clone(),
            request_model: id.clone(),
            billing_model: id,
            aliases: Vec::new(),
            description: None,
            metadata: Default::default(),
            capabilities: caps,
        })
        .collect()
}

/// The BARE model id `model_ref` contributes to the Anthropic profile's
/// exact-id registry, or `None` when it names a model on another provider.
///
/// One rule for every source that can add a route (the configured default and
/// the `ANTHROPIC_SMALL_FAST_MODEL` / `ANTHROPIC_DEFAULT_HAIKU_MODEL` env ids):
/// the ref must ROUTE to anthropic (a `claude-*` id, an unqualified custom id,
/// or an `anthropic/…` ref) AND the remainder must be a BARE model id. A ref
/// qualified for another provider must never land here — a client that stored a
/// qualified id and re-qualified it on the way back in
/// (`anthropic/deepseek/deepseek-flash`) otherwise registered a `deepseek`
/// model inside the Anthropic profile, and the picker then rendered that
/// model's name under the ANTHROPIC header in Anthropic's colour.
pub(super) fn anthropic_route_id(model_ref: &str) -> Option<String> {
    let (profile, bare) = llm_runtime::split_profile_model(model_ref.trim());
    (profile == "anthropic" && !bare.is_empty() && !bare.contains('/')).then_some(bare)
}

/// The assembled provider profiles flattened into the [`lingxi_core::host::ModelListing`]s
/// that [`resolve_default_model_ref`] and [`lingxi_core::host::parse_model_ref`] resolve
/// against.
///
/// `display_model` / `provider_label` are immaterial to parsing, so
/// `request_model` and the profile name stand in for both. Shared with the
/// tests so they cannot drift from the shape production actually feeds in.
pub(super) fn model_listings(
    providers: &[llm_runtime::ProviderProfile],
) -> Vec<lingxi_core::host::ModelListing> {
    llm_runtime::ModelRegistry::from_config(llm_runtime::ClientConfig {
        providers: providers.to_vec(),
    })
    .map(|registry| {
        registry
            .available_models()
            .into_iter()
            .map(orchestrator::provider_adapter::lower_model_listing)
            .collect()
    })
    .unwrap_or_default()
}

/// Parse the configured `default_model` into `(request_model, profile)`, and
/// self-heal a reference that routes to NO registered provider.
///
/// A client persists its last-picked model and hands it back on the next
/// launch, so a client-side bug can hand us a reference no profile serves (iOS
/// re-qualified an already-qualified id into `anthropic/deepseek/deepseek-v4-
/// flash`). [`lingxi_core::host::parse_model_ref`] then returns the whole string as a bare
/// id, which boots the session onto an unroutable model: the picker shows a
/// junk row and the first turn fails `ModelUnavailable`. Rewriting it to a
/// model that IS registered keeps the session usable and lets the user re-pick.
///
/// Bare custom ids still resolve — [`anthropic_models`] registers them under
/// the Anthropic profile — so only genuinely unroutable refs are rewritten.
///
/// The replacement is picked FROM `listings`, never from a constant: the mobile
/// allowlist (`mobileEnabledProfiles`) is fail-closed and can strip the
/// Anthropic profile entirely, and healing onto a hardcoded `claude-sonnet-5`
/// there would swap one unroutable ref for another while the log claimed the
/// session was repaired. The chosen profile is returned too — a bare
/// `ClientEvent::ModelList { current }` matches none of the provider-qualified
/// rows `lingxi_core::host::curated_model_refs` emits, so the client's picker would render
/// with nothing selected.
pub(super) fn resolve_default_model_ref(
    default_model: &str,
    listings: &[lingxi_core::host::ModelListing],
) -> (String, Option<String>) {
    let (model, profile) = lingxi_core::host::parse_model_ref(default_model, listings);
    // `parse_model_ref` returns `Some(profile)` only after matching a listing on
    // that exact `(provider_id, request_model)` pair, so a qualified ref is
    // already proven routable and keeps its profile as-is.
    if profile.is_some() || listings.is_empty() {
        return (model, profile);
    }
    // A BARE id is routable when some listing serves it — and when exactly one
    // does, scope it to that provider. `curated_model_refs` performs the same
    // unique-provider inference for the rows it emits, so leaving the profile
    // unscoped made `ModelList { current }` bare while every row was qualified,
    // and the client's picker rendered with nothing selected. That is the
    // default on every fresh launch, since `MobileEngineConfig::default()`'s
    // `default_model` is a bare id.
    let mut serving = listings.iter().filter(|l| l.request_model == model);
    match (serving.next(), serving.next()) {
        // Ambiguous across profiles — stay unscoped and let the registry report
        // the ambiguity rather than silently picking a provider.
        (Some(_), Some(_)) => return (model, profile),
        (Some(only), None) => return (model, Some(only.provider_id.clone())),
        (None, _) => {}
    }
    let fallback = listings
        .iter()
        .find(|listing| {
            listing.provider_id == "anthropic"
                && Some(listing.request_model.as_str())
                    == lingxi_core::host::provider_default_model("anthropic")
        })
        .or_else(|| {
            listings.iter().find(|listing| {
                lingxi_core::host::is_curated_model(&listing.provider_id, &listing.request_model)
            })
        })
        .unwrap_or(&listings[0]);
    (
        fallback.request_model.clone(),
        Some(fallback.provider_id.clone()),
    )
}

/// Preserve the admitted source layers for policies that cannot use a merged scalar.
pub(super) fn mobile_effective_settings(
    cfg: &MobileConfig,
) -> Result<lingxi_core::settings::EffectiveSettings, lingxi_core::settings::SettingsError> {
    use lingxi_core::settings::{
        FileLayerScope, LoadInputs, RawSupplementalLayers, Settings, SettingsJson,
    };
    let env = std::env::vars().collect();
    Settings::load_raw_layer_stack_from_user_path(
        LoadInputs {
            env: &env,
            project_dir: &cfg.cwd,
            defaults: SettingsJson::default(),
        },
        FileLayerScope::ALL,
        RawSupplementalLayers::default(),
        Some(&cfg.lingxi_home.join("settings.json")),
    )?
    .into_effective_settings()
}

pub(super) const MOBILE_ENABLED_PROFILES_KEY: &str = "mobileEnabledProfiles";

/// File-backed settings override legacy native launch defaults using the same
/// field merge rules as desktop. Explicit file profiles remain selectable even
/// when an older native launcher sends its own profile allowlist.
pub(super) fn mobile_provider_settings(
    cfg: &MobileConfig,
) -> Result<lingxi_core::settings::SettingsJson, lingxi_core::settings::SettingsError> {
    use lingxi_core::settings::SettingsJson;

    let layered = mobile_effective_settings(cfg)?.settings;
    let explicit_allowlist = layered
        .routing
        .as_ref()
        .and_then(|routing| routing.get(MOBILE_ENABLED_PROFILES_KEY))
        .is_some();
    let file_profiles: Vec<String> = layered
        .providers
        .as_ref()
        .map(|providers| providers.keys().cloned().collect())
        .unwrap_or_default();
    let mut merged = lingxi_core::settings::merger::merge(
        SettingsJson {
            providers: cfg.provider_profiles.clone(),
            routing: cfg.routing.clone(),
            ..SettingsJson::default()
        },
        layered,
    )?;
    if !explicit_allowlist {
        if let Some(enabled) = merged
            .routing
            .as_mut()
            .and_then(|routing| routing.get_mut(MOBILE_ENABLED_PROFILES_KEY))
            .and_then(serde_json::Value::as_array_mut)
        {
            for profile in file_profiles {
                let value = serde_json::Value::String(profile);
                if !enabled.contains(&value) {
                    enabled.push(value);
                }
            }
        }
    }
    Ok(merged)
}

/// Read the mobile host's explicit profile allowlist. An absent key preserves
/// the shared catalog; a malformed value enables no profiles.
pub(super) fn mobile_enabled_profiles(
    routing: Option<&serde_json::Value>,
) -> Option<std::collections::BTreeSet<String>> {
    let Some(value) = routing.and_then(|routing| routing.get(MOBILE_ENABLED_PROFILES_KEY)) else {
        return None;
    };
    let enabled_profiles = match value.as_array() {
        Some(items) => {
            let mut profiles = std::collections::BTreeSet::new();
            for item in items {
                let Some(profile) = item.as_str().filter(|profile| !profile.is_empty()) else {
                    profiles.clear();
                    break;
                };
                profiles.insert(profile.to_string());
            }
            profiles
        }
        None => std::collections::BTreeSet::new(),
    };

    Some(enabled_profiles)
}

/// Filter execution routes with the same allowlist used by account snapshots.
pub(super) fn apply_mobile_profile_allowlist(
    assembled: &mut provider_config::Assembled,
    routing: Option<&serde_json::Value>,
) {
    let Some(enabled_profiles) = mobile_enabled_profiles(routing) else {
        return;
    };

    let enabled_profiles: std::collections::BTreeSet<String> = assembled
        .client_config
        .providers
        .iter()
        .filter(|profile| provider_config::mobile_profile_allowed(profile, &enabled_profiles))
        .map(|profile| profile.profile_name.clone())
        .collect();

    let allowed_routes: Vec<(llm_runtime::ProviderId, String)> = assembled
        .client_config
        .providers
        .iter()
        .filter(|provider| enabled_profiles.contains(&provider.profile_name))
        .flat_map(|provider| {
            let provider_id = provider.provider_id.clone();
            provider
                .models
                .iter()
                .map(move |model| (provider_id.clone(), model.request_model.clone()))
        })
        .collect();

    assembled
        .client_config
        .providers
        .retain(|provider| enabled_profiles.contains(&provider.profile_name));
    assembled
        .credential_sources
        .retain(|source| enabled_profiles.contains(&source.profile_name));
    assembled.chains.aliases.retain(|_, target| {
        target
            .split_once('/')
            .is_some_and(|(profile, _)| enabled_profiles.contains(profile))
    });
    assembled.chains.chains.retain(|_, entries| {
        entries.retain(|entry| {
            allowed_routes.iter().any(|(provider_id, model)| {
                provider_id == &entry.provider_id && model == &entry.model
            })
        });
        !entries.is_empty()
    });
}
