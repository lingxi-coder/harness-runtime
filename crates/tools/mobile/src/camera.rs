//! `tool-camera` (M8-P11) — the mobile-exclusive `camera` tool.
//!
//! Routes to the `Arc<dyn CameraControl>` carried in [`BuiltinToolContext`]
//! (`ctx.camera`). On desktop that handle is `None`, so the tool reports the
//! capability is unavailable; mobile composition roots wire a native Swift /
//! Kotlin impl via `UniFFI` (P12). The Rust side is platform-agnostic.

use async_trait::async_trait;
use once_cell::sync::Lazy;
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use platform_api::camera::{CameraError, CameraPosition, CapturePhotoOpts};
use serde_json::{json, Value};

use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
    ValidationError,
};
use tool_api::BuiltinToolContext;

/// Tool name byte-lock.
pub const TOOL_NAME: &str = "camera";

/// `CameraTool` — capture a photo or pick from the photo library.
#[derive(Clone)]
pub struct CameraTool {
    ctx: BuiltinToolContext,
}

impl CameraTool {
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
            "action": { "type": "string", "enum": ["capture", "pick_from_library"] },
            "position": { "type": "string", "enum": ["front", "back"] },
            "allow_editing": { "type": "boolean" }
        },
        "required": ["action"]
    })
});

fn map_camera_err(e: &CameraError) -> ToolError {
    match e {
        CameraError::PermissionDenied => {
            ToolError::PermissionDenied("camera permission denied".into())
        }
        other => ToolError::Internal(other.to_string()),
    }
}

#[async_trait]
impl Tool for CameraTool {
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
        4096
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
                reason: "camera tool — native OS permission prompt gates capture".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    async fn description(&self, input: &Value, _: &DescriptionOptions) -> String {
        match input.get("action").and_then(Value::as_str) {
            Some("pick_from_library") => "Picking an image from the photo library".into(),
            _ => "Capturing a photo with the camera".into(),
        }
    }

    async fn prompt(&self, _: &PromptOptions) -> String {
        "Capture a photo or pick one from the device photo library.".into()
    }

    async fn validate_input(
        &self,
        input: &Value,
        _: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        match input.get("action").and_then(Value::as_str) {
            Some("capture" | "pick_from_library") => Ok(()),
            _ => Err(ValidationError(
                "`action` must be `capture` or `pick_from_library`".into(),
            )),
        }
    }

    async fn call(
        &self,
        input: Value,
        _ctx: ToolUseContext,
        _progress_tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let camera =
            self.ctx.camera.as_ref().ok_or_else(|| {
                ToolError::Internal("camera not available on this platform".into())
            })?;

        let action = input
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("capture");
        let image = if action == "pick_from_library" {
            camera
                .pick_from_library()
                .await
                .map_err(|e| map_camera_err(&e))?
        } else {
            let position = match input.get("position").and_then(Value::as_str) {
                Some("front") => CameraPosition::Front,
                _ => CameraPosition::Back,
            };
            let allow_editing = input
                .get("allow_editing")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            camera
                .capture_photo(CapturePhotoOpts {
                    position,
                    allow_editing,
                })
                .await
                .map_err(|e| map_camera_err(&e))?
        };

        Ok(ToolCallResult {
            data: json!({
                "captured": true,
                "width": image.width,
                "height": image.height,
                "jpeg_bytes_len": image.jpeg_bytes.len(),
            }),
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }
}

/// Register the `camera` tool against `reg`.
pub fn register_all(reg: &mut tool_api::ToolRegistry, ctx: tool_api::BuiltinToolContext) {
    use std::sync::Arc;
    reg.register_builtin(Arc::new(CameraTool::new(ctx)));
}
