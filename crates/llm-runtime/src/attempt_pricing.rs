//! Conservative published rate bounds for admitting a physical model attempt.
//!
//! These rates authorize a budget; they are never an actual usage estimate.
//! The SDK still selects every applicable price rule, and its frozen estimate
//! must supply the final charge after the provider reports complete usage.

use std::collections::BTreeSet;

use lingxi_llm_client as sdk;
use sdk::protocol::{BillingMode, PriceStatus, PricingContext, ServiceTier, Submission};

use crate::{LlmError, ResolvedRoute};

/// Optional published USD prices per million tokens for each billing bucket.
pub type AttemptTokenRates = sdk::protocol::TokenRates;

/// Per-bucket ceilings across the selected model's published pricing contexts.
#[derive(Debug, Clone, PartialEq)]
pub struct AttemptPriceBounds {
    /// An omitted request tier uses the selected model/connection's Fast default.
    pub default_fast: bool,
    /// Interactive standard execution. Missing buckets remain unpriced.
    pub standard: AttemptTokenRates,
    /// Interactive Fast execution, when the SDK can select published prices.
    pub fast: Option<AttemptTokenRates>,
}

fn unavailable(message: impl Into<String>) -> LlmError {
    LlmError::CostUnavailable {
        message: message.into(),
    }
}

/// Capture dynamic price ceilings without substituting the current time or an
/// estimated prompt size for facts that can change before dispatch.
pub(crate) fn bounds(
    profile: &sdk::protocol::ProviderProfile,
    route: &ResolvedRoute,
) -> Result<Option<AttemptPriceBounds>, LlmError> {
    let snapshot = sdk::FrozenPricing::capture(profile, &route.display_model, &route.request_model)
        .map_err(crate::upstream::error)?;
    let model = snapshot.model();
    if profile.profile_name != route.profile_name
        || model.billing_model != route.pricing_model.billing_model
    {
        return Err(unavailable(
            "attempt price bounds do not match the captured route",
        ));
    }
    let default_fast = model
        .info
        .features
        .on_connection(&profile.info.features)
        .default_service_tier
        == Some(ServiceTier::Fast);
    let dynamic = default_fast
        || profile.pricing.peak.is_some()
        || model
            .pricing
            .as_ref()
            .is_some_and(|prices| !prices.rules.is_empty());
    if !dynamic {
        return Ok(None);
    }
    let prices = model
        .pricing
        .as_ref()
        .ok_or_else(|| unavailable("dynamic attempt prices are unpublished"))?;
    if model.billing_mode_on(&profile.pricing) != BillingMode::PerToken
        || prices.currency.as_deref().unwrap_or("USD") != "USD"
    {
        return Err(unavailable(
            "attempt price bounds require published per-token USD rates",
        ));
    }

    // The SDK changes rule selection only at these input/time boundaries.
    // Sampling both sides covers gaps and open-ended bands without reproducing
    // rule precedence. Ambiguous billing time zones cannot authorize a bound.
    let mut inputs = BTreeSet::from([0, u64::MAX]);
    let mut times = BTreeSet::from([0, u64::MAX]);
    for rule in &prices.rules {
        for boundary in [rule.min_input_tokens, rule.max_input_tokens]
            .into_iter()
            .flatten()
        {
            insert_boundary(&mut inputs, boundary);
        }
        for boundary in [rule.valid_from.as_ref(), rule.valid_until.as_ref()]
            .into_iter()
            .flatten()
        {
            let (earliest, latest) = boundary.utc_bounds().map_err(unavailable)?;
            if earliest != latest {
                return Err(unavailable(
                    "attempt price boundaries require a confirmed billing time zone",
                ));
            }
            insert_boundary(&mut times, earliest);
            insert_boundary(&mut times, latest);
        }
    }

    // Peak schedules repeat independently of dated rules. Removing the
    // schedule from this temporary snapshot lets us bound it with one factor
    // instead of enumerating calendar days. Applying the factor even to a
    // rule that opts out is deliberately conservative for budget admission.
    let schedule_factor = if let Some(schedule) = &profile.pricing.peak {
        schedule.validate().map_err(unavailable)?;
        schedule.off_peak_multiplier.max(1.0)
    } else {
        1.0
    };
    let mut unscheduled = profile.clone();
    unscheduled.pricing.peak = None;
    let snapshot =
        sdk::FrozenPricing::capture(&unscheduled, &route.display_model, &route.request_model)
            .map_err(crate::upstream::error)?;
    let standard = tier_bounds(
        &snapshot,
        ServiceTier::Standard,
        &inputs,
        &times,
        schedule_factor,
    )?
    .ok_or_else(|| unavailable("no published standard rates can bound this attempt"))?;
    let fast = tier_bounds(
        &snapshot,
        ServiceTier::Fast,
        &inputs,
        &times,
        schedule_factor,
    )?;
    Ok(Some(AttemptPriceBounds {
        standard,
        fast,
        default_fast,
    }))
}

