//! Private parent/worker pipe format; never registered on product IPC.

use super::{
    TeamHumanQuestion, TeamOwnerPermission, TeamProcessError, TeamProcessModel, TeamProcessResult,
};
use crate::desktop::team_invocation::{TeamInvocationIdentity, TeamInvocationLimits};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
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
    pub model_provider: String,
    pub model_protocol: lingxi_llm_client::protocol::ProtocolFamily,
    pub model_profile: lingxi_llm_client::protocol::ProviderProfile,
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
        TeamProcessModel::selected_api_key(
            super::TeamModelRoute {
                model_provider: self.model_provider.clone(),
                model_protocol: self.model_protocol,
                model_id: self.model_id.clone(),
                model_endpoint: self.endpoint.clone(),
                credential_id: super::model::PRIVATE_CREDENTIAL.into(),
                model_profile: self.model_profile.clone(),
            },
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
    validate_result(&result, 1)?;
    Ok(result)
}

/// Version two carries only one correlated interaction at a time. These types
/// are private to anonymous worker pipes, never product IPC or settings.
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum WorkerFrame {
    OwnerPermission { request: TeamOwnerPermission },
    HumanQuestion { request: TeamHumanQuestion },
    Result { result: TeamProcessResult },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum ParentFrame {
    OwnerPermission {
        id: String,
        allow_once: bool,
    },
    HumanQuestion {
        id: String,
        #[serde(deserialize_with = "unique_answers")]
        answers: Option<BTreeMap<String, String>>,
    },
}

pub(super) fn valid_id(id: &str) -> bool {
    (1..=128).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_graphic())
}

pub(super) fn validate_permission(request: &TeamOwnerPermission) -> Result<(), TeamProcessError> {
    if !valid_id(&request.id) || request.tool_name != "Bash" || !request.input.is_object() {
        return Err(TeamProcessError::Protocol);
    }
    // The serialized frame supplies the aggregate payload bound.
    encode(request).map(|_| ())
}

pub(super) fn validate_question(request: &TeamHumanQuestion) -> Result<(), TeamProcessError> {
    let mut seen = BTreeSet::new();
    if !valid_id(&request.id)
        || !(1..=4).contains(&request.questions.len())
        || request.text.is_empty()
        || request.text.len() > 8192
        || request.text.contains('\0')
        || request.questions.iter().any(|q| {
            q.trim().is_empty()
                || q.encode_utf16().count() > 2000
                || q.contains('\0')
                || !seen.insert(q)
        })
    {
        return Err(TeamProcessError::Protocol);
    }
    Ok(())
}

pub(super) fn validate_answers(
    request: &TeamHumanQuestion,
    answers: &Option<BTreeMap<String, String>>,
) -> Result<(), TeamProcessError> {
    validate_question(request)?;
    if let Some(answers) = answers {
        if answers.len() != request.questions.len()
            || request.questions.iter().any(|q| !answers.contains_key(q))
            || answers.values().any(|answer| {
                answer.trim().is_empty()
                    || answer.encode_utf16().count() > 2000
                    || answer.contains('\0')
            })
            || serde_json::to_vec(answers)
                .map_err(|_| TeamProcessError::Protocol)?
                .len()
                > 32768
        {
            return Err(TeamProcessError::Protocol);
        }
    }
    Ok(())
}

pub(super) fn validate_result(
    result: &TeamProcessResult,
    version: u32,
) -> Result<(), TeamProcessError> {
    if result.version != version || result.text.len() > MAX_RESULT_TEXT {
        return Err(TeamProcessError::Protocol);
    }
    Ok(())
}

pub(super) async fn read_frame<T: serde::de::DeserializeOwned>(
    input: &mut (impl tokio::io::AsyncRead + Unpin),
) -> Result<T, TeamProcessError> {
    use tokio::io::AsyncReadExt;
    let length = input.read_u32().await? as usize;
    if length == 0 || length > MAX_FRAME {
        return Err(TeamProcessError::Protocol);
    }
    let mut bytes = vec![0; length];
    input.read_exact(&mut bytes).await?;
    serde_json::from_slice(&bytes).map_err(|_| TeamProcessError::Protocol)
}

// A repeated JSON answer key must not silently overwrite an earlier answer.
fn unique_answers<'de, D>(deserializer: D) -> Result<Option<BTreeMap<String, String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct UniqueAnswers(BTreeMap<String, String>);
    impl<'de> Deserialize<'de> for UniqueAnswers {
        fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            struct Visitor;
            impl<'de> serde::de::Visitor<'de> for Visitor {
                type Value = UniqueAnswers;
                fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                    formatter.write_str("unique exact question keys")
                }
                fn visit_map<M: serde::de::MapAccess<'de>>(
                    self,
                    mut map: M,
                ) -> Result<Self::Value, M::Error> {
                    let mut answers = BTreeMap::new();
                    while let Some((key, value)) = map.next_entry::<String, String>()? {
                        if answers.insert(key, value).is_some() {
                            return Err(serde::de::Error::custom("duplicate answer key"));
                        }
                    }
                    Ok(UniqueAnswers(answers))
                }
            }
            deserializer.deserialize_map(Visitor)
        }
    }
    Option::<UniqueAnswers>::deserialize(deserializer).map(|answers| answers.map(|a| a.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_have_no_extra_fields_or_duplicate_answer_keys() {
        for json in [
            r#"{"type":"owner_permission","id":"interaction-1","allow_once":true,"always":true}"#,
            r#"{"type":"owner_permission","id":"interaction-1","allow_once":"yes"}"#,
            r#"{"type":"human_question","id":"interaction-1","answers":{"original":"first","original":"second"}}"#,
            r#"{"type":"start","id":"interaction-1"}"#,
        ] {
            assert!(serde_json::from_str::<ParentFrame>(json).is_err());
        }
    }

    #[test]
    fn answer_keys_and_utf16_bounds_are_checked_before_projection() {
        let request = TeamHumanQuestion {
            id: "interaction-1".into(),
            questions: vec!["Which résumé?".into()],
            text: "Which résumé?\n- Current: use current".into(),
        };
        assert!(validate_answers(&request, &None).is_ok());
        assert!(validate_answers(
            &request,
            &Some(BTreeMap::from([("Which résumé?".into(), "Current".into())]))
        )
        .is_ok());
        for answers in [
            BTreeMap::from([("Which resume?".into(), "Current".into())]),
            BTreeMap::from([("Which résumé?".into(), " ".into())]),
            BTreeMap::from([("Which résumé?".into(), "😀".repeat(1001))]),
        ] {
            assert!(validate_answers(&request, &Some(answers)).is_err());
        }
    }
}
