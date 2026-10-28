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
    ops::{Deref, DerefMut},
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ChatIndex {
    chats: BTreeMap<String, ChatRecord>,
    /// Retained even after a user deletes the Chat, so its request ID can
    /// never silently create a second resource.
    #[serde(default)]
    creations: BTreeMap<String, ChatCreation>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ChatCreation {
    fingerprint: Option<String>,
    deleted: bool,
}

struct IndexState {
    index: ChatIndex,
    /// A failed index write may already have renamed the new index. Never
    /// serve or overwrite stale in-memory state after that point.
    uncertain: bool,
}

#[derive(Default)]
struct CleanupDiagnostics {
    failed_ids: BTreeSet<String>,
    parent_sync_failed: bool,
}

impl Deref for IndexState {
    type Target = ChatIndex;
    fn deref(&self) -> &Self::Target {
        &self.index
    }
}

impl DerefMut for IndexState {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.index
    }
}

/// One instance per Local App; its root is `<app>/agent`.
pub struct ChatStore {
    root: PathBuf,
    index_path: PathBuf,
    index: Mutex<IndexState>,
    cleanup_diagnostics: Mutex<CleanupDiagnostics>,
    #[cfg(test)]
    injected_persist_failure: Mutex<Option<PersistStage>>,
    #[cfg(test)]
    injected_cleanup_failure: Mutex<Option<CleanupStage>>,
}

/// Chat summaries exposed by one Local App. `next_cursor` identifies the last
/// included chat in the same archive-filtered, stable sort order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatListPage {
    /// Chat summaries in the current stable-order page.
    pub chats: Vec<ChatSummary>,
    /// Chat ID of the last included summary when another page is available.
    pub next_cursor: Option<String>,
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

fn safe_request_id(value: &str) -> bool {
    uuid::Uuid::parse_str(value).is_ok_and(|uuid| {
        uuid.to_string() == value
            && uuid.get_version_num() == 4
            && uuid.get_variant() == uuid::Variant::RFC4122
    })
}

fn creation_fingerprint(title: Option<&str>) -> Result<String, AppError> {
    // The raw title is part of the identity; defaulting or display trimming
    // must not turn two distinct caller inputs into the same request.
    let bytes = serde_json::to_vec(&("agent.chats.create.v1", title))?;
    Ok(format!("{:x}", sha2::Sha256::digest(bytes)))
}

