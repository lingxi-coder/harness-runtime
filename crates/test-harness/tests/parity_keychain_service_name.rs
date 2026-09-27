//! Parity fixture: macOS Keychain service-name format.
//!
//! Locks the `full_service_name` builder against claude-code's
//! `src/utils/macOsKeychainHelpers.ts::getMacOsKeychainStorageServiceName`:
//!
//! `"Claude Code" + oauth_suffix + service_suffix + dir_hash`
//!
//! - `oauth_suffix` is empty for the default OAuth flow, otherwise
//!   `"-{flow}"`.
//! - `service_suffix` is `"-credentials"` for OAuth entries; empty for the
//!   legacy API-key entry.
//! - `dir_hash` is empty when the user is using the default config dir,
//!   otherwise `"-" + first 8 hex chars of sha256(config_dir)`.
//!
//! Drift on this format breaks cross-version OAuth token lookup on macOS,
//! so a parity test is the right place to lock it.

use serde::Deserialize;
use test_harness::parity::load_fixture;

#[derive(Deserialize)]
struct Case {
    label: String,
    input_base: String,
    input_oauth_suffix: String,
    input_service_suffix: String,
    input_dir_hash: String,
    expected_service_name: String,
}

#[derive(Deserialize)]
struct Fixture {
    cases: Vec<Case>,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn keychain_service_name_matches_claude_code() {
    use platform_posix::secure_storage::full_service_name;

    let fx: Fixture = load_fixture("secure_storage_macos_service_name");
    for case in &fx.cases {
        let got = full_service_name(
            &case.input_base,
            &case.input_oauth_suffix,
            &case.input_service_suffix,
            &case.input_dir_hash,
        );
        assert_eq!(
            got,
            case.expected_service_name,
            "[{}] full_service_name({:?}, {:?}, {:?}, {:?}) must yield {:?}, got {:?}",
            case.label,
            case.input_base,
            case.input_oauth_suffix,
            case.input_service_suffix,
            case.input_dir_hash,
            case.expected_service_name,
            got,
        );
    }
}

#[cfg(target_os = "windows")]
#[test]
fn keychain_service_name_fixture_loads_on_non_posix() {
    // The full_service_name helper lives in the posix platform crate
    // (Linux + macOS). On Windows we just confirm the fixture parses so a
    // syntax error in JSON would still fail this test.
    let fx: Fixture = load_fixture("secure_storage_macos_service_name");
    assert!(!fx.cases.is_empty(), "fixture must declare cases");
}
