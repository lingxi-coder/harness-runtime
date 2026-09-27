//! Shared Plan 3c types: cross-provider fallback chains, retry overrides,
//! per-profile credential sources, and the `assemble` input/output bundles.
//! Spec §5.2 / §5.3.

use std::collections::BTreeMap;

use llm_runtime::{ClientConfig, ProviderId};

/// Per-model cross-provider failover entry (one hop in a `ChainConfig` chain).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainEntry {
    /// Resolved provider identity for this hop.
    pub provider_id: ProviderId,
    /// Provider-local model id sent on the wire for this hop.
    pub model: String,
}

/// Retry budget override parsed from `settings.routing.retry`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryOverride {
    /// Max total attempts per chain entry (clamped to >= 1 at parse time).
    pub max_attempts: u32,
    /// Base linear backoff in milliseconds (driver computes its own backoff;
    /// retained for parity / future custom-delay wiring).
    pub backoff_ms: u64,
}

/// Parsed routing config: aliases, fallback chains, and the retry override.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChainConfig {
    /// `alias -> "provider/model"` (folded into model aliases by `assemble`).
    pub aliases: BTreeMap<String, String>,
    /// `key -> ordered failover entries` (`key` is an alias or `"provider/model"`).
    pub chains: BTreeMap<String, Vec<ChainEntry>>,
    /// Optional per-entry retry budget override.
    pub retry: Option<RetryOverride>,
}

/// Credential kind recorded per profile in `Assembled.credential_sources`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialKind {
    /// Provider API key (keychain or env).
    ApiKey,
    /// Anthropic OAuth (delegated to the host OAuth provider).
    OAuth,
    /// Keychain-only (no env fallback).
    Keychain,
}

/// One profile's resolved credential source (keychain id + env fallback).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialSource {
    /// Provider identity this source serves.
    pub provider_id: ProviderId,
    /// Human profile name (== `ProviderProfile::profile_name`); the availability
    /// map keys by this (spec §6.2).
    pub profile_name: String,
    /// Keychain key id (== the profile's `CredentialConfig::Static{id}`).
    pub credential_id: String,
    /// Env fallback var (from `apiKeyEnv` / preset default), when any.
    pub env_var: Option<String>,
    /// Credential kind for this profile.
    pub kind: CredentialKind,
}

/// Inputs to `assemble` (spec §5.3). Carries the Anthropic 3-way auth state +
/// the raw settings JSON the engine already holds.
#[derive(Debug, Clone)]
pub struct AssembleInputs {
    /// Anthropic base URL (`cfg.api_base`).
    pub anthropic_api_base: String,
    /// Anthropic model ids to declare on the Anthropic profile (pre-resolved by
    /// the caller via `anthropic_models_for`, including the host fallback model).
    pub anthropic_models: Vec<llm_runtime::ModelProfile>,
    /// Whether an Anthropic API key is configured (api-key wins over OAuth).
    pub anthropic_has_api_key: bool,
    /// Whether an Anthropic OAuth session is available (only when no api key).
    pub anthropic_has_oauth: bool,
    /// Raw `settings.providers` block (`cfg.provider_profiles`).
    pub user_providers: BTreeMap<String, serde_json::Value>,
    /// Raw `settings.routing` block (`cfg.routing`).
    pub routing: Option<serde_json::Value>,
}

/// Result of `assemble` (spec §5.3).
//
// NOTE (plan deviation): the plan derived `Debug` here, but `cost::PricingCatalog`
// does not implement `Debug`, so deriving it does not compile. Dropped the derive
// (minimal fix; the `cost` crate is out of scope for this types-only task).
pub struct Assembled {
    /// Merged provider profiles (anthropic + presets + user) ready for the client.
    pub client_config: ClientConfig,
    /// Cost pricing catalog (Anthropic/OpenAI/Gemini reference tiers + non-Anthropic
    /// rows), ready to wrap in `Arc` and pass to `cost::CostTracker::new`.
    pub pricing: cost::PricingCatalog,
    /// Parsed + validated routing chains.
    pub chains: ChainConfig,
    /// Per-profile credential sources for the composite provider + availability.
    pub credential_sources: Vec<CredentialSource>,
    /// Non-fatal parse/merge warnings (surfaced via tracing by the engine).
    pub warnings: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chain_config_default_is_empty() {
        let c = ChainConfig::default();
        assert!(c.aliases.is_empty());
        assert!(c.chains.is_empty());
        assert!(c.retry.is_none());
    }

    #[test]
    fn chain_entry_carries_provider_and_model() {
        let e = ChainEntry {
            provider_id: ProviderId::OpenAICompatible {
                name: "deepseek".to_string(),
            },
            model: "deepseek-chat".to_string(),
        };
        assert_eq!(e.model, "deepseek-chat");
        assert_eq!(
            e.provider_id,
            ProviderId::OpenAICompatible {
                name: "deepseek".to_string()
            }
        );
    }

    #[test]
    fn credential_source_records_profile_and_env_fallback() {
        let s = CredentialSource {
            provider_id: ProviderId::OpenAICompatible {
                name: "openrouter".to_string(),
            },
            profile_name: "openrouter".to_string(),
            credential_id: "openrouter".to_string(),
            env_var: Some("OPENROUTER_API_KEY".to_string()),
            kind: CredentialKind::ApiKey,
        };
        assert_eq!(s.profile_name, "openrouter");
        assert_eq!(s.env_var.as_deref(), Some("OPENROUTER_API_KEY"));
        assert_eq!(s.kind, CredentialKind::ApiKey);
    }
}
