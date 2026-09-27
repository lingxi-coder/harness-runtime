//! Extracted tests for `platforms::common::llm_config`.

use super::*;
use llm_runtime::{DefaultLlmClient, LlmError, PricingConfig, ProtocolFamily, ProviderId};

/// Build a test config with `AuthStrategy::None` + `CredentialConfig::None`
/// so `prepare()` never attempts a credential lookup (no env var needed).
fn test_config(api_base: &str) -> ClientConfig {
    let mut cfg = builtin_anthropic_config(api_base, false);
    // Swap auth+credential to None so prepare() is credential-free in tests.
    for p in &mut cfg.providers {
        p.auth = AuthStrategy::None;
        p.credential = CredentialConfig::None;
    }
    cfg
}

/// Every `display_model` in the builtin table must be resolvable via
/// `available_models()`. This pins the complete 10-entry table so a
/// future edit that accidentally drops a model is caught immediately.
#[test]
fn all_table_entries_resolvable() {
    let cfg = builtin_anthropic_config("https://api.anthropic.com", false);
    let client = DefaultLlmClient::from_config(cfg).expect("config must be valid");
    let available: Vec<String> = client
        .available_models()
        .into_iter()
        .map(|m| m.display_model)
        .collect();

    let expected = [
        "claude-sonnet-4-20250514",
        "claude-sonnet-4-5-20250929",
        "claude-sonnet-4-6",
        "claude-opus-4-20250514",
        "claude-opus-4-1-20250805",
        "claude-opus-4-5-20251101",
        "claude-opus-4-6",
        // orchestrator DEFAULT_MODEL (config.rs:18) — was absent before this fix
        "claude-opus-4-7",
        "claude-haiku-4-20250307",
        "claude-haiku-4-5",
    ];
    for model_id in &expected {
        assert!(
            available.iter().any(|m| m == model_id),
            "model {model_id:?} missing from available_models(); got: {available:?}",
        );
    }
}

/// `claude-opus-4-7` (orchestrator DEFAULT_MODEL, config.rs:18) must survive
/// `prepare()` so the orchestrator's default model actually routes at runtime.
#[tokio::test]
async fn default_model_resolves() {
    let cfg = test_config("https://api.anthropic.com");
    let client = DefaultLlmClient::from_config(cfg).expect("config must be valid");

    // `prepare()` exercises registry resolution + codec encoding + auth —
    // with AuthStrategy::None it short-circuits before any network call.
    let req = llm_runtime::LlmRequest::new("claude-opus-4-7");
    let result = client.prepare(&req).await;
    assert!(
        result.is_ok(),
        "prepare(claude-opus-4-7) failed: {:?} — orchestrator DEFAULT_MODEL must be in the table",
        result.err(),
    );
}

/// An unknown model id must yield `LlmError::ModelUnavailable`, not a panic
/// or a misleading error variant.
#[tokio::test]
async fn unknown_model_yields_model_unavailable() {
    let cfg = test_config("https://api.anthropic.com");
    let client = DefaultLlmClient::from_config(cfg).expect("config must be valid");

    let req = llm_runtime::LlmRequest::new("claude-unknown-999");
    let err = client
        .prepare(&req)
        .await
        .expect_err("unknown model must not resolve");
    assert!(
        matches!(err, LlmError::ModelUnavailable),
        "expected ModelUnavailable, got: {err:?}",
    );
}

// ---- apply_settings_providers tests ------------------------------------

/// A groq-style OpenAI-compatible profile parses correctly and is appended
/// to the builtin Anthropic profile, so both profiles are available.
#[test]
fn openai_profile_appended() {
    let mut cfg = builtin_anthropic_config("https://api.anthropic.com", false);
    let providers: BTreeMap<String, serde_json::Value> = serde_json::from_str(
        r#"{
        "groq": {
            "type": "openai",
            "baseUrl": "https://api.groq.com/openai/v1",
            "apiKeyEnv": "GROQ_API_KEY",
            "models": [
                { "id": "llama-3.3-70b-versatile" }
            ]
        }
    }"#,
    )
    .unwrap();

    apply_settings_providers(&mut cfg, &providers, None).expect("must succeed");

    assert_eq!(cfg.providers.len(), 2, "builtin + groq");
    let groq = cfg
        .providers
        .iter()
        .find(|p| p.profile_name == "groq")
        .unwrap();
    assert_eq!(groq.base_url, "https://api.groq.com/openai/v1");
    assert_eq!(groq.protocol, ProtocolFamily::OpenAiChat);
    assert!(
        matches!(&groq.provider_id, ProviderId::OpenAICompatible { name } if name == "groq"),
        "provider_id must be OpenAICompatible with name=groq"
    );
    assert_eq!(
        groq.credential,
        CredentialConfig::Env {
            var: "GROQ_API_KEY".to_string()
        }
    );
    assert_eq!(groq.models.len(), 1);
    assert_eq!(groq.models[0].display_model, "llama-3.3-70b-versatile");
    // Default capabilities: streaming + tools, no vision/docs/reasoning.
    assert!(groq.models[0].capabilities.streaming);
    assert!(groq.models[0].capabilities.tools);
    assert!(!groq.models[0].capabilities.vision);
    assert!(!groq.models[0].capabilities.reasoning);
}

/// A gemini profile parses with the correct protocol family and provider id.
#[test]
fn gemini_profile_parses() {
    let mut cfg = builtin_anthropic_config("https://api.anthropic.com", false);
    let providers: BTreeMap<String, serde_json::Value> = serde_json::from_str(
        r#"{
        "my-gemini": {
            "type": "gemini",
            "baseUrl": "https://generativelanguage.googleapis.com/v1beta",
            "apiKeyEnv": "GEMINI_API_KEY",
            "models": [
                { "id": "gemini-2.0-flash" }
            ]
        }
    }"#,
    )
    .unwrap();

    apply_settings_providers(&mut cfg, &providers, None).expect("must succeed");

    let gemini = cfg
        .providers
        .iter()
        .find(|p| p.profile_name == "my-gemini")
        .unwrap();
    assert_eq!(gemini.protocol, ProtocolFamily::GeminiGenerateContent);
    assert_eq!(gemini.provider_id, ProviderId::Gemini);
    assert_eq!(
        gemini.credential,
        CredentialConfig::Env {
            var: "GEMINI_API_KEY".to_string()
        }
    );
}

/// An alias in `routing.aliases` pointing at a custom profile's model
/// gets pushed onto that model's alias list.
#[test]
fn alias_injected_into_custom_profile() {
    let mut cfg = builtin_anthropic_config("https://api.anthropic.com", false);
    let providers: BTreeMap<String, serde_json::Value> = serde_json::from_str(
        r#"{
        "groq": {
            "type": "openai",
            "baseUrl": "https://api.groq.com/openai/v1",
            "apiKeyEnv": "GROQ_API_KEY",
            "models": [{ "id": "llama-3.3-70b-versatile" }]
        }
    }"#,
    )
    .unwrap();
    let routing: serde_json::Value = serde_json::from_str(
        r#"{
        "aliases": { "llama": "groq/llama-3.3-70b-versatile" }
    }"#,
    )
    .unwrap();

    apply_settings_providers(&mut cfg, &providers, Some(&routing)).expect("must succeed");

    let groq = cfg
        .providers
        .iter()
        .find(|p| p.profile_name == "groq")
        .unwrap();
    assert!(
        groq.models[0].aliases.contains(&"llama".to_string()),
        "alias 'llama' must be injected into the model's aliases"
    );
}

/// An alias in `routing.aliases` pointing at a builtin profile's model
/// (e.g. "anthropic/claude-sonnet-4-20250514") is also wired.
#[test]
fn alias_injected_into_builtin_profile() {
    let mut cfg = builtin_anthropic_config("https://api.anthropic.com", false);
    let routing: serde_json::Value = serde_json::from_str(
        r#"{
        "aliases": { "my-sonnet": "anthropic/claude-sonnet-4-20250514" }
    }"#,
    )
    .unwrap();

    apply_settings_providers(&mut cfg, &BTreeMap::new(), Some(&routing)).expect("must succeed");

    let anthropic = cfg
        .providers
        .iter()
        .find(|p| p.profile_name == "anthropic")
        .unwrap();
    let sonnet = anthropic
        .models
        .iter()
        .find(|m| m.display_model == "claude-sonnet-4-20250514")
        .unwrap();
    assert!(
        sonnet.aliases.contains(&"my-sonnet".to_string()),
        "alias 'my-sonnet' must be injected into the builtin sonnet model's aliases"
    );
}

