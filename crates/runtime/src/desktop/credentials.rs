use lingxi_core::host::CredentialStoragePolicy;
use llm_runtime::auth::openai as openai_oauth;
use llm_runtime::{ModelRuntime, Transport};
use orchestrator::model::user_agent::UserAgentEnv;
use orchestrator::provider_adapter::SubscriberState;
use platform_posix::{PosixClock, PosixHttp};
#[cfg(windows)]
use platform_windows::process::supervisor as shell_supervisor;
#[cfg(windows)]
use platform_windows::WindowsMcpTransport;
use secret::CredentialManager;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::{
    anthropic_models_for, api_provider, desktop_fusion_catalog_row,
    effective_model_source_for_config, filter_fusion_catalog, load_effective_settings_for_config,
    managed_model_policy_source, managed_model_setting_for_config, model_provenance_for_config,
    provider_profile_label, register_fusion_catalog_refresher, ApiProvider, BuildError,
    DefaultModelFallbackNotice, DesktopConfig, FusionCatalogModelSource, FusionCatalogRefresher,
    FusionCatalogRegistry,
};

/// Desktop [`ClaudeAiAuthProvider`](tool_cron::ClaudeAiAuthProvider) backed by
/// the credential store.
///
/// `RemoteTrigger` calls this in-process to add the refreshed claude.ai OAuth
/// access token + organization UUID to its requests — the token never reaches
/// the shell. The token is read at call-time (not snapshotted at boot) so the
/// proactive/reactive OAuth refresh driver — which persists rotated tokens back
/// to the same keychain — is always reflected. Mirrors TS
/// `checkAndRefreshOAuthTokenIfNeeded()` + `getClaudeAIOAuthTokens()`.
///
pub(super) struct AccountClaudeAiAuthProvider {
    pub(super) credentials: Arc<CredentialManager>,
    pub(super) data_root: std::path::PathBuf,
    pub(super) store_scope: String,
}

#[async_trait::async_trait]
impl tool_cron::ClaudeAiAuthProvider for AccountClaudeAiAuthProvider {
    async fn credentials(&self) -> Option<tool_cron::ClaudeAiCredentials> {
        let root = self.data_root.clone();
        let scope = self.store_scope.clone();
        let mut snapshot = tokio::task::spawn_blocking(move || {
            crate::provider_accounts::load_account_snapshot(&root, &scope)
        })
        .await
        .ok()?
        .ok()?;
        snapshot
            .accounts
            .sort_by_key(|account| account.user_priority);
        for account in snapshot.accounts.into_iter().filter(|account| {
            account.enabled
                && account.provider_id == "anthropic"
                && account.auth_kind == lingxi_core::host::provider_accounts::AccountAuthKind::OAuth
        }) {
            let mut grant = match self
                .credentials
                .get_account_credential(&account.credential_ref)
                .await
            {
                Ok(Some(grant)) if grant.identity_generation == account.identity_generation => {
                    grant
                }
                _ => continue,
            };
            if let secret::AccountCredentialKind::OAuth { expires_at, .. } = &grant.kind {
                if *expires_at <= std::time::SystemTime::now() {
                    grant = match self
                        .credentials
                        .refresh_account_credential(account.clone(), grant.credential_revision)
                        .await
                    {
                        Ok(Some(grant))
                            if grant.identity_generation == account.identity_generation =>
                        {
                            grant
                        }
                        _ => continue,
                    };
                }
            }
            if let secret::AccountCredentialKind::OAuth {
                access_token,
                organization_id,
                ..
            } = grant.kind
            {
                return Some(tool_cron::ClaudeAiCredentials {
                    access_token: access_token.expose_secret().clone(),
                    org_uuid: organization_id.unwrap_or_default(),
                    base_api_url: tool_cron::remote_trigger::DEFAULT_BASE_API_URL.into(),
                });
            }
        }
        None
    }
}

