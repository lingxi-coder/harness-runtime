//! `tool-notification` — the mobile-exclusive `notification` tool.
//!
//! Routes to `ctx.notifications` (`Arc<dyn NotificationService>`). `None` on
//! desktop; mobile composition roots wire a native Swift / Kotlin impl via
//! `UniFFI`. Engine-driven: the model posts a local system notification (the
//! analog of the camera/voice/share seams) — there is no user-facing UI
//! affordance. The single action is `post` (title, body, optional tag).

use async_trait::async_trait;
use once_cell::sync::Lazy;
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use platform_api::notification::{NotificationError, NotificationRequest};
use serde_json::{json, Value};

use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
    ValidationError,
};
use tool_api::BuiltinToolContext;

/// Tool name byte-lock.
pub const TOOL_NAME: &str = "notification";

/// `NotificationTool` — post a local system notification.
#[derive(Clone)]
pub struct NotificationTool {
    ctx: BuiltinToolContext,
}

impl NotificationTool {
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
            "action": { "type": "string", "enum": ["post"] },
            "title": { "type": "string", "description": "Notification title." },
            "body": { "type": "string", "description": "Notification body text." },
            "tag": { "type": "string", "description": "Optional channel/identifier tag for coalescing (optional)." }
        },
        "required": ["action", "title", "body"]
    })
});

fn map_notification_err(e: &NotificationError) -> ToolError {
    match e {
        NotificationError::PermissionDenied => {
            ToolError::PermissionDenied("notification permission denied".into())
        }
        other @ NotificationError::Other(_) => ToolError::Internal(other.to_string()),
    }
}

#[async_trait]
impl Tool for NotificationTool {
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
    fn is_read_only(&self, _input: &Value) -> bool {
        // Posting a notification has no effect on the workspace files.
        true
    }

    async fn check_permissions(&self, _: &Value, _: &ToolUseContext) -> PermissionResult {
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "notification tool — native OS notification prompt gates posting".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    async fn description(&self, _input: &Value, _: &DescriptionOptions) -> String {
        "Posting a notification".into()
    }

    async fn prompt(&self, _: &PromptOptions) -> String {
        "Post a local system notification (title + body) to the device.".into()
    }

    async fn validate_input(
        &self,
        input: &Value,
        _: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        match input.get("action").and_then(Value::as_str) {
            Some("post") => {
                let has_title = input
                    .get("title")
                    .and_then(Value::as_str)
                    .is_some_and(|t| !t.trim().is_empty());
                let has_body = input.get("body").and_then(Value::as_str).is_some();
                if has_title && has_body {
                    Ok(())
                } else {
                    Err(ValidationError(
                        "`post` requires a non-empty `title` and a `body`".into(),
                    ))
                }
            }
            _ => Err(ValidationError("`action` must be `post`".into())),
        }
    }

    async fn call(
        &self,
        input: Value,
        _ctx: ToolUseContext,
        _progress_tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let notifications = self.ctx.notifications.as_ref().ok_or_else(|| {
            ToolError::Internal("notifications not available on this platform".into())
        })?;

        let title = input
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let body = input
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let tag = input.get("tag").and_then(Value::as_str).map(str::to_string);

        notifications
            .notify(NotificationRequest { title, body, tag })
            .await
            .map_err(|e| map_notification_err(&e))?;

        Ok(ToolCallResult {
            data: json!({ "posted": true }),
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }
}

/// Register the `notification` tool against `reg`.
pub fn register_all(reg: &mut tool_api::ToolRegistry, ctx: tool_api::BuiltinToolContext) {
    use std::sync::Arc;
    reg.register_builtin(Arc::new(NotificationTool::new(ctx)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use platform_api::notification::NotificationService;
    use std::sync::{Arc, Mutex};

    /// Fake `NotificationService` that records the last request it received.
    #[derive(Default)]
    struct FakeNotifier {
        last: Mutex<Option<NotificationRequest>>,
        deny: bool,
    }

    #[async_trait]
    impl NotificationService for FakeNotifier {
        async fn notify(&self, req: NotificationRequest) -> Result<(), NotificationError> {
            if self.deny {
                return Err(NotificationError::PermissionDenied);
            }
            *self.last.lock().unwrap() = Some(req);
            Ok(())
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

    fn ctx_with(notifier: Option<Arc<dyn NotificationService>>) -> BuiltinToolContext {
        let mut ctx = tool_api::test_support::shell_test_ctx(empty_output());
        ctx.notifications = notifier;
        ctx
    }

    #[tokio::test]
    async fn post_routes_to_notification_service() {
        let fake = Arc::new(FakeNotifier::default());
        let tool = NotificationTool::new(ctx_with(Some(fake.clone())));
        let res = tool
            .call(
                json!({ "action": "post", "title": "Build done", "body": "All green", "tag": "ci" }),
                tool_api::test_support::fresh_ctx(),
                tool_api::test_support::fresh_tx(),
            )
            .await
            .expect("ok");
        assert_eq!(res.data["posted"], true);
        let last = fake.last.lock().unwrap().clone().expect("notify called");
        assert_eq!(last.title, "Build done");
        assert_eq!(last.body, "All green");
        assert_eq!(last.tag.as_deref(), Some("ci"));
    }

    #[tokio::test]
    async fn missing_backend_errors() {
        let tool = NotificationTool::new(ctx_with(None));
        let err = tool
            .call(
                json!({ "action": "post", "title": "t", "body": "b" }),
                tool_api::test_support::fresh_ctx(),
                tool_api::test_support::fresh_tx(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Internal(_)));
    }

    #[tokio::test]
    async fn permission_denied_maps_to_permission_error() {
        let fake = Arc::new(FakeNotifier {
            deny: true,
            ..Default::default()
        });
        let tool = NotificationTool::new(ctx_with(Some(fake)));
        let err = tool
            .call(
                json!({ "action": "post", "title": "t", "body": "b" }),
                tool_api::test_support::fresh_ctx(),
                tool_api::test_support::fresh_tx(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::PermissionDenied(_)));
    }

    #[tokio::test]
    async fn post_requires_title() {
        let fake = Arc::new(FakeNotifier::default());
        let tool = NotificationTool::new(ctx_with(Some(fake)));
        let err = tool
            .validate_input(
                &json!({ "action": "post", "body": "b" }),
                &tool_api::test_support::fresh_ctx(),
            )
            .await
            .unwrap_err();
        assert!(err.0.contains("title"));
    }
}
