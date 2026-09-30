//! One-shot native location as an independently discoverable model tool.

use async_trait::async_trait;
use lingxi_core::host::location::LocationError;
use once_cell::sync::Lazy;
use permission::PermissionResult;
use serde_json::{json, Value};
use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
    ValidationError,
};
use tool_api::BuiltinToolContext;

use crate::device_support::{ask, execute, failure, invalid, parse, EmptyInput};

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

static INPUT_SCHEMA: Lazy<Value> = Lazy::new(|| {
    json!({
        "type": "object", "properties": {}, "additionalProperties": false
    })
});

#[async_trait]
impl Tool for LocationTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }
    fn input_schema(&self) -> &Value {
        &INPUT_SCHEMA
    }
    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        self.ctx.location.is_some()
    }
    fn max_result_size_chars(&self) -> usize {
        2048
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        false
    }
    fn is_read_only(&self, _: &Value) -> bool {
        true
    }
    fn search_hint(&self) -> Option<&str> {
        Some("Current device location, GPS coordinates and accuracy")
    }
    async fn check_permissions(&self, _: &Value, _: &ToolUseContext) -> PermissionResult {
        ask(
            TOOL_NAME,
            "Read the device's current location. Native location permission may also be required.",
        )
    }
    async fn description(&self, _: &Value, _: &DescriptionOptions) -> String {
        "Reading the current device location".into()
    }
    async fn prompt(&self, _: &PromptOptions) -> String {
        "Read the current device location once. Returns WGS-84 latitude, longitude, accuracy_m and timestamp_ms; does not track movement. Use only when location is needed for the user's request.".into()
    }
    async fn validate_input(
        &self,
        input: &Value,
        _: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        parse::<EmptyInput>(input).map(|_| ())
    }
    async fn call(
        &self,
        input: Value,
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
            Ok(fix) => ToolCallResult::from_data(json!({
                "latitude": fix.latitude, "longitude": fix.longitude,
                "accuracy_m": fix.accuracy_m, "timestamp_ms": fix.timestamp_ms
            })),
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
