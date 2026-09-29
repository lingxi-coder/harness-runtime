//! OAuth scope upgrade: 403-with-`required_scopes` re-triggers PKCE preserving
//! the existing `refresh_token`. See spec §3 / §8 M3-04 phase 4.
//!
//! On success: atomically swap `access_token` + `refresh_token` + `scopes`.
//! On failure: leave the existing `TokenInfo` untouched (the user isn't logged
//! out — the next API call retries with the still-valid `refresh_token`).

use crate::auth::anthropic::login::OAuthError;
use crate::auth::anthropic::refresh::{AuthState, TokenInfo};
use async_trait::async_trait;
use lingxi_llm_client::auth::oauth::anthropic::ScopeUpgradeRequired;
use protocol::Secret;
use std::sync::Arc;
use std::time::SystemTime;

/// Result of running a PKCE flow: a complete `TokenInfo` ready to swap into
/// `AuthState`.
#[derive(Debug)]
pub struct PkceRunResult {
    /// Fresh `access_token` from the `IdP`.
    pub access_token: Secret<String>,
    /// Fresh `refresh_token` from the `IdP`. Always rotated on a successful PKCE.
    pub refresh_token: Secret<String>,
    /// Scopes granted by the new token.
    pub scopes: Vec<String>,
    /// Expiry instant (wall clock).
    pub expires_at: SystemTime,
}

/// Abstraction over the PKCE flow. Production uses
/// `BrowserOpeningPkceRunner`; tests inject mocks.
#[async_trait]
pub trait PkceRunner: Send + Sync {
    /// Run a PKCE flow against the IdP and return the fresh tokens, or an
    /// `OAuthError` if the flow fails (e.g. user closed browser).
    async fn run_pkce(
        &self,
        config: &lingxi_llm_client::auth::oauth::anthropic::ClaudeAiOAuthConfig,
        required_scopes: &[String],
    ) -> Result<PkceRunResult, OAuthError>;
}

/// Run a scope upgrade. Acquires `refresh_lock` (the same lock the reactive
/// path uses), runs PKCE, and atomically swaps `TokenInfo` on success.
///
/// On `Err(OAuthError::ScopeRejected)` (PKCE failed), the old `TokenInfo` is
/// preserved — the user is not logged out.
pub async fn run_scope_upgrade(
    state: &Arc<AuthState>,
    upgrade: ScopeUpgradeRequired,
    pkce_runner: &dyn PkceRunner,
) -> Result<(), OAuthError> {
    // Acquire the same lock the reactive refresh path uses so we serialize
    // against concurrent refreshes.
    let _guard = state.refresh_lock.lock().await;

    emit_scope_upgrade_started(&state.bus).await;

    // Run PKCE with the union of currently-granted + newly-required scopes.
    let mut requested = upgrade.granted.clone();
    for s in &upgrade.required {
        if !requested.contains(s) {
            requested.push(s.clone());
        }
    }
    let result = match pkce_runner.run_pkce(&state.config, &requested).await {
        Ok(r) => r,
        Err(e) => {
            emit_refresh_failed_scope_rejected(&state.bus).await;
            tracing::warn!(
                target: "lingxi::anthropic_oauth::scope_upgrade",
                error = ?e,
                "PKCE failed during scope upgrade; preserving existing token",
            );
            return Err(OAuthError::ScopeRejected {
                required: upgrade.required,
                granted: upgrade.granted,
            });
        }
    };

    // Atomically swap TokenInfo.
    let granted_count = i64::try_from(result.scopes.len()).unwrap_or(i64::MAX);
    let required_count = i64::try_from(upgrade.required.len()).unwrap_or(i64::MAX);
    let new_info = TokenInfo {
        access_token: result.access_token,
        refresh_token: Some(result.refresh_token),
        expires_at: result.expires_at,
        scopes: result.scopes,
    };
    {
        let mut guard = state.token.write().await;
        *guard = new_info;
    }

    emit_scope_upgraded(&state.bus, granted_count, required_count).await;
    Ok(())
}

async fn emit_scope_upgrade_started(bus: &Option<Arc<telemetry::AnalyticsBus>>) {
    let Some(bus) = bus else { return };
    let mut m = telemetry::sink::LogEventMetadata::new();
    m.insert(
        "trigger".into(),
        telemetry::sink::AnalyticsValue::String(
            telemetry::Verified::assert_safe("scope_upgrade".to_string())
                .as_str()
                .to_string(),
        ),
    );
    bus.log_event("tengu_oauth_refresh_started", m).await;
}

async fn emit_scope_upgraded(
    bus: &Option<Arc<telemetry::AnalyticsBus>>,
    granted_count: i64,
    required_count: i64,
) {
    let Some(bus) = bus else { return };
    let mut m = telemetry::sink::LogEventMetadata::new();
    m.insert(
        "granted_count".into(),
        telemetry::sink::AnalyticsValue::Int(granted_count),
    );
    m.insert(
        "required_count".into(),
        telemetry::sink::AnalyticsValue::Int(required_count),
    );
    bus.log_event("tengu_oauth_scope_upgraded", m).await;
}

async fn emit_refresh_failed_scope_rejected(bus: &Option<Arc<telemetry::AnalyticsBus>>) {
    let Some(bus) = bus else { return };
    let mut m = telemetry::sink::LogEventMetadata::new();
    m.insert(
        "trigger".into(),
        telemetry::sink::AnalyticsValue::String(
            telemetry::Verified::assert_safe("scope_upgrade".to_string())
                .as_str()
                .to_string(),
        ),
    );
    m.insert(
        "error_kind".into(),
        telemetry::sink::AnalyticsValue::String(
            telemetry::Verified::assert_safe("scope_rejected".to_string())
                .as_str()
                .to_string(),
        ),
    );
    bus.log_event("tengu_oauth_refresh_failed", m).await;
}

// ---- Test-only PKCE runners ----

/// Test runner that always returns `OAuthError::Callback` (simulates user
/// closing the browser or any callback-handshake failure).
#[doc(hidden)]
pub struct PkceFailingRunner;

#[async_trait]
impl PkceRunner for PkceFailingRunner {
    async fn run_pkce(
        &self,
        _config: &lingxi_llm_client::auth::oauth::anthropic::ClaudeAiOAuthConfig,
        _required_scopes: &[String],
    ) -> Result<PkceRunResult, OAuthError> {
        Err(OAuthError::Callback("user closed browser".into()))
    }
}

/// Test runner that returns fixed tokens + scopes for a successful upgrade.
#[doc(hidden)]
pub struct PkceFakeSuccessRunner {
    access_token: String,
    refresh_token: String,
    scopes: Vec<String>,
    expires_at: SystemTime,
}

impl PkceFakeSuccessRunner {
    #[must_use]
    pub fn new(
        access_token: &str,
        refresh_token: &str,
        scopes: Vec<String>,
        expires_at: SystemTime,
    ) -> Self {
        Self {
            access_token: access_token.into(),
            refresh_token: refresh_token.into(),
            scopes,
            expires_at,
        }
    }
}

#[async_trait]
impl PkceRunner for PkceFakeSuccessRunner {
    async fn run_pkce(
        &self,
        _config: &lingxi_llm_client::auth::oauth::anthropic::ClaudeAiOAuthConfig,
        _required_scopes: &[String],
    ) -> Result<PkceRunResult, OAuthError> {
        Ok(PkceRunResult {
            access_token: Secret::new(self.access_token.clone()),
            refresh_token: Secret::new(self.refresh_token.clone()),
            scopes: self.scopes.clone(),
            expires_at: self.expires_at,
        })
    }
}
