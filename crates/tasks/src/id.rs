//! Task identifier generation.
//!
//! IDs are an 8-char base-36 suffix prefixed by a type-specific character
//! (e.g. `b` for `LocalBash`). The sampler uses uniform sampling rather than
//! modulo to avoid bias.

use rand::Rng;
use serde::{Deserialize, Serialize};

/// Top-level task variant. Used both for ID prefixes and for typed state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TaskType {
    /// Local shell command.
    LocalBash,
    /// In-process LLM agent (subagent).
    LocalAgent,
    /// Remote agent over RPC.
    RemoteAgent,
    /// Teammate agent sharing the runtime process.
    InProcessTeammate,
    /// Local workflow (deterministic graph).
    LocalWorkflow,
    /// MCP server monitor.
    MonitorMcp,
    /// WebSocket monitor wire type and internal command-monitor handler key.
    Monitor,
    /// A single long-running MCP `tools/call` that exceeded
    /// `getMcpAutoBackgroundMs` and was moved to the background (claude-code
    /// 2.1.212 `mcp_task`). Distinct from [`Self::MonitorMcp`], which watches a
    /// server rather than tracking one detached tool call.
    McpTask,
    /// Background long-running "dream" loop.
    Dream,
    /// Environment reconnaissance for /auto-mode-setup.
    AutoModeScan,
    /// Fusion multi-model deliberation (`/fusion`).
    LocalFusion,
}

impl TaskType {
    /// The single character prefix used at the front of task IDs.
    #[must_use]
    pub fn id_prefix(self) -> char {
        match self {
            Self::LocalBash => 'b',
            Self::LocalAgent => 'a',
            Self::RemoteAgent => 'r',
            Self::InProcessTeammate => 't',
            Self::LocalWorkflow => 'w',
            Self::MonitorMcp => 'm',
            // 2.1.263: WebSocket monitors use s; command monitors mint b.
            Self::Monitor => 's',
            Self::McpTask => 'k',
            Self::Dream => 'd',
            Self::AutoModeScan => 'e',
            Self::LocalFusion => 'f',
        }
    }
}

const TASK_ID_ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";

/// Generate a new task ID. Uniform sampling (not modulo) to avoid bias for
/// 256/36 not being an integer.
#[must_use]
pub fn generate_task_id(task_type: TaskType) -> String {
    let mut rng = rand::rng();
    let suffix: String = (0..8)
        .map(|_| TASK_ID_ALPHABET[rng.random_range(0..TASK_ID_ALPHABET.len())] as char)
        .collect();
    format!("{}{}", task_type.id_prefix(), suffix)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn ids_are_unique_over_1000_samples() {
        let mut set = HashSet::new();
        for _ in 0..1000 {
            set.insert(generate_task_id(TaskType::LocalBash));
        }
        assert!(set.len() > 990, "too many collisions in 1000 samples");
    }

    #[test]
    fn local_fusion_id_is_f_plus_eight_base36_chars() {
        let id = generate_task_id(TaskType::LocalFusion);
        assert_eq!(id.len(), 9);
        assert!(id.starts_with('f'));
        assert!(id[1..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte.is_ascii_lowercase()));
    }
}
