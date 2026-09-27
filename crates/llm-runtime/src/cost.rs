//! Pricing catalog and per-call cost estimation.

use std::collections::HashMap;

use crate::{CostEstimate, LlmError, PricingModelRef, ProviderId, Usage};

/// Unknown-pricing policy for cost estimation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PricingPolicy {
    /// Return an unestimated cost when pricing is unknown.
    MarkUnestimated,
    /// Return an error when pricing is unknown.
    RequirePriced,
}

/// Per-million-token prices for independent billable buckets.
///
/// All fields are USD per million tokens (llm-runtime native unit).  Use
/// [`TokenPricing::input_output`] to create a value with only input/output
/// buckets set; reasoning uses the output rate and cache buckets default to
/// `0.0`. An explicitly supplied zero reasoning rate remains free.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenPricing {
    /// Input-token price per million tokens.
    #[serde(rename = "inputPerMtok")]
    pub input_per_million: f64,
    /// Output-token price per million tokens.
    #[serde(rename = "outputPerMtok")]
    pub output_per_million: f64,
    /// Cache-write price per million tokens.
    #[serde(rename = "cacheWritePerMtok", default)]
    pub cache_write_per_million: f64,
    /// Cache-read price per million tokens.
    #[serde(rename = "cacheReadPerMtok", default)]
    pub cache_read_per_million: f64,
    /// Resolved reasoning-token price per million tokens, including an
    /// output-rate fallback when the input did not specify a separate rate.
    #[serde(rename = "reasoningPerMtok", default)]
    pub reasoning_per_million: f64,
}

impl<'de> serde::Deserialize<'de> for TokenPricing {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        struct Input {
            #[serde(rename = "inputPerMtok")]
            input: f64,
            #[serde(rename = "outputPerMtok")]
            output: f64,
            #[serde(rename = "cacheWritePerMtok", default)]
            cache_write: f64,
            #[serde(rename = "cacheReadPerMtok", default)]
            cache_read: f64,
            #[serde(rename = "reasoningPerMtok", default)]
            reasoning: Option<f64>,
        }

        let input = Input::deserialize(deserializer)?;
        Ok(Self {
            input_per_million: input.input,
            output_per_million: input.output,
            cache_write_per_million: input.cache_write,
            cache_read_per_million: input.cache_read,
            reasoning_per_million: input.reasoning.unwrap_or(input.output),
        })
    }
}

impl TokenPricing {
    /// Create pricing with reasoning billed at the output rate and no cache charges.
    #[must_use]
    pub fn input_output(input_per_million: f64, output_per_million: f64) -> Self {
        Self {
            input_per_million,
            output_per_million,
            cache_write_per_million: 0.0,
            cache_read_per_million: 0.0,
            reasoning_per_million: output_per_million,
        }
    }
}

/// Pricing catalog with built-in prices and external overrides.
#[derive(Debug, Clone, Default)]
pub struct PricingCatalog {
    prices: HashMap<PricingKey, TokenPricing>,
    overrides: HashMap<PricingKey, TokenPricing>,
}

impl PricingCatalog {
    /// Create an empty pricing catalog.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Add a built-in price entry.
    #[must_use]
    pub fn with_price(
        mut self,
        provider_id: ProviderId,
        billing_model: impl Into<String>,
        pricing: TokenPricing,
    ) -> Self {
        self.prices
            .insert(PricingKey::new(provider_id, billing_model), pricing);
        self
    }

    /// Add an external override price entry.
    #[must_use]
    pub fn with_override(
        mut self,
        provider_id: ProviderId,
        billing_model: impl Into<String>,
        pricing: TokenPricing,
    ) -> Self {
        self.overrides
            .insert(PricingKey::new(provider_id, billing_model), pricing);
        self
    }

