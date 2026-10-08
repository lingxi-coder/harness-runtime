//! Group-aware routing: several CONNECTIONS to one provider.
//!
//! A provider that publishes both a domestic and an international host, or that
//! accepts several API keys, is one group with several connections. Every
//! connection serves the same model ids, so resolving a bare id inside one group
//! must pick a connection rather than refuse the request as ambiguous — and it
//! must hand back the remaining connections as the failover order.

use llm_runtime::{
    AuthStrategy, Capabilities, ClientConfig, ConnectionSpec, CredentialConfig, ModelProfile,
    ModelRegistry, PricingConfig, ProtocolFamily, ProviderId, ProviderProfile,
};

fn model(id: &str) -> ModelProfile {
    ModelProfile {
        display_model: id.to_string(),
        request_model: id.to_string(),
        billing_model: id.to_string(),
        aliases: Vec::new(),
        description: None,
        metadata: Default::default(),
        capabilities: Capabilities {
            streaming: true,
            tools: true,
            vision: false,
            documents: false,
            reasoning: false,
            structured_output: true,
        },
    }
}

/// One connection of the `deepseek` group.
fn connection(conn_id: &str, base_url: &str, order: u32, hidden: bool) -> ProviderProfile {
    ProviderProfile {
        wire_profile: None,
        regions: lingxi_llm_client::protocol::Region::all(),
        provider_id: ProviderId::OpenAICompatible {
            name: "deepseek".to_string(),
        },
        profile_name: format!("deepseek:{conn_id}"),
        base_url: base_url.to_string(),
        protocol: ProtocolFamily::OpenAiChat,
        auth: AuthStrategy::ApiKey,
        credential: CredentialConfig::Static {
            id: format!("deepseek-{conn_id}"),
        },
        models: vec![model("deepseek-flash")],
        pricing: PricingConfig::default(),
        signing: None,
        azure: None,
        supports_websockets: false,
        supports_websocket_compression: false,
        websocket_connect_timeout_ms: None,
        vision_delegate: None,
        connection: ConnectionSpec {
            group: Some("deepseek".to_string()),
            connection_id: Some(conn_id.to_string()),
            order,
            hidden,
            failover: llm_runtime::FailoverTriggers::DEFAULT,
        },
    }
}

fn two_connection_group() -> ClientConfig {
    ClientConfig {
        providers: vec![
            connection("intl", "https://api.deepseek.com", 0, false),
            connection("cn", "https://api.deepseek.cn/v1", 1, false),
        ],
    }
}

/// The defect this whole change exists to fix: two connections to ONE provider
/// both serve `deepseek-flash`, and today `resolve` refuses the request as
/// "ambiguous across profiles" instead of picking the first connection.
#[test]
fn bare_id_inside_one_group_resolves_to_the_first_connection() {
    let registry = ModelRegistry::from_config(two_connection_group()).expect("registry");

    let route = registry
        .resolve("deepseek-flash")
        .expect("a bare id served by one group must resolve, not error as ambiguous");

    assert_eq!(route.profile_name, "deepseek:intl");
    assert_eq!(route.request_model, "deepseek-flash");
}

/// Resolution must also hand back where to go NEXT, in configured order — that
/// list is what the drive loop fails over along.
#[test]
fn resolution_returns_the_remaining_connections_as_the_failover_chain() {
    let registry = ModelRegistry::from_config(two_connection_group()).expect("registry");

    let route = registry.resolve("deepseek-flash").expect("resolve");

    let hops: Vec<&str> = route
        .connection_chain
        .iter()
        .map(|hop| hop.profile_name.as_str())
        .collect();
    assert_eq!(
        hops,
        vec!["deepseek:cn"],
        "the chain must carry the connections not yet tried, in `order`"
    );
}

/// A group-qualified ref is what pickers show, so it must route the same way.
#[test]
fn group_qualified_ref_resolves_through_the_group() {
    let registry = ModelRegistry::from_config(two_connection_group()).expect("registry");

    let route = registry
        .resolve("deepseek/deepseek-flash")
        .expect("resolve");

    assert_eq!(route.profile_name, "deepseek:intl");
}

