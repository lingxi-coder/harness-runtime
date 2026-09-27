//! Drive the platform [`BridgeTransport`] impls through the canonical
//! contract suite.

use test_harness::contracts::bridge::bridge_transport_contract_tests;

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn posix_bridge_passes_contract() {
    let b = platform_posix::PosixBridgeTransport::new();
    bridge_transport_contract_tests(&b).await;
}

#[cfg(target_os = "windows")]
#[tokio::test]
async fn windows_bridge_passes_contract() {
    let b = platform_windows::WindowsBridgeTransport::new();
    bridge_transport_contract_tests(&b).await;
}
