use super::anthropic_models;

fn ids(default_model: &str) -> Vec<String> {
    anthropic_models(default_model)
        .into_iter()
        .map(|m| m.request_model)
        .collect()
}

/// Regression: the mobile picker's ANTHROPIC section rendered only Sonnet
/// 4.6 + Haiku 4.5 because the other curated ids had no route here, so
/// `curated_model_refs` filtered them out of `ModelList`.
#[test]
fn registry_routes_every_curated_anthropic_model() {
    let ids = ids("claude-sonnet-5");
    for curated in [
        "claude-sonnet-5",
        "claude-opus-5",
        "claude-fable-5-1",
        "claude-haiku-4-5",
    ] {
        assert!(
            lingxi_core::host::is_curated_model("anthropic", curated),
            "{curated} is no longer curated; update this test with the shortlist"
        );
        assert!(
            ids.iter().any(|id| id == curated),
            "missing {curated}: {ids:?}"
        );
    }
}

/// A configured default that routes to anthropic is registered BARE.
#[test]
fn qualified_anthropic_default_registers_bare_id() {
    assert!(ids("anthropic/claude-opus-4-5-20251101")
        .iter()
        .any(|id| id == "claude-opus-4-5-20251101"));
    // An unqualified custom id still routes to anthropic (legacy behavior).
    assert!(ids("my-proxy-model")
        .iter()
        .any(|id| id == "my-proxy-model"));
}

/// Regression (iOS "DeepSeek V4 Flash under ANTHROPIC"): a default qualified
/// for ANOTHER provider — including one a client double-qualified on the way
/// in — must never be registered under the Anthropic profile.
#[test]
fn foreign_qualified_default_is_never_registered_under_anthropic() {
    // (ref, the BARE id it must not leak). Asserting only "no id contains a
    // slash" left `github-copilot/claude-opus-4.8` inert — its bare form has
    // no slash, so that row passed even against the pre-fix code.
    for (foreign, leaked) in [
        ("deepseek/deepseek-flash", "deepseek-flash"),
        (
            "anthropic/deepseek/deepseek-flash",
            "deepseek/deepseek-flash",
        ),
        ("openrouter/openrouter/auto", "openrouter/auto"),
        ("github-copilot/claude-opus-4.8", "claude-opus-4.8"),
    ] {
        let ids = ids(foreign);
        assert!(
            !ids.iter().any(|id| id.contains('/')),
            "{foreign} leaked a qualified id into the anthropic registry: {ids:?}"
        );
        assert!(
            !ids.iter().any(|id| id == leaked),
            "{foreign} leaked {leaked} into the anthropic registry: {ids:?}"
        );
    }
}

/// The single routing rule both the configured default and the env-
/// configured small-fast / haiku ids go through. Tested directly rather
/// than through `ids()`, which reads process-wide env state.
///
/// Regression: the env ids were pushed RAW, so the guard could be bypassed
/// entirely through the env — and a developer with either var exported made
/// `foreign_qualified_default_is_never_registered_under_anthropic` fail for
/// an unrelated reason.
#[test]
fn anthropic_route_id_is_the_one_rule_for_every_registered_source() {
    use super::anthropic_route_id;
    assert_eq!(
        anthropic_route_id("claude-sonnet-5").as_deref(),
        Some("claude-sonnet-5")
    );
    assert_eq!(
        anthropic_route_id("anthropic/claude-haiku-4-5").as_deref(),
        Some("claude-haiku-4-5")
    );
    // An unqualified custom id still routes to anthropic (legacy behavior).
    assert_eq!(
        anthropic_route_id(" my-proxy-model ").as_deref(),
        Some("my-proxy-model")
    );
    for rejected in [
        "",
        "   ",
        "anthropic/",
        "deepseek/deepseek-flash",
        "anthropic/deepseek/deepseek-flash",
        "openrouter/openrouter/auto",
        "github-copilot/claude-opus-4.8",
    ] {
        assert_eq!(anthropic_route_id(rejected), None, "accepted {rejected:?}");
    }
}

/// An empty default (the "let the engine pick" signal) adds nothing.
#[test]
fn empty_default_adds_no_extra_route() {
    assert_eq!(ids(""), ids("claude-opus-5"));
}
