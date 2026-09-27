//! Tests for [`platform_posix::PosixSandbox::unavailable_reason_for`].
//!
//! Faithful to claude-code `getSandboxUnavailableReason()` (sandbox-adapter.ts:562):
//! when `sandbox.enabled` is false the reason is always `None` (missing deps /
//! out-of-list platform are irrelevant if the user never opted in); when enabled
//! but the current platform is not in `sandbox.enabledPlatforms`
//! (`in_enabled_list == false`) the reason mentions the `enabledPlatforms`
//! rejection.

use platform_posix::PosixSandbox;

/// `unavailable_reason_for(false, _)` is always `None` (user did not enable the
/// sandbox); `unavailable_reason_for(true, false)` on a SUPPORTED host surfaces
/// the `enabledPlatforms` rejection string.
#[test]
fn unavailable_reason_for_respects_enabled_and_in_enabled_list() {
    // Not enabled → always None, regardless of in_enabled_list.
    assert_eq!(PosixSandbox::unavailable_reason_for(false, true), None);
    assert_eq!(PosixSandbox::unavailable_reason_for(false, false), None);

    // Enabled but the current platform is NOT in `enabledPlatforms`. On a
    // SUPPORTED host (macOS / Linux / WSL2 — the CI matrix this crate runs on),
    // the `in_enabled_list == false` branch wins BEFORE the deps check, so the
    // reason is deterministically the `enabledPlatforms` rejection. On an
    // unsupported host the platform-unsupported branch would fire first; this
    // crate only builds/tests on supported POSIX hosts, so we assert the list
    // rejection. Guard against the (theoretical) unsupported case by only
    // asserting the substring when a reason is produced AND it is not the
    // platform-unsupported / WSL1 message.
    if let Some(reason) = PosixSandbox::unavailable_reason_for(true, false) {
        // Either the enabledPlatforms rejection (supported host) or an
        // unsupported-platform / WSL1 message (non-supported host). On the
        // supported hosts this crate targets, it must be the former.
        let is_enabled_list = reason.contains("is not in sandbox.enabledPlatforms");
        let is_unsupported = reason.contains("is not supported (requires macOS, Linux, or WSL2)")
            || reason.contains("WSL1 is not supported");
        assert!(
            is_enabled_list || is_unsupported,
            "unexpected unavailable reason: {reason}"
        );
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        assert!(
            is_enabled_list,
            "on a supported POSIX host the in_enabled_list=false branch must \
             produce the enabledPlatforms rejection, got: {reason}"
        );
    } else {
        // A supported host with `in_enabled_list == false` MUST produce a reason.
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        panic!("expected an unavailable reason for in_enabled_list == false on a supported host");
    }
}