/// The resolved LLM stack: credentials, the assembled multi-provider config,
/// and the routed [`ModelRuntime`] every model-facing surface needs.
///
/// Shared by interactive sessions and headless commands so every model-facing
/// surface uses the same account directory, credential authority and routing policy.
pub struct LlmStack {
    /// Catalog notifications owned by this credential scope.
    pub catalog_registry: FusionCatalogRegistry,
    /// Product region captured with this model client.
    pub provider_region: llm_runtime::Region,
    /// See [`build`] for the resolution rules behind `http`.
    pub http: Arc<PosixHttp>,
    /// See [`build`] for the resolution rules behind `clock`.
    pub clock: Arc<PosixClock>,
    /// See [`build`] for the resolution rules behind `mcp_oauth_storage`.
    pub mcp_oauth_storage: Arc<dyn lingxi_core::host::SecureStorage>,
    /// See [`build`] for the resolution rules behind `credentials`.
    pub credentials: Arc<CredentialManager>,
    /// See [`build`] for the resolution rules behind `auth`.
    /// See [`build`] for the resolution rules behind `subscription`.
    pub subscription: lingxi_core::host::subscription::SharedSubscription,
    /// See [`build`] for the resolution rules behind `is_subscriber`.
    pub is_subscriber: bool,
    /// See [`build`] for the resolution rules behind `openai_oauth_handle`.
    /// See [`build`] for the resolution rules behind `pricing`.
    pub pricing: cost::PricingCatalog,
    /// See [`build`] for the resolution rules behind `chains`.
    pub chains: provider_config::ChainConfig,
    /// See [`build`] for the resolution rules behind `model_providers`.
    pub model_providers: std::collections::BTreeMap<String, (String, String)>,
    /// The live Fusion catalog assembled from every configured provider,
    /// filtered through the boot-time availability snapshot. Kept for
    /// callers that want a one-shot filtered listing; the orchestrator
    /// itself is built over [`Self::fusion_catalog_source`] instead, which
    /// re-filters against LIVE availability on every call.
    pub fusion_catalog: Vec<fusion::CatalogModel>,
    /// Round-4 review finding [8]: a refreshable `fusion::ModelSource` over
    /// the SAME unfiltered catalog `fusion_catalog` was filtered from — see
    /// `FusionCatalogModelSource`'s doc comment. This is what
    /// `desktop_fusion_executor` is actually built over, so a credential
    /// connected mid-session (via [`Self::fusion_catalog_refresher`])
    /// becomes visible to Fusion without a process restart.
    pub fusion_catalog_source: Arc<dyn fusion::ModelSource>,
    /// Handle a credential-write path calls after persisting a new
    /// credential so `fusion_catalog_source` picks it up — see
    /// [`FusionCatalogRefresher::refresh`].
    pub fusion_catalog_refresher: FusionCatalogRefresher,
    /// See [`build`] for the resolution rules behind `default_listings`.
    pub default_listings: Vec<lingxi_core::host::ModelListing>,
    /// See [`build`] for the resolution rules behind `default_model_id`.
    pub default_model_id: String,
    /// See [`build`] for the resolution rules behind `default_model_profile`.
    pub default_model_profile: Option<String>,
    /// Claude auto-mode provider tag for each configured profile.
    pub profile_auto_mode_provider: std::collections::BTreeMap<String, String>,
    /// Provider tag applied to the built-in Anthropic profile after env routing.
    pub first_party_environment_provider: String,
    /// See [`build`] for the resolution rules behind `provider_availability`.
    pub provider_availability: std::collections::BTreeMap<String, bool>,
    /// See [`build`] for the resolution rules behind `default_model_fallback`.
    pub default_model_fallback: Option<DefaultModelFallbackNotice>,
    /// See [`build`] for the resolution rules behind `model_provenance`.
    pub model_provenance: lingxi_core::host::ModelProvenance,
    /// See [`build`] for the resolution rules behind `session_model_restriction`.
    pub session_model_restriction:
        Option<(llm_runtime::model::allowlist::ModelEnforcement, Vec<String>)>,
    /// Raw configured model setting that drives main-loop Plan selection.
    pub main_model_setting: String,
    /// Exact winning managed `settings.model` source. It remains separate from
    /// the UTF-8 route selector so valid JavaScript strings with lone UTF-16
    /// units and uncatalogued Native model ids are not replaced by a default.
    pub main_model_source: Option<lingxi_core::types::utf16_json::Utf16JsonProjection>,
    /// Provenance of the raw setting before any connected-provider fallback.
    pub main_model_setting_provenance: lingxi_core::host::ModelProvenance,
    /// See [`build`] for the resolution rules behind `model_setting_for_spawns`.
    pub model_setting_for_spawns: String,
    /// Resolved provider tag for the session's boot auto-mode gate.
    pub session_auto_mode_provider: String,
    /// See [`build`] for the resolution rules behind `llm_runtime`.
    pub llm_runtime: Arc<ModelRuntime>,
    /// Shared route-context provider used by Agent and direct teammate model
    /// postprocessing. It resolves against the same configured ModelRuntime.
    pub model_resolution_context_provider:
        Arc<dyn agent::model_resolution::ModelResolutionContextProvider>,
    /// See [`build`] for the resolution rules behind `llm_transport`.
    pub llm_transport: Arc<dyn Transport>,
    /// See [`build`] for the resolution rules behind `cost_estimator`.
    pub cost_estimator: Arc<llm_runtime::CostEstimator>,
    /// See [`build`] for the resolution rules behind `subscriber_state`.
    pub subscriber_state: SubscriberState,
    /// Where the Anthropic credential came from, for the API-key-disabled error
    /// copy. Carried here rather than on [`SubscriberState`], which lives in
    /// `llm-runtime` and is shared.
    pub credential_origin: orchestrator::api_error_copy::CredentialOrigin,
    /// Whether a claude.ai OAuth access token is present (oracle `zv()`).
    pub has_oauth_token: bool,
}

/// The single credential composition shared by CLI, TUI, and Desktop.
///
/// Keeping the native-storage selection, fallback path, clock, HTTP transport,
/// and [`CredentialManager`] construction together prevents entrypoints from
/// silently drifting to different stores.
pub struct SharedCredentialStack {
    /// Canonical account owner root, independent of a conversation/project config root.
    pub provider_account_home: Option<PathBuf>,
    /// Explicit host account registry snapshot, including Broker-owned accounts.
    pub provider_accounts: Option<lingxi_core::host::provider_accounts::AccountSnapshot>,
    /// Catalog notifications owned by this credential scope.
    pub catalog_registry: FusionCatalogRegistry,
    /// Platform HTTP transport used by the credential manager.
    pub http: Arc<PosixHttp>,
    /// Platform clock used by the credential manager.
    pub clock: Arc<PosixClock>,
    /// Shared storage handle, also reused by MCP OAuth persistence.
    pub storage: Arc<dyn lingxi_core::host::SecureStorage>,
    /// Canonical provider/OAuth credential manager.
    pub credentials: Arc<CredentialManager>,
}

/// Build the credential stack shared by every desktop-class entrypoint.
///
/// Production uses the platform store rooted at `lingxi_home`. Isolated test
/// boots opt into a file-only store and never consult the user's native
/// keychain.
///
/// # Errors
/// Returns [`BuildError::SecureStorage`] when the shared storage cannot be
/// initialized.
pub async fn build_shared_credential_stack(
    lingxi_home: &std::path::Path,
    isolated_credential_storage: bool,
) -> Result<SharedCredentialStack, BuildError> {
    build_shared_credential_stack_with_policy(
        lingxi_home,
        isolated_credential_storage,
        CredentialStoragePolicy::NativePreferred,
    )
    .await
}

/// Is this credential root a throwaway directory rather than a real profile?
///
/// A `lingxi_home` under the OS temporary directory is, by construction, not a
/// durable user profile: it is a fixture, a sandbox, or a scratch session, and
/// it is gone when the run is. Reaching into the machine login keychain for one
/// would be wrong in both directions — it lets an ephemeral session read the
/// real user's saved credentials, and it writes native entries keyed to the OS
/// user that a temporary home can never clean up again.
///
/// Storage selection follows the canonical credential root so test and sandbox
/// account records never reach the user's native secure store.
///
/// `/tmp` and `/private/tmp` are named alongside `temp_dir()` because macOS
/// resolves one to the other through a symlink and a caller may hand us either
/// spelling.
pub(super) fn credential_root_is_ephemeral(lingxi_home: &std::path::Path) -> bool {
    lingxi_home.starts_with(std::env::temp_dir())
        || lingxi_home.starts_with(std::path::Path::new("/tmp"))
        || lingxi_home.starts_with(std::path::Path::new("/private/tmp"))
}