fn insert_boundary(points: &mut BTreeSet<u64>, boundary: u64) {
    points.extend([
        boundary.saturating_sub(1),
        boundary,
        boundary.saturating_add(1),
    ]);
}

fn tier_bounds(
    snapshot: &sdk::FrozenPricing,
    tier: ServiceTier,
    inputs: &BTreeSet<u64>,
    times: &BTreeSet<u64>,
    schedule_factor: f64,
) -> Result<Option<AttemptTokenRates>, LlmError> {
    let mut upper: Option<AttemptTokenRates> = None;
    for input_tokens in inputs {
        for unix_seconds in times {
            let quote = snapshot
                .quote(&PricingContext {
                    service_tier: Some(tier),
                    submission: Submission::Interactive,
                    input_tokens: Some(*input_tokens),
                    unix_seconds: Some(*unix_seconds),
                })
                .map_err(crate::upstream::error)?;
            if quote.status != PriceStatus::Priced {
                continue;
            }
            if quote.currency != "USD" || quote.unit != "per_million_tokens" {
                return Err(unavailable("attempt quote is not a USD token price"));
            }
            let Some(mut rates) = quote.rates else {
                continue;
            };
            rates.reasoning_per_million = rates.reasoning_per_million.or(rates.output_per_million);
            for rate in [
                &mut rates.input_per_million,
                &mut rates.output_per_million,
                &mut rates.cache_read_per_million,
                &mut rates.cache_write_per_million,
                &mut rates.cache_write_1h_per_million,
                &mut rates.reasoning_per_million,
            ] {
                if let Some(value) = rate {
                    *value *= schedule_factor;
                    if !value.is_finite() || *value < 0.0 {
                        return Err(unavailable("attempt rate bound exceeds the numeric range"));
                    }
                }
            }
            upper = Some(match upper {
                None => rates,
                Some(previous) => merge_bounds(previous, rates),
            });
        }
    }
    Ok(upper)
}

fn merge_bounds(a: AttemptTokenRates, b: AttemptTokenRates) -> AttemptTokenRates {
    // A known price in one band cannot bound an unpublished price in another.
    // Keep the missing bucket explicit so admission can reject it if reachable.
    let maximum = |a: Option<f64>, b: Option<f64>| a.zip(b).map(|(a, b)| a.max(b));
    AttemptTokenRates {
        input_per_million: maximum(a.input_per_million, b.input_per_million),
        output_per_million: maximum(a.output_per_million, b.output_per_million),
        cache_read_per_million: maximum(a.cache_read_per_million, b.cache_read_per_million),
        cache_write_per_million: maximum(a.cache_write_per_million, b.cache_write_per_million),
        cache_write_1h_per_million: maximum(
            a.cache_write_1h_per_million,
            b.cache_write_1h_per_million,
        ),
        reasoning_per_million: maximum(a.reasoning_per_million, b.reasoning_per_million),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sdk::protocol::{PriceBoundary, PriceBucket, PriceMultiplier, PriceRule, TokenPricing};

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
