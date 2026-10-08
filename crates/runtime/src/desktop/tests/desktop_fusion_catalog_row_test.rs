use super::*;

/// WP11: `desktop_fusion_catalog_row` reads `structured_output` straight
/// off `ModelProfile.capabilities` — before WP11, `anthropic_model_profiles()`
/// hard-coded that bit `false` for every Anthropic model, so a pure-Anthropic
/// catalog row was always built with `structured_output: false`, which is
/// what made `resolve_analyst`'s judge filter reject every Anthropic model.
/// This pins the wiring itself, not just the upstream capability table.
#[test]
fn anthropic_rows_carry_structured_output_true() {
    let profiles = llm_runtime::anthropic_model_profiles();
    let opus = profiles
        .iter()
        .find(|m| m.request_model == "claude-opus-5")
        .expect("claude-opus-5 present in anthropic_model_profiles()");
    let row = desktop_fusion_catalog_row(
        "anthropic",
        opus,
        Some(lingxi_core::host::ModelBillingMode::PerToken),
        &llm_runtime::ProtocolFamily::AnthropicMessages,
    );
    assert_eq!(row.profile, "anthropic");
    assert_eq!(row.model, "claude-opus-5");
    assert!(
        row.structured_output,
        "an Anthropic catalog row must carry structured_output: true"
    );
}

/// Finding [2]: a model that is absent from the checked-in
/// `llm_runtime::fusion_hints` table (so `hints_for` returns `None` and
/// `unwrap_or_default()` yields `cost_class: Medium`) must still be
/// classified `Subscription` when its OWNING profile bills by
/// subscription — otherwise `budget::model_peak` hard-rejects it as
/// "token-billed ... has no price" under a session `--max-budget`, even
/// though the model is free at the point of use.
#[test]
fn unhinted_model_on_a_subscription_profile_is_classified_subscription() {
    let unhinted = llm_runtime::ModelProfile {
        display_model: "claude-sonnet-4.6".to_string(),
        request_model: "claude-sonnet-4.6".to_string(),
        billing_model: "claude-sonnet-4.6".to_string(),
        aliases: Vec::new(),
        description: None,
        metadata: lingxi_core::host::ModelMetadata::default(),
        capabilities: llm_runtime::Capabilities::default(),
    };
    let row = desktop_fusion_catalog_row(
        "github-copilot",
        &unhinted,
        Some(lingxi_core::host::ModelBillingMode::Subscription),
        &llm_runtime::ProtocolFamily::OpenAiChat,
    );
    assert_eq!(
        row.hints.cost_class,
        lingxi_core::host::FusionCostClass::Subscription,
        "a model unlisted in fusion_hints must inherit its profile's Subscription billing, \
not fall back to FusionModelHints::default()'s cost_class: Medium"
    );
}

/// The override must not fire for a `PerToken` profile absent from the
/// hint table — that model genuinely has no Fusion-known cost class and
/// must stay at the `Medium` default (unpriced-under-cap rejection is
/// the CORRECT behavior there, not a bug this finding touches).
#[test]
fn unhinted_model_on_a_per_token_profile_keeps_the_default_cost_class() {
    let unhinted = llm_runtime::ModelProfile {
        display_model: "some-new-model".to_string(),
        request_model: "some-new-model".to_string(),
        billing_model: "some-new-model".to_string(),
        aliases: Vec::new(),
        description: None,
        metadata: lingxi_core::host::ModelMetadata::default(),
        capabilities: llm_runtime::Capabilities::default(),
    };
    let row = desktop_fusion_catalog_row(
        "openai",
        &unhinted,
        Some(lingxi_core::host::ModelBillingMode::PerToken),
        &llm_runtime::ProtocolFamily::OpenAiResponses,
    );
    assert_eq!(
        row.hints.cost_class,
        lingxi_core::host::FusionCostClass::Medium
    );

    let unsourced = desktop_fusion_catalog_row(
        "openai",
        &unhinted,
        None,
        &llm_runtime::ProtocolFamily::OpenAiResponses,
    );
    assert_eq!(
        unsourced.hints.cost_class,
        lingxi_core::host::FusionCostClass::Medium,
        "an absent billing source must not manufacture subscription pricing"
    );

    let explicitly_unknown = desktop_fusion_catalog_row(
        "openai",
        &unhinted,
        Some(lingxi_core::host::ModelBillingMode::Unknown),
        &llm_runtime::ProtocolFamily::OpenAiResponses,
    );
    assert_eq!(
        explicitly_unknown.hints.cost_class,
        lingxi_core::host::FusionCostClass::Medium,
        "an explicit Unknown mode also must not claim subscription pricing"
    );
}

