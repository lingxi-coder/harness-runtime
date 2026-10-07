//! Mod `tool.call` middleware around a subagent's inherited tool invoker.
//!
//! The child runner still owns its allow-list and the wrapped invoker still
//! owns schema validation, tool checks, and permission prompts. A Mod reaches
//! that exact core through `next(e)`; a direct answer replaces the child tool
//! result without invoking it.

#[async_trait::async_trait]
impl lingxi_core::host::ToolInvoker for ModSubagentToolInvoker {
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
