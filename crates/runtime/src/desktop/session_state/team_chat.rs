//! Chat input facts share the canonical session journal, queue and writer lease.
//! There is no second message cache or independently committed cursor file.
use super::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const MAX_SAFE: u64 = 9_007_199_254_740_991;
fn id(value: &str) -> bool {
    (8..=64).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}
fn rejected(reason: &str) -> CostPersistError {
    CostPersistError::Rejected(reason.into())
}

/// Bot × Chat scope deliberately excludes the current requester.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BotChatScope {
    pub team_id: String,
    pub shared_bot_id: String,
    pub conversation_id: String,
    pub session_epoch: u64,
}
impl BotChatScope {
    pub fn session_id(&self) -> Result<SessionId, CostPersistError> {
        if !id(&self.team_id)
            || !id(&self.shared_bot_id)
            || !id(&self.conversation_id)
            || self.session_epoch == 0
            || self.session_epoch > MAX_SAFE
        {
            return Err(rejected("invalid Bot Chat scope"));
        }
        let bytes = serde_json::to_vec(&(
            "lingxi-bot-chat-1",
            &self.team_id,
            &self.shared_bot_id,
            &self.conversation_id,
            self.session_epoch,
        ))
        .map_err(|e| rejected(&e.to_string()))?;
        let hash = Sha256::digest(bytes);
        let mut uuid: [u8; 16] = hash[..16].try_into().expect("fixed digest");
        uuid[6] = (uuid[6] & 0x0f) | 0x80;
        uuid[8] = (uuid[8] & 0x3f) | 0x80;
        Ok(SessionId::from_uuid(uuid::Uuid::from_bytes(uuid)))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ChatAuthor {
    User {
        uid: String,
        #[serde(rename = "displayName")]
        display_name: String,
    },
    SharedBot {
        #[serde(rename = "sharedBotId")]
        shared_bot_id: String,
        #[serde(rename = "displayName")]
        display_name: String,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ChatSourceMessage {
    pub message_id: String,
    pub seq: u64,
    pub change_seq: u64,
    pub change_id: String,
    pub author: ChatAuthor,
    pub created_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edited_at: Option<u64>,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recalled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivery_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact_id: Option<String>,
}
/// This is the exact model input fact, not a queryable replica of the Chat.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ChatInputBatch {
    pub schema_version: u32,
    pub batch_id: String,
    pub team_id: String,
    pub shared_bot_id: String,
    pub conversation_id: String,
    pub session_epoch: u64,
    pub expected_session_revision: u64,
    pub from_seq_exclusive: u64,
    pub to_seq_inclusive: u64,
    pub last_change_seq: u64,
    pub bootstrap: bool,
    pub current_message_id: String,
    pub messages: Vec<ChatSourceMessage>,
    pub changes: Vec<ChatSourceMessage>,
}
impl ChatInputBatch {
    pub fn scope(&self) -> BotChatScope {
        BotChatScope {
            team_id: self.team_id.clone(),
            shared_bot_id: self.shared_bot_id.clone(),
            conversation_id: self.conversation_id.clone(),
            session_epoch: self.session_epoch,
        }
    }
    fn event_id(&self) -> String {
        format!("team-chat-input:{}", self.batch_id)
    }
    pub fn validate(&self, session_id: SessionId) -> Result<(), CostPersistError> {
        if self.schema_version != 1
            || !id(&self.batch_id)
            || self.scope().session_id()? != session_id
            || self.expected_session_revision >= MAX_SAFE
            || self.to_seq_inclusive > MAX_SAFE
            || self.last_change_seq > MAX_SAFE
            || self.to_seq_inclusive <= self.from_seq_exclusive
            || self.to_seq_inclusive - self.from_seq_exclusive != self.messages.len() as u64
            || (self.bootstrap
                && self.from_seq_exclusive != self.to_seq_inclusive.saturating_sub(200))
            || self.messages.last().map(|m| &m.message_id) != Some(&self.current_message_id)
        {
            return Err(rejected("invalid or incomplete Chat input batch"));
        }
        let mut ids = std::collections::HashSet::new();
        for (index, message) in self.messages.iter().chain(&self.changes).enumerate() {
            let author_valid = match &message.author {
                ChatAuthor::User { uid, display_name } => {
                    !uid.is_empty()
                        && uid.len() <= 128
                        && uid
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                        && !display_name.is_empty()
                        && display_name.encode_utf16().count() <= 80
                }
                ChatAuthor::SharedBot {
                    shared_bot_id,
                    display_name,
                } => {
                    id(shared_bot_id)
                        && !display_name.is_empty()
                        && display_name.encode_utf16().count() <= 80
                }
            };
            if !author_valid
                || !id(&message.message_id)
                || !id(&message.change_id)
                || (index < self.messages.len()
                    && message.seq != self.from_seq_exclusive + index as u64 + 1)
                || (index >= self.messages.len()
                    && (message.seq == 0 || message.seq > self.from_seq_exclusive))
                || message.change_seq == 0
                || message.change_seq > self.last_change_seq
                || message.created_at > MAX_SAFE
                || message.edited_at.is_some_and(|at| at > MAX_SAFE)
                || message.reply_to.as_ref().is_some_and(|reply| !id(reply))
                || [
                    &message.source_task_id,
                    &message.delivery_id,
                    &message.artifact_id,
                ]
                .iter()
                .any(|value| {
                    value.as_ref().is_some_and(|id| {
                        id.is_empty() || id.len() > 128 || id.chars().any(char::is_control)
                    })
                })
                || !ids.insert(&message.message_id)
                || (message.recalled == Some(true) && !message.text.is_empty())
            {
                return Err(rejected("invalid Chat source metadata or sequence"));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BotChatCursor {
    pub scope: Option<BotChatScope>,
    pub session_revision: u64,
    pub last_message_seq: u64,
    pub last_change_seq: u64,
    pub bootstrap_from_seq: u64,
}
impl BotChatCursor {
    fn apply(
        &mut self,
        batch: &ChatInputBatch,
        session_id: SessionId,
    ) -> Result<(), CostPersistError> {
        batch.validate(session_id)?;
        if batch
            .changes
            .iter()
            .any(|m| m.change_seq <= self.last_change_seq)
            || batch.expected_session_revision != self.session_revision
            || batch.last_change_seq < self.last_change_seq
            || (self.session_revision == 0 && !batch.bootstrap)
            || (self.session_revision != 0
                && (batch.bootstrap
                    || batch.from_seq_exclusive != self.last_message_seq
                    || self.scope.as_ref() != Some(&batch.scope())))
        {
            return Err(rejected("Chat input cursor or revision conflict"));
        }
        if self.session_revision == 0 {
            self.bootstrap_from_seq = batch.from_seq_exclusive;
        }
        self.scope = Some(batch.scope());
        self.session_revision += 1;
        self.last_message_seq = batch.to_seq_inclusive;
        self.last_change_seq = batch.last_change_seq;
        Ok(())
    }
}

/// Acknowledges input durability only. It never grants a model execution permit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatInputAck {
    pub journal_revision: u64,
    pub session_revision: u64,
    pub last_message_seq: u64,
    pub last_change_seq: u64,
    pub duplicate: bool,
}
impl SessionStateCoordinator {
    pub fn bot_chat_cursor(&self) -> BotChatCursor {
        self.state
            .projection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .team_chat
            .clone()
    }
    /// Canonical input facts for repairing a missing transcript projection or
    /// invalidating a summary after source edits. Never a second Chat database.
    pub fn chat_input_batches(&self) -> Result<Vec<ChatInputBatch>, CostPersistError> {
        let replay = self.state.journal.replay().map_err(map_journal_error)?;
        replay
            .entries
            .into_iter()
            .filter_map(|entry| match decode_session_event(entry.event) {
                Ok(SessionEvent::ChatInput(batch)) => Some(Ok(batch)),
                Ok(_) => None,
                Err(error) => Some(Err(error)),
            })
            .collect()
    }
    pub async fn append_chat_input(
        &self,
        batch: ChatInputBatch,
    ) -> Result<ChatInputAck, CostPersistError> {
        batch.validate(self.state.session_id)?;
        let (permit, pin) = self.acquire_session_mutation_permit().await?;
        let (ack, receive) = tokio::sync::oneshot::channel();
        permit.send(QueuedMutation {
            mutation: SessionMutation::ChatInput { batch, ack },
            _pin: Some(pin),
        });
        receive
            .await
            .map_err(|_| CostPersistError::Storage("Chat input acknowledgement dropped".into()))?
    }
}
impl CoordinatorState {
    pub(super) fn persist_chat_input(
        &self,
        batch: ChatInputBatch,
    ) -> Result<ChatInputAck, CostPersistError> {
        let _serial = self
            .mutation_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        batch.validate(self.session_id)?;
        let event_id = batch.event_id();
        let value = encode_session_event(&SessionEvent::ChatInput(batch.clone()))?;
        if let Some(existing) = self
            .journal
            .find_event_durable(&event_id)
            .map_err(map_journal_error)?
        {
            if existing.event != value {
                return Err(rejected("Chat batch identity conflict"));
            }
            return Ok(ChatInputAck {
                journal_revision: existing.journal_revision,
                session_revision: batch.expected_session_revision + 1,
                last_message_seq: batch.to_seq_inclusive,
                last_change_seq: batch.last_change_seq,
                duplicate: true,
            });
        }
        if let Some(reason) = self.durability_gate.frozen_reason() {
            return Err(CostPersistError::Frozen(reason));
        }
        let mut next = self
            .projection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .team_chat
            .clone();
        next.apply(&batch, self.session_id)?;
        let append = self
            .journal
            .append_once(&event_id, &value)
            .map_err(map_journal_error)?;
        self.projection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .team_chat = next;
        self.note_durable_append(append.journal_revision);
        Ok(ChatInputAck {
            journal_revision: append.journal_revision,
            session_revision: batch.expected_session_revision + 1,
            last_message_seq: batch.to_seq_inclusive,
            last_change_seq: batch.last_change_seq,
            duplicate: append.duplicate,
        })
    }
}

pub(super) fn fold_chat_input(
    cursor: &mut BotChatCursor,
    batch: ChatInputBatch,
    session_id: SessionId,
    event_id: &str,
) -> Result<(), CostPersistError> {
    if event_id != batch.event_id() {
        return Err(rejected("Chat batch journal identity conflict"));
    }
    cursor.apply(&batch, session_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::live_sessions::SessionWriterLease;
    struct Lease(String);
    impl SessionWriterLease for Lease {
        fn session_id(&self) -> &str {
            &self.0
        }
    }
    fn batch(from: u64, to: u64, revision: u64) -> ChatInputBatch {
        ChatInputBatch {
            schema_version: 1,
            batch_id: format!("batch_{from:04}_{to:04}"),
            team_id: "team_0001".into(),
            shared_bot_id: "sharedbot_01".into(),
            conversation_id: "chat_0001".into(),
            session_epoch: 1,
            expected_session_revision: revision,
            from_seq_exclusive: from,
            to_seq_inclusive: to,
            last_change_seq: to,
            bootstrap: revision == 0,
            current_message_id: format!("message_{to:04}"),
            changes: Vec::new(),
            messages: ((from + 1)..=to)
                .map(|seq| ChatSourceMessage {
                    message_id: format!("message_{seq:04}"),
                    seq,
                    change_seq: seq,
                    change_id: format!("change_{seq:04}"),
                    author: ChatAuthor::User {
                        uid: if seq % 2 == 0 { "alice" } else { "bob" }.into(),
                        display_name: "Same name".into(),
                    },
                    created_at: 1000 + seq,
                    edited_at: None,
                    text: "x".repeat(1024),
                    reply_to: None,
                    recalled: None,
                    source_task_id: None,
                    delivery_id: None,
                    artifact_id: None,
                })
                .collect(),
        }
    }
    fn open(path: &std::path::Path, input: &ChatInputBatch) -> Arc<SessionStateCoordinator> {
        let id = input.scope().session_id().unwrap();
        SessionStateCoordinator::open(path, id, Arc::new(Lease(id.to_string()))).unwrap()
    }
    #[tokio::test]
    async fn chat_batch_and_dual_cursor_are_one_durable_fact_across_restart() {
        let dir = tempfile::tempdir().unwrap();
        let first = batch(50, 250, 0);
        let state = open(dir.path(), &first);
        let worker = state.start().await.unwrap();
        let ack = state.append_chat_input(first.clone()).await.unwrap();
        assert!(!ack.duplicate);
        assert_eq!(state.bot_chat_cursor().last_message_seq, 250);
        assert_eq!(state.bot_chat_cursor().bootstrap_from_seq, 50);
        let duplicate = state.append_chat_input(first.clone()).await.unwrap();
        assert!(duplicate.duplicate);
        assert_eq!(duplicate.journal_revision, ack.journal_revision);
        let delta = batch(250, 700, 1);
        assert!(serde_json::to_vec(&delta).unwrap().len() > 450 * 1024);
        state.append_chat_input(delta).await.unwrap();
        state.close_and_drain().await.unwrap();
        worker.await.unwrap();
        drop(state);
        let state = open(dir.path(), &first);
        let worker = state.start().await.unwrap();
        let cursor = state.bot_chat_cursor();
        assert_eq!(cursor.session_revision, 2);
        assert_eq!(cursor.last_message_seq, 700);
        assert_eq!(cursor.last_change_seq, 700);
        let duplicate = state.append_chat_input(first).await.unwrap();
        assert!(duplicate.duplicate);
        assert_eq!(duplicate.session_revision, 1);
        assert_eq!(state.bot_chat_cursor(), cursor);
        state.close_and_drain().await.unwrap();
        worker.await.unwrap();
    }
    #[tokio::test]
    async fn concurrent_batch_cas_does_not_advance_twice_or_freeze_valid_session() {
        let dir = tempfile::tempdir().unwrap();
        let first = batch(0, 3, 0);
        let state = open(dir.path(), &first);
        let worker = state.start().await.unwrap();
        let mut conflict = first.clone();
        conflict.batch_id = "batch_conflict".into();
        let (a, b) = tokio::join!(
            state.append_chat_input(first.clone()),
            state.append_chat_input(conflict)
        );
        assert_ne!(a.is_ok(), b.is_ok());
        assert_eq!(state.bot_chat_cursor().session_revision, 1);
        state.append_chat_input(batch(3, 5, 1)).await.unwrap();
        let mut changed = first.clone();
        changed.messages[0].text = "changed".into();
        assert!(state.append_chat_input(changed).await.is_err());
        assert_eq!(state.bot_chat_cursor().last_message_seq, 5);
        state.close_and_drain().await.unwrap();
        worker.await.unwrap();
    }
    #[test]
    fn strict_metadata_and_scope_reject_missing_times_old_keys_and_incomplete_input() {
        let value = batch(0, 3, 0);
        let id = value.scope().session_id().unwrap();
        value.validate(id).unwrap();
        let mut gap = value.clone();
        gap.messages.remove(0);
        assert!(gap.validate(id).is_err());
        let mut wrong_chat = value.clone();
        wrong_chat.conversation_id = "other_chat".into();
        assert!(wrong_chat.validate(id).is_err());
        let mut old_key = value.clone();
        old_key.shared_bot_id = "bot:owner:host:local".into();
        assert!(old_key.validate(id).is_err());
        let mut serialized = serde_json::to_value(value).unwrap();
        serialized["messages"][0]
            .as_object_mut()
            .unwrap()
            .remove("createdAt");
        assert!(serde_json::from_value::<ChatInputBatch>(serialized).is_err());
    }
    #[test]
    fn chat_scope_is_stable_across_requesters_but_changes_with_chat_bot_or_epoch() {
        let value = batch(0, 3, 0);
        let scope = value.scope();
        let id = scope.session_id().unwrap();
        assert_eq!(id, value.scope().session_id().unwrap());
        let mut other = scope.clone();
        other.session_epoch += 1;
        assert_ne!(id, other.session_id().unwrap());
        let mut other = scope.clone();
        other.conversation_id = "chat_0002".into();
        assert_ne!(id, other.session_id().unwrap());
        let mut other = scope;
        other.shared_bot_id = "sharedbot_02".into();
        assert_ne!(id, other.session_id().unwrap());
    }
    #[test]
    fn bootstrap_cannot_omit_messages_from_its_latest_two_hundred_window() {
        let short = batch(50, 52, 0);
        assert!(short.validate(short.scope().session_id().unwrap()).is_err());
        let full = batch(50, 250, 0);
        full.validate(full.scope().session_id().unwrap()).unwrap();
    }
    #[test]
    fn cross_language_batch_vectors_are_strict() {
        let vectors: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/desktop/fixtures/shared-bot-execution-vectors.json"
        ))
        .unwrap();
        for vector in vectors["batches"].as_array().unwrap() {
            let decoded = serde_json::from_value::<ChatInputBatch>(vector["input"].clone());
            let valid = decoded
                .and_then(|batch| {
                    let id = batch
                        .scope()
                        .session_id()
                        .map_err(serde::de::Error::custom)?;
                    batch.validate(id).map_err(serde::de::Error::custom)
                })
                .is_ok();
            assert_eq!(
                valid,
                vector["valid"].as_bool().unwrap(),
                "{}",
                vector["name"]
            );
        }
    }
    #[tokio::test]
    async fn source_batch_above_default_journal_record_limit_replays_without_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let first = batch(0, 1, 0);
        let state = open(dir.path(), &first);
        let worker = state.start().await.unwrap();
        state.append_chat_input(first.clone()).await.unwrap();
        let mut delta = batch(1, 1001, 1);
        for message in &mut delta.messages {
            message.text = "record-boundary ".repeat(400);
        }
        assert!(serde_json::to_vec(&delta).unwrap().len() > 4 * 1024 * 1024);
        state.append_chat_input(delta.clone()).await.unwrap();
        state.close_and_drain().await.unwrap();
        worker.await.unwrap();
        drop(state);
        let state = open(dir.path(), &first);
        state.hydrate_blocking().unwrap();
        assert_eq!(state.bot_chat_cursor().last_message_seq, 1001);
        assert_eq!(state.chat_input_batches().unwrap()[1], delta);
    }
}
