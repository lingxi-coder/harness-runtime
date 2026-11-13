//! Provider token adaptation shared by desktop and mobile account assemblies.

use std::sync::Arc;

pub(crate) fn with_token_exchange(
    inner: Arc<dyn llm_runtime::CredentialProvider>,
    transport: Arc<dyn llm_runtime::Transport>,
) -> Arc<dyn llm_runtime::CredentialProvider> {
    Arc::new(llm_runtime::CopilotExchangeCredentialProvider::new(
        inner,
        transport,
        "github-copilot",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::provider_accounts::{
        AccountAuthKind, AccountMetadata, AccountSnapshot, ProviderAccountAction,
        ProviderAccountCommand,
    };
    use llm_runtime::{CredentialScope, LlmRequest, ModelRuntime};
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::Mutex;

    struct ExchangeWire {
        exchanges: AtomicUsize,
        expires_at: AtomicU64,
        requests: Mutex<Vec<lingxi_llm_client::HttpRequest>>,
    }

    impl ExchangeWire {
        fn new(expires_at: u64) -> Self {
            Self {
                exchanges: AtomicUsize::new(0),
                expires_at: AtomicU64::new(expires_at),
                requests: Mutex::new(vec![]),
            }
        }
    }

    #[async_trait::async_trait]
    impl llm_runtime::Transport for ExchangeWire {
        async fn send(
            &self,
            request: lingxi_llm_client::HttpRequest,
        ) -> Result<lingxi_llm_client::StreamResponse, lingxi_llm_client::protocol::LlmError>
        {
            if request.url == lingxi_llm_client::auth::oauth::copilot::COPILOT_TOKEN_EXCHANGE_URL {
                let authorization = request
                    .headers
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
                    .unwrap();
                assert!(authorization.1.starts_with("token gho_synthetic_"));
                let attempt = self.exchanges.fetch_add(1, Ordering::SeqCst) + 1;
                return Ok(lingxi_llm_client::HttpResponse {
                    status: 200,
                    headers: vec![],
                    body: serde_json::to_vec(&serde_json::json!({
                        "token": format!("synthetic-derived-{attempt}"),
                        "expires_at": self.expires_at.load(Ordering::SeqCst),
                    }))
                    .unwrap()
                    .into(),
                }
                .into());
            }
            assert!(request.url.starts_with("https://api.githubcopilot.com/"));
            self.requests.lock().unwrap().push(request);
            Err(lingxi_llm_client::protocol::LlmError::Transport {
                message: "synthetic inference captured after authentication".into(),
            })
        }
    }

    fn scope(account: &AccountMetadata, profile: &llm_runtime::ProviderProfile) -> CredentialScope {
        let mut scope = CredentialScope::new(
            profile.provider_id.clone(),
            format!("account.{}", account.account_id),
        )
        .with_route(profile.base_url.clone(), profile.protocol)
        .with_credential_id(account.credential_ref.secret_ref.clone());
        scope.expected_identity_generation = Some(account.identity_generation);
        scope
    }

    fn add(id: &str, revision: u64) -> ProviderAccountCommand {
        ProviderAccountCommand {
            expected_revision: revision,
            operation_id: None,
            action: ProviderAccountAction::Add {
                account_id: Some(id.into()),
                provider_id: "github-copilot".into(),
                display_name: "Synthetic Copilot".into(),
                execution_binding_id: "github-copilot".into(),
                auth_kind: AccountAuthKind::ApiKey,
            },
        }
    }

    fn configuration(
        profile: &llm_runtime::ProviderProfile,
        snapshot: &AccountSnapshot,
    ) -> llm_runtime::ClientConfig {
        let mut config = llm_runtime::ClientConfig {
            providers: vec![profile.clone()],
        };
        provider_config::assemble_account_client_config(&mut config, &snapshot.accounts).unwrap();
        config
    }

    #[tokio::test]
    async fn shared_mobile_desktop_adapter_exchanges_stored_github_tokens_and_fences_its_cache() {
        for streaming in [false, true] {
            let home = tempfile::tempdir().unwrap();
            let credentials = Arc::new(secret::CredentialManager::new(
                Arc::new(lingxi_core::host::InMemorySecureStorage::new()),
                Arc::new(platform_posix::PosixClock::new()),
                Arc::new(platform_posix::PosixHttp::new()),
            ));
            let store = crate::provider_accounts::ProviderAccountStore::new(
                home.path().to_owned(),
                "copilot_fixture".into(),
                credentials.clone(),
            );
            let a_id = "11111111-1111-4111-8111-111111111111";
            let b_id = "22222222-2222-4222-8222-222222222222";
            store
                .execute(add(a_id, 0), Some("gho_synthetic_shared".into()))
                .await
                .unwrap();
            let snapshot = store
                .execute(add(b_id, 1), Some("gho_synthetic_shared".into()))
                .await
                .unwrap();
            let a = snapshot
                .accounts
                .iter()
                .find(|account| account.account_id == a_id)
                .unwrap();
            let b = snapshot
                .accounts
                .iter()
                .find(|account| account.account_id == b_id)
                .unwrap();
            let directory = Arc::new(std::sync::RwLock::new(snapshot.clone()));
            let profile = llm_runtime::builtin_presets()
                .providers
                .into_iter()
                .find(|profile| profile.profile_name == "github-copilot")
                .unwrap();
            let wire = Arc::new(ExchangeWire::new(9_999_999_999));
            let provider = with_token_exchange(
                Arc::new(
                    provider_config::MultiCredentialProvider::new(credentials.clone())
                        .with_account_directory(directory.clone()),
                ),
                wire.clone(),
            );
            let a_scope = scope(a, &profile);
            let b_scope = scope(b, &profile);
            let a_token = provider.load_versioned(&a_scope).await.unwrap();
            let b_token = provider.load_versioned(&b_scope).await.unwrap();
            assert_ne!(
                a_token.credential, b_token.credential,
                "accounts partition the derived cache even when their raw token matches"
            );
            assert_eq!(
                provider.load_versioned(&a_scope).await.unwrap().credential,
                a_token.credential
            );
            assert_eq!(
                wire.exchanges.load(Ordering::SeqCst),
                2,
                "fresh token is cached"
            );

            let config = configuration(&profile, &snapshot);
            let client = ModelRuntime::from_config(config.clone())
                .unwrap()
                .with_credential_provider(provider.clone())
                .reconfigured_with_account_snapshot(config, &snapshot)
                .unwrap();
            let mut request = LlmRequest::new(profile.models[0].request_model.clone())
                .with_user_text("synthetic Copilot envelope");
            request.profile = Some(format!("account.{a_id}"));
            request.stream = streaming;
            let result = if streaming {
                client
                    .execute_stream(&request, wire.clone())
                    .await
                    .map(|_| ())
            } else {
                client.execute(&request, wire.clone()).await.map(|_| ())
            };
            assert!(matches!(
                result,
                Err(llm_runtime::LlmError::Transport { .. })
            ));
            {
                let requests = wire.requests.lock().unwrap();
                assert_eq!(requests.len(), 1);
                assert!(requests[0]
                    .headers
                    .iter()
                    .any(|(name, value)| name.eq_ignore_ascii_case("authorization")
                        && value == "Bearer synthetic-derived-1"));
                assert!(!requests[0]
                    .headers
                    .iter()
                    .any(|(_, value)| value.contains("gho_synthetic_")));
            }
            assert_eq!(wire.exchanges.load(Ordering::SeqCst), 2);

            let rotated = store
                .execute(
                    ProviderAccountCommand {
                        expected_revision: snapshot.config_revision,
                        operation_id: None,
                        action: ProviderAccountAction::SetCredential {
                            account_id: a_id.into(),
                        },
                    },
                    Some("gho_synthetic_rotated".into()),
                )
                .await
                .unwrap();
            *directory.write().unwrap() = rotated.clone();
            let a = rotated
                .accounts
                .iter()
                .find(|account| account.account_id == a_id)
                .unwrap();
            let new_scope = scope(a, &profile);
            assert!(provider.load_versioned(&a_scope).await.is_err());
            let new_token = provider.load_versioned(&new_scope).await.unwrap();
            assert_ne!(new_token.credential, a_token.credential);
            assert_eq!(new_token.credential_revision, Some(2));
            assert_eq!(
                provider.load_versioned(&b_scope).await.unwrap().credential,
                b_token.credential
            );
            assert_eq!(wire.exchanges.load(Ordering::SeqCst), 3);

            // The response fixture controls expiry directly: no sleep or real
            // clock adjustment is needed to prove expired entries re-exchange.
            let expired_wire = Arc::new(ExchangeWire::new(1));
            let expired = with_token_exchange(
                Arc::new(
                    provider_config::MultiCredentialProvider::new(credentials)
                        .with_account_directory(directory),
                ),
                expired_wire.clone(),
            );
            let first = expired.load_versioned(&new_scope).await.unwrap();
            let second = expired.load_versioned(&new_scope).await.unwrap();
            assert_ne!(first.credential, second.credential);
            assert_eq!(expired_wire.exchanges.load(Ordering::SeqCst), 2);
        }
    }
}
