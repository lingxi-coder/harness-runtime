//! Credential storage for exact provider accounts, OAuth grants and plugin secrets.

use lingxi_core::host::{Clock, HttpTransport, SecureStorage, SecureStorageError};
use lingxi_core::types::{Secret, SecureStorageData, SecureStorageMetadata};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::SystemTime;
use thiserror::Error;
use tokio::sync::{Mutex, RwLock};

use crate::kinds::SecretKind;

/// `(service, account)` keys under which the Anthropic OAuth credentials are
/// stored. The access + refresh tokens live in separate secure-storage entries
/// (each labelled with its own [`SecretKind`]); the non-secret session metadata
/// (email / org / expiry / scopes) lives in a third entry so `current_user`
/// can answer without a network round-trip.
const OAUTH_SERVICE: &str = "lingxi";
const OAUTH_ACCESS_ACCOUNT: &str = "anthropic-oauth-access";
const OAUTH_REFRESH_ACCOUNT: &str = "anthropic-oauth-refresh";
const OAUTH_META_ACCOUNT: &str = "anthropic-oauth-meta";

/// `(service, account)` keys under which the `OpenAI` / `ChatGPT` OAuth credentials
/// are stored. Reuses `OAUTH_SERVICE = "lingxi"`.
const OPENAI_OAUTH_ACCESS_ACCOUNT: &str = "openai-oauth-access";
const OPENAI_OAUTH_REFRESH_ACCOUNT: &str = "openai-oauth-refresh";
const OPENAI_OAUTH_META_ACCOUNT: &str = "openai-oauth-meta";

/// Keychain account name for a sensitive plugin `userConfig` value, namespaced
/// by the owning `plugin` identity and field `key`. Mirrors claude-code's
/// `pluginSecrets` `${plugin}/${key}` identity under the shared `lingxi` service.
fn plugin_secret_account(plugin: &str, key: &str) -> String {
    format!("plugin-secret-{plugin}/{key}")
}

/// A full Anthropic OAuth credential set as returned by [`CredentialManager::get_oauth_tokens`].
///
/// `access_token` / `refresh_token` are wrapped in [`Secret`] so they redact in
/// logs; the remaining fields are non-secret session metadata persisted in the
/// `anthropic-oauth-meta` entry.
pub struct OAuthTokens {
    /// Bearer access token.
    pub access_token: Secret<String>,
    /// Long-lived refresh token (absent if the provider never issued one).
    pub refresh_token: Option<Secret<String>>,
    /// Wall-clock expiry instant of the access token.
    pub expires_at: SystemTime,
    /// Scopes granted on the access token.
    pub scopes: Vec<String>,
    /// Signed-in user's email address.
    pub email: String,
    /// Anthropic organization id.
    pub org_id: String,
    /// Claude.ai subscription tier (`"pro"`/`"max"`/`"team"`/`"enterprise"`),
    /// persisted at login from the profile fetch — claude-code stores
    /// `subscriptionType` INSIDE `claudeAiOauth` so enterprise/tier gates are
    /// correct on the FIRST request of a fresh process, before any background
    /// profile refresh lands. `None` on pre-existing blobs / profile-less
    /// tokens.
    pub subscription_type: Option<String>,
    /// Organization `rate_limit_tier` from the same profile fetch
    /// (claude-code `rateLimitTier`).
    pub rate_limit_tier: Option<String>,
}

/// Non-secret session metadata persisted alongside the OAuth tokens. Serialized
/// to JSON and stored in the `anthropic-oauth-meta` entry.
///
/// `subscription_type` / `rate_limit_tier` are `serde(default)` so blobs
/// written before they existed still deserialize.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct OAuthSessionMeta {
    expires_at: SystemTime,
    scopes: Vec<String>,
    email: String,
    org_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    subscription_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rate_limit_tier: Option<String>,
}

/// A full `OpenAI` / `ChatGPT` OAuth credential set as returned by
/// [`CredentialManager::get_openai_oauth_tokens`].
///
/// `access_token` / `refresh_token` are wrapped in [`Secret`] so they redact in
/// logs; the remaining fields are non-secret session metadata persisted in the
/// `openai-oauth-meta` entry.
pub struct OpenAiOAuthTokens {
    /// Bearer access token.
    pub access_token: Secret<String>,
    /// Long-lived refresh token (absent if the provider never issued one).
    pub refresh_token: Option<Secret<String>>,
    /// Wall-clock expiry instant of the access token.
    pub expires_at: SystemTime,
    /// Scopes granted on the access token.
    pub scopes: Vec<String>,
    /// `ChatGPT` account id (from `chatgpt_account_id` JWT claim).
    pub account_id: Option<String>,
    /// `FedRAMP` account flag (from `chatgpt_account_is_fedramp` JWT claim).
    pub fedramp: bool,
    /// Signed-in email (from the `email` JWT claim), for display only.
    pub email: Option<String>,
}

/// Receives rotations for a host-owned persistent OAuth session.
#[async_trait::async_trait]
pub trait OpenAiOAuthObserver: Send + Sync {
    /// Publish a successfully stored session without exposing it to logs.
    async fn updated(&self, tokens: OpenAiOAuthTokens);
}

/// Non-secret session metadata persisted alongside the `OpenAI` OAuth tokens.
/// Serialized to JSON and stored in the `openai-oauth-meta` entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct OpenAiOAuthSessionMeta {
    expires_at: SystemTime,
    scopes: Vec<String>,
    account_id: Option<String>,
    fedramp: bool,
    /// Absent in entries written before email was captured; keep the default
    /// so an old `openai-oauth-meta` blob still deserializes.
    #[serde(default)]
    email: Option<String>,
}

/// One atomic account credential record. Debug output redacts all tokens.
#[derive(Debug, Serialize, Deserialize)]
pub struct AccountCredential {
    /// Revision of this account's stored credential material.
    pub credential_revision: u64,
    /// Account identity generation to which this credential belongs.
    pub identity_generation: u64,
    /// Authentication material for this account.
    pub kind: AccountCredentialKind,
}
/// Authentication material stored for one provider account.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AccountCredentialKind {
    /// A provider-issued API key.
    ApiKey {
        /// API key, redacted in debug output.
        key: Secret<String>,
    },
    /// An OAuth grant and its associated account metadata.
    #[serde(rename = "oauth")]
    OAuth {
        /// Bearer access token, redacted in debug output.
        access_token: Secret<String>,
        /// Refresh token when one was issued by the provider.
        refresh_token: Option<Secret<String>>,
        /// Wall-clock expiry of the access token.
        expires_at: SystemTime,
        /// Scopes granted by the provider.
        scopes: Vec<String>,
        /// Issuer of the OAuth grant.
        issuer: String,
        /// Provider subject identifier, when supplied.
        subject: Option<String>,
        /// Provider organization identifier, when supplied.
        organization_id: Option<String>,
        /// Account email for display, when supplied.
        email: Option<String>,
        /// Whether the account uses the provider's `FedRAMP` environment.
        #[serde(default)]
        fedramp: bool,
    },
}

