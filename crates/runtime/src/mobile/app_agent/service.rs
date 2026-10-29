//! Process-owned task service. The page and native control APIs are separate:
//! only the latter can configure grants or resolve permissions/questions.
use super::*;
use crate::mobile::unified_local_apps::MobileLocalAppServiceHandle;
use lingxi_core::{
    host::{permission_gate::PermissionDecision, CostSnapshot},
    types::{ConversationMessage, MessageId, ToolUseId},
};
use local_app::v2::agent::{
    AgentStore, AttachmentMetadata, ChatMessageMetadata, ChatRole, ChatState, ChatStore,
    ChatTranscriptCheckpoint, Input, Owner, ProtocolCommitPermit, ProtocolEventPage, Settings,
    State, TaskResult,
};
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Weak,
    },
};
use tokio::sync::{oneshot, watch, Mutex as AsyncMutex, Notify};
use tokio_util::sync::CancellationToken;

pub struct AppAgentHost {
    service: Weak<MobileLocalAppServiceHandle>,
    factory: Arc<AppAgentFactory>,
    apps_root: PathBuf,
    apps: Mutex<HashMap<String, Arc<AppAgentState>>>,
    wake_reservations: Mutex<WakeRegistry>,
    active: AsyncMutex<HashMap<String, Arc<Control>>>,
    admission: AsyncMutex<()>,
    shutdown: AtomicBool,
    #[cfg(test)]
    force_wake_policy: AtomicBool,
    #[cfg(test)]
    before_started_event: Mutex<Option<Box<dyn FnOnce(&Path, &str) + Send>>>,
    #[cfg(test)]
    fail_started_event_schema: AtomicBool,
    #[cfg(test)]
    duplicate_chat_wait_started: Mutex<Option<oneshot::Sender<()>>>,
    #[cfg(test)]
    duplicate_pre_admission_pause: Mutex<Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>>,
    #[cfg(test)]
    duplicate_probe_none_barrier: Mutex<Option<Arc<tokio::sync::Barrier>>>,
    #[cfg(test)]
    chat_executor_init_pause: Mutex<Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>>,
    #[cfg(test)]
    duplicate_task_snapshot_pause: Mutex<Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>>,
}
struct AppAgentState {
    store: Arc<AgentStore>,
    chats: Arc<ChatStore>,
    root: PathBuf,
}
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct WakeKey {
    app_id: String,
    revision: u64,
    request_id: String,
}
struct WakeReservation {
    reservation_id: String,
    expires_at_ms: u64,
    run_id: Option<String>,
    claimed: bool,
}
#[derive(Debug, PartialEq, Eq)]
struct WakeReceipt {
    reservation_id: String,
    expires_at_ms: u64,
    created: bool,
}
#[derive(Default)]
struct WakeRegistry {
    reservations: HashMap<WakeKey, WakeReservation>,
}
impl WakeRegistry {
    fn reserve(&mut self, key: WakeKey, now: u64, timeout_ms: u64) -> Result<WakeReceipt, String> {
        self.reservations.retain(|_, reservation| {
            reservation.run_id.is_some() || reservation.expires_at_ms > now
        });
        if let Some(existing) = self.reservations.get(&key) {
            return Ok(WakeReceipt {
                reservation_id: existing.reservation_id.clone(),
                expires_at_ms: existing.expires_at_ms,
                created: false,
            });
        }
        if self
            .reservations
            .keys()
            .any(|existing| existing.app_id == key.app_id)
        {
            return Err("busy".into());
        }
        if self.reservations.len() >= 1024 {
            return Err("wake_reservations_full".into());
        }
        let expires_at_ms = now.saturating_add(timeout_ms);
        let reservation_id = uuid::Uuid::new_v4().to_string();
        self.reservations.insert(
            key,
            WakeReservation {
                reservation_id: reservation_id.clone(),
                expires_at_ms,
                run_id: None,
                claimed: false,
            },
        );
        Ok(WakeReceipt {
            reservation_id,
            expires_at_ms,
            created: true,
        })
    }

    fn available(&mut self, key: &WakeKey, now: u64) -> bool {
        if self.reservations.get(key).is_some_and(|reservation| {
            reservation.run_id.is_none() && reservation.expires_at_ms <= now
        }) {
            self.reservations.remove(key);
        }
        self.reservations
            .get(key)
            .is_some_and(|reservation| reservation.run_id.is_none() && !reservation.claimed)
    }

    fn claim(&mut self, key: &WakeKey, now: u64) -> bool {
        let Some(reservation) = self.reservations.get_mut(key) else {
            return false;
        };
        if reservation.expires_at_ms <= now || reservation.run_id.is_some() || reservation.claimed {
            return false;
        }
        reservation.claimed = true;
        true
    }

    fn unclaim(&mut self, key: &WakeKey) {
        if let Some(reservation) = self.reservations.get_mut(key) {
            reservation.claimed = false;
        }
    }

    fn consume(&mut self, key: &WakeKey, now: u64, run_id: &str) -> bool {
        let Some(reservation) = self.reservations.get_mut(key) else {
            return false;
        };
        if reservation.expires_at_ms <= now || reservation.run_id.is_some() || !reservation.claimed
        {
            return false;
        }
        reservation.run_id = Some(run_id.to_owned());
        reservation.claimed = false;
        true
    }

    fn clear(&mut self, key: &WakeKey, reservation_id: &str) -> Option<String> {
        if !self
            .reservations
            .get(key)
            .is_some_and(|reservation| reservation.reservation_id == reservation_id)
        {
            return None;
        }
        self.reservations
            .remove(key)
            .and_then(|reservation| reservation.run_id)
    }

    fn clear_run(&mut self, run_id: &str) {
        self.reservations
            .retain(|_, reservation| reservation.run_id.as_deref() != Some(run_id));
    }

    fn clear_app(&mut self, app_id: &str) {
        self.reservations.retain(|key, _| key.app_id != app_id);
    }
}
struct Control {
    owner: Owner,
    revision: u64,
    request_id: String,
    id: String,
    protocol_thread_id: Option<String>,
    cancel: CancellationToken,
    finished: AtomicBool,
    done: Notify,
    lease: Mutex<Option<SessionToolGrantLease>>,
    interaction: AsyncMutex<Option<(String, Value, Reply)>>,
    store: Arc<AgentStore>,
    chats: Arc<ChatStore>,
    chat_id: Option<String>,
    user_message_id: Option<String>,
    chat_orchestrator: Mutex<Option<Arc<orchestrator::ConversationOrchestrator>>>,
    chat_persisted: watch::Sender<Option<Result<(), String>>>,
    chat_commit: AsyncMutex<Option<oneshot::Receiver<()>>>,
    attachment_metadata: Vec<AttachmentMetadata>,
    assistant_message_ids: AsyncMutex<HashSet<MessageId>>,
    assistant_display_text: AsyncMutex<HashMap<MessageId, String>>,
    protocol_events_failed: AtomicBool,
    native_output: NativeOutputProgress,
    text: AsyncMutex<String>,
    model_failed: AtomicBool,
    model_failure: Mutex<Option<String>>,
    timed_out: AtomicBool,
    usage: AsyncMutex<Option<Value>>,
    #[cfg(test)]
    fail_after_final_ledger_before_chat_authority: AtomicBool,
    #[cfg(test)]
    after_final_ledger_error_barrier: Mutex<Option<Arc<std::sync::Barrier>>>,
}
// JSON numbers remain exact in both the JavaScript bridge and the native host.
const MAX_NATIVE_OUTPUT_BYTES: u64 = 9_007_199_254_740_991;

#[derive(Default)]
struct NativeOutputProgress {
    bytes: AtomicU64,
    pending_high_surrogates: Mutex<HashSet<u32>>,
}

impl NativeOutputProgress {
    fn add(&self, bytes: u64) {
        let _ = self
            .bytes
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                Some(current.saturating_add(bytes).min(MAX_NATIVE_OUTPUT_BYTES))
            });
    }

    fn observe(&self, block_index: u32, units: &[u16]) {
        if units.is_empty() {
            return;
        }
        let Ok(mut pending) = self.pending_high_surrogates.lock() else {
            return;
        };
        let mut bytes = 0u64;
        for &unit in units {
            if pending.remove(&block_index) {
                if (0xdc00..=0xdfff).contains(&unit) {
                    bytes = bytes.saturating_add(4);
                    continue;
                }
                // Match UTF-8 encoding of a JavaScript string with a lone unit.
                bytes = bytes.saturating_add(3);
            }
            bytes = bytes.saturating_add(match unit {
                0..=0x7f => 1,
                0x80..=0x7ff => 2,
                0xd800..=0xdbff => {
                    pending.insert(block_index);
                    0
                }
                _ => 3,
            });
        }
        self.add(bytes);
    }

    fn finish_block(&self, block_index: u32) {
        if self
            .pending_high_surrogates
            .lock()
            .is_ok_and(|mut pending| pending.remove(&block_index))
        {
            self.add(3);
        }
    }

    fn finish_all(&self) {
        if let Ok(mut pending) = self.pending_high_surrogates.lock() {
            let bytes = (pending.len() as u64).saturating_mul(3);
            pending.clear();
            self.add(bytes);
        }
    }

    fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }
}

enum Reply {
    Permission(oneshot::Sender<bool>),
    Question(oneshot::Sender<Vec<(Utf16JsonProjection, Utf16JsonProjection)>>),
}

impl AppAgentState {
    fn open(app_root: &Path, app_id: &str) -> Result<Self, String> {
        let owner = Owner {
            app_id: app_id.into(),
            release_digest: "0".repeat(64),
            invocation_id: "startup-recovery".into(),
        };
        owner.validate().map_err(|_| "invalid_app")?;
        let legacy_root = app_root.join(app_id).join("agent");
        // Inspect an existing Chat index before AgentStore recovery can modify
        // task records or protocol events. Never initialize over an index we
        // cannot interpret.
        ChatStore::preflight_existing_index(&legacy_root.join("agui-v1"))
            .map_err(chat_store_open_error)?;
        let root = initialize_agui_agent_store(&legacy_root)?;
        let store = Arc::new(
            AgentStore::open(&root.join("tasks")).map_err(|_| "agent_storage_unavailable")?,
        );
        let chats = Arc::new(ChatStore::open(&root).map_err(chat_store_open_error)?);
        let committed_assistants = store
            .committed_protocol_assistants()
            .map_err(|_| "agent_storage_unavailable")?;
        chats
            .set_committed_assistants(&committed_assistants)
            .map_err(chat_store_open_error)?;
        let task_states = store
            .recovery_states(app_id)
            .map_err(|_| "agent_storage_unavailable")?;
        chats
            .reconcile_startup(&task_states)
            .map_err(|_| "agent_storage_unavailable")?;
        Ok(Self { store, chats, root })
    }
}

fn chat_store_open_error(error: local_app::v2::AppError) -> &'static str {
    match error {
        local_app::v2::AppError::ChatLegacyIndexUnsupported => "chat_legacy_index_unsupported",
        local_app::v2::AppError::ChatStorageUncertain => "chat_storage_uncertain",
        _ => "agent_storage_unavailable",
    }
}

/// Create a fresh AG-UI 1.0 store once per App, while retaining any older
/// Agent data that cannot be identified as safe to discard.
fn initialize_agui_agent_store(agent_root: &Path) -> Result<PathBuf, String> {
    ensure_agent_directory(agent_root)?;
    let store_root = agent_root.join("agui-v1");
    let marker = store_root.join("protocol-store.json");
    match std::fs::symlink_metadata(&store_root) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err("agent_storage_unavailable".into());
            }
            match std::fs::symlink_metadata(&marker) {
                Ok(metadata) => {
                    if metadata.file_type().is_symlink() || !metadata.is_file() {
                        return Err("agent_storage_unavailable".into());
                    }
                    let value: Value = serde_json::from_slice(
                        &std::fs::read(&marker).map_err(|_| "agent_storage_unavailable")?,
                    )
                    .map_err(|_| "agent_storage_unavailable")?;
                    if value["protocol"] != "AG-UI"
                        || value["version"] != "1.0"
                        || value["schemaSha256"]
                            != local_app::v2::agent::protocol::pinned_schema_sha256()
                    {
                        return Err("agent_protocol_store_version_mismatch".into());
                    }
                    // The marker only identifies the new subdirectory. An
                    // older sibling store can still hold user data and must
                    // be surfaced before any task recovery or cleanup runs.
                    reject_existing_legacy_agent_data(agent_root)?;
                    return Ok(store_root);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    if std::fs::read_dir(&store_root)
                        .map_err(|_| "agent_storage_unavailable")?
                        .next()
                        .is_some()
                    {
                        return Err("agent_protocol_store_marker_missing".into());
                    }
                    reject_existing_legacy_agent_data(agent_root)?;
                }
                Err(_) => return Err("agent_storage_unavailable".into()),
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            reject_existing_legacy_agent_data(agent_root)?;
            create_agent_directory(&store_root)?;
        }
        Err(_) => return Err("agent_storage_unavailable".into()),
    }
    let payload = serde_json::to_vec(&json!({
        "protocol":"AG-UI",
        "version":"1.0",
        "schemaSha256":local_app::v2::agent::protocol::pinned_schema_sha256()
    }))
    .map_err(|_| "agent_storage_unavailable")?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&marker)
        .map_err(|_| "agent_storage_unavailable")?;
    use std::io::Write;
    file.write_all(&payload)
        .and_then(|_| file.sync_all())
        .map_err(|_| "agent_storage_unavailable")?;
    std::fs::File::open(&store_root)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| "agent_storage_unavailable")?;
    Ok(store_root)
}

fn ensure_agent_directory(path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            Err("agent_storage_unavailable".into())
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => create_agent_directory(path),
        Err(_) => Err("agent_storage_unavailable".into()),
    }
}

fn create_agent_directory(path: &Path) -> Result<(), String> {
    let parent = path.parent().ok_or("agent_storage_unavailable")?;
    let parent_metadata =
        std::fs::symlink_metadata(parent).map_err(|_| "agent_storage_unavailable")?;
    if parent_metadata.file_type().is_symlink() || !parent_metadata.is_dir() {
        return Err("agent_storage_unavailable".into());
    }
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(path)
        .map_err(|_| "agent_storage_unavailable")?;
    Ok(())
}

