//! Process-owned task service. The page and native control APIs are separate:
//! only the latter can configure grants or resolve permissions/questions.
use super::*;
use crate::mobile::unified_local_apps::MobileLocalAppServiceHandle;
use lingxi_core::{
    host::{permission_gate::PermissionDecision, CostSnapshot},
    types::ToolUseId,
};
use local_app::v2::agent::{AgentStore, Settings, State};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap},
    sync::{
        atomic::{AtomicBool, Ordering},
        Weak,
    },
};
use tokio::sync::{oneshot, Mutex as AsyncMutex, Notify};
use tokio_util::sync::CancellationToken;

pub struct AppAgentHost {
    service: Weak<MobileLocalAppServiceHandle>,
    factory: Arc<AppAgentFactory>,
    store: Arc<AgentStore>,
    root: PathBuf,
    active: AsyncMutex<HashMap<String, Arc<Control>>>,
    admission: AsyncMutex<()>,
    shutdown: AtomicBool,
}
struct Control {
    owner: Owner,
    id: String,
    cancel: CancellationToken,
    finished: AtomicBool,
    done: Notify,
    lease: Mutex<Option<SessionToolGrantLease>>,
    interaction: AsyncMutex<Option<(String, Value, Reply)>>,
    text: AsyncMutex<String>,
    model_failed: AtomicBool,
    timed_out: AtomicBool,
    usage: AsyncMutex<Option<Value>>,
}
enum Reply {
    Permission(oneshot::Sender<bool>),
    Question(oneshot::Sender<Vec<(Utf16JsonProjection, Utf16JsonProjection)>>),
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
        if allow && !self.cancel.is_cancelled() {
            PermissionDecision::Allow
        } else {
            deny()
        }
    }
}
#[async_trait::async_trait]
impl OutputStream for Control {
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
    async fn emit_end_turn(&self, _: &str, _: &CostSnapshot) {}
    async fn emit_terminal_api_error(&self, _: Option<u16>, _: &str, _: &str) {
        self.model_failed.store(true, Ordering::Release);
    }
}
impl AppAgentHost {
    pub fn new(
        service: &Arc<MobileLocalAppServiceHandle>,
        factory: Arc<AppAgentFactory>,
        config_home: &std::path::Path,
    ) -> Result<Arc<Self>, String> {
        let root = config_home.join("local-apps/v2/agent");
        let store = Arc::new(AgentStore::open(&root).map_err(|_| "agent_storage_unavailable")?);
        Ok(Arc::new(Self {
            service: Arc::downgrade(service),
            factory,
            store,
            root,
            active: AsyncMutex::new(HashMap::new()),
            admission: AsyncMutex::new(()),
            shutdown: AtomicBool::new(false),
        }))
    }
    async fn service(&self) -> Result<Arc<MobileLocalAppServiceHandle>, String> {
        self.service
            .upgrade()
            .ok_or_else(|| "app_service_unavailable".into())
    }
    async fn owner(&self, app: &str, invocation: &str) -> Result<(Owner, Vec<String>), String> {
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
        let tools = release["manifest"]["agent"]["tools"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        Ok((owner, tools))
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
        let (owner, declared) = self.owner(app, invocation).await?;
        match method {
            "agent.run" => {
                let input: Input =
                    serde_json::from_value(args).map_err(|_| "invalid_agent_input")?;
                let _serial = self.admission.lock().await;
                if self.shutdown.load(Ordering::Acquire) {
                    return Err("agent_host_replaced".into());
                }
                let owner_cancel = self
                    .service()
                    .await?
                    .agent_invocation_token(app, invocation)
                    .await?;
                let admitted = self
                    .store
                    .admit(&owner, &input, &declared)
                    .map_err(|e| local_app::v2::agent::admission_error_code(&e).to_owned())?;
                let id = admitted.task.run_id.clone();
                if admitted.start {
                    let control = Arc::new(Control {
                        owner: owner.clone(),
                        id: id.clone(),
                        cancel: owner_cancel.child_token(),
                        finished: AtomicBool::new(false),
                        done: Notify::new(),
                        lease: Mutex::new(None),
                        interaction: AsyncMutex::new(None),
                        text: AsyncMutex::new(String::new()),
                        model_failed: AtomicBool::new(false),
                        timed_out: AtomicBool::new(false),
                        usage: AsyncMutex::new(None),
                    });
                    self.active.lock().await.insert(id.clone(), control.clone());
                    let host = self.clone();
                    tokio::spawn(async move {
                        host.execute(control, admitted, input).await;
                    });
                }
                Ok(json!({"runId":id}))
            }
            "agent.get" => {
                let id = only_run_id(&args)?;
                serde_json::to_value(self.store.get(&owner, id).map_err(|_| "task_not_found")?)
                    .map_err(|_| "invalid_task".into())
            }
            "agent.cancel" => {
                let id = only_run_id(&args)?;
                self.store.get(&owner, id).map_err(|_| "task_not_found")?;
                self.cancel_ids(&[id.into()]).await;
                Ok(Value::Null)
            }
            _ => Err("unsupported_agent_method".into()),
        }
    }
    async fn execute(self: Arc<Self>, control: Arc<Control>, admission: Admission, input: Input) {
        let id = control.id.clone();
        let owner = control.owner.clone();
        let cwd = self.root.join("runs").join(&id);
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
            if self
                .store
                .update(&owner, &id, state, output, usage, error)
                .is_ok()
            {
                self.active.lock().await.remove(&id);
                let _ = tokio::fs::remove_dir_all(cwd).await;
            }
        } else {
            let _ = self.store.update(
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
    ) -> Result<(Value, Value), String> {
        use base64::Engine;
        self.owner(&control.owner.app_id, &control.owner.invocation_id)
            .await?;
        tokio::fs::create_dir_all(&cwd)
            .await
            .map_err(|_| "task_storage_failed")?;
        let (question_tx, mut question_rx) =
            tokio::sync::mpsc::channel::<tool_api::ask_user_question::AskUserQuestionExchange>(8);
        let building = self.factory.build(
            &control.owner,
            admission,
            &input,
            cwd,
            control.clone(),
            control.clone(),
            control.cancel.clone(),
            question_tx,
        );
        let session = tokio::select! {biased;_=control.cancel.cancelled()=>return Err("cancelled".into()),result=building=>result?};
        *control.lease.lock().map_err(|_| "task_unavailable")? = Some(session.lease.clone());
        if !self.store.lease_valid(&control.owner, &control.id) || control.cancel.is_cancelled() {
            return Err("cancelled".into());
        }
        self.store
            .update(
                &control.owner,
                &control.id,
                State::Running,
                None,
                None,
                None,
            )
            .map_err(|_| "task_storage_failed")?;
        let mut images = Vec::new();
        for image in input.images {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(image.base64)
                .map_err(|_| "invalid_images")?;
            if bytes.len() > 20 * 1024 * 1024 || !image_signature(&bytes, &image.media_type) {
                return Err("invalid_images".into());
            }
            let processed = tokio::task::spawn_blocking(move || {
                tool_api::util::image_budget::process_image(bytes)
            })
            .await
            .map_err(|_| "invalid_images")?
            .map_err(|_| "invalid_images")?;
            images.push(lingxi_core::types::ImageSource::Base64 {
                media_type: processed.media_type,
                data: processed.base64,
            });
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
                if slot.is_none() {
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
        let outcome = session
            .orchestrator
            .run_turn_streaming_with_cancel_image_sources(
                Utf16JsonProjection::from(input.prompt),
                images,
                control.cancel.clone(),
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
            return Err("model_execution_failed".into());
        }
        if control.cancel.is_cancelled() {
            return Err("cancelled".into());
        }
        match outcome {
            Ok(orchestrator::conversation::TurnOutcome::EndTurn) => {}
            Ok(orchestrator::conversation::TurnOutcome::MaxTurns) => {
                return Err("turn_limit_exceeded".into())
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
        let usage = json!({"inputTokens":cost.input_tokens,"outputTokens":cost.output_tokens,"costUsd":cost.total_usd,"turns":cost.api_calls});
        let result = if let Some(output) = output {
            json!({"output":output})
        } else {
            json!({"text":control.text.lock().await.clone()})
        };
        if result.to_string().len() > 2 * 1024 * 1024 {
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
            let _ = self.store.request_cancel(&control.owner, &control.id);
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
    pub async fn stop_app(&self, app: &str) {
        let ids = self
            .active
            .lock()
            .await
            .iter()
            .filter(|(_, c)| c.owner.app_id == app)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        self.cancel_ids(&ids).await;
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
        let (owner, _) = self.owner(app, invocation).await?;
        if !self
            .store
            .settings(&owner)
            .map_err(|_| "agent_storage_unavailable")?
            .is_some_and(|s| s.background)
        {
            self.stop_app(app).await;
        }
        Ok(())
    }
    /// Trusted native settings/interaction UI only; never exposed by SDK invoke.
    pub async fn host(self: &Arc<Self>, request: Value) -> Result<Value, String> {
        let _serial = self.admission.lock().await;
        if self.shutdown.load(Ordering::Acquire) {
            return Err("agent_host_replaced".into());
        }
        let op = request["op"].as_str().ok_or("invalid_host_request")?;
        match op {
            "cancel" => {
                let run = request["runId"].as_str().ok_or("invalid_run")?;
                self.cancel_ids(&[run.into()]).await;
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
                if op == "settings" {
                    return serde_json::to_value(
                        self.store
                            .settings(&owner)
                            .map_err(|_| "agent_storage_unavailable")?,
                    )
                    .map_err(|_| "invalid_settings".into());
                }
                let settings: Option<Settings> =
                    serde_json::from_value(request["settings"].clone())
                        .map_err(|_| "invalid_settings")?;
                if let Some(settings) = &settings {
                    // The native hosts cancel on background until a platform
                    // execution lease is explicitly implemented and exposed.
                    if settings.background {
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
                }
                let ids = self
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
                self.owner(&c.owner.app_id, &c.owner.invocation_id).await?;
                if !self.store.lease_valid(&c.owner, run) || c.cancel.is_cancelled() {
                    return Err("task_revoked".into());
                }
                let mut slot = c.interaction.lock().await;
                if !slot.as_ref().is_some_and(|(current, _, _)| current == id) {
                    return Err("request_not_found".into());
                }
                let (_, _, reply) = slot.take().ok_or("request_not_found")?;
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

fn usage_value(cost: &CostSnapshot) -> Value {
    json!({"inputTokens":cost.input_tokens,"outputTokens":cost.output_tokens,"costUsd":cost.total_usd,"turns":cost.api_calls})
}
fn image_signature(bytes: &[u8], mime: &str) -> bool {
    match mime {
        "image/jpeg" => bytes.starts_with(&[0xff, 0xd8, 0xff]),
        "image/png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "image/gif" => bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
        "image/webp" => bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP"),
        _ => false,
    }
}
