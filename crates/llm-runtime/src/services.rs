//! Independent provider services for host applications.
//!
//! Keep one [`ProviderServices`] per configuration/transport lifetime. Its SDK
//! client shares HTTP resources and immutable configuration snapshots across
//! audio, image, embedding, batch and retrieval calls. Credentials, account
//! scopes and request deadlines are supplied on each operation.
//!
//! These operations do not enter [`crate::ApiService`] or its agent-turn retry,
//! permission and durable cost-accounting hooks. Hosts own authorization,
//! cancellation, storage and accounting for independent service operations.
//! Audio capture/playback remain host capabilities. Realtime sessions require
//! a separate [`realtime::RealtimeTransport`]; Responses WebSocket continuation
//! is not a bidirectional audio transport.

use std::sync::Arc;

/// The exact shared SDK version used by this runtime, including provider-specific
/// services and their request, response, error and resource-scope types.
pub use lingxi_llm_client as sdk;
pub use sdk::ClientSnapshot as ProviderServiceSnapshot;
pub use sdk::{audio, files, providers, realtime};

/// Shared service client using an explicitly selected usage region and transport.
#[derive(Clone)]
pub struct ProviderServices {
    client: sdk::LlmClient,
    transport: Arc<dyn sdk::Transport>,
    credential_source: Option<crate::ModelRuntime>,
}

impl ProviderServices {
    /// Presence/origin only for an exact service profile. The provider's source
    /// contract must not execute token refresh or reveal credential material.
    pub async fn service_credential_source(
        &self,
        profile: &str,
    ) -> Result<crate::CredentialSource, crate::LlmError> {
        let current = self.current_operation_services().await?;
        current
            .credential_source
            .as_ref()
            .expect("configured source")
            .service_credential_source_exact(profile)
            .await
    }
    /// Build services over a native SDK transport. This preserves streamed
    /// request bodies and raw binary responses when the transport supports them.
    pub fn with_transport(
        profiles: &[sdk::protocol::ProviderProfile],
        region: sdk::protocol::Region,
        transport: Arc<dyn sdk::Transport>,
    ) -> Result<Self, sdk::BuildError> {
        Self::with_configured_transport(profiles, region, transport, |_| {})
    }

    /// Build services with host authenticators, clocks, attachment resolution or
    /// account sources registered on the same shared SDK builder.
    pub fn with_configured_transport(
        profiles: &[sdk::protocol::ProviderProfile],
        region: sdk::protocol::Region,
        transport: Arc<dyn sdk::Transport>,
        configure: impl FnOnce(&mut sdk::LlmClientBuilder),
    ) -> Result<Self, sdk::BuildError> {
        let mut builder =
            sdk::LlmClientBuilder::with_transport(transport.clone(), profiles).with_region(region);
        configure(&mut builder);
        Ok(Self {
            client: builder.build()?,
            transport,
            credential_source: None,
        })
    }

    pub(crate) fn with_credential_source(mut self, source: crate::ModelRuntime) -> Self {
        self.credential_source = Some(source);
        self
    }

    /// Resolve the exact configured key/token for an independent operation.
    /// This stays in Rust and uses the same source as inference, including
    /// authorized environment and host-managed sources. It never guesses an ID.
    pub async fn service_credential(
        &self,
        profile: &str,
    ) -> Result<sdk::protocol::Secret<String>, crate::LlmError> {
        let source = self.credential_source.as_ref().ok_or_else(|| {
            crate::LlmError::UnsupportedCapability {
                capability: "independent services have no configured host credential source".into(),
            }
        })?;
        source.service_credential(profile).await
    }

    /// Reject a service configuration which no longer matches the exact
    /// profile in the caller's current catalog scope. An absent credential
    /// source or profile is not sufficient authority to use a stale route.
    pub fn profile_is_current(&self, profile: &str) -> bool {
        let Some(source) = &self.credential_source else {
            return false;
        };
        source.same_service_profile(&source.current(), profile)
    }

