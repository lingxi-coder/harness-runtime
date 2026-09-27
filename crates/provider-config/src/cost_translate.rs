//! Build the `cost::PricingCatalog` returned by `assemble` from the merged
//! provider profiles (Plan 3c §8). Anthropic / `OpenAI` / Gemini reference tiers
//! come from `cost::PricingCatalog::builtin_reference()`; other models use
//! published USD rates from the pinned client. Missing cache rates stay
//! unknown without discarding known input/output rates. Only a consumed
//! missing bucket uses the host's unknown-price fallback; a model with no
//! published rates uses that fallback for the whole response.

use std::collections::HashMap;

use llm_runtime::{ProviderId as LlmProviderId, ProviderProfile, TokenPricing};

use cost::pricing::{
    MoneyPerToken, NonTokenBillableUnit, PricingSource, ProviderId as CostProviderId, TokenClass,
};
use cost::{ModelPricing, ModelRef, PricingCatalog};
use llm_runtime::ModelBillingMode;

/// Convert a per-million-token USD price to nano-USD per token.
/// `$X / Mtok` = `X * 1000` nano-USD per token (1 USD = 1e9 nano-USD; 1 Mtok = 1e6 tokens).
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn nano_per_token(usd_per_million: f64) -> u64 {
    (usd_per_million * 1000.0).round().max(0.0) as u64
}

/// Build a real `cost::ModelPricing` from the models.dev [`TokenPricing`].
fn model_pricing_from_token_pricing(mr: &ModelRef, tp: &TokenPricing) -> ModelPricing {
    let mut rates: HashMap<TokenClass, MoneyPerToken> = HashMap::new();
    rates.insert(
        TokenClass::Input,
        MoneyPerToken {
            nano_usd_per_token: nano_per_token(tp.input_per_million),
        },
    );
    rates.insert(
        TokenClass::Output,
        MoneyPerToken {
            nano_usd_per_token: nano_per_token(tp.output_per_million),
        },
    );
    rates.insert(
        TokenClass::CacheWrite,
        MoneyPerToken {
            nano_usd_per_token: nano_per_token(tp.cache_write_per_million),
        },
    );
    rates.insert(
        TokenClass::CacheRead,
        MoneyPerToken {
            nano_usd_per_token: nano_per_token(tp.cache_read_per_million),
        },
    );
    // TokenPricing resolves an absent reasoning rate to the output rate when
    // constructed. Zero here is an explicit free rate and must stay zero in
    // both the host ledger and Fusion's captured prices.
    rates.insert(
        TokenClass::ReasoningOutput,
        MoneyPerToken {
            nano_usd_per_token: nano_per_token(tp.reasoning_per_million),
        },
    );
    ModelPricing {
        model_ref: mr.clone(),
        token_rates: rates,
        // models.dev non-Anthropic models bill only tokens; no per-request units.
        non_token_rates_nano_usd: HashMap::<NonTokenBillableUnit, u64>::new(),
        effective_from: None,
        source: PricingSource::BuiltInReference {
            provider: mr.provider.clone(),
        },
    }
}

/// Preserve published USD buckets when the provider has not published every
/// cache price. A missing bucket stays absent; the host ledger applies its
/// unknown-price policy only if that bucket actually appears in usage.
fn partial_published_pricing(
    mr: &ModelRef,
    published: &platform_api::ModelPricing,
    conditional: bool,
) -> Option<ModelPricing> {
    if published.billing_mode != ModelBillingMode::PerToken {
        return None;
    }
    let mut rates = HashMap::new();
    for (class, value) in [
        (TokenClass::Input, published.input_per_million),
        (TokenClass::Output, published.output_per_million),
        (TokenClass::CacheRead, published.cache_read_per_million),
        (TokenClass::CacheWrite, published.cache_write_per_million),
    ] {
        if let Some(value) = value.filter(|value| value.is_finite() && *value >= 0.0) {
            rates.insert(
                class,
                MoneyPerToken {
                    nano_usd_per_token: nano_per_token(value),
                },
            );
        }
    }
    // An absent reasoning rate means reasoning is included at the output rate.
    if let Some(value) = published
        .reasoning_per_million
        .or(published.output_per_million)
        .filter(|value| value.is_finite() && *value >= 0.0)
    {
        rates.insert(
            TokenClass::ReasoningOutput,
            MoneyPerToken {
                nano_usd_per_token: nano_per_token(value),
            },
        );
    }
    (!rates.is_empty()).then(|| ModelPricing {
        model_ref: mr.clone(),
        token_rates: rates,
        non_token_rates_nano_usd: HashMap::new(),
        effective_from: None,
        source: PricingSource::PublishedPartial {
            provider: mr.provider.clone(),
            conditional,
        },
    })
}

