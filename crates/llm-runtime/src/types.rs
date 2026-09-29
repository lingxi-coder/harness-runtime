//! Provider-neutral public types.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Explicit provider identity resolved before request execution.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderId {
    /// Anthropic first-party Messages API.
    AnthropicFirstParty,
    /// `OpenAI` first-party APIs.
    #[serde(rename = "open_ai")]
    OpenAI,
    /// `OpenAI`-compatible API profile with a configured provider name.
    #[serde(rename = "open_ai_compatible")]
    OpenAICompatible {
        /// Configured provider/profile family name.
        name: String,
    },
    /// Gemini first-party API.
    Gemini,
    /// Gemini on Vertex AI.
    VertexGemini,
    /// Claude on Vertex AI.
    VertexClaude,
    /// Claude on AWS Bedrock.
    BedrockClaude,
    /// Claude on Azure AI Foundry.
    FoundryClaude,
    /// Azure `OpenAI`.
    #[serde(rename = "azure_open_ai")]
    AzureOpenAI,
    /// Custom provider profile.
    Custom {
        /// Configured custom provider name.
        name: String,
    },
}

/// Concrete pricing identity emitted by route resolution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PricingModelRef {
    /// Provider namespace used by pricing lookup.
    pub pricing_provider_id: ProviderId,
    /// Model key used by the pricing catalog.
    pub billing_model: String,
    /// Provider-local model value sent on the wire.
    pub request_model: String,
    /// Human-facing model label.
    pub display_model: String,
}

/// Host execution envelope around the SDK's measured usage and inference.
///
/// Token counts and completion state belong to the SDK. The remaining fields
/// are presentation or settlement data and must never be used as protocol
/// observations when pricing an attempt.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExecutionUsage {
    /// Canonical SDK usage, including whether the observation is partial.
    pub report: lingxi_llm_client::protocol::UsageReport,
    /// Canonical SDK inference observations, including the reported tier.
    pub inference: lingxi_llm_client::protocol::InferenceReport,
    /// Context occupancy shown to the host; never used for SDK pricing.
    pub context_tokens: Option<u64>,
    /// Provider total retained for historical presentation.
    pub provider_reported_total_tokens: Option<u64>,
    /// Redacted presentation metadata retained for transcript consumers.
    pub provider_metadata: Value,
    /// Host-only estimate frozen from the completed physical stream. It is
    /// transferred to accounting and never serialized as provider metadata.
    pub cost_estimate: Option<CostEstimate>,
}

impl ExecutionUsage {
    /// Combine cumulative start and terminal stream observations.
    #[must_use]
    pub fn merge_snapshot(&self, delta: &Self) -> Self {
        crate::stream_accumulator::merge_usage(self, delta)
    }

    /// Construct a complete SDK measurement for a host-owned fixture or result.
    #[must_use]
    pub fn from_counts(usage: lingxi_llm_client::protocol::Usage) -> Self {
        let total = usage.total();
        Self {
            report: lingxi_llm_client::protocol::UsageReport::measured(
                usage,
                lingxi_llm_client::protocol::UsageState::Complete,
            ),
            context_tokens: Some(total),
            provider_reported_total_tokens: Some(total),
            ..Self::default()
        }
    }
    /// Provider counters, defaulting to zero only for display of missing data.
    #[must_use]
    pub fn counts(&self) -> lingxi_llm_client::protocol::Usage {
        self.report.usage.unwrap_or_default()
    }

    /// Mutable canonical counters, creating a partial report when necessary.
    pub fn counts_mut(&mut self) -> &mut lingxi_llm_client::protocol::Usage {
        if self.report.usage.is_none()
            && self.report.state == lingxi_llm_client::protocol::UsageState::Missing
        {
            self.report.state = lingxi_llm_client::protocol::UsageState::Partial;
        }
        self.report.usage.get_or_insert_default()
    }

    /// Context total derived from the SDK's disjoint token buckets.
    #[must_use]
    pub fn context_tokens(&self) -> Option<u64> {
        self.context_tokens
            .or_else(|| self.report.usage.map(|usage| usage.total()))
    }

    /// Provider-reported hosted tool counters, when present.
    #[must_use]
    pub fn server_tool_usage(&self) -> Option<lingxi_llm_client::protocol::ServerToolUsage> {
        self.report.usage.and_then(|usage| usage.server_tool_usage)
    }
}

/// Per-call cost estimate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostEstimate {
    /// Pricing identity used for this estimate.
    pub pricing_model: PricingModelRef,
    /// Total cost in USD when pricing is known.
    pub total_cost_usd: Option<f64>,
    /// Input-token cost in USD when pricing is known.
    pub input_cost_usd: Option<f64>,
    /// Output-token cost in USD when pricing is known.
    pub output_cost_usd: Option<f64>,
    /// Cache-read cost in USD when pricing is known.
    pub cache_read_cost_usd: Option<f64>,
    /// Cache-write cost in USD when pricing is known.
    pub cache_write_cost_usd: Option<f64>,
    /// Reasoning-token cost in USD when priced separately.
    pub reasoning_cost_usd: Option<f64>,
    /// Whether the returned numeric costs are estimates.
    pub estimated: bool,
    /// Catalog or fallback source for the pricing decision.
    pub pricing_source: Option<String>,
}

impl CostEstimate {
    /// Build the default unknown-pricing result used by `MarkUnestimated`.
    #[must_use]
    pub fn unestimated(pricing_model: PricingModelRef) -> Self {
        Self {
            pricing_model,
            total_cost_usd: None,
            input_cost_usd: None,
            output_cost_usd: None,
            cache_read_cost_usd: None,
            cache_write_cost_usd: None,
            reasoning_cost_usd: None,
            estimated: false,
            pricing_source: None,
        }
    }
}
