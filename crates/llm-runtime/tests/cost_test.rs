//! Regression tests for cost test.

use llm_runtime::{
    CostEstimator, LlmError, PricingCatalog, PricingConfig, PricingModelRef, PricingOverride,
    PricingPolicy, ProviderId, TokenPricing,
};

fn pricing_model() -> PricingModelRef {
    PricingModelRef {
        pricing_provider_id: ProviderId::AnthropicFirstParty,
        billing_model: "claude-sonnet-4".to_string(),
        request_model: "claude-sonnet-4-20250514".to_string(),
        display_model: "Claude Sonnet 4".to_string(),
    }
}

fn usage() -> lingxi_llm_client::protocol::Usage {
    lingxi_llm_client::protocol::Usage {
        input_tokens: 1_000_000,
        output_tokens: 2_100_000,
        cache_write_tokens: 500_000,
        cache_read_tokens: 250_000,
        reasoning_tokens: 100_000,
        ..Default::default()
    }
}

fn fixed_price(input: f64, output: f64) -> TokenPricing {
    TokenPricing {
        cache_read_per_million: Some(0.0),
        cache_write_per_million: Some(0.0),
        cache_write_1h_per_million: Some(0.0),
        ..TokenPricing::input_output(input, output)
    }
}

#[test]
fn exact_price_match_computes_independent_token_bucket_costs() {
    let catalog = PricingCatalog::empty().with_price(
        ProviderId::AnthropicFirstParty,
        "claude-sonnet-4",
        TokenPricing {
            input_per_million: Some(3.0),
            output_per_million: Some(15.0),
            cache_write_per_million: Some(3.75),
            cache_write_1h_per_million: Some(3.75),
            cache_read_per_million: Some(0.30),
            reasoning_per_million: Some(15.0),
            ..Default::default()
        },
    );
    let estimator = CostEstimator::new(catalog, PricingPolicy::MarkUnestimated);

    let estimate = estimator
        .estimate(pricing_model(), &usage())
        .expect("estimate");

    assert!(estimate.estimated);
    assert_eq!(estimate.input_cost_usd, Some(3.0));
    assert_eq!(estimate.output_cost_usd, Some(30.0));
    assert_eq!(estimate.cache_write_cost_usd, Some(1.875));
    assert_eq!(estimate.cache_read_cost_usd, Some(0.075));
    assert_eq!(estimate.reasoning_cost_usd, Some(1.5));
    assert_eq!(estimate.total_cost_usd, Some(36.45));
}

#[test]
fn external_override_wins_over_builtin_price() {
    let catalog = PricingCatalog::empty()
        .with_price(
            ProviderId::AnthropicFirstParty,
            "claude-sonnet-4",
            fixed_price(3.0, 15.0),
        )
        .with_override(
            ProviderId::AnthropicFirstParty,
            "claude-sonnet-4",
            fixed_price(1.0, 2.0),
        );
    let estimator = CostEstimator::new(catalog, PricingPolicy::MarkUnestimated);

    let estimate = estimator
        .estimate(pricing_model(), &usage())
        .expect("estimate");

    assert_eq!(estimate.input_cost_usd, Some(1.0));
    assert_eq!(estimate.output_cost_usd, Some(4.0));
    assert_eq!(estimate.pricing_source.as_deref(), Some("override"));
}

#[test]
fn mark_unestimated_policy_returns_unestimated_cost_for_unknown_pricing() {
    let estimator = CostEstimator::new(PricingCatalog::empty(), PricingPolicy::MarkUnestimated);

    let estimate = estimator
        .estimate(pricing_model(), &usage())
        .expect("estimate");

    assert!(!estimate.estimated);
    assert_eq!(estimate.total_cost_usd, None);
    assert_eq!(estimate.pricing_model.billing_model, "claude-sonnet-4");
}

#[test]
fn require_priced_policy_returns_cost_unavailable_for_unknown_pricing() {
    let estimator = CostEstimator::new(PricingCatalog::empty(), PricingPolicy::RequirePriced);

    let error = estimator
        .estimate(pricing_model(), &usage())
        .expect_err("missing pricing should fail");

    assert!(matches!(error, LlmError::CostUnavailable { .. }));
}

