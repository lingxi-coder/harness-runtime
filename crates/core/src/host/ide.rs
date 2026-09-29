//! Provider-neutral local IDE integration primitives.
//!
//! The engine only needs an endpoint inventory and a small lifecycle surface;
//! discovery, local authentication, and transport details stay in the
//! composition/platform layer. In particular, endpoint summaries deliberately
//! contain no bearer token so they are safe to render in `/ide` status output.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Local transport advertised by an IDE lockfile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IdeTransport {
    /// Server-Sent Events over loopback HTTP.
    Sse,
    /// MCP messages over a loopback WebSocket.
    Ws,
}

/// Public, secret-free description of a discovered IDE endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdeEndpointInfo {
    /// Stable local identifier used by connect/disconnect commands. This is
    /// normally the lockfile path, never a token.
    pub id: String,
    /// Human-readable IDE name from the lockfile.
    pub name: String,
    /// Wire transport selected by the lockfile.
    pub transport: IdeTransport,
    /// Loopback port encoded by the lockfile filename.
    pub port: u16,
    /// Workspace roots advertised by the IDE peer.
    pub workspace_folders: Vec<PathBuf>,
    /// Whether the peer uses Windows path semantics.
    pub running_in_windows: bool,
    /// Whether this endpoint is currently selected and connected.
    pub connected: bool,
}

/// Snapshot returned by the live IDE controller.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdeStatus {
    /// Valid endpoints discovered at the time of the snapshot.
    pub endpoints: Vec<IdeEndpointInfo>,
    /// Stable id of the selected endpoint, if any.
    pub selected: Option<String>,
}

impl IdeStatus {
    /// Whether a live endpoint is currently connected.
    #[must_use]
    pub fn connected(&self) -> bool {
        self.selected.is_some() && self.endpoints.iter().any(|endpoint| endpoint.connected)
    }
}

/// Engine-facing lifecycle for local IDE endpoints.
///
/// Implementations own discovery and transport credentials. Callers receive
/// only secret-free status and pass the live session CWD to [`Self::open`].
#[async_trait]
pub trait IdeHandle: Send + Sync {
    /// Refresh and return the current endpoint/status snapshot.
    async fn status(&self) -> IdeStatus;

    /// Connect to the endpoint identified by `endpoint_id`.
    async fn connect(&self, endpoint_id: &str) -> Result<IdeStatus, String>;

    /// Disconnect the selected endpoint, if one is connected.
    async fn disconnect(&self) -> Result<IdeStatus, String>;

    /// Ask the selected IDE to open the supplied live session directory.
    async fn open(&self, cwd: PathBuf) -> Result<String, String>;

    /// Auto-connect only when discovery yields exactly one valid endpoint.
    /// Returns `true` when a connection was attempted and established.
    async fn auto_connect_if_single(&self) -> Result<bool, String>;
}
