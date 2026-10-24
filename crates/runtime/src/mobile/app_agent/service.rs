//! Process-owned task service. The page and native control APIs are separate:
//! only the latter can configure grants or resolve permissions/questions.
use super::*;
use crate::mobile::unified_local_apps::MobileLocalAppServiceHandle;
use lingxi_core::{
    host::{CostSnapshot, permission_gate::PermissionDecision},
    types::ToolUseId,
};
use local_app::v2::agent::{
    AgentStore, AttachmentMetadata, ChatHistory, ChatMessage, ChatMessageMetadata, ChatRole,
    ChatState, ChatStore, Input, Owner, Settings, State, TaskResult,
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
    sync::{
        Weak,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
};
use tokio::sync::{Mutex as AsyncMutex, Notify, oneshot, watch};
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
    completed_model_turns: AtomicU32,
    text: AsyncMutex<String>,
    model_failed: AtomicBool,
    model_failure: Mutex<Option<String>>,
    timed_out: AtomicBool,
    usage: AsyncMutex<Option<Value>>,
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
        let root = app_root.join(app_id).join("agent");
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

impl Control {
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
                json!({"type":"permission","requestId":id,"runId":self.id,"appId":self.owner.app_id,"toolName":name,"input":input}),
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
            let mut output = self.text.lock().await;
            if output.len() + text.len() > 1024 * 1024 {
                self.model_failed.store(true, Ordering::Release);
                self.revoke();
            } else {
                output.push_str(text);
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
                        message_id: id,
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
                let input = input_argument(&args)?;
                let owner_cancel = self
                    .service()
                    .await?
                    .agent_invocation_token(app, invocation)
                    .await?;
                let allow_new = !cfg!(target_os = "ios")
                    || self.has_wake_reservation(app, revision, &input.request_id);
                let run_id = self
                    .start_task(
                        owner,
                        revision,
                        &declared,
                        &declared_skills,
                        input,
                        None,
                        owner_cancel,
                        allow_new,
                        false,
                    )
                    .await?;
                Ok(json!({"runId":run_id}))
            }
            "agent.tasks.get" => {
                let id = only_run_id(&args)?;
                let state = self.app_state(app)?;
                serde_json::to_value(state.store.get(&owner, id).map_err(|_| "task_not_found")?)
                    .map_err(|_| "invalid_task".into())
            }
            "agent.tasks.list" => {
                reject_unknown(&args, &["cursor", "limit"])?;
                let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(50) as usize;
                let cursor = args.get("cursor").and_then(Value::as_str);
                let state = self.app_state(app)?;
                serde_json::to_value(
                    state
                        .store
                        .list(&owner, cursor, limit)
                        .map_err(|_| "invalid_task_list")?,
                )
                .map_err(|_| "invalid_task_list".into())
            }
            "agent.tasks.cancel" => {
                let id = only_run_id(&args)?;
                let state = self.app_state(app)?;
                state.store.get(&owner, id).map_err(|_| "task_not_found")?;
                self.cancel_ids(&[id.into()]).await;
                Ok(Value::Null)
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
                let history = self.chat_history(&state, chat_id, cursor, limit).await?;
                serde_json::to_value(history).map_err(|_| "invalid_chat_history".into())
            }
            "agent.chats.send" => {
                reject_unknown(&args, &["chatId", "input"])?;
                let chat_id = required_string(&args, "chatId")?.to_owned();
                let input = input_argument(&args)?;
                let owner_cancel = self
                    .service()
                    .await?
                    .agent_invocation_token(app, invocation)
                    .await?;
                let allow_new = !cfg!(target_os = "ios")
                    || self.has_wake_reservation(app, revision, &input.request_id);
                let run_id = self
                    .start_task(
                        owner,
                        revision,
                        &declared,
                        &declared_skills,
                        input,
                        Some(chat_id),
                        owner_cancel,
                        allow_new,
                        false,
                    )
                    .await?;
                Ok(json!({"runId":run_id}))
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

        let attachment_metadata = input
            .attachments
            .iter()
            .map(|attachment| AttachmentMetadata {
                name: attachment.name.clone(),
                media_type: attachment.media_type.clone(),
                size: decoded_size(&attachment.base64),
            })
            .collect::<Vec<_>>();
        let user_message_id = if let Some(chat_id) = chat_id.as_deref() {
            if state
                .chats
                .get(chat_id)
                .is_ok_and(|summary| summary.state == ChatState::Running)
            {
                if claimed_wake {
                    self.unclaim_wake(&owner.app_id, revision, &input.request_id);
                }
                let _ = state.store.update(
                    &owner,
                    &id,
                    State::Failed,
                    None,
                    None,
                    Some("chat_busy".into()),
                );
                return Err("busy".into());
            }
            if let Err(error) = state.chats.start_turn(chat_id, &id) {
                if claimed_wake {
                    self.unclaim_wake(&owner.app_id, revision, &input.request_id);
                }
                let _ = state.store.update(
                    &owner,
                    &id,
                    State::Failed,
                    None,
                    None,
                    Some("chat_busy".into()),
                );
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
                let _ = state.store.update(
                    &owner,
                    &id,
                    State::Failed,
                    None,
                    None,
                    Some("task_storage_failed".into()),
                );
                return Err("chat_storage_unavailable".into());
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
            let _ = state.store.update(
                &owner,
                &id,
                State::Failed,
                None,
                None,
                Some("host_wake_required".into()),
            );
            if let Some(chat_id) = &chat_id {
                let _ = state.chats.finish_turn(chat_id, &id);
            }
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
            completed_model_turns: AtomicU32::new(0),
            text: AsyncMutex::new(String::new()),
            model_failed: AtomicBool::new(false),
            model_failure: Mutex::new(None),
            timed_out: AtomicBool::new(false),
            usage: AsyncMutex::new(None),
        });
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

    async fn chat_history(
        &self,
        state: &AppAgentState,
        chat_id: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<ChatHistory, String> {
        if !(1..=100).contains(&limit) {
            return Err("invalid_chat_history".into());
        }
        state.chats.get(chat_id).map_err(|_| "chat_not_found")?;
        let transcript_path = state
            .chats
            .transcript_path(chat_id)
            .map_err(|_| "chat_storage_unavailable")?;
        if !transcript_path.is_file() {
            return Ok(ChatHistory {
                messages: Vec::new(),
                next_cursor: None,
            });
        }
        let session_id = state
            .chats
            .session_id(chat_id)
            .map_err(|_| "chat_not_found")?;
        let session_uuid = uuid::Uuid::parse_str(&session_id).map_err(|_| "invalid_chat")?;
        let replayed = orchestrator::resume::replay_session_state_from_path(
            &transcript_path,
            session_uuid,
            self.factory.context.fs.clone(),
        )
        .await
        .map_err(|_| "chat_transcript_unavailable")?;
        recover_chat_message_metadata(&state.chats, chat_id, &replayed)?;
        let metadata = state
            .chats
            .message_metadata(chat_id)
            .map_err(|_| "chat_storage_unavailable")?
            .into_iter()
            .map(|message| (message.message_id.clone(), message))
            .collect::<HashMap<_, _>>();
        let mut messages = Vec::new();
        for row in replayed.display_history {
            if row.is_meta()
                || row.is_compact_summary()
                || row.role() == lingxi_core::types::MessageRole::System
            {
                continue;
            }
            let id = row.id().to_string();
            let Some(info) = metadata.get(&id) else {
                continue;
            };
            messages.push(ChatMessage {
                message_id: id,
                run_id: info.run_id.clone(),
                role: info.role.clone(),
                text: row.text_content(),
                created_at: info.created_at,
                attachments: info.attachments.clone(),
            });
        }
        if let Some(cursor) = cursor {
            let index = messages
                .iter()
                .position(|message| message.message_id == cursor)
                .ok_or("invalid_chat_cursor")?;
            messages.drain(..=index);
        }
        let has_more = messages.len() > limit;
        messages.truncate(limit);
        let next_cursor = has_more
            .then(|| messages.last().map(|message| message.message_id.clone()))
            .flatten();
        Ok(ChatHistory {
            messages,
            next_cursor,
        })
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
            if control
                .store
                .update(&owner, &id, state, output, usage, error)
                .is_ok()
            {
                self.active.lock().await.remove(&id);
                if let Some(chat_id) = &control.chat_id {
                    let _ = control.chats.finish_turn(chat_id, &id);
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
                        json!({"type":"question","requestId":id,"runId":question_control.id,"appId":question_control.owner.app_id,"questions":questions}),
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
        let cost = session.orchestrator.snapshot_cost().await;
        let usage = usage_value(&cost);
        let text = control.text.lock().await.clone();
        let assistant_message_id = control.assistant_message_id.lock().await.clone();
        if control.chat_id.is_some() && assistant_message_id.is_none() {
            return Err("chat_message_identity_missing".into());
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
                    &["op", "appId", "revision", "requestId", "receiveTimeoutMs"],
                )?;
                let app = required_string(&request, "appId")?;
                let revision = request["revision"].as_u64().ok_or("invalid_revision")?;
                let request_id = required_string(&request, "requestId")?;
                validate_request_id(request_id)?;
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
                    request_id: request_id.to_owned(),
                };
                let expires_at_ms = self
                    .wake_reservations
                    .lock()
                    .map_err(|_| "agent_storage_unavailable")?
                    .reserve(key, now_millis(), receive_timeout)?;
                Ok(
                    json!({"appId":app,"revision":revision,"requestId":request_id,"expiresAtMs":expires_at_ms}),
                )
            }
            "wake.clear" => {
                reject_unknown(&request, &["op", "appId", "revision", "requestId"])?;
                let app = required_string(&request, "appId")?;
                let revision = request["revision"].as_u64().ok_or("invalid_revision")?;
                let request_id = required_string(&request, "requestId")?;
                validate_request_id(request_id)?;
                let _serial = self.admission.lock().await;
                let key = WakeKey {
                    app_id: app.to_owned(),
                    revision,
                    request_id: request_id.to_owned(),
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
                            && control.request_id == request_id
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
                let input: Input = serde_json::from_value(request["input"].clone())
                    .map_err(|_| "invalid_agent_input")?;
                let (owner, tools, skills) = self
                    .current_release(app, revision, &format!("native:{}", input.request_id))
                    .await?;
                let state = self.app_state(app)?;
                if state
                    .store
                    .settings(&owner)
                    .map_err(|_| "agent_storage_unavailable")?
                    .is_none()
                {
                    return Err("agent_not_authorized".into());
                }
                // Native callers may omit a request ID only in the public SDK;
                // this private operation always binds a complete durable input.
                validate_request_id(&input.request_id)?;
                let run_id = self
                    .start_task(
                        owner,
                        revision,
                        &tools,
                        &skills,
                        input,
                        None,
                        CancellationToken::new(),
                        true,
                        true,
                    )
                    .await?;
                Ok(json!({"runId":run_id}))
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
                let run = request["runId"].as_str().ok_or("invalid_run")?;
                let id = request["requestId"].as_str().ok_or("invalid_request")?;
                let c = self
                    .active
                    .lock()
                    .await
                    .get(run)
                    .cloned()
                    .ok_or("task_not_found")?;
                self.validate_current_release(&c.owner).await?;
                if !c.store.lease_valid(&c.owner, run) || c.cancel.is_cancelled() {
                    return Err("task_revoked".into());
                }
                let mut slot = c.interaction.lock().await;
                if !slot.as_ref().is_some_and(|(current, _, _)| current == id) {
                    return Err("request_not_found".into());
                }
                let (_, _, reply) = slot.take().ok_or("request_not_found")?;
                c.store
                    .set_phase(&c.owner, run, None)
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

fn input_argument(args: &Value) -> Result<Input, String> {
    serde_json::from_value(args.get("input").cloned().ok_or("invalid_agent_input")?)
        .map_err(|_| "invalid_agent_input".into())
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
}