/// A provider entry with no `models` key is rejected with [`LlmError::InvalidRequest`].
#[test]
fn missing_models_is_error() {
    let mut cfg = builtin_anthropic_config("https://api.anthropic.com", false);
    let providers: BTreeMap<String, serde_json::Value> = serde_json::from_str(
        r#"{
        "bad-provider": {
            "type": "openai",
            "baseUrl": "https://example.com",
            "apiKeyEnv": "SOME_KEY"
        }
    }"#,
    )
    .unwrap();

    let err = apply_settings_providers(&mut cfg, &providers, None).unwrap_err();
    assert!(
        matches!(&err, LlmError::InvalidRequest { message } if message.contains("required")),
        "expected InvalidRequest about missing models, got: {err:?}"
    );
}

/// A provider entry with an empty `models` array is rejected.
#[test]
fn empty_models_is_error() {
    let mut cfg = builtin_anthropic_config("https://api.anthropic.com", false);
    let providers: BTreeMap<String, serde_json::Value> = serde_json::from_str(
        r#"{
        "empty-provider": {
            "type": "openai",
            "baseUrl": "https://example.com",
            "apiKeyEnv": "SOME_KEY",
            "models": []
        }
    }"#,
    )
    .unwrap();

    let err = apply_settings_providers(&mut cfg, &providers, None).unwrap_err();
    assert!(
        matches!(&err, LlmError::InvalidRequest { message } if message.contains("at least one")),
        "expected InvalidRequest about empty models, got: {err:?}"
    );
}

/// An unknown `type` value is rejected with a message naming the type.
#[test]
fn unknown_type_is_error() {
    let mut cfg = builtin_anthropic_config("https://api.anthropic.com", false);
    let providers: BTreeMap<String, serde_json::Value> = serde_json::from_str(
        r#"{
        "weird": {
            "type": "cohere",
            "baseUrl": "https://api.cohere.ai",
            "apiKeyEnv": "COHERE_KEY",
            "models": [{ "id": "command-r" }]
        }
    }"#,
    )
    .unwrap();

    let err = apply_settings_providers(&mut cfg, &providers, None).unwrap_err();
    assert!(
        matches!(&err, LlmError::InvalidRequest { message } if message.contains("cohere")),
        "expected InvalidRequest naming the unknown type, got: {err:?}"
    );
}

/// A duplicate profile name is rejected with a clear error.
#[test]
fn duplicate_profile_name_is_error() {
    let mut cfg = builtin_anthropic_config("https://api.anthropic.com", false);
    // "anthropic" is already the builtin profile name.
    let providers: BTreeMap<String, serde_json::Value> = serde_json::from_str(
        r#"{
        "anthropic": {
            "type": "openai",
            "baseUrl": "https://example.com",
            "apiKeyEnv": "KEY",
            "models": [{ "id": "some-model" }]
        }
    }"#,
    )
    .unwrap();

    let err = apply_settings_providers(&mut cfg, &providers, None).unwrap_err();
    assert!(
        matches!(&err, LlmError::InvalidRequest { message } if message.contains("duplicate")),
        "expected InvalidRequest about duplicate profile, got: {err:?}"
    );
}

/// A provider entry with a missing `baseUrl` is rejected.
#[test]
fn missing_base_url_is_error() {
    let mut cfg = builtin_anthropic_config("https://api.anthropic.com", false);
    let providers: BTreeMap<String, serde_json::Value> = serde_json::from_str(
        r#"{
        "no-url": {
            "type": "openai",
            "apiKeyEnv": "SOME_KEY",
            "models": [{ "id": "some-model" }]
        }
    }"#,
    )
    .unwrap();

    let err = apply_settings_providers(&mut cfg, &providers, None).unwrap_err();
    assert!(
        matches!(&err, LlmError::InvalidRequest { message } if message.contains("baseUrl")),
        "expected InvalidRequest about missing baseUrl, got: {err:?}"
    );
}

/// A provider entry with an empty `baseUrl` is rejected.
#[test]
fn empty_base_url_is_error() {
    let mut cfg = builtin_anthropic_config("https://api.anthropic.com", false);
    let providers: BTreeMap<String, serde_json::Value> = serde_json::from_str(
        r#"{
        "empty-url": {
            "type": "openai",
            "baseUrl": "",
            "apiKeyEnv": "SOME_KEY",
            "models": [{ "id": "some-model" }]
        }
    }"#,
    )
    .unwrap();

    let err = apply_settings_providers(&mut cfg, &providers, None).unwrap_err();
    assert!(
        matches!(&err, LlmError::InvalidRequest { message } if message.contains("baseUrl")),
        "expected InvalidRequest about empty baseUrl, got: {err:?}"
    );
}

/// A provider entry with a missing `apiKeyEnv` is rejected.
#[test]
fn missing_api_key_env_is_error() {
    let mut cfg = builtin_anthropic_config("https://api.anthropic.com", false);
    let providers: BTreeMap<String, serde_json::Value> = serde_json::from_str(
        r#"{
        "no-key-env": {
            "type": "openai",
            "baseUrl": "https://example.com",
            "models": [{ "id": "some-model" }]
        }
    }"#,
    )
    .unwrap();

    let err = apply_settings_providers(&mut cfg, &providers, None).unwrap_err();
    assert!(
        matches!(&err, LlmError::InvalidRequest { message } if message.contains("apiKeyEnv")),
        "expected InvalidRequest about missing apiKeyEnv, got: {err:?}"
    );
}

/// A provider entry with an empty `apiKeyEnv` is rejected.
#[test]
fn empty_api_key_env_is_error() {
    let mut cfg = builtin_anthropic_config("https://api.anthropic.com", false);
    let providers: BTreeMap<String, serde_json::Value> = serde_json::from_str(
        r#"{
        "empty-key-env": {
            "type": "openai",
            "baseUrl": "https://example.com",
            "apiKeyEnv": "",
            "models": [{ "id": "some-model" }]
        }
    }"#,
    )
    .unwrap();

    let err = apply_settings_providers(&mut cfg, &providers, None).unwrap_err();
    assert!(
        matches!(&err, LlmError::InvalidRequest { message } if message.contains("apiKeyEnv")),
        "expected InvalidRequest about empty apiKeyEnv, got: {err:?}"
    );
}

/// An alias targeting an unknown profile/model is rejected.
#[test]
fn alias_unknown_target_is_error() {
    let mut cfg = builtin_anthropic_config("https://api.anthropic.com", false);
    let routing: serde_json::Value = serde_json::from_str(
        r#"{
        "aliases": { "fast": "nonexistent-profile/some-model" }
    }"#,
    )
    .unwrap();

    let err = apply_settings_providers(&mut cfg, &BTreeMap::new(), Some(&routing)).unwrap_err();
    assert!(
        matches!(&err, LlmError::InvalidRequest { message } if message.contains("not found")),
        "expected InvalidRequest about unknown alias target, got: {err:?}"
    );
}

// ── parse_routing_overrides tests ─────────────────────────────────────────

fn routing_test_cfg() -> ClientConfig {
    let mut cfg = builtin_anthropic_config("https://api.anthropic.com", false);
    // Add a second provider so we can test cross-profile fallback.
    cfg.providers.push(llm_runtime::ProviderProfile {
        wire_profile: None,
        regions: llm_runtime::Region::all(),
        provider_id: llm_runtime::ProviderId::OpenAICompatible {
            name: "groq".to_string(),
        },
        profile_name: "groq".to_string(),
        base_url: "https://api.groq.com".to_string(),
        protocol: llm_runtime::ProtocolFamily::OpenAiChat,
        auth: llm_runtime::AuthStrategy::ApiKey,
        credential: llm_runtime::CredentialConfig::Env {
            var: "GROQ_KEY".to_string(),
        },
        models: vec![llm_runtime::ModelProfile {
            display_model: "llama-3.3-70b".to_string(),
            request_model: "llama-3.3-70b".to_string(),
            billing_model: "llama-3.3-70b".to_string(),
            aliases: vec!["llama".to_string()],
            description: None,
            metadata: Default::default(),
            capabilities: llm_runtime::Capabilities {
                streaming: true,
                tools: true,
                ..Default::default()
            },
        }],
        pricing: PricingConfig::default(),
        signing: None,
        azure: None,
        supports_websockets: false,
        supports_websocket_compression: false,
        websocket_connect_timeout_ms: None,
        vision_delegate: None,
        connection: Default::default(),
    });
    cfg
}

/// Happy path: fallback + retry numbers parsed correctly.
#[test]
fn parse_routing_overrides_happy() {
    let cfg = routing_test_cfg();
    let routing: serde_json::Value = serde_json::from_str(
        r#"{
        "fallback": {
            "anthropic/claude-opus-4-7": ["anthropic/claude-sonnet-4-20250514"]
        },
        "retry": { "maxAttempts": 5, "backoffMs": 1000 }
    }"#,
    )
    .unwrap();

    let overrides = parse_routing_overrides(&routing, &cfg).expect("must succeed");
    assert_eq!(
        overrides.fallback.get("claude-opus-4-7"),
        Some(&vec!["claude-sonnet-4-20250514".to_string()]),
        "fallback key must normalize to display model; chain stored as Vec"
    );
    assert_eq!(overrides.max_retries, Some(5));
    assert_eq!(overrides.backoff_ms, Some(1000));
}

