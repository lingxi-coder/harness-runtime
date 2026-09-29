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
    DescriptionOptions, InterruptBehavior, PromptOptions, Tool, ToolCallResult, ToolError,
    ToolStaticContext, ValidationError,
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

static SCHEMA: Lazy<Value> = Lazy::new(|| {
    // TS `inputSchema = z.object({}).passthrough()` — accept any object; the
    // real per-call schema is supplied dynamically by the caller.
    json!({
        "type": "object",
        "additionalProperties": true
    })
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
    fn search_hint(&self) -> Option<&str> {
        Some("return the final response as structured JSON")
    }
    fn input_schema(&self) -> &Value {
        &SCHEMA
    }
    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        MAX_TOOL_OUTPUT_LENGTH
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }
    fn is_read_only(&self, _: &Value) -> bool {
        true
    }
    fn is_destructive(&self, _: &Value) -> bool {
        false
    }
    fn is_open_world(&self, _: &Value) -> bool {
        false
    }
    fn interrupt_behavior(&self, _: &Value) -> InterruptBehavior {
        InterruptBehavior::Block
    }

    async fn check_permissions(&self, _: &Value, _: &ToolUseContext) -> PermissionResult {
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "StructuredOutput echoes input (replay-only)".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    async fn description(&self, _: &Value, _: &DescriptionOptions) -> String {
        // TS `description()` => 'Return structured output in the requested format'.
        "Return structured output in the requested format".into()
    }

    async fn prompt(&self, _: &PromptOptions) -> String {
        // TS `prompt()` byte-for-byte (`SyntheticOutputTool.ts:50-52`).
        "Use this tool to return your final response in the requested structured format. You MUST \
         call this tool exactly once at the end of your response to provide the structured output."
            .into()
    }

    async fn validate_input(
        &self,
        input: &Value,
        _: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        // TS base schema `z.object({}).passthrough()` accepts ANY object; the
        // per-call schema is applied dynamically by the caller. Reject only
        // non-object input.
        if !input.is_object() {
            return Err(ValidationError(
                "StructuredOutput: input must be a JSON object".into(),
            ));
        }
        Ok(())
    }

    async fn call(
        &self,
        input: Value,
        _ctx: ToolUseContext,
        _progress: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let started = Instant::now();
        let bus = self.ctx.bus.clone();

        // TS base tool `z.object({}).passthrough()` accepts any object. Reject
        // non-object input so a malformed call maps to the trait's error
        // variant rather than silently echoing a scalar.
        if !input.is_object() {
            emit_failed(
                &bus,
                "non_object_input",
                started.elapsed().as_millis() as u64,
            )
            .await;
            return Err(ToolError::InvalidInput(
                "StructuredOutput: input must be a JSON object".into(),
            ));
        }

        let serialized = serde_json::to_string(&input).unwrap_or_default();
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
        Ok(ToolCallResult {
            data: json!({
                "data": STRUCTURED_OUTPUT_SUCCESS_DATA,
                "structured_output": input,
            }),
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::process::ProcessOutput;
    use tool_api::test_support::{fresh_ctx, fresh_tx, shell_test_ctx};

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
                json!({ "bugs": ["a", "b"], "count": 2 }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("ok");
        assert_eq!(out.data["data"], json!(STRUCTURED_OUTPUT_SUCCESS_DATA));
        assert_eq!(
            out.data["structured_output"],
            json!({ "bugs": ["a", "b"], "count": 2 })
        );
    }

    #[tokio::test]
    async fn empty_object_is_accepted() {
        // `{}` is a valid passthrough object (no longer requires `content`).
        let tool = SyntheticOutputTool::new(shell_test_ctx(dummy_out()));
        tool.validate_input(&json!({}), &fresh_ctx())
            .await
            .expect("empty object validates");
        let out = tool
            .call(json!({}), fresh_ctx(), fresh_tx())
            .await
            .expect("ok");
        assert_eq!(out.data["data"], json!(STRUCTURED_OUTPUT_SUCCESS_DATA));
        assert_eq!(out.data["structured_output"], json!({}));
    }

    #[tokio::test]
    async fn rejects_non_object_input() {
        let tool = SyntheticOutputTool::new(shell_test_ctx(dummy_out()));
        let err = tool
            .call(json!("scalar"), fresh_ctx(), fresh_tx())
            .await
            .expect_err("scalar input must fail");
        assert!(matches!(err, ToolError::InvalidInput(_)));
        assert!(format!("{err}").contains("must be a JSON object"));
    }
}
