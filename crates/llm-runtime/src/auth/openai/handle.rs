//! Account-scoped OAuth network operations. Persistence belongs to the host authority.
use crate::auth::openai::login::into_login_tokens;
use lingxi_core::host::Clock;
use lingxi_llm_client::auth::oauth::openai::{self as sdk, OpenAiOAuthConfig};
use lingxi_llm_client::Transport;
use std::sync::Arc;
use std::time::SystemTime;
use thiserror::Error;

struct RealClock;
impl Clock for RealClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// Failure from an account-bound OAuth exchange.
#[derive(Debug, Error)]
pub enum OpenAiAuthError {
    #[error("server error: {0}")]
    ServerError(String),
    #[error("login cancelled")]
    Cancelled,
}

/// Detached account OAuth network operations; persistence stays with the host authority.
pub struct AccountOAuthClient {
    config: OpenAiOAuthConfig,
    http: Arc<dyn Transport>,
    clock: Arc<dyn Clock>,
}
impl AccountOAuthClient {
    /// Construct network operations without a credential store or login singleton.
    pub fn new(config: OpenAiOAuthConfig, http: Arc<dyn Transport>) -> Self {
        Self {
            config,
            http,
            clock: Arc::new(RealClock),
        }
    }
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    pub fn begin_browser_login(&self, redirect_uri: &str) -> (String, String, String) {
        self.config.build_authorize_url_with_redirect(redirect_uri)
    }
    pub fn device_verification_url(&self) -> String {
        self.config.device_verify_url.clone()
    }
    pub async fn request_account_device_code(
        &self,
    ) -> Result<sdk::DeviceUserCode, OpenAiAuthError> {
        sdk::request_device_code(self.http.as_ref(), &self.config)
            .await
            .map_err(|_| {
                OpenAiAuthError::ServerError("device code authorization unavailable".into())
            })
    }
    pub async fn poll_account_device_code(
        &self,
        device_auth_id: &str,
        user_code: &str,
    ) -> Result<Option<secret::AccountCredential>, OpenAiAuthError> {
        match sdk::poll_for_token(self.http.as_ref(), &self.config, device_auth_id, user_code)
            .await
            .map_err(|_| OpenAiAuthError::ServerError("device authorization poll failed".into()))?
        {
            sdk::PollOutcome::Pending => Ok(None),
            sdk::PollOutcome::Ready {
                authorization_code,
                code_verifier,
            } => self
                .exchange_account_code(
                    &authorization_code,
                    &code_verifier,
                    &self.config.device_redirect_uri(),
                )
                .await
                .map(Some),
        }
    }
    /// Exchange an account-bound code without mutating any provider-wide slot.
    pub async fn exchange_account_code(
        &self,
        code: &str,
        verifier: &str,
        redirect_uri: &str,
    ) -> Result<secret::AccountCredential, OpenAiAuthError> {
        let tokens = sdk::exchange_code(
            self.http.as_ref(),
            &self.config,
            code,
            verifier,
            redirect_uri,
        )
        .await
        .map_err(|_| OpenAiAuthError::ServerError("account token exchange rejected".into()))?;
        let tokens = into_login_tokens(tokens, self.clock.as_ref());
        let id_token = tokens
            .id_token
            .as_deref()
            .ok_or_else(|| OpenAiAuthError::ServerError("account identity token missing".into()))?;
        let subject = account_subject(id_token, &self.config, self.clock.now())?;
        let claims = sdk::parse_id_token(id_token)
            .ok_or_else(|| OpenAiAuthError::ServerError("account identity token invalid".into()))?;
        Ok(secret::AccountCredential {
            credential_revision: 0,
            identity_generation: 0,
            kind: secret::AccountCredentialKind::OAuth {
                access_token: tokens.access_token,
                refresh_token: tokens.refresh_token,
                expires_at: tokens.expires_at,
                scopes: vec![],
                issuer: self.config.issuer.clone(),
                subject: Some(subject),
                organization_id: claims.account_id,
                email: claims.email,
                fedramp: claims.fedramp,
            },
        })
    }
    /// Rotate one grant in memory. Persistence authority must commit it before publication.
    pub async fn refresh_account_grant(
        &self,
        old: &secret::AccountCredential,
    ) -> Result<secret::AccountCredential, OpenAiAuthError> {
        let secret::AccountCredentialKind::OAuth {
            refresh_token: Some(refresh),
            scopes,
            issuer,
            subject,
            organization_id,
            email,
            fedramp,
            ..
        } = &old.kind
        else {
            return Err(OpenAiAuthError::Cancelled);
        };
        let tokens = sdk::refresh_token(self.http.as_ref(), &self.config, refresh.expose_secret())
            .await
            .map_err(|_| OpenAiAuthError::ServerError("account token refresh rejected".into()))?;
        let tokens = into_login_tokens(tokens, self.clock.as_ref());
        let claims = tokens.id_token.as_deref().and_then(sdk::parse_id_token);
        if let Some(id_token) = tokens.id_token.as_deref() {
            let refreshed_subject = account_subject(id_token, &self.config, self.clock.now())?;
            if subject
                .as_ref()
                .is_some_and(|old| old != &refreshed_subject)
            {
                return Err(OpenAiAuthError::ServerError(
                    "account principal changed during token rotation".into(),
                ));
            }
        }
        if let (Some(old), Some(new)) = (
            organization_id.as_ref(),
            claims.as_ref().and_then(|c| c.account_id.as_ref()),
        ) {
            if old != new {
                return Err(OpenAiAuthError::ServerError(
                    "account workspace changed during token rotation".into(),
                ));
            }
        }

        Ok(secret::AccountCredential {
            credential_revision: old
                .credential_revision
                .checked_add(1)
                .ok_or(OpenAiAuthError::Cancelled)?,
            identity_generation: old.identity_generation,
            kind: secret::AccountCredentialKind::OAuth {
                access_token: tokens.access_token,
                refresh_token: tokens.refresh_token.or_else(|| {
                    Some(lingxi_core::types::Secret::new(
                        refresh.expose_secret().clone(),
                    ))
                }),
                expires_at: tokens.expires_at,
                scopes: scopes.clone(),
                issuer: issuer.clone(),
                subject: tokens
                    .id_token
                    .as_deref()
                    .map(|token| account_subject(token, &self.config, self.clock.now()))
                    .transpose()?
                    .or_else(|| subject.clone()),
                organization_id: claims
                    .as_ref()
                    .and_then(|c| c.account_id.clone())
                    .or_else(|| organization_id.clone()),
                email: claims
                    .as_ref()
                    .and_then(|c| c.email.clone())
                    .or_else(|| email.clone()),
                fedramp: claims.map_or(*fedramp, |c| c.fedramp),
            },
        })
    }
}

