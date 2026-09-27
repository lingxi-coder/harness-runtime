//! `crate::CredentialProvider` over the `OpenAI` OAuth refresh machinery.
//!
//! [`OpenAiOAuthCredentialProvider`] serves the current OAuth access token,
//! refreshing in place (single-flight via the underlying `refresh_lock`)
//! when the token is expired per the state's clock.
//!
//! Returns `Credential::ChatGptOAuth { access_token, account_id, fedramp }`
//! (not `Credential::BearerToken`), carrying the ChatGPT-Account-ID and
//! `FedRAMP` flag alongside the access token so the HTTP adapter can set the
//! correct request headers.

use std::fmt;
use std::sync::Arc;

use crate::{BoxFuture, Credential, CredentialProvider, CredentialScope, LlmError};

use crate::oauth::openai::refresh::{OAuthHookError, RefreshDriver};

/// Which [`LlmError`] a failed refresh becomes — the `OpenAI` twin of
/// `oauth::anthropic::credential_provider::llm_error_for`.
///
/// Deliberately duplicated rather than shared: the two modules own SEPARATE
/// `OAuthHookError` types, and coupling them through a common trait would make
/// a change to one provider's refresh contract silently reinterpret the
/// other's. The variant meanings, not the type, are what must agree.
///
/// Only an IdP that REJECTED the refresh token is a dead session. A stale token
/// hash means another caller already rotated, and an unreachable IdP is a
/// transport failure — neither means the user has to sign in again.
#[must_use]
pub(crate) fn llm_error_for(err: &OAuthHookError) -> LlmError {
    match err {
        OAuthHookError::RefreshFailed(_) => LlmError::OAuthRefreshDead,
        OAuthHookError::TokenStale | OAuthHookError::ProviderUnreachable(_) => {
            LlmError::Authentication {
                message: String::new(),
            }
        }
    }
}

/// Serves the current `OpenAI` OAuth access token, refreshing in place when expired
/// (single-flight via the underlying refresh lock).
///
/// Wraps an `Arc<RefreshDriver>` and calls [`RefreshDriver::refresh`] when the
/// in-memory token is expired.
pub struct OpenAiOAuthCredentialProvider {
    driver: Arc<RefreshDriver>,
}

impl OpenAiOAuthCredentialProvider {
    /// Wrap a refresh driver.
    #[must_use]
    pub fn new(driver: Arc<RefreshDriver>) -> Self {
        Self { driver }
    }
}

impl fmt::Debug for OpenAiOAuthCredentialProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAiOAuthCredentialProvider")
            .finish_non_exhaustive()
    }
}

impl CredentialProvider for OpenAiOAuthCredentialProvider {
    fn load<'a>(
        &'a self,
        _scope: &'a CredentialScope,
    ) -> BoxFuture<'a, Result<Credential, LlmError>> {
        Box::pin(async move {
            let state = &self.driver.state;

            // Check expiry under a read lock.
            let (is_expired, token_hash, access_token_str, account_id, fedramp) = {
                let token = state.token.read().await;
                let now = state.clock.now();
                let expired = token.expires_at <= now;
                let hash = token.token_hash();
                let s = token.access_token.expose_secret().clone();
                let account_id = token.account_id.clone();
                let fedramp = token.fedramp;
                (expired, hash, s, account_id, fedramp)
            };

            if !is_expired {
                return Ok(Credential::ChatGptOAuth {
                    access_token: access_token_str,
                    account_id,
                    fedramp,
                });
            }

            // Expired → single-flight refresh. The failure KIND survives via
            // `llm_error_for`; the failure MESSAGE never does, so no secret
            // material can leak into the rendered error.
            let bearer = self
                .driver
                .refresh(token_hash)
                .await
                .map_err(|e| llm_error_for(&e))?;

            // Read the updated account_id/fedramp after rotation.
            let (new_account_id, new_fedramp) = {
                let token = state.token.read().await;
                (token.account_id.clone(), token.fedramp)
            };

            Ok(Credential::ChatGptOAuth {
                access_token: bearer.0.expose_secret().clone(),
                account_id: new_account_id,
                fedramp: new_fedramp,
            })
        })
    }
}

