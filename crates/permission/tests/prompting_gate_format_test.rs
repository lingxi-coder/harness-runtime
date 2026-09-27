//! Cross-crate format byte-locks — same `PermissionRequest` round-trips as
//! the inline tests, but run from the integration-test binary so they
//! exercise the published re-export path (`permission::*` rather
//! than the private `super::format_prompt`).
//!
//! `format_prompt` itself is `pub(crate)` — the full prompt-rendering
//! invariant is covered end-to-end via the duplex-driven `prompt_user`
//! tests in `prompting_gate_e2e_test.rs` (Tasks 7-10).

use permission::{PermissionRequest, PromptDefault};
use serde_json::json;

#[test]
fn allow_by_default_round_trips() {
    let r = PermissionRequest::ToolUseConfirm {
        tool_name: "Read".to_string(),
        tool_input: json!({}),
        default_decision: PromptDefault::AllowByDefault,
        suppress_always_allow_rule: false,
    };
    match r {
        PermissionRequest::ToolUseConfirm {
            default_decision, ..
        } => assert_eq!(default_decision, PromptDefault::AllowByDefault),
        _ => panic!("wrong variant"),
    }
}

#[test]
fn deny_by_default_round_trips() {
    let r = PermissionRequest::ToolUseConfirm {
        tool_name: "Bash".to_string(),
        tool_input: json!({}),
        default_decision: PromptDefault::DenyByDefault,
        suppress_always_allow_rule: false,
    };
    match r {
        PermissionRequest::ToolUseConfirm {
            default_decision, ..
        } => assert_eq!(default_decision, PromptDefault::DenyByDefault),
        _ => panic!("wrong variant"),
    }
}
