//! Frozen Native wire inputs for host-owned background memory forks.
//!
//! The task-local is an admission carrier only. The spawner captures its value
//! into the child context before allocating the worker, and every model call
//! then owns an explicit projection. Ordinary Agent calls never consume it.

use lingxi_core::host::{task_delivery::TaskDeliveryRequest, SourceToolWireDefinition};
use lingxi_core::types::{utf16_json::Utf16JsonProjection, MessageId};
use lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;

pub use lingxi_core::host::memory_tool_scope::{
    current_memory_settlement as current_settlement, with_memory_settlement as with_settlement,
    MemoryForkLease, MemoryForkSettlement,
};

/// Source-backed request facts that flattening the rendered system text loses.
#[derive(Clone)]
pub struct MemoryForkProjection {
    /// Host-only private file-tool state frozen with the inherited history.
    /// This capability is never serialized into the provider request.
    pub tool_invoker: Option<Arc<dyn lingxi_core::host::ToolInvoker>>,
    /// Native source vector, preserving section and cache-boundary identities.
    pub system_prompt_input: Option<SystemPromptInput>,
    /// Parent's source schemas in their original order.
    pub tools: Vec<SourceToolWireDefinition>,
    /// Identity metadata for typed messages already present in the prefix.
    pub typed_attachment_ids: HashSet<MessageId>,
    /// Native task-delivery snapshots indexed by durable message identity.
    pub task_deliveries: HashMap<MessageId, TaskDeliveryRequest>,
    /// Exact wire-model source when route resolution alone cannot reproduce it.
    pub model_source: Option<Utf16JsonProjection>,
}

impl MemoryForkProjection {
    /// Keep source bytes for the same selected prompt bundle. A route into a
    /// different bundle must use its freshly rendered system and tool schemas.
    pub(crate) fn rebind_for_route(
        wire: &Arc<Self>,
        route_changed: bool,
        source_policy: Option<&lingxi_core::host::prompt_policy::ResolvedPromptPolicy>,
        target_policy: Option<&lingxi_core::host::prompt_policy::ResolvedPromptPolicy>,
        tools: &[SourceToolWireDefinition],
    ) -> Arc<Self> {
        if !route_changed {
            return wire.clone();
        }
        let mut routed = (**wire).clone();
        routed.model_source = None;
        if source_policy
            .zip(target_policy)
            .is_some_and(|(source, target)| {
                (
                    source.bundle_id,
                    source.bundle_version,
                    source.system,
                    source.tools,
                    source.memory,
                    source.reminders,
                ) != (
                    target.bundle_id,
                    target.bundle_version,
                    target.system,
                    target.tools,
                    target.memory,
                    target.reminders,
                )
            })
        {
            routed.system_prompt_input = None;
            routed.tools = tools.to_vec();
        }
        Arc::new(routed)
    }

    /// Freeze the same parent inputs used by the existing side-query fork path.
    #[must_use]
    pub fn from_cache_params(params: &sidequery::CacheSafeParams) -> Self {
        Self {
            tool_invoker: None,
            system_prompt_input: params.system_prompt_input.clone(),
            tools: params.tools.clone(),
            typed_attachment_ids: params.typed_attachment_ids.clone(),
            task_deliveries: params.task_deliveries.clone(),
            model_source: params.model_source.clone(),
        }
    }
}

impl std::fmt::Debug for MemoryForkProjection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MemoryForkProjection")
            .field("has_system_source", &self.system_prompt_input.is_some())
            .field("tool_count", &self.tools.len())
            .field("typed_attachment_count", &self.typed_attachment_ids.len())
            .field("task_delivery_count", &self.task_deliveries.len())
            .field("has_model_source", &self.model_source.is_some())
            .finish()
    }
}

tokio::task_local! {
    static MEMORY_FORK_WIRE: Arc<MemoryForkProjection>;
}

/// Admit one spawner call with a frozen wire projection. This scope does not
/// survive a runtime spawn; the caller must wrap the actual spawner future.
pub async fn with_wire_snapshot<F: Future>(
    snapshot: Arc<MemoryForkProjection>,
    future: F,
) -> F::Output {
    MEMORY_FORK_WIRE.scope(snapshot, future).await
}

pub(crate) fn current_wire_snapshot() -> Option<Arc<MemoryForkProjection>> {
    MEMORY_FORK_WIRE.try_with(Arc::clone).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn wire_scope_is_explicit_and_does_not_leak_to_an_unrelated_worker() {
        let snapshot = Arc::new(MemoryForkProjection {
            tool_invoker: None,
            system_prompt_input: None,
            tools: Vec::new(),
            typed_attachment_ids: HashSet::new(),
            task_deliveries: HashMap::new(),
            model_source: Some("private-model-id".into()),
        });
        assert!(current_wire_snapshot().is_none());
        with_wire_snapshot(snapshot.clone(), async {
            let captured = current_wire_snapshot().unwrap();
            assert!(Arc::ptr_eq(&captured, &snapshot));
            tokio::spawn(async move {
                assert!(current_wire_snapshot().is_none());
                assert_eq!(
                    captured.model_source.as_ref().unwrap().string_at(""),
                    Some("private-model-id")
                );
            })
            .await
            .unwrap();
        })
        .await;
        assert!(current_wire_snapshot().is_none());
        assert!(!format!("{snapshot:?}").contains("private-model-id"));
    }
}