fn reject_existing_legacy_agent_data(agent_root: &Path) -> Result<(), String> {
    // These paths may contain real user data. A filename or UUID alone cannot
    // prove that deleting it is safe, so leave the whole old store untouched.
    for name in ["tasks", "chats", "chats-index.json", "protocol-events"] {
        match std::fs::symlink_metadata(agent_root.join(name)) {
            Ok(_) => return Err("agent_legacy_store_unsupported".into()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("agent_storage_unavailable".into()),
        }
    }
    Ok(())
}

impl Control {
    async fn run_stateless_query<F>(&self, query: F) -> Result<(TaskResult, Value), String>
    where
        F: std::future::Future<Output = Result<(TaskResult, Value), String>>,
    {
        // Dropping the local query future on revocation lets the task reach its
        // terminal state even when the provider never completes the request.
        // Cancellation wins when both branches are ready in the same poll.
        let (result, usage) = tokio::select! {
            biased;
            _ = self.cancel.cancelled() => return Err("cancelled".into()),
            result = query => result?,
        };
        self.store
            .set_completed_model_turns(&self.owner, &self.id, 1)
            .map_err(|_| "task_storage_failed")?;
        *self.usage.lock().await = Some(usage.clone());
        Ok((result, usage))
    }

    fn append_protocol_event(&self, event: Value) -> Result<(), String> {
        self.append_protocol_event_inner(event, None)
    }

    fn append_protocol_commit_event(
        &self,
        permit: &ProtocolCommitPermit<'_>,
        event: Value,
    ) -> Result<(), String> {
        self.append_protocol_event_inner(event, Some(permit))
    }

    fn append_protocol_event_inner(
        &self,
        event: Value,
        permit: Option<&ProtocolCommitPermit<'_>>,
    ) -> Result<(), String> {
        let Some(_) = self.protocol_thread_id else {
            return Ok(());
        };
        let append = if let Some(permit) = permit {
            self.store
                .append_protocol_commit_event(permit, &self.owner, &self.id, event)
        } else {
            self.store.append_event(&self.owner, &self.id, event)
        };
        match append {
            Ok(_) => Ok(()),
            Err(_) => {
                self.protocol_events_failed.store(true, Ordering::Release);
                self.store.quarantine_storage();
                self.chats.quarantine_storage();
                if let Ok(mut failure) = self.model_failure.lock() {
                    *failure = Some("agent_storage_unavailable".into());
                }
                self.model_failed.store(true, Ordering::Release);
                self.revoke();
                Err("agent_storage_unavailable".into())
            }
        }
    }

    // Only a successfully committed task may publish its one accepted answer.
    // The same value is stored in chat history and emitted to the pinned SDK.
    fn publish_accepted_result(
        &self,
        permit: &ProtocolCommitPermit<'_>,
        result: &TaskResult,
    ) -> Result<(), String> {
        if self.protocol_thread_id.is_none() {
            return Ok(());
        }
        let message_id = result
            .message_id
            .as_deref()
            .ok_or("chat_message_identity_missing")?;
        if let Some(chat_id) = &self.chat_id {
            let message = json!({"id":message_id,"role":"assistant","content":result.text});
            self.chats
                .record_protocol_message(chat_id, &self.id, &message)
                .map_err(|_| "chat_storage_unavailable")?;
            self.chats
                .verify_protocol_assistant(chat_id, &self.id, message_id, &result.text)
                .map_err(|_| "chat_storage_unavailable")?;
        }
        self.append_protocol_commit_event(
            permit,
            json!({
                "type":"TEXT_MESSAGE_START", "messageId":message_id, "role":"assistant"
            }),
        )?;
        // Keep each event below the serialized 3 MiB limit even when every
        // byte requires JSON escaping. Never split a UTF-8 character.
        // Empty deltas are valid and preserve structured-only messages.
        const CHUNK_BYTES: usize = 256 * 1024;
        if result.text.is_empty() {
            self.append_protocol_commit_event(
                permit,
                json!({
                    "type":"TEXT_MESSAGE_CONTENT", "messageId":message_id, "delta":""
                }),
            )?;
        } else {
            let mut start = 0;
            while start < result.text.len() {
                let mut end = start.saturating_add(CHUNK_BYTES).min(result.text.len());
                while !result.text.is_char_boundary(end) {
                    end -= 1;
                }
                self.append_protocol_commit_event(
                    permit,
                    json!({
                        "type":"TEXT_MESSAGE_CONTENT", "messageId":message_id,
                        "delta":&result.text[start..end]
                    }),
                )?;
                start = end;
            }
        }
        self.append_protocol_commit_event(
            permit,
            json!({
                "type":"TEXT_MESSAGE_END", "messageId":message_id
            }),
        )
    }

    fn commit_terminal(
        &self,
        state: State,
        output: Option<TaskResult>,
        usage: Option<Value>,
        error: Option<String>,
    ) -> Result<(local_app::v2::agent::Task, bool), String> {
        let commit = self.store.with_protocol_commit_permit(|permit| {
            let result: Result<(local_app::v2::agent::Task, bool), String> = (|| {
                if self.protocol_thread_id.is_none() {
                    let (state, output, error) =
                        if state == State::Succeeded && self.cancel.is_cancelled() {
                            (State::Cancelled, None, Some("cancelled".into()))
                        } else {
                            (state, output, error)
                        };
                    return self
                        .store
                        .update(&self.owner, &self.id, state, output, usage, error)
                        .map(|task| (task, false))
                        .map_err(|_| "agent_storage_unavailable".to_owned());
                }

                if self.protocol_events_failed.load(Ordering::Acquire) {
                    return Err("agent_storage_unavailable".to_owned());
                }
                let (state, output, error) =
                    if state == State::Succeeded && self.cancel.is_cancelled() {
                        (State::Cancelled, None, Some("cancelled".into()))
                    } else {
                        (state, output, error)
                    };
                let candidate = self
                    .store
                    .prepare_protocol_terminal(
                        permit,
                        &self.owner,
                        &self.id,
                        state,
                        output,
                        usage,
                        error,
                    )
                    .map_err(|_| "agent_storage_unavailable".to_owned())?;
                if candidate.state == State::Succeeded {
                    let result = candidate
                        .result
                        .as_ref()
                        .ok_or_else(|| "agent_storage_unavailable".to_owned())?;
                    self.publish_accepted_result(permit, result)?;
                }
                let thread_id = self
                    .protocol_thread_id
                    .as_deref()
                    .expect("protocol task has a thread ID");
                let terminal = protocol_terminal_event(&candidate, thread_id)
                    .ok_or_else(|| "agent_storage_unavailable".to_owned())?;
                self.store
                    .append_protocol_commit_event(permit, &self.owner, &self.id, terminal)
                    .map_err(|_| "agent_storage_unavailable".to_owned())?;
                let task = self
                    .store
                    .finalize_protocol_terminal(permit, &self.owner, &self.id)
                    .map_err(|_| "agent_storage_unavailable".to_owned())?;
                if task.state == State::Succeeded && self.chat_id.is_some() {
                    let accepted = self
                        .store
                        .committed_protocol_assistants()
                        .map_err(|_| "agent_storage_unavailable".to_owned())?;
                    #[cfg(test)]
                    if self
                        .fail_after_final_ledger_before_chat_authority
                        .swap(false, Ordering::AcqRel)
                    {
                        if let Some(barrier) =
                            self.after_final_ledger_error_barrier.lock().unwrap().take()
                        {
                            barrier.wait();
                            barrier.wait();
                        }
                        self.chats.quarantine_storage();
                    }
                    self.chats
                        .set_committed_assistants(&accepted)
                        .map_err(|_| "chat_storage_unavailable".to_owned())?;
                }
                Ok((task, false))
            })();

            if result.is_err() {
                // Readers queue on the Agent commit lock. Fence both stores
                // before releasing it so no reader can observe half a commit.
                self.store.quarantine_storage();
                self.chats.quarantine_storage();
                self.protocol_events_failed.store(true, Ordering::Release);
            }
            result
        });

        match commit {
            Ok(Ok(committed)) => Ok(committed),
            Ok(Err(error)) => Err(error),
            Err(_) => {
                self.store.quarantine_storage();
                self.chats.quarantine_storage();
                self.protocol_events_failed.store(true, Ordering::Release);
                Err("agent_storage_unavailable".into())
            }
        }
    }

    fn native_output_for(&self, owner: &Owner, revision: u64) -> u64 {
        if self.owner.app_id == owner.app_id
            && self.owner.release_digest == owner.release_digest
            && self.revision == revision
        {
            self.native_output.bytes()
        } else {
            0
        }
    }

    fn revoke_lease(&self) {
        if let Ok(lease) = self.lease.lock() {
            if let Some(lease) = lease.as_ref() {
                lease.revoke();
            }
        }
    }

    fn revoke(&self) {
        self.revoke_lease();
        self.cancel.cancel();
    }

    fn report_chat_persisted(&self, result: Result<(), String>) {
        if self.chat_persisted.borrow().is_none() {
            self.chat_persisted.send_replace(Some(result));
        }
    }

    async fn wait_for_chat_commit(&self) -> bool {
        let Some(receiver) = self.chat_commit.lock().await.take() else {
            return self.chat_id.is_none();
        };
        tokio::select! {
            biased;
            _ = self.cancel.cancelled() => false,
            result = receiver => result.is_ok(),
        }
    }
}
#[async_trait::async_trait]
impl PermissionGate for Control {
    async fn has_pending_request(&self) -> bool {
        self.interaction.lock().await.is_some()
    }
    async fn check(&self, name: &str, input: &Utf16JsonProjection) -> PermissionDecision {
        let deny = || PermissionDecision::Deny {
            reason: "The Local App request was not approved.".into(),
        };
        if self.cancel.is_cancelled() {
            return deny();
        }
        if name == STRUCTURED_OUTPUT_TOOL_NAME {
            return PermissionDecision::Allow;
        }
        let Ok(input) = input.try_to_serde_value_with_array_budget(100_000) else {
            return deny();
        };
        let id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        {
            let mut slot = self.interaction.lock().await;
            if slot.is_some() {
                return deny();
            }
            if self
                .store
                .set_phase(&self.owner, &self.id, Some("waiting_for_permission"))
                .is_err()
            {
                return deny();
            }
            *slot = Some((
                id.clone(),
                json!({"type":"permission","requestId":id,"runId":self.request_id,"appId":self.owner.app_id,"toolName":name,"input":input}),
                Reply::Permission(tx),
            ));
        }
        let allow =
            tokio::select! {biased;_=self.cancel.cancelled()=>false,r=rx=>r.unwrap_or(false)};
        let mut slot = self.interaction.lock().await;
        if slot.as_ref().is_some_and(|(current, _, _)| current == &id) {
            slot.take();
        }
        let _ = self.store.set_phase(&self.owner, &self.id, None);
        if allow && !self.cancel.is_cancelled() {
            PermissionDecision::Allow
        } else {
            deny()
        }
    }
}
#[async_trait::async_trait]
impl OutputStream for Control {
    fn wants_model_output_progress(&self) -> bool {
        true
    }
    fn note_model_output_delta(&self, block_index: u32, units: &[u16]) {
        self.native_output.observe(block_index, units);
    }
    fn finish_model_output_block(&self, block_index: u32) {
        self.native_output.finish_block(block_index);
    }
    fn note_model_error_kind(&self, kind: &'static str) {
        if let Ok(mut failure) = self.model_failure.lock() {
            *failure = Some(format!("model_{kind}"));
        }
    }
    async fn emit_text(&self, text: &Utf16JsonProjection) {
        if let Some(text) = text.string_at("") {
            let mut output = self.text.lock().await;
            if output.len() + text.len() > 1024 * 1024 {
                self.model_failed.store(true, Ordering::Release);
                self.revoke();
            } else {
                // Keep the existing run-level bound, but do not publish
                // tentative model text: an accepted fallback may retract it.
                output.push_str(text);
            }
        }
    }
    async fn observe_assistant_display_text(
        &self,
        message_id: &MessageId,
        text: &Utf16JsonProjection,
    ) {
        if let Some(display_text) = text.string_at("") {
            self.assistant_display_text
                .lock()
                .await
                .insert(*message_id, display_text.to_owned());
        } else {
            // An unrepresentable managed display override must never fall back
            // to publishing the unfiltered transcript content.
            self.model_failed.store(true, Ordering::Release);
            self.revoke();
        }
    }
    async fn emit_assistant_message_identity(&self, message_id: &MessageId) {
        self.assistant_message_ids.lock().await.insert(*message_id);
        if let Some(chat_id) = &self.chat_id {
            if self
                .chats
                .record_message(
                    chat_id,
                    ChatMessageMetadata {
                        message_id: message_id.to_string(),
                        run_id: self.id.clone(),
                        role: ChatRole::Assistant,
                        created_at: now_millis(),
                        attachments: Vec::new(),
                    },
                )
                .is_err()
            {
                self.model_failed.store(true, Ordering::Release);
                self.revoke();
            }
        }
    }
    async fn emit_tool_call(&self, _: &ToolUseId, _: &str, _: &Utf16JsonProjection) {}
    async fn emit_tool_result(
        &self,
        _: &ToolUseId,
        _: &str,
        _: &Utf16JsonProjection,
        _: &Utf16JsonProjection,
        _: Option<&Utf16JsonProjection>,
    ) {
    }
    async fn emit_end_turn(&self, _: &str, _: &CostSnapshot) {
        // This callback reports an orchestration terminal outcome, including
        // cancellation and errors, not a confirmed completed model turn.
    }
    async fn emit_terminal_api_error(&self, status: Option<u16>, kind: &str, stop_reason: &str) {
        // Native-only diagnostics: never log provider bodies, prompts or keys,
        // and never expose another host channel through the page result.
        let kind: String = kind
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
            .take(64)
            .collect();
        eprintln!(
            "[local-app-agent] model request failed run={} status={status:?} kind={kind}",
            self.id
        );
        if let Ok(mut failure) = self.model_failure.lock() {
            if failure.is_none() {
                *failure = Some(if stop_reason == "refusal" {
                    "model_refused".into()
                } else {
                    format!("model_request_{}_{}", status.unwrap_or(0), kind)
                });
            }
        }
        self.model_failed.store(true, Ordering::Release);
    }
}
impl AppAgentHost {
    pub fn new(
        service: &Arc<MobileLocalAppServiceHandle>,
        factory: Arc<AppAgentFactory>,
        _config_home: &std::path::Path,
    ) -> Result<Arc<Self>, String> {
        let apps_root = service.library.root().join("apps");
        let mut apps = HashMap::new();
        for record in service
            .library
            .list()
            .map_err(|_| "agent_storage_unavailable")?
        {
            match AppAgentState::open(&apps_root, &record.app_id) {
                Ok(state) => {
                    apps.insert(record.app_id.clone(), Arc::new(state));
                }
                Err(code) => {
                    // Keep other Apps available. A later request for this App
                    // retries opening it and returns the same scoped error.
                    tracing::warn!(app_id = %record.app_id, error_code = %code,
                        "Local App Agent storage is unavailable");
                }
            }
        }
        Ok(Arc::new(Self {
            service: Arc::downgrade(service),
            factory,
            apps_root,
            apps: Mutex::new(apps),
            wake_reservations: Mutex::new(WakeRegistry::default()),
            active: AsyncMutex::new(HashMap::new()),
            admission: AsyncMutex::new(()),
            shutdown: AtomicBool::new(false),
            #[cfg(test)]
            force_wake_policy: AtomicBool::new(false),
            #[cfg(test)]
            before_started_event: Mutex::new(None),
            #[cfg(test)]
            fail_started_event_schema: AtomicBool::new(false),
            #[cfg(test)]
            duplicate_chat_wait_started: Mutex::new(None),
            #[cfg(test)]
            duplicate_pre_admission_pause: Mutex::new(None),
            #[cfg(test)]
            duplicate_probe_none_barrier: Mutex::new(None),
            #[cfg(test)]
            chat_executor_init_pause: Mutex::new(None),
            #[cfg(test)]
            duplicate_task_snapshot_pause: Mutex::new(None),
        }))
    }
    fn app_state(&self, app: &str) -> Result<Arc<AppAgentState>, String> {
        let mut apps = self.apps.lock().map_err(|_| "agent_storage_unavailable")?;
        if let Some(state) = apps.get(app) {
            return Ok(state.clone());
        }
        let state = Arc::new(AppAgentState::open(&self.apps_root, app)?);
        apps.insert(app.to_owned(), state.clone());
        Ok(state)
    }
    async fn service(&self) -> Result<Arc<MobileLocalAppServiceHandle>, String> {
        self.service
            .upgrade()
            .ok_or_else(|| "app_service_unavailable".into())
    }
    async fn owner(
        &self,
        app: &str,
        invocation: &str,
    ) -> Result<(Owner, u64, Vec<String>, Vec<String>), String> {
        let service = self.service().await?;
        let status = service
            .request_value(json!({"op":"status","appId":app}))
            .await
            .map_err(|_| "app_not_running")?;
        let run = &status["run"];
        if run["phase"] != "running" || run["runId"] != invocation {
            return Err("stale_app_invocation".into());
        }
        let release = service
            .request_value(json!({"op":"getRelease","appId":app,"revision":run["revision"]}))
            .await
            .map_err(|_| "release_unavailable")?;
        if !release["manifest"]["capabilities"]
            .as_array()
            .is_some_and(|values| values.contains(&json!("agent")))
        {
            return Err("agent_not_declared".into());
        }
        let owner = Owner {
            app_id: app.into(),
            release_digest: run["releaseDigest"]
                .as_str()
                .ok_or("invalid_release")?
                .into(),
            invocation_id: invocation.into(),
        };
        owner.validate().map_err(|_| "invalid_owner")?;
        if release["digest"] != owner.release_digest {
            return Err("release_changed".into());
        }
        let revision = run["revision"].as_u64().ok_or("invalid_release")?;
        let tools = release["manifest"]["agent"]["tools"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let skills = release["manifest"]["agent"]["skills"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item["id"].as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        Ok((owner, revision, tools, skills))
    }
    async fn current_release(
        &self,
        app: &str,
        revision: u64,
        invocation: &str,
    ) -> Result<(Owner, Vec<String>, Vec<String>), String> {
        let service = self.service().await?;
        let record = service
            .request_value(json!({"op":"get","appId":app}))
            .await
            .map_err(|_| "app_not_found")?;
        if record["removed"] == true || record["currentRevision"].as_u64() != Some(revision) {
            return Err("release_changed".into());
        }
        let release = service
            .request_value(json!({"op":"getRelease","appId":app,"revision":revision}))
            .await
            .map_err(|_| "release_unavailable")?;
        if !release["manifest"]["capabilities"]
            .as_array()
            .is_some_and(|values| values.contains(&json!("agent")))
        {
            return Err("agent_not_declared".into());
        }
        let owner = Owner {
            app_id: app.into(),
            release_digest: release["digest"].as_str().ok_or("invalid_release")?.into(),
            invocation_id: invocation.into(),
        };
        owner.validate().map_err(|_| "invalid_owner")?;
        let tools = release["manifest"]["agent"]["tools"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let skills = release["manifest"]["agent"]["skills"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item["id"].as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        Ok((owner, tools, skills))
    }
    async fn duplicate_before_normalization(
        &self,
        state: &AppAgentState,
        owner: &Owner,
        input: &Value,
        required_chat: Option<&str>,
        owner_cancel: &CancellationToken,
        detached: bool,
    ) -> Result<Option<String>, String> {
        #[cfg(test)]
        let test_pause = self.duplicate_pre_admission_pause.lock().unwrap().take();
        #[cfg(test)]
        if let Some((ready, release)) = test_pause {
            let _ = ready.send(());
            let _ = release.await;
        }
        let serial = self.admission.lock().await;
        if self.shutdown.load(Ordering::Acquire) {
            return Err("agent_host_replaced".into());
        }
        if !detached && owner_cancel.is_cancelled() {
            return Err("app_invocation_closed".into());
        }
        self.validate_current_release(owner).await?;
        if state
            .store
            .settings(owner)
            .map_err(|_| "agent_storage_unavailable")?
            .is_none()
        {
            return Err("agent_not_authorized".into());
        }

        let duplicate = protocol_duplicate_run(state, owner, input, required_chat)?;
        let Some(run_id) = duplicate else {
            drop(serial);
            #[cfg(test)]
            let barrier = { self.duplicate_probe_none_barrier.lock().unwrap().clone() };
            #[cfg(test)]
            if let Some(barrier) = barrier {
                barrier.wait().await;
            }
            return Ok(None);
        };
        let mut persisted = None;
        if let Some(chat_id) = state
            .store
            .protocol_chat_id_for_request(owner, &run_id)
            .map_err(|_| "agent_storage_unavailable")?
        {
            let task_id = state
                .store
                .task_id_for_request(owner, &run_id)
                .map_err(|_| "agent_storage_unavailable")?;
            let _task = state
                .store
                .get(owner, &task_id)
                .map_err(|_| "agent_storage_unavailable")?;
            #[cfg(test)]
            let snapshot_pause = { self.duplicate_task_snapshot_pause.lock().unwrap().take() };
            #[cfg(test)]
            if let Some((ready, release)) = snapshot_pause {
                let _ = ready.send(());
                let _ = release.await;
            }
            let active = self.active.lock().await.get(&task_id).cloned();
            if let Some(control) = active {
                if control.owner.app_id != owner.app_id
                    || control.owner.release_digest != owner.release_digest
                    || control.id != task_id
                    || control.request_id != run_id
                    || control.chat_id.as_deref() != Some(chat_id.as_str())
                {
                    return Err("chat_storage_unavailable".into());
                }
                persisted = Some((control.chat_persisted.subscribe(), control));
                #[cfg(test)]
                if let Some(ready) = self.duplicate_chat_wait_started.lock().unwrap().take() {
                    let _ = ready.send(());
                }
            } else {
                let task = state
                    .store
                    .get(owner, &task_id)
                    .map_err(|_| "agent_storage_unavailable")?;
                if task.state != State::Succeeded {
                    return Err("chat_storage_unavailable".into());
                }
                state
                    .store
                    .successful_chat_history_task(&owner.app_id, &chat_id, &task_id)
                    .map_err(|_| "chat_storage_unavailable")?;
            }
        }

        // Admission stays serialized through identity and authorization checks,
        // but a duplicate must not hold this lock while waiting for the original
        // executor's transcript durability handshake.
        drop(serial);
        if let Some((mut persisted, control)) = persisted {
            #[cfg(test)]
            if let Some(ready) = self.duplicate_chat_wait_started.lock().unwrap().take() {
                let _ = ready.send(());
            }
            wait_for_duplicate_chat_persistence(&mut persisted, &control, owner_cancel, detached)
                .await?;
        }
        if !detached && owner_cancel.is_cancelled() {
            return Err("app_invocation_closed".into());
        }
        Ok(Some(run_id))
    }
    async fn validate_current_release(&self, owner: &Owner) -> Result<(), String> {
        let service = self.service().await?;
        let record = service
            .request_value(json!({"op":"get","appId":owner.app_id}))
            .await
            .map_err(|_| "app_not_found")?;
        if record["removed"] == true {
            return Err("release_changed".into());
        }
        let revision = record["currentRevision"]
            .as_u64()
            .ok_or("invalid_release")?;
        let release = service
            .request_value(json!({"op":"getRelease","appId":owner.app_id,"revision":revision}))
            .await
            .map_err(|_| "release_unavailable")?;
        if release["digest"].as_str() != Some(owner.release_digest.as_str())
            || !release["manifest"]["capabilities"]
                .as_array()
                .is_some_and(|values| values.contains(&json!("agent")))
        {
            return Err("release_changed".into());
        }
        Ok(())
    }
    /// The native frame/origin broker supplies app and invocation; page input
    /// may contain only the method's narrow arguments.
    pub async fn page(
        self: &Arc<Self>,
        app: &str,
        invocation: &str,
        method: &str,
        args: Value,
    ) -> Result<Value, String> {
        let (owner, revision, declared, declared_skills) = self.owner(app, invocation).await?;
        match method {
            "agent.tasks.start" => {
                reject_unknown(&args, &["input"])?;
                let state = self.app_state(app)?;
                let raw_input = args.get("input").cloned().ok_or("invalid_agent_input")?;
                if state
                    .store
                    .settings(&owner)
                    .map_err(|_| "agent_storage_unavailable")?
                    .is_none()
                {
                    return Err("agent_not_authorized".into());
                }
                let owner_cancel = self
                    .service()
                    .await?
                    .agent_invocation_token(app, invocation)
                    .await?;
                if let Some(run_id) = self
                    .duplicate_before_normalization(
                        &state,
                        &owner,
                        &raw_input,
                        None,
                        &owner_cancel,
                        false,
                    )
                    .await?
                {
                    return Ok(json!({"runId":run_id}));
                }
                let (normalized, chat_id) = normalize_page_run_input(&state, raw_input, None)?;
                let duplicate_only = normalized.message_already_persisted;
                let public_run_id = normalized.run_id;
                let input = normalized.input;
                let user_message = normalized.user_message;
                let allow_new = !duplicate_only
                    && (!cfg!(target_os = "ios")
                        || self.has_wake_reservation(app, revision, &input.request_id));
                let started = self
                    .start_task(
                        owner,
                        revision,
                        &declared,
                        &declared_skills,
                        input,
                        Some(user_message),
                        chat_id,
                        owner_cancel,
                        allow_new,
                        false,
                    )
                    .await;
                match started {
                    Ok(_) => Ok(json!({"runId":public_run_id})),
                    Err(error) if duplicate_only && error == "host_wake_required" => {
                        Err("agui_message_already_submitted".into())
                    }
                    Err(error) => Err(error),
                }
            }
            "agent.tasks.get" => {
                let id = only_run_id(&args)?;
                let state = self.app_state(app)?;
                let task = state
                    .store
                    .get_by_request(&owner, id)
                    .map_err(task_access_error)?;
                let thread_id = state
                    .store
                    .thread_id_for_request(&owner, id)
                    .map_err(task_access_error)?;
                public_task_value(&task, thread_id)
            }
            "agent.tasks.list" => {
                reject_unknown(&args, &["cursor", "limit"])?;
                let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(50) as usize;
                let cursor = args.get("cursor").and_then(Value::as_str);
                let state = self.app_state(app)?;
                task_inventory_page(&state, &owner, cursor, limit)
            }
            "agent.tasks.cancel" => {
                let id = only_run_id(&args)?;
                let state = self.app_state(app)?;
                let task = state
                    .store
                    .get_by_request(&owner, id)
                    .map_err(task_access_error)?;
                state
                    .store
                    .request_cancel(&owner, &task.run_id)
                    .map_err(task_access_error)?;
                self.cancel_ids(&[task.run_id]).await;
                Ok(Value::Null)
            }
            "agent.tasks.events" => {
                reject_unknown(&args, &["runId", "afterSequence"])?;
                let run_id = required_string(&args, "runId")?;
                validate_request_id(run_id)?;
                let after_sequence = args
                    .get("afterSequence")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let state = self.app_state(app)?;
                let page = state
                    .store
                    .protocol_events(&owner, run_id, after_sequence)
                    .map_err(|_| "agent_events_unavailable")?;
                serde_json::to_value(page).map_err(|_| "agent_events_unavailable".into())
            }
            "agent.chats.create" | "agent.chats.creationStatus" => {
                let state = self.app_state(app)?;
                chat_creation_value(&state.chats, &args, method == "agent.chats.creationStatus")
            }
            "agent.chats.get" => {
                let chat_id = required_string(&args, "chatId")?;
                let state = self.app_state(app)?;
                serde_json::to_value(state.chats.get(chat_id).map_err(|_| "chat_not_found")?)
                    .map_err(|_| "invalid_chat".into())
            }
            "agent.chats.list" => {
                reject_unknown(&args, &["cursor", "limit", "includeArchived"])?;
                let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(50) as usize;
                let cursor = args.get("cursor").and_then(Value::as_str);
                let include_archived = args
                    .get("includeArchived")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let state = self.app_state(app)?;
                chat_inventory_page(&state, cursor, limit, include_archived)
            }
            "agent.chats.archive" | "agent.chats.delete" => {
                let chat_id = required_string(&args, "chatId")?;
                let state = self.app_state(app)?;
                if method == "agent.chats.archive" {
                    state
                        .chats
                        .archive(chat_id)
                        .map_err(|_| "chat_storage_unavailable")?;
                } else {
                    state
                        .chats
                        .delete(chat_id)
                        .map_err(|_| "chat_storage_unavailable")?;
                }
                Ok(Value::Null)
            }
            "agent.chats.history" => {
                reject_unknown(&args, &["chatId", "cursor", "limit"])?;
                let chat_id = required_string(&args, "chatId")?;
                let cursor = args.get("cursor").and_then(Value::as_str);
                let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(50) as usize;
                let state = self.app_state(app)?;
                let history = state
                    .chats
                    .protocol_history_page(chat_id, cursor, limit)
                    .map_err(|_| "invalid_chat_history")?;
                serde_json::to_value(history).map_err(|_| "invalid_chat_history".into())
            }
            "agent.chats.send" => {
                reject_unknown(&args, &["chatId", "input"])?;
                let chat_id = required_string(&args, "chatId")?.to_owned();
                let state = self.app_state(app)?;
                let raw_input = args.get("input").cloned().ok_or("invalid_agent_input")?;
                if state
                    .store
                    .settings(&owner)
                    .map_err(|_| "agent_storage_unavailable")?
                    .is_none()
                {
                    return Err("agent_not_authorized".into());
                }
                let owner_cancel = self
                    .service()
                    .await?
                    .agent_invocation_token(app, invocation)
                    .await?;
                if let Some(run_id) = self
                    .duplicate_before_normalization(
                        &state,
                        &owner,
                        &raw_input,
                        Some(&chat_id),
                        &owner_cancel,
                        false,
                    )
                    .await?
                {
                    return Ok(json!({"runId":run_id}));
                }
                let (normalized, chat_id) =
                    normalize_page_run_input(&state, raw_input, Some(&chat_id))?;
                let duplicate_only = normalized.message_already_persisted;
                let public_run_id = normalized.run_id;
                let input = normalized.input;
                let user_message = normalized.user_message;
                let allow_new = !duplicate_only
                    && (!cfg!(target_os = "ios")
                        || self.has_wake_reservation(app, revision, &input.request_id));
                let started = self
                    .start_task(
                        owner,
                        revision,
                        &declared,
                        &declared_skills,
                        input,
                        Some(user_message),
                        chat_id,
                        owner_cancel,
                        allow_new,
                        false,
                    )
                    .await;
                match started {
                    Ok(_) => Ok(json!({"runId":public_run_id})),
                    Err(error) if duplicate_only && error == "host_wake_required" => {
                        Err("agui_message_already_submitted".into())
                    }
                    Err(error) => Err(error),
                }
            }
            "agent.chats.cancel" => {
                let chat_id = required_string(&args, "chatId")?;
                let state = self.app_state(app)?;
                let run_id = state
                    .chats
                    .active_run(chat_id)
                    .map_err(|_| "chat_not_found")?;
                if let Some(run_id) = run_id {
                    state
                        .store
                        .request_cancel(&owner, &run_id)
                        .map_err(task_access_error)?;
                    self.cancel_ids(&[run_id]).await;
                }
                Ok(Value::Null)
            }
            _ => Err("unsupported_agent_method".into()),
        }
    }
    async fn start_task(
        self: &Arc<Self>,
        owner: Owner,
        revision: u64,
        declared_tools: &[String],
        declared_skills: &[String],
        input: Input,
        protocol_user_message: Option<Value>,
        chat_id: Option<String>,
        owner_cancel: CancellationToken,
        allow_new: bool,
        native_detached: bool,
    ) -> Result<String, String> {
        let _serial = self.admission.lock().await;
        if self.shutdown.load(Ordering::Acquire) {
            return Err("agent_host_replaced".into());
        }
        // Stop cancels the page invocation before taking this admission lock.
        // A request queued before Stop cannot consume a later wake afterward.
        if !native_detached && owner_cancel.is_cancelled() {
            return Err("app_invocation_closed".into());
        }
        let state = self.app_state(&owner.app_id)?;
        if let Some(chat_id) = chat_id.as_deref() {
            let summary = match state.chats.get(chat_id) {
                Ok(summary) => summary,
                Err(local_app::v2::AppError::NotFound(_)) => return Err("chat_not_found".into()),
                Err(_) => return Err("chat_storage_unavailable".into()),
            };
            if summary.state == ChatState::Interrupted {
                return Err("agui_history_unavailable".into());
            }
            if summary.archived {
                return Err("chat_archived".into());
            }
        }
        validate_chat_output_mode(chat_id.as_deref(), &input)?;
        // Validate the exact generated event and initialize the pinned schema
        // before persisting a run or claiming a wake. Schema initialization
        // failure is a host availability problem, not a damaged run log.
        let started_event = if let Some(thread_id) = &input.protocol_thread_id {
            let mut event = json!({
                "type":"RUN_STARTED", "protocolVersion":"1.0",
                "threadId":thread_id, "runId":input.request_id
            });
            if let Some(parent_run_id) = &input.protocol_parent_run_id {
                event["parentRunId"] = json!(parent_run_id);
            }
            #[cfg(test)]
            if self.fail_started_event_schema.load(Ordering::Acquire) {
                return Err("agent_storage_unavailable".into());
            }
            local_app::v2::agent::protocol::validate_event(&event).map_err(|error| {
                if error.code == "protocol_schema_unavailable" {
                    "agent_storage_unavailable"
                } else {
                    "invalid_agent_input"
                }
            })?;
            Some(event)
        } else {
            None
        };
        #[cfg(test)]
        let is_ios = cfg!(target_os = "ios") || self.force_wake_policy.load(Ordering::Acquire);
        #[cfg(not(test))]
        let is_ios = cfg!(target_os = "ios");
        let wake_required = is_ios && !native_detached;
        let claimed_wake =
            wake_required && self.claim_wake(&owner.app_id, revision, &input.request_id);
        let allow_new = allow_new && (!wake_required || claimed_wake);
        let history_digest = input.protocol_history_digest.clone();
        let admission_chats = state.chats.clone();
        let admission_chat_id = chat_id.clone();
        let admitted = match state.store.admit_scoped_with_check(
            &owner,
            &input,
            declared_tools,
            declared_skills,
            chat_id.as_deref(),
            allow_new,
            move || {
                if let Some(chat_id) = admission_chat_id.as_deref() {
                    let expected = history_digest.as_deref().ok_or_else(|| {
                        local_app::v2::AppError::Invalid(
                            "AG-UI Chat history snapshot is missing".into(),
                        )
                    })?;
                    let current = admission_chats.protocol_history_digests(chat_id)?;
                    if local_app::v2::agent::protocol::history_fingerprint(&current) != expected {
                        return Err(local_app::v2::AppError::Invalid(
                            "AG-UI Chat history changed during Agent admission".into(),
                        ));
                    }
                }
                Ok(())
            },
        ) {
            Ok(admitted) => admitted,
            Err(error) => {
                if claimed_wake {
                    self.unclaim_wake(&owner.app_id, revision, &input.request_id);
                }
                return Err(local_app::v2::agent::admission_error_code(&error).into());
            }
        };
        let id = admitted.task.run_id.clone();
        if !admitted.start {
            if claimed_wake {
                self.unclaim_wake(&owner.app_id, revision, &input.request_id);
            }
            // The first duplicate probe runs before normalization. A caller
            // can still have normalized while another admission was pending,
            // so validate the complete durable initialization here too.
            // This prevents an interrupted/rolled-back first initializer from
            // being acknowledged as a successful duplicate.
            if let Some(thread_id) = input.protocol_thread_id.as_deref() {
                validate_duplicate_initialization(
                    &state,
                    &owner,
                    &input,
                    protocol_user_message.as_ref(),
                    chat_id.as_deref(),
                    thread_id,
                    &id,
                )?;
            }
            let task = state
                .store
                .get(&owner, &id)
                .map_err(|_| "agent_storage_unavailable")?;
            #[cfg(test)]
            let snapshot_pause = { self.duplicate_task_snapshot_pause.lock().unwrap().take() };
            #[cfg(test)]
            if let Some((ready, release)) = snapshot_pause {
                let _ = ready.send(());
                let _ = release.await;
            }
            let mut persisted = None;
            if let Some(chat_id) = chat_id.as_deref() {
                let control = { self.active.lock().await.get(&id).cloned() };
                if let Some(control) = control {
                    if control.owner.app_id != owner.app_id
                        || control.owner.release_digest != owner.release_digest
                        || control.id != id
                        || control.request_id != input.request_id
                        || control.protocol_thread_id.as_deref()
                            != input.protocol_thread_id.as_deref()
                        || control.chat_id.as_deref() != Some(chat_id)
                    {
                        return Err("chat_storage_unavailable".into());
                    }
                    persisted = Some((control.chat_persisted.subscribe(), control));
                    #[cfg(test)]
                    if let Some(ready) = self.duplicate_chat_wait_started.lock().unwrap().take() {
                        let _ = ready.send(());
                    }
                } else {
                    let task = state
                        .store
                        .get(&owner, &id)
                        .map_err(|_| "agent_storage_unavailable")?;
                    // A terminal run can outlive its Control, but a queued or
                    // running task without an owner cannot prove that Chat
                    // initialization reached the executor handshake.
                    if task.state != State::Succeeded {
                        return Err("chat_storage_unavailable".into());
                    }
                    state
                        .store
                        .successful_chat_history_task(&owner.app_id, chat_id, &id)
                        .map_err(|_| "chat_storage_unavailable")?;
                }
            } else if !task.state.terminal() {
                return Err("task_unavailable".into());
            }
            // Never retain admission while waiting for the original writer.
            // wake.clear, revoke and stop paths all need this same lock.
            drop(_serial);
            if let Some((mut persisted, control)) = persisted {
                wait_for_duplicate_chat_persistence(
                    &mut persisted,
                    &control,
                    &owner_cancel,
                    native_detached,
                )
                .await?;
            }
            return Ok(id);
        }

        let mut attachment_metadata = input
            .attachments
            .iter()
            .map(|attachment| AttachmentMetadata {
                name: attachment.name.clone(),
                media_type: attachment.media_type.clone(),
                size: decoded_size(&attachment.base64),
            })
            .collect::<Vec<_>>();
        attachment_metadata.extend(input.content_parts.iter().filter_map(|part| match part {
            local_app::v2::agent::InputContentPart::Attachment {
                name,
                media_type,
                base64,
            } => Some(AttachmentMetadata {
                name: name.clone(),
                media_type: media_type.clone(),
                size: decoded_size(base64),
            }),
            local_app::v2::agent::InputContentPart::Text { .. } => None,
        }));
        let user_message_id = if let Some(chat_id) = chat_id.as_deref() {
            if state
                .chats
                .get(chat_id)
                .is_ok_and(|summary| summary.state == ChatState::Running)
            {
                if claimed_wake {
                    self.unclaim_wake(&owner.app_id, revision, &input.request_id);
                }
                let _ = state.store.rollback_queued_admission(&owner, &id);
                return Err("busy".into());
            }
            if let Err(error) = state.chats.start_turn(chat_id, &id) {
                if claimed_wake {
                    self.unclaim_wake(&owner.app_id, revision, &input.request_id);
                }
                let _ = state.store.rollback_queued_admission(&owner, &id);
                return Err(if matches!(error, local_app::v2::AppError::Busy(_)) {
                    "busy"
                } else {
                    "chat_storage_unavailable"
                }
                .into());
            }
            let run_uuid = uuid::Uuid::parse_str(&id).map_err(|_| "invalid_run")?;
            let message_id = lingxi_core::types::MessageId::from_uuid(run_uuid).to_string();
            if state
                .chats
                .record_message(
                    chat_id,
                    ChatMessageMetadata {
                        message_id: message_id.clone(),
                        run_id: id.clone(),
                        role: ChatRole::User,
                        created_at: now_millis(),
                        attachments: attachment_metadata.clone(),
                    },
                )
                .is_err()
            {
                if claimed_wake {
                    self.unclaim_wake(&owner.app_id, revision, &input.request_id);
                }
                let _ = state.chats.finish_turn(chat_id, &id);
                let _ = state.store.rollback_queued_admission(&owner, &id);
                return Err("chat_storage_unavailable".into());
            }
            if let Some(message) = protocol_user_message.as_ref() {
                if state
                    .chats
                    .record_protocol_message(chat_id, &id, message)
                    .is_err()
                {
                    if claimed_wake {
                        self.unclaim_wake(&owner.app_id, revision, &input.request_id);
                    }
                    let _ = state.chats.finish_turn(chat_id, &id);
                    let _ = state
                        .chats
                        .rollback_message_metadata(chat_id, &message_id, &id);
                    let _ = state.chats.rollback_protocol_message(
                        chat_id,
                        &id,
                        message["id"].as_str().unwrap_or_default(),
                    );
                    let _ = state.store.rollback_queued_admission(&owner, &id);
                    return Err("chat_storage_unavailable".into());
                }
            }
            let _ = state.chats.set_initial_title(chat_id, &input.prompt);
            Some(message_id)
        } else {
            None
        };

        let wake_detached = if claimed_wake {
            self.consume_wake(&owner.app_id, revision, &input.request_id, &id)
        } else {
            false
        };
        if wake_required && !wake_detached {
            if let Some(chat_id) = &chat_id {
                if let Some(message_id) = &user_message_id {
                    let _ = state
                        .chats
                        .rollback_message_metadata(chat_id, message_id, &id);
                }
                if let Some(message) = protocol_user_message.as_ref() {
                    let _ = state.chats.rollback_protocol_message(
                        chat_id,
                        &id,
                        message["id"].as_str().unwrap_or_default(),
                    );
                }
                let _ = state.chats.finish_turn(chat_id, &id);
            }
            let _ = state.store.rollback_queued_admission(&owner, &id);
            return Err("host_wake_required".into());
        }
        let cancel = if native_detached || wake_detached {
            CancellationToken::new()
        } else {
            owner_cancel.child_token()
        };
        let is_chat = chat_id.is_some();
        let detached = native_detached || wake_detached;
        let (chat_persisted_tx, mut chat_persisted_rx) = watch::channel(None);
        let (chat_commit_tx, chat_commit_rx) = oneshot::channel::<()>();
        let chat_commit_tx = is_chat.then_some(chat_commit_tx);
        let control = Arc::new(Control {
            owner: owner.clone(),
            revision,
            request_id: admitted.task.request_id.clone(),
            id: id.clone(),
            protocol_thread_id: input.protocol_thread_id.clone(),
            cancel,
            finished: AtomicBool::new(false),
            done: Notify::new(),
            lease: Mutex::new(None),
            interaction: AsyncMutex::new(None),
            store: state.store.clone(),
            chats: state.chats.clone(),
            chat_id,
            user_message_id,
            chat_orchestrator: Mutex::new(None),
            chat_persisted: chat_persisted_tx,
            chat_commit: AsyncMutex::new(if is_chat { Some(chat_commit_rx) } else { None }),
            attachment_metadata,
            assistant_message_ids: AsyncMutex::new(HashSet::new()),
            assistant_display_text: AsyncMutex::new(HashMap::new()),
            protocol_events_failed: AtomicBool::new(false),
            native_output: NativeOutputProgress::default(),
            text: AsyncMutex::new(String::new()),
            model_failed: AtomicBool::new(false),
            model_failure: Mutex::new(None),
            timed_out: AtomicBool::new(false),
            usage: AsyncMutex::new(None),
            #[cfg(test)]
            fail_after_final_ledger_before_chat_authority: AtomicBool::new(false),
            #[cfg(test)]
            after_final_ledger_error_barrier: Mutex::new(None),
        });
        if let Some(started) = started_event {
            #[cfg(test)]
            {
                let hook = self.before_started_event.lock().unwrap().take();
                if let Some(hook) = hook {
                    hook(&state.root, &id);
                }
            }
            if state.store.append_event(&owner, &id, started).is_err() {
                if let Some(chat_id) = &control.chat_id {
                    if let Some(message_id) = &control.user_message_id {
                        let _ = control
                            .chats
                            .rollback_message_metadata(chat_id, message_id, &id);
                    }
                    if let Some(message) = protocol_user_message.as_ref() {
                        let _ = control.chats.rollback_protocol_message(
                            chat_id,
                            &id,
                            message["id"].as_str().unwrap_or_default(),
                        );
                    }
                    let _ = control.chats.finish_turn(chat_id, &id);
                }
                // Do not roll back first: an event append or sync failure may
                // have left a partial or complete RUN_STARTED on disk. Mark
                // this exact unstarted run interrupted without touching those
                // bytes, so the request can never be replayed automatically.
                if state
                    .store
                    .interrupt_unstarted_protocol_admission(&owner, &id)
                    .is_err()
                {
                    // The queued record remains admission-blocking when the
                    // interrupted state cannot be durably committed.
                    tracing::warn!(run_id = %id, "could not persist interrupted Agent admission");
                }
                if wake_detached {
                    self.clear_wake_run(&id);
                }
                return Err("agent_storage_unavailable".into());
            }
        }
        self.active.lock().await.insert(id.clone(), control.clone());
        drop(_serial);
        let host = self.clone();
        let app_root = state.root.clone();
        let execute_control = control.clone();
        tokio::spawn(async move {
            #[cfg(test)]
            let init_pause = { host.chat_executor_init_pause.lock().unwrap().take() };
            #[cfg(test)]
            if let Some((started, release)) = init_pause {
                let _ = started.send(());
                tokio::select! {
                    _ = execute_control.cancel.cancelled() => {},
                    _ = release => {},
                }
            }
            host.execute(execute_control, admitted, input, app_root)
                .await;
        });
        if is_chat {
            if let Err(error) =
                wait_for_chat_persistence(&mut chat_persisted_rx, &owner_cancel, detached).await
            {
                control.revoke();
                drop(chat_commit_tx);
                super::lifecycle::wait_for_completion(&control.done, &control.finished).await;
                return Err(error);
            }
            if !detached && owner_cancel.is_cancelled() {
                control.revoke();
                drop(chat_commit_tx);
                super::lifecycle::wait_for_completion(&control.done, &control.finished).await;
                return Err("cancelled".into());
            }
            let commit_sent = match chat_commit_tx {
                Some(sender) => sender.send(()).is_ok(),
                None => false,
            };
            if !commit_sent {
                control.revoke();
                super::lifecycle::wait_for_completion(&control.done, &control.finished).await;
                return Err("task_unavailable".into());
            }
        }
        Ok(id)
    }

    fn has_wake_reservation(&self, app: &str, revision: u64, request_id: &str) -> bool {
        let key = WakeKey {
            app_id: app.to_owned(),
            revision,
            request_id: request_id.to_owned(),
        };
        let Ok(mut registry) = self.wake_reservations.lock() else {
            return false;
        };
        registry.available(&key, now_millis())
    }

    fn claim_wake(&self, app: &str, revision: u64, request_id: &str) -> bool {
        let key = WakeKey {
            app_id: app.to_owned(),
            revision,
            request_id: request_id.to_owned(),
        };
        let Ok(mut registry) = self.wake_reservations.lock() else {
            return false;
        };
        registry.claim(&key, now_millis())
    }

    fn unclaim_wake(&self, app: &str, revision: u64, request_id: &str) {
        let key = WakeKey {
            app_id: app.to_owned(),
            revision,
            request_id: request_id.to_owned(),
        };
        if let Ok(mut registry) = self.wake_reservations.lock() {
            registry.unclaim(&key);
        }
    }

    fn consume_wake(&self, app: &str, revision: u64, request_id: &str, run_id: &str) -> bool {
        let key = WakeKey {
            app_id: app.to_owned(),
            revision,
            request_id: request_id.to_owned(),
        };
        let Ok(mut registry) = self.wake_reservations.lock() else {
            return false;
        };
        registry.consume(&key, now_millis(), run_id)
    }

    fn clear_wake_run(&self, run_id: &str) {
        if let Ok(mut registry) = self.wake_reservations.lock() {
            registry.clear_run(run_id);
        }
    }

    async fn load_skill_prompt(
        &self,
        owner: &Owner,
        selected: &[String],
        workspace: &Path,
    ) -> Result<Option<String>, String> {
        if selected.is_empty() {
            return Ok(None);
        }
        self.validate_current_release(owner).await?;
        let service = self.service().await?;
        let record = service
            .request_value(json!({"op":"get","appId":owner.app_id}))
            .await
            .map_err(|_| "app_not_found")?;
        let revision = record["currentRevision"]
            .as_u64()
            .ok_or("invalid_release")?;
        let release_value = service
            .request_value(json!({"op":"getRelease","appId":owner.app_id,"revision":revision}))
            .await
            .map_err(|_| "release_unavailable")?;
        let release: local_app::v2::AppRelease =
            serde_json::from_value(release_value).map_err(|_| "release_unavailable")?;
        if release.digest != owner.release_digest {
            return Err("release_changed".into());
        }
        let source = service
            .library
            .root()
            .join("apps")
            .join(&owner.app_id)
            .join("releases")
            .join(revision.to_string())
            .join("source");
        let skills = release
            .manifest
            .agent
            .as_ref()
            .map(|agent| agent.skills.as_slice())
            .ok_or("skill_not_declared")?;
        crate::app_agent_skills::materialize(&source, workspace, skills, &release.files, selected)
            .await
    }

    async fn execute(
        self: Arc<Self>,
        control: Arc<Control>,
        admission: Admission,
        input: Input,
        app_root: PathBuf,
    ) {
        let id = control.id.clone();
        let owner = control.owner.clone();
        let cwd = app_root.join("tasks").join(&id);
        let timer_control = control.clone();
        let timeout = admission.task.limits.timeout_ms;
        let timer = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(timeout)).await;
            timer_control.timed_out.store(true, Ordering::Release);
            timer_control.revoke();
        });
        let result = self
            .execute_inner(&control, &admission, input, cwd.clone())
            .await;
        timer.abort();
        let result = if control.timed_out.load(Ordering::Acquire)
            && result
                .as_ref()
                .err()
                .is_none_or(|e| e != "tool_cleanup_incomplete")
        {
            Err("timeout_exceeded".into())
        } else {
            result
        };
        let mut result = if control.protocol_events_failed.load(Ordering::Acquire) {
            Err("agent_storage_unavailable".into())
        } else {
            result
        };
        if let Ok((task_result, _)) = &result {
            if let Err(error) = validate_task_result(task_result) {
                result = Err(error);
            }
        }
        control.native_output.finish_all();
        let observed_bytes = control.native_output.bytes();
        if observed_bytes > 0
            && control
                .store
                .record_native_output_bytes(&owner, &id, control.revision, observed_bytes)
                .is_err()
            && result
                .as_ref()
                .err()
                .is_none_or(|error| error != "tool_cleanup_incomplete")
        {
            result = Err("agent_storage_unavailable".into());
        }
        if result.is_ok() && control.cancel.is_cancelled() {
            result = Err("cancelled".into());
        }
        control.report_chat_persisted(result.as_ref().map(|_| ()).map_err(|error| error.clone()));
        // End tool authority immediately, while retaining the external cancel
        // signal for the finalization guard's last cancellation check.
        control.revoke_lease();
        control.interaction.lock().await.take();
        if control.chat_id.is_some()
            && result
                .as_ref()
                .err()
                .is_none_or(|error| error != "tool_cleanup_incomplete")
        {
            let is_success = result.is_ok();
            let checkpoint_result: Result<Option<ChatTranscriptCheckpoint>, String> = async {
                let chat_id = control
                    .chat_id
                    .as_deref()
                    .ok_or("chat_transcript_unavailable")?;
                let orchestrator = control
                    .chat_orchestrator
                    .lock()
                    .map_err(|_| "chat_transcript_unavailable")?
                    .clone();
                let Some(orchestrator) = orchestrator else {
                    return if is_success {
                        Err("chat_transcript_unavailable".into())
                    } else {
                        Ok(None)
                    };
                };
                // Tool and executor cleanup has completed at this point. The
                // orchestrator captures and seals one exact writer revision
                // under its turn and append gates; successful output is
                // accepted only from a replay of those same bytes.
                let captured = match orchestrator.verify_persistent_chat_checkpoint(true).await {
                    Ok(captured) => captured,
                    Err(_) if !is_success => return Ok(None),
                    Err(_) => return Err("chat_transcript_unavailable".into()),
                };
                if !is_success {
                    // Failed/cancelled turns remain sealed but have no
                    // resumable transcript authority. Their original terminal
                    // reason is preserved while the ledger blocks later turns.
                    return Ok(None);
                }
                let transcript_path = control
                    .chats
                    .transcript_path(chat_id)
                    .map_err(|_| "chat_storage_unavailable")?;
                let session_id = control
                    .chats
                    .session_id(chat_id)
                    .map_err(|_| "chat_storage_unavailable")?;
                let session_id = uuid::Uuid::parse_str(&session_id)
                    .map_err(|_| "chat_transcript_unavailable")?;
                if captured.path() != transcript_path.as_path()
                    || captured.session_id() != session_id
                {
                    return Err("chat_transcript_unavailable".into());
                }
                let (task_result, _) =
                    result.as_ref().map_err(|_| "chat_transcript_unavailable")?;
                let replayed =
                    orchestrator::ConversationOrchestrator::replay_persistent_chat_snapshot(
                        &captured,
                    )
                    .map_err(|_| "chat_transcript_unavailable")?;
                super::validate_local_app_chat_resume(&replayed)?;
                if task_result.chat_id.as_deref() != Some(chat_id) {
                    return Err("chat_transcript_unavailable".into());
                }
                validate_chat_result_in_transcript(&replayed, task_result)?;
                let user_message_id = control
                    .user_message_id
                    .as_deref()
                    .ok_or("chat_transcript_unavailable")?;
                recover_chat_message_metadata(
                    &control.chats,
                    chat_id,
                    &replayed,
                    &control.store,
                    &control.owner,
                    Some(&control.id),
                    Some(user_message_id),
                )?;
                if control.cancel.is_cancelled() {
                    return Err("cancelled".into());
                }
                let checkpoint = captured.checkpoint();
                let checkpoint = ChatTranscriptCheckpoint {
                    byte_len: checkpoint.byte_len,
                    sha256: checkpoint.sha256.clone(),
                };
                control
                    .store
                    .record_chat_transcript_checkpoint(
                        &control.owner,
                        &control.id,
                        chat_id,
                        checkpoint.clone(),
                    )
                    .map_err(|_| "agent_storage_unavailable")?;
                Ok(Some(checkpoint))
            }
            .await;
            if let Err(error) = checkpoint_result {
                result = Err(error);
            }
        }
        self.finish_execution(&control, result).await;
    }

    async fn finish_execution(
        &self,
        control: &Control,
        result: Result<(TaskResult, Value), String>,
    ) {
        let recorded_usage = control.usage.lock().await.clone();
        let (state, output, usage, error) = match result {
            Ok((result, usage)) => (State::Succeeded, Some(result), Some(usage), None),
            Err(code) if code == "cancelled" => {
                (State::Cancelled, None, recorded_usage.clone(), Some(code))
            }
            Err(code) => (State::Failed, None, recorded_usage, Some(code)),
        };
        // A failed foreground reap keeps admission occupied. Do not release the
        // app or remove its workspace while a tool may still have effects.
        if error.as_deref() != Some("tool_cleanup_incomplete") {
            let committed = control.commit_terminal(state, output, usage, error);
            // The orchestrator owns this Control as its output/permission sink.
            // Break that strong cycle only after cleanup and terminal commit
            // have finished, while the Chat is still busy. Dropping its writer
            // releases the transcript lease before another turn can start.
            let orchestrator = control
                .chat_orchestrator
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            drop(orchestrator);
            match committed {
                Ok((task, event_storage_failed)) => {
                    if let Some(chat_id) = &control.chat_id {
                        if event_storage_failed || task.state != State::Succeeded {
                            let _ = control.chats.interrupt_turn(chat_id, &control.id);
                        } else {
                            let _ = control.chats.finish_turn(chat_id, &control.id);
                        }
                    }
                    self.active.lock().await.remove(&control.id);
                    self.clear_wake_run(&control.id);
                }
                Err(_) => {
                    // The executor returned only after the tools and lease
                    // cleanup completed. Storage quarantine protects the
                    // unresolved record; retaining this finished Control
                    // would prevent stop/reopen recovery.
                    self.active.lock().await.remove(&control.id);
                    self.clear_wake_run(&control.id);
                }
            }
        } else {
            let _ = control.store.update(
                &control.owner,
                &control.id,
                State::Running,
                None,
                usage,
                Some("tool_cleanup_incomplete".into()),
            );
        }
        control.report_chat_persisted(Err("task_unavailable".into()));
        control.revoke();
        control.finished.store(true, Ordering::Release);
        control.done.notify_waiters();
    }
    async fn execute_inner(
        &self,
        control: &Arc<Control>,
        admission: &Admission,
        input: Input,
        cwd: PathBuf,
    ) -> Result<(TaskResult, Value), String> {
        self.validate_current_release(&control.owner).await?;
        tokio::fs::create_dir_all(&cwd)
            .await
            .map_err(|_| "task_storage_failed")?;
        if admission.tools.is_empty() && admission.skills.is_empty() && control.chat_id.is_none() {
            if !control.store.lease_valid(&control.owner, &control.id)
                || control.cancel.is_cancelled()
            {
                return Err("cancelled".into());
            }
            control
                .store
                .update(
                    &control.owner,
                    &control.id,
                    State::Running,
                    None,
                    None,
                    None,
                )
                .map_err(|_| "task_storage_failed")?;
            let (mut result, usage) = control
                .run_stateless_query(self.factory.query_stateless(admission, &input))
                .await?;
            if control.protocol_thread_id.is_some() {
                // A stateless side query has no canonical conversation message.
                result.message_id = Some(format!("msg_{}", uuid::Uuid::new_v4()));
            }
            return Ok((result, usage));
        }
        let prepared = crate::app_agent_input::prepare(&input).await?;
        let (question_tx, mut question_rx) =
            tokio::sync::mpsc::channel::<tool_api::ask_user_question::AskUserQuestionExchange>(8);
        let skill_prompt = self
            .load_skill_prompt(&control.owner, &admission.skills, &cwd)
            .await?;
        let chat_resume = if let Some(chat_id) = &control.chat_id {
            let session_id = control
                .chats
                .session_id(chat_id)
                .map_err(|_| "chat_not_found")?;
            let session_uuid = uuid::Uuid::parse_str(&session_id).map_err(|_| "invalid_chat")?;
            let transcript_path = control
                .chats
                .transcript_path(chat_id)
                .map_err(|_| "chat_storage_unavailable")?;
            let previous_checkpoint = control
                .store
                .latest_chat_transcript_checkpoint(&control.owner.app_id, chat_id, &control.id)
                .map_err(|_| "chat_transcript_unavailable")?
                .map(|(_, checkpoint)| session::jsonl::ChatTranscriptCheckpoint {
                    byte_len: checkpoint.byte_len,
                    sha256: checkpoint.sha256,
                });
            let has_previous_checkpoint = previous_checkpoint.is_some();
            let writer = Arc::new(
                session::jsonl::JsonlWriter::open_persistent_chat(
                    transcript_path.clone(),
                    self.factory.context.fs.clone(),
                    previous_checkpoint,
                )
                .await
                .map_err(|_| "chat_transcript_unavailable")?,
            );
            let replayed = if has_previous_checkpoint {
                let captured = writer
                    .capture_persistent_chat_snapshot(false)
                    .await
                    .map_err(|_| "chat_transcript_unavailable")?;
                if captured.path() != transcript_path || captured.session_id() != session_uuid {
                    return Err("chat_transcript_unavailable".into());
                }
                let replayed =
                    orchestrator::ConversationOrchestrator::replay_persistent_chat_snapshot(
                        &captured,
                    )
                    .map_err(|_| "chat_transcript_unavailable")?;
                // A generic orchestrator resume can replay unresolved deferred
                // hook tools. Refuse before even reconciling transcript rows
                // into Chat metadata; a user must review the saved history.
                super::validate_local_app_chat_resume(&replayed)?;
                recover_chat_message_metadata(
                    &control.chats,
                    chat_id,
                    &replayed,
                    &control.store,
                    &control.owner,
                    None,
                    None,
                )?;
                Some(replayed)
            } else {
                None
            };
            Some(super::AppAgentChatResume {
                session_id: SessionId::from_uuid(session_uuid),
                transcript_path,
                replayed,
                writer,
            })
        } else {
            None
        };
        let building = self.factory.build(
            &control.owner,
            admission,
            &input,
            cwd,
            control.clone(),
            control.clone(),
            control.cancel.clone(),
            question_tx,
            chat_resume,
            skill_prompt,
        );
        let session = tokio::select! {biased;_=control.cancel.cancelled()=>return Err("cancelled".into()),result=building=>result?};
        if control.chat_id.is_some() {
            *control
                .chat_orchestrator
                .lock()
                .map_err(|_| "task_unavailable")? = Some(session.orchestrator.clone());
        }
        *control.lease.lock().map_err(|_| "task_unavailable")? = Some(session.lease.clone());
        if !control.store.lease_valid(&control.owner, &control.id) || control.cancel.is_cancelled()
        {
            return Err("cancelled".into());
        }
        control
            .store
            .update(
                &control.owner,
                &control.id,
                State::Running,
                None,
                None,
                None,
            )
            .map_err(|_| "task_storage_failed")?;
        if let (Some(chat_id), Some(user_message_id)) = (
            control.chat_id.as_deref(),
            control.user_message_id.as_deref(),
        ) {
            if !control.store.lease_valid(&control.owner, &control.id)
                || control.cancel.is_cancelled()
            {
                return Err("cancelled".into());
            }
            persist_chat_user_row(
                &session.orchestrator,
                &control.chats,
                chat_id,
                user_message_id,
                prepared.blocks.clone(),
                &control.store,
                &control.owner,
                &control.id,
                user_message_id,
            )
            .await?;
            if !control.store.lease_valid(&control.owner, &control.id)
                || control.cancel.is_cancelled()
            {
                return Err("cancelled".into());
            }
            control.report_chat_persisted(Ok(()));
            if !control.wait_for_chat_commit().await {
                return Err("cancelled".into());
            }
            if !control.store.lease_valid(&control.owner, &control.id)
                || control.cancel.is_cancelled()
            {
                return Err("cancelled".into());
            }
        }
        let question_control = control.clone();
        let questions = tokio::spawn(async move {
            while let Some(exchange) = question_rx.recv().await {
                let id = uuid::Uuid::new_v4().to_string();
                let mut seen = std::collections::HashSet::new();
                if exchange.questions.iter().any(|q| {
                    q.question
                        .string_at("")
                        .is_none_or(|text| text.is_empty() || !seen.insert(text.to_owned()))
                }) {
                    // A native answer map cannot represent duplicate questions.
                    // Dropping the reply produces a tool error, never approval.
                    continue;
                }
                let questions:Vec<_>=exchange.questions.iter().map(|q|json!({"question":q.question.string_at("").unwrap_or(""),"header":q.header.string_at("").unwrap_or(""),"multiSelect":q.multi_select,"options":q.options.iter().map(|o|json!({"label":o.label.string_at("").unwrap_or(""),"description":o.description.string_at("").unwrap_or("")})).collect::<Vec<_>>()})).collect();
                let mut slot = question_control.interaction.lock().await;
                if slot.is_none()
                    && question_control
                        .store
                        .set_phase(
                            &question_control.owner,
                            &question_control.id,
                            Some("waiting_for_answer"),
                        )
                        .is_ok()
                {
                    *slot = Some((
                        id.clone(),
                        json!({"type":"question","requestId":id,"runId":question_control.request_id,"appId":question_control.owner.app_id,"questions":questions}),
                        Reply::Question(exchange.resp_tx),
                    ));
                }
            }
        });
        // Await the cancel-aware foreground to completion; do not drop it at a
        // timeout and accidentally release concurrency before native shell reap.
        let projected_blocks = Value::Array(
            prepared
                .blocks
                .into_iter()
                .map(serde_json::to_value)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| "invalid_attachments")?,
        );
        let projected = Utf16JsonProjection::from_serde_value(&projected_blocks)
            .map_err(|_| "invalid_attachments")?;
        let message_id = control
            .user_message_id
            .as_deref()
            .and_then(lingxi_core::types::MessageId::parse_prefixed);
        // Snapshot before the run, including any persisted user row, so old
        // assistant messages can never become this run's public result.
        let baseline_assistant_ids: HashSet<MessageId> = session
            .orchestrator
            .snapshot_history()
            .await
            .into_iter()
            .filter(|message| matches!(message, ConversationMessage::Assistant { .. }))
            .map(|message| message.id())
            .collect();
        let outcome = session
            .orchestrator
            .run_turn_streaming_with_cancel_projected_content(
                &projected,
                control.cancel.clone(),
                message_id,
            )
            .await;
        *control.usage.lock().await =
            Some(usage_value(&session.orchestrator.snapshot_cost().await));
        questions.abort();
        control.interaction.lock().await.take();
        if session.cleanup_failed.load(Ordering::Acquire) {
            return Err("tool_cleanup_incomplete".into());
        }
        if control.model_failed.load(Ordering::Acquire) {
            return Err(control
                .model_failure
                .lock()
                .ok()
                .and_then(|v| v.clone())
                .unwrap_or_else(|| "model_execution_failed".into()));
        }
        if control.cancel.is_cancelled() {
            return Err("cancelled".into());
        }
        match outcome {
            Ok(orchestrator::conversation::TurnOutcome::EndTurn) => {}
            Ok(orchestrator::conversation::TurnOutcome::MaxTurns) => {
                return Err("turn_limit_exceeded".into());
            }
            Err(error) => {
                // Preserve the failure class without provider bodies, tool arguments,
                // model text, paths or credentials in the app-visible result.
                use orchestrator::error::OrchestratorError;
                let code = match error {
                    OrchestratorError::PermissionAbort { .. } => "tool_permission_aborted",
                    OrchestratorError::StreamingProtocol(_) => "model_stream_protocol_failed",
                    OrchestratorError::StreamEndedWithoutStop => "model_stream_incomplete",
                    OrchestratorError::Internal(_) => "agent_internal_error",
                    OrchestratorError::ApiCall(error) | OrchestratorError::Streaming(error) => {
                        super::stateless::safe_llm_error_code(&error)
                    }
                    _ => "model_execution_failed",
                };
                return Err(code.into());
            }
            _ => return Err("model_execution_failed".into()),
        }
        let structured = session
            .structured_output
            .as_ref()
            .and_then(|slot| slot.lock().ok()?.clone());
        if input.output_schema.is_some() && structured.is_none() {
            return Err("structured_output_missing".into());
        }
        let output = structured
            .map(|v| v.try_to_serde_value_with_array_budget(100_000))
            .transpose()
            .map_err(|_| "invalid_output")?;
        if output.as_ref().is_some_and(Value::is_null) {
            return Err("structured_output_not_representable".into());
        }
        let cost = session.orchestrator.snapshot_cost().await;
        let usage = usage_value(&cost);
        let final_history = session.orchestrator.snapshot_history().await;
        let observed_ids = control.assistant_message_ids.lock().await;
        let display_text = control.assistant_display_text.lock().await;
        let (message_id, text) = accepted_assistant_projection(
            &final_history,
            &baseline_assistant_ids,
            &observed_ids,
            &display_text,
        )?;
        let assistant_message_id = Some(message_id);
        let result = TaskResult {
            text,
            output,
            attachments: Vec::new(),
            chat_id: control.chat_id.clone(),
            message_id: assistant_message_id,
        };
        Ok((result, usage))
    }
    async fn cancel_ids(&self, ids: &[String]) {
        let controls: Vec<_> = {
            let active = self.active.lock().await;
            ids.iter()
                .filter_map(|id| active.get(id).cloned())
                .collect()
        };
        for control in &controls {
            let _ = control.store.request_cancel(&control.owner, &control.id);
            control.revoke();
            control.interaction.lock().await.take();
        }
        for control in controls {
            super::lifecycle::wait_for_completion(&control.done, &control.finished).await;
        }
    }

    async fn expire_background_control(&self, control: &Arc<Control>) -> Result<(), String> {
        // The store serializes this intent with terminal publication and
        // refuses further access if its persistence outcome is uncertain.
        // Revoke even when persistence fails; a storage failure cannot grant
        // an expired background execution more time or tool authority.
        let persisted = control
            .store
            .request_background_expiration(&control.owner, &control.id);
        control.revoke();
        control.interaction.lock().await.take();
        super::lifecycle::wait_for_completion(&control.done, &control.finished).await;
        persisted.map_err(|_| "agent_storage_unavailable".into())
    }

    pub async fn stop_all(&self) -> Result<(), String> {
        let _serial = self.admission.lock().await;
        self.shutdown.store(true, Ordering::Release);
        let ids = self.active.lock().await.keys().cloned().collect::<Vec<_>>();
        self.cancel_ids(&ids).await;
        if !self.active.lock().await.is_empty() {
            return Err("agent_cleanup_incomplete".into());
        }
        Ok(())
    }
    pub async fn stop_app(&self, app: &str) -> Result<(), String> {
        let _serial = self.admission.lock().await;
        let ids = self
            .active
            .lock()
            .await
            .iter()
            .filter(|(_, c)| c.owner.app_id == app)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        self.cancel_ids(&ids).await;
        self.wake_reservations
            .lock()
            .map_err(|_| "agent_storage_unavailable")?
            .clear_app(app);
        if self
            .active
            .lock()
            .await
            .values()
            .any(|control| control.owner.app_id == app)
        {
            return Err("agent_cleanup_incomplete".into());
        }
        Ok(())
    }
    pub async fn stop_invocation(&self, app: &str, invocation: &str) -> Result<(), String> {
        let ids = self
            .active
            .lock()
            .await
            .iter()
            .filter(|(_, c)| c.owner.app_id == app && c.owner.invocation_id == invocation)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        self.cancel_ids(&ids).await;
        if self
            .active
            .lock()
            .await
            .values()
            .any(|c| c.owner.app_id == app && c.owner.invocation_id == invocation)
        {
            return Err("agent_cleanup_incomplete".into());
        }
        Ok(())
    }
    pub async fn close_page(&self, app: &str, invocation: &str) -> Result<(), String> {
        // Mobile does not grant background execution. A delayed close belongs
        // to its captured invocation even if a newer run is already current.
        // Never let an old page cancel a replacement run's tasks.
        self.stop_invocation(app, invocation).await
    }
    /// Trusted native settings/interaction UI only; never exposed by SDK invoke.
    pub async fn host(self: &Arc<Self>, request: Value) -> Result<Value, String> {
        if self.shutdown.load(Ordering::Acquire) {
            return Err("agent_host_replaced".into());
        }
        let op = request["op"].as_str().ok_or("invalid_host_request")?;
        match op {
            "wake.reserve" => {
                reject_unknown(
                    &request,
                    &["op", "appId", "revision", "runId", "receiveTimeoutMs"],
                )?;
                let app = required_string(&request, "appId")?;
                let revision = request["revision"].as_u64().ok_or("invalid_revision")?;
                let run_id = required_string(&request, "runId")?;
                validate_request_id(run_id)?;
                let receive_timeout = request["receiveTimeoutMs"].as_u64().unwrap_or(60_000);
                if receive_timeout == 0 {
                    return Err("invalid_receive_timeout".into());
                }
                let receive_timeout = receive_timeout.min(120_000);
                // Configuration updates use this same admission lock. Read
                // release and authorization only after it is held so a
                // revoked grant cannot create a new wake reservation.
                let _serial = self.admission.lock().await;
                if self.shutdown.load(Ordering::Acquire) {
                    return Err("agent_host_replaced".into());
                }
                let (owner, _, _) = self.current_release(app, revision, "native-wake").await?;
                let state = self.app_state(app)?;
                if state
                    .store
                    .settings(&owner)
                    .map_err(|_| "agent_storage_unavailable")?
                    .is_none()
                {
                    return Err("agent_not_authorized".into());
                }
                let key = WakeKey {
                    app_id: app.to_owned(),
                    revision,
                    request_id: run_id.to_owned(),
                };
                let receipt = self
                    .wake_reservations
                    .lock()
                    .map_err(|_| "agent_storage_unavailable")?
                    .reserve(key, now_millis(), receive_timeout)?;
                Ok(json!({"appId":app,"revision":revision,"runId":run_id,
                        "expiresAtMs":receipt.expires_at_ms,"created":receipt.created,
                        "reservationId":receipt.reservation_id}))
            }
            "wake.clear" => {
                reject_unknown(
                    &request,
                    &[
                        "op",
                        "appId",
                        "revision",
                        "runId",
                        "reservationId",
                        "reason",
                    ],
                )?;
                let app = required_string(&request, "appId")?;
                let revision = request["revision"].as_u64().ok_or("invalid_revision")?;
                let run_id_request = required_string(&request, "runId")?;
                let reservation_id = required_string(&request, "reservationId")?;
                let system_expiration = match request.get("reason") {
                    None => false,
                    Some(Value::String(reason)) if reason == "system_expiration" => true,
                    Some(_) => return Err("invalid_wake_clear_reason".into()),
                };
                uuid::Uuid::parse_str(reservation_id).map_err(|_| "invalid_reservation_id")?;
                validate_request_id(run_id_request)?;
                let _serial = self.admission.lock().await;
                let key = WakeKey {
                    app_id: app.to_owned(),
                    revision,
                    request_id: run_id_request.to_owned(),
                };
                let run_id = self
                    .wake_reservations
                    .lock()
                    .map_err(|_| "agent_storage_unavailable")?
                    .clear(&key, reservation_id);
                if let Some(run_id) = run_id {
                    // Drop the active-map guard before cancel_ids acquires it.
                    let control = { self.active.lock().await.get(&run_id).cloned() };
                    if let Some(control) = control {
                        if control.owner.app_id == app
                            && control.revision == revision
                            && control.request_id == run_id_request
                        {
                            if system_expiration {
                                self.expire_background_control(&control).await?;
                            } else {
                                self.cancel_ids(&[run_id]).await;
                            }
                        }
                    }
                }
                Ok(Value::Null)
            }
            "startNative" => {
                reject_unknown(&request, &["op", "appId", "revision", "input"])?;
                let app = required_string(&request, "appId")?;
                let revision = request["revision"].as_u64().ok_or("invalid_revision")?;
                let raw_input = request.get("input").cloned().ok_or("invalid_agent_input")?;
                let (owner, tools, skills) =
                    self.current_release(app, revision, "native-intent").await?;
                let state = self.app_state(app)?;
                if state
                    .store
                    .settings(&owner)
                    .map_err(|_| "agent_storage_unavailable")?
                    .is_none()
                {
                    return Err("agent_not_authorized".into());
                }
                if let Some(run_id) = self
                    .duplicate_before_normalization(
                        &state,
                        &owner,
                        &raw_input,
                        None,
                        &CancellationToken::new(),
                        true,
                    )
                    .await?
                {
                    return Ok(json!({"runId":run_id}));
                }
                let (normalized, chat_id) = normalize_page_run_input(&state, raw_input, None)?;
                let duplicate_only = normalized.message_already_persisted;
                let public_run_id = normalized.run_id;
                validate_request_id(&public_run_id)?;
                let started = self
                    .start_task(
                        owner,
                        revision,
                        &tools,
                        &skills,
                        normalized.input,
                        Some(normalized.user_message),
                        chat_id,
                        CancellationToken::new(),
                        !duplicate_only,
                        true,
                    )
                    .await;
                match started {
                    Ok(_) => Ok(json!({"runId":public_run_id})),
                    Err(error) if duplicate_only && error == "host_wake_required" => {
                        Err("agui_message_already_submitted".into())
                    }
                    Err(error) => Err(error),
                }
            }
            "eventsNative" => {
                reject_unknown(
                    &request,
                    &["op", "appId", "revision", "runId", "afterSequence"],
                )?;
                let app = required_string(&request, "appId")?;
                let revision = request["revision"].as_u64().ok_or("invalid_revision")?;
                let run_id = required_string(&request, "runId")?;
                validate_request_id(run_id)?;
                let after_sequence = request
                    .get("afterSequence")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let (owner, _, _) = self.current_release(app, revision, "native-events").await?;
                let state = self.app_state(app)?;
                let page = state
                    .store
                    .protocol_events(&owner, run_id, after_sequence)
                    .map_err(|_| "agent_events_unavailable")?;
                let task_id = state
                    .store
                    .task_id_for_request(&owner, run_id)
                    .map_err(|_| "agent_events_unavailable")?;
                let active = self.active.lock().await.get(&task_id).cloned();
                let active_bytes =
                    active.map_or(0, |control| control.native_output_for(&owner, revision));
                // Read after the active lookup. Finalization saves before removal,
                // so this order cannot miss the hand-off between the two sources.
                let saved_bytes = state
                    .store
                    .native_output_bytes_for_request(&owner, run_id, revision)
                    .map_err(|_| "agent_events_unavailable")?
                    .unwrap_or(0);
                native_events_value(page, active_bytes, saved_bytes)
            }
            "tasks.list" => {
                reject_unknown(&request, &["op", "appId", "revision", "requestId"])?;
                let app = required_string(&request, "appId")?;
                let revision = request["revision"].as_u64().ok_or("invalid_revision")?;
                let request_id = request.get("requestId").and_then(Value::as_str);
                if let Some(request_id) = request_id {
                    validate_request_id(request_id)?;
                }
                let (owner, _, _) = self
                    .current_release(app, revision, "native-control")
                    .await?;
                let state = self.app_state(app)?;
                let tasks = state
                    .store
                    .list(&owner, None, 100)
                    .map_err(|_| "agent_storage_unavailable")?
                    .into_iter()
                    .filter(|task| request_id.is_none_or(|id| task.request_id == id))
                    .collect::<Vec<_>>();
                Ok(json!({"appId":app,"revision":revision,"tasks":tasks}))
            }
            "tasks.cancel" => {
                reject_unknown(&request, &["op", "appId", "revision", "requestId", "runId"])?;
                let app = required_string(&request, "appId")?;
                let revision = request["revision"].as_u64().ok_or("invalid_revision")?;
                let request_id = request.get("requestId").and_then(Value::as_str);
                let run_id = request.get("runId").and_then(Value::as_str);
                if request_id.is_none() && run_id.is_none() {
                    return Err("invalid_task_selector".into());
                }
                if let Some(request_id) = request_id {
                    validate_request_id(request_id)?;
                }
                let (owner, _, _) = self
                    .current_release(app, revision, "native-control")
                    .await?;
                let state = self.app_state(app)?;
                let tasks = state
                    .store
                    .list(&owner, None, 100)
                    .map_err(|_| "agent_storage_unavailable")?;
                let ids = tasks
                    .into_iter()
                    .filter(|task| {
                        run_id.is_none_or(|id| task.run_id == id)
                            && request_id.is_none_or(|id| task.request_id == id)
                            && !task.state.terminal()
                    })
                    .map(|task| task.run_id)
                    .collect::<Vec<_>>();
                for id in &ids {
                    state
                        .store
                        .request_cancel(&owner, id)
                        .map_err(task_access_error)?;
                }
                self.cancel_ids(&ids).await;
                Ok(Value::Null)
            }
            "connectMcp" => {
                let id = request["serverId"].as_str().ok_or("invalid_mcp_server")?;
                let config = self
                    .factory
                    .configured_servers()?
                    .into_iter()
                    .find(|server| {
                        mcp::tool_identity::McpToolIdentity::server_catalog_id(server) == id
                    })
                    .ok_or("mcp_configuration_changed")?;
                if !matches!(
                    config.spec,
                    lingxi_core::host::McpTransportSpec::Http { .. }
                        | lingxi_core::host::McpTransportSpec::Sse { .. }
                ) {
                    return Err("mcp_transport_requires_desktop".into());
                }
                let registry = self
                    .factory
                    .context
                    .mcp_registry
                    .as_ref()
                    .ok_or("mcp_unavailable")?;
                registry
                    .replace_config_atomically(config)
                    .await
                    .map_err(|_| "mcp_connection_failed")?;
                self.catalog().await
            }
            "catalog" => self.catalog().await,
            "settings" | "configure" => {
                let _serial = self.admission.lock().await;
                let app = request["appId"].as_str().ok_or("invalid_app")?;
                let service = self.service().await?;
                let record = service
                    .request_value(json!({"op":"get","appId":app}))
                    .await
                    .map_err(|_| "app_not_found")?;
                let release = service
                    .request_value(
                        json!({"op":"getRelease","appId":app,"revision":record["currentRevision"]}),
                    )
                    .await
                    .map_err(|_| "release_unavailable")?;
                let owner = Owner {
                    app_id: app.into(),
                    release_digest: release["digest"].as_str().ok_or("invalid_release")?.into(),
                    invocation_id: "native-settings".into(),
                };
                let state = self.app_state(app)?;
                if op == "settings" {
                    return serde_json::to_value(
                        state
                            .store
                            .settings(&owner)
                            .map_err(|_| "agent_storage_unavailable")?,
                    )
                    .map_err(|_| "invalid_settings".into());
                }
                let settings: Option<Settings> =
                    serde_json::from_value(request["settings"].clone())
                        .map_err(|_| "invalid_settings")?;
                if let Some(settings) = &settings {
                    if settings.background && !cfg!(target_os = "ios") {
                        return Err("background_not_supported".into());
                    }
                    if !release["manifest"]["capabilities"]
                        .as_array()
                        .is_some_and(|v| v.contains(&json!("agent")))
                    {
                        return Err("agent_not_declared".into());
                    }
                    let catalog = self.catalog().await?;
                    if !catalog["models"].as_array().is_some_and(|models| {
                        models.iter().any(|m| {
                            m["providerId"] == settings.model.provider_id
                                && m["modelId"] == settings.model.model_id
                                && m["canSelect"] == true
                        })
                    }) {
                        return Err("model_unavailable".into());
                    }
                    let granted: BTreeMap<String, ToolGrant> =
                        serde_json::from_value(catalog["tools"].clone())
                            .map_err(|_| "invalid_tool_catalog")?;
                    for (id, grant) in &settings.tools {
                        if granted.get(id) != Some(grant) {
                            return Err("tool_catalog_changed".into());
                        }
                    }
                    let declared_skills = release["manifest"]["agent"]["skills"]
                        .as_array()
                        .map(|items| {
                            items
                                .iter()
                                .filter_map(|item| item["id"].as_str())
                                .collect::<std::collections::HashSet<_>>()
                        })
                        .unwrap_or_default();
                    if settings
                        .skills
                        .iter()
                        .any(|id| !declared_skills.contains(id.as_str()))
                    {
                        return Err("skill_not_declared".into());
                    }
                }
                let result = state
                    .store
                    .configure_with_change(owner, settings)
                    .map_err(|_| "agent_storage_unavailable")?;
                if result.changed {
                    self.wake_reservations
                        .lock()
                        .map_err(|_| "agent_storage_unavailable")?
                        .clear_app(app);
                }
                self.cancel_ids(&result.affected_run_ids).await;
                Ok(json!({"changed": result.changed}))
            }
            "interactions" => {
                let controls = self
                    .active
                    .lock()
                    .await
                    .values()
                    .cloned()
                    .collect::<Vec<_>>();
                let mut result = Vec::new();
                for c in controls {
                    if let Some((_, value, _)) = c.interaction.lock().await.as_ref() {
                        result.push(value.clone());
                    }
                }
                Ok(json!(result))
            }
            "answer" => {
                let run_id = request["runId"].as_str().ok_or("invalid_run")?;
                let id = request["requestId"].as_str().ok_or("invalid_request")?;
                let c = self
                    .active
                    .lock()
                    .await
                    .values()
                    .find(|control| control.request_id == run_id)
                    .cloned()
                    .ok_or("task_not_found")?;
                self.validate_current_release(&c.owner).await?;
                if !c.store.lease_valid(&c.owner, &c.id) || c.cancel.is_cancelled() {
                    return Err("task_revoked".into());
                }
                let mut slot = c.interaction.lock().await;
                if !slot.as_ref().is_some_and(|(current, _, _)| current == id) {
                    return Err("request_not_found".into());
                }
                let (_, _, reply) = slot.take().ok_or("request_not_found")?;
                c.store
                    .set_phase(&c.owner, &c.id, None)
                    .map_err(|_| "agent_storage_unavailable")?;
                match reply {
                    Reply::Permission(tx) => {
                        let _ = tx.send(request["approved"] == true);
                    }
                    Reply::Question(tx) => {
                        let values: BTreeMap<String, String> =
                            serde_json::from_value(request["answers"].clone())
                                .map_err(|_| "invalid_answers")?;
                        let _ = tx.send(
                            values
                                .into_iter()
                                .map(|(q, a)| (q.into(), a.into()))
                                .collect(),
                        );
                    }
                }
                Ok(Value::Null)
            }
            _ => Err("unsupported_host_operation".into()),
        }
    }
    async fn catalog(&self) -> Result<Value, String> {
        self.factory.catalog().await
    }
}
fn only_run_id(args: &Value) -> Result<&str, String> {
    if args.as_object().is_none_or(|m| m.len() != 1) {
        return Err("invalid_agent_arguments".into());
    }
    args["runId"].as_str().ok_or_else(|| "invalid_run".into())
}

async fn wait_for_chat_persistence(
    receiver: &mut watch::Receiver<Option<Result<(), String>>>,
    cancel: &CancellationToken,
    detached: bool,
) -> Result<(), String> {
    loop {
        if let Some(result) = receiver.borrow().clone() {
            return result;
        }
        tokio::select! {
            biased;
            _ = cancel.cancelled(), if !detached => return Err("cancelled".into()),
            changed = receiver.changed() => {
                if changed.is_err() {
                    return Err("task_unavailable".into());
                }
            }
        }
    }
}

async fn wait_for_duplicate_chat_persistence(
    receiver: &mut watch::Receiver<Option<Result<(), String>>>,
    control: &Control,
    owner_cancel: &CancellationToken,
    detached: bool,
) -> Result<(), String> {
    loop {
        if let Some(result) = receiver.borrow().clone() {
            return result;
        }
        if control.finished.load(Ordering::Acquire) {
            return Err("task_unavailable".into());
        }
        tokio::select! {
            biased;
            _ = control.cancel.cancelled() => return Err("cancelled".into()),
            _ = owner_cancel.cancelled(), if !detached => return Err("app_invocation_closed".into()),
            changed = receiver.changed() => {
                if changed.is_err() {
                    return Err("task_unavailable".into());
                }
            }
        }
    }
}

/// The one pre-admission Chat persistence step. The task executor appends the
/// stable user row to the App-owned Harness session before it acknowledges
/// `{runId}`; only then may the caller release the executor to make a model
/// request. The strict writer capture is replayed directly to verify the row.
async fn persist_chat_user_row(
    orchestrator: &orchestrator::ConversationOrchestrator,
    chats: &ChatStore,
    chat_id: &str,
    message_id: &str,
    blocks: Vec<lingxi_core::types::ContentBlock>,
    store: &AgentStore,
    owner: &Owner,
    current_task_id: &str,
    current_user_message_id: &str,
) -> Result<(), String> {
    let message_id =
        lingxi_core::types::MessageId::parse_prefixed(message_id).ok_or("invalid_chat_message")?;
    if orchestrator.session_transcript_writer().is_none() {
        return Err("chat_transcript_unavailable".into());
    }
    if !orchestrator
        .session_contains_message_uuid(&message_id.to_string())
        .await
    {
        let mut message = lingxi_core::types::ConversationMessage::user(message_id, String::new());
        if let lingxi_core::types::ConversationMessage::User { content, .. } = &mut message {
            *content = blocks;
        }
        orchestrator.append_external_history_message(message).await;
    }
    if !orchestrator
        .session_contains_message_uuid(&message_id.to_string())
        .await
    {
        return Err("chat_transcript_unavailable".into());
    }
    let transcript_path = chats
        .transcript_path(chat_id)
        .map_err(|_| "chat_storage_unavailable")?;
    if !checked_chat_transcript_exists(&transcript_path)? {
        return Err("chat_transcript_unavailable".into());
    }
    let session_id = chats.session_id(chat_id).map_err(|_| "chat_not_found")?;
    let session_uuid = uuid::Uuid::parse_str(&session_id).map_err(|_| "invalid_chat")?;
    let captured = orchestrator
        .verify_persistent_chat_checkpoint(false)
        .await
        .map_err(|_| "chat_transcript_unavailable")?;
    if captured.path() != transcript_path.as_path() || captured.session_id() != session_uuid {
        return Err("chat_transcript_unavailable".into());
    }
    let replayed =
        orchestrator::ConversationOrchestrator::replay_persistent_chat_snapshot(&captured)
            .map_err(|_| "chat_transcript_unavailable")?;
    super::validate_local_app_chat_resume(&replayed)?;
    let persisted_user_rows = replayed
        .display_history
        .iter()
        .filter(|row| row.id() == message_id && row.role() == lingxi_core::types::MessageRole::User)
        .count();
    if persisted_user_rows != 1 {
        return Err("chat_transcript_unavailable".into());
    }
    recover_chat_message_metadata(
        chats,
        chat_id,
        &replayed,
        store,
        owner,
        Some(current_task_id),
        Some(current_user_message_id),
    )
}

/// Reconcile transcript rows with the host-owned Chat index. Harness writes
/// the durable session before its output callback can finish committing the
/// index; this idempotent repair makes refresh/history safe after that crash
/// window without replaying any model or tool side effects.
fn recover_chat_message_metadata(
    chats: &ChatStore,
    chat_id: &str,
    replayed: &orchestrator::resume::ReplayedSession,
    store: &AgentStore,
    owner: &Owner,
    current_task_id: Option<&str>,
    current_user_message_id: Option<&str>,
) -> Result<(), String> {
    let known = chats
        .message_metadata(chat_id)
        .map_err(|_| "chat_storage_unavailable")?
        .into_iter()
        .map(|message| (message.message_id.clone(), message))
        .collect::<HashMap<_, _>>();
    let mut current_run: Option<String> = None;
    let mut tool_use_owners = HashMap::<String, String>::new();
    let mut saw_current_task_user = false;
    let mut seen_transcript_rows = HashSet::new();
    let mut planned = Vec::new();
    for row in &replayed.display_history {
        // Compact summaries have no durable per-run provenance. They could
        // contain text from a failed or cancelled turn, so they cannot be
        // replayed as a trusted substitute for the original successful rows.
        if row.is_compact_summary() {
            return Err("chat_transcript_unavailable".into());
        }
        if row.is_meta() || row.role() == lingxi_core::types::MessageRole::System {
            continue;
        }
        let message_id = row.id().to_string();
        if !seen_transcript_rows.insert(message_id.clone()) {
            return Err("chat_transcript_unavailable".into());
        }
        let (role, content) = match row {
            ConversationMessage::User { content, .. } => (ChatRole::User, content.as_slice()),
            ConversationMessage::Assistant { content, .. } => {
                (ChatRole::Assistant, content.as_slice())
            }
            ConversationMessage::System { .. } => continue,
        };
        let existing = known.get(&message_id).cloned();
        if existing
            .as_ref()
            .is_some_and(|metadata| metadata.role != role)
        {
            return Err("chat_transcript_unavailable".into());
        }
        let tool_result_ids = content
            .iter()
            .filter_map(|block| match block {
                lingxi_core::types::ContentBlock::ToolResult { tool_use_id, .. } => {
                    Some(tool_use_id.to_string())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        let is_tool_result = role == ChatRole::User && !tool_result_ids.is_empty();
        let run_id = match role {
            ChatRole::User if !tool_result_ids.is_empty() => {
                let mut owner_run_id: Option<String> = None;
                for tool_use_id in &tool_result_ids {
                    let tool_owner = tool_use_owners
                        .get(tool_use_id)
                        .ok_or("chat_transcript_unavailable")?;
                    if owner_run_id
                        .as_ref()
                        .is_some_and(|owner| owner != tool_owner)
                    {
                        return Err("chat_transcript_unavailable".into());
                    }
                    owner_run_id = Some(tool_owner.clone());
                }
                owner_run_id.expect("tool result IDs were checked")
            }
            ChatRole::User
                if current_task_id.is_some()
                    && current_user_message_id == Some(message_id.as_str()) =>
            {
                current_task_id.unwrap().to_owned()
            }
            ChatRole::User => {
                let message_task_id = lingxi_core::types::MessageId::parse_prefixed(&message_id)
                    .map(|message_id| message_id.as_uuid().to_string());
                if let (Some(existing), Some(message_task_id)) =
                    (existing.as_ref(), message_task_id.as_ref())
                {
                    if &existing.run_id != message_task_id {
                        return Err("chat_transcript_unavailable".into());
                    }
                }
                message_task_id
                    .or_else(|| existing.as_ref().map(|metadata| metadata.run_id.clone()))
                    .ok_or("chat_transcript_unavailable")?
            }
            ChatRole::Assistant => current_run.clone().ok_or("chat_transcript_unavailable")?,
        };
        if existing
            .as_ref()
            .is_some_and(|metadata| metadata.run_id != run_id && !is_tool_result)
        {
            return Err("chat_transcript_unavailable".into());
        }
        if is_tool_result
            && existing
                .as_ref()
                .is_some_and(|metadata| metadata.run_id != run_id)
        {
            let previous_run_id = lingxi_core::types::MessageId::parse_prefixed(&message_id)
                .map(|message_id| message_id.as_uuid().to_string());
            if previous_run_id.as_deref()
                != existing.as_ref().map(|metadata| metadata.run_id.as_str())
            {
                return Err("chat_transcript_unavailable".into());
            }
        }
        if current_task_id == Some(run_id.as_str()) {
            if role == ChatRole::User && current_user_message_id == Some(message_id.as_str()) {
                if saw_current_task_user {
                    return Err("chat_transcript_unavailable".into());
                }
                saw_current_task_user = true;
            } else if role == ChatRole::Assistant || is_tool_result {
                if !saw_current_task_user {
                    return Err("chat_transcript_unavailable".into());
                }
            } else {
                return Err("chat_transcript_unavailable".into());
            }
        } else {
            let (task, _checkpoint) = store
                .successful_chat_history_task(&owner.app_id, chat_id, &run_id)
                .map_err(|_| "chat_transcript_unavailable")?;
            if task.state != State::Succeeded || saw_current_task_user {
                return Err("chat_transcript_unavailable".into());
            }
        }
        let created_at = replayed
            .display_timestamps
            .get(&message_id)
            .copied()
            .unwrap_or_else(|| now_millis() as i64)
            .max(0) as u64;
        let is_assistant = role == ChatRole::Assistant;
        planned.push((
            message_id.clone(),
            run_id.clone(),
            role,
            created_at,
            existing,
            is_tool_result,
        ));
        if is_assistant {
            for block in content {
                if let lingxi_core::types::ContentBlock::ToolUse { id, .. } = block {
                    let tool_use_id = id.to_string();
                    if tool_use_owners
                        .insert(tool_use_id, run_id.clone())
                        .is_some_and(|previous| previous != run_id)
                    {
                        return Err("chat_transcript_unavailable".into());
                    }
                }
            }
        }
        current_run = Some(run_id);
    }
    if current_task_id.is_some() && !saw_current_task_user {
        return Err("chat_transcript_unavailable".into());
    }
    // Only mutate the host index after validating the complete transcript.
    // A bad row late in the file must not leave a partially repaired prefix.
    for (message_id, run_id, role, created_at, existing, is_tool_result) in planned {
        if let Some(existing) = existing.as_ref() {
            if existing.run_id != run_id {
                if !is_tool_result || existing.role != ChatRole::User {
                    return Err("chat_transcript_unavailable".into());
                }
                chats
                    .repair_recovered_message_run_id(
                        chat_id,
                        &message_id,
                        &existing.run_id,
                        &run_id,
                    )
                    .map_err(|_| "chat_storage_unavailable")?;
            }
        } else {
            let metadata = ChatMessageMetadata {
                message_id: message_id.clone(),
                run_id,
                role,
                created_at,
                attachments: Vec::new(),
            };
            chats
                .record_message(chat_id, metadata.clone())
                .map_err(|_| "chat_storage_unavailable")?;
        }
    }
    Ok(())
}

/// A successful TaskResult must name the last canonical assistant row in the
/// exact captured replay, and its public text must be the text stored there.
/// Display-hook overrides are intentionally refused because they are not part
/// of the durable transcript and cannot serve as resumable transcript proof.
fn validate_chat_result_in_transcript(
    replayed: &orchestrator::resume::ReplayedSession,
    result: &TaskResult,
) -> Result<(), String> {
    // Structured output is collected through StructuredOutputSlot after tool
    // dispatch. The transcript preserves the model's proposed tool input, but
    // managed PreToolUse may rewrite the effective input before the slot is
    // filled. Without a durable receipt binding that effective value to this
    // captured transcript, publishing it as resumable Chat authority is unsafe.
    if result.output.is_some() || !result.attachments.is_empty() {
        return Err("chat_transcript_unavailable".into());
    }
    let message_id = result
        .message_id
        .as_deref()
        .filter(|id| {
            lingxi_core::types::MessageId::parse_prefixed(id)
                .is_some_and(|parsed| parsed.to_string() == *id)
        })
        .ok_or("chat_transcript_unavailable")?;
    let mut matching = None;
    let mut matching_count = 0usize;
    let mut last_assistant = None;
    for row in replayed
        .display_history
        .iter()
        .filter(|row| !row.is_meta() && row.role() == lingxi_core::types::MessageRole::Assistant)
    {
        last_assistant = Some(row);
        if row.id().to_string() == message_id {
            matching_count += 1;
            matching = Some(row);
        }
    }
    let accepted = matching.ok_or("chat_transcript_unavailable")?;
    if matching_count != 1
        || last_assistant.is_none_or(|last| last.id().to_string() != message_id)
        || accepted.text_content() != result.text
    {
        return Err("chat_transcript_unavailable".into());
    }
    Ok(())
}

fn validate_chat_output_mode(chat_id: Option<&str>, input: &Input) -> Result<(), String> {
    if chat_id.is_some() && input.output_schema.is_some() {
        Err("chat_structured_output_unsupported".into())
    } else {
        Ok(())
    }
}

fn normalize_page_run_input(
    state: &AppAgentState,
    input: Value,
    required_chat: Option<&str>,
) -> Result<
    (
        local_app::v2::agent::protocol::NormalizedInput,
        Option<String>,
    ),
    String,
> {
    let thread_id = input
        .get("threadId")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= 128)
        .ok_or("invalid_agui_input")?
        .to_owned();
    let chat_id = if let Some(required_chat) = required_chat {
        if required_chat != thread_id
            || !uuid::Uuid::parse_str(required_chat)
                .is_ok_and(|uuid| uuid.to_string() == required_chat)
        {
            return Err("chat_thread_mismatch".into());
        }
        match state.chats.get(required_chat) {
            Ok(summary) if summary.state != ChatState::Interrupted => {}
            Ok(_) => return Err("agui_history_unavailable".into()),
            Err(local_app::v2::AppError::NotFound(_)) => return Err("chat_not_found".into()),
            Err(_) => return Err("chat_storage_unavailable".into()),
        }
        Some(required_chat.to_owned())
    } else {
        if !uuid::Uuid::parse_str(&thread_id).is_ok_and(|uuid| uuid.to_string() == thread_id) {
            None
        } else {
            match state.chats.get(&thread_id) {
                Ok(summary) if summary.state != ChatState::Interrupted => Some(thread_id.clone()),
                Ok(_) => return Err("agui_history_unavailable".into()),
                Err(local_app::v2::AppError::NotFound(_)) => None,
                Err(_) => return Err("chat_storage_unavailable".into()),
            }
        }
    };
    let history = if let Some(chat_id) = chat_id.as_deref() {
        if state
            .chats
            .has_legacy_history_without_protocol(chat_id)
            .map_err(|_| "chat_storage_unavailable")?
        {
            return Err("agui_history_unavailable".into());
        }
        state
            .chats
            .protocol_history_digests(chat_id)
            .map_err(|_| "chat_storage_unavailable")?
    } else {
        Vec::new()
    };
    let mut normalized = local_app::v2::agent::protocol::normalize_run_input(&input, &history)
        .map_err(|error| error.code)?;
    if chat_id.is_some() {
        normalized.input.protocol_history_digest = Some(
            local_app::v2::agent::protocol::history_fingerprint(&history),
        );
    }
    Ok((normalized, chat_id))
}

fn protocol_duplicate_run(
    state: &AppAgentState,
    owner: &Owner,
    input: &Value,
    required_chat: Option<&str>,
) -> Result<Option<String>, String> {
    use local_app::v2::{agent::protocol, AppError};

    let thread_id = input
        .get("threadId")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= 128)
        .ok_or("invalid_agui_input")?;
    let chat_id = if let Some(required_chat) = required_chat {
        if required_chat != thread_id
            || !uuid::Uuid::parse_str(required_chat)
                .is_ok_and(|uuid| uuid.to_string() == required_chat)
        {
            return Err("chat_thread_mismatch".into());
        }
        match state.chats.get(required_chat) {
            Ok(_) => Some(required_chat.to_owned()),
            Err(AppError::NotFound(_)) => return Err("chat_not_found".into()),
            Err(_) => return Err("chat_storage_unavailable".into()),
        }
    } else if uuid::Uuid::parse_str(thread_id).is_ok_and(|uuid| uuid.to_string() == thread_id) {
        match state.chats.get(thread_id) {
            Ok(_) => Some(thread_id.to_owned()),
            Err(AppError::NotFound(_)) => None,
            Err(_) => return Err("chat_storage_unavailable".into()),
        }
    } else {
        None
    };
    let run_id = protocol::run_id(input).map_err(|error| error.code)?;
    let fingerprint = protocol::run_input_fingerprint(input).map_err(|error| error.code)?;
    match state.store.get_by_request(owner, &run_id) {
        Ok(_) => {}
        Err(AppError::NotFound(_)) => return Ok(None),
        Err(_) => return Err("agent_storage_unavailable".into()),
    }
    let original = match state.store.protocol_input_fingerprint(owner, &run_id) {
        Ok(fingerprint) => fingerprint,
        Err(AppError::NotFound(_)) => return Err("request_conflict".into()),
        Err(_) => return Err("agent_storage_unavailable".into()),
    };
    let stored_thread_id = state
        .store
        .thread_id_for_request(owner, &run_id)
        .map_err(|_| "agent_storage_unavailable")?;
    if original != fingerprint || stored_thread_id != thread_id {
        return Err("request_conflict".into());
    }
    let task_id = state
        .store
        .task_id_for_request(owner, &run_id)
        .map_err(|_| "agent_storage_unavailable")?;
    let task_chat_id = state
        .store
        .protocol_chat_id_for_request(owner, &run_id)
        .map_err(|_| "agent_storage_unavailable")?;
    match (task_chat_id.as_deref(), chat_id.as_deref()) {
        (Some(expected), Some(actual)) if expected == actual => {}
        (Some(_), None) => return Err("chat_storage_unavailable".into()),
        (Some(_), Some(_)) | (None, Some(_)) => return Err("request_conflict".into()),
        (None, None) => {}
    }
    let events = state
        .store
        .protocol_events(owner, &run_id, 0)
        .map_err(|_| "agent_events_unavailable")?;
    if events
        .events
        .first()
        .and_then(|event| event.get("type"))
        .and_then(Value::as_str)
        != Some("RUN_STARTED")
    {
        return Err("agent_storage_unavailable".into());
    }

    if let Some(chat_id) = chat_id {
        let user_message = input
            .get("messages")
            .and_then(Value::as_array)
            .and_then(|messages| messages.last())
            .filter(|message| message.get("role").and_then(Value::as_str) == Some("user"))
            .ok_or("invalid_agui_input")?;
        let message_id = user_message
            .get("id")
            .and_then(Value::as_str)
            .ok_or("invalid_agui_input")?;
        let persisted_user = state
            .chats
            .protocol_message(&chat_id, message_id)
            .map_err(|_| "chat_storage_unavailable")?;
        if &persisted_user != user_message
            || !state
                .chats
                .message_metadata(&chat_id)
                .map_err(|_| "chat_storage_unavailable")?
                .iter()
                .any(|message| message.run_id == task_id && message.role == ChatRole::User)
        {
            return Err("chat_storage_unavailable".into());
        }
    }
    Ok(Some(run_id))
}

fn validate_duplicate_initialization(
    state: &AppAgentState,
    owner: &Owner,
    input: &Input,
    protocol_user_message: Option<&Value>,
    chat_id: Option<&str>,
    thread_id: &str,
    task_id: &str,
) -> Result<(), String> {
    if state
        .store
        .thread_id_for_request(owner, &input.request_id)
        .map_err(|_| "agent_storage_unavailable")?
        != thread_id
        || state
            .store
            .task_id_for_request(owner, &input.request_id)
            .map_err(|_| "agent_storage_unavailable")?
            != task_id
    {
        return Err("request_conflict".into());
    }
    let events = state
        .store
        .protocol_events(owner, &input.request_id, 0)
        .map_err(|_| "agent_events_unavailable")?;
    if events
        .events
        .first()
        .and_then(|event| event.get("type"))
        .and_then(Value::as_str)
        != Some("RUN_STARTED")
    {
        return Err("agent_storage_unavailable".into());
    }
    if let Some(chat_id) = chat_id {
        if state
            .store
            .protocol_chat_id_for_request(owner, &input.request_id)
            .map_err(|_| "agent_storage_unavailable")?
            .as_deref()
            != Some(chat_id)
        {
            return Err("request_conflict".into());
        }
        let message = protocol_user_message.ok_or("chat_storage_unavailable")?;
        let message_id = message
            .get("id")
            .and_then(Value::as_str)
            .ok_or("chat_storage_unavailable")?;
        let persisted = state
            .chats
            .protocol_message(chat_id, message_id)
            .map_err(|_| "chat_storage_unavailable")?;
        if &persisted != message {
            return Err("chat_storage_unavailable".into());
        }
        let task_message_id = MessageId::from_uuid(
            uuid::Uuid::parse_str(task_id).map_err(|_| "agent_storage_unavailable")?,
        )
        .to_string();
        if !state
            .chats
            .message_metadata(chat_id)
            .map_err(|_| "chat_storage_unavailable")?
            .iter()
            .any(|metadata| {
                metadata.message_id == task_message_id
                    && metadata.run_id == task_id
                    && metadata.role == ChatRole::User
            })
        {
            return Err("chat_storage_unavailable".into());
        }
    } else if state
        .store
        .protocol_chat_id_for_request(owner, &input.request_id)
        .map_err(|_| "agent_storage_unavailable")?
        .is_some()
    {
        return Err("request_conflict".into());
    }
    Ok(())
}

fn protocol_terminal_event(task: &local_app::v2::agent::Task, thread_id: &str) -> Option<Value> {
    match task.state {
        State::Succeeded => {
            if task
                .result
                .as_ref()
                .and_then(|result| result.output.as_ref())
                .is_some_and(Value::is_null)
            {
                return Some(json!({
                    "type":"RUN_ERROR",
                    "message":"The requested structured result cannot be represented as null.",
                    "code":"structured_output_not_representable"
                }));
            }
            let mut event = json!({
                "type":"RUN_FINISHED",
                "threadId":thread_id,
                "runId":task.request_id,
                "outcome":{"type":"success"}
            });
            if let Some(result) = task
                .result
                .as_ref()
                .and_then(|result| result.output.as_ref())
                .filter(|result| !result.is_null())
            {
                event["result"] = result.clone();
            }
            Some(event)
        }
        State::Cancelled => Some(json!({
            "type":"RUN_FINISHED",
            "threadId":thread_id,
            "runId":task.request_id,
            "outcome":{"type":"cancelled"}
        })),
        State::Failed | State::Interrupted => Some(json!({
            "type":"RUN_ERROR",
            "message":task.error.as_ref().map_or("The Agent run did not complete.", |error| error.message.as_str()),
            "code":task.error.as_ref().map_or("agent_failed", |error| error.code.as_str())
        })),
        State::Queued | State::Running => None,
    }
}

#[cfg(test)]
fn commit_agent_terminal_for_test(
    store: &AgentStore,
    owner: &Owner,
    id: &str,
    state: State,
    result: Option<TaskResult>,
    error: Option<String>,
) -> Result<local_app::v2::agent::Task, String> {
    let current = store
        .get(owner, id)
        .map_err(|_| "agent_storage_unavailable")?;
    if state == State::Succeeded {
        if let Some(chat_id) = result.as_ref().and_then(|result| result.chat_id.as_deref()) {
            let previous_length = store
                .latest_chat_transcript_checkpoint(&owner.app_id, chat_id, id)
                .map_err(|_| "agent_storage_unavailable")?
                .map(|(_, checkpoint)| checkpoint.byte_len)
                .unwrap_or(0);
            // Ledger-only protocol fixtures do not create a Harness JSONL
            // transcript. A shape-valid synthetic checkpoint exercises the
            // private ledger contract; it is never evidence of a resumable
            // physical Chat in production.
            store
                .record_chat_transcript_checkpoint(
                    owner,
                    id,
                    chat_id,
                    ChatTranscriptCheckpoint {
                        byte_len: previous_length.saturating_add(1),
                        sha256: "0".repeat(64),
                    },
                )
                .map_err(|_| "agent_storage_unavailable")?;
        }
    }
    let thread_id = store
        .thread_id_for_request(owner, &current.request_id)
        .map_err(|_| "agent_storage_unavailable")?;
    store
        .with_protocol_commit_permit(|permit| {
            let candidate = store
                .prepare_protocol_terminal(permit, owner, id, state, result, None, error)
                .map_err(|_| "agent_storage_unavailable")?;
            if candidate.state == State::Succeeded {
                let result = candidate
                    .result
                    .as_ref()
                    .ok_or("agent_storage_unavailable")?;
                let message_id = result
                    .message_id
                    .as_deref()
                    .ok_or("agent_storage_unavailable")?;
                for event in [
                    json!({"type":"TEXT_MESSAGE_START","messageId":message_id,"role":"assistant"}),
                    json!({"type":"TEXT_MESSAGE_CONTENT","messageId":message_id,"delta":result.text.clone()}),
                    json!({"type":"TEXT_MESSAGE_END","messageId":message_id}),
                ] {
                    store
                        .append_protocol_commit_event(permit, owner, id, event)
                        .map_err(|_| "agent_storage_unavailable")?;
                }
            }
            let terminal = protocol_terminal_event(&candidate, &thread_id)
                .ok_or("agent_storage_unavailable")?;
            store
                .append_protocol_commit_event(permit, owner, id, terminal)
                .map_err(|_| "agent_storage_unavailable")?;
            store
                .finalize_protocol_terminal(permit, owner, id)
                .map_err(|_| "agent_storage_unavailable")
        })
        .map_err(|_| "agent_storage_unavailable")?
        .map_err(str::to_owned)
}

// The native history is authoritative for acceptance and fallback retraction.
// Identity callbacks constrain this selection to responses from this run.
fn accepted_assistant_projection(
    history: &[ConversationMessage],
    baseline_ids: &HashSet<MessageId>,
    observed_ids: &HashSet<MessageId>,
    display_text: &HashMap<MessageId, String>,
) -> Result<(String, String), String> {
    history
        .iter()
        .rev()
        .find_map(|message| match message {
            ConversationMessage::Assistant { id, .. }
                if !baseline_ids.contains(id) && observed_ids.contains(id) =>
            {
                Some((
                    id.to_string(),
                    display_text
                        .get(id)
                        .cloned()
                        .unwrap_or_else(|| message.text_content()),
                ))
            }
            _ => None,
        })
        .ok_or_else(|| "chat_message_identity_missing".into())
}

// Both stateless and session paths share the existing serialized result cap.
// Together with bounded text-event chunks this leaves room for the terminal
// event in the 8 MiB / 10,000-event durable AG-UI log.
fn validate_task_result(result: &TaskResult) -> Result<(), String> {
    if serde_json::to_value(result)
        .map_err(|_| "invalid_output")?
        .to_string()
        .len()
        > 2 * 1024 * 1024
    {
        return Err("result_too_large".into());
    }
    Ok(())
}

fn native_events_value(
    page: ProtocolEventPage,
    active_bytes: u64,
    saved_bytes: u64,
) -> Result<Value, String> {
    let mut response = serde_json::to_value(page).map_err(|_| "agent_events_unavailable")?;
    let bytes = active_bytes.max(saved_bytes).min(MAX_NATIVE_OUTPUT_BYTES);
    if bytes > 0 {
        response["nativeOutputBytes"] = json!(bytes);
    }
    Ok(response)
}

fn public_task_value(
    task: &local_app::v2::agent::Task,
    thread_id: String,
) -> Result<Value, String> {
    let mut value = json!({
        "runId":task.request_id,
        "threadId":thread_id,
        "state":task.state,
        "sequence":task.sequence,
        "createdAt":task.created_at,
        "updatedAt":task.updated_at,
        "limits":task.limits
    });
    if let Some(phase) = &task.phase {
        value["phase"] = json!(phase);
    }
    if let Some(progress) = &task.progress {
        value["progress"] = serde_json::to_value(progress).map_err(|_| "invalid_task")?;
    }
    if let Some(error) = &task.error {
        value["error"] = json!({"code":error.code,"message":error.message});
    }
    // The private TaskResult remains available only to trusted native views.
    // Public result content is reconstructed from standard AG-UI events.
    Ok(value)
}

fn task_inventory_page(
    state: &AppAgentState,
    owner: &Owner,
    cursor: Option<&str>,
    limit: usize,
) -> Result<Value, String> {
    let page = state
        .store
        .list_page(owner, cursor, limit)
        .map_err(|_| "invalid_task_list")?;
    let mut summaries = Vec::with_capacity(page.tasks.len());
    for task in &page.tasks {
        let thread_id = state
            .store
            .thread_id_for_request(owner, &task.request_id)
            .map_err(|_| "invalid_task_list")?;
        summaries.push(public_task_value(task, thread_id)?);
    }
    public_inventory_page("tasks", summaries, page.next_cursor)
}

fn chat_inventory_page(
    state: &AppAgentState,
    cursor: Option<&str>,
    limit: usize,
    include_archived: bool,
) -> Result<Value, String> {
    let page = state
        .chats
        .list_page(cursor, limit, include_archived)
        .map_err(|_| "invalid_chat_list")?;
    public_inventory_page("chats", page.chats, page.next_cursor)
}

fn public_inventory_page<T: Serialize>(
    field: &str,
    items: Vec<T>,
    next_cursor: Option<String>,
) -> Result<Value, String> {
    let mut page = serde_json::Map::new();
    page.insert(
        field.to_owned(),
        serde_json::to_value(items).map_err(|_| "invalid_agent_list")?,
    );
    if let Some(cursor) = next_cursor {
        page.insert("nextCursor".into(), Value::String(cursor));
    }
    Ok(Value::Object(page))
}

#[cfg(test)]
mod inventory_page_tests {
    use super::*;

    fn make_owner(app_id: &str, release_digest: &str) -> Owner {
        Owner {
            app_id: app_id.into(),
            release_digest: release_digest.into(),
            invocation_id: "inventory-page-test".into(),
        }
    }

    fn settings() -> Settings {
        serde_json::from_value(json!({
            "model":{"providerId":"provider","modelId":"model"},
            "tools":{},"skills":[],"background":false,"maxTurns":4,"timeoutMs":30000
        }))
        .unwrap()
    }

    fn admit_succeeded(state: &AppAgentState, owner: &Owner, request_id: &str) {
        let input: Input = serde_json::from_value(json!({
            "requestId":request_id,
            "protocolThreadId":format!("thread-{request_id}"),
            "prompt":"page test",
            "attachments":[]
        }))
        .unwrap();
        let admission = state.store.admit(owner, &input, &[]).unwrap();
        state
            .store
            .update(
                owner,
                &admission.task.run_id,
                State::Running,
                None,
                None,
                None,
            )
            .unwrap();
        state
            .store
            .append_event(
                owner,
                &admission.task.run_id,
                json!({
                    "type":"RUN_STARTED",
                    "protocolVersion":"1.0",
                    "threadId":format!("thread-{request_id}"),
                    "runId":request_id
                }),
            )
            .unwrap();
        commit_agent_terminal_for_test(
            &state.store,
            owner,
            &admission.task.run_id,
            State::Succeeded,
            Some(TaskResult {
                text: String::new(),
                output: None,
                attachments: Vec::new(),
                chat_id: None,
                message_id: Some(format!("assistant-{}", request_id)),
            }),
            None,
        )
        .unwrap();
    }

    #[test]
    fn host_task_inventory_returns_two_scoped_public_pages() {
        let root = tempfile::tempdir().unwrap();
        let app_id = format!("a{}", "1".repeat(32));
        std::fs::create_dir_all(root.path().join(&app_id)).unwrap();
        let state = AppAgentState::open(root.path(), &app_id).unwrap();
        let owner = make_owner(&app_id, &"2".repeat(64));
        state
            .store
            .configure(owner.clone(), Some(settings()))
            .unwrap();
        for index in 0..3 {
            admit_succeeded(&state, &owner, &format!("request-{index}"));
        }

        let other_release = make_owner(&app_id, &"3".repeat(64));
        state
            .store
            .configure(other_release.clone(), Some(settings()))
            .unwrap();
        admit_succeeded(&state, &other_release, "other-release-request");

        let other_app_id = format!("a{}", "4".repeat(32));
        std::fs::create_dir_all(root.path().join(&other_app_id)).unwrap();
        let other_app = AppAgentState::open(root.path(), &other_app_id).unwrap();
        let other_app_owner = make_owner(&other_app_id, &"2".repeat(64));
        other_app
            .store
            .configure(other_app_owner.clone(), Some(settings()))
            .unwrap();
        admit_succeeded(&other_app, &other_app_owner, "other-app-request");

        let all = state.store.list(&owner, None, 100).unwrap();
        let first = task_inventory_page(&state, &owner, None, 2).unwrap();
        let first_tasks = first["tasks"].as_array().unwrap();
        assert_eq!(first_tasks.len(), 2);
        assert_eq!(
            first["nextCursor"].as_str(),
            Some(all[1].request_id.as_str())
        );
        assert_eq!(
            first_tasks[0]["runId"].as_str(),
            Some(all[0].request_id.as_str())
        );
        assert_eq!(
            first_tasks[1]["runId"].as_str(),
            Some(all[1].request_id.as_str())
        );

        let second = task_inventory_page(&state, &owner, first["nextCursor"].as_str(), 2).unwrap();
        let second_tasks = second["tasks"].as_array().unwrap();
        assert_eq!(second_tasks.len(), 1);
        assert_eq!(
            second_tasks[0]["runId"].as_str(),
            Some(all[2].request_id.as_str())
        );
        assert!(second.get("nextCursor").is_none());
        assert!(all
            .iter()
            .all(|task| task.request_id != "other-release-request"));
        assert!(all
            .iter()
            .all(|task| task.request_id != "other-app-request"));
    }

    #[test]
    fn chat_create_and_status_use_an_app_scoped_exact_request_identity() {
        let root = tempfile::tempdir().unwrap();
        let app = format!("a{}", "5".repeat(32));
        let other = format!("a{}", "6".repeat(32));
        std::fs::create_dir_all(root.path().join(&app)).unwrap();
        std::fs::create_dir_all(root.path().join(&other)).unwrap();
        let state = AppAgentState::open(root.path(), &app).unwrap();
        let other_state = AppAgentState::open(root.path(), &other).unwrap();
        let id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let args = json!({"requestId":id, "title":"  Budget  "});
        assert_eq!(
            chat_creation_value(&state.chats, &args, true).unwrap(),
            Value::Null
        );
        let created = chat_creation_value(&state.chats, &args, false).unwrap();
        assert_eq!(created["chatId"], id);
        assert_eq!(
            chat_creation_value(&state.chats, &args, false).unwrap(),
            created
        );
        assert_eq!(
            chat_creation_value(&state.chats, &args, true).unwrap(),
            created
        );
        assert_eq!(
            chat_creation_value(&other_state.chats, &args, true).unwrap(),
            Value::Null
        );
        let other_created = chat_creation_value(&other_state.chats, &args, false).unwrap();
        assert_eq!(other_created["chatId"], id);

        let changed_title = json!({"requestId":id, "title":"Budget"});
        assert_eq!(
            chat_creation_value(&state.chats, &changed_title, false).unwrap_err(),
            "chat_create_conflict"
        );
        assert_eq!(
            chat_creation_value(&state.chats, &changed_title, true).unwrap_err(),
            "chat_create_conflict"
        );
        state.chats.delete(id).unwrap();
        assert_eq!(
            chat_creation_value(&state.chats, &args, false).unwrap_err(),
            "chat_create_deleted"
        );
        assert_eq!(
            chat_creation_value(&state.chats, &args, true).unwrap_err(),
            "chat_create_deleted"
        );
        assert_eq!(
            chat_creation_value(&other_state.chats, &args, true).unwrap(),
            other_created
        );
    }

    #[test]
    fn chat_create_and_status_reject_malformed_or_ambiguous_arguments() {
        let root = tempfile::tempdir().unwrap();
        let app = format!("a{}", "5".repeat(32));
        std::fs::create_dir_all(root.path().join(&app)).unwrap();
        let state = AppAgentState::open(root.path(), &app).unwrap();
        let id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let invalid = [
            json!({}),
            json!({"requestId":null}),
            json!({"requestId":"AAAAAAAA-AAAA-4AAA-8AAA-AAAAAAAAAAAA"}),
            json!({"requestId":"aaaaaaaaaaaa4aaa8aaaaaaaaaaaaaaa"}),
            json!({"requestId":"aaaaaaaa-aaaa-1aaa-8aaa-aaaaaaaaaaaa"}),
            json!({"requestId":"aaaaaaaa-aaaa-4aaa-0aaa-aaaaaaaaaaaa"}),
            json!({"requestId":id, "title":null}),
            json!({"requestId":id, "title":true}),
            json!({"requestId":id, "title":""}),
            json!({"requestId":id, "title":"unsafe\nname"}),
            json!({"requestId":id, "title":"x".repeat(513)}),
            json!({"requestId":id, "unknown":true}),
        ];
        for value in invalid {
            for status_only in [false, true] {
                assert_eq!(
                    chat_creation_value(&state.chats, &value, status_only).unwrap_err(),
                    "invalid_chat"
                );
            }
        }
        assert_eq!(
            chat_creation_value(&state.chats, &json!({"requestId":id}), true).unwrap(),
            Value::Null
        );
    }

    #[test]
    fn host_chat_inventory_returns_named_filtered_pages_without_duplicates() {
        let root = tempfile::tempdir().unwrap();
        let app_id = format!("a{}", "5".repeat(32));
        std::fs::create_dir_all(root.path().join(&app_id)).unwrap();
        let state = AppAgentState::open(root.path(), &app_id).unwrap();
        let chats = (0..5)
            .map(|index| {
                state
                    .chats
                    .create(
                        &uuid::Uuid::new_v4().to_string(),
                        Some(&format!("chat-{index}")),
                        &uuid::Uuid::new_v4().to_string(),
                    )
                    .unwrap()
            })
            .collect::<Vec<_>>();
        state.chats.archive(&chats[2].chat_id).unwrap();
        let expected = state.chats.list(None, 100, false).unwrap();

        let first = chat_inventory_page(&state, None, 2, false).unwrap();
        let first_chats = first["chats"].as_array().unwrap();
        assert_eq!(first_chats.len(), 2);
        assert_eq!(
            first_chats[0]["chatId"].as_str(),
            Some(expected[0].chat_id.as_str())
        );
        assert_eq!(
            first_chats[1]["chatId"].as_str(),
            Some(expected[1].chat_id.as_str())
        );
        assert_eq!(
            first["nextCursor"].as_str(),
            Some(expected[1].chat_id.as_str())
        );

        let second = chat_inventory_page(&state, first["nextCursor"].as_str(), 2, false).unwrap();
        let second_chats = second["chats"].as_array().unwrap();
        assert_eq!(second_chats.len(), 2);
        assert_eq!(
            second_chats[0]["chatId"].as_str(),
            Some(expected[2].chat_id.as_str())
        );
        assert_eq!(
            second_chats[1]["chatId"].as_str(),
            Some(expected[3].chat_id.as_str())
        );
        assert!(second.get("nextCursor").is_none());
        assert!(!expected.iter().any(|chat| chat.chat_id == chats[2].chat_id));

        let archived = chat_inventory_page(&state, None, 100, true).unwrap();
        assert_eq!(archived["chats"].as_array().unwrap().len(), 5);
        assert!(archived.get("nextCursor").is_none());
    }
}

fn required_string<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("invalid_{key}"))
}

