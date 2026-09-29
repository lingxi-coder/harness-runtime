//! Host Anthropic login, credential-source policy and refresh lifecycle.
//! Authorization, token and profile protocols are supplied by llm-client;
//! this module owns callbacks, secure storage, coordination and presentation.

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
