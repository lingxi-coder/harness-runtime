//! Regression tests for dependency check test.

use sandbox::dependency_check::{sandbox_unavailable_reason, MissingDeps, SandboxDependencyCheck};
use sandbox::runtime_config::Platform;

#[test]
fn unavailable_reason_wsl1_string_exact() {
    let r = sandbox_unavailable_reason(
        true,
        true,
        Some(Platform::Wsl),
        true, // wsl_one detected
        None,
        &SandboxDependencyCheck::default(),
    );
    assert_eq!(
        r.as_deref(),
        Some("sandbox.enabled is set but WSL1 is not supported (requires WSL2)")
    );
}

#[test]
fn unavailable_reason_unsupported_platform_string_exact() {
    // Platform::None case: we pass None as the detected platform.
    let r = sandbox_unavailable_reason(
        true,
        false,
        None,
        false,
        Some("windows".to_string()),
        &SandboxDependencyCheck::default(),
    );
    assert_eq!(
        r.as_deref(),
        Some(
            "sandbox.enabled is set but windows is not supported \
             (requires macOS, Linux, or WSL2)"
        )
    );
}

#[test]
fn unavailable_reason_platform_not_in_enabled_list() {
    let r = sandbox_unavailable_reason(
        true,
        true,
        Some(Platform::Mac),
        false,
        None,
        &SandboxDependencyCheck {
            errors: vec![],
            warnings: vec![],
            in_enabled_list: false,
        },
    );
    assert_eq!(
        r.as_deref(),
        Some("sandbox.enabled is set but macos is not in sandbox.enabledPlatforms")
    );
}

#[test]
fn unavailable_reason_missing_deps_macos_hint() {
    let r = sandbox_unavailable_reason(
        true,
        true,
        Some(Platform::Mac),
        false,
        None,
        &SandboxDependencyCheck {
            errors: vec!["sandbox-exec not found".into()],
            warnings: vec![],
            in_enabled_list: true,
        },
    );
    assert_eq!(
        r.as_deref(),
        Some(
            "sandbox.enabled is set but dependencies are missing: \
             sandbox-exec not found · run /sandbox or /doctor for details"
        )
    );
}

#[test]
fn unavailable_reason_missing_deps_linux_hint() {
    let r = sandbox_unavailable_reason(
        true,
        true,
        Some(Platform::Linux),
        false,
        None,
        &SandboxDependencyCheck {
            errors: vec!["bwrap not found".into(), "socat not found".into()],
            warnings: vec![],
            in_enabled_list: true,
        },
    );
    assert_eq!(
        r.as_deref(),
        Some(
            "sandbox.enabled is set but dependencies are missing: \
             bwrap not found, socat not found · install missing tools \
             (e.g. apt install bubblewrap socat) or run /sandbox for details"
        )
    );
}

#[test]
fn no_reason_when_sandbox_not_enabled() {
    // If sandbox.enabled is false, missing deps are irrelevant; no warning.
    let r = sandbox_unavailable_reason(
        false,
        true,
        Some(Platform::Mac),
        false,
        None,
        &SandboxDependencyCheck {
            errors: vec!["sandbox-exec not found".into()],
            warnings: vec![],
            in_enabled_list: true,
        },
    );
    assert_eq!(r, None);
}

#[test]
fn missing_deps_helpers() {
    // Compile-only: ensure the public type exists.
    let _ = MissingDeps {
        sandbox_exec: false,
        bwrap: true,
        socat: true,
    };
}

#[test]
fn wsl1_refusal_byte_for_byte() {
    let r = sandbox_unavailable_reason(
        true,
        true,
        Some(Platform::Wsl),
        true, // wsl_one_detected
        None,
        &SandboxDependencyCheck::default(),
    );
    let expected = "sandbox.enabled is set but WSL1 is not supported (requires WSL2)";
    let actual = r.expect("WSL1 with sandbox.enabled must produce a reason");
    assert_eq!(actual.as_bytes(), expected.as_bytes());
}
