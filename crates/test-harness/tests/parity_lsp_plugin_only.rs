//! Parity fixture: LSP plugin-only registration.
//!
//! claude-code's `services/lsp/config.ts::getAllLspServers()` only consults
//! `getPluginLspServers()` — user/project settings cannot register LSP
//! servers. We mirror that by making `LspRegistry::register_config`
//! `pub(crate)` so the only public path is
//! `LspRegistry::register_plugin_servers`.
//!
//! The existing `crates/lsp/tests/plugin_only_registration_test.rs`
//! exercises the `compile_fail` doctest end-to-end inside the `lingxi-lsp`
//! crate. This parity driver lives in the cross-cutting test-harness so a
//! future change to the registry signature trips an alarm even if the
//! per-crate test were accidentally moved.

use lsp::LspRegistry;
use platform_api::{
    LspError, LspRawConnection, LspServerCapabilities, LspServerConfig, LspTransport,
};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use test_harness::parity::load_fixture;

#[derive(Deserialize)]
struct Fixture {
    behavior: String,
    rationale: String,
    permitted_callers: Vec<String>,
}

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

#[test]
fn lsp_plugin_only_fixture_loads() {
    let fx: Fixture = load_fixture("lsp_plugin_only");
    assert!(
        fx.behavior.contains("pub(crate)"),
        "fixture must describe the pub(crate) visibility lock",
    );
    assert!(
        fx.rationale.contains("getPluginLspServers"),
        "fixture must cite claude-code's getPluginLspServers anchor",
    );
    assert!(
        fx.permitted_callers
            .iter()
            .any(|p| p.contains("crates/plugin/src/manager.rs")),
        "fixture must list the plugin manager as a permitted caller, got {:?}",
        fx.permitted_callers,
    );
}

/// Smoke test: this test file lives in `lingxi-test-harness/tests/`
/// (outside the `lingxi-lsp` crate). The only LSP registration we can call
/// from here is `register_plugin_servers`, the plugin-only public path.
///
/// `register_config` is `pub(crate)` — if it were ever loosened to `pub`,
/// the compile-fail doctest inside
/// `crates/lsp/tests/plugin_only_registration_test.rs` would start
/// compiling and `cargo test -p lingxi-lsp` would fail. We don't duplicate
/// the compile_fail block here (cargo only runs doctests on the host
/// crate); we just exercise the legal path so that any signature change to
/// `register_plugin_servers` also has to update the parity driver.
#[tokio::test]
async fn lsp_plugin_only_public_path_works_from_external_crate() {
    let registry = LspRegistry::new(Arc::new(DummyTransport));
    let config = LspServerConfig {
        name: "rust-analyzer-parity".into(),
        command: "rust-analyzer".into(),
        args: vec![],
        env: HashMap::new(),
        trigger_languages: vec!["rust".into()],
        root_dir_markers: vec!["Cargo.toml".into()],
        initialization_options: None,
        extension_to_language: HashMap::new(),
        ..Default::default()
    };
    // Only public registration entry point: register_plugin_servers.
    // Calling `registry.register_config(...)` here would not compile.
    registry
        .register_plugin_servers(protocol::PluginId::new(), vec![config])
        .await;
}
