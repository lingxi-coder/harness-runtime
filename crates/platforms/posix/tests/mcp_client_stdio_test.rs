//! Verify `mcp::McpClient` can be constructed on top of the
//! `Connection` produced by `spawn_stdio` and successfully run an
//! `initialize` handshake (plus a `ping` liveness probe) against the
//! mock fixture binary.
//!
//! The mock fixture (`platforms/posix/tests/fixtures/mock_stdio_mcp`)
//! intentionally implements only `initialize` and `ping` — the full
//! tools/prompts/resources surface is exercised by the in-process mock in
//! `crates/mcp/tests/mock_mcp.rs`. This file's job is the stdio-level wiring:
//! prove that the real `McpClient` round-trips through `spawn_stdio` end to
//! end without any plumbing surprises (Arc wrapping, async constructor,
//! capability decode from the wire payload, etc.).

use mcp::McpClient;
use platform_common::mcp_stdio::StdioConfig;
use platform_posix::mcp::spawn_stdio;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

mod support;

fn fixture_bin_path() -> PathBuf {
    support::mock_stdio_mcp_bin()
}

fn spawn_cfg(bin: &Path) -> StdioConfig {
    StdioConfig {
        cmd: bin.to_string_lossy().into_owned(),
        args: vec![],
        env: HashMap::new(),
        cwd: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_client_initialize_handshake_completes_over_stdio() {
    let bin = fixture_bin_path();
    let conn = spawn_stdio(spawn_cfg(&bin))
        .await
        .expect("spawn_stdio failed");
    let client = McpClient::new("mock", PathBuf::from("/tmp/lingxi-test"), Arc::new(conn)).await;

    let caps = tokio::time::timeout(Duration::from_secs(5), client.initialize())
        .await
        .expect("initialize timed out")
        .expect("initialize returned an error");

    // The fixture advertises `tools`/`resources`/`prompts`/`logging` (each as
    // an empty object) plus an `experimental` map — presence of each key must
    // decode to `true`.
    assert!(caps.tools, "expected tools capability from fixture");
    assert!(caps.resources, "expected resources capability from fixture");
    assert!(caps.prompts, "expected prompts capability from fixture");
    assert!(caps.logging, "expected logging capability from fixture");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_client_ping_roundtrips_over_stdio() {
    let bin = fixture_bin_path();
    let conn = spawn_stdio(spawn_cfg(&bin))
        .await
        .expect("spawn_stdio failed");
    let client = McpClient::new("mock", PathBuf::from("/tmp/lingxi-test"), Arc::new(conn)).await;

    tokio::time::timeout(Duration::from_secs(5), client.ping())
        .await
        .expect("ping timed out")
        .expect("ping returned an error");
}
