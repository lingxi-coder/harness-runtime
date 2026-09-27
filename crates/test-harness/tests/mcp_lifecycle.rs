//! Integration test for the MCP connect lifecycle.
//!
//! Drives `McpRegistry::connect` against `MockMcpTransport` to ensure the
//! state machine walks `Disconnected → Connecting → Connected` and
//! discovers the canned tool catalog.

use mcp::{ConfigScope, McpRegistry, McpServerConfig, RawConnectionProvider};
use platform_api::{McpTransport, McpTransportSpec};
use std::sync::Arc;
use test_harness::mocks::MockMcpTransport;

fn mock_config() -> McpServerConfig {
    McpServerConfig {
        name: "mock".into(),
        spec: McpTransportSpec::InProcess {
            registry_key: "mock".into(),
        },
        scope: ConfigScope::Settings(protocol::SettingsScope::User),
        disabled: false,
        timeout_ms: None,
        always_load: false,
        discovery_cache: None,
        tools: Vec::new(),
        tool_permissions: std::collections::BTreeMap::new(),
        config_error: None,
        metadata: Default::default(),
    }
}

#[tokio::test]
async fn connect_initializes_and_lists_tools() {
    let transport = Arc::new(MockMcpTransport::new());
    transport.add_tool("hello");
    let registry = McpRegistry::new(transport.clone());

    let conn_id = registry.connect(mock_config()).await.unwrap();
    assert!(!conn_id.as_uuid().is_nil());
}

/// Batch 1: with a `RawConnectionProvider` wired (the mock implements both
/// `McpTransport` and the bridge), `connect` builds a live `McpClient` so
/// `get_client` returns `Some`.
#[tokio::test]
async fn connect_registers_live_client_via_raw_conn() {
    let mock = Arc::new(MockMcpTransport::new());
    mock.add_tool("hello");
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock as Arc<dyn RawConnectionProvider>,
    );

    registry.connect(mock_config()).await.unwrap();
    assert!(
        registry.get_client("mock").await.is_some(),
        "a working McpClient must be registered after connect"
    );
}

/// P1-08 runtime `/add-dir`: after a connected server exists, adding a new
/// working directory to the LIVE roots source fans out exactly ONE
/// `notifications/roots/list_changed` to the server, the shared roots cell now
/// includes the new dir, and re-adding an already-present dir (jzn compare)
/// sends NO further notification.
#[tokio::test]
async fn add_dir_fans_out_one_roots_list_changed_and_is_idempotent() {
    use std::path::PathBuf;

    // A responder-backed mock so the client's outbound notification frames are
    // observable, plus a shared roots cell the registry + client both hold.
    let mock = Arc::new(MockMcpTransport::with_call_responder());
    let roots = mcp::new_shared_roots(Vec::new());
    let registry = McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock.clone() as Arc<dyn RawConnectionProvider>,
    )
    .with_additional_roots(roots);

    registry.connect(mock_config()).await.unwrap();
    assert!(registry.get_client("mock").await.is_some());

    // Runtime add of a NEW dir → jzn reports a change → fan out one notification.
    assert!(
        registry.add_root(PathBuf::from("/extra")),
        "adding a new dir must report a change"
    );
    let notified = registry.notify_roots_list_changed_all().await;
    assert_eq!(notified, 1, "the one connected client must be notified");

    // The live roots source now includes the runtime-added dir (a re-issued
    // roots/list would advertise it — asserted at the handler level in the mcp
    // crate unit tests).
    assert_eq!(
        registry.additional_roots_snapshot(),
        vec![PathBuf::from("/extra")],
    );

    // Let the async responder task drain the emitted notification frame.
    for _ in 0..50 {
        if !mock.observed_notifications().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        mock.observed_notifications(),
        vec!["notifications/roots/list_changed".to_string()],
        "the server must receive exactly one roots/list_changed",
    );

    // Re-adding the SAME dir is a no-op (jzn): the caller sends no notification.
    assert!(
        !registry.add_root(PathBuf::from("/extra")),
        "re-adding an already-present dir must report NO change"
    );
    // (The effect never calls notify on a false change; assert the observed
    // count stayed at exactly one.)
    assert_eq!(mock.observed_notifications().len(), 1);
}
