//! Synchronize validated Workflow descriptors into the live slash registry.

use std::sync::Arc;

use command_api::CommandRegistry;
use tokio::sync::RwLock;
use tool_workflow::WorkflowTool;

/// Refresh the command snapshot while the caller already holds the registry's
/// write lock (used by the synchronous `/reload-skills` finalizer).
pub(crate) fn replace_workflow_commands(
    registry: &mut CommandRegistry,
    workflow_tool: &WorkflowTool,
) {
    let descriptors = workflow_tool.refresh_workflow_catalog();
    registry.replace_native_workflow_commands(crate::workflow_slash::workflow_slash_commands(
        &descriptors,
    ));
}

/// Rebuild the validated workflow catalog and atomically publish its separate
/// producer snapshot in the shared slash registry.
pub(crate) async fn refresh_workflow_commands(
    registry: &Arc<RwLock<CommandRegistry>>,
    workflow_tool: &WorkflowTool,
) {
    let descriptors = workflow_tool.refresh_workflow_catalog();
    registry.write().await.replace_native_workflow_commands(
        crate::workflow_slash::workflow_slash_commands(&descriptors),
    );
}
