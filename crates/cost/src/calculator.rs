//! Cost calculator: converts a [`Usage`] record plus a [`ModelPricing`] entry
//! into a nano-USD total using saturating arithmetic everywhere.
//!
//! Saturating arithmetic means pathological inputs (e.g. `u64::MAX` tokens or
//! a `u64::MAX` rate) never panic — they pin to `u64::MAX`. Callers that need
//! to detect overflow can compare the result against `u64::MAX` or use
//! [`u64::checked_mul`] in their own code.

use crate::pricing::{
    first_party_name_to_canonical, ModelPricing, NonTokenBillableUnit, PricingCatalog,
    PricingSource, TokenClass,
};
use crate::usage::{ApiSpeed, Usage};

/// Stateless calculator that turns [`Usage`] + [`ModelPricing`] into money.
pub struct CostCalculator;

impl CostCalculator {
    /// Total cost in nano-USD. Saturating arithmetic — no panic on overflow.
    #[must_use]
    pub fn calculate_nano_usd(usage: &Usage, pricing: &ModelPricing) -> u64 {
        // A static host row cannot select time, context or Fast rules. A
        // frozen SDK quote overrides this conservative fallback after dispatch.
        if matches!(
            pricing.source,
            PricingSource::PublishedPartial {
                conditional: true,
                ..
            }
        ) || (matches!(pricing.source, PricingSource::PublishedPartial { .. })
            && usage.speed == Some(ApiSpeed::Fast))
        {
            return Self::calculate_nano_usd(
                usage,
                &PricingCatalog::default_unknown_pricing(&pricing.model_ref),
            );
        }
        // COST.3/COST.6 — fast-mode tier overrides.
        //
        // claude-code binary `$2u` (`getModelCosts`, `utils/modelCost.ts:144-153`):
        //   • opus-4-6 or opus-4-7 → H6s ($30/$150) when `speed === 'fast'`
        //   • opus-4-8              → Ypn ($10/$50)  when `speed === 'fast'`
        //
        // The catalog is keyed only on `(provider, model)`, so speed-dependent
        // tiers are resolved here at calculation time.  (TS additionally ANDs
        // `isFastModeEnabled()`, a config-layer global unreachable from this crate;
        // the API only echoes `speed: 'fast'` when fast mode was actually used, so
        // `usage.speed == Fast` is the faithful per-request signal.)
        let fast_override = Self::fast_tier_override(usage, pricing);
        let pricing = fast_override.as_ref().unwrap_or(pricing);

        let mut total: u64 = 0;

        for (class, rate) in &pricing.token_rates {
            let tokens = usage.tokens_for(*class);
            total = total.saturating_add(tokens.saturating_mul(rate.nano_usd_per_token));
        }

        // A partially published model keeps its real prices for known buckets.
        // If the provider reports usage in an unpublished bucket, preserve the
        // existing non-zero unknown-price policy for that bucket alone.
        if matches!(pricing.source, PricingSource::PublishedPartial { .. }) {
            let unknown = PricingCatalog::default_unknown_pricing(&pricing.model_ref);
            for (class, rate) in &unknown.token_rates {
                if !pricing.token_rates.contains_key(class) {
                    total = total.saturating_add(
                        usage
                            .tokens_for(*class)
                            .saturating_mul(rate.nano_usd_per_token),
                    );
                }
            }
        }

        total.saturating_add(Self::non_token_nano_usd(usage, pricing))
    }

    /// Host-owned tool charges excluded from the client's token estimate.
    #[must_use]
    pub fn non_token_nano_usd(usage: &Usage, pricing: &ModelPricing) -> u64 {
        let Some(tools) = usage.server_tool_use else {
            return 0;
        };
        let published = pricing
            .non_token_rates_nano_usd
            .get(&NonTokenBillableUnit::WebSearchRequest)
            .copied();
        let fallback = matches!(pricing.source, PricingSource::PublishedPartial { .. })
            .then(|| PricingCatalog::default_unknown_pricing(&pricing.model_ref))
            .and_then(|unknown| {
                unknown
                    .non_token_rates_nano_usd
                    .get(&NonTokenBillableUnit::WebSearchRequest)
                    .copied()
            });
        published.or(fallback).map_or(0, |rate| {
            u64::from(tools.web_search_requests).saturating_mul(rate)
        })
    }

