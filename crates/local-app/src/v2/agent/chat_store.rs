//! Durable App-owned Chat metadata. Transcript text stays in the Harness
//! session JSONL; this index holds only routing and attachment metadata.
use super::{AttachmentMetadata, ChatRole, ChatState, ChatSummary, State, invalid};
use crate::v2::AppError;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::Mutex,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChatMessageMetadata {
    pub message_id: String,
    pub run_id: String,
    pub role: ChatRole,
    pub created_at: u64,
    pub attachments: Vec<AttachmentMetadata>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ChatRecord {
    summary: ChatSummary,
    /// Harness identity is private host state and is never part of ChatSummary.
    session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    active_run_id: Option<String>,
    #[serde(default)]
    messages: BTreeMap<String, ChatMessageMetadata>,
}

#[derive(Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ChatIndex {
    chats: BTreeMap<String, ChatRecord>,
}

/// One instance per Local App; its root is `<app>/agent`.
pub struct ChatStore {
    root: PathBuf,
    index_path: PathBuf,
    index: Mutex<ChatIndex>,
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn safe_id(value: &str) -> bool {
    uuid::Uuid::parse_str(value).is_ok_and(|uuid| uuid.to_string() == value)
}

fn ensure_real_directory(path: &Path) -> Result<(), AppError> {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(invalid("Chat storage unavailable"));
        }
    } else {
        if let Some(parent) = path.parent() {
            ensure_real_directory(parent)?;
        }
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(path)?;
    }
    Ok(())
}