/// Build the shared desktop credential stack with an explicit fallback policy.
///
/// Production CLI/TUI use [`CredentialStoragePolicy::NativePreferred`];
/// packaged bridge-server builds can request
/// [`CredentialStoragePolicy::NativeOrMemory`] so a missing native vault never
/// downgrades to plaintext on disk.
pub async fn build_shared_credential_stack_with_policy(
    lingxi_home: &std::path::Path,
    isolated_credential_storage: bool,
    credential_storage_policy: CredentialStoragePolicy,
) -> Result<SharedCredentialStack, BuildError> {
    let http = Arc::new(PosixHttp::new().with_monitor_proxy(Arc::new(
        crate::desktop::sandbox_runner::MonitorProxyConnector,
    )));
    let clock = Arc::new(PosixClock::new());
    let credentials_path = lingxi_home.join(".credentials.json");
    // Only `NativePreferred` takes the ephemeral shortcut. `NativeOrMemory` is
    // the packaged-desktop policy whose whole point is that a missing native
    // vault falls back to process memory and never to plaintext on disk, and
    // `PlainTextFixture` already reaches the plaintext store through the
    // factory — neither wants this decided for it here.
    let ephemeral_root = matches!(
        credential_storage_policy,
        CredentialStoragePolicy::NativePreferred
    ) && credential_root_is_ephemeral(lingxi_home);
    let storage = if isolated_credential_storage || ephemeral_root {
        build_platform_plaintext_secure_storage(credentials_path).await
    } else {
        build_platform_secure_storage(
            current_credential_user(),
            lingxi_home.to_path_buf(),
            credentials_path,
            credential_storage_policy,
        )
        .await
    }
    .map_err(|e| BuildError::SecureStorage(e.to_string()))?;
    let credentials = Arc::new(CredentialManager::new(
        storage.clone(),
        clock.clone(),
        http.clone(),
    ));
    Ok(SharedCredentialStack {
        provider_account_home: None,
        provider_accounts: None,
        catalog_registry: FusionCatalogRegistry::default(),
        http,
        clock,
        storage,
        credentials,
    })
}

pub(super) async fn build_shared_credential_stack_for_config(
    cfg: &DesktopConfig,
) -> Result<SharedCredentialStack, BuildError> {
    if cfg.team_invocation.is_some() {
        let http = Arc::new(PosixHttp::new());
        let clock = Arc::new(PosixClock::new());
        let storage: Arc<dyn lingxi_core::host::SecureStorage> =
            Arc::new(lingxi_core::host::InMemorySecureStorage::new());
        let credentials = Arc::new(CredentialManager::new(
            storage.clone(),
            clock.clone(),
            http.clone(),
        ));
        return Ok(SharedCredentialStack {
            provider_account_home: None,
            provider_accounts: None,
            catalog_registry: FusionCatalogRegistry::default(),
            http,
            clock,
            storage,
            credentials,
        });
    }
    build_shared_credential_stack_with_policy(
        &cfg.lingxi_home,
        cfg.isolated_credential_storage,
        cfg.credential_storage_policy,
    )
    .await
}

pub(super) async fn build_platform_plaintext_secure_storage(
    credentials_path: PathBuf,
) -> Result<Arc<dyn lingxi_core::host::SecureStorage>, lingxi_core::host::SecureStorageError> {
    #[cfg(windows)]
    {
        platform_windows::plaintext_secure_storage(credentials_path).await
    }
    #[cfg(not(windows))]
    {
        platform_posix::plaintext_secure_storage(credentials_path).await
    }
}

pub(super) async fn build_platform_secure_storage(
    user: String,
    lingxi_home: PathBuf,
    credentials_path: PathBuf,
    policy: CredentialStoragePolicy,
) -> Result<Arc<dyn lingxi_core::host::SecureStorage>, lingxi_core::host::SecureStorageError> {
    #[cfg(windows)]
    {
        platform_windows::secure_storage_for_policy(user, lingxi_home, credentials_path, policy)
            .await
    }
    #[cfg(not(windows))]
    {
        platform_posix::secure_storage_for_policy(user, lingxi_home, credentials_path, policy).await
    }
}

pub(super) fn current_credential_user() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "default".to_string())
}

/// Resolve the LLM stack from a [`DesktopConfig`] alone.
///
/// Pure with respect to the session: it touches the keychain, the process
/// environment and the network (the availability probe), but creates no
/// session, no transcript and no hooks.
pub async fn resolve_llm_stack(cfg: &DesktopConfig) -> Result<LlmStack, BuildError> {
    // Headless callers must receive the same frozen authority as full assembly.
    // Do this before opening storage or constructing credential helpers.
    let mut scoped;
    let cfg = if cfg.team_invocation.is_some() {
        scoped = cfg.clone();
        super::team_invocation::enforce_team_config(&mut scoped)?;
        &scoped
    } else {
        cfg
    };
    let shared = build_shared_credential_stack_for_config(cfg).await?;
    resolve_llm_stack_with_credentials(cfg, shared, None).await
}

