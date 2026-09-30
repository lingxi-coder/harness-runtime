//! Regression tests for types test.

use llm_runtime::{CostEstimate, ExecutionUsage, PricingModelRef, ProviderId};

#[test]
fn provider_id_serializes_first_class_variants() {
    let provider = ProviderId::OpenAICompatible {
        name: "openrouter".to_string(),
    };

    let json = serde_json::to_value(&provider).expect("serialize provider id");

    assert_eq!(
        json,
        serde_json::json!({"open_ai_compatible":{"name":"openrouter"}})
    );
}

#[test]
fn execution_usage_keeps_sdk_counters_and_counts_reasoning_once() {
    let usage = ExecutionUsage {
        report: lingxi_llm_client::protocol::UsageReport::measured(
            lingxi_llm_client::protocol::Usage {
                input_tokens: 10,
                output_tokens: 9,
                cache_write_tokens: 3,
                cache_read_tokens: 5,
                reasoning_tokens: 2,

                ..Default::default()
            },
            lingxi_llm_client::protocol::UsageState::Complete,
        ),

        ..ExecutionUsage::default()
    };

    assert_eq!(usage.counts().input_tokens, 10);
    assert_eq!(usage.counts().output_tokens, 9);
    assert_eq!(usage.counts().cache_write_tokens, 3);
    assert_eq!(usage.counts().cache_read_tokens, 5);
    assert_eq!(usage.counts().reasoning_tokens, 2);
    assert_eq!(usage.context_tokens(), Some(27));
    assert_eq!(
        usage.report.state,
        lingxi_llm_client::protocol::UsageState::Complete
    );
}

#[test]
fn cost_estimate_can_mark_unknown_pricing_without_dropping_usage() {
    let estimate = CostEstimate::unestimated(PricingModelRef {
        pricing_provider_id: ProviderId::AnthropicFirstParty,
        billing_model: "unknown-model".to_string(),
        request_model: "unknown-model".to_string(),
        display_model: "Unknown Model".to_string(),
    });

    assert!(!estimate.estimated);
    assert_eq!(estimate.total_cost_usd, None);
    assert_eq!(estimate.pricing_model.billing_model, "unknown-model");
}

#[test]
fn missing_usage_stays_unknown_until_a_partial_counter_is_observed() {
    let mut usage = ExecutionUsage::default();
    usage.provider_metadata =
        serde_json::json!({"input_tokens": 999, "upstreamUsageState":"complete"});
    assert_eq!(usage.context_tokens(), None);
    assert!(usage.report.complete().is_none());
    assert_eq!(usage.counts().input_tokens, 0);
    usage.counts_mut().input_tokens = 12;
    assert_eq!(
        usage.report.state,
        lingxi_llm_client::protocol::UsageState::Partial
    );
    assert_eq!(usage.context_tokens(), Some(12));
    assert!(usage.report.complete().is_none());
    let serialized = serde_json::to_value(&usage).unwrap();
    assert_eq!(serialized["billable_tokens"]["input"], 12);
    assert_eq!(
        serialized["provider_metadata"]["upstreamUsageState"],
        "partial"
    );
}

