//! Tests for `OAuthCredentialProvider` — `llm_runtime::CredentialProvider` impl.
//!
//! Behavioral contracts:
//!   1. Fresh token and scopes returned as `Credential::AnthropicOAuth` (no refresh).
//!   2. Expired token triggers single-flight refresh; refreshed token returned.
//!   3. Refresh failure maps to `LlmError::Authentication` (no secret material leaked).

use async_trait::async_trait;
use lingxi_core::host::Clock;
use lingxi_core::types::Secret;
use lingxi_llm_client::auth::oauth::anthropic::ClaudeAiOAuthConfig;
use lingxi_llm_client::{HttpRequest, StreamResponse, Transport};
use llm_runtime::auth::anthropic::OAuthCredentialProvider;
use llm_runtime::auth::anthropic::{refresh::AuthState, refresh::RefreshDriver};
use llm_runtime::{Credential, CredentialProvider, CredentialScope, LlmError, ProviderId};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

// ---------------------------------------------------------------------------
// Test doubles (defined locally — testsupport is crate-private)
// ---------------------------------------------------------------------------

/// Clock pinned to a fixed instant.
struct FixedClock(SystemTime);

impl Clock for FixedClock {
    fn now(&self) -> SystemTime {
        self.0
    }
}

/// Transport that always returns a fresh token response.
struct FreshTokenTransport;

