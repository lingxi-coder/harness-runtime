//! Translation helpers between `llm_runtime::ExecutionUsage` and
//! `cost::Usage` plus model-string → `ProviderId` resolution.
//!
//! Also contains the bridge that populates an `llm_runtime::PricingCatalog` from
//! the `cost::PricingCatalog` so `HistoryResponse.cost` carries real estimates.
//!
//! Used by M6-06 to feed `HistoryResponse.usage` into `CostTracker`.

use cost::pricing::{ProviderId, TokenClass};
use cost::usage::{ApiSpeed, ServerToolUsage, TokenUsage, Usage};
use cost::ModelRef;
use llm_runtime::ExecutionUsage as LlmUsage;

/// Adapt canonical SDK counters to the host cost ledger's disjoint buckets.
/// Provider metadata is presentation data and never participates in billing.
#[must_use]
pub(crate) fn llm_usage_to_cost_usage(usage: &LlmUsage) -> Usage {
    let counts = usage.counts();
    Usage {
        tokens: TokenUsage {
            input: counts.input_tokens,
            output: counts.output_tokens.saturating_sub(counts.reasoning_tokens),
            cache_write: counts
                .cache_write_tokens
                .saturating_sub(counts.cache_write_1h_tokens),
            cache_read: counts.cache_read_tokens,
            reasoning_output: counts.reasoning_tokens,
            cache_write_1h: counts.cache_write_1h_tokens,
        },
        server_tool_use: counts
            .server_tool_usage
            .and_then(|s| s.web_search_requests)
            .map(|count| ServerToolUsage {
                web_search_requests: u32::try_from(count).unwrap_or(u32::MAX),
            }),
        speed: usage.inference.service_tier.map(|tier| {
            if tier == llm_runtime::services::sdk::protocol::ServiceTier::Fast {
                ApiSpeed::Fast
            } else {
                ApiSpeed::Standard
            }
        }),
    }
}

/// Validate the SDK report before adapting its observations for settlement.
pub fn llm_usage_to_cost_usage_checked(usage: &LlmUsage) -> Result<Usage, cost::AttemptFoldError> {
    use cost::AttemptFoldError;
    use llm_runtime::services::sdk::protocol::UsageState;
    if matches!(
        usage.report.state,
        UsageState::Invalid | UsageState::Missing
    ) {
        return Err(AttemptFoldError::Invalid(
            "usage report has no valid measurement",
        ));
    }
    let counts = usage
        .report
        .usage
        .ok_or(AttemptFoldError::Invalid("usage report has no counters"))?;
    if counts.reasoning_tokens > counts.output_tokens
        || counts.cache_write_1h_tokens > counts.cache_write_tokens
    {
        return Err(AttemptFoldError::Invalid("usage subset exceeds its total"));
    }
    if let Some(count) = counts
        .server_tool_usage
        .and_then(|tools| tools.web_search_requests)
    {
        u32::try_from(count).map_err(|_| AttemptFoldError::Arithmetic)?;
    }
    Ok(llm_usage_to_cost_usage(usage))
}

fn llm_provider_to_cost_provider(provider: &llm_runtime::ProviderId) -> ProviderId {
    match provider {
        llm_runtime::ProviderId::AnthropicFirstParty => ProviderId::Anthropic,
        llm_runtime::ProviderId::OpenAI | llm_runtime::ProviderId::AzureOpenAI => {
            ProviderId::OpenAI
        }
        llm_runtime::ProviderId::Gemini
        | llm_runtime::ProviderId::VertexGemini
        | llm_runtime::ProviderId::VertexClaude => ProviderId::GoogleGemini,
        llm_runtime::ProviderId::BedrockClaude => ProviderId::AmazonBedrock,
        // Foundry hosts Claude models — attribute cost/telemetry to Anthropic.
        llm_runtime::ProviderId::FoundryClaude => ProviderId::Anthropic,
        llm_runtime::ProviderId::OpenAICompatible { name } => {
            ProviderId::OpenAICompatible { name: name.clone() }
        }
        llm_runtime::ProviderId::Custom { name } => ProviderId::Custom { name: name.clone() },
    }
}

