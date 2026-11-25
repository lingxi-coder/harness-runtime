//! Durable Inline calendar state and append-only date notifications.

use super::{ConversationOrchestrator, DateChangeState};
use crate::OrchestratorError;
use lingxi_core::types::{
    utf16_json::Utf16JsonProjection as Source, ConversationMessage, MessageId, SessionId,
};
use session::JsonlMessage;
use tokio_util::sync::CancellationToken;

const CHECKPOINT_KIND: &str = "inline_date_state";
const NOTIFICATION_KIND: &str = "date_change";

#[derive(Clone, Debug)]
pub(super) struct PendingDateChange {
    date: String,
    message: ConversationMessage,
    payload: Source,
}

fn valid_date(date: &str) -> bool {
    jiff::civil::Date::strptime("%Y-%m-%d", date)
        .is_ok_and(|parsed| parsed.strftime("%Y-%m-%d").to_string() == date)
}

fn calendar_payload(kind: &str, session_date: &str, accepted_date: Option<&str>) -> Source {
    Source::from_serde_value(&serde_json::json!({
        "type": kind,
        "contextRendering": "inline",
        "sessionDate": session_date,
        "acceptedDate": accepted_date,
    }))
    .expect("host calendar state is JSON")
}

fn date_metadata(row: &Source, session: SessionId) -> Option<(MessageId, &str, Option<&str>)> {
    if row.string_at("/type") != Some("attachment")
        || row
            .string_at("/sessionId")
            .and_then(|id| uuid::Uuid::parse_str(id.strip_prefix("sess:").unwrap_or(id)).ok())
            != Some(session.as_uuid())
        || row
            .at_pointer("/isSidechain")
            .and_then(|value| value.as_bool())
            == Some(true)
        || row.at_pointer("/forkedFrom").is_some()
        || row.string_at("/attachment/contextRendering") != Some("inline")
        || !matches!(
            row.string_at("/attachment/type"),
            Some(CHECKPOINT_KIND | NOTIFICATION_KIND)
        )
    {
        return None;
    }
    let id = MessageId::from_uuid(uuid::Uuid::parse_str(row.string_at("/uuid")?).ok()?);
    let initial = row
        .string_at("/attachment/sessionDate")
        .filter(|date| valid_date(date))?;
    let accepted = row.string_at("/attachment/acceptedDate");
    if accepted.is_some_and(|date| !valid_date(date)) {
        return None;
    }
    if row.string_at("/attachment/type") == Some(NOTIFICATION_KIND)
        && (accepted.is_none()
            || row
                .string_units("/rendered/0/content")
                .is_none_or(|text| text.is_empty())
            || row.string_at("/renderedRole") != Some("system"))
    {
        return None;
    }
    Some((id, initial, accepted))
}

/// Capture bounded host records from the exact full transcript source. A
/// compacted model chain need not contain the initial calendar checkpoint.
pub(crate) fn metadata_from_source(source: &str, session: SessionId) -> Vec<Source> {
    let mut initial = None;
    let mut notification = None;
    for line in source.lines() {
        let Ok(row) = Source::parse(line) else {
            continue;
        };
        if date_metadata(&row, session).is_none() {
            continue;
        }
        if row.string_at("/attachment/type") == Some(CHECKPOINT_KIND) {
            if initial.is_none() {
                initial = Some(row);
            }
        } else {
            notification = Some(row);
        }
    }
    initial.into_iter().chain(notification).collect()
}

impl ConversationOrchestrator {
    pub(crate) fn restore_inline_date_metadata(&self, session: SessionId, rows: &[Source]) {
        let mut recovered = None::<DateChangeState>;
        for row in rows {
            let Some((id, initial, accepted)) = date_metadata(row, session) else {
                continue;
            };
            let state = recovered.get_or_insert_with(|| DateChangeState {
                session_id: Some(session),
                session_date: initial.to_owned(),
                checkpoint_id: Some(id),
                checkpoint_persisted: true,
                ..Default::default()
            });
            if state.session_date != initial {
                continue;
            }
            if let Some(date) = accepted {
                state.delivered_date = Some(date.to_owned());
            }
        }
        if let Some(recovered) = recovered {
            *self
                .prompt_runtime
                .date_change
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = recovered;
        }
    }

    pub(crate) async fn restore_inline_date_messages(&self, messages: &[JsonlMessage]) {
        let session = self.session.lock().await.session_id;
        let rows = messages
            .iter()
            .filter(|row| {
                matches!(
                    row.attachment_projection
                        .as_ref()
                        .and_then(|payload| payload.string_at("/type")),
                    Some(CHECKPOINT_KIND | NOTIFICATION_KIND)
                )
            })
            .filter_map(|row| Source::parse(&serde_json::to_string(row).ok()?).ok())
            .collect::<Vec<_>>();
        self.restore_inline_date_metadata(session, &rows);
    }

