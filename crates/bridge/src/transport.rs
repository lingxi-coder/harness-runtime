//! Engine-facing `IdeBridge`: starts the MCP endpoint, writes the lockfile,
//! and exposes a handle the engine keeps for the lifetime of the session.
//!
//! The wiring is five steps:
//! 1. Bind a TCP listener on `127.0.0.1:0` (ephemeral port) via
//!    [`McpEndpoint::start_on_ephemeral_port`].
//! 2. Build an [`IdeLockfile`] which generates a fresh 32-hex-char auth token.
//! 3. Hand the token to the running endpoint with
//!    [`McpEndpoint::set_auth_token`].
//! 4. Write `~/.lingxi/ide/<port>.lock` carrying that token.
//! 5. Install a [`LockfileGuard`] that removes the lockfile on shutdown or
//!    panic.
//!
//! The auth header echoed by IDE plugins is exactly
//! `X-LingXi-Ide-Authorization` — NOT `Authorization: Bearer …`.

use crate::lockfile::{IdeLockfile, LockfileGuard};
use crate::mcp_endpoint::McpEndpoint;
use crate::state::BridgeState;
use std::path::PathBuf;
use thiserror::Error;
use tokio::sync::RwLock;

/// Errors returned by [`IdeBridge::start`].
#[derive(Debug, Error)]
pub enum BridgeError {
    /// I/O failure (lockfile write, listener bind, etc.).
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// Configuration error (no workspace folders, etc.).
    #[error("config: {0}")]
    Config(String),
}

/// Engine-side bridge handle.
///
/// Construct with [`IdeBridge::start`]; the returned value owns the
/// [`McpEndpoint`] and the [`LockfileGuard`] for the lifetime of the session.
/// Dropping the handle (or calling [`shutdown`](Self::shutdown)) tears down
/// the accept loop and removes the lockfile.
pub struct IdeBridge {
    endpoint: McpEndpoint,
    // Field order matters: `endpoint` MUST drop before `_guard`. But we also
    // want the lockfile removed AFTER the accept loop quiesces — the guard
    // is here last so its `Drop::drop` fires after `endpoint`.
    _guard: LockfileGuard,
    lockfile_path: PathBuf,
    auth_token: String,
    state: RwLock<BridgeState>,
}

impl IdeBridge {
    /// Start the bridge. `workspace_folders` becomes the `workspaceFolders`
    /// array in the lockfile body and identifies the IDE session to clients
    /// scanning `~/.lingxi/ide/`.
    ///
    /// # Errors
    /// - [`BridgeError::Config`] if `workspace_folders` is empty.
    /// - [`BridgeError::Io`] if binding the listener, resolving `$HOME`, or
    ///   writing the lockfile fails.
    pub async fn start(workspace_folders: Vec<PathBuf>) -> Result<Self, BridgeError> {
        if workspace_folders.is_empty() {
            return Err(BridgeError::Config(
                "at least one workspace folder required".into(),
            ));
        }
        let endpoint = McpEndpoint::start_on_ephemeral_port().await?;
        let port = endpoint.port();
        let lockfile = IdeLockfile::for_user(port, workspace_folders)?;
        endpoint.set_auth_token(lockfile.auth_token().to_string());
        lockfile.write()?;
        let path = lockfile.path();
        let guard = LockfileGuard::new(path.clone());
        Ok(Self {
            endpoint,
            _guard: guard,
            lockfile_path: path,
            auth_token: lockfile.auth_token().to_string(),
            state: RwLock::new(BridgeState::default()),
        })
    }

    /// Bound port (ephemeral, encoded in the lockfile filename).
    #[must_use]
    pub fn port(&self) -> u16 {
        self.endpoint.port()
    }

    /// Path of the lockfile this bridge owns.
    #[must_use]
    pub fn lockfile_path(&self) -> &PathBuf {
        &self.lockfile_path
    }

    /// Auth token clients must echo in `X-LingXi-Ide-Authorization`.
    #[must_use]
    pub fn auth_token(&self) -> &str {
        &self.auth_token
    }

    /// Read-only snapshot of the bridge's observable state (connected /
    /// current file). Maintained by future M2-02d tasks as MCP events arrive.
    pub async fn state(&self) -> BridgeState {
        self.state.read().await.clone()
    }

    /// Block until shutdown. Stops the accept loop and (via the embedded
    /// [`LockfileGuard`]) removes the lockfile.
    ///
    /// The endpoint is shut down BEFORE the lockfile guard drops so any
    /// in-flight clients see the WebSocket close before the discovery file
    /// disappears.
    pub async fn shutdown(self) {
        // Bind the guard locally so its `Drop` (lockfile removal) fires AFTER
        // the endpoint accept loop has been signalled to stop. The leading
        // underscore in the field name (`_guard`) is dropped here because
        // we're rebinding it.
        let Self {
            endpoint,
            _guard: guard,
            ..
        } = self;
        endpoint.shutdown().await;
        drop(guard);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn start_rejects_empty_workspace_folders() {
        let res = IdeBridge::start(vec![]).await;
        assert!(matches!(res, Err(BridgeError::Config(_))));
    }

    #[tokio::test]
    async fn state_is_disconnected_after_start() {
        // We can't easily run `IdeBridge::start` in tests without touching
        // `~/.lingxi/ide/`, so this test only verifies the empty-input guard
        // above. Full start/shutdown is covered by the integration test in
        // tests/mcp_endpoint_test.rs.
        let bridge = IdeBridge {
            endpoint: McpEndpoint::start_on_ephemeral_port().await.unwrap(),
            _guard: LockfileGuard::new(PathBuf::from("/nonexistent-test-path")),
            lockfile_path: PathBuf::from("/nonexistent-test-path"),
            auth_token: "t".into(),
            state: RwLock::new(BridgeState::default()),
        };
        let s = bridge.state().await;
        assert!(!s.connected);
        assert!(s.current_file.is_none());
        bridge.shutdown().await;
    }
}
