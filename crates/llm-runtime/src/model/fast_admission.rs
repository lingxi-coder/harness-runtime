//! Native Fast policy, backed by the SDK organization-status request.
use crate::{Credential, Transport};
use lingxi_core::host::auth::AccountChangeObserver;
use lingxi_core::host::fast_mode::{Decline, Inputs, OrgStatus, StatusSource};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Default)]
pub struct Policy {
    pub flag_fast: bool,
    pub policy_fast: Option<bool>,
    pub policy_session_opt_in: Option<bool>,
    pub allowed_models: Option<Vec<String>>,
    pub model_overrides: std::collections::BTreeMap<String, String>,
    pub cached_org_enabled: bool,
    pub remote: bool,
    pub agent_owned_remote: bool,
    pub agent_sdk: bool,
    pub session_access_token: bool,
    pub no_user_account: bool,
    pub kill_switch: Option<String>,
}
#[derive(Clone)]
struct Entry {
    identity: [u8; 32],
    status: OrgStatus,
    last_fetch: Option<Instant>,
    oauth: bool,
}
#[derive(Default)]
struct State {
    epoch: u64,
    profiles: HashMap<String, Entry>,
}
#[derive(Default)]
pub struct Availability {
    state: Mutex<State>,
    prefetch: tokio::sync::Mutex<()>,
}
impl AccountChangeObserver for Availability {
    fn account_changed(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.epoch = state.epoch.wrapping_add(1);
        state.profiles.clear();
    }
}
pub(crate) struct Account {
    pub profile: String,
    pub base_url: String,
    pub model: String,
    pub default_model: String,
    pub first_party: bool,
    pub credential: Option<Credential>,
    pub alternate_api_key: Option<String>,
    pub oauth_scope: Option<crate::CredentialScope>,
}