    /// Mutably insert an override entry (post-construction — mirrors
    /// [`PricingCatalog::with_override`] but takes `&mut self` instead of
    /// consuming `self`).
    ///
    /// Use in the build phase after `llm_catalog_from_cost` populates the
    /// built-in prices, so settings-declared per-profile price overrides are
    /// applied before handing the catalog to `CostEstimator::new`.
    pub fn add_override(
        &mut self,
        provider_id: ProviderId,
        billing_model: impl Into<String>,
        pricing: TokenPricing,
    ) {
        self.overrides
            .insert(PricingKey::new(provider_id, billing_model), pricing);
    }

    /// Look up the [`TokenPricing`] for a `(provider_id, billing_model)` pair,
    /// preferring an override over the built-in entry. Returns `None` when the
    /// catalog has no price for that model.
    ///
    /// Used by the host cost-catalog bridge (`provider-config::cost_translate`)
    /// to source real per-model prices from the models.dev presets instead of
    /// the Claude `$5/$25` default-unknown tier.
    #[must_use]
    pub fn get(&self, provider_id: &ProviderId, billing_model: &str) -> Option<TokenPricing> {
        let key = PricingKey::new(provider_id.clone(), billing_model.to_string());
        self.overrides
            .get(&key)
            .copied()
            .or_else(|| self.prices.get(&key).copied())
    }

    fn lookup(&self, pricing_model: &PricingModelRef) -> Option<(TokenPricing, &'static str)> {
        let key = PricingKey::new(
            pricing_model.pricing_provider_id.clone(),
            pricing_model.billing_model.clone(),
        );

        self.overrides
            .get(&key)
            .copied()
            .map(|pricing| (pricing, "override"))
            .or_else(|| {
                self.prices
                    .get(&key)
                    .copied()
                    .map(|pricing| (pricing, "builtin"))
            })
    }
}

/// Cost estimator using a pricing catalog and unknown-pricing policy.
#[derive(Debug, Clone)]
pub struct CostEstimator {
    catalog: PricingCatalog,
    policy: PricingPolicy,
}

impl CostEstimator {
    /// Create a cost estimator.
    #[must_use]
    pub fn new(catalog: PricingCatalog, policy: PricingPolicy) -> Self {
        Self { catalog, policy }
    }

