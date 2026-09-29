//! Pre-warm the macOS keychain access prompt at startup so it doesn't
//! interrupt the first interactive moment.
//!
//! Spawns a background task that immediately retrieves a credential from
//! [`SecureStorage`]; the first OS-level prompt happens during the warm-up
//! rather than during the user's first request.
//!
//! The eventual result is delivered through a `oneshot` channel callers
//! drain via [`KeychainPrefetch::consume`].
//!
//! M2-06 wired this through to the real
//! `platform_posix::secure_storage::MacOsKeychainStorage`. Callers
//! supply the `(service, account)` pair so the prefetch can target either
//! the OAuth entry (`service = "-credentials"`) or the legacy API-key entry
//! (`service = ""`). When the backend is the plaintext fallback, the same
//! call still works — the storage layer just hits the disk.

use lingxi_core::host::{RuntimeError, RuntimeSpawner, SecureStorage, SecureStorageError};
use lingxi_core::types::SecureStorageData;
use std::sync::Arc;
use tokio::sync::oneshot;

/// Result delivered by the background prefetch task.
type PrefetchResult = Result<Option<SecureStorageData>, SecureStorageError>;

/// Handle to the background keychain warm-up task.
pub struct KeychainPrefetch {
    rx: tokio::sync::Mutex<Option<oneshot::Receiver<PrefetchResult>>>,
}

impl KeychainPrefetch {
    /// Spawn the prefetch task on `runtime` and return a handle whose
    /// [`Self::consume`] yields the eventual storage result.
    ///
    /// `service` and `account` are passed straight through to
    /// [`SecureStorage::retrieve`]. For macOS keychain warm-up, pass
    /// `service = "-credentials"` and `account = $USER` so the cache is
    /// populated with the OAuth entry. For plaintext fallback, pass
    /// whatever `(service, account)` scheme the engine uses.
    ///
    /// The returned `BackgroundTaskHandle` from the spawner is intentionally
    /// dropped — prefetch is fire-and-forget and is not cancelled
    /// explicitly.
    pub async fn start(
        storage: Arc<dyn SecureStorage>,
        runtime: &dyn RuntimeSpawner,
        service: impl Into<String>,
        account: impl Into<String>,
    ) -> Result<Self, RuntimeError> {
        let service = service.into();
        let account = account.into();
        let (tx, rx) = oneshot::channel();
        runtime
            .spawn(
                "keychain-prefetch",
                Box::pin(async move {
                    let _ = tx.send(storage.retrieve(&service, &account).await);
                }),
            )
            .await?;
        Ok(Self {
            rx: tokio::sync::Mutex::new(Some(rx)),
        })
    }

    /// Consume the prefetched storage result, if available.
    ///
    /// Returns `None` if the result has already been consumed or if the
    /// background task panicked / was cancelled before sending.
    pub async fn consume(&self) -> Option<PrefetchResult> {
        let rx = self.rx.lock().await.take()?;
        rx.await.ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use lingxi_core::host::{BackgroundTaskHandle, SecureStorageBackend};
    use lingxi_core::types::{SecretKindDto, SecureStorageData, SecureStorageMetadata};
    use std::sync::Mutex;
    use std::time::Duration;

    struct MockStorage {
        invocations: Mutex<Vec<(String, String)>>,
    }

    #[async_trait]
    impl SecureStorage for MockStorage {
        async fn store(
            &self,
            _service: &str,
            _account: &str,
            _data: SecureStorageData,
        ) -> Result<(), SecureStorageError> {
            Ok(())
        }
        async fn retrieve(
            &self,
            service: &str,
            account: &str,
        ) -> Result<Option<SecureStorageData>, SecureStorageError> {
            self.invocations
                .lock()
                .unwrap()
                .push((service.into(), account.into()));
            Ok(Some(SecureStorageData::new(
                b"ok".to_vec(),
                SecureStorageMetadata {
                    created_at: std::time::SystemTime::now(),
                    last_accessed: None,
                    kind: SecretKindDto("test".into()),
                },
            )))
        }
        async fn delete(&self, _service: &str, _account: &str) -> Result<(), SecureStorageError> {
            Ok(())
        }
        async fn list(&self, _service: &str) -> Result<Vec<String>, SecureStorageError> {
            Ok(vec![])
        }
        fn is_encrypted(&self) -> bool {
            false
        }
        fn backend(&self) -> SecureStorageBackend {
            SecureStorageBackend::PlainText
        }
    }

    struct TokioSpawner;
    #[async_trait]
    impl RuntimeSpawner for TokioSpawner {
        async fn spawn(
            &self,
            name: &str,
            task: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
        ) -> Result<BackgroundTaskHandle, RuntimeError> {
            tokio::spawn(task);
            Ok(BackgroundTaskHandle {
                task_name: name.to_string(),
                task_id: 1,
            })
        }
        async fn sleep(&self, duration: Duration) {
            tokio::time::sleep(duration).await;
        }
        async fn cancel(&self, _h: &BackgroundTaskHandle) -> Result<(), RuntimeError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn start_passes_service_and_account_through_to_storage() {
        let storage = Arc::new(MockStorage {
            invocations: Mutex::new(vec![]),
        });
        let spawner = TokioSpawner;
        let prefetch = KeychainPrefetch::start(
            storage.clone() as Arc<dyn SecureStorage>,
            &spawner,
            "-credentials",
            "alice",
        )
        .await
        .expect("start");
        let result = prefetch.consume().await.expect("result delivered");
        assert!(result.is_ok());
        let inv = storage.invocations.lock().unwrap();
        assert_eq!(
            inv.as_slice(),
            &[("-credentials".to_string(), "alice".to_string())]
        );
    }
}
