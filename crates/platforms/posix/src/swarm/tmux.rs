//! Real `tmux`-backed `SwarmBackend` for POSIX hosts.
//!
//! Mirrors claude-code `src/utils/swarm/backends/TmuxBackend.ts`. Behavioral
//! parity items locked in M2-05 plan:
//! - `SWARM_SESSION_NAME = "claude-swarm"`
//! - `SWARM_VIEW_WINDOW_NAME = "swarm-view"`
//! - socket name `claude-swarm-<pid>` (per `getSwarmSocketName()`)
//! - `PANE_SHELL_INIT_DELAY_MS = 200`
//! - global pane-creation lock to prevent concurrent `tmux split-window` races
//! - color map literal (see `agent_color_to_tmux`)
//! - probes availability through `tmux -V` exit status

use async_trait::async_trait;
use platform_api::{PaneId, PanePosition, SwarmBackend, SwarmError, SwarmHandle, SwarmLayout};
use protocol::AgentId;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::Mutex;

use super::detection;

/// Shell-init delay after pane creation, per `TmuxBackend.ts:33`.
pub const PANE_SHELL_INIT_DELAY_MS: u64 = 200;

/// Process-global pane-creation lock. Mirrors claude-code's `paneCreationLock`
/// promise chain — prevents two concurrent `tmux split-window` calls from
/// racing for the same target pane.
fn pane_creation_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Constants mirrored verbatim from `claude-code/src/utils/swarm/constants.ts`.
pub struct SwarmConstants;

impl SwarmConstants {
    /// External tmux session name (claude-code: `SWARM_SESSION_NAME`).
    pub const SESSION_NAME: &'static str = "claude-swarm";
    /// External tmux window name (claude-code: `SWARM_VIEW_WINDOW_NAME`).
    pub const VIEW_WINDOW_NAME: &'static str = "swarm-view";
    /// Name of the `tmux` binary on PATH.
    pub const TMUX_COMMAND: &'static str = "tmux";

    /// Per-pid socket name so multiple Claude instances don't collide.
    #[must_use]
    pub fn socket_name_for_pid(pid: u32) -> String {
        format!("claude-swarm-{pid}")
    }

    /// Socket name for the running process (uses [`std::process::id`]).
    #[must_use]
    pub fn current_socket_name() -> String {
        Self::socket_name_for_pid(std::process::id())
    }
}

/// Agent colors we support (matches `AgentColorName` in claude-code's
/// `agentColorManager.ts`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentColor {
    /// `red`
    Red,
    /// `blue`
    Blue,
    /// `green`
    Green,
    /// `yellow`
    Yellow,
    /// `cyan`
    Cyan,
    /// `magenta` (claude-code's "purple")
    Purple,
    /// `colour208` (256-palette orange)
    Orange,
    /// `colour205` (256-palette pink)
    Pink,
}

/// Color literal mapping — see `TmuxBackend.ts:60-69`.
#[must_use]
pub fn agent_color_to_tmux(c: AgentColor) -> &'static str {
    match c {
        AgentColor::Red => "red",
        AgentColor::Blue => "blue",
        AgentColor::Green => "green",
        AgentColor::Yellow => "yellow",
        AgentColor::Cyan => "cyan",
        AgentColor::Purple => "magenta",
        AgentColor::Orange => "colour208",
        AgentColor::Pink => "colour205",
    }
}

// --- argv builders (pure, unit-testable) ---

/// Build the argv passed to `tmux split-window`. `horizontal=true` selects
/// `-h` (left/right split), `false` selects `-v` (top/bottom). When
/// `size_pct` is set we add `-l <pct>` to fix the new pane's size. The
/// `-P -F #{pane_id}` suffix makes tmux print the new pane id on stdout.
#[must_use]
pub fn build_split_window_argv(
    target_pane: &str,
    horizontal: bool,
    size_pct: Option<&str>,
) -> Vec<String> {
    let mut a = vec![
        "split-window".to_string(),
        "-t".to_string(),
        target_pane.to_string(),
        (if horizontal { "-h" } else { "-v" }).to_string(),
    ];
    if let Some(pct) = size_pct {
        a.push("-l".to_string());
        a.push(pct.to_string());
    }
    a.push("-P".to_string());
    a.push("-F".to_string());
    a.push("#{pane_id}".to_string());
    a
}