/// Unknown fallback target errors with not found.
#[test]
fn parse_routing_overrides_unknown_fallback_target_error() {
    let cfg = routing_test_cfg();
    let routing: serde_json::Value = serde_json::from_str(
        r#"{
        "fallback": {
            "anthropic/claude-opus-4-7": ["nonexistent/model"]
        }
    }"#,
    )
    .unwrap();

    let err = parse_routing_overrides(&routing, &cfg).unwrap_err();
    assert!(
        matches!(&err, LlmError::InvalidRequest { message } if message.contains("not found")),
        "expected not found error, got: {err:?}"
    );
}

/// Unknown fallback target in chain[1] also errors (every entry validated).
#[test]
fn parse_routing_overrides_unknown_chain1_target_error() {
    let cfg = routing_test_cfg();
    let routing: serde_json::Value = serde_json::from_str(
        r#"{
        "fallback": {
            "anthropic/claude-opus-4-7": [
                "anthropic/claude-sonnet-4-20250514",
                "nonexistent/model-two"
            ]
        }
    }"#,
    )
    .unwrap();

    let err = parse_routing_overrides(&routing, &cfg).unwrap_err();
    assert!(
        matches!(&err, LlmError::InvalidRequest { message } if message.contains("not found")),
        "chain[1] unknown target must error with not found, got: {err:?}"
    );
}

/// Multi-entry chain: all entries validated and stored in order (no warn, no truncation).
#[test]
fn parse_routing_overrides_full_chain_stored() {
    let cfg = routing_test_cfg();
    let routing: serde_json::Value = serde_json::from_str(
        r#"{
        "fallback": {
            "anthropic/claude-opus-4-7": [
                "anthropic/claude-sonnet-4-20250514",
                "groq/llama-3.3-70b"
            ]
        }
    }"#,
    )
    .unwrap();

    let overrides =
        parse_routing_overrides(&routing, &cfg).expect("must succeed with multi-entry chain");
    // Full chain must be stored in order.
    assert_eq!(
        overrides.fallback.get("claude-opus-4-7"),
        Some(&vec![
            "claude-sonnet-4-20250514".to_string(),
            "llama-3.3-70b".to_string(),
        ]),
        "full chain must be stored with all entries in order"
    );
}

/// Retry numbers parsed: maxAttempts and backoffMs.
#[test]
fn parse_routing_overrides_retry_numbers() {
    let cfg = routing_test_cfg();
    let routing: serde_json::Value = serde_json::from_str(
        r#"{
        "retry": { "maxAttempts": 3, "backoffMs": 2000 }
    }"#,
    )
    .unwrap();

    let overrides = parse_routing_overrides(&routing, &cfg).expect("must succeed");
    assert_eq!(overrides.max_retries, Some(3));
    assert_eq!(overrides.backoff_ms, Some(2000));
    assert!(overrides.fallback.is_empty());
}

/// `backoffMs: 0` is rejected at parse time (zero-delay retries are a
/// tight loop hammering the provider).
#[test]
fn parse_routing_overrides_backoff_zero_rejected() {
    let cfg = routing_test_cfg();
    let routing: serde_json::Value = serde_json::from_str(
        r#"{
        "retry": { "backoffMs": 0 }
    }"#,
    )
    .unwrap();

    let err = parse_routing_overrides(&routing, &cfg).expect_err("backoffMs=0 must error");
    let llm_runtime::LlmError::InvalidRequest { message } = err else {
        panic!("expected InvalidRequest, got {err:?}");
    };
    assert!(message.contains("backoffMs must be >= 1"), "got: {message}");
}

/// Absent routing → defaults (no overrides).
#[test]
fn parse_routing_overrides_absent_gives_defaults() {
    let cfg = routing_test_cfg();
    let routing: serde_json::Value = serde_json::from_str("{}").unwrap();

    let overrides = parse_routing_overrides(&routing, &cfg).expect("must succeed");
    assert!(overrides.fallback.is_empty());
    assert!(overrides.max_retries.is_none());
    assert!(overrides.backoff_ms.is_none());
}

/// Alias in key is resolved to display model.
#[test]
fn parse_routing_overrides_key_alias_resolves_to_display_model() {
    let cfg = routing_test_cfg();
    // "llama" is an alias for "llama-3.3-70b" in the groq profile.
    let routing: serde_json::Value = serde_json::from_str(
        r#"{
        "fallback": {
            "llama": ["anthropic/claude-sonnet-4-20250514"]
        }
    }"#,
    )
    .unwrap();

    let overrides = parse_routing_overrides(&routing, &cfg).expect("must succeed");
    assert_eq!(
        overrides.fallback.get("llama-3.3-70b"),
        Some(&vec!["claude-sonnet-4-20250514".to_string()]),
        "alias key 'llama' must resolve to display model 'llama-3.3-70b'"
    );
}

// ── parse_pricing_overrides (Task 2) tests ─────────────────────────────────

/// Happy path: a provider with a pricing block parses correctly and
/// `PricingConfig::overrides` carries the right `TokenPricing` values.
#[test]
fn pricing_overrides_happy_path() {
    let mut cfg = builtin_anthropic_config("https://api.anthropic.com", false);
    let providers: BTreeMap<String, serde_json::Value> = serde_json::from_str(
        r#"{
        "myprovider": {
            "type": "openai",
            "baseUrl": "https://api.example.com/v1",
            "apiKeyEnv": "MY_API_KEY",
            "models": [{ "id": "gpt-custom" }],
            "pricing": {
                "gpt-custom": {
                    "inputPerMtok": 1.5,
                    "outputPerMtok": 6.0,
                    "cacheWritePerMtok": 1.875,
                    "cacheReadPerMtok": 0.15,
                    "reasoningPerMtok": 6.0
                }
            }
        }
    }"#,
    )
    .unwrap();

    apply_settings_providers(&mut cfg, &providers, None).expect("must succeed");

    let p = cfg
        .providers
        .iter()
        .find(|p| p.profile_name == "myprovider")
        .unwrap();
    assert_eq!(p.pricing.overrides.len(), 1);
    let (model_id, tp) = &p.pricing.overrides[0];
    assert_eq!(model_id, "gpt-custom");
    assert!(
        (tp.input_per_million - 1.5).abs() < 1e-12,
        "input_per_million"
    );
    assert!(
        (tp.output_per_million - 6.0).abs() < 1e-12,
        "output_per_million"
    );
    assert!(
        (tp.cache_write_per_million - 1.875).abs() < 1e-12,
        "cache_write_per_million"
    );
    assert!(
        (tp.cache_read_per_million - 0.15).abs() < 1e-12,
        "cache_read_per_million"
    );
    assert!(
        (tp.reasoning_per_million - 6.0).abs() < 1e-12,
        "reasoning_per_million"
    );
}

/// Absent pricing block → empty overrides (existing behavior preserved).
#[test]
fn pricing_overrides_absent_gives_empty() {
    let mut cfg = builtin_anthropic_config("https://api.anthropic.com", false);
    let providers: BTreeMap<String, serde_json::Value> = serde_json::from_str(
        r#"{
        "myprovider": {
            "type": "openai",
            "baseUrl": "https://api.example.com/v1",
            "apiKeyEnv": "MY_API_KEY",
            "models": [{ "id": "gpt-custom" }]
        }
    }"#,
    )
    .unwrap();

    apply_settings_providers(&mut cfg, &providers, None).expect("must succeed");

    let p = cfg
        .providers
        .iter()
        .find(|p| p.profile_name == "myprovider")
        .unwrap();
    assert!(
        p.pricing.overrides.is_empty(),
        "absent pricing must produce empty overrides"
    );
}

/// Unknown model id in the pricing block → `LlmError::InvalidRequest`.
#[test]
fn pricing_overrides_unknown_model_is_error() {
    let mut cfg = builtin_anthropic_config("https://api.anthropic.com", false);
    let providers: BTreeMap<String, serde_json::Value> = serde_json::from_str(
        r#"{
        "myprovider": {
            "type": "openai",
            "baseUrl": "https://api.example.com/v1",
            "apiKeyEnv": "MY_API_KEY",
            "models": [{ "id": "gpt-custom" }],
            "pricing": {
                "nonexistent-model": { "inputPerMtok": 1.0, "outputPerMtok": 2.0 }
            }
        }
    }"#,
    )
    .unwrap();

    let err = apply_settings_providers(&mut cfg, &providers, None).unwrap_err();
    assert!(
        matches!(&err, LlmError::InvalidRequest { message } if message.contains("nonexistent-model")),
        "expected InvalidRequest naming the unknown model, got: {err:?}"
    );
}