#[test]
fn static_estimator_rejects_context_dependent_rules() {
    let mut pricing = fixed_price(1.0, 2.0);
    pricing
        .rules
        .push(lingxi_llm_client::protocol::PriceRule::default());
    let estimator = CostEstimator::new(
        PricingCatalog::empty().with_price(
            ProviderId::AnthropicFirstParty,
            "claude-sonnet-4",
            pricing,
        ),
        PricingPolicy::RequirePriced,
    );
    assert!(matches!(
        estimator.estimate(pricing_model(), &usage()),
        Err(LlmError::CostUnavailable { .. })
    ));

    let mut pricing = fixed_price(1.0, 2.0);
    pricing.quota.push(lingxi_llm_client::protocol::QuotaPrice {
        service_tier: lingxi_llm_client::protocol::ServiceTier::Standard,
        multiplier: 1.0,
        unit: "request".into(),
        source: "fixture".into(),
        verified_at: None,
    });
    let estimator = CostEstimator::new(
        PricingCatalog::empty().with_price(
            ProviderId::AnthropicFirstParty,
            "claude-sonnet-4",
            pricing,
        ),
        PricingPolicy::RequirePriced,
    );
    assert!(matches!(
        estimator.estimate(pricing_model(), &usage()),
        Err(LlmError::CostUnavailable { .. })
    ));
}

#[test]
fn unknown_cache_bucket_fails_only_when_consumed() {
    let pricing = TokenPricing::input_output(1.0, 2.0);
    let estimator = CostEstimator::new(
        PricingCatalog::empty().with_price(
            ProviderId::AnthropicFirstParty,
            "claude-sonnet-4",
            pricing,
        ),
        PricingPolicy::RequirePriced,
    );
    let mut observed = lingxi_llm_client::protocol::Usage {
        input_tokens: 1_000_000,
        ..Default::default()
    };
    assert_eq!(
        estimator
            .estimate(pricing_model(), &observed)
            .unwrap()
            .total_cost_usd,
        Some(1.0)
    );
    observed.cache_read_tokens = 1;
    assert!(matches!(
        estimator.estimate(pricing_model(), &observed),
        Err(LlmError::CostUnavailable { .. })
    ));
}

#[test]
fn explicit_zero_reasoning_rate_remains_free() {
    let mut pricing = fixed_price(1.0, 2.0);
    pricing.reasoning_per_million = Some(0.0);
    let estimator = CostEstimator::new(
        PricingCatalog::empty().with_price(
            ProviderId::AnthropicFirstParty,
            "claude-sonnet-4",
            pricing,
        ),
        PricingPolicy::RequirePriced,
    );
    let observed = lingxi_llm_client::protocol::Usage {
        output_tokens: 1_000_000,
        reasoning_tokens: 1_000_000,
        ..Default::default()
    };
    let estimate = estimator.estimate(pricing_model(), &observed).unwrap();
    assert_eq!(estimate.output_cost_usd, Some(0.0));
    assert_eq!(estimate.reasoning_cost_usd, Some(0.0));
}

#[test]
fn catalog_provenance_wins_over_embedded_price_source() {
    let mut pricing = fixed_price(1.0, 2.0);
    pricing.source = Some("external-label".into());
    let estimator = CostEstimator::new(
        PricingCatalog::empty().with_price(
            ProviderId::AnthropicFirstParty,
            "claude-sonnet-4",
            pricing,
        ),
        PricingPolicy::RequirePriced,
    );
    assert_eq!(
        estimator
            .estimate(pricing_model(), &usage())
            .unwrap()
            .pricing_source
            .as_deref(),
        Some("builtin")
    );
}

// ── Task 2: add_override + PricingConfig serde tests ─────────────────────────

