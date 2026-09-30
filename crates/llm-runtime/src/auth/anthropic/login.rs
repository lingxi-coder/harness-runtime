//! Host Anthropic login adapter: SDK authentication plus stored credentials and clock.

use crate::auth::anthropic::refresh::{AuthState, RefreshDriver};
use lingxi_core::host::Clock;
use lingxi_core::types::Secret;
use lingxi_llm_client::auth::oauth::anthropic as sdk;
use lingxi_llm_client::auth::oauth::anthropic::ClaudeAiOAuthConfig;
use lingxi_llm_client::transport::Transport;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use thiserror::Error;

/// OAuth-flow failures.
#[derive(Debug, Error)]
pub enum OAuthError {
    /// Loopback callback failed (bind, parse, or state mismatch).
    #[error("callback failed: {0}")]
    Callback(String),
    /// Token exchange against the `IdP` failed.
    #[error("token exchange failed: {0}")]
    TokenExchange(String),
    /// Stored `refresh_token` has expired or was revoked. User must re-authenticate.
    ///
    /// OAUTHREF.5: this Display string is a LingXi-specific message, NOT a
    /// claude-code byte-for-byte port (an earlier comment wrongly claimed it
    /// was). claude-code's refresh path throws a generic
    /// `Token refresh failed: ${statusText}` (services/oauth/client.ts) and has
    /// no "Session expired" / re-auth string anywhere. This string also serves
    /// as a control-flow key in the proactive-refresh loop (`refresh.rs`), which
    /// is itself a LingXi-only redesign with no TS counterpart — so the string
    /// is pinned as a LingXi-side value, not as a TS-parity target.
    #[error("Session expired. Re-authenticate?")]
    RefreshExpired,
    /// Scope upgrade attempt was denied by the provider.
    #[error("Scope upgrade denied by provider")]
    ScopeRejected {
        /// Scopes the provider required.
        required: Vec<String>,
        /// Scopes the token currently holds.
        granted: Vec<String>,
    },
    /// Proactive refresh task failed and is shutting down.
    #[error("proactive refresh failed: {source}")]
    ProactiveFailed {
        /// The underlying error that caused the proactive task to fail.
        source: Box<OAuthError>,
    },
}

/// Tokens converted to host secrets and wall-clock expiry for storage.
#[derive(Debug)]
pub struct ExchangedTokens {
    pub access_token: Secret<String>,
    pub refresh_token: Option<Secret<String>>,
    pub expires_at: SystemTime,
    pub scopes: Vec<String>,
    pub account: Option<sdk::ExchangeAccount>,
    pub organization: Option<sdk::ExchangeOrganization>,
}

/// Apply host clock and secret policy to the SDK token response.
pub fn prepare_exchanged_tokens(
    parsed: sdk::ExchangeResponse,
    config: &ClaudeAiOAuthConfig,
    clock: &dyn Clock,
) -> ExchangedTokens {
    let scopes = parsed.granted_scopes(config);
    ExchangedTokens {
        access_token: Secret::new(parsed.access_token),
        refresh_token: parsed.refresh_token.map(Secret::new),
        expires_at: clock.now() + Duration::from_secs(parsed.expires_in),
        scopes,
        account: parsed.account,
        organization: parsed.organization,
    }
}

/// Construct the Anthropic refresh state and spawn its proactive task.
///
/// Returns the `Arc<AuthState>` so the engine can call `shutdown()` at
/// `Engine::shutdown` time. Each call creates a separate state and task;
/// callers should initialize only one driver for each credential lifecycle.
///
/// # Errors
/// * [`OAuthError::TokenExchange`] if the runtime spawner fails to spawn.
#[allow(clippy::too_many_arguments)]
pub async fn init_refresh_driver(
    config: ClaudeAiOAuthConfig,
    access_token: Secret<String>,
    refresh_token: Option<Secret<String>>,
    expires_at: SystemTime,
    http: Arc<dyn Transport>,
    clock: Arc<dyn lingxi_core::host::Clock>,
    bus: Option<Arc<telemetry::AnalyticsBus>>,
    credentials: Option<Arc<secret::CredentialManager>>,
    spawner: Arc<dyn lingxi_core::host::RuntimeSpawner>,
) -> Result<Arc<AuthState>, OAuthError> {
    let state = AuthState::new(
        config,
        access_token,
        refresh_token,
        expires_at,
        http,
        clock,
        bus,
        credentials,
    );
    // Spawn the proactive refresh loop for this state.
    RefreshDriver::spawn_proactive(state.clone(), spawner)
        .await
        .map_err(|e| OAuthError::TokenExchange(format!("spawn_proactive: {e}")))?;
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::anthropic::testsupport::TestClock;

    #[test]
    fn sdk_tokens_use_host_clock_and_default_scopes() {
        let config = ClaudeAiOAuthConfig::default_with_port(45321);
        let response: sdk::ExchangeResponse =
            serde_json::from_str(r#"{"access_token":"acc","expires_in":3600}"#).unwrap();
        let tokens = prepare_exchanged_tokens(response, &config, TestClock::new(1_000).as_ref());
        assert_eq!(tokens.access_token.expose_secret(), "acc");
        assert!(tokens.refresh_token.is_none());
        assert_eq!(tokens.scopes, config.scopes);
        assert_eq!(
            tokens.expires_at,
            SystemTime::UNIX_EPOCH + Duration::from_secs(4_600)
        );
    }
}