/// Negative price → `LlmError::InvalidRequest`.
#[test]
fn pricing_overrides_negative_price_is_error() {
    let mut cfg = builtin_anthropic_config("https://api.anthropic.com", false);
    let providers: BTreeMap<String, serde_json::Value> = serde_json::from_str(
        r#"{
        "myprovider": {
            "type": "openai",
            "baseUrl": "https://api.example.com/v1",
            "apiKeyEnv": "MY_API_KEY",
            "models": [{ "id": "gpt-custom" }],
            "pricing": {
                "gpt-custom": { "inputPerMtok": -1.0, "outputPerMtok": 2.0 }
            }
        }
    }"#,
    )
    .unwrap();

    let err = apply_settings_providers(&mut cfg, &providers, None).unwrap_err();
    assert!(
        matches!(&err, LlmError::InvalidRequest { message } if message.contains("inputPerMtok") && message.contains(">= 0")),
        "expected InvalidRequest about negative price, got: {err:?}"
    );
}

/// Unknown key in a model's pricing object → `LlmError::InvalidRequest` naming it.
#[test]
fn pricing_overrides_unknown_key_is_error() {
    let mut cfg = builtin_anthropic_config("https://api.anthropic.com", false);
    let providers: BTreeMap<String, serde_json::Value> = serde_json::from_str(
        r#"{
        "myprovider": {
            "type": "openai",
            "baseUrl": "https://api.example.com/v1",
            "apiKeyEnv": "MY_API_KEY",
            "models": [{ "id": "gpt-custom" }],
            "pricing": {
                "gpt-custom": {
                    "inputPerMtok": 1.0,
                    "outputPerMtok": 2.0,
                    "typoKey": 3.0
                }
            }
        }
    }"#,
    )
    .unwrap();

    let err = apply_settings_providers(&mut cfg, &providers, None).unwrap_err();
    assert!(
        matches!(&err, LlmError::InvalidRequest { message } if message.contains("typoKey")),
        "expected InvalidRequest naming the unknown key, got: {err:?}"
    );
}

/// Non-number price value → `LlmError::InvalidRequest`.
#[test]
fn pricing_overrides_non_number_price_is_error() {
    let mut cfg = builtin_anthropic_config("https://api.anthropic.com", false);
    let providers: BTreeMap<String, serde_json::Value> = serde_json::from_str(
        r#"{
        "myprovider": {
            "type": "openai",
            "baseUrl": "https://api.example.com/v1",
            "apiKeyEnv": "MY_API_KEY",
            "models": [{ "id": "gpt-custom" }],
            "pricing": {
                "gpt-custom": { "inputPerMtok": "not-a-number", "outputPerMtok": 2.0 }
            }
        }
    }"#,
    )
    .unwrap();

    let err = apply_settings_providers(&mut cfg, &providers, None).unwrap_err();
    assert!(
        matches!(&err, LlmError::InvalidRequest { message } if message.contains("inputPerMtok") && message.contains("number")),
        "expected InvalidRequest about non-number, got: {err:?}"
    );
}

/// Optional fields (cacheWrite/cacheRead/reasoning) may be omitted.
#[test]
fn pricing_overrides_optional_fields_may_be_absent() {
    let mut cfg = builtin_anthropic_config("https://api.anthropic.com", false);
    let providers: BTreeMap<String, serde_json::Value> = serde_json::from_str(
        r#"{
        "myprovider": {
            "type": "openai",
            "baseUrl": "https://api.example.com/v1",
            "apiKeyEnv": "MY_API_KEY",
            "models": [{ "id": "gpt-custom" }],
            "pricing": {
                "gpt-custom": { "inputPerMtok": 2.0, "outputPerMtok": 8.0 }
            }
        }
    }"#,
    )
    .unwrap();

    apply_settings_providers(&mut cfg, &providers, None)
        .expect("must succeed with minimal pricing");

    let p = cfg
        .providers
        .iter()
        .find(|p| p.profile_name == "myprovider")
        .unwrap();
    let (_, tp) = &p.pricing.overrides[0];
    assert!((tp.input_per_million - 2.0).abs() < 1e-12);
    assert!((tp.output_per_million - 8.0).abs() < 1e-12);
    assert!(
        (tp.cache_write_per_million - 0.0).abs() < 1e-12,
        "cache_write defaults to 0"
    );
    assert!(
        (tp.cache_read_per_million - 0.0).abs() < 1e-12,
        "cache_read defaults to 0"
    );
    assert!(
        (tp.reasoning_per_million - 0.0).abs() < 1e-12,
        "reasoning defaults to 0"
    );
}

// ── azure-openai settings type (Task 5) tests ─────────────────────────────

/// E2E: an azure-openai profile parses correctly, is built into a
/// `DefaultLlmClient`, and `prepare()` produces:
/// - A URL with `/openai/deployments/<model>/chat/completions?api-version=...`
/// - An `api-key` header (AzureToken auth)
/// - No `model` key in the request body
///
/// Settings E2E test name: `azure_profile_prepare_url_and_api_key_header`
#[tokio::test]
async fn azure_profile_prepare_url_and_api_key_header() {
    std::env::set_var("PLATFORM_COMMON_TEST_AZURE_KEY", "my-azure-api-key");

    let mut cfg = builtin_anthropic_config("https://api.anthropic.com", false);
    let providers: BTreeMap<String, serde_json::Value> = serde_json::from_str(
        r#"{
        "my-azure": {
            "type": "azure-openai",
            "baseUrl": "https://myresource.openai.azure.com",
            "apiKeyEnv": "PLATFORM_COMMON_TEST_AZURE_KEY",
            "apiVersion": "2024-02-01",
            "models": [
                { "id": "gpt-4o-deployment" }
            ]
        }
    }"#,
    )
    .unwrap();

    apply_settings_providers(&mut cfg, &providers, None).expect("must succeed");

    // Verify the profile was parsed correctly.
    let azure_profile = cfg
        .providers
        .iter()
        .find(|p| p.profile_name == "my-azure")
        .expect("my-azure profile must be present");
    assert_eq!(
        azure_profile.protocol,
        llm_runtime::ProtocolFamily::AzureOpenAi
    );
    assert_eq!(azure_profile.auth, llm_runtime::AuthStrategy::AzureToken);
    assert!(
        azure_profile.azure.as_ref().map(|a| a.api_version.as_str()) == Some("2024-02-01"),
        "azure config must have apiVersion=2024-02-01"
    );

    // Build a client and prepare a request.
    let client = DefaultLlmClient::from_config(cfg).expect("client must build");
    let req = llm_runtime::LlmRequest::new("gpt-4o-deployment");
    let prepared = client.prepare(&req).await.expect("prepare must succeed");

    // URL check: deployment pattern.
    assert!(
        prepared
            .provider_request
            .url
            .contains("/openai/deployments/gpt-4o-deployment/chat/completions"),
        "URL must include deployment path; got: {}",
        prepared.provider_request.url
    );
    assert!(
        prepared
            .provider_request
            .url
            .contains("api-version=2024-02-01"),
        "URL must include api-version; got: {}",
        prepared.provider_request.url
    );

    // Auth check: api-key header present.
    assert_eq!(
        prepared
            .provider_request
            .headers
            .get("api-key")
            .map(String::as_str),
        Some("my-azure-api-key"),
        "api-key header must be injected by AzureToken auth"
    );

    // No Authorization header (Azure uses api-key, not Bearer).
    assert!(
        !prepared
            .provider_request
            .headers
            .contains_key("Authorization"),
        "AzureToken must NOT inject Authorization header"
    );

    // Model key must be absent from body (deployment is in the URL).
    assert!(
        prepared.provider_request.body_json.get("model").is_none(),
        "Azure request body must not include model key; got: {}",
        prepared.provider_request.body_json
    );
}

/// azure-openai with missing apiVersion → error naming apiVersion.
#[test]
fn azure_openai_missing_api_version_is_error() {
    let mut cfg = builtin_anthropic_config("https://api.anthropic.com", false);
    let providers: BTreeMap<String, serde_json::Value> = serde_json::from_str(
        r#"{
        "my-azure": {
            "type": "azure-openai",
            "baseUrl": "https://myresource.openai.azure.com",
            "apiKeyEnv": "SOME_KEY",
            "models": [{ "id": "gpt-4o" }]
        }
    }"#,
    )
    .unwrap();

    let err = apply_settings_providers(&mut cfg, &providers, None).unwrap_err();
    assert!(
        matches!(&err, LlmError::InvalidRequest { message } if message.contains("apiVersion")),
        "expected InvalidRequest about missing apiVersion, got: {err:?}"
    );
}