    /// Compare this bound service with another runtime's scoped execution
    /// profile. Scheduled children retain the parent's credential authority,
    /// but may use it only for the exact configuration selected by the child.
    pub fn profile_matches_current_of(&self, other: &Self, profile: &str) -> bool {
        let (Some(bound), Some(other_source)) = (&self.credential_source, &other.credential_source)
        else {
            return false;
        };
        self.snapshot().region() == other.snapshot().region()
            && self.profile_is_current(profile)
            && bound.same_service_profile(&other_source.current(), profile)
    }

    /// Access all SDK services without duplicating their provider-specific APIs.
    /// Bind an exact profile using `provider::<providers::OpenAiClient>(name)`
    /// (or another provider client), then call its `audio()`, `files()` or other
    /// SDK resource. Credentials, account scopes and deadlines travel in each
    /// operation's `RequestOptions`.
    pub fn client(&self) -> &sdk::LlmClient {
        &self.client
    }

    /// Reuse the same transport for explicit-scope services such as Anthropic
    /// Skills or provider-specific realtime setup over HTTP.
    pub fn transport(&self) -> &dyn sdk::Transport {
        self.transport.as_ref()
    }

    /// Pin one configuration revision for related resource and model operations.
    pub fn snapshot(&self) -> ProviderServiceSnapshot {
        self.client.snapshot()
    }
}

/// One explicitly scoped independent operation. This owns a business probe
/// lease; callers settle it after the operation and never replay output here.
pub struct SelectedServiceAccount {
    pub actual_profile: String,
    pub account_scope: String,
    pub account_id: Option<String>,
    pub identity_generation: Option<u64>,
    pub credential: sdk::protocol::Secret<String>,
    pub services: ProviderServices,
    coordinator: Option<crate::accounts::AttemptCoordinator>,
    credential_revision: Option<u64>,
}
impl std::fmt::Debug for SelectedServiceAccount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SelectedServiceAccount")
            .field("actual_profile", &self.actual_profile)
            .field("account_id", &self.account_id)
            .field("identity_generation", &self.identity_generation)
            .finish_non_exhaustive()
    }
}
impl SelectedServiceAccount {
    /// Record a typed SDK service failure, preserving machine codes and reset
    /// timing. This settles health only and never retries the operation.
    pub async fn failed_sdk(&mut self, error: &sdk::protocol::LlmError) -> bool {
        let retry_after = match error.classification_source() {
            sdk::protocol::LlmError::ProviderResponse { retry_after, .. } => *retry_after,
            _ => None,
        };
        self.failed_with_retry_after(&crate::upstream::error(error.clone()), retry_after)
            .await
    }

