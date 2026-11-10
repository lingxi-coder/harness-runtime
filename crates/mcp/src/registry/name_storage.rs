//! Raw UTF-16 MCP names use a separate secure-storage namespace family.
use super::*;
use lingxi_core::host::{SecureStorageBackend, SecureStorageError};
use lingxi_core::types::SecureStorageData;

pub(super) struct RawNameOAuthStorage {
    base: Arc<dyn SecureStorage>,
}
impl RawNameOAuthStorage {
    pub(super) fn new(base: Arc<dyn SecureStorage>) -> Self {
        Self { base }
    }
}
fn service_name(service: &str) -> String {
    format!("mcp-utf16-{service}")
}
#[async_trait::async_trait]
impl SecureStorage for RawNameOAuthStorage {
    async fn store(
        &self,
        service: &str,
        account: &str,
        data: SecureStorageData,
    ) -> Result<(), SecureStorageError> {
        self.base.store(&service_name(service), account, data).await
    }
    async fn retrieve(
        &self,
        service: &str,
        account: &str,
    ) -> Result<Option<SecureStorageData>, SecureStorageError> {
        self.base.retrieve(&service_name(service), account).await
    }
    async fn contains(&self, service: &str, account: &str) -> Result<bool, SecureStorageError> {
        self.base.contains(&service_name(service), account).await
    }
    async fn delete(&self, service: &str, account: &str) -> Result<(), SecureStorageError> {
        self.base.delete(&service_name(service), account).await
    }
    async fn list(&self, service: &str) -> Result<Vec<String>, SecureStorageError> {
        self.base.list(&service_name(service)).await
    }
    fn is_encrypted(&self) -> bool {
        self.base.is_encrypted()
    }
    fn backend(&self) -> SecureStorageBackend {
        self.base.backend()
    }
}

impl McpRegistry {
    /// Select from one registry-owned base/wrapper pair. Nested OAuth/XAA
    /// helpers cannot wrap twice or change the discovery cache's storage owner.
    pub(super) fn oauth_storage_for_config(
        &self,
        config: &McpServerConfig,
        _storage: &Arc<dyn SecureStorage>,
    ) -> Arc<dyn SecureStorage> {
        if config.has_raw_credential_name() {
            self.raw_name_oauth_storage
                .as_ref()
                .expect("OAuth dependencies install raw-name storage once")
                .clone()
        } else {
            self.oauth
                .as_ref()
                .expect("OAuth storage selection requires dependencies")
                .storage
                .clone()
        }
    }
}