/// azure-openai with empty apiVersion → error.
#[test]
fn azure_openai_empty_api_version_is_error() {
    let mut cfg = builtin_anthropic_config("https://api.anthropic.com", false);
    let providers: BTreeMap<String, serde_json::Value> = serde_json::from_str(
        r#"{
        "my-azure": {
            "type": "azure-openai",
            "baseUrl": "https://myresource.openai.azure.com",
            "apiKeyEnv": "SOME_KEY",
            "apiVersion": "",
            "models": [{ "id": "gpt-4o" }]
        }
    }"#,
    )
    .unwrap();

    let err = apply_settings_providers(&mut cfg, &providers, None).unwrap_err();
    assert!(
        matches!(&err, LlmError::InvalidRequest { message } if message.contains("apiVersion")),
        "expected InvalidRequest about empty apiVersion, got: {err:?}"
    );
}

/// The error message for unknown type now includes both "azure-openai" and "bedrock-claude".
#[test]
fn unknown_type_mentions_azure_openai_in_supported_list() {
    let mut cfg = builtin_anthropic_config("https://api.anthropic.com", false);
    let providers: BTreeMap<String, serde_json::Value> = serde_json::from_str(
        r#"{
        "weird": {
            "type": "cohere-v2",
            "baseUrl": "https://api.cohere.ai",
            "apiKeyEnv": "COHERE_KEY",
            "models": [{ "id": "command-r" }]
        }
    }"#,
    )
    .unwrap();

    let err = apply_settings_providers(&mut cfg, &providers, None).unwrap_err();
    assert!(
        matches!(&err, LlmError::InvalidRequest { message } if message.contains("azure-openai")),
        "error for unknown type must list azure-openai as supported, got: {err:?}"
    );
    assert!(
        matches!(&err, LlmError::InvalidRequest { message } if message.contains("bedrock-claude")),
        "error for unknown type must list bedrock-claude as supported, got: {err:?}"
    );
}

// ── bedrock-claude settings type tests ────────────────────────────────────

/// A bedrock-claude profile with `region` and no `baseUrl` defaults the base
/// URL to `https://bedrock-runtime.<region>.amazonaws.com`.
#[test]
fn bedrock_claude_default_base_url_from_region() {
    let mut cfg = builtin_anthropic_config("https://api.anthropic.com", false);
    let providers: BTreeMap<String, serde_json::Value> = serde_json::from_str(
        r#"{
        "my-bedrock": {
            "type": "bedrock-claude",
            "region": "us-east-1",
            "models": [{ "id": "anthropic.claude-3-5-sonnet-20241022-v2:0" }]
        }
    }"#,
    )
    .unwrap();

    apply_settings_providers(&mut cfg, &providers, None).expect("must succeed");

    let bedrock = cfg
        .providers
        .iter()
        .find(|p| p.profile_name == "my-bedrock")
        .unwrap();
    assert_eq!(
        bedrock.base_url, "https://bedrock-runtime.us-east-1.amazonaws.com",
        "base_url must default to region-derived endpoint"
    );
    assert_eq!(bedrock.protocol, ProtocolFamily::BedrockClaude);
    assert_eq!(bedrock.auth, AuthStrategy::AwsSigV4);
    assert!(
        bedrock
            .signing
            .as_ref()
            .map(|s| (s.region.as_str(), s.service.as_str()))
            == Some(("us-east-1", "bedrock")),
        "signing config must have region=us-east-1 and service=bedrock; got {:?}",
        bedrock.signing
    );
    assert_eq!(
        bedrock.credential,
        CredentialConfig::HostManaged { id: "bedrock_sigv4".to_string() },
        "bedrock-claude must use CredentialConfig::HostManaged so the injected provider is consulted"
    );
}

/// A bedrock-claude profile with an explicit `baseUrl` must use that URL
/// instead of the default region-derived endpoint.
#[test]
fn bedrock_claude_explicit_base_url_overrides_default() {
    let mut cfg = builtin_anthropic_config("https://api.anthropic.com", false);
    let providers: BTreeMap<String, serde_json::Value> = serde_json::from_str(
        r#"{
        "my-bedrock": {
            "type": "bedrock-claude",
            "region": "eu-west-1",
            "baseUrl": "https://custom-bedrock.example.com",
            "models": [{ "id": "anthropic.claude-3-haiku-20240307-v1:0" }]
        }
    }"#,
    )
    .unwrap();

    apply_settings_providers(&mut cfg, &providers, None).expect("must succeed");

    let bedrock = cfg
        .providers
        .iter()
        .find(|p| p.profile_name == "my-bedrock")
        .unwrap();
    assert_eq!(
        bedrock.base_url, "https://custom-bedrock.example.com",
        "explicit baseUrl must override the region-derived default"
    );
    assert!(
        bedrock.signing.as_ref().map(|s| s.region.as_str()) == Some("eu-west-1"),
        "region in signing config must still come from \"region\" key"
    );
}

/// A bedrock-claude profile without a `region` key is rejected.
#[test]
fn bedrock_claude_missing_region_is_error() {
    let mut cfg = builtin_anthropic_config("https://api.anthropic.com", false);
    let providers: BTreeMap<String, serde_json::Value> = serde_json::from_str(
        r#"{
        "my-bedrock": {
            "type": "bedrock-claude",
            "models": [{ "id": "anthropic.claude-3-5-sonnet-20241022-v2:0" }]
        }
    }"#,
    )
    .unwrap();

    let err = apply_settings_providers(&mut cfg, &providers, None).unwrap_err();
    assert!(
        matches!(&err, LlmError::InvalidRequest { message } if message.contains("region")),
        "expected InvalidRequest about missing region, got: {err:?}"
    );
}

/// E2E: a `bedrock-claude` profile parsed from settings builds a
/// [`DefaultLlmClient`], and `prepare_at` with a fixed clock produces:
/// - URL: `{base_url}/model/{model_id}/invoke`
/// - `x-amz-date` header present
/// - `x-amz-content-sha256` header present
/// - `Authorization` header starting with `AWS4-HMAC-SHA256`
/// - No `model` key in the request body
/// - `anthropic_version: "bedrock-2023-05-31"` in body
///
/// Test name: `bedrock_claude_prepare_e2e_sigv4_headers`
#[tokio::test]
async fn bedrock_claude_prepare_e2e_sigv4_headers() {
    use llm_runtime::{Credential, DefaultLlmClient, StaticCredentialProvider};
    use std::sync::Arc;
    use std::time::{Duration, UNIX_EPOCH};

    let mut cfg = builtin_anthropic_config("https://api.anthropic.com", false);
    let providers: BTreeMap<String, serde_json::Value> = serde_json::from_str(r#"{
        "my-bedrock": {
            "type": "bedrock-claude",
            "region": "us-east-1",
            "models": [
                { "id": "anthropic.claude-3-5-sonnet-20241022-v2:0", "capabilities": {"streaming": true, "tools": true} }
            ]
        }
    }"#).unwrap();

    apply_settings_providers(&mut cfg, &providers, None).expect("must succeed");

    // Inject static SigV4 credentials so prepare_at succeeds without
    // requiring real AWS environment variables.
    let credentials = Arc::new(StaticCredentialProvider::new(Credential::AwsSigV4 {
        access_key_id: "AKIAIOSFODNN7EXAMPLE".to_string(),
        secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".to_string(),
        session_token: None,
    }));

    let client = DefaultLlmClient::from_config(cfg)
        .expect("client must build")
        .with_credential_provider(credentials);

    // Fixed clock: 2024-01-15T12:34:56Z (Unix epoch 1705322096)
    let fixed_now = UNIX_EPOCH + Duration::from_secs(1_705_322_096);
    let req = llm_runtime::LlmRequest::new("anthropic.claude-3-5-sonnet-20241022-v2:0");
    let prepared = client
        .prepare_at(&req, fixed_now)
        .await
        .expect("prepare_at must succeed");

    // URL: non-streaming must use /invoke.
    assert!(
        prepared.provider_request.url.ends_with("/invoke"),
        "URL must end with /invoke; got: {}",
        prepared.provider_request.url
    );
    assert!(
        prepared
            .provider_request
            .url
            .contains("anthropic.claude-3-5-sonnet-20241022-v2:0"),
        "URL must contain model id with raw ':'; got: {}",
        prepared.provider_request.url
    );

    // x-amz-date must be present and match the fixed clock.
    let amz_date = prepared
        .provider_request
        .headers
        .get("x-amz-date")
        .expect("x-amz-date header must be present");
    assert_eq!(
        amz_date, "20240115T123456Z",
        "x-amz-date must match the fixed clock"
    );

    // x-amz-content-sha256 must be present.
    assert!(
        prepared
            .provider_request
            .headers
            .contains_key("x-amz-content-sha256"),
        "x-amz-content-sha256 header must be present"
    );

    // Authorization header must use AWS4-HMAC-SHA256.
    let auth = prepared
        .provider_request
        .headers
        .get("Authorization")
        .expect("Authorization header must be present");
    assert!(
        auth.starts_with("AWS4-HMAC-SHA256"),
        "Authorization must start with AWS4-HMAC-SHA256; got: {auth}"
    );
    assert!(
        auth.contains("20240115"),
        "Authorization must contain the signing date 20240115; got: {auth}"
    );
    assert!(
        auth.contains("us-east-1/bedrock/aws4_request"),
        "Authorization must contain the credential scope; got: {auth}"
    );

    // Model key must be absent from body.
    assert!(
        prepared.provider_request.body_json.get("model").is_none(),
        "body must not contain model key; got: {}",
        prepared.provider_request.body_json
    );

    // anthropic_version must be in body.
    assert_eq!(
        prepared
            .provider_request
            .body_json
            .get("anthropic_version")
            .and_then(serde_json::Value::as_str),
        Some("bedrock-2023-05-31"),
        "body must contain anthropic_version=bedrock-2023-05-31"
    );

    // No anthropic-version header.
    assert!(
        !prepared
            .provider_request
            .headers
            .contains_key("anthropic-version"),
        "anthropic-version header must NOT be present for Bedrock"
    );
}

