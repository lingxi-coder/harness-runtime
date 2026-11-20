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
    /// A task-owned Data workspace with no shared Chat or mutation tools.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub private_data: bool,
    /// Register the ask-the-requester tool for this process.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ask_requester: bool,
    /// Register the shared-list tools for this process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared_list: Option<super::TeamSharedList>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shared_data: Vec<super::TeamDatasetOffer>,
    /// Register the no-reply tool for this process.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub may_decline: bool,
    /// With `may_decline` only: the message was written to the Bot directly.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub addressed: bool,
}
pub(super) fn private_data_scope(root: &std::path::Path, scope: &BotChatScope, task_id: &str) -> bool {
    valid_id(task_id)
        && scope.conversation_id == task_id
        && root.file_name().and_then(|name| name.to_str()) == Some(task_id)
        && root.parent().and_then(|parent| parent.file_name()).and_then(|name| name.to_str()) == Some("data-private")
}

pub(super) fn private_data_requester(
    root: &std::path::Path,
    scope: &BotChatScope,
    task_id: &str,
) -> Result<String, TeamProcessError> {
    use std::io::Read;
    use lingxi_core::host::rooted_fs::{open_read_file_pinned, root_identity};
    if !private_data_scope(root, scope, task_id) {
        return Err(TeamProcessError::Protocol);
    }
    let identity = root_identity(root).map_err(|_| TeamProcessError::Protocol)?;
    let file = open_read_file_pinned(root, std::path::Path::new("admission.json"), Some(&identity))
        .map_err(|_| TeamProcessError::Protocol)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > 2048 {
        return Err(TeamProcessError::Protocol);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.mode() & 0o077 != 0 || metadata.nlink() != 1
            || metadata.uid() != nix::unistd::geteuid().as_raw()
        {
            return Err(TeamProcessError::Protocol);
        }
    }
    let mut bytes = Vec::new();
    file.take(2049).read_to_end(&mut bytes)?;
    let (version, owner, requester, team, bot, saved_task): (u8, String, String, String, String, String) =
        serde_json::from_slice(&bytes).map_err(|_| TeamProcessError::Protocol)?;
    let uid = |value: &str| {
        !value.is_empty() && value.len() <= 128
            && value.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    };
    if version != 1 || !uid(&owner) || !uid(&requester)
        || team != scope.team_id || bot != scope.shared_bot_id || saved_task != task_id
    {
        return Err(TeamProcessError::Protocol);
    }
    Ok(requester)
}

pub(super) fn validate_private_data_input(
    batch: &crate::desktop::session_state::team_chat::ChatInputBatch,
    requester: &str,
    task_id: &str,
) -> Result<(), TeamProcessError> {
    use crate::desktop::session_state::team_chat::ChatAuthor;
    let [message] = batch.messages.as_slice() else {
        return Err(TeamProcessError::Protocol);
    };
    if batch.batch_id != task_id || !batch.bootstrap || !batch.files.is_empty()
        || !batch.changes.is_empty() || message.data_ref.is_some() || message.reply_to.is_some()
        || !matches!(&message.author, ChatAuthor::User { uid, .. } if uid == requester)
    {
        return Err(TeamProcessError::Protocol);
    }
    Ok(())
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
    Operate {
        request: super::TeamOperationRequest,
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
    Operated {
        id: String,
        result: super::TeamOperationResult,
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
pub(super) fn validate_result(result: &TeamProcessResult, private_data: bool) -> Result<(), TeamProcessError> {
    use sha2::{Digest, Sha256};
    if result.version != 3 || result.text.len() > 8192 || result.finished_at == 0 {
        return Err(TeamProcessError::Protocol);
    }
    if matches!(
        result.outcome,
        super::TeamProcessOutcome::Waiting | super::TeamProcessOutcome::Declined
    ) && !result.text.is_empty()
    {
        return Err(TeamProcessError::Protocol);
    }
    if let super::TeamUsage::Complete { currency, .. } = &result.usage {
        if currency != "USD" {
            return Err(TeamProcessError::Protocol);
        }
    }
    if private_data {
        if result.document.is_some() || matches!(result.outcome, super::TeamProcessOutcome::Waiting | super::TeamProcessOutcome::Declined) {
            return Err(TeamProcessError::Protocol);
        }
        return Ok(());
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
        assert!(validate_result(&result, false).is_ok());
        result
            .document
            .as_mut()
            .unwrap()
            .markdown
            .push_str(" changed");
        assert!(validate_result(&result, false).is_err());
        result.document = Some(super::super::chat_session::document(
            "task_test",
            "Accepted",
        ));
        result.text = "x".repeat(8193);
        assert!(validate_result(&result, false).is_err());
        result.text.clear();
        result.outcome = super::super::TeamProcessOutcome::ModelFailed;
        assert!(validate_result(&result, false).is_err());
        result.document = None;
        assert!(validate_result(&result, false).is_ok());
    }
}
