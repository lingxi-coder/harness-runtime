//! Regression: the latest GPT-6 / GPT-5.6 models are reachable through the
//! openai-chatgpt (ChatGPT OAuth/Codex-backend) profile even though the same
//! ids are also exposed by the OpenAI API and Copilot profiles. Profile-qualified
//! resolution must remain deterministic; bare shared ids remain ambiguous.

use llm_runtime::{builtin_presets, ClientConfig, ModelRegistry};

fn registry() -> ModelRegistry {
    let cat = builtin_presets();
    ModelRegistry::from_config(ClientConfig {
        providers: cat.providers,
    })
    .expect("registry from builtin presets")
}

#[test]
fn latest_models_resolve_to_openai_chatgpt_by_profile() {
    let reg = registry();
    for id in [
        "gpt-6-astra",
        "gpt-5.6-sol",
        "gpt-5.6-terra",
        "gpt-5.6-luna",
    ] {
        let route = reg
            .resolve_in(id, Some("openai-chatgpt"))
            .unwrap_or_else(|e| panic!("'{id}' must resolve, got: {e:?}"));
        assert_eq!(
            route.profile_name, "openai-chatgpt",
            "'{id}' must route to the ChatGPT-login profile"
        );
    }
}

#[test]
fn latest_models_are_present_in_chatgpt_catalog() {
    let cat = builtin_presets();
    let profile = cat
        .providers
        .iter()
        .find(|p| p.profile_name == "openai-chatgpt")
        .expect("ChatGPT profile");
    for id in [
        "gpt-6-astra",
        "gpt-5.6-sol",
        "gpt-5.6-terra",
        "gpt-5.6-luna",
    ] {
        assert!(
            profile.models.iter().any(|model| model.request_model == id),
            "'{id}' must be present in ChatGPT catalog"
        );
    }
}