/// Naming one connection selects where to START; it does not disable the rest
/// of the group.
///
/// This originally asserted the opposite — that a connection-qualified ref
/// pinned one endpoint with no chain — which made the whole feature dead on the
/// real path: `ModelListing.provider_id` IS the connection profile name, so the
/// picker hands back `deepseek:cn`, `switch_model` stores it, and every live
/// request arrives scoped to one connection. The chain was therefore empty
/// exactly when failover was needed, and no test noticed because they all
/// resolved unscoped.
#[test]
fn scoping_to_one_connection_still_offers_the_rest_of_the_group() {
    let registry = ModelRegistry::from_config(two_connection_group()).expect("registry");

    let route = registry
        .resolve("deepseek:cn/deepseek-flash")
        .expect("resolve");

    assert_eq!(route.profile_name, "deepseek:cn");
    assert_eq!(
        route
            .connection_chain
            .iter()
            .map(|hop| hop.profile_name.as_str())
            .collect::<Vec<_>>(),
        vec!["deepseek:intl"],
        "the sibling connection must remain reachable"
    );
}

/// The exact shape the session threads through after the user picks a model.
#[test]
fn a_session_scoped_to_a_connection_can_still_fail_over() {
    let registry = ModelRegistry::from_config(two_connection_group()).expect("registry");

    // `ApiService` passes `req.profile` straight to `resolve_in`.
    let route = registry
        .resolve_in("deepseek-flash", Some("deepseek:intl"))
        .expect("resolve");

    assert_eq!(route.profile_name, "deepseek:intl");
    assert_eq!(
        route.connection_chain.len(),
        1,
        "failover must be reachable"
    );
    assert_eq!(route.connection_chain[0].profile_name, "deepseek:cn");
}

/// A hop must send the SAME wire model: `advance_connection` re-points only the
/// endpoint and credential, so a hop carrying a different model would answer as
/// something the caller never asked for.
#[test]
fn a_duplicate_alias_within_one_profile_is_ambiguous_not_a_hop() {
    let mut cfg = two_connection_group();
    // One profile serving two models that both answer to "deepseek-flash".
    let mut twin = cfg.providers[0].models[0].clone();
    twin.display_model = "deepseek-flash-latest".to_string();
    twin.request_model = "deepseek-flash-latest".to_string();
    twin.aliases = vec!["deepseek-flash".to_string()];
    cfg.providers[0].models.push(twin);

    let registry = ModelRegistry::from_config(cfg).expect("registry");
    let err = registry
        .resolve_in("deepseek-flash", Some("deepseek:intl"))
        .expect_err("two models of one endpoint answering to one string is ambiguous");
    let message = format!("{err:?}");
    assert!(
        message.contains("more than one model"),
        "expected a same-profile ambiguity error, got: {message}"
    );
}

/// The same id served by two DIFFERENT providers stays ambiguous — that is a
/// real user error and widening the group rule must not swallow it.
#[test]
fn bare_id_across_two_groups_is_still_ambiguous() {
    let mut cfg = two_connection_group();
    let mut other = connection("main", "https://api.example.com", 0, false);
    other.provider_id = ProviderId::OpenAICompatible {
        name: "other".to_string(),
    };
    other.profile_name = "other".to_string();
    other.connection = ConnectionSpec::default();
    cfg.providers.push(other);

    let registry = ModelRegistry::from_config(cfg).expect("registry");

    let err = registry
        .resolve("deepseek-flash")
        .expect_err("an id served by two different providers must still be ambiguous");
    let message = format!("{err:?}");
    assert!(
        message.contains("ambiguous"),
        "expected an ambiguity error, got: {message}"
    );
}

/// Extra key slots of one connection are failover targets, never picker rows.
#[test]
fn hidden_connections_are_not_listed() {
    let cfg = ClientConfig {
        providers: vec![
            connection("cn#0", "https://api.deepseek.cn/v1", 0, false),
            connection("cn#1", "https://api.deepseek.cn/v1", 1, true),
        ],
    };
    let registry = ModelRegistry::from_config(cfg).expect("registry");

    let listed: Vec<String> = registry
        .available_models()
        .into_iter()
        .map(|listing| listing.profile_name)
        .collect();

    assert_eq!(listed, vec!["deepseek:cn#0".to_string()]);
}

// ── Desugaring `settings.providers` into connections ───────────────────────

use llm_runtime::{parse_provider_profiles_lenient, ProviderCredentialMode, ProviderParseOptions};
use std::collections::BTreeMap;

