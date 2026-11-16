//! Session binding for the separate computer-use MCP tools.
use super::*;
use permission::computer_access::{
    ComputerAccessRequest, ComputerAccessResponse, ComputerInteraction,
};
use std::collections::{HashMap, HashSet};

#[derive(Default)]
pub(super) struct ProtocolState {
    pub background: background_protocol::BackgroundState,
    pub app_records: HashMap<String, Value>,
    pub teach_active: bool,
    pub takeover_approved: bool,
    pub preferred_mode: Option<String>,
    pub warned: HashSet<String>,
    pub warn_turn: Option<String>,
    pub escape_active: bool,
    pub escape_owner: Option<String>,
    pub exit_progress: Option<(lingxi_core::types::ToolUseId, ToolProgressSender)>,
}

impl ComputerTool {
    pub(super) fn protocol_capabilities(&self) -> Value {
        protocol_capabilities_for_host(
            self.ctx.computer_control.as_deref(),
            self.access_resolver.supports_interactions(),
        )
    }
}

pub(super) fn protocol_capabilities_for_host(
    backend: Option<&dyn lingxi_core::host::computer_control::ComputerControl>,
    supports_interactions: bool,
) -> Value {
    let background = backend.is_some_and(|backend| backend.supports_app_scoped());
    // The reference's raw profile publishes drag and Space movement together.
    // Select it only when the host can implement both optional operations.
    let raw_input = backend.is_some_and(|backend| {
        backend.supports_app_scoped_raw_input() && backend.supports_app_scoped_space_move()
    });
    let filtering = backend.map_or("none", |backend| {
        if backend.supports_native_filtered_capture() {
            "native"
        } else {
            backend.screenshot_filtering()
        }
    });
    // Unknown backend modes must not borrow a stronger reviewed descriptor.
    if !matches!(filtering, "native" | "mask" | "none") {
        return Value::Null;
    }
    json!({"platform":if cfg!(target_os="windows") {"win32"} else {"darwin"},"screenshotFiltering":filtering,"adaptiveResolution":true,"saveToDisk":false,"teachMode":supports_interactions,"appScoped":if background {json!({"supportsRawInput":raw_input})}else{Value::Bool(false)}})
}

impl ComputerTool {
    pub(super) fn reset_protocol_grants(&self) -> Result<(), ToolError> {
        let mut protocol = self
            .protocol
            .lock()
            .map_err(|_| ToolError::Internal("computer protocol state poisoned".into()))?;
        protocol.background.clear();
        protocol.app_records.clear();
        protocol.teach_active = false;
        protocol.takeover_approved = false;
        Ok(())
    }
    pub(super) async fn resolve_protocol_interaction(
        &self,
        request: ComputerAccessRequest,
        ctx: &ToolUseContext,
    ) -> Result<ComputerAccessResponse, ToolError> {
        self.check_cancel(ctx)?;
        let revision = self.sync_policy(ctx).await?;
        if ctx.is_scheduled_turn {
            return Ok(ComputerAccessResponse::default());
        }
        let response = if let Some(cancel) = &ctx.cancel {
            tokio::select! { biased; () = cancel.cancelled() => return Err(ToolError::Aborted), response = self.access_resolver.resolve(request.clone()) => response }
        } else {
            self.access_resolver.resolve(request.clone()).await
        };
        self.check_cancel(ctx)?;
        self.check_policy_revision(revision.as_deref())?;
        if !request.allows_response(&response) {
            return Err(ToolError::PermissionDenied(
                "Computer approval exceeds the pending request".into(),
            ));
        }
        Ok(response)
    }

