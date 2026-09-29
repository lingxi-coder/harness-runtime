//! Personal Access Token (PAT) auth for the `OpenAI` Codex backend.
//!
//! A PAT (`at-…`) is a long-lived bearer token. On load the engine resolves the
//! `account_id` / fedramp once via `whoami`, then a static credential provider
//! serves `Credential::ChatGptOAuth` per request (no refresh). Byte-aligned with
//! codex `login/src/auth/personal_access_token.rs`.

use std::fmt;
use std::sync::Arc;

use crate::{BoxFuture, Credential, CredentialProvider, CredentialScope, LlmError};

use crate::oauth::openai::client::OAuthError;
use crate::oauth::openai::config::OpenAiOAuthConfig;

pub use lingxi_llm_client::auth::oauth::openai::PatMetadata;

pub async fn whoami(
    cfg: &OpenAiOAuthConfig,
    http: &Arc<dyn lingxi_llm_client::Transport>,
    pat: &str,
) -> Result<PatMetadata, OAuthError> {
    lingxi_llm_client::auth::oauth::openai::whoami(http.as_ref(), cfg, pat)
        .await
        .map_err(|e| OAuthError::TokenExchange(e.to_string()))
}

/// Static credential provider for a PAT. Serves `Credential::ChatGptOAuth` with
/// the PAT as the bearer + the resolved `account_id/fedramp`. No refresh.
pub struct PatCredentialProvider {
    pat: String,
    account_id: Option<String>,
    fedramp: bool,
}

impl PatCredentialProvider {
    /// Build from a PAT + its resolved metadata (the engine calls [`whoami`] once).
    #[must_use]
    pub fn new(pat: impl Into<String>, metadata: PatMetadata) -> Self {
        Self {
            pat: pat.into(),
            account_id: metadata.account_id,
            fedramp: metadata.fedramp,
        }
    }
}

impl fmt::Debug for PatCredentialProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PatCredentialProvider")
            .field("pat", &"[REDACTED]")
            .field("account_id", &self.account_id)
            .field("fedramp", &self.fedramp)
            .finish()
    }
}

impl CredentialProvider for PatCredentialProvider {
    fn load<'a>(
        &'a self,
        _scope: &'a CredentialScope,
    ) -> BoxFuture<'a, Result<Credential, LlmError>> {
        let cred = Credential::ChatGptOAuth {
            access_token: self.pat.clone(),
            account_id: self.account_id.clone(),
            fedramp: self.fedramp,
        };
        Box::pin(async move { Ok(cred) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth::openai::testsupport::{Canned, MockHttp};

    fn cfg() -> OpenAiOAuthConfig {
        OpenAiOAuthConfig::default()
    }

    #[tokio::test]
    async fn whoami_parses_account_and_fedramp() {
        let http: Arc<dyn lingxi_llm_client::Transport> = MockHttp::new(vec![(
            "whoami",
            Canned {
                status: 200,
                body: r#"{"chatgpt_account_id":"acc_7","chatgpt_account_is_fedramp":true,"email":"u@x.com","chatgpt_plan_type":"pro"}"#.into(),
            },
        )]);
        let md = whoami(&cfg(), &http, "at-token").await.expect("ok");
        assert_eq!(md.account_id.as_deref(), Some("acc_7"));
        assert!(md.fedramp);
        assert_eq!(md.email.as_deref(), Some("u@x.com"));
    }

    #[tokio::test]
    async fn whoami_non_200_errors() {
        let http: Arc<dyn lingxi_llm_client::Transport> = MockHttp::new(vec![(
            "whoami",
            Canned {
                status: 401,
                body: "{}".into(),
            },
        )]);
        assert!(whoami(&cfg(), &http, "at-bad").await.is_err());
    }

    #[tokio::test]
    async fn provider_returns_chatgpt_oauth() {
        let p = PatCredentialProvider::new(
            "at-token",
            PatMetadata {
                account_id: Some("acc_7".into()),
                fedramp: false,
                ..Default::default()
            },
        );
        let scope = CredentialScope::new(
            crate::ProviderId::OpenAICompatible {
                name: "openai-chatgpt".into(),
            },
            "openai-chatgpt",
        );
        let got = p.load(&scope).await.expect("load");
        match got {
            Credential::ChatGptOAuth {
                access_token,
                account_id,
                fedramp,
            } => {
                assert_eq!(access_token, "at-token");
                assert_eq!(account_id.as_deref(), Some("acc_7"));
                assert!(!fedramp);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn debug_redacts_pat() {
        let p = PatCredentialProvider::new("at-secret", PatMetadata::default());
        assert!(!format!("{p:?}").contains("at-secret"));
    }
}
