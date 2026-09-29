//! Durable history adapter for the usage shape written before the SDK became
//! the source of truth. The legacy buckets exist only at this serde boundary.

use crate::ExecutionUsage;
use lingxi_llm_client::protocol::{
    InferenceReport, ServerToolUsage, ServiceTier, Usage, UsageReport, UsageState,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

#[derive(Serialize, Deserialize)]
struct TokenBuckets {
    input: u64,
    output: u64,
    cache_write: u64,
    cache_read: u64,
    reasoning_output: u64,
}

#[derive(Serialize, Deserialize)]
struct HostedTools {
    web_search_requests: u64,
}

#[derive(Serialize, Deserialize)]
struct StoredUsage {
    billable_tokens: TokenBuckets,
    context_tokens: Option<u64>,
    provider_reported_total_tokens: Option<u64>,
    server_tool_use: Option<HostedTools>,
    #[serde(default)]
    provider_metadata: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    speed: Option<String>,
}

/// Additive metadata keeps SDK-only observations durable without changing the
/// established top-level history/FFI usage fields.
#[derive(Serialize, Deserialize)]
struct CanonicalHistoryUsage {
    schema: u8,
    report: UsageReport,
    inference: InferenceReport,
}

impl Serialize for ExecutionUsage {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let counts = self.counts();
        let mut provider_metadata = self.provider_metadata.clone();
        if !provider_metadata.is_object() {
            provider_metadata = if provider_metadata.is_null() {
                serde_json::json!({})
            } else {
                serde_json::json!({ "native": provider_metadata })
            };
        }
        provider_metadata["upstreamUsageState"] = serde_json::json!(self.report.state);
        if counts.cache_write_1h_tokens > 0 || provider_metadata.get("cache_creation").is_some() {
            if !provider_metadata["cache_creation"].is_object() {
                provider_metadata["cache_creation"] = serde_json::json!({});
            }
            provider_metadata["cache_creation"]["ephemeral_1h_input_tokens"] =
                serde_json::json!(counts.cache_write_1h_tokens);
        }
        if !provider_metadata["llm_client"].is_object() {
            let native = provider_metadata["llm_client"].take();
            provider_metadata["llm_client"] = if native.is_null() {
                serde_json::json!({})
            } else {
                serde_json::json!({ "native": native })
            };
        }
        provider_metadata["llm_client"]["history_usage"] = serde_json::json!({
            "schema": 1,
            "report": self.report,
            "inference": self.inference,
        });
        StoredUsage {
            billable_tokens: TokenBuckets {
                input: counts.input_tokens,
                output: counts.output_tokens.saturating_sub(counts.reasoning_tokens),
                cache_write: counts.cache_write_tokens,
                cache_read: counts.cache_read_tokens,
                reasoning_output: counts.reasoning_tokens,
            },
            context_tokens: self.context_tokens(),
            provider_reported_total_tokens: self.provider_reported_total_tokens,
            server_tool_use: counts.server_tool_usage.and_then(|tools| {
                tools
                    .web_search_requests
                    .map(|web_search_requests| HostedTools {
                        web_search_requests,
                    })
            }),
            provider_metadata,
            speed: self.inference.raw_speed.clone().or_else(|| {
                (self.inference.service_tier == Some(ServiceTier::Fast)).then(|| "fast".into())
            }),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ExecutionUsage {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let stored = StoredUsage::deserialize(deserializer)?;
        let canonical = stored
            .provider_metadata
            .pointer("/llm_client/history_usage")
            .and_then(|value| serde_json::from_value::<CanonicalHistoryUsage>(value.clone()).ok())
            .filter(|extension| extension.schema == 1);
        let state = stored
            .provider_metadata
            .get("upstreamUsageState")
            .and_then(|value| serde_json::from_value(value.clone()).ok())
            .unwrap_or(UsageState::Partial);
        let usage = Usage {
            input_tokens: stored.billable_tokens.input,
            output_tokens: stored
                .billable_tokens
                .output
                .saturating_add(stored.billable_tokens.reasoning_output),
            cache_write_tokens: stored.billable_tokens.cache_write,
            cache_read_tokens: stored.billable_tokens.cache_read,
            reasoning_tokens: stored.billable_tokens.reasoning_output,
            cache_write_1h_tokens: stored
                .provider_metadata
                .pointer("/cache_creation/ephemeral_1h_input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or_default(),
            server_tool_usage: stored.server_tool_use.map(|tools| ServerToolUsage {
                web_search_requests: Some(tools.web_search_requests),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut inference = InferenceReport::default();
        if let Some(speed) = stored.speed {
            if speed == "fast" {
                inference.service_tier = Some(ServiceTier::Fast);
            }
            inference.raw_speed = Some(speed);
        }
        let fallback_report = if state == UsageState::Missing {
            UsageReport { usage: None, state }
        } else {
            UsageReport::measured(usage, state)
        };
        Ok(Self {
            report: canonical
                .as_ref()
                .map_or(fallback_report, |extension| extension.report.clone()),
            inference: canonical.map_or(inference, |extension| extension.inference),
            context_tokens: stored.context_tokens,
            provider_reported_total_tokens: stored.provider_reported_total_tokens,
            provider_metadata: stored.provider_metadata,
            cost_estimate: None,
        })
    }
}