// ── vertex-claude settings type tests ─────────────────────────────────────

/// A `vertex-claude` profile parses correctly: `VertexClaude` protocol,
/// `GcpToken` auth, `CredentialConfig::Env` from `apiKeyEnv`, `baseUrl` REQUIRED.
#[test]
fn vertex_claude_profile_parses() {
    let mut cfg = builtin_anthropic_config("https://api.anthropic.com", false);
    let providers: BTreeMap<String, serde_json::Value> = serde_json::from_str(r#"{
        "my-vertex-claude": {
            "type": "vertex-claude",
            "baseUrl": "https://us-central1-aiplatform.googleapis.com/v1/projects/my-proj/locations/us-central1",
            "apiKeyEnv": "VERTEX_BEARER_TOKEN",
            "models": [{ "id": "claude-sonnet-4@20250514" }]
        }
    }"#).unwrap();

    apply_settings_providers(&mut cfg, &providers, None).expect("must succeed");

    let p = cfg
        .providers
        .iter()
        .find(|p| p.profile_name == "my-vertex-claude")
        .unwrap();
    assert_eq!(p.protocol, ProtocolFamily::VertexClaude);
    assert_eq!(p.auth, AuthStrategy::GcpToken);
    assert_eq!(
        p.credential,
        CredentialConfig::Env {
            var: "VERTEX_BEARER_TOKEN".to_string()
        },
        "vertex-claude must use CredentialConfig::Env so the token env var is consulted"
    );
    assert_eq!(
        p.base_url,
        "https://us-central1-aiplatform.googleapis.com/v1/projects/my-proj/locations/us-central1"
    );
}

/// `vertex-claude` with missing `baseUrl` is rejected (baseUrl is REQUIRED).
#[test]
fn vertex_claude_missing_base_url_is_error() {
    let mut cfg = builtin_anthropic_config("https://api.anthropic.com", false);
    let providers: BTreeMap<String, serde_json::Value> = serde_json::from_str(
        r#"{
        "my-vertex-claude": {
            "type": "vertex-claude",
            "apiKeyEnv": "VERTEX_BEARER_TOKEN",
            "models": [{ "id": "claude-sonnet-4@20250514" }]
        }
    }"#,
    )
    .unwrap();

    let err = apply_settings_providers(&mut cfg, &providers, None).unwrap_err();
    assert!(
        matches!(&err, LlmError::InvalidRequest { message } if message.contains("baseUrl")),
        "expected InvalidRequest about missing baseUrl, got: {err:?}"
    );
}

/// E2E: a `vertex-claude` profile parsed from settings builds a
/// [`DefaultLlmClient`], and `prepare()` with a token env var produces:
/// - URL: `{base_url}/publishers/anthropic/models/{model}:rawPredict`
/// - `Authorization: Bearer <token>` header
/// - No `model` key in body
/// - `anthropic_version: "vertex-2023-10-16"` in body
/// - No `anthropic-version` header
///
/// Credential-loading choice: `CredentialConfig::Env { var }` causes
/// `EnvCredentialProvider` to load the env var as `Credential::ApiKey(value)`.
/// The `GcpToken` authenticate arm calls `load_secret()`, which accepts both
/// `Credential::ApiKey` and `Credential::BearerToken` as a plain string and
/// passes it to `BearerAuthenticator` → `Authorization: Bearer <value>`.
#[tokio::test]
async fn vertex_claude_prepare_e2e_bearer_header() {
    std::env::set_var(
        "PLATFORM_COMMON_TEST_VERTEX_CLAUDE_TOKEN",
        "my-gcp-bearer-token",
    );

    let mut cfg = builtin_anthropic_config("https://api.anthropic.com", false);
    let providers: BTreeMap<String, serde_json::Value> = serde_json::from_str(r#"{
        "my-vertex-claude": {
            "type": "vertex-claude",
            "baseUrl": "https://us-central1-aiplatform.googleapis.com/v1/projects/my-proj/locations/us-central1",
            "apiKeyEnv": "PLATFORM_COMMON_TEST_VERTEX_CLAUDE_TOKEN",
            "models": [
                { "id": "claude-sonnet-4@20250514", "capabilities": {"streaming": true, "tools": true} }
            ]
        }
    }"#).unwrap();

    apply_settings_providers(&mut cfg, &providers, None).expect("must succeed");

    let client = DefaultLlmClient::from_config(cfg).expect("client must build");
    let req = llm_runtime::LlmRequest::new("claude-sonnet-4@20250514");
    let prepared = client.prepare(&req).await.expect("prepare must succeed");

    // URL: non-streaming must use :rawPredict.
    assert!(
        prepared.provider_request.url.ends_with(":rawPredict"),
        "URL must end with :rawPredict; got: {}",
        prepared.provider_request.url
    );
    assert!(
        prepared
            .provider_request
            .url
            .contains("/publishers/anthropic/models/"),
        "URL must contain /publishers/anthropic/models/; got: {}",
        prepared.provider_request.url
    );

    // Authorization: Bearer <token> must be present.
    let auth_header = prepared
        .provider_request
        .headers
        .get("Authorization")
        .expect("Authorization header must be present for GcpToken");
    assert_eq!(
        auth_header, "Bearer my-gcp-bearer-token",
        "Authorization must be Bearer <token>"
    );

    // No model key in body.
    assert!(
        prepared.provider_request.body_json.get("model").is_none(),
        "body must not contain model key; got: {}",
        prepared.provider_request.body_json
    );

    // anthropic_version: vertex-2023-10-16 in body.
    assert_eq!(
        prepared
            .provider_request
            .body_json
            .get("anthropic_version")
            .and_then(serde_json::Value::as_str),
        Some("vertex-2023-10-16"),
        "body must contain anthropic_version=vertex-2023-10-16"
    );

    // No anthropic-version header.
    assert!(
        !prepared
            .provider_request
            .headers
            .contains_key("anthropic-version"),
        "anthropic-version header must NOT be present for VertexClaude"
    );
}

// ── vertex-gemini settings type tests ─────────────────────────────────────

/// A `vertex-gemini` profile parses correctly: `VertexGemini` protocol,
/// `GcpToken` auth, `CredentialConfig::Env` from `apiKeyEnv`, `baseUrl` REQUIRED.
#[test]
fn vertex_gemini_profile_parses() {
    let mut cfg = builtin_anthropic_config("https://api.anthropic.com", false);
    let providers: BTreeMap<String, serde_json::Value> = serde_json::from_str(r#"{
        "my-vertex-gemini": {
            "type": "vertex-gemini",
            "baseUrl": "https://us-central1-aiplatform.googleapis.com/v1/projects/my-proj/locations/us-central1",
            "apiKeyEnv": "VERTEX_BEARER_TOKEN",
            "models": [{ "id": "gemini-2.0-flash" }]
        }
    }"#).unwrap();

    apply_settings_providers(&mut cfg, &providers, None).expect("must succeed");

    let p = cfg
        .providers
        .iter()
        .find(|p| p.profile_name == "my-vertex-gemini")
        .unwrap();
    assert_eq!(p.protocol, ProtocolFamily::VertexGemini);
    assert_eq!(p.auth, AuthStrategy::GcpToken);
    assert_eq!(
        p.credential,
        CredentialConfig::Env {
            var: "VERTEX_BEARER_TOKEN".to_string()
        },
        "vertex-gemini must use CredentialConfig::Env so the token env var is consulted"
    );
}

