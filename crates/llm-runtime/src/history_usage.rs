//! Durable history adapter for the usage shape written before the SDK became
//! the source of truth. The legacy buckets exist only at this serde boundary.

use crate::ExecutionUsage;
use lingxi_llm_client::protocol::{
    InferenceReport, JsonPayload, LlmError, ServerToolUsage, ServiceTier, Usage, UsageReport,
    UsageState,
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
    provider_metadata: JsonPayload,
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

fn empty_source_object() -> JsonPayload {
    JsonPayload::parse("{}").expect("empty object is valid JSON")
}

fn source_object_or_native(
    value: &JsonPayload,
    null_is_empty: bool,
) -> Result<JsonPayload, LlmError> {
    if value.is_root_object() {
        return Ok(value.clone());
    }
    if null_is_empty && value.as_json_source().trim() == "null" {
        return Ok(empty_source_object());
    }
    empty_source_object().with_object_field("native", value)
}

fn generated_payload(value: Value) -> Result<JsonPayload, serde_json::Error> {
    JsonPayload::from_generated_value(&value)
}

fn ser_source_error<E: serde::ser::Error>(error: impl std::fmt::Display) -> E {
    E::custom(error.to_string())
}

impl Serialize for ExecutionUsage {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let counts = self.counts();
        let mut provider_metadata = source_object_or_native(&self.provider_metadata, true)
            .map_err(ser_source_error::<S::Error>)?;
        let upstream_state = generated_payload(serde_json::json!(self.report.state))
            .map_err(ser_source_error::<S::Error>)?;
        provider_metadata = provider_metadata
            .with_object_field("upstreamUsageState", &upstream_state)
            .map_err(ser_source_error::<S::Error>)?;

        if counts.cache_write_1h_tokens > 0
            || provider_metadata.raw_source_at("/cache_creation").is_some()
        {
            let mut cache_creation = provider_metadata
                .payload_at("/cache_creation")
                .filter(JsonPayload::is_root_object)
                .unwrap_or_else(empty_source_object);
            let one_hour_tokens =
                generated_payload(serde_json::json!(counts.cache_write_1h_tokens))
                    .map_err(ser_source_error::<S::Error>)?;
            cache_creation = cache_creation
                .with_object_field("ephemeral_1h_input_tokens", &one_hour_tokens)
                .map_err(ser_source_error::<S::Error>)?;
            provider_metadata = provider_metadata
                .with_object_field("cache_creation", &cache_creation)
                .map_err(ser_source_error::<S::Error>)?;
        }

        let mut llm_client = match provider_metadata.payload_at("/llm_client") {
            Some(value) => {
                source_object_or_native(&value, true).map_err(ser_source_error::<S::Error>)?
            }
            None => empty_source_object(),
        };
        let history_usage = generated_payload(serde_json::json!({
            "schema": 1,
            "report": &self.report,
            "inference": &self.inference,
        }))
        .map_err(ser_source_error::<S::Error>)?;
        llm_client = llm_client
            .with_object_field("history_usage", &history_usage)
            .map_err(ser_source_error::<S::Error>)?;
        provider_metadata = provider_metadata
            .with_object_field("llm_client", &llm_client)
            .map_err(ser_source_error::<S::Error>)?;
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
            .payload_at("/llm_client/history_usage")
            .and_then(|value| {
                serde_json::from_str::<CanonicalHistoryUsage>(value.as_json_source()).ok()
            })
            .filter(|extension| extension.schema == 1);
        let state = stored
            .provider_metadata
            .raw_source_at("/upstreamUsageState")
            .and_then(|value| serde_json::from_str(&value).ok())
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
                .raw_source_at("/cache_creation/ephemeral_1h_input_tokens")
                .and_then(|value| serde_json::from_str::<u64>(&value).ok())
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
            native_resume_context_exhausted: None,
            native_resume_context_observed: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durable_usage_updates_keep_exact_provider_metadata_source() {
        let provider_metadata =
            JsonPayload::parse(r#"{"opaque":"\ud800","llm_client":{"foreign":"\ud801"}}"#).unwrap();
        let usage = ExecutionUsage {
            report: UsageReport::measured(
                Usage {
                    cache_write_tokens: 9,
                    cache_write_1h_tokens: 4,
                    ..Default::default()
                },
                UsageState::Complete,
            ),
            provider_metadata: provider_metadata.clone(),
            ..Default::default()
        };

        let encoded = serde_json::to_string(&usage).unwrap();
        let stored = JsonPayload::parse(&encoded).unwrap();
        assert_eq!(
            stored.raw_source_at("/provider_metadata/opaque").as_deref(),
            Some(r#""\ud800""#)
        );
        assert_eq!(
            stored
                .raw_source_at("/provider_metadata/llm_client/foreign")
                .as_deref(),
            Some(r#""\ud801""#)
        );
        assert_eq!(
            stored
                .raw_source_at("/provider_metadata/cache_creation/ephemeral_1h_input_tokens")
                .as_deref(),
            Some("4")
        );

        let decoded: ExecutionUsage = serde_json::from_str(&encoded).unwrap();
        assert_eq!(
            decoded
                .provider_metadata
                .raw_source_at("/opaque")
                .as_deref(),
            Some(r#""\ud800""#)
        );
        assert_eq!(
            decoded
                .provider_metadata
                .raw_source_at("/llm_client/foreign")
                .as_deref(),
            Some(r#""\ud801""#)
        );
    }

    #[test]
    fn native_resume_context_fact_is_not_persisted_as_usage_metadata() {
        let usage = ExecutionUsage {
            native_resume_context_exhausted: Some(false),
            native_resume_context_observed: true,
            ..Default::default()
        };
        let encoded = serde_json::to_value(&usage).expect("usage serializes");
        assert!(encoded.get("native_resume_context_exhausted").is_none());
        let restored: ExecutionUsage = serde_json::from_value(encoded).expect("usage restores");
        assert_eq!(restored.native_resume_context_exhausted, None);
        assert!(!restored.native_resume_context_observed);
    }
}
