//! Source-preserving adapter for native tool input parsing.
//!
//! Tool schemas and inputs remain `Utf16JsonProjection` values at this
//! boundary. The tool's own native parser owns structural/default handling;
//! tool-local refinements are then formatted from their exact issue
//! projections. This module deliberately does not route native Zod semantics
//! through a generic JSON-Schema validator.

use lingxi_core::types::utf16_json::Utf16JsonProjection;
use tool_api::native_schema::{format_issues, format_raw_issues, NativeSchemaError};
use tool_api::Tool;

/// Run the resolved tool's native source parser and refinement issue collector.
///
/// `parse_native_input` remains authoritative for structural validation and
/// normalization. A tool that has not opted into that parser uses the shared
/// source-native flat schema gate when it can represent the declared schema;
/// unsupported schema vocabularies remain unsupported instead of being
/// converted through `serde_json::Value` to a different validator.
pub(crate) fn validate_tool_schema_detailed(
    tool: &dyn Tool,
    input: &Utf16JsonProjection,
) -> Result<(), NativeSchemaError> {
    let structural = match tool.parse_native_input(input) {
        Some(result) => result.map(|_| ()),
        None => tool_api::native_schema::validate_flat_input(
            tool.name(),
            tool.input_validation_schema(),
            input,
        )
        .unwrap_or(Ok(())),
    };
    structural?;

    let issues = tool.input_validation_issues(input);
    if issues.is_empty() {
        return Ok(());
    }
    Err(NativeSchemaError {
        display: format_issues(tool.name(), &issues),
        raw: format_raw_issues(&issues),
    })
}

/// The display diagnostic used by the model-facing rejection path.
pub(crate) fn validate_tool_schema(
    tool: &dyn Tool,
    input: &Utf16JsonProjection,
) -> Result<(), Utf16JsonProjection> {
    validate_tool_schema_detailed(tool, input).map_err(|error| error.display)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::types::utf16_json::Utf16JsonProjection;
    use serde_json::Value;
    use tool_api::context::ToolUseContext;
    use tool_api::progress::ToolProgressSender;
    use tool_api::tool_trait::{
        DescriptionOptions, PromptOptions, ToolCallResult, ToolError, ToolInputRepairMetadata,
        ToolInputRepairSchema, ToolInputSchemaProvenance, ToolStaticContext,
    };

    struct NativeCronSchema {
        name: String,
        schema: Utf16JsonProjection,
        refinement_issue: Option<Utf16JsonProjection>,
    }

    #[async_trait::async_trait]
    impl Tool for NativeCronSchema {
        fn name(&self) -> &str {
            &self.name
        }

        fn input_schema(&self) -> &Utf16JsonProjection {
            &self.schema
        }

        fn input_repair_metadata(&self) -> ToolInputRepairMetadata {
            ToolInputRepairMetadata::Known {
                schema: ToolInputRepairSchema::Advertised,
                schema_provenance: ToolInputSchemaProvenance::GeneratedBuiltin,
                exempt_top_level_keys: None,
            }
        }

        fn native_input_validation(&self) -> bool {
            true
        }

        fn input_validation_issues(&self, _: &Utf16JsonProjection) -> Vec<Utf16JsonProjection> {
            self.refinement_issue.iter().cloned().collect()
        }

        fn is_enabled(&self, _: &ToolStaticContext) -> bool {
            true
        }

        fn max_result_size_chars(&self) -> usize {
            1000
        }

        fn is_concurrency_safe(&self, _: &Utf16JsonProjection) -> bool {
            true
        }

        fn is_read_only(&self, _: &Utf16JsonProjection) -> bool {
            false
        }

        async fn description(&self, _: &Utf16JsonProjection, _: &DescriptionOptions) -> String {
            unreachable!("fixture tool is never executed")
        }

        async fn prompt(
            &self,
            _: &PromptOptions,
        ) -> lingxi_core::types::utf16_json::Utf16JsonProjection {
            unreachable!("fixture tool is never prompted")
        }

        async fn check_permissions(
            &self,
            _: &Utf16JsonProjection,
            _: &ToolUseContext,
        ) -> permission::PermissionResult {
            unreachable!("fixture tool is never permission-checked")
        }

        async fn call(
            &self,
            _: Utf16JsonProjection,
            _: ToolUseContext,
            _: ToolProgressSender,
        ) -> Result<ToolCallResult, ToolError> {
            unreachable!("fixture tool is never executed")
        }
    }

    fn source(value: &Value) -> Utf16JsonProjection {
        Utf16JsonProjection::from_serde_value(value).expect("UTF-8 fixture is valid JSON")
    }

    #[test]
    fn cron_source_schema_gate_matches_native_diagnostic_fixture() {
        let oracle: Value = serde_json::from_str(include_str!(
            "../../tools/cron/tests/fixtures/malformed_input_2_1_270.json"
        ))
        .unwrap();
        for (index, case) in oracle["cases"].as_array().unwrap().iter().enumerate() {
            let name = case["tool"].as_str().unwrap();
            let tool = NativeCronSchema {
                name: name.into(),
                schema: source(&oracle["schemas"][name]),
                refinement_issue: None,
            };
            let input = source(&case["coerced"]);
            let result = validate_tool_schema_detailed(&tool, &input);
            if case["success"] == true {
                assert!(result.is_ok(), "case {index}: {case}: {result:?}");
            } else {
                let error = result.unwrap_err();
                assert_eq!(
                    error.display.string_at(""),
                    case["display"].as_str(),
                    "display case {index}"
                );
                assert_eq!(
                    error.raw.string_at(""),
                    case["raw"].as_str(),
                    "raw case {index}"
                );
            }
        }
    }

    #[test]
    fn source_native_gate_preserves_lone_surrogate_values() {
        let schema = Utf16JsonProjection::parse(
            r#"{"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false}"#,
        )
        .unwrap();
        let tool = NativeCronSchema {
            name: "NativeCronFixture".into(),
            schema,
            refinement_issue: None,
        };
        let input = Utf16JsonProjection::parse(r#"{"path":"/tmp/\ud800.txt"}"#).unwrap();
        assert!(validate_tool_schema_detailed(&tool, &input).is_ok());
        assert_eq!(
            tool.parse_native_input(&input)
                .unwrap()
                .unwrap()
                .string_units("/path"),
            Some(vec![
                b'/' as u16,
                b't' as u16,
                b'm' as u16,
                b'p' as u16,
                b'/' as u16,
                0xd800,
                b'.' as u16,
                b't' as u16,
                b'x' as u16,
                b't' as u16,
            ]),
        );
    }

    #[test]
    fn refinements_keep_their_source_issue_json() {
        let issue = Utf16JsonProjection::parse(
            r#"{"code":"custom","path":["field"],"message":"bad \ud800 value"}"#,
        )
        .unwrap();
        let tool = NativeCronSchema {
            name: "Fixture".into(),
            schema: Utf16JsonProjection::parse(r#"{"type":"object"}"#).unwrap(),
            refinement_issue: Some(issue),
        };
        let input = Utf16JsonProjection::parse("{}").unwrap();
        let error = validate_tool_schema_detailed(&tool, &input).unwrap_err();
        assert!(error.raw.string_units("").unwrap().contains(&0xd800));
        assert!(format_raw_issues(&tool.input_validation_issues(&input))
            .string_units("")
            .unwrap()
            .contains(&0xd800));
    }
}
