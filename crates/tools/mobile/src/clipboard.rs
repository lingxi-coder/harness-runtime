//! `tool-clipboard` — the mobile-exclusive `clipboard` tool.
//!
//! Routes to `ctx.clipboard` (`Arc<dyn Clipboard>`). `None` on desktop; mobile
//! composition roots wire a native Swift / Kotlin impl via `UniFFI`. Engine-
//! driven: the model reads/writes the system clipboard (the analog of the
//! camera/voice/share seams) — there is no user-facing UI affordance. The two
//! actions are `set` (write `text`) and `get` (read the current contents).

use async_trait::async_trait;
use once_cell::sync::Lazy;
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use platform_api::clipboard::ClipboardError;
use serde_json::{json, Value};

use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
    ValidationError,
};
use tool_api::BuiltinToolContext;

/// Tool name byte-lock.
pub const TOOL_NAME: &str = "clipboard";

/// `ClipboardTool` — read/write the system clipboard.
#[derive(Clone)]
pub struct ClipboardTool {
    ctx: BuiltinToolContext,
}

impl ClipboardTool {
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
            "action": { "type": "string", "enum": ["set", "get"] },
            "text": { "type": "string", "description": "Text to write to the clipboard (required for `set`)." }
        },
        "required": ["action"]
    })
});

fn map_clipboard_err(e: &ClipboardError) -> ToolError {
    match e {
        ClipboardError::Unsupported => {
            ToolError::Internal("clipboard operation unsupported on this platform".into())
        }
        other @ ClipboardError::Other(_) => ToolError::Internal(other.to_string()),
    }
}

#[async_trait]
impl Tool for ClipboardTool {
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
    fn is_read_only(&self, input: &Value) -> bool {
        // `get` is read-only; `set` mutates external clipboard state. Neither
        // touches workspace files, but `set` is not a pure read.
        input.get("action").and_then(Value::as_str) == Some("get")
    }

    async fn check_permissions(&self, _: &Value, _: &ToolUseContext) -> PermissionResult {
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "clipboard tool — native OS clipboard access".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    async fn description(&self, _input: &Value, _: &DescriptionOptions) -> String {
        "Accessing the clipboard".into()
    }

    async fn prompt(&self, _: &PromptOptions) -> String {
        "Read or write the device system clipboard (plain text).".into()
    }

    async fn validate_input(
        &self,
        input: &Value,
        _: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        match input.get("action").and_then(Value::as_str) {
            Some("set") => {
                let has_text = input.get("text").and_then(Value::as_str).is_some();
                if has_text {
                    Ok(())
                } else {
                    Err(ValidationError("`set` requires a `text` field".into()))
                }
            }
            Some("get") => Ok(()),
            _ => Err(ValidationError("`action` must be `set` or `get`".into())),
        }
    }

    async fn call(
        &self,
        input: Value,
        _ctx: ToolUseContext,
        _progress_tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let clipboard = self.ctx.clipboard.as_ref().ok_or_else(|| {
            ToolError::Internal("clipboard not available on this platform".into())
        })?;

        let action = input
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or_default();

        let data = match action {
            "set" => {
                let text = input
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                clipboard
                    .set_text(text)
                    .await
                    .map_err(|e| map_clipboard_err(&e))?;
                json!({ "set": true })
            }
            "get" => {
                let text = clipboard
                    .get_text()
                    .await
                    .map_err(|e| map_clipboard_err(&e))?;
                json!({ "text": text })
            }
            other => {
                return Err(ToolError::Internal(format!("unknown action `{other}`")));
            }
        };

        Ok(ToolCallResult {
            data,
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }
}

/// Register the `clipboard` tool against `reg`.
pub fn register_all(reg: &mut tool_api::ToolRegistry, ctx: tool_api::BuiltinToolContext) {
    use std::sync::Arc;
    reg.register_builtin(Arc::new(ClipboardTool::new(ctx)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use platform_api::clipboard::Clipboard;
    use std::sync::{Arc, Mutex};

    /// Fake `Clipboard` that records the last written text and serves a
    /// configurable read value.
    #[derive(Default)]
    struct FakeClipboard {
        last: Mutex<Option<String>>,
        read: Option<String>,
        unsupported: bool,
    }

    #[async_trait]
    impl Clipboard for FakeClipboard {
        async fn set_text(&self, text: String) -> Result<(), ClipboardError> {
            if self.unsupported {
                return Err(ClipboardError::Unsupported);
            }
            *self.last.lock().unwrap() = Some(text);
            Ok(())
        }
        async fn get_text(&self) -> Result<Option<String>, ClipboardError> {
            if self.unsupported {
                return Err(ClipboardError::Unsupported);
            }
            Ok(self.read.clone())
        }
    }

    fn empty_output() -> platform_api::process::ProcessOutput {
        platform_api::process::ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        }
    }

    fn ctx_with(clipboard: Option<Arc<dyn Clipboard>>) -> BuiltinToolContext {
        let mut ctx = tool_api::test_support::shell_test_ctx(empty_output());
        ctx.clipboard = clipboard;
        ctx
    }

    #[tokio::test]
    async fn set_routes_to_clipboard() {
        let fake = Arc::new(FakeClipboard::default());
        let tool = ClipboardTool::new(ctx_with(Some(fake.clone())));
        let res = tool
            .call(
                json!({ "action": "set", "text": "hello" }),
                tool_api::test_support::fresh_ctx(),
                tool_api::test_support::fresh_tx(),
            )
            .await
            .expect("ok");
        assert_eq!(res.data["set"], true);
        assert_eq!(fake.last.lock().unwrap().clone().as_deref(), Some("hello"));
    }

    #[tokio::test]
    async fn get_routes_to_clipboard() {
        let fake = Arc::new(FakeClipboard {
            read: Some("copied".into()),
            ..Default::default()
        });
        let tool = ClipboardTool::new(ctx_with(Some(fake)));
        let res = tool
            .call(
                json!({ "action": "get" }),
                tool_api::test_support::fresh_ctx(),
                tool_api::test_support::fresh_tx(),
            )
            .await
            .expect("ok");
        assert_eq!(res.data["text"], "copied");
    }

    #[tokio::test]
    async fn missing_backend_errors() {
        let tool = ClipboardTool::new(ctx_with(None));
        let err = tool
            .call(
                json!({ "action": "get" }),
                tool_api::test_support::fresh_ctx(),
                tool_api::test_support::fresh_tx(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Internal(_)));
    }

    #[tokio::test]
    async fn set_requires_text() {
        let fake = Arc::new(FakeClipboard::default());
        let tool = ClipboardTool::new(ctx_with(Some(fake)));
        let err = tool
            .validate_input(
                &json!({ "action": "set" }),
                &tool_api::test_support::fresh_ctx(),
            )
            .await
            .unwrap_err();
        assert!(err.0.contains("text"));
    }
}
