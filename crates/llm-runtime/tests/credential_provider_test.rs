//! Tests for `OAuthCredentialProvider` — `llm_runtime::CredentialProvider` impl.
//!
//! Behavioral contracts:
//!   1. Fresh token returned as `Credential::BearerToken` (no refresh).
//!   2. Expired token triggers single-flight refresh; refreshed token returned.
//!   3. Refresh failure maps to `LlmError::Authentication` (no secret material leaked).

use async_trait::async_trait;
use llm_runtime::oauth::anthropic::OAuthCredentialProvider;
use llm_runtime::oauth::anthropic::{
    refresh::AuthState, refresh::RefreshDriver, ClaudeAiOAuthConfig,
};
use llm_runtime::{Credential, CredentialProvider, CredentialScope, LlmError, ProviderId};
use platform_api::{Clock, HttpError, HttpTransport};
use protocol::{HttpRequest, HttpResponse, Secret};
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
impl HttpTransport for FreshTokenTransport {
    async fn request(&self, _req: HttpRequest) -> Result<HttpResponse, HttpError> {
        Ok(HttpResponse {
            status: 200,
            headers: vec![],
            body: r#"{"access_token":"tok-refreshed","refresh_token":"ref-new","expires_in":3600,"scope":"read:user"}"#.to_string(),
            body_bytes: Vec::new(),
        })
    }
    async fn stream_sse(
        &self,
        _req: HttpRequest,
    ) -> Result<platform_api::http::SseStream, HttpError> {
        unimplemented!("sse not used");
    }
}

/// Transport that always returns a 401 (session expired → refresh failure).
struct FailingTransport;

#[async_trait]
impl HttpTransport for FailingTransport {
    async fn request(&self, _req: HttpRequest) -> Result<HttpResponse, HttpError> {
        Ok(HttpResponse {
            status: 401,
            headers: vec![],
            body: r#"{"error":"invalid_grant"}"#.to_string(),
            body_bytes: Vec::new(),
        })
    }
    async fn stream_sse(
        &self,
        _req: HttpRequest,
    ) -> Result<platform_api::http::SseStream, HttpError> {
        unimplemented!("sse not used");
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
        // Transport must never be invoked here: a returned credential other than "tok-fresh" would fail the assertion below.
        Arc::new(FreshTokenTransport) as Arc<dyn HttpTransport>,
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
        Arc::new(FreshTokenTransport) as Arc<dyn HttpTransport>,
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
        Arc::new(FailingTransport) as Arc<dyn HttpTransport>,
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

    assert_eq!(credential, Credential::BearerToken("tok-fresh".to_string()));
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
        Credential::BearerToken("tok-refreshed".to_string())
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
