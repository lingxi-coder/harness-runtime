//! Attempt pricing and usage publication.

use crate::service::ApiService;
use crate::{HistoryEvent, LlmError};

pub(super) fn frozen_stream_quote(
    pricing: Option<&lingxi_llm_client::FrozenPricing>,
    pricing_model: &crate::PricingModelRef,
    usage: &lingxi_llm_client::protocol::UsageReport,
    inference: &lingxi_llm_client::protocol::InferenceReport,
) -> Option<crate::CostEstimate> {
    let estimate = pricing?
        .estimate(
            usage,
            inference,
            lingxi_llm_client::protocol::Submission::default(),
        )
        .ok()?;
    crate::cost::project_estimate(estimate, pricing_model.clone()).ok()
}

fn server_fallback_quote_envelope(
    quote: Option<&lingxi_llm_client::AnthropicFallbackCostQuote>,
    summary_model: Option<&str>,
    complete: bool,
    reason: Option<&str>,
) -> lingxi_llm_client::protocol::JsonPayload {
    let mut value = serde_json::json!({
        "kind": "anthropic_server_fallback_per_iteration",
        "completeness": if complete { "complete" } else { "incomplete" },
        "summaryModel": summary_model,
        "quote": quote.and_then(|quote| serde_json::to_value(quote).ok()),
    });
    if let Some(reason) = reason {
        value["reason"] = serde_json::Value::String(reason.into());
    }
    lingxi_llm_client::protocol::JsonPayload::from_generated_value(&value)
        .expect("host-generated fallback quote metadata is serializable")
}

pub(super) struct ServerFallbackQuoteProjection {
    pub(super) estimate: Option<crate::CostEstimate>,
    pub(super) metadata: lingxi_llm_client::protocol::JsonPayload,
    pub(super) summary_model: Option<String>,
}

/// `Some` means native cNe's served-fallback branch ran. The estimate may still
/// be absent when the frozen profile cannot price every native component; that
/// must suppress the ordinary dispatched-model aggregate estimate.
pub(super) fn frozen_server_fallback_quote(
    pricing: Option<&lingxi_llm_client::FrozenPricing>,
    pricing_model: &crate::PricingModelRef,
    lane_model: &str,
    fallback: Option<&lingxi_llm_client::providers::anthropic::fallback_response::FallbackResponse>,
    inference: &lingxi_llm_client::protocol::InferenceReport,
) -> Option<ServerFallbackQuoteProjection> {
    use lingxi_llm_client::protocol::Submission;

    let facts = fallback?;
    let iterations = facts.iterations.as_ref()?;
    // This is the native cNe branch predicate. Some("") is intentionally
    // distinct from no model-bearing fallback iteration.
    iterations.served_fallback_model.as_ref()?;
    let resolved_summary_model = lingxi_llm_client::resolve_anthropic_server_fallback_summary_model(
        Some(lane_model),
        iterations,
    );
    let stop_reason = facts.final_stop_reason.as_deref();
    let Some(pricing) = pricing else {
        return Some(ServerFallbackQuoteProjection {
            estimate: None,
            metadata: server_fallback_quote_envelope(
                None,
                resolved_summary_model.as_deref(),
                false,
                Some("frozen_pricing_unavailable"),
            ),
            summary_model: resolved_summary_model,
        });
    };
    match pricing.estimate_anthropic_server_fallback(
        iterations,
        Some(lane_model),
        stop_reason,
        inference,
        Submission::default(),
    ) {
        Ok(Some(quote)) => {
            let summary_model = Some(quote.summary_model.clone());
            let estimate = quote
                .clone()
                .into_cost_estimate_if_complete()
                .and_then(|estimate| {
                    crate::cost::project_fallback_estimate(estimate, pricing_model).ok()
                });
            let complete = estimate.is_some();
            let reason = (!complete).then_some("native_quote_incomplete");
            Some(ServerFallbackQuoteProjection {
                estimate,
                metadata: server_fallback_quote_envelope(
                    Some(&quote),
                    summary_model.as_deref(),
                    complete,
                    reason,
                ),
                summary_model,
            })
        }
        Ok(None) => Some(ServerFallbackQuoteProjection {
            estimate: None,
            metadata: server_fallback_quote_envelope(
                None,
                resolved_summary_model.as_deref(),
                false,
                Some("native_quote_unavailable"),
            ),
            summary_model: resolved_summary_model,
        }),
        Err(_) => Some(ServerFallbackQuoteProjection {
            estimate: None,
            metadata: server_fallback_quote_envelope(
                None,
                resolved_summary_model.as_deref(),
                false,
                Some("native_quote_failed"),
            ),
            summary_model: resolved_summary_model,
        }),
    }
}