/// Build the argv for `tmux select-pane -P bg=default,fg=<color>` — sets
/// the pane's runtime foreground/background color (claude-code applies this
/// after splitting; see `TmuxBackend.ts`).
#[must_use]
pub fn build_select_pane_color_argv(pane_id: &str, tmux_color: &str) -> Vec<String> {
    vec![
        "select-pane".to_string(),
        "-t".to_string(),
        pane_id.to_string(),
        "-P".to_string(),
        format!("bg=default,fg={tmux_color}"),
    ]
}

/// Build the argv for `tmux set-option -p -t <pane> pane-border-style fg=<color>`
/// — paints the pane border. Requires tmux >= 3.2 because `-p` is per-pane-only
/// from 3.2 onward.
#[must_use]
pub fn build_set_pane_border_argv(pane_id: &str, tmux_color: &str) -> Vec<String> {
    vec![
        "set-option".to_string(),
        "-p".to_string(),
        "-t".to_string(),
        pane_id.to_string(),
        "pane-border-style".to_string(),
        format!("fg={tmux_color}"),
    ]
}

/// Build the argv for `tmux set-option -p -t <pane> pane-border-format <fmt>`
/// — sets the per-pane border title shown when `pane-border-status` is on.
#[must_use]
pub fn build_set_pane_border_format_argv(pane_id: &str, fmt: &str) -> Vec<String> {
    vec![
        "set-option".to_string(),
        "-p".to_string(),
        "-t".to_string(),
        pane_id.to_string(),
        "pane-border-format".to_string(),
        fmt.to_string(),
    ]
}

/// Build the argv for `tmux send-keys -t <pane> <cmd> Enter` — types a
/// command into the target pane and presses Enter.
#[must_use]
pub fn build_send_keys_argv(pane_id: &str, cmd: &str) -> Vec<String> {
    vec![
        "send-keys".to_string(),
        "-t".to_string(),
        pane_id.to_string(),
        cmd.to_string(),
        "Enter".to_string(),
    ]
}

// --- real backend impl ---

/// POSIX `SwarmBackend` using `tmux` shell-out.
#[derive(Default)]
pub struct TmuxBackend {
    /// Optional per-pid socket name; defaults to `current_socket_name()`.
    socket_name: Option<String>,
    #[cfg(test)]
    command_override: Option<std::path::PathBuf>,
}

impl TmuxBackend {
    /// Construct a new backend that derives its socket name from the
    /// running process pid.
    #[must_use]
    pub fn new() -> Self {
        Self {
            socket_name: None,
            #[cfg(test)]
            command_override: None,
        }
    }

    fn socket(&self) -> String {
        self.socket_name
            .clone()
            .unwrap_or_else(SwarmConstants::current_socket_name)
    }

