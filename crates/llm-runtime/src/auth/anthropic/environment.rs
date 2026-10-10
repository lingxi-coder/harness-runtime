//! Host-owned environmental OAuth material; no fabricated expiry or renewal.
use crate::{BoxFuture, Credential, CredentialProvider, CredentialScope, LlmError};
use std::sync::Arc;

/// A live host environment accessor. Debug never evaluates or exposes values.
#[derive(Clone)]
pub struct EnvironmentLookup {
    environment: Arc<dyn Fn(&str) -> Option<String> + Send + Sync>,
    descriptor: Arc<dyn Fn() -> Option<OAuthDescriptorCredential> + Send + Sync>,
}
/// Material acquired and cached by the embedding host, never by the runtime.
#[derive(Clone)]
pub struct OAuthDescriptorCredential {
    pub access_token: String,
    pub scopes: Option<Vec<String>>,
    pub from_background_snapshot: bool,
    pub host_managed: bool,
}
impl std::fmt::Debug for OAuthDescriptorCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthDescriptorCredential")
            .field("access_token", &"<redacted>")
            .field("from_background_snapshot", &self.from_background_snapshot)
            .field("host_managed", &self.host_managed)
            .finish_non_exhaustive()
    }
}
impl EnvironmentLookup {
    pub fn new(lookup: impl Fn(&str) -> Option<String> + Send + Sync + 'static) -> Self {
        Self {
            environment: Arc::new(lookup),
            descriptor: Arc::new(|| None),
        }
    }
    pub fn get(&self, name: &str) -> Option<String> {
        (self.environment)(name)
    }
    pub fn with_descriptor_lookup(
        mut self,
        lookup: impl Fn() -> Option<OAuthDescriptorCredential> + Send + Sync + 'static,
    ) -> Self {
        self.descriptor = Arc::new(lookup);
        self
    }
}
impl std::fmt::Debug for EnvironmentLookup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("EnvironmentLookup(<host>)")
    }
}

