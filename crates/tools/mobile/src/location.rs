//! One-shot native location as an independently discoverable model tool.

use async_trait::async_trait;
use lingxi_core::host::location::LocationError;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use once_cell::sync::Lazy;
use permission::PermissionResult;
use serde_json::json;
use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
    ValidationError,
};
use tool_api::BuiltinToolContext;

use crate::device_support::{
    ask, execute, failure, generated_projection, generated_schema_repair, invalid, parse,
    EmptyInput,
};

/// Canonical model-facing tool name.
pub const TOOL_NAME: &str = "location";

/// Resolve one location fix through the host's native provider.
#[derive(Clone)]
pub struct LocationTool {
    ctx: BuiltinToolContext,
}

impl LocationTool {
    /// Construct using the native provider in the builtin context.
    #[must_use]
    pub fn new(ctx: BuiltinToolContext) -> Self {
        Self { ctx }
    }
}

static INPUT_SCHEMA: Lazy<Utf16JsonProjection> = Lazy::new(|| {
    generated_projection(&json!({
        "type": "object", "properties": {}, "additionalProperties": false
    }))
});

#[async_trait]
impl Tool for LocationTool {
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
        self.ctx.location.is_some()
    }
    fn max_result_size_chars(&self) -> usize {
        2048
    }
    fn is_concurrency_safe(&self, _: &Utf16JsonProjection) -> bool {
        false
    }
    fn is_read_only(&self, _: &Utf16JsonProjection) -> bool {
        true
    }
    fn search_hint(&self) -> Option<Utf16JsonProjection> {
        Some("Current device location, GPS coordinates and accuracy".into())
    }
    async fn check_permissions(
        &self,
        _: &Utf16JsonProjection,
        _: &ToolUseContext,
    ) -> PermissionResult {
        ask(
            TOOL_NAME,
            "Read the device's current location. Native location permission may also be required.",
        )
    }
    async fn description(&self, _: &Utf16JsonProjection, _: &DescriptionOptions) -> String {
        "Reading the current device location".into()
    }
    async fn prompt(&self, _: &PromptOptions) -> Utf16JsonProjection {
        "Read the current device location once. Returns WGS-84 latitude, longitude, accuracy_m and timestamp_ms; does not track movement. Use only when location is needed for the user's request.".into()
    }
    async fn validate_input(
        &self,
        input: &Utf16JsonProjection,
        _: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        parse::<EmptyInput>(input).map(|_| ())
    }
    async fn call(
        &self,
        input: Utf16JsonProjection,
        ctx: ToolUseContext,
        _: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        parse::<EmptyInput>(&input).map_err(invalid)?;
        let Some(provider) = &self.ctx.location else {
            return Ok(failure(
                "unavailable",
                "Location is unavailable on this device",
            ));
        };
        Ok(match execute(&ctx, provider.current_location()).await? {
            Ok(fix) => ToolCallResult::from_data(generated_projection(&json!({
                "latitude": fix.latitude, "longitude": fix.longitude,
                "accuracy_m": fix.accuracy_m, "timestamp_ms": fix.timestamp_ms
            }))),
            Err(error) => {
                let code = match &error {
                    LocationError::PermissionDenied => "permission_denied",
                    LocationError::Unavailable => "unavailable",
                    LocationError::Timeout => "timeout",
                    LocationError::Other(_) => "native_error",
                };
                failure(code, error)
            }
        })
    }
}