pub(super) async fn resolve_llm_stack_with_credentials(
    cfg: &DesktopConfig,
    shared: SharedCredentialStack,
    proxy_auth_helper: Option<Arc<dyn lingxi_llm_client::transport::ProxyAuthHelper>>,
) -> Result<LlmStack, BuildError> {
    if cfg.team_invocation.is_some()
        && (shared.storage.backend() != lingxi_core::host::SecureStorageBackend::MemorySession)
    {
        return Err(super::team_invocation::TeamInvocationError::Credentials.into());
    }
    let _catalog_refresh = cfg.startup_catalog.as_ref().map(|source| source.prefetch());
    // Resolve the winning model setting from the canonical tier stack before
    // assembling provider profiles. Managed `settings.model` is the only
    // administrator-owned default; explicit CLI/env pins remain user-owned.
    let managed_settings_for_model =
        crate::desktop::settings_watch::managed_settings_snapshot_for_config(cfg).await;
    let effective_settings_for_model =
        load_effective_settings_for_config(cfg, Some(&managed_settings_for_model))?;
    let mut model_provenance =
        model_provenance_for_config(cfg, Some(&effective_settings_for_model));
    let configured_model = if cfg.team_invocation.is_some() {
        cfg.default_model.clone()
    } else {
        managed_model_setting_for_config(cfg, Some(&effective_settings_for_model))
            .unwrap_or_else(|| cfg.default_model.clone())
    };
    let main_model_source =
        effective_model_source_for_config(cfg, Some(&effective_settings_for_model));
    let main_model_setting = configured_model.clone();
    let main_model_setting_provenance = model_provenance;
    // (1) Platform-minimal façade shared byte-for-byte with CLI/TUI auth.
    let SharedCredentialStack {
        provider_account_home,
        provider_accounts,
        catalog_registry,
        http,
        clock,
        storage: mcp_oauth_storage,
        credentials,
    } = shared;

    // The packaged Electron parent owns durable plugin-secret persistence in
    // a dedicated Keychain service. Seed the shared runtime manager before
    // plugin discovery so `${user_config.*}` resolution uses the same path as
    // CLI/TUI while never writing a duplicate plaintext or Keychain entry.
    for (plugin, values) in &cfg.injected_plugin_secrets {
        for (key, value) in values {
            credentials
                .set_plugin_secret_ephemeral(plugin, key, value)
                .await;
        }
    }

    // The shared SDK transport owns provider networking. The original `http`
    // handle continues to serve MCP and hooks.
    let provider_transport = match proxy_auth_helper {
        Some(helper) => platform_common::provider_transport_with_proxy_auth_helper(helper),
        None => platform_common::provider_transport(),
    }
    .map_err(|e| BuildError::ApiBase(e.to_string()))?;
    let llm_transport: Arc<dyn Transport> = Arc::new(provider_transport);
    let subscription: lingxi_core::host::subscription::SharedSubscription =
        Arc::new(std::sync::RwLock::new(None));
    let is_subscriber = false;
    let persisted_subscription_type: Option<String> = None;
    let credential_origin = orchestrator::api_error_copy::CredentialOrigin::Other;
    let has_oauth_token = false;
    let has_api_key = false;
    let has_oauth = false;
    let openai_oauth_handle = Arc::new(
        openai_oauth::AccountOAuthClient::new(
            lingxi_llm_client::auth::oauth::openai::OpenAiOAuthConfig::default(),
            llm_transport.clone(),
        )
        .with_clock(clock.clone()),
    );

    let provider_policy = managed_settings_for_model
        .managed_overlay()?
        .unwrap_or_else(|| {
            lingxi_core::types::utf16_json::Utf16JsonProjection::parse("{}")
                .expect("empty object is valid JSON")
        });
    let provider_region = super::resolve_provider_region(cfg, &provider_policy)?;
    let official_catalog = match &cfg.startup_catalog {
        Some(source) => Some((*source.load().await).clone()),
        None => None,
    };
    let catalog_inputs = provider_config::AssembleInputs {
        official_catalog,
        anthropic_api_base: cfg.api_base.clone(),
        anthropic_models: anthropic_models_for(&configured_model, cfg.fallback_model.as_deref()),
        anthropic_has_api_key: has_api_key && !has_oauth,
        anthropic_has_oauth: has_oauth,
        user_providers: cfg.provider_profiles.clone().unwrap_or_default(),
        routing: cfg.routing.clone(),
    };
    let mut assembled =
        provider_config::assemble_for_region(catalog_inputs.clone(), provider_region);
    if let Some(model) = cfg
        .team_invocation
        .as_ref()
        .and_then(|profile| profile.selected_model())
    {
        let envelope = model.account_credentials();
        assembled.client_config.providers = envelope
            .provider_account_profiles
            .iter()
            .map(|profile| {
                let selected = profile
                    .models
                    .first()
                    .ok_or(super::team_invocation::TeamInvocationError::Model)?;
                super::team_process::TeamModelRoute::for_account(
                    profile.clone(),
                    &selected.request_model,
                )
                .and_then(|route| route.host_profile())
                .map_err(|_| super::team_invocation::TeamInvocationError::Model)
            })
            .collect::<Result<Vec<_>, _>>()?;
        assembled.credential_sources.clear();
        assembled.chains = Default::default();
        assembled.warnings.clear();
    }
    let account_home = provider_account_home.unwrap_or_else(|| cfg.lingxi_home.clone());
    let external_account_authority = provider_accounts.is_some();
    let account_snapshot = match provider_accounts {
        Some(snapshot) => snapshot,
        None => crate::account_pool_store::load_native_account_snapshot(
            account_home.clone(),
            credentials.clone(),
        )
        .await
        .map_err(BuildError::ApiBase)?,
    };
    if !external_account_authority
        && cfg.team_invocation.is_none()
        && !credentials.has_account_credential_refresher()
    {
        let store = Arc::new(
            crate::provider_accounts::ProviderAccountStore::new(
                account_home.clone(),
                account_snapshot.store_scope.clone(),
                credentials.clone(),
            )
            .with_catalog(crate::provider_accounts::ProviderAccountCatalog::new(
                catalog_inputs.clone(),
                provider_region,
                cfg.startup_catalog.clone(),
                None,
            )),
        );
        credentials.set_account_credential_refresher(Arc::new(
            crate::provider_accounts::AccountOAuthCoordinator::new(
                store,
                openai_oauth_handle.clone(),
            ),
        ));
    }
    let account_metadata = account_snapshot.accounts.clone();
    let account_pool = Some(
        crate::account_pool_store::assemble_pool(
            &mut assembled,
            account_snapshot,
            account_home.clone(),
        )
        .map_err(BuildError::ApiBase)?,
    );
    let account_directory = account_pool.as_ref().expect("account pool").directory();
    for w in &assembled.warnings {
        tracing::warn!(warning = %w, "provider-config assembly");
    }

    // Model-id lookup is used only when there is exactly one configured
    // profile. Duplicate ids are left unqualified; the actual ModelRuntime
    // resolver below reports ambiguity instead of first/last-profile guessing.
    let mut model_provider_matches: std::collections::BTreeMap<String, Vec<(String, String)>> =
        std::collections::BTreeMap::new();
    for profile in assembled
        .client_config
        .providers
        .iter()
        .filter(|p| !p.connection.hidden)
    {
        let label = provider_profile_label(&profile.profile_name);
        for model in &profile.models {
            let matches = model_provider_matches
                .entry(model.request_model.clone())
                .or_default();
            let candidate = (profile.profile_name.clone(), label.clone());
            if !matches.contains(&candidate) {
                matches.push(candidate);
            }
        }
    }
    let model_providers = model_provider_matches
        .into_iter()
        .filter_map(|(model, matches)| (matches.len() == 1).then(|| (model, matches[0].clone())))
        .collect::<std::collections::BTreeMap<_, _>>();
    let fusion_catalog = assembled
        .client_config
        .providers
        .iter()
        .filter(|provider| !provider.connection.hidden)
        .flat_map(|provider| {
            // Keep absent mode distinct from an explicit Unknown override;
            // the Fusion projection only applies a subscription hint when a
            // real source resolved the owning profile as Subscription.
            let billing_mode = provider.pricing.billing_mode;
            let protocol = provider.protocol;
            provider.models.iter().map(move |model| {
                desktop_fusion_catalog_row(&provider.profile_name, model, billing_mode, &protocol)
            })
        })
        .collect::<Vec<_>>();

    let default_listings: Vec<lingxi_core::host::ModelListing> = assembled
        .client_config
        .providers
        .iter()
        .filter(|profile| !profile.connection.hidden)
        .flat_map(|p| {
            let profile = p.profile_name.clone();
            let label = provider_profile_label(&p.profile_name);
            p.models
                .iter()
                .map(move |m| lingxi_core::host::ModelListing {
                    display_model: m.display_model.clone(),
                    request_model: m.request_model.clone(),
                    provider_id: profile.clone(),
                    provider_label: label.clone(),
                    description: m.description.clone(),
                    metadata: Default::default(),
                    capabilities: Default::default(),
                    reasoning: Default::default(),
                    supports_reasoning: m.capabilities.reasoning,
                    fusion_analyst_capable: false,
                    connection: Default::default(),
                })
        })
        .collect();
    let (mut default_model_id, mut default_model_profile) =
        lingxi_core::host::parse_model_ref(&configured_model, &default_listings);

    // Per-profile Claude provider tag, captured while
    // `assembled.client_config.providers` is still owned (`from_config` moves it
    // below). The tag drives both Explore's first-party gate and auto mode's
    // provider-sensitive model exclusions.
    let profile_auto_mode_provider: std::collections::BTreeMap<String, String> = assembled
        .client_config
        .providers
        .iter()
        .map(|p| {
            let provider = match &p.provider_id {
                llm_runtime::ProviderId::AnthropicFirstParty => "firstParty",
                llm_runtime::ProviderId::BedrockClaude => "anthropicAws",
                llm_runtime::ProviderId::VertexClaude => "vertex",
                llm_runtime::ProviderId::FoundryClaude => "foundry",
                _ => "other",
            };
            (p.profile_name.clone(), provider.to_string())
        })
        .collect();
    let model_resolution_client_config = assembled.client_config.clone();
    let mut client = ModelRuntime::from_config(assembled.client_config)
        .map_err(|e| BuildError::ApiBase(format!("llm-runtime config: {e}")))?;
    if let Some(source) = &cfg.startup_catalog {
        client = client.with_catalog_source(source.runtime_source(catalog_inputs, provider_region));
    }
    let model_resolution_context_provider: Arc<
        dyn agent::model_resolution::ModelResolutionContextProvider,
    > = Arc::new(
        crate::model_resolution::RuntimeModelResolutionProvider::new(
            Arc::new(client.clone()),
            &model_resolution_client_config,
        ),
    );

    // Establish the actual configured route before any fallback or provider
    // policy uses it. A duplicate unqualified model is a startup selection
    // error; it is never assigned to the first profile in map order.
    let selected_model_context = model_resolution_context_provider
        .context_for_route(&default_model_id, default_model_profile.as_deref())
        .map_err(|error| BuildError::ApiBase(format!("default model route: {error}")))?;
    default_model_profile = selected_model_context.route.profile.clone();
    // §6.1: ONE composite credential slot for ALL providers (anthropic api-key /
    // oauth-delegate + every per-profile credential source).

    // Attribute-only account presence drives the initial picker catalog.
    let availability_snapshot = account_pool
        .as_ref()
        .map(|pool| pool.snapshot())
        .unwrap_or_else(|| provider_config::AccountRegistry::new("unmanaged".into()).snapshot());
    let availability_probe = provider_config::compute_availability_with_accounts(
        &credentials,
        &assembled.credential_sources,
        &availability_snapshot,
    );
    let availability_rows =
        match tokio::time::timeout(std::time::Duration::from_secs(5), availability_probe).await {
            Ok(rows) => rows,
            Err(_) => {
                tracing::warn!(
                    "account availability probe timed out; model catalog remains unavailable"
                );
                Vec::new()
            }
        };
    let provider_availability: std::collections::BTreeMap<String, bool> = availability_rows
        .into_iter()
        .map(|a| (a.profile_name, a.available))
        .collect();
    let default_model_fallback: Option<DefaultModelFallbackNotice> = None;

    // ── Managed availableModels / enforceAvailableModels constraint ─────────
    // (parity 2.1.207 H-BIN-08.) When a MANAGED (`policySettings`) tier owns an
    // `availableModels` allowlist AND sets `enforceAvailableModels: true`, the
    // Default model selection is constrained (binary `enforceAvailableModels`
    // describe text): "if the default model for the user tier is not in
    // availableModels, Default resolves to the first allowed availableModels
    // entry instead." The enforce flag is inert without a policy-OWNED
    // allowlist, and a managed source that fails to parse refuses cascade-trust
    // mode (fail-closed). Consumed via `llm_runtime::model::allowlist`.
    // The managed `availableModels` restriction threaded into the subagent /
    // plan-mode spawn path (parity 2.1.207 H-BIN-08). Populated from the resolved
    // enforcement below when an active policy allowlist exists; `None` (default
    // install) leaves subagent / plan-mode resolution unrestricted.
    let mut session_model_restriction: Option<(
        llm_runtime::model::allowlist::ModelEnforcement,
        Vec<String>,
    )> = None;
    {
        use llm_runtime::model::allowlist;
        // Use the same managed-settings snapshot that resolved the raw model
        // setting/provenance above. A settings-file race must not pair a raw
        // model from one policy revision with a restriction from another.
        let policy_source = managed_model_policy_source(&managed_settings_for_model);
        // Deduplicate the byte-exact warnings (binary module-level `SN` set).
        let mut seen: Vec<String> = Vec::new();
        let enforcement = allowlist::resolve_enforcement(&policy_source, &mut |m| {
            if !seen.iter().any(|w| w == m) {
                seen.push(m.to_string());
                tracing::warn!("{m}");
            }
        });
        // Retain an ACTIVE enforcement (+ the concrete catalog) for the spawner
        // so subagent inherit-on-barred + the plan-mode upgrade gate can fire.
        if matches!(enforcement, allowlist::ModelEnforcement::Active { .. }) {
            let catalog: Vec<String> = default_listings
                .iter()
                .map(|m| m.request_model.clone())
                .collect();
            session_model_restriction = Some((enforcement.clone(), catalog));
        }
        if allowlist::model_allowed_under(&enforcement, &default_model_id) == Some(false) {
            if let allowlist::ModelEnforcement::Active {
                allowlist: al,
                overrides,
            } = &enforcement
            {
                let candidates: Vec<String> = default_listings
                    .iter()
                    .map(|m| m.request_model.clone())
                    .collect();
                if let Some(picked) =
                    allowlist::first_allowed_model(al, &candidates, Some(overrides))
                {
                    let picked_context = model_resolution_context_provider
                        .context_for_route(&picked, None)
                        .map_err(|error| {
                            BuildError::ApiBase(format!("managed default model route: {error}"))
                        })?;
                    tracing::warn!(
                        from = %default_model_id,
                        to = %picked,
                        "default model is not in the managed availableModels allowlist; \
                         resolving Default to the first allowed availableModels entry"
                    );
                    model_provenance =
                        lingxi_core::host::ModelProvenance::ManagedAdministratorDefault;
                    default_model_id = picked;
                    default_model_profile = picked_context.route.profile;
                }
            }
        }
    }

    // Resolving the default is metadata-only. Exact account credentials are
    // loaded by the first business call after pool selection, so an empty
    // account directory can still boot and expose account configuration.
    let selected_model_context = model_resolution_context_provider
        .context_for_route(&default_model_id, default_model_profile.as_deref())
        .map_err(|error| BuildError::ApiBase(format!("selected model route: {error}")))?;
    default_model_profile = selected_model_context.route.profile.clone();

    // The raw "user model setting" seam (the opusplan/haiku plan-mode swap
    // anchor threaded into subagent/teammate model resolution). When the
    // fallback rerouted the session, the persisted alias no longer describes
    // the booted main loop — thread the rerouted ref instead so a plan-mode
    // `AgentModel::Inherit` spawn cannot swap back onto the provider the
    // fallback just declared disconnected.
    let model_setting_for_spawns = default_model_fallback
        .as_ref()
        .map_or_else(|| configured_model.clone(), |n| n.to.clone());

    // Resolve the selected route from the actual ModelRuntime registry after
    // fallback/managed selection. Provider identity is not inferred from a
    // first/last profile map or the endpoint URL.
    let session_profile = selected_model_context
        .route
        .profile
        .as_ref()
        .ok_or_else(|| BuildError::ApiBase("selected model route has no profile".into()))?;
    let session_profile_auto_mode_provider = profile_auto_mode_provider
        .get(session_profile)
        .cloned()
        .ok_or_else(|| {
            BuildError::ApiBase(format!(
                "selected model profile {session_profile:?} has no provider classification"
            ))
        })?;
    let session_provider_first_party = selected_model_context.route.provider
        == Some(agent::model_resolution::ModelProviderKind::FirstParty);
    let first_party_environment_provider = match if cfg.team_invocation.is_some() {
        ApiProvider::FirstParty
    } else {
        api_provider()
    } {
        ApiProvider::FirstParty => "firstParty",
        ApiProvider::Bedrock => "anthropicAws",
        ApiProvider::Vertex => "vertex",
        ApiProvider::Foundry => "foundry",
    };
    let session_auto_mode_provider = if session_profile_auto_mode_provider == "firstParty" {
        first_party_environment_provider.to_string()
    } else {
        session_profile_auto_mode_provider
    };
    if !cfg.custom_betas.is_empty() && (!has_api_key || !session_provider_first_party) {
        return Err(BuildError::InvalidCustomBetas);
    }

    let mut composite = provider_config::MultiCredentialProvider::new(credentials.clone())
        .with_account_version_root(account_home.clone())
        .with_accounts(account_metadata);
    if let Some(pool) = account_pool.as_ref() {
        composite = composite.with_account_directory(pool.directory());
    }
    // GitHub Copilot needs a short-lived token minted from the raw OAuth token
    // (api.githubcopilot.com rejects the raw token). Wrap the composite so the
    // `github-copilot` credential is exchanged + cached; every other credential
    // id passes straight through unchanged.
    let copilot_creds =
        crate::credential_provider::with_token_exchange(Arc::new(composite), llm_transport.clone());
    client = client.with_credential_provider(copilot_creds);
    if let Some(pool) = account_pool {
        client = crate::account_pool_store::attach_account_runtime(client, pool, &catalog_registry);
    }
    let llm_runtime = Arc::new(client);

    // 3c-T3: build the cost estimator from the assembled pricing catalog so
    // LlmResponse.cost is populated on every successful decode. The catalog
    // already carries the built-in reference tiers + non-Anthropic preset rows +
    // any settings per-profile pricing overrides folded in by `assemble`. Unpriced
    // / unknown models leave cost = None (never an error).
    let cost_estimator = {
        use llm_runtime::{CostEstimator, PricingPolicy};
        use orchestrator::cost_wiring::llm_catalog_from_cost;
        let llm_cat = llm_catalog_from_cost(&assembled.pricing);
        Arc::new(CostEstimator::new(llm_cat, PricingPolicy::MarkUnestimated))
    };
    let subscriber_state = SubscriberState {
        is_subscriber,
        is_enterprise: persisted_subscription_type.as_deref() == Some("enterprise"),
    };

    // F011 item 1: the fusion catalog must reflect what a panel can ACTUALLY
    // reach. Filtered HERE (the earliest point `provider_availability` AND
    // `session_model_restriction` are both final) rather than left as "every
    // provider's every model" — otherwise `model_resolver::resolve` treats an
    // uncredentialed or managed-barred model as available, §4's "fewer than
    // required models -> TooFewModels before any panel call" preflight
    // guarantee is false, and a panel can burn up to 12 turns before failing
    // on `LlmError::Authentication`.
    // Round-4 review finding [8]: build a REFRESHABLE catalog source
    // (`FusionCatalogModelSource`) alongside the frozen filtered snapshot
    // below, over the SAME boot-time `provider_availability` — a
    // credential-write path calling `fusion_catalog_refresher.refresh()`
    // later updates `fusion_catalog_availability` in place, and every
    // subsequent `FusionCatalogModelSource::list()` (called once per
    // `/fusion` run, same as `DesktopFusionConfigSource::load()`) re-filters
    // against the fresh value instead of this boot-time snapshot.
    let fusion_catalog_availability: Arc<
        std::sync::RwLock<std::collections::BTreeMap<String, bool>>,
    > = Arc::new(std::sync::RwLock::new(provider_availability.clone()));
    let fusion_catalog_mutation_epoch = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let fusion_catalog_source: Arc<dyn fusion::ModelSource> = Arc::new(FusionCatalogModelSource {
        unfiltered: fusion_catalog.clone(),
        availability: fusion_catalog_availability.clone(),
        account_directory: account_directory.clone(),
        mutation_epoch: fusion_catalog_mutation_epoch.clone(),
        session_model_restriction: session_model_restriction.clone(),
        reload_managed_model_restriction: true,
        managed_policy_snapshot_cache: cfg.managed_policy_snapshot_cache.clone(),
    });
    let fusion_catalog_refresher = FusionCatalogRefresher {
        availability: fusion_catalog_availability,
        mutation_epoch: fusion_catalog_mutation_epoch,
        credentials: Arc::downgrade(&credentials),
        credential_sources: assembled.credential_sources.clone(),
        account_directory: account_directory.clone(),
    };
    // Finding [15]: publish this runtime's refresher so credential writes on
    // seams that never see a `FusionCatalogRefresher` handle (the TUI
    // `/connect` key view, which writes straight through
    // `secret::CredentialManager`) can still reach it — see
    // `FusionCatalogRegistry`.
    register_fusion_catalog_refresher(&catalog_registry, fusion_catalog_refresher.clone());

    let fusion_catalog = filter_fusion_catalog(
        fusion_catalog,
        &provider_availability,
        session_model_restriction.as_ref(),
    );

    Ok(LlmStack {
        catalog_registry,
        provider_region,
        http,
        clock,
        mcp_oauth_storage,
        credentials,
        subscription,
        is_subscriber,
        pricing: assembled.pricing,
        chains: assembled.chains,
        model_providers,
        fusion_catalog,
        fusion_catalog_source,
        fusion_catalog_refresher,
        default_listings,
        default_model_id,
        default_model_profile,
        profile_auto_mode_provider,
        first_party_environment_provider: first_party_environment_provider.to_string(),
        provider_availability,
        default_model_fallback,
        model_provenance,
        session_model_restriction,
        main_model_setting,
        main_model_source,
        main_model_setting_provenance,
        model_setting_for_spawns,
        session_auto_mode_provider,
        llm_runtime,
        model_resolution_context_provider,
        llm_transport,
        cost_estimator,
        subscriber_state,
        credential_origin,
        has_oauth_token,
    })
}

