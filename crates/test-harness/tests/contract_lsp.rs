//! Drive the platform [`LspTransport`] impls through the canonical contract.

use test_harness::contracts::lsp::lsp_transport_contract_tests;

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn posix_lsp_passes_contract() {
    let t = platform_posix::PosixLspTransport::new();
    lsp_transport_contract_tests(&t).await;
}

#[cfg(target_os = "windows")]
#[tokio::test]
async fn windows_lsp_passes_contract() {
    let t = platform_windows::WindowsLspTransport::new();
    lsp_transport_contract_tests(&t).await;
}
