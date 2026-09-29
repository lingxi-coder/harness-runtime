//! Per-connection LSP state machine.
//!
//! Each registered server progresses through these states in order:
//! `Disconnected` → `Starting` → `Initialized` → `Stopped` (with a side
//! transition to `Failed` if anything goes wrong). The registry holds one
//! [`LspConnectionState`] per server name.
//!
//! See spec §25.2 (`LspConnectionState`).

use lingxi_core::host::{LspServerCapabilities, LspServerConfig};
use lingxi_core::types::McpConnectionId;
use std::time::SystemTime;

/// State of one LSP server's connection.
#[derive(Debug, Clone)]
pub enum LspConnectionState {
    /// Server is registered but not yet started.
    Disconnected {
        /// Configuration registered for this server.
        config: LspServerConfig,
    },
    /// Server process has been spawned but `initialize` has not yet
    /// completed.
    Starting {
        /// Configuration registered for this server.
        config: LspServerConfig,
        /// When the start was triggered.
        started_at: SystemTime,
        /// OS-level process id.
        pid: u32,
        /// Restart counter inherited from the prior `Failed` / `Initialized`
        /// state so cancellation during startup does not reset crash recovery.
        restarts: u32,
    },
    /// `initialize` succeeded and the server is ready to handle requests.
    Initialized {
        /// Configuration registered for this server.
        config: LspServerConfig,
        /// Stable connection identifier.
        connection_id: McpConnectionId,
        /// Capabilities reported by the server.
        server_capabilities: LspServerCapabilities,
        /// OS-level process id.
        pid: u32,
        /// Restart counter for the current crash-recovery window. Successful
        /// startup resets this to zero; runtime crashes increment it when the
        /// registry re-enters `Failed`.
        restarts: u32,
    },
    /// Start-up or runtime failed; the registry retries the next request
    /// from this state until the crash-recovery cap (claude-code's
    /// `ensureServerStarted` retries from `error` until
    /// `restartCount > maxRestarts ?? 3`).
    Failed {
        /// Configuration registered for this server.
        config: LspServerConfig,
        /// Human-readable failure description (claude-code `lastError`).
        error: String,
        /// Failed start attempts so far (claude-code `restartCount`).
        restarts: u32,
        /// Whether the exceeded-max-crash-recovery error has already been
        /// logged + recorded (claude-code reports it exactly once, then keeps
        /// rethrowing the recorded error).
        max_recovery_reported: bool,
    },
    /// Server was shut down cleanly.
    Stopped {
        /// Configuration registered for this server.
        config: LspServerConfig,
    },
}

impl LspConnectionState {
    /// The server configuration this state carries (present in every variant).
    #[must_use]
    pub fn config(&self) -> &LspServerConfig {
        match self {
            LspConnectionState::Disconnected { config }
            | LspConnectionState::Starting { config, .. }
            | LspConnectionState::Initialized { config, .. }
            | LspConnectionState::Failed { config, .. }
            | LspConnectionState::Stopped { config } => config,
        }
    }
}
