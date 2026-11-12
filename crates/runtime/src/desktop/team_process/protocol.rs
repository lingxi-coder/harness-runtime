//! Major 3 only. Bounded control frames refer to complete digest-checked private data.
use super::{
    TeamChatReadRequest, TeamDataRef, TeamProcessError, TeamProcessModel, TeamProcessResult,
};
use crate::desktop::team_invocation::{BotChatScope, TeamInvocationLimits};
use serde::{Deserialize, Serialize};
use std::num::NonZeroU32;
use std::path::PathBuf;
pub(super) const MAX_FRAME: usize = 256 * 1024;
pub(super) const MAX_RESULT_TEXT: usize = 64 * 1024;
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Bootstrap {
    pub version: u32,
    pub storage_root: PathBuf,
    pub scope: BotChatScope,
    pub invocation_id: uuid::Uuid,
    pub task_id: String,
    pub instructions: String,
    pub input_batch_ref: TeamDataRef,
    pub knowledge_ref: TeamDataRef,
    pub max_turns: u32,
    pub context_window_tokens: u32,
    pub output_tokens_per_request: u32,
    pub physical_dispatches: u32,
    pub deadline_unix_ms: u64,
    pub model_provider: String,
    pub model_protocol: lingxi_llm_client::protocol::ProtocolFamily,
    pub model_profile: lingxi_llm_client::protocol::ProviderProfile,
    pub model_id: String,
    pub endpoint: String,
    pub credentials: super::TeamAccountEnvelope,
    /// Present only for `continue`; absent bytes are the epoch-1 `run` bootstrap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation: Option<super::TeamContinuation>,
    /// Register the ask-the-requester tool for this process.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ask_requester: bool,
}
impl Bootstrap {
    pub fn limits(&self) -> Result<TeamInvocationLimits, TeamProcessError> {
        let n = |x| NonZeroU32::new(x).ok_or(TeamProcessError::Protocol);
        Ok(TeamInvocationLimits {
            max_turns: n(self.max_turns)?,
            context_window_tokens: n(self.context_window_tokens)?,
            output_tokens_per_request: n(self.output_tokens_per_request)?,
            physical_dispatches: n(self.physical_dispatches)?,
        })
    }
    pub fn model(&self) -> Result<TeamProcessModel, TeamProcessError> {
        TeamProcessModel::selected_account_pool(
            super::TeamModelRoute {
                model_provider: self.model_provider.clone(),
                model_protocol: self.model_protocol,
                model_id: self.model_id.clone(),
                model_endpoint: self.endpoint.clone(),
                credential_id: super::model::PRIVATE_CREDENTIAL.into(),
                model_profile: self.model_profile.clone(),
            },
            self.credentials.clone_private()?,
        )
    }
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum WorkerFrame {
    Failed {
        code: WorkerFailure,
    },
    Authorize {
        id: String,
    },
    ProviderAccountRefresh {
        request: super::TeamProviderAccountRefresh,
    },
    ReadChat {
        request: TeamChatReadRequest,
    },
    Result {
        result: TeamProcessResult,
    },
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum WorkerFailure {
    Capacity,
    ResourceLimit,
    Protocol,
    Profile,
    Environment,
    Model,
    Storage,
    Worker,
    CheckpointMismatch,
    NotContinuable,
    RouteUnsupported,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum ParentFrame {
    Authorized {
        id: String,
    },
    ProviderAccountsUpdated {
        id: String,
        credentials: super::TeamAccountEnvelope,
    },
    ChatRead {
        id: String,
        data: TeamDataRef,
    },
}
pub(super) fn valid_id(id: &str) -> bool {
    (1..=128).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_graphic())
}
pub(super) fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, TeamProcessError> {
    let body = serde_json::to_vec(value).map_err(|_| TeamProcessError::Protocol)?;
    if body.is_empty() || body.len() > MAX_FRAME {
        return Err(TeamProcessError::Protocol);
    }
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend(body);
    Ok(frame)
}
pub(super) async fn read_frame<T: serde::de::DeserializeOwned>(
    input: &mut (impl tokio::io::AsyncRead + Unpin),
) -> Result<T, TeamProcessError> {
    use tokio::io::AsyncReadExt;
    let n = input.read_u32().await? as usize;
    if n == 0 || n > MAX_FRAME {
        return Err(TeamProcessError::Protocol);
    }
    let mut b = vec![0; n];
    input.read_exact(&mut b).await?;
    serde_json::from_slice(&b).map_err(|_| TeamProcessError::Protocol)
}
pub(super) fn validate_result(result: &TeamProcessResult) -> Result<(), TeamProcessError> {
    use sha2::{Digest, Sha256};
    if result.version != 3 || result.text.len() > 8192 || result.finished_at == 0 {
        return Err(TeamProcessError::Protocol);
    }
    if result.outcome == super::TeamProcessOutcome::Waiting && !result.text.is_empty() {
        return Err(TeamProcessError::Protocol);
    }
    if let super::TeamUsage::Complete { currency, .. } = &result.usage {
        if currency != "USD" {
            return Err(TeamProcessError::Protocol);
        }
    }
    match (&result.outcome, &result.document) {
        (super::TeamProcessOutcome::Completed, Some(document))
            if !document.markdown.trim().is_empty()
                && document.markdown.len() <= MAX_RESULT_TEXT
                && document.sha256
                    == format!("{:x}", Sha256::digest(document.markdown.as_bytes())) => {}
        (super::TeamProcessOutcome::Completed, _) | (_, Some(_)) => {
            return Err(TeamProcessError::Protocol);
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_result_rejects_corrupt_document_or_oversized_chat_summary() {
        let mut result = TeamProcessResult {
            version: 3,
            outcome: super::super::TeamProcessOutcome::Completed,
            text: "Accepted".into(),
            dispatched_requests: 1,
            attempt_dispatched_requests: 1,
            denied_tools: 0,
            finished_at: 1,
            cursor: Default::default(),
            document: Some(super::super::chat_session::document(
                "task_test",
                "Accepted",
            )),
            usage: super::super::TeamUsage::Missing,
        };
        assert!(validate_result(&result).is_ok());
        result
            .document
            .as_mut()
            .unwrap()
            .markdown
            .push_str(" changed");
        assert!(validate_result(&result).is_err());
        result.document = Some(super::super::chat_session::document(
            "task_test",
            "Accepted",
        ));
        result.text = "x".repeat(8193);
        assert!(validate_result(&result).is_err());
        result.text.clear();
        result.outcome = super::super::TeamProcessOutcome::ModelFailed;
        assert!(validate_result(&result).is_err());
        result.document = None;
        assert!(validate_result(&result).is_ok());
    }
}
