//! Per-profile availability: drives the `/model` picker's Connect badge.
//!
//! `available = keychain_has(id) || env_set(var) || anthropic key/oauth present`.
//! A sibling list keyed by `profile_name` (spec §8) so the frozen `ModelListing`
//! DTO stays untouched; the tui joins it by provider/profile name.

use std::sync::Arc;

use llm_runtime::ProviderId;

use crate::CredentialSource;

/// Availability of one provider profile for the picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderAvailability {
    /// Provider identity.
    pub provider_id: ProviderId,
    /// Human profile name (the engine map keys by this; spec §6.2).
    pub profile_name: String,
    /// Credential id this availability was computed for.
    pub credential_id: String,
    /// Whether a usable credential is present.
    pub available: bool,
}

/// Compute availability for every `CredentialSource`.
///
/// `anthropic_has_api_key` / `anthropic_has_oauth` reflect the engine's resolved
/// Anthropic auth state (the composite serves those without a keychain/env id).
/// `openai_chatgpt_available` mirrors that pattern for the ChatGPT credential —
/// the engine ORs PAT-env / external-tokens-env / OAuth-session and passes the
/// combined result here (OpenAI OAuth tokens are stored under `openai-oauth-*`
/// keychain accounts, not under a `get_provider_key("openai-chatgpt")` slot, so
/// the generic arm cannot detect any of the three sources).
/// `isolated` suppresses AMBIENT credential sources — the process environment
/// — leaving only what this boot was explicitly given.
///
/// A preset counts as available when its env var merely EXISTS, so any
/// `DEEPSEEK_API_KEY`/`OPENAI_API_KEY`/… in the developer's shell silently makes
/// a provider "connected". That is correct for a real session and wrong for a
/// sandboxed one: it makes credential-dependent behaviour answer differently on
/// a machine that happens to export a key, which is a test-isolation hazard
/// rather than a preference.
pub async fn compute_availability_with_isolation(
    credentials: &Arc<secret::CredentialManager>,
    sources: &[CredentialSource],
    anthropic_has_api_key: bool,
    anthropic_has_oauth: bool,
    openai_chatgpt_available: bool,
    isolated: bool,
) -> Vec<ProviderAvailability> {
    let mut out = Vec::with_capacity(sources.len());
    for source in sources {
        let available = match source.credential_id.as_str() {
            // Host-managed API keys do not own either OAuth session. In
            // particular, an empty API-key override map on a cold sidecar
            // must not hide a host's live ChatGPT/Anthropic OAuth delegate.
            "anthropic-oauth" => anthropic_has_oauth,
            "openai-chatgpt" => openai_chatgpt_available,
            id if credentials.provider_key_is_suppressed(id).await => false,
            "anthropic-api-key" => anthropic_has_api_key,
            id => {
                // Availability needs presence, not secret bytes. On macOS the
                // attribute-only query avoids decrypting every provider key
                // (and therefore avoids one ACL prompt per saved item) while a
                // session still loads its selected provider below during boot.
                let keychain_has = credentials.has_provider_key(id).await.unwrap_or(false);
                let env_set = !isolated
                    && source
                        .env_var
                        .as_deref()
                        .is_some_and(|var| std::env::var(var).is_ok());
                keychain_has || env_set
            }
        };
        out.push(ProviderAvailability {
            provider_id: source.provider_id.clone(),
            profile_name: source.profile_name.clone(),
            credential_id: source.credential_id.clone(),
            available,
        });
    }
    out
}

