//! `crate::CredentialProvider` over the OAuth refresh machinery.
//!
//! [`OAuthCredentialProvider`] serves the current OAuth access token,
//! refreshing in place (single-flight via the underlying `refresh_lock`)
//! when the token is expired per the state's clock.
//!
//! `load` captures token and granted scopes together. `refresh` handles an
//! explicit rejection through the same single-flight driver, even before expiry.
//! The Fast-status caller decides whether the native HTTP/scope conditions admit
//! one refresh; the provider does not retry model requests implicitly.

use std::fmt;
use std::sync::Arc;

use crate::{BoxFuture, Credential, CredentialProvider, CredentialScope, LlmError};

use crate::auth::anthropic::refresh::RefreshDriver;

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
    fn source<'a>(
        &'a self,
        _scope: &'a CredentialScope,
    ) -> BoxFuture<'a, Result<crate::CredentialSource, LlmError>> {
        Box::pin(async move {
            let token = self.driver.state.token.read().await;
            Ok(if token.access_token.expose_secret().is_empty() {
                crate::CredentialSource::None
            } else {
                crate::CredentialSource::OAuth
            })
        })
    }

    fn anthropic_auth_snapshot<'a>(
        &'a self,
        scope: &'a CredentialScope,
    ) -> BoxFuture<'a, Result<crate::AnthropicAuthSnapshot, LlmError>> {
        Box::pin(async move {
            if scope.provider_id != crate::ProviderId::AnthropicFirstParty {
                return Ok(crate::AnthropicAuthSnapshot::default());
            }
            let token = self.driver.state.token.read().await;
            if token.access_token.expose_secret().is_empty() {
                return Ok(crate::AnthropicAuthSnapshot::default());
            }
            Ok(crate::AnthropicAuthSnapshot::from_credential(
                scope.clone(),
                Credential::AnthropicOAuth {
                    access_token: token.access_token.expose_secret().clone(),
                    scopes: token.scopes.clone(),
                },
            ))
        })
    }
    fn load<'a>(
        &'a self,
        _scope: &'a CredentialScope,
    ) -> BoxFuture<'a, Result<Credential, LlmError>> {
        Box::pin(async move {
            let state = &self.driver.state;

            // Check expiry under a read lock.
            let (is_expired, token_hash, credential) = {
                let token = state.token.read().await;
                let now = state.clock.now();
                let expired = token.expires_at <= now;
                let hash = token.token_hash();
                // Clone the string out from under the lock (Secret does not
                // implement Clone; we expose only at this final conversion point).
                let credential = Credential::AnthropicOAuth {
                    access_token: token.access_token.expose_secret().clone(),
                    scopes: token.scopes.clone(),
                };
                (expired, hash, credential)
            };

            if !is_expired {
                return Ok(credential);
            }

            // Expired → single-flight refresh (double-check-after-acquire lives
            // inside `RefreshDriver::refresh`).  The failure KIND survives via
            // `llm_error_for` (a rejected refresh token is a dead session and
            // gets its own surface); the failure MESSAGE never does, so no
            // secret material can leak into the rendered error.
            self.driver
                .refresh(token_hash)
                .await
                .map_err(|e| crate::auth::lifecycle::llm_error_for(&e))?;

            let token = state.token.read().await;
            Ok(Credential::AnthropicOAuth {
                access_token: token.access_token.expose_secret().clone(),
                scopes: token.scopes.clone(),
            })
        })
    }

    fn refresh<'a>(
        &'a self,
        _scope: &'a CredentialScope,
        rejected: &'a Credential,
    ) -> BoxFuture<'a, Result<Option<Credential>, LlmError>> {
        Box::pin(async move {
            let Credential::AnthropicOAuth { access_token, .. } = rejected else {
                return Ok(None);
            };
            let hash = crate::auth::lifecycle::token_hash(&lingxi_core::types::Secret::new(
                access_token.clone(),
            ));
            self.driver
                .refresh(hash)
                .await
                .map_err(|error| crate::auth::lifecycle::llm_error_for(&error))?;
            let token = self.driver.state.token.read().await;
            Ok(Some(Credential::AnthropicOAuth {
                access_token: token.access_token.expose_secret().clone(),
                scopes: token.scopes.clone(),
            }))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::lifecycle::OAuthHookError;

    /// The whole point of the split: three refresh failures, but only one of
    /// them means the user has to log in again.
    #[test]
    fn only_a_rejected_refresh_token_is_the_dead_oauth_session() {
        assert_eq!(
            crate::auth::lifecycle::llm_error_for(&OAuthHookError::RefreshFailed(
                "idp said no".into()
            )),
            LlmError::OAuthRefreshDead
        );
        // Another caller rotated first — the retry succeeds, nothing expired.
        assert_eq!(
            crate::auth::lifecycle::llm_error_for(&OAuthHookError::TokenStale),
            LlmError::Authentication {
                message: String::new()
            }
        );
        // The IdP was unreachable; the refresh token may be perfectly valid.
        assert_eq!(
            crate::auth::lifecycle::llm_error_for(&OAuthHookError::ProviderUnreachable(
                "dns".into()
            )),
            LlmError::Authentication {
                message: String::new()
            }
        );
    }
}