fn validated_title(title: Option<&str>) -> Result<&str, AppError> {
    let raw = title.unwrap_or("New chat");
    let display = raw.trim();
    if raw.len() > 512
        || raw.chars().any(char::is_control)
        || display.is_empty()
        || display.chars().count() > 80
    {
        return Err(invalid("invalid Chat title"));
    }
    Ok(display)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PersistStage {
    BeforeRename,
    AfterRename,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CleanupStage {
    BeforeRemove,
    BeforeParentSync,
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

fn atomic_json(
    path: &Path,
    value: &impl Serialize,
    mut check: impl FnMut(PersistStage) -> Result<(), AppError>,
) -> Result<(), AppError> {
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
        check(PersistStage::BeforeRename)?;
        fs::rename(&temp, path)?;
        check(PersistStage::AfterRename)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

impl ChatStore {
    /// Validate an existing index without creating, modifying, or cleaning up
    /// any storage. Hosts can use this before opening other per-app stores.
    /// A missing index is allowed only when no Chat files or pending index
    /// writes are present; an empty chats directory alone is harmless.
    pub fn preflight_existing_index(root: &Path) -> Result<(), AppError> {
        Self::read_existing_index(root).map(|_| ())
    }

    fn read_existing_index(root: &Path) -> Result<Option<ChatIndex>, AppError> {
        let root_meta = match fs::symlink_metadata(root) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if !root_meta.is_dir() || root_meta.file_type().is_symlink() {
            return Err(AppError::ChatStorageUncertain);
        }
        // Validate this path even when an index exists: otherwise a host can
        // recover other stores before open notices that chats is unusable.
        let chat_files_present = Self::existing_chat_files_present(root)?;
        let index_path = root.join("chats-index.json");
        let index_meta = match fs::symlink_metadata(&index_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Self::check_no_unindexed_chat_data(root, chat_files_present)?;
                return Ok(None);
            }
            Err(error) => return Err(error.into()),
        };
        if !index_meta.is_file() || index_meta.file_type().is_symlink() {
            return Err(AppError::ChatStorageUncertain);
        }
        // Deserialize directly as well as inspecting the top-level value so
        // duplicate fields and other invalid structures are still rejected.
        let bytes = fs::read(&index_path)?;
        let value: Value =
            serde_json::from_slice(&bytes).map_err(|_| AppError::ChatStorageUncertain)?;
        let object = value.as_object().ok_or(AppError::ChatStorageUncertain)?;
        let index: ChatIndex =
            serde_json::from_slice(&bytes).map_err(|_| AppError::ChatStorageUncertain)?;
        if !object.contains_key("creations") {
            return Err(AppError::ChatLegacyIndexUnsupported);
        }
        for (id, creation) in &index.creations {
            let fingerprint_valid = creation.fingerprint.as_deref().is_some_and(|value| {
                value.len() == 64
                    && value
                        .bytes()
                        .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            });
            if !safe_request_id(id)
                || !fingerprint_valid
                || (creation.deleted && index.chats.contains_key(id))
                || (!creation.deleted
                    && index
                        .chats
                        .get(id)
                        .is_none_or(|chat| chat.summary.chat_id != *id))
            {
                return Err(AppError::ChatStorageUncertain);
            }
        }
        for (id, chat) in &index.chats {
            if !safe_request_id(id)
                || chat.summary.chat_id != *id
                || !safe_id(&chat.session_id)
                || !index
                    .creations
                    .get(id)
                    .is_some_and(|creation| !creation.deleted)
            {
                return Err(AppError::ChatStorageUncertain);
            }
        }
        Ok(Some(index))
    }

    fn existing_chat_files_present(root: &Path) -> Result<bool, AppError> {
        let chats = root.join("chats");
        match fs::symlink_metadata(&chats) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                // Read at most one entry. This checks directory access but
                // neither enumerates nor modifies any Chat contents.
                Ok(fs::read_dir(&chats)?.next().transpose()?.is_some())
            }
            Ok(_) => Err(AppError::ChatStorageUncertain),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    fn check_no_unindexed_chat_data(root: &Path, chat_files_present: bool) -> Result<(), AppError> {
        if chat_files_present {
            return Err(AppError::ChatStorageUncertain);
        }
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(".chat-index-") && name.ends_with(".tmp") {
                return Err(AppError::ChatStorageUncertain);
            }
        }
        Ok(())
    }

    pub fn open(root: &Path) -> Result<Self, AppError> {
        // Check before creating directories or performing tombstone cleanup.
        // This also rechecks a host's earlier read-only preflight.
        let existing_index = Self::read_existing_index(root)?;
        ensure_real_directory(root)?;
        ensure_real_directory(&root.join("chats"))?;
        let index_exists = existing_index.is_some();
        let index = existing_index.unwrap_or_default();
        let index_path = root.join("chats-index.json");
        // A previous rename may have succeeded before its directory sync failed.
        // Re-establish durability before reporting that index as committed.
        if index_exists {
            File::open(root)?.sync_all()?;
        }
        let store = Self {
            root: root.to_path_buf(),
            index_path,
            index: Mutex::new(IndexState {
                index,
                uncertain: false,
            }),
            cleanup_diagnostics: Mutex::new(CleanupDiagnostics::default()),
            #[cfg(test)]
            injected_persist_failure: Mutex::new(None),
            #[cfg(test)]
            injected_cleanup_failure: Mutex::new(None),
        };
        store.recover_deleted_directories();
        Ok(store)
    }

    /// Opaque Chat IDs and a parent-sync flag for cleanup that needs retry.
    /// No titles, paths, transcript content, or operating-system errors are exposed.
    pub fn pending_deletion_cleanup(&self) -> Result<(Vec<String>, bool), AppError> {
        let diagnostics = self
            .cleanup_diagnostics
            .lock()
            .map_err(|_| invalid("Chat cleanup diagnostics unavailable"))?;
        Ok((
            diagnostics.failed_ids.iter().cloned().collect(),
            diagnostics.parent_sync_failed,
        ))
    }

    fn check_cleanup_stage(&self, stage: CleanupStage) -> Result<(), AppError> {
        #[cfg(test)]
        {
            let mut injected = self.injected_cleanup_failure.lock().unwrap();
            if *injected == Some(stage) {
                *injected = None;
                return Err(std::io::Error::other("injected Chat cleanup failure").into());
            }
        }
        let _ = stage;
        Ok(())
    }

    fn cleanup_parent(&self) -> Result<PathBuf, AppError> {
        let path = self.root.join("chats");
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(invalid("Chat cleanup parent is unavailable"));
        }
        Ok(path)
    }

    fn remove_deleted_directory(&self, chat_id: &str) -> Result<(), AppError> {
        if !safe_id(chat_id) {
            return Err(invalid("invalid Chat ID"));
        }
        let directory = self.cleanup_parent()?.join(chat_id);
        match fs::symlink_metadata(&directory) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(invalid("Chat cleanup path is unavailable"));
            }
            Ok(_) => {}
        }
        self.check_cleanup_stage(CleanupStage::BeforeRemove)?;
        match fs::remove_dir_all(&directory) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn sync_deleted_directory_parent(&self) -> Result<(), AppError> {
        self.check_cleanup_stage(CleanupStage::BeforeParentSync)?;
        File::open(self.cleanup_parent()?)?.sync_all()?;
        Ok(())
    }

    fn recover_deleted_directories(&self) {
        // Run only on tombstones already read and validated from the index.
        // A failure is contained to this cleanup; other Apps can still open.
        let ids = match self.lock_index() {
            Ok(index) => index
                .creations
                .iter()
                .filter(|(_, creation)| creation.deleted)
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>(),
            Err(_) => return,
        };
        if ids.is_empty() {
            return;
        }
        let failed_ids = ids
            .into_iter()
            .filter(|id| self.remove_deleted_directory(id).is_err())
            .collect::<BTreeSet<_>>();
        let parent_sync_failed = self.sync_deleted_directory_parent().is_err();
        let count = failed_ids.len() + usize::from(parent_sync_failed);
        if let Ok(mut diagnostics) = self.cleanup_diagnostics.lock() {
            diagnostics.failed_ids = failed_ids;
            diagnostics.parent_sync_failed = parent_sync_failed;
        }
        if count > 0 {
            tracing::warn!(failure_count = count, "Deleted Chat cleanup needs retry");
        }
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
        atomic_json(&self.index_path, index, |stage| {
            #[cfg(test)]
            {
                let mut injected = self.injected_persist_failure.lock().unwrap();
                if *injected == Some(stage) {
                    *injected = None;
                    return Err(std::io::Error::other("injected Chat index failure").into());
                }
            }
            let _ = stage;
            Ok(())
        })
    }

    fn lock_index(&self) -> Result<MutexGuard<'_, IndexState>, AppError> {
        let state = self
            .index
            .lock()
            .map_err(|_| AppError::ChatStorageUncertain)?;
        if state.uncertain {
            return Err(AppError::ChatStorageUncertain);
        }
        Ok(state)
    }

    fn commit_index(&self, state: &mut IndexState, next: ChatIndex) -> Result<(), AppError> {
        if self.persist(&next).is_err() {
            state.uncertain = true;
            return Err(AppError::ChatStorageUncertain);
        }
        state.index = next;
        Ok(())
    }

    fn change<T>(
        &self,
        edit: impl FnOnce(&mut ChatIndex) -> Result<T, AppError>,
    ) -> Result<T, AppError> {
        let mut index = self.lock_index()?;
        let mut next = index.index.clone();
        let result = edit(&mut next)?;
        self.commit_index(&mut index, next)?;
        Ok(result)
    }

    pub fn create(
        &self,
        request_id: &str,
        title: Option<&str>,
        session_id: &str,
    ) -> Result<ChatSummary, AppError> {
        if !safe_request_id(request_id) {
            return Err(invalid("invalid Chat creation request ID"));
        }
        if !safe_id(session_id) {
            return Err(invalid("invalid Harness session ID"));
        }
        if title.is_some_and(|value| value.len() > 512) {
            return Err(invalid("invalid Chat title"));
        }
        let fingerprint = creation_fingerprint(title)?;
        let mut index = self.lock_index()?;
        if let Some(summary) = Self::lookup_creation(&index, request_id, &fingerprint)? {
            return Ok(summary);
        }
        let display_title = validated_title(title)?;
        let chat_id = request_id.to_owned();
        let timestamp = now();
        let summary = ChatSummary {
            chat_id: chat_id.clone(),
            title: display_title.to_owned(),
            created_at: timestamp,
            updated_at: timestamp,
            archived: false,
            state: ChatState::Idle,
        };
        let mut next = index.index.clone();
        next.chats.insert(
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
        next.creations.insert(
            chat_id,
            ChatCreation {
                fingerprint: Some(fingerprint),
                deleted: false,
            },
        );
        self.commit_index(&mut index, next)?;
        Ok(summary)
    }

    /// A missing entry is not proof that an earlier submission cannot still arrive.
    pub fn creation_status(
        &self,
        request_id: &str,
        title: Option<&str>,
    ) -> Result<Option<ChatSummary>, AppError> {
        if !safe_request_id(request_id) {
            return Err(invalid("invalid Chat creation request ID"));
        }
        if title.is_some_and(|value| value.len() > 512) {
            return Err(invalid("invalid Chat title"));
        }
        let fingerprint = creation_fingerprint(title)?;
        let index = self.lock_index()?;
        let result = Self::lookup_creation(&index, request_id, &fingerprint)?;
        validated_title(title)?;
        Ok(result)
    }

    fn lookup_creation(
        index: &ChatIndex,
        request_id: &str,
        fingerprint: &str,
    ) -> Result<Option<ChatSummary>, AppError> {
        match index.creations.get(request_id) {
            Some(creation) => {
                if creation
                    .fingerprint
                    .as_deref()
                    .is_some_and(|value| value != fingerprint)
                {
                    return Err(AppError::ChatCreateConflict);
                }
                if creation.deleted {
                    return Err(AppError::ChatCreateDeleted);
                }
                if creation.fingerprint.is_none() {
                    return Err(AppError::ChatStorageUncertain);
                }
                index
                    .chats
                    .get(request_id)
                    .map(|record| Some(record.summary.clone()))
                    .ok_or(AppError::ChatStorageUncertain)
            }
            None if index.chats.contains_key(request_id) => Err(AppError::ChatStorageUncertain),
            None => Ok(None),
        }
    }

    pub fn get(&self, chat_id: &str) -> Result<ChatSummary, AppError> {
        if !safe_id(chat_id) {
            return Err(invalid("invalid Chat ID"));
        }
        self.lock_index()?
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
        self.list_rows(cursor, limit, include_archived)
    }

    /// Return one App-scoped page in the same archive-filtered, stable order.
    /// The next cursor is the last included Chat ID and is absent at exhaustion.
    pub fn list_page(
        &self,
        cursor: Option<&str>,
        limit: usize,
        include_archived: bool,
    ) -> Result<ChatListPage, AppError> {
        if !(1..=100).contains(&limit) {
            return Err(invalid("invalid Chat list limit"));
        }
        let mut chats = self.list_rows(cursor, limit + 1, include_archived)?;
        let has_more = chats.len() > limit;
        chats.truncate(limit);
        let next_cursor = if has_more {
            chats.last().map(|chat| chat.chat_id.clone())
        } else {
            None
        };
        Ok(ChatListPage { chats, next_cursor })
    }

    fn list_rows(
        &self,
        cursor: Option<&str>,
        limit: usize,
        include_archived: bool,
    ) -> Result<Vec<ChatSummary>, AppError> {
        let index = self.lock_index()?;
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
        self.lock_index()?
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
        self.lock_index()?
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
        let index = self.lock_index()?;
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
            let index = self.lock_index()?;
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
            .lock_index()?
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
            let mut index = self.lock_index()?;
            let mut next = index.index.clone();
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
            self.commit_index(&mut index, next)?;
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
        let index = self.lock_index()?;
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
            let index = self.lock_index()?;
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
        let index = self.lock_index()?;
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
            let index = self.lock_index()?;
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
        let mut index = self.lock_index()?;
        if let Some(record) = index.chats.get(chat_id) {
            if record.summary.state == ChatState::Running {
                return Err(AppError::Busy(chat_id.into()));
            }
            let mut next = index.index.clone();
            next.chats.remove(chat_id);
            let creation = next
                .creations
                .get_mut(chat_id)
                .ok_or(AppError::ChatStorageUncertain)?;
            creation.deleted = true;
            self.commit_index(&mut index, next)?;
        } else if !index
            .creations
            .get(chat_id)
            .is_some_and(|creation| creation.deleted)
        {
            return Err(AppError::NotFound("App Chat".into()));
        }
        drop(index);

        if let Err(error) = self.remove_deleted_directory(chat_id) {
            if let Ok(mut diagnostics) = self.cleanup_diagnostics.lock() {
                diagnostics.failed_ids.insert(chat_id.to_owned());
            }
            tracing::warn!("Deleted Chat directory cleanup needs retry");
            return Err(error);
        }
        if let Err(error) = self.sync_deleted_directory_parent() {
            if let Ok(mut diagnostics) = self.cleanup_diagnostics.lock() {
                diagnostics.failed_ids.remove(chat_id);
                diagnostics.parent_sync_failed = true;
            }
            tracing::warn!("Deleted Chat directory sync needs retry");
            return Err(error);
        }
        if let Ok(mut diagnostics) = self.cleanup_diagnostics.lock() {
            diagnostics.failed_ids.remove(chat_id);
            diagnostics.parent_sync_failed = false;
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
    fn deleted_chat_cleanup_recovers_on_reopen_without_touching_other_data() {
        let root = tempfile::tempdir().unwrap();
        let store = ChatStore::open(root.path()).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let session = uuid::Uuid::new_v4().to_string();
        store.create(&id, None, &session).unwrap();
        let transcript = store.transcript_path(&id).unwrap();
        fs::write(&transcript, b"deleted transcript").unwrap();
        let other_id = uuid::Uuid::new_v4().to_string();
        store
            .create(&other_id, None, &uuid::Uuid::new_v4().to_string())
            .unwrap();
        let other_transcript = store.transcript_path(&other_id).unwrap();
        fs::write(&other_transcript, b"keep active chat").unwrap();
        let orphan = root
            .path()
            .join("chats")
            .join(uuid::Uuid::new_v4().to_string());
        fs::create_dir(&orphan).unwrap();
        fs::write(orphan.join("keep"), b"unindexed data").unwrap();

        *store.injected_cleanup_failure.lock().unwrap() = Some(CleanupStage::BeforeRemove);
        assert!(store.delete(&id).is_err());
        assert_eq!(
            store.pending_deletion_cleanup().unwrap(),
            (vec![id.clone()], false)
        );
        assert!(matches!(
            store.creation_status(&id, None),
            Err(AppError::ChatCreateDeleted)
        ));
        assert!(transcript.exists());
        drop(store);

        let reopened = ChatStore::open(root.path()).unwrap();
        assert!(!transcript.exists());
        assert_eq!(
            reopened.pending_deletion_cleanup().unwrap(),
            (vec![], false)
        );
        assert_eq!(fs::read(&other_transcript).unwrap(), b"keep active chat");
        assert_eq!(fs::read(orphan.join("keep")).unwrap(), b"unindexed data");
        assert!(matches!(
            reopened.create(&id, None, &session),
            Err(AppError::ChatCreateDeleted)
        ));
        reopened.delete(&id).unwrap();
        reopened.delete(&id).unwrap();
        assert!(matches!(
            reopened.delete(&uuid::Uuid::new_v4().to_string()),
            Err(AppError::NotFound(_))
        ));
    }

    #[test]
    fn deleted_parent_sync_failure_is_retryable_even_after_directory_is_gone() {
        let root = tempfile::tempdir().unwrap();
        let store = ChatStore::open(root.path()).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        store
            .create(&id, None, &uuid::Uuid::new_v4().to_string())
            .unwrap();
        let transcript = store.transcript_path(&id).unwrap();
        fs::write(&transcript, b"delete me").unwrap();
        *store.injected_cleanup_failure.lock().unwrap() = Some(CleanupStage::BeforeParentSync);
        assert!(store.delete(&id).is_err());
        assert!(!transcript.exists());
        assert_eq!(store.pending_deletion_cleanup().unwrap(), (vec![], true));
        store.delete(&id).unwrap();
        assert_eq!(store.pending_deletion_cleanup().unwrap(), (vec![], false));
        drop(store);
        let reopened = ChatStore::open(root.path()).unwrap();
        assert!(matches!(
            reopened.creation_status(&id, None),
            Err(AppError::ChatCreateDeleted)
        ));
    }

    #[test]
    fn uncertain_delete_rename_preserves_files_until_open_reconciles_index() {
        for stage in [PersistStage::BeforeRename, PersistStage::AfterRename] {
            let root = tempfile::tempdir().unwrap();
            let store = ChatStore::open(root.path()).unwrap();
            let id = uuid::Uuid::new_v4().to_string();
            store
                .create(&id, None, &uuid::Uuid::new_v4().to_string())
                .unwrap();
            let transcript = store.transcript_path(&id).unwrap();
            fs::write(&transcript, b"preserve until commit is known").unwrap();
            *store.injected_persist_failure.lock().unwrap() = Some(stage);
            assert!(matches!(
                store.delete(&id),
                Err(AppError::ChatStorageUncertain)
            ));
            assert!(transcript.exists());
            drop(store);
            let reopened = ChatStore::open(root.path()).unwrap();
            if stage == PersistStage::BeforeRename {
                assert!(reopened.get(&id).is_ok());
                assert!(transcript.exists());
                reopened.delete(&id).unwrap();
            } else {
                assert!(matches!(
                    reopened.creation_status(&id, None),
                    Err(AppError::ChatCreateDeleted)
                ));
            }
            assert!(!transcript.exists());
            reopened.delete(&id).unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn failed_automatic_cleanup_is_observable_without_blocking_other_apps() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let first_root = root.path().join("first");
        let other_root = root.path().join("other");
        let store = ChatStore::open(&first_root).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        store
            .create(&id, None, &uuid::Uuid::new_v4().to_string())
            .unwrap();
        let path = store.transcript_path(&id).unwrap();
        *store.injected_cleanup_failure.lock().unwrap() = Some(CleanupStage::BeforeRemove);
        assert!(store.delete(&id).is_err());
        drop(store);
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
        let external = root.path().join("outside");
        fs::create_dir(&external).unwrap();
        fs::write(external.join("keep"), b"outside").unwrap();
        symlink(&external, path.parent().unwrap()).unwrap();

        let reopened = ChatStore::open(&first_root).unwrap();
        assert_eq!(
            reopened.pending_deletion_cleanup().unwrap(),
            (vec![id.clone()], false)
        );
        assert_eq!(fs::read(external.join("keep")).unwrap(), b"outside");
        assert!(reopened.delete(&id).is_err());
        let other = ChatStore::open(&other_root).unwrap();
        let other_id = uuid::Uuid::new_v4().to_string();
        assert!(other
            .create(&other_id, None, &uuid::Uuid::new_v4().to_string())
            .is_ok());
        fs::remove_file(path.parent().unwrap()).unwrap();
        reopened.delete(&id).unwrap();
        assert_eq!(
            reopened.pending_deletion_cleanup().unwrap(),
            (vec![], false)
        );
        assert_eq!(fs::read(external.join("keep")).unwrap(), b"outside");
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_rejects_a_replaced_parent_symlink() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let store = ChatStore::open(root.path()).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let session = uuid::Uuid::new_v4().to_string();
        store.create(&id, None, &session).unwrap();
        let transcript = store.transcript_path(&id).unwrap();
        fs::write(&transcript, b"original").unwrap();
        let chats = root.path().join("chats");
        let moved = root.path().join("saved-chats");
        let external = root.path().join("outside");
        fs::create_dir(&external).unwrap();
        fs::create_dir(external.join(&id)).unwrap();
        fs::write(external.join(&id).join("keep"), b"outside").unwrap();
        fs::rename(&chats, &moved).unwrap();
        symlink(&external, &chats).unwrap();

        assert!(store.delete(&id).is_err());
        assert_eq!(
            fs::read(external.join(&id).join("keep")).unwrap(),
            b"outside"
        );
        assert_eq!(
            fs::read(moved.join(&id).join(format!("{session}.jsonl"))).unwrap(),
            b"original"
        );
        assert_eq!(
            store.pending_deletion_cleanup().unwrap(),
            (vec![id.clone()], false)
        );
        fs::remove_file(&chats).unwrap();
        fs::rename(&moved, &chats).unwrap();
        store.delete(&id).unwrap();
        assert_eq!(
            fs::read(external.join(&id).join("keep")).unwrap(),
            b"outside"
        );
        assert!(!transcript.exists());
    }

    #[test]
    fn automatic_parent_sync_failure_is_reported_and_can_be_retried() {
        let root = tempfile::tempdir().unwrap();
        let store = ChatStore::open(root.path()).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        store
            .create(&id, None, &uuid::Uuid::new_v4().to_string())
            .unwrap();
        store.delete(&id).unwrap();
        *store.injected_cleanup_failure.lock().unwrap() = Some(CleanupStage::BeforeParentSync);
        store.recover_deleted_directories();
        assert_eq!(store.pending_deletion_cleanup().unwrap(), (vec![], true));
        store.recover_deleted_directories();
        assert_eq!(store.pending_deletion_cleanup().unwrap(), (vec![], false));
    }

    #[test]
    fn creation_identity_is_exact_durable_and_tombstoned() {
        let root = tempfile::tempdir().unwrap();
        let store = ChatStore::open(root.path()).unwrap();
        let request_id = uuid::Uuid::new_v4().to_string();
        let session_id = uuid::Uuid::new_v4().to_string();
        assert!(store.creation_status(&request_id, None).unwrap().is_none());
        let chat = store.create(&request_id, None, &session_id).unwrap();
        assert_eq!(chat.chat_id, request_id);
        assert!(!root.path().join("chats").join(&request_id).exists());
        assert_eq!(
            store
                .create(&request_id, None, &uuid::Uuid::new_v4().to_string())
                .unwrap(),
            chat
        );
        assert_eq!(store.session_id(&request_id).unwrap(), session_id);
        assert!(matches!(
            store.create(&request_id, Some("New chat"), &session_id),
            Err(AppError::ChatCreateConflict)
        ));
        assert!(matches!(
            store.creation_status(&request_id, Some("New chat")),
            Err(AppError::ChatCreateConflict)
        ));
        let other = uuid::Uuid::new_v4().to_string();
        assert_ne!(
            store
                .create(&other, None, &uuid::Uuid::new_v4().to_string())
                .unwrap()
                .chat_id,
            chat.chat_id
        );
        assert!(store
            .transcript_path(&request_id)
            .unwrap()
            .parent()
            .unwrap()
            .is_dir());
        drop(store);

        let store = ChatStore::open(root.path()).unwrap();
        assert_eq!(
            store.creation_status(&request_id, None).unwrap(),
            Some(chat)
        );
        assert_eq!(store.session_id(&request_id).unwrap(), session_id);
        store.delete(&request_id).unwrap();
        drop(store);
        let store = ChatStore::open(root.path()).unwrap();
        assert!(matches!(
            store.creation_status(&request_id, None),
            Err(AppError::ChatCreateDeleted)
        ));
        assert!(matches!(
            store.create(&request_id, None, &session_id),
            Err(AppError::ChatCreateDeleted)
        ));
        assert!(matches!(
            store.create(&request_id, Some("different"), &session_id),
            Err(AppError::ChatCreateConflict)
        ));
        assert_eq!(store.list(None, 100, true).unwrap().len(), 1);
    }

    #[test]
    fn concurrent_create_commits_one_private_session() {
        let root = tempfile::tempdir().unwrap();
        let store = ChatStore::open(root.path()).unwrap();
        let request_id = uuid::Uuid::new_v4().to_string();
        let barrier = std::sync::Barrier::new(8);
        std::thread::scope(|scope| {
            let handles = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        store
                            .create(
                                &request_id,
                                Some("same title"),
                                &uuid::Uuid::new_v4().to_string(),
                            )
                            .unwrap()
                    })
                })
                .collect::<Vec<_>>();
            for handle in handles {
                assert_eq!(handle.join().unwrap().chat_id, request_id);
            }
        });
        assert_eq!(store.list(None, 100, true).unwrap().len(), 1);
        let session = store.session_id(&request_id).unwrap();
        drop(store);
        let reopened = ChatStore::open(root.path()).unwrap();
        assert_eq!(reopened.session_id(&request_id).unwrap(), session);
        assert_eq!(
            reopened
                .create(
                    &request_id,
                    Some("same title"),
                    &uuid::Uuid::new_v4().to_string()
                )
                .unwrap()
                .chat_id,
            request_id
        );
    }

    #[test]
    fn index_write_failures_block_all_access_until_reopen() {
        for stage in [PersistStage::BeforeRename, PersistStage::AfterRename] {
            let root = tempfile::tempdir().unwrap();
            let store = ChatStore::open(root.path()).unwrap();
            let request_id = uuid::Uuid::new_v4().to_string();
            let session_id = uuid::Uuid::new_v4().to_string();
            *store.injected_persist_failure.lock().unwrap() = Some(stage);
            assert!(matches!(
                store.create(&request_id, None, &session_id),
                Err(AppError::ChatStorageUncertain)
            ));
            assert!(matches!(
                store.creation_status(&request_id, None),
                Err(AppError::ChatStorageUncertain)
            ));
            assert!(matches!(
                store.list(None, 10, true),
                Err(AppError::ChatStorageUncertain)
            ));
            assert!(matches!(
                store.create(&request_id, None, &session_id),
                Err(AppError::ChatStorageUncertain)
            ));
            assert!(!root.path().join("chats").join(&request_id).exists());
            drop(store);
            let reopened = ChatStore::open(root.path()).unwrap();
            let found = reopened.creation_status(&request_id, None).unwrap();
            assert_eq!(found.is_some(), stage == PersistStage::AfterRename);
            let restored = reopened.create(&request_id, None, &session_id).unwrap();
            assert_eq!(restored.chat_id, request_id);
            assert_eq!(reopened.list(None, 10, true).unwrap().len(), 1);
        }
    }

    #[test]
    fn corrupt_creation_index_is_never_rewritten_or_opened() {
        let root = tempfile::tempdir().unwrap();
        let store = ChatStore::open(root.path()).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        store
            .create(&id, Some("test"), &uuid::Uuid::new_v4().to_string())
            .unwrap();
        drop(store);
        let path = root.path().join("chats-index.json");
        let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        value["chats"].as_object_mut().unwrap().remove(&id);
        let bytes = serde_json::to_vec(&value).unwrap();
        fs::write(&path, &bytes).unwrap();
        assert!(matches!(
            ChatStore::open(root.path()),
            Err(AppError::ChatStorageUncertain)
        ));
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }

    #[test]
    fn missing_creation_fingerprint_or_reverse_mapping_is_never_accepted() {
        let root = tempfile::tempdir().unwrap();
        let store = ChatStore::open(root.path()).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        store
            .create(&id, Some("exact title"), &uuid::Uuid::new_v4().to_string())
            .unwrap();
        drop(store);
        let path = root.path().join("chats-index.json");
        let original: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let mut missing_mapping = original.clone();
        missing_mapping["creations"]
            .as_object_mut()
            .unwrap()
            .remove(&id);
        let mut missing_fingerprint = original.clone();
        missing_fingerprint["creations"][&id]["fingerprint"] = Value::Null;
        let mut mismatched_summary = original.clone();
        mismatched_summary["chats"][&id]["summary"]["chatId"] =
            json!(uuid::Uuid::new_v4().to_string());
        for value in [missing_mapping, missing_fingerprint, mismatched_summary] {
            let bytes = serde_json::to_vec(&value).unwrap();
            fs::write(&path, &bytes).unwrap();
            assert!(matches!(
                ChatStore::open(root.path()),
                Err(AppError::ChatStorageUncertain)
            ));
            assert_eq!(fs::read(&path).unwrap(), bytes);
        }
    }

    #[test]
    fn existing_index_preflight_is_read_only_and_distinguishes_missing_creation_index() {
        let temp = tempfile::tempdir().unwrap();
        let new_root = temp.path().join("new");
        ChatStore::preflight_existing_index(&new_root).unwrap();
        assert!(!new_root.exists());

        let empty_root = temp.path().join("empty");
        fs::create_dir(&empty_root).unwrap();
        ChatStore::preflight_existing_index(&empty_root).unwrap();
        assert_eq!(fs::read_dir(&empty_root).unwrap().count(), 0);
        fs::create_dir(empty_root.join("chats")).unwrap();
        ChatStore::preflight_existing_index(&empty_root).unwrap();
        assert_eq!(fs::read_dir(empty_root.join("chats")).unwrap().count(), 0);

        let legacy_root = temp.path().join("legacy");
        let store = ChatStore::open(&legacy_root).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        store
            .create(&id, Some("preserved"), &uuid::Uuid::new_v4().to_string())
            .unwrap();
        let transcript = store.transcript_path(&id).unwrap();
        fs::write(&transcript, b"original transcript").unwrap();
        drop(store);
        let index_path = legacy_root.join("chats-index.json");
        let mut legacy: Value = serde_json::from_slice(&fs::read(&index_path).unwrap()).unwrap();
        legacy.as_object_mut().unwrap().remove("creations");
        let bytes = serde_json::to_vec(&legacy).unwrap();
        fs::write(&index_path, &bytes).unwrap();
        assert!(matches!(
            ChatStore::preflight_existing_index(&legacy_root),
            Err(AppError::ChatLegacyIndexUnsupported)
        ));
        assert!(matches!(
            ChatStore::open(&legacy_root),
            Err(AppError::ChatLegacyIndexUnsupported)
        ));
        assert_eq!(fs::read(&index_path).unwrap(), bytes);
        assert_eq!(fs::read(&transcript).unwrap(), b"original transcript");

        // A missing marker must not be silently accepted even for an empty
        // older index, and a failed open must not create a chats directory.
        let empty_legacy_root = temp.path().join("empty-legacy");
        fs::create_dir(&empty_legacy_root).unwrap();
        fs::write(
            empty_legacy_root.join("chats-index.json"),
            b"{\"chats\":{}}",
        )
        .unwrap();
        assert!(matches!(
            ChatStore::preflight_existing_index(&empty_legacy_root),
            Err(AppError::ChatLegacyIndexUnsupported)
        ));
        assert!(matches!(
            ChatStore::open(&empty_legacy_root),
            Err(AppError::ChatLegacyIndexUnsupported)
        ));
        assert!(!empty_legacy_root.join("chats").exists());
    }

    #[test]
    fn missing_index_with_chat_files_or_pending_index_is_not_treated_as_new() {
        let temp = tempfile::tempdir().unwrap();
        let orphan_root = temp.path().join("orphan");
        let transcript = orphan_root
            .join("chats")
            .join("unknown")
            .join("transcript.jsonl");
        fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        fs::write(&transcript, b"preserve unindexed data").unwrap();
        assert!(matches!(
            ChatStore::preflight_existing_index(&orphan_root),
            Err(AppError::ChatStorageUncertain)
        ));
        assert!(matches!(
            ChatStore::open(&orphan_root),
            Err(AppError::ChatStorageUncertain)
        ));
        assert_eq!(fs::read(&transcript).unwrap(), b"preserve unindexed data");
        assert!(!orphan_root.join("chats-index.json").exists());

        let pending_root = temp.path().join("pending");
        fs::create_dir(&pending_root).unwrap();
        let pending = pending_root.join(".chat-index-unknown.tmp");
        fs::write(&pending, b"possibly interrupted index write").unwrap();
        assert!(matches!(
            ChatStore::preflight_existing_index(&pending_root),
            Err(AppError::ChatStorageUncertain)
        ));
        assert!(matches!(
            ChatStore::open(&pending_root),
            Err(AppError::ChatStorageUncertain)
        ));
        assert_eq!(
            fs::read(&pending).unwrap(),
            b"possibly interrupted index write"
        );
        assert!(!pending_root.join("chats").exists());
    }

    #[test]
    fn preflight_rejects_corrupt_index_without_writing_or_cleaning() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("chats-index.json");
        let cases: &[&[u8]] = &[
            b"not JSON",
            b"[]",
            b"{\"chats\":{},\"creations\":null}",
            b"{\"chats\":{},\"creations\":{},\"creations\":{}}",
            b"{\"creations\":{}}",
        ];
        for bytes in cases {
            fs::write(&path, bytes).unwrap();
            assert!(matches!(
                ChatStore::preflight_existing_index(root.path()),
                Err(AppError::ChatStorageUncertain)
            ));
            assert!(matches!(
                ChatStore::open(root.path()),
                Err(AppError::ChatStorageUncertain)
            ));
            assert_eq!(fs::read(&path).unwrap(), *bytes);
            assert!(!root.path().join("chats").exists());
        }
    }

    #[test]
    fn preflight_does_not_clean_pending_tombstones() {
        let root = tempfile::tempdir().unwrap();
        let store = ChatStore::open(root.path()).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        store
            .create(&id, None, &uuid::Uuid::new_v4().to_string())
            .unwrap();
        let transcript = store.transcript_path(&id).unwrap();
        fs::write(&transcript, b"pending cleanup").unwrap();
        *store.injected_cleanup_failure.lock().unwrap() = Some(CleanupStage::BeforeRemove);
        assert!(store.delete(&id).is_err());
        drop(store);
        let index_path = root.path().join("chats-index.json");
        let bytes = fs::read(&index_path).unwrap();
        ChatStore::preflight_existing_index(root.path()).unwrap();
        assert_eq!(fs::read(&index_path).unwrap(), bytes);
        assert_eq!(fs::read(&transcript).unwrap(), b"pending cleanup");
        let reopened = ChatStore::open(root.path()).unwrap();
        assert!(!transcript.exists());
        assert!(matches!(
            reopened.creation_status(&id, None),
            Err(AppError::ChatCreateDeleted)
        ));
    }

    #[test]
    fn preflight_checks_existing_chat_root_before_opening_a_valid_index() {
        let root = tempfile::tempdir().unwrap();
        let store = ChatStore::open(root.path()).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        store
            .create(&id, None, &uuid::Uuid::new_v4().to_string())
            .unwrap();
        drop(store);
        let index_path = root.path().join("chats-index.json");
        let index_bytes = fs::read(&index_path).unwrap();
        let chats = root.path().join("chats");
        fs::remove_dir(&chats).unwrap();
        fs::write(&chats, b"preserve this file").unwrap();

        assert!(matches!(
            ChatStore::preflight_existing_index(root.path()),
            Err(AppError::ChatStorageUncertain)
        ));
        assert!(matches!(
            ChatStore::open(root.path()),
            Err(AppError::ChatStorageUncertain)
        ));
        assert_eq!(fs::read(&chats).unwrap(), b"preserve this file");
        assert_eq!(fs::read(&index_path).unwrap(), index_bytes);

        // A missing chats directory is still allowed and preflight itself
        // must not create it. Opening the store may then create it normally.
        fs::remove_file(&chats).unwrap();
        ChatStore::preflight_existing_index(root.path()).unwrap();
        assert!(!chats.exists());
        let reopened = ChatStore::open(root.path()).unwrap();
        assert!(chats.is_dir());
        assert_eq!(reopened.get(&id).unwrap().chat_id, id);
        assert_eq!(fs::read(index_path).unwrap(), index_bytes);
    }

    #[cfg(unix)]
    #[test]
    fn preflight_rejects_symlinked_chat_root_with_valid_index() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let store = ChatStore::open(root.path()).unwrap();
        store
            .create(
                &uuid::Uuid::new_v4().to_string(),
                None,
                &uuid::Uuid::new_v4().to_string(),
            )
            .unwrap();
        drop(store);
        let index_path = root.path().join("chats-index.json");
        let index_bytes = fs::read(&index_path).unwrap();
        let chats = root.path().join("chats");
        fs::remove_dir(&chats).unwrap();
        let outside = tempfile::tempdir().unwrap();
        let sentinel = outside.path().join("keep");
        fs::write(&sentinel, b"preserve target").unwrap();
        symlink(outside.path(), &chats).unwrap();

        assert!(matches!(
            ChatStore::preflight_existing_index(root.path()),
            Err(AppError::ChatStorageUncertain)
        ));
        assert!(matches!(
            ChatStore::open(root.path()),
            Err(AppError::ChatStorageUncertain)
        ));
        assert_eq!(fs::read(index_path).unwrap(), index_bytes);
        assert_eq!(fs::read(sentinel).unwrap(), b"preserve target");
        assert!(fs::symlink_metadata(chats)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn preflight_propagates_chat_directory_permission_errors() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let store = ChatStore::open(root.path()).unwrap();
        store
            .create(
                &uuid::Uuid::new_v4().to_string(),
                None,
                &uuid::Uuid::new_v4().to_string(),
            )
            .unwrap();
        drop(store);
        let chats = root.path().join("chats");
        let original_mode = fs::metadata(&chats).unwrap().permissions().mode();
        fs::set_permissions(&chats, fs::Permissions::from_mode(0)).unwrap();
        let directory_denied = fs::read_dir(&chats).is_err();
        let preflight = ChatStore::preflight_existing_index(root.path());
        fs::set_permissions(&chats, fs::Permissions::from_mode(original_mode)).unwrap();
        // Privileged test runners can bypass directory permissions.
        if directory_denied {
            assert!(matches!(preflight, Err(AppError::Io(_))));
        }
    }

    #[cfg(unix)]
    #[test]
    fn preflight_refuses_index_or_root_symlinks_without_touching_target() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        fs::create_dir(&root).unwrap();
        let external = temp.path().join("external");
        fs::write(&external, b"{\"chats\":{},\"creations\":{}}").unwrap();
        symlink(&external, root.join("chats-index.json")).unwrap();
        assert!(matches!(
            ChatStore::preflight_existing_index(&root),
            Err(AppError::ChatStorageUncertain)
        ));
        assert!(matches!(
            ChatStore::open(&root),
            Err(AppError::ChatStorageUncertain)
        ));
        assert_eq!(
            fs::read(&external).unwrap(),
            b"{\"chats\":{},\"creations\":{}}"
        );
        assert!(!root.join("chats").exists());

        let link = temp.path().join("root-link");
        symlink(&root, &link).unwrap();
        assert!(matches!(
            ChatStore::preflight_existing_index(&link),
            Err(AppError::ChatStorageUncertain)
        ));
        assert!(matches!(
            ChatStore::open(&link),
            Err(AppError::ChatStorageUncertain)
        ));
    }

    #[test]
    fn creation_ids_and_titles_are_bounded() {
        let root = tempfile::tempdir().unwrap();
        let store = ChatStore::open(root.path()).unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let id = uuid::Uuid::new_v4().to_string();
        assert!(store.create(&id.to_uppercase(), None, &session).is_err());
        assert!(store
            .create("00000000-0000-0000-0000-000000000000", None, &session)
            .is_err());
        assert!(store.create(&id, Some(&"x".repeat(81)), &session).is_err());
        assert!(store.create(&id, Some(&" ".repeat(513)), &session).is_err());
        assert!(store.create(&id, Some("bad\ntitle"), &session).is_err());
        assert!(store.list(None, 100, true).unwrap().is_empty());
    }

    fn collect_chat_pages(store: &ChatStore, include_archived: bool) -> Vec<ChatSummary> {
        let mut all = Vec::new();
        let mut cursor = None;
        for _ in 0..10 {
            let page = store
                .list_page(cursor.as_deref(), 2, include_archived)
                .unwrap();
            all.extend(page.chats);
            match page.next_cursor {
                Some(next) => {
                    assert_eq!(
                        all.last().map(|chat| chat.chat_id.as_str()),
                        Some(next.as_str())
                    );
                    cursor = Some(next);
                }
                None => return all,
            }
        }
        panic!("chat list pagination did not reach its final page");
    }

    #[test]
    fn protocol_history_page_omits_exhausted_cursor_and_retains_next_page_cursor() {
        let page = ProtocolHistoryPage {
            messages: vec![serde_json::json!({"id":"m1","role":"user","content":"Hello"})],
            next_cursor: None,
        };
        let exhausted = serde_json::to_value(&page).unwrap();
        assert_eq!(exhausted, serde_json::json!({"messages":page.messages}));
        assert_eq!(
            serde_json::from_value::<ProtocolHistoryPage>(exhausted).unwrap(),
            page
        );

        let more = ProtocolHistoryPage {
            next_cursor: Some("m1".into()),
            ..page
        };
        let wire = serde_json::to_value(&more).unwrap();
        assert_eq!(wire["nextCursor"], "m1");
        assert_eq!(
            serde_json::from_value::<ProtocolHistoryPage>(wire).unwrap(),
            more
        );
    }

    #[test]
    fn chat_list_pages_preserve_order_archive_filter_and_cursor_boundaries() {
        let root = tempfile::tempdir().unwrap();
        let store = ChatStore::open(root.path()).unwrap();
        let chats = (0..5)
            .map(|index| {
                store
                    .create(
                        &uuid::Uuid::new_v4().to_string(),
                        Some(&format!("chat-{index}")),
                        &uuid::Uuid::new_v4().to_string(),
                    )
                    .unwrap()
            })
            .collect::<Vec<_>>();
        store.archive(&chats[2].chat_id).unwrap();

        let expected_active = store.list(None, 100, false).unwrap();
        assert_eq!(collect_chat_pages(&store, false), expected_active);
        assert!(!expected_active
            .iter()
            .any(|chat| chat.chat_id == chats[2].chat_id));

        let first = store.list_page(None, 2, true).unwrap();
        assert_eq!(first.chats, store.list(None, 2, true).unwrap());
        assert_eq!(
            first.next_cursor.as_deref(),
            first.chats.last().map(|chat| chat.chat_id.as_str())
        );
        let second = store
            .list_page(first.next_cursor.as_deref(), 2, true)
            .unwrap();
        assert_eq!(
            second.chats,
            store.list(first.next_cursor.as_deref(), 2, true).unwrap()
        );
        assert_ne!(
            first
                .chats
                .iter()
                .map(|chat| &chat.chat_id)
                .collect::<Vec<_>>(),
            second
                .chats
                .iter()
                .map(|chat| &chat.chat_id)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            collect_chat_pages(&store, true),
            store.list(None, 100, true).unwrap()
        );
        assert!(store.list_page(Some("missing-chat"), 2, false).is_err());
        assert!(store.list_page(None, 0, false).is_err());
        assert!(store.list_page(None, 101, false).is_err());
    }

    #[test]
    fn chat_session_identity_is_host_only_and_startup_reconciles_task_state() {
        let root = tempfile::tempdir().unwrap();
        let session_id = uuid::Uuid::new_v4().to_string();
        let store = ChatStore::open(root.path()).unwrap();
        let chat = store
            .create(&uuid::Uuid::new_v4().to_string(), None, &session_id)
            .unwrap();
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
            .create(
                &uuid::Uuid::new_v4().to_string(),
                None,
                &uuid::Uuid::new_v4().to_string(),
            )
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
            .create(
                &uuid::Uuid::new_v4().to_string(),
                None,
                &uuid::Uuid::new_v4().to_string(),
            )
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
            .create(
                &uuid::Uuid::new_v4().to_string(),
                None,
                &uuid::Uuid::new_v4().to_string(),
            )
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