fn checked_chat_transcript_exists(path: &Path) -> Result<bool, String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => Ok(true),
        Ok(_) => Err("chat_transcript_unavailable".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err("chat_transcript_unavailable".into()),
    }
}

fn task_access_error(error: local_app::v2::AppError) -> &'static str {
    match error {
        local_app::v2::AppError::NotFound(_) => "task_not_found",
        _ => "agent_storage_unavailable",
    }
}

fn chat_creation_value(
    chats: &ChatStore,
    args: &Value,
    status_only: bool,
) -> Result<Value, String> {
    reject_unknown(args, &["requestId", "title"]).map_err(|_| "invalid_chat")?;
    let request_id = args
        .get("requestId")
        .and_then(Value::as_str)
        .ok_or("invalid_chat")?;
    let uuid = uuid::Uuid::parse_str(request_id).map_err(|_| "invalid_chat")?;
    if uuid.to_string() != request_id
        || uuid.get_version_num() != 4
        || uuid.get_variant() != uuid::Variant::RFC4122
    {
        return Err("invalid_chat".into());
    }
    let title = match args.get("title") {
        None => None,
        Some(Value::String(title)) => Some(title.as_str()),
        Some(_) => return Err("invalid_chat".into()),
    };
    let map_error = |error| match error {
        local_app::v2::AppError::ChatCreateConflict => "chat_create_conflict",
        local_app::v2::AppError::ChatCreateDeleted => "chat_create_deleted",
        local_app::v2::AppError::ChatStorageUncertain => "chat_storage_uncertain",
        local_app::v2::AppError::Invalid(_) => "invalid_chat",
        _ => "chat_storage_unavailable",
    };
    if status_only {
        let status = chats
            .creation_status(request_id, title)
            .map_err(map_error)?;
        serde_json::to_value(status).map_err(|_| "invalid_chat".into())
    } else {
        let chat = chats
            .create(request_id, title, &uuid::Uuid::new_v4().to_string())
            .map_err(map_error)?;
        serde_json::to_value(chat).map_err(|_| "invalid_chat".into())
    }
}