/// `add_override` (mutable builder) produces the same result as the existing
/// `with_override` (consuming builder) — both override wins over builtin.
#[test]
fn add_override_wins_over_builtin_price() {
    let mut catalog = PricingCatalog::empty().with_price(
        ProviderId::AnthropicFirstParty,
        "claude-sonnet-4",
        fixed_price(3.0, 15.0),
    );
    // add_override applied after construction — should shadow the builtin.
    catalog.add_override(
        ProviderId::AnthropicFirstParty,
        "claude-sonnet-4",
        fixed_price(1.5, 7.0),
    );
    let estimator = CostEstimator::new(catalog, PricingPolicy::MarkUnestimated);

    let usage_1m = lingxi_llm_client::protocol::Usage {
        input_tokens: 1_000_000,
        output_tokens: 1_000_000,
        ..Default::default()
    };
    let estimate = estimator
        .estimate(pricing_model(), &usage_1m)
        .expect("estimate");

    assert_eq!(estimate.pricing_source.as_deref(), Some("override"));
    // 1M input × $1.5/M = $1.5 + 1M output × $7.0/M = $7.0 → $8.5
    let total = estimate.total_cost_usd.expect("total must be Some");
    assert!(
        (total - 8.5).abs() < 1e-9,
        "total must be $8.5, got ${total}"
    );
    let input = estimate.input_cost_usd.expect("input must be Some");
    assert!(
        (input - 1.5).abs() < 1e-9,
        "input must be $1.5/M, got ${input}"
    );
}

/// `PricingConfig` with overrides round-trips through JSON serde.
#[test]
fn pricing_config_with_overrides_serde_roundtrip() {
    let tp = PricingOverride {
        input_per_million: 1.5,
        output_per_million: 6.0,
        cache_write_per_million: 1.875,
        cache_read_per_million: 0.15,
        reasoning_per_million: 6.0,
    };
    let cfg = PricingConfig {
        billing_mode: Default::default(),
        require_priced: false,
        overrides: vec![("my-model".to_string(), tp)],
    };

    let json = serde_json::to_string(&cfg).expect("serialize");
    let back: PricingConfig = serde_json::from_str(&json).expect("deserialize");

    assert!(!back.require_priced);
    assert_eq!(back.overrides.len(), 1);
    let (model_id, tp_back) = &back.overrides[0];
    assert_eq!(model_id, "my-model");
    assert!((tp_back.input_per_million - 1.5).abs() < 1e-12);
    assert!((tp_back.output_per_million - 6.0).abs() < 1e-12);
    assert!((tp_back.cache_write_per_million - 1.875).abs() < 1e-12);
    assert!((tp_back.cache_read_per_million - 0.15).abs() < 1e-12);
    assert!((tp_back.reasoning_per_million - 6.0).abs() < 1e-12);
}

/// `PricingConfig` with absent overrides field (e.g. from old configs) deserializes
/// with an empty overrides vec — no serde errors, no regression.
#[test]
fn pricing_config_absent_overrides_deserializes_to_empty() {
    // The old config shape only had `require_priced`.
    let json = r#"{"require_priced": false}"#;
    let cfg: PricingConfig = serde_json::from_str(json).expect("must deserialize");
    assert!(
        cfg.overrides.is_empty(),
        "absent overrides must default to empty"
    );

    // Also the default-derived shape must have an empty overrides.
    let default_cfg = PricingConfig::default();
    assert!(default_cfg.overrides.is_empty());
}

/// The settings override JSON retains its camelCase field names.
#[test]
fn token_pricing_serde_roundtrip_camel_case() {
    let json = r#"{
        "inputPerMtok": 3.0,
        "outputPerMtok": 15.0,
        "cacheWritePerMtok": 3.75,
        "cacheReadPerMtok": 0.30,
        "reasoningPerMtok": 1.5
    }"#;
    let tp: PricingOverride = serde_json::from_str(json).expect("deserialize");
    assert!((tp.input_per_million - 3.0).abs() < 1e-12);
    assert!((tp.output_per_million - 15.0).abs() < 1e-12);
    assert!((tp.cache_write_per_million - 3.75).abs() < 1e-12);
    assert!((tp.cache_read_per_million - 0.30).abs() < 1e-12);
    assert!((tp.reasoning_per_million - 1.5).abs() < 1e-12);

    // Serialized form must use camelCase keys.
    let back = serde_json::to_string(&tp).expect("serialize");
    assert!(
        back.contains("inputPerMtok"),
        "must serialize to camelCase: {back}"
    );
    assert!(
        back.contains("outputPerMtok"),
        "must serialize to camelCase: {back}"
    );
}