/// E2E: a `vertex-gemini` profile parsed from settings builds a
/// [`DefaultLlmClient`], and `prepare()` with a token env var produces:
/// - URL: `{base_url}/publishers/google/models/{model}:generateContent`
/// - `Authorization: Bearer <token>` header
/// - `contents` array in body (Gemini format)
#[tokio::test]
async fn vertex_gemini_prepare_e2e_bearer_header() {
    std::env::set_var(
        "PLATFORM_COMMON_TEST_VERTEX_GEMINI_TOKEN",
        "my-vertex-gemini-token",
    );

    let mut cfg = builtin_anthropic_config("https://api.anthropic.com", false);
    let providers: BTreeMap<String, serde_json::Value> = serde_json::from_str(r#"{
        "my-vertex-gemini": {
            "type": "vertex-gemini",
            "baseUrl": "https://us-central1-aiplatform.googleapis.com/v1/projects/my-proj/locations/us-central1",
            "apiKeyEnv": "PLATFORM_COMMON_TEST_VERTEX_GEMINI_TOKEN",
            "models": [
                { "id": "gemini-2.0-flash", "capabilities": {"streaming": true, "tools": true} }
            ]
        }
    }"#).unwrap();

    apply_settings_providers(&mut cfg, &providers, None).expect("must succeed");

    let client = DefaultLlmClient::from_config(cfg).expect("client must build");
    let req = llm_runtime::LlmRequest::new("gemini-2.0-flash");
    let prepared = client.prepare(&req).await.expect("prepare must succeed");

    // URL: non-streaming must use :generateContent.
    assert!(
        prepared.provider_request.url.ends_with(":generateContent"),
        "URL must end with :generateContent; got: {}",
        prepared.provider_request.url
    );
    assert!(
        prepared
            .provider_request
            .url
            .contains("/publishers/google/models/"),
        "URL must contain /publishers/google/models/; got: {}",
        prepared.provider_request.url
    );

    // Authorization: Bearer <token> must be present.
    let auth_header = prepared
        .provider_request
        .headers
        .get("Authorization")
        .expect("Authorization header must be present for GcpToken");
    assert_eq!(
        auth_header, "Bearer my-vertex-gemini-token",
        "Authorization must be Bearer <token>"
    );

    // Body must have contents array (Gemini format).
    assert!(
        prepared
            .provider_request
            .body_json
            .get("contents")
            .is_some(),
        "body must have 'contents' (Gemini format); got: {}",
        prepared.provider_request.body_json
    );

    // No x-goog-api-key header (Vertex uses Bearer, not api-key).
    assert!(
        !prepared
            .provider_request
            .headers
            .contains_key("x-goog-api-key"),
        "x-goog-api-key must NOT be present for Vertex Gemini"
    );
}

/// The error message for unknown type includes `vertex-claude` and `vertex-gemini`.
#[test]
fn unknown_type_mentions_vertex_types_in_supported_list() {
    let mut cfg = builtin_anthropic_config("https://api.anthropic.com", false);
    let providers: BTreeMap<String, serde_json::Value> = serde_json::from_str(
        r#"{
        "weird": {
            "type": "cohere-v3",
            "baseUrl": "https://api.cohere.ai",
            "apiKeyEnv": "COHERE_KEY",
            "models": [{ "id": "command-r" }]
        }
    }"#,
    )
    .unwrap();

    let err = apply_settings_providers(&mut cfg, &providers, None).unwrap_err();
    let LlmError::InvalidRequest { message } = err else {
        panic!("expected InvalidRequest, got something else");
    };
    assert!(
        message.contains("vertex-claude"),
        "must list vertex-claude; got: {message}"
    );
    assert!(
        message.contains("vertex-gemini"),
        "must list vertex-gemini; got: {message}"
    );
}

// ── openai-responses settings type tests ──────────────────────────────────

/// An `openai-responses` profile parses correctly: `OpenAiResponses`
/// protocol, `ApiKey` auth, `CredentialConfig::Env` from `apiKeyEnv`,
/// `baseUrl` REQUIRED (mirrors the `"openai"` type exactly).
#[test]
fn openai_responses_profile_parses() {
    let mut cfg = builtin_anthropic_config("https://api.anthropic.com", false);
    let providers: BTreeMap<String, serde_json::Value> = serde_json::from_str(
        r#"{
        "my-responses": {
            "type": "openai-responses",
            "baseUrl": "https://api.openai.com/v1",
            "apiKeyEnv": "OPENAI_API_KEY",
            "models": [
                { "id": "gpt-4o" }
            ]
        }
    }"#,
    )
    .unwrap();

    apply_settings_providers(&mut cfg, &providers, None).expect("must succeed");

    assert_eq!(cfg.providers.len(), 2, "builtin + my-responses");
    let p = cfg
        .providers
        .iter()
        .find(|p| p.profile_name == "my-responses")
        .unwrap();
    assert_eq!(p.base_url, "https://api.openai.com/v1");
    assert_eq!(p.protocol, ProtocolFamily::OpenAiResponses);
    assert_eq!(p.auth, AuthStrategy::ApiKey);
    assert!(
        matches!(&p.provider_id, ProviderId::OpenAICompatible { name } if name == "my-responses"),
        "provider_id must be OpenAICompatible with name=my-responses"
    );
    assert_eq!(
        p.credential,
        CredentialConfig::Env {
            var: "OPENAI_API_KEY".to_string()
        }
    );
    assert_eq!(p.models.len(), 1);
    assert_eq!(p.models[0].display_model, "gpt-4o");
    // Default capabilities: streaming + tools, no vision/docs/reasoning.
    assert!(p.models[0].capabilities.streaming);
    assert!(p.models[0].capabilities.tools);
    assert!(!p.models[0].capabilities.vision);
    assert!(!p.models[0].capabilities.reasoning);
}

/// openai-responses with no baseUrl → error (same rule as `"openai"`).
#[test]
fn openai_responses_missing_base_url_is_error() {
    let mut cfg = builtin_anthropic_config("https://api.anthropic.com", false);
    let providers: BTreeMap<String, serde_json::Value> = serde_json::from_str(
        r#"{
        "my-responses": {
            "type": "openai-responses",
            "apiKeyEnv": "OPENAI_API_KEY",
            "models": [{ "id": "gpt-4o" }]
        }
    }"#,
    )
    .unwrap();

    let err = apply_settings_providers(&mut cfg, &providers, None).unwrap_err();
    assert!(
        matches!(&err, LlmError::InvalidRequest { message } if message.contains("baseUrl")),
        "expected InvalidRequest about missing baseUrl, got: {err:?}"
    );
}

/// openai-responses with no apiKeyEnv → error (same rule as `"openai"`).
#[test]
fn openai_responses_missing_api_key_env_is_error() {
    let mut cfg = builtin_anthropic_config("https://api.anthropic.com", false);
    let providers: BTreeMap<String, serde_json::Value> = serde_json::from_str(
        r#"{
        "my-responses": {
            "type": "openai-responses",
            "baseUrl": "https://api.openai.com/v1",
            "models": [{ "id": "gpt-4o" }]
        }
    }"#,
    )
    .unwrap();

    let err = apply_settings_providers(&mut cfg, &providers, None).unwrap_err();
    assert!(
        matches!(&err, LlmError::InvalidRequest { message } if message.contains("apiKeyEnv")),
        "expected InvalidRequest about missing apiKeyEnv, got: {err:?}"
    );
}

/// E2E: an `openai-responses` profile parsed from settings builds a
/// [`DefaultLlmClient`], and `prepare()` with a key env var produces:
/// - URL: `{base_url}/responses`, method POST
/// - `Authorization: Bearer <key>` header (OpenAI family ApiKey auth)
#[tokio::test]
async fn openai_responses_prepare_e2e_responses_url_and_bearer_header() {
    std::env::set_var(
        "PLATFORM_COMMON_TEST_OPENAI_RESPONSES_KEY",
        "my-responses-key",
    );

    let mut cfg = builtin_anthropic_config("https://api.anthropic.com", false);
    let providers: BTreeMap<String, serde_json::Value> = serde_json::from_str(
        r#"{
        "my-responses": {
            "type": "openai-responses",
            "baseUrl": "https://api.openai.com/v1",
            "apiKeyEnv": "PLATFORM_COMMON_TEST_OPENAI_RESPONSES_KEY",
            "models": [
                { "id": "gpt-4o", "capabilities": {"streaming": true, "tools": true} }
            ]
        }
    }"#,
    )
    .unwrap();

    apply_settings_providers(&mut cfg, &providers, None).expect("must succeed");

    let client = DefaultLlmClient::from_config(cfg).expect("client must build");
    let req = llm_runtime::LlmRequest::new("gpt-4o");
    let prepared = client.prepare(&req).await.expect("prepare must succeed");

    assert_eq!(prepared.provider_request.method, "POST");
    assert_eq!(
        prepared.provider_request.url, "https://api.openai.com/v1/responses",
        "URL must be {{baseUrl}}/responses"
    );

    let auth_header = prepared
        .provider_request
        .headers
        .get("Authorization")
        .expect("Authorization header must be present for ApiKey auth");
    assert_eq!(
        auth_header, "Bearer my-responses-key",
        "Authorization must be Bearer <key>"
    );
}

