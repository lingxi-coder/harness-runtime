//! Private parent/worker pipe format; never registered on product IPC.

use super::{TeamProcessError, TeamProcessModel, TeamProcessResult};
use crate::desktop::team_invocation::{TeamInvocationIdentity, TeamInvocationLimits};
use serde::{Deserialize, Serialize};
use std::num::NonZeroU32;
use std::path::PathBuf;

pub(super) const MAX_FRAME: usize = 256 * 1024;
pub(super) const MAX_RESULT_TEXT: usize = 8192;

// No Debug: the bootstrap contains the single selected credential and text.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Bootstrap {
    pub version: u32,
    pub storage_root: PathBuf,
    pub owner_uid: String,
    pub bot_key: String,
    pub team_id: String,
    pub conversation_id: String,
    pub origin_human_uid: String,
    pub membership_generation: u64,
    pub invocation_id: uuid::Uuid,
    pub instructions: String,
    pub input: String,
    pub max_turns: u32,
    pub max_input_bytes: u32,
    pub output_tokens_per_request: u32,
    pub physical_dispatches: u32,
    pub deadline_unix_ms: u64,
    pub model_id: String,
    pub endpoint: String,
    pub api_key: String,
}

impl Bootstrap {
    pub fn identity(&self) -> TeamInvocationIdentity {
        TeamInvocationIdentity {
            owner_uid: self.owner_uid.clone(),
            bot_key: self.bot_key.clone(),
            team_id: self.team_id.clone(),
            conversation_id: self.conversation_id.clone(),
            origin_human_uid: self.origin_human_uid.clone(),
            membership_generation: self.membership_generation,
        }
    }

    pub fn limits(&self) -> Result<TeamInvocationLimits, TeamProcessError> {
        let nonzero = |n| NonZeroU32::new(n).ok_or(TeamProcessError::Protocol);
        Ok(TeamInvocationLimits {
            max_turns: nonzero(self.max_turns)?,
            max_input_bytes: nonzero(self.max_input_bytes)?,
            output_tokens_per_request: nonzero(self.output_tokens_per_request)?,
            physical_dispatches: nonzero(self.physical_dispatches)?,
        })
    }

    pub fn model(&self) -> Result<TeamProcessModel, TeamProcessError> {
        TeamProcessModel::anthropic_api_key(
            self.model_id.clone(),
            self.endpoint.clone(),
            self.api_key.clone(),
        )
    }
}

pub(super) fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, TeamProcessError> {
    let body = serde_json::to_vec(value).map_err(|_| TeamProcessError::Protocol)?;
    if body.is_empty() || body.len() > MAX_FRAME {
        return Err(TeamProcessError::Protocol);
    }
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend_from_slice(&body);
    Ok(frame)
}

pub(super) fn decode_result(bytes: &[u8]) -> Result<TeamProcessResult, TeamProcessError> {
    if bytes.len() < 4 {
        return Err(TeamProcessError::Protocol);
    }
    let length = u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize;
    if length == 0 || length > MAX_FRAME || bytes.len() != length + 4 {
        return Err(TeamProcessError::Protocol);
    }
    let result: TeamProcessResult =
        serde_json::from_slice(&bytes[4..]).map_err(|_| TeamProcessError::Protocol)?;
    if result.version != 1 || result.text.len() > MAX_RESULT_TEXT {
        return Err(TeamProcessError::Protocol);
    }
    Ok(result)
}
