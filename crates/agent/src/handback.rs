//! Private ordinary-Agent reporting tool and per-run contract capability.

use async_trait::async_trait;
use lingxi_core::host::handback::*;
use lingxi_core::host::task_registry::TaskRegistryHandle;
use lingxi_core::types::{AgentId, MessageId};
use serde_json::{Value, json};
use std::sync::{Arc, OnceLock};
use tokio::sync::Mutex;
use tool_api::tool_trait::{
    DescriptionOptions, InterruptBehavior, PromptOptions, ToolCallResult, ToolError,
    ToolStaticContext, ValidationError,
};
use tool_api::{Tool, ToolUseContext};

/// Shared only within one retained child. Tokens are replaced at each logical
/// run; old receipts remain in task persistence rather than banning later runs.
pub struct HandbackRuntime {
    pub registry: Arc<dyn TaskRegistryHandle>,
    pub agent_id: AgentId,
    pub eligible: bool,
    pub caller: Option<AgentId>,
    pub sender_name: String,
    pub sender_id: String,
    pub agent_type: String,
    pub spawn_bypass_gates: crate::permission_mode::SpawnBypassGates,
    /// Immediate parent's enforcing mode supplied by the trusted host. Without
    /// this carrier, each retained run reads the bound invoker's live mode.
    /// Definition fallbacks remain separate from this inheritance anchor.
    pub trusted_parent_permission_mode: Option<permission::PermissionMode>,
    pub ends_turn_enabled: Option<bool>,
    pub report_output: Option<crate::handback_output::HandbackReportOutput>,
    restored: Mutex<Option<HandbackState>>,
    pub restored_history: Mutex<Vec<HandbackState>>,
    token: Mutex<Option<HandbackRunToken>>,
}

impl HandbackRuntime {
    pub fn new(
        registry: Arc<dyn TaskRegistryHandle>,
        agent_id: AgentId,
        caller: Option<AgentId>,
        sender_name: String,
        agent_type: String,
        restored: Option<HandbackState>,
    ) -> Self {
        Self {
            registry,
            agent_id,
            eligible: false,
            caller,
            sender_name,
            sender_id: agent_id.to_string(),
            agent_type,
            spawn_bypass_gates: Default::default(),
            trusted_parent_permission_mode: None,
            ends_turn_enabled: None,
            report_output: None,
            restored: Mutex::new(restored),
            restored_history: Mutex::new(Vec::new()),
            token: Mutex::new(None),
        }
    }

    pub async fn begin(&self, active: bool) -> bool {
        let Some(scope) = self.registry.handback_scope().await else {
            *self.token.lock().await = None;
            return false;
        };
        let previous = self.restored.lock().await.take();
        let token = self
            .registry
            .begin_handback_run(BeginHandbackRun {
                agent_id: self.agent_id,
                scope,
                active,
                caller: self
                    .caller
                    .map(|agent_id| HandbackRecipient::Agent { scope, agent_id }),
                resumer: HandbackRecipient::Main { scope },
                restored_state: previous,
                restored_history: std::mem::take(&mut *self.restored_history.lock().await),
            })
            .await
            .ok();
        let started = token.is_some();
        *self.token.lock().await = token;
        started && active
    }

    pub async fn state(&self) -> Option<HandbackState> {
        let token = self.token.lock().await.clone()?;
        self.registry.handback_state(&token).await
    }

    pub async fn next_bounce(&self) -> Option<u8> {
        let token = self.token.lock().await.clone()?;
        self.registry.next_handback_bounce(&token).await
    }

    pub async fn finalize(
        &self,
        waiting: bool,
        resumable: bool,
    ) -> Option<(HandbackState, Option<String>)> {
        let token = self.token.lock().await.clone()?;
        let state = self.registry.handback_state(&token).await?;
        if !state.active {
            return Some((state, None));
        }
        let flagged = state.disposition == Some(HandbackDisposition::Flagged);
        let (disposition, text) = if state.receipt.is_some() {
            (
                if flagged {
                    HandbackDisposition::Flagged
                } else {
                    HandbackDisposition::Send
                },
                handback_pointer(flagged, &self.sender_name),
            )
        } else {
            (
                HandbackDisposition::Withheld,
                if waiting {
                    HANDBACK_INTERIM.into()
                } else {
                    handback_withheld(resumable)
                },
            )
        };
        self.registry
            .set_handback_disposition(&token, disposition)
            .await;
        self.registry
            .handback_state(&token)
            .await
            .map(|state| (state, Some(text)))
    }
}

