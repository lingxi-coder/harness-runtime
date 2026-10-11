use std::collections::BTreeMap;

use serde_json::{json, Value};

use super::{anthropic_models, apply_mobile_profile_allowlist};

#[test]
fn file_provider_settings_override_launch_defaults_and_enable_new_profiles() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let cwd = temp.path().join("project");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(cwd.join(branding::DOT_DIR)).unwrap();
    std::fs::write(
        home.join("settings.json"),
        json!({
            "providers": {"alpha": {"baseUrl": "https://user.example/v1"},
                "beta": {"type": "openai", "baseUrl": "https://beta.example/v1",
                    "models": [{"id": "model-b"}]}},
            "routing": {"aliases": {"selected": "beta/model-b"}}
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        cwd.join(branding::DOT_DIR).join("settings.json"),
        json!({
            "providers": {"alpha": {"baseUrl": "https://project.example/v1"}}
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        cwd.join(branding::DOT_DIR).join("settings.local.json"),
        json!({
            "providers": {"alpha": {"baseUrl": "https://local.example/v1"}}
        })
        .to_string(),
    )
    .unwrap();
    let cfg = super::MobileConfig {
        cwd,
        lingxi_home: home,
        provider_profiles: Some(BTreeMap::from([
            (
                "alpha".into(),
                json!({"type":"openai", "baseUrl":"https://legacy.example/v1",
                "models":[{"id":"model-a"}]}),
            ),
            ("legacy".into(), json!({"type":"openai"})),
        ])),
        routing: Some(json!({"mobileEnabledProfiles":["alpha"],
            "aliases":{"selected":"alpha/model-a", "retained":"alpha/model-a"}})),
        ..Default::default()
    };
    let settings = super::mobile_provider_settings(&cfg).unwrap();
    let profiles = settings.providers.unwrap();
    assert_eq!(profiles["alpha"]["baseUrl"], "https://local.example/v1");
    assert_eq!(profiles["alpha"]["models"][0]["id"], "model-a");
    assert!(profiles.contains_key("legacy"));
    let routing = settings.routing.unwrap();
    assert_eq!(routing["aliases"]["selected"], "beta/model-b");
    assert_eq!(routing["aliases"]["retained"], "alpha/model-a");
    assert_eq!(routing["mobileEnabledProfiles"], json!(["alpha", "beta"]));
    let mut assembled = provider_config::assemble(provider_config::AssembleInputs {
        anthropic_api_base: cfg.api_base,
        anthropic_models: anthropic_models(&cfg.default_model),
        anthropic_has_api_key: false,
        anthropic_has_oauth: false,
        user_providers: profiles,
        routing: Some(routing.clone()),
    });
    apply_mobile_profile_allowlist(&mut assembled, Some(&routing));
    assert!(assembled
        .client_config
        .providers
        .iter()
        .any(|p| p.profile_name == "beta"));
    assert_eq!(assembled.chains.aliases["selected"], "beta/model-b");
}

#[test]
fn explicit_file_allowlist_remains_authoritative() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(
        temp.path().join("settings.json"),
        json!({
            "providers": {"new": {"type": "openai"}},
            "routing": {"mobileEnabledProfiles": []}
        })
        .to_string(),
    )
    .unwrap();
    let cfg = super::MobileConfig {
        cwd: temp.path().join("project"),
        lingxi_home: temp.path().to_path_buf(),
        routing: Some(json!({"mobileEnabledProfiles": ["legacy"]})),
        ..Default::default()
    };
    let settings = super::mobile_provider_settings(&cfg).unwrap();
    assert_eq!(
        settings.routing.unwrap()["mobileEnabledProfiles"],
        json!([])
    );
}

fn assembled(routing: Option<Value>) -> provider_config::Assembled {
    let user_providers = BTreeMap::from([
        (
            "alpha".to_string(),
            json!({
                "type": "openai",
                "baseUrl": "https://alpha.example/v1",
                "apiKeyEnv": "ALPHA_API_KEY",
                "models": [{"id": "model-a"}]
            }),
        ),
        (
            "beta".to_string(),
            json!({
                "type": "openai",
                "baseUrl": "https://beta.example/v1",
                "apiKeyEnv": "BETA_API_KEY",
                "models": [{"id": "model-b"}]
            }),
        ),
    ]);
    provider_config::assemble(provider_config::AssembleInputs {
        anthropic_api_base: "https://api.anthropic.com".to_string(),
        anthropic_models: anthropic_models("claude-sonnet-4-20250514"),
        anthropic_has_api_key: false,
        anthropic_has_oauth: false,
        user_providers,
        routing,
    })
}

#[test]
fn absent_mobile_allowlist_preserves_full_catalog() {
    let mut assembled = assembled(None);
    let provider_count = assembled.client_config.providers.len();
    let credential_count = assembled.credential_sources.len();

    apply_mobile_profile_allowlist(&mut assembled, None);

    assert_eq!(provider_count, assembled.client_config.providers.len());
    assert_eq!(credential_count, assembled.credential_sources.len());
}

#[test]
fn explicit_empty_mobile_allowlist_filters_every_profile_and_route() {
    let routing = json!({
        "mobileEnabledProfiles": [],
        "fallback": {
            "primary": ["alpha/model-a", "beta/model-b"]
        }
    });
    let mut assembled = assembled(Some(routing.clone()));
    assert!(!assembled.client_config.providers.is_empty());
    assert!(!assembled.chains.chains.is_empty());

    apply_mobile_profile_allowlist(&mut assembled, Some(&routing));

    assert!(assembled.client_config.providers.is_empty());
    assert!(assembled.credential_sources.is_empty());
    assert!(assembled.chains.aliases.is_empty());
    assert!(assembled.chains.chains.is_empty());
}

#[test]
fn mobile_allowlist_filters_providers_credentials_aliases_and_fallbacks() {
    let routing = json!({
        "mobileEnabledProfiles": ["alpha"],
        "aliases": {
            "allowed": "alpha/model-a",
            "blocked": "beta/model-b"
        },
        "fallback": {
            "mixed": ["alpha/model-a", "beta/model-b"],
            "blocked": ["beta/model-b"]
        }
    });
    let mut assembled = assembled(Some(routing.clone()));

    apply_mobile_profile_allowlist(&mut assembled, Some(&routing));

    let profiles: Vec<_> = assembled
        .client_config
        .providers
        .iter()
        .map(|provider| provider.profile_name.as_str())
        .collect();
    assert_eq!(vec!["alpha"], profiles);
    assert_eq!(1, assembled.credential_sources.len());
    assert_eq!("alpha", assembled.credential_sources[0].profile_name);
    assert_eq!(
        Some(&"alpha/model-a".to_string()),
        assembled.chains.aliases.get("allowed")
    );
    assert!(!assembled.chains.aliases.contains_key("blocked"));
    assert_eq!(1, assembled.chains.chains["mixed"].len());
    assert!(!assembled.chains.chains.contains_key("blocked"));
}
