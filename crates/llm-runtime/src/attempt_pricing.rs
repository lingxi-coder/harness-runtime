//! Host route validation and USD admission policy for SDK price bounds.

use crate::{LlmError, ResolvedRoute};
use lingxi_llm_client as sdk;

pub type AttemptTokenRates = sdk::protocol::TokenRates;
pub type AttemptPriceBounds = sdk::client::PriceBounds;

fn unavailable(message: impl Into<String>) -> LlmError {
    LlmError::CostUnavailable {
        message: message.into(),
    }
}

pub(crate) fn bounds(
    profile: &sdk::protocol::ProviderProfile,
    route: &ResolvedRoute,
) -> Result<Option<AttemptPriceBounds>, LlmError> {
    let snapshot = sdk::FrozenPricing::capture(profile, &route.display_model, &route.request_model)
        .map_err(crate::upstream::error)?;
    if profile.profile_name != route.profile_name
        || snapshot.model().billing_model != route.pricing_model.billing_model
    {
        return Err(unavailable(
            "attempt price bounds do not match the captured route",
        ));
    }
    let bounds = snapshot
        .interactive_price_bounds()
        .map_err(crate::upstream::error)?;
    if bounds.is_some()
        && snapshot
            .model()
            .pricing
            .as_ref()
            .and_then(|pricing| pricing.currency.as_deref())
            .unwrap_or("USD")
            != "USD"
    {
        return Err(unavailable(
            "attempt price bounds require published per-token USD rates",
        ));
    }
    Ok(bounds)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sdk::protocol::{
        BillingMode, PriceBoundary, PriceBucket, PriceMultiplier, PriceRule, ServiceTier,
        TokenPricing,
    };

    fn fixture() -> (sdk::protocol::ProviderProfile, ResolvedRoute) {
        let mut profile = sdk::builtin_providers()
            .unwrap()
            .into_iter()
            .find(|profile| profile.profile_name == "deepseek")
            .unwrap();
        profile
            .models
            .retain(|model| model.request_model == "deepseek-flash");
        let model = &profile.models[0];
        let provider = crate::ProviderId::OpenAICompatible {
            name: profile.profile_name.clone(),
        };
        let route = ResolvedRoute {
            provider_id: provider.clone(),
            profile_name: profile.profile_name.clone(),
            request_model: model.request_model.clone(),
            display_model: model.display_model.clone(),
            pricing_model: crate::PricingModelRef {
                pricing_provider_id: provider,
                billing_model: model.billing_model.clone(),
                request_model: model.request_model.clone(),
                display_model: model.display_model.clone(),
            },
            capabilities: crate::Capabilities::default(),
            connection_chain: Vec::new(),
            failover: crate::FailoverTriggers::NONE,
        };
        (profile, route)
    }

    fn rates(input: f64, output: f64, cache: Option<f64>) -> AttemptTokenRates {
        AttemptTokenRates {
            input_per_million: Some(input),
            output_per_million: Some(output),
            cache_read_per_million: cache,
            ..Default::default()
        }
    }

    fn with_rules(profile: &mut sdk::protocol::ProviderProfile, rules: Vec<PriceRule>) {
        profile.pricing.peak = None;
        profile.models[0].pricing = Some(TokenPricing {
            input_per_million: Some(1.0),
            output_per_million: Some(2.0),
            cache_read_per_million: Some(0.1),
            rules,
            ..Default::default()
        });
    }

    #[test]
    fn deepseek_bound_retains_peak_rates_and_unknown_cache_write() {
        let (profile, route) = fixture();
        let upper = bounds(&profile, &route).unwrap().unwrap();
        assert_eq!(upper.standard.input_per_million, Some(0.3));
        assert_eq!(upper.standard.output_per_million, Some(1.2));
        assert_eq!(upper.standard.cache_read_per_million, Some(0.006));
        assert_eq!(upper.standard.cache_write_per_million, None);
        assert_eq!(upper.fast, None);
    }

    #[test]
    fn saved_override_bounds_the_effective_sdk_row_above_published_peak() {
        let (_, route) = fixture();
        let mut host = crate::builtin_presets()
            .providers
            .into_iter()
            .find(|profile| profile.profile_name == route.profile_name)
            .unwrap();
        host.pricing.overrides.push((
            route.display_model.clone(),
            crate::PricingOverride::input_output(10.0, 20.0),
        ));
        let effective = crate::upstream::profile(&host).unwrap();
        let upper = bounds(&effective, &route).unwrap().unwrap();
        assert_eq!(upper.standard.input_per_million, Some(10.0));
        assert_eq!(upper.standard.output_per_million, Some(20.0));
        assert_eq!(upper.fast, None);
    }

    #[test]
    fn context_bounds_use_the_high_band_and_preserve_unknown_buckets() {
        let (mut profile, route) = fixture();
        with_rules(
            &mut profile,
            vec![
                PriceRule {
                    max_input_tokens: Some(99),
                    rates: rates(1.0, 2.0, Some(0.1)),
                    ..Default::default()
                },
                PriceRule {
                    min_input_tokens: Some(100),
                    rates: rates(3.0, 5.0, None),
                    ..Default::default()
                },
            ],
        );
        let upper = bounds(&profile, &route).unwrap().unwrap();
        assert_eq!(upper.standard.input_per_million, Some(3.0));
        assert_eq!(upper.standard.output_per_million, Some(5.0));
        assert_eq!(upper.standard.reasoning_per_million, Some(5.0));
        assert_eq!(upper.standard.cache_read_per_million, None);
    }

    #[test]
    fn fast_multiplier_inherits_the_sdk_selected_standard_context() {
        let (mut profile, route) = fixture();
        with_rules(
            &mut profile,
            vec![
                PriceRule {
                    max_input_tokens: Some(99),
                    rates: rates(2.0, 5.0, Some(0.2)),
                    ..Default::default()
                },
                PriceRule {
                    min_input_tokens: Some(100),
                    rates: rates(4.0, 10.0, Some(0.4)),
                    ..Default::default()
                },
                PriceRule {
                    service_tier: ServiceTier::Fast,
                    multiplier: Some(PriceMultiplier {
                        factor: 2.0,
                        buckets: vec![PriceBucket::Input, PriceBucket::Output],
                    }),
                    ..Default::default()
                },
            ],
        );
        let upper = bounds(&profile, &route).unwrap().unwrap();
        let fast = upper.fast.unwrap();
        assert_eq!(upper.standard.input_per_million, Some(4.0));
        assert_eq!(fast.input_per_million, Some(8.0));
        assert_eq!(fast.output_per_million, Some(20.0));
        assert_eq!(fast.reasoning_per_million, Some(20.0));
        assert_eq!(fast.cache_read_per_million, Some(0.4));
        profile.models[0].info.features.default_service_tier = None;
        profile.models[0].info.features.fast = sdk::protocol::CapabilitySupport::Supported;
        profile.info.features.fast = sdk::protocol::CapabilitySupport::Supported;
        profile.info.features.default_service_tier = Some(ServiceTier::Fast);
        assert!(bounds(&profile, &route).unwrap().unwrap().default_fast);
        profile.models[0].info.features.default_service_tier = Some(ServiceTier::Standard);
        assert!(!bounds(&profile, &route).unwrap().unwrap().default_fast);
    }

    #[test]
    fn dated_price_change_is_bounded_on_both_sides_without_a_clock() {
        let (mut profile, route) = fixture();
        let boundary = PriceBoundary {
            local: "2027-01-01T00:00:00".into(),
            time_zone: Some("UTC".into()),
        };
        with_rules(
            &mut profile,
            vec![
                PriceRule {
                    valid_until: Some(boundary.clone()),
                    rates: rates(1.0, 2.0, Some(0.1)),
                    ..Default::default()
                },
                PriceRule {
                    valid_from: Some(boundary),
                    rates: rates(3.0, 4.0, Some(0.3)),
                    ..Default::default()
                },
            ],
        );
        let upper = bounds(&profile, &route).unwrap().unwrap();
        assert_eq!(upper.standard.input_per_million, Some(3.0));
        assert_eq!(upper.standard.output_per_million, Some(4.0));
        profile.models[0].pricing.as_mut().unwrap().rules[0]
            .valid_until
            .as_mut()
            .unwrap()
            .time_zone = None;
        assert!(bounds(&profile, &route).is_err());
    }

    #[test]
    fn fixed_rows_need_no_dynamic_bound_and_non_usd_rows_are_rejected() {
        let (mut profile, route) = fixture();
        profile.models[0].pricing.as_mut().unwrap().currency = Some("CNY".into());
        assert!(bounds(&profile, &route).is_err());
        profile.models[0].pricing.as_mut().unwrap().currency = None;
        profile.models[0].billing_mode = Some(BillingMode::Subscription);
        assert!(bounds(&profile, &route).is_err());
        profile.models[0].billing_mode = None;
        profile.pricing.peak = None;
        assert_eq!(bounds(&profile, &route).unwrap(), None);
    }
}
