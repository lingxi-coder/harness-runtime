//! Shared desktop/mobile transcript identity projection.

use client::protocol::error::ClientError;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn main_native_rows_keep_identity_gaps_and_use_the_existing_replay_decoder() {
        let raw = concat!(
            "{\"type\":\"user\",\"uuid\":\"11111111-1111-4111-8111-111111111111\",\"sessionId\":\"22222222-2222-4222-8222-222222222222\",\"parentUuid\":null,\"timestamp\":\"2026-10-08T00:00:00Z\",\"cwd\":\"/workspace\",\"version\":\"2.1.293\",\"isSidechain\":false,\"message\":{\"role\":\"user\",\"content\":\"hello\"}}\n"
        );
        let messages = session::jsonl::reader::route_lines(raw).messages_in_order;
        let state = orchestrator::state_from_messages(uuid::Uuid::new_v4(), &messages);
        assert_eq!(state.history.len(), 1);
        let runtime_metadata = orchestrator::runtime_metadata_from_messages(&messages);
        let replayed = orchestrator::ReplayedSession {
            display_history: state.history.clone(),
            state,
            messages,
            last_message_uuid: None,
            runtime_metadata,
            client_state_tool_results: Default::default(),
        };
        let mut identities = session::jsonl::SessionMessageIdentitySnapshot::default();
        identities
            .by_uuid
            .insert("11111111-1111-4111-8111-111111111111".into(), 7);
        identities.next_message_index = 11;
        let rows = parse_main_session_agent_message_rows(&replayed, &identities).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].message_index, 7);
        assert_eq!(rows[0].message_uuid, "11111111-1111-4111-8111-111111111111");
        assert!(parse_main_session_agent_message_rows(&replayed, &Default::default()).is_err());
    }
}

/// Project Agent rows using their recorded stream indices and row identities.
pub fn parse_session_agent_message_rows(
    raw: &[u8],
) -> Result<Vec<client::protocol::listings::SessionAgentMessageRowDto>, String> {
    parse_session_agent_message_rows_with_identity(raw, |value, message| {
        Some((
            value.get("message_index")?.as_u64()?,
            value
                .get("uuid")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| message.id().as_uuid().to_string()),
        ))
    })
}

/// Project an already replayed main transcript against its Host identity index.
pub fn parse_main_session_agent_message_rows(
    replayed: &orchestrator::ReplayedSession,
    identities: &session::jsonl::SessionMessageIdentitySnapshot,
) -> Result<Vec<client::protocol::listings::SessionAgentMessageRowDto>, String> {
    client::adapter::lowering::lower_transcript_with_identities(
        &replayed.display_history,
        &replayed.client_state_tool_results,
    )
    .into_iter()
    .map(|(id, message)| {
        let message_uuid = id.as_uuid().to_string();
        let message_index = *identities.by_uuid.get(&message_uuid).ok_or_else(|| {
            format!("main transcript row lacks its stable identity index: {message_uuid}")
        })?;
        let original = replayed
            .messages
            .iter()
            .find(|row| row.uuid == message_uuid);
        let api_error_json = match original {
            Some(row)
                if row.extra.get("isApiErrorMessage") == Some(&serde_json::Value::Bool(true)) =>
            {
                Some(
                    String::from_utf8(
                        session::jsonl::exact_json::native_message_bytes(row)
                            .map_err(|error| error.to_string())?,
                    )
                    .map_err(|error| error.to_string())?,
                )
            }
            Some(row) => row
                .extra
                .get("server_fallback_api_error_json")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            None => None,
        };
        Ok(client::protocol::listings::SessionAgentMessageRowDto {
            message_index,
            message_uuid,
            message,
            api_error_json,
        })
    })
    .collect()
}

fn session_agent_message_from_projection(
    row: &lingxi_core::types::utf16_json::Utf16JsonProjection,
) -> Result<lingxi_core::types::ConversationMessage, String> {
    use lingxi_core::types::{ContentBlock, ConversationMessage};

    let value = row
        .value
        .get("message")
        .cloned()
        .ok_or_else(|| "session-agent transcript row omitted its message".to_string())?;
    let mut message: ConversationMessage = serde_json::from_value(value)
        .map_err(|error| format!("invalid session-agent message: {error}"))?;
    let content = match &mut message {
        ConversationMessage::User { content, .. }
        | ConversationMessage::Assistant { content, .. } => content,
        ConversationMessage::System { .. } => return Ok(message),
    };
    for (index, block) in content.iter_mut().enumerate() {
        let pointer = format!("/message/content/{index}/text");
        let Some(code_units) = row.string_units(&pointer) else {
            continue;
        };
        let ContentBlock::Text { text, citations } = block else {
            return Err(format!(
                "session-agent exact text points to a non-text block at {pointer}"
            ));
        };
        if String::from_utf16_lossy(&code_units) != *text {
            return Err(format!(
                "session-agent exact text display does not match {pointer}"
            ));
        }
        *block = ContentBlock::TextJsUtf16 {
            text: text.clone(),
            utf16_code_units: code_units,
            citations: citations.clone(),
        };
    }
    Ok(message)
}

