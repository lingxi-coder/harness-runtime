//! Stub [`SwarmBackend`] — tmux/screen integration ships in
//! `platforms/posix` (Plan 17).

use async_trait::async_trait;
use platform_api::{PaneId, PanePosition, SwarmBackend, SwarmError, SwarmHandle, SwarmLayout};
use protocol::AgentId;

/// Stub swarm backend.
#[derive(Default)]
pub struct PosixSwarm;

impl PosixSwarm {
    /// Construct.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl SwarmBackend for PosixSwarm {
    async fn start_swarm(&self, _layout: SwarmLayout) -> Result<SwarmHandle, SwarmError> {
        Err(SwarmError::Unsupported)
    }

    async fn create_teammate_pane(
        &self,
        _agent_id: &AgentId,
        _position: PanePosition,
    ) -> Result<PaneId, SwarmError> {
        Err(SwarmError::Unsupported)
    }

    async fn destroy_swarm(&self, _handle: SwarmHandle) -> Result<(), SwarmError> {
        Ok(())
    }

    fn is_available(&self) -> bool {
        false
    }
}