fn atomic_json(path: &Path, value: &impl Serialize) -> Result<(), AppError> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid("Chat storage unavailable"))?;
    ensure_real_directory(parent)?;
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(invalid("Chat storage unavailable"));
    }
    let temp = parent.join(format!(".chat-index-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<(), AppError> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        file.write_all(&serde_json::to_vec(value)?)?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

impl ChatStore {
    pub fn open(root: &Path) -> Result<Self, AppError> {
        ensure_real_directory(root)?;
        ensure_real_directory(&root.join("chats"))?;
        let index_path = root.join("chats-index.json");
        let mut index: ChatIndex = match fs::read(&index_path) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => ChatIndex::default(),
            Err(error) => return Err(error.into()),
        };
        Ok(Self {
            root: root.to_path_buf(),
            index_path,
            index: Mutex::new(index),
        })
    }

    /// Reconcile interrupted Chat metadata only after the durable task ledger
    /// has completed its own startup recovery. A Task that terminalized before
    /// Chat finish metadata was written closes the Chat as Idle; interrupted,
    /// missing, or previously in-flight tasks leave the Chat Interrupted.
    pub fn reconcile_startup(&self, task_states: &BTreeMap<String, State>) -> Result<(), AppError> {
        self.change(|index| {
            for record in index.chats.values_mut() {
                if record.summary.state != ChatState::Running {
                    continue;
                }
                let task_state = record
                    .active_run_id
                    .as_deref()
                    .and_then(|run_id| task_states.get(run_id));
                record.summary.state = match task_state {
                    Some(State::Succeeded | State::Failed | State::Cancelled) => ChatState::Idle,
                    Some(State::Queued | State::Running | State::Interrupted) | None => {
                        ChatState::Interrupted
                    }
                };
                record.summary.updated_at = now();
                record.active_run_id = None;
            }
            Ok(())
        })
    }

    fn persist(&self, index: &ChatIndex) -> Result<(), AppError> {
        atomic_json(&self.index_path, index)
    }

    fn change<T>(
        &self,
        edit: impl FnOnce(&mut ChatIndex) -> Result<T, AppError>,
    ) -> Result<T, AppError> {
        let mut index = self
            .index
            .lock()
            .map_err(|_| invalid("Chat storage unavailable"))?;
        let mut next = index.clone();
        let result = edit(&mut next)?;
        self.persist(&next)?;
        *index = next;
        Ok(result)
    }

    pub fn create(&self, title: Option<&str>, session_id: &str) -> Result<ChatSummary, AppError> {
        if !safe_id(session_id) {
            return Err(invalid("invalid Harness session ID"));
        }
        let title = title.unwrap_or("New chat").trim();
        if title.is_empty() || title.chars().count() > 80 || title.chars().any(char::is_control) {
            return Err(invalid("invalid Chat title"));
        }
        let chat_id = uuid::Uuid::new_v4().to_string();
        let timestamp = now();
        let summary = ChatSummary {
            chat_id: chat_id.clone(),
            title: title.to_owned(),
            created_at: timestamp,
            updated_at: timestamp,
            archived: false,
            state: ChatState::Idle,
        };
        let directory = self.root.join("chats").join(&chat_id);
        ensure_real_directory(&directory)?;
        let result = self.change(|index| {
            index.chats.insert(
                chat_id.clone(),
                ChatRecord {
                    summary: summary.clone(),
                    session_id: session_id.to_owned(),
                    active_run_id: None,
                    messages: BTreeMap::new(),
                },
            );
            Ok(summary.clone())
        });
        if result.is_err() {
            let _ = fs::remove_dir_all(directory);
        }
        result
    }

    pub fn get(&self, chat_id: &str) -> Result<ChatSummary, AppError> {
        if !safe_id(chat_id) {
            return Err(invalid("invalid Chat ID"));
        }
        self.index
            .lock()
            .map_err(|_| invalid("Chat storage unavailable"))?
            .chats
            .get(chat_id)
            .map(|record| record.summary.clone())
            .ok_or_else(|| AppError::NotFound("App Chat".into()))
    }

    pub fn list(
        &self,
        cursor: Option<&str>,
        limit: usize,
        include_archived: bool,
    ) -> Result<Vec<ChatSummary>, AppError> {
        if !(1..=100).contains(&limit) {
            return Err(invalid("invalid Chat list limit"));
        }
        let index = self
            .index
            .lock()
            .map_err(|_| invalid("Chat storage unavailable"))?;
        let mut chats = index
            .chats
            .values()
            .filter(|record| include_archived || !record.summary.archived)
            .map(|record| record.summary.clone())
            .collect::<Vec<_>>();
        chats.sort_by(|left, right| {
            right
                .updated_at
                .cmp(&left.updated_at)
                .then_with(|| right.chat_id.cmp(&left.chat_id))
        });
        if let Some(cursor) = cursor {
            let position = chats
                .iter()
                .position(|summary| summary.chat_id == cursor)
                .ok_or_else(|| invalid("invalid Chat cursor"))?;
            chats.drain(..=position);
        }
        chats.truncate(limit);
        Ok(chats)
    }

    pub fn session_id(&self, chat_id: &str) -> Result<String, AppError> {
        if !safe_id(chat_id) {
            return Err(invalid("invalid Chat ID"));
        }
        self.index
            .lock()
            .map_err(|_| invalid("Chat storage unavailable"))?
            .chats
            .get(chat_id)
            .map(|record| record.session_id.clone())
            .ok_or_else(|| AppError::NotFound("App Chat".into()))
    }

    pub fn transcript_path(&self, chat_id: &str) -> Result<PathBuf, AppError> {
        let session_id = self.session_id(chat_id)?;
        let directory = self.root.join("chats").join(chat_id);
        ensure_real_directory(&directory)?;
        Ok(directory.join(format!("{session_id}.jsonl")))
    }

    pub fn start_turn(&self, chat_id: &str, run_id: &str) -> Result<(), AppError> {
        if !safe_id(chat_id) || run_id.is_empty() || run_id.len() > 128 {
            return Err(invalid("invalid Chat turn"));
        }
        self.change(|index| {
            let record = index
                .chats
                .get_mut(chat_id)
                .ok_or_else(|| AppError::NotFound("App Chat".into()))?;
            if record.summary.archived {
                return Err(invalid("Chat is archived"));
            }
            if record.summary.state == ChatState::Running {
                return Err(AppError::Busy(chat_id.into()));
            }
            record.summary.state = ChatState::Running;
            record.summary.updated_at = now();
            record.active_run_id = Some(run_id.to_owned());
            Ok(())
        })
    }

    pub fn finish_turn(&self, chat_id: &str, run_id: &str) -> Result<(), AppError> {
        self.change(|index| {
            let record = index
                .chats
                .get_mut(chat_id)
                .ok_or_else(|| AppError::NotFound("App Chat".into()))?;
            if record.active_run_id.as_deref() == Some(run_id) {
                record.summary.state = ChatState::Idle;
                record.summary.updated_at = now();
                record.active_run_id = None;
            }
            Ok(())
        })
    }

    pub fn active_run(&self, chat_id: &str) -> Result<Option<String>, AppError> {
        if !safe_id(chat_id) {
            return Err(invalid("invalid Chat ID"));
        }
        self.index
            .lock()
            .map_err(|_| invalid("Chat storage unavailable"))?
            .chats
            .get(chat_id)
            .map(|record| record.active_run_id.clone())
            .ok_or_else(|| AppError::NotFound("App Chat".into()))
    }

    pub fn record_message(
        &self,
        chat_id: &str,
        message: ChatMessageMetadata,
    ) -> Result<(), AppError> {
        if !safe_id(chat_id)
            || message.message_id.is_empty()
            || message.message_id.len() > 128
            || message.run_id.is_empty()
            || message.run_id.len() > 128
        {
            return Err(invalid("invalid Chat message"));
        }
        self.change(|index| {
            let record = index
                .chats
                .get_mut(chat_id)
                .ok_or_else(|| AppError::NotFound("App Chat".into()))?;
            if let Some(existing) = record.messages.get(&message.message_id) {
                if existing != &message {
                    return Err(invalid("Chat message identity conflict"));
                }
            } else {
                record.messages.insert(message.message_id.clone(), message);
            }
            record.summary.updated_at = now();
            Ok(())
        })
    }

    pub fn message_metadata(&self, chat_id: &str) -> Result<Vec<ChatMessageMetadata>, AppError> {
        if !safe_id(chat_id) {
            return Err(invalid("invalid Chat ID"));
        }
        let index = self
            .index
            .lock()
            .map_err(|_| invalid("Chat storage unavailable"))?;
        let record = index
            .chats
            .get(chat_id)
            .ok_or_else(|| AppError::NotFound("App Chat".into()))?;
        let mut messages = record.messages.values().cloned().collect::<Vec<_>>();
        messages.sort_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| left.message_id.cmp(&right.message_id))
        });
        Ok(messages)
    }

    pub fn set_initial_title(&self, chat_id: &str, prompt: &str) -> Result<(), AppError> {
        let title = prompt
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(80)
            .collect::<String>();
        if title.is_empty() {
            return Ok(());
        }
        self.change(|index| {
            let record = index
                .chats
                .get_mut(chat_id)
                .ok_or_else(|| AppError::NotFound("App Chat".into()))?;
            if record.summary.title == "New chat" {
                record.summary.title = title;
                record.summary.updated_at = now();
            }
            Ok(())
        })
    }

    pub fn archive(&self, chat_id: &str) -> Result<(), AppError> {
        if !safe_id(chat_id) {
            return Err(invalid("invalid Chat ID"));
        }
        self.change(|index| {
            let record = index
                .chats
                .get_mut(chat_id)
                .ok_or_else(|| AppError::NotFound("App Chat".into()))?;
            if record.summary.state == ChatState::Running {
                return Err(AppError::Busy(chat_id.into()));
            }
            record.summary.archived = true;
            record.summary.updated_at = now();
            Ok(())
        })
    }

    pub fn delete(&self, chat_id: &str) -> Result<(), AppError> {
        if !safe_id(chat_id) {
            return Err(invalid("invalid Chat ID"));
        }
        let directory = self.root.join("chats").join(chat_id);
        if fs::symlink_metadata(&directory).is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            return Err(invalid("Chat storage unavailable"));
        }
        let removed = self.change(|index| {
            let record = index
                .chats
                .get(chat_id)
                .ok_or_else(|| AppError::NotFound("App Chat".into()))?;
            if record.summary.state == ChatState::Running {
                return Err(AppError::Busy(chat_id.into()));
            }
            index.chats.remove(chat_id);
            Ok(())
        });
        removed?;
        if directory.exists() {
            fs::remove_dir_all(directory)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_session_identity_is_host_only_and_startup_reconciles_task_state() {
        let root = tempfile::tempdir().unwrap();
        let session_id = uuid::Uuid::new_v4().to_string();
        let store = ChatStore::open(root.path()).unwrap();
        let chat = store.create(None, &session_id).unwrap();
        let public = serde_json::to_value(&chat).unwrap();
        assert!(public.get("sessionId").is_none());
        assert_eq!(store.session_id(&chat.chat_id).unwrap(), session_id);
        store.start_turn(&chat.chat_id, "run-1").unwrap();
        drop(store);

        let recovered = ChatStore::open(root.path()).unwrap();
        assert_eq!(
            recovered.get(&chat.chat_id).unwrap().state,
            ChatState::Running
        );
        let mut task_states = BTreeMap::new();
        task_states.insert("run-1".into(), State::Interrupted);
        recovered.reconcile_startup(&task_states).unwrap();
        assert_eq!(
            recovered.get(&chat.chat_id).unwrap().state,
            ChatState::Interrupted
        );
        assert_eq!(recovered.active_run(&chat.chat_id).unwrap(), None);
        assert_eq!(recovered.session_id(&chat.chat_id).unwrap(), session_id);
    }

    #[test]
    fn startup_closes_chat_when_task_terminalized_before_chat_metadata() {
        let root = tempfile::tempdir().unwrap();
        let store = ChatStore::open(root.path()).unwrap();
        let chat = store
            .create(None, &uuid::Uuid::new_v4().to_string())
            .unwrap();
        store.start_turn(&chat.chat_id, "run-1").unwrap();
        drop(store);

        let recovered = ChatStore::open(root.path()).unwrap();
        let mut task_states = BTreeMap::new();
        task_states.insert("run-1".into(), State::Succeeded);
        recovered.reconcile_startup(&task_states).unwrap();
        assert_eq!(recovered.get(&chat.chat_id).unwrap().state, ChatState::Idle);
        assert_eq!(recovered.active_run(&chat.chat_id).unwrap(), None);
    }

    #[test]
    fn messages_are_deduplicated_and_listed_in_creation_order() {
        let root = tempfile::tempdir().unwrap();
        let store = ChatStore::open(root.path()).unwrap();
        let chat = store
            .create(None, &uuid::Uuid::new_v4().to_string())
            .unwrap();
        for (message_id, created_at, role) in [
            ("assistant-id", 2, ChatRole::Assistant),
            ("user-id", 1, ChatRole::User),
        ] {
            store
                .record_message(
                    &chat.chat_id,
                    ChatMessageMetadata {
                        message_id: message_id.into(),
                        run_id: "run-1".into(),
                        role,
                        created_at,
                        attachments: Vec::new(),
                    },
                )
                .unwrap();
        }
        let messages = store.message_metadata(&chat.chat_id).unwrap();
        assert_eq!(messages[0].message_id, "user-id");
        assert_eq!(messages[1].message_id, "assistant-id");
        assert!(
            store
                .record_message(
                    &chat.chat_id,
                    ChatMessageMetadata {
                        message_id: "assistant-id".into(),
                        run_id: "other-run".into(),
                        role: ChatRole::Assistant,
                        created_at: 2,
                        attachments: Vec::new(),
                    },
                )
                .is_err()
        );
    }
}