/// Build a model-facing [`llm_runtime::ApiService`] with NO session attached.
///
/// This is the entry point for one-shot commands that need to ask a model a
/// question without becoming a session: nothing here writes a transcript,
/// fires a `SessionStart` hook, starts an MCP server, or registers a tool. The
/// credential resolution and provider assembly come from
/// [`resolve_llm_stack`], so a headless call routes and authenticates exactly
/// as the interactive runtime does.
///
/// Differences from the service [`build`] constructs, all of them the absence
/// of a session rather than a change in behaviour:
///
/// - no retry reporter — there is no output stream to narrate backoff to, so
///   retries stay silent instead of being announced to nobody;
/// - no `request_metadata` — `user_id` carries a session id, and there is no
///   session;
/// - no forced `StructuredOutput` tool choice — a headless caller that wants
///   structured output asks for it per-request via `stream_json_schema`.
///
/// The routing config, fallback chains, retry overrides, cost estimator,
/// subscriber state, custom betas and AWS auth refresher are all identical to
/// the interactive path: those are properties of the install, not the session.
pub async fn build_api_service(
    cfg: &DesktopConfig,
    cwd: &std::path::Path,
) -> Result<Arc<llm_runtime::ApiService>, BuildError> {
    let mut scoped;
    let cfg = if cfg.team_invocation.is_some() {
        scoped = cfg.clone();
        super::team_invocation::enforce_team_config(&mut scoped)?;
        &scoped
    } else {
        cfg
    };
    let stack = resolve_llm_stack(cfg).await?;
    Ok(Arc::new(api_service_from_stack(cfg, cwd, stack)?))
}