pub struct EnvironmentOAuthCredentialProvider {
    pub(crate) credential: Credential,
    pub subscription_type: Option<String>,
    pub rate_limit_tier: Option<String>,
    lookup: Option<EnvironmentLookup>,
    fallback: Option<Arc<dyn CredentialProvider>>,
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
        Self::capture_with_lookup(EnvironmentLookup::new(|name| std::env::var(name).ok()))
    }
    #[must_use]
    pub fn capture_with_lookup(lookup: EnvironmentLookup) -> Option<Self> {
        Some(Self {
            credential: {
                let descriptor = if lookup
                    .get("CLAUDE_CODE_OAUTH_TOKEN")
                    .is_some_and(|token| !token.is_empty())
                {
                    None
                } else {
                    (lookup.descriptor)()
                };
                if !lookup
                    .get("CLAUDE_CODE_OAUTH_TOKEN")
                    .is_some_and(|token| !token.is_empty())
                    && descriptor
                        .as_ref()
                        .is_some_and(|value| value.from_background_snapshot && !value.host_managed)
                {
                    return None;
                }
                credential_from_sources_with_descriptor(&lookup, None, descriptor.as_ref())?
            },
            subscription_type: metadata(&lookup, branding::SUBSCRIPTION_TYPE_ENV),
            rate_limit_tier: metadata(&lookup, branding::RATE_LIMIT_TIER_ENV),
            lookup: Some(lookup),
            fallback: None,
        })
    }
    /// Environmental OAuth is checked before the existing stored OAuth owner.
    /// An empty or removed variable never revives the boot-captured token.
    pub fn live(lookup: EnvironmentLookup, fallback: Option<Arc<dyn CredentialProvider>>) -> Self {
        Self {
            credential: Credential::AnthropicOAuth {
                access_token: String::new(),
                scopes: Vec::new(),
            },
            subscription_type: metadata(&lookup, branding::SUBSCRIPTION_TYPE_ENV),
            rate_limit_tier: metadata(&lookup, branding::RATE_LIMIT_TIER_ENV),
            lookup: Some(lookup),
            fallback,
        }
    }
    async fn current(&self, scope: &CredentialScope) -> Result<Credential, LlmError> {
        if scope.provider_id != crate::ProviderId::AnthropicFirstParty {
            return Err(LlmError::Authentication {
                message: "environmental Anthropic OAuth is not available for this provider".into(),
            });
        }
        if let Some(lookup) = &self.lookup {
            let descriptor = if lookup
                .get("CLAUDE_CODE_OAUTH_TOKEN")
                .is_some_and(|token| !token.is_empty())
            {
                None
            } else {
                (lookup.descriptor)()
            };
            let defer_descriptor = descriptor
                .as_ref()
                .is_some_and(|value| value.from_background_snapshot && !value.host_managed);
            if !defer_descriptor
                || lookup
                    .get("CLAUDE_CODE_OAUTH_TOKEN")
                    .is_some_and(|value| !value.is_empty())
            {
                if let Some(credential) =
                    credential_from_sources_with_descriptor(lookup, None, descriptor.as_ref())
                {
                    return Ok(credential);
                }
            }
            if let Some(fallback) = &self.fallback {
                let stored = fallback.load(scope).await.ok();
                return credential_from_sources_with_descriptor(
                    lookup,
                    stored.as_ref(),
                    descriptor.as_ref(),
                )
                .ok_or_else(|| LlmError::Authentication {
                    message: String::new(),
                });
            }
            return credential_from_sources_with_descriptor(lookup, None, descriptor.as_ref())
                .ok_or_else(|| LlmError::Authentication {
                    message: String::new(),
                });
        }
        Ok(self.credential.clone())
    }
    #[must_use]
    pub fn scopes(&self) -> &[String] {
        match &self.credential {
            Credential::AnthropicOAuth { scopes, .. } => scopes,
            _ => unreachable!(),
        }
    }
}
fn metadata(lookup: &EnvironmentLookup, name: &str) -> Option<String> {
    lookup
        .get(name)
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
                        self.current(scope).await?,
                    )
                } else {
                    crate::AnthropicAuthSnapshot::default()
                },
            )
        })
    }
    fn refresh<'a>(
        &'a self,
        scope: &'a CredentialScope,
        rejected: &'a Credential,
    ) -> BoxFuture<'a, Result<Option<Credential>, LlmError>> {
        Box::pin(async move {
            if scope.provider_id != crate::ProviderId::AnthropicFirstParty {
                return Ok(None);
            }
            if let Some(lookup) = &self.lookup {
                if lookup
                    .get("CLAUDE_CODE_OAUTH_TOKEN")
                    .is_some_and(|token| !token.is_empty())
                {
                    return Ok(None);
                }
                if (lookup.descriptor)().is_some_and(|value| {
                    !value.access_token.is_empty()
                        && (!value.from_background_snapshot || value.host_managed)
                }) {
                    return Ok(None);
                }
            }
            match &self.fallback {
                Some(fallback) => fallback.refresh(scope, rejected).await,
                None => Ok(None),
            }
        })
    }
    fn load<'a>(
        &'a self,
        scope: &'a CredentialScope,
    ) -> BoxFuture<'a, Result<Credential, LlmError>> {
        Box::pin(self.current(scope))
    }
}

