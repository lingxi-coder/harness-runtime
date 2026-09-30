//! Validated external URL opening through the native host.

use async_trait::async_trait;
use lingxi_core::host::deep_link::DeepLinkError;
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
use url::Url;

use crate::device_support::{ask, execute, failure, invalid, parse};

/// Canonical model-facing tool name.
pub const TOOL_NAME: &str = "open_url";
const MAX_URL_CHARS: usize = 4096;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    url: String,
}

fn validated_url(input: &Value) -> Result<String, ValidationError> {
    let request: Input = parse(input)?;
    let raw = &request.url;
    if raw.is_empty()
        || raw.chars().count() > MAX_URL_CHARS
        || raw.chars().any(|c| c.is_control() || c.is_whitespace())
        || raw.contains('\\')
    {
        return Err(ValidationError(
            "url must be 1..=4096 characters without whitespace, control characters or backslashes"
                .into(),
        ));
    }
    // URL parsing tolerates malformed percent escapes; don't forward those or
    // encoded control bytes to platform-specific URL parsers.
    let bytes = raw.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'%' {
            let escaped = bytes
                .get(index + 1..index + 3)
                .filter(|pair| pair.iter().all(u8::is_ascii_hexdigit));
            let Some(escaped) = escaped else {
                return Err(ValidationError(
                    "url contains an invalid percent escape".into(),
                ));
            };
            let decoded = (char::from(escaped[0]).to_digit(16).unwrap() * 16)
                + char::from(escaped[1]).to_digit(16).unwrap();
            if decoded <= 0x1f || decoded == 0x7f {
                return Err(ValidationError(
                    "url contains an encoded control character".into(),
                ));
            }
        }
    }
    let parsed = Url::parse(raw).map_err(|e| ValidationError(format!("invalid URL: {e}")))?;
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(ValidationError("url must not contain credentials".into()));
    }
    let scheme_specific = raw
        .split_once(':')
        .map(|(_, rest)| rest)
        .unwrap_or_default();
    match parsed.scheme() {
        "http" | "https" => {
            if !scheme_specific.starts_with("//")
                || scheme_specific.starts_with("///")
                || parsed.host_str().is_none()
            {
                return Err(ValidationError(
                    "http(s) URL requires // followed by a host".into(),
                ));
            }
        }
        "mailto" | "tel" => {
            if !parsed.cannot_be_a_base()
                || parsed.path().is_empty()
                || parsed.path().contains('/')
                || parsed.fragment().is_some()
            {
                return Err(ValidationError("mailto and tel URLs require a recipient and cannot contain an authority or fragment".into()));
            }
            if parsed.scheme() == "tel" {
                let number = parsed.path().split(';').next().unwrap_or_default();
                if !number.bytes().any(|b| b.is_ascii_digit())
                    || !number.bytes().all(|b| {
                        b.is_ascii_digit() || matches!(b, b'+' | b'-' | b'.' | b'(' | b')')
                    })
                    || parsed.query().is_some()
                {
                    return Err(ValidationError(
                        "tel URL requires a phone number and cannot contain a query".into(),
                    ));
                }
            }
        }
        _ => {
            return Err(ValidationError(
                "url scheme must be http, https, mailto or tel".into(),
            ))
        }
    }
    Ok(parsed.to_string())
}

/// Open a standard external web, email or phone URL with host approval.
#[derive(Clone)]
pub struct OpenUrlTool {
    ctx: BuiltinToolContext,
}

impl OpenUrlTool {
    /// Construct using the native provider in the builtin context.
    #[must_use]
    pub fn new(ctx: BuiltinToolContext) -> Self {
        Self { ctx }
    }
}

static INPUT_SCHEMA: Lazy<Value> = Lazy::new(|| {
    json!({
        "type": "object", "properties": {
            "url": { "type": "string", "minLength": 1, "maxLength": MAX_URL_CHARS,
                "description": "An absolute http, https, mailto or tel URL. No credentials, internal/file/script schemes or control characters." }
        }, "required": ["url"], "additionalProperties": false
    })
});

#[async_trait]
impl Tool for OpenUrlTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }
    fn input_schema(&self) -> &Value {
        &INPUT_SCHEMA
    }
    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        self.ctx.deep_link.is_some()
    }
    fn max_result_size_chars(&self) -> usize {
        8192
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        false
    }
    fn is_read_only(&self, _: &Value) -> bool {
        false
    }
    fn search_hint(&self) -> Option<&str> {
        Some("Open external browser, website, email composer or phone dialer URL")
    }
    async fn check_permissions(&self, input: &Value, _: &ToolUseContext) -> PermissionResult {
        ask(
            TOOL_NAME,
            &format!(
                "Open this URL in an external app: {}",
                input.get("url").and_then(Value::as_str).unwrap_or_default()
            ),
        )
    }
    async fn description(&self, _: &Value, _: &DescriptionOptions) -> String {
        "Opening a URL in an external app".into()
    }
    async fn prompt(&self, _: &PromptOptions) -> String {
        "Open an absolute http/https web URL, mailto recipient or tel phone number in the device's external app. Opening is an external side effect subject to tool permission; success reports that the host submitted the open request, not that an app opened, an email was sent or a call completed.".into()
    }
    async fn validate_input(
        &self,
        input: &Value,
        _: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        validated_url(input).map(|_| ())
    }
    async fn call(
        &self,
        input: Value,
        ctx: ToolUseContext,
        _: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let url = validated_url(&input).map_err(invalid)?;
        let Some(opener) = &self.ctx.deep_link else {
            return Ok(failure(
                "unavailable",
                "Opening external URLs is unavailable on this device",
            ));
        };
        Ok(match execute(&ctx, opener.open(url.clone())).await? {
            Ok(()) => ToolCallResult::from_data(json!({ "requested": true, "url": url })),
            Err(error) => failure(
                match &error {
                    DeepLinkError::Unavailable => "unavailable",
                    DeepLinkError::Rejected(_) => "rejected",
                    DeepLinkError::Other(_) => "native_error",
                },
                error,
            ),
        })
    }
}