/// Assemble the drive service (retry / rate-limit / betas loop) over an
/// already-resolved [`LlmStack`].
///
/// Split out from [`build_api_service`] so a caller that already holds a stack
/// — and wants the other halves of it too — does not resolve credentials twice.
#[must_use]
pub fn api_service_from_stack(
    cfg: &DesktopConfig,
    cwd: &std::path::Path,
    stack: LlmStack,
) -> Result<llm_runtime::ApiService, BuildError> {
    let mut scoped;
    let cfg = if cfg.team_invocation.is_some() {
        scoped = cfg.clone();
        super::team_invocation::enforce_team_config(&mut scoped)?;
        &scoped
    } else {
        cfg
    };
    // Same CHAINS BRIDGE as `build`: the assembled per-model chain becomes the
    // adapter's `fallback_overrides`, keyed by model id.
    let fallback_overrides: std::collections::BTreeMap<String, Vec<String>> = stack
        .chains
        .chains
        .iter()
        .map(|(key, entries)| {
            (
                key.clone(),
                entries.iter().map(|e| e.model.clone()).collect(),
            )
        })
        .collect();
    let settings_max_retries = stack.chains.retry.as_ref().map(|r| r.max_attempts);
    let settings_backoff_ms = stack.chains.retry.as_ref().map(|r| r.backoff_ms);
    let analytics_bus = Arc::new(telemetry::AnalyticsBus::new());

    let service = llm_runtime::ApiService::new_with_routing(
        stack.llm_runtime,
        stack.llm_transport,
        stack.subscriber_state,
        if cfg.team_invocation.is_some() {
            UserAgentEnv::default()
        } else {
            UserAgentEnv::from_process_env()
        },
        env!("CARGO_PKG_VERSION"),
        Some(analytics_bus.clone()),
        cfg.fallback_model.clone(),
        Some(stack.cost_estimator),
        fallback_overrides,
        settings_max_retries,
        settings_backoff_ms,
    )
    .with_scope_output_quota(cfg.scope_output_quota.clone())
    .with_subscription(stack.subscription)
    .with_custom_cli_betas(cfg.custom_betas.clone())
    .with_thinking(cfg.session_thinking)
    .with_effort_table_options(crate::effort_settings::table_options(&cfg.lingxi_home))
    .with_effort_settings_source({
        let effort_cfg = cfg.clone_for_runtime_controls();
        Arc::new(move || {
            let snapshot = crate::desktop::settings_watch::managed_settings_snapshot_sync(
                &effort_cfg,
            )
            .ok_or(lingxi_core::settings::SettingsError::SourceUnavailable(
                "managed policy snapshot",
            ))?;
            super::load_effective_settings_for_config(&effort_cfg, Some(&snapshot))
                .map(|settings| settings.effort_layers)
        })
    })?
    .with_prompt_cache_ttl_settings_source({
        let cache_cfg = cfg.clone_for_runtime_controls();
        Arc::new(move || super::prompt_cache_ttl_settings_for_config(&cache_cfg))
    });

    let service = if let Some(metadata) = &cfg.anthropic_client_metadata {
        service
            .with_anthropic_client_metadata(metadata.clone())
            .with_native_anthropic_endpoint(cfg.api_base.clone())
    } else {
        service
    };

    match aws_auth_refresher(cfg, cwd, analytics_bus)? {
        Some(refresher) => Ok(service.with_aws_auth(refresher)),
        None => Ok(service),
    }
}

