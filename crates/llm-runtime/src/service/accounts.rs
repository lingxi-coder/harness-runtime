//! Account routing and credential revisions for model calls.

use crate::model::retry_scope::ModelCallRetryScope;
use crate::service::response::PromptCacheOverageState;
use crate::service::retry::allows_account_switch;
use crate::service::ApiService;
use crate::{LlmError, LlmRequest};
use std::sync::Arc;
use std::time::Duration;

pub(super) struct ApiServiceAccountObserver {
    pub(super) fast_availability: Arc<crate::model::fast_admission::Availability>,
    pub(super) prompt_cache_overage: Arc<PromptCacheOverageState>,
}

impl lingxi_core::host::auth::AccountChangeObserver for ApiServiceAccountObserver {
    fn account_changed(&self) {
        self.fast_availability.account_changed();
        self.prompt_cache_overage.reset_for_account_change();
    }
}

impl ApiService {
    /// Apply an account pool's next hop once for the current prepared route.
    pub(super) async fn select_attempt_account(
        &self,
        coordinator: Option<&mut crate::accounts::AttemptCoordinator>,
        scope: &ModelCallRetryScope,
        request: &mut LlmRequest,
    ) -> Result<(), LlmError> {
        let Some(coordinator) =
            coordinator.filter(|coordinator| coordinator.active_scope().is_none())
        else {
            return Ok(());
        };
        let selected = coordinator.select_with_scope(scope).await;
        self.report_account_routing(coordinator.routing_event(
            scope.account_call_id(),
            selected.as_ref().ok().map(|hop| hop.profile_name.clone()),
        ))
        .await;
        let hop = selected?;
        request.profile = Some(hop.profile_name);
        request.input.model = hop.request_model;
        request.execution.selected_account = coordinator.active_scope();
        request
            .execution
            .request_credentials
            .get_or_insert_with(Default::default);
        // Continuations are tied to account identity. Refreshing the same
        // account's token keeps that identity stable.
        request.execution.account_scope = request
            .execution
            .selected_account
            .as_ref()
            .map(|scope| serde_json::to_string(scope).expect("account scope"));
        Ok(())
    }

    pub(super) async fn retry_after_preparation_failure(
        &self,
        request: &LlmRequest,
        coordinator: Option<&mut crate::accounts::AttemptCoordinator>,
        error: &LlmError,
        scope: &ModelCallRetryScope,
    ) -> Result<bool, LlmError> {
        let Some(coordinator) = coordinator else {
            return Ok(false);
        };
        if !self
            .record_pool_failure(request, coordinator, error, None)
            .await?
        {
            return Ok(false);
        }
        scope.mark_account_failed(coordinator.last_failed_account_key());
        Ok(allows_account_switch(request))
    }

    pub fn set_account_routing_reporter(
        &self,
        reporter: Arc<dyn crate::accounts::AccountRoutingReporter>,
    ) {
        *self
            .account_routing_reporter
            .write()
            .unwrap_or_else(|e| e.into_inner()) = Some(reporter);
    }

    pub(super) async fn report_account_routing(
        &self,
        event: lingxi_core::host::provider_accounts::ProviderAccountRoutingEvent,
    ) {
        let reporter = self
            .account_routing_reporter
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(reporter) = reporter {
            reporter.report(event).await;
        }
    }

    pub(super) async fn make_account_coordinator(
        &self,
        request: &LlmRequest,
        scope: &ModelCallRetryScope,
    ) -> Result<Option<crate::accounts::AttemptCoordinator>, LlmError> {
        match self
            .client()
            .account_coordinator(&request.input.model, request.profile.as_deref())
        {
            Ok(mut coordinator) => {
                if let Some(coordinator) = coordinator.as_mut() {
                    let runtime = self.client();
                    coordinator.retain_supported(|candidate| {
                        runtime.account_supports_request(request, candidate)
                    });
                    let resource_bound = request.input.continuation.is_some()
                        || request.execution.file_account_scope.is_some()
                        || request.execution.expected_computer_binding.is_some()
                        || request.execution.computer_submission.is_some();
                    if resource_bound {
                        let mut scopes = Vec::new();
                        if let Some(selected) = request.execution.selected_account.as_ref() {
                            scopes.push(selected.clone());
                        }
                        let raw_scopes = [
                            request.execution.account_scope.as_deref(),
                            request.execution.file_account_scope.as_deref(),
                            request
                                .input
                                .continuation
                                .as_ref()
                                .map(|continuation| continuation.account_scope.as_str()),
                            request
                                .execution
                                .expected_computer_binding
                                .as_ref()
                                .map(|binding| binding.account.as_str()),
                        ];
                        for raw in raw_scopes.into_iter().flatten() {
                            scopes.push(
                                serde_json::from_str::<crate::accounts::AccountResourceScope>(raw)
                                    .map_err(|_| LlmError::InvalidRequest {
                                        message: "account-owned resource scope is invalid".into(),
                                    })?,
                            );
                        }
                        let scope = scopes.first().ok_or_else(|| LlmError::InvalidRequest {
                            message:
                                "account-owned resources require their original account identity"
                                    .into(),
                        })?;
                        if scopes.iter().any(|other| other != scope) {
                            return Err(LlmError::InvalidRequest {message:"account-owned resources belong to different identities or storage domains".into()});
                        }
                        let profile = format!("account.{}", scope.account_id);
                        if request.profile.as_deref().is_some_and(|explicit| {
                            explicit.starts_with("account.") && explicit != profile
                        }) || request
                            .execution
                            .expected_computer_binding
                            .as_ref()
                            .is_some_and(|binding| binding.profile != profile)
                        {
                            return Err(LlmError::InvalidRequest {
                                message: "resource owner differs from the selected account profile"
                                    .into(),
                            });
                        }
                        if !coordinator.pin_scope(scope) {
                            return Err(LlmError::InvalidRequest {message:"account-owned resource belongs to a replaced account identity or storage domain".into()});
                        }
                    }
                }
                Ok(coordinator)
            }
            Err(error) => {
                if let LlmError::AllAccountsUnavailable {
                    accounts,
                    earliest_recovery_ms,
                } = &error
                {
                    self.report_account_routing(
                        lingxi_core::host::provider_accounts::ProviderAccountRoutingEvent {
                            logical_call_id: scope.account_call_id(),
                            model: request.input.model.clone(),
                            attempt: 0,
                            status: "all_unavailable".into(),
                            selected_account_id: None,
                            profile: None,
                            identity_generation: None,
                            config_revision: None,
                            failed_accounts: accounts
                                .iter()
                                .map(crate::accounts::AccountUnavailable::as_public_failure)
                                .collect(),
                            earliest_recovery_ms: *earliest_recovery_ms,
                        },
                    )
                    .await;
                }
                Err(error)
            }
        }
    }