    /// Preserve structured provider machine codes and Retry-After through any
    /// typed audio adapter wrappers. This only records health; it never replays.
    pub async fn failed_audio(&mut self, error: &audio::AudioError) -> bool {
        let mut source: Option<&(dyn std::error::Error + 'static)> = Some(error);
        while let Some(current) = source {
            if let Some(provider) = current.downcast_ref::<sdk::protocol::LlmError>() {
                return self.failed_sdk(provider).await;
            }
            if let Some(sdk::providers::openai::audio::AudioError::Provider {
                status, body, ..
            }) = current.downcast_ref::<sdk::providers::openai::audio::AudioError>()
            {
                // Reuse the SDK's structured machine-code classifier; a 429
                // alone never distinguishes quota exhaustion from throttling.
                let provider =
                    sdk::codecs::openai::chat::classify_error(*status, body, None, false);
                return self.failed(&crate::upstream::error(provider)).await;
            }
            source = current.source();
        }
        false
    }
    pub async fn succeeded(&mut self) {
        if let Some(coordinator) = self.coordinator.as_mut() {
            coordinator.succeeded().await;
        }
    }
    pub async fn failed(&mut self, error: &crate::LlmError) -> bool {
        self.failed_with_retry_after(error, None).await
    }
    async fn failed_with_retry_after(
        &mut self,
        error: &crate::LlmError,
        retry_after: Option<std::time::Duration>,
    ) -> bool {
        if self.coordinator.is_some()
            && matches!(
                error.classification_source(),
                crate::LlmError::Authentication { .. } | crate::LlmError::OAuthRefreshDead
            )
        {
            let Some(source) = self.services.credential_source.as_ref() else {
                return false;
            };
            if !source
                .current_service_credential_revision(&self.actual_profile)
                .await
                .is_ok_and(|current| current == self.credential_revision)
            {
                return false;
            }
        }
        match self.coordinator.as_mut() {
            Some(coordinator) => {
                coordinator.set_rejected_credential_revision(self.credential_revision);
                coordinator
                    .failed_with_retry_after(error, retry_after)
                    .await
            }
            None => false,
        }
    }
}
fn audio_eligible(
    services: &ProviderServices,
    profile: &str,
    operation: audio::AudioOperation,
    model: Option<&str>,
    scope: &str,
) -> Option<audio::AudioCapabilities> {
    let capabilities = services
        .client()
        .audio()
        .capabilities(&audio::AudioRoute::new(profile, scope))
        .ok()?;
    let descriptor = capabilities.operation(operation)?;
    if !(descriptor.common_adapter
        || (operation == audio::AudioOperation::NativeRealtime
            && capabilities.native_realtime_contract.is_some()))
        || capabilities.model(operation, model).is_err()
    {
        return None;
    }
    Some(capabilities)
}
impl ProviderServices {
    /// Resolve a provider file on its recorded account and current execution
    /// binding. Invalid or stale resource identities are rejected before any
    /// credential read. This never selects another account or retries a download.
    pub async fn resolve_file_account(
        &self,
        resource: &files::ProviderFileRef,
    ) -> Result<SelectedServiceAccount, crate::LlmError> {
        let account_scope =
            resource
                .account_scope
                .as_deref()
                .ok_or_else(|| crate::LlmError::InvalidRequest {
                    message: "provider file has no managed account scope".into(),
                })?;
        let scope: crate::accounts::AccountResourceScope = serde_json::from_str(account_scope)
            .map_err(|_| crate::LlmError::InvalidRequest {
                message: "provider file account scope is not a managed account identity".into(),
            })?;
        if scope.account_id.is_empty()
            || scope.store_scope.is_empty()
            || resource.profile_name != format!("account.{}", scope.account_id)
        {
            return Err(crate::LlmError::InvalidRequest {
                message: "provider file profile does not match its account identity".into(),
            });
        }
        let current = self.current_operation_services().await?;
        let source = current
            .credential_source
            .as_ref()
            .expect("configured source");
        let snapshot = current.snapshot();
        let profile = snapshot.profile(&resource.profile_name).ok_or_else(|| {
            crate::LlmError::InvalidRequest {
                message: "provider file account binding no longer exists".into(),
            }
        })?;
        files::FileService::new(
            current.transport(),
            profile,
            None,
            None,
            Some(account_scope),
        )
        .validate_reference(resource)
        .map_err(crate::upstream::error)?;
        let mut coordinator = source
            .service_account_coordinator(&resource.profile_name, "files/download")?
            .ok_or_else(|| crate::LlmError::InvalidRequest {
                message: "provider file requires a managed account runtime".into(),
            })?;
        if !coordinator.pin_scope(&scope) {
            return Err(crate::LlmError::InvalidRequest {
                message: "provider file belongs to a removed or replaced account".into(),
            });
        }
        let actual_profile = coordinator.select().await?.profile_name;
        let (credential, credential_revision) =
            match source.service_credential_versioned(&actual_profile).await {
                Ok(credential) => credential,
                Err(failure) => {
                    let error = failure.error;
                    let auth_failure = matches!(
                        error.classification_source(),
                        crate::LlmError::Authentication { .. } | crate::LlmError::OAuthRefreshDead
                    );
                    if !auth_failure
                        || source
                            .current_service_credential_revision(&actual_profile)
                            .await
                            .is_ok_and(|revision| revision == failure.credential_revision)
                    {
                        coordinator.set_rejected_credential_revision(failure.credential_revision);
                        coordinator.failed(&error).await;
                    }
                    return Err(error);
                }
            };
        Ok(SelectedServiceAccount {
            actual_profile,
            account_scope: account_scope.to_owned(),
            account_id: Some(scope.account_id),
            identity_generation: Some(scope.identity_generation),
            credential,
            services: current,
            coordinator: Some(coordinator),
            credential_revision,
        })
    }

