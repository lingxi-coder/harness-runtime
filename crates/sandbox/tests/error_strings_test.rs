use sandbox::dependency_check::error_strings;

#[test]
fn wsl1_refusal_constant() {
    assert_eq!(
        error_strings::WSL1_REFUSAL,
        "sandbox.enabled is set but WSL1 is not supported (requires WSL2)"
    );
}

#[test]
fn unsupported_template_constant() {
    assert_eq!(
        error_strings::UNSUPPORTED_PLATFORM_TEMPLATE,
        "sandbox.enabled is set but {platform} is not supported (requires macOS, Linux, or WSL2)"
    );
}

#[test]
fn enabled_platforms_template_constant() {
    assert_eq!(
        error_strings::NOT_IN_ENABLED_PLATFORMS_TEMPLATE,
        "sandbox.enabled is set but {platform} is not in sandbox.enabledPlatforms"
    );
}

#[test]
fn missing_deps_macos_hint() {
    assert_eq!(
        error_strings::MISSING_DEPS_HINT_MAC,
        "run /sandbox or /doctor for details"
    );
}

#[test]
fn missing_deps_linux_hint() {
    assert_eq!(
        error_strings::MISSING_DEPS_HINT_LINUX,
        "install missing tools (e.g. apt install bubblewrap socat) or run /sandbox for details"
    );
}