/// Read or explicitly import identity facts through the session's existing writer.
pub async fn session_identity_snapshot_for_path(
    writer: &session::jsonl::JsonlWriter,
    transcript_path: &std::path::Path,
) -> Result<session::jsonl::SessionMessageIdentitySnapshot, String> {
    match tokio::fs::symlink_metadata(transcript_path).await {
        Ok(metadata) if !metadata.file_type().is_file() => {
            return Err(format!(
                "transcript is not a regular file: {}",
                transcript_path.display()
            ));
        }
        Ok(metadata) if metadata.len() > 0 => writer
            .bootstrap_session_message_identity_snapshot(transcript_path)
            .await
            .map_err(|error| error.to_string()),
        Ok(_) => writer
            .read_session_message_identity_snapshot(transcript_path)
            .await
            .map_err(|error| error.to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => writer
            .read_session_message_identity_snapshot(transcript_path)
            .await
            .map_err(|error| error.to_string()),
        Err(error) => Err(format!(
            "could not inspect transcript {}: {error}",
            transcript_path.display()
        )),
    }
}

fn parse_session_agent_message_rows_with_identity(
    raw: &[u8],
    mut identity_for: impl FnMut(
        &serde_json::Value,
        &lingxi_core::types::ConversationMessage,
    ) -> Option<(u64, String)>,
) -> Result<Vec<client::protocol::listings::SessionAgentMessageRowDto>, String> {
    let mut rows = Vec::new();
    let mut tool_index = client::adapter::turn::ToolUseIndex::default();
    for line in raw
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let line = std::str::from_utf8(line)
            .map_err(|error| format!("invalid session-agent transcript UTF-8: {error}"))?;
        let projection = lingxi_core::types::utf16_json::Utf16JsonProjection::parse(line)
            .map_err(|error| format!("invalid session-agent transcript row: {error}"))?;
        let value = &projection.value;
        // Source-attachment sidecars retain a duplicate `message` projection
        // beside their payload; they are not another conversation row.
        if value.get("type").and_then(serde_json::Value::as_str) == Some("attachment") {
            continue;
        }
        let Some(message_value) = value.get("message") else {
            continue;
        };
        if message_value.is_null() {
            continue;
        }
        let message = session_agent_message_from_projection(&projection)?;
        if matches!(
            &message,
            lingxi_core::types::ConversationMessage::System {
                subtype: Some(subtype),
                ..
            } if subtype.starts_with("agent_")
        ) || !session_agent_conversation_is_visible(&message)
        {
            continue;
        }
        let (message_index, message_uuid) = identity_for(value, &message).ok_or_else(|| {
            "session-agent message row lacks its stable identity index".to_string()
        })?;
        let message =
            client::adapter::lowering::lower_conversation_message_with(&message, &mut tool_index);
        rows.push(client::protocol::listings::SessionAgentMessageRowDto {
            message_index,
            message_uuid,
            message,
            api_error_json: value
                .get("server_fallback_api_error_json")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .or_else(|| {
                    (value.get("isApiErrorMessage") == Some(&serde_json::Value::Bool(true)))
                        .then(|| serde_json::to_string(value).ok())
                        .flatten()
                }),
        });
    }
    Ok(rows)
}

/// Read the Agent allocator's persisted high-water mark through rooted file IO.
pub async fn read_session_agent_next_message_index(
    fs: &dyn lingxi_core::host::FileSystem,
    path: &std::path::Path,
) -> Result<u64, ClientError> {
    let root = path.parent().ok_or_else(|| ClientError::Rejected {
        message: "session-agent transcript has no parent directory".into(),
    })?;
    let filename = path.file_name().ok_or_else(|| ClientError::Rejected {
        message: "session-agent transcript has no filename".into(),
    })?;
    let mut metadata_path = filename.to_os_string();
    metadata_path.push(".meta");
    let metadata_path = std::path::PathBuf::from(metadata_path);
    let file = fs
        .read_file_rooted_no_follow_window(root, &metadata_path, None, None)
        .await
        .map_err(|error| ClientError::Rejected {
            message: format!("read session-agent message index failed: {error}"),
        })?;
    if file.truncated {
        return Err(ClientError::Rejected {
            message: "session-agent message index is truncated".into(),
        });
    }
    let metadata: serde_json::Value =
        serde_json::from_str(&file.content).map_err(|error| ClientError::Rejected {
            message: format!("parse session-agent message index failed: {error}"),
        })?;
    metadata
        .get("next_message_index")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| ClientError::Rejected {
            message: "session-agent index metadata omitted next_message_index".into(),
        })
}

/// Whether a message belongs in a visible Agent transcript snapshot.
pub fn session_agent_conversation_is_visible(
    message: &lingxi_core::types::ConversationMessage,
) -> bool {
    !matches!(
        message,
        lingxi_core::types::ConversationMessage::User { is_meta: true, .. }
            | lingxi_core::types::ConversationMessage::User {
                is_compact_summary: true,
                ..
            }
            | lingxi_core::types::ConversationMessage::User {
                is_visible_in_transcript_only: true,
                ..
            }
    )
}