    pub fn account_change_observer(
        &self,
    ) -> Arc<dyn lingxi_core::host::auth::AccountChangeObserver> {
        self.account_change_observer.clone()
    }

    /// Keep an account observer alive while the auth owner retains only its
    /// weak callback. Desktop uses this for its generation-guarded startup
    /// profile refresh writer.
    pub fn retain_account_change_observer(
        mut self,
        observer: Arc<dyn lingxi_core::host::auth::AccountChangeObserver>,
    ) -> Self {
        self.account_change_observer_keepalive.push(observer);
        self
    }

    pub(super) fn captured_request_credential(
        request: &LlmRequest,
        profile: &str,
    ) -> Option<crate::Credential> {
        let credentials = request.execution.request_credentials.as_ref()?;
        let snapshots = credentials
            .snapshots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        snapshots.get(profile)?.credential.get()?.clone()
    }

    pub(super) fn captured_request_credential_revision(
        request: &LlmRequest,
        profile: &str,
    ) -> Option<u64> {
        let credentials = request.execution.request_credentials.as_ref()?;
        let snapshots = credentials
            .snapshots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let snapshot = snapshots.get(profile)?;
        if snapshot.credential.get().is_some() {
            snapshot.credential_revision.get().copied().flatten()
        } else {
            let revision = *snapshot
                .rejected_credential_revision
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            revision
        }
    }

    async fn account_auth_failure_revision(
        &self,
        request: &LlmRequest,
        error: &LlmError,
    ) -> Result<Option<u64>, LlmError> {
        let Some(profile) = request.profile.as_deref() else {
            return Ok(None);
        };
        let rejected = Self::captured_request_credential_revision(request, profile);
        self.check_account_auth_failure_revision(request, error, rejected, true)
            .await
    }

    async fn check_account_auth_failure_revision(
        &self,
        request: &LlmRequest,
        error: &LlmError,
        rejected: Option<u64>,
        check_material: bool,
    ) -> Result<Option<u64>, LlmError> {
        let Some(profile) = request.profile.as_deref() else {
            return Ok(rejected);
        };
        if matches!(
            error.classification_source(),
            LlmError::Authentication { .. } | LlmError::OAuthRefreshDead
        ) {
            let current = self
                .client()
                .current_account_credential_revision(&request.input.model, profile)
                .await?;
            if current != rejected {
                return Err(error.clone());
            }
            if let Some(credential) =
                Self::captured_request_credential(request, profile).filter(|_| check_material)
            {
                if !self
                    .client()
                    .rejected_account_credential_is_current(profile, &credential)
                    .await?
                {
                    return Err(error.clone());
                }
            }
        }
        Ok(rejected)
    }

    pub(super) async fn record_pool_failure(
        &self,
        request: &LlmRequest,
        coordinator: &mut crate::accounts::AttemptCoordinator,
        error: &LlmError,
        retry_after: Option<Duration>,
    ) -> Result<bool, LlmError> {
        let revision = self.account_auth_failure_revision(request, error).await?;
        coordinator.set_rejected_credential_revision(revision);
        if matches!(
            error.classification_source(),
            LlmError::Authentication { .. }
        ) && request.profile.as_deref().is_some_and(|profile| {
            self.client().derives_account_bearer(profile)
                && Self::captured_request_credential(request, profile).is_some()
        }) {
            return Ok(coordinator.derived_token_rejected().await);
        }
        let recorded = coordinator
            .failed_with_retry_after(error, retry_after)
            .await;
        if !recorded
            && matches!(
                error.classification_source(),
                LlmError::Authentication { .. } | LlmError::OAuthRefreshDead
            )
        {
            return Err(error.clone());
        }
        Ok(recorded)
    }

