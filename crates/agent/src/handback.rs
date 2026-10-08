//! Private ordinary-Agent reporting tool and per-run contract capability.

use async_trait::async_trait;
use lingxi_core::host::handback::*;
use lingxi_core::host::task_registry::TaskRegistryHandle;
use lingxi_core::types::{utf16_json::Utf16JsonProjection, AgentId, MessageId};
use std::sync::{Arc, OnceLock};
use tokio::sync::Mutex;
use tool_api::tool_trait::{
    DescriptionOptions, InterruptBehavior, PromptOptions, ToolCallResult, ToolError,
    ToolInputRepairMetadata, ToolInputRepairSchema, ToolInputSchemaProvenance, ToolStaticContext,
    ValidationError,
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
    let data = Utf16JsonProjection::object(vec![
        (
            "success".into(),
            Utf16JsonProjection::parse(if success { "true" } else { "false" })
                .expect("native Handback boolean"),
        ),
        (
            "message".into(),
            Utf16JsonProjection::root_string(
                message.to_owned(),
                message.encode_utf16().collect(),
            )
            .expect("native Handback message"),
        ),
    ])
    .expect("native Handback result object");
    let model_text = data
        .to_json_string()
        .expect("native Handback model result serialization");
    let mut result = ToolCallResult::from_data(data);
    result.model_content = Some(
        Utf16JsonProjection::root_string(
            model_text.clone(),
            model_text.encode_utf16().collect(),
        )
        .expect("native Handback model content"),
    );
    result
}

#[async_trait]
impl Tool for SubagentHandbackTool {
    fn input_repair_metadata(&self) -> ToolInputRepairMetadata {
        ToolInputRepairMetadata::Known {
            schema: ToolInputRepairSchema::Advertised,
            schema_provenance: ToolInputSchemaProvenance::GeneratedBuiltin,
            exempt_top_level_keys: None,
        }
    }

