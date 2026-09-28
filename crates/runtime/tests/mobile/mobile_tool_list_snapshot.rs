//! M8-P11 — lock the mobile tool set assembled by the composition root, and
//! assert it omits the desktop-only + device-control tools (the composition
//! roots must not drift into registering the same set).

#![allow(clippy::unwrap_used)]

use harness_runtime::mobile::mobile_tool_registry;
use platform_api::process::ProcessOutput;

#[test]
fn mobile_tool_list_snapshot() {
    let ctx = tool_api::test_support::shell_test_ctx(ProcessOutput {
        stdout: String::new(),
        stderr: String::new(),
        exit_code: 0,
        timed_out: false,
    });
    let reg = mobile_tool_registry(ctx);

    let mut names = reg.all_names();
    names.sort();

    // Removed tools must be absent on every host, including mobile.
    for removed in ["TeamCreate", "TeamDelete"] {
        assert!(!names.iter().any(|name| name == removed));
    }

    // Desktop-only + device-control tools must NOT be in the mobile set.
    for forbidden in [
        "Bash",
        "PowerShell",
        "REPL",
        "MCP",
        "Agent",
        "EnterWorktree",
        "ExitWorktree",
        "computer",
        "android_use",
        "ios_use",
        // Monitor's Bash/process-substitution contract is not portable to the
        // restricted mobile Shell runtime.
        "Monitor",
    ] {
        assert!(
            !names.iter().any(|n| n == forbidden),
            "mobile tool set must not include `{forbidden}`"
        );
    }
    // Mobile-exclusive tools must always be present. LSP is part of the
    // production UniFFI profile; the default host-only test build intentionally
    // omits its optional runtime dependency.
    for required in ["camera", "voice", "share"] {
        assert!(
            names.iter().any(|n| n == required),
            "mobile tool set must include `{required}`"
        );
    }
    #[cfg(feature = "mobile")]
    assert!(
        names.iter().any(|name| name == "LSP"),
        "the production mobile tool set must include `LSP`"
    );

    #[cfg(feature = "mobile")]
    insta::assert_yaml_snapshot!("mobile_tool_list", names);
    #[cfg(not(feature = "mobile"))]
    insta::assert_yaml_snapshot!("mobile_tool_list_host_only", names);
}
