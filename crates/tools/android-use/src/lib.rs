//! Android-native Computer Use tool.
//!
//! This is intentionally separate from the desktop `computer` tool: Android
//! has accessibility nodes, gestures and global navigation, not a desktop
//! cursor/right-click abstraction.

#![forbid(unsafe_code)]
#![allow(missing_docs)]

use async_trait::async_trait;
use lingxi_core::host::{
    AndroidAccessRequest, AndroidAccessTier, AndroidAction, AndroidAudioListenRequest,
    AndroidAudioSpeakRequest, AndroidAutomationError, AndroidGlobalAction, AndroidNodeQuery,
    AndroidUiAutomation, AndroidWaitCondition, MAX_ANDROID_AUDIO_LISTEN_MS,
    MAX_ANDROID_AUDIO_SPEAK_CHARS, MAX_ANDROID_UI_BATCH, MAX_ANDROID_UI_WAIT_MS,
};
use once_cell::sync::Lazy;
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use serde_json::{json, Value};
use std::sync::Arc;

use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
    ValidationError,
};

pub const TOOL_NAME: &str = "android_use";
const EPHEMERAL_MARKER: &str = "_lingxi_ephemeral";

#[derive(Clone)]
pub struct AndroidUseTool {
    automation: Arc<dyn AndroidUiAutomation>,
}

impl AndroidUseTool {
    #[must_use]
    pub fn new(automation: Arc<dyn AndroidUiAutomation>) -> Self {
        Self { automation }
    }
}

const ACTIONS: &[&str] = &[
    "status",
    "request_access",
    "list_granted_apps",
    "stop",
    "screenshot",
    "ui_tree",
    "find",
    "inspect",
    "tap",
    "long_press",
    "set_text",
    "clear_text",
    "key",
    "scroll",
    "swipe",
    "pinch",
    "open_app",
    "wait_for",
    "wait_idle",
    "listen",
    "speak",
    "stop_audio",
    "batch",
];

const ANDROID_USE_PROMPT: &str = "\
Operate Android apps and system UI through the supported native Accessibility \
Computer Use channel. For any request to open, inspect, tap, type in, scroll, or \
navigate an Android app, use this tool — NEVER use Shell commands such as \
`monkey`, `am`, `cmd`, `pm`, `input`, `settings`, or `dumpsys`; the app-sandboxed \
Shell is not adb/system shell and those commands will be denied.\n\n\
Always call `status` before the first UI action. A session and its per-app grants \
must already have been started by the user in LingXi Settings. `request_access` \
only verifies those existing grants; it cannot turn on Accessibility or silently \
start/expand a session. If status reports service disabled/session inactive, or \
the target app is not granted, stop and ask the user to enable/start Computer Use \
and authorize that app. Do not fall back to Shell or suppress the error.\n\n\
Use `list_granted_apps` to obtain package names, then `open_app` with an authorized \
package. Prefer accessibility node IDs over coordinates and re-observe with \
`ui_tree`/`find` after UI changes. The same active session may `listen` through \
the microphone or `speak` through Android TTS only when those capabilities are \
enabled in Settings. Protected and high-risk surfaces remain enforced by the \
Android host.";

static INPUT_SCHEMA: Lazy<Value> = Lazy::new(|| {
    json!({
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "enum": ACTIONS,
                "description": "Android native Computer Use action. Call status first; use open_app instead of Shell/monkey/am/cmd."
            },
            "reason": {
                "type": "string",
                "maxLength": 500,
                "description": "Why access is needed. request_access validates grants already started by the user."
            },
            "apps": {
                "type": "array",
                "items": { "type": "string" },
                "maxItems": 20,
                "description": "Package names whose existing session grants should be verified."
            },
            "tier": {
                "type": "string",
                "enum": ["read", "click", "full"],
                "description": "Minimum tier to verify. open_app and text/global navigation require full."
            },
            "clipboard_read": { "type": "boolean" },
            "clipboard_write": { "type": "boolean" },
            "include_system_ui": { "type": "boolean" },
            "node_id": { "type": "string" },
            "x": { "type": "integer", "minimum": 0 },
            "y": { "type": "integer", "minimum": 0 },
            "start_x": { "type": "integer", "minimum": 0 },
            "start_y": { "type": "integer", "minimum": 0 },
            "end_x": { "type": "integer", "minimum": 0 },
            "end_y": { "type": "integer", "minimum": 0 },
            "center_x": { "type": "integer", "minimum": 0 },
            "center_y": { "type": "integer", "minimum": 0 },
            "duration_ms": { "type": "integer", "minimum": 1, "maximum": 5000 },
            "scale": { "type": "number", "minimum": 0.1, "maximum": 10.0 },
            "text": { "type": "string" },
            "package_name": {
                "type": "string",
                "description": "Authorized Android package from list_granted_apps; required by open_app."
            },
            "key": {
                "type": "string",
                "enum": ["back", "home", "recents", "notifications", "quick_settings",
                         "enter", "up", "down", "left", "right"]
            },
            "direction": { "type": "string", "enum": ["up", "down", "left", "right"] },
            "amount": { "type": "integer", "minimum": 1, "maximum": 10 },
            "query": { "type": "object" },
            "condition": { "type": "object" },
            "timeout_ms": { "type": "integer", "minimum": 1, "maximum": MAX_ANDROID_UI_WAIT_MS },
            "listen_timeout_ms": {
                "type": "integer",
                "minimum": 1000,
                "maximum": MAX_ANDROID_AUDIO_LISTEN_MS
            },
            "language": { "type": "string", "maxLength": 64 },
            "voice": { "type": "string", "maxLength": 128 },
            "speed": { "type": "number", "minimum": 0.5, "maximum": 2.0 },
            "actions": {
                "type": "array",
                "items": { "type": "object" },
                "minItems": 1,
                "maxItems": MAX_ANDROID_UI_BATCH
            }
        },
        "required": ["action"],
        "additionalProperties": false
    })
});