    async fn persist_inline_date_row(
        &self,
        session: SessionId,
        id: MessageId,
        payload: Source,
        rendered: Option<Source>,
    ) -> Result<(), OrchestratorError> {
        self.check_storage_pause().await?;
        let Some(writer) = self.transcript.jsonl_writer.as_ref() else {
            return Ok(());
        };
        let mut extra = Source::object(Vec::new()).expect("empty metadata");
        if let Some(rendered) = rendered {
            extra
                .set_field("rendered", rendered)
                .expect("date rendering metadata");
            extra
                .set_field("renderedRole", "system".into())
                .expect("date rendering metadata");
        }
        let row = JsonlMessage {
            message_type: "attachment".into(),
            uuid: id.as_uuid().to_string(),
            parent_uuid: None,
            session_id: session.as_uuid().to_string(),
            timestamp: jiff::Timestamp::now()
                .strftime("%Y-%m-%dT%H:%M:%S%.3fZ")
                .to_string(),
            cwd: self.current_cwd().to_string_lossy().into_owned(),
            version: env!("CARGO_PKG_VERSION").into(),
            message: Source::parse("null").expect("null is JSON"),
            attachment_projection: Some(payload),
            tool_use_result_projection: None,
            mcp_meta_projection: None,
            turn_companion: None,
            usage_limit_note: None,
            origin_projection: None,
            turn_origin_projection: None,
            is_sidechain: false,
            user_type: Some("external".into()),
            git_branch: self.resolve_git_branch().await,
            entrypoint: Some(super::entrypoint_value()),
            slug: None,
            prompt_id: None,
            logical_parent_uuid: None,
            extra,
        };
        self.append_pending_transcript_row(writer, row)
            .await
            .map_err(|error| {
                OrchestratorError::Internal(format!(
                    "could not persist Inline calendar context: {error}"
                ))
            })?;
        Ok(())
    }

    /// The turn gate owns this commit. A cancelled caller finishes durable
    /// admission before dropping its future; retries keep the same row IDs.
    pub(crate) async fn prepare_inline_date_context(
        &self,
        cancel: Option<&CancellationToken>,
    ) -> Result<Option<ConversationMessage>, OrchestratorError> {
        if self.is_persistent_app_chat() || self.uses_announced_context().await {
            return Ok(None);
        }
        let session = self.session.lock().await.session_id;
        let initial = self.session_start_date(session);
        let _commit = crate::request_context::preparation_commit_scope();
        let checkpoint = {
            let mut state = self
                .prompt_runtime
                .date_change
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.checkpoint_persisted {
                None
            } else {
                let id = *state.checkpoint_id.get_or_insert_with(MessageId::new);
                Some((
                    id,
                    calendar_payload(CHECKPOINT_KIND, &initial, state.delivered_date.as_deref()),
                ))
            }
        };
        if let Some((id, payload)) = checkpoint {
            self.persist_inline_date_row(session, id, payload, None)
                .await?;
            self.prompt_runtime
                .date_change
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .checkpoint_persisted = true;
        }
        if cancel.is_some_and(CancellationToken::is_cancelled) {
            return Ok(None);
        }
        let today = crate::prompt::env_meta::current_date_string();
        let pending = {
            let state = self
                .prompt_runtime
                .date_change
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.session_date == today
                || state.delivered_date.as_deref() == Some(today.as_str())
            {
                return Ok(None);
            }
            state
                .pending
                .as_ref()
                .filter(|pending| pending.date == today)
                .cloned()
        };
        let pending = match pending {
            Some(pending) => pending,
            None => {
                let Some(message) = self.date_change_reminder_message(session) else {
                    return Ok(None);
                };
                let pending = PendingDateChange {
                    date: today.clone(),
                    message,
                    payload: calendar_payload(NOTIFICATION_KIND, &initial, Some(&today)),
                };
                self.prompt_runtime
                    .date_change
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .pending = Some(pending.clone());
                pending
            }
        };
        let rendered = crate::request_context::rendered_system_message(&pending.message)
            .expect("date notification is text");
        self.persist_inline_date_row(
            session,
            pending.message.id(),
            pending.payload.clone(),
            Some(rendered),
        )
        .await?;
        self.register_mod_persisted_attachment(
            &pending.message,
            NOTIFICATION_KIND,
            serde_json::json!({"kind":"engine"}),
        )
        .await;
        let mut live = self.session.lock().await;
        if live.session_id != session {
            return Err(OrchestratorError::Internal(
                "conversation changed during calendar context commit".into(),
            ));
        }
        self.transcript
            .model_reminder_attachments
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(pending.message.id(), pending.payload);
        if !live
            .history
            .iter()
            .any(|row| row.id() == pending.message.id())
        {
            live.history.push(pending.message.clone());
        }
        let mut state = self
            .prompt_runtime
            .date_change
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.delivered_date = Some(pending.date);
        state.pending = None;
        Ok(Some(pending.message))
    }
}
