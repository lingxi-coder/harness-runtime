//! `SyntheticOutputTool` — `StructuredOutput`: return the final response as
//! structured JSON (1:1 with claude-code `SyntheticOutputTool.ts`).
//!
//! Wire identifiers locked in spec §7:
//! - Input is an open passthrough object (`z.object({}).passthrough()`): any
//!   JSON object is accepted; the per-call schema is supplied dynamically by
//!   the caller, so the base tool validates only that the input is an object.
//! - `call()` echoes the input back verbatim under `structured_output` with the
//!   model-facing data string "Structured output provided successfully".

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use once_cell::sync::Lazy;
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use serde_json::{json, Value};
use telemetry::pii::{PiiTagged, Verified};
use telemetry::sink::{AnalyticsValue, LogEventMetadata};
use telemetry::tengu::tool::{
    SYNTHETIC_OUTPUT_COMPLETED, SYNTHETIC_OUTPUT_FAILED, SYNTHETIC_OUTPUT_STARTED,
};
use telemetry::AnalyticsBus;

use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    generated_builtin_result, generated_builtin_schema, source_text_projection, DescriptionOptions,
    InterruptBehavior, PromptOptions, Tool, ToolCallResult, ToolError, ToolInputRepairMetadata,
    ToolInputRepairSchema, ToolInputSchemaProvenance, ToolStaticContext, ValidationError,
};
use tool_api::util::output_truncation::MAX_TOOL_OUTPUT_LENGTH;

/// Tool name byte-lock. Wire name `StructuredOutput` (claude-code
/// `SyntheticOutputTool.ts:20` `SYNTHETIC_OUTPUT_TOOL_NAME = 'StructuredOutput'`).
pub const SYNTHETIC_OUTPUT_TOOL_NAME: &str = "StructuredOutput";
/// Model-facing `data` string returned on success (1:1 with the TS tool
/// `SyntheticOutputTool.ts:62`).
pub const STRUCTURED_OUTPUT_SUCCESS_DATA: &str = "Structured output provided successfully";

/// `SyntheticOutputTool` — echo-input stub for parity replay.
pub struct SyntheticOutputTool {
    pub(crate) ctx: tool_api::BuiltinToolContext,
}

impl SyntheticOutputTool {
    /// Construct.
    #[must_use]
    pub fn new(ctx: tool_api::BuiltinToolContext) -> Self {
        Self { ctx }
    }
}

static SCHEMA: Lazy<Utf16JsonProjection> = Lazy::new(|| {
    // TS `inputSchema = z.object({}).passthrough()` — accept any object; the
    // real per-call schema is supplied dynamically by the caller.
    generated_builtin_schema(&json!({
        "type": "object",
        "additionalProperties": true
    }))
});

fn pii_str(s: &str) -> AnalyticsValue {
    AnalyticsValue::String(PiiTagged::assert_pii_tagged_column(s.to_string()).into_inner())
}

fn verified_str(s: &str) -> AnalyticsValue {
    AnalyticsValue::String(Verified::assert_safe(s.to_string()).into_inner())
}

async fn emit_failed(bus: &Arc<AnalyticsBus>, kind: &str, duration_ms: u64) {
    let mut md: LogEventMetadata = HashMap::new();
    md.insert("error_kind".into(), verified_str(kind));
    md.insert(
        "duration_ms".into(),
        AnalyticsValue::Int(duration_ms as i64),
    );
    bus.log_event(SYNTHETIC_OUTPUT_FAILED, md).await;
}