fn parse(json: serde_json::Value) -> (Vec<ProviderProfile>, Vec<String>) {
    let providers: BTreeMap<String, serde_json::Value> =
        serde_json::from_value(json).expect("fixture");
    let (parsed, warnings) = parse_provider_profiles_lenient(
        &providers,
        ProviderParseOptions {
            credential_mode: ProviderCredentialMode::DeferredStatic,
            models_required: false,
        },
    );
    (parsed.into_iter().map(|p| p.profile).collect(), warnings)
}

/// The historical flat form must survive untouched — same profile name, same
/// provider id, and a default identity so nothing downstream sees a change.
#[test]
fn a_provider_without_connections_is_unchanged() {
    let (profiles, warnings) = parse(serde_json::json!({
        "groq": {
            "type": "openai",
            "baseUrl": "https://api.groq.com/openai/v1",
            "apiKeyEnv": "GROQ_API_KEY",
            "models": [{ "id": "llama-3.3-70b" }]
        }
    }));

    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(profiles.len(), 1);
    assert_eq!(profiles[0].profile_name, "groq");
    assert_eq!(profiles[0].group(), "groq");
    assert_eq!(profiles[0].connection_id(), "default");
    assert!(profiles[0].connection.is_default());
    assert_eq!(
        profiles[0].provider_id,
        ProviderId::OpenAICompatible {
            name: "groq".to_string()
        }
    );
}

/// The headline case: one provider, two endpoints, each with its own URL and
/// key — and BOTH billing to the same provider identity.
#[test]
fn connections_expand_to_sibling_profiles_sharing_one_provider_id() {
    let (profiles, warnings) = parse(serde_json::json!({
        "deepseek": {
            "type": "openai",
            "models": [{ "id": "deepseek-flash" }],
            "connections": [
                { "id": "intl", "baseUrl": "https://api.deepseek.com", "apiKeyEnv": "DEEPSEEK_API_KEY" },
                { "id": "cn", "baseUrl": "https://api.deepseek.cn/v1", "apiKeyEnv": "DEEPSEEK_CN_API_KEY" }
            ]
        }
    }));

    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(profiles.len(), 2);

    assert_eq!(profiles[0].profile_name, "deepseek:intl");
    assert_eq!(profiles[0].base_url, "https://api.deepseek.com");
    assert_eq!(profiles[0].connection.order, 0);
    assert_eq!(profiles[1].profile_name, "deepseek:cn");
    assert_eq!(profiles[1].base_url, "https://api.deepseek.cn/v1");
    assert_eq!(profiles[1].connection.order, 1);

    for profile in &profiles {
        assert_eq!(profile.group(), "deepseek");
        assert_eq!(
            profile.provider_id,
            ProviderId::OpenAICompatible {
                name: "deepseek".to_string()
            },
            "every connection must bill to the PROVIDER, not to itself"
        );
    }
}

/// Provider-level fields are defaults; a connection may narrow them. This is the
/// real Zhipu shape — the two endpoints do not even speak the same wire.
#[test]
fn a_connection_overrides_provider_level_defaults() {
    let (profiles, warnings) = parse(serde_json::json!({
        "zhipu": {
            "type": "openai",
            "baseUrl": "https://api.z.ai/api/paas/v4",
            "apiKeyEnv": "ZAI_API_KEY",
            "models": [{ "id": "glm-4.6" }, { "id": "glm-4.5-air" }],
            "connections": [
                { "id": "api" },
                { "id": "coding",
                  "type": "anthropic",
                  "baseUrl": "https://open.bigmodel.cn/api/anthropic",
                  "models": [{ "id": "glm-4.6" }] }
            ]
        }
    }));

    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(profiles.len(), 2);

    let api = &profiles[0];
    assert_eq!(api.protocol, ProtocolFamily::OpenAiChat);
    assert_eq!(
        api.models.len(),
        2,
        "inherits the provider-level model list"
    );

    let coding = &profiles[1];
    assert_eq!(
        coding.protocol,
        ProtocolFamily::AnthropicMessages,
        "a connection may speak a different wire protocol"
    );
    assert_eq!(coding.base_url, "https://open.bigmodel.cn/api/anthropic");
    assert_eq!(
        coding.models.len(),
        1,
        "a redeclared model list REPLACES the provider's, it does not extend it"
    );
}