fn reject_unknown(value: &Value, allowed: &[&str]) -> Result<(), String> {
    let Some(object) = value.as_object() else {
        return Err("invalid_agent_arguments".into());
    };
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err("invalid_agent_arguments".into());
    }
    Ok(())
}

fn validate_request_id(request_id: &str) -> Result<(), String> {
    if request_id.is_empty() || request_id.len() > 128 || request_id.chars().any(char::is_control) {
        return Err("invalid_request".into());
    }
    Ok(())
}

fn decoded_size(base64: &str) -> u64 {
    let padding = base64
        .as_bytes()
        .iter()
        .rev()
        .take_while(|byte| **byte == b'=')
        .count();
    base64
        .len()
        .saturating_div(4)
        .saturating_mul(3)
        .saturating_sub(padding) as u64
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn usage_value(cost: &CostSnapshot) -> Value {
    json!({"inputTokens":cost.input_tokens,"outputTokens":cost.output_tokens,"costUsd":cost.total_usd,"costEstimated":!cost.unknown_models,"turns":cost.api_calls})
}

#[cfg(test)]
mod progress_tests {
    use super::protocol_retry_tests::{chat_control, settings, test_host};
    use super::*;

    fn running_control(root: &Path) -> Control {
        running_control_with_protocol(root, false)
    }

    fn running_control_with_protocol(root: &Path, protocol: bool) -> Control {
        let app_id = format!("a{}", "6".repeat(32));
        std::fs::create_dir_all(root.join(&app_id)).unwrap();
        let state = AppAgentState::open(root, &app_id).unwrap();
        let owner = Owner {
            app_id,
            release_digest: "2".repeat(64),
            invocation_id: "progress-test".into(),
        };
        let settings: Settings = serde_json::from_value(json!({
            "model":{"providerId":"provider","modelId":"model"},
            "tools":{},"skills":[],"background":false,"maxTurns":4,"timeoutMs":30000
        }))
        .unwrap();
        state
            .store
            .configure(owner.clone(), Some(settings))
            .unwrap();
        let mut input: Input = serde_json::from_value(json!({
            "requestId":"progress-run",
            "prompt":"check progress"
        }))
        .unwrap();
        if protocol {
            input.protocol_thread_id = Some("progress-thread".into());
        }
        let admission = state.store.admit(&owner, &input, &[]).unwrap();
        let id = admission.task.run_id;
        state
            .store
            .update(&owner, &id, State::Running, None, None, None)
            .unwrap();
        let (chat_persisted, _) = watch::channel(None);
        let control = Control {
            owner,
            revision: 1,
            request_id: input.request_id,
            id,
            protocol_thread_id: input.protocol_thread_id,
            cancel: CancellationToken::new(),
            finished: AtomicBool::new(false),
            done: Notify::new(),
            lease: Mutex::new(None),
            interaction: AsyncMutex::new(None),
            store: state.store,
            chats: state.chats,
            chat_id: None,
            user_message_id: None,
            chat_orchestrator: Mutex::new(None),
            chat_persisted,
            chat_commit: AsyncMutex::new(None),
            attachment_metadata: Vec::new(),
            assistant_message_ids: AsyncMutex::new(HashSet::new()),
            assistant_display_text: AsyncMutex::new(HashMap::new()),
            protocol_events_failed: AtomicBool::new(false),
            native_output: NativeOutputProgress::default(),
            text: AsyncMutex::new(String::new()),
            model_failed: AtomicBool::new(false),
            model_failure: Mutex::new(None),
            timed_out: AtomicBool::new(false),
            usage: AsyncMutex::new(None),
            fail_after_final_ledger_before_chat_authority: AtomicBool::new(false),
            after_final_ledger_error_barrier: Mutex::new(None),
        };
        if protocol {
            control.store.append_event(
                &control.owner,
                &control.id,
                json!({"type":"RUN_STARTED","threadId":"progress-thread","runId":"progress-run"}),
            ).unwrap();
        }
        control
    }

    pub(super) async fn test_wake_host(
        root: &Path,
    ) -> (Arc<AppAgentHost>, Arc<MobileLocalAppServiceHandle>) {
        use crate::mobile::test_support::{
            test_config, CollectingPermissionSink, FakeListener, HostFakePlatform,
        };
        let cfg = test_config(root);
        let runtime = crate::mobile::host::build_mobile(
            cfg.clone(),
            Arc::new(HostFakePlatform::new(root.to_path_buf())),
            Arc::new(FakeListener::default()),
            Arc::new(CollectingPermissionSink::default()),
        )
        .await
        .unwrap();
        // The service requires an ordinary Linux user directory name.
        let service_home = root.join("user").join(branding::DOT_DIR);
        let service = MobileLocalAppServiceHandle::new(service_home, None, "ios".into()).unwrap();
        let host = AppAgentHost::new(
            &service,
            runtime.app_agent_factory.clone(),
            &cfg.lingxi_home,
        )
        .unwrap();
        (host, service)
    }

    #[tokio::test]
    async fn unavailable_app_storage_does_not_block_other_apps_or_modify_evidence() {
        let root = tempfile::tempdir().unwrap();
        let (initial_host, service) = test_wake_host(root.path()).await;
        let mut published_apps = Vec::new();
        for name in [
            "old-index",
            "corrupt-index",
            "old-agent",
            "bad-chat-root",
            "healthy",
        ] {
            let source = root.path().join(name);
            std::fs::create_dir(&source).unwrap();
            std::fs::write(source.join("index.html"), "fixture").unwrap();
            std::fs::write(
                source.join("lingxi-app.json"),
                json!({
                    "schema":2,"name":name,"entry":{"type":"static","path":"index.html"},
                    "targets":[{"platform":"ios","environment":"web"}],
                    "capabilities":["agent"],"agent":{"tools":[]}
                })
                .to_string(),
            )
            .unwrap();
            let published = service
                .request_value(json!({"op":"publish", "expectedRevision":0, "sourcePath":source}))
                .await
                .unwrap();
            published_apps.push((
                published["record"]["appId"].as_str().unwrap().to_owned(),
                published["record"]["currentRevision"].as_u64().unwrap(),
            ));
        }

        let apps_root = service.library.root().join("apps");
        let mut evidence = Vec::new();
        for (index, (app_id, _)) in published_apps.iter().take(4).enumerate() {
            let state = AppAgentState::open(&apps_root, app_id).unwrap();
            let owner = Owner {
                app_id: app_id.clone(),
                release_digest: "2".repeat(64),
                invocation_id: "before-restart".into(),
            };
            let settings: Settings = serde_json::from_value(json!({
                "model":{"providerId":"provider","modelId":"model"},"tools":{},"skills":[],
                "background":false,"maxTurns":4,"timeoutMs":30000
            }))
            .unwrap();
            state
                .store
                .configure(owner.clone(), Some(settings))
                .unwrap();
            let mut input: Input = serde_json::from_value(json!({
                "requestId":"existing-run", "prompt":"preserve this run"
            }))
            .unwrap();
            input.protocol_thread_id = Some("existing-thread".into());
            let task = state.store.admit(&owner, &input, &[]).unwrap().task;
            state
                .store
                .update(&owner, &task.run_id, State::Running, None, None, None)
                .unwrap();
            state
                .store
                .append_event(
                    &owner,
                    &task.run_id,
                    json!({
                        "type":"RUN_STARTED", "threadId":"existing-thread", "runId":"existing-run"
                    }),
                )
                .unwrap();
            let chat_id = uuid::Uuid::new_v4().to_string();
            let session_id = uuid::Uuid::new_v4().to_string();
            state.chats.create(&chat_id, None, &session_id).unwrap();
            let transcript = state.chats.transcript_path(&chat_id).unwrap();
            std::fs::write(&transcript, b"preserve prior transcript\n").unwrap();
            let index_path = state.root.join("chats-index.json");
            let mut chat_index: Value =
                serde_json::from_slice(&std::fs::read(&index_path).unwrap()).unwrap();
            if index == 0 {
                chat_index.as_object_mut().unwrap().remove("creations");
            } else if index == 1 {
                chat_index["creations"] = json!({});
            }
            std::fs::write(&index_path, serde_json::to_vec(&chat_index).unwrap()).unwrap();
            let ledger_path = state.root.join("tasks/agent-tasks.json");
            let events_path = state
                .root
                .join("tasks/protocol-events")
                .join(format!("{}.jsonl", task.run_id));
            let transcript = if index == 3 {
                let chat_root = state.root.join("chats");
                let displaced = state.root.join("preserved-chat-data");
                std::fs::rename(&chat_root, &displaced).unwrap();
                std::fs::write(&chat_root, b"not a directory").unwrap();
                evidence.push((chat_root.clone(), std::fs::read(&chat_root).unwrap()));
                displaced.join(chat_id).join(format!("{session_id}.jsonl"))
            } else {
                transcript
            };
            for path in [
                index_path,
                ledger_path,
                events_path,
                transcript,
                state.root.join("protocol-store.json"),
            ] {
                evidence.push((path.clone(), std::fs::read(&path).unwrap()));
            }
        }
        let old_agent = apps_root.join(&published_apps[2].0).join("agent");
        std::fs::create_dir_all(old_agent.join("chats")).unwrap();
        let old_index = old_agent.join("chats-index.json");
        let old_transcript = old_agent.join("chats/previous-transcript.jsonl");
        std::fs::write(&old_index, b"legacy index bytes").unwrap();
        std::fs::write(&old_transcript, b"legacy transcript bytes").unwrap();
        for path in [old_index, old_transcript] {
            evidence.push((path.clone(), std::fs::read(&path).unwrap()));
        }

        let factory = initial_host.factory.clone();
        drop(initial_host);
        let host = AppAgentHost::new(&service, factory, root.path()).unwrap();
        assert_eq!(host.apps.lock().unwrap().len(), 1);
        for (index, expected) in [
            "chat_legacy_index_unsupported",
            "chat_storage_uncertain",
            "agent_legacy_store_unsupported",
            "chat_storage_uncertain",
        ]
        .into_iter()
        .enumerate()
        {
            let (app_id, revision) = &published_apps[index];
            assert_eq!(host.app_state(app_id).err().as_deref(), Some(expected));
            assert_eq!(
                host.host(
                    json!({"op":"wake.reserve", "appId":app_id, "revision":revision,
                    "runId":"new-run", "receiveTimeoutMs":120000})
                )
                .await
                .err()
                .as_deref(),
                Some(expected)
            );
        }
        for (path, before) in evidence {
            assert_eq!(std::fs::read(path).unwrap(), before);
        }
        assert!(old_agent.join("agui-v1/protocol-store.json").is_file());

        let (app_id, revision) = &published_apps[4];
        let (owner, _, _) = host
            .current_release(app_id, *revision, "healthy")
            .await
            .unwrap();
        let settings: Settings = serde_json::from_value(json!({
            "model":{"providerId":"provider","modelId":"model"},"tools":{},"skills":[],
            "background":false,"maxTurns":4,"timeoutMs":30000
        }))
        .unwrap();
        let state = host.app_state(app_id).unwrap();
        state
            .store
            .configure(owner.clone(), Some(settings))
            .unwrap();
        let receipt = host
            .host(
                json!({"op":"wake.reserve", "appId":app_id, "revision":revision,
            "runId":"healthy-run", "receiveTimeoutMs":120000}),
            )
            .await
            .unwrap();
        assert_eq!(receipt["created"], true);
        host.force_wake_policy.store(true, Ordering::Release);
        let mut input: Input = serde_json::from_value(json!({
            "requestId":"healthy-run", "prompt":"healthy app remains usable"
        }))
        .unwrap();
        input.protocol_thread_id = Some("healthy-thread".into());
        let id = host
            .start_task(
                owner.clone(),
                *revision,
                &[],
                &[],
                input,
                None,
                None,
                CancellationToken::new(),
                true,
                false,
            )
            .await
            .unwrap();
        assert_eq!(
            state
                .store
                .get_by_request(&owner, "healthy-run")
                .unwrap()
                .run_id,
            id
        );
        tokio::time::timeout(std::time::Duration::from_secs(10), host.stop_all())
            .await
            .unwrap()
            .unwrap();
    }

    fn configured_wake_fixture(host: &AppAgentHost) -> (Owner, Input, WakeKey, Arc<AppAgentState>) {
        let app_id = format!("a{}", "8".repeat(32));
        std::fs::create_dir_all(host.apps_root.join(&app_id)).unwrap();
        let state = host.app_state(&app_id).unwrap();
        let owner = Owner {
            app_id,
            release_digest: "2".repeat(64),
            invocation_id: "fixture-page".into(),
        };
        let settings: Settings = serde_json::from_value(json!({
            "model":{"providerId":"provider","modelId":"model"},"tools":{},"skills":[],
            "background":false,"maxTurns":4,"timeoutMs":30000
        }))
        .unwrap();
        state
            .store
            .configure(owner.clone(), Some(settings))
            .unwrap();
        let mut input: Input =
            serde_json::from_value(json!({"requestId":"wake-a", "prompt":"fixture"})).unwrap();
        input.protocol_thread_id = Some("fixture-thread".into());
        let key = WakeKey {
            app_id: owner.app_id.clone(),
            revision: 1,
            request_id: input.request_id.clone(),
        };
        host.wake_reservations
            .lock()
            .unwrap()
            .reserve(key.clone(), now_millis(), 120_000)
            .unwrap();
        host.force_wake_policy.store(true, Ordering::Release);
        (owner, input, key, state)
    }

    #[cfg(unix)]
    #[test]
    fn chat_transcript_only_treats_a_missing_file_as_new() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("missing.jsonl");
        assert!(!checked_chat_transcript_exists(&missing).unwrap());
        let regular = root.path().join("regular.jsonl");
        std::fs::write(&regular, b"existing transcript").unwrap();
        assert!(checked_chat_transcript_exists(&regular).unwrap());
        let directory = root.path().join("directory.jsonl");
        std::fs::create_dir(&directory).unwrap();
        assert_eq!(
            checked_chat_transcript_exists(&directory).unwrap_err(),
            "chat_transcript_unavailable"
        );
        let link = root.path().join("link.jsonl");
        symlink(&regular, &link).unwrap();
        assert_eq!(
            checked_chat_transcript_exists(&link).unwrap_err(),
            "chat_transcript_unavailable"
        );
        let dangling = root.path().join("dangling.jsonl");
        symlink(&missing, &dangling).unwrap();
        assert_eq!(
            checked_chat_transcript_exists(&dangling).unwrap_err(),
            "chat_transcript_unavailable"
        );

        let inaccessible = root.path().join("inaccessible");
        std::fs::create_dir(&inaccessible).unwrap();
        let hidden = inaccessible.join("hidden.jsonl");
        std::fs::write(&hidden, b"preserved").unwrap();
        std::fs::set_permissions(&inaccessible, std::fs::Permissions::from_mode(0o000)).unwrap();
        let metadata_denied = std::fs::symlink_metadata(&hidden).is_err();
        let result = checked_chat_transcript_exists(&hidden);
        std::fs::set_permissions(&inaccessible, std::fs::Permissions::from_mode(0o700)).unwrap();
        if metadata_denied {
            assert_eq!(result.unwrap_err(), "chat_transcript_unavailable");
        } else {
            // Some privileged test environments bypass directory mode bits.
            assert!(result.unwrap());
        }
        assert_eq!(std::fs::read(&hidden).unwrap(), b"preserved");
    }

    #[tokio::test]
    async fn stopped_page_waiting_for_admission_cannot_consume_a_later_wake() {
        let root = tempfile::tempdir().unwrap();
        let (host, _service) = test_wake_host(root.path()).await;
        let (owner, input, key, state) = configured_wake_fixture(&host);
        let cancellation = CancellationToken::new();
        let gate = host.admission.lock().await;
        let mut stop = Box::pin(host.stop_app(&owner.app_id));
        let mut start = Box::pin(host.start_task(
            owner.clone(),
            1,
            &[],
            &[],
            input,
            None,
            None,
            cancellation.clone(),
            true,
            false,
        ));
        assert!(matches!(
            futures::poll!(stop.as_mut()),
            std::task::Poll::Pending
        ));
        assert!(matches!(
            futures::poll!(start.as_mut()),
            std::task::Poll::Pending
        ));
        cancellation.cancel(); // stop cancels the invocation before entering admission
        drop(gate);
        stop.await.unwrap();
        // Simulate a new handoff for the same tuple after Stop, before the
        // previously queued page request is polled again.
        host.wake_reservations
            .lock()
            .unwrap()
            .reserve(key.clone(), now_millis(), 120_000)
            .unwrap();
        assert_eq!(start.await.unwrap_err(), "app_invocation_closed");
        let mut registry = host.wake_reservations.lock().unwrap();
        assert!(registry.available(&key, now_millis()));
        drop(registry);
        assert!(state
            .store
            .task_id_for_request(&owner, &key.request_id)
            .is_err());
        assert!(host.active.lock().await.is_empty());
    }

    #[tokio::test]
    async fn unavailable_started_event_schema_does_not_admit_or_claim_a_wake() {
        let root = tempfile::tempdir().unwrap();
        let (host, _service) = test_wake_host(root.path()).await;
        let (owner, input, key, state) = configured_wake_fixture(&host);
        host.fail_started_event_schema
            .store(true, Ordering::Release);
        let result = host
            .start_task(
                owner.clone(),
                1,
                &[],
                &[],
                input,
                None,
                None,
                CancellationToken::new(),
                true,
                false,
            )
            .await;
        assert_eq!(result.unwrap_err(), "agent_storage_unavailable");
        assert!(state
            .store
            .task_id_for_request(&owner, &key.request_id)
            .is_err());
        assert!(host
            .wake_reservations
            .lock()
            .unwrap()
            .available(&key, now_millis()));
        assert!(host.active.lock().await.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_started_event_write_clears_the_consumed_wake() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let (host, _service) = test_wake_host(root.path()).await;
        let (owner, input, key, state) = configured_wake_fixture(&host);
        let events = state.root.join("tasks/protocol-events");
        // The ledger remains writable; the actual RUN_STARTED event file does not.
        std::fs::set_permissions(&events, std::fs::Permissions::from_mode(0o500)).unwrap();
        let result = host
            .start_task(
                owner.clone(),
                1,
                &[],
                &[],
                input,
                None,
                None,
                CancellationToken::new(),
                true,
                false,
            )
            .await;
        std::fs::set_permissions(&events, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(result.unwrap_err(), "agent_storage_unavailable");
        assert!(!host
            .wake_reservations
            .lock()
            .unwrap()
            .reservations
            .contains_key(&key));
        let task = state.store.get_by_request(&owner, &key.request_id).unwrap();
        assert_eq!(task.state, State::Interrupted);
        assert_eq!(
            task.error.as_ref().unwrap().code,
            "protocol_event_storage_failed"
        );
        assert!(state
            .store
            .protocol_events(&owner, &key.request_id, 0)
            .is_err());
        assert!(host.active.lock().await.is_empty());
    }

    #[tokio::test]
    async fn failed_started_event_preserves_partial_or_complete_bytes_and_isolates_the_run() {
        let started = json!({"type":"RUN_STARTED", "protocolVersion":"1.0",
            "threadId":"fixture-thread", "runId":"wake-a"});
        assert!(local_app::v2::agent::protocol::validate_event(&started).is_ok());
        let mut complete = serde_json::to_vec(&json!({"sequence":1, "event":started})).unwrap();
        complete.push(b'\n');
        for bytes in [b"{\"sequence\":1,\"event\":".to_vec(), complete] {
            let root = tempfile::tempdir().unwrap();
            let (host, _service) = test_wake_host(root.path()).await;
            let (owner, input, key, state) = configured_wake_fixture(&host);
            let saved = bytes.clone();
            *host.before_started_event.lock().unwrap() = Some(Box::new(move |root, id| {
                std::fs::write(
                    root.join("tasks/protocol-events")
                        .join(format!("{id}.jsonl")),
                    bytes,
                )
                .unwrap();
            }));
            let result = host
                .start_task(
                    owner.clone(),
                    1,
                    &[],
                    &[],
                    input.clone(),
                    None,
                    None,
                    CancellationToken::new(),
                    true,
                    false,
                )
                .await;
            assert_eq!(result.unwrap_err(), "agent_storage_unavailable");
            let task = state.store.get_by_request(&owner, &key.request_id).unwrap();
            assert_eq!(task.state, State::Interrupted);
            assert_eq!(
                task.error.as_ref().unwrap().code,
                "protocol_event_storage_failed"
            );
            assert_eq!(
                std::fs::read(
                    state
                        .root
                        .join("tasks/protocol-events")
                        .join(format!("{}.jsonl", task.run_id))
                )
                .unwrap(),
                saved
            );
            assert!(state
                .store
                .protocol_events(&owner, &key.request_id, 0)
                .is_err());
            assert!(!host
                .wake_reservations
                .lock()
                .unwrap()
                .reservations
                .contains_key(&key));
            assert!(host.active.lock().await.is_empty());
            let mut later = input;
            later.request_id = "independent-run".into();
            assert!(state.store.admit(&owner, &later, &[]).unwrap().start);
        }
    }

    pub(super) fn complete_protocol_chat_turn(
        state: &AppAgentState,
        owner: &Owner,
        chat_id: &str,
        input: Value,
        assistant_id: &str,
        answer: &str,
    ) -> (String, Value, Value) {
        let (normalized, normalized_chat_id) =
            normalize_page_run_input(state, input, Some(chat_id)).unwrap();
        let admission = state
            .store
            .admit_scoped(
                owner,
                &normalized.input,
                &[],
                &[],
                normalized_chat_id.as_deref(),
                true,
            )
            .unwrap();
        let task_id = admission.task.run_id;
        state
            .store
            .update(owner, &task_id, State::Running, None, None, None)
            .unwrap();
        state
            .store
            .append_event(
                owner,
                &task_id,
                json!({
                    "type":"RUN_STARTED",
                    "protocolVersion":"1.0",
                    "threadId":chat_id,
                    "runId":normalized.run_id
                }),
            )
            .unwrap();
        state.chats.start_turn(chat_id, &task_id).unwrap();
        let internal_user_id =
            MessageId::from_uuid(uuid::Uuid::parse_str(&task_id).unwrap()).to_string();
        state
            .chats
            .record_message(
                chat_id,
                ChatMessageMetadata {
                    message_id: internal_user_id,
                    run_id: task_id.clone(),
                    role: ChatRole::User,
                    created_at: now_millis(),
                    attachments: Vec::new(),
                },
            )
            .unwrap();
        state
            .chats
            .record_protocol_message(chat_id, &task_id, &normalized.user_message)
            .unwrap();
        let assistant = json!({"id":assistant_id,"role":"assistant","content":answer});
        state
            .chats
            .record_protocol_message(chat_id, &task_id, &assistant)
            .unwrap();
        commit_agent_terminal_for_test(
            &state.store,
            owner,
            &task_id,
            State::Succeeded,
            Some(TaskResult {
                text: answer.into(),
                output: None,
                attachments: Vec::new(),
                chat_id: Some(chat_id.into()),
                message_id: Some(assistant_id.into()),
            }),
            None,
        )
        .unwrap();
        let accepted = state.store.committed_protocol_assistants().unwrap();
        state.chats.set_committed_assistants(&accepted).unwrap();
        state.chats.finish_turn(chat_id, &task_id).unwrap();
        (task_id, normalized.user_message, assistant)
    }

    #[tokio::test]
    async fn concurrent_duplicate_waits_for_chat_initialization_and_fails_closed() {
        let root = tempfile::tempdir().unwrap();
        let (host, _service) = test_wake_host(root.path()).await;
        let app_id = format!("a{}", "9".repeat(32));
        std::fs::create_dir_all(host.apps_root.join(&app_id)).unwrap();
        let state = host.app_state(&app_id).unwrap();
        let owner = Owner {
            app_id,
            release_digest: "2".repeat(64),
            invocation_id: "concurrent-chat-init".into(),
        };
        let settings: Settings = serde_json::from_value(json!({
            "model":{"providerId":"provider","modelId":"model"},
            "tools":{},"skills":[],"background":false,"maxTurns":4,"timeoutMs":30000
        }))
        .unwrap();
        state
            .store
            .configure(owner.clone(), Some(settings))
            .unwrap();
        let chat = state
            .chats
            .create(
                &uuid::Uuid::new_v4().to_string(),
                None,
                &uuid::Uuid::new_v4().to_string(),
            )
            .unwrap();
        let raw = json!({
            "threadId":chat.chat_id,
            "runId":"concurrent-chat-init-run",
            "protocolVersion":"1.0",
            "messages":[{"id":"concurrent-chat-user","role":"user","content":"question"}]
        });
        let (normalized, chat_id) =
            normalize_page_run_input(&state, raw, Some(&chat.chat_id)).unwrap();
        let barrier = Arc::new(tokio::sync::Barrier::new(3));
        let chat_index = state.root.join("chats-index.json");
        let chat_index_backup = state.root.join("chats-index.test-backup");
        std::fs::rename(&chat_index, &chat_index_backup).unwrap();
        std::fs::create_dir(&chat_index).unwrap();
        let start_duplicate = |host: Arc<AppAgentHost>, barrier: Arc<tokio::sync::Barrier>| {
            let owner = owner.clone();
            let input = normalized.input.clone();
            let user_message = normalized.user_message.clone();
            let chat_id = chat_id.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                host.start_task(
                    owner,
                    1,
                    &[],
                    &[],
                    input,
                    Some(user_message),
                    chat_id,
                    CancellationToken::new(),
                    true,
                    true,
                )
                .await
            })
        };
        let first = start_duplicate(host.clone(), barrier.clone());
        let second = start_duplicate(host.clone(), barrier.clone());
        barrier.wait().await;
        let first = first.await.unwrap();
        let second = second.await.unwrap();

        assert_eq!(first.unwrap_err(), "chat_storage_unavailable");
        assert_eq!(second.unwrap_err(), "chat_storage_unavailable");
        assert!(host.active.lock().await.is_empty());
        assert!(matches!(
            state
                .store
                .get_by_request(&owner, "concurrent-chat-init-run"),
            Err(local_app::v2::AppError::NotFound(_))
        ));
        std::fs::remove_dir(&chat_index).unwrap();
        std::fs::rename(&chat_index_backup, &chat_index).unwrap();
        let recovered = AppAgentState::open(&host.apps_root, &owner.app_id).unwrap();
        assert!(matches!(
            recovered
                .store
                .get_by_request(&owner, "concurrent-chat-init-run"),
            Err(local_app::v2::AppError::NotFound(_))
        ));
        assert_eq!(
            recovered.chats.get(&chat.chat_id).unwrap().state,
            ChatState::Idle
        );
        let mut unrelated = normalized.input;
        unrelated.request_id = "unrelated-after-init-rollback".into();
        let admitted = recovered
            .store
            .admit_scoped(&owner, &unrelated, &[], &[], None, true)
            .unwrap();
        assert!(admitted.start);
    }

    #[tokio::test]
    async fn duplicate_start_without_active_control_rejects_failed_chat_run() {
        let root = tempfile::tempdir().unwrap();
        let (host, _service) = test_wake_host(root.path()).await;
        let app_id = format!("a{}", "6".repeat(32));
        std::fs::create_dir_all(host.apps_root.join(&app_id)).unwrap();
        let state = host.app_state(&app_id).unwrap();
        let owner = Owner {
            app_id,
            release_digest: "2".repeat(64),
            invocation_id: "inactive-chat-duplicate".into(),
        };
        state
            .store
            .configure(owner.clone(), Some(settings()))
            .unwrap();
        let chat = state
            .chats
            .create(
                &uuid::Uuid::new_v4().to_string(),
                None,
                &uuid::Uuid::new_v4().to_string(),
            )
            .unwrap();
        let raw = json!({
            "threadId":chat.chat_id,
            "runId":"inactive-failed-chat-run",
            "protocolVersion":"1.0",
            "messages":[{"id":"inactive-failed-user","role":"user","content":"hello"}]
        });
        let (normalized, normalized_chat) =
            normalize_page_run_input(&state, raw, Some(&chat.chat_id)).unwrap();
        let admitted = state
            .store
            .admit_scoped(
                &owner,
                &normalized.input,
                &[],
                &[],
                normalized_chat.as_deref(),
                true,
            )
            .unwrap();
        state
            .store
            .update(
                &owner,
                &admitted.task.run_id,
                State::Running,
                None,
                None,
                None,
            )
            .unwrap();
        state
            .store
            .append_event(
                &owner,
                &admitted.task.run_id,
                json!({
                    "type":"RUN_STARTED",
                    "protocolVersion":"1.0",
                    "threadId":chat.chat_id,
                    "runId":"inactive-failed-chat-run"
                }),
            )
            .unwrap();
        state
            .chats
            .start_turn(&chat.chat_id, &admitted.task.run_id)
            .unwrap();
        let user_message_id =
            MessageId::from_uuid(uuid::Uuid::parse_str(&admitted.task.run_id).unwrap()).to_string();
        state
            .chats
            .record_message(
                &chat.chat_id,
                ChatMessageMetadata {
                    message_id: user_message_id,
                    run_id: admitted.task.run_id.clone(),
                    role: ChatRole::User,
                    created_at: now_millis(),
                    attachments: Vec::new(),
                },
            )
            .unwrap();
        state
            .chats
            .record_protocol_message(
                &chat.chat_id,
                &admitted.task.run_id,
                &normalized.user_message,
            )
            .unwrap();
        commit_agent_terminal_for_test(
            &state.store,
            &owner,
            &admitted.task.run_id,
            State::Failed,
            None,
            Some("model_execution_failed".into()),
        )
        .unwrap();
        let before_events = state
            .store
            .protocol_events(&owner, "inactive-failed-chat-run", 0)
            .unwrap()
            .events;

        let result = host
            .start_task(
                owner.clone(),
                1,
                &[],
                &[],
                normalized.input,
                Some(normalized.user_message),
                Some(chat.chat_id.clone()),
                CancellationToken::new(),
                true,
                true,
            )
            .await;
        assert_eq!(result.unwrap_err(), "chat_storage_unavailable");
        assert_eq!(
            state
                .store
                .protocol_events(&owner, "inactive-failed-chat-run", 0)
                .unwrap()
                .events,
            before_events,
            "an inactive failed run must not be acknowledged or restarted"
        );
        assert!(host.active.lock().await.is_empty());
    }

    #[tokio::test]
    async fn duplicate_paths_reread_succeeded_task_after_active_control_removal() {
        let root = tempfile::tempdir().unwrap();
        let (host, service) = test_host(root.path()).await;
        let source = root.path().join("duplicate-terminal-race-source");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("index.html"), "fixture").unwrap();
        std::fs::write(
            source.join("lingxi-app.json"),
            json!({
                "schema":2,
                "name":"Duplicate terminal race test",
                "entry":{"type":"static","path":"index.html"},
                "targets":[{"platform":"ios","environment":"web"}],
                "capabilities":["agent"],
                "agent":{"tools":[]}
            })
            .to_string(),
        )
        .unwrap();
        let published = service
            .request_value(json!({
                "op":"publish",
                "expectedRevision":0,
                "sourcePath":source
            }))
            .await
            .unwrap();
        let app_id = published["record"]["appId"].as_str().unwrap().to_owned();
        let revision = published["record"]["currentRevision"].as_u64().unwrap();
        let (owner, _, _) = host
            .current_release(&app_id, revision, "duplicate-terminal-race")
            .await
            .unwrap();
        let state = host.app_state(&app_id).unwrap();
        state
            .store
            .configure(owner.clone(), Some(settings()))
            .unwrap();

        let seed_running_chat = |chat_id: &str, request_id: &str| {
            let raw = json!({
                "threadId":chat_id,
                "runId":request_id,
                "protocolVersion":"1.0",
                "messages":[{"id":format!("{request_id}-user"),"role":"user","content":"hello"}]
            });
            let (normalized, normalized_chat) =
                normalize_page_run_input(&state, raw, Some(chat_id)).unwrap();
            let admitted = state
                .store
                .admit_scoped(
                    &owner,
                    &normalized.input,
                    &[],
                    &[],
                    normalized_chat.as_deref(),
                    true,
                )
                .unwrap();
            let task_id = admitted.task.run_id;
            state
                .store
                .update(&owner, &task_id, State::Running, None, None, None)
                .unwrap();
            state
                .store
                .append_event(
                    &owner,
                    &task_id,
                    json!({
                        "type":"RUN_STARTED",
                        "protocolVersion":"1.0",
                        "threadId":chat_id,
                        "runId":request_id
                    }),
                )
                .unwrap();
            state.chats.start_turn(chat_id, &task_id).unwrap();
            let user_message_id =
                MessageId::from_uuid(uuid::Uuid::parse_str(&task_id).unwrap()).to_string();
            state
                .chats
                .record_message(
                    chat_id,
                    ChatMessageMetadata {
                        message_id: user_message_id.clone(),
                        run_id: task_id.clone(),
                        role: ChatRole::User,
                        created_at: now_millis(),
                        attachments: Vec::new(),
                    },
                )
                .unwrap();
            state
                .chats
                .record_protocol_message(chat_id, &task_id, &normalized.user_message)
                .unwrap();
            (
                task_id,
                normalized.input,
                normalized.user_message,
                user_message_id,
            )
        };
        let finish_chat = |chat_id: &str, task_id: &str, assistant_id: &str| {
            let assistant = json!({
                "id":assistant_id,
                "role":"assistant",
                "content":"committed answer"
            });
            state
                .chats
                .record_protocol_message(chat_id, task_id, &assistant)
                .unwrap();
            commit_agent_terminal_for_test(
                &state.store,
                &owner,
                task_id,
                State::Succeeded,
                Some(TaskResult {
                    text: "committed answer".into(),
                    output: None,
                    attachments: Vec::new(),
                    chat_id: Some(chat_id.into()),
                    message_id: Some(assistant_id.into()),
                }),
                None,
            )
            .unwrap();
            let accepted = state.store.committed_protocol_assistants().unwrap();
            state.chats.set_committed_assistants(&accepted).unwrap();
            state.chats.finish_turn(chat_id, task_id).unwrap();
        };

        // Exercise the normalized !admitted.start path. Its first task read
        // sees Running; the original commits and disappears while this caller
        // waits before active-map lookup. It must reread the terminal record.
        let first_chat = state
            .chats
            .create(
                &uuid::Uuid::new_v4().to_string(),
                None,
                &uuid::Uuid::new_v4().to_string(),
            )
            .unwrap();
        let (first_task_id, first_input, first_user, first_user_row) =
            seed_running_chat(&first_chat.chat_id, "fallback-task-race");
        let (first_persisted, _) = watch::channel(Some(Ok(())));
        let first_control = Arc::new(chat_control(
            owner.clone(),
            revision,
            first_input.request_id.clone(),
            first_task_id.clone(),
            first_chat.chat_id.clone(),
            first_user_row,
            state.store.clone(),
            state.chats.clone(),
            first_persisted,
        ));
        host.active
            .lock()
            .await
            .insert(first_task_id.clone(), first_control.clone());
        let (first_snapshot_tx, first_snapshot_rx) = oneshot::channel();
        let (first_release_tx, first_release_rx) = oneshot::channel();
        *host.duplicate_task_snapshot_pause.lock().unwrap() =
            Some((first_snapshot_tx, first_release_rx));
        let host_for_start = host.clone();
        let owner_for_start = owner.clone();
        let first_chat_id = first_chat.chat_id.clone();
        let first_duplicate = tokio::spawn(async move {
            host_for_start
                .start_task(
                    owner_for_start,
                    revision,
                    &[],
                    &[],
                    first_input,
                    Some(first_user),
                    Some(first_chat_id),
                    CancellationToken::new(),
                    true,
                    true,
                )
                .await
        });
        first_snapshot_rx.await.unwrap();
        let first_assistant_id = MessageId::new().to_string();
        finish_chat(&first_chat.chat_id, &first_task_id, &first_assistant_id);
        first_control.finished.store(true, Ordering::Release);
        host.active.lock().await.remove(&first_task_id);
        first_release_tx.send(()).unwrap();
        assert_eq!(first_duplicate.await.unwrap().unwrap(), first_task_id);

        // Exercise the pre-normalization duplicate path with the same stale
        // snapshot interleaving. The no-Control branch must validate the fresh
        // success and its complete terminal evidence before returning runId.
        let second_chat = state
            .chats
            .create(
                &uuid::Uuid::new_v4().to_string(),
                None,
                &uuid::Uuid::new_v4().to_string(),
            )
            .unwrap();
        let (second_task_id, _second_input, _second_user, second_user_row) =
            seed_running_chat(&second_chat.chat_id, "probe-task-race");
        let (second_persisted, _) = watch::channel(Some(Ok(())));
        let second_control = Arc::new(chat_control(
            owner.clone(),
            revision,
            "probe-task-race".into(),
            second_task_id.clone(),
            second_chat.chat_id.clone(),
            second_user_row,
            state.store.clone(),
            state.chats.clone(),
            second_persisted,
        ));
        host.active
            .lock()
            .await
            .insert(second_task_id.clone(), second_control.clone());
        let (second_snapshot_tx, second_snapshot_rx) = oneshot::channel();
        let (second_release_tx, second_release_rx) = oneshot::channel();
        *host.duplicate_task_snapshot_pause.lock().unwrap() =
            Some((second_snapshot_tx, second_release_rx));
        let host_for_probe = host.clone();
        let state_for_probe = state.clone();
        let owner_for_probe = owner.clone();
        let probe_cancel = CancellationToken::new();
        let second_chat_id = second_chat.chat_id.clone();
        let raw_input = json!({
            "threadId":second_chat_id.clone(),
            "runId":"probe-task-race",
            "protocolVersion":"1.0",
            "messages":[{"id":"probe-task-race-user","role":"user","content":"hello"}]
        });
        let duplicate_probe = tokio::spawn(async move {
            host_for_probe
                .duplicate_before_normalization(
                    &state_for_probe,
                    &owner_for_probe,
                    &raw_input,
                    Some(&second_chat_id),
                    &probe_cancel,
                    true,
                )
                .await
        });
        second_snapshot_rx.await.unwrap();
        let second_assistant_id = MessageId::new().to_string();
        finish_chat(&second_chat.chat_id, &second_task_id, &second_assistant_id);
        second_control.finished.store(true, Ordering::Release);
        host.active.lock().await.remove(&second_task_id);
        second_release_tx.send(()).unwrap();
        assert_eq!(
            duplicate_probe.await.unwrap().unwrap().as_deref(),
            Some("probe-task-race")
        );
    }

    #[tokio::test]
    async fn persistent_chat_structured_schema_is_rejected_before_admission() {
        let root = tempfile::tempdir().unwrap();
        let (host, _service) = test_wake_host(root.path()).await;
        let app_id = format!("a{}", "7".repeat(32));
        std::fs::create_dir_all(host.apps_root.join(&app_id)).unwrap();
        let state = host.app_state(&app_id).unwrap();
        let owner = Owner {
            app_id: app_id.clone(),
            release_digest: "2".repeat(64),
            invocation_id: "chat-structured-output-test".into(),
        };
        state
            .store
            .configure(owner.clone(), Some(settings()))
            .unwrap();
        let chat = state
            .chats
            .create(
                &uuid::Uuid::new_v4().to_string(),
                None,
                &uuid::Uuid::new_v4().to_string(),
            )
            .unwrap();
        let mut input: Input = serde_json::from_value(json!({
            "requestId":"chat-structured-output-run",
            "prompt":"return a structured answer"
        }))
        .unwrap();
        input.protocol_thread_id = Some(chat.chat_id.clone());
        input.protocol_history_digest =
            Some(local_app::v2::agent::protocol::history_fingerprint(&[]));
        input.output_schema = Some(json!({
            "type":"object",
            "properties":{"answer":{"type":"string"}},
            "required":["answer"]
        }));
        assert!(validate_chat_output_mode(None, &input).is_ok());

        assert_eq!(
            host.start_task(
                owner.clone(),
                1,
                &[],
                &[],
                input,
                Some(json!({"id":"chat-structured-user","role":"user","content":"answer"})),
                Some(chat.chat_id.clone()),
                CancellationToken::new(),
                true,
                true,
            )
            .await
            .unwrap_err(),
            "chat_structured_output_unsupported"
        );
        assert!(matches!(
            state
                .store
                .task_id_for_request(&owner, "chat-structured-output-run"),
            Err(local_app::v2::AppError::NotFound(_))
        ));
        assert_eq!(
            state.chats.get(&chat.chat_id).unwrap().state,
            ChatState::Idle
        );
        assert!(host.active.lock().await.is_empty());
        assert_eq!(
            std::fs::read_dir(state.root.join("tasks/protocol-events"))
                .unwrap()
                .count(),
            0
        );
    }

    #[tokio::test]
    async fn failed_interruption_persist_quarantines_then_recovers_original_event_evidence() {
        let root = tempfile::tempdir().unwrap();
        let (host, _service) = test_wake_host(root.path()).await;
        let (owner, input, key, state) = configured_wake_fixture(&host);
        *host.before_started_event.lock().unwrap() = Some(Box::new(move |root, id| {
            let tasks = root.join("tasks");
            std::fs::write(
                tasks.join("protocol-events").join(format!("{id}.jsonl")),
                b"{partial",
            )
            .unwrap();
            // Replacing the ledger path with a directory makes the later
            // quarantine write fail deterministically, including under a
            // privileged test runner which can bypass mode bits.
            let ledger = tasks.join("agent-tasks.json");
            std::fs::rename(&ledger, tasks.join("agent-tasks.test-backup")).unwrap();
            std::fs::create_dir(ledger).unwrap();
        }));
        let result = host
            .start_task(
                owner.clone(),
                1,
                &[],
                &[],
                input.clone(),
                None,
                None,
                CancellationToken::new(),
                true,
                false,
            )
            .await;
        assert_eq!(result.unwrap_err(), "agent_storage_unavailable");
        assert!(
            state.store.get_by_request(&owner, &key.request_id).is_err(),
            "uncertain in-memory task state must be quarantined"
        );
        assert!(!host
            .wake_reservations
            .lock()
            .unwrap()
            .reservations
            .contains_key(&key));
        assert!(host.active.lock().await.is_empty());

        let tasks = state.root.join("tasks");
        let ledger = tasks.join("agent-tasks.json");
        std::fs::remove_dir(&ledger).unwrap();
        std::fs::rename(tasks.join("agent-tasks.test-backup"), &ledger).unwrap();
        let event_path = std::fs::read_dir(tasks.join("protocol-events"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let event_bytes = std::fs::read(&event_path).unwrap();
        assert_eq!(event_bytes, b"{partial");

        let recovered = AgentStore::open(&tasks).unwrap();
        let task = recovered.get_by_request(&owner, &key.request_id).unwrap();
        assert_eq!(task.state, State::Interrupted);
        assert_eq!(
            task.error.as_ref().map(|error| error.code.as_str()),
            Some("protocol_event_storage_failed")
        );
        assert_eq!(std::fs::read(&event_path).unwrap(), event_bytes);

        let mut later = input;
        later.request_id = "independent-run".into();
        let independent = recovered.admit(&owner, &later, &[]).unwrap();
        assert!(independent.start);
    }

    #[tokio::test]
    async fn ios_wake_claim_does_not_override_duplicate_only_gate_after_restart() {
        let root = tempfile::tempdir().unwrap();
        let (host, service) = test_wake_host(root.path()).await;
        let (owner, input, key, state) = configured_wake_fixture(&host);
        let factory = host.factory.clone();
        assert!(host.has_wake_reservation(&owner.app_id, 1, &input.request_id));

        let error = host
            .start_task(
                owner.clone(),
                1,
                &[],
                &[],
                input.clone(),
                None,
                None,
                CancellationToken::new(),
                false,
                false,
            )
            .await
            .unwrap_err();
        assert_eq!(error, "host_wake_required");
        assert!(matches!(
            state.store.get_by_request(&owner, &key.request_id),
            Err(local_app::v2::AppError::NotFound(_))
        ));
        assert!(host.has_wake_reservation(&owner.app_id, 1, &input.request_id));
        assert!(host.active.lock().await.is_empty());
        assert_eq!(
            std::fs::read_dir(state.root.join("tasks/protocol-events"))
                .unwrap()
                .count(),
            0
        );

        drop(host);
        let reopened = AppAgentHost::new(&service, factory, root.path()).unwrap();
        let recovered_state = reopened.app_state(&owner.app_id).unwrap();
        assert!(matches!(
            recovered_state
                .store
                .get_by_request(&owner, &key.request_id),
            Err(local_app::v2::AppError::NotFound(_))
        ));
        assert!(reopened.active.lock().await.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_stream_event_never_appends_a_terminal_event() {
        use std::os::unix::fs::PermissionsExt;
        for fail_after_write in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let control = running_control_with_protocol(root.path(), true);
            let event_path = root
                .path()
                .join(&control.owner.app_id)
                .join("agent/agui-v1/tasks/protocol-events")
                .join(format!("{}.jsonl", control.id));
            let event = json!({"type":"TEXT_MESSAGE_START", "messageId":"assistant-one", "role":"assistant"});
            if fail_after_write {
                // Model an error returned by sync after all event bytes landed.
                control
                    .store
                    .append_event(&control.owner, &control.id, event)
                    .unwrap();
                control
                    .protocol_events_failed
                    .store(true, Ordering::Release);
            } else {
                std::fs::set_permissions(&event_path, std::fs::Permissions::from_mode(0o400))
                    .unwrap();
                assert!(control.append_protocol_event(event).is_err());
                std::fs::set_permissions(&event_path, std::fs::Permissions::from_mode(0o600))
                    .unwrap();
                assert!(control.protocol_events_failed.load(Ordering::Acquire));
            }
            let before = std::fs::read(&event_path).unwrap();
            assert_eq!(
                control
                    .commit_terminal(
                        State::Failed,
                        None,
                        None,
                        Some("agent_storage_unavailable".into()),
                    )
                    .unwrap_err(),
                "agent_storage_unavailable"
            );
            assert_eq!(
                std::fs::read(&event_path).unwrap(),
                before,
                "A failed event write must not be followed by another event"
            );
            assert!(control
                .store
                .protocol_events(&control.owner, &control.request_id, 0)
                .is_err());
            let recovered =
                AgentStore::open(event_path.parent().unwrap().parent().unwrap()).unwrap();
            let task = recovered
                .get_by_request(&control.owner, &control.request_id)
                .unwrap();
            assert_eq!(task.state, State::Interrupted);
            assert_eq!(task.error.as_ref().unwrap().code, "host_restarted");
            assert!(std::fs::read(&event_path).unwrap().starts_with(&before));
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn terminal_event_storage_failure_releases_cleaned_control_for_recovery() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let app_id = format!("a{}", "6".repeat(32));
        std::fs::create_dir_all(root.path().join(&app_id)).unwrap();
        let prior_state = AppAgentState::open(root.path(), &app_id).unwrap();
        let prior_owner = Owner {
            app_id: app_id.clone(),
            release_digest: "2".repeat(64),
            invocation_id: "progress-test".into(),
        };
        let prior_settings: Settings = serde_json::from_value(json!({
            "model":{"providerId":"provider","modelId":"model"},
            "tools":{},"skills":[],"background":false,"maxTurns":4,"timeoutMs":30000
        }))
        .unwrap();
        prior_state
            .store
            .configure(prior_owner.clone(), Some(prior_settings))
            .unwrap();
        let mut prior_input: Input =
            serde_json::from_value(json!({"requestId":"prior-success", "prompt":"fixture"}))
                .unwrap();
        prior_input.protocol_thread_id = Some("prior-success-thread".into());
        let prior = prior_state
            .store
            .admit(&prior_owner, &prior_input, &[])
            .unwrap()
            .task;
        prior_state
            .store
            .update(
                &prior_owner,
                &prior.run_id,
                State::Running,
                None,
                None,
                None,
            )
            .unwrap();
        prior_state
            .store
            .append_event(
                &prior_owner,
                &prior.run_id,
                json!({
                    "type":"RUN_STARTED", "protocolVersion":"1.0",
                    "threadId":"prior-success-thread", "runId":"prior-success"
                }),
            )
            .unwrap();
        commit_agent_terminal_for_test(
            &prior_state.store,
            &prior_owner,
            &prior.run_id,
            State::Succeeded,
            Some(TaskResult {
                text: "saved success".into(),
                output: None,
                attachments: Vec::new(),
                chat_id: None,
                message_id: Some("prior-assistant".into()),
            }),
            None,
        )
        .unwrap();
        drop(prior_state);

        let (host, _service) = test_wake_host(root.path()).await;
        let control = Arc::new(running_control_with_protocol(root.path(), true));
        let app_root = root.path().join(&control.owner.app_id);
        let event_path = app_root
            .join("agent/agui-v1/tasks")
            .join("protocol-events")
            .join(format!("{}.jsonl", control.id));
        let before = std::fs::read(&event_path).unwrap();
        let key = WakeKey {
            app_id: control.owner.app_id.clone(),
            revision: control.revision,
            request_id: control.request_id.clone(),
        };
        {
            let mut registry = host.wake_reservations.lock().unwrap();
            let now = now_millis();
            registry.reserve(key.clone(), now, 120_000).unwrap();
            assert!(registry.claim(&key, now));
            assert!(registry.consume(&key, now, &control.id));
        }
        host.active
            .lock()
            .await
            .insert(control.id.clone(), control.clone());
        let admission = Admission {
            task: control.store.get(&control.owner, &control.id).unwrap(),
            start: true,
            tools: BTreeMap::new(),
            skills: Vec::new(),
            chat_id: None,
        };
        let input: Input =
            serde_json::from_value(json!({"requestId":control.request_id, "prompt":"fixture"}))
                .unwrap();
        std::fs::set_permissions(&event_path, std::fs::Permissions::from_mode(0o400)).unwrap();
        host.clone()
            .execute(control.clone(), admission, input, app_root)
            .await;
        std::fs::set_permissions(&event_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(control.finished.load(Ordering::Acquire));
        assert!(!host.active.lock().await.contains_key(&control.id));
        assert!(!host
            .wake_reservations
            .lock()
            .unwrap()
            .reservations
            .contains_key(&key));
        assert!(control.store.get(&control.owner, &control.id).is_err());
        assert_eq!(std::fs::read(&event_path).unwrap(), before);
        host.stop_all().await.unwrap();

        let recovered = AppAgentState::open(root.path(), &control.owner.app_id).unwrap();
        let old_success = recovered
            .store
            .get_by_request(&prior_owner, "prior-success")
            .unwrap();
        assert_eq!(old_success.state, State::Succeeded);
        assert_eq!(old_success.result.as_ref().unwrap().text, "saved success");
        assert_eq!(
            recovered
                .store
                .protocol_events(&prior_owner, "prior-success", 0)
                .unwrap()
                .events
                .last()
                .unwrap()["type"],
            "RUN_FINISHED"
        );
        let interrupted = recovered
            .store
            .get_by_request(&prior_owner, "progress-run")
            .unwrap();
        assert_eq!(interrupted.state, State::Interrupted);
        assert!(interrupted.result.is_none());
        assert_eq!(
            interrupted.error.as_ref().unwrap().code,
            "protocol_commit_incomplete"
        );
        assert_eq!(std::fs::read(&event_path).unwrap(), before);

        let mut independent: Input = serde_json::from_value(
            json!({"requestId":"independent-after-reopen", "prompt":"fixture"}),
        )
        .unwrap();
        independent.protocol_thread_id = Some("independent-thread".into());
        assert!(
            recovered
                .store
                .admit(&prior_owner, &independent, &[])
                .unwrap()
                .start
        );
    }

    #[tokio::test]
    async fn native_wake_clear_fences_generations_and_reaps_running_task() {
        let root = tempfile::tempdir().unwrap();
        let (host, service) = test_wake_host(root.path()).await;
        let control = Arc::new(running_control(root.path()));
        let key = WakeKey {
            app_id: control.owner.app_id.clone(),
            revision: control.revision,
            request_id: control.request_id.clone(),
        };
        let (old, current) = {
            let mut registry = host.wake_reservations.lock().unwrap();
            let old = registry.reserve(key.clone(), 100, 10).unwrap();
            let current = registry.reserve(key.clone(), 110, 100).unwrap();
            assert!(registry.claim(&key, 111));
            assert!(registry.consume(&key, 111, &control.id));
            (old, current)
        };
        host.active
            .lock()
            .await
            .insert(control.id.clone(), control.clone());
        let clear = |id: &str| {
            json!({"op":"wake.clear", "appId":key.app_id,
            "revision":key.revision, "runId":key.request_id, "reservationId":id})
        };
        host.host(clear(&old.reservation_id)).await.unwrap();
        assert!(
            !control.cancel.is_cancelled(),
            "A stale receipt must not cancel the current run"
        );
        assert!(host
            .wake_reservations
            .lock()
            .unwrap()
            .reservations
            .contains_key(&key));

        // Simulate the production executor's terminal path after cancellation.
        let worker_host = host.clone();
        let worker_control = control.clone();
        let worker = tokio::spawn(async move {
            worker_control.cancel.cancelled().await;
            worker_host.active.lock().await.remove(&worker_control.id);
            worker_control.finished.store(true, Ordering::Release);
            worker_control.done.notify_waiters();
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            host.host(clear(&current.reservation_id)),
        )
        .await
        .expect("wake.clear must release active-map lock before cancel_ids")
        .unwrap();
        worker.await.unwrap();
        assert!(control.cancel.is_cancelled());
        assert!(host.active.lock().await.is_empty());
        host.host(clear(&current.reservation_id)).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), host.stop_all())
            .await
            .expect("stopped host must not retain a locked active map")
            .unwrap();
        // Replacement construction still succeeds after the real clear/reap path.
        let replacement = AppAgentHost::new(&service, host.factory.clone(), root.path()).unwrap();
        assert!(!replacement.shutdown.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn system_expiration_cancels_only_the_owned_wake_and_emits_run_error() {
        let root = tempfile::tempdir().unwrap();
        let (host, _) = test_wake_host(root.path()).await;
        let control = Arc::new(running_control_with_protocol(root.path(), true));
        let key = WakeKey {
            app_id: control.owner.app_id.clone(),
            revision: control.revision,
            request_id: control.request_id.clone(),
        };
        let (old, current) = {
            let mut registry = host.wake_reservations.lock().unwrap();
            let old = registry.reserve(key.clone(), 100, 10).unwrap();
            let current = registry.reserve(key.clone(), 110, 100).unwrap();
            assert!(registry.claim(&key, 111));
            assert!(registry.consume(&key, 111, &control.id));
            (old, current)
        };
        host.active
            .lock()
            .await
            .insert(control.id.clone(), control.clone());
        let clear = |reservation_id: &str, reason: Value| {
            json!({"op":"wake.clear", "appId":key.app_id, "revision":key.revision,
                "runId":key.request_id, "reservationId":reservation_id, "reason":reason})
        };
        assert_eq!(
            host.host(clear(&current.reservation_id, json!("user_cancelled")))
                .await
                .unwrap_err(),
            "invalid_wake_clear_reason"
        );
        host.host(clear(&old.reservation_id, json!("system_expiration")))
            .await
            .unwrap();
        assert!(
            !control.cancel.is_cancelled(),
            "An old token cannot affect the new run"
        );
        assert_eq!(
            control
                .store
                .get(&control.owner, &control.id)
                .unwrap()
                .state,
            State::Running
        );

        let worker_host = host.clone();
        let worker_control = control.clone();
        let worker = tokio::spawn(async move {
            worker_control.cancel.cancelled().await;
            let completed = TaskResult {
                text: "A result that raced with expiration".into(),
                output: None,
                attachments: Vec::new(),
                chat_id: None,
                message_id: Some("assistant-raced".into()),
            };
            let (task, event_failed) = worker_control
                .commit_terminal(State::Succeeded, Some(completed), None, None)
                .unwrap();
            assert!(!event_failed);
            worker_host.active.lock().await.remove(&worker_control.id);
            worker_control.finished.store(true, Ordering::Release);
            worker_control.done.notify_waiters();
            task
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            host.host(clear(&current.reservation_id, json!("system_expiration"))),
        )
        .await
        .expect("expiration must release the active-map guard before waiting")
        .unwrap();
        let task = worker.await.unwrap();
        assert_eq!(task.state, State::Interrupted);
        assert!(
            task.result.is_none(),
            "Expiration must not publish the raced result"
        );
        assert_eq!(task.error.unwrap().code, "ios_background_expired");
        let events = control
            .store
            .protocol_events(&control.owner, &control.request_id, 0)
            .unwrap();
        assert_eq!(events.events.last().unwrap()["type"], "RUN_ERROR");
        assert_eq!(
            events.events.last().unwrap()["code"],
            "ios_background_expired"
        );
        host.host(clear(&current.reservation_id, json!("system_expiration")))
            .await
            .unwrap();
        assert!(host.active.lock().await.is_empty());
    }

    #[tokio::test]
    async fn expiration_does_not_overwrite_an_already_committed_success() {
        let root = tempfile::tempdir().unwrap();
        let (host, _) = test_wake_host(root.path()).await;
        let control = Arc::new(running_control_with_protocol(root.path(), true));
        let key = WakeKey {
            app_id: control.owner.app_id.clone(),
            revision: control.revision,
            request_id: control.request_id.clone(),
        };
        let receipt = {
            let mut registry = host.wake_reservations.lock().unwrap();
            let now = now_millis();
            let receipt = registry.reserve(key.clone(), now, 120_000).unwrap();
            assert!(registry.claim(&key, now));
            assert!(registry.consume(&key, now, &control.id));
            receipt
        };
        host.active
            .lock()
            .await
            .insert(control.id.clone(), control.clone());
        let result = TaskResult {
            text: "Already completed".into(),
            output: None,
            attachments: Vec::new(),
            chat_id: None,
            message_id: Some("assistant-complete".into()),
        };
        let (committed, event_failed) = control
            .commit_terminal(State::Succeeded, Some(result.clone()), None, None)
            .unwrap();
        assert!(!event_failed);
        assert_eq!(committed.state, State::Succeeded);
        // The executor has committed but has not yet removed this Control.
        control.finished.store(true, Ordering::Release);
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            host.host(
                json!({"op":"wake.clear", "appId":key.app_id, "revision":key.revision,
                "runId":key.request_id, "reservationId":receipt.reservation_id,
                "reason":"system_expiration"}),
            ),
        )
        .await
        .expect("expiration must finish after the committed success")
        .unwrap();
        let after = control.store.get(&control.owner, &control.id).unwrap();
        assert_eq!(after.state, State::Succeeded);
        assert_eq!(after.result, Some(result));
        let events = control
            .store
            .protocol_events(&control.owner, &control.request_id, 0)
            .unwrap();
        assert_eq!(events.events.last().unwrap()["type"], "RUN_FINISHED");
        assert_eq!(events.events.last().unwrap()["outcome"]["type"], "success");
    }

    #[tokio::test]
    async fn expiration_does_not_replace_a_previously_requested_cancel() {
        let root = tempfile::tempdir().unwrap();
        let (host, _) = test_wake_host(root.path()).await;
        let control = Arc::new(running_control_with_protocol(root.path(), true));
        let key = WakeKey {
            app_id: control.owner.app_id.clone(),
            revision: control.revision,
            request_id: control.request_id.clone(),
        };
        let receipt = {
            let mut registry = host.wake_reservations.lock().unwrap();
            let now = now_millis();
            let receipt = registry.reserve(key.clone(), now, 120_000).unwrap();
            assert!(registry.claim(&key, now));
            assert!(registry.consume(&key, now, &control.id));
            receipt
        };
        host.active
            .lock()
            .await
            .insert(control.id.clone(), control.clone());
        control
            .store
            .request_cancel(&control.owner, &control.id)
            .unwrap();
        let worker_host = host.clone();
        let worker_control = control.clone();
        let worker = tokio::spawn(async move {
            worker_control.cancel.cancelled().await;
            let (task, _) = worker_control
                .commit_terminal(State::Cancelled, None, None, Some("cancelled".into()))
                .unwrap();
            worker_host.active.lock().await.remove(&worker_control.id);
            worker_control.finished.store(true, Ordering::Release);
            worker_control.done.notify_waiters();
            task
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            host.host(
                json!({"op":"wake.clear", "appId":key.app_id, "revision":key.revision,
                "runId":key.request_id, "reservationId":receipt.reservation_id,
                "reason":"system_expiration"}),
            ),
        )
        .await
        .expect("expiration must finish after prior cancellation")
        .unwrap();
        let task = worker.await.unwrap();
        assert_eq!(task.state, State::Cancelled);
        assert_eq!(task.error.unwrap().code, "cancelled");
        let events = control
            .store
            .protocol_events(&control.owner, &control.request_id, 0)
            .unwrap();
        assert_eq!(events.events.last().unwrap()["type"], "RUN_FINISHED");
        assert_eq!(
            events.events.last().unwrap()["outcome"]["type"],
            "cancelled"
        );
    }

    #[tokio::test]
    async fn system_expiration_keeps_unfinished_cleanup_busy() {
        let root = tempfile::tempdir().unwrap();
        let (host, _) = test_wake_host(root.path()).await;
        let control = Arc::new(running_control_with_protocol(root.path(), true));
        let key = WakeKey {
            app_id: control.owner.app_id.clone(),
            revision: control.revision,
            request_id: control.request_id.clone(),
        };
        let receipt = {
            let mut registry = host.wake_reservations.lock().unwrap();
            let now = now_millis();
            let receipt = registry.reserve(key.clone(), now, 120_000).unwrap();
            assert!(registry.claim(&key, now));
            assert!(registry.consume(&key, now, &control.id));
            receipt
        };
        host.active
            .lock()
            .await
            .insert(control.id.clone(), control.clone());
        let worker_control = control.clone();
        let worker = tokio::spawn(async move {
            worker_control.cancel.cancelled().await;
            worker_control
                .store
                .update(
                    &worker_control.owner,
                    &worker_control.id,
                    State::Running,
                    None,
                    None,
                    Some("tool_cleanup_incomplete".into()),
                )
                .unwrap();
            worker_control.finished.store(true, Ordering::Release);
            worker_control.done.notify_waiters();
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            host.host(
                json!({"op":"wake.clear", "appId":key.app_id, "revision":key.revision,
                "runId":key.request_id, "reservationId":receipt.reservation_id,
                "reason":"system_expiration"}),
            ),
        )
        .await
        .expect("expiration must finish without claiming cleanup succeeded")
        .unwrap();
        worker.await.unwrap();
        let task = control.store.get(&control.owner, &control.id).unwrap();
        assert_eq!(task.state, State::Running);
        assert_eq!(task.error.unwrap().code, "tool_cleanup_incomplete");
        assert!(!control.store.lease_valid(&control.owner, &control.id));
        assert!(host.active.lock().await.contains_key(&control.id));
        assert_eq!(
            host.stop_all().await.unwrap_err(),
            "agent_cleanup_incomplete"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn expiration_storage_failure_revokes_without_publishing_a_terminal_state() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let (host, _) = test_wake_host(root.path()).await;
        let control = Arc::new(running_control_with_protocol(root.path(), true));
        let key = WakeKey {
            app_id: control.owner.app_id.clone(),
            revision: control.revision,
            request_id: control.request_id.clone(),
        };
        let receipt = {
            let mut registry = host.wake_reservations.lock().unwrap();
            let now = now_millis();
            let receipt = registry.reserve(key.clone(), now, 120_000).unwrap();
            assert!(registry.claim(&key, now));
            assert!(registry.consume(&key, now, &control.id));
            receipt
        };
        host.active
            .lock()
            .await
            .insert(control.id.clone(), control.clone());
        let tasks = root
            .path()
            .join(&control.owner.app_id)
            .join("agent/agui-v1/tasks");
        std::fs::set_permissions(&tasks, std::fs::Permissions::from_mode(0o500)).unwrap();
        let worker_control = control.clone();
        let worker = tokio::spawn(async move {
            worker_control.cancel.cancelled().await;
            let result = worker_control.commit_terminal(
                State::Cancelled,
                None,
                None,
                Some("cancelled".into()),
            );
            worker_control.finished.store(true, Ordering::Release);
            worker_control.done.notify_waiters();
            result
        });
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            host.host(
                json!({"op":"wake.clear", "appId":key.app_id, "revision":key.revision,
                "runId":key.request_id, "reservationId":receipt.reservation_id,
                "reason":"system_expiration"}),
            ),
        )
        .await
        .expect("expiration must revoke even when persistence fails");
        std::fs::set_permissions(&tasks, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(result.unwrap_err(), "agent_storage_unavailable");
        assert_eq!(
            worker.await.unwrap().unwrap_err(),
            "agent_storage_unavailable"
        );
        assert!(control.cancel.is_cancelled());
        assert!(!control.store.lease_valid(&control.owner, &control.id));
        let error = control
            .store
            .get_by_request(&control.owner, &control.request_id)
            .unwrap_err();
        assert_eq!(task_access_error(error), "agent_storage_unavailable");
        assert_eq!(
            task_access_error(local_app::v2::AppError::NotFound("task".into())),
            "task_not_found"
        );
        assert!(control
            .store
            .protocol_events(&control.owner, &control.request_id, 0)
            .is_err());
        assert!(host.active.lock().await.contains_key(&control.id));
        assert_eq!(
            host.stop_all().await.unwrap_err(),
            "agent_cleanup_incomplete"
        );
    }

    #[tokio::test]
    async fn deferred_chat_tool_fails_after_admission_without_replaying_it() {
        let root = tempfile::tempdir().unwrap();
        let (host, service) = test_wake_host(root.path()).await;
        let source = root.path().join("agent-app");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("index.html"), "fixture").unwrap();
        std::fs::write(
            source.join("lingxi-app.json"),
            json!({
                "schema":2,"name":"Agent fixture","entry":{"type":"static","path":"index.html"},
                "targets":[{"platform":"ios","environment":"web"}],
                "capabilities":["agent"],"agent":{"tools":[]}
            })
            .to_string(),
        )
        .unwrap();
        let published = service
            .request_value(json!({"op":"publish", "expectedRevision":0,
            "sourcePath":source}))
            .await
            .unwrap();
        let app = published["record"]["appId"].as_str().unwrap();
        let revision = published["record"]["currentRevision"].as_u64().unwrap();
        let (owner, _, _) = host
            .current_release(app, revision, "deferred-test")
            .await
            .unwrap();
        let state = host.app_state(app).unwrap();
        let settings: Settings = serde_json::from_value(json!({
            "model":{"providerId":"provider","modelId":"model"},"tools":{},"skills":[],
            "background":false,"maxTurns":4,"timeoutMs":30000
        }))
        .unwrap();
        state
            .store
            .configure(owner.clone(), Some(settings))
            .unwrap();
        let chat_id = uuid::Uuid::new_v4().to_string();
        let session_id = uuid::Uuid::new_v4();
        state
            .chats
            .create(&chat_id, None, &session_id.to_string())
            .unwrap();
        let path = state.chats.transcript_path(&chat_id).unwrap();
        let user_id = uuid::Uuid::new_v4();
        let assistant_id = uuid::Uuid::new_v4();
        let attachment_id = uuid::Uuid::new_v4();
        let cwd = root.path().to_string_lossy().to_string();
        let rows = [
            json!({"type":"user", "uuid":user_id, "parentUuid":null, "sessionId":session_id,
                "timestamp":"2026-10-27T01:00:00.000Z", "cwd":cwd,
                "message":{"role":"user","content":"previous request"}}),
            json!({"type":"assistant", "uuid":assistant_id, "parentUuid":user_id, "sessionId":session_id,
                "timestamp":"2026-10-27T01:00:01.000Z", "cwd":cwd,
                "message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_pending",
                    "name":"Shell","input":{"command":"touch forbidden"}}]}}),
            json!({"type":"attachment", "uuid":attachment_id, "parentUuid":assistant_id,
                "sessionId":session_id, "timestamp":"2026-10-27T01:00:02.000Z", "cwd":cwd,
                "attachment":{"type":"hook_deferred_tool","toolUseID":"toolu_pending",
                    "toolName":"Shell","toolInput":{"command":"touch forbidden"},
                    "hookName":"PreToolUse:Shell","hookEvent":"PreToolUse","permissionMode":"default"}}),
        ];
        let transcript = rows
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        std::fs::write(&path, &transcript).unwrap();
        let mut input: Input =
            serde_json::from_value(json!({"requestId":"deferred-run", "prompt":"new request"}))
                .unwrap();
        input.protocol_thread_id = Some(chat_id.clone());
        input.protocol_history_digest = Some(local_app::v2::agent::protocol::history_fingerprint(
            &state.chats.protocol_history_digests(&chat_id).unwrap(),
        ));
        let new_user = json!({"id":"new-user", "role":"user", "content":"new request"});
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            host.start_task(
                owner.clone(),
                revision,
                &[],
                &[],
                input.clone(),
                Some(new_user.clone()),
                Some(chat_id.clone()),
                CancellationToken::new(),
                true,
                true,
            ),
        )
        .await
        .expect("deferred guard must complete without starting the model");
        assert_eq!(result.unwrap_err(), "chat_deferred_tools_pending");
        let task = state
            .store
            .get_by_request(&owner, &input.request_id)
            .unwrap();
        assert_eq!(task.state, State::Failed);
        let error = task.error.unwrap();
        assert_eq!(error.code, "chat_deferred_tools_pending");
        assert!(error.message.contains("Review its effects"));
        assert!(error.message.contains("not be replayed automatically"));
        let events = state
            .store
            .protocol_events(&owner, &input.request_id, 0)
            .unwrap();
        assert_eq!(events.events.first().unwrap()["type"], "RUN_STARTED");
        assert_eq!(events.events.last().unwrap()["type"], "RUN_ERROR");
        assert_eq!(
            events.events.last().unwrap()["code"],
            "chat_deferred_tools_pending"
        );
        let history = state
            .chats
            .protocol_history_page(&chat_id, None, 10)
            .unwrap();
        assert_eq!(
            history.messages,
            vec![new_user],
            "Current user input may already be saved on rejection"
        );
        assert_eq!(
            state.chats.message_metadata(&chat_id).unwrap().len(),
            1,
            "Existing transcript rows must not be imported before the guard"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), transcript);
        assert!(!root.path().join("forbidden").exists());
        assert!(host.active.lock().await.is_empty());
    }

    #[tokio::test]
    async fn revoke_serializes_before_waiting_wake_reservation() {
        let root = tempfile::tempdir().unwrap();
        let (host, service) = test_wake_host(root.path()).await;
        let source = root.path().join("agent-app");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("index.html"), "fixture").unwrap();
        std::fs::write(
            source.join("lingxi-app.json"),
            json!({
                "schema":2,"name":"Agent fixture","entry":{"type":"static","path":"index.html"},
                "targets":[{"platform":"ios","environment":"web"}],
                "capabilities":["agent"],"agent":{"tools":[]}
            })
            .to_string(),
        )
        .unwrap();
        let published = service
            .request_value(json!({"op":"publish", "expectedRevision":0,
            "sourcePath":source}))
            .await
            .unwrap();
        let app = published["record"]["appId"].as_str().unwrap();
        let revision = published["record"]["currentRevision"].as_u64().unwrap();
        let (owner, _, _) = host.current_release(app, revision, "test").await.unwrap();
        let state = host.app_state(app).unwrap();
        let settings: Settings = serde_json::from_value(json!({
            "model":{"providerId":"provider","modelId":"model"},"tools":{},"skills":[],
            "background":false,"maxTurns":4,"timeoutMs":30000
        }))
        .unwrap();
        state.store.configure(owner, Some(settings)).unwrap();
        let reserve = json!({"op":"wake.reserve", "appId":app, "revision":revision,
            "runId":"wake-a", "receiveTimeoutMs":120000});
        let receipt = host.host(reserve.clone()).await.unwrap();
        assert_eq!(receipt["created"], true);
        let duplicate = host.host(reserve.clone()).await.unwrap();
        assert_eq!(duplicate["created"], false);
        assert_eq!(duplicate["reservationId"], receipt["reservationId"]);

        // Queue real operations in order while holding the common admission gate.
        let gate = host.admission.lock().await;
        let mut revoke =
            Box::pin(host.host(json!({"op":"configure", "appId":app, "settings":null})));
        let mut waiting_reserve = Box::pin(host.host(reserve));
        assert!(matches!(
            futures::poll!(revoke.as_mut()),
            std::task::Poll::Pending
        ));
        assert!(matches!(
            futures::poll!(waiting_reserve.as_mut()),
            std::task::Poll::Pending
        ));
        drop(gate);
        tokio::time::timeout(std::time::Duration::from_secs(3), revoke)
            .await
            .unwrap()
            .unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(3), waiting_reserve)
            .await
            .unwrap();
        assert_eq!(result.unwrap_err(), "agent_not_authorized");
        assert!(host
            .wake_reservations
            .lock()
            .unwrap()
            .reservations
            .is_empty());

        // A revoke must also clear a stale earlier-release reservation when
        // settings(current release) already reports None.
        let stale = WakeKey {
            app_id: app.into(),
            revision: revision.saturating_sub(1),
            request_id: "old-wake".into(),
        };
        host.wake_reservations
            .lock()
            .unwrap()
            .reserve(stale, now_millis(), 120_000)
            .unwrap();
        host.host(json!({"op":"configure", "appId":app, "settings":null}))
            .await
            .unwrap();
        assert!(host
            .wake_reservations
            .lock()
            .unwrap()
            .reservations
            .is_empty());
    }

    #[test]
    fn ending_tool_authority_keeps_external_cancel_observable_until_commit() {
        let root = tempfile::tempdir().unwrap();
        let control = running_control(root.path());
        let lease = SessionToolGrantLease::new(SessionToolPolicy::default());
        let captured = lease.clone();
        *control.lease.lock().unwrap() = Some(lease);
        control.revoke_lease();
        assert!(captured.is_revoked());
        assert!(!control.cancel.is_cancelled());
        control.revoke();
        assert!(control.cancel.is_cancelled());
    }

    #[tokio::test]
    async fn native_output_progress_counts_real_utf16_deltas_without_public_output() {
        let root = tempfile::tempdir().unwrap();
        let control = running_control(root.path());
        assert!(control.wants_model_output_progress());
        control
            .emit_text(&Utf16JsonProjection::from("aggregate"))
            .await;
        assert_eq!(
            control.native_output.bytes(),
            0,
            "whole-body emissions are not stream deltas"
        );
        control.note_model_output_delta(0, &[]);
        assert_eq!(control.native_output.bytes(), 0);
        control.note_model_output_delta(0, &"A汉".encode_utf16().collect::<Vec<_>>());
        assert_eq!(control.native_output.bytes(), 4);
        control.note_model_output_delta(0, &[0xd83d]);
        control.note_model_output_delta(1, &[0xd800]);
        assert_eq!(
            control.native_output.bytes(),
            4,
            "a high surrogate alone is not yet counted"
        );
        control.note_model_output_delta(0, &[0xde42]);
        assert_eq!(
            control.native_output.bytes(),
            8,
            "split emoji is four UTF-8 bytes"
        );
        control.finish_model_output_block(1);
        assert_eq!(
            control.native_output.bytes(),
            11,
            "a lone unit encodes as U+FFFD"
        );
        control.note_model_output_delta(2, &[0xd800]);
        control.native_output.finish_all();
        assert_eq!(control.native_output.bytes(), 14);
        control
            .native_output
            .bytes
            .store(MAX_NATIVE_OUTPUT_BYTES - 1, Ordering::Relaxed);
        control.note_model_output_delta(3, &[0x61, 0x62, 0x63]);
        assert_eq!(control.native_output.bytes(), MAX_NATIVE_OUTPUT_BYTES);
    }

    #[tokio::test]
    async fn native_only_counter_advances_on_same_event_sequence_and_survives_removal() {
        let root = tempfile::tempdir().unwrap();
        let control = running_control_with_protocol(root.path(), true);
        let public = control
            .store
            .protocol_events(&control.owner, "progress-run", 0)
            .unwrap();
        let before = native_events_value(public.clone(), 0, 0).unwrap();
        assert!(before.get("nativeOutputBytes").is_none());
        control.note_model_output_delta(1, &"hello".encode_utf16().collect::<Vec<_>>());
        let again = control
            .store
            .protocol_events(&control.owner, "progress-run", 0)
            .unwrap();
        let live = native_events_value(
            again.clone(),
            control.native_output_for(&control.owner, 1),
            0,
        )
        .unwrap();
        assert_eq!(again.sequence, public.sequence);
        assert_eq!(live["nativeOutputBytes"], 5);
        assert!(serde_json::to_value(&again)
            .unwrap()
            .get("nativeOutputBytes")
            .is_none());
        assert!(public_task_value(
            &control.store.get(&control.owner, &control.id).unwrap(),
            "progress-thread".into()
        )
        .unwrap()
        .get("nativeOutputBytes")
        .is_none());
        assert_eq!(control.native_output_for(&control.owner, 2), 0);
        let mut wrong_owner = control.owner.clone();
        wrong_owner.release_digest = "3".repeat(64);
        assert_eq!(control.native_output_for(&wrong_owner, 1), 0);
        assert!(control
            .store
            .native_output_bytes_for_request(&wrong_owner, "progress-run", 1)
            .is_err());
        control
            .store
            .record_native_output_bytes(
                &control.owner,
                &control.id,
                1,
                control.native_output.bytes(),
            )
            .unwrap();
        let saved = control
            .store
            .native_output_bytes_for_request(&control.owner, "progress-run", 1)
            .unwrap()
            .unwrap();
        let after_removal = native_events_value(again, 0, saved).unwrap();
        assert_eq!(after_removal["nativeOutputBytes"], 5);
    }

    #[tokio::test]
    async fn session_terminal_callbacks_leave_model_progress_unknown() {
        let cases = [
            ("end_turn", State::Succeeded, None, 1),
            ("aborted_streaming", State::Cancelled, Some("cancelled"), 1),
            ("model_error", State::Failed, Some("model_error"), 1),
            (
                "budget_exhausted",
                State::Failed,
                Some("budget_exhausted"),
                1,
            ),
            // Multiple callbacks and a multi-call cost snapshot must not be
            // mistaken for reliable per-model-turn completion events.
            ("end_turn", State::Succeeded, None, 3),
        ];
        for (reason, terminal, error, callbacks) in cases {
            let root = tempfile::tempdir().unwrap();
            let control = running_control(root.path());
            let before = control.store.get(&control.owner, &control.id).unwrap();
            if terminal == State::Cancelled {
                control.cancel.cancel();
            }
            if reason == "model_error" {
                control
                    .emit_terminal_api_error(Some(503), "server_error", reason)
                    .await;
            }
            let cost = CostSnapshot {
                api_calls: 4,
                ..CostSnapshot::default()
            };
            for _ in 0..callbacks {
                control.emit_end_turn(reason, &cost).await;
            }
            let running = control.store.get(&control.owner, &control.id).unwrap();
            assert_eq!(running.progress, None, "{reason}");
            assert_eq!(running.sequence, before.sequence, "{reason}");
            assert!(public_task_value(&running, "thread".into())
                .unwrap()
                .get("progress")
                .is_none());
            assert_eq!(
                control.model_failed.load(Ordering::Acquire),
                reason == "model_error"
            );
            let finished = control
                .store
                .update(
                    &control.owner,
                    &control.id,
                    terminal.clone(),
                    None,
                    None,
                    error.map(str::to_owned),
                )
                .unwrap();
            assert_eq!(finished.state, terminal);
            assert_eq!(finished.progress, None, "{reason}");
            assert!(public_task_value(&finished, "thread".into())
                .unwrap()
                .get("progress")
                .is_none());
        }
    }

    struct QueryDropNotice(Arc<AtomicBool>);

    impl Drop for QueryDropNotice {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    #[tokio::test]
    async fn stateless_query_cancellation_drops_pending_future_without_progress() {
        let root = tempfile::tempdir().unwrap();
        let control = Arc::new(running_control(root.path()));
        let before = control.store.get(&control.owner, &control.id).unwrap();
        let dropped = Arc::new(AtomicBool::new(false));
        let notice = QueryDropNotice(dropped.clone());
        let (started_tx, started_rx) = oneshot::channel();
        let running = control.clone();
        let task = tokio::spawn(async move {
            running
                .run_stateless_query(async move {
                    let _notice = notice;
                    started_tx.send(()).unwrap();
                    std::future::pending::<Result<(TaskResult, Value), String>>().await
                })
                .await
        });
        started_rx.await.unwrap();
        control.revoke();
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .expect("cancellation must end the local query")
            .unwrap();
        assert_eq!(result.unwrap_err(), "cancelled");
        assert!(dropped.load(Ordering::Acquire));
        let after = control.store.get(&control.owner, &control.id).unwrap();
        assert_eq!(after.sequence, before.sequence);
        assert_eq!(after.progress, None);
        assert_eq!(*control.usage.lock().await, None);
    }

    #[tokio::test]
    async fn stateless_query_cancelled_before_poll_wins_over_ready_query() {
        let root = tempfile::tempdir().unwrap();
        let control = running_control(root.path());
        let before = control.store.get(&control.owner, &control.id).unwrap();
        let dropped = Arc::new(AtomicBool::new(false));
        let polled = Arc::new(AtomicBool::new(false));
        let notice = QueryDropNotice(dropped.clone());
        let query_polled = polled.clone();
        control.revoke();
        let result = control
            .run_stateless_query(async move {
                let _notice = notice;
                query_polled.store(true, Ordering::Release);
                Ok((
                    TaskResult {
                        text: "answer".into(),
                        output: None,
                        attachments: Vec::new(),
                        chat_id: None,
                        message_id: None,
                    },
                    json!({"outputTokens": 1}),
                ))
            })
            .await;
        assert_eq!(result.unwrap_err(), "cancelled");
        assert!(dropped.load(Ordering::Acquire));
        assert!(!polled.load(Ordering::Acquire));
        let after = control.store.get(&control.owner, &control.id).unwrap();
        assert_eq!(after.sequence, before.sequence);
        assert_eq!(after.progress, None);
        assert_eq!(*control.usage.lock().await, None);
    }

    #[tokio::test]
    async fn stateless_progress_is_published_only_after_a_successful_query() {
        for succeeds in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let control = Arc::new(running_control(root.path()));
            let before = control.store.get(&control.owner, &control.id).unwrap();
            let (started_tx, started_rx) = oneshot::channel();
            let (result_tx, result_rx) = oneshot::channel();
            let running = control.clone();
            let task = tokio::spawn(async move {
                running
                    .run_stateless_query(async move {
                        started_tx.send(()).unwrap();
                        result_rx.await.unwrap()
                    })
                    .await
            });
            started_rx.await.unwrap();
            let pending = control.store.get(&control.owner, &control.id).unwrap();
            assert_eq!(pending.progress, None);
            assert_eq!(pending.sequence, before.sequence);

            let expected_result = TaskResult {
                text: "answer".into(),
                output: None,
                attachments: Vec::new(),
                chat_id: None,
                message_id: None,
            };
            let expected_usage = json!({"inputTokens":3,"outputTokens":2});
            result_tx
                .send(if succeeds {
                    Ok((expected_result.clone(), expected_usage.clone()))
                } else {
                    Err("model_error".to_owned())
                })
                .unwrap();
            let result = task.await.unwrap();
            let observed = control.store.get(&control.owner, &control.id).unwrap();
            if succeeds {
                let (answer, usage) = result.unwrap();
                assert_eq!(answer, expected_result);
                assert_eq!(usage, expected_usage);
                let progress = observed.progress.as_ref().unwrap();
                assert_eq!(progress.completed_model_turns, 1);
                assert_eq!(progress.turn_limit, 4);
                assert_eq!(observed.sequence, before.sequence + 1);
                assert_eq!(*control.usage.lock().await, Some(usage.clone()));
                let public = public_task_value(&observed, "thread".into()).unwrap();
                assert_eq!(public["progress"]["completedModelTurns"], 1);
                assert_eq!(public["progress"]["turnLimit"], 4);
                let finished = control
                    .store
                    .update(
                        &control.owner,
                        &control.id,
                        State::Succeeded,
                        Some(answer),
                        Some(usage),
                        None,
                    )
                    .unwrap();
                assert_eq!(finished.progress, observed.progress);
            } else {
                assert_eq!(result.unwrap_err(), "model_error");
                assert_eq!(observed.progress, None);
                assert_eq!(observed.sequence, before.sequence);
                assert_eq!(*control.usage.lock().await, None);
                assert!(public_task_value(&observed, "thread".into())
                    .unwrap()
                    .get("progress")
                    .is_none());
            }
        }
    }
}

