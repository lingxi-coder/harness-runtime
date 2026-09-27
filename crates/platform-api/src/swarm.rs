//! Swarm backend trait for multi-agent tmux/terminal coordination.
//!
//! See spec §4 (Trait System) — platform implementations live in
//! `platforms/posix/tmux` and similar crates. The engine depends only on
//! this trait so it can run on systems without a swarm backend.

use async_trait::async_trait;
use protocol::AgentId;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Platform-provided swarm backend (tmux/screen/etc.).
#[async_trait]
pub trait SwarmBackend: Send + Sync {
    /// Start a new swarm session with the given layout.
    async fn start_swarm(&self, layout: SwarmLayout) -> Result<SwarmHandle, SwarmError>;
    /// Create a new pane within the swarm for the given teammate agent.
    async fn create_teammate_pane(
        &self,
        agent_id: &AgentId,
        position: PanePosition,
    ) -> Result<PaneId, SwarmError>;
    /// Resolve coordinates for an externally running teammate pane.
    async fn pane_metadata(
        &self,
        _pane: &PaneId,
    ) -> Result<crate::team_spawn::PaneLaunchMetadata, SwarmError> {
        Err(SwarmError::Unsupported)
    }
    /// Dispatch a validated worker launch command to an existing pane.
    async fn send_command_to_pane(&self, _pane: &PaneId, _command: &str) -> Result<(), SwarmError> {
        Err(SwarmError::Unsupported)
    }
    /// Stop and remove a teammate pane without destroying the leader session.
    async fn kill_pane(&self, _pane: &PaneId) -> Result<(), SwarmError> {
        Err(SwarmError::Unsupported)
    }
    /// Tear down a swarm session.
    async fn destroy_swarm(&self, handle: SwarmHandle) -> Result<(), SwarmError>;
    /// Whether the backend is available on the current platform.
    fn is_available(&self) -> bool;
}

/// Layout describing how panes are arranged.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum SwarmLayout {
    /// One leader pane, additional follower panes.
    LeaderFollower,
    /// Tiled grid layout.
    Tiled,
    /// External (user-managed) layout.
    External,
}

/// Position for a new pane within the swarm.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum PanePosition {
    /// Place the pane at the top.
    Top,
    /// Place the pane at the bottom.
    Bottom,
    /// Place the pane on the left.
    Left,
    /// Place the pane on the right.
    Right,
}

/// Handle returned by [`SwarmBackend::start_swarm`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmHandle {
    /// Backend-specific session name (e.g. tmux session).
    pub session_name: String,
}

/// Identifier for a pane within a swarm.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneId {
    /// Backend-specific pane identifier.
    pub raw: String,
}

/// Errors produced by [`SwarmBackend`] implementations.
#[derive(Debug, Clone, Error)]
pub enum SwarmError {
    /// Swarm not supported on this platform.
    #[error("swarm not supported on this platform")]
    Unsupported,
    /// Tmux-specific (or general backend) error.
    #[error("{0}")]
    Tmux(String),
}
