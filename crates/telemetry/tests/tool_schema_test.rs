//! Regression tests for tool schema test.

use telemetry::tengu::tool;

#[test]
fn all_34_tool_event_names_are_locked() {
    // The M3-06 baseline was 40 events; the 6 fabricated grep/glob events
    // (tengu_tool_grep_* / tengu_tool_glob_*) were removed to match
    // claude-code v2.1.183, which emits none — so the baseline is now 34.
    let names: &[&str] = &[
        tool::STARTED,
        tool::COMPLETED,
        tool::FAILED,
        tool::CANCELLED,
        tool::PERMISSION_REQUESTED,
        tool::PERMISSION_GRANTED,
        tool::PERMISSION_DENIED,
        tool::PERMISSION_REMEMBERED,
        tool::BASH_STARTED,
        tool::BASH_COMPLETED,
        tool::BASH_FAILED,
        tool::BASH_TIMEOUT,
        tool::EDIT_STARTED,
        tool::EDIT_COMPLETED,
        tool::EDIT_FAILED,
        tool::READ_STARTED,
        tool::READ_COMPLETED,
        tool::READ_FAILED,
        tool::WRITE_STARTED,
        tool::WRITE_COMPLETED,
        tool::WRITE_FAILED,
        tool::WEB_FETCH_STARTED,
        tool::WEB_FETCH_COMPLETED,
        tool::WEB_FETCH_FAILED,
        tool::TASK_DISPATCHED,
        tool::TASK_COMPLETED,
        tool::TASK_FAILED,
        tool::NOTEBOOK_STARTED,
        tool::NOTEBOOK_COMPLETED,
        tool::NOTEBOOK_FAILED,
        tool::MCP_INVOKED,
        tool::MCP_COMPLETED,
        tool::MCP_FAILED,
        tool::SKILL_INVOKED,
    ];
    assert_eq!(
        names.len(),
        34,
        "tool category baseline must declare exactly 34 events (40 − 6 grep/glob)"
    );
    for n in names {
        assert!(
            n.starts_with("tengu_tool_"),
            "{n} must start with tengu_tool_"
        );
    }
}

#[test]
fn bash_started_payload_uses_pii_tagged_for_command() {
    use telemetry::pii::PiiTagged;
    let p = tool::BashStartedPayload {
        invocation_id: telemetry::Verified::assert_safe("inv-1".into()),
        command: PiiTagged::assert_pii_tagged_column("echo hi".into()),
        timeout_ms: 30_000,
    };
    let json = serde_json::to_string(&p).expect("serialize");
    let _: tool::BashStartedPayload = serde_json::from_str(&json).expect("round-trip");
}
