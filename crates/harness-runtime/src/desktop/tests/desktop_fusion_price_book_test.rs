    use super::*;

    /// Serializes the three tests below, which are the ONLY tests in this
    /// binary that touch `ENABLE_PROMPT_CACHING_1H` (`git grep -n
    /// ENABLE_PROMPT_CACHING_1H lingxi-code/apps/harness-runtime::desktop` matches
    /// nothing else), so one lock is enough — a second guard over the same
    /// variable would serialize nothing.
    static PROMPT_CACHE_1H_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn rates_for_prices_a_known_model_through_the_shared_catalog() {
        // Before this adapter was wired in (F001/G003), `FusionOrchestrator`
        // always ran with the `()` price book, so this call would have
        // returned `None` for every model and made `budget::quote` reject
        // any token-billed panel under a session `--max-budget`.
        let catalog = Arc::new(cost::PricingCatalog::builtin_reference());
        let book = DesktopFusionPriceBook::with_ttl_gate(catalog, false);
        let rates = fusion::FusionPriceBook::rates_for(&book, "anthropic", "claude-opus-4-6")
            .expect("the builtin reference catalog prices claude-opus-4-6");
        assert_eq!(rates.input_nano_usd_per_token, 5_000);
        assert_eq!(rates.output_nano_usd_per_token, 25_000);
        // G003-cache follow-up: the SAME catalog's cache rates
        // (`insert_anthropic("claude-opus-4-6", 5_000, 25_000, 6_250, 500)`)
        // must reach `ModelRates` too, not just input/output — otherwise a
        // Fusion run using prompt caching under-bills against the exact
        // catalog the main turn loop bills the identical usage from in full.
        assert_eq!(rates.cache_write_nano_usd_per_token, 6_250);
        assert_eq!(rates.cache_read_nano_usd_per_token, 500);
    }

    /// Finding [1]: a catalog entry that prices `TokenClass::ReasoningOutput`
    /// separately (the shape `provider-config::cost_translate` produces for
    /// a models.dev row with `reasoning_per_million > 0`, e.g. DeepSeek /
    /// Gemini / OpenRouter) must reach `ModelRates.reasoning_nano_usd_per_token`
    /// — before this fix `rates_for` never read `TokenClass::ReasoningOutput`
    /// at all, so this rate was silently dropped and `price_component` billed
    /// reasoning tokens at 0 regardless of what the catalog priced them at.
    #[test]
    fn rates_for_reads_the_reasoning_output_rate() {
        let mr = cost::ModelRef {
            provider: cost::pricing::ProviderId::OpenAICompatible {
                name: "deepseek".to_string(),
            },
            model: "deepseek-v4-pro".to_string(),
        };
        let mut rates = std::collections::HashMap::new();
        rates.insert(
            cost::pricing::TokenClass::Input,
            cost::pricing::MoneyPerToken {
                nano_usd_per_token: 435,
            },
        );
        rates.insert(
            cost::pricing::TokenClass::Output,
            cost::pricing::MoneyPerToken {
                nano_usd_per_token: 870,
            },
        );
        rates.insert(
            cost::pricing::TokenClass::ReasoningOutput,
            cost::pricing::MoneyPerToken {
                nano_usd_per_token: 870,
            },
        );
        let catalog = Arc::new(cost::PricingCatalog::builtin_reference().with_entry(
            cost::pricing::ModelPricing {
                model_ref: mr.clone(),
                token_rates: rates,
                non_token_rates_nano_usd: std::collections::HashMap::new(),
                effective_from: None,
                source: cost::pricing::PricingSource::RemoteManagedSettings,
            },
        ));
        let book = DesktopFusionPriceBook::with_ttl_gate(catalog, false);
        let priced = fusion::FusionPriceBook::rates_for(&book, "deepseek", "deepseek-v4-pro")
            .expect("deepseek-v4-pro has token rates");
        assert_eq!(
            priced.reasoning_nano_usd_per_token, 870,
            "the catalog's ReasoningOutput rate must reach ModelRates, not default to 0"
        );
    }

    /// Round-7 finding [1] (RED): `ENABLE_PROMPT_CACHING_1H` makes
    /// `ApiService::build_request` stamp `ttl_1h` on the system cache blocks
    /// of every request it assembles — Fusion panel turns and the
    /// analyst/synth side queries included (llm-runtime/src/service.rs:1090,
    /// :1186). Anthropic then bills those cache-CREATION tokens at the 1-hour
    /// rate, which the shared catalog carries as
    /// `TokenClass::CacheWrite1h` (`cost/src/pricing.rs:543-560`; for the
    /// $5/$25 Opus tier, 6_250 -> 10_000 nano-USD/token) and which the main
    /// turn loop bills correctly through
    /// `orchestrator::cost_wiring` -> `CostCalculator`. `rates_for` read only
    /// `TokenClass::CacheWrite` (the 5-minute rate), so Fusion priced the
    /// same tokens 37.5% low against the SAME catalog while
    /// `price_realized_usage` still reported `estimated: false`.
    #[test]
    fn rates_for_prices_cache_writes_at_the_one_hour_rate_when_the_1h_gate_is_on() {
        let _guard = PROMPT_CACHE_1H_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::env::set_var("ENABLE_PROMPT_CACHING_1H", "1");
        let catalog = Arc::new(cost::PricingCatalog::builtin_reference());
        let book = DesktopFusionPriceBook::new(catalog);
        let rates = fusion::FusionPriceBook::rates_for(&book, "anthropic", "claude-opus-4-6")
            .expect("the builtin reference catalog prices claude-opus-4-6");
        std::env::remove_var("ENABLE_PROMPT_CACHING_1H");
        assert_eq!(
            rates.cache_write_nano_usd_per_token, 10_000,
            "with the 1h prompt-cache TTL armed, Fusion must price \
cache-creation tokens through TokenClass::CacheWrite1h (10_000) — pricing \
them at the 5-minute rate (6_250) under-bills the run 37.5% against the \
same catalog the main turn loop bills them from"
        );
        assert!(
            rates.cache_write_rate_is_ttl_approximated,
            "under the gate the single flattened cache_write bucket mixes 1h system \
blocks with the 5-minute last-message breakpoint, so the rate is an approximation and \
`price_realized_usage` must refuse to report `estimated: false` over it"
        );
    }

    /// The companion negative: with the dormant opt-in OFF (the shipped
    /// default), the 5-minute rate is the correct and only rate, so this
    /// fix must be a no-op for every normal session.
    #[test]
    fn rates_for_prices_cache_writes_at_the_five_minute_rate_by_default() {
        let _guard = PROMPT_CACHE_1H_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::env::remove_var("ENABLE_PROMPT_CACHING_1H");
        let catalog = Arc::new(cost::PricingCatalog::builtin_reference());
        let book = DesktopFusionPriceBook::new(catalog);
        let rates = fusion::FusionPriceBook::rates_for(&book, "anthropic", "claude-opus-4-6")
            .expect("the builtin reference catalog prices claude-opus-4-6");
        assert_eq!(
            rates.cache_write_nano_usd_per_token, 6_250,
            "without ENABLE_PROMPT_CACHING_1H every cache-creation token is \
written with the default 5-minute TTL and must stay priced at 6_250"
        );
        assert!(
            !rates.cache_write_rate_is_ttl_approximated,
            "with the gate off every cache block is 5-minute: the rate is EXACT and the \
run must keep reporting an exact total"
        );
    }

    /// A catalog entry with no `CacheWrite1h` rate at all — the shape
    /// `provider-config::cost_translate::model_pricing_from_token_pricing`
    /// produces for every models.dev / OpenRouter row (it inserts Input,
    /// Output, CacheWrite, CacheRead and ReasoningOutput and never
    /// CacheWrite1h) — must fall back to its 5-minute rate rather than
    /// dropping to 0 when the 1h gate is on.
    #[test]
    fn rates_for_falls_back_to_the_five_minute_rate_when_no_1h_rate_exists() {
        let _guard = PROMPT_CACHE_1H_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::env::set_var("ENABLE_PROMPT_CACHING_1H", "1");
        let mr = cost::ModelRef {
            provider: cost::pricing::ProviderId::OpenAICompatible {
                name: "openrouter".to_string(),
            },
            model: "some-cached-model".to_string(),
        };
        let mut rates = std::collections::HashMap::new();
        rates.insert(
            cost::pricing::TokenClass::Input,
            cost::pricing::MoneyPerToken {
                nano_usd_per_token: 100,
            },
        );
        rates.insert(
            cost::pricing::TokenClass::Output,
            cost::pricing::MoneyPerToken {
                nano_usd_per_token: 200,
            },
        );
        rates.insert(
            cost::pricing::TokenClass::CacheWrite,
            cost::pricing::MoneyPerToken {
                nano_usd_per_token: 125,
            },
        );
        let catalog = Arc::new(cost::PricingCatalog::builtin_reference().with_entry(
            cost::pricing::ModelPricing {
                model_ref: mr,
                token_rates: rates,
                non_token_rates_nano_usd: std::collections::HashMap::new(),
                effective_from: None,
                source: cost::pricing::PricingSource::RemoteManagedSettings,
            },
        ));
        let book = DesktopFusionPriceBook::new(catalog);
        let priced = fusion::FusionPriceBook::rates_for(&book, "openrouter", "some-cached-model")
            .expect("the entry has token rates");
        std::env::remove_var("ENABLE_PROMPT_CACHING_1H");
        assert_eq!(
            priced.cache_write_nano_usd_per_token, 125,
            "an entry with no CacheWrite1h class must keep its 5-minute \
rate under the 1h gate, never fall to 0"
        );
    }

    #[test]
    fn rates_for_is_none_for_an_explicitly_unpriced_model() {
        // A component the catalog genuinely cannot price must surface as
        // `None` (the caller then marks the run `estimated = true`, or —
        // under a session cap at `quote()` time — rejects the run per
        // design §4) rather than the adapter guessing a rate.
        let catalog = Arc::new(cost::PricingCatalog::builtin_reference().mark_unpriced(
            cost::ModelRef {
                provider: cost::pricing::ProviderId::Anthropic,
                model: "claude-opus-4-6".into(),
            },
        ));
        let book = DesktopFusionPriceBook::with_ttl_gate(catalog, false);
        assert!(
            fusion::FusionPriceBook::rates_for(&book, "anthropic", "claude-opus-4-6").is_none()
        );
    }
