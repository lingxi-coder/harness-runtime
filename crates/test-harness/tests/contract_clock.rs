//! Drive the platform [`Clock`] impls through the canonical contract suite.
//!
//! The mock impl, the posix impl (Linux/macOS), and the windows impl all
//! exercise the same `clock_contract_tests` body.

use test_harness::contracts::clock::clock_contract_tests;
use test_harness::mocks::MockClock;

#[tokio::test]
async fn mock_clock_passes_contract() {
    // `MockClock::at(0)` is fine — the suite only asserts non-decreasing and
    // saturate-at-zero behaviour, which the mock satisfies.
    let c = MockClock::at(0);
    clock_contract_tests(&c).await;
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn posix_clock_passes_contract() {
    let c = platform_posix::PosixClock::new();
    clock_contract_tests(&c).await;
}

#[cfg(target_os = "windows")]
#[tokio::test]
async fn windows_clock_passes_contract() {
    let c = platform_windows::WindowsClock::new();
    clock_contract_tests(&c).await;
}
