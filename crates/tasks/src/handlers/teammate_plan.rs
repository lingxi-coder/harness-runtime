//! Teammate-owned permission state and lead review requests.
use async_trait::async_trait;
use lingxi_core::host::mailbox::{MailboxMessage, MailboxRouterHandle};
use lingxi_core::host::teammate_plan::{PlanApprovalResponse, TeammatePlanRequester};
use lingxi_core::host::tool_invoker::{SubagentInvocationContext, ToolInvokerError};
use lingxi_core::host::{FileSystem, ToolInvoker};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

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
    pub(super) async fn apply(&self, response: PlanApprovalResponse) -> Option<String> {
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
    fn apply_verdict(&self, response: PlanApprovalResponse) -> Option<String> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let pending = state.pending.take()?;
        if pending != response.request_id {
            return Some("[Plan Rejected] The team lead's verdict was for a different request, not this plan. Call ExitPlanMode again to resubmit it for approval.".into());
        }
        let feedback = response.feedback.filter(|s| !s.is_empty());
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
                |f| format!("[Plan Approved] {f}"),
            ))
        } else {
            Some(format!(
                "[Plan Rejected] {}",
                feedback.unwrap_or_else(|| "Please revise your plan".into())
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
    async fn invoke(
        &self,
        name: &str,
        input: Value,
        context: SubagentInvocationContext,
    ) -> Result<Value, ToolInvokerError> {
        self.inner.invoke(name, input, self.context(context)).await
    }
    async fn invoke_with_workspace_lease(
        &self,
        name: &str,
        input: Value,
        context: SubagentInvocationContext,
        lease: Option<u64>,
    ) -> Result<Value, ToolInvokerError> {
        self.inner
            .invoke_with_workspace_lease(name, input, self.context(context), lease)
            .await
    }
    async fn invoke_detailed(
        &self,
        name: &str,
        input: Value,
        context: SubagentInvocationContext,
        lease: Option<u64>,
    ) -> Result<lingxi_core::host::tool_invoker::ToolInvocationResult, ToolInvokerError> {
        self.inner
            .invoke_detailed(name, input, self.context(context), lease)
            .await
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    async fn cleanup_computer_inputs(
        &self,
        agent_id: lingxi_core::types::AgentId,
        origin_session_id: Option<lingxi_core::types::SessionId>,
    ) -> Result<(), lingxi_core::host::tool_invoker::ToolInvokerError> {
        self.inner
            .cleanup_computer_inputs(agent_id, origin_session_id)
            .await
    }
}
#[async_trait]
impl TeammatePlanRequester for PlanAwareInvoker {
    fn writable_plan_path(&self) -> Option<&str> {
        Some(&self.plan_path)
    }

    async fn submit(&self, input: Value) -> Result<Value, String> {
        let plan = match input.get("plan").and_then(Value::as_str) {
            Some(plan) => {
                let path = std::path::Path::new(&self.plan_path);
                let parent = path.parent().ok_or("Plan file has no parent directory")?;
                let name = path.file_name().ok_or("Plan file has no file name")?;
                self.fs
                    .write_file_rooted_atomic(parent, std::path::Path::new(name), plan)
                    .await
                    .map_err(|error| error.to_string())?;
                plan.to_owned()
            }
            None => self
                .fs
                .read_file(&self.plan_path, None, None)
                .await
                .map(|file| file.content)
                .unwrap_or_default(),
        };
        if plan.is_empty() {
            return Err(format!("No plan file found at {}. Please write your plan to this file before calling ExitPlanMode.",self.plan_path));
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
        let payload = json!({"type":"plan_approval_request","from":self.name,"timestamp":lingxi_core::types::iso8601::iso8601_utc(now),"planFilePath":self.plan_path,"planContent":plan,"requestId":request_id});
        if self
            .mailbox
            .route(
                &self.name,
                "team-lead",
                MailboxMessage {
                    message_id: request_id.clone(),
                    content: payload.to_string(),
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
        let model_content=format!("Your plan has been submitted to the team lead for approval.\n\nPlan file: {}\n\n**What happens next:**\n1. Wait for the team lead to review your plan\n2. You will receive a message in your inbox with approval/rejection\n3. If approved, you can proceed with implementation\n4. If rejected, refine your plan based on the feedback\n\n**Important:** Do NOT proceed until you receive approval. Check your inbox for response.\n\nRequest ID: {request_id}",self.plan_path);
        Ok(
            json!({"plan":plan,"isAgent":true,"filePath":self.plan_path,"awaitingLeaderApproval":true,"requestId":request_id,"model_content":model_content}),
        )
    }
}