/// Build a fully-qualified [`ModelRef`] for cost + telemetry attribution.
///
/// Prefer the LIVE session `profile` when known: `model` is the bare wire id and
/// the real provider lives in `session.model_profile`. Falling back to
/// [`llm_runtime::split_profile_model`] (the `profile = None` path) only sees the
/// bare id and mis-infers `anthropic` for every non-`claude-` bare id (deepseek,
/// `gpt-*`, `gemini-*`) AND for provider-shared `claude-*` ids (e.g. Copilot /
/// Bedrock Claude) — misattributing the cost and the `tengu_api_success`
/// `provider` tag to Anthropic. String-parsing is kept only as the no-profile
/// fallback (e.g. an explicit `openai/gpt-4o` reference).
#[must_use]
pub fn model_ref_from_string(model: &str, profile: Option<&str>) -> ModelRef {
    let (profile, bare) = match profile {
        Some(p) => (p.to_string(), model.to_string()),
        // No LIVE profile (e.g. a cross-provider `--resume` cleared it): recover
        // the REAL provider by resolving the bare wire id against the catalog
        // before falling back to string-parsing (which mis-infers `anthropic`
        // for every bare non-`claude-` id, misattributing cost + the
        // `tengu_api_success` provider tag).
        None => match crate::provider_adapter::provider_for_model(model) {
            Some(p) => (p, model.to_string()),
            // A shared wire id such as deepseek-flash can mean metered chat
            // or a different connection with distinct billing. If session
            // provenance was lost, charge the unknown tier and flag it instead
            // of fabricating a first-party Anthropic price.
            None if crate::provider_adapter::model_has_ambiguous_profile(model)
                && !model.starts_with("claude-") =>
            {
                return ModelRef {
                    provider: ProviderId::Custom {
                        name: "unknown-profile".into(),
                    },
                    model: model.to_string(),
                };
            }
            None => llm_runtime::split_profile_model(model),
        },
    };
    let pricing_provider = llm_runtime::pricing_provider_id_for_profile(
        &profile,
        &llm_runtime::ProviderId::OpenAICompatible {
            name: profile.clone(),
        },
    );
    ModelRef {
        provider: llm_provider_to_cost_provider(&pricing_provider),
        model: bare,
    }
}

/// Stable provider tag string for the `tengu_api_success` `provider` field
/// (claude `provider:y9()`). The value comes from the RESOLVED provider, so
/// multi-LLM routing is preserved (it is not hardcoded to Anthropic).
#[must_use]
pub(crate) fn provider_tag(provider: &ProviderId) -> String {
    match provider {
        ProviderId::Anthropic => "anthropic".to_string(),
        ProviderId::OpenAI => "openai".to_string(),
        ProviderId::GoogleGemini => "gemini".to_string(),
        ProviderId::AmazonBedrock => "bedrock".to_string(),
        ProviderId::OpenAICompatible { name } | ProviderId::Custom { name } => name.clone(),
    }
}

/// Map a `cost::pricing::ProviderId` to its `llm_runtime::ProviderId` equivalent.
///
/// Mapping:
/// - `Anthropic` → `AnthropicFirstParty`
/// - `OpenAI` → `OpenAI`
/// - `GoogleGemini` → `Gemini`
/// - `AmazonBedrock` → `BedrockClaude`
/// - `OpenAICompatible { name }` → `OpenAICompatible { name }`
/// - `Custom { name }` → `Custom { name }`
fn cost_provider_to_llm_provider(p: &ProviderId) -> llm_runtime::ProviderId {
    match p {
        ProviderId::Anthropic => llm_runtime::ProviderId::AnthropicFirstParty,
        ProviderId::OpenAI => llm_runtime::ProviderId::OpenAI,
        ProviderId::GoogleGemini => llm_runtime::ProviderId::Gemini,
        ProviderId::AmazonBedrock => llm_runtime::ProviderId::BedrockClaude,
        ProviderId::OpenAICompatible { name } => {
            llm_runtime::ProviderId::OpenAICompatible { name: name.clone() }
        }
        ProviderId::Custom { name } => llm_runtime::ProviderId::Custom { name: name.clone() },
    }
}

