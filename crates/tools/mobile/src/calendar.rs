//! Bounded read-only native calendar access.

use async_trait::async_trait;
use lingxi_core::host::calendar::{CalendarError, CalendarQuery};
use once_cell::sync::Lazy;
use permission::PermissionResult;
use serde::Deserialize;
use serde_json::{json, Value};
use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
    ValidationError,
};
use tool_api::BuiltinToolContext;

use crate::device_support::{ask, bound_text, bounded_records, execute, failure, invalid, parse};

/// Canonical model-facing tool name.
pub const TOOL_NAME: &str = "calendar";
const MAX_RANGE_MS: u64 = 366 * 24 * 60 * 60 * 1000;
const MAX_LIMIT: u32 = 100;
// Native Android timestamps are signed 64-bit milliseconds.
const MAX_TIMESTAMP_MS: u64 = i64::MAX as u64;

fn default_limit() -> u32 {
    50
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    start_ms: u64,
    end_ms: u64,
    #[serde(default = "default_limit")]
    limit: u32,
}

fn query(input: &Value) -> Result<CalendarQuery, ValidationError> {
    let input: Input = parse(input)?;
    if input.end_ms > MAX_TIMESTAMP_MS
        || input.end_ms <= input.start_ms
        || input.end_ms - input.start_ms > MAX_RANGE_MS
    {
        return Err(ValidationError("end_ms must be after start_ms, the range must not exceed 366 days, and timestamps must fit signed 64-bit epoch milliseconds".into()));
    }
    if !(1..=MAX_LIMIT).contains(&input.limit) {
        return Err(ValidationError(
            "limit must be an integer from 1 through 100".into(),
        ));
    }
    Ok(CalendarQuery {
        start_ms: input.start_ms,
        end_ms: input.end_ms,
        limit: input.limit,
    })
}

/// List native calendar events within an explicit bounded range.
#[derive(Clone)]
pub struct CalendarTool {
    ctx: BuiltinToolContext,
}

impl CalendarTool {
    /// Construct using the native provider in the builtin context.
    #[must_use]
    pub fn new(ctx: BuiltinToolContext) -> Self {
        Self { ctx }
    }
}

static INPUT_SCHEMA: Lazy<Value> = Lazy::new(|| {
    json!({
        "type": "object", "properties": {
            "start_ms": { "type": "integer", "minimum": 0, "maximum": MAX_TIMESTAMP_MS, "description": "Inclusive start in Unix epoch milliseconds." },
            "end_ms": { "type": "integer", "minimum": 1, "maximum": MAX_TIMESTAMP_MS, "description": "Exclusive end in Unix epoch milliseconds; later than start_ms and at most 366 days after it." },
            "limit": { "type": "integer", "minimum": 1, "maximum": MAX_LIMIT, "default": 50 }
        }, "required": ["start_ms", "end_ms"], "additionalProperties": false
    })
});

#[async_trait]
impl Tool for CalendarTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }
    fn input_schema(&self) -> &Value {
        &INPUT_SCHEMA
    }
    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        self.ctx.calendar.is_some()
    }
    fn max_result_size_chars(&self) -> usize {
        65536
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        false
    }
    fn is_read_only(&self, _: &Value) -> bool {
        true
    }
    fn search_hint(&self) -> Option<&str> {
        Some("Read calendar events, appointments, meeting times and schedule")
    }
    async fn check_permissions(&self, _: &Value, _: &ToolUseContext) -> PermissionResult {
        ask(TOOL_NAME, "Read calendar events in the requested time range. Native calendar permission may also be required.")
    }
    async fn description(&self, _: &Value, _: &DescriptionOptions) -> String {
        "Reading calendar events in a time range".into()
    }
    async fn prompt(&self, _: &PromptOptions) -> String {
        "Read native calendar events intersecting [start_ms, end_ms), expressed in Unix epoch milliseconds. The range is at most 366 days and limit is 1..100 (default 50). Returns events with id, title, start_ms, end_ms, all_day and optional location, notes, calendar. Use a narrow range for the user's request; this tool cannot create or modify events.".into()
    }
    async fn validate_input(
        &self,
        input: &Value,
        _: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        query(input).map(|_| ())
    }
    async fn call(
        &self,
        input: Value,
        ctx: ToolUseContext,
        _: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let request = query(&input).map_err(invalid)?;
        let limit = request.limit as usize;
        let Some(provider) = &self.ctx.calendar else {
            return Ok(failure(
                "unavailable",
                "Calendar is unavailable on this device",
            ));
        };
        Ok(match execute(&ctx, provider.list_events(request)).await? {
            Ok(mut events) => {
                let mut truncated = events.len() > limit;
                events.truncate(limit);
                for event in &mut events {
                    truncated |= bound_text(&mut event.id, 512);
                    truncated |= bound_text(&mut event.title, 500);
                    for (text, max) in [
                        (&mut event.location, 1000),
                        (&mut event.notes, 4000),
                        (&mut event.calendar, 200),
                    ] {
                        if let Some(text) = text {
                            truncated |= bound_text(text, max);
                        }
                    }
                }
                bounded_records(
                    "events",
                    events.into_iter().map(|event| json!(event)),
                    self.max_result_size_chars(),
                    truncated,
                )
            }
            Err(error) => failure(
                match &error {
                    CalendarError::Unavailable => "unavailable",
                    CalendarError::PermissionDenied => "permission_denied",
                    CalendarError::Invalid(_) => "invalid_request",
                    CalendarError::Other(_) => "native_error",
                },
                error,
            ),
        })
    }
}
