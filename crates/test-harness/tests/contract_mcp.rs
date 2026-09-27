//! Drive the platform [`McpTransport`] impls through the canonical contract.

use test_harness::contracts::mcp::mcp_transport_contract_tests;
use test_harness::mocks::MockMcpTransport;

#[tokio::test]
async fn mock_mcp_passes_contract() {
    let t = MockMcpTransport::new();
    mcp_transport_contract_tests(&t).await;
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn posix_mcp_passes_contract() {
    let t = platform_posix::PosixMcpTransport::new();
    mcp_transport_contract_tests(&t).await;
}

#[cfg(target_os = "windows")]
#[tokio::test]
async fn windows_mcp_passes_contract() {
    let t = platform_windows::WindowsMcpTransport::new();
    mcp_transport_contract_tests(&t).await;
}
