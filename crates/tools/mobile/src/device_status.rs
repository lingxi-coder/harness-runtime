//! Non-sensitive native device status for the model.

use async_trait::async_trait;
use lingxi_core::host::device_status::DeviceStatusError;
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
    allow, execute, failure, generated_projection, generated_schema_repair, invalid, parse,
    EmptyInput,
};

/// Canonical model-facing tool name.
pub const TOOL_NAME: &str = "device_status";

/// Read the host's bounded battery and connectivity snapshot.
#[derive(Clone)]
pub struct DeviceStatusTool {
    ctx: BuiltinToolContext,
}

impl DeviceStatusTool {
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
impl Tool for DeviceStatusTool {
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
        self.ctx.device_status.is_some()
    }
    fn max_result_size_chars(&self) -> usize {
        2048
    }
    fn is_concurrency_safe(&self, _: &Utf16JsonProjection) -> bool {
        true
    }
    fn is_read_only(&self, _: &Utf16JsonProjection) -> bool {
        true
    }
    fn search_hint(&self) -> Option<Utf16JsonProjection> {
        Some("Device battery level, charging, low power mode, network status".into())
    }
    async fn check_permissions(
        &self,
        _: &Utf16JsonProjection,
        _: &ToolUseContext,
    ) -> PermissionResult {
        allow("Read non-sensitive device battery and coarse connectivity status")
    }
    async fn description(&self, _: &Utf16JsonProjection, _: &DescriptionOptions) -> String {
        "Reading device battery and connectivity status".into()
    }
    async fn prompt(&self, _: &PromptOptions) -> Utf16JsonProjection {
        "Read device battery_percent, charging, network and low_power_mode. Unknown values are null; no device or network identifiers are exposed.".into()
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
        let Some(provider) = &self.ctx.device_status else {
            return Ok(failure("unavailable", "Device status is unavailable"));
        };
        Ok(match execute(&ctx, provider.status()).await? {
            Ok(status) => ToolCallResult::from_data(generated_projection(&json!({
                "battery_percent": status.battery_percent, "charging": status.charging,
                "network": status.network, "low_power_mode": status.low_power_mode
            }))),
            Err(error) => failure(
                match &error {
                    DeviceStatusError::Unavailable => "unavailable",
                    DeviceStatusError::Other(_) => "native_error",
                },
                error,
            ),
        })
    }
}