/// A connection with no `credentialIds` must use the PROVIDER's stored key.
///
/// Every editor saves the key under the provider name, so deriving the id from
/// the connection's profile name (`deepseek:cn`) meant a multi-connection
/// provider configured through the UI authenticated against a credential that
/// was never written. It also breaks the ordinary case outright: one key, two
/// regions.
#[test]
fn a_connection_without_its_own_keys_uses_the_providers_credential() {
    let (profiles, warnings) = parse(serde_json::json!({
        "deepseek": {
            "type": "openai",
            "models": [{ "id": "deepseek-flash" }],
            "connections": [
                { "id": "intl", "baseUrl": "https://api.deepseek.com" },
                { "id": "cn", "baseUrl": "https://api.deepseek.cn/v1" }
            ]
        }
    }));
    assert!(warnings.is_empty(), "{warnings:?}");
    for profile in &profiles {
        assert_eq!(
            profile.credential,
            CredentialConfig::Static {
                id: "deepseek".to_string()
            },
            "{} must read the provider's stored key",
            profile.profile_name
        );
    }
}

/// A connection that DOES name its own credentials keeps them.
#[test]
fn a_connection_with_credential_ids_overrides_the_inherited_one() {
    let (profiles, _) = parse(serde_json::json!({
        "deepseek": {
            "type": "openai",
            "models": [{ "id": "deepseek-flash" }],
            "connections": [
                { "id": "intl", "baseUrl": "https://api.deepseek.com" },
                { "id": "cn", "baseUrl": "https://api.deepseek.cn/v1", "credentialIds": ["ds-cn"] }
            ]
        }
    }));
    let by_name: std::collections::BTreeMap<_, _> = profiles
        .iter()
        .map(|p| (p.profile_name.as_str(), p))
        .collect();
    assert_eq!(
        by_name["deepseek:intl"].credential,
        CredentialConfig::Static {
            id: "deepseek".to_string()
        }
    );
    assert_eq!(
        by_name["deepseek:cn#0"].credential,
        CredentialConfig::Static {
            id: "ds-cn".to_string()
        }
    );
}

/// Several stored credentials on one connection become hidden sibling slots
/// that differ only in credential — the mechanism that makes key rotation free.
///
/// The field is `credentialIds`, not `apiKeys`: these name secrets already in
/// the keychain, and a settings key spelled `apiKeys` invites pasting the real
/// secret into `settings.json`.
#[test]
fn credential_ids_expand_to_hidden_slots_differing_only_in_credential() {
    let (profiles, warnings) = parse(serde_json::json!({
        "deepseek": {
            "type": "openai",
            "models": [{ "id": "deepseek-flash" }],
            "connections": [
                { "id": "cn", "baseUrl": "https://api.deepseek.cn/v1",
                  "credentialIds": ["ds-cn-a", "ds-cn-b"] }
            ]
        }
    }));

    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(profiles.len(), 2);

    assert_eq!(profiles[0].profile_name, "deepseek:cn#0");
    assert_eq!(profiles[1].profile_name, "deepseek:cn#1");
    assert!(!profiles[0].connection.hidden);
    assert!(
        profiles[1].connection.hidden,
        "spare key slots must never be offered as a pickable model"
    );

    assert_eq!(
        profiles[0].credential,
        CredentialConfig::Static {
            id: "ds-cn-a".to_string()
        }
    );
    assert_eq!(
        profiles[1].credential,
        CredentialConfig::Static {
            id: "ds-cn-b".to_string()
        }
    );
    assert_eq!(
        profiles[0].base_url, profiles[1].base_url,
        "key slots are the SAME connection: everything but the credential matches"
    );
    assert_eq!(profiles[0].connection_id(), profiles[1].connection_id());
}

/// A connection id becomes part of a qualified model reference, so the
/// separators must be rejected rather than silently producing an unroutable ref.
#[test]
fn connection_ids_reject_reference_separators() {
    for bad in ["a/b", "a:b", "a#b"] {
        let (_profiles, warnings) = parse(serde_json::json!({
            "p": {
                "type": "openai",
                "baseUrl": "https://x",
                "models": [{ "id": "m" }],
                "connections": [{ "id": bad, "baseUrl": "https://x" }]
            }
        }));
        assert_eq!(warnings.len(), 1, "id {bad:?} should be rejected");
        assert!(
            warnings[0].contains("must not contain"),
            "id {bad:?}: {warnings:?}"
        );
    }
}

