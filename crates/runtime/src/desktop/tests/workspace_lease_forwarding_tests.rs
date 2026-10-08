use std::sync::{Arc, Mutex as StdMutex};

use lingxi_core::host::tool_invoker::{SubagentInvocationContext, ToolInvoker, ToolInvokerError};

/// Terminal invoker that records the lease token it was dispatched with.
struct RecordingInvoker {
    seen: Arc<StdMutex<Option<Option<u64>>>>,
}

#[async_trait::async_trait]
impl ToolInvoker for RecordingInvoker {
    fn permission_mode(&self) -> Option<String> {
        Some("plan".into())
    }

    async fn invoke_supplied_detailed(
        &self,
        name: &str,
        input: serde_json::Value,
        ctx: SubagentInvocationContext,
        workspace_lease_token: Option<u64>,
        supplied: Arc<dyn std::any::Any + Send + Sync>,
    ) -> Result<lingxi_core::host::tool_invoker::ToolInvocationResult, ToolInvokerError> {
        assert_eq!(
            supplied.downcast_ref::<String>().map(String::as_str),
            Some("private child tool")
        );
        let mut result = self
            .invoke_detailed(name, input, ctx, workspace_lease_token)
            .await?;
        result.turn_end = Some(lingxi_core::host::tool_invoker::ToolResultTurnEnd {
            source: lingxi_core::host::tool_invoker::ToolResultTurnEndSource::Tool,
        });
        Ok(result)
    }

    async fn invoke_detailed(
        &self,
        _name: &str,
        _input: serde_json::Value,
        _ctx: SubagentInvocationContext,
        workspace_lease_token: Option<u64>,
    ) -> Result<lingxi_core::host::tool_invoker::ToolInvocationResult, ToolInvokerError> {
        *self.seen.lock().unwrap() = Some(workspace_lease_token);
        Ok(lingxi_core::host::tool_invoker::ToolInvocationResult { mcp_meta_projection: None, model_content_projection: None, data_projection: None,
            turn_end: None,
            is_error: false,
            data: serde_json::json!({"awaitingLeaderApproval": true}),
            model_content: Some("Wait for the team lead to review your plan".into()),
            new_messages: Vec::new(),
            context_modifier: None,
            mcp_meta: None,
            context: lingxi_core::types::utf16_json::Utf16JsonProjection::plain(
                serde_json::Value::Array(Vec::new()),
            ),
            context_state: None,
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
        input_projection: None,
        cancellation_token: lingxi_core::host::CancellationToken::new(),
        permission_pause_observer: None,
        parent_agent_id: None,
        origin_session_id: None,
        instruction_context: None,
        fork_context: None,
        tool_execution_policy: lingxi_core::host::tool_invoker::ToolExecutionPolicy::Ordinary,
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
        agent_spawn_provenance: Default::default(),
        tool_context_state: None,
        assistant_message: None,
        same_turn_tool_uses: Vec::new(),
        current_history: Vec::new(),
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

#[tokio::test]
async fn deferred_invoker_preserves_private_child_tool_and_live_mode() {
    let seen = Arc::new(StdMutex::new(None));
    let deferred = super::DeferredToolInvoker::new();
    deferred.set(Arc::new(RecordingInvoker { seen: seen.clone() }));
    assert_eq!(deferred.permission_mode().as_deref(), Some("plan"));
    let result = deferred
        .invoke_supplied_detailed(
            "SubagentHandback",
            serde_json::json!({}),
            bare_ctx(),
            Some(91),
            Arc::new("private child tool".to_string()),
        )
        .await
        .unwrap();
    assert_eq!(
        result.turn_end.unwrap().source,
        lingxi_core::host::tool_invoker::ToolResultTurnEndSource::Tool
    );
    assert_eq!(*seen.lock().unwrap(), Some(Some(91)));
}