/// Failure modes for [`CredentialManager`] operations.
#[derive(Debug, Clone, Error)]
pub enum CredentialError {
    /// Underlying [`SecureStorage`] call failed.
    #[error(transparent)]
    Storage(#[from] SecureStorageError),
    /// Stored token had expired and the refresh attempt failed.
    #[error("token expired and refresh failed: {0}")]
    RefreshFailed(String),
    /// No credential is available (and storage returned no entry).
    #[error("no credential available")]
    Unavailable,
}

/// Build display-safe credential text without exposing the full value.
/// Secrets of four characters or fewer are masked completely.
#[must_use]
pub fn masked_credential_preview(value: &str) -> String {
    let suffix = value.chars().rev().take(4).collect::<Vec<_>>();
    if suffix.len() < 4 || value.chars().count() <= 4 {
        return "••••".to_string();
    }
    format!("••••{}", suffix.into_iter().rev().collect::<String>())
}

/// Account refresh authority installed by the actual credential owner.
#[async_trait::async_trait]
pub trait AccountCredentialRefresher: Send + Sync {
    /// Ask the credential owner to replace a rejected account revision.
    /// Returns `None` when the owner cannot provide a replacement.
    async fn refresh(
        &self,
        account: lingxi_core::host::provider_accounts::AccountMetadata,
        rejected_revision: u64,
    ) -> Result<Option<AccountCredential>, CredentialError>;
}

/// Manages host secure-storage credentials and injected account grants.
pub struct CredentialManager {
    account_refresher: std::sync::RwLock<Option<Arc<dyn AccountCredentialRefresher>>>,
    storage: Arc<dyn SecureStorage>,
    account_credentials: RwLock<HashMap<String, Option<AccountCredential>>>,
    clock: Arc<dyn Clock>,
    #[allow(dead_code)]
    http: Arc<dyn HttpTransport>,
    /// Guards read → merge → write over `anthropic-oauth-meta`, so a token
    /// rotation and a subscription refresh running concurrently cannot lose
    /// each other's fields. claude-code funnels every credential mutation
    /// through `ixt()` → `Hcg()` (@228065879), which serializes on an
    /// in-process promise chain plus a `.storage-write` lockfile; this is the
    /// in-process half of that contract.
    session_meta_lock: Mutex<()>,
    /// Host-owned account grants must never fall back to native storage when
    /// the host did not inject the requested account.
    provider_keys_host_managed: AtomicBool,
    /// Sensitive plugin options injected by the packaged Desktop parent.
    /// The parent owns durable Keychain persistence; this cache gives the
    /// plugin loader the same `CredentialManager` API without writing a
    /// second copy through Rust secure storage.
    plugin_secret_cache: RwLock<HashMap<(String, String), Secret<String>>>,
    openai_oauth_observer: std::sync::RwLock<Option<Arc<dyn OpenAiOAuthObserver>>>,
}

impl CredentialManager {
    /// Build a `CredentialManager` over the supplied platform implementations.
    #[must_use]
    pub fn new(
        storage: Arc<dyn SecureStorage>,
        clock: Arc<dyn Clock>,
        http: Arc<dyn HttpTransport>,
    ) -> Self {
        Self {
            storage,
            account_refresher: std::sync::RwLock::new(None),
            account_credentials: RwLock::new(HashMap::new()),
            clock,
            http,
            session_meta_lock: Mutex::new(()),
            provider_keys_host_managed: AtomicBool::new(false),
            plugin_secret_cache: RwLock::new(HashMap::new()),
            openai_oauth_observer: std::sync::RwLock::new(None),
        }
    }

    /// Whether a host authority can refresh account credentials.
    pub fn has_account_credential_refresher(&self) -> bool {
        self.account_refresher
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
    }
    /// Install the host authority that owns account credential refreshes.
    pub fn set_account_credential_refresher(&self, authority: Arc<dyn AccountCredentialRefresher>) {
        *self
            .account_refresher
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(authority);
    }
    /// Delegate a rejected credential revision to the installed host authority.
    /// Returns `None` if no authority is installed or no replacement is available.
    pub async fn refresh_account_credential(
        &self,
        account: lingxi_core::host::provider_accounts::AccountMetadata,
        rejected_revision: u64,
    ) -> Result<Option<AccountCredential>, CredentialError> {
        let authority = self
            .account_refresher
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        match authority {
            Some(authority) => authority.refresh(account, rejected_revision).await,
            None => Ok(None),
        }
    }
    /// Actual backend identity, used with the configured host root to scope account slots.
    pub fn account_storage_backend(&self) -> lingxi_core::host::SecureStorageBackend {
        self.storage.backend()
    }

    /// Install a host-owned credential without writing platform storage.
    pub async fn set_account_credential_ephemeral(
        &self,
        reference: &lingxi_core::host::provider_accounts::CredentialRef,
        credential: Option<AccountCredential>,
    ) -> Result<(), CredentialError> {
        account_slot(reference)?;
        self.account_credentials
            .write()
            .await
            .insert(account_cache_key(reference), credential);
        Ok(())
    }
    /// Read one exact account; never resolve environment or provider-wide fallbacks.
    pub async fn get_account_credential(
        &self,
        reference: &lingxi_core::host::provider_accounts::CredentialRef,
    ) -> Result<Option<AccountCredential>, CredentialError> {
        let service = account_slot(reference)?;
        if let Some(value) = self
            .account_credentials
            .read()
            .await
            .get(&account_cache_key(reference))
        {
            return value.as_ref().map(clone_account_credential).transpose();
        }
        if self.provider_keys_host_managed.load(Ordering::Acquire) {
            return Ok(None);
        }
        self.storage
            .retrieve(&service, &reference.secret_ref)
            .await?
            .map(|data| {
                serde_json::from_slice(data.expose_secret_bytes())
                    .map_err(|_| CredentialError::Unavailable)
            })
            .transpose()
    }
    /// Presence-only read for account settings. Does not decrypt on native backends.
    pub async fn has_account_credential(
        &self,
        reference: &lingxi_core::host::provider_accounts::CredentialRef,
    ) -> Result<bool, CredentialError> {
        let service = account_slot(reference)?;
        if let Some(value) = self
            .account_credentials
            .read()
            .await
            .get(&account_cache_key(reference))
        {
            return Ok(value.is_some());
        }
        if self.provider_keys_host_managed.load(Ordering::Acquire) {
            return Ok(false);
        }
        Ok(self
            .storage
            .contains(&service, &reference.secret_ref)
            .await?)
    }
    /// Store the complete grant in one backend write; no partial three-slot rotations.
    /// The host account lock fences revision checks across processes.
    pub async fn store_account_credential(
        &self,
        reference: &lingxi_core::host::provider_accounts::CredentialRef,
        credential: &AccountCredential,
    ) -> Result<(), CredentialError> {
        let service = account_slot(reference)?;
        let bytes = serde_json::to_vec(credential).map_err(|_| CredentialError::Unavailable)?;
        self.storage
            .store(
                &service,
                &reference.secret_ref,
                SecureStorageData::new(
                    bytes,
                    SecureStorageMetadata {
                        created_at: self.clock.now(),
                        last_accessed: None,
                        kind: lingxi_core::types::SecretKindDto(
                            "provider_account_credential".into(),
                        ),
                    },
                ),
            )
            .await?;
        self.account_credentials
            .write()
            .await
            .remove(&account_cache_key(reference));
        Ok(())
    }
    /// Delete only this account slot. Other accounts and provider defaults are untouched.
    pub async fn delete_account_credential(
        &self,
        reference: &lingxi_core::host::provider_accounts::CredentialRef,
    ) -> Result<(), CredentialError> {
        let service = account_slot(reference)?;
        self.storage.delete(&service, &reference.secret_ref).await?;
        self.account_credentials
            .write()
            .await
            .remove(&account_cache_key(reference));
        Ok(())
    }

    /// Let the host supply account grants through process-local injection.
    /// Missing account injections cannot reach native storage. Plugin secrets
    /// and independent OAuth storage keep their own persistence.
    pub fn use_host_managed_provider_keys(&self) {
        self.provider_keys_host_managed
            .store(true, Ordering::Release);
    }

    /// Attach the packaged host's session persistence observer before boot.
    pub fn set_openai_oauth_observer(&self, observer: Arc<dyn OpenAiOAuthObserver>) {
        *self
            .openai_oauth_observer
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(observer);
    }

    /// Persist a sensitive plugin `userConfig` value in [`SecureStorage`],
    /// keyed by the owning `plugin` identity and field `key`. Stored under the
    /// shared `service = "lingxi"` keychain at
    /// `plugin-secret-{plugin}/{key}`, labelled [`SecretKind::PluginSecret`].
    ///
    /// Parity with claude-code's `pluginSecrets`: a plugin's `sensitive: true`
    /// userConfig fields NEVER land in settings.json — only here. Overwrites any
    /// existing entry so re-configuring rotates the value. Not cached: the
    /// loader reads it live so a freshly stored secret takes effect on the next
    /// plugin load without a restart.
    pub async fn set_plugin_secret(
        &self,
        plugin: &str,
        key: &str,
        secret: &str,
    ) -> Result<(), CredentialError> {
        let metadata = SecureStorageMetadata {
            created_at: self.clock.now(),
            last_accessed: None,
            kind: SecretKind::PluginSecret {
                plugin: plugin.to_string(),
                key: key.to_string(),
            }
            .as_dto(),
        };
        let data = SecureStorageData::new(secret.as_bytes().to_vec(), metadata);
        self.storage
            .store("lingxi", &plugin_secret_account(plugin, key), data)
            .await?;
        self.plugin_secret_cache
            .write()
            .await
            .remove(&(plugin.to_string(), key.to_string()));
        Ok(())
    }

