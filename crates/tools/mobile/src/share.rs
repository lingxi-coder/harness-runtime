//! `tool-share` (M8-P11) — the mobile-exclusive `share` tool.
//!
//! Routes to `ctx.share` (`Arc<dyn SharingService>`). `None` on desktop; mobile
//! composition roots wire a native Swift / Kotlin impl via `UniFFI` (P12).

use async_trait::async_trait;
use lingxi_core::host::share::{ShareError, SharePayload, ShareResult};
use once_cell::sync::Lazy;
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use serde_json::{json, Value};

use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
    ValidationError,
};
use tool_api::BuiltinToolContext;

/// Tool name byte-lock.
pub const TOOL_NAME: &str = "share";

/// `ShareTool` — present the native share sheet with text and/or a URL.
#[derive(Clone)]
pub struct ShareTool {
    ctx: BuiltinToolContext,
}

impl ShareTool {
    /// Construct from the builtin tool context.
    #[must_use]
    pub fn new(ctx: BuiltinToolContext) -> Self {
        Self { ctx }
    }
}

static INPUT_SCHEMA: Lazy<Value> = Lazy::new(|| {
    json!({
        "type": "object",
        "properties": {
            "text": { "type": "string" },
            "url": { "type": "string" }
        }
    })
});

fn map_share_err(e: &ShareError) -> ToolError {
    ToolError::Internal(e.to_string())
}

#[async_trait]
impl Tool for ShareTool {
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
        false
    }
    fn is_read_only(&self, _: &Value) -> bool {
        false
    }

    async fn check_permissions(&self, _: &Value, _: &ToolUseContext) -> PermissionResult {
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "share tool — user confirms in the native share sheet".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    async fn description(&self, _: &Value, _: &DescriptionOptions) -> String {
        "Presenting the native share sheet".into()
    }

    async fn prompt(&self, _: &PromptOptions) -> String {
        "Share text or a URL via the device's native share sheet.".into()
    }

    async fn validate_input(
        &self,
        input: &Value,
        _: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        if input.get("text").is_none() && input.get("url").is_none() {
            return Err(ValidationError(
                "provide at least one of `text` or `url`".into(),
            ));
        }
        Ok(())
    }

    async fn call(
        &self,
        input: Value,
        _ctx: ToolUseContext,
        _progress_tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let share =
            self.ctx.share.as_ref().ok_or_else(|| {
                ToolError::Internal("sharing not available on this platform".into())
            })?;

        let payload = SharePayload {
            text: input
                .get("text")
                .and_then(Value::as_str)
                .map(str::to_string),
            image_bytes: None,
            url: input.get("url").and_then(Value::as_str).map(str::to_string),
        };
        let outcome = share.share(payload).await.map_err(|e| map_share_err(&e))?;

        Ok(ToolCallResult {
            data: json!({ "shared": matches!(outcome, ShareResult::Success) }),
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }
}

/// Register the `share` tool against `reg`.
pub fn register_all(reg: &mut tool_api::ToolRegistry, ctx: tool_api::BuiltinToolContext) {
    use std::sync::Arc;
    reg.register_builtin(Arc::new(ShareTool::new(ctx)));
}
