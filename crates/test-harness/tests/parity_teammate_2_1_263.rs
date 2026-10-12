//! Current Agent schema surface after the 2.1.295 tool-schema projection (`meo` in `src_194447854.js`).

use mobile_linux_api::ProcessOutput;

use tool_api::tool_trait::Tool;

#[test]
fn agent_schema_uses_current_published_fields() {
    let ctx = tool_api::test_support::shell_test_ctx(ProcessOutput {
        stdout: String::new(),
        stderr: String::new(),
        exit_code: 0,
        timed_out: false,
    });
    let tool = tool_agent::agent::AgentTool::new(ctx);
    let schema = tool.input_schema();

    assert_eq!(schema.string_at("/type"), Some("object"));
    assert_eq!(schema.array_len("/required"), Some(2));
    assert_eq!(schema.string_at("/required/0"), Some("description"));
    assert_eq!(schema.string_at("/required/1"), Some("prompt"));
    for field in [
        "description",
        "prompt",
        "subagent_type",
        "name",
        "isolation",
    ] {
        assert!(
            schema.at_pointer(&format!("/properties/{field}")).is_some(),
            "current Agent schema publishes {field}"
        );
    }
    assert_eq!(
        schema.string_at("/properties/isolation/enum/0"),
        Some("worktree")
    );
    assert_eq!(
        schema.string_at("/properties/isolation/enum/1"),
        Some("remote")
    );

    // The current model-facing projection excludes old team/mode fields and
    // the internal cwd override.
    for obsolete in ["mode", "team_name", "cwd"] {
        assert!(
            schema
                .at_pointer(&format!("/properties/{obsolete}"))
                .is_none(),
            "obsolete Agent field {obsolete} is not model-facing"
        );
    }
}