    /// Inject a sensitive plugin option for this process only.
    ///
    /// Packaged Desktop persists these values in its dedicated plugin-secret
    /// Keychain service, then supplies them over the bounded stdin envelope.
    /// Keeping the override here prevents duplicate Keychain prompts and makes
    /// the normal plugin loader observe the value without a special code path.
    pub async fn set_plugin_secret_ephemeral(&self, plugin: &str, key: &str, secret: &str) {
        self.plugin_secret_cache.write().await.insert(
            (plugin.to_string(), key.to_string()),
            Secret::new(secret.to_string()),
        );
    }

    /// Load a sensitive plugin `userConfig` value stored under `(plugin, key)`.
    /// Returns `Ok(None)` when no value has been stored (the loader then treats
    /// the field as absent — required ⇒ `MissingRequired`, optional ⇒ skipped).
    pub async fn get_plugin_secret(
        &self,
        plugin: &str,
        key: &str,
    ) -> Result<Option<Secret<String>>, CredentialError> {
        if let Some(value) = self
            .plugin_secret_cache
            .read()
            .await
            .get(&(plugin.to_string(), key.to_string()))
        {
            return Ok(Some(Secret::new(value.expose_secret().clone())));
        }
        let Some(raw) = self
            .storage
            .retrieve("lingxi", &plugin_secret_account(plugin, key))
            .await?
        else {
            return Ok(None);
        };
        let s = String::from_utf8(raw.expose_secret_bytes().to_vec())
            .map_err(|_| CredentialError::Unavailable)?;
        Ok(Some(Secret::new(s)))
    }

    /// Delete a sensitive plugin `userConfig` value under `(plugin, key)`.
    /// Idempotent — deleting a missing entry is not an error. Used by the
    /// uninstall path (claude-code `deletePluginOptions` clears the plugin's
    /// keychain `pluginSecrets`).
    pub async fn delete_plugin_secret(
        &self,
        plugin: &str,
        key: &str,
    ) -> Result<(), CredentialError> {
        self.storage
            .delete("lingxi", &plugin_secret_account(plugin, key))
            .await?;
        self.delete_plugin_secret_ephemeral(plugin, key).await;
        Ok(())
    }

    /// Remove one process-local plugin secret without touching durable
    /// storage owned by the Desktop parent.
    pub async fn delete_plugin_secret_ephemeral(&self, plugin: &str, key: &str) {
        self.plugin_secret_cache
            .write()
            .await
            .remove(&(plugin.to_string(), key.to_string()));
    }

    /// Persist a full Anthropic OAuth credential set.
    ///
    /// Writes three secure-storage entries under `service = "lingxi"`:
    /// - `anthropic-oauth-access`  — the access token (`AnthropicOAuthAccessToken`)
    /// - `anthropic-oauth-refresh` — the refresh token (`AnthropicOAuthRefreshToken`),
    ///   deleted if `refresh` is `None`
    /// - `anthropic-oauth-meta`    — JSON session metadata (email / org / expiry / scopes)
    ///
    /// Each `store` overwrites any existing entry, so this is also the rotation
    /// path used by the reactive / proactive refresh driver. The subscription
    /// fields (`subscription_type` / `rate_limit_tier`) are carried over from
    /// the previous session blob — claude-code's `ltu()` merge preserves them
    /// on every save; fresh profile values update them through
    /// [`Self::update_oauth_subscription`] afterwards.
    #[allow(clippy::too_many_arguments)]
    pub async fn store_oauth_tokens(
        &self,
        access: &str,
        refresh: Option<&str>,
        expires_at: SystemTime,
        scopes: Vec<String>,
        email: &str,
        org_id: &str,
    ) -> Result<(), CredentialError> {
        // Held across the whole read → write span: a concurrent
        // `update_oauth_subscription` must not slip its own read in between and
        // then overwrite the rotated `expires_at` with the pre-rotation one.
        let _guard = self.session_meta_lock.lock().await;
        let now = self.clock.now();

        // Read the prior session blob (best-effort) BEFORE the meta entry is
        // overwritten below, to preserve its subscription fields.
        let prior_subscription = self
            .read_oauth_session_meta()
            .await
            .map(|meta| (meta.subscription_type, meta.rate_limit_tier));

        let access_meta = SecureStorageMetadata {
            created_at: now,
            last_accessed: None,
            kind: SecretKind::AnthropicOAuthAccessToken.as_dto(),
        };
        self.storage
            .store(
                OAUTH_SERVICE,
                OAUTH_ACCESS_ACCOUNT,
                SecureStorageData::new(access.as_bytes().to_vec(), access_meta),
            )
            .await?;

        match refresh {
            Some(refresh) => {
                let refresh_meta = SecureStorageMetadata {
                    created_at: now,
                    last_accessed: None,
                    kind: SecretKind::AnthropicOAuthRefreshToken.as_dto(),
                };
                self.storage
                    .store(
                        OAUTH_SERVICE,
                        OAUTH_REFRESH_ACCOUNT,
                        SecureStorageData::new(refresh.as_bytes().to_vec(), refresh_meta),
                    )
                    .await?;
            }
            None => {
                // No refresh token this rotation — clear any stale entry.
                self.storage
                    .delete(OAUTH_SERVICE, OAUTH_REFRESH_ACCOUNT)
                    .await?;
            }
        }

        let (subscription_type, rate_limit_tier) = prior_subscription.unwrap_or((None, None));
        let meta = OAuthSessionMeta {
            expires_at,
            scopes,
            email: email.to_string(),
            org_id: org_id.to_string(),
            subscription_type,
            rate_limit_tier,
        };
        self.write_oauth_session_meta(&meta, now).await
    }

    /// Merge freshly-resolved subscription fields into the persisted session
    /// blob — claude-code `ltu()`:
    /// `subscriptionType: t.subscriptionType ?? e?.subscriptionType ?? null`
    /// (an incoming `None` PRESERVES the stored value; it never clears one).
    /// No-op when no session blob exists (not logged in).
    ///
    /// The merge runs under `session_meta_lock` so it composes with a
    /// concurrent [`Self::store_oauth_tokens`] rotation instead of racing it —
    /// whichever wins the lock, the loser re-reads and preserves the winner's
    /// fields, matching the oracle's single serialized `mutate()` (`Wer`
    /// @228948885 merges tokens AND tier in one callback).
    pub async fn update_oauth_subscription(
        &self,
        subscription_type: Option<&str>,
        rate_limit_tier: Option<&str>,
    ) -> Result<(), CredentialError> {
        let _guard = self.session_meta_lock.lock().await;
        let Some(mut meta) = self.read_oauth_session_meta().await else {
            return Ok(());
        };
        meta.subscription_type = subscription_type
            .map(str::to_string)
            .or(meta.subscription_type);
        meta.rate_limit_tier = rate_limit_tier.map(str::to_string).or(meta.rate_limit_tier);
        self.write_oauth_session_meta(&meta, self.clock.now()).await
    }

    /// Merge profile results only when the OAuth session still belongs to the
    /// account whose token produced the request. The account tuple is captured
    /// before background I/O; an account switch or logout makes this a no-op.
    pub async fn update_oauth_subscription_for_account(
        &self,
        expected_org_id: &str,
        expected_email: &str,
        subscription_type: Option<&str>,
        rate_limit_tier: Option<&str>,
    ) -> Result<bool, CredentialError> {
        let _guard = self.session_meta_lock.lock().await;
        let Some(mut meta) = self.read_oauth_session_meta().await else {
            return Ok(false);
        };
        if meta.org_id != expected_org_id || meta.email != expected_email {
            return Ok(false);
        }
        meta.subscription_type = subscription_type
            .map(str::to_string)
            .or(meta.subscription_type);
        meta.rate_limit_tier = rate_limit_tier.map(str::to_string).or(meta.rate_limit_tier);
        self.write_oauth_session_meta(&meta, self.clock.now())
            .await?;
        Ok(true)
    }

    /// Best-effort read of the persisted session blob (`None` on absence or an
    /// undecodable entry).
    async fn read_oauth_session_meta(&self) -> Option<OAuthSessionMeta> {
        let raw = self
            .storage
            .retrieve(OAUTH_SERVICE, OAUTH_META_ACCOUNT)
            .await
            .ok()??;
        serde_json::from_slice(raw.expose_secret_bytes()).ok()
    }