/// Gemini rows retain structured output now that the SDK codec encodes it.
#[test]
fn a_gemini_row_is_marked_structured_output_capable() {
    let providers = llm_runtime::builtin_presets().providers;
    let gemini = providers
        .iter()
        .find(|p| p.profile_name == "gemini")
        .expect("gemini preset must exist in the builtin catalog");
    assert_eq!(
        gemini.protocol,
        llm_runtime::ProtocolFamily::GeminiGenerateContent,
        "sanity: the gemini preset must still be on the Gemini codec"
    );
    let pro = gemini
        .models
        .iter()
        .find(|m| m.request_model == "gemini-3.1-pro-preview")
        .expect("gemini-3.1-pro-preview present in the vendored gemini slice");
    assert!(
        pro.capabilities.structured_output,
        "the Gemini model must advertise structured output"
    );
    let row = desktop_fusion_catalog_row(
        &gemini.profile_name,
        pro,
        gemini.pricing.billing_mode,
        &gemini.protocol,
    );
    assert!(
        row.structured_output,
        "the SDK Gemini codec encodes structured output for a capable model"
    );
}

/// The class guard for finding [3]: NO judge-eligible row anywhere in the
/// real builtin catalog may claim `structured_output` while sitting on a
/// protocol family whose codec cannot encode `response_format`. Pins the
/// property for every preset at once instead of just the `gemini` one the
/// finding named — a future preset on `VertexGemini` (or a new
/// non-encoding codec) fails here rather than in production after spend.
#[test]
fn no_judge_eligible_row_claims_structured_output_on_a_non_encoding_codec() {
    let providers = llm_runtime::builtin_presets().providers;
    let mut offenders: Vec<String> = Vec::new();
    let mut checked = 0usize;
    for provider in &providers {
        for model in &provider.models {
            checked += 1;
            let row = desktop_fusion_catalog_row(
                &provider.profile_name,
                model,
                provider.pricing.billing_mode,
                &provider.protocol,
            );
            if row.hints.judge_eligible
                && row.structured_output
                && !provider.protocol.encodes_response_format()
            {
                offenders.push(format!(
                    "{}/{} ({:?})",
                    row.profile, row.model, provider.protocol
                ));
            }
        }
    }
    assert!(
        checked > 50,
        "coverage check: expected the real builtin catalog, only saw {checked} rows"
    );
    assert!(
        offenders.is_empty(),
        "these rows would be elected Fusion analyst and then hard-fail at encode time: {offenders:?}"
    );
}

/// Codec support must preserve capable models and leave incapable models false.
#[test]
fn supported_protocols_preserve_the_model_capability_bit() {
    let mut caps = llm_runtime::Capabilities::default();
    caps.structured_output = true;
    let capable = llm_runtime::ModelProfile {
        display_model: "m".to_string(),
        request_model: "m".to_string(),
        billing_model: "m".to_string(),
        aliases: Vec::new(),
        description: None,
        metadata: lingxi_core::host::ModelMetadata::default(),
        capabilities: caps,
    };
    let mut incapable = capable.clone();
    incapable.capabilities.structured_output = false;

    for family in [
        llm_runtime::ProtocolFamily::GeminiGenerateContent,
        llm_runtime::ProtocolFamily::VertexGemini,
        llm_runtime::ProtocolFamily::AnthropicMessages,
        llm_runtime::ProtocolFamily::OpenAiResponses,
        llm_runtime::ProtocolFamily::OpenAiChat,
        llm_runtime::ProtocolFamily::VertexClaude,
        llm_runtime::ProtocolFamily::BedrockClaude,
        llm_runtime::ProtocolFamily::FoundryClaude,
        llm_runtime::ProtocolFamily::AzureOpenAi,
    ] {
        assert!(
            family.encodes_response_format(),
            "{family:?} encodes response_format (directly or via the codec it delegates to)"
        );
        assert!(
            desktop_fusion_catalog_row(
                "p",
                &capable,
                Some(lingxi_core::host::ModelBillingMode::PerToken),
                &family,
            )
            .structured_output,
            "{family:?} must not narrow a genuinely capable model"
        );
        assert!(
            !desktop_fusion_catalog_row(
                "p",
                &incapable,
                Some(lingxi_core::host::ModelBillingMode::PerToken),
                &family,
            )
            .structured_output,
            "{family:?} must not manufacture a capability the model lacks"
        );
    }
}