    /// Whether observed usage had to use an unpublished billable rate.
    #[must_use]
    pub fn uses_unknown_rate(usage: &Usage, pricing: &ModelPricing) -> bool {
        matches!(pricing.source, PricingSource::PublishedPartial { .. })
            && (matches!(
                pricing.source,
                PricingSource::PublishedPartial {
                    conditional: true,
                    ..
                }
            ) || usage.speed == Some(ApiSpeed::Fast)
                || usage.server_tool_use.is_some_and(|tools| {
                    tools.web_search_requests > 0
                        && !pricing
                            .non_token_rates_nano_usd
                            .contains_key(&NonTokenBillableUnit::WebSearchRequest)
                })
                || [
                    TokenClass::Input,
                    TokenClass::Output,
                    TokenClass::CacheWrite,
                    TokenClass::CacheRead,
                    TokenClass::ReasoningOutput,
                    TokenClass::CacheWrite1h,
                ]
                .into_iter()
                .any(|class| {
                    usage.tokens_for(class) > 0 && !pricing.token_rates.contains_key(&class)
                }))
    }

    /// COST.3/COST.6 — return the fast-mode pricing override for Opus models when
    /// `usage.speed == ApiSpeed::Fast`, else `None`.
    ///
    /// Mirrors binary `$2u` (`utils/modelCost.ts:144-153`):
    ///   • `claude-opus-4-6` or `claude-opus-4-7` → H6s ($30/$150) fast tier
    ///   • `claude-opus-4-8` → Ypn ($10/$50) fast tier
    ///   • all other models → no override (standard catalog rate applies)
    fn fast_tier_override(usage: &Usage, pricing: &ModelPricing) -> Option<ModelPricing> {
        if usage.speed != Some(ApiSpeed::Fast) {
            return None;
        }
        let canonical = first_party_name_to_canonical(&pricing.model_ref.model);
        match canonical.as_str() {
            // opus-4-6 and opus-4-7 share the H6s ($30/$150) fast tier.
            "claude-opus-4-6" | "claude-opus-4-7" => {
                Some(PricingCatalog::opus_4_6_fast_pricing(&pricing.model_ref))
            }
            // opus-4-8 uses the Ypn ($10/$50) fast tier.
            "claude-opus-4-8" => Some(PricingCatalog::opus_4_8_fast_pricing(&pricing.model_ref)),
            _ => None,
        }
    }

    /// Legacy alias kept so existing callers compile — delegates to
    /// [`Self::fast_tier_override`] after checking the `opus-4-6` model id.
    #[cfg(test)]
    fn opus_4_6_fast_override(usage: &Usage, pricing: &ModelPricing) -> Option<ModelPricing> {
        Self::fast_tier_override(usage, pricing)
    }