    /// Serialize + store the session blob under `anthropic-oauth-meta`.
    async fn write_oauth_session_meta(
        &self,
        meta: &OAuthSessionMeta,
        now: SystemTime,
    ) -> Result<(), CredentialError> {
        // Serialization of this fixed-shape struct cannot fail; fall back to an
        // empty object rather than panicking.
        let meta_json = serde_json::to_vec(meta).unwrap_or_else(|_| b"{}".to_vec());
        let meta_meta = SecureStorageMetadata {
            created_at: now,
            last_accessed: None,
            kind: SecretKind::AnthropicOAuthSessionMeta.as_dto(),
        };
        self.storage
            .store(
                OAUTH_SERVICE,
                OAUTH_META_ACCOUNT,
                SecureStorageData::new(meta_json, meta_meta),
            )
            .await?;
        Ok(())
    }

    /// Load the persisted Anthropic OAuth credential set.
    ///
    /// Returns `Ok(None)` if no access token or no session metadata is present
    /// (a partially-written state is treated as "not logged in").
    pub async fn get_oauth_tokens(&self) -> Result<Option<OAuthTokens>, CredentialError> {
        let Some(access_raw) = self
            .storage
            .retrieve(OAUTH_SERVICE, OAUTH_ACCESS_ACCOUNT)
            .await?
        else {
            return Ok(None);
        };
        let Some(meta_raw) = self
            .storage
            .retrieve(OAUTH_SERVICE, OAUTH_META_ACCOUNT)
            .await?
        else {
            return Ok(None);
        };

        let access = String::from_utf8(access_raw.expose_secret_bytes().to_vec())
            .map_err(|_| CredentialError::Unavailable)?;
        let meta: OAuthSessionMeta = serde_json::from_slice(meta_raw.expose_secret_bytes())
            .map_err(|_| CredentialError::Unavailable)?;

        let refresh = match self
            .storage
            .retrieve(OAUTH_SERVICE, OAUTH_REFRESH_ACCOUNT)
            .await?
        {
            Some(raw) => Some(Secret::new(
                String::from_utf8(raw.expose_secret_bytes().to_vec())
                    .map_err(|_| CredentialError::Unavailable)?,
            )),
            None => None,
        };

        Ok(Some(OAuthTokens {
            access_token: Secret::new(access),
            refresh_token: refresh,
            expires_at: meta.expires_at,
            scopes: meta.scopes,
            email: meta.email,
            org_id: meta.org_id,
            subscription_type: meta.subscription_type,
            rate_limit_tier: meta.rate_limit_tier,
        }))
    }

    /// Delete every persisted Anthropic OAuth entry. Idempotent — deleting a
    /// missing entry is not an error.
    pub async fn delete_oauth_tokens(&self) -> Result<(), CredentialError> {
        self.storage
            .delete(OAUTH_SERVICE, OAUTH_ACCESS_ACCOUNT)
            .await?;
        self.storage
            .delete(OAUTH_SERVICE, OAUTH_REFRESH_ACCOUNT)
            .await?;
        self.storage
            .delete(OAUTH_SERVICE, OAUTH_META_ACCOUNT)
            .await?;
        Ok(())
    }

    // ── OpenAI / ChatGPT OAuth ─────────────────────────────────────────────

    /// Persist a full `OpenAI` OAuth credential set.
    ///
    /// Writes three secure-storage entries under `service = "lingxi"`:
    /// - `openai-oauth-access`  — the access token (`OpenAiOAuthAccessToken`)
    /// - `openai-oauth-refresh` — the refresh token (`OpenAiOAuthRefreshToken`),
    ///   deleted if `refresh` is `None`
    /// - `openai-oauth-meta`    — JSON session metadata (`account_id` / fedramp /
    ///   expiry / scopes)
    ///
    /// Each `store` overwrites any existing entry, so this is also the rotation
    /// path used by the reactive / proactive refresh driver.
    #[allow(clippy::too_many_arguments)]
    pub async fn store_openai_oauth_tokens(
        &self,
        access: &str,
        refresh: Option<&str>,
        expires_at: SystemTime,
        scopes: Vec<String>,
        account_id: Option<&str>,
        fedramp: bool,
        email: Option<&str>,
    ) -> Result<(), CredentialError> {
        let now = self.clock.now();

        let access_meta = SecureStorageMetadata {
            created_at: now,
            last_accessed: None,
            kind: SecretKind::OpenAiOAuthAccessToken.as_dto(),
        };
        self.storage
            .store(
                OAUTH_SERVICE,
                OPENAI_OAUTH_ACCESS_ACCOUNT,
                SecureStorageData::new(access.as_bytes().to_vec(), access_meta),
            )
            .await?;

        match refresh {
            Some(refresh) => {
                let refresh_meta = SecureStorageMetadata {
                    created_at: now,
                    last_accessed: None,
                    kind: SecretKind::OpenAiOAuthRefreshToken.as_dto(),
                };
                self.storage
                    .store(
                        OAUTH_SERVICE,
                        OPENAI_OAUTH_REFRESH_ACCOUNT,
                        SecureStorageData::new(refresh.as_bytes().to_vec(), refresh_meta),
                    )
                    .await?;
            }
            None => {
                // No refresh token this rotation — clear any stale entry.
                self.storage
                    .delete(OAUTH_SERVICE, OPENAI_OAUTH_REFRESH_ACCOUNT)
                    .await?;
            }
        }

        let meta = OpenAiOAuthSessionMeta {
            expires_at,
            scopes,
            account_id: account_id.map(str::to_string),
            fedramp,
            email: email.map(str::to_string),
        };
        // Serialization of this fixed-shape struct cannot fail; fall back to an
        // empty object rather than panicking.
        let meta_json = serde_json::to_vec(&meta).unwrap_or_else(|_| b"{}".to_vec());
        let meta_meta = SecureStorageMetadata {
            created_at: now,
            last_accessed: None,
            kind: SecretKind::OpenAiOAuthSessionMeta.as_dto(),
        };
        self.storage
            .store(
                OAUTH_SERVICE,
                OPENAI_OAUTH_META_ACCOUNT,
                SecureStorageData::new(meta_json, meta_meta),
            )
            .await?;
        let observer = self
            .openai_oauth_observer
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(observer) = observer {
            observer
                .updated(OpenAiOAuthTokens {
                    access_token: Secret::new(access.to_string()),
                    refresh_token: refresh.map(|value| Secret::new(value.to_string())),
                    expires_at,
                    scopes: meta.scopes,
                    account_id: account_id.map(str::to_string),
                    fedramp,
                    email: email.map(str::to_string),
                })
                .await;
        }
        Ok(())
    }

    /// Load the persisted `OpenAI` OAuth credential set.
    ///
    /// Returns `Ok(None)` if no access token or no session metadata is present
    /// (a partially-written state is treated as "not logged in").
    pub async fn get_openai_oauth_tokens(
        &self,
    ) -> Result<Option<OpenAiOAuthTokens>, CredentialError> {
        let Some(access_raw) = self
            .storage
            .retrieve(OAUTH_SERVICE, OPENAI_OAUTH_ACCESS_ACCOUNT)
            .await?
        else {
            return Ok(None);
        };
        let Some(meta_raw) = self
            .storage
            .retrieve(OAUTH_SERVICE, OPENAI_OAUTH_META_ACCOUNT)
            .await?
        else {
            return Ok(None);
        };

        let access = String::from_utf8(access_raw.expose_secret_bytes().to_vec())
            .map_err(|_| CredentialError::Unavailable)?;
        let meta: OpenAiOAuthSessionMeta = serde_json::from_slice(meta_raw.expose_secret_bytes())
            .map_err(|_| CredentialError::Unavailable)?;

        let refresh = match self
            .storage
            .retrieve(OAUTH_SERVICE, OPENAI_OAUTH_REFRESH_ACCOUNT)
            .await?
        {
            Some(raw) => Some(Secret::new(
                String::from_utf8(raw.expose_secret_bytes().to_vec())
                    .map_err(|_| CredentialError::Unavailable)?,
            )),
            None => None,
        };

        Ok(Some(OpenAiOAuthTokens {
            access_token: Secret::new(access),
            refresh_token: refresh,
            expires_at: meta.expires_at,
            scopes: meta.scopes,
            account_id: meta.account_id,
            fedramp: meta.fedramp,
            email: meta.email,
        }))
    }

