//! `SleepTool` — sleeps for `duration_ms` via `tokio::time::sleep`.
//!
//! Reads start/elapsed via the injected `Clock` so tests stay hermetic.

use async_trait::async_trait;
use once_cell::sync::Lazy;
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::Duration;
use telemetry::sink::{AnalyticsValue, LogEventMetadata};
use telemetry::tengu::tool::{SLEEP_COMPLETED, SLEEP_FAILED, SLEEP_STARTED};
use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
    ValidationError,
};
use tool_api::BuiltinToolContext;

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

static INPUT_SCHEMA: Lazy<Value> = Lazy::new(|| {
    json!({
        "type": "object",
        "properties": {
            "duration_ms": { "type": "integer", "minimum": 0, "maximum": 600_000 }
        },
        "required": ["duration_ms"]
    })
});

#[async_trait]
impl Tool for SleepTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }
    fn input_schema(&self) -> &Value {
        &INPUT_SCHEMA
    }
    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        1024
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

    async fn check_permissions(&self, _: &Value, _: &ToolUseContext) -> PermissionResult {
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "allow-all-gate (M4-02 default)".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    async fn description(&self, input: &Value, _: &DescriptionOptions) -> String {
        let d = input
            .get("duration_ms")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        format!("Sleeping {d}ms")
    }

    async fn prompt(&self, _: &PromptOptions) -> String {
        "Pause execution for a fixed duration.".into()
    }

    async fn validate_input(
        &self,
        input: &Value,
        _: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        let d = input
            .get("duration_ms")
            .and_then(Value::as_u64)
            .ok_or_else(|| ValidationError("missing `duration_ms`".into()))?;
        if d > SLEEP_MAX_DURATION_MS {
            return Err(ValidationError(format!(
                "sleep duration {d}ms exceeds limit {SLEEP_MAX_DURATION_MS}ms"
            )));
        }
        Ok(())
    }

    async fn call(
        &self,
        input: Value,
        _ctx: ToolUseContext,
        _progress_tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let d = input
            .get("duration_ms")
            .and_then(Value::as_u64)
            .ok_or_else(|| ToolError::InvalidInput("missing duration_ms".into()))?;
        if d > SLEEP_MAX_DURATION_MS {
            let mut meta: LogEventMetadata = HashMap::new();
            meta.insert(
                "error_kind".into(),
                AnalyticsValue::String("validation".into()),
            );
            self.ctx.bus.log_event(SLEEP_FAILED, meta).await;
            return Err(ToolError::InvalidInput(format!(
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
            data: json!({ "duration_ms": d, "slept": true }),
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
    fn locked_constants_unchanged() {
        assert_eq!(SLEEP_MAX_DURATION_MS, 600_000);
        assert_eq!(TOOL_NAME, "Sleep");
    }

    #[tokio::test]
    async fn sleep_zero_returns_immediately() {
        let tool = SleepTool::new(shell_test_ctx(dummy_out()));
        let start = std::time::Instant::now();
        let r = tool
            .call(json!({"duration_ms": 0}), fresh_ctx(), fresh_tx())
            .await
            .expect("ok");
        assert!(start.elapsed() < Duration::from_millis(50));
        assert_eq!(r.data["slept"], true);
        assert_eq!(r.data["duration_ms"], 0);
    }

    #[tokio::test]
    async fn sleep_rejects_overlong_duration() {
        let tool = SleepTool::new(shell_test_ctx(dummy_out()));
        let err = tool
            .call(json!({"duration_ms": 600_001}), fresh_ctx(), fresh_tx())
            .await
            .expect_err("too long");
        assert!(err.to_string().contains("600000"), "got {err}");
    }

    #[tokio::test]
    async fn validate_rejects_overlong_duration() {
        let tool = SleepTool::new(shell_test_ctx(dummy_out()));
        let r = tool
            .validate_input(&json!({"duration_ms": 600_001}), &fresh_ctx())
            .await;
        assert!(r.is_err());
    }
}
