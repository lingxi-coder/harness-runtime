//! `tool-share` (M8-P11) — the mobile-exclusive `share` tool.
//!
//! Routes to `ctx.share` (`Arc<dyn SharingService>`). `None` on desktop; mobile
//! composition roots wire a native Swift / Kotlin impl via `UniFFI` (P12).

use async_trait::async_trait;
use lingxi_core::host::share::{ShareError, SharePayload, ShareResult};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
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

use crate::device_support::{generated_projection, generated_schema_repair};

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

static INPUT_SCHEMA: Lazy<Utf16JsonProjection> = Lazy::new(|| {
    generated_projection(&json!({
        "type": "object",
        "properties": {
            "text": { "type": "string" },
            "url": { "type": "string" }
        }
    }))
});

fn map_share_err(e: &ShareError) -> ToolError {
    ToolError::Internal(e.to_string())
}

#[async_trait]
impl Tool for ShareTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }
    fn input_schema(&self) -> &Utf16JsonProjection {
        &INPUT_SCHEMA
    }
    fn input_repair_metadata(&self) -> tool_api::tool_trait::ToolInputRepairMetadata {
        generated_schema_repair()
    }

    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        self.ctx.share.is_some()
    }
    fn max_result_size_chars(&self) -> usize {
        1024
    }
    fn is_concurrency_safe(&self, _: &Utf16JsonProjection) -> bool {
        false
    }
    fn is_read_only(&self, _: &Utf16JsonProjection) -> bool {
        false
    }

    async fn check_permissions(
        &self,
        _: &Utf16JsonProjection,
        _: &ToolUseContext,
    ) -> PermissionResult {
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "share tool — user confirms in the native share sheet".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    async fn description(&self, _: &Utf16JsonProjection, _: &DescriptionOptions) -> String {
        "Presenting the native share sheet".into()
    }

    async fn prompt(&self, _: &PromptOptions) -> Utf16JsonProjection {
        "Share text or a URL via the device's native share sheet.".into()
    }

    async fn validate_input(
        &self,
        input: &Utf16JsonProjection,
        _: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        if input.get_property("text").is_none() && input.get_property("url").is_none() {
            return Err(ValidationError(
                "provide at least one of `text` or `url`".into(),
            ));
        }
        Ok(())
    }

    async fn call(
        &self,
        input: Utf16JsonProjection,
        _ctx: ToolUseContext,
        _progress_tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let share =
            self.ctx.share.as_ref().ok_or_else(|| {
                ToolError::Internal("sharing not available on this platform".into())
            })?;

        let payload = SharePayload {
            text: input
                .get_property("text")
                .and_then(|value| value.as_str())
                .map(str::to_string),
            image_bytes: None,
            url: input
                .get_property("url")
                .and_then(|value| value.as_str())
                .map(str::to_string),
        };
        let outcome = share.share(payload).await.map_err(|e| map_share_err(&e))?;

        Ok(ToolCallResult {
            data: generated_projection(
                &json!({ "shared": matches!(outcome, ShareResult::Success) }),
            ),
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
            subagent_hooks_fired: false,
        })
    }
}

/// Register the `share` tool against `reg`.
pub fn register_all(reg: &mut tool_api::ToolRegistry, ctx: tool_api::BuiltinToolContext) {
    use std::sync::Arc;
    reg.register_builtin(Arc::new(ShareTool::new(ctx)));
}