/// Resolve the `awsAuthRefresh` / `awsCredentialExport` settings and build the
/// refresher when either is configured.
///
/// Returns `None` when neither is set — the common case — so a Bedrock 401 stays
/// terminal exactly as it does today. Shared by [`build`] and the headless path
/// so the workspace-trust gate (a project-sourced refresh command is refused
/// before trust is accepted) is enforced identically in both.
pub(super) fn aws_auth_refresher(
    cfg: &DesktopConfig,
    cwd: &std::path::Path,
    analytics_bus: Arc<telemetry::AnalyticsBus>,
) -> Result<Option<Arc<llm_runtime::AwsAuthRefresher>>, lingxi_core::settings::SettingsError> {
    if cfg.team_invocation.is_some() {
        return Ok(None);
    }
    let aws_settings = if cfg.restricted {
        let snapshot = crate::desktop::settings_watch::managed_settings_snapshot_sync(cfg).ok_or(
            lingxi_core::settings::SettingsError::SourceUnavailable("managed policy snapshot"),
        )?;
        load_effective_settings_for_config(cfg, Some(&snapshot))?
    } else {
        let env_vars: std::collections::BTreeMap<String, String> = std::env::vars().collect();
        lingxi_core::settings::Settings::load(lingxi_core::settings::LoadInputs {
            env: &env_vars,
            project_dir: cwd,
            defaults: lingxi_core::settings::schema::SettingsJson::default(),
        })?
    };
    let from_project = |field: &str| {
        aws_settings.effective_for(field).is_some_and(|p| {
            p.contributors.last() == Some(&lingxi_core::settings::tracer::Source::Project)
        })
    };
    let aws_settings = llm_runtime::AwsAuthSettings {
        aws_auth_refresh: aws_settings.settings.aws_auth_refresh.clone(),
        aws_auth_refresh_from_project: from_project("awsAuthRefresh"),
        aws_credential_export: aws_settings.settings.aws_credential_export.clone(),
        aws_credential_export_from_project: from_project("awsCredentialExport"),
        // No global config path ⇒ the CLI trust gate proceeds (mode.rs
        // `trust_gate_should_prompt` — nothing to check against), so treat
        // as trusted like the gate does.
        workspace_trusted: match migrations::global_config::global_config_path() {
            Some(p) => migrations::global_config::check_has_trust_dialog_accepted(&p, cwd),
            None => true,
        },
    };
    if aws_settings.aws_auth_refresh.is_none() && aws_settings.aws_credential_export.is_none() {
        return Ok(None);
    }
    Ok(Some(Arc::new(llm_runtime::AwsAuthRefresher::new(
        aws_settings,
        Arc::new(llm_runtime::ShellAwsAuthProcess),
        Some(analytics_bus),
    ))))
}

