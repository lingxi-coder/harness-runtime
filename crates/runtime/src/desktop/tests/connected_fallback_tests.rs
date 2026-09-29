use super::{connected_provider_fallback, RecentModelRef};
use std::collections::BTreeMap;

fn listing(provider_id: &str, request_model: &str) -> platform_api::ModelListing {
    platform_api::ModelListing {
        display_model: request_model.to_string(),
        request_model: request_model.to_string(),
        provider_id: provider_id.to_string(),
        provider_label: provider_id.to_string(),
        description: None,
        metadata: Default::default(),
        capabilities: Default::default(),
        reasoning: Default::default(),
        supports_reasoning: false,
        fusion_analyst_capable: false,
        connection: Default::default(),
    }
}

/// Catalog fixture: anthropic + a few presets + a user-defined "groq".
fn listings() -> Vec<platform_api::ModelListing> {
    vec![
        listing("anthropic", "claude-sonnet-5"),
        listing("anthropic", "claude-opus-4-8"),
        listing("openai", "gpt-5.5"),
        listing("deepseek", "deepseek-chat"),
        listing("deepseek", "deepseek-reasoner"),
        listing("zai", "glm-5.1"),
        listing("zai", "glm-5"),
        listing("github-copilot", "claude-opus-4.8"),
        listing("openrouter", "openrouter/auto"),
        listing("groq", "llama-3.3-70b-versatile"),
    ]
}

fn avail(pairs: &[(&str, bool)]) -> BTreeMap<String, bool> {
    pairs.iter().map(|(k, v)| ((*k).to_string(), *v)).collect()
}

/// `model_providers` fixture mapping bare ids to their profile.
fn providers() -> BTreeMap<String, (String, String)> {
    listings()
        .into_iter()
        .map(|l| (l.request_model, (l.provider_id.clone(), l.provider_id)))
        .collect()
}

fn recent(provider: &str, model: &str) -> RecentModelRef {
    RecentModelRef {
        provider: provider.to_string(),
        model: model.to_string(),
    }
}

/// Default's provider connected ⇒ no fallback, even with others connected.
#[test]
fn connected_default_is_kept() {
    let fb = connected_provider_fallback(
        "claude-sonnet-5",
        None,
        true,
        &providers(),
        &avail(&[("anthropic", true), ("deepseek", true)]),
        &listings(),
        &[],
    );
    assert!(fb.is_none());
}

/// Provider ABSENT from the availability map (probe blind) ⇒ conservative
/// keep — only a definitive `false` reroutes.
#[test]
fn probe_unknown_provider_is_kept() {
    let fb = connected_provider_fallback(
        "llama-3.3-70b-versatile",
        Some("groq"),
        true,
        &providers(),
        &avail(&[("anthropic", false), ("deepseek", true)]),
        &listings(),
        &[],
    );
    assert!(fb.is_none());
}

/// The headline case: fresh anthropic default, no anthropic creds, one
/// connected API-key provider ⇒ boot on that provider's default model.
#[test]
fn disconnected_anthropic_falls_to_connected_provider() {
    let fb = connected_provider_fallback(
        "claude-sonnet-5",
        None,
        true,
        &providers(),
        &avail(&[("anthropic", false), ("deepseek", true)]),
        &listings(),
        &[],
    )
    .expect("must reroute");
    assert_eq!(fb.model, "deepseek-chat");
    assert_eq!(fb.profile, "deepseek");
}

/// A stale profile-qualified default (e.g. persisted copilot pick) with
/// anthropic connected ⇒ anthropic wins (first in the fallback order) and
/// keeps the legacy bare/no-profile shape.
#[test]
fn disconnected_qualified_default_prefers_anthropic() {
    let fb = connected_provider_fallback(
        "claude-opus-4.8",
        Some("github-copilot"),
        true,
        &providers(),
        &avail(&[
            ("anthropic", true),
            ("deepseek", true),
            ("github-copilot", false),
        ]),
        &listings(),
        &[],
    )
    .expect("must reroute");
    assert_eq!(fb.model, "claude-sonnet-5");
    assert_eq!(
        fb.profile, "anthropic",
        "profile-scoped so shared wire ids resolve unambiguously"
    );
}

/// The most recent `/model` pick on a CONNECTED provider wins over the
/// static provider order.
#[test]
fn recents_win_over_provider_order() {
    let fb = connected_provider_fallback(
        "claude-opus-4.8",
        Some("github-copilot"),
        true,
        &providers(),
        &avail(&[
            ("anthropic", false),
            ("deepseek", true),
            ("zai", true),
            ("github-copilot", false),
        ]),
        &listings(),
        &[recent("zai", "glm-5")],
    )
    .expect("must reroute");
    assert_eq!(fb.model, "glm-5");
    assert_eq!(fb.profile, "zai");
}

