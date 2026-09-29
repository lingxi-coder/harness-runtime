//! Shell metadata transferred alongside an authenticated native supervisor.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShellTaskHandoff {
    pub task_id: String,
    pub command: String,
    pub description: String,
    #[serde(default)]
    pub tool_use_id: Option<String>,
    #[serde(default)]
    pub creator_agent_id: Option<crate::types::AgentId>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub caller: Option<String>,
    #[serde(default)]
    pub output_offset: u64,
    pub process: crate::host::process::ShellProcessHandoff,
}
