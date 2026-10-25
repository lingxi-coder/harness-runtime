//! Process-owned task service. The page and native control APIs are separate:
//! only the latter can configure grants or resolve permissions/questions.
use super::*;
use crate::mobile::unified_local_apps::MobileLocalAppServiceHandle;
use lingxi_core::{
    host::{permission_gate::PermissionDecision, CostSnapshot},
    types::ToolUseId,
};
use local_app::v2::agent::{
    AgentStore, AttachmentMetadata, ChatMessageMetadata, ChatRole, ChatState, ChatStore, Input,
    Owner, Settings, State, TaskResult,
};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
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
    expires_at_ms: u64,
    run_id: Option<String>,
    claimed: bool,
}
#[derive(Default)]
struct WakeRegistry {
    reservations: HashMap<WakeKey, WakeReservation>,
}
impl WakeRegistry {
    fn reserve(&mut self, key: WakeKey, now: u64, timeout_ms: u64) -> Result<u64, String> {
        self.reservations.retain(|_, reservation| {
            reservation.run_id.is_some() || reservation.expires_at_ms > now
        });
        if let Some(existing) = self.reservations.get(&key) {
            return Ok(existing.expires_at_ms);
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
        self.reservations.insert(
            key,
            WakeReservation {
                expires_at_ms,
                run_id: None,
                claimed: false,
            },
        );
        Ok(expires_at_ms)
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

    fn clear(&mut self, key: &WakeKey) -> Option<String> {
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
    protocol_parent_run_id: Option<String>,
    cancel: CancellationToken,
    finished: AtomicBool,
    done: Notify,
    lease: Mutex<Option<SessionToolGrantLease>>,
    interaction: AsyncMutex<Option<(String, Value, Reply)>>,
    store: Arc<AgentStore>,
    chats: Arc<ChatStore>,
    chat_id: Option<String>,
    user_message_id: Option<String>,
    chat_persisted: watch::Sender<Option<Result<(), String>>>,
    chat_commit: AsyncMutex<Option<oneshot::Receiver<()>>>,
    attachment_metadata: Vec<AttachmentMetadata>,
    assistant_message_id: AsyncMutex<Option<String>>,
    protocol_message: AsyncMutex<ProtocolMessageState>,
    protocol_events_failed: AtomicBool,
    completed_model_turns: AtomicU32,
    text: AsyncMutex<String>,
    model_failed: AtomicBool,
    model_failure: Mutex<Option<String>>,
    timed_out: AtomicBool,
    usage: AsyncMutex<Option<Value>>,
}
#[derive(Default)]
struct ProtocolMessageState {
    message_id: Option<String>,
    open: bool,
    pending_text: String,
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
        let root = initialize_agui_agent_store(&legacy_root)?;
        let store = Arc::new(
            AgentStore::open(&root.join("tasks")).map_err(|_| "agent_storage_unavailable")?,
        );
        let chats = Arc::new(ChatStore::open(&root).map_err(|_| "agent_storage_unavailable")?);
        let task_states = store
            .recovery_states(app_id)
            .map_err(|_| "agent_storage_unavailable")?;
        chats
            .reconcile_startup(&task_states)
            .map_err(|_| "agent_storage_unavailable")?;
        Ok(Self { store, chats, root })
    }
}

/// Create a fresh AG-UI 1.0 store once per App. The caller (`attach_agent_factory`)
/// stops the previous executor before constructing a replacement, so known
/// prototype task/chat journals can be removed without touching release files,
/// app data, upload spools, or directories supplied by page input.
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
                    purge_prototype_agent_store(agent_root)?;
                }
                Err(_) => return Err("agent_storage_unavailable".into()),
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            purge_prototype_agent_store(agent_root)?;
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

fn purge_prototype_agent_store(agent_root: &Path) -> Result<(), String> {
    let task_root = agent_root.join("tasks");
    match std::fs::symlink_metadata(&task_root) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err("agent_storage_unavailable".into());
        }
        Ok(_) => {
            for entry in std::fs::read_dir(&task_root).map_err(|_| "agent_storage_unavailable")? {
                let entry = entry.map_err(|_| "agent_storage_unavailable")?;
                let name = entry.file_name();
                let name = name.to_string_lossy();
                let metadata = std::fs::symlink_metadata(entry.path())
                    .map_err(|_| "agent_storage_unavailable")?;
                if metadata.file_type().is_symlink() {
                    return Err("agent_storage_unavailable".into());
                }
                if name == "agent-tasks.json" && metadata.is_file() {
                    std::fs::remove_file(entry.path()).map_err(|_| "agent_storage_unavailable")?;
                } else if metadata.is_dir()
                    && uuid::Uuid::parse_str(name.as_ref())
                        .is_ok_and(|uuid| uuid.to_string() == name)
                {
                    std::fs::remove_dir_all(entry.path())
                        .map_err(|_| "agent_storage_unavailable")?;
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err("agent_storage_unavailable".into()),
    }
    let chats_root = agent_root.join("chats");
    match std::fs::symlink_metadata(&chats_root) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err("agent_storage_unavailable".into());
        }
        Ok(_) => {
            for entry in std::fs::read_dir(&chats_root).map_err(|_| "agent_storage_unavailable")? {
                let entry = entry.map_err(|_| "agent_storage_unavailable")?;
                let name = entry.file_name();
                let name = name.to_string_lossy();
                let metadata = std::fs::symlink_metadata(entry.path())
                    .map_err(|_| "agent_storage_unavailable")?;
                if metadata.file_type().is_symlink() {
                    return Err("agent_storage_unavailable".into());
                }
                if metadata.is_dir()
                    && uuid::Uuid::parse_str(name.as_ref())
                        .is_ok_and(|uuid| uuid.to_string() == name)
                {
                    std::fs::remove_dir_all(entry.path())
                        .map_err(|_| "agent_storage_unavailable")?;
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err("agent_storage_unavailable".into()),
    }
    remove_known_agent_file(&agent_root.join("chats-index.json"))?;
    remove_known_agent_directory(&agent_root.join("protocol-events"))?;
    Ok(())
}

fn remove_known_agent_file(path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err("agent_storage_unavailable".into())
        }
        Ok(_) => std::fs::remove_file(path).map_err(|_| "agent_storage_unavailable".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err("agent_storage_unavailable".into()),
    }
}

fn remove_known_agent_directory(path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            Err("agent_storage_unavailable".into())
        }
        Ok(_) => std::fs::remove_dir_all(path).map_err(|_| "agent_storage_unavailable".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err("agent_storage_unavailable".into()),
    }
}

impl Control {
    async fn append_protocol_event(&self, event: Value) -> Result<(), String> {
        let Some(_) = self.protocol_thread_id else {
            return Ok(());
        };
        match self.store.append_event(&self.owner, &self.id, event) {
            Ok(_) => Ok(()),
            Err(_) => {
                self.protocol_events_failed.store(true, Ordering::Release);
                if let Ok(mut failure) = self.model_failure.lock() {
                    *failure = Some("agent_storage_unavailable".into());
                }
                self.model_failed.store(true, Ordering::Release);
                self.revoke();
                Err("agent_storage_unavailable".into())
            }
        }
    }

