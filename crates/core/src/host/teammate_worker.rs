//! Private host transport for a teammate running in a terminal pane.
//! Prompt/context travel over a private manifest, never shell arguments.
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneTeammateManifest {
    pub socket_path: PathBuf,
    pub token: String,
    pub agent_id: crate::types::AgentId,
    pub name: String,
    pub team_name: String,
    pub parent_session_id: crate::types::SessionId,
    pub request: crate::host::subagent_spawn::SubagentSpawnRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerToParent {
    Hello {
        token: String,
    },
    Ready {
        task_id: String,
    },
    SendMessage {
        id: u64,
        input: Value,
    },
    State {
        status: String,
        error: Option<String>,
        awaiting_plan_approval: bool,
    },
    Output {
        text: String,
    },
    CoordinatorMessage {
        message: Value,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ParentToWorker {
    Message {
        text: String,
    },
    PlanApprovalResponse {
        response: crate::host::teammate_plan::PlanApprovalResponse,
    },
    SendMessageResult {
        id: u64,
        result: Value,
        is_error: bool,
    },
    Shutdown,
}

#[derive(Debug, Clone)]
pub struct PaneMessageResult {
    pub result: Value,
    pub is_error: bool,
}

/// Forward the complete SendMessage operation to its owning session.
#[async_trait]
pub trait PaneMessageForwarder: Send + Sync {
    async fn send_message(&self, input: Value) -> Result<PaneMessageResult, String>;
}
