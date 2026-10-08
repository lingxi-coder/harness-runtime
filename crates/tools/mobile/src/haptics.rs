//! Bounded, single-event native haptic feedback.

use async_trait::async_trait;
use lingxi_core::host::haptics::{HapticError, HapticStyle};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use once_cell::sync::Lazy;
use permission::PermissionResult;
use serde::Deserialize;
use serde_json::{Value, json};
use tool_api::BuiltinToolContext;
use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
    ValidationError,
};

use crate::device_support::{
    allow, execute, failure, generated_projection, generated_schema_repair, invalid, parse,
};

/// Canonical model-facing tool name.
pub const TOOL_NAME: &str = "haptics";

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Style {
    Light,
    Medium,
    Heavy,
    Success,
    Warning,
    Error,
}

impl Style {
    fn native(self) -> HapticStyle {
        match self {
            Self::Light => HapticStyle::Light,
            Self::Medium => HapticStyle::Medium,
            Self::Heavy => HapticStyle::Heavy,
            Self::Success => HapticStyle::Success,
            Self::Warning => HapticStyle::Warning,
            Self::Error => HapticStyle::Error,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    style: Style,
}

/// Trigger one of the host's six bounded feedback styles.
#[derive(Clone)]
pub struct HapticsTool {
    ctx: BuiltinToolContext,
}

impl HapticsTool {
    /// Construct using the native provider in the builtin context.
    #[must_use]
    pub fn new(ctx: BuiltinToolContext) -> Self {
        Self { ctx }
    }
}

static INPUT_SCHEMA: Lazy<Utf16JsonProjection> = Lazy::new(|| {
    generated_projection(&json!({
        "type": "object", "properties": {
            "style": { "type": "string", "enum": ["light", "medium", "heavy", "success", "warning", "error"] }
        }, "required": ["style"], "additionalProperties": false
    }))
});

#[async_trait]
impl Tool for HapticsTool {
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
        self.ctx.haptics.is_some()
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
    fn search_hint(&self) -> Option<Utf16JsonProjection> {
        Some("Haptic feedback, vibration, tactile success or warning".into())
    }
    async fn check_permissions(
        &self,
        _: &Utf16JsonProjection,
        _: &ToolUseContext,
    ) -> PermissionResult {
        allow("Trigger one bounded native haptic feedback event")
    }
    async fn description(&self, _: &Utf16JsonProjection, _: &DescriptionOptions) -> String {
        "Triggering device haptic feedback".into()
    }
    async fn prompt(&self, _: &PromptOptions) -> Utf16JsonProjection {
        "Trigger one brief native haptic feedback event using a supported style. Use for user-requested tactile feedback; no custom vibration durations or repeating patterns.".into()
    }
    async fn validate_input(
        &self,
        input: &Utf16JsonProjection,
        _: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        parse::<Input>(input).map(|_| ())
    }
    async fn call(
        &self,
        input: Utf16JsonProjection,
        ctx: ToolUseContext,
        _: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let request: Input = parse(&input).map_err(invalid)?;
        let Some(service) = &self.ctx.haptics else {
            return Ok(failure(
                "unavailable",
                "Haptics are unavailable on this device",
            ));
        };
        Ok(
            match execute(&ctx, service.trigger(request.style.native())).await? {
                Ok(()) => {
                    let mut result = generated_projection(&json!({ "triggered": true }));
                    let style = input
                        .subprojection("/style")
                        .map_err(|error| ToolError::Internal(error.to_string().into()))?;
                    result
                        .set_field("style", style)
                        .map_err(|error| ToolError::Internal(error.to_string().into()))?;
                    ToolCallResult::from_data(result)
                }
                Err(error) => failure(
                    match &error {
                        HapticError::Unavailable => "unavailable",
                        HapticError::Other(_) => "native_error",
                    },
                    error,
                ),
            },
        )
    }
}
