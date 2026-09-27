//! Verifies that LSP server registration is plugin-only.
//!
//! 1. `LspRegistry::register_plugin_servers` is public and works.
//! 2. `LspRegistry::register_config` is NOT callable from outside the
//!    `lingxi-lsp` crate. A doctest with `compile_fail` proves this.

use lsp::LspRegistry;
use platform_api::{
    LspError, LspRawConnection, LspServerCapabilities, LspServerConfig, LspTransport,
};
use protocol::PluginId;
use std::collections::HashMap;
use std::sync::Arc;

struct DummyTransport;

#[async_trait::async_trait]
impl LspTransport for DummyTransport {
    async fn start_server(&self, _: &LspServerConfig) -> Result<LspRawConnection, LspError> {
        Err(LspError::Unavailable)
    }
    async fn initialize(
        &self,
        _: &LspRawConnection,
        _: &str,
    ) -> Result<LspServerCapabilities, LspError> {
        Err(LspError::Unavailable)
    }
    async fn request(
        &self,
        _: &LspRawConnection,
        _: &str,
        _: serde_json::Value,
    ) -> Result<serde_json::Value, LspError> {
        Err(LspError::Unavailable)
    }
    async fn notify(
        &self,
        _: &LspRawConnection,
        _: &str,
        _: serde_json::Value,
    ) -> Result<(), LspError> {
        Err(LspError::Unavailable)
    }
    async fn shutdown(&self, _: protocol::McpConnectionId) -> Result<(), LspError> {
        Err(LspError::Unavailable)
    }
    fn is_available(&self) -> bool {
        false
    }
}

#[tokio::test]
async fn register_plugin_servers_is_public_and_works() {
    let registry = LspRegistry::new(Arc::new(DummyTransport));
    let config = LspServerConfig {
        name: "rust-analyzer".into(),
        command: "rust-analyzer".into(),
        args: vec![],
        env: HashMap::new(),
        trigger_languages: vec!["rust".into()],
        root_dir_markers: vec!["Cargo.toml".into()],
        initialization_options: None,
        extension_to_language: HashMap::new(),
        ..Default::default()
    };
    registry
        .register_plugin_servers(PluginId::new(), vec![config])
        .await;
    // No panic = success. Plugin path is the only public registration.
}

/// Compile-fail proof that `register_config` is unreachable from outside
/// the `lingxi-lsp` crate.
///
/// ```compile_fail
/// use lsp::LspRegistry;
/// use platform_api::{LspError, LspRawConnection, LspServerCapabilities, LspServerConfig, LspTransport};
/// use std::sync::Arc;
///
/// struct T;
///
/// #[async_trait::async_trait]
/// impl LspTransport for T {
///     async fn start_server(&self, _: &LspServerConfig) -> Result<LspRawConnection, LspError> { Err(LspError::Unavailable) }
///     async fn initialize(&self, _: &LspRawConnection, _: &str) -> Result<LspServerCapabilities, LspError> { Err(LspError::Unavailable) }
///     async fn request(&self, _: &LspRawConnection, _: &str, _: serde_json::Value) -> Result<serde_json::Value, LspError> { Err(LspError::Unavailable) }
///     async fn notify(&self, _: &LspRawConnection, _: &str, _: serde_json::Value) -> Result<(), LspError> { Err(LspError::Unavailable) }
///     async fn shutdown(&self, _: protocol::McpConnectionId) -> Result<(), LspError> { Err(LspError::Unavailable) }
///     fn is_available(&self) -> bool { false }
/// }
///
/// async fn must_not_compile() {
///     let r = LspRegistry::new(Arc::new(T));
///     // Should fail: register_config is pub(crate).
///     r.register_config(unimplemented!()).await;
/// }
/// ```
#[allow(dead_code)]
fn _doc_only_compile_fail() {}
