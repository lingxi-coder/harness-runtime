use llm_runtime::oauth::anthropic::client::ClaudeAiOAuthClient;
use llm_runtime::oauth::anthropic::config::ClaudeAiOAuthConfig;
use llm_runtime::oauth::anthropic::handle::OAuthHandle;
use llm_runtime::oauth::anthropic::{OAuthCredentialProvider, RefreshDriver};
use llm_runtime::oauth::openai as openai_oauth;
use llm_runtime::LlmTransportBridge;
use llm_runtime::{DefaultLlmClient, Transport};
use orchestrator::model::user_agent::UserAgentEnv;
use orchestrator::provider_adapter::SubscriberState;
use platform_api::{AuthHandle, CredentialStoragePolicy};
use platform_posix::{PosixClock, PosixHttp, PosixRuntime};
#[cfg(windows)]
use platform_windows::process::supervisor as shell_supervisor;
#[cfg(windows)]
use platform_windows::WindowsMcpTransport;
use secret::CredentialManager;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::{
    anthropic_models_for, api_provider, connect, connected_provider_fallback,
    desktop_fusion_catalog_row, filter_fusion_catalog, fusion_route_flag,
    load_effective_settings_for_config, managed_model_policy_source,
    managed_model_setting_for_config, model_provenance_for_config, provider_profile_label,
    register_fusion_catalog_refresher, subscription_seed, subscription_snapshot_from, ApiProvider,
    BuildError, DefaultModelFallbackNotice, DesktopConfig, FusionCatalogClearingAuth,
    FusionCatalogModelSource, FusionCatalogRefresher, FusionCatalogRegistry,
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
/// The credential read is async; the trait surface is sync. Desktop runs on a
/// multi-thread tokio runtime, so we bridge with
/// `block_in_place` + `Handle::block_on` (safe only on `rt-multi-thread`,
/// which the desktop binary uses).
pub(super) struct CredentialStoreAuthProvider {
    pub(super) credentials: Arc<CredentialManager>,
    pub(super) base_api_url: String,
}

impl CredentialStoreAuthProvider {
    /// Snapshot the current persisted tokens (`None` when unauthenticated or the
    /// keychain read fails / the bridge cannot run — e.g. off a multi-thread
    /// runtime). Reading at call-time keeps the token fresh across refreshes.
    pub(super) fn snapshot(&self) -> Option<secret::credential::OAuthTokens> {
        let creds = self.credentials.clone();
        let read = move || {
            tokio::runtime::Handle::try_current()
                .ok()
                .and_then(|h| h.block_on(async { creds.get_oauth_tokens().await.ok().flatten() }))
        };
        // `block_on` inside an async task requires `block_in_place` (multi-thread
        // runtime). If we're already off-runtime, call directly.
        match tokio::runtime::Handle::try_current() {
            Ok(_) => tokio::task::block_in_place(read),
            Err(_) => None,
        }
    }
}

impl tool_cron::ClaudeAiAuthProvider for CredentialStoreAuthProvider {
    fn access_token(&self) -> Option<String> {
        self.snapshot()
            .map(|t| t.access_token.expose_secret().clone())
    }
    fn org_uuid(&self) -> Option<String> {
        self.snapshot().map(|t| t.org_id).filter(|o| !o.is_empty())
    }
    fn base_api_url(&self) -> String {
        self.base_api_url.clone()
    }
}

/// The resolved LLM stack: credentials, the assembled multi-provider config,
/// and the routed [`DefaultLlmClient`] every model-facing surface needs.
///
/// Extracted verbatim out of [`build`] so that headless one-shot commands
/// (which must NOT boot a session, fire `SessionStart` hooks, or start MCP
/// servers) reach the SAME credential-precedence and provider-assembly logic
/// the interactive runtime uses. Duplicating that resolution is the failure
/// mode this type exists to prevent: the API-key-beats-OAuth exclusion and the
/// ChatGPT `PAT > external-tokens > OAuth login` precedence are security
/// boundaries, and a second copy of them would drift silently.
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
    pub mcp_oauth_storage: Arc<dyn platform_api::SecureStorage>,
    /// See [`build`] for the resolution rules behind `credentials`.
    pub credentials: Arc<CredentialManager>,
    /// See [`build`] for the resolution rules behind `auth`.
    pub auth: Arc<dyn AuthHandle>,
    /// See [`build`] for the resolution rules behind `subscription`.
    pub subscription: platform_api::subscription::SharedSubscription,
    /// See [`build`] for the resolution rules behind `resolved_anthropic_api_key`.
    pub resolved_anthropic_api_key: Option<String>,
    /// See [`build`] for the resolution rules behind `is_subscriber`.
    pub is_subscriber: bool,
    /// See [`build`] for the resolution rules behind `openai_oauth_client`.
    pub openai_oauth_client: Arc<openai_oauth::OpenAiOAuthClient>,
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
    pub default_listings: Vec<platform_api::ModelListing>,
    /// See [`build`] for the resolution rules behind `default_model_id`.
    pub default_model_id: String,
    /// See [`build`] for the resolution rules behind `default_model_profile`.
    pub default_model_profile: Option<String>,
    /// See [`build`] for the resolution rules behind `profile_first_party`.
    pub profile_first_party: std::collections::BTreeMap<String, bool>,
    /// Claude auto-mode provider tag for each configured profile.
    pub profile_auto_mode_provider: std::collections::BTreeMap<String, String>,
    /// Provider tag applied to the built-in Anthropic profile after env routing.
    pub first_party_environment_provider: String,
    /// See [`build`] for the resolution rules behind `provider_availability`.
    pub provider_availability: std::collections::BTreeMap<String, bool>,
    /// See [`build`] for the resolution rules behind `default_model_fallback`.
    pub default_model_fallback: Option<DefaultModelFallbackNotice>,
    /// See [`build`] for the resolution rules behind `model_provenance`.
    pub model_provenance: platform_api::ModelProvenance,
    /// See [`build`] for the resolution rules behind `session_model_restriction`.
    pub session_model_restriction:
        Option<(llm_runtime::model::allowlist::ModelEnforcement, Vec<String>)>,
    /// See [`build`] for the resolution rules behind `model_setting_for_spawns`.
    pub model_setting_for_spawns: String,
    /// See [`build`] for the resolution rules behind `session_provider_first_party`.
    pub session_provider_first_party: bool,
    /// Resolved provider tag for the session's boot auto-mode gate.
    pub session_auto_mode_provider: String,
    /// See [`build`] for the resolution rules behind `llm_runtime`.
    pub llm_runtime: Arc<DefaultLlmClient>,
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
    /// Catalog notifications owned by this credential scope.
    pub catalog_registry: FusionCatalogRegistry,
    /// Platform HTTP transport used by the credential manager.
    pub http: Arc<PosixHttp>,
    /// Platform clock used by the credential manager.
    pub clock: Arc<PosixClock>,
    /// Shared storage handle, also reused by MCP OAuth persistence.
    pub storage: Arc<dyn platform_api::SecureStorage>,
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
/// So the storage choice keys off the credential ROOT, not only off the
/// caller's `isolated_credential_storage` flag. This deliberately does NOT
/// widen that flag: it also suppresses ambient provider credentials (see
/// `compute_availability_with_isolation`) and pins the Fusion catalog
/// refresher's isolation, and "the home is scratch" is no reason to stop
/// honouring an `ANTHROPIC_API_KEY` the caller put in the environment on
/// purpose.
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
    build_shared_credential_stack_with_policy(
        &cfg.lingxi_home,
        cfg.isolated_credential_storage,
        cfg.credential_storage_policy,
    )
    .await
}

