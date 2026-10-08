//! Boot-captured environmental OAuth material; no fabricated expiry or renewal.
use crate::{BoxFuture, Credential, CredentialProvider, CredentialScope, LlmError};

pub struct EnvironmentOAuthCredentialProvider {
    pub(crate) credential: Credential,
    pub subscription_type: Option<String>,
    pub rate_limit_tier: Option<String>,
}
impl std::fmt::Debug for EnvironmentOAuthCredentialProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EnvironmentOAuthCredentialProvider")
            .field("credential", &self.credential)
            .finish_non_exhaustive()
    }
}
impl EnvironmentOAuthCredentialProvider {
    #[must_use]
    pub fn capture() -> Option<Self> {
        use lingxi_llm_client::providers::anthropic::oauth_source::{
            select_oauth_source, OAuthSourceInputs,
        };
        let token = std::env::var("CLAUDE_CODE_OAUTH_TOKEN").ok();
        let scopes = std::env::var("CLAUDE_CODE_OAUTH_SCOPES").ok();
        let selected = select_oauth_source(OAuthSourceInputs {
            environment_token: token.as_deref(),
            environment_scopes: scopes.as_deref(),
            ..OAuthSourceInputs::default()
        })?;
        Some(Self {
            credential: Credential::AnthropicOAuth {
                access_token: selected.access_token.into(),
                scopes: selected.scopes.into_owned(),
            },
            subscription_type: metadata(branding::SUBSCRIPTION_TYPE_ENV),
            rate_limit_tier: metadata(branding::RATE_LIMIT_TIER_ENV),
        })
    }
    #[must_use]
    pub fn scopes(&self) -> &[String] {
        match &self.credential {
            Credential::AnthropicOAuth { scopes, .. } => scopes,
            _ => unreachable!(),
        }
    }
}
fn metadata(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| {
            value
                .trim_matches(|c| {
                    matches!(c, '\u{0009}'..='\u{000D}' | '\u{0020}' | '\u{00A0}' |
                        '\u{1680}' | '\u{2000}'..='\u{200A}' | '\u{2028}' | '\u{2029}' |
                        '\u{202F}' | '\u{205F}' | '\u{3000}' | '\u{FEFF}')
                })
                .to_owned()
        })
        .filter(|value| !value.is_empty())
}
impl CredentialProvider for EnvironmentOAuthCredentialProvider {
    fn source<'a>(
        &'a self,
        _scope: &'a CredentialScope,
    ) -> BoxFuture<'a, Result<crate::CredentialSource, LlmError>> {
        Box::pin(async { Ok(crate::CredentialSource::OAuth) })
    }

    fn anthropic_auth_snapshot<'a>(
        &'a self,
        scope: &'a CredentialScope,
    ) -> BoxFuture<'a, Result<crate::AnthropicAuthSnapshot, LlmError>> {
        Box::pin(async move {
            Ok(
                if scope.provider_id == crate::ProviderId::AnthropicFirstParty {
                    crate::AnthropicAuthSnapshot::from_credential(
                        scope.clone(),
                        self.credential.clone(),
                    )
                } else {
                    crate::AnthropicAuthSnapshot::default()
                },
            )
        })
    }
    fn load<'a>(&'a self, _: &'a CredentialScope) -> BoxFuture<'a, Result<Credential, LlmError>> {
        Box::pin(async { Ok(self.credential.clone()) })
    }
}
