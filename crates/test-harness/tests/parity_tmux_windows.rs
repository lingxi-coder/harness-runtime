//! Parity fixture: Windows tmux refusal.
//!
//! claude-code refuses `--tmux` on Windows. The Rust port mirrors that at
//! the [`SwarmBackend`] trait boundary:
//!
//! - `WindowsSwarmBackend::is_available()` returns `false`.
//! - Every [`SwarmBackend`] method on `WindowsSwarmBackend` returns
//!   [`SwarmError::Unsupported`].
//!
//! The fixture documents both the boolean and the `Display` string. The
//! latter is what surfaces in CLI / `/doctor` output and must stay stable
//! across releases.

use serde::Deserialize;
use test_harness::parity::load_fixture;

#[derive(Deserialize)]
#[allow(clippy::struct_field_names)] // mirrors the JSON fixture's `expected_*` keys
struct Fixture {
    expected_is_available: bool,
    expected_variant: String,
    expected_display_message: String,
}

#[cfg(target_os = "windows")]
#[tokio::test]
async fn windows_swarm_refuses_tmux_with_claude_code_message() {
    use platform_api::swarm::{SwarmBackend, SwarmError, SwarmLayout};

    let fx: Fixture = load_fixture("tmux_windows_refusal");
    let s = platform_windows::WindowsSwarmBackend::new();

    assert_eq!(
        s.is_available(),
        fx.expected_is_available,
        "WindowsSwarmBackend::is_available() must match claude-code refusal",
    );

    let r = s.start_swarm(SwarmLayout::LeaderFollower).await;
    let err = r.expect_err("Windows start_swarm must return an error");
    match &err {
        SwarmError::Unsupported => {
            assert_eq!(
                fx.expected_variant, "Unsupported",
                "fixture variant must match SwarmError::Unsupported",
            );
            let displayed = format!("{err}");
            assert_eq!(
                displayed, fx.expected_display_message,
                "Display impl of SwarmError::Unsupported must match fixture exactly",
            );
        }
        other => panic!("Windows start_swarm must return SwarmError::Unsupported, got {other:?}",),
    }
}

#[cfg(not(target_os = "windows"))]
#[test]
fn windows_swarm_fixture_loads_on_non_windows() {
    // Linux + macOS CI still verifies the fixture is well-formed JSON.
    // The variant + display string are also asserted here so the fixture
    // cannot silently drift away from the trait's actual `Display` impl
    // on POSIX hosts (where the variant is identical — only the platform
    // gating of the test driver differs).
    let fx: Fixture = load_fixture("tmux_windows_refusal");
    let err = platform_api::swarm::SwarmError::Unsupported;
    let displayed = format!("{err}");
    assert!(!fx.expected_is_available, "Windows must report unavailable");
    assert_eq!(
        fx.expected_variant, "Unsupported",
        "fixture must name the SwarmError::Unsupported variant",
    );
    assert_eq!(
        fx.expected_display_message, displayed,
        "fixture must match the Display impl of SwarmError::Unsupported on POSIX hosts too \
         (the variant is platform-agnostic)",
    );
}
