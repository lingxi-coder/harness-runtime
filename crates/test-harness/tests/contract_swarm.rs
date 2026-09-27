//! Drive the platform [`SwarmBackend`] impls through the canonical contract
//! suite. Real tmux exec coverage lives in `platforms/posix/src/swarm/`.

use test_harness::contracts::swarm::swarm_backend_contract_tests;

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn posix_inprocess_swarm_passes_contract() {
    // The `InProcessSwarmBackend` is always available (no external deps) so
    // it's the canonical contract subject. Real tmux / iTerm backends are
    // covered by their own integration tests under `platforms/posix/`.
    let s = platform_posix::swarm::InProcessSwarmBackend::new();
    swarm_backend_contract_tests(&s).await;
}

#[cfg(target_os = "windows")]
#[tokio::test]
async fn windows_swarm_passes_contract() {
    // Windows swarm is intentionally Unsupported (M2-01 correction).
    let s = platform_windows::WindowsSwarmBackend::new();
    assert!(!s.is_available(), "Windows swarm must report unavailable");
    swarm_backend_contract_tests(&s).await;
}