/// This instance is offered only to its owning child and dispatched via the
/// bound invoker's supplied-tool entrypoint, including classifier-only review.
pub struct SubagentHandbackTool(pub Arc<HandbackRuntime>);

fn handback_result(success: bool, message: &str) -> ToolCallResult {
    #[derive(serde::Serialize)]
    struct NativeResult<'a> {
        success: bool,
        message: &'a str,
    }
    let mut result = ToolCallResult::from_data(json!({"success":success,"message":message}));
    result.model_content = Some(
        serde_json::to_string(&NativeResult { success, message })
            .expect("native Handback result serialization"),
    );
    result
}

#[async_trait]
impl Tool for SubagentHandbackTool {
    fn name(&self) -> &str {
        HANDBACK_TOOL_NAME
    }
    fn input_schema(&self) -> &Value {
        static INPUT: OnceLock<Value> = OnceLock::new();
        INPUT.get_or_init(|| json!({"type":"object","properties":{"message":{"type":"string","description":"Your full report for your caller"}},"required":["message"],"additionalProperties":false}))
    }
    fn output_schema(&self) -> Option<&Value> {
        static OUTPUT: OnceLock<Value> = OnceLock::new();
        Some(OUTPUT.get_or_init(|| json!({"type":"object","properties":{"success":{"type":"boolean"},"message":{"type":"string"}},"required":["success","message"],"additionalProperties":false})))
    }
    fn native_input_validation(&self) -> bool {
        true
    }
    fn parse_native_input(
        &self,
        input: &Value,
    ) -> Option<Result<Value, tool_api::native_schema::NativeSchemaError>> {
        let stripped = input
            .as_object()
            .map(|object| {
                let mut parsed = serde_json::Map::new();
                if let Some(message) = object.get("message") {
                    parsed.insert("message".into(), message.clone());
                }
                Value::Object(parsed)
            })
            .unwrap_or_else(|| input.clone());
        tool_api::native_schema::validate_flat_input(self.name(), self.input_schema(), &stripped)
            .map(|result| result.map(|()| stripped))
    }
    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        true
    }
    fn always_load(&self) -> bool {
        true
    }
    fn search_hint(&self) -> Option<&str> {
        Some("deliver your final report to the agent that spawned you")
    }
    fn user_facing_name(&self) -> Option<&str> {
        Some(HANDBACK_TOOL_NAME)
    }
    fn max_result_size_chars(&self) -> usize {
        100_000
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        false
    }
    fn is_read_only(&self, _: &Value) -> bool {
        false
    }
    fn interrupt_behavior(&self, _: &Value) -> InterruptBehavior {
        InterruptBehavior::Block
    }
    fn classifier_only(&self) -> Option<lingxi_core::host::permission_gate::ClassifierOnlyPolicy> {
        Some(lingxi_core::host::permission_gate::ClassifierOnlyPolicy {
            on_block: lingxi_core::host::permission_gate::ClassifierOnlyOnBlock::Flag,
        })
    }
    fn classifier_only_action(&self, input: &Value) -> String {
        handback_classifier_input(input["message"].as_str().unwrap_or_default())
    }
    async fn validate_input(
        &self,
        input: &Value,
        _: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        let Some(message) = input["message"].as_str() else {
            return Err(ValidationError("message must be a string".into()));
        };
        if lingxi_core::host::instruction_memory_sanitize::js_trim(message).is_empty() {
            return Err(ValidationError("message must not be empty".into()));
        }
        Ok(())
    }
    async fn check_permissions(
        &self,
        _: &Value,
        _: &ToolUseContext,
    ) -> permission::PermissionResult {
        permission::PermissionResult::Allow {
            reason: permission::PermissionDecisionReason::Other {
                reason: "classifier-only reporting".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: Default::default(),
        }
    }
    async fn description(&self, _: &Value, _: &DescriptionOptions) -> String {
        HANDBACK_DESCRIPTION.into()
    }
    async fn prompt(&self, _: &PromptOptions) -> String {
        HANDBACK_PROMPT.into()
    }
    fn result_ends_turn(&self, result: &ToolCallResult) -> bool {
        self.0.ends_turn_enabled.unwrap_or(true) && result.data["success"] == true
    }
    async fn call(
        &self,
        input: Value,
        ctx: ToolUseContext,
        _: tool_api::progress::ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        if ctx.agent_id != Some(self.0.agent_id) {
            return Ok(handback_result(false, HANDBACK_INACTIVE));
        }
        let Some(token) = self.0.token.lock().await.clone() else {
            return Ok(handback_result(false, HANDBACK_INACTIVE));
        };
        let Some(state) = self.0.registry.handback_state(&token).await else {
            return Ok(handback_result(false, HANDBACK_INACTIVE));
        };
        if !state.active || state.receipt.is_some() {
            return Ok(handback_result(
                false,
                if state.active {
                    HANDBACK_DUPLICATE
                } else {
                    HANDBACK_INACTIVE
                },
            ));
        }
        let Some(review) = ctx.classifier_only_review else {
            return Err(ToolError::Internal(
                "Report has no bound classifier review".into(),
            ));
        };
        let review = match review {
            ReportReview::Blocked { reason } => ReportReview::Blocked {
                reason: lingxi_core::host::subagent_output_guard::sanitize_text(&reason, false)
                    .sanitized,
            },
            other => other,
        };
        let warning = handback_report_warning(&review);
        let message = input["message"].as_str().unwrap_or_default();
        let text =
            lingxi_core::host::subagent_output_guard::sanitize_text(message, false).sanitized;
        let mut inbox_text =
            lingxi_core::host::subagent_output_guard::sanitize_text(message, true).sanitized;
        let mut inbox_utf16 = None;
        if let Some(output) = &self.0.report_output {
            if let Some(pointer) = output
                .substitute(&inbox_text, ctx.tool_use_id.as_ref().map(|id| id.as_str()))
                .await
            {
                inbox_text = pointer.text;
                inbox_utf16 = pointer.utf16;
            }
        }
        let body = warning.as_ref().map_or_else(
            || handback_frame(&inbox_text),
            |warning| {
                format!(
                    "{}\n{}",
                    handback_indent(warning),
                    handback_frame(&inbox_text)
                )
            },
        );
        let body_utf16 = inbox_utf16.map(|units| {
            let mut framed = frame_utf16(&units);
            if let Some(warning) = warning.as_ref() {
                let mut prefix: Vec<u16> = handback_indent(warning).encode_utf16().collect();
                prefix.push(u16::from(b'\n'));
                prefix.append(&mut framed);
                prefix
            } else {
                framed
            }
        });
        let task_id = self.0.agent_id.to_string();
        let outcome = self
            .0
            .registry
            .try_deliver_handback(
                &token,
                PreparedHandbackReport {
                    message_id: MessageId::new(),
                    report: HandbackReport { text, warning },
                    body,
                    body_utf16,
                    sender_name: self.0.sender_name.clone(),
                    sender_id: self.0.sender_id.clone(),
                    sender_task_id: task_id,
                    agent_type: self.0.agent_type.clone(),
                    flagged: review.flagged(),
                },
            )
            .await;
        Ok(handback_result(
            matches!(outcome, HandbackAdmissionOutcome::Admitted(_)),
            outcome.message(),
        ))
    }
}

/// Native cut/fqn over the exact preview units. All line separators are BMP;
/// unmatched surrogate units pass through unchanged to the provider sidecar.
fn frame_utf16(text: &[u16]) -> Vec<u16> {
    let mut framed: Vec<u16> = format!("{HANDBACK_FRAME}\n  ").encode_utf16().collect();
    let mut cursor = 0;
    while cursor < text.len() {
        let unit = text[cursor];
        if unit == u16::from(b'\r') {
            if text.get(cursor + 1) == Some(&u16::from(b'\n')) {
                cursor += 1;
            }
            framed.extend("\n  ".encode_utf16());
        } else if matches!(
            unit,
            0x000a | 0x2028 | 0x2029 | 0x0085 | 0x000b | 0x000c | 0x001c..=0x001e
        ) {
            framed.extend("\n  ".encode_utf16());
        } else {
            framed.push(unit);
        }
        cursor += 1;
    }
    framed
}