    pub(super) async fn record_pool_refresh_failure(
        &self,
        request: &LlmRequest,
        coordinator: &mut crate::accounts::AttemptCoordinator,
        failure: &crate::auth::provider::VersionedCredentialError,
    ) -> Result<bool, LlmError> {
        let error = &failure.error;
        let rejected = failure.credential_revision.or_else(|| {
            request
                .profile
                .as_deref()
                .and_then(|profile| Self::captured_request_credential_revision(request, profile))
        });
        let revision = self
            .check_account_auth_failure_revision(request, error, rejected, false)
            .await?;
        coordinator.set_rejected_credential_revision(revision);
        let recorded = coordinator.refresh_failed(error).await;
        if !recorded
            && matches!(
                error.classification_source(),
                LlmError::Authentication { .. } | LlmError::OAuthRefreshDead
            )
        {
            return Err(error.clone());
        }
        Ok(recorded)
    }

    pub(super) fn replace_request_credential(
        request: &mut LlmRequest,
        profile: &str,
        credential: crate::auth::provider::VersionedCredential,
    ) {
        let credentials = request
            .execution
            .request_credentials
            .get_or_insert_with(crate::RequestCredentials::default);
        let snapshot = Arc::new(crate::execution::RequestCredentialSnapshot {
            profile: profile.to_owned(),
            credential: tokio::sync::OnceCell::new(),
            credential_revision: std::sync::OnceLock::new(),
            rejected_credential_revision: std::sync::Mutex::new(None),
        });
        snapshot
            .credential
            .set(Some(credential.credential))
            .expect("new request credential snapshot is empty");
        let _ = snapshot
            .credential_revision
            .set(credential.credential_revision);
        credentials
            .snapshots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(profile.to_owned(), snapshot);
    }

    pub(super) async fn refresh_pool_account(
        &self,
        request: &mut LlmRequest,
        scope: &ModelCallRetryScope,
        error: &LlmError,
    ) -> Result<bool, crate::auth::provider::VersionedCredentialError> {
        if !matches!(
            error.classification_source(),
            LlmError::Authentication { .. }
        ) {
            return Ok(false);
        }
        let Some(account) = request.execution.selected_account.as_ref() else {
            return Ok(false);
        };
        let Some(profile) = request.profile.clone() else {
            return Ok(false);
        };
        let Some(rejected) = Self::captured_request_credential(request, &profile) else {
            return Ok(false);
        };
        if !matches!(
            rejected,
            crate::Credential::AnthropicOAuth { .. }
                | crate::Credential::ChatGptOAuth { .. }
                | crate::Credential::BearerToken(_)
        ) {
            return Ok(false);
        }
        let key = serde_json::to_string(account).expect("account scope");
        if !scope.take_account_renewal(key) {
            return Ok(false);
        }
        let refreshed = self
            .client()
            .refresh_rejected_account_credential(&request.input.model, Some(&profile), &rejected)
            .await?;
        if let Some(refreshed) = refreshed {
            Self::replace_request_credential(request, &profile, refreshed);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Validate an accounting route against this call's authoritative account
    /// candidates. A request payload cannot manufacture another provider route.
    pub fn authorized_account_route(
        &self,
        original: &crate::ResolvedRoute,
        selected: &crate::accounts::AccountResourceScope,
        actual: &crate::ResolvedRoute,
    ) -> Option<lingxi_core::host::provider_accounts::AccountMetadata> {
        let runtime = self.client();
        let coordinator = runtime
            .account_coordinator(&original.request_model, Some(&original.profile_name))
            .ok()??;
        let account = coordinator.permits(selected, &actual.profile_name, &actual.request_model)?;
        let mut resolved = runtime
            .resolve_media_route(&actual.request_model, Some(&actual.profile_name))
            .ok()?
            .main;
        let mut actual = actual.clone();
        resolved.connection_chain.clear();
        actual.connection_chain.clear();
        (resolved == actual).then_some(account)
    }

    /// Validate an audio session's owner independently of chat-model routing.
    /// The refresh happens before the check; no base credential is loaded.
    pub async fn audio_account_binding_is_current(
        &self,
        original_profile: &str,
        actual_profile: &str,
        scope: &str,
    ) -> bool {
        let Ok(runtime) = self.client.account_call_snapshot().await else {
            return false;
        };
        match runtime.service_account_coordinator(original_profile, "audio/binding") {
            Ok(Some(coordinator)) => {
                let Ok(scope) =
                    serde_json::from_str::<crate::accounts::AccountResourceScope>(scope)
                else {
                    return false;
                };
                coordinator
                    .permits(&scope, actual_profile, "audio/binding")
                    .is_some()
            }
            Ok(None) => {
                actual_profile == original_profile && scope.starts_with("explicit-service.")
            }
            Err(_) => false,
        }
    }
}
