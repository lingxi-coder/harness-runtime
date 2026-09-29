//! Parity: MCP Batch 5c — all-servers `ListMcpResources`.
//!
//! Locks the claim that `ListMcpResourcesTool` with NO `server_name` lists EVERY
//! connected server's resources, tags each row with its `server`, and
//! ERROR-ISOLATES per server (one server's `resources/list` failure does not
//! sink the whole call).
//!
//! TS ref: `tools/ListMcpResourcesTool/ListMcpResourcesTool.ts:66-101`
//! (no `targetServer` → all `mcpClients`; per-client `try/catch` returning [];
//! flattened output rows `{uri, name, mimeType?, server}`).

#![allow(clippy::field_reassign_with_default)]

use std::sync::Arc;

use mcp::{ConfigScope, McpRegistry, McpServerConfig, RawConnectionProvider};
use mobile_linux_api::ProcessOutput;
use platform_api::{McpTransport, McpTransportSpec};
use test_harness::mocks::MockMcpTransport;
use tool_api::BuiltinToolContext;
use tool_mcp::ListMcpResourcesTool;

// ============================================================================
// Helpers
// ============================================================================

fn config(name: &str) -> McpServerConfig {
    McpServerConfig {
        name: name.into(),
        spec: McpTransportSpec::InProcess {
            registry_key: name.into(),
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

fn ctx_with_registry(registry: Arc<McpRegistry>) -> BuiltinToolContext {
    let mut ctx = tool_api::test_support::shell_test_ctx(ProcessOutput {
        stdout: String::new(),
        stderr: String::new(),
        exit_code: 0,
        timed_out: false,
    });
    ctx.mcp_registry = Some(registry);
    ctx
}

async fn call_list(tool: &ListMcpResourcesTool, input: serde_json::Value) -> serde_json::Value {
    use tool_api::tool_trait::Tool;
    let ctx = tool_api::test_support::fresh_ctx();
    let tx = tool_api::test_support::fresh_tx();
    tool.call(input, ctx, tx).await.expect("call ok").data
}

/// Extract the flat `resources` array from the tool result.
fn resources(data: &serde_json::Value) -> &Vec<serde_json::Value> {
    data.get("resources")
        .and_then(|v| v.as_array())
        .expect("result carries a `resources` array")
}

/// All `(server, uri)` pairs in the result, sorted for order-insensitive asserts.
fn server_uri_pairs(data: &serde_json::Value) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = resources(data)
        .iter()
        .map(|r| {
            (
                r.get("server")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .into(),
                r.get("uri").and_then(|s| s.as_str()).unwrap_or("").into(),
            )
        })
        .collect();
    v.sort();
    v
}

// ============================================================================
// no server_name → every connected server's resources, tagged by server
// ============================================================================

#[tokio::test]
async fn no_server_name_lists_all_servers_tagged_by_server() {
    let mock = Arc::new(MockMcpTransport::with_call_responder());
    mock.set_resources("s1", &[("file:///a", "a")]);
    mock.set_resources("s2", &[("file:///b", "b"), ("file:///c", "c")]);

    let registry = Arc::new(McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock.clone() as Arc<dyn RawConnectionProvider>,
    ));
    registry.connect(config("s1")).await.unwrap();
    registry.connect(config("s2")).await.unwrap();

    let tool = ListMcpResourcesTool::new(ctx_with_registry(registry));
    // No `server_name` → all servers.
    let data = call_list(&tool, serde_json::json!({})).await;

    assert_eq!(
        server_uri_pairs(&data),
        vec![
            ("s1".to_string(), "file:///a".to_string()),
            ("s2".to_string(), "file:///b".to_string()),
            ("s2".to_string(), "file:///c".to_string()),
        ],
        "every connected server's resources must be present, each tagged by server"
    );

    // Each row carries name + server (the TS output shape).
    let row = resources(&data)
        .iter()
        .find(|r| r.get("uri").and_then(|u| u.as_str()) == Some("file:///a"))
        .unwrap();
    assert_eq!(row.get("name").and_then(|v| v.as_str()), Some("a"));
    assert_eq!(row.get("server").and_then(|v| v.as_str()), Some("s1"));
}

// ============================================================================
// a per-server error is isolated — other servers still return
// ============================================================================

#[tokio::test]
async fn per_server_error_is_isolated() {
    let mock = Arc::new(MockMcpTransport::with_call_responder());
    mock.set_resources("s1", &[("file:///ok", "ok")]);
    mock.set_resources_error("s2"); // s2's resources/list returns a JSON-RPC error

    let registry = Arc::new(McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock.clone() as Arc<dyn RawConnectionProvider>,
    ));
    registry.connect(config("s1")).await.unwrap();
    registry.connect(config("s2")).await.unwrap();

    let tool = ListMcpResourcesTool::new(ctx_with_registry(registry));
    let data = call_list(&tool, serde_json::json!({})).await;

    // s1 still returns despite s2 failing — the whole call did NOT error.
    assert_eq!(
        server_uri_pairs(&data),
        vec![("s1".to_string(), "file:///ok".to_string())],
        "s2's failure must be isolated; s1's resources still return"
    );
}

// ============================================================================
// server_name present → single-server behavior preserved (now also tagged)
// ============================================================================

#[tokio::test]
async fn explicit_server_name_lists_only_that_server() {
    let mock = Arc::new(MockMcpTransport::with_call_responder());
    mock.set_resources("s1", &[("file:///a", "a")]);
    mock.set_resources("s2", &[("file:///b", "b")]);

    let registry = Arc::new(McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock.clone() as Arc<dyn RawConnectionProvider>,
    ));
    registry.connect(config("s1")).await.unwrap();
    registry.connect(config("s2")).await.unwrap();

    let tool = ListMcpResourcesTool::new(ctx_with_registry(registry));
    let data = call_list(&tool, serde_json::json!({ "server_name": "s2" })).await;

    assert_eq!(
        server_uri_pairs(&data),
        vec![("s2".to_string(), "file:///b".to_string())],
        "an explicit server_name must restrict the result to that server"
    );
}
