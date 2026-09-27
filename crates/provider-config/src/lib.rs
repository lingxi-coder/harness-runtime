//! Plan 3c — settings→config assembly for multi-provider live routing.
//!
//! Pure, no-I/O parsing + assembly of `settings.providers` / `settings.routing`
//! into an `llm_runtime::ClientConfig` + cross-provider fallback `ChainConfig` +
//! a composite credential provider. See
//! `docs/superpowers/specs/2026-06-15-llm-runtime-plan3c-multi-provider-routing-design.md`.

#![forbid(unsafe_code)]

pub mod assemble;
mod availability;
mod cost_translate;
mod credentials;
pub mod parse_providers;
pub mod parse_routing;
pub mod types;

pub use assemble::{assemble, assemble_for_region};
pub use availability::{
    compute_availability, compute_availability_with_isolation, ProviderAvailability,
};
pub use credentials::MultiCredentialProvider;
pub use parse_providers::{parse_user_providers, ParsedUserProvider};
pub use parse_routing::parse_routing;
pub use types::{
    AssembleInputs, Assembled, ChainConfig, ChainEntry, CredentialKind, CredentialSource,
    RetryOverride,
};

#[cfg(test)]
mod smoke_tests {
    #[test]
    fn links_llm_runtime() {
        let cat = llm_runtime::builtin_presets();
        assert!(cat.providers.iter().any(|p| p.profile_name == "openai"));
        assert!(cat
            .providers
            .iter()
            .any(|p| p.profile_name == "openai-chatgpt"));
        assert!(cat.providers.iter().all(|p| p.wire_profile.is_some()));
    }
}
