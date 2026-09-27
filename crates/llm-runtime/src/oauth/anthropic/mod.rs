//! Claude.ai OAuth client and Anthropic auth-source resolver.
//!
//! See spec §30 (Anthropic Auth). This crate owns:
//! - PKCE (RFC 7636) verifier/challenge + CSRF state generation
//! - Loopback HTTP listener for the redirect URI
//! - Auth-source resolver covering the 9 documented sources (A6)
//! - Static config + a placeholder rate-limit tracker
//! - Reactive + proactive token refresh (M3-04)
//! - Scope upgrade flow (M3-04)
//!
//! Plan 3a/3b: `BearerToken`/`OAuthHookError`/`TokenHash` are owned locally
//! in `refresh.rs`; `RefreshDriver::refresh` is an inherent method. The
//! api-client crate has been removed from the workspace entirely.

#![forbid(unsafe_code)]

// `callback` uses `tokio::net::TcpListener` which tokio gates out under
// `--cfg loom`. Loom doesn't model network primitives anyway — the loom test
// only exercises the single-flight invariant on synthetic atomics — so we
// exclude this module from loom builds. Normal builds are unaffected.
pub mod api_key_helper;
#[cfg(not(loom))]
pub mod callback;
pub mod client;
pub mod config;
pub mod credential_provider;
pub mod handle;
pub mod limits;
pub mod pkce;
pub mod profile;
pub mod refresh;
pub mod resolver;
pub mod scope_upgrade;
pub mod subscription;

#[cfg(test)]
mod testsupport;

pub use api_key_helper::{
    api_key_helper_ttl_ms, fetch_api_key, fetch_api_key_result, resolve_ttl_ms, run_api_key_helper,
    run_api_key_helper_with_timeout, ApiKeyHelperCache, API_KEY_HELPER_TIMEOUT,
    API_KEY_HELPER_TTL_ENV, DEFAULT_API_KEY_HELPER_TTL_MS,
};
#[cfg(not(loom))]
pub use callback::{await_callback, CallbackError, CallbackListener, CallbackParams};
pub use client::{ClaudeAiOAuthClient, OAuthError};
pub use config::{ClaudeAiOAuthConfig, CLAUDE_CODE_OAUTH_SCOPES, REFRESH_GRANT_TYPE};
pub use credential_provider::OAuthCredentialProvider;
pub use handle::OAuthHandle;
pub use limits::{ClaudeAiLimitsState, ClaudeAiLimitsTracker, SubscriptionType};
pub use pkce::{generate_pkce, generate_state_token};
pub use profile::{
    fetch_profile_from_api_key, fetch_profile_from_oauth_token, fetch_user_roles, OAuthAccount,
    OAuthOrganization, OAuthProfileResponse, UserRolesResponse,
};
pub use refresh::{AuthState, RefreshDriver};
pub use resolver::{resolve, AuthSource, ResolverContext};
pub use scope_upgrade::{
    parse_scope_upgrade, run_scope_upgrade, PkceRunResult, PkceRunner, ScopeUpgradeRequired,
};
pub use subscription::{
    apply_profile, has_profile_scope, is_enterprise, is_subscriber_tier, publish_subscription,
    resolve_subscription_snapshot, subscription_from_scopes,
};
