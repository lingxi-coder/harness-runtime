//! Integration test: drive the mock stdio MCP fixture binary through
//! `spawn_stdio` and verify a JSON-RPC `ping` request round-trips.

use platform_common::mcp_stdio::StdioConfig;
use platform_posix::mcp::spawn_stdio;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

mod support;

fn fixture_bin_path() -> PathBuf {
    support::mock_stdio_mcp_bin()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ping_roundtrips_via_stdio() {
    let bin = fixture_bin_path();
    let cfg = StdioConfig {
        cmd: bin.to_string_lossy().into_owned(),
        args: vec![],
        env: HashMap::new(),
        cwd: None,
    };

    let conn = spawn_stdio(cfg).await.expect("spawn_stdio failed");

    let result: serde_json::Value = tokio::time::timeout(
        Duration::from_secs(5),
        conn.call("ping", serde_json::json!({})),
    )
    .await
    .expect("ping request timed out")
    .expect("ping returned an error");

    assert_eq!(result, serde_json::json!({"pong": true}));

    conn.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn initialize_roundtrips_via_stdio() {
    let bin = fixture_bin_path();
    let cfg = StdioConfig {
        cmd: bin.to_string_lossy().into_owned(),
        args: vec![],
        env: HashMap::new(),
        cwd: None,
    };

    let conn = spawn_stdio(cfg).await.expect("spawn_stdio failed");

    let result: serde_json::Value = tokio::time::timeout(
        Duration::from_secs(5),
        conn.call("initialize", serde_json::json!({})),
    )
    .await
    .expect("initialize request timed out")
    .expect("initialize returned an error");

    assert_eq!(
        result.get("protocolVersion").and_then(|v| v.as_str()),
        Some("2025-11-25")
    );
    assert_eq!(
        result.pointer("/serverInfo/name").and_then(|v| v.as_str()),
        Some("mock_stdio_mcp")
    );

    conn.close();
}
