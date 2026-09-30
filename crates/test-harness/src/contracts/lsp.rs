//! [`LspTransport`] contract test suite.
//!
//! Real LSP servers are heavy and not portable to ubuntu-latest CI without a
//! sidecar install; the contract verifies trait invariants only. Roundtrip
//! tests against the bundled `mock_lsp_server` binary live in
//! `crates/test-harness/tests/` and in `lingxi-lsp`'s own integration suite.
//!
//! Invariants:
//!
//! * `is_available()` answers a `bool` without panicking.
//! * `shutdown(unknown_id)` is idempotent — does not panic; returns `Ok` or a
//!   transport error, never `unwrap` on a missing entry.
//! * On a transport that reports `is_available() == true`, `start_server`
//!   with a nonexistent binary surfaces an [`LspError`] rather than hanging
//!   or succeeding.

use lingxi_core::host::{LspServerConfig, LspTransport};
use lingxi_core::types::McpConnectionId;
use std::collections::HashMap;

/// Run the standard [`LspTransport`] contract against an impl.
///
/// # Panics
///
/// Panics on the first invariant violation.
pub async fn lsp_transport_contract_tests<T: LspTransport>(t: &T) {
    test_is_available_returns_bool(t);
    test_shutdown_unknown_conn_is_idempotent(t).await;
    test_start_server_with_bogus_binary_returns_error(t).await;
}

fn test_is_available_returns_bool<T: LspTransport>(t: &T) {
    let _ = t.is_available();
}

async fn test_shutdown_unknown_conn_is_idempotent<T: LspTransport>(t: &T) {
    // Calling shutdown twice on an id that never existed must not panic.
    // Either Ok or a transport-level error is within spec.
    let bogus = McpConnectionId::new();
    let _ = t.shutdown(bogus).await;
    let r = t.shutdown(bogus).await;
    let _ = r; // any non-panic outcome is acceptable
}

async fn test_start_server_with_bogus_binary_returns_error<T: LspTransport>(t: &T) {
    if !t.is_available() {
        return;
    }
    let config = LspServerConfig {
        name: "contract-bogus".into(),
        command: "__nonexistent_binary_contract_test__".into(),
        args: vec![],
        env: HashMap::new(),
        trigger_languages: vec![],
        root_dir_markers: vec![],
        initialization_options: None,
        extension_to_language: HashMap::new(),
        ..Default::default()
    };
    let r = t.start_server(&config).await;
    assert!(
        r.is_err(),
        "starting a nonexistent binary must error, got Ok({r:?})"
    );
}
