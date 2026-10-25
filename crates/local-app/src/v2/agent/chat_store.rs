//! Durable App-owned Chat metadata. Transcript text stays in the Harness
//! session JSONL; this index holds only routing and attachment metadata.
use super::{
    invalid,
    protocol::{self, ProtocolMessageDigest},
    AttachmentMetadata, ChatRole, ChatState, ChatSummary, State,
};
use crate::v2::AppError;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::Digest;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
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
    /// Canonical AG-UI messages. Data-source bytes are stored as private
    /// content-addressed assets and represented here only by hash and size.
    #[serde(default)]
    protocol_messages: Vec<StoredProtocolMessage>,
    #[serde(default)]
    protocol_asset_bytes: u64,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredProtocolMessage {
    id: String,
    role: String,
    run_id: String,
    sha256: String,
    message: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProtocolHistoryPage {
    pub messages: Vec<Value>,
    pub next_cursor: Option<String>,
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
        let index: ChatIndex = match fs::read(&index_path) {
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
                    protocol_messages: Vec::new(),
                    protocol_asset_bytes: 0,
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

    /// Mark a Chat interrupted when its protocol event stream cannot be
    /// persisted. The run remains available in the task store for diagnosis.
    pub fn interrupt_turn(&self, chat_id: &str, run_id: &str) -> Result<(), AppError> {
        self.change(|index| {
            let record = index
                .chats
                .get_mut(chat_id)
                .ok_or_else(|| AppError::NotFound("App Chat".into()))?;
            if record.active_run_id.as_deref() == Some(run_id) {
                record.summary.state = ChatState::Interrupted;
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

    /// Add one official Message to the host-owned protocol history. Inline
    /// DataSource bytes are stored in per-thread content-addressed files; no
    /// page path or provider file handle is persisted.
    pub fn record_protocol_message(
        &self,
        chat_id: &str,
        run_id: &str,
        message: &Value,
    ) -> Result<(), AppError> {
        if !safe_id(chat_id) || run_id.is_empty() || run_id.len() > 128 {
            return Err(invalid("invalid Chat ID"));
        }
        let (stored, assets) = prepare_protocol_message(run_id, message)?;
        let mut unique_assets = BTreeMap::<String, Vec<u8>>::new();
        for (hash, bytes) in assets {
            unique_assets.entry(hash).or_insert(bytes);
        }

        let (existing_asset_hashes, current_asset_bytes, existing_message) = {
            let index = self
                .index
                .lock()
                .map_err(|_| invalid("Chat storage unavailable"))?;
            let record = index
                .chats
                .get(chat_id)
                .ok_or_else(|| AppError::NotFound("App Chat".into()))?;
            let hashes = record
                .protocol_messages
                .iter()
                .flat_map(|entry| stored_asset_hashes(&entry.message))
                .collect::<BTreeSet<_>>();
            let existing = record
                .protocol_messages
                .iter()
                .find(|entry| entry.id == stored.id)
                .cloned();
            (hashes, record.protocol_asset_bytes, existing)
        };
        if let Some(existing) = existing_message {
            if existing.sha256 == stored.sha256 && existing.run_id == stored.run_id {
                return Ok(());
            }
            return Err(invalid("AG-UI message ID conflicts with stored history"));
        }
        if self
            .index
            .lock()
            .map_err(|_| invalid("Chat storage unavailable"))?
            .chats
            .get(chat_id)
            .is_some_and(|record| record.protocol_messages.len() >= 10_000)
        {
            return Err(invalid("AG-UI chat history is full"));
        }

        let mut new_bytes = 0u64;
        for (hash, bytes) in &unique_assets {
            if !existing_asset_hashes.contains(hash) {
                new_bytes = new_bytes.saturating_add(bytes.len() as u64);
            }
        }
        if current_asset_bytes.saturating_add(new_bytes) > 256 * 1024 * 1024 {
            return Err(invalid("AG-UI chat attachments exceed the storage limit"));
        }
        for (hash, bytes) in &unique_assets {
            self.write_protocol_asset(chat_id, hash, bytes)?;
        }
        self.change(|index| {
            let record = index
                .chats
                .get_mut(chat_id)
                .ok_or_else(|| AppError::NotFound("App Chat".into()))?;
            if let Some(existing) = record
                .protocol_messages
                .iter()
                .find(|entry| entry.id == stored.id)
            {
                if existing.sha256 == stored.sha256 && existing.run_id == stored.run_id {
                    return Ok(());
                }
                return Err(invalid("AG-UI message ID conflicts with stored history"));
            }
            if record.protocol_messages.len() >= 10_000
                || record.protocol_asset_bytes.saturating_add(new_bytes) > 256 * 1024 * 1024
            {
                return Err(invalid("AG-UI chat history exceeds the storage limit"));
            }
            record.protocol_asset_bytes = record.protocol_asset_bytes.saturating_add(new_bytes);
            record.protocol_messages.push(stored);
            record.summary.updated_at = now();
            Ok(())
        })
    }

    pub fn rollback_protocol_message(
        &self,
        chat_id: &str,
        run_id: &str,
        message_id: &str,
    ) -> Result<(), AppError> {
        if !safe_id(chat_id) || run_id.is_empty() || message_id.is_empty() {
            return Err(invalid("invalid AG-UI message selector"));
        }
        let (removed, remaining_hashes) = {
            let mut index = self
                .index
                .lock()
                .map_err(|_| invalid("Chat storage unavailable"))?;
            let mut next = index.clone();
            let record = next
                .chats
                .get_mut(chat_id)
                .ok_or_else(|| AppError::NotFound("App Chat".into()))?;
            let Some(position) = record
                .protocol_messages
                .iter()
                .position(|message| message.id == message_id && message.run_id == run_id)
            else {
                return Ok(());
            };
            let removed = record.protocol_messages.remove(position);
            let mut referenced = BTreeMap::<String, u64>::new();
            for message in &record.protocol_messages {
                for (hash, size) in stored_asset_lengths(&message.message) {
                    referenced.entry(hash).or_insert(size);
                }
            }
            record.protocol_asset_bytes = referenced.values().copied().sum();
            self.persist(&next)?;
            *index = next;
            (removed, referenced.keys().cloned().collect::<BTreeSet<_>>())
        };
        for (hash, _) in stored_asset_lengths(&removed.message) {
            if !remaining_hashes.contains(&hash) {
                let path = self
                    .root
                    .join("chats")
                    .join(chat_id)
                    .join("assets")
                    .join(format!("{hash}.bin"));
                if std::fs::symlink_metadata(&path)
                    .is_ok_and(|metadata| !metadata.file_type().is_symlink() && metadata.is_file())
                {
                    let _ = fs::remove_file(path);
                }
            }
        }
        Ok(())
    }

    pub fn rollback_message_metadata(
        &self,
        chat_id: &str,
        message_id: &str,
        run_id: &str,
    ) -> Result<(), AppError> {
        if !safe_id(chat_id) || message_id.is_empty() || run_id.is_empty() {
            return Err(invalid("invalid Chat message selector"));
        }
        self.change(|index| {
            let record = index
                .chats
                .get_mut(chat_id)
                .ok_or_else(|| AppError::NotFound("App Chat".into()))?;
            if record
                .messages
                .get(message_id)
                .is_some_and(|message| message.run_id == run_id)
            {
                record.messages.remove(message_id);
            }
            Ok(())
        })
    }

    pub fn protocol_history_digests(
        &self,
        chat_id: &str,
    ) -> Result<Vec<ProtocolMessageDigest>, AppError> {
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
        Ok(record
            .protocol_messages
            .iter()
            .map(|message| ProtocolMessageDigest {
                id: message.id.clone(),
                role: message.role.clone(),
                sha256: message.sha256.clone(),
            })
            .collect())
    }

    pub fn protocol_message(&self, chat_id: &str, message_id: &str) -> Result<Value, AppError> {
        if !safe_id(chat_id) || message_id.is_empty() || message_id.len() > 128 {
            return Err(invalid("invalid AG-UI message selector"));
        }
        let stored = {
            let index = self
                .index
                .lock()
                .map_err(|_| invalid("Chat storage unavailable"))?;
            index
                .chats
                .get(chat_id)
                .ok_or_else(|| AppError::NotFound("App Chat".into()))?
                .protocol_messages
                .iter()
                .find(|message| message.id == message_id)
                .map(|message| message.message.clone())
                .ok_or_else(|| AppError::NotFound("AG-UI chat message".into()))?
        };
        self.rehydrate_protocol_message(chat_id, &stored)
    }

    pub fn has_legacy_history_without_protocol(&self, chat_id: &str) -> Result<bool, AppError> {
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
        Ok(!record.messages.is_empty() && record.protocol_messages.is_empty())
    }

    /// Rehydrate a bounded official Message page. The returned JSON contains
    /// standard DataSource base64, never asset paths or opaque host IDs.
    pub fn protocol_history_page(
        &self,
        chat_id: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<ProtocolHistoryPage, AppError> {
        if !safe_id(chat_id) || !(1..=100).contains(&limit) {
            return Err(invalid("invalid AG-UI chat history request"));
        }
        let (entries, has_more_entries) = {
            let index = self
                .index
                .lock()
                .map_err(|_| invalid("Chat storage unavailable"))?;
            let record = index
                .chats
                .get(chat_id)
                .ok_or_else(|| AppError::NotFound("App Chat".into()))?;
            let start = if let Some(cursor) = cursor {
                record
                    .protocol_messages
                    .iter()
                    .position(|message| message.id == cursor)
                    .map(|position| position + 1)
                    .ok_or_else(|| invalid("invalid AG-UI history cursor"))?
            } else {
                0
            };
            let remaining = &record.protocol_messages[start..];
            (
                remaining.iter().take(limit).cloned().collect::<Vec<_>>(),
                remaining.len() > limit,
            )
        };
        let mut messages = Vec::with_capacity(entries.len());
        let mut total_bytes = 2usize;
        for entry in &entries {
            let message = self.rehydrate_protocol_message(chat_id, &entry.message)?;
            let size = serde_json::to_vec(&message)?.len();
            if size > 72 * 1024 * 1024 {
                return Err(invalid("AG-UI history message exceeds the page size limit"));
            }
            if total_bytes.saturating_add(size) > 72 * 1024 * 1024 {
                break;
            }
            total_bytes = total_bytes.saturating_add(size);
            messages.push(message);
        }
        if messages.is_empty() && !entries.is_empty() {
            return Err(invalid("AG-UI history message exceeds the page size limit"));
        }
        let returned = messages.len();
        let has_more = has_more_entries || entries.len() > returned;
        let next_cursor = has_more
            .then(|| {
                messages
                    .last()
                    .and_then(|message| message.get("id").and_then(Value::as_str))
            })
            .flatten()
            .map(str::to_owned);
        Ok(ProtocolHistoryPage {
            messages,
            next_cursor,
        })
    }

    fn write_protocol_asset(
        &self,
        chat_id: &str,
        hash: &str,
        bytes: &[u8],
    ) -> Result<(), AppError> {
        if !valid_asset_hash(hash)
            || format!("{:x}", sha2::Sha256::digest(bytes)) != hash
            || bytes.len() > 25 * 1024 * 1024
        {
            return Err(invalid("AG-UI chat attachment is invalid"));
        }
        let directory = self.root.join("chats").join(chat_id).join("assets");
        ensure_private_directory(&directory)?;
        let path = directory.join(format!("{hash}.bin"));
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_file() {
                    return Err(invalid("Chat attachment storage is unavailable"));
                }
                let existing = fs::read(&path)?;
                if existing.len() != bytes.len()
                    || format!("{:x}", sha2::Sha256::digest(&existing)) != hash
                {
                    return Err(invalid("Chat attachment storage failed integrity checks"));
                }
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let temp = directory.join(format!(".asset-{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| -> Result<(), AppError> {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&temp)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            match fs::rename(&temp, &path) {
                Ok(()) => {}
                Err(error) if path.exists() => {
                    let existing = fs::read(&path)?;
                    if existing.len() != bytes.len()
                        || format!("{:x}", sha2::Sha256::digest(&existing)) != hash
                    {
                        return Err(invalid("Chat attachment storage failed integrity checks"));
                    }
                    let _ = fs::remove_file(&temp);
                    let _ = error;
                }
                Err(error) => return Err(error.into()),
            }
            File::open(&directory)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    }

    fn rehydrate_protocol_message(&self, chat_id: &str, stored: &Value) -> Result<Value, AppError> {
        let mut message = stored.clone();
        let Some(parts) = message.get_mut("content").and_then(Value::as_array_mut) else {
            return Ok(message);
        };
        for part in parts {
            let Some(source) = part.get_mut("source") else {
                continue;
            };
            if source.get("type").and_then(Value::as_str) != Some("data") {
                continue;
            }
            let Some(marker) = source.get("value").and_then(Value::as_object) else {
                continue;
            };
            let hash = marker
                .get("sha256")
                .and_then(Value::as_str)
                .filter(|hash| valid_asset_hash(hash))
                .ok_or_else(|| invalid("Chat attachment storage is invalid"))?;
            let byte_length = marker
                .get("byteLength")
                .and_then(Value::as_u64)
                .and_then(|size| usize::try_from(size).ok())
                .filter(|size| *size <= 25 * 1024 * 1024)
                .ok_or_else(|| invalid("Chat attachment storage is invalid"))?;
            let path = self
                .root
                .join("chats")
                .join(chat_id)
                .join("assets")
                .join(format!("{hash}.bin"));
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(invalid("Chat attachment storage is unavailable"));
            }
            let mut file = File::open(&path)?;
            let mut bytes = Vec::with_capacity(byte_length);
            file.read_to_end(&mut bytes)?;
            if bytes.len() != byte_length || format!("{:x}", sha2::Sha256::digest(&bytes)) != hash {
                return Err(invalid("Chat attachment storage failed integrity checks"));
            }
            source["value"] = Value::String(STANDARD.encode(bytes));
        }
        protocol::validate_message(&message)
            .map_err(|_| invalid("Stored AG-UI message failed schema validation"))?;
        Ok(message)
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

fn prepare_protocol_message(
    run_id: &str,
    message: &Value,
) -> Result<(StoredProtocolMessage, Vec<(String, Vec<u8>)>), AppError> {
    let digest = protocol::message_digest(message)
        .map_err(|_| invalid("AG-UI message does not match the pinned protocol"))?;
    let mut stored = message.clone();
    let mut assets = Vec::new();
    if let Some(parts) = stored.get_mut("content").and_then(Value::as_array_mut) {
        for part in parts {
            if !matches!(
                part.get("type").and_then(Value::as_str),
                Some("image" | "document")
            ) {
                continue;
            }
            let Some(source) = part.get_mut("source") else {
                continue;
            };
            if source.get("type").and_then(Value::as_str) != Some("data") {
                continue;
            }
            let encoded = source
                .get("value")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("AG-UI attachment data is invalid"))?;
            let bytes = STANDARD
                .decode(encoded)
                .map_err(|_| invalid("AG-UI attachment data is invalid"))?;
            if bytes.is_empty()
                || bytes.len() > 25 * 1024 * 1024
                || STANDARD.encode(&bytes) != encoded
            {
                return Err(invalid("AG-UI attachment data is invalid"));
            }
            let hash = format!("{:x}", sha2::Sha256::digest(&bytes));
            source["value"] = json!({"sha256":hash,"byteLength":bytes.len()});
            assets.push((hash, bytes));
        }
    }
    Ok((
        StoredProtocolMessage {
            id: digest.id,
            role: digest.role,
            run_id: run_id.to_owned(),
            sha256: digest.sha256,
            message: stored,
        },
        assets,
    ))
}

fn stored_asset_hashes(message: &Value) -> Vec<String> {
    let Some(parts) = message.get("content").and_then(Value::as_array) else {
        return Vec::new();
    };
    parts
        .iter()
        .filter(|part| {
            matches!(
                part.get("type").and_then(Value::as_str),
                Some("image" | "document")
            )
        })
        .filter_map(|part| part.pointer("/source/value/sha256").and_then(Value::as_str))
        .filter(|hash| valid_asset_hash(hash))
        .map(str::to_owned)
        .collect()
}

fn stored_asset_lengths(message: &Value) -> Vec<(String, u64)> {
    let Some(parts) = message.get("content").and_then(Value::as_array) else {
        return Vec::new();
    };
    parts
        .iter()
        .filter(|part| {
            matches!(
                part.get("type").and_then(Value::as_str),
                Some("image" | "document")
            )
        })
        .filter_map(|part| {
            let hash = part.pointer("/source/value/sha256")?.as_str()?;
            let size = part.pointer("/source/value/byteLength")?.as_u64()?;
            valid_asset_hash(hash).then(|| (hash.to_owned(), size))
        })
        .collect()
}

fn valid_asset_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn ensure_private_directory(path: &Path) -> Result<(), AppError> {
    ensure_real_directory(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(invalid("Chat attachment storage is unavailable"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(invalid("Chat attachment storage permissions are too broad"));
        }
    }
    Ok(())
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
        assert!(store
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
            .is_err());
    }

    #[test]
    fn agui_history_rehydrates_private_data_assets_and_rolls_back_prestart_message() {
        let root = tempfile::tempdir().unwrap();
        let store = ChatStore::open(root.path()).unwrap();
        let chat = store
            .create(None, &uuid::Uuid::new_v4().to_string())
            .unwrap();
        let encoded = STANDARD.encode(b"private document bytes");
        let message = json!({
            "id":"user-message-1",
            "role":"user",
            "content":[
                {"type":"text","text":"Read this"},
                {"type":"document","source":{"type":"data","value":encoded,"mimeType":"text/plain"},
                 "metadata":{"com.lingxi.localApp":{"version":1,"filename":"notes.txt"}}
                }
            ]
        });
        store
            .record_protocol_message(&chat.chat_id, "task-private-uuid", &message)
            .unwrap();
        let disk = std::fs::read_to_string(root.path().join("chats-index.json")).unwrap();
        assert!(!disk.contains(&encoded));
        let page = store
            .protocol_history_page(&chat.chat_id, None, 10)
            .unwrap();
        assert_eq!(page.messages, vec![message.clone()]);
        assert_eq!(
            store.protocol_history_digests(&chat.chat_id).unwrap()[0].id,
            "user-message-1"
        );

        let recovered = ChatStore::open(root.path()).unwrap();
        assert_eq!(
            recovered
                .protocol_history_page(&chat.chat_id, None, 10)
                .unwrap()
                .messages,
            vec![message]
        );
        recovered
            .rollback_protocol_message(&chat.chat_id, "task-private-uuid", "user-message-1")
            .unwrap();
        assert!(recovered
            .protocol_history_page(&chat.chat_id, None, 10)
            .unwrap()
            .messages
            .is_empty());
        assert_eq!(
            recovered.protocol_history_digests(&chat.chat_id).unwrap(),
            Vec::new()
        );
    }
}