pub(super) async fn build_platform_plaintext_secure_storage(
    credentials_path: PathBuf,
) -> Result<Arc<dyn platform_api::SecureStorage>, platform_api::SecureStorageError> {
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
) -> Result<Arc<dyn platform_api::SecureStorage>, platform_api::SecureStorageError> {
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
    let shared = build_shared_credential_stack_for_config(cfg).await?;
    resolve_llm_stack_with_credentials(cfg, shared).await
}

pub(super) async fn resolve_llm_stack_with_credentials(
    cfg: &DesktopConfig,
    shared: SharedCredentialStack,
) -> Result<LlmStack, BuildError> {
    // Resolve the winning model setting from the canonical tier stack before
    // assembling provider profiles. Managed `settings.model` is the only
    // administrator-owned default; explicit CLI/env pins remain user-owned.
    let managed_settings_for_model =
        crate::desktop::settings_watch::managed_settings_raw_tiers().await;
    let effective_settings_for_model =
        load_effective_settings_for_config(cfg, &managed_settings_for_model);
    let mut model_provenance =
        model_provenance_for_config(cfg, effective_settings_for_model.as_ref());
    let configured_model =
        managed_model_setting_for_config(cfg, effective_settings_for_model.as_ref())
            .unwrap_or_else(|| cfg.default_model.clone());
    // (1) Platform-minimal façade shared byte-for-byte with CLI/TUI auth.
    let SharedCredentialStack {
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

    // (2a) Task 10: LlmTransportBridge wraps the PosixHttp transport for
    //      `DefaultLlmClient`. A second `PosixHttp` instance is used so the
    //      bridge owns its own (stateless) handle; the original `http` Arc
    //      continues to serve MCP / hooks / side-query.
    let llm_transport: Arc<dyn Transport> = Arc::new(LlmTransportBridge::new(PosixHttp::new()));
    // Defer client construction to step 3.1 where we know whether OAuth is
    // active (determines auth strategy + credential config). Placeholder: the
    // resolved OAuth `AuthState` (`Some` only for an OAuth-effective subscriber
    // session) that step (2) bridges into the assembled client's credential
    // seam as an `oauth_delegate`.
    let mut oauth_auth_state: Option<Arc<llm_runtime::oauth::anthropic::refresh::AuthState>> = None;
    let mut openai_oauth_state: Option<Arc<openai_oauth::AuthState>> = None;
    // (3) Credential manager + OAuth client (used by /login, /logout).
    //
    // Task 4 (future-work batch 4): the shared subscription slot UI layers read
    // at compose time. Seeded with the conservative default snapshot here; the
    // `Ok(Some(tokens))` arm below re-seeds it with the resolved subscriber
    // flag, and (for subscribers) a background profile+roles fetch overwrites
    // it with the full snapshot once the endpoints respond.
    let subscription: platform_api::subscription::SharedSubscription =
        std::sync::Arc::new(std::sync::RwLock::new(Some(
            platform_api::subscription::SubscriptionSnapshot::default(),
        )));
    // `mcp_oauth_storage` and `credentials` originate from the same shared
    // stack, so provider keys and MCP OAuth never split across backends.
    // (M13) Track WHERE the key came from — the auth resolver ranks an
    // env/host-supplied key ABOVE stored OAuth but a keychain-stored key BELOW
    // it, so the two sources must stay distinguishable.
    let mut stored_anthropic_api_key = false;
    let resolved_anthropic_api_key = if cfg.api_key.is_empty() {
        match credentials.get_anthropic_api_key().await {
            Ok(Some(key)) => {
                stored_anthropic_api_key = true;
                Some(key.expose_secret().clone())
            }
            Ok(None) => None,
            Err(error) => {
                tracing::warn!(%error, "could not read Anthropic API key from secure storage");
                None
            }
        }
    } else {
        Some(cfg.api_key.clone())
    };
    let oauth_cfg = ClaudeAiOAuthConfig::default_with_port(0);
    let oauth_client = Arc::new(ClaudeAiOAuthClient::new(
        oauth_cfg.clone(),
        http.clone(),
        credentials.clone(),
    ));
    let auth: Arc<dyn AuthHandle> = Arc::new(OAuthHandle::new(oauth_client));

    // (3.1) M5-13 / Task 10: build the OAuth refresh driver when the keychain
    //        already holds a logged-in OAuth token.  `init_refresh_driver` spawns
    //        the proactive-refresh task and returns the shared `AuthState`.  The
    //        returned state is used BOTH for the old api-client hook path (removed
    //        in Plan 3a Task 9) and to wire `OAuthCredentialProvider` into the
    //        new `DefaultLlmClient` path.
    //
    //        (3.2) API.6: while we have the token in hand, resolve the Claude.ai
    //        subscriber flag from its scopes (see [`oauth_subscriber_flag`]).
    let mut is_subscriber = false;
    let mut persisted_subscription_type: Option<String> = None;
    // Where the Anthropic credential came from, for the error copy that names
    // WHICH setting to unset when a 403 says API-key auth is off. Accumulated
    // beside `is_subscriber` because `auth_source` below is scoped to the match
    // arm. Defaults keep the `/login` wording, which is right for a stored or
    // OAuth credential.
    let mut credential_origin = orchestrator::api_error_copy::CredentialOrigin::Other;
    let mut has_oauth_token = false;
    match credentials.get_oauth_tokens().await {
        Ok(Some(tokens)) => {
            // (M13) Drive the documented auth-source resolver with the full
            // context instead of a hand-rolled two-flag exclusion: HOST-forced
            // OAuth makes the stored session the effective auth EVEN with an
            // env key present, an FD-inherited key outranks the stored
            // session, and a keychain-stored key ranks BELOW it. The
            // below-OAuth sources (settings key / helper / Bedrock) cannot
            // change the outcome once `has_stored_oauth` is true, so their
            // slots stay conservative.
            let auth_source = llm_runtime::oauth::anthropic::resolver::resolve(
                &llm_runtime::oauth::anthropic::resolver::ResolverContext {
                    // The ONLY thing that demotes an env key below the
                    // stored session is `KWr()` (@228931361), read HERE —
                    // the credential-resolution point, exactly where
                    // `zb()` (@228933355) evaluates it — so EVERY
                    // entrypoint agrees, including the ones that never
                    // pass through the CLI's `build_runtime_from_config`
                    // (`mcp serve`, `auto-mode-setup`, bridge-server).
                    managed_oauth_only: cfg.managed_oauth_only
                        || llm_runtime::oauth::anthropic::resolver::host_managed_oauth_only(),
                    env_auth_token: std::env::var("ANTHROPIC_AUTH_TOKEN")
                        .ok()
                        .filter(|v| !v.is_empty()),
                    env_api_key: (!stored_anthropic_api_key)
                        .then(|| resolved_anthropic_api_key.clone())
                        .flatten(),
                    fd_present: cfg.anthropic_key_fd_present,
                    has_stored_oauth: true,
                    has_stored_api_key: stored_anthropic_api_key,
                    settings_api_key: None,
                    api_key_helper_script: cfg
                        .api_key_helper
                        .as_ref()
                        .map(std::path::PathBuf::from),
                    aws_present: false,
                },
            );
            // (M13) The stored credential carries the tier persisted at login
            // (claude-code keeps `subscriptionType`/`rateLimitTier` inside
            // `claudeAiOauth`), so enterprise/tier-gated behaviour is correct
            // from request #1 — no async profile-fetch window — but only while
            // the stored session is the effective auth source (see
            // [`subscription_seed`]).
            // Oracle `e1().source`, narrowed to what the copy branches on.
            // Only the two EXTERNAL sources map: `/login managed key` has no
            // LingXi equivalent, and everything else takes the `/login` wording
            // anyway.
            credential_origin = match &auth_source {
                llm_runtime::oauth::anthropic::resolver::AuthSource::EnvApiKey => {
                    // This is the ANTHROPIC auth resolver, and its `EnvApiKey`
                    // is defined as `ANTHROPIC_API_KEY` (see `AuthSource`), so
                    // naming the variable here is a fact, not a guess. Another
                    // provider's resolver supplies its own `apiKeyEnv` name.
                    orchestrator::api_error_copy::CredentialOrigin::EnvApiKey {
                        var: "ANTHROPIC_API_KEY".to_string(),
                    }
                }
                llm_runtime::oauth::anthropic::resolver::AuthSource::ApiKeyHelper { .. } => {
                    orchestrator::api_error_copy::CredentialOrigin::ApiKeyHelper
                }
                _ => orchestrator::api_error_copy::CredentialOrigin::Other,
            };
            // Oracle `zv()` = `ms()?.accessToken != null` — reaching this arm
            // means stored OAuth tokens were read.
            has_oauth_token = true;
            let seed = subscription_seed(
                &auth_source,
                &tokens.scopes,
                tokens.subscription_type.as_ref(),
                tokens.rate_limit_tier.as_ref(),
            );
            is_subscriber = seed.is_subscriber;
            persisted_subscription_type.clone_from(&seed.subscription_type);
            // Re-seed the shared slot with the resolved subscriber flag + the
            // PERSISTED tier so readers see them even before (or without) the
            // background profile+roles fetch landing. SECRECY: deliberately
            // copy the access token (a `Secret<String>`, intentionally
            // non-`Clone`) by exposing + re-wrapping — the audited copy
            // pattern — BEFORE the original moves into `init_refresh_driver`;
            // it is exposed again only inside the spawned fetch task.
            if let Ok(mut guard) = subscription.write() {
                *guard = Some(seed);
            }
            let profile_token = protocol::Secret::new(tokens.access_token.expose_secret().clone());
            match llm_runtime::oauth::anthropic::client::init_refresh_driver(
                oauth_cfg,
                tokens.access_token,
                tokens.refresh_token,
                tokens.expires_at,
                http.clone(),
                clock.clone(),
                Some(Arc::new(telemetry::AnalyticsBus::new())),
                Some(credentials.clone()),
                Arc::new(PosixRuntime::new()),
            )
            .await
            {
                Ok(auth_state) => {
                    // Task 10: capture the OAuth `AuthState` so the assembled
                    // client gets an `OAuthBearer` credential delegate below.
                    // Only active when the subscriber flag confirms OAuth is the
                    // effective auth source (API-key overrides it).
                    if is_subscriber {
                        oauth_auth_state = Some(auth_state);

                        // Task 4: background OAuth profile + roles fetch — the
                        // FRESHENER over the persisted-tier seed above (closes
                        // the RENDERING half of the profile-fetch PARITY-GAP
                        // documented at `orchestrator/src/config.rs:134` —
                        // tier/billing/role data for rate-limit copy — without
                        // touching the build hot path). Both fetchers swallow
                        // every error → `None` (matching the TS `logError` /
                        // `return undefined` stance); (M13) a FAILED profile
                        // fetch skips the write entirely so the seeded
                        // persisted-tier snapshot is never clobbered with an
                        // empty one (oracle preserves stored `subscriptionType`
                        // when the refresh can't resolve a new value).
                        //
                        // SharedSubscription locking contract (std `RwLock`):
                        // the guard must NEVER be held across an `.await` —
                        // build the full snapshot FIRST, then write-and-drop.
                        // Poisoned-lock stance: writer skips on poison
                        // (`if let Ok(mut guard)`); readers degrade to the
                        // default snapshot. SECRECY: the access token is
                        // exposed (`expose_secret`) only into the two fetch
                        // calls and never logged or formatted.
                        {
                            let slot = subscription.clone();
                            let transport: std::sync::Arc<dyn platform_api::HttpTransport> =
                                http.clone();
                            let creds = credentials.clone();
                            // Move (not copy) the token into the task — its
                            // only consumer.
                            let token = profile_token;
                            tokio::spawn(async move {
                                let token = token.expose_secret();
                                let Some(profile) =
                                    llm_runtime::oauth::anthropic::fetch_profile_from_oauth_token(
                                        token, &transport,
                                    )
                                    .await
                                else {
                                    return;
                                };
                                let roles = llm_runtime::oauth::anthropic::fetch_user_roles(
                                    token, &transport,
                                )
                                .await;
                                let snap = subscription_snapshot_from(
                                    true,
                                    Some(&profile),
                                    roles.as_ref(),
                                );
                                // (M13) Freshen the persisted tier too, so
                                // pre-M13 logins self-heal and the NEXT boot
                                // seeds from up-to-date values. `new ?? old`
                                // merge — never clears a stored tier.
                                if let Err(error) = creds
                                    .update_oauth_subscription(
                                        snap.subscription_type.as_deref(),
                                        snap.rate_limit_tier.as_deref(),
                                    )
                                    .await
                                {
                                    tracing::warn!(%error, "could not persist freshened subscription tier");
                                }
                                if let Ok(mut guard) = slot.write() {
                                    *guard = Some(snap);
                                }
                            });
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "failed to attach OAuth refresh driver; 401 auto-refresh disabled");
                }
            }
        }
        Ok(None) => {
            // No stored OAuth session — API-key path.
        }
        Err(e) => {
            tracing::warn!(error = %e, "could not read OAuth tokens from keychain; skipping refresh-driver wiring");
        }
    }

    // (3.2a) Wire the OpenAI ChatGPT OAuth refresh driver when the keychain
    //        already holds a ChatGPT session. Mirrors the anthropic block above.
    //        Returns `Arc<openai_oauth::AuthState>` for the credential delegate;
    //        on Ok(None) / Err we leave `openai_oauth_state = None` (warn on Err).
    //        No subscriber-flag / profile-fetch needed for OpenAI — minimal path.
    let openai_oauth_cfg = openai_oauth::OpenAiOAuthConfig::default();
    let openai_oauth_client = Arc::new(openai_oauth::OpenAiOAuthClient::new(
        openai_oauth_cfg.clone(),
        http.clone(),
    ));

    // (3.2a-pre) P3 enterprise precedence for the openai-chatgpt credential:
    // PAT env  >  external-tokens env  >  OAuth login session. First hit wins.
    let mut openai_chatgpt_delegate: Option<Arc<dyn llm_runtime::CredentialProvider>> = None;
    if let Ok(pat) = std::env::var("OPENAI_PERSONAL_ACCESS_TOKEN") {
        if !pat.trim().is_empty() {
            let http_dyn: Arc<dyn platform_api::HttpTransport> =
                http.clone() as Arc<dyn platform_api::HttpTransport>;
            match openai_oauth::whoami(&openai_oauth_cfg, &http_dyn, &pat).await {
                Ok(md) => {
                    openai_chatgpt_delegate =
                        Some(Arc::new(openai_oauth::PatCredentialProvider::new(pat, md))
                            as Arc<dyn llm_runtime::CredentialProvider>);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "OPENAI_PERSONAL_ACCESS_TOKEN whoami failed; ignoring PAT")
                }
            }
        }
    }
    if openai_chatgpt_delegate.is_none() {
        match (
            std::env::var("OPENAI_CHATGPT_ACCESS_TOKEN")
                .ok()
                .filter(|s| !s.trim().is_empty()),
            std::env::var("OPENAI_CHATGPT_ACCOUNT_ID")
                .ok()
                .filter(|s| !s.trim().is_empty()),
        ) {
            (Some(tok), Some(acc)) => {
                openai_chatgpt_delegate = Some(Arc::new(
                    openai_oauth::ExternalTokensCredentialProvider::from_supplied(tok, Some(acc)),
                )
                    as Arc<dyn llm_runtime::CredentialProvider>);
            }
            (Some(_), None) | (None, Some(_)) => tracing::warn!(
                "incomplete external ChatGPT tokens: set BOTH OPENAI_CHATGPT_ACCESS_TOKEN and OPENAI_CHATGPT_ACCOUNT_ID"
            ),
            (None, None) => {}
        }
    }

    if openai_chatgpt_delegate.is_none() {
        match credentials.get_openai_oauth_tokens().await {
            Ok(Some(tokens)) => {
                match openai_oauth::client::init_refresh_driver(
                    openai_oauth_cfg.clone(),
                    tokens.access_token,
                    tokens.refresh_token,
                    tokens.expires_at,
                    tokens.account_id,
                    tokens.fedramp,
                    tokens.email,
                    http.clone(),
                    clock.clone(),
                    Some(Arc::new(telemetry::AnalyticsBus::new())),
                    Some(credentials.clone()),
                    Arc::new(PosixRuntime::new()),
                )
                .await
                {
                    Ok(state) => {
                        openai_oauth_state = Some(state);
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "failed to attach OpenAI OAuth refresh driver; ChatGPT routing disabled");
                    }
                }
            }
            Ok(None) => {
                // No stored ChatGPT OAuth session.
            }
            Err(e) => {
                tracing::warn!(error = %e, "could not read OpenAI OAuth tokens from keychain; skipping chatgpt refresh-driver wiring");
            }
        }
    }

    // (3.3) Phase 2a §8: assemble the FULL multi-provider client config
    //       (Anthropic + builtin catalog presets + settings `providers`) + chains
    //       + credential sources + pricing catalog, instead of the single-Anthropic
    //       config. `provider_config::assemble` owns the byte-equivalent Anthropic
    //       profile + the catalog merge; the engine bridges OAuth in via a
    //       pre-built delegate so provider-config stays free of an anthropic-oauth
    //       dep. A bad settings entry only emits a warning — the engine still boots
    //       with every well-formed profile (incl. the built-in Anthropic one).
    let has_api_key = resolved_anthropic_api_key.is_some();
    // OAuth bridges into the client exactly when the auth RESOLVER made the
    // stored session the effective source (M13): `oauth_auth_state` is only
    // captured for an OAuth-effective subscriber session, which outranks a
    // keychain-stored key and — under HOST forcing (`KWr()` @228931361) — even
    // an env key. `has_oauth` selects `AuthStrategy::OAuthBearer`, which
    // is what injects the required `oauth-2025-04-20` beta on Anthropic routes;
    // the assemble input below drops the key claim when OAuth is effective so
    // the ApiKey strategy can't shadow it.
    let has_oauth = oauth_auth_state.is_some();
    let oauth_delegate: Option<Arc<dyn llm_runtime::CredentialProvider>> =
        oauth_auth_state.clone().map(|state| {
            let driver = Arc::new(RefreshDriver::new(state));
            Arc::new(OAuthCredentialProvider::new(driver))
                as Arc<dyn llm_runtime::CredentialProvider>
        });

    let provider_region = match effective_settings_for_model
        .as_ref()
        .and_then(|settings| settings.settings.provider_region)
        .unwrap_or_default()
    {
        lingxi_core::settings::ProviderRegion::ChinaMainland => llm_runtime::Region::ChinaMainland,
        lingxi_core::settings::ProviderRegion::International => llm_runtime::Region::International,
    };
    let assembled = provider_config::assemble_for_region(
        provider_config::AssembleInputs {
            anthropic_api_base: cfg.api_base.clone(),
            anthropic_models: anthropic_models_for(
                &configured_model,
                cfg.fallback_model.as_deref(),
            ),
            anthropic_has_api_key: has_api_key && !has_oauth,
            anthropic_has_oauth: has_oauth,
            user_providers: cfg.provider_profiles.clone().unwrap_or_default(),
            routing: cfg.routing.clone(),
        },
        provider_region,
    );
    for w in &assembled.warnings {
        tracing::warn!(warning = %w, "provider-config assembly");
    }

    // (Phase 2a I1/I2) Authoritative `request_model -> (profile_name,
    // provider_label)` map for the `/model` picker, built from the assembled
    // multi-provider config BEFORE `client_config` is consumed by `from_config`.
    // Every profile (anthropic + presets + USER providers) contributes its
    // `models[].request_model`. First-profile-wins on a duplicate request_model
    // (anthropic + presets come before user profiles in `assemble`).
    let mut model_providers: std::collections::BTreeMap<String, (String, String)> =
        std::collections::BTreeMap::new();
    for profile in &assembled.client_config.providers {
        let label = provider_profile_label(&profile.profile_name);
        for model in &profile.models {
            model_providers
                .entry(model.request_model.clone())
                .or_insert_with(|| (profile.profile_name.clone(), label.clone()));
        }
    }
    let fusion_catalog = assembled
        .client_config
        .providers
        .iter()
        .flat_map(|provider| {
            let billing_mode = provider.pricing.billing_mode;
            let protocol = provider.protocol.clone();
            provider.models.iter().map(move |model| {
                desktop_fusion_catalog_row(&provider.profile_name, model, billing_mode, &protocol)
            })
        })
        .collect::<Vec<_>>();

    // Task-5 (TPM-C): resolve an optional `profile/model` qualifier in the
    // configured default_model so a shared id routes deterministically on the
    // first turn.  Must run while `assembled.client_config.providers` is still
    // owned (before `from_config` moves it).
    let default_listings: Vec<platform_api::ModelListing> = assembled
        .client_config
        .providers
        .iter()
        .flat_map(|p| {
            let profile = p.profile_name.clone();
            let label = provider_profile_label(&p.profile_name);
            p.models.iter().map(move |m| platform_api::ModelListing {
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
        platform_api::parse_model_ref(&configured_model, &default_listings);

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
    let profile_first_party = profile_auto_mode_provider
        .iter()
        .map(|(profile, provider)| (profile.clone(), provider == "firstParty"))
        .collect();

    let mut client = DefaultLlmClient::from_config(assembled.client_config)
        .map_err(|e| BuildError::ApiBase(format!("llm-runtime config: {e}")))?;
    // §6.1: ONE composite credential slot for ALL providers (anthropic api-key /
    // oauth-delegate + every per-profile credential source).
    let mut oauth_delegates: std::collections::BTreeMap<
        String,
        std::sync::Arc<dyn llm_runtime::CredentialProvider>,
    > = std::collections::BTreeMap::new();
    if let Some(d) = oauth_delegate {
        oauth_delegates.insert("anthropic-oauth".to_string(), d);
    }
    // OAuth login fills the slot only if PAT/external didn't.
    if openai_chatgpt_delegate.is_none() {
        if let Some(state) = openai_oauth_state {
            let driver = std::sync::Arc::new(openai_oauth::RefreshDriver::new(state));
            openai_chatgpt_delegate = Some(std::sync::Arc::new(
                openai_oauth::OpenAiOAuthCredentialProvider::new(driver),
            )
                as Arc<dyn llm_runtime::CredentialProvider>);
        }
    }
    let has_openai_chatgpt = openai_chatgpt_delegate.is_some();
    if let Some(d) = openai_chatgpt_delegate {
        oauth_delegates.insert("openai-chatgpt".to_string(), d);
    }

    // Phase 2a §6.2: per-profile availability from the assembled credential
    // sources (each profile is "available" iff its keychain entry / env var
    // resolves). Computed HERE — the earliest point all five inputs exist — so
    // the connected-provider default-model fallback below can consult it; the
    // same map later drives the `/model` picker's Connect badge via
    // `DesktopRuntime.provider_availability`. Nothing between here and the
    // runtime literal mutates credentials, so early == late computation.
    let availability_probe = provider_config::compute_availability_with_isolation(
        &credentials,
        &assembled.credential_sources,
        has_api_key,
        has_oauth,
        has_openai_chatgpt,
        // Isolated boots ignore ambient provider env vars too — see the
        // `isolated_credential_storage` doc: the flag means "this boot inherits
        // no machine credentials", and env is the other half of that.
        cfg.isolated_credential_storage,
    );
    // Finding [7]: distinguish "the probe ran and found nothing" from "the
    // probe never completed" (a 5s timeout, e.g. a slow/contended macOS
    // keychain). `provider_availability` is otherwise empty in BOTH cases,
    // and `filter_fusion_catalog`'s `unwrap_or(false)` cannot tell them
    // apart — on the timeout path every non-anthropic profile reads as
    // "genuinely uncredentialed" and the whole Fusion catalog for that
    // profile is dropped for the runtime's lifetime, even though the
    // ordinary turn loop routes the same profile fine on the same
    // credentials. `availability_probe_completed` lets the Fusion filter
    // skip its availability half instead of fail-closing on a transient
    // stall (mirrors the sibling `connected_provider_fallback` rule at
    // line ~5898, which already treats an unknown provider as "don't
    // reroute" rather than "disconnected").
    let (availability_rows, availability_probe_completed) =
        match tokio::time::timeout(std::time::Duration::from_secs(5), availability_probe).await {
            Ok(rows) => (rows, true),
            Err(_) => {
                tracing::warn!("provider availability probe timed out; continuing engine startup");
                (Vec::new(), false)
            }
        };
    let mut provider_availability: std::collections::BTreeMap<String, bool> = availability_rows
        .into_iter()
        .map(|a| (a.profile_name, a.available))
        .collect();
    // `assemble` emits NO anthropic credential source in the unauthenticated
    // (no key / no oauth) path, so `compute_availability` yields no "anthropic"
    // entry there. The picker's Connect badge still needs anthropic represented,
    // so surface it unconditionally from the engine's resolved auth state.
    provider_availability
        .entry("anthropic".to_string())
        .or_insert(has_api_key || has_oauth);

    // The anthropic probe is DEFINITIVE only on the stock first-party API
    // (`api_provider() == FirstParty`) with the default `api_base` and no
    // gateway auth override. On an env-routed Bedrock/Vertex/Foundry install
    // (`api_provider() != FirstParty`), a custom `api_base`
    // (`LINGXI_API_BASE_URL` — an enterprise/auth-free gateway serving Claude
    // with no local key), or an `ANTHROPIC_AUTH_TOKEN`, anthropic models are
    // served WITHOUT a local key/OAuth, so the forced
    // `availability["anthropic"] = false` above is probe-BLINDNESS, not
    // disconnection — both the default-model fallback below and
    // `filter_fusion_catalog` (F011 item 1) must not treat it as
    // disconnection (the same probe-blindness `connected_model_rows` guards
    // in the TUI picker).
    let anthropic_probe_definitive = api_provider() == ApiProvider::FirstParty
        && cfg.api_base == DesktopConfig::default().api_base
        && std::env::var("ANTHROPIC_AUTH_TOKEN").map_or(true, |v| v.is_empty());

    // ── Boot-time connected-provider default-model fallback ─────────────────
    // (LingXi multi-provider divergence — upstream is Anthropic-only.) When the
    // configured default model's provider is definitively disconnected and
    // another provider IS connected, boot on the connected provider instead of
    // into guaranteed first-turn auth failures. Skipped when the model was an
    // EXPLICIT `--model` choice (the user asked for exactly that model), and on
    // env-routed Bedrock/Vertex/Foundry installs (anthropic models are served
    // WITHOUT anthropic key/oauth there, so "anthropic disconnected" is
    // meaningless and the reroute would break a working setup).
    let mut default_model_fallback: Option<DefaultModelFallbackNotice> = None;
    // An `ANTHROPIC_MODEL` env pin (claude-code D4) is exempt from the reroute
    // just like an explicit `--model`: the user pinned exactly that model, so a
    // fallback would defeat the pin. `default_model_env_pinned` is kept separate
    // from `default_model_explicit` (which stays `--model`-only for the `--agent`
    // override gate); only the fallback treats an env pin as explicit.
    if !cfg.default_model_explicit
        && !cfg.default_model_env_pinned
        && api_provider() == ApiProvider::FirstParty
    {
        if let Some(fb) = connected_provider_fallback(
            &default_model_id,
            default_model_profile.as_deref(),
            anthropic_probe_definitive,
            &model_providers,
            &provider_availability,
            &default_listings,
            &cfg.recent_models,
        ) {
            let to = format!("{}/{}", fb.profile, fb.model);
            tracing::warn!(
                from = %configured_model,
                to = %to,
                "default model's provider is not connected; booting on a connected provider"
            );
            default_model_fallback = Some(DefaultModelFallbackNotice {
                from: configured_model.clone(),
                to,
            });
            // A disconnected-provider fallback is a catalog choice, not an
            // administrator default, even when the displaced model came from
            // a lower-priority settings source.
            model_provenance = platform_api::ModelProvenance::ProviderCatalogTier;
            default_model_id = fb.model;
            default_model_profile = Some(fb.profile);
        }
    }

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
        let managed_model_tiers =
            crate::desktop::settings_watch::managed_settings_raw_tiers().await;
        let policy_source = managed_model_policy_source(&managed_model_tiers);
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
                    let picked_profile = default_listings
                        .iter()
                        .find(|m| m.request_model == picked)
                        .map(|m| m.provider_id.clone());
                    tracing::warn!(
                        from = %default_model_id,
                        to = %picked,
                        "default model is not in the managed availableModels allowlist; \
                         resolving Default to the first allowed availableModels entry"
                    );
                    model_provenance = platform_api::ModelProvenance::ManagedAdministratorDefault;
                    default_model_id = picked;
                    default_model_profile = picked_profile.or(default_model_profile);
                }
            }
        }
    }

    // Open exactly the credential the session is about to use. Provider
    // availability above is intentionally attribute-only on macOS; loading the
    // selected provider here makes authorization deterministic at session
    // entry without decrypting every unrelated saved key. Anthropic's resolved
    // auth was already loaded earlier in this build path.
    let boot_profile = default_model_profile
        .clone()
        .or_else(|| {
            model_providers
                .get(&default_model_id)
                .map(|(profile, _)| profile.clone())
        })
        .unwrap_or_else(|| "anthropic".to_string());
    if boot_profile != "anthropic" {
        if let Some(source) = assembled
            .credential_sources
            .iter()
            .find(|source| source.profile_name == boot_profile)
        {
            match credentials.has_provider_key(&source.credential_id).await {
                Ok(true) => {
                    if let Err(error) = credentials.get_provider_key(&source.credential_id).await {
                        tracing::warn!(
                            provider = %boot_profile,
                            credential_id = %source.credential_id,
                            %error,
                            "failed to load the session provider credential during boot"
                        );
                    }
                }
                Ok(false) => {}
                Err(error) => tracing::warn!(
                    provider = %boot_profile,
                    credential_id = %source.credential_id,
                    %error,
                    "failed to inspect the session provider credential during boot"
                ),
            }
        }
    }

    // The raw "user model setting" seam (the opusplan/haiku plan-mode swap
    // anchor threaded into subagent/teammate model resolution). When the
    // fallback rerouted the session, the persisted alias no longer describes
    // the booted main loop — thread the rerouted ref instead so a plan-mode
    // `AgentModel::Inherit` spawn cannot swap back onto the provider the
    // fallback just declared disconnected.
    let model_setting_for_spawns = default_model_fallback
        .as_ref()
        .map_or_else(|| configured_model.clone(), |n| n.to.clone());

    // (M10 cc2.1.198) LingXi multi-provider half of the Explore `GAe`/`obm`
    // firstParty gate: `false` when the session's default model routes to a
    // NON-Anthropic provider profile (OpenAI/Gemini/…) so the built-in Explore
    // agent resolves to `inherit` (the opus cap never fires for a foreign
    // provider — same behavior as the TS `fr() !== "firstParty"` branch). The
    // env half (Bedrock/Vertex/Foundry) is checked inside
    // `agent::model_resolution::resolve_builtin_explore_model`. Evaluated over
    // the POST-fallback default (the model the session actually boots on),
    // via the `profile_first_party` capture taken before `from_config`.
    let session_profile_auto_mode_provider = {
        let profile_name = default_model_profile.clone().or_else(|| {
            model_providers
                .get(&default_model_id)
                .map(|(profile, _)| profile.clone())
        });
        match profile_name {
            // Unknown profile name → the built-in Anthropic route.
            Some(name) => profile_auto_mode_provider
                .get(&name)
                .cloned()
                .unwrap_or_else(|| "firstParty".to_string()),
            // No configured profile serves the default model → the built-in
            // Anthropic route (plain api-key / OAuth install).
            None => "firstParty".to_string(),
        }
    };
    let session_provider_first_party = session_profile_auto_mode_provider == "firstParty";
    let first_party_environment_provider = match api_provider() {
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

    let composite = provider_config::MultiCredentialProvider::new(
        credentials.clone(),
        assembled.credential_sources.clone(),
        // A key loaded from CredentialManager is mutable session state, not
        // immutable config. Let MultiCredentialProvider read that shared store
        // on every request so rotation/delete takes effect immediately and a
        // removed key cannot survive in this boot snapshot. Host/env/FD keys
        // remain static because they are outside the credential-write seams.
        (!stored_anthropic_api_key)
            .then(|| resolved_anthropic_api_key.clone())
            .flatten(),
        cfg.api_key_helper.clone(),
        oauth_delegates,
    );
    // GitHub Copilot needs a short-lived token minted from the raw OAuth token
    // (api.githubcopilot.com rejects the raw token). Wrap the composite so the
    // `github-copilot` credential is exchanged + cached; every other credential
    // id passes straight through unchanged.
    let copilot_creds = llm_runtime::CopilotExchangeCredentialProvider::new(
        Arc::new(composite),
        Arc::new(connect::PosixCopilotHttp::new()),
        "github-copilot",
    );
    client = client.with_credential_provider(Arc::new(copilot_creds));
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
    // (M13) Enterprise state now comes from the PERSISTED credential tier
    // (claude-code reads `subscriptionType` synchronously from the stored
    // tokens), so the static build-time state is correct from request #1;
    // the shared subscription slot freshens it per request. `Ger()`
    // (@228959874) is `Aa() === "enterprise"`, so it inherits `Aa()`'s
    // `isAnthropicAuthEnabled` gate — `persisted_subscription_type` is `None`
    // whenever a non-OAuth source outranks the stored blob
    // ([`subscription_seed`]).
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
    // Round-7 finding [2]: the probe-completion flag is shared state, not a
    // construction-time constant. A boot probe that TIMED OUT leaves it
    // `false` (the availability half of `filter_fusion_catalog` fails open,
    // finding [7]); the refresher below re-arms it the first time a
    // mid-session re-probe genuinely completes, so the fail-open is
    // transient like the stall that caused it instead of permanent.
    let fusion_probe_completed = Arc::new(std::sync::atomic::AtomicBool::new(
        availability_probe_completed,
    ));
    let fusion_catalog_mutation_epoch = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let fusion_catalog_source: Arc<dyn fusion::ModelSource> = Arc::new(FusionCatalogModelSource {
        unfiltered: fusion_catalog.clone(),
        availability: fusion_catalog_availability.clone(),
        anthropic_probe_definitive,
        availability_probe_completed: fusion_probe_completed.clone(),
        mutation_epoch: fusion_catalog_mutation_epoch.clone(),
        session_model_restriction: session_model_restriction.clone(),
        reload_managed_model_restriction: true,
    });
    let fusion_catalog_refresher = FusionCatalogRefresher {
        availability: fusion_catalog_availability,
        availability_probe_completed: fusion_probe_completed,
        mutation_epoch: fusion_catalog_mutation_epoch,
        credentials: credentials.clone(),
        credential_sources: assembled.credential_sources.clone(),
        anthropic_has_api_key: fusion_route_flag(has_api_key),
        anthropic_has_oauth: fusion_route_flag(has_oauth),
        openai_chatgpt_available: fusion_route_flag(has_openai_chatgpt),
        // Round-4 review finding: a refresh must reproduce the boot probe
        // exactly, including the isolation boundary — otherwise an isolated
        // boot's first `/connect` replaces the whole availability map with
        // one that counts ambient machine env-var credentials the boot
        // deliberately excluded.
        isolated: cfg.isolated_credential_storage,
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
        anthropic_probe_definitive,
        availability_probe_completed,
        session_model_restriction.as_ref(),
    );

    // Round-12 finding [2], class sweep: `/logout` and the bridge-server's
    // `ClientCommand::Logout` are credential REMOVALS, and the availability
    // map's merge rule can never lower the `true` a sign-in published. Wrap
    // the one shared handle here, after the refresher is registered, so every
    // sign-out surface in this process clears Fusion's entry.
    let auth: Arc<dyn AuthHandle> = Arc::new(FusionCatalogClearingAuth {
        inner: auth,
        catalog_registry: catalog_registry.clone(),
    });

    Ok(LlmStack {
        catalog_registry,
        provider_region,
        http,
        clock,
        mcp_oauth_storage,
        credentials,
        auth,
        subscription,
        resolved_anthropic_api_key,
        is_subscriber,
        openai_oauth_client,
        pricing: assembled.pricing,
        chains: assembled.chains,
        model_providers,
        fusion_catalog,
        fusion_catalog_source,
        fusion_catalog_refresher,
        default_listings,
        default_model_id,
        default_model_profile,
        profile_first_party,
        profile_auto_mode_provider,
        first_party_environment_provider: first_party_environment_provider.to_string(),
        provider_availability,
        default_model_fallback,
        model_provenance,
        session_model_restriction,
        model_setting_for_spawns,
        session_provider_first_party,
        session_auto_mode_provider,
        llm_runtime,
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
    let stack = resolve_llm_stack(cfg).await?;
    Ok(Arc::new(api_service_from_stack(cfg, cwd, stack)))
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
) -> llm_runtime::ApiService {
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
        UserAgentEnv::from_process_env(),
        env!("CARGO_PKG_VERSION"),
        Some(analytics_bus.clone()),
        cfg.fallback_model.clone(),
        Some(stack.cost_estimator),
        fallback_overrides,
        settings_max_retries,
        settings_backoff_ms,
    )
    .with_subscription(stack.subscription)
    .with_custom_cli_betas(cfg.custom_betas.clone())
    .with_thinking(cfg.session_thinking);

    match aws_auth_refresher(cfg, cwd, analytics_bus) {
        Some(refresher) => service.with_aws_auth(refresher),
        None => service,
    }
}

/// Read managed settings synchronously for the already-resolved LLM stack.
/// Restricted sessions use this narrow mirror so user/project/local settings
/// remain excluded while managed AWS refresh policy still applies.
pub(super) fn managed_settings_raw_tiers_sync() -> Vec<String> {
    let managed = crate::desktop::settings_watch::managed_settings_dir();
    let mut out = Vec::new();
    if let Ok(raw) = std::fs::read_to_string(managed.join("managed-settings.json")) {
        out.push(raw);
    }
    let drop_in = managed.join("managed-settings.d");
    let Ok(entries) = std::fs::read_dir(&drop_in) else {
        return out;
    };
    let mut names = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_string = name.to_string_lossy();
        if name_string.ends_with(".json") && !name_string.starts_with('.') {
            names.push(name);
        }
    }
    names.sort();
    for name in names {
        if let Ok(raw) = std::fs::read_to_string(drop_in.join(name)) {
            out.push(raw);
        }
    }
    out
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
) -> Option<Arc<llm_runtime::AwsAuthRefresher>> {
    let aws_settings = if cfg.restricted {
        let managed = managed_settings_raw_tiers_sync();
        load_effective_settings_for_config(cfg, &managed)
    } else {
        let env_vars: std::collections::BTreeMap<String, String> = std::env::vars().collect();
        lingxi_core::settings::Settings::load(lingxi_core::settings::LoadInputs {
            env: &env_vars,
            project_dir: cwd,
            defaults: lingxi_core::settings::schema::SettingsJson::default(),
        })
        .ok()
    }
    .map(|eff| {
        let from_project = |field: &str| {
            eff.effective_for(field).is_some_and(|p| {
                p.contributors.last() == Some(&lingxi_core::settings::tracer::Source::Project)
            })
        };
        llm_runtime::AwsAuthSettings {
            aws_auth_refresh: eff.settings.aws_auth_refresh.clone(),
            aws_auth_refresh_from_project: from_project("awsAuthRefresh"),
            aws_credential_export: eff.settings.aws_credential_export.clone(),
            aws_credential_export_from_project: from_project("awsCredentialExport"),
            // No global config path ⇒ the CLI trust gate proceeds (mode.rs
            // `trust_gate_should_prompt` — nothing to check against), so treat
            // as trusted like the gate does.
            workspace_trusted: match migrations::global_config::global_config_path() {
                Some(p) => migrations::global_config::check_has_trust_dialog_accepted(&p, cwd),
                None => true,
            },
        }
    })
    .unwrap_or_default();
    if aws_settings.aws_auth_refresh.is_none() && aws_settings.aws_credential_export.is_none() {
        return None;
    }
    Some(Arc::new(llm_runtime::AwsAuthRefresher::new(
        aws_settings,
        Arc::new(llm_runtime::ShellAwsAuthProcess),
        Some(analytics_bus),
    )))
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
) -> Option<(protocol::SessionId, u64)> {
    let config_path = config_path?;
    let project_key = migrations::global_config::project_path_for_config(cwd);
    let project = migrations::global_config::get_project_config(config_path, &project_key).ok()?;
    let session = project
        .get("lastSessionId")
        .and_then(serde_json::Value::as_str)?;
    let session_id = protocol::SessionId::parse_prefixed(session)?;
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
