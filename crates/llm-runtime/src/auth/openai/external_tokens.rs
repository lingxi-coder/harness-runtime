//! Externally-supplied `ChatGPT` tokens (codex `ChatgptAuthTokens` mode).
//!
//! An enterprise auth server supplies a ready access token + `account_id`; we serve
//! it statically as `Credential::ChatGptOAuth` (no refresh — the external system
//! owns the lifecycle). Same header set as the OAuth-login path.

use std::fmt;

use crate::{BoxFuture, Credential, CredentialProvider, CredentialScope, LlmError};

/// Static credential provider over externally-supplied `ChatGPT` tokens.
pub struct ExternalTokensCredentialProvider {
    access_token: String,
    account_id: Option<String>,
    fedramp: bool,
}

impl ExternalTokensCredentialProvider {
    /// Build from a supplied access token + `account_id`. `fedramp` is parsed from
    /// the token's `id_token` claims when the token is a JWT (best-effort; else false).
    #[must_use]
    pub fn from_supplied(access_token: impl Into<String>, account_id: Option<String>) -> Self {
        let access_token = access_token.into();
        let fedramp = lingxi_llm_client::auth::oauth::openai::parse_id_token(&access_token)
            .is_some_and(|c| c.fedramp);
        Self {
            access_token,
            account_id,
            fedramp,
        }
    }
}

impl fmt::Debug for ExternalTokensCredentialProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExternalTokensCredentialProvider")
            .field("access_token", &"[REDACTED]")
            .field("account_id", &self.account_id)
            .field("fedramp", &self.fedramp)
            .finish()
    }
}

impl CredentialProvider for ExternalTokensCredentialProvider {
    fn load<'a>(
        &'a self,
        _scope: &'a CredentialScope,
    ) -> BoxFuture<'a, Result<Credential, LlmError>> {
        let cred = Credential::ChatGptOAuth {
            access_token: self.access_token.clone(),
            account_id: self.account_id.clone(),
            fedramp: self.fedramp,
        };
        Box::pin(async move { Ok(cred) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn returns_chatgpt_oauth_with_supplied_account() {
        let p =
            ExternalTokensCredentialProvider::from_supplied("plain-token", Some("acc_2".into()));
        let scope = CredentialScope::new(
            crate::ProviderId::OpenAICompatible {
                name: "openai-chatgpt".into(),
            },
            "openai-chatgpt",
        );
        match p.load(&scope).await.expect("load") {
            Credential::ChatGptOAuth {
                access_token,
                account_id,
                fedramp,
            } => {
                assert_eq!(access_token, "plain-token");
                assert_eq!(account_id.as_deref(), Some("acc_2"));
                assert!(!fedramp);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn debug_redacts_token() {
        let p = ExternalTokensCredentialProvider::from_supplied("secret-tok", None);
        assert!(!format!("{p:?}").contains("secret-tok"));
    }
}