    async fn current_operation_services(&self) -> Result<Self, crate::LlmError> {
        let source = self.credential_source.as_ref().ok_or_else(|| {
            crate::LlmError::UnsupportedCapability {
                capability: "independent services have no configured host credential source".into(),
            }
        })?;
        let current = source.account_call_snapshot().await?;
        current
            .provider_services(self.snapshot().region(), self.transport.clone())
            .map_err(|_| crate::LlmError::InvalidRequest {
                message: "independent service account catalog is invalid".into(),
            })
    }
    /// Metadata-only capability projection. It never reads credentials, obtains
    /// a recovery lease, refreshes a token, or records a successful provider call.
    pub async fn audio_account_capabilities(
        &self,
        profile: &str,
        operation: audio::AudioOperation,
        model: Option<&str>,
    ) -> Result<Vec<audio::AudioCapabilities>, crate::LlmError> {
        let current = self.current_operation_services().await?;
        let source = current
            .credential_source
            .as_ref()
            .expect("configured source");
        let key = format!("audio/{operation:?}/{}", model.unwrap_or("<default>"));
        let profiles = match source.service_account_coordinator(profile, &key)? {
            Some(coordinator) => coordinator.candidate_profiles(),
            None => vec![profile.to_owned()],
        };
        Ok(profiles
            .into_iter()
            .filter_map(|profile| {
                audio_eligible(&current, &profile, operation, model, "metadata-preview")
            })
            .collect())
    }
    /// Resolve a real audio operation through the shared pool. Capability and
    /// exact audio model eligibility are checked before selection. The operation
    /// remains pinned to this result, including streaming and realtime sessions.
    pub async fn resolve_audio_account(
        &self,
        profile: &str,
        operation: audio::AudioOperation,
        model: Option<&str>,
        pinned_scope: Option<&str>,
    ) -> Result<SelectedServiceAccount, crate::LlmError> {
        let current = self.current_operation_services().await?;
        let source = current
            .credential_source
            .as_ref()
            .expect("configured source");
        let key = format!("audio/{operation:?}/{}", model.unwrap_or("<default>"));
        let mut coordinator = source.service_account_coordinator(profile, &key)?;
        if let Some(coordinator) = coordinator.as_mut() {
            coordinator.retain_supported(|candidate| {
                audio_eligible(
                    &current,
                    &candidate.profile_name,
                    operation,
                    model,
                    "capability-check",
                )
                .is_some()
            });
            coordinator.map_candidate_models(|candidate| {
                let capabilities =
                    audio_eligible(&current, candidate, operation, model, "capability-check")
                        .expect("retained capability-eligible candidate");
                let resolved = capabilities
                    .model(operation, model)
                    .expect("eligible audio model");
                format!(
                    "audio/{operation:?}/{}",
                    resolved.id.as_deref().unwrap_or("<no-model-selector>")
                )
            });
            if let Some(pinned) = pinned_scope {
                let scope = serde_json::from_str::<crate::accounts::AccountResourceScope>(pinned)
                    .map_err(|_| crate::LlmError::InvalidRequest {
                    message: "audio session account scope is not a managed account identity".into(),
                })?;
                if !coordinator.pin_scope(&scope) {
                    return Err(crate::LlmError::InvalidRequest {
                        message: "audio session belongs to a removed or replaced account".into(),
                    });
                }
            }
        }
        loop {
            let actual_profile = match coordinator.as_mut() {
                Some(coordinator) => coordinator.select().await?.profile_name,
                None => profile.to_owned(),
            };
            let (credential, credential_revision) =
                match source.service_credential_versioned(&actual_profile).await {
                    Ok(credential) => credential,
                    Err(failure) => {
                        let error = failure.error;
                        if let Some(coordinator) = coordinator.as_mut() {
                            if matches!(
                                error.classification_source(),
                                crate::LlmError::Authentication { .. }
                                    | crate::LlmError::OAuthRefreshDead
                            ) {
                                let current_revision = source
                                    .current_service_credential_revision(&actual_profile)
                                    .await?;
                                if current_revision != failure.credential_revision {
                                    return Err(error);
                                }
                            }
                            coordinator
                                .set_rejected_credential_revision(failure.credential_revision);
                            if pinned_scope.is_none() && coordinator.failed(&error).await {
                                continue;
                            }
                        }
                        return Err(error);
                    }
                };
            let scope = coordinator
                .as_ref()
                .and_then(crate::accounts::AttemptCoordinator::active_scope);
            let account_scope = if let Some(scope) = scope.as_ref() {
                serde_json::to_string(scope).expect("account resource scope")
            } else {
                use sha2::{Digest, Sha256};
                format!(
                    "explicit-service.{:x}",
                    Sha256::digest(credential.expose_secret().as_bytes())
                )
            };
            if scope.is_none() && pinned_scope.is_some_and(|expected| expected != account_scope) {
                return Err(crate::LlmError::InvalidRequest {
                    message: "audio session credential identity changed".into(),
                });
            }
            if audio_eligible(&current, &actual_profile, operation, model, &account_scope).is_none()
            {
                return Err(crate::LlmError::UnsupportedCapability {
                    capability: "audio operation/model is unavailable on the selected account"
                        .into(),
                });
            }
            return Ok(SelectedServiceAccount {
                actual_profile,
                account_scope,
                account_id: scope.as_ref().map(|scope| scope.account_id.clone()),
                identity_generation: scope.as_ref().map(|scope| scope.identity_generation),
                credential,
                services: current,
                coordinator,
                credential_revision,
            });
        }
    }
}

#[cfg(test)]
mod account_tests {
    use super::*;
    use crate::{
        BoxFuture, Credential, CredentialProvider, CredentialScope, LlmError, ProviderRequest,
        ProviderResponse, StreamingResponse,
    };
    use lingxi_core::host::provider_accounts::{AccountAuthKind, AccountMetadata, CredentialRef};
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    struct NoNetwork;
    impl crate::test_support::FixtureTransport for NoNetwork {
        fn execute<'a>(
            &'a self,
            _: &'a ProviderRequest,
        ) -> BoxFuture<'a, Result<ProviderResponse, LlmError>> {
            Box::pin(async { panic!("preview/resolution must not dispatch provider traffic") })
        }
        fn open_stream<'a>(
            &'a self,
            _: &'a ProviderRequest,
        ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>> {
            Box::pin(async { panic!("preview/resolution must not dispatch provider traffic") })
        }
    }
    crate::impl_fixture_transport!(NoNetwork);
    #[derive(Debug, Default)]
    struct Clock(AtomicU64);
    impl crate::accounts::AccountClock for Clock {
        fn now_ms(&self) -> u64 {
            self.0.load(Ordering::SeqCst)
        }
    }
    #[derive(Debug, Default)]
    struct Credentials(AtomicUsize);
    impl CredentialProvider for Credentials {
        fn source<'a>(
            &'a self,
            _: &'a CredentialScope,
        ) -> BoxFuture<'a, Result<crate::CredentialSource, LlmError>> {
            Box::pin(async { Ok(crate::CredentialSource::Stored) })
        }
        fn load<'a>(
            &'a self,
            scope: &'a CredentialScope,
        ) -> BoxFuture<'a, Result<Credential, LlmError>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                Ok(Credential::ApiKey(format!(
                    "fixture-{}",
                    scope.profile_name
                )))
            })
        }
    }
    fn account_fixture() -> (
        ProviderServices,
        Arc<crate::accounts::AccountPool>,
        Arc<Clock>,
        Arc<Credentials>,
    ) {
        let mut template = crate::builtin_presets()
            .providers
            .into_iter()
            .find(|profile| profile.profile_name == "openai")
            .unwrap();
        template.connection.group = Some("openai".into());
        let mut profiles = vec![template.clone()];
        let mut accounts = Vec::new();
        for (priority, id) in ["a", "b"].into_iter().enumerate() {
            let mut profile = template.clone();
            profile.profile_name = format!("account.{id}");
            profile.connection.connection_id = Some(id.into());
            profile.connection.order = priority as u32;
            profile.credential = crate::CredentialConfig::Static {
                id: format!("acct.{id}"),
            };
            profiles.push(profile);
            accounts.push(AccountMetadata {
                account_id: id.into(),
                provider_id: "openai".into(),
                display_name: id.into(),
                auth_kind: AccountAuthKind::ApiKey,
                execution_binding_id: "openai".into(),
                credential_ref: CredentialRef {
                    store_scope: "test".into(),
                    secret_ref: format!("acct.{id}"),
                },
                enabled: true,
                user_priority: priority as u32,
                config_revision: 1,
                identity_generation: 1,
            });
        }
        let clock = Arc::new(Clock::default());
        let credentials = Arc::new(Credentials::default());
        let pool = Arc::new(
            crate::accounts::AccountPool::new(
                accounts,
                Arc::new(crate::accounts::MemoryAccountPoolStore::default()),
            )
            .with_clock(clock.clone()),
        );
        let runtime = crate::ModelRuntime::from_config(crate::ClientConfig {
            providers: profiles,
        })
        .unwrap()
        .with_credential_provider(credentials.clone())
        .with_account_pool(pool.clone());
        let services = runtime
            .provider_services(sdk::protocol::Region::International, Arc::new(NoNetwork))
            .unwrap();
        (services, pool, clock, credentials)
    }

    #[tokio::test]
    async fn file_resolution_validates_identity_before_credentials_and_never_changes_owner() {
        let (services, pool, clock, credentials) = account_fixture();
        let snapshot = services.snapshot();
        let profile = snapshot.profile("account.b").unwrap();
        let scope = crate::accounts::AccountResourceScope {
            store_scope: "test".into(),
            account_id: "b".into(),
            identity_generation: 1,
        };
        let resource = files::ProviderFileRef {
            provider_id: profile.provider_id.clone(),
            profile_name: profile.profile_name.clone(),
            endpoint_fingerprint: files::provider_file_endpoint_fingerprint(&profile.base_url),
            account_scope: Some(serde_json::to_string(&scope).unwrap()),
            protocol: profile.protocol,
            file_id: "file-fixture".into(),
            uri: None,
            filename: None,
            media_type: None,
            size_bytes: None,
            expires_at: None,
            processing_status: None,
            downloadable: Some(true),
            purpose: None,
        };
        let mut invalid = Vec::new();
        let mut changed = resource.clone();
        changed.account_scope = None;
        invalid.push(changed);
        let mut changed = resource.clone();
        changed.account_scope = Some("opaque-old-scope".into());
        invalid.push(changed);
        let mut changed = resource.clone();
        changed.profile_name = "account.a".into();
        invalid.push(changed);
        let mut changed = resource.clone();
        changed.endpoint_fingerprint = "another-endpoint".into();
        invalid.push(changed);
        let mut changed = resource.clone();
        changed.protocol = sdk::protocol::ProtocolFamily::GeminiGenerateContent;
        invalid.push(changed);
        let mut changed = resource.clone();
        changed.account_scope = Some(
            serde_json::to_string(&crate::accounts::AccountResourceScope {
                store_scope: "another-authority".into(),
                ..scope.clone()
            })
            .unwrap(),
        );
        invalid.push(changed);
        let mut changed = resource.clone();
        changed.account_scope = Some(
            serde_json::to_string(&crate::accounts::AccountResourceScope {
                identity_generation: 2,
                ..scope
            })
            .unwrap(),
        );
        invalid.push(changed);
        for changed in invalid {
            assert!(services.resolve_file_account(&changed).await.is_err());
            assert_eq!(credentials.0.load(Ordering::SeqCst), 0);
        }

        // Higher-priority A is ready, but a B-owned resource stays on B.
        let mut selected = services.resolve_file_account(&resource).await.unwrap();
        assert_eq!(selected.actual_profile, "account.b");
        assert_eq!(credentials.0.load(Ordering::SeqCst), 1);
        assert!(
            selected
                .failed_sdk(&sdk::protocol::LlmError::RateLimited {
                    message: "fixture rate limit".into(),
                    retry_after: Some(std::time::Duration::from_secs(30)),
                })
                .await
        );
        assert!(matches!(
            services.resolve_file_account(&resource).await,
            Err(LlmError::AllAccountsUnavailable { .. })
        ));
        assert_eq!(credentials.0.load(Ordering::SeqCst), 1);
        clock.0.store(30_001, Ordering::SeqCst);
        let mut recovered = services.resolve_file_account(&resource).await.unwrap();
        assert_eq!(recovered.actual_profile, "account.b");
        recovered.succeeded().await;

        let mut removed = pool.snapshot();
        removed.config_revision += 1;
        removed.accounts.retain(|account| account.account_id != "b");
        pool.refresh(removed).unwrap();
        assert!(services.resolve_file_account(&resource).await.is_err());
        assert_eq!(credentials.0.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn audio_preview_is_read_only_and_business_probe_remains_single_owner() {
        let (services, pool, clock, credentials) = account_fixture();
        let preview = services
            .audio_account_capabilities("openai", audio::AudioOperation::Synthesis, None)
            .await
            .unwrap();
        assert_eq!(preview.len(), 2);
        assert_eq!(credentials.0.load(Ordering::SeqCst), 0);
        assert_eq!(
            services
                .service_credential_source("account.a")
                .await
                .unwrap(),
            crate::CredentialSource::Stored
        );
        assert_eq!(credentials.0.load(Ordering::SeqCst), 0);
        let mut first = services
            .resolve_audio_account("openai", audio::AudioOperation::Synthesis, None, None)
            .await
            .unwrap();
        assert_eq!(first.account_id.as_deref(), Some("a"));
        first
            .failed(&LlmError::RateLimited {
                retry_after: Some(std::time::Duration::from_secs(30)),
                scope: None,
            })
            .await;
        let mut second = services
            .resolve_audio_account("openai", audio::AudioOperation::Synthesis, None, None)
            .await
            .unwrap();
        assert_eq!(second.account_id.as_deref(), Some("b"));
        second.succeeded().await;
        clock.0.store(30_001, Ordering::SeqCst);
        let before = credentials.0.load(Ordering::SeqCst);
        services
            .audio_account_capabilities("openai", audio::AudioOperation::Synthesis, None)
            .await
            .unwrap();
        assert_eq!(credentials.0.load(Ordering::SeqCst), before);
        let mut probe = services
            .resolve_audio_account("openai", audio::AudioOperation::Synthesis, None, None)
            .await
            .unwrap();
        assert_eq!(probe.account_id.as_deref(), Some("a"));
        let held = services
            .resolve_audio_account("openai", audio::AudioOperation::Synthesis, None, None)
            .await
            .unwrap();
        assert_eq!(held.account_id.as_deref(), Some("b"));
        probe.succeeded().await;
        let pinned = services
            .resolve_audio_account(
                "openai",
                audio::AudioOperation::Synthesis,
                None,
                Some(&held.account_scope),
            )
            .await
            .unwrap();
        assert_eq!(pinned.account_id.as_deref(), Some("b"));
        let mut removed = pool.snapshot();
        removed.config_revision += 1;
        removed.accounts.retain(|account| account.account_id != "b");
        pool.refresh(removed).unwrap();
        assert!(
            matches!(
                services
                    .resolve_audio_account(
                        "openai",
                        audio::AudioOperation::Synthesis,
                        None,
                        Some(&held.account_scope)
                    )
                    .await,
                Err(LlmError::InvalidRequest { .. })
            ),
            "a removed session owner cannot fall back to recovered account A"
        );
    }
}
