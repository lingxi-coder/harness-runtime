//! Session-level metadata persisted alongside the transcript (spec §22.2).
//!
//! Survives across resumes so the engine can reconstruct project root,
//! model selection, enabled plugins, and other context that does not live
//! inside the conversation itself.

use protocol::{PluginId, SessionId};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::SystemTime;

/// Stable, on-disk session header. Written once at create and rewritten in
/// place whenever long-lived fields change.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMetadata {
    /// This session's identifier.
    pub session_id: SessionId,
    /// Parent session this one was forked or resumed from, if any.
    pub parent_session_id: Option<SessionId>,
    /// When the session was created.
    pub created_at: SystemTime,
    /// Top-level project directory (workspace root).
    pub project_dir: PathBuf,
    /// Current working directory at start.
    pub cwd: PathBuf,
    /// Optional named agent type (e.g. "coordinator", "task").
    pub agent_type: Option<String>,
    /// Model id (e.g. "claude-3-7-sonnet").
    pub model: String,
    /// Permission mode (`ask`, `auto`, etc.).
    pub permission_mode: String,
    /// Whether the session is running in coordinator mode.
    pub coordinator_mode: bool,
    /// Plugin ids enabled at session start.
    pub enabled_plugins: Vec<PluginId>,
    /// MCP servers enabled at session start.
    pub mcp_servers_enabled: Vec<String>,
    /// Extra working directories (multi-root sessions).
    pub working_directories: Vec<PathBuf>,
    /// Output style name in effect.
    pub current_output_style: String,
    /// LINGXI.md hierarchy used for context priming.
    pub lingxi_md_paths: Vec<PathBuf>,
    /// Wall-clock time of the most recent metadata write.
    pub last_modified: SystemTime,
}