/// [`compute_availability_with_isolation`] with ambient sources ENABLED — the
/// production behaviour.
pub async fn compute_availability(
    credentials: &Arc<secret::CredentialManager>,
    sources: &[CredentialSource],
    anthropic_has_api_key: bool,
    anthropic_has_oauth: bool,
    openai_chatgpt_available: bool,
) -> Vec<ProviderAvailability> {
    compute_availability_with_isolation(
        credentials,
        sources,
        anthropic_has_api_key,
        anthropic_has_oauth,
        openai_chatgpt_available,
        false,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_runtime::ProviderId;
    use std::sync::Arc;

    #[derive(Default)]
    struct MemStorage {
        map: std::sync::Mutex<
            std::collections::HashMap<(String, String), lingxi_core::types::SecureStorageData>,
        >,
        retrieves: std::sync::atomic::AtomicUsize,
        contains: std::sync::atomic::AtomicUsize,
    }
    #[async_trait::async_trait]
    impl lingxi_core::host::SecureStorage for MemStorage {
        async fn store(
            &self,
            service: &str,
            account: &str,
            data: lingxi_core::types::SecureStorageData,
        ) -> Result<(), lingxi_core::host::SecureStorageError> {
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
        ) -> Result<
            Option<lingxi_core::types::SecureStorageData>,
            lingxi_core::host::SecureStorageError,
        > {
            self.retrieves
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(self
                .map
                .lock()
                .unwrap()
                .get(&(service.into(), account.into()))
                .cloned())
        }
        async fn contains(
            &self,
            service: &str,
            account: &str,
        ) -> Result<bool, lingxi_core::host::SecureStorageError> {
            self.contains
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(self
                .map
                .lock()
                .unwrap()
                .contains_key(&(service.into(), account.into())))
        }
        async fn delete(
            &self,
            service: &str,
            account: &str,
        ) -> Result<(), lingxi_core::host::SecureStorageError> {
            self.map
                .lock()
                .unwrap()
                .remove(&(service.into(), account.into()));
            Ok(())
        }
        async fn list(
            &self,
            service: &str,
        ) -> Result<Vec<String>, lingxi_core::host::SecureStorageError> {
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
        fn backend(&self) -> lingxi_core::host::SecureStorageBackend {
            lingxi_core::host::SecureStorageBackend::PlainText
        }
    }
    struct FixedClock;
    impl lingxi_core::host::Clock for FixedClock {
        fn now(&self) -> std::time::SystemTime {
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000)
        }
    }
    struct NoHttp;
    #[async_trait::async_trait]
    impl lingxi_core::host::HttpTransport for NoHttp {
        async fn request(
            &self,
            _req: lingxi_core::types::HttpRequest,
        ) -> Result<lingxi_core::types::HttpResponse, lingxi_core::host::HttpError> {
            panic!("availability tests must not perform HTTP");
        }
        async fn stream_sse(
            &self,
            _req: lingxi_core::types::HttpRequest,
        ) -> Result<lingxi_core::host::http::SseStream, lingxi_core::host::HttpError> {
            panic!("availability tests must not perform HTTP");
        }
    }
    fn manager() -> Arc<secret::CredentialManager> {
        Arc::new(secret::CredentialManager::new(
            Arc::new(MemStorage::default()),
            Arc::new(FixedClock),
            Arc::new(NoHttp),
        ))
    }
    fn src(name: &str, env: Option<&str>) -> crate::CredentialSource {
        crate::CredentialSource {
            provider_id: ProviderId::OpenAICompatible {
                name: name.to_string(),
            },
            profile_name: name.to_string(),
            credential_id: name.to_string(),
            env_var: env.map(str::to_string),
            kind: crate::CredentialKind::ApiKey,
        }
    }

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[tokio::test]
    async fn host_managed_api_keys_do_not_suppress_oauth_availability() {
        let credentials = manager();
        credentials.use_host_managed_provider_keys();
        let sources = [
            src("anthropic-api-key", None),
            src("deepseek", None),
            src("anthropic-oauth", None),
            src("openai-chatgpt", None),
        ];
        let rows = compute_availability(&credentials, &sources, true, true, true).await;
        let actual: Vec<bool> = rows.iter().map(|row| row.available).collect();
        assert_eq!(actual, [false, false, true, true]);

        credentials
            .set_provider_key_ephemeral("anthropic", "key")
            .await;
        let rows = compute_availability(&credentials, &sources, true, true, true).await;
        assert!(rows[0].available);
        credentials
            .suppress_provider_key_ephemeral("anthropic")
            .await;
        let rows = compute_availability(&credentials, &sources, true, true, true).await;
        assert!(!rows[0].available);
        assert!(rows[2].available && rows[3].available);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn host_deletion_suppresses_stale_flags_and_environment_availability() {
        let _guard = ENV_LOCK.lock().unwrap();
        let variable = "HOST_DELETED_AVAILABILITY_REGRESSION_KEY";
        let previous = std::env::var_os(variable);
        std::env::set_var(variable, "environment-key");
        let credentials = manager();
        credentials
            .suppress_provider_key_ephemeral("anthropic")
            .await;
        credentials
            .suppress_provider_key_ephemeral("host-test")
            .await;
        let sources = [
            src("anthropic-api-key", None),
            src("host-test", Some(variable)),
        ];
        let unavailable = compute_availability(&credentials, &sources, true, false, false).await;
        assert!(unavailable.iter().all(|entry| !entry.available));

        credentials
            .set_provider_key_ephemeral("anthropic", "replacement")
            .await;
        credentials
            .set_provider_key_ephemeral("host-test", "replacement")
            .await;
        let available = compute_availability(&credentials, &sources, true, false, false).await;
        assert!(available.iter().all(|entry| entry.available));
        match previous {
            Some(value) => std::env::set_var(variable, value),
            None => std::env::remove_var(variable),
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // serialize env-var mutation across async tests
    async fn keychain_makes_available() {
        let _g = ENV_LOCK.lock().unwrap();
        let storage = Arc::new(MemStorage::default());
        let cm = Arc::new(secret::CredentialManager::new(
            storage.clone(),
            Arc::new(FixedClock),
            Arc::new(NoHttp),
        ));
        cm.set_provider_key("openrouter", "k").await.expect("store");
        let map = compute_availability(
            &cm,
            &[src("openrouter", Some("NOPE_VAR"))],
            false,
            false,
            false,
        )
        .await;
        let entry = map
            .iter()
            .find(|a| a.profile_name == "openrouter")
            .expect("entry");
        assert!(entry.available);
        assert_eq!(
            storage.retrieves.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "availability must not decrypt provider credentials"
        );
        assert_eq!(
            storage.contains.load(std::sync::atomic::Ordering::Relaxed),
            1
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // serialize env-var mutation across async tests
    async fn env_makes_available() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("DEEPSEEK_AVAIL_VAR", "k");
        let map = compute_availability(
            &manager(),
            &[src("deepseek", Some("DEEPSEEK_AVAIL_VAR"))],
            false,
            false,
            false,
        )
        .await;
        std::env::remove_var("DEEPSEEK_AVAIL_VAR");
        assert!(
            map.iter()
                .find(|a| a.profile_name == "deepseek")
                .unwrap()
                .available
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // serialize env-var mutation across async tests
    async fn neither_is_unavailable() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("GLM_AVAIL_VAR");
        let map = compute_availability(
            &manager(),
            &[src("glm", Some("GLM_AVAIL_VAR"))],
            false,
            false,
            false,
        )
        .await;
        assert!(
            !map.iter()
                .find(|a| a.profile_name == "glm")
                .unwrap()
                .available
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // serialize env-var mutation across async tests
    async fn anthropic_api_key_marks_anthropic_available() {
        let _g = ENV_LOCK.lock().unwrap();
        let anthropic = crate::CredentialSource {
            provider_id: ProviderId::AnthropicFirstParty,
            profile_name: "anthropic".to_string(),
            credential_id: "anthropic-api-key".to_string(),
            env_var: None,
            kind: crate::CredentialKind::ApiKey,
        };
        let map = compute_availability(&manager(), &[anthropic], true, false, false).await;
        assert!(
            map.iter()
                .find(|a| a.profile_name == "anthropic")
                .unwrap()
                .available
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // serialize env-var mutation across async tests
    async fn anthropic_oauth_marks_anthropic_available() {
        let _g = ENV_LOCK.lock().unwrap();
        let anthropic = crate::CredentialSource {
            provider_id: ProviderId::AnthropicFirstParty,
            profile_name: "anthropic".to_string(),
            credential_id: "anthropic-oauth".to_string(),
            env_var: None,
            kind: crate::CredentialKind::OAuth,
        };
        let map = compute_availability(&manager(), &[anthropic], false, true, false).await;
        assert!(
            map.iter()
                .find(|a| a.profile_name == "anthropic")
                .unwrap()
                .available
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // serialize env-var mutation across async tests
    async fn openai_chatgpt_available_when_oauth_session_present() {
        let _g = ENV_LOCK.lock().unwrap();
        let chatgpt = crate::CredentialSource {
            provider_id: ProviderId::OpenAICompatible {
                name: "openai-chatgpt".to_string(),
            },
            profile_name: "openai-chatgpt".to_string(),
            credential_id: "openai-chatgpt".to_string(),
            env_var: None,
            kind: crate::CredentialKind::ApiKey,
        };
        // not available with the flag false …
        let map = compute_availability(
            &manager(),
            std::slice::from_ref(&chatgpt),
            false,
            false,
            false,
        )
        .await;
        assert!(
            !map.iter()
                .find(|a| a.profile_name == "openai-chatgpt")
                .unwrap()
                .available
        );
        // … available with the flag true (OAuth session present)
        let map = compute_availability(&manager(), &[chatgpt], false, false, true).await;
        assert!(
            map.iter()
                .find(|a| a.profile_name == "openai-chatgpt")
                .unwrap()
                .available
        );
    }
}
