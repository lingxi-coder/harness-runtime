//! Drive the platform [`RuntimeSpawner`] impls through the canonical contract.

use test_harness::contracts::runtime::runtime_spawner_contract_tests;
use test_harness::mocks::MockRuntimeSpawner;

#[tokio::test]
async fn mock_runtime_passes_contract() {
    let rt = MockRuntimeSpawner::default();
    runtime_spawner_contract_tests(&rt).await;
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn posix_runtime_passes_contract() {
    let rt = platform_posix::PosixRuntime::new();
    runtime_spawner_contract_tests(&rt).await;
}

#[cfg(target_os = "windows")]
#[tokio::test]
async fn windows_runtime_passes_contract() {
    let rt = platform_windows::WindowsRuntime::new();
    runtime_spawner_contract_tests(&rt).await;
}