/// Omitted reasoning uses the output rate; omitted cache buckets remain free.
#[test]
fn token_pricing_omitted_reasoning_defaults_to_output() {
    let json = r#"{"inputPerMtok": 2.0, "outputPerMtok": 8.0}"#;
    let tp: PricingOverride = serde_json::from_str(json).expect("deserialize");
    assert!((tp.cache_write_per_million - 0.0).abs() < 1e-12);
    assert!((tp.cache_read_per_million - 0.0).abs() < 1e-12);
    assert!((tp.reasoning_per_million - 8.0).abs() < 1e-12);
    assert_eq!(tp, PricingOverride::input_output(2.0, 8.0));
}

#[test]
fn token_pricing_explicit_zero_reasoning_survives_serde() {
    let json = r#"{"inputPerMtok": 2.0, "outputPerMtok": 8.0, "reasoningPerMtok": 0.0}"#;
    let tp: PricingOverride = serde_json::from_str(json).expect("deserialize");
    assert_eq!(tp.reasoning_per_million, 0.0);
    let roundtrip: PricingOverride =
        serde_json::from_value(serde_json::to_value(tp).unwrap()).unwrap();
    assert_eq!(roundtrip, tp);
}

#[tokio::test]
async fn deepseek_override_is_fixed_across_peak_hours_without_changing_sibling_prices() {
    use lingxi_llm_client::protocol::{
        InferenceReport, ServiceTier, Submission, UsageReport, UsageState,
    };
    use llm_runtime::{client::ModelRuntime, ClientConfig, CredentialConfig, LlmRequest};

    let mut profile = llm_runtime::builtin_presets()
        .providers
        .into_iter()
        .find(|profile| profile.profile_name == "deepseek")
        .unwrap();
    let model = profile
        .models
        .iter()
        .find(|model| model.request_model == "deepseek-flash")
        .unwrap()
        .clone();
    let sibling = profile
        .models
        .iter()
        .find(|sibling| sibling.request_model != model.request_model)
        .unwrap()
        .clone();
    profile.pricing.overrides.push((
        model.display_model.clone(),
        PricingOverride::input_output(2.0, 8.0),
    ));
    std::env::set_var("LLM_DEEPSEEK_OVERRIDE_TEST_KEY", "test-key");
    profile.credential = CredentialConfig::Env {
        var: "LLM_DEEPSEEK_OVERRIDE_TEST_KEY".into(),
    };
    let client = ModelRuntime::from_config(ClientConfig {
        providers: vec![profile],
    })
    .unwrap();
    let report = UsageReport::measured(
        lingxi_llm_client::protocol::Usage {
            output_tokens: 1_000_000,
            ..Default::default()
        },
        UsageState::Complete,
    );
    let mut sibling_costs = Vec::new();
    for selected in [&model, &sibling] {
        let request = LlmRequest::new(&selected.display_model).with_user_text("hello");
        let prepared = client.prepare(&request).await.unwrap();
        let snapshot = prepared.pricing_snapshot().unwrap();
        for executed_at in [1_790_208_000, 1_790_215_200] {
            let estimate = snapshot
                .estimate(
                    &report,
                    &InferenceReport {
                        executed_at: Some(executed_at),
                        service_tier: Some(ServiceTier::Standard),
                        ..Default::default()
                    },
                    Submission::Interactive,
                )
                .unwrap();
            if selected.request_model == model.request_model {
                assert_eq!(estimate.total_cost, 8.0);
            } else {
                sibling_costs.push(estimate.total_cost);
            }
        }
        if selected.request_model == model.request_model {
            assert!(
                snapshot
                    .estimate(
                        &report,
                        &InferenceReport {
                            executed_at: Some(1_790_208_000),
                            requested_service_tier: Some(ServiceTier::Fast),
                            ..Default::default()
                        },
                        Submission::Interactive,
                    )
                    .is_err(),
                "an override cannot turn unknown Fast execution into Standard pricing"
            );
        }
    }
    assert!(sibling_costs[1] > 0.0);
    assert_eq!(sibling_costs[0] * 2.0, sibling_costs[1]);
}