/// Convert the client's completed, USD-only frozen token estimate to the
/// ledger unit and exact route identity. Provider-reported money is separate.
#[must_use]
pub(crate) fn frozen_cost_quote(estimate: &llm_runtime::CostEstimate) -> Option<(ModelRef, u64)> {
    let amount = estimate.total_cost_usd? * 1_000_000_000.0;
    if !estimate.estimated || !amount.is_finite() || amount < 0.0 || amount >= u64::MAX as f64 {
        return None;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let nano_usd = amount.round() as u64;
    Some((
        ModelRef {
            provider: llm_provider_to_cost_provider(&estimate.pricing_model.pricing_provider_id),
            model: estimate.pricing_model.billing_model.clone(),
        },
        nano_usd,
    ))
}

/// Build an `llm_runtime::PricingCatalog` populated from a `cost::PricingCatalog`.
///
/// Conversion: for each [`cost::ModelPricing`] entry, the `billing_model` is the
/// catalog key (the stripped model name the cost crate uses, e.g. `"claude-opus-4-6"`),
/// and per-bucket rates are converted from **nano-USD per token** to
/// **USD per million tokens** via `usd_per_million = nano_usd_per_token as f64 / 1000.0`.
///
/// Partially published entries are omitted because the legacy estimator's
/// `TokenPricing` requires every bucket; production responses use the SDK's
/// frozen price snapshot instead. Provider defaults are not iterable from the
/// cost catalog and are omitted.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn llm_catalog_from_cost(
    catalog: &cost::pricing::PricingCatalog,
) -> llm_runtime::PricingCatalog {
    let mut out = llm_runtime::PricingCatalog::empty();
    for entry in catalog.entries() {
        // Preserve absent buckets as unknown; production responses use the
        // SDK's frozen price snapshot for the selected attempt.
        if matches!(entry.source, cost::PricingSource::PublishedPartial { .. }) {
            continue;
        }
        let provider = cost_provider_to_llm_provider(&entry.model_ref.provider);
        let billing_model = entry.model_ref.model.clone();
        let nano_to_usd = |class: TokenClass| -> f64 {
            entry
                .token_rates
                .get(&class)
                .map_or(0.0, |m| m.nano_usd_per_token as f64 / 1000.0)
        };
        let pricing = llm_runtime::TokenPricing {
            currency: Some("USD".into()),
            input_per_million: entry
                .token_rates
                .get(&TokenClass::Input)
                .map(|_| nano_to_usd(TokenClass::Input)),
            output_per_million: entry
                .token_rates
                .get(&TokenClass::Output)
                .map(|_| nano_to_usd(TokenClass::Output)),
            cache_write_per_million: entry
                .token_rates
                .get(&TokenClass::CacheWrite)
                .map(|_| nano_to_usd(TokenClass::CacheWrite)),
            cache_read_per_million: entry
                .token_rates
                .get(&TokenClass::CacheRead)
                .map(|_| nano_to_usd(TokenClass::CacheRead)),
            reasoning_per_million: entry
                .token_rates
                .get(&TokenClass::ReasoningOutput)
                .map(|_| nano_to_usd(TokenClass::ReasoningOutput)),
            ..Default::default()
        };
        out = out.with_price(provider, billing_model, pricing);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_runtime::ServerToolUsage as LlmServerToolUsage;

    #[test]
    fn frozen_quote_carries_exact_price_identity_and_nano_amount() {
        let mut estimate = llm_runtime::CostEstimate::unestimated(llm_runtime::PricingModelRef {
            pricing_provider_id: llm_runtime::ProviderId::OpenAICompatible {
                name: "deepseek".into(),
            },
            billing_model: "deepseek-flash".into(),
            request_model: "deepseek-flash".into(),
            display_model: "deepseek-flash".into(),
        });
        estimate.estimated = true;
        estimate.total_cost_usd = Some(0.00075);
        let (model, nano) = frozen_cost_quote(&estimate).unwrap();
        assert_eq!(
            model.provider,
            ProviderId::OpenAICompatible {
                name: "deepseek".into()
            }
        );
        assert_eq!(model.model, "deepseek-flash");
        assert_eq!(nano, 750_000);
        estimate.total_cost_usd = Some(f64::INFINITY);
        assert!(frozen_cost_quote(&estimate).is_none());
    }

    /// Resolve a model-name string to its `ProviderId` by parsing the
    /// `provider/model` prefix. Only needed in tests — the production path goes
    /// through `model_ref_from_string`.
    fn provider_from_model(model: &str) -> ProviderId {
        let (profile, _) = llm_runtime::split_profile_model(model);
        let llm_provider = llm_runtime::pricing_provider_id_for_profile(
            &profile,
            &llm_runtime::ProviderId::OpenAICompatible {
                name: profile.clone(),
            },
        );
        llm_provider_to_cost_provider(&llm_provider)
    }

    // --- split_profile_model tests (ported from providers::model_spec::tests) -----

    #[test]
    fn split_prefixed_splits_profile_and_model() {
        let (p, m) = llm_runtime::split_profile_model("openai/gpt-4o");
        assert_eq!(p, "openai");
        assert_eq!(m, "gpt-4o");
    }

    #[test]
    fn split_bare_string_is_anthropic_backcompat() {
        let (p, m) = llm_runtime::split_profile_model("claude-opus-4-7");
        assert_eq!(p, "anthropic");
        assert_eq!(m, "claude-opus-4-7");
    }

    #[test]
    fn split_claude_with_slash_stays_anthropic() {
        // A claude model id is never reinterpreted as profile/model.
        let (p, m) = llm_runtime::split_profile_model("claude-3-5/sonnet");
        assert_eq!(p, "anthropic");
        assert_eq!(m, "claude-3-5/sonnet");
    }

    #[test]
    fn split_non_claude_no_slash_is_anthropic_profile() {
        let (p, m) = llm_runtime::split_profile_model("some-model");
        assert_eq!(p, "anthropic");
        assert_eq!(m, "some-model");
    }

    #[test]
    fn split_custom_profile_name() {
        let (p, m) = llm_runtime::split_profile_model("groq/llama-3.3-70b");
        assert_eq!(p, "groq");
        assert_eq!(m, "llama-3.3-70b");
    }

    fn make_llm_usage(
        input: u64,
        output: u64,
        cache_write: u64,
        cache_read: u64,
        server_tool_use: Option<LlmServerToolUsage>,
        speed: Option<String>,
    ) -> LlmUsage {
        LlmUsage {
            report: llm_runtime::UsageReport::measured(
                llm_runtime::Usage {
                    input_tokens: input,
                    output_tokens: output,
                    cache_write_tokens: cache_write,
                    cache_read_tokens: cache_read,
                    reasoning_tokens: 0,
                    server_tool_usage: server_tool_use,
                    ..Default::default()
                },
                llm_runtime::services::sdk::protocol::UsageState::Complete,
            ),
            inference: llm_runtime::services::sdk::protocol::InferenceReport {
                service_tier: speed.map(|s| {
                    if s == "fast" {
                        llm_runtime::services::sdk::protocol::ServiceTier::Fast
                    } else {
                        llm_runtime::services::sdk::protocol::ServiceTier::Standard
                    }
                }),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn checked_usage_preserves_canonical_subsets_independent_of_metadata() {
        for split in [
            serde_json::json!({"ephemeral_5m_input_tokens":12,"ephemeral_1h_input_tokens":18}),
            serde_json::json!({"ephemeral_1h_input_tokens":18}),
            serde_json::json!({"ephemeral_5m_input_tokens":12}),
        ] {
            let mut usage = make_llm_usage(
                100,
                40,
                30,
                10,
                Some(LlmServerToolUsage {
                    web_search_requests: Some(3),
                    ..Default::default()
                }),
                Some("fast".into()),
            );
            usage.counts_mut().reasoning_tokens = 60;
            usage.counts_mut().output_tokens = 100;
            usage.counts_mut().cache_write_1h_tokens = 18;
            usage.provider_metadata =
                serde_json::json!({"cache_creation_input_tokens":30,"cache_creation":split});
            let before = usage.clone();
            let result = llm_usage_to_cost_usage_checked(&usage).unwrap();
            assert_eq!(
                (result.tokens.cache_write, result.tokens.cache_write_1h),
                (12, 18)
            );
            assert_eq!(
                (result.tokens.output, result.tokens.reasoning_output),
                (40, 60)
            );
            assert_eq!((result.tokens.input, result.tokens.cache_read), (100, 10));
            assert_eq!(result.server_tool_use.unwrap().web_search_requests, 3);
            assert_eq!(result.speed, Some(ApiSpeed::Fast));
            assert_eq!(usage, before);
        }
        let usage = make_llm_usage(100, 50, 30, 10, None, None);
        assert_eq!(
            llm_usage_to_cost_usage_checked(&usage).unwrap(),
            llm_usage_to_cost_usage(&usage)
        );
    }

    #[test]
    fn checked_usage_ignores_non_authoritative_presentation_metadata() {
        for metadata in [
            serde_json::json!({"cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":10}}),
            serde_json::json!({"cache_creation":{"ephemeral_1h_input_tokens":31}}),
            serde_json::json!({"cache_creation":{"ephemeral_5m_input_tokens":31}}),
            serde_json::json!({"cache_creation":{"ephemeral_5m_input_tokens":u64::MAX,"ephemeral_1h_input_tokens":1}}),
            serde_json::json!({"cache_creation_input_tokens":29}),
            serde_json::json!({"cache_creation":{}}),
            serde_json::json!({"cache_creation":"bad"}),
            serde_json::json!({"cache_creation":{"ephemeral_1h_input_tokens":"18"}}),
            serde_json::json!({"cache_read_input_tokens":-1}),
            serde_json::json!({"reasoning_output_tokens":1.5}),
            serde_json::json!({"server_tool_use":{"web_search_requests":"1"}}),
        ] {
            let mut usage = make_llm_usage(100, 50, 30, 10, None, None);
            usage.provider_metadata = metadata;
            assert!(
                llm_usage_to_cost_usage_checked(&usage).is_ok(),
                "{:?}",
                usage.provider_metadata
            );
        }
    }

    #[test]
    fn checked_usage_rejects_invalid_canonical_measurements() {
        let mut usage = make_llm_usage(100, 50, 30, 10, None, None);
        usage.counts_mut().reasoning_tokens = 51;
        assert!(llm_usage_to_cost_usage_checked(&usage).is_err());
        usage.counts_mut().reasoning_tokens = 0;
        usage.counts_mut().cache_write_1h_tokens = 31;
        assert!(llm_usage_to_cost_usage_checked(&usage).is_err());
        usage.counts_mut().cache_write_1h_tokens = 0;
        usage.report.state = llm_runtime::services::sdk::protocol::UsageState::Invalid;
        assert!(llm_usage_to_cost_usage_checked(&usage).is_err());
        assert!(llm_usage_to_cost_usage_checked(&LlmUsage::default()).is_err());
    }

    #[test]
    fn checked_usage_rejects_server_tool_overflow_without_changing_legacy_clamp() {
        let usage = make_llm_usage(
            0,
            0,
            0,
            0,
            Some(LlmServerToolUsage {
                web_search_requests: Some(u64::from(u32::MAX) + 1),
                ..Default::default()
            }),
            None,
        );
        assert_eq!(
            llm_usage_to_cost_usage_checked(&usage),
            Err(cost::AttemptFoldError::Arithmetic)
        );
        assert_eq!(
            llm_usage_to_cost_usage(&usage)
                .server_tool_use
                .unwrap()
                .web_search_requests,
            u32::MAX
        );
        let mut inconsistent = make_llm_usage(0, 0, 0, 0, None, None);
        inconsistent.provider_metadata =
            serde_json::json!({"server_tool_use":{"web_search_requests":1}});
        assert!(llm_usage_to_cost_usage_checked(&inconsistent).is_ok());
    }

    #[test]
    fn translates_tokens_one_to_one() {
        let usage = make_llm_usage(100, 50, 20, 10, None, None);
        let u = llm_usage_to_cost_usage(&usage);
        assert_eq!(u.tokens.input, 100);
        assert_eq!(u.tokens.output, 50);
        assert_eq!(u.tokens.cache_write, 20);
        assert_eq!(u.tokens.cache_read, 10);
        assert_eq!(u.tokens.reasoning_output, 0);
        // Absent server_tool_use / speed default to None — no regression.
        assert!(u.server_tool_use.is_none());
        assert!(u.speed.is_none());
    }

    #[test]
    fn splits_anthropic_cache_creation_by_ttl() {
        let mut usage = make_llm_usage(100, 50, 30, 10, None, None);
        usage.counts_mut().cache_write_1h_tokens = 18;
        usage.provider_metadata = serde_json::json!({
            "cache_creation_input_tokens": 30,
            "cache_creation": {
                "ephemeral_5m_input_tokens": 12,
                "ephemeral_1h_input_tokens": 18
            }
        });

        let translated = llm_usage_to_cost_usage(&usage);
        assert_eq!(translated.tokens.cache_write, 12);
        assert_eq!(translated.tokens.cache_write_1h, 18);
    }

    #[test]
    fn subtracts_one_hour_bucket_when_five_minute_breakdown_is_absent() {
        let mut usage = make_llm_usage(100, 50, 30, 10, None, None);
        usage.counts_mut().cache_write_1h_tokens = 18;
        usage.provider_metadata = serde_json::json!({
            "cache_creation_input_tokens": 30,
            "cache_creation": { "ephemeral_1h_input_tokens": 18 }
        });

        let translated = llm_usage_to_cost_usage(&usage);
        assert_eq!(translated.tokens.cache_write, 12);
        assert_eq!(translated.tokens.cache_write_1h, 18);
    }

    use cost::pricing::{ModelRef, PricingCatalog, ProviderId};
    use cost::{ApiSpeed, CostCalculator};

    fn opus_4_6_pricing() -> cost::ModelPricing {
        let c = PricingCatalog::builtin_reference();
        let mr = ModelRef {
            provider: ProviderId::Anthropic,
            model: "claude-opus-4-6".into(),
        };
        c.resolve(&mr).unwrap().0
    }

    #[test]
    fn web_search_requests_thread_through_and_bill_one_cent_each() {
        // COST.5: usage.server_tool_use.web_search_requests on the wire → cost
        // Usage → billed at $0.01 (10_000_000 nano-USD) per request.
        let usage = make_llm_usage(
            0,
            0,
            0,
            0,
            Some(LlmServerToolUsage {
                web_search_requests: Some(3),
                ..Default::default()
            }),
            None,
        );
        let u = llm_usage_to_cost_usage(&usage);
        assert_eq!(u.server_tool_use.unwrap().web_search_requests, 3);
        // No tokens → only the web-search charge: 3 × $0.01 = 30_000_000 nano-USD.
        assert_eq!(
            CostCalculator::calculate_nano_usd(&u, &opus_4_6_pricing()),
            30_000_000
        );
    }

    #[test]
    fn speed_fast_threads_through_and_bills_opus_4_6_fast_tier() {
        // COST.3: usage.speed == "fast" → cost ApiSpeed::Fast → Opus-4.6
        // rebills at the $30/$150 fast tier instead of the catalog $5/$25.
        let usage = make_llm_usage(1_000_000, 0, 0, 0, None, Some("fast".to_string()));
        let u = llm_usage_to_cost_usage(&usage);
        assert_eq!(u.speed, Some(ApiSpeed::Fast));
        // 1M input × $30/Mtok = 30e9 nano-USD.
        assert_eq!(
            CostCalculator::calculate_nano_usd(&u, &opus_4_6_pricing()),
            30_000_000_000
        );
    }

    #[test]
    fn non_fast_speed_maps_to_standard_and_keeps_base_tier() {
        // A non-"fast" speed string maps to Standard (explicitly not fast), so
        // Opus-4.6 stays on the $5/$25 catalog tier.
        let usage = make_llm_usage(1_000_000, 0, 0, 0, None, Some("standard".to_string()));
        let u = llm_usage_to_cost_usage(&usage);
        assert_eq!(u.speed, Some(ApiSpeed::Standard));
        assert_eq!(
            CostCalculator::calculate_nano_usd(&u, &opus_4_6_pricing()),
            5_000_000_000
        );
    }

    #[test]
    fn provider_from_model_maps_prefixes() {
        assert_eq!(
            provider_from_model("claude-opus-4-7"),
            ProviderId::Anthropic
        );
        assert_eq!(
            provider_from_model("anthropic/claude-opus-4-7"),
            ProviderId::Anthropic
        );
        assert_eq!(provider_from_model("openai/gpt-4o"), ProviderId::OpenAI);
        assert_eq!(
            provider_from_model("gemini/gemini-2.0-flash"),
            ProviderId::GoogleGemini
        );
        assert_eq!(
            provider_from_model("some-bare-model"),
            ProviderId::Anthropic
        );
        assert_eq!(
            provider_from_model("groq/llama-3.3-70b"),
            ProviderId::OpenAICompatible {
                name: "groq".to_string()
            }
        );
    }

    #[test]
    fn managed_cloud_profiles_map_to_priced_providers() {
        // Bedrock has its own price table; Vertex reuses Gemini; Azure reuses OpenAI.
        assert_eq!(
            provider_from_model("bedrock/anthropic.claude-3-5-sonnet-20241022-v2:0"),
            ProviderId::AmazonBedrock
        );
        assert_eq!(
            provider_from_model("vertex/gemini-2.0-flash"),
            ProviderId::GoogleGemini
        );
        assert_eq!(provider_from_model("azure/gpt-4o"), ProviderId::OpenAI);
    }

    #[test]
    fn model_ref_strips_prefix_for_priced_lookup() {
        // No live profile + an UNKNOWN prefixed ref (not in the catalog) →
        // string-parse fallback: the prefix selects the provider, the remainder
        // is the stripped local id. (A catalog-KNOWN slashed id like the real
        // openrouter `openai/gpt-4o` resolves to its actual provider instead —
        // see `model_ref_recovers_provider_by_id_when_profile_is_none`.)
        let mr = model_ref_from_string("openai/gpt-4o-unknownsnap", None);
        assert_eq!(mr.provider, ProviderId::OpenAI);
        assert_eq!(mr.model, "gpt-4o-unknownsnap");
        // Anthropic back-compat: bare `claude-*` full string kept as the model id.
        let mr = model_ref_from_string("claude-opus-4-7", None);
        assert_eq!(mr.provider, ProviderId::Anthropic);
        assert_eq!(mr.model, "claude-opus-4-7");
    }

    #[test]
    fn model_ref_attributes_to_the_live_provider_profile() {
        // Bug fix: with a bare wire id, the provider lives in the LIVE profile.
        // Without it, split_profile_model mis-infers `anthropic` for every one of
        // these — misattributing cost + the tengu_api_success provider tag.

        // DeepSeek V4 bare id → deepseek, NOT anthropic.
        let mr = model_ref_from_string("deepseek-flash", Some("deepseek"));
        assert_eq!(
            mr.provider,
            ProviderId::OpenAICompatible {
                name: "deepseek".to_string()
            }
        );
        assert_eq!(mr.model, "deepseek-flash");

        // Provider-shared claude id served by Copilot → copilot, NOT anthropic.
        let mr = model_ref_from_string("claude-opus-4-8", Some("github-copilot"));
        assert_eq!(
            mr.provider,
            ProviderId::OpenAICompatible {
                name: "github-copilot".to_string()
            }
        );

        // Live `anthropic` profile still attributes to Anthropic.
        let mr = model_ref_from_string("claude-opus-4-8", Some("anthropic"));
        assert_eq!(mr.provider, ProviderId::Anthropic);

        // openai / gemini bare ids attribute correctly, not to anthropic.
        assert_eq!(
            model_ref_from_string("gpt-5.2", Some("openai")).provider,
            ProviderId::OpenAI
        );
        assert_eq!(
            model_ref_from_string("gemini-2.5-pro", Some("gemini")).provider,
            ProviderId::GoogleGemini
        );
    }

    #[test]
    fn model_ref_recovers_provider_by_id_when_profile_is_none() {
        // Two DeepSeek profiles serve this id. Without a live profile, neither
        // the provider nor the price can be recovered from the model alone.
        let mr = model_ref_from_string("deepseek-flash", None);
        assert_eq!(
            mr.provider,
            ProviderId::Custom {
                name: "unknown-profile".to_string()
            },
            "ambiguous bare id must not inherit another provider's price"
        );

        assert_eq!(
            model_ref_from_string("gpt-4o", None).provider,
            ProviderId::OpenAI,
            "unique catalog ids still recover their provider"
        );

        // A bare `claude-` id with no profile still resolves to Anthropic
        // (unique in the catalog / string-parse fallback agree).
        assert_eq!(
            model_ref_from_string("claude-opus-4-8", None).provider,
            ProviderId::Anthropic
        );

        // A genuinely unknown id falls back to the string-parse default.
        assert_eq!(
            model_ref_from_string("totally-unknown-model", None).provider,
            ProviderId::Anthropic
        );
    }

    // ── 3c-T3: llm_catalog_from_cost bridge conversion tests ────────────────

    /// Pinned conversion: `claude-opus-4-6` input rate is `5_000` `nano_usd/token`
    /// (= `5_000 / 1_000` = `5.0` `usd/million`).  Output is `25_000` nano → `25.0` `usd/M`.
    /// Cache-write is `6_250` → `6.25` `usd/M`.  Cache-read is `500` → `0.5` `usd/M`.
    #[test]
    fn bridge_opus_4_6_converts_exact_rates() {
        use cost::pricing::PricingCatalog as CostCatalog;
        use llm_runtime::{CostEstimator, ExecutionUsage as LlmUsage, PricingPolicy};

        let cost_cat = CostCatalog::builtin_reference();
        let llm_cat = llm_catalog_from_cost(&cost_cat);

        // Build an estimator with MarkUnestimated so unknown models return None-cost.
        let estimator = CostEstimator::new(llm_cat, PricingPolicy::MarkUnestimated);

        // Construct the PricingModelRef that the estimator needs.
        // billing_model is the raw model name (no prefix) as stored in the catalog.
        let pricing_ref = llm_runtime::PricingModelRef {
            pricing_provider_id: llm_runtime::ProviderId::AnthropicFirstParty,
            billing_model: "claude-opus-4-6".to_string(),
            request_model: "claude-opus-4-6".to_string(),
            display_model: "Claude Opus 4.6".to_string(),
        };
        let usage = LlmUsage {
            report: llm_runtime::UsageReport::measured(
                llm_runtime::Usage {
                    input_tokens: 1_000_000,
                    output_tokens: 1_000_000,
                    cache_write_tokens: 0,
                    cache_read_tokens: 0,
                    reasoning_tokens: 0,
                    ..Default::default()
                },
                llm_runtime::services::sdk::protocol::UsageState::Complete,
            ),
            ..Default::default()
        };
        let estimate = estimator
            .estimate(pricing_ref, &usage.counts())
            .expect("opus-4-6 must be priced");
        // 1M input × $5.0/M = $5.0
        let total = estimate
            .total_cost_usd
            .expect("total_cost_usd must be Some");
        let expected = 5.0 + 25.0; // input + output
        assert!(
            (total - expected).abs() < 1e-9,
            "expected total ${expected}, got ${total}"
        );
        let input = estimate.input_cost_usd.unwrap();
        assert!(
            (input - 5.0).abs() < 1e-9,
            "input cost must be $5.0/M, got ${input}"
        );
        let output = estimate.output_cost_usd.unwrap();
        assert!(
            (output - 25.0).abs() < 1e-9,
            "output cost must be $25.0/M, got ${output}"
        );
    }

    /// Unknown model → cost stays `None` (`MarkUnestimated` policy).
    #[test]
    fn bridge_unknown_model_returns_unestimated() {
        use cost::pricing::PricingCatalog as CostCatalog;
        use llm_runtime::{CostEstimator, PricingPolicy};

        let cost_cat = CostCatalog::builtin_reference();
        let llm_cat = llm_catalog_from_cost(&cost_cat);
        let estimator = CostEstimator::new(llm_cat, PricingPolicy::MarkUnestimated);

        let pricing_ref = llm_runtime::PricingModelRef {
            pricing_provider_id: llm_runtime::ProviderId::AnthropicFirstParty,
            billing_model: "claude-nonexistent-9999".to_string(),
            request_model: "claude-nonexistent-9999".to_string(),
            display_model: "Claude Nonexistent".to_string(),
        };
        let estimate = estimator
            .estimate(pricing_ref, &llm_runtime::Usage::default())
            .expect("MarkUnestimated must not error");
        // Unpriced → total_cost_usd is None.
        assert!(
            estimate.total_cost_usd.is_none(),
            "unpriced model must yield None total_cost_usd"
        );
        assert!(
            !estimate.estimated,
            "estimated flag must be false for unpriced"
        );
    }

    /// Provider mapping: `OpenAI` `gpt-4o` converts at the expected rates.
    #[test]
    fn bridge_openai_gpt4o_maps_to_correct_provider() {
        use cost::pricing::PricingCatalog as CostCatalog;
        use llm_runtime::{CostEstimator, ExecutionUsage as LlmUsage, PricingPolicy};

        let cost_cat = CostCatalog::builtin_reference();
        let llm_cat = llm_catalog_from_cost(&cost_cat);
        let estimator = CostEstimator::new(llm_cat, PricingPolicy::MarkUnestimated);

        let pricing_ref = llm_runtime::PricingModelRef {
            pricing_provider_id: llm_runtime::ProviderId::OpenAI,
            billing_model: "gpt-4o".to_string(),
            request_model: "gpt-4o".to_string(),
            display_model: "GPT-4o".to_string(),
        };
        let usage = LlmUsage {
            report: llm_runtime::UsageReport::measured(
                llm_runtime::Usage {
                    input_tokens: 1_000_000,
                    output_tokens: 0,
                    cache_write_tokens: 0,
                    cache_read_tokens: 0,
                    reasoning_tokens: 0,
                    ..Default::default()
                },
                llm_runtime::services::sdk::protocol::UsageState::Complete,
            ),
            ..Default::default()
        };
        let estimate = estimator
            .estimate(pricing_ref, &usage.counts())
            .expect("gpt-4o must be priced");
        // gpt-4o input = 2_500 nano_usd/token → 2.5 usd/M
        let input = estimate
            .input_cost_usd
            .expect("input_cost_usd must be Some");
        assert!(
            (input - 2.5).abs() < 1e-9,
            "gpt-4o input must be $2.5/M, got ${input}"
        );
    }
}