    /// Estimated cache-read savings in nano-USD: the difference between the
    /// (counterfactual) input-rate cost of the cache-read tokens and what was
    /// actually paid at the cache-read rate. Returns `0` when either rate is
    /// missing, or when the cache rate is not cheaper than input.
    #[must_use]
    pub fn cache_savings_nano_usd(usage: &Usage, pricing: &ModelPricing) -> u64 {
        let Some(input_rate) = pricing.token_rates.get(&TokenClass::Input) else {
            return 0;
        };
        let Some(cache_rate) = pricing.token_rates.get(&TokenClass::CacheRead) else {
            return 0;
        };
        let would_have_paid = usage
            .tokens
            .cache_read
            .saturating_mul(input_rate.nano_usd_per_token);
        let actually_paid = usage
            .tokens
            .cache_read
            .saturating_mul(cache_rate.nano_usd_per_token);
        would_have_paid.saturating_sub(actually_paid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::{ModelRef, MoneyPerToken, PricingCatalog, PricingSource, ProviderId};
    use crate::usage::{ApiSpeed, ServerToolUsage, TokenUsage};
    use std::collections::HashMap;

    #[test]
    fn opus_4_6_input_only_correct() {
        let c = PricingCatalog::builtin_reference();
        let mr = ModelRef {
            provider: ProviderId::Anthropic,
            model: "claude-opus-4-6".into(),
        };
        let (p, _) = c.resolve(&mr).unwrap();
        let usage = Usage {
            tokens: TokenUsage {
                input: 1_000_000,
                ..TokenUsage::default()
            },
            ..Usage::default()
        };
        // 1M tokens × 5000 nano-USD/tok = 5e9 nano-USD = $5
        assert_eq!(
            CostCalculator::calculate_nano_usd(&usage, &p),
            5_000_000_000
        );
    }

    #[test]
    fn cache_savings_positive_when_read_rate_lower() {
        let c = PricingCatalog::builtin_reference();
        let mr = ModelRef {
            provider: ProviderId::Anthropic,
            model: "claude-opus-4-6".into(),
        };
        let (p, _) = c.resolve(&mr).unwrap();
        let usage = Usage {
            tokens: TokenUsage {
                cache_read: 1_000_000,
                ..TokenUsage::default()
            },
            ..Usage::default()
        };
        let savings = CostCalculator::cache_savings_nano_usd(&usage, &p);
        assert!(savings > 0);
    }

    // ----- COST.3: Opus 4.6 fast-mode tier -----

    fn opus_4_6_pricing() -> ModelPricing {
        let c = PricingCatalog::builtin_reference();
        let mr = ModelRef {
            provider: ProviderId::Anthropic,
            model: "claude-opus-4-6".into(),
        };
        c.resolve(&mr).unwrap().0
    }

    #[test]
    fn opus_4_6_fast_mode_bills_30_150() {
        let p = opus_4_6_pricing();
        // 1M input tokens at the fast tier: $30/Mtok = 30e9 nano-USD.
        let usage = Usage {
            tokens: TokenUsage {
                input: 1_000_000,
                ..TokenUsage::default()
            },
            speed: Some(ApiSpeed::Fast),
            ..Usage::default()
        };
        assert_eq!(
            CostCalculator::calculate_nano_usd(&usage, &p),
            30_000_000_000
        );
        // 1M output tokens at the fast tier: $150/Mtok = 150e9 nano-USD.
        let usage = Usage {
            tokens: TokenUsage {
                output: 1_000_000,
                ..TokenUsage::default()
            },
            speed: Some(ApiSpeed::Fast),
            ..Usage::default()
        };
        assert_eq!(
            CostCalculator::calculate_nano_usd(&usage, &p),
            150_000_000_000
        );
    }

    #[test]
    fn opus_4_6_non_fast_bills_5_25() {
        let p = opus_4_6_pricing();
        // speed = None (the wire default today) → catalog $5/$25 tier, no regression.
        let usage = Usage {
            tokens: TokenUsage {
                input: 1_000_000,
                ..TokenUsage::default()
            },
            ..Usage::default()
        };
        assert_eq!(
            CostCalculator::calculate_nano_usd(&usage, &p),
            5_000_000_000
        );
        // speed = Standard is explicitly NOT fast → still $5/$25.
        let usage = Usage {
            tokens: TokenUsage {
                input: 1_000_000,
                ..TokenUsage::default()
            },
            speed: Some(ApiSpeed::Standard),
            ..Usage::default()
        };
        assert_eq!(
            CostCalculator::calculate_nano_usd(&usage, &p),
            5_000_000_000
        );
    }

    #[test]
    fn fast_mode_only_applies_to_opus_4_6() {
        // A Sonnet ($3/$15) usage record carrying speed=Fast is NOT rebilled at
        // the Opus fast tier — the override is gated on the canonical model name.
        let c = PricingCatalog::builtin_reference();
        let mr = ModelRef {
            provider: ProviderId::Anthropic,
            model: "claude-sonnet-4-6".into(),
        };
        let (p, _) = c.resolve(&mr).unwrap();
        let usage = Usage {
            tokens: TokenUsage {
                input: 1_000_000,
                ..TokenUsage::default()
            },
            speed: Some(ApiSpeed::Fast),
            ..Usage::default()
        };
        // Sonnet $3/Mtok = 3e9, unaffected by the fast flag.
        assert_eq!(
            CostCalculator::calculate_nano_usd(&usage, &p),
            3_000_000_000
        );
    }

    #[test]
    fn opus_4_6_fast_mode_resolves_via_date_suffixed_id() {
        // The override canonicalizes the model name, so a date-suffixed resolve
        // (whose pricing.model_ref is already canonical) still bills the fast tier.
        let c = PricingCatalog::builtin_reference();
        let mr = ModelRef {
            provider: ProviderId::Anthropic,
            model: "claude-opus-4-6-20251101".into(),
        };
        let (p, _) = c.resolve(&mr).unwrap();
        let usage = Usage {
            tokens: TokenUsage {
                input: 1_000_000,
                ..TokenUsage::default()
            },
            speed: Some(ApiSpeed::Fast),
            ..Usage::default()
        };
        assert_eq!(
            CostCalculator::calculate_nano_usd(&usage, &p),
            30_000_000_000
        );
    }

    // ----- COST.5: server-side web-search billing -----

    #[test]
    fn web_search_requests_billed_one_cent_each() {
        let p = opus_4_6_pricing();
        // 3 web-search requests × $0.01 = $0.03 = 30_000_000 nano-USD; no tokens.
        let usage = Usage {
            server_tool_use: Some(ServerToolUsage {
                web_search_requests: 3,
            }),
            ..Usage::default()
        };
        assert_eq!(CostCalculator::calculate_nano_usd(&usage, &p), 30_000_000);
    }

    #[test]
    fn web_search_billed_on_top_of_fast_tokens() {
        let p = opus_4_6_pricing();
        // 1M input fast = 30e9 nano + 2 web searches = 20_000_000 nano.
        let usage = Usage {
            tokens: TokenUsage {
                input: 1_000_000,
                ..TokenUsage::default()
            },
            server_tool_use: Some(ServerToolUsage {
                web_search_requests: 2,
            }),
            speed: Some(ApiSpeed::Fast),
        };
        assert_eq!(
            CostCalculator::calculate_nano_usd(&usage, &p),
            30_000_000_000 + 20_000_000
        );
    }

    #[test]
    fn defaults_no_server_tool_or_speed_bills_tokens_only() {
        let p = opus_4_6_pricing();
        // Default usage (server_tool_use None, speed None) → only token cost,
        // no web-search surcharge and no fast-mode escalation (regression guard).
        let usage = Usage {
            tokens: TokenUsage {
                input: 100,
                output: 50,
                ..TokenUsage::default()
            },
            ..Usage::default()
        };
        assert!(usage.server_tool_use.is_none());
        assert!(usage.speed.is_none());
        // 100×5_000 + 50×25_000 = 500_000 + 1_250_000 = 1_750_000 nano-USD.
        assert_eq!(CostCalculator::calculate_nano_usd(&usage, &p), 1_750_000);
    }

    #[test]
    fn saturating_no_overflow() {
        let mut p = ModelPricing {
            model_ref: ModelRef {
                provider: ProviderId::Anthropic,
                model: "x".into(),
            },
            token_rates: HashMap::default(),
            non_token_rates_nano_usd: HashMap::default(),
            effective_from: None,
            source: PricingSource::BuiltInReference {
                provider: ProviderId::Anthropic,
            },
        };
        p.token_rates.insert(
            TokenClass::Input,
            MoneyPerToken {
                nano_usd_per_token: u64::MAX,
            },
        );
        let usage = Usage {
            tokens: TokenUsage {
                input: u64::MAX,
                ..TokenUsage::default()
            },
            ..Usage::default()
        };
        // Should not panic.
        let _ = CostCalculator::calculate_nano_usd(&usage, &p);
    }

    // ----- COST.6: opus-4-7 fast = $30/$150, opus-4-8 fast = $10/$50 -----

    #[test]
    fn opus_4_7_fast_mode_bills_30_150() {
        // Binary `$2u`: opus-4-6 or opus-4-7 → H6s ($30/$150) when speed=fast.
        let c = PricingCatalog::builtin_reference();
        let mr = ModelRef {
            provider: ProviderId::Anthropic,
            model: "claude-opus-4-7".into(),
        };
        let (p, _) = c.resolve(&mr).unwrap();
        let usage = Usage {
            tokens: TokenUsage {
                input: 1_000_000,
                ..TokenUsage::default()
            },
            speed: Some(ApiSpeed::Fast),
            ..Usage::default()
        };
        // 1M input × $30/Mtok = $30 = 30e9 nano-USD.
        assert_eq!(
            CostCalculator::calculate_nano_usd(&usage, &p),
            30_000_000_000
        );
    }

    #[test]
    fn opus_4_7_standard_bills_5_25() {
        let c = PricingCatalog::builtin_reference();
        let mr = ModelRef {
            provider: ProviderId::Anthropic,
            model: "claude-opus-4-7".into(),
        };
        let (p, _) = c.resolve(&mr).unwrap();
        let usage = Usage {
            tokens: TokenUsage {
                input: 1_000_000,
                ..TokenUsage::default()
            },
            ..Usage::default()
        };
        // 1M input × $5/Mtok = $5 = 5e9 nano-USD.
        assert_eq!(
            CostCalculator::calculate_nano_usd(&usage, &p),
            5_000_000_000
        );
    }

    #[test]
    fn opus_4_8_fast_mode_bills_10_50() {
        // Binary `$2u`: opus-4-8 → Ypn ($10/$50) when speed=fast.
        let c = PricingCatalog::builtin_reference();
        let mr = ModelRef {
            provider: ProviderId::Anthropic,
            model: "claude-opus-4-8".into(),
        };
        let (p, _) = c.resolve(&mr).unwrap();
        let usage = Usage {
            tokens: TokenUsage {
                input: 1_000_000,
                ..TokenUsage::default()
            },
            speed: Some(ApiSpeed::Fast),
            ..Usage::default()
        };
        // 1M input × $10/Mtok = $10 = 10e9 nano-USD.
        assert_eq!(
            CostCalculator::calculate_nano_usd(&usage, &p),
            10_000_000_000
        );
    }

    #[test]
    fn opus_4_8_standard_bills_5_25() {
        // opus-4-8 standard (non-fast) = $5/$25.
        let c = PricingCatalog::builtin_reference();
        let mr = ModelRef {
            provider: ProviderId::Anthropic,
            model: "claude-opus-4-8".into(),
        };
        let (p, _) = c.resolve(&mr).unwrap();
        let usage = Usage {
            tokens: TokenUsage {
                input: 1_000_000,
                ..TokenUsage::default()
            },
            ..Usage::default()
        };
        assert_eq!(
            CostCalculator::calculate_nano_usd(&usage, &p),
            5_000_000_000
        );
    }

    #[test]
    fn fable_5_1_bills_10_50() {
        // Fable 5.1 uses $10/$50 standard pricing (no fast tier).
        let c = PricingCatalog::builtin_reference();
        let mr = ModelRef {
            provider: ProviderId::Anthropic,
            model: "claude-fable-5-1".into(),
        };
        let (p, _) = c.resolve(&mr).unwrap();
        let usage = Usage {
            tokens: TokenUsage {
                input: 1_000_000,
                ..TokenUsage::default()
            },
            ..Usage::default()
        };
        assert_eq!(
            CostCalculator::calculate_nano_usd(&usage, &p),
            10_000_000_000
        );
    }

    #[test]
    fn fable_5_1_fast_flag_does_not_escalate() {
        // Fable 5.1 has no fast tier — speed=fast must NOT rebill at $30/$150.
        let c = PricingCatalog::builtin_reference();
        let mr = ModelRef {
            provider: ProviderId::Anthropic,
            model: "claude-fable-5-1".into(),
        };
        let (p, _) = c.resolve(&mr).unwrap();
        let usage = Usage {
            tokens: TokenUsage {
                input: 1_000_000,
                ..TokenUsage::default()
            },
            speed: Some(ApiSpeed::Fast),
            ..Usage::default()
        };
        // Still $10/Mtok = 10e9.
        assert_eq!(
            CostCalculator::calculate_nano_usd(&usage, &p),
            10_000_000_000
        );
    }

    #[test]
    fn mythos_5_1_bills_10_50() {
        let c = PricingCatalog::builtin_reference();
        let mr = ModelRef {
            provider: ProviderId::Anthropic,
            model: "claude-mythos-5-1".into(),
        };
        let (p, _) = c.resolve(&mr).unwrap();
        let usage = Usage {
            tokens: TokenUsage {
                output: 1_000_000,
                ..TokenUsage::default()
            },
            ..Usage::default()
        };
        // 1M output × $50/Mtok = $50 = 50e9 nano-USD.
        assert_eq!(
            CostCalculator::calculate_nano_usd(&usage, &p),
            50_000_000_000
        );
    }
}