#[tokio::test]
async fn api_service_override_prices_omitted_and_explicit_zero_reasoning() {
    use llm_runtime::{
        client::ModelRuntime, parse_provider_profiles_strict, pricing_provider_id_for_profile,
        ApiService, BoxFuture, ClientConfig, ProviderParseOptions, ProviderRequest,
        ProviderResponse, StreamingResponse,
    };
    use std::sync::{atomic::AtomicUsize, atomic::Ordering, Arc};

    struct ResponseTransport(AtomicUsize);
    impl llm_runtime::test_support::FixtureTransport for ResponseTransport {
        fn execute<'a>(
            &'a self,
            _: &'a ProviderRequest,
        ) -> BoxFuture<'a, Result<ProviderResponse, LlmError>> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Box::pin(async {
                Ok(ProviderResponse::json(
                    200,
                    serde_json::json!({
                        "id": "reasoning-override",
                        "model": "custom",
                        "choices": [{
                            "index": 0,
                            "message": {"role": "assistant", "content": "done"},
                            "finish_reason": "stop"
                        }],
                        "usage": {
                            "prompt_tokens": 0,
                            "completion_tokens": 40000,
                            "total_tokens": 40000,
                            "completion_tokens_details": {"reasoning_tokens": 30000}
                        }
                    }),
                ))
            })
        }

        fn open_stream<'a>(
            &'a self,
            _: &'a ProviderRequest,
        ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>> {
            unreachable!("nonstream pricing test")
        }
    }
    llm_runtime::impl_fixture_transport!(ResponseTransport);

    std::env::set_var("LLM_REASONING_OVERRIDE_TEST_KEY", "test-key");
    for (reasoning, expected) in [(None, 0.32), (Some(0.0), 0.08), (Some(3.0), 0.17)] {
        let mut pricing = serde_json::json!({"inputPerMtok": 2.0, "outputPerMtok": 8.0});
        if let Some(reasoning) = reasoning {
            pricing["reasoningPerMtok"] = serde_json::json!(reasoning);
        }
        let providers = serde_json::json!({"reasoning-test": {
            "type": "openai",
            "baseUrl": "https://example.com/v1",
            "apiKeyEnv": "LLM_REASONING_OVERRIDE_TEST_KEY",
            "models": [{"id": "custom"}],
            "pricing": {"custom": pricing}
        }});
        let profile = parse_provider_profiles_strict(
            &serde_json::from_value(providers).unwrap(),
            ProviderParseOptions::strict_env(),
        )
        .expect("parse provider")
        .remove(0)
        .profile;
        let provider = pricing_provider_id_for_profile(&profile.profile_name, &profile.provider_id);
        let catalog = PricingCatalog::empty().with_override(
            provider,
            "custom",
            profile.pricing.overrides[0].1.to_sdk(),
        );
        let client = ModelRuntime::from_config(ClientConfig {
            providers: vec![profile],
        })
        .expect("client");
        let transport = Arc::new(ResponseTransport(AtomicUsize::new(0)));
        let service = ApiService::new_with_estimator(
            Arc::new(client),
            transport.clone(),
            Default::default(),
            Default::default(),
            "test",
            None,
            None,
            Some(Arc::new(CostEstimator::new(
                catalog,
                PricingPolicy::RequirePriced,
            ))),
        );
        let response = service
            .messages_create(llm_runtime::MessagesCreateRequest::new(
                "custom",
                Some("reasoning-test"),
                None,
                vec![],
                vec![],
            ))
            .await
            .expect("decoded response with SDK quote");
        assert_eq!(response.usage.counts().output_tokens, 40_000);
        assert_eq!(response.usage.counts().reasoning_tokens, 30_000);
        let cost = response.cost.expect("SDK cost");
        assert!(cost.estimated);
        assert!((cost.total_cost_usd.unwrap() - expected).abs() < 1e-12);
        assert_eq!(transport.0.load(Ordering::Relaxed), 1);
    }
}