fn map_err(error: AndroidAutomationError) -> ToolError {
    match error {
        AndroidAutomationError::PermissionDenied(message)
        | AndroidAutomationError::TargetNotAllowed(message)
        | AndroidAutomationError::TierInsufficient(message)
        | AndroidAutomationError::ProtectedSurface(message) => ToolError::PermissionDenied(message),
        AndroidAutomationError::Timeout(message) => ToolError::Io(format!("timeout: {message}")),
        other => ToolError::Internal(other.to_string()),
    }
}

fn parse_tier(input: &Value) -> Result<AndroidAccessTier, ToolError> {
    match input.get("tier").and_then(Value::as_str).unwrap_or("read") {
        "read" => Ok(AndroidAccessTier::Read),
        "click" => Ok(AndroidAccessTier::Click),
        "full" => Ok(AndroidAccessTier::Full),
        other => Err(ToolError::InvalidInput(format!("unknown tier `{other}`"))),
    }
}

fn u32_field(input: &Value, key: &str) -> Result<u32, ToolError> {
    input
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| ToolError::InvalidInput(format!("`{key}` is required")))
}

fn optional_u32(input: &Value, key: &str) -> Option<u32> {
    input
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
}

fn optional_node(input: &Value) -> Option<String> {
    input
        .get("node_id")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn node_or_coordinate(
    input: &Value,
) -> Result<(Option<String>, Option<u32>, Option<u32>), ToolError> {
    let node_id = optional_node(input);
    let x = optional_u32(input, "x");
    let y = optional_u32(input, "y");
    if node_id.is_none() && (x.is_none() || y.is_none()) {
        return Err(ToolError::InvalidInput(
            "provide `node_id` or both `x` and `y`".into(),
        ));
    }
    Ok((node_id, x, y))
}

fn parse_query(value: Option<&Value>) -> Result<AndroidNodeQuery, ToolError> {
    serde_json::from_value(value.cloned().unwrap_or_else(|| json!({})))
        .map_err(|error| ToolError::InvalidInput(format!("invalid `query`: {error}")))
}

fn action_result(data: Value) -> ToolCallResult {
    ToolCallResult {
        data,
        model_content: None,
        new_messages: vec![],
        context_modifier: None,
        is_error: false,
        mcp_meta: None,
    }
}

fn ephemeral_audio_result(mut data: Value) -> ToolCallResult {
    if let Some(object) = data.as_object_mut() {
        object.insert(EPHEMERAL_MARKER.into(), Value::Bool(true));
        object.insert(
            "summary".into(),
            Value::String(
                "Temporary Android microphone transcript omitted from session persistence.".into(),
            ),
        );
    }
    action_result(data)
}

fn screenshot_result(
    screenshot: lingxi_core::host::AndroidScreenshot,
) -> Result<ToolCallResult, ToolError> {
    let original_size = u64::try_from(screenshot.png_bytes.len()).unwrap_or(u64::MAX);
    let processed = tool_api::util::image_budget::process_image(screenshot.png_bytes)
        .map_err(ToolError::Internal)?;
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
    Ok(ToolCallResult {
        data: json!({
            "type": "image",
            "file": file,
            "width": screenshot.width,
            "height": screenshot.height,
            EPHEMERAL_MARKER: true,
            "summary": "Temporary Android screen capture; pixels are excluded from session persistence."
        }),
        model_content: Some(
            json!({
                EPHEMERAL_MARKER: true,
                "summary": "Temporary Android screenshot attached; pixels are not persisted."
            })
            .to_string(),
        ),
        new_messages: vec![],
        context_modifier: None,
        is_error: false,
        mcp_meta: None,
    })
}

#[async_trait]
impl Tool for AndroidUseTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }

    fn user_facing_name(&self) -> Option<&str> {
        Some("Android Computer Use")
    }

    fn input_schema(&self) -> &Value {
        &INPUT_SCHEMA
    }

    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        true
    }

    fn max_result_size_chars(&self) -> usize {
        512 * 1024
    }

    fn is_concurrency_safe(&self, _: &Value) -> bool {
        false
    }

    fn is_read_only(&self, input: &Value) -> bool {
        matches!(
            input.get("action").and_then(Value::as_str),
            Some("status" | "list_granted_apps" | "screenshot" | "ui_tree" | "find" | "inspect")
        )
    }

    fn requires_user_interaction(&self) -> bool {
        true
    }

    async fn check_permissions(&self, _: &Value, _: &ToolUseContext) -> PermissionResult {
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "Android host enforces active-session grants and high-risk confirmation"
                    .into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    async fn description(&self, input: &Value, _: &DescriptionOptions) -> String {
        let action = input
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("status");
        format!("Android Computer Use: {action}")
    }

    async fn prompt(&self, _: &PromptOptions) -> String {
        ANDROID_USE_PROMPT.into()
    }

    async fn validate_input(
        &self,
        input: &Value,
        _: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        match input.get("action").and_then(Value::as_str) {
            Some(action) if ACTIONS.contains(&action) => Ok(()),
            _ => Err(ValidationError("unsupported Android action".into())),
        }
    }

    async fn call(
        &self,
        input: Value,
        _ctx: ToolUseContext,
        _progress_tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let action = input
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("status");
        match action {
            "status" => Ok(action_result(json!(self
                .automation
                .status()
                .await
                .map_err(map_err)?))),
            "request_access" => {
                let apps = input
                    .get("apps")
                    .and_then(Value::as_array)
                    .map(|values| {
                        values
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                let request = AndroidAccessRequest {
                    reason: input
                        .get("reason")
                        .and_then(Value::as_str)
                        .unwrap_or("Agent requested Android Computer Use access")
                        .to_string(),
                    apps,
                    tier: parse_tier(&input)?,
                    clipboard_read: input
                        .get("clipboard_read")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    clipboard_write: input
                        .get("clipboard_write")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    include_system_ui: input
                        .get("include_system_ui")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                };
                Ok(action_result(json!(self
                    .automation
                    .request_access(request)
                    .await
                    .map_err(map_err)?)))
            }
            "list_granted_apps" => Ok(action_result(json!(self
                .automation
                .list_granted_apps()
                .await
                .map_err(map_err)?))),
            "stop" => {
                self.automation.stop().await.map_err(map_err)?;
                Ok(action_result(json!({ "stopped": true })))
            }
            "screenshot" => {
                let screenshot = self.automation.screenshot().await.map_err(map_err)?;
                screenshot_result(screenshot)
            }
            "ui_tree" => Ok(action_result(json!(self
                .automation
                .ui_tree()
                .await
                .map_err(map_err)?))),
            "find" => {
                let query = parse_query(input.get("query"))?;
                Ok(action_result(json!(self
                    .automation
                    .find_nodes(query)
                    .await
                    .map_err(map_err)?)))
            }
            "inspect" => {
                let node_id = input
                    .get("node_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        ToolError::InvalidInput("`inspect` requires `node_id`".into())
                    })?;
                Ok(action_result(json!(self
                    .automation
                    .inspect_node(node_id.to_string())
                    .await
                    .map_err(map_err)?)))
            }
            "wait_for" => {
                let condition: AndroidWaitCondition =
                    serde_json::from_value(input.get("condition").cloned().ok_or_else(|| {
                        ToolError::InvalidInput("`wait_for` requires `condition`".into())
                    })?)
                    .map_err(|error| {
                        ToolError::InvalidInput(format!("invalid `condition`: {error}"))
                    })?;
                let timeout = input
                    .get("timeout_ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(10_000)
                    .min(MAX_ANDROID_UI_WAIT_MS);
                Ok(action_result(json!(self
                    .automation
                    .wait_for(condition, timeout)
                    .await
                    .map_err(map_err)?)))
            }
            "wait_idle" => {
                let timeout = input
                    .get("timeout_ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(10_000)
                    .min(MAX_ANDROID_UI_WAIT_MS);
                let quiet_ms = input
                    .get("duration_ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(500)
                    .min(5_000);
                Ok(action_result(json!(self
                    .automation
                    .wait_for(AndroidWaitCondition::Idle { quiet_ms }, timeout)
                    .await
                    .map_err(map_err)?)))
            }
            "listen" => {
                let timeout_ms = input
                    .get("listen_timeout_ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(15_000)
                    .clamp(1_000, MAX_ANDROID_AUDIO_LISTEN_MS);
                let language = input
                    .get("language")
                    .and_then(Value::as_str)
                    .filter(|value| !value.trim().is_empty() && *value != "auto")
                    .map(str::to_string);
                Ok(ephemeral_audio_result(json!(self
                    .automation
                    .listen(AndroidAudioListenRequest {
                        language,
                        timeout_ms,
                    })
                    .await
                    .map_err(map_err)?)))
            }
            "speak" => {
                let text = input
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ToolError::InvalidInput("`speak` requires `text`".into()))?;
                if text.is_empty() || text.chars().count() > MAX_ANDROID_AUDIO_SPEAK_CHARS {
                    return Err(ToolError::InvalidInput(format!(
                        "`speak` text must contain 1..={MAX_ANDROID_AUDIO_SPEAK_CHARS} characters"
                    )));
                }
                let speed = input
                    .get("speed")
                    .and_then(Value::as_f64)
                    .map(|value| value.clamp(0.5, 2.0) as f32);
                Ok(action_result(json!(self
                    .automation
                    .speak(AndroidAudioSpeakRequest {
                        text: text.to_string(),
                        voice: input
                            .get("voice")
                            .and_then(Value::as_str)
                            .filter(|value| !value.trim().is_empty())
                            .map(str::to_string),
                        speed,
                    })
                    .await
                    .map_err(map_err)?)))
            }
            "stop_audio" => {
                self.automation.stop_audio().await.map_err(map_err)?;
                Ok(action_result(json!({ "stopped": true })))
            }
            "batch" => {
                let actions = input
                    .get("actions")
                    .and_then(Value::as_array)
                    .ok_or_else(|| ToolError::InvalidInput("`batch` requires `actions`".into()))?;
                if actions.is_empty() || actions.len() > MAX_ANDROID_UI_BATCH {
                    return Err(ToolError::InvalidInput(format!(
                        "`batch` requires 1..={MAX_ANDROID_UI_BATCH} actions"
                    )));
                }
                let mut results = Vec::with_capacity(actions.len());
                for (index, step) in actions.iter().enumerate() {
                    let native = parse_native_action(step)?;
                    match self.automation.perform(native).await {
                        Ok(result) => results.push(json!({ "index": index, "result": result })),
                        Err(error) => {
                            return Ok(action_result(json!({
                                "success": false,
                                "steps_completed": results.len(),
                                "failed_index": index,
                                "error": error.to_string(),
                                "results": results,
                            })));
                        }
                    }
                }
                Ok(action_result(json!({
                    "success": true,
                    "steps_completed": results.len(),
                    "results": results,
                })))
            }
            _ => {
                let native = parse_native_action(&input)?;
                Ok(action_result(json!(self
                    .automation
                    .perform(native)
                    .await
                    .map_err(map_err)?)))
            }
        }
    }
}

fn parse_native_action(input: &Value) -> Result<AndroidAction, ToolError> {
    let action = input
        .get("action")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::InvalidInput("missing `action`".into()))?;
    match action {
        "tap" => {
            let (node_id, x, y) = node_or_coordinate(input)?;
            Ok(AndroidAction::Tap { node_id, x, y })
        }
        "long_press" => {
            let (node_id, x, y) = node_or_coordinate(input)?;
            Ok(AndroidAction::LongPress {
                node_id,
                x,
                y,
                duration_ms: input
                    .get("duration_ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(600)
                    .min(5_000),
            })
        }
        "set_text" => Ok(AndroidAction::SetText {
            node_id: optional_node(input),
            text: input
                .get("text")
                .and_then(Value::as_str)
                .ok_or_else(|| ToolError::InvalidInput("`set_text` requires `text`".into()))?
                .to_string(),
        }),
        "clear_text" => Ok(AndroidAction::ClearText {
            node_id: optional_node(input),
        }),
        "key" => match input.get("key").and_then(Value::as_str) {
            Some("back") => Ok(AndroidAction::Global {
                action: AndroidGlobalAction::Back,
            }),
            Some("home") => Ok(AndroidAction::Global {
                action: AndroidGlobalAction::Home,
            }),
            Some("recents") => Ok(AndroidAction::Global {
                action: AndroidGlobalAction::Recents,
            }),
            Some("notifications") => Ok(AndroidAction::Global {
                action: AndroidGlobalAction::Notifications,
            }),
            Some("quick_settings") => Ok(AndroidAction::Global {
                action: AndroidGlobalAction::QuickSettings,
            }),
            Some("enter") => Ok(AndroidAction::Enter {
                node_id: optional_node(input),
            }),
            Some(direction @ ("up" | "down" | "left" | "right")) => Ok(AndroidAction::Direction {
                direction: direction.to_string(),
            }),
            _ => Err(ToolError::InvalidInput("invalid or missing `key`".into())),
        },
        "scroll" => Ok(AndroidAction::Scroll {
            node_id: optional_node(input),
            x: optional_u32(input, "x"),
            y: optional_u32(input, "y"),
            direction: input
                .get("direction")
                .and_then(Value::as_str)
                .unwrap_or("down")
                .to_string(),
            amount: optional_u32(input, "amount").unwrap_or(1).min(10),
        }),
        "swipe" => Ok(AndroidAction::Swipe {
            start_x: u32_field(input, "start_x")?,
            start_y: u32_field(input, "start_y")?,
            end_x: u32_field(input, "end_x")?,
            end_y: u32_field(input, "end_y")?,
            duration_ms: input
                .get("duration_ms")
                .and_then(Value::as_u64)
                .unwrap_or(350)
                .min(5_000),
        }),
        "pinch" => Ok(AndroidAction::Pinch {
            center_x: u32_field(input, "center_x")?,
            center_y: u32_field(input, "center_y")?,
            scale: input
                .get("scale")
                .and_then(Value::as_f64)
                .unwrap_or(1.5)
                .clamp(0.1, 10.0) as f32,
            duration_ms: input
                .get("duration_ms")
                .and_then(Value::as_u64)
                .unwrap_or(500)
                .min(5_000),
        }),
        "open_app" => Ok(AndroidAction::OpenApp {
            package_name: input
                .get("package_name")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    ToolError::InvalidInput("`open_app` requires `package_name`".into())
                })?
                .to_string(),
        }),
        other => Err(ToolError::InvalidInput(format!(
            "`{other}` is not valid inside a control batch"
        ))),
    }
}

pub fn register_all(reg: &mut tool_api::ToolRegistry, automation: Arc<dyn AndroidUiAutomation>) {
    reg.register_builtin(Arc::new(AndroidUseTool::new(automation)));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_contains_complete_android_action_set() {
        let actions = INPUT_SCHEMA["properties"]["action"]["enum"]
            .as_array()
            .expect("action enum");
        for expected in ACTIONS {
            assert!(actions.iter().any(|value| value == expected));
        }
    }

    #[test]
    fn batch_is_bounded() {
        assert_eq!(
            INPUT_SCHEMA["properties"]["actions"]["maxItems"],
            MAX_ANDROID_UI_BATCH
        );
    }

    #[test]
    fn audio_actions_have_bounded_inputs_and_ephemeral_transcripts() {
        assert_eq!(
            INPUT_SCHEMA["properties"]["listen_timeout_ms"]["maximum"],
            MAX_ANDROID_AUDIO_LISTEN_MS
        );
        assert!(ACTIONS.contains(&"listen"));
        assert!(ACTIONS.contains(&"speak"));
        assert!(ACTIONS.contains(&"stop_audio"));

        let result = ephemeral_audio_result(json!({"text":"private words"}));
        assert_eq!(result.data[EPHEMERAL_MARKER], true);
        assert_eq!(result.data["text"], "private words");
        assert!(result.data["summary"]
            .as_str()
            .is_some_and(|summary| summary.contains("omitted")));
    }

    #[test]
    fn native_actions_require_target_coordinates_or_node() {
        assert!(parse_native_action(&json!({"action":"tap"})).is_err());
        assert!(parse_native_action(&json!({"action":"tap","node_id":"n"})).is_ok());
        assert!(parse_native_action(&json!({"action":"tap","x":1,"y":2})).is_ok());
    }

    #[test]
    fn prompt_routes_android_host_actions_away_from_shell() {
        for required in [
            "Always call `status`",
            "`open_app`",
            "`monkey`",
            "Do not fall back to Shell",
            "list_granted_apps",
        ] {
            assert!(
                ANDROID_USE_PROMPT.contains(required),
                "missing Android routing guidance: {required}"
            );
        }
    }
}
