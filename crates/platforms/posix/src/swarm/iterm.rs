//! Native iTerm2 panes through the it2 CLI, matching Claude Code 2.1.263.

use async_trait::async_trait;
use platform_api::{PaneId, PanePosition, SwarmBackend, SwarmError, SwarmHandle, SwarmLayout};
use protocol::AgentId;
use tokio::process::Command;
use tokio::sync::Mutex;

/// Pane state also serializes creation and pruning of dead sessions.
#[derive(Default)]
pub struct ITermSwarmBackend {
    sessions: Mutex<Vec<String>>,
}

impl ITermSwarmBackend {
    /// Construct a native iTerm2 backend.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    async fn run(args: &[&str]) -> Result<std::process::Output, SwarmError> {
        Command::new(super::detection::it2_command())
            .args(args)
            .output()
            .await
            .map_err(|error| SwarmError::Tmux(format!("spawn it2 failed: {error}")))
    }

    fn parse_session_id(output: &str) -> Option<&str> {
        output.lines().find_map(|line| {
            line.split_once("Created new pane:")
                .map(|(_, id)| id.trim())
                .filter(|id| !id.is_empty())
        })
    }
}

#[async_trait]
impl SwarmBackend for ITermSwarmBackend {
    async fn start_swarm(&self, _layout: SwarmLayout) -> Result<SwarmHandle, SwarmError> {
        // iTerm2 splits the leader's existing session; it never creates a window.
        Ok(SwarmHandle {
            session_name: "current".to_owned(),
        })
    }

    async fn create_teammate_pane(
        &self,
        _agent_id: &AgentId,
        _position: PanePosition,
    ) -> Result<PaneId, SwarmError> {
        let mut sessions = self.sessions.lock().await;
        let leader = std::env::var("ITERM_SESSION_ID")
            .ok()
            .and_then(|value| value.split_once(':').map(|(_, id)| id.to_owned()));
        loop {
            let mut args = vec!["session", "split"];
            let target = if sessions.is_empty() {
                args.push("-v");
                leader.as_deref()
            } else {
                sessions.last().map(String::as_str)
            };
            if let Some(target) = target {
                args.extend(["-s", target]);
            }
            let output = Self::run(&args).await?;
            if !output.status.success() {
                if let Some(previous) = sessions.last() {
                    let listing = Self::run(&["session", "list"]).await?;
                    if listing.status.success()
                        && !String::from_utf8_lossy(&listing.stdout).contains(previous)
                    {
                        sessions.pop();
                        continue;
                    }
                }
                return Err(SwarmError::Tmux(format!(
                    "Failed to create iTerm2 split pane: {}",
                    String::from_utf8_lossy(&output.stderr)
                )));
            }
            let stdout = String::from_utf8_lossy(&output.stdout);
            let id = Self::parse_session_id(&stdout)
                .ok_or_else(|| {
                    SwarmError::Tmux(format!(
                        "Failed to parse session ID from split output: {stdout}"
                    ))
                })?
                .to_owned();
            sessions.push(id.clone());
            return Ok(PaneId { raw: id });
        }
    }

    async fn pane_metadata(
        &self,
        pane: &PaneId,
    ) -> Result<platform_api::team_spawn::PaneLaunchMetadata, SwarmError> {
        // iTerm2 has no tmux session/window coordinates. These are the logical
        // swarm-view labels emitted by upstream spawnTeammate (pe).
        let inside_tmux = super::tmux::TmuxBackend::is_running_inside();
        Ok(platform_api::team_spawn::PaneLaunchMetadata {
            backend_type: "iterm2".into(),
            session_name: if inside_tmux {
                "current"
            } else {
                "lingxi-swarm"
            }
            .into(),
            window_name: if inside_tmux {
                "current"
            } else {
                super::tmux::SwarmConstants::VIEW_WINDOW_NAME
            }
            .into(),
            pane_id: pane.raw.clone(),
        })
    }

    async fn send_command_to_pane(&self, pane: &PaneId, command: &str) -> Result<(), SwarmError> {
        super::validate_pane_command(command)?;
        let _ = Self::run(&["session", "send", "-s", &pane.raw, "\u{15}"]).await?;
        let output = Self::run(&["session", "run", "-s", &pane.raw, command]).await?;
        if !output.status.success() {
            return Err(SwarmError::Tmux(format!(
                "Failed to send command to iTerm2 pane {}: {}",
                pane.raw,
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        Ok(())
    }

    async fn kill_pane(&self, pane: &PaneId) -> Result<(), SwarmError> {
        let output = Self::run(&["session", "close", "-f", "-s", &pane.raw]).await?;
        self.sessions.lock().await.retain(|id| id != &pane.raw);
        if !output.status.success() {
            return Err(SwarmError::Tmux(
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ));
        }
        Ok(())
    }

    async fn destroy_swarm(&self, _handle: SwarmHandle) -> Result<(), SwarmError> {
        let sessions = self.sessions.lock().await.clone();
        for raw in sessions {
            self.kill_pane(&PaneId { raw }).await?;
        }
        Ok(())
    }

    fn is_available(&self) -> bool {
        std::env::var("TERM_PROGRAM").as_deref() == Ok("iTerm.app") && which::which("it2").is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_response_requires_upstream_prefix() {
        assert_eq!(
            ITermSwarmBackend::parse_session_id("Created new pane: session-123\n"),
            Some("session-123")
        );
        assert_eq!(ITermSwarmBackend::parse_session_id("session-123"), None);
        assert_eq!(
            ITermSwarmBackend::parse_session_id("Created new pane: \n"),
            None
        );
    }
}