#[cfg(test)]
mod accepted_projection_tests {
    use super::*;
    use lingxi_core::types::ContentBlock;

    fn assistant(id: MessageId, text: &str) -> ConversationMessage {
        ConversationMessage::Assistant {
            id,
            content: vec![ContentBlock::Text {
                text: text.into(),
                citations: None,
            }],
            stop_reason: Some("end_turn".into()),
            resumable: None,
            per_turn_effort: None,
        }
    }

    fn setup() -> (tempfile::TempDir, AppAgentState, Control, Value, String) {
        let root = tempfile::tempdir().unwrap();
        let app_id = format!("a{}", "6".repeat(32));
        std::fs::create_dir_all(root.path().join(&app_id)).unwrap();
        let state = AppAgentState::open(root.path(), &app_id).unwrap();
        let owner = Owner {
            app_id,
            release_digest: "2".repeat(64),
            invocation_id: "projection-test".into(),
        };
        let settings: Settings = serde_json::from_value(json!({
            "model":{"providerId":"provider","modelId":"model"},
            "tools":{},"skills":[],"background":false,"maxTurns":4,"timeoutMs":30000
        }))
        .unwrap();
        state
            .store
            .configure(owner.clone(), Some(settings))
            .unwrap();
        let chat = state
            .chats
            .create(
                &uuid::Uuid::new_v4().to_string(),
                None,
                &uuid::Uuid::new_v4().to_string(),
            )
            .unwrap();
        let user = json!({"id":"user-1","role":"user","content":"hello"});
        let input = json!({"threadId":chat.chat_id,"runId":"public-run-1","protocolVersion":"1.0","messages":[user.clone()]});
        let (normalized, chat_id) =
            normalize_page_run_input(&state, input, Some(&chat.chat_id)).unwrap();
        let admission = state
            .store
            .admit_scoped(
                &owner,
                &normalized.input,
                &[],
                &[],
                chat_id.as_deref(),
                true,
            )
            .unwrap();
        let id = admission.task.run_id;
        state.chats.start_turn(&chat.chat_id, &id).unwrap();
        state
            .chats
            .record_protocol_message(&chat.chat_id, &id, &user)
            .unwrap();
        state
            .store
            .update(&owner, &id, State::Running, None, None, None)
            .unwrap();
        state.store.append_event(&owner, &id, json!({
            "type":"RUN_STARTED", "protocolVersion":"1.0", "threadId":chat.chat_id,"runId":"public-run-1"
        })).unwrap();
        let (chat_persisted, _) = watch::channel(None);
        let control = Control {
            owner,
            revision: 1,
            request_id: "public-run-1".into(),
            id,
            protocol_thread_id: Some(chat.chat_id.clone()),
            cancel: CancellationToken::new(),
            finished: AtomicBool::new(false),
            done: Notify::new(),
            lease: Mutex::new(None),
            interaction: AsyncMutex::new(None),
            store: state.store.clone(),
            chats: state.chats.clone(),
            chat_id: Some(chat.chat_id.clone()),
            user_message_id: None,
            chat_orchestrator: Mutex::new(None),
            chat_persisted,
            chat_commit: AsyncMutex::new(None),
            attachment_metadata: Vec::new(),
            assistant_message_ids: AsyncMutex::new(HashSet::new()),
            assistant_display_text: AsyncMutex::new(HashMap::new()),
            protocol_events_failed: AtomicBool::new(false),
            native_output: NativeOutputProgress::default(),
            text: AsyncMutex::new(String::new()),
            model_failed: AtomicBool::new(false),
            model_failure: Mutex::new(None),
            timed_out: AtomicBool::new(false),
            usage: AsyncMutex::new(None),
            fail_after_final_ledger_before_chat_authority: AtomicBool::new(false),
            after_final_ledger_error_barrier: Mutex::new(None),
        };
        (root, state, control, user, chat.chat_id)
    }