/// A stream may expose iteration facts before its terminal stop reason and
/// final iteration array arrive. Publish only a retractable marker at that
/// point; pricing waits for the terminal snapshot so refusal exclusion and
/// explicit empty-array replacement use the final facts.
pub(super) fn server_fallback_quote_candidate(
    lane_model: &str,
    fallback: Option<&lingxi_llm_client::providers::anthropic::fallback_response::FallbackResponse>,
) -> Option<ServerFallbackQuoteProjection> {
    let iterations = fallback?.iterations.as_ref()?;
    iterations.served_fallback_model.as_ref()?;
    let summary_model = lingxi_llm_client::resolve_anthropic_server_fallback_summary_model(
        Some(lane_model),
        iterations,
    );
    Some(ServerFallbackQuoteProjection {
        estimate: None,
        metadata: server_fallback_quote_envelope(
            None,
            summary_model.as_deref(),
            false,
            Some("awaiting_terminal_fallback_facts"),
        ),
        summary_model,
    })
}

pub(super) fn attach_frozen_stream_quote(
    events: &mut [HistoryEvent],
    quote: Option<&crate::CostEstimate>,
) {
    for event in events {
        if let HistoryEvent::MessageDelta {
            usage: Some(usage), ..
        } = event
        {
            usage.cost_estimate = quote.cloned();
        }
    }
}

/// Prompt-free facts from the final prepared request. Defaults are recorded
/// only for a documented native endpoint/model; other defaults remain unknown.
pub(super) fn json_payload_u64_at(
    payload: &lingxi_llm_client::protocol::JsonPayload,
    pointer: &str,
) -> Option<u64> {
    payload.raw_source_at(pointer)?.parse().ok()
}

pub(super) fn prepared_output_budget_observation(
    prepared: &crate::PreparedLlmCall,
    request: &lingxi_llm_client::protocol::ChatRequest,
) -> serde_json::Value {
    let body = &prepared.provider_request.body_json;
    let mut field = None;
    let mut value = None;
    for key in ["max_tokens", "max_completion_tokens", "max_output_tokens"] {
        let pointer = format!("/{key}");
        if let Some(n) = json_payload_u64_at(body, &pointer) {
            field = Some(key);
            value = Some(n);
        }
    }
    for (parent, key) in [
        ("generationConfig", "maxOutputTokens"),
        ("generation_config", "max_output_tokens"),
    ] {
        let pointer = format!("/{parent}/{key}");
        if let Some(n) = json_payload_u64_at(body, &pointer) {
            field = Some(key);
            value = Some(n);
        }
    }
    let model = &prepared.route.resolved_route.request_model;
    let is_current_deepseek = prepared.route.protocol == crate::ProtocolFamily::OpenAiChat
        && url::Url::parse(&prepared.provider_request.url)
            .ok()
            .is_some_and(|u| u.host_str() == Some("api.deepseek.com"))
        && matches!(model.as_str(), "deepseek-flash" | "deepseek-v4-pro");
    let default = if value.is_none() && is_current_deepseek {
        Some(
            if body.string_at("/reasoning_effort").as_deref() == Some("none")
                || body.string_at("/thinking/type").as_deref() == Some("disabled")
            {
                8192
            } else if body.string_at("/reasoning_effort").as_deref() == Some("max") {
                131072
            } else {
                65536
            },
        )
    } else {
        None
    };
    let hosted_wire_tools = body.array_len("/tools").is_some_and(|length| {
        (0..length).any(|index| {
            let Some(tool) = body.array_item_at("/tools", index) else {
                return false;
            };
            tool.string_at("/type")
                .is_some_and(|kind| !matches!(kind.as_str(), "function" | "custom"))
                || tool
                    .string_at("/function/name")
                    .is_some_and(|name| name.starts_with('$'))
        })
    });
    let native_search = ["enable_search", "web_search"]
        .iter()
        .any(|key| body.bool_at(&format!("/{key}")) == Some(true));
    serde_json::json!({"field":field,"value":value,"provider_default":default,
        "can_repeat_original":request.hosted_tools.is_empty() && request.native_options.is_empty() && !hosted_wire_tools && !native_search,
        "source":if request.max_tokens.is_some() {"explicit"} else if value.is_some() {"application_default"} else {"provider_default"},
        "profile":prepared.route.resolved_route.profile_name,"model":model})
}

impl ApiService {
    /// Capture the configured pricing policy for one exact provider profile.
    /// Unlike diagnostic model metadata, this preserves explicit override
    /// declarations. It exposes no credentials and does not resolve a price.
    #[must_use]
    pub fn profile_pricing_config(&self, profile: &str) -> Option<crate::PricingConfig> {
        self.client().profile_pricing_config(profile)
    }

    /// Conservative rate ceilings for a model with contextual or scheduled prices.
    /// These authorize a budget; only a frozen execution quote may settle it.
    pub fn attempt_price_bounds(
        &self,
        route: &crate::ResolvedRoute,
    ) -> Result<Option<crate::AttemptPriceBounds>, LlmError> {
        self.client().attempt_price_bounds(route)
    }
}
