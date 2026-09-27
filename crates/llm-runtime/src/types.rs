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

/// Independent billable token buckets.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Billable input tokens.
    pub input: u64,
    /// Billable output tokens.
    pub output: u64,
    /// Billable cache-write tokens.
    pub cache_write: u64,
    /// Billable cache-read tokens.
    pub cache_read: u64,
    /// Separately billable reasoning output tokens.
    pub reasoning_output: u64,
}

/// Normalized usage returned by provider codecs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    /// Independent billable token buckets used for cost calculation.
    pub billable_tokens: TokenUsage,
    /// Optional context-window total.
    pub context_tokens: Option<u64>,
    /// Optional total exactly as reported by the provider.
    pub provider_reported_total_tokens: Option<u64>,
    /// Optional provider-reported server tool usage.
    pub server_tool_use: Option<ServerToolUsage>,
    /// Redacted provider metadata retained for diagnostics.
    #[serde(default)]
    pub provider_metadata: Value,
    /// API speed tier actually used for this request (`"fast"` for the
    /// priority/low-latency tier; absent = standard tier).
    ///
    /// Mirrors `api-client::UsageApi.speed` and claude-code's
    /// `BetaUsage.speed` (`services/api/claude.ts:2985`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<String>,
    /// Host-only estimate frozen from the completed physical stream. It is
    /// transferred to accounting and never serialized as provider metadata.
    #[serde(skip)]
    pub cost_estimate: Option<CostEstimate>,
}

/// Provider-side server tool usage counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerToolUsage {
    /// Number of provider-side web search requests.
    pub web_search_requests: u64,
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
