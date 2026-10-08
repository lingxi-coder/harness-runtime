//! `tool-camera` (M8-P11) — the mobile-exclusive `camera` tool.
//!
//! Routes to the `Arc<dyn CameraControl>` carried in [`BuiltinToolContext`]
//! (`ctx.camera`). On desktop that handle is `None`, so the tool reports the
//! capability is unavailable; mobile composition roots wire a native Swift /
//! Kotlin impl via `UniFFI` (P12). The Rust side is platform-agnostic.

use async_trait::async_trait;
use lingxi_core::host::camera::{CameraError, CameraPosition, CapturePhotoOpts};
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
use tool_api::util::image_budget::{process_image, IMAGE_MAX_DIM};
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
        self.ctx.camera.is_some()
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
        "Capture a photo or let the user pick one from the device photo library. The image is attached for visual analysis. A cancelled picker returns cancelled=true without an image.".into()
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
        let captured = if action == "pick_from_library" {
            camera.pick_from_library_sized(IMAGE_MAX_DIM, 0.8).await
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
                .capture_photo_sized(
                    CapturePhotoOpts {
                        position,
                        allow_editing,
                    },
                    IMAGE_MAX_DIM,
                    0.8,
                )
                .await
        };
        let image = match captured {
            Ok(image) => image,
            Err(CameraError::Cancelled) => {
                return Ok(ToolCallResult::from_data(json!({
                    "captured": false,
                    "cancelled": true,
                })));
            }
            Err(error) => return Err(map_camera_err(&error)),
        };

        let original_size = image.jpeg_bytes.len();
        // Native sizing keeps the transfer small; the shared processor also
        // validates bytes and enforces the model image budget for backends
        // whose default sized implementation returns the original image.
        let processed = process_image(image.jpeg_bytes).map_err(ToolError::Internal)?;
        let (width, height) = processed.dimensions;
        let mut file = json!({
            "base64": processed.base64,
            "type": processed.media_type,
            "originalSize": original_size,
        });
        if let Some((original_width, original_height, display_width, display_height)) =
            processed.resized
        {
            file["dimensions"] = json!({
                "originalWidth": original_width,
                "originalHeight": original_height,
                "displayWidth": display_width,
                "displayHeight": display_height,
            });
        }

        Ok(ToolCallResult { mcp_meta_projection: None, model_content_projection: None, data_projection: None,
            data: json!({
                "type": "image",
                "file": file,
                "captured": true,
                "cancelled": false,
                "width": width,
                "height": height,
                "jpeg_bytes_len": original_size,
            }),
            // Both main and subagent dispatch convert the image data shape
            // into a real image block; avoid duplicating base64 as model text.
            model_content: Some("[Camera image attached for visual analysis.]".into()),
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

#[cfg(test)]
#[path = "camera_tests.rs"]
mod tests;
