//! `SleepTool` — sleeps for `duration_ms` via `tokio::time::sleep`.
//!
//! Reads start/elapsed via the injected `Clock` so tests stay hermetic.

use async_trait::async_trait;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use once_cell::sync::Lazy;
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::Duration;
use telemetry::sink::{AnalyticsValue, LogEventMetadata};
use telemetry::tengu::tool::{SLEEP_COMPLETED, SLEEP_FAILED, SLEEP_STARTED};
use tool_api::BuiltinToolContext;
use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolInputRepairMetadata,
    ToolInputRepairSchema, ToolInputSchemaProvenance, ToolStaticContext, ValidationError,
    generated_builtin_result, generated_builtin_schema, source_text_projection,
};

/// 10-minute cap on the sleep duration — matches BashTool's max.
pub const SLEEP_MAX_DURATION_MS: u64 = 600_000;
/// Tool name byte-lock.
pub const TOOL_NAME: &str = "Sleep";

/// `SleepTool` — pauses execution for `duration_ms` via `tokio::time::sleep`.
#[derive(Clone)]
pub struct SleepTool {
    ctx: BuiltinToolContext,
}

impl SleepTool {
    /// Construct a fresh tool.
    #[must_use]
    pub fn new(ctx: BuiltinToolContext) -> Self {
        Self { ctx }
    }
}

static INPUT_SCHEMA: Lazy<Utf16JsonProjection> = Lazy::new(|| {
    generated_builtin_schema(&json!({
        "type": "object",
        "properties": {
            "duration_ms": { "type": "integer", "minimum": 0, "maximum": 600_000 }
        },
        "required": ["duration_ms"]
    }))
});

fn duration_ms(input: &Utf16JsonProjection) -> Option<u64> {
    input
        .number_at("/duration_ms")
        .filter(|value| value.is_finite() && *value >= 0.0 && value.fract() == 0.0)
        .filter(|value| *value <= u64::MAX as f64)
        .map(|value| value as u64)
}

#[async_trait]
impl Tool for SleepTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }
    fn input_schema(&self) -> &Utf16JsonProjection {
        &INPUT_SCHEMA
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
        1024
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

    async fn check_permissions(
        &self,
        _: &Utf16JsonProjection,
        _: &ToolUseContext,
    ) -> PermissionResult {
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "allow-all-gate (M4-02 default)".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    async fn description(&self, input: &Utf16JsonProjection, _: &DescriptionOptions) -> String {
        let d = duration_ms(input).unwrap_or(0);
        format!("Sleeping {d}ms")
    }

    async fn prompt(&self, _: &PromptOptions) -> Utf16JsonProjection {
        "Pause execution for a fixed duration.".into()
    }

    async fn validate_input(
        &self,
        input: &Utf16JsonProjection,
        _: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        let d = duration_ms(input)
            .ok_or_else(|| ValidationError(source_text_projection("missing `duration_ms`")))?;
        if d > SLEEP_MAX_DURATION_MS {
            return Err(ValidationError(source_text_projection(&format!(
                "sleep duration {d}ms exceeds limit {SLEEP_MAX_DURATION_MS}ms"
            ))));
        }
        Ok(())
    }

    async fn call(
        &self,
        input: Utf16JsonProjection,
        _ctx: ToolUseContext,
        _progress_tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let d = duration_ms(&input).ok_or_else(|| invalid_tool_input("missing duration_ms"))?;
        if d > SLEEP_MAX_DURATION_MS {
            let mut meta: LogEventMetadata = HashMap::new();
            meta.insert(
                "error_kind".into(),
                AnalyticsValue::String("validation".into()),
            );
            self.ctx.bus.log_event(SLEEP_FAILED, meta).await;
            return Err(invalid_tool_input(format!(
                "sleep duration {d}ms exceeds limit {SLEEP_MAX_DURATION_MS}ms"
            )));
        }

        let mut meta: LogEventMetadata = HashMap::new();
        meta.insert("duration_ms".into(), AnalyticsValue::Int(d as i64));
        self.ctx.bus.log_event(SLEEP_STARTED, meta).await;

        let _start = self.ctx.clock.now();
        tokio::time::sleep(Duration::from_millis(d)).await;
        // Production builds advance real wall-clock; tests use StubClock which
        // does not advance, so we don't debug_assert elapsed here (would fail
        // under StubClock).

        let mut meta: LogEventMetadata = HashMap::new();
        meta.insert("duration_ms".into(), AnalyticsValue::Int(d as i64));
        self.ctx.bus.log_event(SLEEP_COMPLETED, meta).await;

        Ok(ToolCallResult {
            data: generated_builtin_result(&json!({ "duration_ms": d, "slept": true })),
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

    fn dummy_out() -> ProcessOutput {
        ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        }
    }

    #[test]
    fn locked_constants_unchanged() {
        assert_eq!(SLEEP_MAX_DURATION_MS, 600_000);
        assert_eq!(TOOL_NAME, "Sleep");
    }

    #[tokio::test]
    async fn sleep_zero_returns_immediately() {
        let tool = SleepTool::new(shell_test_ctx(dummy_out()));
        let start = std::time::Instant::now();
        let r = tool
            .call(fixture(json!({"duration_ms": 0})), fresh_ctx(), fresh_tx())
            .await
            .expect("ok");
        assert!(start.elapsed() < Duration::from_millis(50));
        assert_eq!(
            r.data
                .at_pointer("/slept")
                .and_then(|value| value.as_bool()),
            Some(true)
        );
        assert_eq!(r.data.number_at("/duration_ms"), Some(0.0));
    }

    #[tokio::test]
    async fn sleep_rejects_overlong_duration() {
        let tool = SleepTool::new(shell_test_ctx(dummy_out()));
        let err = tool
            .call(
                fixture(json!({"duration_ms": 600_001})),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect_err("too long");
        assert!(err.to_string().contains("600000"), "got {err}");
    }

    #[tokio::test]
    async fn validate_rejects_overlong_duration() {
        let tool = SleepTool::new(shell_test_ctx(dummy_out()));
        let r = tool
            .validate_input(&fixture(json!({"duration_ms": 600_001})), &fresh_ctx())
            .await;
        assert!(r.is_err());
    }
}
