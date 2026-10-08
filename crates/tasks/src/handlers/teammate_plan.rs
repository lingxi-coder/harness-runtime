//! Teammate-owned permission state and lead review requests.
use async_trait::async_trait;
use lingxi_core::host::mailbox::{MailboxMessage, MailboxRouterHandle};
use lingxi_core::host::teammate_plan::{PlanApprovalResponse, TeammatePlanRequester};
use lingxi_core::host::tool_invoker::{SubagentInvocationContext, ToolInvokerError};
use lingxi_core::host::{FileSystem, ToolInvoker};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use std::sync::{Arc, Mutex};

fn root_string_from_units(code_units: Vec<u16>) -> Utf16JsonProjection {
    Utf16JsonProjection::root_string(String::from_utf16_lossy(&code_units), code_units)
        .expect("display is derived from the exact UTF-16 code units")
}

fn prefixed_text(prefix: &str, value: &Utf16JsonProjection) -> Utf16JsonProjection {
    let mut units = prefix.encode_utf16().collect::<Vec<_>>();
    units.extend(
        value
            .string_units("")
            .expect("plan feedback is a root-string projection"),
    );
    root_string_from_units(units)
}

fn json_bool(value: bool) -> Utf16JsonProjection {
    Utf16JsonProjection::parse(if value { "true" } else { "false" })
        .expect("JSON boolean literal is valid")
}

pub(super) struct PlanAwareInvoker {
    inner: Arc<dyn ToolInvoker>,
    mailbox: Arc<dyn MailboxRouterHandle>,
    fs: Arc<dyn FileSystem>,
    name: String,
    team_name: String,
    plan_path: String,
    state: Mutex<PlanState>,
    permission_gate: Option<Arc<dyn lingxi_core::host::PermissionGate>>,
    status_sink: Option<(Arc<dyn super::TaskStatusSink>, String)>,
}
struct PlanState {
    mode: String,
    pending: Option<String>,
    last_request_millis: u128,
}
impl PlanAwareInvoker {
    pub(super) fn new(
        inner: Arc<dyn ToolInvoker>,
        mailbox: Arc<dyn MailboxRouterHandle>,
        fs: Arc<dyn FileSystem>,
        name: String,
        team_name: String,
        plan_path: String,
    ) -> Self {
        Self {
            inner,
            mailbox,
            fs,
            name,
            team_name,
            plan_path,
            permission_gate: None,
            status_sink: None,
            state: Mutex::new(PlanState {
                mode: "plan".into(),
                pending: None,
                last_request_millis: 0,
            }),
        }
    }
    pub(super) fn with_permission_gate(
        mut self,
        gate: Option<Arc<dyn lingxi_core::host::PermissionGate>>,
    ) -> Self {
        self.permission_gate = gate;
        self
    }
    pub(super) fn with_status_sink(
        mut self,
        sink: Arc<dyn super::TaskStatusSink>,
        task_id: String,
    ) -> Self {
        self.status_sink = Some((sink, task_id));
        self
    }
    async fn publish_awaiting(&self, awaiting: bool) {
        if let Some((sink, task_id)) = &self.status_sink {
            sink.set_awaiting_plan_approval(task_id, awaiting).await;
        }
    }
    pub(super) async fn apply(
        &self,
        response: PlanApprovalResponse,
    ) -> Option<Utf16JsonProjection> {
        let outcome = self.apply_verdict(response);
        if outcome.is_some() {
            self.publish_awaiting(false).await;
        }
        outcome
    }
    pub(super) fn permission_mode(&self) -> String {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .mode
            .clone()
    }
    pub(super) fn plan_path(&self) -> &str {
        &self.plan_path
    }
    pub(super) fn awaiting(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pending
            .is_some()
    }
    fn apply_verdict(&self, response: PlanApprovalResponse) -> Option<Utf16JsonProjection> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let pending = state.pending.take()?;
        if pending != response.request_id {
            return Some("[Plan Rejected] The team lead's verdict was for a different request, not this plan. Call ExitPlanMode again to resubmit it for approval.".into());
        }
        let feedback = response.feedback.filter(|value| {
            value
                .string_units("")
                .is_some_and(|units| !units.is_empty())
        });
        if response.approved {
            // Oracle $2o: parse a known mode, then downgrade unavailable
            // bypass/auto using the host's live availability gates.
            state.mode = match response.permission_mode.as_deref() {
                Some("acceptEdits") => "acceptEdits",
                Some("plan") => "plan",
                Some("dontAsk") => "dontAsk",
                Some("auto")
                    if self
                        .permission_gate
                        .as_ref()
                        .is_some_and(|gate| gate.can_request_auto_mode()) =>
                {
                    "auto"
                }
                Some("bypassPermissions")
                    if self
                        .permission_gate
                        .as_ref()
                        .is_some_and(|gate| gate.can_request_bypass_permissions()) =>
                {
                    "bypassPermissions"
                }
                _ => "default",
            }
            .into();
            Some(feedback.map_or_else(
                || "[Plan Approved] You can now proceed with implementation".into(),
                |value| prefixed_text("[Plan Approved] ", &value),
            ))
        } else {
            Some(feedback.map_or_else(
                || "[Plan Rejected] Please revise your plan".into(),
                |value| prefixed_text("[Plan Rejected] ", &value),
            ))
        }
    }
    fn context(&self, mut context: SubagentInvocationContext) -> SubagentInvocationContext {
        context.mode_override = Some(
            self.state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .mode
                .clone(),
        );
        context
    }
}
#[async_trait]
impl ToolInvoker for PlanAwareInvoker {
    async fn cleanup_computer_inputs(
        &self,
        agent_id: lingxi_core::types::AgentId,
        origin_session_id: Option<lingxi_core::types::SessionId>,
    ) -> Result<(), lingxi_core::host::tool_invoker::ToolInvokerError> {
        self.inner
            .cleanup_computer_inputs(agent_id, origin_session_id)
            .await
    }

