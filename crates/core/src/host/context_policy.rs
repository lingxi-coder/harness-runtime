//! Context facts and application policy. None means unknown, never unlimited.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum ContextPolicy {
    #[default]
    Auto,
    Long,
    Custom {
        input_tokens: u64,
    },
}

/// An official input-length recommendation, distinct from a billing threshold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContextRecommendation {
    pub input_tokens: u64,
    pub source: String,
    pub verified_at: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContextMetadata {
    /// True only when the endpoint documents input/output sharing this window.
    pub shared_window: Option<bool>,
    pub thinking_max_input_tokens: Option<u64>,
    pub recommendation: Option<ContextRecommendation>,
    pub application: Option<ContextPolicy>,
}

impl ContextMetadata {
    pub fn validate(&self) -> Result<(), String> {
        if self.thinking_max_input_tokens == Some(0) {
            return Err("thinking input limit must be positive".into());
        }
        if matches!(
            self.application,
            Some(ContextPolicy::Custom { input_tokens: 0 })
        ) {
            return Err("custom context input budget must be positive".into());
        }
        if self.recommendation.as_ref().is_some_and(|r| {
            r.input_tokens == 0 || r.source.trim().is_empty() || r.verified_at.trim().is_empty()
        }) {
            return Err("context recommendation requires a positive input limit, source and verification date".into());
        }
        Ok(())
    }
}

/// Current catalog-derived input policy for UI and diagnostics, not live usage.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelContextBudget {
    pub policy: String,
    pub effective_input_budget: Option<u64>,
    pub hard_input_limit: Option<u64>,
    pub standard_price_input_tokens: Option<u64>,
    pub recommendation_input_tokens: Option<u64>,
    pub recommendation_source: Option<String>,
}
