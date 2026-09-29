//! P4-T9 — assert the Git tool registration gate:
//!   - default ctx (`android_git: None`) → Git is ABSENT
//!   - ctx with `android_git: Some(enabled: true, ..)` → Git is PRESENT
//!   - ctx with `android_git: Some(enabled: false, ..)` → Git is ABSENT
//!
//! Also asserts the existing `mobile_tool_list_snapshot` is unchanged:
//! Git is absent from the default (None) registry.

#![allow(clippy::unwrap_used)]

use harness_runtime::mobile::mobile_tool_registry;
use lingxi_core::host::process::ProcessOutput;
use tool_api::{AndroidGitToolCtx, BuiltinToolContext};

fn base_ctx() -> BuiltinToolContext {
    tool_api::test_support::shell_test_ctx(ProcessOutput {
        stdout: String::new(),
        stderr: String::new(),
        exit_code: 0,
        timed_out: false,
    })
}

/// Helper: does the registry built from `ctx` contain a tool named "Git"?
fn has_git(ctx: BuiltinToolContext) -> bool {
    let reg = mobile_tool_registry(ctx);
    reg.all_names().iter().any(|n| n == "Git")
}

#[test]
fn git_absent_when_android_git_is_none() {
    let ctx = base_ctx(); // android_git defaults to None
    assert!(
        !has_git(ctx),
        "Git must NOT be registered when android_git is None"
    );
}

#[test]
fn git_present_when_android_git_enabled() {
    let mut ctx = base_ctx();
    ctx.android_git = Some(AndroidGitToolCtx {
        enabled: true,
        has_token: true,
        workspace_root: "/x".into(),
    });
    assert!(
        has_git(ctx),
        "Git MUST be registered when android_git.enabled == true"
    );
}

#[test]
fn git_absent_when_android_git_disabled() {
    let mut ctx = base_ctx();
    ctx.android_git = Some(AndroidGitToolCtx {
        enabled: false,
        has_token: false,
        workspace_root: "/x".into(),
    });
    assert!(
        !has_git(ctx),
        "Git must NOT be registered when android_git.enabled == false"
    );
}

/// Verify the default tool list (`android_git`: None) does NOT include "Git".
/// This mirrors the assertion in `mobile_tool_list_snapshot` that the snapshot
/// must be unchanged when Git is gated off by default.
#[test]
fn git_absent_in_default_tool_list() {
    let ctx = base_ctx(); // android_git defaults to None
    let reg = mobile_tool_registry(ctx);
    let names = reg.all_names();
    assert!(
        !names.iter().any(|n| n == "Git"),
        "Git must be absent from the default mobile tool list (android_git: None)"
    );
}
