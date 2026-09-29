//! What happens when a settings provider is named like a built-in preset.
//!
//! `assemble` pushes every catalog preset and then appends every user provider
//! with no name check, while `ModelRuntime::from_config` rejects a repeated
//! `profile_name` outright. This pins which of the two wins, because the mobile
//! clients decide whether to emit a provider entry based on the answer.

use provider_config::{assemble, AssembleInputs};
use serde_json::json;
use std::collections::BTreeMap;

fn inputs() -> AssembleInputs {
    AssembleInputs {
        anthropic_api_base: "https://api.anthropic.com".to_string(),
        anthropic_models: vec![llm_runtime::ModelProfile {
            display_model: "claude-opus-4-6".to_string(),
            request_model: "claude-opus-4-6".to_string(),
            billing_model: "claude-opus-4-6".to_string(),
            aliases: Vec::new(),
            description: None,
            metadata: Default::default(),
            capabilities: llm_runtime::Capabilities::default(),
        }],
        anthropic_has_api_key: true,
        anthropic_has_oauth: false,
        user_providers: BTreeMap::new(),
        routing: None,
    }
}

#[test]
fn a_user_provider_named_like_a_preset_is_not_silently_dropped() {
    let mut inp = inputs();
    inp.user_providers.insert(
        "deepseek".to_string(),
        json!({
            "type": "openai",
            "baseUrl": "https://api.deepseek.com",
            "apiKeyEnv": "DEEPSEEK_API_KEY",
            "models": ["deepseek-flash"],
        }),
    );

    let out = assemble(inp);
    let deepseek: Vec<String> = out
        .client_config
        .providers
        .iter()
        .filter(|p| p.profile_name == "deepseek")
        .map(|p| p.base_url.clone())
        .collect();

    println!(
        "profiles named deepseek: {} -> base urls {:?}",
        deepseek.len(),
        deepseek
    );
    // The consequence, not just the count: this config is what the engine hands
    // to the client, and a repeated profile_name fails the WHOLE build — every
    // provider becomes unusable, not just this one.
    let built = llm_runtime::ModelRuntime::from_config(out.client_config);
    match &built {
        Ok(_) => {}
        Err(error) => println!("from_config rejected the assembled config: {error}"),
    }
    assert!(
        built.is_ok(),
        "a settings entry named like a preset must not brick the client"
    );
    assert_eq!(
        deepseek.len(),
        1,
        "a settings entry must REPLACE the preset of the same name, not sit beside it"
    );

    // The credential id is what the keychain lookup is keyed by. If overriding a
    // preset changed it, every key already stored for that provider would stop
    // being found — an auth failure with no configuration change in sight.
    let sources: Vec<_> = out
        .credential_sources
        .iter()
        .filter(|source| source.profile_name == "deepseek")
        .collect();
    println!(
        "credential sources for deepseek: {:?}",
        sources
            .iter()
            .map(|source| (source.credential_id.clone(), source.env_var.clone()))
            .collect::<Vec<_>>()
    );
    assert_eq!(sources.len(), 1, "exactly one credential slot per profile");
    assert_eq!(
        sources[0].credential_id, "deepseek",
        "overriding a preset must keep the credential id the preset used, or every \
         key already stored under it stops resolving"
    );
}
