//! Host state and time conversion for OpenAI OAuth login.

use crate::auth::openai::refresh::{AuthState, RefreshDriver};
use lingxi_llm_client::auth::oauth::openai::{self as sdk, OpenAiOAuthConfig};
use platform_api::Clock;
use protocol::Secret;
use std::sync::Arc;
use std::time::SystemTime;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum OAuthError {
    #[error("callback failed: {0}")]
    Callback(String),
    #[error("token exchange failed: {0}")]
    TokenExchange(String),
    #[error("Session expired. Re-authenticate?")]
    RefreshExpired,
    #[error("Scope upgrade denied by provider")]
    ScopeRejected {
        required: Vec<String>,
        granted: Vec<String>,
    },
    #[error("proactive refresh failed: {source}")]
    ProactiveFailed { source: Box<OAuthError> },
    #[error("device code login failed: {0}")]
    DeviceCode(String),
}

#[derive(Debug)]
pub struct LoginTokens {
    pub access_token: Secret<String>,
    pub refresh_token: Option<Secret<String>>,
    pub id_token: Option<String>,
    pub expires_at: SystemTime,
}

/// Apply the host clock after the SDK has parsed the token endpoint response.
pub(crate) fn into_login_tokens(token: sdk::ExchangedTokens, clock: &dyn Clock) -> LoginTokens {
    let expires_at = clock.now() + token.effective_lifetime();
    LoginTokens {
        access_token: Secret::new(token.access_token),
        refresh_token: token.refresh_token.map(Secret::new),
        id_token: token.id_token,
        expires_at,
    }
}

/// Preserve the existing user-facing authorization-code rejection message.
pub(crate) fn exchange_error(error: sdk::OAuthProtocolError) -> OAuthError {
    match error {
        sdk::OAuthProtocolError::Status(401) => {
            OAuthError::TokenExchange("Authentication failed: Invalid authorization code".into())
        }
        other => OAuthError::TokenExchange(other.to_string()),
    }
}

/// Wire the OAuth refresh subsystem and spawn the proactive task.
#[allow(clippy::too_many_arguments)]
pub async fn init_refresh_driver(
    config: OpenAiOAuthConfig,
    access_token: Secret<String>,
    refresh_token: Option<Secret<String>>,
    expires_at: SystemTime,
    account_id: Option<String>,
    fedramp: bool,
    email: Option<String>,
    http: Arc<dyn lingxi_llm_client::Transport>,
    clock: Arc<dyn Clock>,
    bus: Option<Arc<telemetry::AnalyticsBus>>,
    credentials: Option<Arc<secret::CredentialManager>>,
    spawner: Arc<dyn platform_api::RuntimeSpawner>,
) -> Result<Arc<AuthState>, OAuthError> {
    let state = AuthState::new(
        config,
        access_token,
        refresh_token,
        expires_at,
        account_id,
        fedramp,
        email,
        http,
        clock,
        bus,
        credentials,
    );
    RefreshDriver::spawn_proactive(state.clone(), spawner)
        .await
        .map_err(|e| OAuthError::TokenExchange(format!("spawn_proactive: {e}")))?;
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::openai::testsupport::TestClock;
    use std::time::Duration;

    #[test]
    fn sdk_default_expiry_is_projected_onto_host_clock() {
        let tokens = into_login_tokens(
            sdk::ExchangedTokens {
                id_token: None,
                access_token: "access".into(),
                refresh_token: Some("refresh".into()),
                expires_in: 0,
            },
            TestClock::new(1_000).as_ref(),
        );
        assert_eq!(
            tokens.expires_at,
            SystemTime::UNIX_EPOCH + Duration::from_secs(4_600)
        );
        assert_eq!(tokens.access_token.expose_secret(), "access");
    }

    #[test]
    fn unauthorized_code_keeps_existing_message() {
        assert_eq!(
            exchange_error(sdk::OAuthProtocolError::Status(401)).to_string(),
            "token exchange failed: Authentication failed: Invalid authorization code"
        );
    }
}
