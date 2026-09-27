use llm_runtime::{CostEstimate, PricingModelRef, ProviderId, TokenUsage, Usage};

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
fn token_usage_defaults_keep_billable_buckets_independent() {
    let usage = Usage {
        billable_tokens: TokenUsage {
            input: 10,
            output: 7,
            cache_write: 3,
            cache_read: 5,
            reasoning_output: 2,
        },
        context_tokens: Some(30),
        provider_reported_total_tokens: Some(99),
        ..Usage::default()
    };

    assert_eq!(usage.billable_tokens.input, 10);
    assert_eq!(usage.billable_tokens.output, 7);
    assert_eq!(usage.billable_tokens.cache_write, 3);
    assert_eq!(usage.billable_tokens.cache_read, 5);
    assert_eq!(usage.billable_tokens.reasoning_output, 2);
    assert_eq!(usage.context_tokens, Some(30));
    assert_eq!(usage.provider_reported_total_tokens, Some(99));
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
