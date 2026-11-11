use std::sync::Arc;

use lingxi_core::types::utf16_json::Utf16JsonProjection;
use tool_api::tool_trait::{DescriptionOptions, PromptOptions, Tool};
use tool_file::notebook_edit::NotebookEditTool;
use tool_file::{FileEditTool, FileWriteTool, GlobTool, GrepTool};

fn fixture() -> serde_json::Value {
    serde_json::from_str(include_str!(
        "../../../../scripts/tests/fixtures/file_tool_contract_322.json"
    ))
    .unwrap()
}
fn tools() -> Vec<Box<dyn Tool>> {
    let ctx = tool_api::test_support::ctx_for_file_tools(
        tool_api::test_support::make_dummy_fs(),
        Arc::new(telemetry::AnalyticsBus::new()),
        Vec::new(),
    );
    vec![
        Box::new(FileEditTool::new(ctx.clone())),
        Box::new(FileWriteTool::new(ctx.clone())),
        Box::new(GlobTool::new(ctx.clone())),
        Box::new(GrepTool::new(ctx.clone())),
        Box::new(NotebookEditTool::new(ctx)),
    ]
}
fn value(projection: &Utf16JsonProjection) -> serde_json::Value {
    serde_json::from_str(&projection.to_json_string().unwrap()).unwrap()
}
fn clean_metadata(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(fields) => {
            fields.remove("catchUndefinedFields");
            fields.values_mut().for_each(clean_metadata);
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(clean_metadata),
        _ => {}
    }
}

#[test]
fn latest_native_factory_metadata_matches_production_schemas() {
    let fixture = fixture();
    for tool in tools() {
        let reference = fixture["schemas"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["tool"] == tool.name())
            .unwrap();
        let mut input = reference["input"].clone();
        clean_metadata(&mut input);
        assert_eq!(value(tool.input_schema()), input, "{} input", tool.name());
        let mut expected = reference["output"].clone();
        clean_metadata(&mut expected);
        let mut actual = value(
            tool.output_schema()
                .expect("file outputs expose their native shape"),
        );
        let extension_names: &[&str] = match tool.name() {
            "Edit" => &[
                "staleRecovered",
                "contentNotInModelContext",
                "syncedSkillNext",
            ],
            "Write" => &["memdirStamped", "syncedSkillNext"],
            _ => &[],
        };
        for name in extension_names {
            actual["properties"].as_object_mut().unwrap().remove(*name);
        }
        assert_eq!(actual, expected, "{} output", tool.name());
    }
}

struct PromptEnvironment(Vec<(&'static str, Option<String>)>);
impl Drop for PromptEnvironment {
    fn drop(&mut self) {
        for (key, value) in &self.0 {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
        lingxi_core::host::live_sessions::set_subagent_steer("default");
    }
}

#[tokio::test]
async fn latest_native_prompt_and_description_bytes_match_dispatched_tools() {
    let keys = [
        "LINGXI_MODEL_CAPABILITIES",
        "CLAUDE_CODE_MODEL_CAPABILITIES",
        "CLAUDE_CODE_SIMPLE_SYSTEM_PROMPT",
        "LINGXI_SIMPLE_SYSTEM_PROMPT",
    ];
    let _environment = PromptEnvironment(
        keys.iter()
            .map(|key| (*key, std::env::var(key).ok()))
            .collect(),
    );
    for key in keys {
        std::env::remove_var(key);
    }
    let fixture = fixture();
    let tools = tools();
    for row in fixture["cases"].as_array().unwrap() {
        lingxi_core::host::live_sessions::set_subagent_steer(row["style"].as_str().unwrap());
        let tool = tools
            .iter()
            .find(|tool| row["tool"] == tool.name())
            .unwrap();
        let model = if row["lean"] == true {
            "claude-opus-5-5"
        } else {
            "claude-sonnet-4-6"
        };
        let result = tool
            .prompt(&PromptOptions {
                model: Some(model.into()),
                ..Default::default()
            })
            .await;
        let expected = row["output"]
            .as_str()
            .unwrap()
            .replace("as a Bash command", "through a registered shell tool")
            .replace("via Bash", "via a registered shell tool");
        assert_eq!(
            result.string_at(""),
            Some(expected.as_str()),
            "{} lean={} style={}",
            tool.name(),
            row["lean"],
            row["style"]
        );
    }
    lingxi_core::host::live_sessions::set_subagent_steer("default");
    for row in fixture["descriptions"].as_array().unwrap() {
        let tool = tools
            .iter()
            .find(|tool| row["tool"] == tool.name())
            .unwrap();
        let description = tool
            .description(
                &Utf16JsonProjection::parse("{}").unwrap(),
                &DescriptionOptions {
                    is_non_interactive_session: false,
                },
            )
            .await;
        let expected = row["output"]
            .as_str()
            .unwrap()
            .replace("as a Bash command", "through a registered shell tool");
        assert_eq!(description, expected, "{} description", tool.name());
    }
    assert!(fixture["comparisonTo319"]
        .as_array()
        .unwrap()
        .iter()
        .all(|row| row["equal"] == true));
}

#[tokio::test]
async fn production_mutation_output_parsers_validate_and_strip_native_shapes() {
    for tool in tools()
        .into_iter()
        .filter(|tool| matches!(tool.name(), "Edit" | "Write" | "NotebookEdit"))
    {
        let raw = match tool.name() {
            "Edit" => {
                r#"{"unknown":1,"filePath":"/a","oldString":"a","newString":"b","originalFile":"a","structuredPatch":[{"unknown":1,"oldStart":1,"oldLines":1,"newStart":1,"newLines":1,"lines":["-a","+b"]}],"userModified":false,"replaceAll":false,"staleRecovered":true,"stagedWording":999}"#
            }
            "Write" => {
                r#"{"unknown":1,"type":"update","filePath":"/a","content":"b","originalFile":"a","structuredPatch":[],"userModified":false,"memdirStamped":true,"stagedWording":"review"}"#
            }
            _ => {
                r#"{"unknown":1,"new_source":"b","cell_id":"x","cell_type":"code","language":"python","edit_mode":"replace","notebook_path":"/a.ipynb","original_file":"{}","updated_file":"{}"}"#
            }
        };
        let parsed = tool
            .parse_output(&Utf16JsonProjection::parse(raw).unwrap())
            .await
            .unwrap();
        assert!(!parsed.contains_property("unknown"));
        match tool.name() {
            "Edit" => {
                assert!(!parsed.contains_property("stagedWording"));
                assert!(!parsed
                    .subprojection("/structuredPatch/0")
                    .unwrap()
                    .contains_property("unknown"));
                assert_eq!(
                    parsed
                        .at_pointer("/staleRecovered")
                        .and_then(|value| value.as_bool()),
                    Some(true)
                );
            }
            "Write" => assert_eq!(
                parsed
                    .at_pointer("/memdirStamped")
                    .and_then(|value| value.as_bool()),
                Some(true)
            ),
            _ => assert_eq!(parsed.string_at("/cell_id"), Some("x")),
        }
        assert!(tool
            .parse_output(&Utf16JsonProjection::parse("{}").unwrap())
            .await
            .is_err());
    }
}