    #[tokio::test]
    async fn chat_completion_releases_transcript_authority_unless_tool_cleanup_is_incomplete() {
        use orchestrator::test_support::{
            noop_hook_executor, MockApiClient, MockStreamingApiClient, NoOpPermissionGate,
            StaticMemoryProvider,
        };
        use session::jsonl::JsonlWriter;

        for outcome in [
            "success",
            "failed",
            "cancelled",
            "commit_failed",
            "cleanup_incomplete",
        ] {
            let (root, state, control, _user, chat_id) = setup();
            let (host, _service) = super::progress_tests::test_wake_host(root.path()).await;
            let control = Arc::new(control);
            let session_id = SessionId::from_uuid(
                uuid::Uuid::parse_str(&state.chats.session_id(&chat_id).unwrap()).unwrap(),
            );
            let path = state.chats.transcript_path(&chat_id).unwrap();
            let fs = host.factory.context.fs.clone();
            let writer = Arc::new(
                JsonlWriter::open_persistent_chat(path.clone(), fs.clone(), None)
                    .await
                    .unwrap(),
            );
            let api = Arc::new(MockApiClient::new(Vec::new()));
            let orchestrator = Arc::new(
                ConversationOrchestrator::new_side_chat(
                    OrchestratorConfig::default(),
                    api,
                    Arc::new(MockStreamingApiClient::empty()),
                    Arc::new(ToolRegistry::new()),
                    noop_hook_executor(),
                    Arc::new(NoOpPermissionGate),
                    control.clone(),
                    Arc::new(StaticMemoryProvider::empty()),
                    root.path().to_path_buf(),
                )
                .with_session_id(session_id)
                .with_jsonl_writer(writer.clone()),
            );
            orchestrator
                .append_external_history_message(ConversationMessage::user(
                    MessageId::from_uuid(uuid::Uuid::parse_str(&control.id).unwrap()),
                    "hello".into(),
                ))
                .await;
            let assistant_id = MessageId::new();
            let mut answer = assistant(assistant_id, "answer");
            if let ConversationMessage::Assistant { stop_reason, .. } = &mut answer {
                // External assistant messages use the synthetic transcript form.
                *stop_reason = Some("stop_sequence".into());
            }
            orchestrator.append_external_history_message(answer).await;
            let captured = orchestrator
                .verify_persistent_chat_checkpoint(true)
                .await
                .unwrap();
            let checkpoint = captured.checkpoint().clone();
            state
                .store
                .record_chat_transcript_checkpoint(
                    &control.owner,
                    &control.id,
                    &chat_id,
                    ChatTranscriptCheckpoint {
                        byte_len: checkpoint.byte_len,
                        sha256: checkpoint.sha256.clone(),
                    },
                )
                .unwrap();
            let weak_orchestrator = Arc::downgrade(&orchestrator);
            let weak_writer = Arc::downgrade(&writer);
            let weak_control = Arc::downgrade(&control);
            *control.chat_orchestrator.lock().unwrap() = Some(orchestrator);
            drop(writer);
            host.active
                .lock()
                .await
                .insert(control.id.clone(), control.clone());
            assert!(
                JsonlWriter::open_persistent_chat(
                    path.clone(),
                    fs.clone(),
                    Some(checkpoint.clone())
                )
                .await
                .is_err(),
                "the transcript must stay owned before terminal commit"
            );

            let execution = match outcome {
                "success" => Ok((
                    result(&chat_id, assistant_id.to_string(), "answer".into(), None),
                    json!({}),
                )),
                "cancelled" => Err("cancelled".into()),
                "cleanup_incomplete" => Err("tool_cleanup_incomplete".into()),
                _ => Err("model_execution_failed".into()),
            };
            if outcome == "commit_failed" {
                control
                    .protocol_events_failed
                    .store(true, Ordering::Release);
            }
            host.finish_execution(&control, execution).await;
            assert!(control.finished.load(Ordering::Acquire));
            if outcome == "cleanup_incomplete" {
                assert!(host.active.lock().await.contains_key(&control.id));
                assert!(control.chat_orchestrator.lock().unwrap().is_some());
                assert!(weak_orchestrator.upgrade().is_some());
                assert_eq!(
                    state.store.get(&control.owner, &control.id).unwrap().state,
                    State::Running
                );
                assert_eq!(state.chats.get(&chat_id).unwrap().state, ChatState::Running);
                assert!(
                    JsonlWriter::open_persistent_chat(
                        path.clone(),
                        fs.clone(),
                        Some(checkpoint.clone())
                    )
                    .await
                    .is_err(),
                    "unfinished tool cleanup must retain the transcript lease"
                );
                // This fixture has no actual tool. Tear down its deliberately
                // retained authority so the test itself does not leak the cycle.
                control.chat_orchestrator.lock().unwrap().take();
                host.active.lock().await.remove(&control.id);
            } else {
                assert!(!host.active.lock().await.contains_key(&control.id));
                assert!(control.chat_orchestrator.lock().unwrap().is_none());
                if outcome == "commit_failed" {
                    assert!(state.store.get(&control.owner, &control.id).is_err());
                } else {
                    let expected = match outcome {
                        "success" => State::Succeeded,
                        "cancelled" => State::Cancelled,
                        _ => State::Failed,
                    };
                    assert_eq!(
                        state.store.get(&control.owner, &control.id).unwrap().state,
                        expected
                    );
                }
            }
            assert!(weak_orchestrator.upgrade().is_none(), "{outcome}");
            assert!(weak_writer.upgrade().is_none(), "{outcome}");
            let reopened = JsonlWriter::open_persistent_chat(path, fs, Some(checkpoint))
                .await
                .unwrap();
            drop(reopened);
            drop(control);
            assert!(weak_control.upgrade().is_none(), "{outcome}");
        }
    }

