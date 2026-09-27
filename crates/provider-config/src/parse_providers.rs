//! Compatibility wrapper for parsing `settings.providers`.
//!
//! Provider semantics live in `llm-runtime`; this crate keeps the historical
//! tolerant API used by provider-config assembly and tests.

use std::collections::BTreeMap;

use llm_runtime::{parse_provider_profiles_lenient, ProviderCredentialMode, ProviderParseOptions};
use serde_json::Value;

pub use llm_runtime::ParsedUserProvider;

/// Parse `settings.providers` into profiles + warnings (spec section 5.1).
///
/// Invalid entries are skipped with warnings. API-key credentials are left
/// deferred so `assemble` can rewrite them to static host credential ids while
/// retaining the original `apiKeyEnv` metadata.
#[must_use]
pub fn parse_user_providers(
    providers: &BTreeMap<String, Value>,
) -> (Vec<ParsedUserProvider>, Vec<String>) {
    parse_provider_profiles_lenient(
        providers,
        ProviderParseOptions {
            credential_mode: ProviderCredentialMode::DeferredStatic,
            models_required: false,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_runtime::{AuthStrategy, CredentialConfig, ProtocolFamily, ProviderId};
    use serde_json::json;

    fn one(name: &str, value: Value) -> BTreeMap<String, Value> {
        let mut providers = BTreeMap::new();
        providers.insert(name.to_string(), value);
        providers
    }

    #[test]
    fn host_managed_key_does_not_require_environment_metadata() {
        let raw = one(
            "custom",
            json!({
                "type": "openai", "baseUrl": "https://example.com/v1",
                "models": [{"id": "model"}]
            }),
        );
        let (parsed, warnings) = parse_user_providers(&raw);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].env_var, None);
        assert_eq!(parsed[0].profile.credential, CredentialConfig::None);
    }

    #[test]
    fn wrapper_preserves_deferred_credential_and_env_metadata() {
        let raw = one(
            "groq",
            json!({
                "type": "openai",
                "baseUrl": "https://api.groq.com/openai/v1",
                "apiKeyEnv": "GROQ_API_KEY",
                "models": [{"id": "llama-3.3-70b"}]
            }),
        );

        let (parsed, warnings) = parse_user_providers(&raw);

        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
        assert_eq!(parsed.len(), 1);
        let p = &parsed[0];
        assert_eq!(p.profile.profile_name, "groq");
        assert_eq!(
            p.profile.provider_id,
            ProviderId::OpenAICompatible {
                name: "groq".to_string()
            }
        );
        assert_eq!(p.profile.protocol, ProtocolFamily::OpenAiChat);
        assert_eq!(p.profile.credential, CredentialConfig::None);
        assert_eq!(p.env_var.as_deref(), Some("GROQ_API_KEY"));
        assert_eq!(p.profile.models[0].request_model, "llama-3.3-70b");
    }

    #[test]
    fn wrapper_accepts_all_provider_kinds_with_llm_runtime_identities() {
        let raw: BTreeMap<String, Value> = serde_json::from_value(json!({
            "openai_chat": {
                "type": "openai",
                "baseUrl": "https://api.openai.com/v1",
                "apiKeyEnv": "OPENAI_API_KEY",
                "models": [{"id": "gpt-4o"}]
            },
            "openai_responses": {
                "type": "openai-responses",
                "baseUrl": "https://api.openai.com/v1",
                "apiKeyEnv": "OPENAI_API_KEY",
                "models": [{"id": "gpt-5"}]
            },
            "anthropic_user": {
                "type": "anthropic",
                "baseUrl": "https://api.anthropic.com",
                "apiKeyEnv": "ANTHROPIC_API_KEY",
                "models": [{"id": "claude-sonnet-4-20250514"}]
            },
            "gemini_user": {
                "type": "gemini",
                "baseUrl": "https://generativelanguage.googleapis.com/v1beta",
                "apiKeyEnv": "GEMINI_API_KEY",
                "models": [{"id": "gemini-2.5-pro"}]
            },
            "azure_user": {
                "type": "azure-openai",
                "baseUrl": "https://example.openai.azure.com",
                "apiKeyEnv": "AZURE_OPENAI_API_KEY",
                "apiVersion": "2024-02-01",
                "models": [{"id": "gpt-4o"}]
            },
            "bedrock_user": {
                "type": "bedrock-claude",
                "region": "us-east-1",
                "models": [{"id": "anthropic.claude-3-5-sonnet-20241022-v2:0"}]
            },
            "vertex_claude_user": {
                "type": "vertex-claude",
                "baseUrl": "https://us-central1-aiplatform.googleapis.com/v1/projects/p/locations/us-central1",
                "apiKeyEnv": "GOOGLE_OAUTH_TOKEN",
                "models": [{"id": "claude-sonnet-4@20250514"}]
            },
            "vertex_gemini_user": {
                "type": "vertex-gemini",
                "baseUrl": "https://us-central1-aiplatform.googleapis.com/v1/projects/p/locations/us-central1",
                "apiKeyEnv": "GOOGLE_OAUTH_TOKEN",
                "models": [{"id": "gemini-2.5-pro"}]
            }
        }))
        .unwrap();

        let (parsed, warnings) = parse_user_providers(&raw);

        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
        assert_eq!(parsed.len(), 8);
        let by_name: BTreeMap<_, _> = parsed
            .iter()
            .map(|p| (p.profile.profile_name.as_str(), &p.profile))
            .collect();
        assert_eq!(by_name["openai_chat"].protocol, ProtocolFamily::OpenAiChat);
        assert_eq!(
            by_name["openai_responses"].protocol,
            ProtocolFamily::OpenAiResponses
        );
        assert_eq!(
            by_name["anthropic_user"].provider_id,
            ProviderId::AnthropicFirstParty
        );
        assert_eq!(by_name["gemini_user"].provider_id, ProviderId::Gemini);
        assert_eq!(by_name["azure_user"].provider_id, ProviderId::AzureOpenAI);
        assert_eq!(
            by_name["bedrock_user"].provider_id,
            ProviderId::BedrockClaude
        );
        assert_eq!(
            by_name["vertex_claude_user"].provider_id,
            ProviderId::VertexClaude
        );
        assert_eq!(
            by_name["vertex_gemini_user"].provider_id,
            ProviderId::VertexGemini
        );
        assert_eq!(by_name["azure_user"].auth, AuthStrategy::AzureToken);
        assert_eq!(by_name["bedrock_user"].auth, AuthStrategy::AwsSigV4);
    }

    #[test]
    fn wrapper_warns_and_skips_bad_provider() {
        let raw = one("bad", json!("just-a-string"));

        let (parsed, warnings) = parse_user_providers(&raw);

        assert!(parsed.is_empty());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("not an object"));
        assert!(warnings[0].contains("skipped"));
    }

    #[test]
    fn model_less_entry_parses_with_no_models() {
        let raw = one(
            "listingonly",
            json!({
                "type": "openai",
                "baseUrl": "https://x",
                "apiKeyEnv": "OPENAI_API_KEY"
            }),
        );

        let (parsed, warnings) = parse_user_providers(&raw);

        assert!(warnings.is_empty());
        assert_eq!(parsed.len(), 1);
        assert!(parsed[0].profile.models.is_empty());
    }
}
