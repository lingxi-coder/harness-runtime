use super::{anthropic_models, resolve_default_model_ref};

fn listings() -> Vec<lingxi_core::host::ModelListing> {
    let assembled = provider_config::assemble(provider_config::AssembleInputs {
        anthropic_api_base: "https://api.anthropic.com".to_string(),
        anthropic_models: anthropic_models("claude-sonnet-5"),
        anthropic_has_api_key: false,
        anthropic_has_oauth: false,
        user_providers: std::collections::BTreeMap::new(),
        routing: None,
    });
    // The SAME mapping production feeds `resolve_default_model_ref`, so a
    // change to the listing shape cannot leave these tests validating a
    // stale one.
    super::model_listings(&assembled.client_config.providers)
}

#[test]
fn routable_refs_are_preserved() {
    let listings = listings();
    assert_eq!(
        resolve_default_model_ref("anthropic/claude-sonnet-5", &listings),
        ("claude-sonnet-5".to_string(), Some("anthropic".to_string()))
    );
    assert_eq!(
        resolve_default_model_ref("deepseek/deepseek-flash", &listings),
        ("deepseek-flash".to_string(), Some("deepseek".to_string()))
    );
    // A bare id that is served by more than one assembled profile remains
    // unscoped. The built-in catalog includes the same Claude ids for
    // GitHub Copilot, so silently choosing Anthropic here would make the
    // current row disagree with the actual route. Fresh mobile defaults
    // are provider-qualified (see `MobileEngineConfig::default`), while
    // this assertion protects ambiguous persisted bare ids.
    assert_eq!(
        resolve_default_model_ref("claude-sonnet-5", &listings),
        ("claude-sonnet-5".to_string(), None)
    );
}

/// A bare id served by MORE THAN ONE profile stays unscoped, so the registry
/// reports the ambiguity rather than this function silently picking one.
#[test]
fn a_bare_id_served_by_two_profiles_stays_unscoped() {
    let listing = |provider: &str| lingxi_core::host::ModelListing {
        connection: Default::default(),
        display_model: "claude-fable-5-1".to_string(),
        request_model: "claude-fable-5-1".to_string(),
        provider_id: provider.to_string(),
        provider_label: provider.to_string(),
        description: None,
        metadata: Default::default(),
        capabilities: Default::default(),
        reasoning: Default::default(),
        supports_reasoning: true,
        fusion_analyst_capable: false,
    };
    let listings = vec![listing("anthropic"), listing("github-copilot")];
    assert_eq!(
        resolve_default_model_ref("claude-fable-5-1", &listings),
        ("claude-fable-5-1".to_string(), None)
    );
}

/// Regression: a client that re-qualified an already-qualified reference
/// used to boot the session onto the whole unroutable string.
#[test]
fn double_qualified_ref_falls_back_to_the_anthropic_boot_default() {
    let listings = listings();
    let (model, profile) =
        resolve_default_model_ref("anthropic/deepseek/deepseek-flash", &listings);
    assert_eq!(model, "claude-sonnet-5");
    // The PROFILE must come back too: `curated_model_refs` emits
    // provider-qualified rows, so a bare `current` matches none of them and
    // the client's picker renders with nothing selected.
    assert_eq!(profile.as_deref(), Some("anthropic"));
    assert!(lingxi_core::host::is_curated_model("anthropic", &model));
}

#[test]
fn unknown_qualified_ref_falls_back() {
    let listings = listings();
    assert_eq!(
        resolve_default_model_ref("nosuchprovider/nosuchmodel", &listings),
        ("claude-sonnet-5".to_string(), Some("anthropic".to_string()))
    );
}

/// Regression: the self-heal used to return a hardcoded `claude-sonnet-5`
/// without checking it was REGISTERED. `apply_mobile_profile_allowlist` is
/// fail-closed and can strip the Anthropic profile, so that swapped one
/// unroutable ref for another while the warn log claimed a repair.
#[test]
fn fallback_is_taken_from_the_listings_when_anthropic_is_not_registered() {
    let listings = vec![lingxi_core::host::ModelListing {
        connection: Default::default(),
        display_model: "deepseek-flash".to_string(),
        request_model: "deepseek-flash".to_string(),
        provider_id: "deepseek".to_string(),
        provider_label: "deepseek".to_string(),
        description: None,
        metadata: Default::default(),
        capabilities: Default::default(),
        reasoning: Default::default(),
        supports_reasoning: false,
        fusion_analyst_capable: false,
    }];
    assert_eq!(
        resolve_default_model_ref("anthropic/claude-sonnet-5", &listings),
        ("deepseek-flash".to_string(), Some("deepseek".to_string()))
    );
}

/// No catalog to validate against ⇒ leave the caller's value alone.
#[test]
fn empty_listings_preserve_the_configured_ref() {
    assert_eq!(
        resolve_default_model_ref("anthropic/deepseek/deepseek-flash", &[]),
        ("anthropic/deepseek/deepseek-flash".to_string(), None)
    );
}