fn account_subject(
    jwt: &str,
    config: &OpenAiOAuthConfig,
    now: SystemTime,
) -> Result<String, OpenAiAuthError> {
    use base64::Engine as _;
    let invalid =
        || OpenAiAuthError::ServerError("account identity token validation failed".into());
    let mut parts = jwt.split('.');
    let _header = parts.next().ok_or_else(invalid)?;
    let payload = parts.next().ok_or_else(invalid)?;
    let signature = parts.next().ok_or_else(invalid)?;
    if parts.next().is_some() || signature.is_empty() {
        return Err(invalid());
    }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| invalid())?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    let audience = value.get("aud").is_some_and(|aud| {
        aud.as_str() == Some(config.client_id.as_str())
            || aud.as_array().is_some_and(|values| {
                values
                    .iter()
                    .any(|v| v.as_str() == Some(config.client_id.as_str()))
            })
    });
    if value.get("iss").and_then(|v| v.as_str()) != Some(config.issuer.as_str())
        || !audience
        || value
            .get("exp")
            .and_then(|v| v.as_u64())
            .is_none_or(|expires| {
                expires
                    <= now
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs()
            })
    {
        return Err(invalid());
    }
    value
        .get("sub")
        .and_then(|v| v.as_str())
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
        .ok_or_else(invalid)
}

#[cfg(test)]
mod account_identity_tests {
    use super::*;
    use base64::Engine as _;
    use std::time::Duration;
    fn token(value: serde_json::Value) -> String {
        format!(
            "header.{}.endpoint-signature",
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(serde_json::to_vec(&value).unwrap())
        )
    }
    #[test]
    fn principal_uses_sub_and_validates_endpoint_identity_claims() {
        let config = OpenAiOAuthConfig::default();
        let value = serde_json::json!({"iss":config.issuer,"aud":config.client_id,"sub":"user-a","exp":5000,"https://api.openai.com/auth":{"chatgpt_account_id":"shared-workspace"}});
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        assert_eq!(
            account_subject(&token(value.clone()), &config, now).unwrap(),
            "user-a"
        );
        for field in ["iss", "aud", "exp", "sub"] {
            let mut invalid = value.clone();
            invalid.as_object_mut().unwrap().remove(field);
            assert!(account_subject(&token(invalid), &config, now).is_err());
        }
    }
}
