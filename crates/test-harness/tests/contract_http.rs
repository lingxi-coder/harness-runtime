//! Drive the platform [`HttpTransport`] impls through the canonical contract,
//! using an in-process hyper echo server so the test never hits the network.
//!
//! The mock transport ([`test_harness::mocks::MockHttpTransport`]) is
//! scripted-response only — it does not adapt to an arbitrary base URL — so
//! it has its own narrower assertions and is exempt from the live-server
//! contract sweep.

use test_harness::contracts::http::{http_transport_contract_tests, spawn_echo_server};

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn posix_http_passes_contract() {
    let server = spawn_echo_server().await;
    let base = server.base_url();
    let http = platform_posix::PosixHttp::new();
    http_transport_contract_tests(&http, &base).await;
    server.shutdown().await;
}

#[cfg(target_os = "windows")]
#[tokio::test]
async fn windows_http_passes_contract() {
    let server = spawn_echo_server().await;
    let base = server.base_url();
    let http = platform_windows::WindowsHttp::new();
    http_transport_contract_tests(&http, &base).await;
    server.shutdown().await;
}
