//! Model catalog conversion.
use super::lower_reasoning_control_spec;
use crate::protocol::listings::{
    ModelBillingModeDto, ModelCapabilitiesDto, ModelDetailsDto, ModelPricingDto,
    ModelPricingTierDto, ProviderModelCatalogEntryDto,
};
use lingxi_core::host::orchestrator::ProviderModelCatalogEntry;

/// Lower one provider-qualified model listing without inventing missing facts.
#[must_use]
pub fn lower_model_details(listing: &lingxi_core::host::ModelListing) -> ModelDetailsDto {
    let pricing = listing
        .metadata
        .pricing
        .as_ref()
        .map(|pricing| ModelPricingDto {
            billing_mode: match pricing.billing_mode {
                lingxi_core::host::ModelBillingMode::PerToken => ModelBillingModeDto::PerToken,
                lingxi_core::host::ModelBillingMode::Subscription => {
                    ModelBillingModeDto::Subscription
                }
                lingxi_core::host::ModelBillingMode::Free => ModelBillingModeDto::Free,
                lingxi_core::host::ModelBillingMode::Unknown => ModelBillingModeDto::Unknown,
            },
            input_per_million: pricing.input_per_million,
            output_per_million: pricing.output_per_million,
            cache_read_per_million: pricing.cache_read_per_million,
            cache_write_per_million: pricing.cache_write_per_million,
            reasoning_per_million: pricing.reasoning_per_million,
            tiers: pricing
                .tiers
                .iter()
                .map(|tier| ModelPricingTierDto {
                    context_threshold_tokens: tier.context_threshold_tokens,
                    input_per_million: tier.input_per_million,
                    output_per_million: tier.output_per_million,
                    cache_read_per_million: tier.cache_read_per_million,
                    cache_write_per_million: tier.cache_write_per_million,
                    reasoning_per_million: tier.reasoning_per_million,
                })
                .collect(),
            source: pricing.source.clone(),
        });
    ModelDetailsDto {
        reference: lingxi_core::host::qualified_model_ref(
            &listing.request_model,
            Some(&listing.provider_id),
        ),
        provider_id: listing.provider_id.clone(),
        provider_label: listing.provider_label.clone(),
        display_name: listing.display_model.clone(),
        model_id: listing.request_model.clone(),
        description: listing.description.clone(),
        family: listing.metadata.family.clone(),
        status: listing.metadata.status.clone(),
        release_date: listing.metadata.release_date.clone(),
        last_updated: listing.metadata.last_updated.clone(),
        knowledge_cutoff: listing.metadata.knowledge_cutoff.clone(),
        input_modalities: listing.metadata.input_modalities.clone(),
        output_modalities: listing.metadata.output_modalities.clone(),
        context_window_tokens: listing.metadata.context_window_tokens,
        max_input_tokens: listing.metadata.max_input_tokens,
        max_output_tokens: listing.metadata.max_output_tokens,
        open_weights: listing.metadata.open_weights,
        attachments: listing.metadata.attachments,
        temperature_control: listing.metadata.temperature_control,
        pricing,
        capabilities: ModelCapabilitiesDto {
            streaming: listing.capabilities.streaming,
            tools: listing.capabilities.tools,
            vision: listing.capabilities.vision,
            documents: listing.capabilities.documents,
            reasoning: listing.capabilities.reasoning,
            structured_output: listing.capabilities.structured_output,
        },
        reasoning: lower_reasoning_control_spec(&listing.reasoning),
        supports_fast_mode: listing.provider_id == "anthropic"
            && lingxi_core::host::model_capabilities::has_capability(
                &listing.request_model,
                lingxi_core::host::model_capabilities::ModelCapability::FastMode,
            ),
        fusion_analyst_capable: listing.fusion_analyst_capable,
    }
}

/// Lower one settings-visible provider catalog group from route listings.
#[must_use]
pub fn lower_provider_model_catalog_entry(
    entry: &ProviderModelCatalogEntry,
) -> ProviderModelCatalogEntryDto {
    ProviderModelCatalogEntryDto {
        provider_id: entry.provider_id.clone(),
        provider_label: entry.provider_label.clone(),
        models: entry.models.iter().map(lower_model_details).collect(),
        group: entry.connection.group.clone(),
        connection_id: entry.connection.connection_id.clone(),
    }
}
