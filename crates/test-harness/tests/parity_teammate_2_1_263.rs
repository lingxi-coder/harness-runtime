//! Finite Agent schema contract captured from the real 2.1.263 executable.
//! Source excerpts in the fixture document additional, not yet asserted surfaces.

use platform_api::ProcessOutput;
use serde_json::Value;
use test_harness::parity::load_fixture;
use tool_api::tool_trait::Tool;

#[test]
fn agent_schema_descriptions_match_2_1_263_oracle_bytes() {
    let fixture: Value = load_fixture("teammate_2_1_263");
    let ctx = tool_api::test_support::shell_test_ctx(ProcessOutput {
        stdout: String::new(),
        stderr: String::new(),
        exit_code: 0,
        timed_out: false,
    });
    let tool = tool_agent::agent::AgentTool::new(ctx);
    let schema = tool.input_schema();
    for (field, expected) in fixture["agent_descriptions"]
        .as_object()
        .expect("oracle descriptions object")
    {
        let actual = schema["properties"][field]["description"]
            .as_str()
            .expect("published schema description");
        assert_eq!(
            actual.as_bytes(),
            expected.as_str().expect("oracle literal").as_bytes(),
            "Agent.{field} description differs from 2.1.263"
        );
    }
}