#[test]
fn openai_responses_websocket_settings_parse_capability_and_timeout() {
    let mut cfg = builtin_anthropic_config("https://api.anthropic.com", false);
    let providers: BTreeMap<String, serde_json::Value> = serde_json::from_str(
        r#"{
        "my-responses": {
            "type": "openai-responses",
            "baseUrl": "https://api.openai.com/v1",
            "apiKeyEnv": "OPENAI_API_KEY",
            "supportsWebsockets": true,
            "websocketConnectTimeoutMs": 2500,
            "models": [{ "id": "gpt-5" }]
        }
    }"#,
    )
    .unwrap();

    apply_settings_providers(&mut cfg, &providers, None).expect("must succeed");

    let profile = cfg
        .providers
        .iter()
        .find(|p| p.profile_name == "my-responses")
        .expect("profile");
    assert!(profile.supports_websockets);
    assert_eq!(profile.websocket_connect_timeout_ms, Some(2500));
}

#[test]
fn websocket_settings_reject_non_responses_provider() {
    let mut cfg = builtin_anthropic_config("https://api.anthropic.com", false);
    let providers: BTreeMap<String, serde_json::Value> = serde_json::from_str(
        r#"{
        "chat": {
            "type": "openai",
            "baseUrl": "https://api.openai.com/v1",
            "apiKeyEnv": "OPENAI_API_KEY",
            "supportsWebsockets": true,
            "models": [{ "id": "gpt-4o" }]
        }
    }"#,
    )
    .unwrap();

    let err = apply_settings_providers(&mut cfg, &providers, None).unwrap_err();
    assert!(
        matches!(err, LlmError::InvalidRequest { ref message } if message.contains("openai-responses")),
        "got: {err:?}"
    );
}

#[test]
fn websocket_settings_reject_bedrock_sigv4_provider() {
    let mut cfg = builtin_anthropic_config("https://api.anthropic.com", false);
    let providers: BTreeMap<String, serde_json::Value> = serde_json::from_str(
        r#"{
        "bedrock": {
            "type": "bedrock-claude",
            "region": "us-east-1",
            "supportsWebsockets": true,
            "models": [{ "id": "anthropic.claude-3-5-sonnet-20241022-v2:0" }]
        }
    }"#,
    )
    .unwrap();

    let err = apply_settings_providers(&mut cfg, &providers, None).unwrap_err();
    assert!(
        matches!(err, LlmError::InvalidRequest { ref message } if message.contains("AWS SigV4")),
        "got: {err:?}"
    );
}

#[test]
fn websocket_compression_setting_is_explicitly_rejected_until_supported() {
    let mut cfg = builtin_anthropic_config("https://api.anthropic.com", false);
    let providers: BTreeMap<String, serde_json::Value> = serde_json::from_str(
        r#"{
        "my-responses": {
            "type": "openai-responses",
            "baseUrl": "https://api.openai.com/v1",
            "apiKeyEnv": "OPENAI_API_KEY",
            "supportsWebsockets": true,
            "supportsWebsocketCompression": true,
            "models": [{ "id": "gpt-5" }]
        }
    }"#,
    )
    .unwrap();

    let err = apply_settings_providers(&mut cfg, &providers, None).unwrap_err();
    assert!(
        matches!(err, LlmError::InvalidRequest { ref message } if message.contains("not supported")),
        "got: {err:?}"
    );
}

/// The error message for unknown type includes `openai-responses`.
#[test]
fn unknown_type_mentions_openai_responses_in_supported_list() {
    let mut cfg = builtin_anthropic_config("https://api.anthropic.com", false);
    let providers: BTreeMap<String, serde_json::Value> = serde_json::from_str(
        r#"{
        "weird": {
            "type": "cohere-v4",
            "baseUrl": "https://api.cohere.ai",
            "apiKeyEnv": "COHERE_KEY",
            "models": [{ "id": "command-r" }]
        }
    }"#,
    )
    .unwrap();

    let err = apply_settings_providers(&mut cfg, &providers, None).unwrap_err();
    let LlmError::InvalidRequest { message } = err else {
        panic!("expected InvalidRequest, got something else");
    };
    assert!(
        message.contains("openai-responses"),
        "must list openai-responses; got: {message}"
    );
}

#[test]
fn supported_provider_kinds_parse_to_expected_protocols() {
    let mut cfg = builtin_anthropic_config("https://api.anthropic.com", false);
    let providers: BTreeMap<String, serde_json::Value> = serde_json::from_str(r#"{
        "openai-chat": {
            "type": "openai",
            "baseUrl": "https://api.openai.com/v1",
            "apiKeyEnv": "OPENAI_API_KEY",
            "models": [{ "id": "gpt-4o" }]
        },
        "openai-resp": {
            "type": "openai-responses",
            "baseUrl": "https://api.openai.com/v1",
            "apiKeyEnv": "OPENAI_API_KEY",
            "models": [{ "id": "gpt-4o" }]
        },
        "anthropic-api": {
            "type": "anthropic",
            "baseUrl": "https://api.anthropic.com",
            "apiKeyEnv": "ANTHROPIC_API_KEY",
            "models": [{ "id": "claude-sonnet-4-20250514" }]
        },
        "gemini-api": {
            "type": "gemini",
            "baseUrl": "https://generativelanguage.googleapis.com/v1beta",
            "apiKeyEnv": "GEMINI_API_KEY",
            "models": [{ "id": "gemini-2.5-pro" }]
        },
        "azure-api": {
            "type": "azure-openai",
            "baseUrl": "https://example.openai.azure.com/openai/deployments/gpt-4o",
            "apiKeyEnv": "AZURE_OPENAI_API_KEY",
            "apiVersion": "2024-02-01",
            "models": [{ "id": "gpt-4o" }]
        },
        "bedrock-api": {
            "type": "bedrock-claude",
            "region": "us-east-1",
            "models": [{ "id": "anthropic.claude-3-5-sonnet-20241022-v2:0" }]
        },
        "vertex-claude-api": {
            "type": "vertex-claude",
            "baseUrl": "https://us-central1-aiplatform.googleapis.com/v1/projects/p/locations/us-central1/publishers/anthropic/models/claude-sonnet-4:rawPredict",
            "apiKeyEnv": "VERTEX_TOKEN",
            "models": [{ "id": "claude-sonnet-4" }]
        },
        "vertex-gemini-api": {
            "type": "vertex-gemini",
            "baseUrl": "https://us-central1-aiplatform.googleapis.com/v1/projects/p/locations/us-central1/publishers/google/models/gemini-2.5-pro:generateContent",
            "apiKeyEnv": "VERTEX_TOKEN",
            "models": [{ "id": "gemini-2.5-pro" }]
        }
    }"#).unwrap();

    apply_settings_providers(&mut cfg, &providers, None).expect("providers parse");

    let by_name = |name: &str| {
        cfg.providers
            .iter()
            .find(|p| p.profile_name == name)
            .unwrap_or_else(|| panic!("missing provider {name}"))
    };
    assert_eq!(by_name("openai-chat").protocol, ProtocolFamily::OpenAiChat);
    assert_eq!(
        by_name("openai-resp").protocol,
        ProtocolFamily::OpenAiResponses
    );
    assert_eq!(
        by_name("anthropic-api").protocol,
        ProtocolFamily::AnthropicMessages
    );
    assert_eq!(
        by_name("gemini-api").protocol,
        ProtocolFamily::GeminiGenerateContent
    );
    assert_eq!(by_name("azure-api").protocol, ProtocolFamily::AzureOpenAi);
    assert_eq!(by_name("azure-api").auth, AuthStrategy::AzureToken);
    assert_eq!(by_name("azure-api").provider_id, ProviderId::AzureOpenAI);
    assert_eq!(
        by_name("azure-api")
            .azure
            .as_ref()
            .map(|c| c.api_version.as_str()),
        Some("2024-02-01")
    );
    assert_eq!(
        by_name("bedrock-api").protocol,
        ProtocolFamily::BedrockClaude
    );
    assert_eq!(by_name("bedrock-api").auth, AuthStrategy::AwsSigV4);
    assert_eq!(
        by_name("bedrock-api").provider_id,
        ProviderId::BedrockClaude
    );
    assert_eq!(
        by_name("bedrock-api")
            .signing
            .as_ref()
            .map(|s| (s.region.as_str(), s.service.as_str())),
        Some(("us-east-1", "bedrock"))
    );
    assert_eq!(
        by_name("vertex-claude-api").protocol,
        ProtocolFamily::VertexClaude
    );
    assert_eq!(by_name("vertex-claude-api").auth, AuthStrategy::GcpToken);
    assert_eq!(
        by_name("vertex-claude-api").provider_id,
        ProviderId::VertexClaude
    );
    assert_eq!(
        by_name("vertex-gemini-api").protocol,
        ProtocolFamily::VertexGemini
    );
    assert_eq!(by_name("vertex-gemini-api").auth, AuthStrategy::GcpToken);
    assert_eq!(
        by_name("vertex-gemini-api").provider_id,
        ProviderId::VertexGemini
    );
}