    async fn close_protocol_message(&self, fallback_text: Option<&str>) -> Result<(), String> {
        let Some(thread_id) = &self.protocol_thread_id else {
            return Ok(());
        };
        let mut state = self.protocol_message.lock().await;
        if state.open {
            if !state.pending_text.is_empty() {
                let delta = std::mem::take(&mut state.pending_text);
                self.append_protocol_event(json!({
                    "type":"TEXT_MESSAGE_CONTENT",
                    "messageId":state.message_id.as_deref().unwrap_or_default(),
                    "delta":delta
                }))
                .await?;
            }
            let id = state.message_id.clone().unwrap_or_default();
            state.open = false;
            return self
                .append_protocol_event(json!({"type":"TEXT_MESSAGE_END","messageId":id}))
                .await;
        }
        let text = if state.pending_text.is_empty() {
            fallback_text.unwrap_or_default().to_owned()
        } else {
            std::mem::take(&mut state.pending_text)
        };
        if text.is_empty() {
            return Ok(());
        }
        let message_id = format!("msg_{}", uuid::Uuid::new_v4());
        self.append_protocol_event(json!({
            "type":"TEXT_MESSAGE_START",
            "messageId":message_id,
            "role":"assistant"
        }))
        .await?;
        self.append_protocol_event(json!({
            "type":"TEXT_MESSAGE_CONTENT",
            "messageId":message_id,
            "delta":text
        }))
        .await?;
        self.append_protocol_event(json!({
            "type":"TEXT_MESSAGE_END",
            "messageId":message_id
        }))
        .await?;
        let _ = thread_id;
        Ok(())
    }

