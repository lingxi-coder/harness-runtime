//! Private host transport for a teammate running in a terminal pane.
//! Prompt/context travel over a private manifest, never shell arguments.
use crate::types::utf16_json::Utf16JsonProjection;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
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
        selection: crate::host::team_spawn::TeammateModelSelection,
    },
    SendMessage {
        id: u64,
        #[serde(with = "crate::types::utf16_json::raw_json_serde")]
        input: Utf16JsonProjection,
    },
    State {
        status: String,
        error: Option<String>,
        awaiting_plan_approval: bool,
    },
    Output {
        #[serde(with = "crate::types::utf16_json::raw_json_serde")]
        text: Utf16JsonProjection,
    },
    CoordinatorMessage {
        #[serde(with = "crate::types::utf16_json::raw_json_serde")]
        message: Utf16JsonProjection,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ParentToWorker {
    Message {
        #[serde(with = "crate::types::utf16_json::raw_json_serde")]
        text: Utf16JsonProjection,
    },
    PlanApprovalResponse {
        response: crate::host::teammate_plan::PlanApprovalResponse,
    },
    SendMessageResult {
        id: u64,
        #[serde(with = "crate::types::utf16_json::raw_json_serde")]
        result: Utf16JsonProjection,
        is_error: bool,
    },
    Shutdown,
}

#[derive(Debug, Clone)]
pub struct PaneMessageResult {
    pub result: Utf16JsonProjection,
    pub is_error: bool,
}

/// Forward the complete SendMessage operation to its owning session.
#[async_trait]
pub trait PaneMessageForwarder: Send + Sync {
    async fn send_message(&self, input: Utf16JsonProjection) -> Result<PaneMessageResult, String>;
}
