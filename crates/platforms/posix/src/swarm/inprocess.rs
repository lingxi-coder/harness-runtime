//! No-pane `SwarmBackend` fallback.
//!
//! Always available. Methods succeed with synthetic identifiers and emit
//! a `tracing::debug!` log line — there is no actual terminal-pane
//! visualization. The engine drives multi-agent coordination through the
//! same effect-handler / mailbox machinery either way, so this backend
//! is fully functional from the agent's perspective; only the operator's
//! visual feedback is missing. Mirrors claude-code
//! `src/utils/swarm/backends/InProcessBackend.ts`.

use async_trait::async_trait;
use lingxi_core::host::{PaneId, PanePosition, SwarmBackend, SwarmError, SwarmHandle, SwarmLayout};
use lingxi_core::types::AgentId;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// No-pane `SwarmBackend` that's always available; the always-on fallback
/// when neither tmux nor iTerm.app are detected.
#[derive(Default)]
pub struct InProcessSwarmBackend;

impl InProcessSwarmBackend {
    /// Construct a new no-pane backend.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl SwarmBackend for InProcessSwarmBackend {
    async fn start_swarm(&self, _layout: SwarmLayout) -> Result<SwarmHandle, SwarmError> {
        tracing::debug!("swarm running in-process; no pane visualization");
        Ok(SwarmHandle {
            session_name: "in-process".to_string(),
        })
    }

    async fn create_teammate_pane(
        &self,
        agent_id: &AgentId,
        _position: PanePosition,
    ) -> Result<PaneId, SwarmError> {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        tracing::debug!(
            "swarm running in-process; no pane visualization (agent={agent_id:?}, synthetic pane #{n})"
        );
        Ok(PaneId {
            raw: format!("in-process-pane-{n}"),
        })
    }

    async fn destroy_swarm(&self, _handle: SwarmHandle) -> Result<(), SwarmError> {
        tracing::debug!("swarm running in-process; no pane visualization (destroy no-op)");
        Ok(())
    }

    fn is_available(&self) -> bool {
        true
    }
}