    fn permission_mode(&self) -> Option<String> {
        Some(PlanAwareInvoker::permission_mode(self))
    }

    fn tool_is_concurrency_safe(&self, name: &str, input: &Utf16JsonProjection) -> Option<bool> {
        self.inner.tool_is_concurrency_safe(name, input)
    }

    async fn invoke(
        &self,
        name: &str,
        input: Utf16JsonProjection,
        context: SubagentInvocationContext,
    ) -> Result<Utf16JsonProjection, ToolInvokerError> {
        self.inner.invoke(name, input, self.context(context)).await
    }
    async fn invoke_with_workspace_lease(
        &self,
        name: &str,
        input: Utf16JsonProjection,
        context: SubagentInvocationContext,
        lease: Option<u64>,
    ) -> Result<Utf16JsonProjection, ToolInvokerError> {
        self.inner
            .invoke_with_workspace_lease(name, input, self.context(context), lease)
            .await
    }
    async fn invoke_detailed(
        &self,
        name: &str,
        input: Utf16JsonProjection,
        context: SubagentInvocationContext,
        lease: Option<u64>,
    ) -> Result<lingxi_core::host::tool_invoker::ToolInvocationResult, ToolInvokerError> {
        self.inner
            .invoke_detailed(name, input, self.context(context), lease)
            .await
    }
    async fn invoke_supplied_detailed(
        &self,
        name: &str,
        input: Utf16JsonProjection,
        context: SubagentInvocationContext,
        lease: Option<u64>,
        supplied: Arc<dyn std::any::Any + Send + Sync>,
    ) -> Result<lingxi_core::host::tool_invoker::ToolInvocationResult, ToolInvokerError> {
        self.inner
            .invoke_supplied_detailed(name, input, self.context(context), lease, supplied)
            .await
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
#[async_trait]
impl TeammatePlanRequester for PlanAwareInvoker {
    fn writable_plan_path(&self) -> Option<&str> {
        Some(&self.plan_path)
    }

    async fn submit(
        &self,
        input: Utf16JsonProjection,
    ) -> Result<Utf16JsonProjection, Utf16JsonProjection> {
        let plan_units = input.string_units("/plan");
        let plan = match plan_units {
            Some(units) => {
                let plan_text = String::from_utf16(&units).map_err(|_| {
                    Utf16JsonProjection::from(
                        "Plan text contains invalid UTF-16 and cannot be written to a UTF-8 file.",
                    )
                })?;
                let path = std::path::Path::new(&self.plan_path);
                let parent = path.parent().ok_or_else(|| {
                    Utf16JsonProjection::from("Plan file has no parent directory")
                })?;
                let name = path
                    .file_name()
                    .ok_or_else(|| Utf16JsonProjection::from("Plan file has no file name"))?;
                self.fs
                    .write_file_rooted_atomic(parent, std::path::Path::new(name), &plan_text)
                    .await
                    .map_err(|error| Utf16JsonProjection::from(error.to_string()))?;
                root_string_from_units(units)
            }
            None => self
                .fs
                .read_file(&self.plan_path, None, None)
                .await
                .map(|file| Utf16JsonProjection::from(file.content))
                .unwrap_or_else(|_| Utf16JsonProjection::from("")),
        };
        if plan.string_units("").is_some_and(|units| units.is_empty()) {
            return Err(format!("No plan file found at {}. Please write your plan to this file before calling ExitPlanMode.",self.plan_path).into());
        }
        let now = std::time::SystemTime::now();
        let millis = now
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let request_id = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let millis = millis.max(state.last_request_millis.saturating_add(1));
            state.last_request_millis = millis;
            let id = format!("plan_approval-{millis}@{}@{}", self.name, self.team_name);
            id
        };
        let payload = Utf16JsonProjection::object(vec![
            ("type".into(), "plan_approval_request".into()),
            ("from".into(), self.name.clone().into()),
            (
                "timestamp".into(),
                lingxi_core::types::iso8601::iso8601_utc(now).into(),
            ),
            ("planFilePath".into(), self.plan_path.clone().into()),
            ("planContent".into(), plan.clone()),
            ("requestId".into(), request_id.clone().into()),
        ])
        .expect("generated plan approval payload has unique object keys");
        if self
            .mailbox
            .route(
                &self.name,
                "team-lead",
                MailboxMessage {
                    message_id: request_id.clone(),
                    content: payload,
                    summary: None,
                    timestamp: now,
                    color: None,
                },
            )
            .await
            .is_err()
        {
            return Err("Failed to write the plan approval request to the lead's inbox — plan not submitted; try again".into());
        }
        self.state.lock().unwrap_or_else(|e| e.into_inner()).pending = Some(request_id.clone());
        self.publish_awaiting(true).await;
        let model_content = format!("Your plan has been submitted to the team lead for approval.\n\nPlan file: {}\n\n**What happens next:**\n1. Wait for the team lead to review your plan\n2. You will receive a message in your inbox with approval/rejection\n3. If approved, you can proceed with implementation\n4. If rejected, refine your plan based on the feedback\n\n**Important:** Do NOT proceed until you receive approval. Check your inbox for response.\n\nRequest ID: {request_id}", self.plan_path);
        Ok(Utf16JsonProjection::object(vec![
            ("plan".into(), plan),
            ("isAgent".into(), json_bool(true)),
            ("filePath".into(), self.plan_path.clone().into()),
            ("awaitingLeaderApproval".into(), json_bool(true)),
            ("requestId".into(), request_id.into()),
            ("model_content".into(), model_content.into()),
        ])
        .expect("generated plan response has unique object keys"))
    }
}

#[cfg(test)]
mod source_feedback_tests {
    use super::*;

    #[test]
    fn verdict_prefix_keeps_source_feedback_code_units() {
        let feedback = Utf16JsonProjection::parse(r#""review \ud800 details""#).unwrap();
        let prefixed = prefixed_text("[Plan Rejected] ", &feedback);
        let expected = "[Plan Rejected] review "
            .encode_utf16()
            .chain([0xd800])
            .chain(" details".encode_utf16())
            .collect::<Vec<_>>();
        assert_eq!(prefixed.string_units(""), Some(expected));
    }
}