fn credential_from_sources_with_descriptor(
    lookup: &EnvironmentLookup,
    stored: Option<&Credential>,
    descriptor: Option<&OAuthDescriptorCredential>,
) -> Option<Credential> {
    use lingxi_llm_client::providers::anthropic::oauth_source::{
        select_oauth_source, OAuthSourceInputs,
    };
    let token = lookup.get("CLAUDE_CODE_OAUTH_TOKEN");
    let scopes = lookup.get("CLAUDE_CODE_OAUTH_SCOPES");
    let stored = match stored {
        Some(Credential::AnthropicOAuth {
            access_token,
            scopes,
        }) => Some((access_token, scopes)),
        _ => None,
    };
    let selected = select_oauth_source(OAuthSourceInputs {
        environment_token: token.as_deref(),
        environment_scopes: scopes.as_deref(),
        descriptor_token: descriptor.map(|value| value.access_token.as_str()),
        descriptor_scopes: descriptor.and_then(|value| value.scopes.as_deref()),
        from_background_snapshot: descriptor.is_some_and(|value| value.from_background_snapshot),
        host_managed: descriptor.is_some_and(|value| value.host_managed),
        stored_token: stored.map(|(token, _)| token.as_str()),
        stored_scopes: stored.map(|(_, scopes)| scopes.as_slice()),
    })?;
    Some(Credential::AnthropicOAuth {
        access_token: selected.access_token.into(),
        scopes: selected.scopes.into_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::RwLock;
    fn scope() -> CredentialScope {
        CredentialScope::new(crate::ProviderId::AnthropicFirstParty, "anthropic")
            .with_credential_id("anthropic-oauth")
    }
    #[tokio::test]
    async fn host_oauth_rotation_empty_clearing_and_session_token_are_independent() {
        let values = Arc::new(RwLock::new(std::collections::HashMap::from([
            ("CLAUDE_CODE_OAUTH_TOKEN".to_owned(), "first".to_owned()),
            ("SESSION_ACCESS_TOKEN".to_owned(), "ingress-only".to_owned()),
        ])));
        let read = values.clone();
        let lookup = EnvironmentLookup::new(move |key| read.read().unwrap().get(key).cloned());
        let provider = EnvironmentOAuthCredentialProvider::live(lookup.clone(), None);
        assert!(
            matches!(provider.load(&scope()).await.unwrap(), Credential::AnthropicOAuth {access_token,..} if access_token=="first")
        );
        values
            .write()
            .unwrap()
            .insert("CLAUDE_CODE_OAUTH_TOKEN".into(), "second".into());
        assert!(
            matches!(provider.load(&scope()).await.unwrap(), Credential::AnthropicOAuth {access_token,..} if access_token=="second")
        );
        values
            .write()
            .unwrap()
            .insert("CLAUDE_CODE_OAUTH_TOKEN".into(), String::new());
        assert!(matches!(
            provider.load(&scope()).await,
            Err(LlmError::Authentication { .. })
        ));
        let fallback: Arc<dyn CredentialProvider> = Arc::new(crate::StaticCredentialProvider::new(
            Credential::AnthropicOAuth {
                access_token: "stored".into(),
                scopes: vec!["user:inference".into()],
            },
        ));
        let provider = EnvironmentOAuthCredentialProvider::live(lookup, Some(fallback));
        assert!(
            matches!(provider.load(&scope()).await.unwrap(), Credential::AnthropicOAuth {access_token,..} if access_token=="stored")
        );
        assert!(matches!(
            provider
                .load(&CredentialScope::new(
                    crate::ProviderId::Custom {
                        name: "anthropic-compatible".into()
                    },
                    "custom"
                ))
                .await,
            Err(LlmError::Authentication { .. })
        ));
        assert!(!format!("{provider:?}").contains("stored"));
    }
}

#[cfg(test)]
mod descriptor_tests {
    use super::*;
    fn descriptor(background: bool, managed: bool) -> OAuthDescriptorCredential {
        OAuthDescriptorCredential {
            access_token: "fd-token".into(),
            scopes: None,
            from_background_snapshot: background,
            host_managed: managed,
        }
    }
    #[tokio::test]
    async fn empty_environment_exposes_host_descriptor_then_existing_store() {
        let scope = CredentialScope::new(crate::ProviderId::AnthropicFirstParty, "anthropic");
        let stored: Arc<dyn CredentialProvider> = Arc::new(crate::StaticCredentialProvider::new(
            Credential::AnthropicOAuth {
                access_token: "stored".into(),
                scopes: vec!["stored:scope".into()],
            },
        ));
        for (background, managed, expected) in [
            (false, false, "fd-token"),
            (true, false, "stored"),
            (true, true, "fd-token"),
        ] {
            let lookup = EnvironmentLookup::new(|_| Some(String::new()))
                .with_descriptor_lookup(move || Some(descriptor(background, managed)));
            let provider = EnvironmentOAuthCredentialProvider::live(lookup, Some(stored.clone()));
            assert!(
                matches!(provider.load(&scope).await.unwrap(),Credential::AnthropicOAuth {access_token,..} if access_token==expected)
            );
        }
        let lookup = EnvironmentLookup::new(|_| None)
            .with_descriptor_lookup(|| Some(descriptor(true, false)));
        let provider = EnvironmentOAuthCredentialProvider::live(lookup, None);
        assert!(
            matches!(provider.load(&scope).await.unwrap(),Credential::AnthropicOAuth {access_token,..} if access_token=="fd-token")
        );
    }
}