fn cost_provider_id(profile_name: &str, provider_id: &LlmProviderId) -> CostProviderId {
    let pricing_provider = llm_runtime::pricing_provider_id_for_profile(profile_name, provider_id);
    match pricing_provider {
        LlmProviderId::AnthropicFirstParty => CostProviderId::Anthropic,
        LlmProviderId::OpenAI | LlmProviderId::AzureOpenAI => CostProviderId::OpenAI,
        LlmProviderId::Gemini | LlmProviderId::VertexGemini | LlmProviderId::VertexClaude => {
            CostProviderId::GoogleGemini
        }
        LlmProviderId::BedrockClaude => CostProviderId::AmazonBedrock,
        // Foundry hosts Claude — price via Anthropic (also the normalized value
        // from `pricing_provider_id_for_profile`, so this arm is defensive).
        LlmProviderId::FoundryClaude => CostProviderId::Anthropic,
        LlmProviderId::OpenAICompatible { name } | LlmProviderId::Custom { name } => {
            CostProviderId::OpenAICompatible { name }
        }
    }
}

/// Build the cost catalog from reference tiers and models.dev prices.
///
/// Unknown models are deliberately left out so [`cost::CostTracker`] can apply
/// its default tier while marking the model as unpriced for `/cost` warnings.
#[must_use]
pub fn pricing_for(providers: &[ProviderProfile]) -> PricingCatalog {
    let mut catalog = PricingCatalog::builtin_reference();
    // Real per-model rates from the bundled models.dev slices (deepseek, openai,
    // openrouter, zai/glm, github-copilot, …), keyed by (llm ProviderId, id).
    let preset_pricing = llm_runtime::builtin_presets().pricing;
    for profile in providers {
        let provider = cost_provider_id(&profile.profile_name, &profile.provider_id);
        for model in &profile.models {
            let mr = ModelRef {
                provider: provider.clone(),
                model: model.billing_model.clone(),
            };
            // A concrete user override is authoritative, including for a
            // profile that was initially marked as subscription-backed.
            if let Some((_, override_price)) = profile.pricing.overrides.iter().find(|(id, _)| {
                id == &model.display_model
                    || id == &model.request_model
                    || id == &model.billing_model
            }) {
                catalog = catalog.with_entry(model_pricing_from_token_pricing(&mr, override_price));
                continue;
            }
            // A subscription must never inherit a token price from a
            // models.dev slice that happens to use the same provider/model id.
            if profile.pricing.billing_mode == ModelBillingMode::Subscription {
                catalog = catalog.mark_unpriced(mr);
                continue;
            }
            let published_row = profile.wire_profile.as_ref().and_then(|source| {
                source.models.iter().find(|source_model| {
                    source_model.display_model == model.display_model
                        && source_model.request_model == model.request_model
                })
            });
            let conditional = profile.wire_profile.as_ref().is_some_and(|source| {
                source.pricing.peak.is_some()
                    || published_row
                        .and_then(|model| model.pricing.as_ref())
                        .is_some_and(|prices| !prices.rules.is_empty())
            }) || model
                .metadata
                .pricing
                .as_ref()
                .is_some_and(|pricing| !pricing.tiers.is_empty());
            if let Ok((price, _)) = catalog.resolve(&mr) {
                // Keep reference token and hosted-tool prices, but never
                // present a base rate as exact when this row has SDK rules.
                if conditional {
                    let mut price = price;
                    price.model_ref = mr.clone();
                    price.source = PricingSource::PublishedPartial {
                        provider: mr.provider.clone(),
                        conditional: true,
                    };
                    catalog = catalog.with_entry(price);
                }
                continue;
            }
            // Prefer the model's real models.dev price over the $5/$25 default.
            if let Some(tp) = preset_pricing.get(&profile.provider_id, &model.billing_model) {
                let mut price = model_pricing_from_token_pricing(&mr, &tp);
                if conditional {
                    price.source = PricingSource::PublishedPartial {
                        provider: mr.provider.clone(),
                        conditional: true,
                    };
                }
                catalog = catalog.with_entry(price);
                continue;
            }
            // Retain base rates for the picker and ordinary fixed-price
            // requests. Conditional rules need the SDK's frozen per-attempt
            // quote; the static ledger flags its fallback if no quote arrives.
            if published_row.is_some() {
                if let Some(price) =
                    model.metadata.pricing.as_ref().and_then(|published| {
                        partial_published_pricing(&mr, published, conditional)
                    })
                {
                    catalog = catalog.with_entry(price);
                    continue;
                }
            }
            // No reference tier and no models.dev price: leave the model out.
            // CostTracker applies the default unknown tier when it is used.
        }
    }
    catalog
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_runtime::{
        AuthStrategy, Capabilities, CredentialConfig, ModelProfile, PricingConfig, ProtocolFamily,
        TokenPricing,
    };

    fn user_profile(name: &str, model: &str) -> ProviderProfile {
        ProviderProfile {
            wire_profile: None,
            regions: llm_runtime::Region::all(),
            provider_id: LlmProviderId::OpenAICompatible {
                name: name.to_string(),
            },
            profile_name: name.to_string(),
            base_url: "https://x".to_string(),
            protocol: ProtocolFamily::OpenAiChat,
            auth: AuthStrategy::Bearer,
            credential: CredentialConfig::Static {
                id: name.to_string(),
            },
            models: vec![ModelProfile {
                display_model: model.to_string(),
                request_model: model.to_string(),
                billing_model: model.to_string(),
                aliases: Vec::new(),
                description: None,
                metadata: Default::default(),
                capabilities: Capabilities::default(),
            }],
            pricing: PricingConfig::default(),
            signing: None,
            azure: None,
            supports_websockets: false,
            supports_websocket_compression: false,
            websocket_connect_timeout_ms: None,
            vision_delegate: None,
            connection: Default::default(),
        }
    }

    #[test]
    fn anthropic_reference_tiers_preserved() {
        let cat = pricing_for(&[]);
        let opus = ModelRef {
            provider: CostProviderId::Anthropic,
            model: "claude-opus-4-6".to_string(),
        };
        assert!(cat.resolve(&opus).is_ok());
    }

    #[test]
    fn conditional_rules_are_marked_even_for_complete_and_reference_prices() {
        let providers = llm_runtime::builtin_presets().providers;
        let catalog = pricing_for(&providers);
        let reference = PricingCatalog::builtin_reference();
        let mut checked = 0;
        let mut checked_reference = 0;
        let mut checked_complete = 0;
        for profile in &providers {
            if profile.pricing.billing_mode == ModelBillingMode::Subscription {
                continue;
            }
            let Some(wire_profile) = &profile.wire_profile else {
                continue;
            };
            for model in &profile.models {
                let Some(wire_model) = wire_profile.models.iter().find(|candidate| {
                    candidate.display_model == model.display_model
                        && candidate.request_model == model.request_model
                }) else {
                    continue;
                };
                let Some(rates) = wire_model.pricing.as_ref() else {
                    continue;
                };
                if rates.currency.as_deref().unwrap_or("USD") != "USD"
                    || (wire_profile.pricing.peak.is_none() && rates.rules.is_empty())
                {
                    continue;
                }
                let model_ref = ModelRef {
                    provider: cost_provider_id(&profile.profile_name, &profile.provider_id),
                    model: model.billing_model.clone(),
                };
                let Ok((price, _)) = catalog.resolve(&model_ref) else {
                    continue;
                };
                assert!(
                    matches!(
                        price.source,
                        PricingSource::PublishedPartial {
                            conditional: true,
                            ..
                        }
                    ),
                    "{}/{} has conditional pricing",
                    profile.profile_name,
                    model.display_model
                );
                if let Ok((reference_price, _)) = reference.resolve(&model_ref) {
                    assert_eq!(price.token_rates, reference_price.token_rates);
                    assert_eq!(
                        price.non_token_rates_nano_usd,
                        reference_price.non_token_rates_nano_usd
                    );
                    checked_reference += 1;
                }
                if rates.input_per_million.is_some()
                    && rates.output_per_million.is_some()
                    && rates.cache_read_per_million.is_some()
                    && rates.cache_write_per_million.is_some()
                {
                    checked_complete += 1;
                }
                checked += 1;
            }
        }
        assert!(checked > 0, "the SDK catalog contains conditional prices");
        assert!(
            checked_reference > 0,
            "reference prices must also honor SDK rules"
        );
        assert!(
            checked_complete > 0,
            "fully populated prices must also honor SDK rules"
        );
    }

    #[test]
    fn explicit_override_remains_authoritative_over_sdk_price_rules() {
        let mut profile = llm_runtime::builtin_presets()
            .providers
            .into_iter()
            .find(|profile| profile.profile_name == "deepseek")
            .unwrap();
        assert!(profile
            .wire_profile
            .as_ref()
            .unwrap()
            .pricing
            .peak
            .is_some());
        let model = profile.models[0].clone();
        profile
            .pricing
            .overrides
            .push((model.display_model, TokenPricing::input_output(1.0, 8.0)));
        let model_ref = ModelRef {
            provider: cost_provider_id(&profile.profile_name, &profile.provider_id),
            model: model.billing_model,
        };
        let catalog = pricing_for(&[profile]);
        let (price, _) = catalog.resolve(&model_ref).unwrap();
        assert!(!matches!(
            price.source,
            PricingSource::PublishedPartial {
                conditional: true,
                ..
            }
        ));
        assert_eq!(
            price.token_rates[&TokenClass::Output].nano_usd_per_token,
            8_000
        );
    }

    #[test]
    fn published_deepseek_rates_survive_a_missing_cache_write_rate() {
        let providers = llm_runtime::builtin_presets().providers;
        let deepseek = providers
            .iter()
            .find(|p| p.profile_name == "deepseek")
            .unwrap();
        let model = deepseek
            .models
            .iter()
            .find(|m| m.request_model == "deepseek-flash")
            .unwrap();
        assert!(model
            .metadata
            .pricing
            .as_ref()
            .unwrap()
            .input_per_million
            .is_some());
        let mr = ModelRef {
            provider: CostProviderId::OpenAICompatible {
                name: "deepseek".into(),
            },
            model: "deepseek-flash".into(),
        };
        let (price, _) = pricing_for(&providers)
            .resolve(&mr)
            .expect("published rates");
        assert_eq!(
            price.token_rates[&TokenClass::Input].nano_usd_per_token,
            300
        );
        assert_eq!(
            price.token_rates[&TokenClass::Output].nano_usd_per_token,
            1_200
        );
        assert_eq!(
            price.token_rates[&TokenClass::CacheRead].nano_usd_per_token,
            6
        );
        assert!(!price.token_rates.contains_key(&TokenClass::CacheWrite));
        assert!(matches!(
            price.source,
            PricingSource::PublishedPartial {
                conditional: true,
                ..
            }
        ));
        let usage = cost::Usage {
            tokens: cost::TokenUsage {
                input: 1_000_000,
                output: 1_000_000,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            cost::CostCalculator::calculate_nano_usd(&usage, &price),
            30_000_000_000
        );
        assert!(cost::CostCalculator::uses_unknown_rate(&usage, &price));
    }

    #[test]
    fn fixed_partial_prices_use_known_rates_and_flag_consumed_unknown_buckets() {
        let providers = llm_runtime::builtin_presets().providers;
        let mr = ModelRef {
            provider: CostProviderId::OpenAICompatible {
                name: "grok-responses".into(),
            },
            model: "grok-4.7".into(),
        };
        let (price, _) = pricing_for(&providers).resolve(&mr).unwrap();
        assert!(matches!(
            price.source,
            PricingSource::PublishedPartial {
                conditional: false,
                ..
            }
        ));
        let usage = cost::Usage {
            tokens: cost::TokenUsage {
                input: 1_000_000,
                output: 1_000_000,
                cache_read: 1_000_000,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            cost::CostCalculator::calculate_nano_usd(&usage, &price),
            8_500_000_000
        );
        assert!(!cost::CostCalculator::uses_unknown_rate(&usage, &price));
        let mut cache_write_usage = usage;
        cache_write_usage.tokens.cache_write = 1_000_000;
        assert_eq!(
            cost::CostCalculator::calculate_nano_usd(&cache_write_usage, &price),
            14_750_000_000
        );
        assert!(cost::CostCalculator::uses_unknown_rate(
            &cache_write_usage,
            &price
        ));
        let mut hosted_tool_usage = usage;
        hosted_tool_usage.server_tool_use = Some(cost::usage::ServerToolUsage {
            web_search_requests: 1,
        });
        assert_eq!(
            cost::CostCalculator::calculate_nano_usd(&hosted_tool_usage, &price),
            8_510_000_000
        );
        assert!(cost::CostCalculator::uses_unknown_rate(
            &hosted_tool_usage,
            &price
        ));
        let mut fast_usage = usage;
        fast_usage.speed = Some(cost::usage::ApiSpeed::Fast);
        assert_eq!(
            cost::CostCalculator::calculate_nano_usd(&fast_usage, &price),
            30_500_000_000
        );
        assert!(cost::CostCalculator::uses_unknown_rate(&fast_usage, &price));
        fast_usage.tokens.reasoning_output = 1_000_000;
        assert_eq!(
            cost::CostCalculator::calculate_nano_usd(&fast_usage, &price),
            55_500_000_000
        );
    }

    #[test]
    fn every_published_usd_catalog_rate_survives_missing_cache_fields() {
        let providers = llm_runtime::builtin_presets().providers;
        let catalog = pricing_for(&providers);
        let reference = PricingCatalog::builtin_reference();
        let mut checked = 0;
        for profile in &providers {
            if profile.profile_name == "anthropic"
                || profile.pricing.billing_mode != ModelBillingMode::PerToken
            {
                continue;
            }
            for model in &profile.models {
                let Some(published) = model.metadata.pricing.as_ref() else {
                    continue;
                };
                if published.billing_mode != ModelBillingMode::PerToken {
                    continue;
                }
                let (Some(input), Some(output)) =
                    (published.input_per_million, published.output_per_million)
                else {
                    continue;
                };
                let model_ref = ModelRef {
                    provider: cost_provider_id(&profile.profile_name, &profile.provider_id),
                    model: model.billing_model.clone(),
                };
                if reference.resolve(&model_ref).is_ok() {
                    continue;
                }
                let (price, _) = catalog.resolve(&model_ref).unwrap_or_else(|_| {
                    panic!(
                        "lost published price for {}/{}",
                        profile.profile_name, model.billing_model
                    )
                });
                assert_eq!(
                    price.token_rates[&TokenClass::Input].nano_usd_per_token,
                    nano_per_token(input),
                    "{model_ref:?}"
                );
                assert_eq!(
                    price.token_rates[&TokenClass::Output].nano_usd_per_token,
                    nano_per_token(output),
                    "{model_ref:?}"
                );
                for (class, published_rate) in [
                    (TokenClass::CacheRead, published.cache_read_per_million),
                    (TokenClass::CacheWrite, published.cache_write_per_million),
                ] {
                    if let Some(rate) = published_rate {
                        assert_eq!(
                            price.token_rates[&class].nano_usd_per_token,
                            nano_per_token(rate),
                            "{model_ref:?} {class:?}"
                        );
                    }
                }
                checked += 1;
            }
        }
        assert!(
            checked > 100,
            "catalog audit unexpectedly covered only {checked} rows"
        );
    }

    #[test]
    fn metered_and_subscription_profiles_for_one_vendor_do_not_share_a_price_key() {
        let providers = llm_runtime::builtin_presets().providers;
        let catalog = pricing_for(&providers);
        let metered = ModelRef {
            provider: CostProviderId::OpenAICompatible { name: "zai".into() },
            model: "glm-4.7".into(),
        };
        let subscription = ModelRef {
            provider: CostProviderId::OpenAICompatible {
                name: "zai-coding".into(),
            },
            model: "glm-4.7".into(),
        };
        assert!(catalog.resolve(&metered).is_ok());
        assert!(matches!(
            catalog.resolve(&subscription),
            Err(cost::pricing::CostError::UnpricedModel(_))
        ));
    }

    #[test]
    fn gemini_model_uses_real_price_not_default_unknown() {
        // M6: with the Gemini slice vendored, gemini-2.5-pro (not in
        // builtin_reference) bills at its real $1.25/$10 rate, not $5/$25.
        let providers = llm_runtime::builtin_presets().providers;
        let cat = pricing_for(&providers);
        let mr = ModelRef {
            provider: CostProviderId::GoogleGemini,
            model: "gemini-2.5-pro".to_string(),
        };
        let (p, _res) = cat.resolve(&mr).expect("priced");
        assert_eq!(
            p.token_rates[&cost::pricing::TokenClass::Input].nano_usd_per_token,
            1_250
        );
        assert_eq!(
            p.token_rates[&cost::pricing::TokenClass::Output].nano_usd_per_token,
            10_000
        );
    }

    #[test]
    fn unpriced_user_model_remains_unpriced_for_tracker_default_tier() {
        let cat = pricing_for(&[user_profile("groq", "llama-3.3-70b")]);
        let mr = ModelRef {
            provider: CostProviderId::OpenAICompatible {
                name: "groq".to_string(),
            },
            model: "llama-3.3-70b".to_string(),
        };
        assert!(matches!(
            cat.resolve(&mr),
            Err(cost::pricing::CostError::UnpricedModel(_))
        ));
    }

    #[test]
    fn subscription_profile_does_not_inherit_preset_token_price() {
        let mut profile = user_profile("github-copilot", "claude-opus-4.6");
        profile.pricing.billing_mode = ModelBillingMode::Subscription;
        let cat = pricing_for(&[profile]);
        let mr = ModelRef {
            provider: CostProviderId::OpenAICompatible {
                name: "github-copilot".to_string(),
            },
            model: "claude-opus-4.6".to_string(),
        };
        assert!(matches!(
            cat.resolve(&mr),
            Err(cost::pricing::CostError::UnpricedModel(_))
        ));
    }

    /// [Finding 1] `gpt-5.6-sol` is reasoning-capable (`reasoning: true`) but
    /// its bundled models.dev row carries no `cost.reasoning` price — the
    /// common case (573 priced rows in the bundled slices, only 33 with a
    /// separate reasoning price). Before this fix `ReasoningOutput` was
    /// simply omitted from the catalog entry, which both
    /// `cost::CostCalculator` (iterates only present classes) and Fusion's
    /// `DesktopFusionPriceBook::rates_for` (defaults an absent class to 0)
    /// read as "reasoning tokens are free" — even though the OpenAI decoder
    /// (`llm_runtime::providers::openai`) splits real, billed reasoning
    /// tokens out of `output` into exactly this bucket. It must instead
    /// fall back to the model's real output rate, since that is what OpenAI
    /// actually bills those tokens at.
    #[test]
    fn reasoning_capable_model_with_no_separate_price_bills_at_the_output_rate() {
        let providers = llm_runtime::builtin_presets().providers;
        let cat = pricing_for(&providers);
        let mr = ModelRef {
            provider: CostProviderId::OpenAI,
            model: "gpt-5.6-sol".to_string(),
        };
        let (p, _res) = cat.resolve(&mr).expect("priced");
        let output = p.token_rates[&cost::pricing::TokenClass::Output].nano_usd_per_token;
        assert_eq!(
            output, 20_000,
            "sanity: gpt-5.6-sol bills output at $20/Mtok"
        );
        let reasoning = p
            .token_rates
            .get(&cost::pricing::TokenClass::ReasoningOutput)
            .map(|r| r.nano_usd_per_token);
        assert_eq!(
            reasoning,
            Some(output),
            "a reasoning-capable model with no separate cost.reasoning price \
             must bill ReasoningOutput at the OUTPUT rate, not be silently \
             omitted (which both CostCalculator and Fusion's rates_for read \
             as a $0 rate)"
        );
    }

    /// Sibling of the above: a model that DOES publish a genuine separate
    /// reasoning price must keep using ITS OWN rate, not fall back to the
    /// output rate. Uses an explicit override (like
    /// `explicit_override_prices_a_subscription_profile` above) rather than
    /// a bundled models.dev row, since real rows can coincidentally price
    /// reasoning == output (e.g. deepseek-flash: both $1.2/Mtok) which
    /// would make this assertion pass either way and prove nothing.
    #[test]
    fn model_with_a_real_reasoning_price_keeps_its_own_rate() {
        let mut profile = user_profile("openrouter", "sonar-deep-research");
        profile.pricing.overrides.push((
            "sonar-deep-research".to_string(),
            TokenPricing {
                input_per_million: 2.0,
                output_per_million: 8.0,
                cache_write_per_million: 0.0,
                cache_read_per_million: 0.0,
                reasoning_per_million: 3.0,
            },
        ));
        let cat = pricing_for(&[profile]);
        let mr = ModelRef {
            provider: CostProviderId::OpenAICompatible {
                name: "openrouter".to_string(),
            },
            model: "sonar-deep-research".to_string(),
        };
        let (p, _res) = cat.resolve(&mr).expect("priced");
        let output = p.token_rates[&cost::pricing::TokenClass::Output].nano_usd_per_token;
        let reasoning =
            p.token_rates[&cost::pricing::TokenClass::ReasoningOutput].nano_usd_per_token;
        assert_eq!(output, 8_000);
        assert_eq!(
            reasoning, 3_000,
            "a model with its own published reasoning price must use it, \
             not the output-rate fallback"
        );
    }

    #[test]
    fn parsed_override_preserves_omitted_and_explicit_zero_reasoning_rates() {
        for (reasoning, expected_nano_usd) in [(None, 320_000_000), (Some(0.0), 80_000_000)] {
            let mut pricing = serde_json::json!({"inputPerMtok": 2.0, "outputPerMtok": 8.0});
            if let Some(reasoning) = reasoning {
                pricing["reasoningPerMtok"] = serde_json::json!(reasoning);
            }
            let providers = serde_json::json!({"custom": {
                "type": "openai",
                "baseUrl": "https://example.com/v1",
                "apiKeyEnv": "CUSTOM_REASONING_PRICE_TEST_KEY",
                "models": [{"id": "custom-model"}],
                "pricing": {"custom-model": pricing}
            }});
            let profile = llm_runtime::parse_provider_profiles_strict(
                &serde_json::from_value(providers).unwrap(),
                llm_runtime::ProviderParseOptions::strict_env(),
            )
            .unwrap()
            .remove(0)
            .profile;
            let catalog = pricing_for(&[profile]);
            let model_ref = ModelRef {
                provider: CostProviderId::OpenAICompatible {
                    name: "custom".into(),
                },
                model: "custom-model".into(),
            };
            let usage = cost::Usage {
                tokens: cost::TokenUsage {
                    output: 10_000,
                    reasoning_output: 30_000,
                    ..Default::default()
                },
                ..Default::default()
            };
            let (price, _) = catalog.resolve(&model_ref).expect("override price");
            assert_eq!(
                cost::CostCalculator::calculate_nano_usd(&usage, &price),
                expected_nano_usd
            );
        }
    }

    /// If a conditional model has no frozen quote, the host fallback must
    /// flag the unknown price and still bill the reasoning output bucket.
    #[test]
    fn conditional_model_without_quote_bills_reasoning_at_flagged_fallback_rate() {
        let providers = llm_runtime::builtin_presets().providers;
        let cat = pricing_for(&providers);
        let mr = ModelRef {
            provider: CostProviderId::OpenAI,
            model: "gpt-5.6-sol".to_string(),
        };
        let (pricing, _res) = cat.resolve(&mr).expect("priced");

        let usage = cost::Usage {
            tokens: cost::TokenUsage {
                input: 0,
                output: 10_000,
                cache_write: 0,
                cache_read: 0,
                reasoning_output: 30_000,
                cache_write_1h: 0,
            },
            server_tool_use: None,
            speed: None,
        };
        let total = cost::CostCalculator::calculate_nano_usd(&usage, &pricing);
        assert!(cost::CostCalculator::uses_unknown_rate(&usage, &pricing));
        // Both visible and reasoning output use the unknown $25/M rate when
        // the request's tier/context are unavailable to select the SDK rule.
        assert_eq!(total, 1_000_000_000);
    }

    #[test]
    fn explicit_override_prices_a_subscription_profile() {
        let mut profile = user_profile("github-copilot", "claude-opus-4.6");
        profile.pricing.billing_mode = ModelBillingMode::Subscription;
        profile.pricing.overrides.push((
            "claude-opus-4.6".to_string(),
            TokenPricing::input_output(1.0, 2.0),
        ));
        let cat = pricing_for(&[profile]);
        let mr = ModelRef {
            provider: CostProviderId::OpenAICompatible {
                name: "github-copilot".to_string(),
            },
            model: "claude-opus-4.6".to_string(),
        };
        let (pricing, _) = cat.resolve(&mr).expect("explicit override is billable");
        assert_eq!(
            pricing.token_rates[&cost::pricing::TokenClass::Input].nano_usd_per_token,
            1_000
        );
        assert_eq!(
            pricing.token_rates[&cost::pricing::TokenClass::Output].nano_usd_per_token,
            2_000
        );
    }
}