    /// Delete every persisted `OpenAI` OAuth entry. Idempotent — deleting a
    /// missing entry is not an error.
    pub async fn delete_openai_oauth_tokens(&self) -> Result<(), CredentialError> {
        self.storage
            .delete(OAUTH_SERVICE, OPENAI_OAUTH_ACCESS_ACCOUNT)
            .await?;
        self.storage
            .delete(OAUTH_SERVICE, OPENAI_OAUTH_REFRESH_ACCOUNT)
            .await?;
        self.storage
            .delete(OAUTH_SERVICE, OPENAI_OAUTH_META_ACCOUNT)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod oauth_tests {
    use super::*;
    use async_trait::async_trait;
    use lingxi_core::host::SecureStorageBackend;
    use std::collections::HashMap;
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;

    #[test]
    fn credential_preview_reveals_only_the_final_four_characters() {
        assert_eq!(masked_credential_preview("sk-test-secret-abcd"), "••••abcd");
        assert_eq!(
            masked_credential_preview("密钥甲乙丙丁戊己"),
            "••••丙丁戊己"
        );
        assert_eq!(masked_credential_preview("abcd"), "••••");
        assert_eq!(masked_credential_preview("abc"), "••••");
    }

    /// In-memory `(service, account) -> data` store for credential tests.
    #[derive(Default)]
    struct MemStorage {
        map: StdMutex<HashMap<(String, String), SecureStorageData>>,
    }

    #[async_trait]
    impl SecureStorage for MemStorage {
        async fn store(
            &self,
            service: &str,
            account: &str,
            data: SecureStorageData,
        ) -> Result<(), SecureStorageError> {
            self.map
                .lock()
                .unwrap()
                .insert((service.into(), account.into()), data);
            Ok(())
        }
        async fn retrieve(
            &self,
            service: &str,
            account: &str,
        ) -> Result<Option<SecureStorageData>, SecureStorageError> {
            Ok(self
                .map
                .lock()
                .unwrap()
                .get(&(service.into(), account.into()))
                .cloned())
        }
        async fn delete(&self, service: &str, account: &str) -> Result<(), SecureStorageError> {
            self.map
                .lock()
                .unwrap()
                .remove(&(service.into(), account.into()));
            Ok(())
        }
        async fn list(&self, service: &str) -> Result<Vec<String>, SecureStorageError> {
            Ok(self
                .map
                .lock()
                .unwrap()
                .keys()
                .filter(|(s, _)| s == service)
                .map(|(_, a)| a.clone())
                .collect())
        }
        fn is_encrypted(&self) -> bool {
            false
        }
        fn backend(&self) -> SecureStorageBackend {
            SecureStorageBackend::PlainText
        }
    }

    struct FixedClock;
    impl Clock for FixedClock {
        fn now(&self) -> SystemTime {
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_000)
        }
    }

    /// HTTP transport that panics — credential tests never make HTTP calls.
    struct NoHttp;
    #[async_trait]
    impl HttpTransport for NoHttp {
        async fn request(
            &self,
            _req: lingxi_core::types::HttpRequest,
        ) -> Result<lingxi_core::types::HttpResponse, lingxi_core::host::HttpError> {
            panic!("credential tests must not perform HTTP");
        }
        async fn stream_sse(
            &self,
            _req: lingxi_core::types::HttpRequest,
        ) -> Result<lingxi_core::host::http::SseStream, lingxi_core::host::HttpError> {
            panic!("credential tests must not perform HTTP");
        }
    }

    fn account_ref(scope: &str, id: &str) -> lingxi_core::host::provider_accounts::CredentialRef {
        lingxi_core::host::provider_accounts::CredentialRef {
            store_scope: scope.into(),
            secret_ref: format!("acct.{id}"),
        }
    }
    fn account_key(value: &str, revision: u64) -> AccountCredential {
        AccountCredential {
            credential_revision: revision,
            identity_generation: 1,
            kind: AccountCredentialKind::ApiKey {
                key: Secret::new(value.into()),
            },
        }
    }
    #[tokio::test]
    async fn account_credentials_isolate_scope_slot_and_delete() {
        let (_, cm) = manager();
        let a = account_ref("brand_dev", "11111111-1111-4111-8111-111111111111");
        let b = account_ref("brand_dev", "22222222-2222-4222-8222-222222222222");
        let other = account_ref("brand_prod", "11111111-1111-4111-8111-111111111111");
        cm.store_account_credential(&a, &account_key("secret-a", 1))
            .await
            .unwrap();
        cm.store_account_credential(&b, &account_key("secret-b", 1))
            .await
            .unwrap();
        cm.store_account_credential(&other, &account_key("secret-prod", 1))
            .await
            .unwrap();
        cm.delete_account_credential(&a).await.unwrap();
        assert!(cm.get_account_credential(&a).await.unwrap().is_none());
        for (reference, expected) in [(&b, "secret-b"), (&other, "secret-prod")] {
            let record = cm.get_account_credential(reference).await.unwrap().unwrap();
            assert!(!format!("{record:?}").contains(expected));
            let AccountCredentialKind::ApiKey { key } = record.kind else {
                panic!("wrong kind")
            };
            assert_eq!(key.expose_secret(), expected);
        }
    }

    #[tokio::test]
    async fn ephemeral_account_does_not_persist_or_fall_back_to_legacy_provider() {
        let (storage, cm) = manager();
        let reference = account_ref("host_owned", "11111111-1111-4111-8111-111111111111");
        storage
            .store(
                "lingxi",
                "provider-key-openai",
                SecureStorageData::new(
                    b"unselected-key".to_vec(),
                    SecureStorageMetadata {
                        created_at: SystemTime::UNIX_EPOCH,
                        last_accessed: None,
                        kind: SecretKind::GenericApiKey {
                            provider: "openai".into(),
                        }
                        .as_dto(),
                    },
                ),
            )
            .await
            .unwrap();
        cm.set_account_credential_ephemeral(&reference, Some(account_key("host-key", 3)))
            .await
            .unwrap();
        assert!(storage
            .list("provider-accounts.host_owned")
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            cm.get_account_credential(&reference)
                .await
                .unwrap()
                .unwrap()
                .credential_revision,
            3
        );
        cm.set_account_credential_ephemeral(&reference, None)
            .await
            .unwrap();
        assert!(!cm.has_account_credential(&reference).await.unwrap());
        assert!(cm
            .get_account_credential(&reference)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn account_oauth_grants_remain_complete_and_isolated() {
        let (_, cm) = manager();
        let a = account_ref("oauth_test", "11111111-1111-4111-8111-111111111111");
        let b = account_ref("oauth_test", "22222222-2222-4222-8222-222222222222");
        for (reference, suffix) in [(&a, "A"), (&b, "B")] {
            cm.store_account_credential(
                reference,
                &AccountCredential {
                    credential_revision: 3,
                    identity_generation: 1,
                    kind: AccountCredentialKind::OAuth {
                        access_token: Secret::new(format!("access-{suffix}")),
                        refresh_token: Some(Secret::new(format!("refresh-{suffix}"))),
                        expires_at: SystemTime::UNIX_EPOCH + Duration::from_secs(5000),
                        scopes: vec!["openid".into()],
                        issuer: "https://issuer.test".into(),
                        subject: Some(format!("subject-{suffix}")),
                        organization_id: Some("organization".into()),
                        email: None,
                        fedramp: false,
                    },
                },
            )
            .await
            .unwrap();
        }
        cm.delete_account_credential(&a).await.unwrap();
        let record = cm.get_account_credential(&b).await.unwrap().unwrap();
        assert_eq!(record.credential_revision, 3);
        let AccountCredentialKind::OAuth {
            access_token,
            refresh_token,
            subject,
            scopes,
            ..
        } = record.kind
        else {
            panic!("wrong kind")
        };
        assert_eq!(access_token.expose_secret(), "access-B");
        assert_eq!(refresh_token.unwrap().expose_secret(), "refresh-B");
        assert_eq!(subject.as_deref(), Some("subject-B"));
        assert_eq!(scopes, vec!["openid"]);
    }
    #[tokio::test]
    async fn host_managed_missing_account_never_reads_ambient_native_slot() {
        let (_, cm) = manager();
        let reference = account_ref("host", "11111111-1111-4111-8111-111111111111");
        cm.store_account_credential(&reference, &account_key("native-unselected", 1))
            .await
            .unwrap();
        cm.use_host_managed_provider_keys();
        assert!(cm
            .get_account_credential(&reference)
            .await
            .unwrap()
            .is_none());
        assert!(!cm.has_account_credential(&reference).await.unwrap());
        cm.set_account_credential_ephemeral(&reference, Some(account_key("host-selected", 2)))
            .await
            .unwrap();
        assert_eq!(
            cm.get_account_credential(&reference)
                .await
                .unwrap()
                .unwrap()
                .credential_revision,
            2
        );
    }

    fn manager() -> (Arc<MemStorage>, CredentialManager) {
        let storage = Arc::new(MemStorage::default());
        let cm = CredentialManager::new(
            storage.clone() as Arc<dyn SecureStorage>,
            Arc::new(FixedClock),
            Arc::new(NoHttp),
        );
        (storage, cm)
    }

    #[tokio::test]
    async fn openai_oauth_observer_receives_rotated_session() {
        struct Observer(tokio::sync::Mutex<Vec<OpenAiOAuthTokens>>);
        #[async_trait::async_trait]
        impl OpenAiOAuthObserver for Observer {
            async fn updated(&self, tokens: OpenAiOAuthTokens) {
                self.0.lock().await.push(tokens);
            }
        }
        let (_, manager) = manager();
        let observer = Arc::new(Observer(tokio::sync::Mutex::new(vec![])));
        manager.set_openai_oauth_observer(observer.clone());
        let expiry = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000_000);
        manager
            .store_openai_oauth_tokens(
                "access",
                Some("rotated"),
                expiry,
                vec![],
                Some("acct"),
                false,
                Some("acct@example.com"),
            )
            .await
            .unwrap();
        let sessions = observer.0.lock().await;
        assert_eq!(sessions.len(), 1);
        assert_eq!(
            sessions[0].refresh_token.as_ref().unwrap().expose_secret(),
            "rotated"
        );
        assert_eq!(sessions[0].account_id.as_deref(), Some("acct"));
        assert_eq!(sessions[0].email.as_deref(), Some("acct@example.com"));
        assert_eq!(
            manager
                .get_openai_oauth_tokens()
                .await
                .unwrap()
                .unwrap()
                .expires_at,
            expiry
        );
    }

    #[tokio::test]
    async fn oauth_tokens_round_trip() {
        let (_storage, cm) = manager();
        let expires = SystemTime::UNIX_EPOCH + Duration::from_secs(5_000);
        cm.store_oauth_tokens(
            "access-abc",
            Some("refresh-xyz"),
            expires,
            vec!["read:user".into(), "write:messages".into()],
            "user@example.com",
            "org-uuid-1",
        )
        .await
        .expect("store");

        let got = cm.get_oauth_tokens().await.expect("get").expect("present");
        assert_eq!(got.access_token.expose_secret(), "access-abc");
        assert_eq!(
            got.refresh_token
                .as_ref()
                .map(|s| s.expose_secret().clone()),
            Some("refresh-xyz".to_string())
        );
        assert_eq!(got.expires_at, expires);
        assert_eq!(got.scopes, vec!["read:user", "write:messages"]);
        assert_eq!(got.email, "user@example.com");
        assert_eq!(got.org_id, "org-uuid-1");
    }

    #[tokio::test]
    async fn get_returns_none_when_absent() {
        let (_storage, cm) = manager();
        assert!(cm.get_oauth_tokens().await.expect("get").is_none());
    }

    /// M13: subscription fields round-trip, survive token rotation (claude-code
    /// `ltu()` carries `subscriptionType`/`rateLimitTier` on every save), and
    /// `update_oauth_subscription`'s `new ?? old` merge never clears a stored
    /// value with `None`.
    #[tokio::test]
    async fn subscription_fields_persist_and_merge() {
        let (_storage, cm) = manager();
        let expires = SystemTime::UNIX_EPOCH + Duration::from_secs(5_000);
        cm.store_oauth_tokens("a1", Some("r1"), expires, vec![], "e@x", "o")
            .await
            .expect("store");
        // Fresh blob: no tier known.
        let got = cm.get_oauth_tokens().await.expect("get").expect("present");
        assert!(got.subscription_type.is_none());
        assert!(got.rate_limit_tier.is_none());

        // Login-time profile resolution writes the tier.
        cm.update_oauth_subscription(Some("max"), Some("default_claude_max_20x"))
            .await
            .expect("update");
        let got = cm.get_oauth_tokens().await.expect("get").expect("present");
        assert_eq!(got.subscription_type.as_deref(), Some("max"));
        assert_eq!(
            got.rate_limit_tier.as_deref(),
            Some("default_claude_max_20x")
        );

        // Refresh-driver rotation (store with no tier inputs) PRESERVES it.
        cm.store_oauth_tokens("a2", None, expires, vec![], "e@x", "o")
            .await
            .expect("rotate");
        let got = cm.get_oauth_tokens().await.expect("get").expect("present");
        assert_eq!(got.access_token.expose_secret(), "a2");
        assert_eq!(got.subscription_type.as_deref(), Some("max"));
        assert_eq!(
            got.rate_limit_tier.as_deref(),
            Some("default_claude_max_20x")
        );

        // Partial update: `None` keeps the old value, `Some` replaces.
        cm.update_oauth_subscription(Some("enterprise"), None)
            .await
            .expect("partial update");
        let got = cm.get_oauth_tokens().await.expect("get").expect("present");
        assert_eq!(got.subscription_type.as_deref(), Some("enterprise"));
        assert_eq!(
            got.rate_limit_tier.as_deref(),
            Some("default_claude_max_20x")
        );

        // Logout clears the blob → a later update is a no-op (not an error).
        cm.delete_oauth_tokens().await.expect("delete");
        cm.update_oauth_subscription(Some("pro"), None)
            .await
            .expect("no-op update after logout");
        assert!(cm.get_oauth_tokens().await.expect("get").is_none());
    }

    #[tokio::test]
    async fn subscription_refresh_is_bound_to_the_captured_account() {
        let (_storage, cm) = manager();
        let expires = SystemTime::UNIX_EPOCH + Duration::from_secs(5_000);
        cm.store_oauth_tokens("a1", Some("r1"), expires, vec![], "old@x", "org-old")
            .await
            .expect("store old account");
        cm.update_oauth_subscription(Some("max"), Some("old-tier"))
            .await
            .expect("seed old subscription");

        // A concurrent old-account profile refresh cannot update the new
        // credential metadata after login has persisted a different account.
        cm.store_oauth_tokens("a2", Some("r2"), expires, vec![], "new@x", "org-new")
            .await
            .expect("store new account");
        assert!(!cm
            .update_oauth_subscription_for_account(
                "org-old",
                "old@x",
                Some("enterprise"),
                Some("stale-tier"),
            )
            .await
            .expect("stale update is rejected"));

        let current = cm.get_oauth_tokens().await.expect("read").expect("present");
        assert_eq!(current.org_id, "org-new");
        assert_eq!(current.email, "new@x");
        assert_eq!(current.subscription_type.as_deref(), Some("max"));
        assert_eq!(current.rate_limit_tier.as_deref(), Some("old-tier"));
    }

    /// Storage double that inserts an await point at every operation, so
    /// `tokio::join!` on the current-thread runtime interleaves two credential
    /// writers exactly at the read → write boundary the lock has to close.
    struct YieldingStorage(Arc<MemStorage>);

    #[async_trait]
    impl SecureStorage for YieldingStorage {
        async fn store(
            &self,
            service: &str,
            account: &str,
            data: SecureStorageData,
        ) -> Result<(), SecureStorageError> {
            tokio::task::yield_now().await;
            self.0.store(service, account, data).await
        }
        async fn retrieve(
            &self,
            service: &str,
            account: &str,
        ) -> Result<Option<SecureStorageData>, SecureStorageError> {
            tokio::task::yield_now().await;
            self.0.retrieve(service, account).await
        }
        async fn delete(&self, service: &str, account: &str) -> Result<(), SecureStorageError> {
            tokio::task::yield_now().await;
            self.0.delete(service, account).await
        }
        async fn list(&self, service: &str) -> Result<Vec<String>, SecureStorageError> {
            self.0.list(service).await
        }
        fn is_encrypted(&self) -> bool {
            false
        }
        fn backend(&self) -> SecureStorageBackend {
            SecureStorageBackend::PlainText
        }
    }

    /// The desktop freshener's tier merge and the refresh driver's rotation
    /// both read-modify-write the session blob on one shared manager. Neither
    /// may drop the other's fields: claude-code applies both in a single
    /// `mutate()` callback (`Wer` @228948885) serialized by `Hcg` @228065879.
    #[tokio::test]
    async fn concurrent_tier_merge_and_rotation_do_not_lose_each_other() {
        let cm = CredentialManager::new(
            Arc::new(YieldingStorage(Arc::new(MemStorage::default()))) as Arc<dyn SecureStorage>,
            Arc::new(FixedClock),
            Arc::new(NoHttp),
        );
        let stale = SystemTime::UNIX_EPOCH + Duration::from_secs(5_000);
        let rotated = SystemTime::UNIX_EPOCH + Duration::from_secs(9_000);
        cm.store_oauth_tokens("a1", Some("r1"), stale, vec![], "e@x", "o")
            .await
            .expect("seed");

        let (merged, rotate) = tokio::join!(
            cm.update_oauth_subscription(Some("max"), Some("default_claude_max_20x")),
            cm.store_oauth_tokens("a2", Some("r2"), rotated, vec![], "e@x", "o"),
        );
        merged.expect("merge");
        rotate.expect("rotate");

        let got = cm.get_oauth_tokens().await.expect("get").expect("present");
        assert_eq!(got.access_token.expose_secret(), "a2");
        assert_eq!(
            got.expires_at, rotated,
            "the merge must not write the pre-rotation expiry back"
        );
        assert_eq!(got.subscription_type.as_deref(), Some("max"));
        assert_eq!(
            got.rate_limit_tier.as_deref(),
            Some("default_claude_max_20x")
        );
    }

    /// M13 back-compat: a pre-existing meta blob WITHOUT the subscription keys
    /// still deserializes (serde defaults).
    #[tokio::test]
    async fn legacy_meta_blob_without_subscription_keys_still_loads() {
        let (storage, cm) = manager();
        let expires = SystemTime::UNIX_EPOCH + Duration::from_secs(5_000);
        cm.store_oauth_tokens("a", None, expires, vec![], "e@x", "o")
            .await
            .expect("store");
        // Rewrite the meta entry with the LEGACY shape (no subscription keys).
        let legacy = serde_json::json!({
            "expires_at": {"secs_since_epoch": 5_000, "nanos_since_epoch": 0},
            "scopes": ["user:inference"],
            "email": "legacy@x",
            "org_id": "org-legacy",
        });
        storage
            .store(
                "lingxi",
                "anthropic-oauth-meta",
                SecureStorageData::new(
                    serde_json::to_vec(&legacy).unwrap(),
                    SecureStorageMetadata {
                        created_at: SystemTime::UNIX_EPOCH,
                        last_accessed: None,
                        kind: SecretKind::AnthropicOAuthSessionMeta.as_dto(),
                    },
                ),
            )
            .await
            .expect("seed legacy meta");
        let got = cm.get_oauth_tokens().await.expect("get").expect("present");
        assert_eq!(got.email, "legacy@x");
        assert!(got.subscription_type.is_none());
        assert!(got.rate_limit_tier.is_none());
    }

    #[tokio::test]
    async fn store_without_refresh_clears_stale_refresh() {
        let (_storage, cm) = manager();
        let expires = SystemTime::UNIX_EPOCH + Duration::from_secs(5_000);
        // First write a refresh token.
        cm.store_oauth_tokens("a1", Some("r1"), expires, vec![], "e@x", "o")
            .await
            .expect("store with refresh");
        // Rotate to a token set with no refresh token.
        cm.store_oauth_tokens("a2", None, expires, vec![], "e@x", "o")
            .await
            .expect("store without refresh");
        let got = cm.get_oauth_tokens().await.expect("get").expect("present");
        assert_eq!(got.access_token.expose_secret(), "a2");
        assert!(got.refresh_token.is_none(), "stale refresh must be cleared");
    }

    #[tokio::test]
    async fn delete_is_idempotent_and_clears() {
        let (_storage, cm) = manager();
        let expires = SystemTime::UNIX_EPOCH + Duration::from_secs(5_000);
        cm.store_oauth_tokens("a", Some("r"), expires, vec![], "e@x", "o")
            .await
            .expect("store");
        cm.delete_oauth_tokens().await.expect("delete");
        assert!(cm.get_oauth_tokens().await.expect("get").is_none());
        // Second delete on an empty store is not an error.
        cm.delete_oauth_tokens().await.expect("idempotent delete");
    }

    // ── Plugin userConfig secret round-trips ────────────────────────────────

    #[tokio::test]
    async fn plugin_secret_round_trips() {
        let (_storage, cm) = manager();
        cm.set_plugin_secret("weather@acme", "API_KEY", "sk-secret")
            .await
            .expect("set");
        let got = cm
            .get_plugin_secret("weather@acme", "API_KEY")
            .await
            .expect("get")
            .expect("present");
        assert_eq!(got.expose_secret(), "sk-secret");
    }

    #[tokio::test]
    async fn plugin_secret_absent_is_none() {
        let (_storage, cm) = manager();
        assert!(cm
            .get_plugin_secret("weather@acme", "API_KEY")
            .await
            .expect("get")
            .is_none());
    }

    #[tokio::test]
    async fn ephemeral_plugin_secret_overrides_storage_without_persisting() {
        let (storage, cm) = manager();
        cm.set_plugin_secret("weather@acme", "API_KEY", "stored")
            .await
            .expect("set stored");
        cm.set_plugin_secret_ephemeral("weather@acme", "API_KEY", "desktop")
            .await;

        let resolved = cm
            .get_plugin_secret("weather@acme", "API_KEY")
            .await
            .expect("resolve")
            .expect("present");
        assert_eq!(resolved.expose_secret(), "desktop");
        let persisted = storage
            .retrieve("lingxi", "plugin-secret-weather@acme/API_KEY")
            .await
            .expect("retrieve")
            .expect("persisted");
        assert_eq!(persisted.expose_secret_bytes(), b"stored");

        cm.delete_plugin_secret_ephemeral("weather@acme", "API_KEY")
            .await;
        let fallback = cm
            .get_plugin_secret("weather@acme", "API_KEY")
            .await
            .expect("resolve fallback")
            .expect("stored fallback");
        assert_eq!(fallback.expose_secret(), "stored");
    }

    #[tokio::test]
    async fn persisted_plugin_secret_replaces_ephemeral_override() {
        let (_storage, cm) = manager();
        cm.set_plugin_secret_ephemeral("weather@acme", "API_KEY", "desktop")
            .await;
        cm.set_plugin_secret("weather@acme", "API_KEY", "replacement")
            .await
            .expect("persist replacement");
        let resolved = cm
            .get_plugin_secret("weather@acme", "API_KEY")
            .await
            .expect("resolve")
            .expect("present");
        assert_eq!(resolved.expose_secret(), "replacement");
    }

    #[tokio::test]
    async fn plugin_secret_delete_is_idempotent() {
        let (_storage, cm) = manager();
        cm.set_plugin_secret("p", "K", "v").await.expect("set");
        cm.delete_plugin_secret("p", "K").await.expect("delete");
        assert!(cm.get_plugin_secret("p", "K").await.expect("get").is_none());
        // Second delete on an empty slot is not an error.
        cm.delete_plugin_secret("p", "K")
            .await
            .expect("idempotent delete");
    }

    #[tokio::test]
    async fn plugin_secrets_isolated_by_plugin_and_key() {
        let (_storage, cm) = manager();
        cm.set_plugin_secret("p1", "K", "a").await.expect("set a");
        cm.set_plugin_secret("p2", "K", "b").await.expect("set b");
        cm.set_plugin_secret("p1", "K2", "c").await.expect("set c");
        assert_eq!(
            cm.get_plugin_secret("p1", "K")
                .await
                .unwrap()
                .unwrap()
                .expose_secret(),
            "a"
        );
        assert_eq!(
            cm.get_plugin_secret("p2", "K")
                .await
                .unwrap()
                .unwrap()
                .expose_secret(),
            "b"
        );
        assert_eq!(
            cm.get_plugin_secret("p1", "K2")
                .await
                .unwrap()
                .unwrap()
                .expose_secret(),
            "c"
        );
    }

    #[tokio::test]
    async fn plugin_secret_persisted_under_lingxi_service_with_plugin_kind() {
        let (storage, cm) = manager();
        cm.set_plugin_secret("weather@acme", "API_KEY", "sk-x")
            .await
            .expect("set");
        let raw = storage
            .retrieve("lingxi", "plugin-secret-weather@acme/API_KEY")
            .await
            .expect("retrieve")
            .expect("present");
        assert_eq!(raw.expose_secret_bytes(), b"sk-x");
        let expected_kind = SecretKind::PluginSecret {
            plugin: "weather@acme".to_string(),
            key: "API_KEY".to_string(),
        }
        .as_dto();
        assert_eq!(raw.metadata.kind, expected_kind);
    }

    // ── OpenAI OAuth storage tests ───────────────────────────────────────────

    #[tokio::test]
    async fn openai_oauth_tokens_round_trip() {
        let (_storage, cm) = manager();
        let expires = SystemTime::UNIX_EPOCH + Duration::from_secs(5_000);
        cm.store_openai_oauth_tokens(
            "acc",
            Some("ref"),
            expires,
            vec!["openid".to_string()],
            Some("acc_1"),
            false,
            Some("acc_1@example.com"),
        )
        .await
        .expect("store");
        let got = cm
            .get_openai_oauth_tokens()
            .await
            .expect("get")
            .expect("present");
        assert_eq!(got.access_token.expose_secret(), "acc");
        assert_eq!(
            got.refresh_token
                .as_ref()
                .map(|s| s.expose_secret().clone()),
            Some("ref".to_string())
        );
        assert_eq!(got.expires_at, expires);
        assert_eq!(got.scopes, vec!["openid"]);
        assert_eq!(got.account_id.as_deref(), Some("acc_1"));
        assert!(!got.fedramp);
        assert_eq!(got.email.as_deref(), Some("acc_1@example.com"));
    }

    /// Sessions persisted before email was captured have no `email` key in the
    /// `openai-oauth-meta` blob; they must still load (with `email: None`)
    /// rather than failing as `Unavailable`.
    #[tokio::test]
    async fn openai_oauth_tokens_load_from_legacy_meta_without_email() {
        let (storage, cm) = manager();
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let meta = |kind: SecretKind| SecureStorageMetadata {
            created_at: now,
            last_accessed: None,
            kind: kind.as_dto(),
        };
        storage
            .store(
                OAUTH_SERVICE,
                OPENAI_OAUTH_ACCESS_ACCOUNT,
                SecureStorageData::new(
                    b"legacy-access".to_vec(),
                    meta(SecretKind::OpenAiOAuthAccessToken),
                ),
            )
            .await
            .expect("store legacy access");
        storage
            .store(
                OAUTH_SERVICE,
                OPENAI_OAUTH_META_ACCOUNT,
                SecureStorageData::new(
                    br#"{"expires_at":{"secs_since_epoch":5000,"nanos_since_epoch":0},"scopes":[],"account_id":"legacy","fedramp":false}"#.to_vec(),
                    meta(SecretKind::OpenAiOAuthSessionMeta),
                ),
            )
            .await
            .expect("store legacy meta");

        let got = cm
            .get_openai_oauth_tokens()
            .await
            .expect("get")
            .expect("legacy session must still load");
        assert_eq!(got.access_token.expose_secret(), "legacy-access");
        assert_eq!(got.account_id.as_deref(), Some("legacy"));
        assert!(got.email.is_none());
    }

    #[tokio::test]
    async fn openai_oauth_tokens_round_trip_fedramp() {
        let (_storage, cm) = manager();
        let expires = SystemTime::UNIX_EPOCH + Duration::from_secs(5_000);
        cm.store_openai_oauth_tokens("acc2", None, expires, vec![], None, true, None)
            .await
            .expect("store");
        let got = cm
            .get_openai_oauth_tokens()
            .await
            .expect("get")
            .expect("present");
        assert_eq!(got.access_token.expose_secret(), "acc2");
        assert!(got.refresh_token.is_none());
        assert!(got.account_id.is_none());
        assert!(got.fedramp);
    }

    #[tokio::test]
    async fn openai_oauth_tokens_delete() {
        let (_storage, cm) = manager();
        let expires = SystemTime::UNIX_EPOCH + Duration::from_secs(5_000);
        cm.store_openai_oauth_tokens("a", Some("r"), expires, vec![], Some("x"), false, None)
            .await
            .expect("store");
        cm.delete_openai_oauth_tokens().await.expect("delete");
        assert!(cm.get_openai_oauth_tokens().await.expect("get").is_none());
        // Second delete on an empty store is not an error.
        cm.delete_openai_oauth_tokens()
            .await
            .expect("idempotent delete");
    }

    #[tokio::test]
    async fn openai_oauth_get_returns_none_when_absent() {
        let (_storage, cm) = manager();
        assert!(cm.get_openai_oauth_tokens().await.expect("get").is_none());
    }

    #[tokio::test]
    async fn openai_oauth_store_without_refresh_clears_stale_refresh() {
        let (_storage, cm) = manager();
        let expires = SystemTime::UNIX_EPOCH + Duration::from_secs(5_000);
        // First write a refresh token.
        cm.store_openai_oauth_tokens("a1", Some("r1"), expires, vec![], Some("id1"), false, None)
            .await
            .expect("store with refresh");
        // Rotate to a token set with no refresh token.
        cm.store_openai_oauth_tokens("a2", None, expires, vec![], Some("id1"), false, None)
            .await
            .expect("store without refresh");
        let got = cm
            .get_openai_oauth_tokens()
            .await
            .expect("get")
            .expect("present");
        assert_eq!(got.access_token.expose_secret(), "a2");
        assert!(got.refresh_token.is_none(), "stale refresh must be cleared");
    }

    #[tokio::test]
    async fn anthropic_and_openai_oauth_are_isolated() {
        // Storing OpenAI tokens must not affect Anthropic slots and vice-versa.
        let (_storage, cm) = manager();
        let expires = SystemTime::UNIX_EPOCH + Duration::from_secs(5_000);
        cm.store_oauth_tokens("ant-acc", Some("ant-ref"), expires, vec![], "e@x", "o")
            .await
            .expect("store anthropic");
        cm.store_openai_oauth_tokens(
            "oai-acc",
            Some("oai-ref"),
            expires,
            vec![],
            None,
            false,
            None,
        )
        .await
        .expect("store openai");

        let ant = cm
            .get_oauth_tokens()
            .await
            .expect("get ant")
            .expect("present");
        assert_eq!(ant.access_token.expose_secret(), "ant-acc");

        let oai = cm
            .get_openai_oauth_tokens()
            .await
            .expect("get oai")
            .expect("present");
        assert_eq!(oai.access_token.expose_secret(), "oai-acc");

        // Delete OpenAI — Anthropic survives.
        cm.delete_openai_oauth_tokens()
            .await
            .expect("delete openai");
        assert!(cm.get_openai_oauth_tokens().await.expect("get").is_none());
        assert!(cm.get_oauth_tokens().await.expect("get").is_some());
    }
}

fn account_cache_key(reference: &lingxi_core::host::provider_accounts::CredentialRef) -> String {
    format!("{}:{}", reference.store_scope, reference.secret_ref)
}
fn account_slot(
    reference: &lingxi_core::host::provider_accounts::CredentialRef,
) -> Result<String, CredentialError> {
    let valid = reference
        .secret_ref
        .strip_prefix("acct.")
        .is_some_and(|id| id.len() == 36 && id.bytes().all(|c| c.is_ascii_hexdigit() || c == b'-'));
    if !valid
        || reference.store_scope.is_empty()
        || reference.store_scope.len() > 128
        || !reference
            .store_scope
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b'_'))
    {
        return Err(CredentialError::Unavailable);
    }
    Ok(format!("provider-accounts.{}", reference.store_scope))
}

fn clone_account_credential(
    value: &AccountCredential,
) -> Result<AccountCredential, CredentialError> {
    // Explicit serialization is confined to the in-memory host credential boundary.
    let bytes = Secret::new(serde_json::to_vec(value).map_err(|_| CredentialError::Unavailable)?);
    serde_json::from_slice(bytes.expose_secret()).map_err(|_| CredentialError::Unavailable)
}
