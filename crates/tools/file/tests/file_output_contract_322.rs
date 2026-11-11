//! Executed native Zod parser/serializer regression receipts for file mutations.

use std::sync::Arc;

use lingxi_core::types::utf16_json::Utf16JsonProjection;
use tool_api::tool_trait::Tool;
use tool_file::notebook_edit::NotebookEditTool;
use tool_file::{FileEditTool, FileReadTool, FileWriteTool, GlobTool, GrepTool};

fn tools() -> Vec<Box<dyn Tool>> {
    let context = tool_api::test_support::ctx_for_file_tools(
        tool_api::test_support::make_dummy_fs(),
        Arc::new(telemetry::AnalyticsBus::new()),
        Vec::new(),
    );
    vec![
        Box::new(FileEditTool::new(context.clone())),
        Box::new(FileWriteTool::new(context.clone())),
        Box::new(NotebookEditTool::new(context.clone())),
        Box::new(GlobTool::new(context.clone())),
        Box::new(GrepTool::new(context)),
    ]
}

#[test]
fn six_input_schema_bytes_match_native_cli_provider_request() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../scripts/tests/fixtures/file_input_schema_322.json"
    ))
    .unwrap();
    let context = tool_api::test_support::ctx_for_file_tools(
        tool_api::test_support::make_dummy_fs(),
        Arc::new(telemetry::AnalyticsBus::new()),
        Vec::new(),
    );
    let mut tools = tools();
    tools.push(Box::new(FileReadTool::new(context)));
    for tool in tools {
        let expected = fixture["schemas"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["tool"] == tool.name())
            .unwrap();
        assert_eq!(
            tool.input_schema().to_json_string().unwrap(),
            expected["json"].as_str().unwrap(),
            "{} schema emitted to the provider",
            tool.name()
        );
    }
}

fn fixture() -> serde_json::Value {
    serde_json::from_str(include_str!(
        "../../../../scripts/tests/fixtures/file_output_contract_322.json"
    ))
    .unwrap()
}

#[tokio::test]
async fn mutation_outputs_match_executed_native_zod_bytes_and_aggregate_paths() {
    let fixture = fixture();
    let tools = tools();
    for case in fixture["cases"].as_array().unwrap() {
        let tool = tools
            .iter()
            .find(|tool| case["tool"] == tool.name())
            .unwrap();
        let input = Utf16JsonProjection::parse(case["inputJson"].as_str().unwrap()).unwrap();
        let result = tool.parse_output(&input).await;
        let label = format!("{} {}", tool.name(), case["name"]);
        if let Some(expected) = case["outputJson"].as_str() {
            assert_eq!(
                result.unwrap().to_json_string().unwrap(),
                expected,
                "{label}"
            );
        } else {
            assert_eq!(
                result
                    .unwrap_err()
                    .model_facing_message()
                    .string_at("")
                    .unwrap(),
                case["issuesJson"].as_str().unwrap(),
                "{label}"
            );
        }
    }
}

#[test]
fn mutation_output_schema_bytes_match_executed_native_serializer() {
    let fixture = fixture();
    for tool in tools()
        .into_iter()
        .filter(|tool| matches!(tool.name(), "Edit" | "Write" | "NotebookEdit"))
    {
        let row = fixture["schemaSerializations"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["tool"] == tool.name())
            .unwrap();
        let mut actual = tool.output_schema().unwrap().clone();
        let mut properties = actual.subprojection("/properties").unwrap();
        let extensions: &[&str] = match tool.name() {
            "Edit" => &[
                "staleRecovered",
                "contentNotInModelContext",
                "syncedSkillNext",
            ],
            "Write" => &["memdirStamped", "syncedSkillNext"],
            _ => &[],
        };
        for extension in extensions {
            properties.remove_field(extension).unwrap();
        }
        actual.set_field("properties", properties).unwrap();
        assert_eq!(
            actual.to_json_string().unwrap(),
            row["json"].as_str().unwrap(),
            "{}",
            tool.name()
        );
    }
}