    #[test]
    fn admission_rechecks_the_committed_chat_history_snapshot() {
        let root = tempfile::tempdir().unwrap();
        let app_id = format!("a{}", "7".repeat(32));
        std::fs::create_dir_all(root.path().join(&app_id)).unwrap();
        let state = AppAgentState::open(root.path(), &app_id).unwrap();
        let owner = Owner {
            app_id,
            release_digest: "2".repeat(64),
            invocation_id: "history-race".into(),
        };
        let settings: Settings = serde_json::from_value(json!({
            "model":{"providerId":"provider","modelId":"model"},
            "tools":{},"skills":[],"background":false,"maxTurns":4,"timeoutMs":30000
        }))
        .unwrap();
        state
            .store
            .configure(owner.clone(), Some(settings))
            .unwrap();
        let chat = state
            .chats
            .create(
                &uuid::Uuid::new_v4().to_string(),
                None,
                &uuid::Uuid::new_v4().to_string(),
            )
            .unwrap();
        let raw = json!({
            "threadId":chat.chat_id.clone(),
            "runId":"history-race-run",
            "protocolVersion":"1.0",
            "messages":[{"id":"user-new","role":"user","content":"question"}]
        });
        let (normalized, _) = normalize_page_run_input(&state, raw, Some(&chat.chat_id)).unwrap();
        let expected = normalized.input.protocol_history_digest.clone().unwrap();
        state
            .chats
            .record_protocol_message(
                &chat.chat_id,
                "concurrent-run",
                &json!({"id":"concurrent-user","role":"user","content":"changed history"}),
            )
            .unwrap();
        let chats = state.chats.clone();
        let checked_chat_id = chat.chat_id.clone();
        let admission = state.store.admit_scoped_with_check(
            &owner,
            &normalized.input,
            &[],
            &[],
            Some(&chat.chat_id),
            true,
            move || {
                let current = chats.protocol_history_digests(&checked_chat_id)?;
                if local_app::v2::agent::protocol::history_fingerprint(&current) != expected {
                    return Err(local_app::v2::AppError::Invalid(
                        "AG-UI Chat history changed during Agent admission".into(),
                    ));
                }
                Ok(())
            },
        );
        assert!(admission.is_err());
        assert!(matches!(
            state.store.get_by_request(&owner, "history-race-run"),
            Err(local_app::v2::AppError::NotFound(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn uncertain_chat_index_prevents_protocol_start_from_falling_back_to_stateless() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let app_id = format!("a{}", "8".repeat(32));
        std::fs::create_dir_all(root.path().join(&app_id)).unwrap();
        let state = AppAgentState::open(root.path(), &app_id).unwrap();
        let owner = Owner {
            app_id,
            release_digest: "2".repeat(64),
            invocation_id: "uncertain-chat-start".into(),
        };
        let settings: Settings = serde_json::from_value(json!({
            "model":{"providerId":"provider","modelId":"model"},
            "tools":{},"skills":[],"background":false,"maxTurns":4,"timeoutMs":30000
        }))
        .unwrap();
        state
            .store
            .configure(owner.clone(), Some(settings))
            .unwrap();
        let chat = state
            .chats
            .create(
                &uuid::Uuid::new_v4().to_string(),
                None,
                &uuid::Uuid::new_v4().to_string(),
            )
            .unwrap();

        std::fs::set_permissions(&state.root, std::fs::Permissions::from_mode(0o500)).unwrap();
        let write_error = state.chats.archive(&chat.chat_id).unwrap_err();
        std::fs::set_permissions(&state.root, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(matches!(
            write_error,
            local_app::v2::AppError::ChatStorageUncertain
        ));

        let raw = json!({
            "threadId":chat.chat_id,
            "runId":"uncertain-chat-run",
            "protocolVersion":"1.0",
            "messages":[{"id":"user-uncertain","role":"user","content":"question"}]
        });
        assert_eq!(
            normalize_page_run_input(&state, raw, None).unwrap_err(),
            "chat_storage_unavailable"
        );
        assert!(matches!(
            state.store.get_by_request(&owner, "uncertain-chat-run"),
            Err(local_app::v2::AppError::NotFound(_))
        ));
    }

    #[test]
    fn post_finalize_chat_authority_failure_quarantines_before_waiting_readers_resume() {
        let (root, state, control, user, chat_id) = setup();
        let app_id = control.owner.app_id.clone();
        let owner = control.owner.clone();
        let control = Arc::new(control);
        let barrier = Arc::new(std::sync::Barrier::new(2));
        control
            .fail_after_final_ledger_before_chat_authority
            .store(true, Ordering::Release);
        *control.after_final_ledger_error_barrier.lock().unwrap() = Some(barrier.clone());

        let writer_control = control.clone();
        let writer_chat_id = chat_id.clone();
        let writer = std::thread::spawn(move || {
            writer_control.commit_terminal(
                State::Succeeded,
                Some(result(
                    &writer_chat_id,
                    "assistant-commit-race".into(),
                    "committed answer".into(),
                    None,
                )),
                None,
                None,
            )
        });
        barrier.wait();

        assert_eq!(
            control
                .chats
                .protocol_history_page(&chat_id, None, 10)
                .unwrap()
                .messages,
            vec![user.clone()],
            "the assistant stays hidden before Chat authority is installed"
        );
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let reader_store = control.store.clone();
        let reader_owner = owner.clone();
        let reader = std::thread::spawn(move || {
            ready_tx.send(()).unwrap();
            reader_store.get_by_request(&reader_owner, "public-run-1")
        });
        ready_rx.recv().unwrap();
        barrier.wait();

        assert_eq!(
            writer.join().unwrap().unwrap_err(),
            "chat_storage_unavailable"
        );
        assert!(matches!(
            reader.join().unwrap(),
            Err(local_app::v2::AppError::Io(_))
        ));
        assert!(control
            .chats
            .protocol_history_page(&chat_id, None, 10)
            .is_err());
        assert!(control
            .store
            .get_by_request(&owner, "public-run-1")
            .is_err());

        drop(control);
        drop(state);
        let recovered = AppAgentState::open(root.path(), &app_id).unwrap();
        let task = recovered
            .store
            .get_by_request(&owner, "public-run-1")
            .unwrap();
        assert_eq!(task.state, State::Succeeded);
        assert_eq!(
            recovered
                .chats
                .protocol_history_page(&chat_id, None, 10)
                .unwrap()
                .messages,
            vec![
                user,
                json!({
                    "id":"assistant-commit-race","role":"assistant","content":"committed answer"
                })
            ]
        );
    }

    fn result(
        chat_id: &str,
        message_id: String,
        text: String,
        output: Option<Value>,
    ) -> TaskResult {
        TaskResult {
            text,
            output,
            attachments: Vec::new(),
            chat_id: Some(chat_id.into()),
            message_id: Some(message_id),
        }
    }

    // Mirrors the SDK's standard text event reduction. The pinned SDK is also
    // exercised independently by official-sdk-behavior.test.mjs.
    fn sdk_text_messages(events: &[Value]) -> Vec<Value> {
        let mut messages: Vec<Value> = Vec::new();
        for event in events {
            local_app::v2::agent::protocol::validate_event(event).unwrap();
            match event["type"].as_str().unwrap() {
                "RUN_STARTED" | "RUN_FINISHED" | "RUN_ERROR" => {}
                "TEXT_MESSAGE_START" => messages.push(json!({
                    "id":event["messageId"],"role":"assistant","content":""
                })),
                "TEXT_MESSAGE_CONTENT" => {
                    let id = event["messageId"].as_str().unwrap();
                    let message = messages
                        .iter_mut()
                        .find(|message| message["id"] == id)
                        .unwrap();
                    let content = message["content"].as_str().unwrap().to_owned()
                        + event["delta"].as_str().unwrap();
                    message["content"] = json!(content);
                }
                "TEXT_MESSAGE_END" => {
                    assert!(messages
                        .iter()
                        .any(|message| message["id"] == event["messageId"]));
                }
                other => panic!("unexpected public event: {other}"),
            }
        }
        messages
    }

    #[tokio::test]
    async fn successful_projection_matches_sdk_chat_history_and_next_turn() {
        let (_root, state, control, user, chat_id) = setup();
        let old = MessageId::new();
        let first = MessageId::new();
        let retracted = MessageId::new();
        let accepted = MessageId::new();
        let unobserved = MessageId::new();
        for (id, text) in [
            (old, "old"),
            (first, "pretool"),
            (retracted, "draft"),
            (accepted, "answer"),
        ] {
            control.emit_text(&Utf16JsonProjection::from(text)).await;
            control.emit_assistant_message_identity(&id).await;
        }
        let page = state
            .store
            .protocol_events(&control.owner, "public-run-1", 0)
            .unwrap();
        assert_eq!(
            page.events.len(),
            1,
            "no tentative public output before success"
        );
        let mut final_assistant = assistant(accepted, "accepted ");
        if let ConversationMessage::Assistant { content, .. } = &mut final_assistant {
            content.push(ContentBlock::ToolUse {
                id: ToolUseId::new(),
                name: "Read".into(),
                input: Utf16JsonProjection::from_serde_value(&json!({"private":"secret"})).unwrap(),
                provider_id: None,
            });
            content.push(ContentBlock::ProviderContent {
                protocol: "anthropic_messages".into(),
                value: Utf16JsonProjection::from_serde_value(
                    &json!({"type":"text","text":"answer"}),
                )
                .unwrap(),
            });
        }
        let history = vec![
            assistant(old, "old"),
            assistant(first, "pretool"),
            ConversationMessage::user(MessageId::new(), "private tool/user text".into()),
            final_assistant,
            assistant(unobserved, "not accepted"),
        ];
        let baseline = HashSet::from([old]);
        let observed = control.assistant_message_ids.lock().await.clone();
        let (message_id, text) = accepted_assistant_projection(
            &history,
            &baseline,
            &observed,
            &*control.assistant_display_text.lock().await,
        )
        .unwrap();
        assert_eq!(message_id, accepted.to_string());
        assert_eq!(text, "accepted answer");
        let expected = result(&chat_id, message_id.clone(), text.clone(), None);
        let (task, failed) = control
            .commit_terminal(State::Succeeded, Some(expected.clone()), None, None)
            .unwrap();
        assert!(!failed);
        assert_eq!(task.result.as_ref(), Some(&expected));
        let events = state
            .store
            .protocol_events(&control.owner, "public-run-1", 0)
            .unwrap();
        let mut sdk_history = vec![user];
        sdk_history.extend(sdk_text_messages(&events.events));
        let canonical = state
            .chats
            .protocol_history_page(&chat_id, None, 100)
            .unwrap();
        assert_eq!(sdk_history, canonical.messages);
        assert_eq!(
            sdk_history[1],
            json!({"id":message_id,"role":"assistant","content":text})
        );
        let next = json!({"threadId":chat_id,"runId":"public-run-2","protocolVersion":"1.0",
            "messages":sdk_history.into_iter().chain(std::iter::once(json!({"id":"user-2","role":"user","content":"next"}))).collect::<Vec<_>>()});
        assert!(normalize_page_run_input(&state, next, Some(&chat_id)).is_ok());
    }

    #[tokio::test]
    async fn pending_chat_assistant_stays_hidden_after_restart_and_blocks_followup_admission() {
        let (root, state, control, user, chat_id) = setup();
        let expected = result(
            &chat_id,
            "assistant-pending".into(),
            "private answer".into(),
            None,
        );
        let candidate = state
            .store
            .with_protocol_commit_permit(|permit| {
                state.store.prepare_protocol_terminal(
                    permit,
                    &control.owner,
                    &control.id,
                    State::Succeeded,
                    Some(expected.clone()),
                    None,
                    None,
                )
            })
            .unwrap()
            .unwrap();
        assert_eq!(candidate.state, State::Succeeded);
        state
            .chats
            .record_protocol_message(
                &chat_id,
                &control.id,
                &json!({
                    "id":"assistant-pending","role":"assistant","content":"private answer"
                }),
            )
            .unwrap();
        assert_eq!(
            state
                .chats
                .protocol_history_page(&chat_id, None, 10)
                .unwrap()
                .messages,
            vec![user.clone()]
        );
        let event_path = state
            .root
            .join("tasks/protocol-events")
            .join(format!("{}.jsonl", control.id));
        let event_bytes = std::fs::read(&event_path).unwrap();
        let app_id = control.owner.app_id.clone();
        drop(control);
        drop(state);

        let recovered = AppAgentState::open(root.path(), &app_id).unwrap();
        let recovered_owner = Owner {
            app_id: app_id.clone(),
            release_digest: "2".repeat(64),
            invocation_id: "recovered".into(),
        };
        let task = recovered
            .store
            .get_by_request(&recovered_owner, "public-run-1")
            .unwrap();
        assert_eq!(task.state, State::Interrupted);
        assert!(task.result.is_none());
        assert_eq!(
            task.error.as_ref().unwrap().code,
            "protocol_commit_incomplete"
        );
        assert_eq!(
            recovered
                .chats
                .protocol_history_page(&chat_id, None, 10)
                .unwrap()
                .messages,
            vec![user.clone()]
        );
        assert_eq!(std::fs::read(&event_path).unwrap(), event_bytes);
        assert!(recovered
            .store
            .protocol_events(&recovered_owner, "public-run-1", 0)
            .is_err());

        let next_user = json!({"id":"next-user","role":"user","content":"follow up"});
        let next_input = json!({
            "threadId":chat_id,
            "runId":"public-run-2",
            "protocolVersion":"1.0",
            "messages":[user, next_user]
        });
        assert_eq!(
            normalize_page_run_input(&recovered, next_input, Some(&chat_id)).unwrap_err(),
            "agui_history_unavailable"
        );
        assert!(matches!(
            recovered
                .store
                .get_by_request(&recovered_owner, "public-run-2"),
            Err(local_app::v2::AppError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn structured_only_empty_text_has_one_consistent_assistant_message() {
        let (_root, state, control, user, chat_id) = setup();
        let id = MessageId::new();
        control.emit_assistant_message_identity(&id).await;
        let history = vec![ConversationMessage::Assistant {
            id,
            content: vec![],
            stop_reason: Some("end_turn".into()),
            resumable: None,
            per_turn_effort: None,
        }];
        let observed = control.assistant_message_ids.lock().await.clone();
        let (message_id, text) = accepted_assistant_projection(
            &history,
            &HashSet::new(),
            &observed,
            &*control.assistant_display_text.lock().await,
        )
        .unwrap();
        assert_eq!(text, "");
        let expected = result(&chat_id, message_id, text, Some(json!({"value":7})));
        let (task, failed) = control
            .commit_terminal(State::Succeeded, Some(expected.clone()), None, None)
            .unwrap();
        assert!(!failed);
        assert_eq!(task.result, Some(expected));
        let page = state
            .store
            .protocol_events(&control.owner, "public-run-1", 0)
            .unwrap();
        assert!(page
            .events
            .iter()
            .any(|event| event["type"] == "TEXT_MESSAGE_CONTENT" && event["delta"] == ""));
        let mut sdk_history = vec![user];
        sdk_history.extend(sdk_text_messages(&page.events));
        assert_eq!(
            sdk_history,
            state
                .chats
                .protocol_history_page(&chat_id, None, 100)
                .unwrap()
                .messages
        );
    }

    #[test]
    fn serialized_result_cap_applies_to_stateless_and_session_results() {
        for chat_id in [None, Some("chat".into())] {
            let mut result = TaskResult {
                text: "x".repeat(2 * 1024 * 1024 - 1024),
                output: None,
                attachments: Vec::new(),
                chat_id,
                message_id: Some("message".into()),
            };
            assert!(validate_task_result(&result).is_ok());
            result.text.push_str(&"x".repeat(2048));
            assert_eq!(
                validate_task_result(&result).unwrap_err(),
                "result_too_large"
            );
        }
    }

    #[tokio::test]
    async fn final_content_chunks_preserve_utf8_and_fit_event_limits() {
        let (_root, state, control, user, chat_id) = setup();
        let id = MessageId::new();
        let text = format!("{}🙂{}", "\0".repeat(260_000), "汉".repeat(90_000));
        let expected = result(&chat_id, id.to_string(), text.clone(), None);
        validate_task_result(&expected).unwrap();
        let (task, failed) = control
            .commit_terminal(State::Succeeded, Some(expected.clone()), None, None)
            .unwrap();
        assert!(!failed);
        assert_eq!(task.result, Some(expected));
        let page = state
            .store
            .protocol_events(&control.owner, "public-run-1", 0)
            .unwrap();
        let contents: Vec<_> = page
            .events
            .iter()
            .filter(|event| event["type"] == "TEXT_MESSAGE_CONTENT")
            .collect();
        assert!(contents.len() > 1);
        assert!(contents
            .iter()
            .all(|event| serde_json::to_vec(event).unwrap().len() < 3 * 1024 * 1024));
        let serialized_log_bytes: usize = page
            .events
            .iter()
            .map(|event| serde_json::to_vec(event).unwrap().len() + 64)
            .sum();
        assert!(serialized_log_bytes < 8 * 1024 * 1024);
        assert!(page.events.len() < 10_000);
        assert_eq!(page.events.last().unwrap()["type"], "RUN_FINISHED");
        let mut sdk_history = vec![user];
        sdk_history.extend(sdk_text_messages(&page.events));
        assert_eq!(sdk_history[1]["content"], text);
        assert_eq!(
            sdk_history,
            state
                .chats
                .protocol_history_page(&chat_id, None, 100)
                .unwrap()
                .messages
        );
    }

    #[tokio::test]
    async fn managed_display_override_is_bound_to_surviving_identity_everywhere() {
        let (_root, state, control, user, chat_id) = setup();
        let removed = MessageId::new();
        let accepted = MessageId::new();
        control
            .observe_assistant_display_text(&removed, &Utf16JsonProjection::from("stale display"))
            .await;
        control.emit_assistant_message_identity(&removed).await;
        control
            .observe_assistant_display_text(&accepted, &Utf16JsonProjection::from("[redacted]"))
            .await;
        control.emit_assistant_message_identity(&accepted).await;
        let history = vec![assistant(accepted, "secret that must not be published")];
        let observed = control.assistant_message_ids.lock().await.clone();
        let (id, text) = accepted_assistant_projection(
            &history,
            &HashSet::new(),
            &observed,
            &*control.assistant_display_text.lock().await,
        )
        .unwrap();
        assert_eq!(id, accepted.to_string());
        assert_eq!(text, "[redacted]");
        let expected = result(&chat_id, id.clone(), text, None);
        let (task, failed) = control
            .commit_terminal(State::Succeeded, Some(expected.clone()), None, None)
            .unwrap();
        assert!(!failed);
        assert_eq!(task.result, Some(expected));
        let events = state
            .store
            .protocol_events(&control.owner, "public-run-1", 0)
            .unwrap();
        let mut sdk_history = vec![user];
        sdk_history.extend(sdk_text_messages(&events.events));
        assert_eq!(
            sdk_history,
            state
                .chats
                .protocol_history_page(&chat_id, None, 100)
                .unwrap()
                .messages
        );
        assert!(!serde_json::to_string(&sdk_history)
            .unwrap()
            .contains("secret"));
        assert!(!serde_json::to_string(&events.events)
            .unwrap()
            .contains("secret"));

        // A later valid empty override is meaningful and must replace the
        // previous value rather than exposing the original transcript.
        control
            .observe_assistant_display_text(&accepted, &Utf16JsonProjection::from(""))
            .await;
        let (_, empty) = accepted_assistant_projection(
            &history,
            &HashSet::new(),
            &observed,
            &*control.assistant_display_text.lock().await,
        )
        .unwrap();
        assert_eq!(empty, "");
    }

    #[tokio::test]
    async fn cancellation_failure_and_commit_race_never_publish_drafts() {
        for (kind, final_state) in [
            ("cancelled", State::Cancelled),
            ("error", State::Failed),
            ("race", State::Cancelled),
            ("parent-token", State::Cancelled),
        ] {
            let (_root, state, control, user, chat_id) = setup();
            let id = MessageId::new();
            control.emit_text(&Utf16JsonProjection::from("draft")).await;
            control.emit_assistant_message_identity(&id).await;
            if kind == "race" {
                state
                    .store
                    .request_cancel(&control.owner, &control.id)
                    .unwrap();
            }
            if kind == "parent-token" {
                control.cancel.cancel();
            }
            let (requested, output, error) = match kind {
                "race" | "parent-token" => (
                    State::Succeeded,
                    Some(result(&chat_id, id.to_string(), "draft".into(), None)),
                    None,
                ),
                "error" => (State::Failed, None, Some("model_error".into())),
                _ => (State::Cancelled, None, Some("cancelled".into())),
            };
            let (task, failed) = control
                .commit_terminal(requested, output, None, error)
                .unwrap();
            assert!(!failed);
            assert_eq!(task.state, final_state);
            assert!(task.result.is_none());
            let page = state
                .store
                .protocol_events(&control.owner, "public-run-1", 0)
                .unwrap();
            assert_eq!(page.events.len(), 2);
            assert!(sdk_text_messages(&page.events).is_empty());
            assert_eq!(
                state
                    .chats
                    .protocol_history_page(&chat_id, None, 100)
                    .unwrap()
                    .messages,
                vec![user]
            );
        }
    }

    #[test]
    fn assistant_identity_must_be_current_observed_and_survive_final_snapshot() {
        let old = MessageId::new();
        let retracted = MessageId::new();
        let unobserved = MessageId::new();
        let baseline = HashSet::from([old]);
        let observed = HashSet::from([old, retracted]);
        let history = vec![assistant(old, "old"), assistant(unobserved, "unobserved")];
        assert_eq!(
            accepted_assistant_projection(&history, &baseline, &observed, &HashMap::new())
                .unwrap_err(),
            "chat_message_identity_missing"
        );
    }
}

#[cfg(test)]
mod wake_tests {
    use super::*;

    fn key(app_id: &str, revision: u64, request_id: &str) -> WakeKey {
        WakeKey {
            app_id: app_id.into(),
            revision,
            request_id: request_id.into(),
        }
    }

    #[test]
    fn wake_admission_requires_the_exact_unexpired_reservation() {
        let correct = key("app-a", 7, "wake-a");
        let mut registry = WakeRegistry::default();
        assert!(!registry.available(&correct, 100));
        registry.reserve(correct.clone(), 100, 50).unwrap();
        assert!(!registry.available(&key("app-b", 7, "wake-a"), 101));
        assert!(!registry.available(&key("app-a", 8, "wake-a"), 101));
        assert!(!registry.available(&key("app-a", 7, "wake-b"), 101));
        assert!(registry.available(&correct, 149));
        assert!(!registry.available(&correct, 150));
        assert!(registry.reservations.is_empty());
    }

    #[test]
    fn consumed_wake_is_one_use_and_clear_cancels_only_its_exact_run() {
        let app_a = key("app-a", 7, "wake-a");
        let app_b = key("app-b", 9, "wake-b");
        let mut registry = WakeRegistry::default();
        let receipt = registry.reserve(app_a.clone(), 100, 50).unwrap();
        registry.reserve(app_b.clone(), 100, 50).unwrap();

        assert!(registry.claim(&app_a, 101));
        assert!(!registry.claim(&app_a, 101));
        assert!(registry.consume(&app_a, 101, "run-a"));
        assert!(!registry.available(&app_a, 102));
        assert!(!registry.claim(&app_a, 102));
        let duplicate = registry.reserve(app_a.clone(), 102, 50).unwrap();
        assert_eq!(duplicate.expires_at_ms, 150);
        assert!(!duplicate.created);
        assert_eq!(duplicate.reservation_id, receipt.reservation_id);
        assert!(!registry.claim(&app_a, 102));
        assert!(!registry.available(&app_a, 151));
        assert!(registry.reservations.contains_key(&app_a));
        assert_eq!(
            registry.clear(&app_a, &receipt.reservation_id).as_deref(),
            Some("run-a")
        );
        assert!(registry.clear(&app_a, &receipt.reservation_id).is_none());
        assert!(registry.available(&app_b, 103));
        assert_eq!(registry.reservations.len(), 1);
    }

    #[test]
    fn terminal_cleanup_removes_a_consumed_wake_link() {
        let key = key("app-a", 7, "wake-a");
        let mut registry = WakeRegistry::default();
        let receipt = registry.reserve(key.clone(), 100, 50).unwrap();
        assert!(registry.claim(&key, 101));
        assert!(registry.consume(&key, 101, "run-a"));
        registry.clear_run("run-a");
        assert!(registry.reservations.is_empty());
        assert!(registry.clear(&key, &receipt.reservation_id).is_none());
    }

    #[test]
    fn only_one_live_wake_reservation_per_app_is_allowed() {
        let first = key("app-a", 7, "wake-a");
        let second = key("app-a", 7, "wake-b");
        let other_app = key("app-b", 3, "wake-other");
        let mut registry = WakeRegistry::default();
        let receipt = registry.reserve(first.clone(), 100, 50).unwrap();
        assert_eq!(
            registry.reserve(second.clone(), 101, 50),
            Err("busy".into())
        );
        registry.reserve(other_app, 101, 50).unwrap();
        registry.clear(&first, &receipt.reservation_id);
        registry.reserve(second.clone(), 102, 50).unwrap();
        registry.claim(&second, 103);
        registry.consume(&second, 103, "run-b");
        assert_eq!(registry.reserve(first.clone(), 104, 50), Err("busy".into()));
        registry.clear_run("run-b");
        registry.reserve(first, 105, 50).unwrap();
    }

    #[test]
    fn stopping_an_app_clears_only_its_pending_wake_reservations() {
        let app_a = key("app-a", 7, "wake-a");
        let app_b = key("app-b", 3, "wake-b");
        let mut registry = WakeRegistry::default();
        registry.reserve(app_a.clone(), 100, 50).unwrap();
        registry.reserve(app_b.clone(), 100, 50).unwrap();
        registry.clear_app("app-a");
        assert_eq!(registry.reservations.len(), 1);
        assert!(!registry.available(&app_a, 101));
        assert!(registry.available(&app_b, 101));
    }

    #[test]
    fn stale_and_duplicate_clears_cannot_remove_a_new_reservation_generation() {
        let key = key("app-a", 7, "wake-a");
        let mut registry = WakeRegistry::default();
        let old = registry.reserve(key.clone(), 100, 10).unwrap();
        assert!(old.created);
        let reused = registry.reserve(key.clone(), 101, 10).unwrap();
        assert!(!reused.created);
        assert_eq!(reused.reservation_id, old.reservation_id);
        // A later reservation for the same tuple has an independent owner.
        let new = registry.reserve(key.clone(), 110, 50).unwrap();
        assert!(new.created);
        assert_ne!(new.reservation_id, old.reservation_id);
        assert!(registry.claim(&key, 111));
        assert!(registry.consume(&key, 111, "new-run"));
        assert!(registry.clear(&key, &old.reservation_id).is_none());
        assert_eq!(
            registry.reservations[&key].run_id.as_deref(),
            Some("new-run")
        );
        assert_eq!(
            registry.clear(&key, &new.reservation_id).as_deref(),
            Some("new-run")
        );
        assert!(registry.clear(&key, &new.reservation_id).is_none());
    }

    #[test]
    fn expired_wake_cannot_be_claimed_or_consumed() {
        let key = key("app-a", 7, "wake-a");
        let mut registry = WakeRegistry::default();
        registry.reserve(key.clone(), 100, 1).unwrap();
        assert!(!registry.claim(&key, 101));
        assert!(!registry.consume(&key, 101, "run-a"));
    }

    #[test]
    fn first_agui_store_preserves_legacy_agent_data() {
        let root = tempfile::tempdir().unwrap();
        let agent = root.path().join("agent");
        let tasks = agent.join("tasks");
        let old_task = uuid::Uuid::new_v4().to_string();
        std::fs::create_dir_all(tasks.join(&old_task)).unwrap();
        std::fs::write(tasks.join("agent-tasks.json"), b"old task ledger").unwrap();
        std::fs::write(tasks.join(&old_task).join("output.txt"), b"old task data").unwrap();
        let chats = agent.join("chats");
        let old_chat = uuid::Uuid::new_v4().to_string();
        std::fs::create_dir_all(chats.join(&old_chat)).unwrap();
        std::fs::write(
            chats.join(&old_chat).join("old-transcript.jsonl"),
            b"old chat",
        )
        .unwrap();
        std::fs::write(agent.join("chats-index.json"), b"old chat index").unwrap();
        let spool = agent.join("assets/upload-spool");
        std::fs::create_dir_all(&spool).unwrap();
        std::fs::write(spool.join("preserved"), b"unrelated upload spool").unwrap();
        std::fs::write(agent.join("app-data.json"), b"preserved app data").unwrap();

        assert_eq!(
            initialize_agui_agent_store(&agent).unwrap_err(),
            "agent_legacy_store_unsupported"
        );
        assert_eq!(
            std::fs::read(tasks.join("agent-tasks.json")).unwrap(),
            b"old task ledger"
        );
        assert_eq!(
            std::fs::read(tasks.join(old_task).join("output.txt")).unwrap(),
            b"old task data"
        );
        assert_eq!(
            std::fs::read(agent.join("chats-index.json")).unwrap(),
            b"old chat index"
        );
        assert_eq!(
            std::fs::read(chats.join(old_chat).join("old-transcript.jsonl")).unwrap(),
            b"old chat"
        );
        assert!(!agent.join("agui-v1").exists());
        assert_eq!(
            std::fs::read(spool.join("preserved")).unwrap(),
            b"unrelated upload spool"
        );
        assert_eq!(
            std::fs::read(agent.join("app-data.json")).unwrap(),
            b"preserved app data"
        );
    }

    #[test]
    fn missing_or_incompatible_protocol_marker_preserves_existing_data() {
        let root = tempfile::tempdir().unwrap();
        for (name, marker, expected) in [
            ("missing", None, "agent_protocol_store_marker_missing"),
            (
                "mismatch",
                Some(br#"{"protocol":"AG-UI","version":"unsupported"}"#.as_slice()),
                "agent_protocol_store_version_mismatch",
            ),
        ] {
            let agent = root.path().join(name);
            let store = agent.join("agui-v1");
            std::fs::create_dir_all(&store).unwrap();
            let data = store.join("existing-data");
            std::fs::write(&data, b"preserve this data").unwrap();
            let marker_path = store.join("protocol-store.json");
            if let Some(bytes) = marker {
                std::fs::write(&marker_path, bytes).unwrap();
            }
            assert_eq!(initialize_agui_agent_store(&agent).unwrap_err(), expected);
            assert_eq!(std::fs::read(&data).unwrap(), b"preserve this data");
            match marker {
                Some(bytes) => assert_eq!(std::fs::read(marker_path).unwrap(), bytes),
                None => assert!(!marker_path.exists()),
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn legacy_store_rejects_symlinked_task_roots_without_following_them() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let agent = root.path().join("agent");
        std::fs::create_dir_all(&agent).unwrap();
        let outside = root.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("important"), b"keep").unwrap();
        symlink(&outside, agent.join("tasks")).unwrap();
        assert!(initialize_agui_agent_store(&agent).is_err());
        assert_eq!(std::fs::read(outside.join("important")).unwrap(), b"keep");
        assert!(!agent.join("agui-v1").exists());
    }
}

#[cfg(test)]
mod protocol_retry_tests {
    use super::progress_tests::{complete_protocol_chat_turn, test_wake_host};
    use super::*;
    use local_app::v2::agent::{Model, Settings, TaskResult};

    pub(super) async fn test_host(
        root: &Path,
    ) -> (Arc<AppAgentHost>, Arc<MobileLocalAppServiceHandle>) {
        use crate::mobile::test_support::{
            test_config, CollectingPermissionSink, FakeListener, HostFakePlatform,
        };
        let cfg = test_config(root);
        let runtime = crate::mobile::host::build_mobile(
            cfg.clone(),
            Arc::new(HostFakePlatform::new(root.to_path_buf())),
            Arc::new(FakeListener::default()),
            Arc::new(CollectingPermissionSink::default()),
        )
        .await
        .unwrap();
        let service_home = root.join("user").join(branding::DOT_DIR);
        let service = MobileLocalAppServiceHandle::new(service_home, None, "ios".into()).unwrap();
        let host = AppAgentHost::new(
            &service,
            runtime.app_agent_factory.clone(),
            &cfg.lingxi_home,
        )
        .unwrap();
        (host, service)
    }

    fn owner(app_id: &str) -> Owner {
        Owner {
            app_id: app_id.to_owned(),
            release_digest: "2".repeat(64),
            invocation_id: "protocol-retry-test".into(),
        }
    }

    pub(super) fn settings() -> Settings {
        Settings {
            model: Model {
                provider_id: "provider".into(),
                model_id: "model".into(),
            },
            tools: BTreeMap::new(),
            skills: Vec::new(),
            background: false,
            max_turns: 4,
            timeout_ms: 30_000,
        }
    }

    pub(super) fn chat_control(
        owner: Owner,
        revision: u64,
        request_id: String,
        id: String,
        chat_id: String,
        user_message_id: String,
        store: Arc<AgentStore>,
        chats: Arc<ChatStore>,
        chat_persisted: watch::Sender<Option<Result<(), String>>>,
    ) -> Control {
        Control {
            owner,
            revision,
            request_id,
            id,
            protocol_thread_id: Some(chat_id.clone()),
            cancel: CancellationToken::new(),
            finished: AtomicBool::new(false),
            done: Notify::new(),
            lease: Mutex::new(None),
            interaction: AsyncMutex::new(None),
            store,
            chats,
            chat_id: Some(chat_id),
            user_message_id: Some(user_message_id),
            chat_orchestrator: Mutex::new(None),
            chat_persisted,
            chat_commit: AsyncMutex::new(None),
            attachment_metadata: Vec::new(),
            assistant_message_ids: AsyncMutex::new(HashSet::new()),
            assistant_display_text: AsyncMutex::new(HashMap::new()),
            protocol_events_failed: AtomicBool::new(false),
            native_output: NativeOutputProgress::default(),
            text: AsyncMutex::new(String::new()),
            model_failed: AtomicBool::new(false),
            model_failure: Mutex::new(None),
            timed_out: AtomicBool::new(false),
            usage: AsyncMutex::new(None),
            fail_after_final_ledger_before_chat_authority: AtomicBool::new(false),
            after_final_ledger_error_barrier: Mutex::new(None),
        }
    }

    #[test]
    fn chat_run_retries_use_original_fingerprint_after_history_advances() {
        let directory = tempfile::tempdir().unwrap();
        let app_id = format!("a{}", "1".repeat(32));
        std::fs::create_dir_all(directory.path().join(&app_id)).unwrap();
        let state = AppAgentState::open(directory.path(), &app_id).unwrap();
        let owner = owner(&app_id);
        state
            .store
            .configure(owner.clone(), Some(settings()))
            .unwrap();
        let chat = state
            .chats
            .create(
                &uuid::Uuid::new_v4().to_string(),
                None,
                &uuid::Uuid::new_v4().to_string(),
            )
            .unwrap();
        let input = json!({
            "threadId":chat.chat_id,
            "runId":"public-run-1",
            "protocolVersion":"1.0",
            "messages":[{"id":"user-message-1","role":"user","content":"hello"}]
        });

        assert_eq!(
            protocol_duplicate_run(&state, &owner, &input, Some(&chat.chat_id)).unwrap(),
            None
        );
        let (normalized, chat_id) =
            normalize_page_run_input(&state, input.clone(), Some(&chat.chat_id)).unwrap();
        let admitted = state
            .store
            .admit_scoped(
                &owner,
                &normalized.input,
                &[],
                &[],
                chat_id.as_deref(),
                true,
            )
            .unwrap();
        state
            .store
            .update(
                &owner,
                &admitted.task.run_id,
                State::Running,
                None,
                None,
                None,
            )
            .unwrap();
        state
            .store
            .append_event(
                &owner,
                &admitted.task.run_id,
                json!({
                    "type":"RUN_STARTED",
                    "protocolVersion":"1.0",
                    "threadId":chat.chat_id,
                    "runId":"public-run-1"
                }),
            )
            .unwrap();
        state
            .chats
            .start_turn(&chat.chat_id, &admitted.task.run_id)
            .unwrap();
        let task_user_message_id =
            MessageId::from_uuid(uuid::Uuid::parse_str(&admitted.task.run_id).unwrap()).to_string();
        state
            .chats
            .record_message(
                &chat.chat_id,
                ChatMessageMetadata {
                    message_id: task_user_message_id,
                    run_id: admitted.task.run_id.clone(),
                    role: ChatRole::User,
                    created_at: now_millis(),
                    attachments: Vec::new(),
                },
            )
            .unwrap();
        state
            .chats
            .record_protocol_message(
                &chat.chat_id,
                &admitted.task.run_id,
                &normalized.user_message,
            )
            .unwrap();
        assert_eq!(
            protocol_duplicate_run(&state, &owner, &input, Some(&chat.chat_id))
                .unwrap()
                .as_deref(),
            Some("public-run-1"),
            "an active run retry must not append or execute again"
        );

        let assistant = json!({
            "id":"assistant-message-1",
            "role":"assistant",
            "content":"answer"
        });
        state
            .chats
            .record_protocol_message(&chat.chat_id, &admitted.task.run_id, &assistant)
            .unwrap();
        commit_agent_terminal_for_test(
            &state.store,
            &owner,
            &admitted.task.run_id,
            State::Succeeded,
            Some(TaskResult {
                text: "answer".into(),
                output: None,
                attachments: Vec::new(),
                chat_id: Some(chat.chat_id.clone()),
                message_id: Some("assistant-message-1".into()),
            }),
            None,
        )
        .unwrap();
        let accepted = state.store.committed_protocol_assistants().unwrap();
        assert_eq!(
            accepted.keys().next().map(String::as_str),
            Some(admitted.task.run_id.as_str())
        );
        state.chats.set_committed_assistants(&accepted).unwrap();
        state
            .chats
            .finish_turn(&chat.chat_id, &admitted.task.run_id)
            .unwrap();

        drop(state);
        let recovered = AppAgentState::open(directory.path(), &app_id).unwrap();
        assert_eq!(
            recovered
                .chats
                .protocol_history_page(&chat.chat_id, None, 10)
                .unwrap()
                .messages,
            vec![normalized.user_message.clone(), assistant.clone()],
            "the ledger's internal task UUID must authorize assistant visibility after restart"
        );

        assert_eq!(
            protocol_duplicate_run(&recovered, &owner, &input, Some(&chat.chat_id))
                .unwrap()
                .as_deref(),
            Some("public-run-1"),
            "a terminal run retry must match its original request after history advances"
        );
        let changed_prefix = json!({
            "threadId":chat.chat_id,
            "runId":"public-run-1",
            "protocolVersion":"1.0",
            "messages":[
                assistant,
                {"id":"user-message-2","role":"user","content":"different turn"}
            ]
        });
        assert_eq!(
            protocol_duplicate_run(&recovered, &owner, &changed_prefix, Some(&chat.chat_id))
                .unwrap_err(),
            "request_conflict"
        );
        assert_eq!(
            recovered
                .chats
                .protocol_history_page(&chat.chat_id, None, 10)
                .unwrap()
                .messages
                .len(),
            2,
            "deduplicated retries must not append duplicate messages"
        );
    }

    #[tokio::test]
    async fn production_duplicate_waits_for_chat_transcript_failure() {
        let root = tempfile::tempdir().unwrap();
        let (host, service) = test_host(root.path()).await;
        let source = root.path().join("duplicate-handshake-source");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("index.html"), "fixture").unwrap();
        std::fs::write(
            source.join("lingxi-app.json"),
            json!({
                "schema":2,
                "name":"Duplicate handshake test",
                "entry":{"type":"static","path":"index.html"},
                "targets":[{"platform":"ios","environment":"web"}],
                "capabilities":["agent"],
                "agent":{"tools":[]}
            })
            .to_string(),
        )
        .unwrap();
        let published = service
            .request_value(json!({
                "op":"publish",
                "expectedRevision":0,
                "sourcePath":source
            }))
            .await
            .unwrap();
        let app_id = published["record"]["appId"].as_str().unwrap().to_owned();
        let revision = published["record"]["currentRevision"].as_u64().unwrap();
        let (owner, _, _) = host
            .current_release(&app_id, revision, "native-intent")
            .await
            .unwrap();
        let state = host.app_state(&app_id).unwrap();
        state
            .store
            .configure(owner.clone(), Some(settings()))
            .unwrap();
        let chat = state
            .chats
            .create(
                &uuid::Uuid::new_v4().to_string(),
                None,
                &uuid::Uuid::new_v4().to_string(),
            )
            .unwrap();
        let input_value = json!({
            "threadId":chat.chat_id,
            "runId":"handshake-failure-run",
            "protocolVersion":"1.0",
            "messages":[{"id":"handshake-user","role":"user","content":"hello"}]
        });
        let (normalized, chat_id) =
            normalize_page_run_input(&state, input_value.clone(), Some(&chat.chat_id)).unwrap();
        let admitted = state
            .store
            .admit_scoped(
                &owner,
                &normalized.input,
                &[],
                &[],
                chat_id.as_deref(),
                true,
            )
            .unwrap();
        state
            .store
            .update(
                &owner,
                &admitted.task.run_id,
                State::Running,
                None,
                None,
                None,
            )
            .unwrap();
        state
            .store
            .append_event(
                &owner,
                &admitted.task.run_id,
                json!({
                    "type":"RUN_STARTED",
                    "protocolVersion":"1.0",
                    "threadId":chat.chat_id,
                    "runId":"handshake-failure-run"
                }),
            )
            .unwrap();
        state
            .chats
            .start_turn(&chat.chat_id, &admitted.task.run_id)
            .unwrap();
        let user_row_id =
            MessageId::from_uuid(uuid::Uuid::parse_str(&admitted.task.run_id).unwrap()).to_string();
        state
            .chats
            .record_message(
                &chat.chat_id,
                ChatMessageMetadata {
                    message_id: user_row_id.clone(),
                    run_id: admitted.task.run_id.clone(),
                    role: ChatRole::User,
                    created_at: now_millis(),
                    attachments: Vec::new(),
                },
            )
            .unwrap();
        state
            .chats
            .record_protocol_message(
                &chat.chat_id,
                &admitted.task.run_id,
                &normalized.user_message,
            )
            .unwrap();
        let (chat_persisted, _) = watch::channel(None);
        let control = Arc::new(chat_control(
            owner.clone(),
            revision,
            admitted.task.request_id.clone(),
            admitted.task.run_id.clone(),
            chat.chat_id.clone(),
            user_row_id,
            state.store.clone(),
            state.chats.clone(),
            chat_persisted,
        ));
        host.active
            .lock()
            .await
            .insert(admitted.task.run_id.clone(), control.clone());
        let (wait_started_tx, wait_started_rx) = oneshot::channel();
        *host.duplicate_chat_wait_started.lock().unwrap() = Some(wait_started_tx);
        let host_for_retry = host.clone();
        let app_for_retry = app_id.clone();
        let retry_input = input_value.clone();
        let mut retry = tokio::spawn(async move {
            host_for_retry
                .host(json!({
                    "op":"startNative",
                    "appId":app_for_retry,
                    "revision":revision,
                    "input":retry_input
                }))
                .await
        });
        wait_started_rx.await.unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), &mut retry)
                .await
                .is_err()
        );
        control.report_chat_persisted(Err("chat_transcript_unavailable".into()));
        assert_eq!(
            retry.await.unwrap().unwrap_err(),
            "chat_transcript_unavailable"
        );
        assert_eq!(
            state
                .store
                .protocol_events(&owner, "handshake-failure-run", 0)
                .unwrap()
                .events
                .len(),
            1,
            "the duplicate waits for the original transcript outcome and does not start a second run"
        );
        host.active.lock().await.remove(&admitted.task.run_id);
    }

    #[tokio::test]
    async fn concurrent_none_probes_release_admission_for_wake_clear_during_chat_init() {
        let root = tempfile::tempdir().unwrap();
        let (host, service) = test_host(root.path()).await;
        let source = root.path().join("duplicate-init-race-source");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("index.html"), "fixture").unwrap();
        std::fs::write(
            source.join("lingxi-app.json"),
            json!({
                "schema":2,
                "name":"Duplicate init race test",
                "entry":{"type":"static","path":"index.html"},
                "targets":[{"platform":"ios","environment":"web"}],
                "capabilities":["agent"],
                "agent":{"tools":[]}
            })
            .to_string(),
        )
        .unwrap();
        let published = service
            .request_value(json!({
                "op":"publish",
                "expectedRevision":0,
                "sourcePath":source
            }))
            .await
            .unwrap();
        let app_id = published["record"]["appId"].as_str().unwrap().to_owned();
        let revision = published["record"]["currentRevision"].as_u64().unwrap();
        service
            .request_value(json!({
                "op":"grant",
                "appId":app_id,
                "digest":published["release"]["digest"]
            }))
            .await
            .unwrap();
        let launch = service
            .request_value(json!({"op":"start","appId":app_id,"revision":revision}))
            .await
            .unwrap();
        let invocation_id = launch["run"]["runId"].as_str().unwrap().to_owned();
        let (owner, _, _) = host
            .current_release(&app_id, revision, &invocation_id)
            .await
            .unwrap();
        let state = host.app_state(&app_id).unwrap();
        state
            .store
            .configure(owner.clone(), Some(settings()))
            .unwrap();
        let chat = state
            .chats
            .create(
                &uuid::Uuid::new_v4().to_string(),
                None,
                &uuid::Uuid::new_v4().to_string(),
            )
            .unwrap();
        let run_id = "duplicate-init-race-run";
        let reservation = host
            .host(json!({
                "op":"wake.reserve",
                "appId":app_id,
                "revision":revision,
                "runId":run_id,
                "receiveTimeoutMs":120000
            }))
            .await
            .unwrap();
        host.force_wake_policy.store(true, Ordering::Release);
        *host.duplicate_probe_none_barrier.lock().unwrap() =
            Some(Arc::new(tokio::sync::Barrier::new(2)));
        let (executor_started_tx, executor_started_rx) = oneshot::channel();
        let (executor_release_tx, executor_release_rx) = oneshot::channel();
        *host.chat_executor_init_pause.lock().unwrap() =
            Some((executor_started_tx, executor_release_rx));
        let (duplicate_waiting_tx, duplicate_waiting_rx) = oneshot::channel();
        *host.duplicate_chat_wait_started.lock().unwrap() = Some(duplicate_waiting_tx);

        let raw_input = json!({
            "threadId":chat.chat_id.clone(),
            "runId":run_id,
            "protocolVersion":"1.0",
            "messages":[{"id":"duplicate-init-user","role":"user","content":"hello"}]
        });
        let start_page = |host: Arc<AppAgentHost>, raw_input: Value| {
            let app_id = app_id.clone();
            let chat_id = chat.chat_id.clone();
            let invocation_id = invocation_id.clone();
            tokio::spawn(async move {
                host.page(
                    &app_id,
                    &invocation_id,
                    "agent.chats.send",
                    json!({"chatId":chat_id,"input":raw_input}),
                )
                .await
            })
        };
        let mut first = start_page(host.clone(), raw_input.clone());
        let mut second = start_page(host.clone(), raw_input);

        tokio::time::timeout(std::time::Duration::from_secs(5), executor_started_rx)
            .await
            .expect("both duplicate probes must pass before the first admission")
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), duplicate_waiting_rx)
            .await
            .expect("the second admission must subscribe to the original Control")
            .unwrap();

        // The original executor has not persisted its Harness user row. A
        // system expiration still needs admission to revoke it and finish.
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            host.host(json!({
                "op":"wake.clear",
                "appId":app_id.clone(),
                "revision":revision,
                "runId":run_id,
                "reservationId":reservation["reservationId"],
                "reason":"system_expiration"
            })),
        )
        .await
        .expect("wake.clear must not wait behind the duplicate admission lock")
        .unwrap();
        let _ = executor_release_tx.send(());
        let first_result = tokio::time::timeout(std::time::Duration::from_secs(5), &mut first)
            .await
            .expect("the initial Chat caller must finish")
            .unwrap();
        let second_result = tokio::time::timeout(std::time::Duration::from_secs(5), &mut second)
            .await
            .expect("the duplicate waiter must observe cancellation")
            .unwrap();
        assert!(first_result.is_err());
        assert!(second_result.is_err());
        let task = state.store.get_by_request(&owner, run_id).unwrap();
        assert_eq!(task.state, State::Interrupted);
        assert_eq!(task.error.as_ref().unwrap().code, "ios_background_expired");
        assert!(host.active.lock().await.is_empty());
        assert_eq!(
            state.chats.get(&chat.chat_id).unwrap().state,
            ChatState::Interrupted
        );
        service
            .request_value(json!({"op":"stop","appId":app_id}))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn production_start_native_retry_is_idempotent_before_and_after_commit_and_restart() {
        async fn start_native(
            host: &Arc<AppAgentHost>,
            app_id: &str,
            revision: u64,
            input: Value,
        ) -> Result<Value, String> {
            host.host(json!({
                "op":"startNative",
                "appId":app_id,
                "revision":revision,
                "input":input
            }))
            .await
        }

        let root = tempfile::tempdir().unwrap();
        let (host, service) = test_wake_host(root.path()).await;
        let source = root.path().join("native-retry-source");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("index.html"), "fixture").unwrap();
        std::fs::write(
            source.join("lingxi-app.json"),
            json!({
                "schema":2,
                "name":"Native retry test",
                "entry":{"type":"static","path":"index.html"},
                "targets":[{"platform":"ios","environment":"web"}],
                "capabilities":["agent"],
                "agent":{"tools":[]}
            })
            .to_string(),
        )
        .unwrap();
        let published = service
            .request_value(json!({
                "op":"publish",
                "expectedRevision":0,
                "sourcePath":source
            }))
            .await
            .unwrap();
        let app_id = published["record"]["appId"].as_str().unwrap().to_owned();
        let revision = published["record"]["currentRevision"].as_u64().unwrap();
        let (owner, _, _) = host
            .current_release(&app_id, revision, "native-intent")
            .await
            .unwrap();
        let state = host.app_state(&app_id).unwrap();
        state
            .store
            .configure(owner.clone(), Some(settings()))
            .unwrap();
        let chat = state
            .chats
            .create(
                &uuid::Uuid::new_v4().to_string(),
                None,
                &uuid::Uuid::new_v4().to_string(),
            )
            .unwrap();
        let raw = json!({
            "threadId":chat.chat_id,
            "runId":"native-idempotent-run",
            "protocolVersion":"1.0",
            "messages":[{"id":"native-user-message","role":"user","content":"hello"}]
        });
        let (normalized, chat_id) =
            normalize_page_run_input(&state, raw.clone(), Some(&chat.chat_id)).unwrap();
        let admission = state
            .store
            .admit_scoped(
                &owner,
                &normalized.input,
                &[],
                &[],
                chat_id.as_deref(),
                true,
            )
            .unwrap();
        state
            .store
            .update(
                &owner,
                &admission.task.run_id,
                State::Running,
                None,
                None,
                None,
            )
            .unwrap();
        state
            .store
            .append_event(
                &owner,
                &admission.task.run_id,
                json!({
                    "type":"RUN_STARTED",
                    "protocolVersion":"1.0",
                    "threadId":chat.chat_id,
                    "runId":"native-idempotent-run"
                }),
            )
            .unwrap();
        state
            .chats
            .start_turn(&chat.chat_id, &admission.task.run_id)
            .unwrap();
        let task_user_message_id =
            MessageId::from_uuid(uuid::Uuid::parse_str(&admission.task.run_id).unwrap())
                .to_string();
        state
            .chats
            .record_message(
                &chat.chat_id,
                ChatMessageMetadata {
                    message_id: task_user_message_id,
                    run_id: admission.task.run_id.clone(),
                    role: ChatRole::User,
                    created_at: now_millis(),
                    attachments: Vec::new(),
                },
            )
            .unwrap();
        state
            .chats
            .record_protocol_message(
                &chat.chat_id,
                &admission.task.run_id,
                &normalized.user_message,
            )
            .unwrap();

        let (chat_persisted, _) = watch::channel(Some(Ok(())));
        let active_control = Arc::new(chat_control(
            owner.clone(),
            revision,
            admission.task.request_id.clone(),
            admission.task.run_id.clone(),
            chat.chat_id.clone(),
            MessageId::from_uuid(uuid::Uuid::parse_str(&admission.task.run_id).unwrap())
                .to_string(),
            state.store.clone(),
            state.chats.clone(),
            chat_persisted,
        ));
        host.active
            .lock()
            .await
            .insert(admission.task.run_id.clone(), active_control.clone());

        assert_eq!(
            start_native(&host, &app_id, revision, raw.clone())
                .await
                .unwrap()["runId"],
            "native-idempotent-run",
            "an active retry must resolve before current-history normalization"
        );
        assert_eq!(
            state
                .store
                .protocol_events(&owner, "native-idempotent-run", 0)
                .unwrap()
                .events
                .len(),
            1,
            "the active retry must not append a second run or execute it again"
        );

        let assistant = json!({
            "id":"native-assistant-message",
            "role":"assistant",
            "content":"answer"
        });
        state
            .chats
            .record_protocol_message(&chat.chat_id, &admission.task.run_id, &assistant)
            .unwrap();
        commit_agent_terminal_for_test(
            &state.store,
            &owner,
            &admission.task.run_id,
            State::Succeeded,
            Some(TaskResult {
                text: "answer".into(),
                output: None,
                attachments: Vec::new(),
                chat_id: Some(chat.chat_id.clone()),
                message_id: Some("native-assistant-message".into()),
            }),
            None,
        )
        .unwrap();
        let accepted = state.store.committed_protocol_assistants().unwrap();
        state.chats.set_committed_assistants(&accepted).unwrap();
        state
            .chats
            .finish_turn(&chat.chat_id, &admission.task.run_id)
            .unwrap();
        host.active.lock().await.remove(&admission.task.run_id);
        drop(active_control);
        assert_eq!(
            host.start_task(
                owner.clone(),
                revision,
                &[],
                &[],
                normalized.input.clone(),
                Some(normalized.user_message.clone()),
                Some(chat.chat_id.clone()),
                CancellationToken::new(),
                true,
                true,
            )
            .await
            .unwrap(),
            admission.task.run_id.clone(),
            "an inactive Chat duplicate must validate its committed success before returning"
        );
        assert_eq!(
            start_native(&host, &app_id, revision, raw.clone())
                .await
                .unwrap()["runId"],
            "native-idempotent-run"
        );

        let mut changed_input = raw.clone();
        changed_input["messages"][0]["content"] = json!("different input");
        assert_eq!(
            start_native(&host, &app_id, revision, changed_input)
                .await
                .unwrap_err(),
            "request_conflict"
        );
        let other_chat = state
            .chats
            .create(
                &uuid::Uuid::new_v4().to_string(),
                None,
                &uuid::Uuid::new_v4().to_string(),
            )
            .unwrap();
        assert_eq!(
            host.page(
                &app_id,
                "native-intent",
                "agent.chats.send",
                json!({"chatId":other_chat.chat_id,"input":raw.clone()}),
            )
            .await
            .unwrap_err(),
            "chat_thread_mismatch",
            "the page Chat route must validate its outer binding before duplicate lookup"
        );
        assert_eq!(
            protocol_duplicate_run(&state, &owner, &raw, Some(&other_chat.chat_id)).unwrap_err(),
            "chat_thread_mismatch",
            "agent.chats.send must validate its outer Chat binding before duplicate lookup"
        );
        let mut different_chat = raw.clone();
        different_chat["threadId"] = json!(other_chat.chat_id);
        assert_eq!(
            start_native(&host, &app_id, revision, different_chat)
                .await
                .unwrap_err(),
            "request_conflict"
        );

        let factory = host.factory.clone();
        drop(host);
        let reopened_host = AppAgentHost::new(&service, factory, root.path()).unwrap();
        assert_eq!(
            start_native(&reopened_host, &app_id, revision, raw)
                .await
                .unwrap()["runId"],
            "native-idempotent-run",
            "a restart retry must resolve the original committed request"
        );
        assert_eq!(
            reopened_host
                .page(
                    &app_id,
                    "native-intent",
                    "agent.chats.send",
                    json!({"chatId":chat.chat_id,"input":json!({
                        "threadId":chat.chat_id,
                        "runId":"native-idempotent-run",
                        "protocolVersion":"1.0",
                        "messages":[{"id":"native-user-message","role":"user","content":"hello"}]
                    })}),
                )
                .await
                .unwrap()["runId"],
            "native-idempotent-run",
            "agent.chats.send must resolve a committed retry after restart"
        );
        let reopened_state = reopened_host.app_state(&app_id).unwrap();
        assert_eq!(
            reopened_state
                .chats
                .protocol_history_page(&chat.chat_id, None, 10)
                .unwrap()
                .messages
                .len(),
            2,
            "retries must not append duplicate Chat messages"
        );
        assert!(reopened_state.store.settings(&owner).unwrap().is_some());

        // The page-side settings read has succeeded, but the duplicate probe
        // has not acquired admission yet. Revoke first; its locked recheck must
        // reject the otherwise-valid duplicate.
        let (probe_ready_tx, probe_ready_rx) = oneshot::channel();
        let (probe_release_tx, probe_release_rx) = oneshot::channel();
        *reopened_host.duplicate_pre_admission_pause.lock().unwrap() =
            Some((probe_ready_tx, probe_release_rx));
        let probe_host = reopened_host.clone();
        let probe_app = app_id.clone();
        let probe_input = json!({
            "threadId":chat.chat_id,
            "runId":"native-idempotent-run",
            "protocolVersion":"1.0",
            "messages":[{"id":"native-user-message","role":"user","content":"hello"}]
        });
        let duplicate = tokio::spawn(async move {
            probe_host
                .host(json!({
                    "op":"startNative",
                    "appId":probe_app,
                    "revision":revision,
                    "input":probe_input
                }))
                .await
        });
        probe_ready_rx.await.unwrap();
        reopened_host
            .host(json!({"op":"configure","appId":app_id,"settings":null}))
            .await
            .unwrap();
        probe_release_tx.send(()).unwrap();
        assert_eq!(
            duplicate.await.unwrap().unwrap_err(),
            "agent_not_authorized"
        );
    }

    #[tokio::test]
    async fn recovered_inline_tool_transcript_repairs_owner_for_admission_history() {
        let directory = tempfile::tempdir().unwrap();
        let app_id = format!("a{}", "4".repeat(32));
        std::fs::create_dir_all(directory.path().join(&app_id)).unwrap();
        let state = AppAgentState::open(directory.path(), &app_id).unwrap();
        let owner = owner(&app_id);
        state
            .store
            .configure(owner.clone(), Some(settings()))
            .unwrap();
        let session_id = uuid::Uuid::new_v4();
        let chat = state
            .chats
            .create(
                &uuid::Uuid::new_v4().to_string(),
                None,
                &session_id.to_string(),
            )
            .unwrap();
        let first_raw = json!({
            "threadId":chat.chat_id,
            "runId":"tool-turn-one",
            "protocolVersion":"1.0",
            "messages":[{"id":"tool-user-one","role":"user","content":"read a file"}]
        });
        let first_assistant_id = MessageId::new().to_string();
        let (first_task_id, first_user, first_assistant) = complete_protocol_chat_turn(
            &state,
            &owner,
            &chat.chat_id,
            first_raw,
            &first_assistant_id,
            "The file says hello.",
        );

        let second_raw = json!({
            "threadId":chat.chat_id,
            "runId":"tool-turn-two",
            "protocolVersion":"1.0",
            "messages":[{"id":"tool-user-two","role":"user","content":"summarize that"}]
        });
        let second_assistant_id = MessageId::new().to_string();
        let (second_task_id, second_user, second_assistant) = complete_protocol_chat_turn(
            &state,
            &owner,
            &chat.chat_id,
            second_raw,
            &second_assistant_id,
            "The file contains a greeting.",
        );

        let transcript_path = state.chats.transcript_path(&chat.chat_id).unwrap();
        assert_eq!(
            transcript_path,
            state
                .root
                .join("chats")
                .join(&chat.chat_id)
                .join(format!("{session_id}.jsonl")),
            "the fixture must use the dedicated AppAgent Chat UUID path"
        );
        let first_task_uuid = uuid::Uuid::parse_str(&first_task_id).unwrap();
        let second_task_uuid = uuid::Uuid::parse_str(&second_task_id).unwrap();
        let assistant_tool_uuid = uuid::Uuid::new_v4();
        let assistant_tool_message_uuid = uuid::Uuid::new_v4();
        let tool_result_uuid = uuid::Uuid::new_v4();
        let first_assistant_uuid = uuid::Uuid::new_v4();
        let second_assistant_uuid = uuid::Uuid::new_v4();
        let first_assistant_message_uuid = MessageId::parse_prefixed(&first_assistant_id)
            .unwrap()
            .as_uuid()
            .to_string();
        let second_assistant_message_uuid = MessageId::parse_prefixed(&second_assistant_id)
            .unwrap()
            .as_uuid()
            .to_string();
        let tool_use_id = ToolUseId::new().to_string();
        let cwd = directory.path().to_string_lossy().to_string();
        let timestamp = "2026-10-28T12:00:00.000Z";
        let rows = vec![
            json!({
                "type":"user","uuid":first_task_uuid,"parentUuid":null,
                "sessionId":session_id,"timestamp":timestamp,"cwd":cwd,
                "message":{"role":"user","content":"read a file"}
            }),
            json!({
                "type":"assistant","uuid":assistant_tool_uuid,"parentUuid":first_task_uuid,
                "sessionId":session_id,"timestamp":timestamp,"cwd":cwd,
                "message":{"id":assistant_tool_message_uuid,"role":"assistant","content":[{
                    "type":"tool_use","id":tool_use_id,"name":"Read","input":{"path":"hello.txt"}
                }]}
            }),
            json!({
                "type":"user","uuid":tool_result_uuid,"parentUuid":assistant_tool_uuid,
                "sessionId":session_id,"timestamp":timestamp,"cwd":cwd,
                "message":{"role":"user","content":[{
                    "type":"tool_result","tool_use_id":tool_use_id,
                    "content":"hello","is_error":false
                }]}
            }),
            json!({
                "type":"assistant","uuid":first_assistant_uuid,"parentUuid":tool_result_uuid,
                "sessionId":session_id,"timestamp":timestamp,"cwd":cwd,
                "message":{"id":first_assistant_message_uuid,"role":"assistant","content":[{"type":"text","text":"The file says hello."}]}
            }),
            json!({
                "type":"user","uuid":second_task_uuid,"parentUuid":first_assistant_uuid,
                "sessionId":session_id,"timestamp":timestamp,"cwd":cwd,
                "message":{"role":"user","content":"summarize that"}
            }),
            json!({
                "type":"assistant","uuid":second_assistant_uuid,"parentUuid":second_task_uuid,
                "sessionId":session_id,"timestamp":timestamp,"cwd":cwd,
                "message":{"id":second_assistant_message_uuid,"role":"assistant","content":[{"type":"text","text":"The file contains a greeting."}]}
            }),
        ];
        let transcript = rows
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        std::fs::write(&transcript_path, transcript.as_bytes()).unwrap();

        // Preserve a stale row written by the old recovery path. Its random
        // transcript MessageId must be corrected from the owning ToolUse.
        state
            .chats
            .record_message(
                &chat.chat_id,
                ChatMessageMetadata {
                    message_id: MessageId::from_uuid(tool_result_uuid).to_string(),
                    run_id: tool_result_uuid.to_string(),
                    role: ChatRole::User,
                    created_at: now_millis(),
                    attachments: Vec::new(),
                },
            )
            .unwrap();

        drop(state);
        let recovered = AppAgentState::open(directory.path(), &app_id).unwrap();
        assert_eq!(
            recovered.chats.get(&chat.chat_id).unwrap().state,
            ChatState::Idle
        );
        let fs: Arc<dyn lingxi_core::host::FileSystem> = Arc::new(
            platform_posix_minimal::PosixFileSystem::new(directory.path().to_path_buf()),
        );
        let replayed =
            orchestrator::resume::replay_session_state_from_path(&transcript_path, session_id, fs)
                .await
                .unwrap();
        super::super::validate_local_app_chat_resume(&replayed).unwrap();
        validate_chat_result_in_transcript(
            &replayed,
            &TaskResult {
                text: "The file contains a greeting.".into(),
                output: None,
                attachments: Vec::new(),
                chat_id: Some(chat.chat_id.clone()),
                message_id: Some(second_assistant_id.clone()),
            },
        )
        .unwrap();
        assert!(validate_chat_result_in_transcript(
            &replayed,
            &TaskResult {
                text: "different answer".into(),
                output: None,
                attachments: Vec::new(),
                chat_id: Some(chat.chat_id.clone()),
                message_id: Some(second_assistant_id.clone()),
            }
        )
        .is_err());
        assert!(validate_chat_result_in_transcript(
            &replayed,
            &TaskResult {
                text: "The file contains a greeting.".into(),
                output: Some(json!({"answer":"The file contains a greeting."})),
                attachments: Vec::new(),
                chat_id: Some(chat.chat_id.clone()),
                message_id: Some(second_assistant_id.clone()),
            }
        )
        .is_err());
        assert!(replayed.display_history.iter().any(|message| matches!(
            message,
            ConversationMessage::User { content, .. }
                if content.iter().any(|block| matches!(block,
                    lingxi_core::types::ContentBlock::ToolResult { tool_use_id: id, .. }
                        if id.to_string() == tool_use_id
                ))
        )));
        recover_chat_message_metadata(
            &recovered.chats,
            &chat.chat_id,
            &replayed,
            &recovered.store,
            &owner,
            None,
            None,
        )
        .unwrap();
        let tool_row = MessageId::from_uuid(tool_result_uuid).to_string();
        assert!(recovered
            .chats
            .message_metadata(&chat.chat_id)
            .unwrap()
            .iter()
            .any(|metadata| metadata.message_id == tool_row && metadata.run_id == first_task_id));
        assert_eq!(
            recovered.chats.get(&chat.chat_id).unwrap().state,
            ChatState::Idle
        );
        let (_, third_turn_checkpoint) = recovered
            .store
            .latest_chat_transcript_checkpoint(&owner.app_id, &chat.chat_id, "tool-turn-three")
            .unwrap()
            .unwrap();
        assert_eq!(third_turn_checkpoint.byte_len, 2);
        assert_eq!(first_user["id"], "tool-user-one");
        assert_eq!(first_assistant["id"], first_assistant_id);
        assert_eq!(second_user["id"], "tool-user-two");
        assert_eq!(second_assistant["id"], second_assistant_id);

        let mut third_messages = recovered
            .chats
            .protocol_history_page(&chat.chat_id, None, 10)
            .unwrap()
            .messages;
        third_messages.push(json!({
            "id":"tool-user-three","role":"user","content":"one more question"
        }));
        let third_raw = json!({
            "threadId":chat.chat_id,
            "runId":"tool-turn-three",
            "protocolVersion":"1.0",
            "messages":third_messages
        });
        // This fixture exercises history normalization and ledger admission
        // only. Runtime transcript continuation is covered by strict writer
        // checkpoints and remains fail-closed for unsupported tool assets.
        let (third, third_chat) =
            normalize_page_run_input(&recovered, third_raw, Some(&chat.chat_id)).unwrap();
        let expected_history = third.input.protocol_history_digest.clone().unwrap();
        let chats = recovered.chats.clone();
        let admission = recovered
            .store
            .admit_scoped_with_check(
                &owner,
                &third.input,
                &[],
                &[],
                third_chat.as_deref(),
                true,
                move || {
                    let history = chats.protocol_history_digests(&chat.chat_id)?;
                    if local_app::v2::agent::protocol::history_fingerprint(&history)
                        != expected_history
                    {
                        return Err(local_app::v2::AppError::Invalid(
                            "AG-UI Chat history changed during Agent admission".into(),
                        ));
                    }
                    Ok(())
                },
            )
            .unwrap();
        assert!(
            admission.start,
            "the third turn should be admitted after restart"
        );
    }

    #[test]
    fn null_structured_result_becomes_run_error_instead_of_success_event() {
        let directory = tempfile::tempdir().unwrap();
        let app_id = format!("a{}", "3".repeat(32));
        let owner = owner(&app_id);
        let store = AgentStore::open(directory.path()).unwrap();
        store.configure(owner.clone(), Some(settings())).unwrap();
        let value = json!({
            "threadId":"thread-null-result",
            "runId":"public-null-run",
            "messages":[{"id":"user-null-run","role":"user","content":"return null"}],
            "forwardedProps":{"com.lingxi.localApp":{"version":1,"outputSchema":{"type":"null"}}}
        });
        let normalized = local_app::v2::agent::protocol::normalize_run_input(&value, &[]).unwrap();
        let admitted = store.admit(&owner, &normalized.input, &[]).unwrap();
        store
            .append_event(
                &owner,
                &admitted.task.run_id,
                json!({
                    "type":"RUN_STARTED", "protocolVersion":"1.0",
                    "threadId":"thread-null-result", "runId":"public-null-run"
                }),
            )
            .unwrap();
        let task = commit_agent_terminal_for_test(
            &store,
            &owner,
            &admitted.task.run_id,
            State::Succeeded,
            Some(TaskResult {
                text: String::new(),
                output: Some(Value::Null),
                attachments: Vec::new(),
                chat_id: None,
                message_id: None,
            }),
            None,
        )
        .unwrap();

        assert_eq!(task.state, State::Failed);
        let event = protocol_terminal_event(&task, "thread-null-result").unwrap();
        assert_eq!(event["type"], "RUN_ERROR");
        assert_eq!(event["code"], "structured_output_not_representable");
        local_app::v2::agent::protocol::validate_event(&event).unwrap();
        let persisted = store.get_by_request(&owner, "public-null-run").unwrap();
        assert_eq!(persisted.state, State::Failed);
        assert_eq!(
            persisted.error.as_ref().map(|error| error.code.as_str()),
            Some("structured_output_not_representable")
        );
        let page = store.protocol_events(&owner, "public-null-run", 0).unwrap();
        assert_eq!(page.events.len(), 2);
        assert_eq!(page.events[0]["type"], "RUN_STARTED");
        assert_eq!(page.events[1], event);
    }
}