#[test]
fn persisted_usage_round_trips_partial_state_fast_tier_and_historical_totals() {
    use lingxi_llm_client::protocol::{ServiceTier, UsageState};
    let historical = serde_json::json!({
        "billable_tokens": {"input":10,"output":7,"cache_write":3,"cache_read":5,"reasoning_output":2},
        "context_tokens":30,
        "provider_reported_total_tokens":99,
        "server_tool_use":{"web_search_requests":2},
        "provider_metadata":{"upstreamUsageState":"partial","cache_creation":{"ephemeral_1h_input_tokens":1}},
        "speed":"fast"
    });
    let usage: ExecutionUsage = serde_json::from_value(historical.clone()).unwrap();
    assert_eq!(usage.counts().output_tokens, 9);
    assert_eq!(usage.counts().reasoning_tokens, 2);
    assert_eq!(usage.counts().total(), 27);
    assert_eq!(usage.report.state, UsageState::Partial);
    assert_eq!(usage.inference.service_tier, Some(ServiceTier::Fast));
    assert_eq!(usage.context_tokens(), Some(30));
    assert_eq!(usage.provider_reported_total_tokens, Some(99));
    assert_eq!(
        usage.server_tool_usage().unwrap().web_search_requests,
        Some(2)
    );
    assert_eq!(usage.counts().cache_write_1h_tokens, 1);
    assert!(usage.cost_estimate.is_none());
    let serialized = serde_json::to_value(&usage).unwrap();
    for key in [
        "billable_tokens",
        "context_tokens",
        "provider_reported_total_tokens",
        "server_tool_use",
        "speed",
    ] {
        assert_eq!(serialized[key], historical[key], "historical field {key}");
    }
    assert_eq!(
        serialized["provider_metadata"]["upstreamUsageState"],
        historical["provider_metadata"]["upstreamUsageState"]
    );
    assert_eq!(
        serialized["provider_metadata"]["cache_creation"],
        historical["provider_metadata"]["cache_creation"]
    );
    let restored: ExecutionUsage = serde_json::from_value(serialized).unwrap();
    assert_eq!(restored.report, usage.report);
    assert_eq!(restored.inference, usage.inference);
}

#[test]
fn persisted_extension_round_trips_sdk_only_usage_and_inference() {
    use lingxi_llm_client::protocol::{
        InferenceReport, ReasoningEffort, ReportedCost, ServerToolUsage, ServiceTier, Usage,
        UsageReport, UsageState,
    };
    for report in [
        UsageReport::default(),
        UsageReport::measured(
            Usage {
                input_tokens: 11,
                output_tokens: 13,
                reasoning_tokens: 5,
                cache_write_tokens: 7,
                cache_write_1h_tokens: 3,
                cost: Some(ReportedCost { nano_usd: 987_654 }),
                server_tool_usage: Some(ServerToolUsage {
                    web_search_requests: Some(1),
                    web_fetch_requests: Some(2),
                    file_search_requests: Some(3),
                    code_interpreter_requests: Some(4),
                    web_extractor_requests: Some(5),
                }),
                ..Default::default()
            },
            UsageState::Partial,
        ),
    ] {
        let usage = ExecutionUsage {
            report: report.clone(),
            inference: InferenceReport {
                executed_at: Some(123),
                service_tier: Some(ServiceTier::Fast),
                requested_effort: Some(ReasoningEffort::High),
                raw_service_tier: Some("priority".into()),
                ..Default::default()
            },
            provider_metadata: serde_json::json!({"llm_client":{"history_usage":{"schema":0,"report":{"state":"complete"}}}}),
            cost_estimate: Some(CostEstimate::unestimated(PricingModelRef {
                pricing_provider_id: ProviderId::OpenAI,
                billing_model: "m".into(),
                request_model: "m".into(),
                display_model: "m".into(),
            })),
            ..Default::default()
        };
        let serialized = serde_json::to_value(&usage).unwrap();
        assert!(serialized.get("cost_estimate").is_none());
        let restored: ExecutionUsage = serde_json::from_value(serialized).unwrap();
        assert_eq!(restored.report, report);
        assert_eq!(restored.inference, usage.inference);
        assert!(restored.cost_estimate.is_none());
    }
}

#[test]
fn persisted_zero_cache_snapshot_clears_stale_legacy_ttl_metadata() {
    let usage = ExecutionUsage {
        report: lingxi_llm_client::protocol::UsageReport::measured(
            lingxi_llm_client::protocol::Usage::default(),
            lingxi_llm_client::protocol::UsageState::Complete,
        ),
        provider_metadata: serde_json::json!({
            "cache_creation": {"ephemeral_1h_input_tokens": 18}
        }),
        ..Default::default()
    };
    let serialized = serde_json::to_value(&usage).unwrap();
    assert_eq!(
        serialized["provider_metadata"]["cache_creation"]["ephemeral_1h_input_tokens"],
        0
    );
    let restored: ExecutionUsage = serde_json::from_value(serialized).unwrap();
    assert_eq!(restored.report, usage.report);
}