    /// Estimate cost from resolved pricing identity and normalized usage.
    pub fn estimate(
        &self,
        pricing_model: PricingModelRef,
        usage: &Usage,
    ) -> Result<CostEstimate, LlmError> {
        let Some((pricing, source)) = self.catalog.lookup(&pricing_model) else {
            return match self.policy {
                PricingPolicy::MarkUnestimated => Ok(CostEstimate::unestimated(pricing_model)),
                PricingPolicy::RequirePriced => Err(LlmError::CostUnavailable {
                    message: format!(
                        "missing pricing for {:?}/{}",
                        pricing_model.pricing_provider_id, pricing_model.billing_model
                    ),
                }),
            };
        };

        let tokens = usage.billable_tokens;
        let sdk_usage = lingxi_llm_client::protocol::Usage {
            input_tokens: tokens.input,
            output_tokens: tokens.output.saturating_add(tokens.reasoning_output),
            reasoning_tokens: tokens.reasoning_output,
            cache_read_tokens: tokens.cache_read,
            cache_write_tokens: tokens.cache_write,
            ..Default::default()
        };
        let quote = lingxi_llm_client::protocol::PriceQuote {
            rule: None,
            multiplier: None,
            status: lingxi_llm_client::protocol::PriceStatus::Priced,
            context: Default::default(),
            rates: Some(lingxi_llm_client::protocol::TokenRates {
                input_per_million: Some(pricing.input_per_million),
                output_per_million: Some(pricing.output_per_million),
                cache_read_per_million: Some(pricing.cache_read_per_million),
                cache_write_per_million: Some(pricing.cache_write_per_million),
                cache_write_1h_per_million: Some(pricing.cache_write_per_million),
                reasoning_per_million: Some(pricing.reasoning_per_million),
            }),
            quota: None,
            currency: "USD".into(),
            unit: "per_million_tokens".into(),
            source: Some(source.into()),
            verified_at: None,
            reason: None,
        };
        let identity = lingxi_llm_client::PricingModelRef {
            pricing_provider_id: lingxi_llm_client::protocol::ProviderId::new(
                crate::upstream::provider_name(&pricing_model.pricing_provider_id),
            ),
            billing_model: pricing_model.billing_model.clone(),
            request_model: pricing_model.request_model.clone(),
            display_model: pricing_model.display_model.clone(),
        };
        let estimate = lingxi_llm_client::client::pricing::estimate(&quote, &sdk_usage, &identity)
            .map_err(crate::upstream::error)?;
        project_estimate(estimate, pricing_model)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PricingKey {
    provider_id: ProviderId,
    billing_model: String,
}

impl PricingKey {
    fn new(provider_id: ProviderId, billing_model: impl Into<String>) -> Self {
        Self {
            provider_id,
            billing_model: billing_model.into(),
        }
    }
}

impl TokenPricing {
    pub(crate) fn to_wire(self, source: &str) -> lingxi_llm_client::protocol::TokenPricing {
        use lingxi_llm_client::protocol::{PriceRule, ServiceTier, Submission, TokenRates};
        let mut pricing = lingxi_llm_client::protocol::TokenPricing {
            currency: Some("USD".into()),
            input_per_million: Some(self.input_per_million),
            output_per_million: Some(self.output_per_million),
            cache_read_per_million: Some(self.cache_read_per_million),
            cache_write_per_million: Some(self.cache_write_per_million),
            cache_write_1h_per_million: Some(self.cache_write_per_million),
            reasoning_per_million: Some(self.reasoning_per_million),
            source: Some(source.into()),
            ..Default::default()
        };
        if source == "override" {
            // User prices replace the model's published schedule as well as
            // its base rates. A model-local rule leaves sibling models on the
            // provider's original peak schedule. It declares no Fast price.
            pricing.rules.push(PriceRule {
                service_tier: ServiceTier::Standard,
                submission: Submission::Interactive,
                rates: TokenRates {
                    input_per_million: pricing.input_per_million,
                    output_per_million: pricing.output_per_million,
                    cache_read_per_million: pricing.cache_read_per_million,
                    cache_write_per_million: pricing.cache_write_per_million,
                    cache_write_1h_per_million: pricing.cache_write_1h_per_million,
                    reasoning_per_million: pricing.reasoning_per_million,
                },
                apply_peak_schedule: false,
                ..Default::default()
            });
        }
        pricing
    }
}
pub(crate) fn project_estimate(
    estimate: lingxi_llm_client::client::pricing::CostEstimate,
    pricing_model: PricingModelRef,
) -> Result<CostEstimate, LlmError> {
    if estimate.currency != "USD" {
        return Err(LlmError::CostUnavailable {
            message: "non-USD prices cannot enter the USD ledger".into(),
        });
    }
    Ok(CostEstimate {
        pricing_model,
        total_cost_usd: Some(estimate.total_cost),
        input_cost_usd: Some(estimate.input_cost),
        output_cost_usd: Some(estimate.output_cost),
        cache_read_cost_usd: Some(estimate.cache_read_cost),
        cache_write_cost_usd: Some(estimate.cache_write_cost),
        reasoning_cost_usd: Some(estimate.reasoning_cost),
        estimated: true,
        pricing_source: estimate.source,
    })
}
impl CostEstimator {
    pub(crate) fn capture(
        &self,
        snapshot: lingxi_llm_client::FrozenPricing,
        identity: &PricingModelRef,
    ) -> lingxi_llm_client::FrozenPricing {
        if let Some((pricing, source)) = self.catalog.lookup(identity) {
            if source == "override" || snapshot.model().pricing.is_none() {
                return snapshot.with_token_pricing(pricing.to_wire(source));
            }
        }
        snapshot
    }
}