#[async_trait]
impl Tool for SyntheticOutputTool {
    fn name(&self) -> &str {
        SYNTHETIC_OUTPUT_TOOL_NAME
    }
    /// 2.1.206 tool-definition `searchHint` (byte-verified).
    fn search_hint(&self) -> Option<Utf16JsonProjection> {
        Some("return the final response as structured JSON".into())
    }
    fn input_schema(&self) -> &Utf16JsonProjection {
        &SCHEMA
    }
    fn input_repair_metadata(&self) -> ToolInputRepairMetadata {
        ToolInputRepairMetadata::Known {
            schema: ToolInputRepairSchema::Advertised,
            schema_provenance: ToolInputSchemaProvenance::GeneratedBuiltin,
            exempt_top_level_keys: None,
        }
    }
    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        MAX_TOOL_OUTPUT_LENGTH
    }
    fn is_concurrency_safe(&self, _: &Utf16JsonProjection) -> bool {
        true
    }
    fn is_read_only(&self, _: &Utf16JsonProjection) -> bool {
        true
    }
    fn is_destructive(&self, _: &Utf16JsonProjection) -> bool {
        false
    }
    fn is_open_world(&self, _: &Utf16JsonProjection) -> bool {
        false
    }
    fn interrupt_behavior(&self, _: &Utf16JsonProjection) -> InterruptBehavior {
        InterruptBehavior::Block
    }

    async fn check_permissions(
        &self,
        _: &Utf16JsonProjection,
        _: &ToolUseContext,
    ) -> PermissionResult {
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "StructuredOutput echoes input (replay-only)".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    async fn description(&self, _: &Utf16JsonProjection, _: &DescriptionOptions) -> String {
        // TS `description()` => 'Return structured output in the requested format'.
        "Return structured output in the requested format".into()
    }

    async fn prompt(&self, _: &PromptOptions) -> Utf16JsonProjection {
        // TS `prompt()` byte-for-byte (`SyntheticOutputTool.ts:50-52`).
        "Use this tool to return your final response in the requested structured format. You MUST \
         call this tool exactly once at the end of your response to provide the structured output."
            .into()
    }

    async fn validate_input(
        &self,
        input: &Utf16JsonProjection,
        _: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        // TS base schema `z.object({}).passthrough()` accepts ANY object; the
        // per-call schema is applied dynamically by the caller. Reject only
        // non-object input.
        if input
            .at_pointer("")
            .and_then(|value| value.object_entries())
            .is_none()
        {
            return Err(ValidationError(source_text_projection(
                "StructuredOutput: input must be a JSON object",
            )));
        }
        Ok(())
    }

    async fn call(
        &self,
        input: Utf16JsonProjection,
        _ctx: ToolUseContext,
        _progress: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let started = Instant::now();
        let bus = self.ctx.bus.clone();

        // TS base tool `z.object({}).passthrough()` accepts any object. Reject
        // non-object input so a malformed call maps to the trait's error
        // variant rather than silently echoing a scalar.
        if input
            .at_pointer("")
            .and_then(|value| value.object_entries())
            .is_none()
        {
            emit_failed(
                &bus,
                "non_object_input",
                started.elapsed().as_millis() as u64,
            )
            .await;
            return Err(invalid_tool_input(
                "StructuredOutput: input must be a JSON object",
            ));
        }

        let serialized = input.to_json_string().unwrap_or_default();
        let preview: String = serialized.chars().take(80).collect();
        let mut md: LogEventMetadata = HashMap::new();
        md.insert(
            "content_len".into(),
            AnalyticsValue::Int(serialized.chars().count() as i64),
        );
        md.insert("_PROTO_content_preview".into(), pii_str(&preview));
        bus.log_event(SYNTHETIC_OUTPUT_STARTED, md).await;

        let mut md: LogEventMetadata = HashMap::new();
        md.insert(
            "duration_ms".into(),
            AnalyticsValue::Int(started.elapsed().as_millis() as i64),
        );
        md.insert(
            "output_len".into(),
            AnalyticsValue::Int(serialized.chars().count() as i64),
        );
        bus.log_event(SYNTHETIC_OUTPUT_COMPLETED, md).await;

        // Model-facing return value is identical to the TS tool
        // (`SyntheticOutputTool.ts:59-65`): echo the input verbatim.
        let data = Utf16JsonProjection::object(vec![
            (
                "data".to_owned(),
                source_text_projection(STRUCTURED_OUTPUT_SUCCESS_DATA),
            ),
            ("structured_output".to_owned(), input),
        ])
        .expect("generated structured-output result is valid JSON");
        Ok(ToolCallResult {
            data,
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
            subagent_hooks_fired: false,
        })
    }
}

fn invalid_tool_input(message: impl Into<String>) -> ToolError {
    let message = message.into();
    ToolError::InvalidInput(tool_api::tool_trait::source_text_projection(&message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mobile_linux_api::ProcessOutput;
    use tool_api::test_support::{fresh_ctx, fresh_tx, shell_test_ctx};

    fn fixture(value: Value) -> Utf16JsonProjection {
        generated_builtin_result(&value)
    }

    fn test_value(value: &Utf16JsonProjection) -> Value {
        serde_json::from_str(&value.to_json_string().unwrap()).unwrap()
    }

    fn dummy_out() -> ProcessOutput {
        ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        }
    }

    #[test]
    fn constants_locked() {
        assert_eq!(SYNTHETIC_OUTPUT_TOOL_NAME, "StructuredOutput");
        assert_eq!(
            STRUCTURED_OUTPUT_SUCCESS_DATA,
            "Structured output provided successfully"
        );
    }

    #[test]
    fn schema_is_open_passthrough_object() {
        // TS `z.object({}).passthrough()` — no required/properties, any object.
        let tool = SyntheticOutputTool::new(shell_test_ctx(dummy_out()));
        let schema = tool.input_schema();
        let schema = test_value(schema);
        assert_eq!(schema["type"], json!("object"));
        assert_eq!(schema["additionalProperties"], json!(true));
        assert!(schema.get("required").is_none());
        assert!(schema.get("properties").is_none());
    }

    #[tokio::test]
    async fn echoes_arbitrary_object_verbatim() {
        let tool = SyntheticOutputTool::new(shell_test_ctx(dummy_out()));
        let out = tool
            .call(
                fixture(json!({ "bugs": ["a", "b"], "count": 2 })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("ok");
        let data = test_value(&out.data);
        assert_eq!(data["data"], json!(STRUCTURED_OUTPUT_SUCCESS_DATA));
        assert_eq!(
            data["structured_output"],
            json!({ "bugs": ["a", "b"], "count": 2 })
        );
    }

    #[tokio::test]
    async fn empty_object_is_accepted() {
        // `{}` is a valid passthrough object (no longer requires `content`).
        let tool = SyntheticOutputTool::new(shell_test_ctx(dummy_out()));
        tool.validate_input(&fixture(json!({})), &fresh_ctx())
            .await
            .expect("empty object validates");
        let out = tool
            .call(fixture(json!({})), fresh_ctx(), fresh_tx())
            .await
            .expect("ok");
        let data = test_value(&out.data);
        assert_eq!(data["data"], json!(STRUCTURED_OUTPUT_SUCCESS_DATA));
        assert_eq!(data["structured_output"], json!({}));
    }

    #[tokio::test]
    async fn rejects_non_object_input() {
        let tool = SyntheticOutputTool::new(shell_test_ctx(dummy_out()));
        let err = tool
            .call(fixture(json!("scalar")), fresh_ctx(), fresh_tx())
            .await
            .expect_err("scalar input must fail");
        assert!(matches!(err, ToolError::InvalidInput(_)));
        assert!(format!("{err}").contains("must be a JSON object"));
    }
}