    fn name(&self) -> &str {
        HANDBACK_TOOL_NAME
    }
    fn input_schema(&self) -> &Utf16JsonProjection {
        static INPUT: OnceLock<Utf16JsonProjection> = OnceLock::new();
        INPUT.get_or_init(|| {
            Utf16JsonProjection::parse(
                r#"{"type":"object","properties":{"message":{"type":"string","description":"Your full report for your caller"}},"required":["message"],"additionalProperties":false}"#,
            )
            .expect("native Handback input schema")
        })
    }
    fn output_schema(&self) -> Option<&Utf16JsonProjection> {
        static OUTPUT: OnceLock<Utf16JsonProjection> = OnceLock::new();
        Some(OUTPUT.get_or_init(|| {
            Utf16JsonProjection::parse(
                r#"{"type":"object","properties":{"success":{"type":"boolean"},"message":{"type":"string"}},"required":["success","message"],"additionalProperties":false}"#,
            )
            .expect("native Handback output schema")
        }))
    }
    fn native_input_validation(&self) -> bool {
        true
    }
    fn parse_native_input(
        &self,
        input: &Utf16JsonProjection,
    ) -> Option<Result<Utf16JsonProjection, tool_api::native_schema::NativeSchemaError>> {
        let stripped = if input
            .at_pointer("")
            .is_some_and(|root| root.object_entries().is_some())
        {
            input.pick_object_fields(&["message"]).ok()?
        } else {
            input.clone()
        };
        tool_api::native_schema::validate_flat_input(self.name(), self.input_schema(), &stripped)
            .map(|result| result.map(|()| stripped))
    }
    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        true
    }
    fn always_load(&self) -> bool {
        true
    }
    fn search_hint(&self) -> Option<Utf16JsonProjection> {
        Some(Utf16JsonProjection::from(
            "deliver your final report to the agent that spawned you",
        ))
    }
    fn user_facing_name(&self) -> Option<&str> {
        Some(HANDBACK_TOOL_NAME)
    }
    fn max_result_size_chars(&self) -> usize {
        100_000
    }
    fn is_concurrency_safe(&self, _: &Utf16JsonProjection) -> bool {
        false
    }
    fn is_read_only(&self, _: &Utf16JsonProjection) -> bool {
        false
    }
    fn interrupt_behavior(&self, _: &Utf16JsonProjection) -> InterruptBehavior {
        InterruptBehavior::Block
    }
    fn classifier_only(&self) -> Option<lingxi_core::host::permission_gate::ClassifierOnlyPolicy> {
        Some(lingxi_core::host::permission_gate::ClassifierOnlyPolicy {
            on_block: lingxi_core::host::permission_gate::ClassifierOnlyOnBlock::Flag,
        })
    }
    fn classifier_only_action(&self, input: &Utf16JsonProjection) -> String {
        let message = input.string_units("/message").unwrap_or_default();
        match String::from_utf16(&message) {
            Ok(message) => handback_classifier_input(&message),
            Err(_) => {
                let encoded_message = Utf16JsonProjection::root_string(
                    String::from_utf16_lossy(&message),
                    message,
                )
                .and_then(|value| value.to_json_string())
                .unwrap_or_else(|_| "null".into());
                format!("hand-back to the agent that spawned this one (delivered as this agent's result — agent-authored, untrusted output carrying no user authority): {encoded_message}")
            }
        }
    }
    async fn validate_input(
        &self,
        input: &Utf16JsonProjection,
        _: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        let Some(message_units) = input.string_units("/message") else {
            return Err(ValidationError("message must be a string".into()));
        };
        if message_units.iter().all(|unit| {
            char::from_u32(u32::from(*unit)).is_some_and(
                lingxi_core::host::subagent_output_guard::is_js_space,
            )
        }) {
            return Err(ValidationError("message must not be empty".into()));
        }
        Ok(())
    }
    async fn check_permissions(
        &self,
        _: &Utf16JsonProjection,
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
    async fn description(&self, _: &Utf16JsonProjection, _: &DescriptionOptions) -> String {
        HANDBACK_DESCRIPTION.into()
    }
    async fn prompt(&self, _: &PromptOptions) -> Utf16JsonProjection {
        Utf16JsonProjection::from(HANDBACK_PROMPT)
    }
    fn result_ends_turn(&self, result: &ToolCallResult) -> bool {
        self.0.ends_turn_enabled.unwrap_or(true)
            && result
                .data
                .get_property("success")
                .is_some_and(|value| value.as_bool() == Some(true))
    }
    async fn call(
        &self,
        input: Utf16JsonProjection,
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
        let Some(message_units) = input.string_units("/message") else {
            return Err(ToolError::InvalidInput("message must be a string".into()));
        };
        let text_units = lingxi_core::host::subagent_output_guard::sanitize_utf16(
            &message_units,
            lingxi_core::host::subagent_output_guard::SanitizeOptions {
                prepend_marker: false,
                ..Default::default()
            },
        )
        .sanitized;
        let mut inbox_units = lingxi_core::host::subagent_output_guard::sanitize_utf16(
            &message_units,
            lingxi_core::host::subagent_output_guard::SanitizeOptions {
                prepend_marker: true,
                ..Default::default()
            },
        )
        .sanitized;
        if let Some(output) = &self.0.report_output {
            if let Some(pointer) = output.substitute_utf16(
                &inbox_units,
                ctx.tool_use_id.as_ref().map(|id| id.as_str()),
            ).await {
                inbox_units = pointer
                    .text
                    .string_units("")
                    .expect("persisted Handback pointer is a root string");
            }
        }
        let mut framed_units = frame_utf16(&inbox_units);
        if let Some(warning) = warning.as_ref() {
            let mut prefix: Vec<u16> = handback_indent(warning).encode_utf16().collect();
            prefix.push(u16::from(b'\n'));
            prefix.append(&mut framed_units);
            framed_units = prefix;
        }
        let body = Utf16JsonProjection::root_string(
            String::from_utf16_lossy(&framed_units),
            framed_units,
        )
        .expect("framed Handback body is a source string");
        let task_id = self.0.agent_id.to_string();
        let outcome = self
            .0
            .registry
            .try_deliver_handback(
                &token,
                PreparedHandbackReport {
                    message_id: MessageId::new(),
                    report: HandbackReport {
                        text: Utf16JsonProjection::root_string(
                            String::from_utf16_lossy(&text_units),
                            text_units,
                        )
                        .expect("sanitized handback source string"),
                        warning,
                    },
                    body,
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