    pub(super) async fn call_protocol(
        &self,
        action: &str,
        input: Utf16JsonProjection,
        ctx: ToolUseContext,
        tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let _flight = escape_protocol::CallFlight::begin(
            &self.calls_in_flight,
            &self.escape_calls,
            Self::owner(&ctx).await,
            ctx.cancel.clone(),
        );
        match self
            .call_protocol_inner(action, input, ctx.clone(), tx)
            .await
        {
            Ok(result) => Ok(result),
            Err(ToolError::Aborted) => {
                self.cleanup_computer_inputs(&ctx).await?;
                Err(ToolError::Aborted)
            }
            Err(error @ (ToolError::InvalidInput(_) | ToolError::PermissionDenied(_))) => {
                Ok(protocol_result::error_source(error.model_facing_message()))
            }
            Err(error) => {
                // The native server catches unexpected executor exceptions,
                // settles held input, and reports a logical MCP error.
                self.end_computer_sequence(&ctx).await?;
                self.release_owned_inputs(&ctx).await?;
                self.protocol
                    .lock()
                    .map_err(|_| ToolError::Internal("computer protocol state poisoned".into()))?
                    .teach_active = false;
                let message = error.model_facing_message();
                let text = message.string_at("").unwrap_or("(withheld)");
                let safe = if action.starts_with("app_") {
                    static SAFE: once_cell::sync::Lazy<regex::Regex> =
                        once_cell::sync::Lazy::new(|| {
                            regex::Regex::new(r#"^[\p{L}\p{N}_ .,;:=(){}\"'§×%/\\+\-]+$"#).unwrap()
                        });
                    let text = text.trim();
                    if !text.is_empty() && text.encode_utf16().count() <= 120 && SAFE.is_match(text)
                    {
                        text
                    } else {
                        "(withheld)"
                    }
                } else {
                    text
                };
                Ok(protocol_result::error(
                    &format!("Tool \"{action}\" failed: {safe}"),
                    "executor_threw",
                ))
            }
        }
    }

    async fn call_protocol_inner(
        &self,
        action: &str,
        input: Utf16JsonProjection,
        ctx: ToolUseContext,
        tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        self.check_cancel(&ctx)?;
        // Cleanup is legal even after revocation, cancellation is settled by
        // the host's unconditional cleanup path.
        if action == "app_release" {
            return self.call_background_protocol(action, input, ctx, tx).await;
        }
        let revision = self.sync_policy(&ctx).await?;
        let permissions = self.read_os_permissions(&ctx).await?;
        if permissions.is_some_and(|(a, r)| !a || !r)
            && !matches!(action, "request_access" | "request_teach_access")
        {
            self.revoke_os_permissions(&ctx).await?;
            return Ok(protocol_result::error("Accessibility and Screen Recording permissions are required. Call request_access to show the permission panel.","tcc_not_granted"));
        }
        let exempt = matches!(
            action,
            "request_access"
                | "request_teach_access"
                | "list_granted_applications"
                | "list_apps"
                | "app_release"
        );
        if !exempt
            && self
                .state
                .lock()
                .map_err(|_| ToolError::Internal("computer state poisoned".into()))?
                .allowed_apps
                .is_empty()
        {
            return Ok(protocol_result::error(
                "No applications are granted for this session. Call request_access first.",
                "allowlist_empty",
            ));
        }
        self.check_policy_revision(revision.as_deref())?;
        match action {
            "request_access"
            | "request_teach_access"
            | "list_granted_applications"
            | "list_apps" => self.call_grant_protocol(action, input, ctx, tx).await,
            "request_full_control" | "release_full_control" | "teach_step" | "teach_batch" => {
                self.call_interaction_protocol(action, input, ctx, tx).await
            }
            _ if action.starts_with("app_") => {
                self.call_background_protocol(action, input, ctx, tx).await
            }
            _ => {
                let owner = Self::owner(&ctx).await;
                let (background, background_apps, needs_approval) = {
                    let state = self.protocol.lock().map_err(|_| {
                        ToolError::Internal("computer protocol state poisoned".into())
                    })?;
                    (
                        state.background.has_owner_locks(&owner),
                        state.background.owner_apps(&owner),
                        state.preferred_mode.as_deref() == Some("background")
                            && !state.takeover_approved,
                    )
                };
                if background && matches!(action, "read_clipboard" | "write_clipboard") {
                    let operation = if action == "read_clipboard" {
                        "read"
                    } else {
                        "write"
                    };
                    return Ok(protocol_result::error(&format!("Clipboard {operation} is unavailable while you hold background app-locks — the user keeps using their machine (and clipboard) while you work in the background. If this work needs the clipboard: app_release your locks, then use the display-scope tools — the next display-scope call takes over the screen with the user's approval."),"state_conflict"));
                }
                if background && action == "cursor_position" {
                    return Ok(protocol_result::error("cursor_position reads the live host cursor and is not available in background app-mode. Use the last app_screenshot's coordinates instead.","feature_unavailable"));
                }
                if background && !matches!(action, "wait" | "cursor_position" | "switch_display") {
                    return Ok(protocol_result::error(&format!("This session is currently controlling {} in the background. Use the app_* tools, or call app_release first to switch to full-screen control.", background_apps.join(", ")),"state_conflict"));
                }
                if needs_approval
                    && !matches!(action, "wait" | "cursor_position" | "switch_display")
                {
                    let result = self
                        .call_interaction_protocol(
                            "request_full_control",
                            Utf16JsonProjection::from_serde_value(&json!({})).unwrap(),
                            ctx.clone(),
                            tx.clone(),
                        )
                        .await?;
                    if result.is_error {
                        return Ok(result);
                    }
                }
                if !background {
                    self.begin_computer_sequence(&ctx).await?;
                    self.ensure_escape_registration(&ctx, &tx).await?;
                }
                self.call_display_protocol(action, input, ctx, tx).await
            }
        }
    }

    async fn call_interaction_protocol(
        &self,
        action: &str,
        input: Utf16JsonProjection,
        ctx: ToolUseContext,
        tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        use protocol_result::{error, json as result_json, text};
        if action == "release_full_control" {
            self.end_computer_sequence(&ctx).await?;
            self.release_owned_inputs(&ctx).await?;
            self.protocol
                .lock()
                .map_err(|_| ToolError::Internal("computer protocol state poisoned".into()))?
                .takeover_approved = false;
            return Ok(text("Released full-screen control. The display overlay is off. Keep going with the app_* tools; call request_full_control again if you need full-screen later (it will ask the user again)."));
        }
        if action == "request_full_control" {
            let available = {
                let state = self
                    .protocol
                    .lock()
                    .map_err(|_| ToolError::Internal("computer protocol state poisoned".into()))?;
                state.takeover_approved || state.preferred_mode.as_deref() == Some("full_control")
            };
            if available {
                return Ok(text("Full-screen control is already available for this session — the display-scope tools (screenshot, left_click, type, ...) will proceed. No further approval is needed."));
            }
            if ctx.is_scheduled_turn {
                return Ok(error("Full-screen control needs your approval, which can't be given during a scheduled run. Stay with the app_* tools for the granted background apps, or send a message in this conversation so the approval card can appear. (Retrying returns this same result.)","unattended_no_approver"));
            }
            if !self.access_resolver.supports_interactions() {
                return Ok(error("Full-screen control was not approved (declined or not confirmed). Stay with the app_* tools for the granted background apps, or explain what full-screen access is needed for and let the user decide.","takeover_declined"));
            }
            let request = interaction_request(ComputerInteraction::Takeover {
                reason: Some("Full-screen computer control".into()),
                explanation: None,
                next_preview: None,
                anchor: None,
            });
            let response = self.resolve_protocol_interaction(request, &ctx).await?;
            if response.interaction_action.as_deref() != Some("allow") {
                return Ok(error("Full-screen control was not approved (declined or not confirmed). Stay with the app_* tools for the granted background apps, or explain what full-screen access is needed for and let the user decide.","takeover_declined"));
            }
            self.protocol
                .lock()
                .map_err(|_| ToolError::Internal("computer protocol state poisoned".into()))?
                .takeover_approved = true;
            return Ok(text("Full-screen control approved for this session. The display-scope tools (screenshot, left_click, type, ...) will now proceed. This approval lasts until the session ends."));
        }
        if !self
            .protocol
            .lock()
            .map_err(|_| ToolError::Internal("computer protocol state poisoned".into()))?
            .teach_active
        {
            return Ok(error(
                "Teach mode is not active. Call request_teach_access first.",
                "teach_mode_not_active",
            ));
        }
        let value: Value = serde_json::from_str(
            &input
                .to_json_string()
                .map_err(|error| ToolError::Internal(error.to_string()))?,
        )
        .map_err(|e| ToolError::InvalidInput(e.to_string().into()))?;
        let steps = if action == "teach_batch" {
            match value
                .get("steps")
                .and_then(Value::as_array)
                .filter(|steps| !steps.is_empty())
            {
                Some(steps) => steps.clone(),
                None => return Ok(error("\"steps\" must be a non-empty array.", "bad_args")),
            }
        } else {
            vec![value]
        };
        // Validate every batch member before presenting the first step or
        // posting input. Permission UI is never interpreted as model commands.
        for (index, step) in steps.iter().enumerate() {
            if action == "teach_batch" && !step.is_object() {
                return Ok(error(
                    &format!("steps[{index}] must be an object"),
                    "bad_args",
                ));
            }
            let label = if action == "teach_batch" {
                format!("steps[{index}]")
            } else {
                "teach_step".into()
            };
            if let Some(message) = validate_teach_step(step, &label) {
                return Ok(error(&message, "bad_args"));
            }
        }
        let mut results = Vec::new();
        let mut any_actions = false;
        for (index, step) in steps.iter().enumerate() {
            let anchor = if let Some(values) = step.get("anchor").and_then(Value::as_array) {
                let values = values.iter().filter_map(Value::as_f64).collect::<Vec<_>>();
                self.protocol_anchor(&values, &ctx).await
            } else {
                None
            };
            let request = interaction_request(ComputerInteraction::TeachStep {
                reason: None,
                explanation: step["explanation"].as_str().map(str::to_owned),
                next_preview: step["next_preview"].as_str().map(str::to_owned),
                anchor,
            });
            let response = self.resolve_protocol_interaction(request, &ctx).await?;
            if response.interaction_action.as_deref() != Some("continue") {
                self.protocol
                    .lock()
                    .map_err(|_| ToolError::Internal("computer protocol state poisoned".into()))?
                    .teach_active = false;
                self.end_computer_sequence(&ctx).await?;
                self.release_owned_inputs(&ctx).await?;
                return Ok(result_json(if action == "teach_batch" {
                    json!({"exited":true,"stepsCompleted":index})
                } else {
                    json!({"exited":true})
                }));
            }
            let actions = step["actions"].as_array().unwrap();
            any_actions |= !actions.is_empty();
            let mut action_results = Vec::new();
            for (action_index, item) in actions.iter().enumerate() {
                self.check_cancel(&ctx)?;
                let name = item["action"].as_str().unwrap();
                let result = self
                    .call_display_protocol(
                        name,
                        Utf16JsonProjection::from_serde_value(item).unwrap(),
                        ctx.clone(),
                        tx.clone(),
                    )
                    .await?;
                let wire = protocol_result::wire(&result);
                let output = wire["content"]
                    .as_array()
                    .and_then(|blocks| blocks.first())
                    .filter(|block| block["type"] == "text")
                    .and_then(|block| block.get("text"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let entry = json!({"action":name,"ok":!result.is_error,"output":output});
                if result.is_error {
                    self.release_owned_inputs(&ctx).await?;
                    return Ok(result_json(if action == "teach_batch" {
                        json!({"stepsCompleted":index,"stepFailed":index,"executed":action_index,"failed":entry,"remaining":actions.len()-action_index-1,"results":results})
                    } else {
                        json!({"executed":action_index,"failed":entry,"remaining":actions.len()-action_index-1})
                    }));
                }
                action_results.push(entry);
            }
            results.push(action_results);
        }
        let value = if action == "teach_batch" {
            json!({"stepsCompleted":steps.len(),"results":results})
        } else {
            json!({"executed":results[0].len(),"results":results.remove(0)})
        };
        let mut result = result_json(value);
        if any_actions {
            let capture = self
                .call_display_protocol(
                    "screenshot",
                    Utf16JsonProjection::from_serde_value(&json!({})).unwrap(),
                    ctx,
                    tx,
                )
                .await?;
            if !capture.is_error {
                let mut wire = protocol_result::wire(&result);
                let blocks = protocol_result::wire(&capture)["content"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                wire["content"].as_array_mut().unwrap().extend(blocks);
                result = protocol_result::from_wire(wire);
            }
        }
        Ok(result)
    }
}

fn interaction_request(interaction: ComputerInteraction) -> ComputerAccessRequest {
    ComputerAccessRequest {
        reason: "Computer use".into(),
        apps: vec![],
        tier: AppTier::Full.to_bridge(),
        clipboard_read: false,
        clipboard_write: false,
        system_key_combos: false,
        tcc_state: None,
        interaction: Some(interaction),
    }
}
fn validate_teach_step(step: &Value, label: &str) -> Option<String> {
    for name in ["explanation", "next_preview"] {
        if !step.get(name).is_some_and(Value::is_string) {
            return Some(format!("{label}: \"{name}\" must be a string."));
        }
    }
    let Some(actions) = step.get("actions").and_then(Value::as_array) else {
        return Some(format!(
            "{label}: \"actions\" must be an array (empty is allowed)."
        ));
    };
    const ALLOWED: &[&str] = &[
        "key",
        "type",
        "mouse_move",
        "left_click",
        "left_click_drag",
        "right_click",
        "middle_click",
        "double_click",
        "triple_click",
        "scroll",
        "hold_key",
        "screenshot",
        "cursor_position",
        "left_mouse_down",
        "left_mouse_up",
        "wait",
    ];
    for (index, action) in actions.iter().enumerate() {
        if !action.is_object() {
            return Some(format!("{label}: actions[{index}] must be an object"));
        }
        let Some(name) = action.get("action").and_then(Value::as_str) else {
            return Some(format!("{label}: actions[{index}].action must be a string"));
        };
        if !ALLOWED.contains(&name) {
            return Some(format!(
                "{label}: actions[{index}].action=\"{name}\" is not allowed. Allowed: {}.",
                ALLOWED.join(", ")
            ));
        }
    }
    if let Some(anchor) = step.get("anchor") {
        if !anchor.as_array().is_some_and(|a| {
            a.len() == 2 && a.iter().all(|v| v.as_f64().is_some_and(f64::is_finite))
        }) {
            return Some(format!(
                "{label}: \"anchor\" must be a [x, y] number tuple or omitted."
            ));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution_tests::{ctx, make_tool, RecordingBackend};
    use std::sync::Arc;

    #[derive(Default)]
    struct EscapeBackend {
        inner: RecordingBackend,
        callback: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
        registrations: std::sync::atomic::AtomicUsize,
        removals: std::sync::atomic::AtomicUsize,
    }
    #[async_trait]
    impl lingxi_core::host::computer_control::ComputerControl for EscapeBackend {
        fn register_escape_abort(&self, callback: Arc<dyn Fn() + Send + Sync>) -> bool {
            self.registrations
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            *self.callback.lock().unwrap() = Some(callback);
            true
        }
        fn unregister_escape_abort(&self) {
            self.removals
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.callback.lock().unwrap().take();
        }
        async fn screenshot(
            &self,
        ) -> Result<lingxi_core::host::computer_control::Screenshot, ComputerError> {
            self.inner.screenshot().await
        }
        async fn display_size(&self) -> Result<(u32, u32), ComputerError> {
            self.inner.display_size().await
        }
        async fn mouse_move(&self, x: u32, y: u32) -> Result<(), ComputerError> {
            self.inner.mouse_move(x, y).await
        }
        async fn left_click(&self, x: u32, y: u32) -> Result<(), ComputerError> {
            self.inner.left_click(x, y).await
        }
        async fn right_click(&self, x: u32, y: u32) -> Result<(), ComputerError> {
            self.inner.right_click(x, y).await
        }
        async fn double_click(&self, x: u32, y: u32) -> Result<(), ComputerError> {
            self.inner.double_click(x, y).await
        }
        async fn type_text(&self, text: String) -> Result<(), ComputerError> {
            self.inner.type_text(text).await
        }
        async fn key(&self, key: String) -> Result<(), ComputerError> {
            self.inner.key(key).await
        }
        async fn scroll(&self, x: u32, y: u32, dx: i32, dy: i32) -> Result<(), ComputerError> {
            self.inner.scroll(x, y, dx, dy).await
        }
    }

    #[tokio::test]
    async fn teach_exit_releases_sequence_and_allows_another_owner() {
        let backend = Arc::new(RecordingBackend::default());
        let mut tool = make_tool(backend.clone());
        tool.access_resolver = Arc::new(Decisions("exit".into()));
        let owner = ctx();
        tool.begin_computer_sequence(&owner).await.unwrap();
        tool.protocol.lock().unwrap().teach_active = true;
        assert!(tool.runtime.lock().unwrap().lease.is_some());
        let result = tool
            .call_protocol(
                "teach_step",
                test_projection(&json!({
                    "explanation":"Review this action", "next_preview":"Type approved text",
                    "actions":[{"action":"type", "text":"must not be typed"}]
                })),
                owner.clone(),
                tool_api::test_support::fresh_tx(),
            )
            .await
            .unwrap();
        assert!(!result.is_error);
        assert!(!tool.protocol.lock().unwrap().teach_active);
        {
            let runtime = tool.runtime.lock().unwrap();
            assert!(!runtime.sequence);
            assert!(runtime.sequence_generation.is_none());
            assert!(runtime.lease.is_none());
            assert!(runtime.owner.is_none());
        }
        assert!(backend.events().is_empty(), "Exit must precede any effects");
        let mut next = ctx();
        next.agent_id = Some(lingxi_core::types::AgentId::new());
        tool.begin_computer_sequence(&next)
            .await
            .expect("Exit releases the first owner's lease");
        tool.cleanup_computer_inputs(&next).await.unwrap();
    }

    #[tokio::test]
    async fn unconditional_cleanup_releases_sequence_and_escape_even_after_cancel() {
        use std::sync::atomic::Ordering;
        let mut tool = make_tool(Arc::new(RecordingBackend::default()));
        let backend = Arc::new(EscapeBackend::default());
        tool.ctx.computer_control = Some(backend.clone());
        let mut owner = ctx();
        owner.cancel = Some(tokio_util::sync::CancellationToken::new());
        tool.begin_computer_sequence(&owner).await.unwrap();
        tool.ensure_escape_registration(&owner, &tool_api::test_support::fresh_tx())
            .await
            .unwrap();
        tool.protocol.lock().unwrap().teach_active = true;
        assert!(backend.callback.lock().unwrap().is_some());
        assert!(tool.runtime.lock().unwrap().sequence);
        owner.cancel.as_ref().unwrap().cancel();
        tool.cleanup_computer_inputs(&owner).await.unwrap();
        {
            let protocol = tool.protocol.lock().unwrap();
            assert!(!protocol.escape_active);
            assert!(protocol.escape_owner.is_none());
            assert!(!protocol.teach_active);
        }
        {
            let runtime = tool.runtime.lock().unwrap();
            assert!(!runtime.sequence);
            assert!(runtime.sequence_generation.is_none());
            assert!(runtime.lease.is_none());
            assert!(runtime.owner.is_none());
        }
        assert!(backend.callback.lock().unwrap().is_none());
        assert_eq!(backend.registrations.load(Ordering::SeqCst), 1);
        assert_eq!(backend.removals.load(Ordering::SeqCst), 1);
        tool.cleanup_computer_inputs(&owner).await.unwrap();
        assert_eq!(
            backend.removals.load(Ordering::SeqCst),
            1,
            "cleanup is idempotent"
        );
    }

    #[tokio::test]
    async fn background_preference_declined_takeover_posts_no_input_or_capture() {
        let backend = Arc::new(RecordingBackend::default());
        let mut tool = make_tool(backend.clone());
        tool.access_resolver = Arc::new(Decisions("decline".into()));
        tool.protocol.lock().unwrap().preferred_mode = Some("background".into());
        for (action, input) in [
            ("type", json!({"text":"must not be typed"})),
            ("screenshot", json!({})),
        ] {
            let result = tool
                .call_protocol(
                    action,
                    test_projection(&input),
                    ctx(),
                    tool_api::test_support::fresh_tx(),
                )
                .await
                .unwrap();
            assert!(result.is_error, "declined takeover must block {action}");
            assert!(protocol_result::wire(&result)["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("Full-screen control was not approved"));
            assert!(backend.events().is_empty());
            assert!(backend.captured_apps().is_empty());
            let runtime = tool.runtime.lock().unwrap();
            assert!(!runtime.sequence);
            assert!(runtime.lease.is_none());
            let protocol = tool.protocol.lock().unwrap();
            assert!(!protocol.takeover_approved);
            assert!(!protocol.escape_active);
        }
    }

    #[tokio::test]
    async fn escape_cancels_current_distinct_and_nested_tokens_only_for_its_owner() {
        use escape_protocol::CallFlight;
        use tokio_util::sync::CancellationToken;
        let mut tool = make_tool(Arc::new(RecordingBackend::default()));
        let backend = Arc::new(EscapeBackend::default());
        tool.ctx.computer_control = Some(backend.clone());
        let context = ctx();
        let owner = ComputerTool::owner(&context).await;
        let completed = CancellationToken::new();
        let first = CallFlight::begin(
            &tool.calls_in_flight,
            &tool.escape_calls,
            owner.clone(),
            Some(completed.clone()),
        );
        tool.ensure_escape_registration(&context, &tool_api::test_support::fresh_tx())
            .await
            .unwrap();
        let escape = backend.callback.lock().unwrap().as_ref().unwrap().clone();
        drop(first);
        let current = CancellationToken::new();
        let nested = CancellationToken::new();
        let unrelated = CancellationToken::new();
        let _current = CallFlight::begin(
            &tool.calls_in_flight,
            &tool.escape_calls,
            owner.clone(),
            Some(current.clone()),
        );
        let _nested = CallFlight::begin(
            &tool.calls_in_flight,
            &tool.escape_calls,
            owner,
            Some(nested.clone()),
        );
        let _unrelated = CallFlight::begin(
            &tool.calls_in_flight,
            &tool.escape_calls,
            "different-owner".into(),
            Some(unrelated.clone()),
        );
        escape();
        assert!(current.is_cancelled());
        assert!(nested.is_cancelled());
        assert!(
            !completed.is_cancelled(),
            "completed call token must not remain registered"
        );
        assert!(
            !unrelated.is_cancelled(),
            "Escape is scoped to the registration owner"
        );
        tool.cleanup_computer_inputs(&context).await.unwrap();
    }

    #[tokio::test]
    async fn escape_while_idle_does_not_abort_the_next_call() {
        use escape_protocol::CallFlight;
        use tokio_util::sync::CancellationToken;
        let mut tool = make_tool(Arc::new(RecordingBackend::default()));
        let backend = Arc::new(EscapeBackend::default());
        tool.ctx.computer_control = Some(backend.clone());
        let context = ctx();
        tool.ensure_escape_registration(&context, &tool_api::test_support::fresh_tx())
            .await
            .unwrap();
        let escape = backend.callback.lock().unwrap().as_ref().unwrap().clone();
        escape();
        assert!(!tool
            .escape_aborted
            .load(std::sync::atomic::Ordering::SeqCst));
        let token = CancellationToken::new();
        let flight = CallFlight::begin(
            &tool.calls_in_flight,
            &tool.escape_calls,
            ComputerTool::owner(&context).await,
            Some(token.clone()),
        );
        assert!(!token.is_cancelled());
        assert!(tool.check_cancel(&context).is_ok());
        drop(flight);
        assert_eq!(
            tool.calls_in_flight
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        tool.cleanup_computer_inputs(&context).await.unwrap();
    }

    #[tokio::test]
    async fn retired_escape_callback_cannot_abort_reinstalled_registration() {
        use escape_protocol::CallFlight;
        use tokio_util::sync::CancellationToken;
        let mut tool = make_tool(Arc::new(RecordingBackend::default()));
        let backend = Arc::new(EscapeBackend::default());
        tool.ctx.computer_control = Some(backend.clone());
        let context = ctx();
        tool.ensure_escape_registration(&context, &tool_api::test_support::fresh_tx())
            .await
            .unwrap();
        let stale = backend.callback.lock().unwrap().as_ref().unwrap().clone();
        tool.cleanup_computer_inputs(&context).await.unwrap();
        tool.ensure_escape_registration(&context, &tool_api::test_support::fresh_tx())
            .await
            .unwrap();
        let token = CancellationToken::new();
        let _flight = CallFlight::begin(
            &tool.calls_in_flight,
            &tool.escape_calls,
            ComputerTool::owner(&context).await,
            Some(token.clone()),
        );
        stale();
        assert!(!token.is_cancelled());
        assert!(!tool
            .escape_aborted
            .load(std::sync::atomic::Ordering::SeqCst));
        let current = backend.callback.lock().unwrap().as_ref().unwrap().clone();
        current();
        assert!(token.is_cancelled(), "the replacement hook must still work");
        tool.cleanup_computer_inputs(&context).await.unwrap();
        assert!(!tool
            .escape_aborted
            .load(std::sync::atomic::Ordering::SeqCst));
        current();
        assert!(
            !tool
                .escape_aborted
                .load(std::sync::atomic::Ordering::SeqCst),
            "queued callbacks must remain inert after teardown"
        );
    }
    struct Decisions(String);
    #[async_trait]
    impl ComputerAccessResolver for Decisions {
        fn supports_interactions(&self) -> bool {
            true
        }
        async fn resolve(&self, request: ComputerAccessRequest) -> ComputerAccessResponse {
            let action = match request.interaction {
                Some(ComputerInteraction::Takeover { .. }) => {
                    (self.0 == "allow").then(|| "allow".to_owned())
                }
                Some(ComputerInteraction::TeachStep { .. }) => Some(self.0.clone()),
                _ => None,
            };
            ComputerAccessResponse {
                interaction_action: action,
                ..Default::default()
            }
        }
    }
    #[tokio::test]
    async fn interaction_model_results_match_native_325() {
        let fixture: Value =
            serde_json::from_str(include_str!("../fixtures/interactions-2.1.325.json")).unwrap();
        for case in fixture["cases"].as_array().unwrap() {
            let backend = Arc::new(RecordingBackend::default());
            let mut tool = make_tool(backend.clone());
            tool.access_resolver = Arc::new(Decisions(case["decision"].as_str().unwrap().into()));
            {
                let mut protocol = tool.protocol.lock().unwrap();
                protocol.teach_active = case["active"].as_bool().unwrap_or(false);
                protocol.takeover_approved = case["approved"].as_bool().unwrap_or(false);
            }
            let mut context = ctx();
            context.is_scheduled_turn = case["scheduled"].as_bool().unwrap_or(false);
            let result = tool
                .call_protocol(
                    case["action"].as_str().unwrap(),
                    test_projection(&case["input"]),
                    context,
                    tool_api::test_support::fresh_tx(),
                )
                .await
                .unwrap();
            let expected = &case["expected"];
            assert_eq!(test_data(&result.data), expected["content"], "{case}");
            assert_eq!(
                result.is_error,
                expected
                    .get("isError")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                "{case}"
            );
            assert!(
                backend.events().is_empty(),
                "interactive/validation-only cases must not post effects"
            );
        }
    }
}