#[async_trait]
impl Transport for FreshTokenTransport {
    async fn send(
        &self,
        _req: HttpRequest,
    ) -> Result<StreamResponse, lingxi_llm_client::protocol::LlmError> {
        Ok(StreamResponse {
            status: 200,
            headers: vec![],
            body: Box::pin(futures::stream::once(async {
                Ok(bytes::Bytes::from_static(br#"{"access_token":"tok-refreshed","refresh_token":"ref-new","expires_in":3600,"scope":"read:user"}"#))
            })),
        })
    }
}

/// Transport that always returns a 401 (session expired → refresh failure).
struct FailingTransport;

#[async_trait]
impl Transport for FailingTransport {
    async fn send(
        &self,
        _req: HttpRequest,
    ) -> Result<StreamResponse, lingxi_llm_client::protocol::LlmError> {
        Ok(StreamResponse {
            status: 401,
            headers: vec![],
            body: Box::pin(futures::stream::once(async {
                Ok(bytes::Bytes::from_static(br#"{"error":"invalid_grant"}"#))
            })),
        })
    }
}

// Clock pinned at t=1_000.  Tokens with `expires_at > EPOCH+1000` are fresh.
const CLOCK_NOW_SECS: u64 = 1_000;

/// Build an `Arc<RefreshDriver>` with a non-expired token ("tok-fresh").
fn fresh_driver() -> Arc<RefreshDriver> {
    let cfg = ClaudeAiOAuthConfig::default_with_port(0);
    let state = AuthState::new(
        cfg,
        Secret::new("tok-fresh".to_string()),
        Some(Secret::new("ref-fresh".to_string())),
        // Expires well in the future relative to CLOCK_NOW_SECS.
        SystemTime::UNIX_EPOCH + Duration::from_secs(CLOCK_NOW_SECS + 3_600),
        lingxi_llm_client::auth::oauth::anthropic::ClaudeAiOAuthConfig::default_with_port(0).scopes,
        // Transport must never be invoked here: a returned credential other than "tok-fresh" would fail the assertion below.
        Arc::new(FreshTokenTransport) as Arc<dyn Transport>,
        Arc::new(FixedClock(
            SystemTime::UNIX_EPOCH + Duration::from_secs(CLOCK_NOW_SECS),
        )) as Arc<dyn Clock>,
        None,
        None,
    );
    Arc::new(RefreshDriver::new(state))
}

/// Build an `Arc<RefreshDriver>` with an expired token + scripted refresh → "tok-refreshed".
fn expired_driver_ok() -> Arc<RefreshDriver> {
    let cfg = ClaudeAiOAuthConfig::default_with_port(0);
    let state = AuthState::new(
        cfg,
        Secret::new("tok-expired".to_string()),
        Some(Secret::new("ref-expired".to_string())),
        // Expired: `expires_at` is before the clock's `now`.
        SystemTime::UNIX_EPOCH + Duration::from_secs(CLOCK_NOW_SECS - 1),
        lingxi_llm_client::auth::oauth::anthropic::ClaudeAiOAuthConfig::default_with_port(0).scopes,
        Arc::new(FreshTokenTransport) as Arc<dyn Transport>,
        Arc::new(FixedClock(
            SystemTime::UNIX_EPOCH + Duration::from_secs(CLOCK_NOW_SECS),
        )) as Arc<dyn Clock>,
        None,
        None,
    );
    Arc::new(RefreshDriver::new(state))
}

/// Build an `Arc<RefreshDriver>` with an expired token + scripted refresh failure.
fn expired_driver_fail() -> Arc<RefreshDriver> {
    let cfg = ClaudeAiOAuthConfig::default_with_port(0);
    let state = AuthState::new(
        cfg,
        Secret::new("tok-expired".to_string()),
        Some(Secret::new("ref-expired".to_string())),
        SystemTime::UNIX_EPOCH + Duration::from_secs(CLOCK_NOW_SECS - 1),
        lingxi_llm_client::auth::oauth::anthropic::ClaudeAiOAuthConfig::default_with_port(0).scopes,
        Arc::new(FailingTransport) as Arc<dyn Transport>,
        Arc::new(FixedClock(
            SystemTime::UNIX_EPOCH + Duration::from_secs(CLOCK_NOW_SECS),
        )) as Arc<dyn Clock>,
        None,
        None,
    );
    Arc::new(RefreshDriver::new(state))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn load_returns_current_token_when_fresh() {
    let provider = OAuthCredentialProvider::new(fresh_driver());

    let credential = provider
        .load(&CredentialScope::new(
            ProviderId::AnthropicFirstParty,
            "anthropic",
        ))
        .await
        .expect("credential");

    assert_eq!(
        credential,
        Credential::AnthropicOAuth {
            access_token: "tok-fresh".into(),
            scopes: ClaudeAiOAuthConfig::default_with_port(0).scopes
        }
    );
}

#[tokio::test]
async fn granted_scopes_do_not_inherit_the_requested_default_scopes() {
    let state = AuthState::new(
        ClaudeAiOAuthConfig::default_with_port(0),
        Secret::new("restricted".into()),
        Some(Secret::new("refresh".into())),
        SystemTime::UNIX_EPOCH + Duration::from_secs(5000),
        vec!["user:inference".into()],
        Arc::new(FreshTokenTransport),
        Arc::new(FixedClock(
            SystemTime::UNIX_EPOCH + Duration::from_secs(CLOCK_NOW_SECS),
        )),
        None,
        None,
    );
    let provider = OAuthCredentialProvider::new(Arc::new(RefreshDriver::new(state)));
    let credential = provider
        .load(&CredentialScope::new(
            ProviderId::AnthropicFirstParty,
            "anthropic",
        ))
        .await
        .unwrap();
    assert_eq!(
        credential,
        Credential::AnthropicOAuth {
            access_token: "restricted".into(),
            scopes: vec!["user:inference".into()]
        }
    );
}

#[tokio::test]
async fn load_refreshes_expired_token_single_flight() {
    let provider = OAuthCredentialProvider::new(expired_driver_ok());

    let credential = provider
        .load(&CredentialScope::new(
            ProviderId::AnthropicFirstParty,
            "anthropic",
        ))
        .await
        .expect("credential");

    assert_eq!(
        credential,
        Credential::AnthropicOAuth {
            access_token: "tok-refreshed".into(),
            scopes: vec!["read:user".into()]
        }
    );
}

/// `FailingTransport` answers the refresh with `401 {"error":"invalid_grant"}`
/// — the IdP REJECTING the stored refresh token, which is the real dead-session
/// case. It must surface as [`LlmError::OAuthRefreshDead`] so the orchestrator
/// can render "Login expired" instead of the generic auth text.
///
/// This assertion changed deliberately on 2026-08-01: it previously expected
/// `Authentication`, back when `credential_provider` collapsed all three
/// `OAuthHookError` variants into one and a dead session was indistinguishable
/// from a transient network failure.
#[tokio::test]
async fn a_rejected_refresh_token_maps_to_the_dead_oauth_session_error() {
    let provider = OAuthCredentialProvider::new(expired_driver_fail());

    let err = provider
        .load(&CredentialScope::new(
            ProviderId::AnthropicFirstParty,
            "anthropic",
        ))
        .await
        .expect_err("must fail");

    assert!(
        matches!(err, LlmError::OAuthRefreshDead),
        "expected LlmError::OAuthRefreshDead, got {err:?}"
    );
}