#[cfg(test)]
mod credential_provider_tests {
    use super::*;
    use crate::oauth::openai::config::OpenAiOAuthConfig;
    use crate::oauth::openai::refresh::AuthState;
    use crate::oauth::openai::testsupport::{Canned, MockHttp, TestClock};
    use protocol::Secret;
    use std::time::{Duration, SystemTime};

    #[tokio::test]
    async fn returns_chatgpt_oauth_credential_when_not_expired() {
        let http = MockHttp::new(vec![]);
        let clock = TestClock::new(100);
        let cfg = OpenAiOAuthConfig::default();
        let state = AuthState::new(
            cfg,
            Secret::new("valid-access".into()),
            Some(Secret::new("refresh-tok".into())),
            // Expires far in the future.
            SystemTime::UNIX_EPOCH + Duration::from_secs(9_999_999),
            Some("acc_XYZ".into()),
            true,
            Some("acc_xyz@example.com".into()),
            http as Arc<dyn platform_api::HttpTransport>,
            clock as Arc<dyn platform_api::Clock>,
            None,
            None,
        );
        let driver = Arc::new(RefreshDriver::new(state));
        let provider = OpenAiOAuthCredentialProvider::new(driver);

        let scope = CredentialScope::new(crate::ProviderId::OpenAI, "default");
        let cred = provider.load(&scope).await.expect("load ok");
        match cred {
            Credential::ChatGptOAuth {
                access_token,
                account_id,
                fedramp,
            } => {
                assert_eq!(access_token, "valid-access");
                assert_eq!(account_id.as_deref(), Some("acc_XYZ"));
                assert!(fedramp);
            }
            other => panic!("expected ChatGptOAuth, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn refreshes_when_token_is_expired_and_returns_new_credential() {
        let resp = r#"{"access_token":"REFRESHED_ACCESS","refresh_token":"NEW_REFRESH","expires_in":3600}"#;
        let http = MockHttp::new(vec![(
            "oauth/token",
            Canned {
                status: 200,
                body: resp.into(),
            },
        )]);
        let clock = TestClock::new(5_000);
        let cfg = OpenAiOAuthConfig::default();
        let state = AuthState::new(
            cfg,
            Secret::new("EXPIRED_ACCESS".into()),
            Some(Secret::new("REFRESH_TOK".into())),
            // Already expired.
            SystemTime::UNIX_EPOCH + Duration::from_secs(1),
            Some("acc_ABC".into()),
            false,
            None,
            http as Arc<dyn platform_api::HttpTransport>,
            clock as Arc<dyn platform_api::Clock>,
            None,
            None,
        );
        let driver = Arc::new(RefreshDriver::new(state));
        let provider = OpenAiOAuthCredentialProvider::new(driver);

        let scope = CredentialScope::new(crate::ProviderId::OpenAI, "default");
        let cred = provider.load(&scope).await.expect("load ok");
        match cred {
            Credential::ChatGptOAuth { access_token, .. } => {
                assert_eq!(access_token, "REFRESHED_ACCESS");
            }
            other => panic!("expected ChatGptOAuth, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_rejected_refresh_token_maps_to_the_dead_oauth_session_error() {
        let http = MockHttp::new(vec![(
            "oauth/token",
            Canned {
                status: 401,
                body: r#"{"error":"invalid_grant"}"#.into(),
            },
        )]);
        let clock = TestClock::new(5_000);
        let cfg = OpenAiOAuthConfig::default();
        let state = AuthState::new(
            cfg,
            Secret::new("EXPIRED_ACCESS".into()),
            Some(Secret::new("REFRESH_TOK".into())),
            SystemTime::UNIX_EPOCH + Duration::from_secs(1),
            None,
            false,
            None,
            http as Arc<dyn platform_api::HttpTransport>,
            clock as Arc<dyn platform_api::Clock>,
            None,
            None,
        );
        let driver = Arc::new(RefreshDriver::new(state));
        let provider = OpenAiOAuthCredentialProvider::new(driver);

        let scope = CredentialScope::new(crate::ProviderId::OpenAI, "default");
        let err = provider.load(&scope).await.expect_err("should fail");
        // `invalid_grant` = the IdP rejected the refresh token, so this is a
        // dead session and the user has to sign in again — not the generic
        // auth failure a transient IdP outage produces.
        assert!(
            matches!(err, LlmError::OAuthRefreshDead),
            "expected OAuthRefreshDead, got {err:?}"
        );
    }
}
