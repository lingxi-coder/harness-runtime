//! The `StructuredOutput` tool for `--json-schema` structured output.
//!
//! The caller compiles Native's Draft-07 schema before sending the first
//! provider request. This tool validates the model's source projection with that
//! plan, captures only accepted output, and returns the canonical acknowledgement.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use lingxi_core::host::json_schema_validator::{
    CompiledJsonSchema, JsonSchemaValidation, SchemaWorkerError,
};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use tool_api::tool_trait::{
    source_text_projection, DescriptionOptions, PromptOptions, ToolInputRepairMetadata,
    ToolInputRepairSchema, ToolInputSchemaProvenance, ToolStaticContext,
};
use tool_api::{Tool, ToolCallResult, ToolError, ToolProgressSender, ToolUseContext};

/// Canonical name of the synthetic structured-output tool.
pub const STRUCTURED_OUTPUT_TOOL_NAME: &str = "StructuredOutput";

/// Shared slot the [`StructuredOutputTool`] writes after schema validation.
pub type StructuredOutputSlot = Arc<Mutex<Option<Utf16JsonProjection>>>;

/// The forced `StructuredOutput` tool, backed by the plan compiled before the
/// model request.
pub struct StructuredOutputTool {
    /// The exact user-supplied JSON schema advertised to the model.
    schema: Utf16JsonProjection,
    /// Native Draft-07 plan for this exact schema.
    compiled: Arc<dyn CompiledJsonSchema>,
    /// Where validated model arguments are captured for the caller.
    captured: StructuredOutputSlot,
}

impl StructuredOutputTool {
    /// Build the tool with the plan compiled for `schema` and its capture slot.
    #[must_use]
    pub fn new(
        schema: Utf16JsonProjection,
        compiled: Arc<dyn CompiledJsonSchema>,
        slot: StructuredOutputSlot,
    ) -> Self {
        Self {
            schema,
            compiled,
            captured: slot,
        }
    }
}

#[async_trait]
impl Tool for StructuredOutputTool {
    fn name(&self) -> &str {
        STRUCTURED_OUTPUT_TOOL_NAME
    }

    /// 2.1.206 tool-definition `searchHint` (byte-verified).
    fn search_hint(&self) -> Option<Utf16JsonProjection> {
        Some("return the final response as structured JSON".into())
    }

    fn input_schema(&self) -> &Utf16JsonProjection {
        &self.schema
    }

    fn input_validation_schema(&self) -> &Utf16JsonProjection {
        static BASE: OnceLock<Utf16JsonProjection> = OnceLock::new();
        BASE.get_or_init(|| {
            Utf16JsonProjection::parse(r#"{"type":"object"}"#)
                .expect("static StructuredOutput parser schema is valid")
        })
    }

    fn input_repair_metadata(&self) -> ToolInputRepairMetadata {
        ToolInputRepairMetadata::Known {
            schema: ToolInputRepairSchema::Advertised,
            schema_provenance: ToolInputSchemaProvenance::SourceNative,
            exempt_top_level_keys: Some(HashSet::new()),
        }
    }

    fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
        true
    }

    fn max_result_size_chars(&self) -> usize {
        // Claude sets maxResultSizeChars to 100,000 for this tool.
        100_000
    }

    fn is_concurrency_safe(&self, _input: &Utf16JsonProjection) -> bool {
        true
    }

    fn is_read_only(&self, _input: &Utf16JsonProjection) -> bool {
        true
    }

    fn result_ends_turn(&self, _result: &ToolCallResult) -> bool {
        true
    }

