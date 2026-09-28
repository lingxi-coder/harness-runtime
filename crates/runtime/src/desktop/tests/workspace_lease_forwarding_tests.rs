use std::sync::{Arc, Mutex as StdMutex};

use platform_api::tool_invoker::{SubagentInvocationContext, ToolInvoker, ToolInvokerError};

/// Terminal invoker that records the lease token it was dispatched with.
struct RecordingInvoker {
    seen: Arc<StdMutex<Option<Option<u64>>>>,
}

#[async_trait::async_trait]
impl ToolInvoker for RecordingInvoker {
    async fn invoke_detailed(
        &self,
        _name: &str,
        _input: serde_json::Value,
        _ctx: SubagentInvocationContext,
        workspace_lease_token: Option<u64>,
    ) -> Result<platform_api::tool_invoker::ToolInvocationResult, ToolInvokerError> {
        *self.seen.lock().unwrap() = Some(workspace_lease_token);
        Ok(platform_api::tool_invoker::ToolInvocationResult {
            is_error: false,
            data: serde_json::json!({"awaitingLeaderApproval": true}),
            model_content: Some("Wait for the team lead to review your plan".into()),
        })
    }

    async fn invoke(
        &self,
        _name: &str,
        _input: serde_json::Value,
        _ctx: SubagentInvocationContext,
    ) -> Result<serde_json::Value, ToolInvokerError> {
        *self.seen.lock().unwrap() = Some(None);
        Ok(serde_json::json!({}))
    }

    async fn invoke_with_workspace_lease(
        &self,
        _name: &str,
        _input: serde_json::Value,
        _ctx: SubagentInvocationContext,
        workspace_lease_token: Option<u64>,
    ) -> Result<serde_json::Value, ToolInvokerError> {
        *self.seen.lock().unwrap() = Some(workspace_lease_token);
        Ok(serde_json::json!({}))
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

fn bare_ctx() -> SubagentInvocationContext {
    SubagentInvocationContext {
        permission_pause_observer: None,
        parent_agent_id: None,
        origin_session_id: None,
        tool_execution_policy: platform_api::tool_invoker::ToolExecutionPolicy::Ordinary,
        agent_name: None,
        team_name: None,
        is_async: false,
        is_non_interactive_session: false,
        can_show_permission_prompts: false,
        cwd: None,
        tool_use_id: None,
        assistant_message_id: None,
        depth: 0,
        observer: None,
        request_source: None,
        parent_model: None,
        parent_model_profile: None,
        mode_override: None,
        frozen_command_denies: Vec::new(),
    }
}

/// Desktop sibling of the mobile guard: the deferred wrapper must forward
/// the lease token rather than inherit the trait's delegating default.
#[tokio::test]
async fn deferred_invoker_forwards_the_workspace_lease_token() {
    let seen = Arc::new(StdMutex::new(None));
    let deferred = super::DeferredToolInvoker::new();
    deferred.set(Arc::new(RecordingInvoker { seen: seen.clone() }));

    deferred
        .invoke_with_workspace_lease("Read", serde_json::json!({}), bare_ctx(), Some(77))
        .await
        .expect("dispatch");

    assert_eq!(
        *seen.lock().unwrap(),
        Some(Some(77)),
        "the deferred invoker must forward the lease token, not swallow it"
    );
}

#[tokio::test]
async fn deferred_invoker_preserves_model_content_and_workspace_lease() {
    let seen = Arc::new(StdMutex::new(None));
    let deferred = super::DeferredToolInvoker::new();
    deferred.set(Arc::new(RecordingInvoker { seen: seen.clone() }));
    let result = deferred
        .invoke_detailed("ExitPlanMode", serde_json::json!({}), bare_ctx(), Some(77))
        .await
        .unwrap();
    assert_eq!(
        result.data,
        serde_json::json!({"awaitingLeaderApproval": true})
    );
    assert_eq!(
        result.model_content.as_deref(),
        Some("Wait for the team lead to review your plan")
    );
    assert_eq!(*seen.lock().unwrap(), Some(Some(77)));
}