    /// Internal helper: shell out to `tmux` with the requested argv.
    async fn run_tmux(&self, in_swarm_socket: bool, args: &[String]) -> Result<String, SwarmError> {
        #[cfg(test)]
        let executable = self
            .command_override
            .as_deref()
            .unwrap_or_else(|| std::path::Path::new(SwarmConstants::TMUX_COMMAND));
        #[cfg(not(test))]
        let executable = SwarmConstants::TMUX_COMMAND;
        let mut cmd = Command::new(executable);
        if in_swarm_socket {
            cmd.arg("-L").arg(self.socket());
        } else if let Ok(tmux) = std::env::var("TMUX") {
            if let Some(socket) = tmux.split(',').next().filter(|socket| !socket.is_empty()) {
                cmd.arg("-S").arg(socket);
            }
        }
        cmd.args(args).kill_on_drop(true);
        let out = cmd
            .output()
            .await
            .map_err(|e| SwarmError::Tmux(format!("spawn tmux failed: {e}")))?;
        if !out.status.success() {
            return Err(SwarmError::Tmux(format!(
                "tmux exited {}: stderr={}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    async fn create_configured_pane(
        &self,
        target: &str,
        horizontal: bool,
        in_swarm_socket: bool,
    ) -> Result<PaneId, SwarmError> {
        let pane = self
            .run_tmux(
                in_swarm_socket,
                &build_split_window_argv(target, horizontal, Some("70%")),
            )
            .await?;
        let configured = async {
            let color = agent_color_to_tmux(AgentColor::Red);
            self.run_tmux(in_swarm_socket, &build_select_pane_color_argv(&pane, color))
                .await?;
            self.run_tmux(in_swarm_socket, &build_set_pane_border_argv(&pane, color))
                .await?;
            Ok::<(), SwarmError>(())
        }
        .await;
        if configured.is_err() {
            // Keep the split's ID owned here until every initialization step
            // succeeds, so a styling failure cannot leave an untracked pane.
            let args = ["kill-pane".into(), "-t".into(), pane.clone()];
            let rollback = self.run_tmux(in_swarm_socket, &args);
            match tokio::time::timeout(Duration::from_secs(5), rollback).await {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => tracing::warn!("Failed to roll back tmux pane {pane}: {error}"),
                Err(_) => tracing::warn!("Timed out rolling back tmux pane {pane}"),
            }
        }
        configured?;
        tokio::time::sleep(Duration::from_millis(PANE_SHELL_INIT_DELAY_MS)).await;
        Ok(PaneId { raw: pane })
    }

    /// Synchronous "are we inside a tmux session" probe — claude-code only
    /// reads `$TMUX`, never shells out (see `detection.ts:35-37`).
    #[must_use]
    pub fn is_running_inside() -> bool {
        std::env::var("TMUX").is_ok_and(|value| !value.is_empty())
    }
}

#[async_trait]
impl SwarmBackend for TmuxBackend {
    async fn start_swarm(&self, _layout: SwarmLayout) -> Result<SwarmHandle, SwarmError> {
        let inside = TmuxBackend::is_running_inside();
        if inside {
            // Inside tmux: we don't create a new session — we'll split the
            // current window when teammates are added. Return the user's
            // current session name as the handle (best-effort).
            let session_name = std::env::var("TMUX")
                .ok()
                .and_then(|_| {
                    // `tmux display-message -p '#S'` returns current session.
                    std::process::Command::new(SwarmConstants::TMUX_COMMAND)
                        .args(["display-message", "-p", "#S"])
                        .output()
                        .ok()
                })
                .map_or_else(
                    || "user-session".to_string(),
                    |o| String::from_utf8_lossy(&o.stdout).trim().to_string(),
                );
            return Ok(SwarmHandle { session_name });
        }

        // Outside tmux: create the external claude-swarm session on a per-pid socket.
        // `tmux -L <socket> new-session -d -s claude-swarm -n swarm-view`
        let args = vec![
            "new-session".to_string(),
            "-d".to_string(),
            "-s".to_string(),
            SwarmConstants::SESSION_NAME.to_string(),
            "-n".to_string(),
            SwarmConstants::VIEW_WINDOW_NAME.to_string(),
        ];
        self.run_tmux(true, &args).await?;
        Ok(SwarmHandle {
            session_name: SwarmConstants::SESSION_NAME.to_string(),
        })
    }

    async fn create_teammate_pane(
        &self,
        _agent_id: &AgentId,
        position: PanePosition,
    ) -> Result<PaneId, SwarmError> {
        // Serialize pane creation per claude-code paneCreationLock.
        let _guard = pane_creation_lock().lock().await;

        let inside = TmuxBackend::is_running_inside();
        let horizontal = matches!(position, PanePosition::Left | PanePosition::Right);

        // Target: inside-tmux uses the leader's pane id (from `$TMUX_PANE`),
        // outside-tmux uses the swarm session's first pane.
        let target_pane = if inside {
            std::env::var("TMUX_PANE")
                .map_err(|_| SwarmError::Tmux("TMUX_PANE not set inside tmux".into()))?
        } else {
            // Look up the first pane in the external swarm session.
            let listing = self
                .run_tmux(
                    true,
                    &[
                        "list-panes".to_string(),
                        "-t".to_string(),
                        format!(
                            "{}:{}",
                            SwarmConstants::SESSION_NAME,
                            SwarmConstants::VIEW_WINDOW_NAME
                        ),
                        "-F".to_string(),
                        "#{pane_id}".to_string(),
                    ],
                )
                .await?;
            listing
                .lines()
                .next()
                .ok_or_else(|| SwarmError::Tmux("no panes in swarm session".into()))?
                .to_string()
        };

        self.create_configured_pane(&target_pane, horizontal, !inside)
            .await
    }

    async fn pane_metadata(
        &self,
        pane: &PaneId,
    ) -> Result<platform_api::team_spawn::PaneLaunchMetadata, SwarmError> {
        // Model-facing labels are logical coordinates in upstream pe, even
        // when the user's tmux session or window has a different actual name.
        let inside = Self::is_running_inside();
        Ok(platform_api::team_spawn::PaneLaunchMetadata {
            backend_type: "tmux".into(),
            session_name: if inside { "current" } else { "lingxi-swarm" }.into(),
            window_name: if inside {
                "current"
            } else {
                SwarmConstants::VIEW_WINDOW_NAME
            }
            .into(),
            pane_id: pane.raw.clone(),
        })
    }

    async fn send_command_to_pane(&self, pane: &PaneId, command: &str) -> Result<(), SwarmError> {
        super::validate_pane_command(command)?;
        let external = !Self::is_running_inside();
        let _ = self
            .run_tmux(
                external,
                &[
                    "set-option".into(),
                    "-p".into(),
                    "-t".into(),
                    pane.raw.clone(),
                    "remain-on-exit".into(),
                    "failed".into(),
                ],
            )
            .await;
        self.run_tmux(
            external,
            &[
                "respawn-pane".into(),
                "-k".into(),
                "-t".into(),
                pane.raw.clone(),
                "--".into(),
                command.into(),
            ],
        )
        .await?;
        Ok(())
    }

    async fn kill_pane(&self, pane: &PaneId) -> Result<(), SwarmError> {
        self.run_tmux(
            !Self::is_running_inside(),
            &["kill-pane".into(), "-t".into(), pane.raw.clone()],
        )
        .await?;
        Ok(())
    }

    async fn destroy_swarm(&self, handle: SwarmHandle) -> Result<(), SwarmError> {
        let inside = TmuxBackend::is_running_inside();
        if inside {
            // Inside tmux: leave the user's session intact; the engine will
            // kill individual panes via break-pane during teammate teardown.
            tracing::debug!(
                "TmuxBackend::destroy_swarm: inside-tmux mode is a no-op for session {}",
                handle.session_name
            );
            return Ok(());
        }
        // Outside tmux: kill the entire claude-swarm session on our socket.
        let args = vec![
            "kill-session".to_string(),
            "-t".to_string(),
            handle.session_name,
        ];
        self.run_tmux(true, &args).await?;
        Ok(())
    }

    fn is_available(&self) -> bool {
        detection::tmux_available()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test]
    async fn post_split_styling_failure_rolls_back_the_created_pane() {
        for failing_command in ["select-pane", "set-option"] {
            let root = tempfile::tempdir().unwrap();
            let command = root.path().join("mock-tmux");
            let script = format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$0.log\"\ncase \" $* \" in\n  *\" split-window \"*) printf '%%42\\n' ;;\n  *\" {failing_command} \"*) printf 'injected styling failure\\n' >&2; exit 7 ;;\nesac\n"
            );
            std::fs::write(&command, script).unwrap();
            std::fs::set_permissions(&command, std::fs::Permissions::from_mode(0o700)).unwrap();
            let backend = TmuxBackend {
                socket_name: Some("isolated-test".into()),
                command_override: Some(command.clone()),
            };
            let error = backend
                .create_configured_pane("%0", true, true)
                .await
                .unwrap_err();
            assert!(error.to_string().contains("injected styling failure"));
            let calls = std::fs::read_to_string(command.with_extension("log")).unwrap();
            let calls: Vec<_> = calls.lines().collect();
            assert!(calls[0].contains("split-window -t %0"));
            assert_eq!(
                calls.last().copied(),
                Some("-L isolated-test kill-pane -t %42")
            );
            assert_eq!(
                calls
                    .iter()
                    .filter(|call| call.contains("kill-pane"))
                    .count(),
                1
            );
        }
    }
}