    async fn check_permissions(
        &self,
        _input: &Utf16JsonProjection,
        _ctx: &ToolUseContext,
    ) -> permission::PermissionResult {
        permission::PermissionResult::Allow {
            reason: permission::PermissionDecisionReason::Other {
                reason: "structured output".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: permission::result::PermissionMetadata::default(),
        }
    }

    async fn description(
        &self,
        _input: &Utf16JsonProjection,
        _opts: &DescriptionOptions,
    ) -> String {
        "Return structured output in the requested format".to_string()
    }

    async fn prompt(
        &self,
        _opts: &PromptOptions,
    ) -> lingxi_core::types::utf16_json::Utf16JsonProjection {
        "Use this tool to return your final response in the requested structured format. You MUST call this tool exactly once at the end of your response to provide the structured output.".to_string().into()
    }

    async fn call(
        &self,
        input: Utf16JsonProjection,
        _ctx: ToolUseContext,
        _progress_tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        match self
            .compiled
            .validate(&input)
            .await
            .map_err(worker_failure)?
        {
            JsonSchemaValidation::Valid => {
                if let Ok(mut slot) = self.captured.lock() {
                    *slot = Some(input);
                }
            }
            JsonSchemaValidation::Invalid { detail, .. } => {
                return Err(ToolError::InvalidInput(detail));
            }
        }

        Ok(ToolCallResult::from_data(source_text_projection(
            "Structured output provided successfully",
        )))
    }
}

fn worker_failure(error: SchemaWorkerError) -> ToolError {
    ToolError::Internal(format!("Structured output validator failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::json_schema_validator::SchemaWorkerError;
    use std::sync::Mutex as StdMutex;

    struct FixedPlan {
        result: JsonSchemaValidation,
        seen: Arc<StdMutex<Option<Utf16JsonProjection>>>,
    }

    #[async_trait]
    impl CompiledJsonSchema for FixedPlan {
        async fn validate(
            &self,
            input: &Utf16JsonProjection,
        ) -> Result<JsonSchemaValidation, SchemaWorkerError> {
            *self.seen.lock().unwrap() = Some(input.clone());
            Ok(self.result.clone())
        }
    }

    fn plan(
        result: JsonSchemaValidation,
    ) -> (
        Arc<dyn CompiledJsonSchema>,
        Arc<StdMutex<Option<Utf16JsonProjection>>>,
    ) {
        let seen = Arc::new(StdMutex::new(None));
        (
            Arc::new(FixedPlan {
                result,
                seen: seen.clone(),
            }),
            seen,
        )
    }

    fn parse(source: &str) -> Utf16JsonProjection {
        Utf16JsonProjection::parse(source).expect("valid source JSON fixture")
    }

    #[tokio::test]
    async fn successful_call_validates_and_captures_the_exact_source_projection() {
        let schema = parse(r#"{"type":"object"}"#);
        let input = parse(r#"{"answer":"\ud800"}"#);
        let (compiled, seen) = plan(JsonSchemaValidation::Valid);
        let slot = Arc::new(Mutex::new(None));
        let tool = StructuredOutputTool::new(schema, compiled, slot.clone());
        let (tx, _) = tool_api::progress::progress_channel();
        let result = tool
            .call(input.clone(), ToolUseContext::model_seed("test".into()), tx)
            .await
            .expect("valid output is accepted");

        assert_eq!(seen.lock().unwrap().as_ref(), Some(&input));
        assert_eq!(slot.lock().unwrap().as_ref(), Some(&input));
        assert_eq!(
            result.data.string_at(""),
            Some("Structured output provided successfully")
        );
        assert!(tool.result_ends_turn(&result));
    }

    #[tokio::test]
    async fn invalid_call_returns_exact_validator_detail_without_replacing_capture() {
        let schema = parse(r#"{"type":"object"}"#);
        let previous = parse(r#"{"answer":42}"#);
        let detail = parse(r#""invalid \ud800 output""#);
        let (compiled, _) = plan(JsonSchemaValidation::Invalid {
            detail: detail.clone(),
            keywords: vec!["required".into()],
        });
        let slot = Arc::new(Mutex::new(Some(previous.clone())));
        let tool = StructuredOutputTool::new(schema, compiled, slot.clone());
        let (tx, _) = tool_api::progress::progress_channel();
        let error = tool
            .call(
                parse(r#"{}"#),
                ToolUseContext::model_seed("test".into()),
                tx,
            )
            .await
            .unwrap_err();

        assert_eq!(error.model_facing_message(), detail);
        assert_eq!(slot.lock().unwrap().as_ref(), Some(&previous));
    }
}
