//! Drive the platform [`Sandbox`] impls through the canonical contract suite.

use test_harness::contracts::sandbox::sandbox_contract_tests;

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn posix_sandbox_passes_contract() {
    let s = platform_posix::PosixSandbox::new();
    sandbox_contract_tests(&s).await;
}

#[cfg(target_os = "windows")]
#[tokio::test]
async fn windows_sandbox_passes_contract() {
    // Windows sandbox is intentionally Unsupported (M2-01 correction). The
    // contract still validates that `bypass_with_audit` works and that
    // `prepare` returns `SandboxError::Unsupported`.
    let s = platform_windows::WindowsSandbox::new();
    sandbox_contract_tests(&s).await;
}
