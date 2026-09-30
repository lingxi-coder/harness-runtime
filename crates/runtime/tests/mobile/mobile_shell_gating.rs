//! P3-T4 — assert the Shell tool registration gate:
//!   - default ctx (`android_shell: None`) → Shell is ABSENT
//!   - ctx with `android_shell: Some(enabled: true, ..)` → Shell is PRESENT
//!   - ctx with `android_shell: Some(enabled: false, ..)` → Shell is ABSENT

#![allow(clippy::unwrap_used)]

use harness_runtime::mobile::mobile_tool_registry;

use mobile_linux_api::ProcessOutput;

use tool_api::{AndroidShellToolCtx, BuiltinToolContext};

fn base_ctx() -> BuiltinToolContext {
    tool_api::test_support::shell_test_ctx(ProcessOutput {
        stdout: String::new(),
        stderr: String::new(),
        exit_code: 0,
        timed_out: false,
    })
}

/// Helper: does the registry built from `ctx` contain a tool named "Shell"?
fn has_shell(ctx: BuiltinToolContext) -> bool {
    let reg = mobile_tool_registry(ctx);
    reg.all_names().iter().any(|n| n == "Shell")
}

#[test]
fn shell_absent_when_android_shell_is_none() {
    let ctx = base_ctx(); // android_shell defaults to None
    assert!(
        !has_shell(ctx),
        "Shell must NOT be registered when android_shell is None"
    );
}

#[test]
fn shell_present_when_android_shell_enabled() {
    let mut ctx = base_ctx();
    ctx.android_shell = Some(AndroidShellToolCtx::android_legacy(
        true,
        vec!["ls".to_string(), "cat".to_string()],
        Some("mksh R59".to_string()),
        false,
    ));
    assert!(
        has_shell(ctx),
        "Shell MUST be registered when android_shell.enabled == true"
    );
}

#[test]
fn shell_absent_when_android_shell_disabled() {
    let mut ctx = base_ctx();
    ctx.android_shell = Some(AndroidShellToolCtx::android_legacy(
        false,
        vec![],
        None,
        false,
    ));
    assert!(
        !has_shell(ctx),
        "Shell must NOT be registered when android_shell.enabled == false"
    );
}