    fn revoke(&self) {
        if let Ok(lease) = self.lease.lock() {
            if let Some(lease) = lease.as_ref() {
                lease.revoke();
            }
        }
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
    fn note_model_error_kind(&self, kind: &'static str) {
        if let Ok(mut failure) = self.model_failure.lock() {
            *failure = Some(format!("model_{kind}"));
        }
    }
    async fn emit_text(&self, text: &Utf16JsonProjection) {
        if let Some(text) = text.string_at("") {
            let delta = text.to_owned();
            let mut output = self.text.lock().await;
            if output.len() + delta.len() > 1024 * 1024 {
                self.model_failed.store(true, Ordering::Release);
                self.revoke();
            } else {
                output.push_str(&delta);
                drop(output);
                if self.protocol_thread_id.is_some() && !delta.is_empty() {
                    let mut state = self.protocol_message.lock().await;
                    if state.open {
                        let message_id = state.message_id.clone().unwrap_or_default();
                        let _ = self
                            .append_protocol_event(json!({
                                "type":"TEXT_MESSAGE_CONTENT",
                                "messageId":message_id,
                                "delta":delta
                            }))
                            .await;
                    } else {
                        state.pending_text.push_str(&delta);
                    }
                }
            }
        }
    }
    async fn emit_assistant_message_identity(&self, message_id: &lingxi_core::types::MessageId) {
        let id = message_id.to_string();
        *self.assistant_message_id.lock().await = Some(id.clone());
        if let Some(chat_id) = &self.chat_id {
            if self
                .chats
                .record_message(
                    chat_id,
                    ChatMessageMetadata {
                        message_id: id.clone(),
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
        if self.protocol_thread_id.is_some() {
            let mut state = self.protocol_message.lock().await;
            if state.open && state.message_id.as_deref() != Some(&id) {
                let previous = state.message_id.clone().unwrap_or_default();
                state.open = false;
                let _ = self
                    .append_protocol_event(json!({"type":"TEXT_MESSAGE_END","messageId":previous}))
                    .await;
            }
            if !state.open {
                state.message_id = Some(id.clone());
                state.open = true;
                let _ = self
                    .append_protocol_event(json!({
                        "type":"TEXT_MESSAGE_START",
                        "messageId":id,
                        "role":"assistant"
                    }))
                    .await;
                if !state.pending_text.is_empty() {
                    let pending = std::mem::take(&mut state.pending_text);
                    let _ = self
                        .append_protocol_event(json!({
                            "type":"TEXT_MESSAGE_CONTENT",
                            "messageId":id,
                            "delta":pending
                        }))
                        .await;
                }
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
        let completed = self.completed_model_turns.fetch_add(1, Ordering::AcqRel) + 1;
        if self
            .store
            .set_completed_model_turns(&self.owner, &self.id, completed)
            .is_err()
        {
            self.model_failed.store(true, Ordering::Release);
            self.revoke();
        }
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
            apps.insert(
                record.app_id.clone(),
                Arc::new(AppAgentState::open(&apps_root, &record.app_id)?),
            );
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
                if let Some(run_id) = protocol_duplicate_run(&state, &owner, &raw_input)? {
                    return Ok(json!({"runId":run_id}));
                }
                let (normalized, chat_id) = normalize_page_run_input(&state, raw_input, None)?;
                if normalized.message_already_persisted {
                    return Err("agui_message_already_submitted".into());
                }
                let input = normalized.input;
                let user_message = normalized.user_message;
                let owner_cancel = self
                    .service()
                    .await?
                    .agent_invocation_token(app, invocation)
                    .await?;
                let allow_new = !cfg!(target_os = "ios")
                    || self.has_wake_reservation(app, revision, &input.request_id);
                self.start_task(
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
                .await?;
                Ok(json!({"runId":normalized.run_id}))
            }
            "agent.tasks.get" => {
                let id = only_run_id(&args)?;
                let state = self.app_state(app)?;
                let task = state
                    .store
                    .get_by_request(&owner, id)
                    .map_err(|_| "task_not_found")?;
                let thread_id = state
                    .store
                    .thread_id_for_request(&owner, id)
                    .map_err(|_| "task_not_found")?;
                public_task_value(&task, thread_id)
            }
            "agent.tasks.list" => {
                reject_unknown(&args, &["cursor", "limit"])?;
                let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(50) as usize;
                let cursor = args.get("cursor").and_then(Value::as_str);
                let state = self.app_state(app)?;
                let private_cursor = cursor
                    .map(|cursor| state.store.task_id_for_request(&owner, cursor))
                    .transpose()
                    .map_err(|_| "invalid_task_list")?;
                let tasks = state
                    .store
                    .list(&owner, private_cursor.as_deref(), limit)
                    .map_err(|_| "invalid_task_list")?;
                let mut summaries = Vec::with_capacity(tasks.len());
                for task in &tasks {
                    let thread_id = state
                        .store
                        .thread_id_for_request(&owner, &task.request_id)
                        .map_err(|_| "invalid_task_list")?;
                    summaries.push(public_task_value(task, thread_id)?);
                }
                Ok(json!({"tasks":summaries}))
            }
            "agent.tasks.cancel" => {
                let id = only_run_id(&args)?;
                let state = self.app_state(app)?;
                let task = state
                    .store
                    .get_by_request(&owner, id)
                    .map_err(|_| "task_not_found")?;
                state
                    .store
                    .request_cancel(&owner, &task.run_id)
                    .map_err(|_| "task_not_found")?;
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
            "agent.chats.create" => {
                reject_unknown(&args, &["title"])?;
                let title = args.get("title").and_then(Value::as_str);
                let state = self.app_state(app)?;
                serde_json::to_value(
                    state
                        .chats
                        .create(title, &uuid::Uuid::new_v4().to_string())
                        .map_err(|_| "chat_storage_unavailable")?,
                )
                .map_err(|_| "invalid_chat".into())
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
                serde_json::to_value(
                    state
                        .chats
                        .list(cursor, limit, include_archived)
                        .map_err(|_| "invalid_chat_list")?,
                )
                .map_err(|_| "invalid_chat_list".into())
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
                if let Some(run_id) = protocol_duplicate_run(&state, &owner, &raw_input)? {
                    return Ok(json!({"runId":run_id}));
                }
                let (normalized, chat_id) =
                    normalize_page_run_input(&state, raw_input, Some(&chat_id))?;
                if normalized.message_already_persisted {
                    return Err("agui_message_already_submitted".into());
                }
                let input = normalized.input;
                let user_message = normalized.user_message;
                let owner_cancel = self
                    .service()
                    .await?
                    .agent_invocation_token(app, invocation)
                    .await?;
                let allow_new = !cfg!(target_os = "ios")
                    || self.has_wake_reservation(app, revision, &input.request_id);
                self.start_task(
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
                .await?;
                Ok(json!({"runId":normalized.run_id}))
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
                        .map_err(|_| "task_not_found")?;
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
        let state = self.app_state(&owner.app_id)?;
        if let Some(chat_id) = chat_id.as_deref() {
            let summary = state.chats.get(chat_id).map_err(|_| "chat_not_found")?;
            if summary.archived {
                return Err("chat_archived".into());
            }
        }
        let claimed_wake = cfg!(target_os = "ios")
            && !native_detached
            && self.claim_wake(&owner.app_id, revision, &input.request_id);
        let allow_new = if cfg!(target_os = "ios") && !native_detached {
            claimed_wake
        } else {
            allow_new
        };
        let admitted = match state.store.admit_scoped(
            &owner,
            &input,
            declared_tools,
            declared_skills,
            chat_id.as_deref(),
            allow_new,
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
            if let Some(chat_id) = chat_id.as_deref() {
                let control = self.active.lock().await.get(&id).cloned();
                if let Some(control) = control {
                    if control.chat_id.as_deref() == Some(chat_id) {
                        let mut persisted = control.chat_persisted.subscribe();
                        wait_for_chat_persistence(&mut persisted, &owner_cancel, false).await?;
                    }
                }
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
        if cfg!(target_os = "ios") && !native_detached && !wake_detached {
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
            protocol_parent_run_id: input.protocol_parent_run_id.clone(),
            cancel,
            finished: AtomicBool::new(false),
            done: Notify::new(),
            lease: Mutex::new(None),
            interaction: AsyncMutex::new(None),
            store: state.store.clone(),
            chats: state.chats.clone(),
            chat_id,
            user_message_id,
            chat_persisted: chat_persisted_tx,
            chat_commit: AsyncMutex::new(if is_chat { Some(chat_commit_rx) } else { None }),
            attachment_metadata,
            assistant_message_id: AsyncMutex::new(None),
            protocol_message: AsyncMutex::new(ProtocolMessageState::default()),
            protocol_events_failed: AtomicBool::new(false),
            completed_model_turns: AtomicU32::new(0),
            text: AsyncMutex::new(String::new()),
            model_failed: AtomicBool::new(false),
            model_failure: Mutex::new(None),
            timed_out: AtomicBool::new(false),
            usage: AsyncMutex::new(None),
        });
        if let Some(thread_id) = &control.protocol_thread_id {
            let mut started = json!({
                "type":"RUN_STARTED",
                "protocolVersion":"1.0",
                "threadId":thread_id,
                "runId":control.request_id
            });
            if let Some(parent_run_id) = &control.protocol_parent_run_id {
                started["parentRunId"] = json!(parent_run_id);
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
                let _ = state.store.rollback_queued_admission(&owner, &id);
                return Err("agent_storage_unavailable".into());
            }
        }
        self.active.lock().await.insert(id.clone(), control.clone());
        drop(_serial);
        let host = self.clone();
        let app_root = state.root.clone();
        let execute_control = control.clone();
        tokio::spawn(async move {
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
        let fallback_text = result.as_ref().ok().map(|(result, _)| result.text.as_str());
        if control.close_protocol_message(fallback_text).await.is_err() {
            result = Err("agent_storage_unavailable".into());
        }
        control.report_chat_persisted(result.as_ref().map(|_| ()).map_err(|error| error.clone()));
        control.revoke();
        control.interaction.lock().await.take();
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
            if let Ok(task) = control
                .store
                .update(&owner, &id, state, output, usage, error)
            {
                let mut event_storage_failed = false;
                if let Some(thread_id) = &control.protocol_thread_id {
                    if let Some(event) = protocol_terminal_event(&task, thread_id) {
                        event_storage_failed =
                            control.store.append_event(&owner, &id, event).is_err();
                    }
                }
                if event_storage_failed {
                    let _ = control.store.interrupt_after_protocol_failure(
                        &owner,
                        &id,
                        "protocol_event_storage_failed",
                    );
                }
                self.active.lock().await.remove(&id);
                if let Some(chat_id) = &control.chat_id {
                    if event_storage_failed {
                        let _ = control.chats.interrupt_turn(chat_id, &id);
                    } else {
                        let _ = control.chats.finish_turn(chat_id, &id);
                    }
                }
                self.clear_wake_run(&id);
            }
        } else {
            let _ = control.store.update(
                &owner,
                &id,
                State::Running,
                None,
                usage,
                Some("tool_cleanup_incomplete".into()),
            );
        }
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
            let (result, usage) = self.factory.query_stateless(admission, &input).await?;
            control
                .store
                .set_completed_model_turns(&control.owner, &control.id, 1)
                .map_err(|_| "task_storage_failed")?;
            *control.usage.lock().await = Some(usage.clone());
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
            let replayed = if transcript_path.is_file() {
                let replayed = orchestrator::resume::replay_session_state_from_path(
                    &transcript_path,
                    session_uuid,
                    self.factory.context.fs.clone(),
                )
                .await
                .map_err(|_| "chat_transcript_unavailable")?;
                recover_chat_message_metadata(&control.chats, chat_id, &replayed)?;
                Some(replayed)
            } else {
                None
            };
            Some(super::AppAgentChatResume {
                session_id: SessionId::from_uuid(session_uuid),
                transcript_path,
                replayed,
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
                self.factory.context.fs.clone(),
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
                    OrchestratorError::ApiCall(_) => "model_api_failed",
                    OrchestratorError::Streaming(_) => "model_stream_failed",
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
        let text = control.text.lock().await.clone();
        let assistant_message_id = control.assistant_message_id.lock().await.clone();
        if control.chat_id.is_some() && assistant_message_id.is_none() {
            return Err("chat_message_identity_missing".into());
        }
        if let (Some(chat_id), Some(message_id)) =
            (control.chat_id.as_deref(), assistant_message_id.as_deref())
        {
            let message = json!({"id":message_id,"role":"assistant","content":text});
            control
                .chats
                .record_protocol_message(chat_id, &control.id, &message)
                .map_err(|_| "chat_storage_unavailable")?;
        }
        let result = TaskResult {
            text,
            output,
            attachments: Vec::new(),
            chat_id: control.chat_id.clone(),
            message_id: assistant_message_id,
        };
        if serde_json::to_value(&result)
            .map_err(|_| "invalid_output")?
            .to_string()
            .len()
            > 2 * 1024 * 1024
        {
            return Err("result_too_large".into());
        }
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
                let _serial = self.admission.lock().await;
                let key = WakeKey {
                    app_id: app.to_owned(),
                    revision,
                    request_id: run_id.to_owned(),
                };
                let expires_at_ms = self
                    .wake_reservations
                    .lock()
                    .map_err(|_| "agent_storage_unavailable")?
                    .reserve(key, now_millis(), receive_timeout)?;
                Ok(
                    json!({"appId":app,"revision":revision,"runId":run_id,"expiresAtMs":expires_at_ms}),
                )
            }
            "wake.clear" => {
                reject_unknown(&request, &["op", "appId", "revision", "runId"])?;
                let app = required_string(&request, "appId")?;
                let revision = request["revision"].as_u64().ok_or("invalid_revision")?;
                let run_id_request = required_string(&request, "runId")?;
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
                    .clear(&key);
                if let Some(run_id) = run_id {
                    if let Some(control) = self.active.lock().await.get(&run_id).cloned() {
                        if control.owner.app_id == app
                            && control.revision == revision
                            && control.request_id == run_id_request
                        {
                            self.cancel_ids(&[run_id]).await;
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
                if let Some(run_id) = protocol_duplicate_run(&state, &owner, &raw_input)? {
                    return Ok(json!({"runId":run_id}));
                }
                let (normalized, chat_id) = normalize_page_run_input(&state, raw_input, None)?;
                if normalized.message_already_persisted {
                    return Err("agui_message_already_submitted".into());
                }
                validate_request_id(&normalized.run_id)?;
                self.start_task(
                    owner,
                    revision,
                    &tools,
                    &skills,
                    normalized.input,
                    Some(normalized.user_message),
                    chat_id,
                    CancellationToken::new(),
                    true,
                    true,
                )
                .await?;
                Ok(json!({"runId":normalized.run_id}))
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
                serde_json::to_value(page).map_err(|_| "agent_events_unavailable".into())
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
                        .map_err(|_| "task_not_found")?;
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
                let ids = state
                    .store
                    .configure(owner, settings)
                    .map_err(|_| "agent_storage_unavailable")?;
                self.cancel_ids(&ids).await;
                Ok(Value::Null)
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

/// The one pre-admission Chat persistence step. The task executor appends the
/// stable user row to the App-owned Harness session before it acknowledges
/// `{runId}`; only then may the caller release the executor to make a model
/// request. Replaying the transcript verifies that the best-effort Harness
/// writer actually committed the row.
async fn persist_chat_user_row(
    orchestrator: &orchestrator::ConversationOrchestrator,
    chats: &ChatStore,
    chat_id: &str,
    message_id: &str,
    blocks: Vec<lingxi_core::types::ContentBlock>,
    fs: Arc<dyn lingxi_core::host::FileSystem>,
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
    if !transcript_path.is_file() {
        return Err("chat_transcript_unavailable".into());
    }
    let session_id = chats.session_id(chat_id).map_err(|_| "chat_not_found")?;
    let session_uuid = uuid::Uuid::parse_str(&session_id).map_err(|_| "invalid_chat")?;
    let replayed =
        orchestrator::resume::replay_session_state_from_path(&transcript_path, session_uuid, fs)
            .await
            .map_err(|_| "chat_transcript_unavailable")?;
    let persisted_user_rows = replayed
        .display_history
        .iter()
        .filter(|row| row.id() == message_id && row.role() == lingxi_core::types::MessageRole::User)
        .count();
    if persisted_user_rows != 1 {
        return Err("chat_transcript_unavailable".into());
    }
    recover_chat_message_metadata(chats, chat_id, &replayed)
}

/// Reconcile transcript rows with the host-owned Chat index. Harness writes
/// the durable session before its output callback can finish committing the
/// index; this idempotent repair makes refresh/history safe after that crash
/// window without replaying any model or tool side effects.
fn recover_chat_message_metadata(
    chats: &ChatStore,
    chat_id: &str,
    replayed: &orchestrator::resume::ReplayedSession,
) -> Result<(), String> {
    let mut known = chats
        .message_metadata(chat_id)
        .map_err(|_| "chat_storage_unavailable")?
        .into_iter()
        .map(|message| (message.message_id.clone(), message))
        .collect::<HashMap<_, _>>();
    let mut current_run: Option<String> = None;
    for row in &replayed.display_history {
        if row.is_meta()
            || row.is_compact_summary()
            || row.role() == lingxi_core::types::MessageRole::System
        {
            continue;
        }
        let message_id = row.id().to_string();
        if let Some(metadata) = known.get(&message_id) {
            current_run = Some(metadata.run_id.clone());
            continue;
        }
        let run_id = match row.role() {
            lingxi_core::types::MessageRole::User => {
                let Some(message_id) = lingxi_core::types::MessageId::parse_prefixed(&message_id)
                else {
                    current_run = None;
                    continue;
                };
                message_id.as_uuid().to_string()
            }
            lingxi_core::types::MessageRole::Assistant => {
                let Some(run_id) = current_run.clone() else {
                    continue;
                };
                run_id
            }
            lingxi_core::types::MessageRole::System => continue,
        };
        let role = match row.role() {
            lingxi_core::types::MessageRole::User => ChatRole::User,
            lingxi_core::types::MessageRole::Assistant => ChatRole::Assistant,
            lingxi_core::types::MessageRole::System => continue,
        };
        let created_at = replayed
            .display_timestamps
            .get(&message_id)
            .copied()
            .unwrap_or_else(|| now_millis() as i64)
            .max(0) as u64;
        let metadata = ChatMessageMetadata {
            message_id: message_id.clone(),
            run_id: run_id.clone(),
            role,
            created_at,
            attachments: Vec::new(),
        };
        chats
            .record_message(chat_id, metadata.clone())
            .map_err(|_| "chat_storage_unavailable")?;
        current_run = Some(run_id);
        known.insert(message_id, metadata);
    }
    Ok(())
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
        state
            .chats
            .get(required_chat)
            .map_err(|_| "chat_not_found")?;
        Some(required_chat.to_owned())
    } else if uuid::Uuid::parse_str(&thread_id).is_ok_and(|uuid| uuid.to_string() == thread_id)
        && state.chats.get(&thread_id).is_ok()
    {
        Some(thread_id.clone())
    } else {
        None
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
    let normalized = local_app::v2::agent::protocol::normalize_run_input(&input, &history)
        .map_err(|error| error.code)?;
    Ok((normalized, chat_id))
}

fn protocol_duplicate_run(
    state: &AppAgentState,
    owner: &Owner,
    input: &Value,
) -> Result<Option<String>, String> {
    use local_app::v2::{agent::protocol, AppError};

    let run_id = protocol::run_id(input).map_err(|error| error.code)?;
    let fingerprint = protocol::run_input_fingerprint(input).map_err(|error| error.code)?;
    match state.store.get_by_request(owner, &run_id) {
        Ok(_) => {
            let original = state
                .store
                .protocol_input_fingerprint(owner, &run_id)
                .map_err(|_| "request_conflict")?;
            if original == fingerprint {
                Ok(Some(run_id))
            } else {
                Err("request_conflict".into())
            }
        }
        Err(AppError::NotFound(_)) => Ok(None),
        Err(_) => Err("agent_storage_unavailable".into()),
    }
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

fn required_string<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("invalid_{key}"))
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
        registry.reserve(app_a.clone(), 100, 50).unwrap();
        registry.reserve(app_b.clone(), 100, 50).unwrap();

        assert!(registry.claim(&app_a, 101));
        assert!(!registry.claim(&app_a, 101));
        assert!(registry.consume(&app_a, 101, "run-a"));
        assert!(!registry.available(&app_a, 102));
        assert!(!registry.claim(&app_a, 102));
        assert_eq!(registry.reserve(app_a.clone(), 102, 50).unwrap(), 150);
        assert!(!registry.claim(&app_a, 102));
        assert!(!registry.available(&app_a, 151));
        assert!(registry.reservations.contains_key(&app_a));
        assert_eq!(registry.clear(&app_a).as_deref(), Some("run-a"));
        assert!(registry.available(&app_b, 103));
        assert_eq!(registry.reservations.len(), 1);
    }

    #[test]
    fn terminal_cleanup_removes_a_consumed_wake_link() {
        let key = key("app-a", 7, "wake-a");
        let mut registry = WakeRegistry::default();
        registry.reserve(key.clone(), 100, 50).unwrap();
        assert!(registry.claim(&key, 101));
        assert!(registry.consume(&key, 101, "run-a"));
        registry.clear_run("run-a");
        assert!(registry.reservations.is_empty());
        assert!(registry.clear(&key).is_none());
    }

    #[test]
    fn only_one_live_wake_reservation_per_app_is_allowed() {
        let first = key("app-a", 7, "wake-a");
        let second = key("app-a", 7, "wake-b");
        let other_app = key("app-b", 3, "wake-other");
        let mut registry = WakeRegistry::default();
        registry.reserve(first.clone(), 100, 50).unwrap();
        assert_eq!(
            registry.reserve(second.clone(), 101, 50),
            Err("busy".into())
        );
        registry.reserve(other_app, 101, 50).unwrap();
        registry.clear(&first);
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
    fn expired_wake_cannot_be_claimed_or_consumed() {
        let key = key("app-a", 7, "wake-a");
        let mut registry = WakeRegistry::default();
        registry.reserve(key.clone(), 100, 1).unwrap();
        assert!(!registry.claim(&key, 101));
        assert!(!registry.consume(&key, 101, "run-a"));
    }

    #[test]
    fn first_agui_store_purges_only_known_prototype_agent_paths() {
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

        let store = initialize_agui_agent_store(&agent).unwrap();
        assert!(store.join("protocol-store.json").is_file());
        assert!(!tasks.join("agent-tasks.json").exists());
        assert!(!tasks.join(old_task).exists());
        assert!(!agent.join("chats-index.json").exists());
        assert!(!chats.join(old_chat).exists());
        assert_eq!(
            std::fs::read(spool.join("preserved")).unwrap(),
            b"unrelated upload spool"
        );
        assert_eq!(
            std::fs::read(agent.join("app-data.json")).unwrap(),
            b"preserved app data"
        );

        std::fs::create_dir_all(store.join("tasks")).unwrap();
        let current = store.join("tasks/current.json");
        std::fs::write(&current, b"new AG-UI data").unwrap();
        assert_eq!(initialize_agui_agent_store(&agent).unwrap(), store);
        assert_eq!(std::fs::read(current).unwrap(), b"new AG-UI data");
    }

    #[cfg(unix)]
    #[test]
    fn prototype_cleanup_rejects_symlinked_task_roots_without_following_them() {
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
    use super::*;
    use local_app::v2::agent::{Model, Settings, TaskResult};

    fn owner(app_id: &str) -> Owner {
        Owner {
            app_id: app_id.to_owned(),
            release_digest: "2".repeat(64),
            invocation_id: "protocol-retry-test".into(),
        }
    }

    fn settings() -> Settings {
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
            .create(None, &uuid::Uuid::new_v4().to_string())
            .unwrap();
        let input = json!({
            "threadId":chat.chat_id,
            "runId":"public-run-1",
            "protocolVersion":"1.0",
            "messages":[{"id":"user-message-1","role":"user","content":"hello"}]
        });

        assert_eq!(
            protocol_duplicate_run(&state, &owner, &input).unwrap(),
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
            .chats
            .start_turn(&chat.chat_id, &admitted.task.run_id)
            .unwrap();
        state
            .chats
            .record_protocol_message(&chat.chat_id, "public-run-1", &normalized.user_message)
            .unwrap();

        assert_eq!(
            protocol_duplicate_run(&state, &owner, &input)
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
            .record_protocol_message(&chat.chat_id, "public-run-1", &assistant)
            .unwrap();
        state
            .store
            .update(
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
                None,
            )
            .unwrap();
        state
            .chats
            .finish_turn(&chat.chat_id, &admitted.task.run_id)
            .unwrap();

        assert_eq!(
            protocol_duplicate_run(&state, &owner, &input)
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
            protocol_duplicate_run(&state, &owner, &changed_prefix).unwrap_err(),
            "request_conflict"
        );
        assert_eq!(
            state
                .chats
                .protocol_history_page(&chat.chat_id, None, 10)
                .unwrap()
                .messages
                .len(),
            2,
            "deduplicated retries must not append duplicate messages"
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
        let task = store
            .update(
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
                None,
            )
            .unwrap();

        let event = protocol_terminal_event(&task, "thread-null-result").unwrap();
        assert_eq!(event["type"], "RUN_ERROR");
        assert_eq!(event["code"], "structured_output_not_representable");
        local_app::v2::agent::protocol::validate_event(&event).unwrap();
    }
}
