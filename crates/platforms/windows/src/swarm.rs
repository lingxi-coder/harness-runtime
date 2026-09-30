//! Swarm backend — Windows (no tmux).
//!
//! claude-code refuses `--tmux` on Windows; we match. Linux + macOS get the
//! tmux / iTerm / `InProcess` trifecta in `platforms/posix/src/swarm/`. See
//! the M2-05 plan for the cross-platform parity story.
//!
//! Future: if Windows ever gains a swarm story, it would land here as a
//! Windows Terminal / wezterm CLI shell-out. Out of scope for v0.3.0.
//!
//! Wire-fidelity note: the error string `--tmux is not supported on Windows`
//! (M2-01) must remain unchanged — only the module doc was updated here.

use async_trait::async_trait;
use lingxi_core::host::{PaneId, PanePosition, SwarmBackend, SwarmError, SwarmHandle, SwarmLayout};
use lingxi_core::types::AgentId;

/// Windows-side [`SwarmBackend`] — always reports unsupported.
#[derive(Default)]
pub struct WindowsSwarmBackend;

impl WindowsSwarmBackend {
    /// Construct a new `WindowsSwarmBackend`. Holds no state.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl SwarmBackend for WindowsSwarmBackend {
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
        Err(SwarmError::Unsupported)
    }

    fn is_available(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::{PanePosition, SwarmBackend, SwarmError, SwarmHandle, SwarmLayout};
    use lingxi_core::types::AgentId;

    #[test]
    fn is_available_returns_false() {
        assert!(!WindowsSwarmBackend::new().is_available());
    }

    #[tokio::test]
    async fn start_swarm_returns_unsupported() {
        let err = WindowsSwarmBackend::new()
            .start_swarm(SwarmLayout::LeaderFollower)
            .await
            .unwrap_err();
        assert!(matches!(err, SwarmError::Unsupported));
    }

    #[tokio::test]
    async fn create_teammate_pane_returns_unsupported() {
        let err = WindowsSwarmBackend::new()
            .create_teammate_pane(&AgentId::nil(), PanePosition::Right)
            .await
            .unwrap_err();
        assert!(matches!(err, SwarmError::Unsupported));
    }

    #[tokio::test]
    async fn destroy_swarm_returns_unsupported() {
        let err = WindowsSwarmBackend::new()
            .destroy_swarm(SwarmHandle {
                session_name: "phantom".into(),
            })
            .await
            .unwrap_err();
        assert!(matches!(err, SwarmError::Unsupported));
    }
}
