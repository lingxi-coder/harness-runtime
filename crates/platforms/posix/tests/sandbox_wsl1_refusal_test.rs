//! Integration test — WSL1 refusal pipeline (Task 19 / M2-04 Phase D).
//!
//! Exercises the `parse_wsl_kind` parser against synthetic `/proc/version`
//! markup and confirms that
//! [`sandbox::dependency_check::sandbox_unavailable_reason`] produces
//! the exact byte-for-byte WSL1 refusal string.

use platform_posix::wsl_detect::{parse_wsl_kind, WslKind};
use sandbox::dependency_check::{
    error_strings, sandbox_unavailable_reason, SandboxDependencyCheck,
};
use sandbox::runtime_config::Platform;

#[test]
fn wsl1_detected_from_proc_version_emits_exact_refusal_string() {
    // Synthetic WSL1 /proc/version content.
    let proc_version = "Linux version 4.4.0-19041-Microsoft (Microsoft@Microsoft.com)";
    let kind = parse_wsl_kind(proc_version);
    assert_eq!(kind, WslKind::WslOne);

    // The full pipeline: sandbox.enabled = true, the detected platform is
    // None (we refuse WSL1), wsl_one_detected = true.
    let deps = SandboxDependencyCheck::default();
    let reason = sandbox_unavailable_reason(
        true,
        false, // supported_platform: WSL1 is NOT supported
        None,  // no Platform variant
        true,  // wsl_one_detected
        Some("wsl".to_string()),
        &deps,
    );
    let actual = reason.expect("WSL1 with sandbox.enabled must produce a reason");
    assert_eq!(actual, error_strings::WSL1_REFUSAL);
    // Re-state the byte sequence explicitly:
    assert_eq!(
        actual,
        "sandbox.enabled is set but WSL1 is not supported (requires WSL2)"
    );
}

#[test]
fn wsl2_detected_from_proc_version_does_not_emit_wsl1_refusal() {
    let proc_version = "Linux version 5.15.90.1-microsoft-standard-WSL2";
    let kind = parse_wsl_kind(proc_version);
    assert_eq!(kind, WslKind::WslTwo);
    // WSL2 is supported. The unavailable reason should not be the WSL1 string.
    let deps = SandboxDependencyCheck::default();
    let reason = sandbox_unavailable_reason(true, true, Some(Platform::Wsl), false, None, &deps);
    assert!(
        reason.as_deref() != Some(error_strings::WSL1_REFUSAL),
        "WSL2 must not yield the WSL1 string, got {reason:?}"
    );
}