fn status_credential<'a>(
    credential: Option<&'a Credential>,
    alternate_api_key: Option<&'a str>,
    policy: &Policy,
) -> Option<lingxi_llm_client::providers::anthropic::fast_mode::FastModeCredential<'a>> {
    use lingxi_llm_client::providers::anthropic::fast_mode::FastModeCredential;
    match credential {
        Some(Credential::AnthropicOAuth {
            access_token,
            scopes,
        }) if !access_token.is_empty()
            && lingxi_core::host::fast_mode::oauth_status_allowed(
                scopes,
                policy.session_access_token,
                policy.no_user_account,
            ) =>
        {
            Some(FastModeCredential::OAuth(access_token))
        }
        Some(Credential::ApiKey(key)) if !key.is_empty() => Some(FastModeCredential::ApiKey(key)),
        _ => alternate_api_key
            .filter(|key| !key.is_empty())
            .map(FastModeCredential::ApiKey),
    }
}
#[derive(Clone, Copy)]
pub(crate) struct Binding {
    pub identity: [u8; 32],
    pub generation: u64,
}
impl Binding {
    pub(crate) fn new(credential: Option<&Credential>, generation: u64) -> Self {
        Self {
            identity: credential_identity(credential),
            generation,
        }
    }
}
fn guessed(policy: &Policy, epoch: u64) -> OrgStatus {
    if policy.cached_org_enabled && !policy.agent_owned_remote && epoch == 0 {
        OrgStatus::Enabled
    } else {
        OrgStatus::Disabled {
            reason: Decline::Unknown,
            source: StatusSource::Guess,
        }
    }
}
pub(crate) fn credential_identity(credential: Option<&Credential>) -> [u8; 32] {
    let (kind, token) = match credential {
        Some(Credential::ApiKey(value)) => ("api-key", value.as_str()),
        Some(Credential::BearerToken(value)) => ("oauth", value.as_str()),
        Some(Credential::AnthropicOAuth { access_token, .. }) => ("oauth", access_token.as_str()),
        _ => ("none", ""),
    };
    Sha256::digest(format!("{kind}:{token}").as_bytes()).into()
}
impl Availability {
    pub(crate) fn generation(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .epoch
    }
    pub(crate) fn observed_for(
        &self,
        profile: &str,
        policy: &Policy,
        binding: Binding,
    ) -> (OrgStatus, bool) {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.epoch == binding.generation {
            match state.profiles.get(profile) {
                Some(entry) if entry.identity == binding.identity || entry.oauth => {
                    return (entry.status.clone(), entry.oauth);
                }
                Some(entry)
                    if binding.identity == credential_identity(None)
                        && (matches!(
                            entry.status,
                            OrgStatus::Disabled {
                                source: StatusSource::Server,
                                ..
                            }
                        ) || (policy.agent_owned_remote
                            && entry.status == OrgStatus::Enabled)) =>
                {
                    return (entry.status.clone(), entry.oauth);
                }
                None => return (guessed(policy, state.epoch), false),
                _ => {}
            }
        }
        (
            OrgStatus::Disabled {
                reason: Decline::Unknown,
                source: StatusSource::Guess,
            },
            false,
        )
    }
    pub(crate) fn observed(&self, profile: &str, policy: &Policy) -> (OrgStatus, bool) {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.profiles.get(profile).map_or_else(
            || (guessed(policy, state.epoch), false),
            |entry| (entry.status.clone(), entry.oauth),
        )
    }
    pub(crate) async fn refresh<F, Fut>(
        &self,
        account: &Account,
        policy: &Policy,
        transport: &dyn Transport,
        user_agent: &str,
        renew: F,
    ) where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<Option<Credential>, crate::LlmError>>,
    {
        if !account.first_party || policy.no_user_account {
            return;
        }
        let _prefetch = self.prefetch.lock().await;
        let mut live_credential = account.credential.clone();
        let credential = status_credential(
            live_credential.as_ref(),
            account.alternate_api_key.as_deref(),
            policy,
        );
        // The status account and refusal copy remain OAuth-owned even when the
        // status GET falls back to its alternate API key (native fn vs g).
        let oauth = matches!(account.credential, Some(Credential::AnthropicOAuth { .. }));
        let mut profile_scope = matches!(&live_credential, Some(Credential::AnthropicOAuth { access_token, scopes }) if !access_token.is_empty() && lingxi_core::host::fast_mode::oauth_profile_scope(scopes));
        let mut eligible_oauth = matches!(
            credential,
            Some(lingxi_llm_client::providers::anthropic::fast_mode::FastModeCredential::OAuth(_))
        );
        let identity = credential_identity(account.credential.as_ref());
        let (epoch, previous) = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let epoch = state.epoch;
            let previous = state.profiles.get(&account.profile).cloned();
            // Native Xu preserves a server-enabled agent-owned remote session
            // when no local user credential remains available.
            if credential.is_none()
                && previous.as_ref().is_some_and(|entry| {
                    matches!(
                        entry.status,
                        OrgStatus::Disabled {
                            source: StatusSource::Server,
                            ..
                        }
                    ) || (policy.agent_owned_remote && entry.status == OrgStatus::Enabled)
                })
            {
                return;
            }
            let seed = guessed(policy, epoch);
            let entry = state
                .profiles
                .entry(account.profile.clone())
                .or_insert(Entry {
                    identity,
                    status: seed.clone(),
                    last_fetch: None,
                    oauth,
                });
            if entry.identity != identity {
                // Native account observers exclude login/logout. Ordinary
                // OAuth renewal keeps the account and its prefetch window.
                if entry.oauth && oauth {
                    entry.identity = identity;
                } else {
                    *entry = Entry {
                        identity,
                        status: seed,
                        last_fetch: None,
                        oauth,
                    };
                }
            }
            if policy.agent_owned_remote && credential.is_none() {
                return;
            }
            if policy.skip_org() {
                if !matches!(
                    entry.status,
                    OrgStatus::Disabled {
                        source: StatusSource::Server,
                        ..
                    }
                ) {
                    entry.status = OrgStatus::Enabled;
                }
                return;
            }
            if credential.is_none() {
                if !matches!(
                    entry.status,
                    OrgStatus::Disabled {
                        source: StatusSource::Server,
                        ..
                    }
                ) {
                    entry.status = if policy.cached_org_enabled && epoch == 0 {
                        OrgStatus::Enabled
                    } else {
                        OrgStatus::Disabled {
                            reason: Decline::Preference,
                            source: StatusSource::Guess,
                        }
                    };
                }
                return;
            }
            if entry
                .last_fetch
                .is_some_and(|last| last.elapsed() < Duration::from_secs(30))
            {
                return;
            }
            entry.last_fetch = Some(Instant::now());
            (epoch, entry.status.clone())
        };
        let mut result = lingxi_llm_client::providers::anthropic::fast_mode::fetch_status(
            transport,
            &account.base_url,
            credential.expect("status credential admitted before prefetch"),
            user_agent,
        )
        .await;
        let mut renewed_identity = None;
        if profile_scope
            && result
                .as_ref()
                .is_err_and(|error| error.requests_oauth_refresh())
        {
            if let Ok(renewed) = renew().await {
                if let Some(renewed) = renewed {
                    renewed_identity = Some(credential_identity(Some(&renewed)));
                    live_credential = Some(renewed);
                }
                profile_scope = matches!(&live_credential, Some(Credential::AnthropicOAuth { scopes, .. }) if lingxi_core::host::fast_mode::oauth_profile_scope(scopes));
                let next = status_credential(
                    live_credential.as_ref(),
                    account.alternate_api_key.as_deref(),
                    policy,
                );
                eligible_oauth = matches!(next, Some(lingxi_llm_client::providers::anthropic::fast_mode::FastModeCredential::OAuth(_)));
                if let Some(next) = next {
                    result = lingxi_llm_client::providers::anthropic::fast_mode::fetch_status(
                        transport,
                        &account.base_url,
                        next,
                        user_agent,
                    )
                    .await;
                }
            }
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.epoch != epoch {
            return;
        }
        let entry = state
            .profiles
            .get_mut(&account.profile)
            .expect("prefetch owns entry");
        if let Some(identity) = renewed_identity {
            entry.identity = identity;
        }
        entry.status = match result {
            Ok(status) if status.enabled => OrgStatus::Enabled,
            Ok(status) => OrgStatus::Disabled {
                reason: Decline::org_reason(status.disabled_reason.as_deref()),
                source: StatusSource::Server,
            },
            Err(_)
                if matches!(
                    previous,
                    OrgStatus::Disabled {
                        source: StatusSource::Server,
                        ..
                    }
                ) || (policy.agent_owned_remote && previous == OrgStatus::Enabled) =>
            {
                previous
            }
            Err(_) if policy.cached_org_enabled && !policy.agent_owned_remote && epoch == 0 => {
                OrgStatus::Enabled
            }
            Err(error) => OrgStatus::Disabled {
                reason: if eligible_oauth
                    && !profile_scope
                    && matches!(error, lingxi_llm_client::providers::anthropic::fast_mode::FastModeStatusError::Http { status: 403, body } if body["error"]["type"] == "permission_error")
                {
                    Decline::Unknown
                } else {
                    Decline::NetworkError
                },
                source: StatusSource::Guess,
            },
        };
    }
}
impl Policy {
    fn skip_org(&self) -> bool {
        crate::structured_output::bool_environment(branding::SKIP_FAST_MODE_ORG_CHECK_ENV)
            && !self.agent_owned_remote
    }
    pub(crate) fn inputs(
        &self,
        account: &Account,
        org: OrgStatus,
        oauth: bool,
        non_interactive: bool,
    ) -> Inputs {
        Inputs {
            first_party: account.first_party,
            disabled: crate::structured_output::bool_environment(branding::DISABLE_FAST_MODE_ENV),
            kill_switch: self.kill_switch.clone(),
            default_model_allowed: super::allowlist::is_model_allowed(
                &account.default_model,
                self.allowed_models.as_deref(),
                Some(&self.model_overrides),
            ),
            remote: self.remote,
            model_fast: super::fast::model_allowed(&account.model),
            model_allowed: super::allowlist::is_model_allowed(
                &account.model,
                self.allowed_models.as_deref(),
                Some(&self.model_overrides),
            ),
            flag_fast: self.flag_fast,
            policy_fast: self.policy_fast,
            policy_session_opt_in: self.policy_session_opt_in,
            non_interactive,
            agent_sdk: self.agent_sdk,
            session_only: false,
            agent_owned_remote: self.agent_owned_remote,
            skip_org: self.skip_org(),
            skip_network: crate::structured_output::bool_environment(
                branding::SKIP_FAST_MODE_NETWORK_ERRORS_ENV,
            ),
            org,
            oauth,
            ..Inputs::default()
        }
    }
}
pub type PolicySource = Arc<dyn Fn() -> Policy + Send + Sync>;

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn native_fast_auth_source_selection_cases() {
        use lingxi_llm_client::providers::anthropic::fast_mode::FastModeCredential;
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/fast_auth_sources_2_1_287.json"
        ))
        .unwrap();
        for row in fixture["cases"].as_array().unwrap() {
            let input = &row["input"];
            let credential =
                input["token"]
                    .as_str()
                    .map(|access_token| Credential::AnthropicOAuth {
                        access_token: access_token.into(),
                        scopes: serde_json::from_value(input["scopes"].clone()).unwrap(),
                    });
            let policy = Policy {
                session_access_token: input["session_token"].as_bool().unwrap(),
                no_user_account: input["no_user_account"].as_bool().unwrap(),
                ..Policy::default()
            };
            let actual = status_credential(credential.as_ref(), input["key"].as_str(), &policy)
                .map(|credential| match credential {
                    FastModeCredential::OAuth(token) => serde_json::json!({"accessToken":token}),
                    FastModeCredential::ApiKey(key) => serde_json::json!({"apiKey":key}),
                });
            assert_eq!(
                serde_json::to_value(actual).unwrap(),
                row["expected"],
                "{row}"
            );
        }
    }

    struct Reply {
        calls: AtomicUsize,
        status: u16,
        body: &'static str,
        invalidate: Option<Arc<Availability>>,
    }
    #[async_trait]
    impl Transport for Reply {
        async fn send(
            &self,
            _: lingxi_llm_client::HttpRequest,
        ) -> Result<lingxi_llm_client::StreamResponse, lingxi_llm_client::protocol::LlmError>
        {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(availability) = &self.invalidate {
                availability.account_changed();
            }
            Ok(lingxi_llm_client::HttpResponse {
                status: self.status,
                headers: vec![],
                body: self.body.as_bytes().to_vec().into(),
            }
            .into())
        }
    }
    fn account(oauth: bool) -> Account {
        Account {
            profile: "test".into(),
            base_url: "https://api.anthropic.com".into(),
            model: "claude-opus-5-5".into(),
            default_model: "claude-opus-5-5".into(),
            first_party: true,
            credential: Some(if oauth {
                Credential::AnthropicOAuth {
                    access_token: "test".into(),
                    scopes: vec!["user:profile".into()],
                }
            } else {
                Credential::ApiKey("test".into())
            }),
            alternate_api_key: None,
            oauth_scope: None,
        }
    }
    fn reply(status: u16, body: &'static str) -> Reply {
        Reply {
            calls: AtomicUsize::new(0),
            status,
            body,
            invalidate: None,
        }
    }
    fn expire(availability: &Availability) {
        availability
            .state
            .lock()
            .unwrap()
            .profiles
            .get_mut("test")
            .unwrap()
            .last_fetch = Some(Instant::now() - Duration::from_secs(31));
    }
    #[tokio::test]
    async fn oauth_renewal_retains_the_same_account_permission_and_prefetch_window() {
        let availability = Availability::default();
        let policy = Policy::default();
        let transport = reply(200, r#"{"enabled":true}"#);
        let mut account = account(true);
        availability
            .refresh(&account, &policy, &transport, "test", || async { Ok(None) })
            .await;
        let generation = availability.generation();
        account.credential = Some(Credential::AnthropicOAuth {
            access_token: "renewed".into(),
            scopes: vec!["user:profile".into()],
        });
        let binding = Binding::new(account.credential.as_ref(), generation);
        assert_eq!(
            availability.observed_for("test", &policy, binding).0,
            OrgStatus::Enabled
        );
        availability
            .refresh(&account, &policy, &transport, "test", || async { Ok(None) })
            .await;
        assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
        availability.account_changed();
        assert_ne!(
            availability.observed_for("test", &policy, binding).0,
            OrgStatus::Enabled
        );
    }
    #[tokio::test]
    async fn endpoint_scope_refusal_is_unknown_only_for_non_user_oauth() {
        for (oauth, oauth_user, reason) in [
            (true, false, Decline::Unknown),
            (true, true, Decline::NetworkError),
            (false, false, Decline::NetworkError),
        ] {
            let availability = Availability::default();
            let mut account = account(oauth);
            if oauth && !oauth_user {
                account.credential = Some(Credential::AnthropicOAuth {
                    access_token: "ccr".into(),
                    scopes: vec!["user:ccr_inference".into()],
                });
            }
            let policy = Policy {
                session_access_token: true,
                ..Policy::default()
            };
            availability
                .refresh(
                    &account,
                    &policy,
                    &reply(403, r#"{"error":{"type":"permission_error"}}"#),
                    "test",
                    || async { Ok(None) },
                )
                .await;
            assert_eq!(
                availability.observed("test", &Policy::default()).0,
                OrgStatus::Disabled {
                    reason,
                    source: StatusSource::Guess
                }
            );
        }
    }
    #[tokio::test]
    async fn authoritative_refusal_survives_fetch_error_and_missing_credential() {
        let availability = Availability::default();
        let policy = Policy {
            cached_org_enabled: true,
            ..Policy::default()
        };
        let mut account = account(false);
        availability
            .refresh(
                &account,
                &policy,
                &reply(200, r#"{"enabled":false,"disabled_reason":"preference"}"#),
                "test",
                || async { Ok(None) },
            )
            .await;
        let expected = OrgStatus::Disabled {
            reason: Decline::Preference,
            source: StatusSource::Server,
        };
        expire(&availability);
        availability
            .refresh(
                &account,
                &policy,
                &reply(500, "failure"),
                "test",
                || async { Ok(None) },
            )
            .await;
        assert_eq!(availability.observed("test", &policy).0, expected);
        assert_eq!(
            availability
                .observed_for(
                    "test",
                    &policy,
                    Binding::new(None, availability.generation())
                )
                .0,
            expected
        );
        account.credential = None;
        let transport = reply(200, r#"{"enabled":true}"#);
        availability
            .refresh(&account, &policy, &transport, "test", || async { Ok(None) })
            .await;
        assert_eq!(transport.calls.load(Ordering::SeqCst), 0);
        assert_eq!(availability.observed("test", &policy).0, expected);
    }
    #[tokio::test]
    async fn remote_server_enable_survives_local_credential_loss_and_network_failure() {
        let availability = Availability::default();
        let policy = Policy {
            agent_owned_remote: true,
            ..Policy::default()
        };
        let mut account = account(false);
        availability
            .refresh(
                &account,
                &policy,
                &reply(200, r#"{"enabled":true}"#),
                "test",
                || async { Ok(None) },
            )
            .await;
        expire(&availability);
        availability
            .refresh(
                &account,
                &policy,
                &reply(500, "failure"),
                "test",
                || async { Ok(None) },
            )
            .await;
        account.credential = None;
        availability
            .refresh(
                &account,
                &policy,
                &reply(500, "failure"),
                "test",
                || async { Ok(None) },
            )
            .await;
        assert_eq!(availability.observed("test", &policy).0, OrgStatus::Enabled);
        assert_eq!(
            availability
                .observed_for(
                    "test",
                    &policy,
                    Binding::new(None, availability.generation())
                )
                .0,
            OrgStatus::Enabled
        );
    }
    #[tokio::test]
    async fn late_prefetch_cannot_restore_admission_after_account_change() {
        let availability = Arc::new(Availability::default());
        let mut transport = reply(200, r#"{"enabled":true}"#);
        transport.invalidate = Some(availability.clone());
        availability
            .refresh(
                &account(false),
                &Policy::default(),
                &transport,
                "test",
                || async { Ok(None) },
            )
            .await;
        assert!(availability.state.lock().unwrap().profiles.is_empty());
        assert_eq!(
            availability
                .observed(
                    "test",
                    &Policy {
                        cached_org_enabled: true,
                        ..Policy::default()
                    }
                )
                .0,
            OrgStatus::Disabled {
                reason: Decline::Unknown,
                source: StatusSource::Guess
            }
        );
    }
    #[tokio::test]
    async fn concurrent_prefetches_share_the_window_and_logout_discards_cached_fallback() {
        let availability = Availability::default();
        let policy = Policy {
            cached_org_enabled: true,
            ..Policy::default()
        };
        let account = account(false);
        let transport = reply(200, r#"{"enabled":true}"#);
        tokio::join!(
            availability.refresh(&account, &policy, &transport, "test", || async { Ok(None) }),
            availability.refresh(&account, &policy, &transport, "test", || async { Ok(None) })
        );
        assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
        availability.account_changed();
        availability
            .refresh(
                &account,
                &policy,
                &reply(500, "failure"),
                "test",
                || async { Ok(None) },
            )
            .await;
        assert_eq!(
            availability.observed("test", &policy).0,
            OrgStatus::Disabled {
                reason: Decline::NetworkError,
                source: StatusSource::Guess
            }
        );
    }
}