/// Two connections with the same id would produce two profiles with the same
/// name — silently unroutable. Reject it.
#[test]
fn duplicate_connection_ids_are_rejected() {
    let (profiles, warnings) = parse(serde_json::json!({
        "p": {
            "type": "openai",
            "baseUrl": "https://x",
            "models": [{ "id": "m" }],
            "connections": [{ "id": "a" }, { "id": "a" }]
        }
    }));
    assert!(profiles.is_empty());
    assert_eq!(warnings.len(), 1);
    assert!(
        warnings[0].contains("duplicate connection id"),
        "{warnings:?}"
    );
}

/// A provider may publish one model on both a subscription endpoint and a
/// metered one. Failing a rate-limited subscription request over onto the
/// metered endpoint would silently start charging for what the plan covers, so
/// such a hop must never appear in the chain.
#[test]
fn failover_never_crosses_billing_modes() {
    use llm_runtime::ModelBillingMode;

    let mut subscription = connection("plan", "https://plan.example.com", 0, false);
    subscription.pricing.billing_mode = Some(ModelBillingMode::Subscription);
    let mut metered = connection("api", "https://api.example.com", 1, false);
    metered.pricing.billing_mode = Some(ModelBillingMode::PerToken);

    let registry = ModelRegistry::from_config(ClientConfig {
        providers: vec![subscription, metered],
    })
    .expect("registry");

    let route = registry.resolve("deepseek-flash").expect("resolve");

    assert_eq!(route.profile_name, "deepseek:plan");
    assert!(
        route.connection_chain.is_empty(),
        "a subscription request must not fail over onto a metered endpoint; chain was {:?}",
        route.connection_chain
    );
}

/// Same billing mode still chains, so the guard above narrows rather than
/// disables failover.
#[test]
fn failover_still_chains_within_one_billing_mode() {
    let registry = ModelRegistry::from_config(two_connection_group()).expect("registry");
    let route = registry.resolve("deepseek-flash").expect("resolve");
    assert_eq!(route.connection_chain.len(), 1);
}

// ── Built-in presets that are one vendor behind two endpoints ──────────────

/// Zhipu ships as two presets — `glm-coding` (subscription, Anthropic wire, CN
/// host) and `zai` (metered, OpenAI wire) — that share eight model ids. They are
/// now one group, so the vendor reads as one provider and its spend rolls up
/// once, while both historical references keep working.
#[test]
fn zhipu_presets_are_one_group_with_both_names_still_routable() {
    let catalog = llm_runtime::builtin_presets();
    let by_name: std::collections::BTreeMap<&str, &ProviderProfile> = catalog
        .providers
        .iter()
        .map(|p| (p.profile_name.as_str(), p))
        .collect();

    let coding = by_name["glm-coding"];
    let api = by_name["zai"];

    assert_eq!(coding.group(), "zhipu");
    assert_eq!(api.group(), "zhipu");
    assert_eq!(coding.connection_id(), "coding");
    assert_eq!(api.connection_id(), "api");
    assert_eq!(
        coding.provider_id, api.provider_id,
        "both connections must bill to the same vendor identity"
    );
    assert_eq!(
        coding.profile_name, "glm-coding",
        "profile names are keychain ids and saved model refs — they must not move"
    );
}

/// Kimi's two presets are likewise one vendor. Their model ids are disjoint, so
/// this is purely grouping: no bare id becomes ambiguous.
#[test]
fn kimi_presets_are_one_group_with_disjoint_models() {
    let catalog = llm_runtime::builtin_presets();
    let open: Vec<&ProviderProfile> = catalog
        .providers
        .iter()
        .filter(|p| p.group() == "kimi")
        .collect();
    assert_eq!(open.len(), 2, "kimi + kimi-code");

    let ids: Vec<&str> = open
        .iter()
        .flat_map(|p| p.models.iter().map(|m| m.request_model.as_str()))
        .collect();
    let mut unique = ids.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(
        ids.len(),
        unique.len(),
        "the two Kimi endpoints must not declare the same model id"
    );
}

/// Every other preset stays exactly what it was: its own standalone provider.
#[test]
fn ungrouped_presets_are_untouched() {
    for profile in llm_runtime::builtin_presets().providers {
        let upstream = profile.wire_profile.as_ref().expect("upstream facts");
        assert_eq!(
            serde_json::to_value(&profile.connection).unwrap(),
            serde_json::to_value(&upstream.connection).unwrap()
        );
    }
}