/// Recents on a DISCONNECTED provider are skipped.
#[test]
fn recents_on_disconnected_provider_skipped() {
    let fb = connected_provider_fallback(
        "claude-sonnet-5",
        None,
        true,
        &providers(),
        &avail(&[("anthropic", false), ("openai", false), ("deepseek", true)]),
        &listings(),
        &[recent("openai", "gpt-5.5")],
    )
    .expect("must reroute");
    assert_eq!(fb.model, "deepseek-chat");
}

/// Recents whose model vanished from the catalog are skipped.
#[test]
fn recents_model_missing_from_listings_skipped() {
    let fb = connected_provider_fallback(
        "claude-sonnet-5",
        None,
        true,
        &providers(),
        &avail(&[("anthropic", false), ("deepseek", true)]),
        &listings(),
        &[recent("deepseek", "deepseek-legacy")],
    )
    .expect("must reroute");
    assert_eq!(fb.model, "deepseek-chat");
}

/// Nothing connected ⇒ keep the configured default (onboarding handles it).
#[test]
fn nothing_connected_keeps_default() {
    let fb = connected_provider_fallback(
        "claude-sonnet-5",
        None,
        true,
        &providers(),
        &avail(&[("anthropic", false), ("deepseek", false)]),
        &listings(),
        &[],
    );
    assert!(fb.is_none());
}

/// A connected user-defined provider (no curated default) falls back to its
/// first listed model.
#[test]
fn user_provider_falls_to_first_listing() {
    let fb = connected_provider_fallback(
        "claude-sonnet-5",
        None,
        true,
        &providers(),
        &avail(&[("anthropic", false), ("groq", true)]),
        &listings(),
        &[],
    )
    .expect("must reroute");
    assert_eq!(fb.model, "llama-3.3-70b-versatile");
    assert_eq!(fb.profile, "groq");
}

/// OpenRouter-only install boots on the `auto` meta-router.
#[test]
fn openrouter_only_falls_to_auto_router() {
    let fb = connected_provider_fallback(
        "claude-sonnet-5",
        None,
        true,
        &providers(),
        &avail(&[("anthropic", false), ("openrouter", true)]),
        &listings(),
        &[],
    )
    .expect("must reroute");
    assert_eq!(fb.model, "openrouter/auto");
    assert_eq!(fb.profile, "openrouter");
}

/// A BARE default id resolves its provider through `model_providers`
/// (same lookup the firstParty gate uses).
#[test]
fn bare_id_provider_resolved_via_model_providers() {
    let fb = connected_provider_fallback(
        "gpt-5.5",
        None,
        true,
        &providers(),
        &avail(&[("openai", false), ("deepseek", true)]),
        &listings(),
        &[],
    )
    .expect("must reroute");
    assert_eq!(fb.model, "deepseek-chat");
}

/// Gateway install (custom base URL / `ANTHROPIC_AUTH_TOKEN`): the anthropic
/// probe is NOT definitive, so an anthropic-routed default is kept even
/// with other providers connected — Claude works through the gateway.
#[test]
fn gateway_install_keeps_anthropic_default() {
    let fb = connected_provider_fallback(
        "claude-sonnet-5",
        None,
        false,
        &providers(),
        &avail(&[("anthropic", false), ("deepseek", true)]),
        &listings(),
        &[],
    );
    assert!(fb.is_none());
}

/// The gateway flag only shields ANTHROPIC-routed defaults: a default on a
/// disconnected non-anthropic provider still reroutes (the gateway serves
/// Claude, not that provider).
#[test]
fn gateway_flag_does_not_shield_non_anthropic_defaults() {
    let fb = connected_provider_fallback(
        "claude-opus-4.8",
        Some("github-copilot"),
        false,
        &providers(),
        &avail(&[
            ("anthropic", false),
            ("deepseek", true),
            ("github-copilot", false),
        ]),
        &listings(),
        &[],
    )
    .expect("must reroute");
    assert_eq!(fb.model, "deepseek-chat");
    assert_eq!(fb.profile, "deepseek");
}

/// A connected provider whose curated default is missing from the live
/// catalog is skipped by the order pass; the first-listing pass still
/// serves it.
#[test]
fn curated_default_missing_from_catalog_falls_to_first_listing() {
    let listings: Vec<platform_api::ModelListing> = vec![
        listing("anthropic", "claude-sonnet-5"),
        listing("deepseek", "deepseek-reasoner"), // no deepseek-chat
    ];
    let fb = connected_provider_fallback(
        "claude-sonnet-5",
        None,
        true,
        &providers(),
        &avail(&[("anthropic", false), ("deepseek", true)]),
        &listings,
        &[],
    )
    .expect("must reroute");
    assert_eq!(fb.model, "deepseek-reasoner");
    assert_eq!(fb.profile, "deepseek");
}
