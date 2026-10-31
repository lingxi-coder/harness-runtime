//! One projection of official setup metadata for Desktop, Android and iOS.
use client::protocol::listings::{ProviderDirectoryDefinitionDto, ProviderModelCatalogEntryDto};

/// Render only offered chat models, retaining the complete connection facts.
pub fn entries(catalog: &llm_runtime::BuiltinCatalog) -> Vec<ProviderModelCatalogEntryDto> {
    let Ok(registry) = llm_runtime::ModelRegistry::from_config(llm_runtime::ClientConfig {
        providers: catalog.providers.clone(),
    }) else {
        return Vec::new();
    };
    let listings = registry
        .available_models()
        .into_iter()
        .map(orchestrator::provider_adapter::lower_model_listing)
        .collect::<Vec<_>>();
    lingxi_core::host::provider_model_catalog(&listings)
        .iter()
        .map(|entry| {
            let mut dto = client::adapter::lowering::lower_provider_model_catalog_entry(entry);
            if let Some(profile) = catalog
                .providers
                .iter()
                .find(|p| p.profile_name == entry.provider_id)
            {
                dto.definition = Some(ProviderDirectoryDefinitionDto {
                    base_url: profile.base_url.clone(),
                    protocol: serde_json::to_value(profile.protocol)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_owned))
                        .unwrap_or_default(),
                    auth: serde_json::to_value(profile.auth)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_owned))
                        .unwrap_or_default(),
                    credential_env: match &profile.credential {
                        llm_runtime::CredentialConfig::Env { var } => Some(var.clone()),
                        _ => None,
                    },
                    default_model_id: profile
                        .models
                        .first()
                        .map(|m| m.request_model.clone())
                        .unwrap_or_default(),
                    description: profile
                        .wire_profile
                        .as_ref()
                        .and_then(|p| p.info.description.clone())
                        .unwrap_or_default(),
                });
            }
            dto
        })
        .collect()
}