/// Read the pre-V1 project `lastCost` only when its session tag matches the
/// boot identity.  Durable coordinators persist the evaluation marker even
/// when this returns `None`, so a later mutable shadow-config edit cannot be
/// imported into an already-authoritative ledger.  This small composition
/// helper intentionally lives above `migrations`: the migration crate owns
/// the JSON substrate, while the app owns the deterministic nano-USD mapping.
/// Capture the single pre-V1 project shadow once at composition time. The
/// returned tuple is immutable and can safely back hot-session matching; the
/// manager never rereads mutable ambient config while opening later sessions.
pub(super) fn capture_legacy_opening_balance(
    config_path: Option<&Path>,
    cwd: &Path,
) -> Option<(lingxi_core::types::SessionId, u64)> {
    let config_path = config_path?;
    let project_key = migrations::global_config::project_path_for_config(cwd);
    let project = migrations::global_config::get_project_config(config_path, &project_key).ok()?;
    let session = project
        .get("lastSessionId")
        .and_then(serde_json::Value::as_str)?;
    let session_id = lingxi_core::types::SessionId::parse_prefixed(session)?;
    let dollars = project
        .get("lastCost")
        .and_then(serde_json::Value::as_f64)?;
    if !dollars.is_finite() || dollars <= 0.0 {
        return None;
    }
    let nanos = (dollars * 1_000_000_000.0).round();
    if nanos >= u64::MAX as f64 {
        Some((session_id, u64::MAX))
    } else {
        Some((session_id, nanos as u64))
    }
}
