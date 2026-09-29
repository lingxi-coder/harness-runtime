//! `crate::CredentialProvider` over the OAuth refresh machinery.
//!
//! [`OAuthCredentialProvider`] serves the current OAuth access token,
//! refreshing in place (single-flight via the underlying `refresh_lock`)
//! when the token is expired per the state's clock.
//!
//! ## Reactive-401 deferral (Plan 3a Task 9)
//!
//! Today this provider only performs **proactive** refresh: it checks the
//! token expiry under a read lock and refreshes when `expires_at <= now`.
//! The reactive-401 path — where the API returns 401 mid-request and the
//! client retries once with a freshly-rotated token — is a future concern.
//! The former api-client `current_hook()` seam was removed in Plan 3a/3b.
//! The practical impact is low: proactive refresh fires well before expiry
//! (½-remaining or 5 min lead), so the access token is fresh on every call
//! in the steady state; a 401 would require the clock to be wrong or the
//! token to be revoked externally.

use std::fmt;
use std::sync::Arc;

use crate::{BoxFuture, Credential, CredentialProvider, CredentialScope, LlmError};

use crate::oauth::anthropic::refresh::RefreshDriver;

/// Serves the current OAuth access token, refreshing in place when expired
/// (single-flight via the underlying refresh lock).
///
/// Wraps an `Arc<RefreshDriver>` and calls [`RefreshDriver::refresh`] (inherent
/// — no api-client trait dependency) when the in-memory token is expired.
pub struct OAuthCredentialProvider {
    driver: Arc<RefreshDriver>,
}

impl OAuthCredentialProvider {
    /// Wrap a refresh driver.
    #[must_use]
    pub fn new(driver: Arc<RefreshDriver>) -> Self {
        Self { driver }
    }
}

impl fmt::Debug for OAuthCredentialProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthCredentialProvider")
            .finish_non_exhaustive()
    }
}

impl CredentialProvider for OAuthCredentialProvider {
    fn load<'a>(
        &'a self,
        _scope: &'a CredentialScope,
    ) -> BoxFuture<'a, Result<Credential, LlmError>> {
        Box::pin(async move {
            let state = &self.driver.state;

            // Check expiry under a read lock.
            let (is_expired, token_hash, access_token_str) = {
                let token = state.token.read().await;
                let now = state.clock.now();
                let expired = token.expires_at <= now;
                let hash = token.token_hash();
                // Clone the string out from under the lock (Secret does not
                // implement Clone; we expose only at this final conversion point).
                let s = token.access_token.expose_secret().clone();
                (expired, hash, s)
            };

            if !is_expired {
                return Ok(Credential::BearerToken(access_token_str));
            }

            // Expired → single-flight refresh (double-check-after-acquire lives
            // inside `RefreshDriver::refresh`).  The failure KIND survives via
            // `llm_error_for` (a rejected refresh token is a dead session and
            // gets its own surface); the failure MESSAGE never does, so no
            // secret material can leak into the rendered error.
            let bearer = self
                .driver
                .refresh(token_hash)
                .await
                .map_err(|e| crate::oauth::lifecycle::llm_error_for(&e))?;

            Ok(Credential::BearerToken(bearer.0.expose_secret().clone()))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth::lifecycle::OAuthHookError;

    /// The whole point of the split: three refresh failures, but only one of
    /// them means the user has to log in again.
    #[test]
    fn only_a_rejected_refresh_token_is_the_dead_oauth_session() {
        assert_eq!(
            crate::oauth::lifecycle::llm_error_for(&OAuthHookError::RefreshFailed(
                "idp said no".into()
            )),
            LlmError::OAuthRefreshDead
        );
        // Another caller rotated first — the retry succeeds, nothing expired.
        assert_eq!(
            crate::oauth::lifecycle::llm_error_for(&OAuthHookError::TokenStale),
            LlmError::Authentication {
                message: String::new()
            }
        );
        // The IdP was unreachable; the refresh token may be perfectly valid.
        assert_eq!(
            crate::oauth::lifecycle::llm_error_for(&OAuthHookError::ProviderUnreachable(
                "dns".into()
            )),
            LlmError::Authentication {
                message: String::new()
            }
        );
    }
}
