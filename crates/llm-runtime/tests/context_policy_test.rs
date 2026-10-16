//! Selected connection context and pricing projection regressions.
use llm_runtime::{builtin_presets, ClientConfig, ModelRegistry};

#[test]
fn context_catalog_projects_actual_price_ranges_and_separate_input_budget() {
    let providers = builtin_presets().providers;
    let registry = ModelRegistry::from_config(ClientConfig { providers }).unwrap();
    let rows = registry.available_models();
    for (provider, model, end) in [
        ("openai", "gpt-6-astra", 272000),
        ("gemini", "gemini-3.1-pro-preview", 200000),
        ("anthropic", "claude-haiku-5-5", 100000),
        ("grok", "grok-4.3", 199999),
    ] {
        let row = rows
            .iter()
            .find(|r| r.profile_name == provider && r.request_model == model)
            .unwrap();
        let budget = row.metadata.context_budget.as_ref().unwrap();
        assert_eq!(budget.standard_price_input_tokens, Some(end), "{provider}");
        assert_eq!(budget.effective_input_budget, Some(end), "{provider}");
        let tiers = &row.metadata.pricing.as_ref().unwrap().tiers;
        assert!(
            tiers.iter().any(|t| t.max_input_tokens == Some(end)),
            "{provider}"
        );
        assert!(
            tiers.iter().any(|t| t.context_threshold_tokens == end + 1),
            "{provider}"
        );
    }
}

#[test]
fn same_model_custom_connection_limits_remain_distinct_in_model_listing() {
    let template = builtin_presets()
        .providers
        .into_iter()
        .find(|p| p.profile_name == "openai-chat")
        .unwrap();
    let providers = [8192, 128000, 1000000]
        .into_iter()
        .map(|window| {
            let mut p = template.clone();
            p.profile_name = format!("custom-{window}");
            p.provider_id = llm_runtime::ProviderId::Custom {
                name: p.profile_name.clone(),
            };
            p.base_url = "https://custom.test/v1".into();
            p.wire_profile = None;
            p.connection = Default::default();
            p.models.truncate(1);
            let m = &mut p.models[0];
            m.request_model = "same".into();
            m.display_model = "same".into();
            m.billing_model = "same".into();
            m.aliases.clear();
            m.metadata = lingxi_core::host::ModelMetadata {
                context_window_tokens: Some(window),
                max_input_tokens: Some(window - 100),
                ..Default::default()
            };
            p
        })
        .collect();
    let rows = ModelRegistry::from_config(ClientConfig { providers })
        .unwrap()
        .available_models();
    for window in [8192, 128000, 1000000] {
        let row = rows
            .iter()
            .find(|r| r.profile_name == format!("custom-{window}"))
            .unwrap();
        assert_eq!(
            row.metadata
                .context_budget
                .as_ref()
                .unwrap()
                .effective_input_budget,
            Some(window - 100)
        );
    }
}
