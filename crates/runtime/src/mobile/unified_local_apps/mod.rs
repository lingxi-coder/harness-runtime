//! Standalone mobile app service. Its lifetime, permissions and processes are
//! independent of the active conversation and Firebase account.
mod gateway;
mod process;

use local_app::v2::{
    AppError, AppLibrary, AppRun, AuthoringRef, PublishRequest, RunPhase, RuntimeLease, Target,
};
use mobile_linux_api::MobileLinuxRuntime;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, OnceLock,
    },
};
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;

#[derive(Debug, thiserror::Error)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Error))]
pub enum MobileLocalAppError {
    #[error("local app service: {message}")]
    Initialization { message: String },
}
#[derive(Debug)]
pub struct ServiceError {
    pub code: String,
    pub message: String,
}
impl ServiceError {
    fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}
impl std::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}
impl std::error::Error for ServiceError {}
impl From<AppError> for ServiceError {
    fn from(e: AppError) -> Self {
        Self::new(e.code(), e.to_string())
    }
}
impl From<std::io::Error> for ServiceError {
    fn from(e: std::io::Error) -> Self {
        Self::new("ioFailure", e.to_string())
    }
}
impl From<mobile_linux_api::MobileLinuxError> for ServiceError {
    fn from(e: mobile_linux_api::MobileLinuxError) -> Self {
        Self::new("runtimeFailure", e.to_string())
    }
}
impl From<serde_json::Error> for ServiceError {
    fn from(e: serde_json::Error) -> Self {
        Self::new("invalidRequest", e.to_string())
    }
}

type Services = HashMap<PathBuf, Arc<MobileLocalAppServiceHandle>>;
static SERVICES: OnceLock<Mutex<Services>> = OnceLock::new();
static EXECUTOR: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
static EXECUTOR_INIT: Mutex<()> = Mutex::new(());

pub(super) struct ActiveRun {
    pub record: Mutex<AppRun>,
    pub cancel: CancellationToken,
    pub agent_access: Mutex<CancellationToken>,
    pub handles: AsyncMutex<Vec<mobile_linux_api::LinuxProcessHandle>>,
    pub gateway: AsyncMutex<Option<tokio::task::JoinHandle<Result<(), std::io::ErrorKind>>>>,
    pub gateway_failure: Mutex<Option<std::io::ErrorKind>>,
    pub listener: AsyncMutex<Option<Arc<tokio::net::TcpListener>>>,
    pub committed: AtomicBool,
    pub cleanup_complete: AtomicBool,
    pub log_task: AsyncMutex<Option<tokio::task::JoinHandle<()>>>,
    pub log_stop: CancellationToken,
    pub log_task_ids: Mutex<BTreeSet<String>>,
    pub url: Mutex<String>,
    pub output: Mutex<BTreeMap<String, String>>,
    pub service_tasks: Mutex<Vec<String>>,
    pub lifecycle: AsyncMutex<()>,
    pub previous: Mutex<Option<Arc<ActiveRun>>>,
    pub execution_lease: Mutex<Option<super::linux_maintenance::AppExecutionLease>>,
}

enum GatewayHealth {
    Starting,
    Healthy,
    Failed(Option<std::io::ErrorKind>),
}

async fn gateway_health(run: &ActiveRun) -> GatewayHealth {
    let gateway = run.gateway.lock().await;
    let failure = *run.gateway_failure.lock().unwrap();
    if failure.is_some() {
        GatewayHealth::Failed(failure)
    } else if gateway.as_ref().is_some_and(|task| task.is_finished()) {
        GatewayHealth::Failed(None)
    } else if gateway.is_none() {
        GatewayHealth::Starting
    } else {
        GatewayHealth::Healthy
    }
}

#[cfg(test)]
pub(super) struct GatewayStartGate {
    pub entered: Arc<tokio::sync::Notify>,
    pub release: Arc<tokio::sync::Notify>,
}

struct AppOperation {
    serial: AsyncMutex<()>,
    cancellation: Mutex<CancellationToken>,
}

#[cfg_attr(feature = "uniffi", derive(uniffi::Object))]
pub struct MobileLocalAppServiceHandle {
    agent: Mutex<Option<Result<Arc<super::app_agent::AppAgentHost>, String>>>,
    pub(crate) agent_generation: Arc<AsyncMutex<u64>>,
    pub(super) library: Arc<AppLibrary>,
    _runtime_owner: RuntimeLease,
    pub(super) config_home: PathBuf,
    runtime: Mutex<Option<Arc<dyn MobileLinuxRuntime>>>,
    target: Mutex<Target>,
    active: AsyncMutex<HashMap<String, Arc<ActiveRun>>>,
    operations: Mutex<HashMap<String, Arc<AppOperation>>>,
    #[cfg(test)]
    gateway_acceptor: Mutex<Option<gateway::AcceptFactory>>,
    #[cfg(test)]
    gateway_start_gate: Mutex<Option<GatewayStartGate>>,
}

impl MobileLocalAppServiceHandle {
    pub(crate) async fn agent_invocation_token(
        &self,
        app: &str,
        invocation: &str,
    ) -> Result<CancellationToken, String> {
        let active = self.active.lock().await;
        let candidate = active.get(app).ok_or("app_not_running")?;
        let previous = candidate
            .previous
            .lock()
            .unwrap()
            .clone()
            .filter(|previous| {
                previous.record.lock().unwrap().phase == RunPhase::Running
                    && !previous.cancel.is_cancelled()
            });
        let run = previous.as_ref().unwrap_or(candidate);
        let record = run.record.lock().unwrap();
        if record.run_id != invocation || record.phase != RunPhase::Running {
            return Err("stale_app_invocation".into());
        }
        let token = run.agent_access.lock().unwrap().clone();
        if token.is_cancelled() {
            return Err("app_invocation_closed".into());
        }
        Ok(token)
    }
    pub(crate) async fn attach_agent_factory(
        self: &Arc<Self>,
        factory: Arc<super::app_agent::AppAgentFactory>,
    ) -> Result<(), String> {
        // One service-owned lock spans every host generation. Recovery may
        // await filesystem work, so a per-host admission lock cannot protect
        // its cached ledger from a replacement loading and changing that file.
        let mut generation = self.agent_generation.lock().await;
        // Revoke and reap the old executor before opening the new ledger.
        let previous = self
            .agent
            .lock()
            .map_err(|_| "agent_host_unavailable")?
            .clone();
        if let Some(Ok(previous)) = previous {
            previous.stop_all_in_generation(&generation).await?;
        }
        let mut slot = self.agent.lock().map_err(|_| "agent_host_unavailable")?;
        let replacement = super::app_agent::AppAgentHost::new_in_generation(
            self,
            factory,
            &self.config_home,
            &mut generation,
        );
        let result = replacement.as_ref().map(|_| ()).map_err(Clone::clone);
        *slot = Some(replacement);
        result
    }
    fn agent_host(&self) -> Result<Arc<super::app_agent::AppAgentHost>, String> {
        self.agent
            .lock()
            .map_err(|_| "agent_host_unavailable")?
            .as_ref()
            .ok_or_else(|| "agent_model_host_not_ready".into())
            .and_then(Clone::clone)
    }
    /// Constructors for different session hosts reuse the same app owner. A
    /// later native runtime handle can fill the lazy slot without booting it.
    pub fn new(
        config_home: PathBuf,
        runtime: Option<Arc<dyn MobileLinuxRuntime>>,
        platform: String,
    ) -> Result<Arc<Self>, MobileLocalAppError> {
        let fail = |message: String| MobileLocalAppError::Initialization { message };
        std::fs::create_dir_all(&config_home).map_err(|e| fail(e.to_string()))?;
        let config_home = config_home
            .canonicalize()
            .map_err(|e| fail(e.to_string()))?;
        let _executor_init = EXECUTOR_INIT
            .lock()
            .map_err(|_| fail("executor initializer poisoned".into()))?;
        if EXECUTOR.get().is_none() {
            let executor = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .thread_name("local-app-runtime")
                .build()
                .map_err(|e| fail(e.to_string()))?;
            let _ = EXECUTOR.set(executor);
        }
        drop(_executor_init);
        let mut instances = SERVICES
            .get_or_init(Mutex::default)
            .lock()
            .map_err(|_| fail("service registry poisoned".into()))?;
        if let Some(existing) = instances.get(&config_home).cloned() {
            if runtime.is_some() {
                let mut slot = existing
                    .runtime
                    .lock()
                    .map_err(|_| fail("runtime registry poisoned".into()))?;
                if slot.is_none() {
                    *slot = runtime;
                }
            }
            return Ok(existing);
        }
        if !matches!(platform.as_str(), "android" | "ios") {
            return Err(fail("platform must be android or ios".into()));
        }
        let library = Arc::new(AppLibrary::open(&config_home).map_err(|e| fail(e.to_string()))?);
        let runtime_owner = library
            .acquire_runtime_lease()
            .map_err(|e| fail(e.to_string()))?;
        let service = Arc::new(Self {
            agent: Mutex::new(None),
            agent_generation: Arc::new(AsyncMutex::new(0)),
            library,
            _runtime_owner: runtime_owner,
            config_home: config_home.clone(),
            runtime: Mutex::new(runtime),
            target: Mutex::new(Target {
                platform,
                environment: "web".into(),
                architecture: None,
                runtime_version: None,
                runtime_abi: None,
                channels: Vec::new(),
                capabilities: Vec::new(),
            }),
            active: AsyncMutex::new(HashMap::new()),
            operations: Mutex::new(HashMap::new()),
            #[cfg(test)]
            gateway_acceptor: Mutex::new(None),
            #[cfg(test)]
            gateway_start_gate: Mutex::new(None),
        });
        instances.insert(config_home, service.clone());
        Ok(service)
    }

    /// Tool calls are cancellation-owned, but app tasks always belong to the
    /// standalone executor. Dropping the tool wait aborts only its operation;
    /// the start guard cleans that run without touching the previous release.
    pub async fn request_for_tool(self: Arc<Self>, request: Value) -> Result<Value, ServiceError> {
        struct AbortWait<T>(tokio::task::JoinHandle<T>);
        impl<T> Drop for AbortWait<T> {
            fn drop(&mut self) {
                self.0.abort();
            }
        }
        let mut wait = AbortWait(
            EXECUTOR
                .get()
                .expect("app executor")
                .spawn(async move { self.request_value(request).await }),
        );
        (&mut wait.0).await.map_err(join_error)?
    }

    pub async fn request_value(self: &Arc<Self>, request: Value) -> Result<Value, ServiceError> {
        if !request.is_object() {
            return Err(ServiceError::new(
                "invalidRequest",
                "request must be an object",
            ));
        }
        let op = required(&request, "op")?;
        let allowed: Option<&[&str]> = match op {
            "start" | "resume" | "restart" | "repair" => Some(&["op", "appId", "revision"]),
            "stop" | "suspend" | "status" => Some(&["op", "appId"]),
            "configureTarget" => Some(&["op", "target"]),
            "createAuthoring" => Some(&["op", "name"]),
            _ => None,
        };
        if let Some(allowed) = allowed {
            if request
                .as_object()
                .unwrap()
                .keys()
                .any(|key| !allowed.contains(&key.as_str()))
            {
                return Err(ServiceError::new(
                    "invalidRequest",
                    "unknown operation field",
                ));
            }
            if request
                .get("revision")
                .is_some_and(|value| value.as_u64().is_none_or(|revision| revision == 0))
            {
                return Err(ServiceError::new(
                    "invalidRequest",
                    "revision must be a positive integer",
                ));
            }
        }
        match op {
            "start" | "resume" | "restart" | "repair" | "stop" | "suspend" | "revoke"
            | "remove" => self.transition(request).await,
            "status" => {
                let id = required(&request, "appId")?;
                let active = self.active.lock().await;
                Ok(match active.get(id) {
                    Some(run) => {
                        let candidate = run.record.lock().unwrap().clone();
                        let serving = run.previous.lock().unwrap().clone().filter(|previous| {
                            previous.record.lock().unwrap().phase == RunPhase::Running
                                && !previous.cancel.is_cancelled()
                        });
                        if let Some(serving) = serving {
                            json!({"run":serving.record.lock().unwrap().clone(),"url":serving.url.lock().unwrap().clone(),"preparingRun":candidate})
                        } else {
                            json!({"run":candidate,"url":run.url.lock().unwrap().clone()})
                        }
                    }
                    None => json!({"run":null,"url":null}),
                })
            }
            "configureTarget" => {
                let target: Target = serde_json::from_value(
                    request
                        .get("target")
                        .cloned()
                        .ok_or_else(|| ServiceError::new("invalidRequest", "target required"))?,
                )?;
                target.validate()?;
                let active = self.active.lock().await;
                let mut current = self.target.lock().unwrap();
                if *current == target {
                    return Ok(serde_json::to_value(target)?);
                }
                if !active.is_empty() {
                    return Err(ServiceError::new(
                        "busy",
                        "stop local applications before changing host capabilities",
                    ));
                }
                if target.platform != current.platform {
                    return Err(ServiceError::new(
                        "invalidRequest",
                        "host platform cannot change",
                    ));
                }
                *current = target.clone();
                Ok(serde_json::to_value(target)?)
            }
            "prepareAuthoring" => {
                let id = required(&request, "appId")?.to_owned();
                let library = self.library.clone();
                let record = tokio::task::spawn_blocking(move || library.get(&id))
                    .await
                    .map_err(join_error)??;
                let source = record
                    .authoring
                    .as_ref()
                    .map(|r| r.source_directory.as_str())
                    .unwrap_or("");
                let exists = self.host_path(source).is_ok_and(|p| p.is_dir());
                let mut request = request;
                request["sourceExists"] = json!(exists);
                self.library_request(request).await
            }
            "createAuthoring" => self.create_authoring(required(&request, "name")?).await,
            // These mutations belong to the runtime, never a UI/web page.
            "prepareRun" | "updateRun" | "activateRelease" => Err(ServiceError::new(
                "invalidOperation",
                "use start to prepare and activate a release",
            )),
            "publish" | "import" => {
                let is_publish = op == "publish";
                let mut request = request;
                if is_publish
                    && request
                        .get("authoring")
                        .is_some_and(|authoring| !authoring.is_null())
                {
                    return Err(ServiceError::new(
                        "invalidRequest",
                        "publish authoring must be registered by the trusted native host",
                    ));
                }
                let source = required(&request, "sourcePath")?.to_owned();
                let host_source = self.host_path(&source)?;
                request["sourcePath"] = json!(host_source.to_string_lossy());
                if is_publish && request.get("appId").and_then(Value::as_str).is_none() {
                    if let Some(source_directory) =
                        self.canonical_owned_authoring_source(&host_source)?
                    {
                        let mut publish_request = request;
                        publish_request
                            .as_object_mut()
                            .expect("request was checked as an object")
                            .remove("op");
                        let publish_request: PublishRequest =
                            serde_json::from_value(publish_request)?;
                        let library = self.library.clone();
                        let published = tokio::task::spawn_blocking(move || {
                            library
                                .publish_with_pending_authoring(publish_request, &source_directory)
                        })
                        .await
                        .map_err(join_error)??;
                        return Ok(serde_json::to_value(published)?);
                    }
                }
                self.library_request(request).await
            }
            _ => self.library_request(request).await,
        }
    }

    /// Publish a staged copy while resolving authoring identity only from the
    /// tool host's separately injected, validated original source path.
    pub(super) async fn publish_from_trusted_source(
        self: &Arc<Self>,
        staged_source_path: &str,
        original_source_path: Option<&str>,
        app_id: Option<&str>,
        expected_revision: u64,
    ) -> Result<Value, ServiceError> {
        if let Some(app_id) = app_id {
            return self
                .request_value(json!({
                    "op":"publish",
                    "appId":app_id,
                    "expectedRevision":expected_revision,
                    "sourcePath":staged_source_path
                }))
                .await;
        }
        let pending_source_directory = if let Some(original_source_path) = original_source_path {
            let host_source = self.host_path(original_source_path)?;
            self.canonical_owned_authoring_source(&host_source)?
        } else {
            None
        };
        let Some(source_directory) = pending_source_directory else {
            return self
                .request_value(json!({
                    "op":"publish",
                    "expectedRevision":expected_revision,
                    "sourcePath":staged_source_path
                }))
                .await;
        };
        let staged_path = self.host_path(staged_source_path)?;
        let request = PublishRequest {
            app_id: None,
            expected_revision,
            source_path: staged_path.to_string_lossy().into_owned(),
            authoring: None,
            market_origin: None,
        };
        let library = self.library.clone();
        let published = tokio::task::spawn_blocking(move || {
            library.publish_with_pending_authoring(request, &source_directory)
        })
        .await
        .map_err(join_error)??;
        Ok(serde_json::to_value(published)?)
    }

    async fn transition(self: &Arc<Self>, request: Value) -> Result<Value, ServiceError> {
        let op = required(&request, "op")?;
        let id = required(&request, "appId")?;
        let operation = self.operation(id);
        let cancellation = {
            let mut current = operation.cancellation.lock().unwrap();
            if !matches!(op, "start" | "resume") {
                // Interrupt both queued starts and the current install/readiness
                // wait before waiting for serialization. A later start observes
                // the grant/removal result after this transition completes.
                current.cancel();
                *current = CancellationToken::new();
            }
            current.child_token()
        };
        let _serial = operation.serial.lock().await;
        let revision = request.get("revision").and_then(Value::as_u64);
        match op {
            "start" | "resume" | "restart" | "repair" => {
                if op == "restart" {
                    self.stop(id, RunPhase::Stopped).await?;
                }
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => Err(ServiceError::new("cancelled", "application operation cancelled")),
                    result = self.start(id, revision, op == "repair") => result,
                }
            }
            "revoke" => {
                // Revoke even if process cleanup fails; the retained stopping
                // owner can retry Stop, but no concurrent start can get consent.
                let value = self.library_request(request.clone()).await?;
                self.stop(id, RunPhase::Stopped).await?;
                Ok(value)
            }
            "remove" => {
                self.stop(id, RunPhase::Stopped).await?;
                self.library_request(request).await
            }
            _ => {
                self.stop(
                    id,
                    if op == "suspend" {
                        RunPhase::Suspended
                    } else {
                        RunPhase::Stopped
                    },
                )
                .await
            }
        }
    }

    fn operation(&self, app_id: &str) -> Arc<AppOperation> {
        self.operations
            .lock()
            .unwrap()
            .entry(app_id.to_owned())
            .or_insert_with(|| {
                Arc::new(AppOperation {
                    serial: AsyncMutex::new(()),
                    cancellation: Mutex::new(CancellationToken::new()),
                })
            })
            .clone()
    }

    pub(super) async fn gateway_failed(
        self: &Arc<Self>,
        app_id: String,
        run: Arc<ActiveRun>,
        error_kind: std::io::ErrorKind,
    ) {
        let operation = self.operation(&app_id);
        let _serial = operation.serial.lock().await;
        if run.cancel.is_cancelled() {
            return;
        }
        let is_current = self
            .active
            .lock()
            .await
            .get(&app_id)
            .is_some_and(|current| Arc::ptr_eq(current, &run));
        if !is_current || run.record.lock().unwrap().phase != RunPhase::Running {
            return;
        }
        {
            let mut record = run.record.lock().unwrap();
            record.phase = RunPhase::Stopping;
            record.services.clear();
            record.error = Some(format!("gateway_accept_failed:{error_kind:?}"));
            *run.url.lock().unwrap() = String::new();
        }
        let _ = self.stop_chain(&app_id, run, RunPhase::Failed, false).await;
    }

    async fn cleanup_failed_gateway(
        &self,
        app_id: &str,
        run: Arc<ActiveRun>,
        health: GatewayHealth,
    ) -> Result<(), ServiceError> {
        let failure = match health {
            GatewayHealth::Failed(Some(kind)) => format!("gateway_accept_failed:{kind:?}"),
            GatewayHealth::Failed(None) => {
                let gateway_task = { run.gateway.lock().await.take() };
                match gateway_task {
                    Some(task) => match task.await {
                        Ok(Err(kind)) => format!("gateway_accept_failed:{kind:?}"),
                        Ok(Ok(())) => "gateway_stopped_unexpectedly".into(),
                        Err(error) if error.is_cancelled() => "gateway_task_aborted".into(),
                        Err(_) => "gateway_task_panicked".into(),
                    },
                    None => "gateway_handle_missing".into(),
                }
            }
            GatewayHealth::Starting => "gateway_start_incomplete".into(),
            GatewayHealth::Healthy => return Ok(()),
        };
        {
            let mut record = run.record.lock().unwrap();
            record.phase = RunPhase::Stopping;
            record.services.clear();
            record.error = Some(failure);
            *run.url.lock().unwrap() = String::new();
        }
        self.stop_chain(app_id, run, RunPhase::Failed, false)
            .await
            .map(|_| ())
    }

    async fn library_request(&self, request: Value) -> Result<Value, ServiceError> {
        let library = self.library.clone();
        let encoded = serde_json::to_string(&request)?;
        tokio::task::spawn_blocking(move || library.request_json(&encoded))
            .await
            .map_err(join_error)?
            .map_err(Into::into)
    }

    pub(super) fn runtime(&self) -> Result<Arc<dyn MobileLinuxRuntime>, ServiceError> {
        self.runtime.lock().unwrap().clone().ok_or_else(|| {
            ServiceError::new(
                "runtimeUnavailable",
                "Linux runtime is not configured; static applications remain available",
            )
        })
    }
    pub(super) fn host_path(&self, path: &str) -> Result<PathBuf, ServiceError> {
        let path = Path::new(path);
        if !path.is_absolute()
            || path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(ServiceError::new(
                "invalidPath",
                "expected an absolute contained source path",
            ));
        }
        if let Ok(suffix) = path.strip_prefix(self.guest_home_path()?) {
            return Ok(self
                .config_home
                .parent()
                .ok_or_else(|| ServiceError::new("invalidPath", "missing user home"))?
                .join(suffix));
        }
        Ok(path.to_path_buf())
    }
    fn guest_home_path(&self) -> Result<PathBuf, ServiceError> {
        let linux_home = self
            .config_home
            .parent()
            .ok_or_else(|| ServiceError::new("invalidPath", "missing Linux user home"))?;
        let linux_user = linux_home
            .file_name()
            .and_then(|value| value.to_str())
            .filter(|value| {
                !value.is_empty()
                    && *value != "."
                    && *value != ".."
                    && value
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
            })
            .ok_or_else(|| ServiceError::new("invalidPath", "invalid Linux user directory"))?;
        Ok(Path::new("/home").join(linux_user))
    }
    pub(super) fn guest_path(&self, path: &Path) -> Result<String, ServiceError> {
        let home = self
            .config_home
            .parent()
            .ok_or_else(|| ServiceError::new("invalidPath", "missing user home"))?;
        if let Ok(suffix) = path.strip_prefix(home) {
            return Ok(self
                .guest_home_path()?
                .join(suffix)
                .to_string_lossy()
                .into_owned());
        }
        let runtime = self.runtime()?;
        let mut mounts = runtime.current_mounts();
        mounts.sort_by_key(|m| std::cmp::Reverse(m.host_path.components().count()));
        for mount in mounts {
            if let Ok(suffix) = path.strip_prefix(&mount.host_path) {
                return Ok(Path::new(&mount.guest_path)
                    .join(suffix)
                    .to_string_lossy()
                    .into_owned());
            }
        }
        Err(ServiceError::new(
            "sourceUnavailable",
            "source directory is not mapped into the Linux user space",
        ))
    }
    fn canonical_owned_authoring_source(
        &self,
        host_source: &Path,
    ) -> Result<Option<String>, ServiceError> {
        let apps_root = self.config_home.join(".apps");
        let apps_metadata = match std::fs::symlink_metadata(&apps_root) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if apps_metadata.file_type().is_symlink() || !apps_metadata.is_dir() {
            return Err(ServiceError::new(
                "invalidPath",
                "the configured application source root is not a real directory",
            ));
        }
        let source_metadata = std::fs::symlink_metadata(host_source)?;
        if source_metadata.file_type().is_symlink() || !source_metadata.is_dir() {
            return Err(ServiceError::new(
                "invalidPath",
                "authoring source must be a real directory",
            ));
        }
        let apps_root = std::fs::canonicalize(apps_root)?;
        let host_source = std::fs::canonicalize(host_source)?;
        let Ok(relative) = host_source.strip_prefix(&apps_root) else {
            return Ok(None);
        };
        if relative.components().count() != 1 || host_source == apps_root {
            return Err(ServiceError::new(
                "invalidPath",
                "authoring source must be a canonical direct child of this brand's application source root",
            ));
        }
        Ok(Some(self.guest_path(&host_source)?))
    }
    async fn create_authoring(&self, name: &str) -> Result<Value, ServiceError> {
        if name.trim().is_empty() || name.chars().count() > 80 {
            return Err(ServiceError::new(
                "invalidRequest",
                "name must contain 1–80 characters",
            ));
        }
        let slug: String = name
            .trim()
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        let slug = if slug.trim_matches('-').is_empty() {
            "app"
        } else {
            slug.trim_matches('-')
        };
        let directory = self
            .config_home
            .join(".apps")
            .join(format!("{slug}-{}", &uuid::Uuid::new_v4().to_string()[..8]));
        let source_directory = self.guest_path(&directory)?;
        let display = name.to_owned();
        let path = directory.clone();
        let targets: Vec<_> = ["android", "ios", "macos", "windows", "linux", "web"]
            .into_iter()
            .map(|platform| json!({"platform":platform,"environment":"web"}))
            .collect();
        tokio::task::spawn_blocking(move||->Result<(),ServiceError>{
            std::fs::create_dir_all(&path)?;
            let manifest=json!({"schema":2,"name":display,"entry":{"type":"static","path":"index.html"},"targets":targets,"package":{"files":["index.html","lingxi-app.json"],"dependencyLocks":[]}});
            std::fs::write(path.join("lingxi-app.json"),serde_json::to_vec_pretty(&manifest)?)?;
            std::fs::write(path.join("index.html"),"<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>Local App</title></head><body><main><h1>Local App</h1><p>Build your application here.</p></main></body></html>")?;
            Ok(())
        }).await.map_err(join_error)??;
        Ok(json!({"sourceDirectory":source_directory,"sourcePath":directory.to_string_lossy()}))
    }

    async fn register_authoring(&self, request: Value) -> Result<Value, ServiceError> {
        let request_object = request
            .as_object()
            .ok_or_else(|| ServiceError::new("invalidRequest", "request must be an object"))?;
        if request_object
            .keys()
            .any(|key| !matches!(key.as_str(), "op" | "authoring"))
        {
            return Err(ServiceError::new(
                "invalidRequest",
                "unknown registerAuthoring field",
            ));
        }
        let authoring_value = request
            .get("authoring")
            .filter(|value| value.is_object())
            .ok_or_else(|| ServiceError::new("invalidRequest", "authoring must be an object"))?;
        let authoring_object = authoring_value.as_object().unwrap();
        if authoring_object.keys().any(|key| {
            !matches!(
                key.as_str(),
                "sourceDirectory" | "workspaceId" | "projectRef" | "authoringSessionId" | "appId"
            )
        }) {
            return Err(ServiceError::new(
                "invalidRequest",
                "unknown authoring binding field",
            ));
        }
        if let Some(project_ref) = authoring_value.get("projectRef") {
            let project_object = project_ref.as_object().ok_or_else(|| {
                ServiceError::new("invalidRequest", "projectRef must be an object")
            })?;
            let kind = project_ref.get("kind").and_then(Value::as_str);
            let valid_fields = match kind {
                Some("mobileProject") => &["kind", "projectId"][..],
                Some("desktopPath") => &["kind", "path"][..],
                _ => {
                    return Err(ServiceError::new(
                        "invalidRequest",
                        "projectRef kind is unsupported",
                    ));
                }
            };
            if project_object
                .keys()
                .any(|key| !valid_fields.contains(&key.as_str()))
            {
                return Err(ServiceError::new(
                    "invalidRequest",
                    "projectRef contains an unknown field",
                ));
            }
        }
        if authoring_value
            .get("appId")
            .is_some_and(|app_id| !app_id.is_null())
        {
            return Err(ServiceError::new(
                "invalidRequest",
                "registerAuthoring cannot bind an application id before publish",
            ));
        }
        let authoring: AuthoringRef = serde_json::from_value(authoring_value.clone())?;
        self.validate_registered_authoring_source(&authoring)?;
        let library = self.library.clone();
        let saved =
            tokio::task::spawn_blocking(move || library.register_pending_authoring(authoring))
                .await
                .map_err(join_error)??;
        Ok(serde_json::to_value(saved)?)
    }

    fn validate_registered_authoring_source(
        &self,
        authoring: &AuthoringRef,
    ) -> Result<(), ServiceError> {
        let source_path = self.host_path(&authoring.source_directory)?;
        let apps_root = self.config_home.join(".apps");
        let apps_metadata = std::fs::symlink_metadata(&apps_root).map_err(|error| {
            ServiceError::new(
                "invalidPath",
                format!("application source root is unavailable: {error}"),
            )
        })?;
        if apps_metadata.file_type().is_symlink() || !apps_metadata.is_dir() {
            return Err(ServiceError::new(
                "invalidPath",
                "the configured application source root is not a real directory",
            ));
        }
        let source_metadata = std::fs::symlink_metadata(&source_path).map_err(|error| {
            ServiceError::new(
                "invalidPath",
                format!("authoring source is unavailable: {error}"),
            )
        })?;
        if source_metadata.file_type().is_symlink() || !source_metadata.is_dir() {
            return Err(ServiceError::new(
                "invalidPath",
                "authoring source must be a real directory",
            ));
        }
        let canonical_root = std::fs::canonicalize(apps_root)?;
        let canonical_source = std::fs::canonicalize(source_path)?;
        let relative = canonical_source
            .strip_prefix(&canonical_root)
            .map_err(|_| {
                ServiceError::new(
                    "invalidPath",
                    "authoring source must belong to this brand's application source root",
                )
            })?;
        if relative.components().count() != 1
            || canonical_source == canonical_root
            || self.guest_path(&canonical_source)? != authoring.source_directory
        {
            return Err(ServiceError::new(
                "invalidPath",
                "authoring source must be a canonical direct child of this brand's application source root",
            ));
        }
        Ok(())
    }

    async fn start(
        self: &Arc<Self>,
        app_id: &str,
        revision: Option<u64>,
        rebuild: bool,
    ) -> Result<Value, ServiceError> {
        let library = self.library.clone();
        let id = app_id.to_owned();
        let (record, release) = tokio::task::spawn_blocking(move || -> Result<_, AppError> {
            let r = library.get(&id)?;
            let release = library.release(&id, revision.unwrap_or(r.current_revision))?;
            Ok((r, release))
        })
        .await
        .map_err(join_error)??;
        let mut target = self.target.lock().unwrap().clone();
        target.environment = if release.manifest.requires_execution() {
            "mobile-linux"
        } else {
            "web"
        }
        .into();
        if !release.manifest.compatible_with(&target)? {
            return Err(ServiceError::new(
                "incompatibleTarget",
                "release does not support this device, runtime, channel, or native capabilities",
            ));
        }
        if !release
            .manifest
            .accepts_data_schema(record.data_schema_version)
        {
            return Err(ServiceError::from(AppError::DataIncompatible {
                current: record.data_schema_version,
                target: release.manifest.schema_version(),
            }));
        }
        let mut previous = None;
        loop {
            let current = self.active.lock().await.get(app_id).cloned();
            let Some(run) = current else { break };
            let r = run.record.lock().unwrap().clone();
            if r.phase != RunPhase::Running {
                return Err(ServiceError::new(
                    "busy",
                    "an application transition is already in progress",
                ));
            }
            if run.cancel.is_cancelled() {
                return Err(ServiceError::new(
                    "cancelled",
                    "the existing application run is stopping",
                ));
            }
            match gateway_health(&run).await {
                GatewayHealth::Failed(health) => {
                    // The old listener may still be bound while a failed
                    // accept task unwinds. Reap before binding that port.
                    self.cleanup_failed_gateway(app_id, run, GatewayHealth::Failed(health))
                        .await?;
                    continue;
                }
                GatewayHealth::Starting => {
                    // A Running record is committed only after its gateway
                    // handle is stored. Do not reuse its URL during startup.
                    return Err(ServiceError::new(
                        "gatewayStarting",
                        "the application gateway is starting; retry the open action",
                    ));
                }
                GatewayHealth::Healthy => {}
            }
            if !rebuild && r.revision == release.revision {
                match gateway_health(&run).await {
                    GatewayHealth::Healthy => {}
                    GatewayHealth::Failed(_) => continue,
                    GatewayHealth::Starting => {
                        return Err(ServiceError::new(
                            "gatewayStarting",
                            "the application gateway is starting; retry the open action",
                        ));
                    }
                }
                return Ok(
                    json!({"run":r,"url":run.url.lock().unwrap().clone(),"origin":format!("http://127.0.0.1:{}",record.origin_port)}),
                );
            }
            previous = Some(run);
            break;
        }
        let library = self.library.clone();
        let id = app_id.to_owned();
        let revision = release.revision;
        let operation = CancellationToken::new();
        let drop_guard = operation.clone().drop_guard();
        let preparing = operation.clone();
        let preparation = tokio::task::spawn_blocking(move || {
            let mut preparation = library.prepare_run_with_options(&id, revision, rebuild)?;
            if preparing.is_cancelled() {
                preparation.run.phase = RunPhase::Stopped;
                library.update_run(preparation.run.clone())?;
            }
            Ok::<_, AppError>(preparation)
        })
        .await
        .map_err(join_error)??;
        let cancellation = CancellationToken::new();
        let run = Arc::new(ActiveRun {
            record: Mutex::new(preparation.run.clone()),
            agent_access: Mutex::new(cancellation.child_token()),
            cancel: cancellation,
            handles: AsyncMutex::new(Vec::new()),
            gateway: AsyncMutex::new(None),
            gateway_failure: Mutex::new(None),
            listener: AsyncMutex::new(None),
            committed: AtomicBool::new(false),
            cleanup_complete: AtomicBool::new(false),
            log_task: AsyncMutex::new(None),
            log_stop: CancellationToken::new(),
            log_task_ids: Mutex::new(BTreeSet::new()),
            url: Mutex::new(String::new()),
            output: Mutex::new(BTreeMap::new()),
            service_tasks: Mutex::new(Vec::new()),
            lifecycle: AsyncMutex::new(()),
            previous: Mutex::new(previous),
            execution_lease: Mutex::new(None),
        });
        let mut active = self.active.lock().await;
        active.insert(app_id.to_owned(), run.clone());
        drop(active);
        let owner = self.clone();
        let owned_run = run.clone();
        let owned_app_id = app_id.to_owned();
        EXECUTOR.get().expect("app executor").spawn(async move {
            tokio::select! { _ = operation.cancelled() => {}, _ = owned_run.cancel.cancelled() => return }
            let lifecycle = owned_run.lifecycle.lock().await;
            // The final durable switch is cancellation-resistant. A dropped
            // waiter cannot undo a commit that completed while it waited here.
            if owned_run.committed.load(Ordering::Acquire) { return; }
            if owner.stop_owned_locked(&owned_app_id, owned_run.clone(), RunPhase::Stopped, true).await.is_err() && !owned_run.cleanup_complete.load(Ordering::Acquire) { return; }
            drop(lifecycle);
            let mut active = owner.active.lock().await;
            if active.get(&owned_app_id).is_some_and(|current| Arc::ptr_eq(current,&owned_run)) {
                if let Some(previous) = owned_run.previous.lock().unwrap().take() { *previous.agent_access.lock().unwrap()=previous.cancel.child_token(); active.insert(owned_app_id,previous); }
                else { active.remove(&owned_app_id); }
            }
        });
        let result = self.prepare_and_launch(&preparation, run.clone()).await;
        drop_guard.disarm();
        match result {
            Ok(value) => match gateway_health(&run).await {
                GatewayHealth::Healthy => Ok(value),
                health => {
                    let _ = self.cleanup_failed_gateway(app_id, run, health).await;
                    Err(ServiceError::new(
                        "gatewayFailed",
                        "the application gateway failed before startup completed",
                    ))
                }
            },
            Err(error) => {
                let stopped_by_host = {
                    let _lifecycle = run.lifecycle.lock().await;
                    matches!(
                        run.record.lock().unwrap().phase,
                        RunPhase::Stopped | RunPhase::Suspended
                    )
                };
                if error.code == "cancelled" && stopped_by_host {
                    return Err(error);
                }
                let cleanup = self
                    .stop_owned(app_id, run.clone(), RunPhase::Failed, true)
                    .await;
                if let Err(cleanup) = cleanup {
                    if !run.cleanup_complete.load(Ordering::Acquire) {
                        return Err(ServiceError::new(
                            "stopFailed",
                            format!("{error}; cleanup: {cleanup}"),
                        ));
                    }
                }
                let mut failed = run.record.lock().unwrap().clone();
                failed.phase = RunPhase::Failed;
                failed.services.clear();
                failed.error = Some(error.to_string());
                let library = self.library.clone();
                let _ = tokio::task::spawn_blocking(move || library.update_run(failed)).await;
                let mut active = self.active.lock().await;
                if active
                    .get(app_id)
                    .is_some_and(|current| Arc::ptr_eq(current, &run))
                {
                    if let Some(previous) = run.previous.lock().unwrap().take() {
                        *previous.agent_access.lock().unwrap() = previous.cancel.child_token();
                        active.insert(app_id.into(), previous);
                    } else {
                        active.remove(app_id);
                    }
                }
                Err(error)
            }
        }
    }

    async fn stop(&self, app_id: &str, phase: RunPhase) -> Result<Value, ServiceError> {
        let run = self.active.lock().await.get(app_id).cloned();
        self.stop_registered_chain(app_id, phase, run).await
    }

    async fn stop_registered_chain(
        &self,
        app_id: &str,
        phase: RunPhase,
        mut next: Option<Arc<ActiveRun>>,
    ) -> Result<Value, ServiceError> {
        let mut result = json!({"appId":app_id,"stopped":true});
        let mut errors = Vec::new();
        while let Some(run) = next {
            run.cancel.cancel();
            match self
                .stop_chain(app_id, run.clone(), phase.clone(), false)
                .await
            {
                Ok(value) => result = value,
                Err(error) => errors.push(error.to_string()),
            }
            next = self.active.lock().await.get(app_id).cloned();
            if next
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &run))
            {
                // A failed cleanup retains its owner for an explicit retry.
                return Err(ServiceError::new("stopFailed", errors.join("; ")));
            }
            // A cancellation watcher may have restored the previous run after
            // Stop took its snapshot. Recheck under the app transition lock.
        }
        if errors.is_empty() {
            Ok(result)
        } else {
            Err(ServiceError::new("stopFailed", errors.join("; ")))
        }
    }

    async fn stop_chain(
        &self,
        app_id: &str,
        run: Arc<ActiveRun>,
        phase: RunPhase,
        keep_registration: bool,
    ) -> Result<Value, ServiceError> {
        let mut chain = Vec::new();
        let mut next = Some(run.clone());
        while let Some(owned) = next {
            owned.cancel.cancel();
            next = owned.previous.lock().unwrap().clone();
            chain.push(owned);
        }
        let mut result = json!({"appId":app_id,"stopped":true});
        let mut errors = Vec::new();
        for owned in chain.iter().rev() {
            match self
                .stop_owned(app_id, owned.clone(), phase.clone(), true)
                .await
            {
                Ok(value) => result = value,
                Err(error) => errors.push(error.to_string()),
            }
        }
        if !errors.is_empty() {
            // Keep the complete ownership chain registered for a later retry,
            // but never let an old cleanup error leave the current run alive.
            let mut record = run.record.lock().unwrap();
            record.phase = RunPhase::Stopping;
            record.error = Some(errors.join("; "));
            return Err(ServiceError::new("stopFailed", errors.join("; ")));
        }
        for owned in &chain {
            owned.previous.lock().unwrap().take();
        }
        if !keep_registration {
            let mut active = self.active.lock().await;
            if active
                .get(app_id)
                .is_some_and(|current| Arc::ptr_eq(current, &run))
            {
                active.remove(app_id);
            }
        }
        Ok(result)
    }

    async fn stop_owned(
        &self,
        app_id: &str,
        run: Arc<ActiveRun>,
        phase: RunPhase,
        keep_registration: bool,
    ) -> Result<Value, ServiceError> {
        run.cancel.cancel();
        let _lifecycle = run.lifecycle.lock().await;
        self.stop_owned_locked(app_id, run.clone(), phase, keep_registration)
            .await
    }

    /// Caller holds this run's lifecycle lock. Kept separate for the dropped
    /// waiter, which must check the commit flag under that same lock.
    async fn stop_owned_locked(
        &self,
        app_id: &str,
        run: Arc<ActiveRun>,
        phase: RunPhase,
        keep_registration: bool,
    ) -> Result<Value, ServiceError> {
        run.cancel.cancel();
        if let Ok(agent) = self.agent_host() {
            agent
                .stop_app(app_id)
                .await
                .map_err(|message| ServiceError::new("agentCleanupIncomplete", message))?;
        }
        let mut stopping = run.record.lock().unwrap().clone();
        stopping.phase = RunPhase::Stopping;
        stopping.services.clear();
        *run.url.lock().unwrap() = String::new();
        *run.record.lock().unwrap() = stopping;

        if let Some(task) = run.gateway.lock().await.take() {
            let _ = task.await;
        }
        // Do not retain the fixed port through monitor/watcher Arc references.
        run.listener.lock().await.take();
        let mut handles = run.handles.lock().await;
        if !handles.is_empty() {
            let runtime = self.runtime()?;
            let mut errors = Vec::new();
            let mut pending = Vec::new();
            for handle in handles.drain(..).rev() {
                if let Err(error) = runtime.kill(&handle).await {
                    errors.push(error.to_string());
                    pending.push(handle);
                }
            }
            *handles = pending;
            if !errors.is_empty() {
                let message = errors.join("; ");
                let mut stopping = run.record.lock().unwrap().clone();
                stopping.error = Some(message.clone());
                stopping.services.clear();
                stopping.updated_at_ms = now_ms();
                *run.record.lock().unwrap() = stopping.clone();
                let library = self.library.clone();
                let _ = tokio::task::spawn_blocking(move || library.update_run(stopping)).await;
                return Err(ServiceError::new("stopFailed", message));
            }
        }
        drop(handles);
        // Process cancellation precedes its final stderr/exit events. Keep the
        // logger and every registered task identity until reaping completes;
        // a failed reap retains both for the explicit cleanup retry.
        run.log_stop.cancel();
        if let Some(task) = run.log_task.lock().await.take() {
            let _ = task.await;
        }
        run.execution_lease.lock().unwrap().take();
        run.cleanup_complete.store(true, Ordering::Release);
        let mut record = run.record.lock().unwrap().clone();
        record.phase = phase;
        record.updated_at_ms = now_ms();
        *run.record.lock().unwrap() = record.clone();
        let library = self.library.clone();
        let saved = record.clone();
        tokio::task::spawn_blocking(move || library.update_run(saved))
            .await
            .map_err(join_error)??;
        let mut active = self.active.lock().await;
        if !keep_registration
            && active
                .get(app_id)
                .is_some_and(|current| Arc::ptr_eq(current, &run))
        {
            active.remove(app_id);
        }
        Ok(json!({"run":record,"stopped":true}))
    }
}

#[cfg_attr(feature = "uniffi", uniffi::export(async_runtime = "tokio"))]
impl MobileLocalAppServiceHandle {
    /// Narrow page ABI. Native code must derive identity from its verified
    /// current main frame, origin and launch; never forward page-owned IDs.
    pub async fn agent_page_json(
        self: Arc<Self>,
        app_id: String,
        invocation_id: String,
        method: String,
        arguments: String,
    ) -> String {
        let host = match self.agent_host() {
            Ok(host) => host,
            Err(error) => return agent_envelope(Err(error)),
        };
        if arguments.len() > 116 * 1024 * 1024 {
            return agent_envelope(Err("agent_input_too_large".into()));
        }
        let args = match serde_json::from_str(&arguments) {
            Ok(args) => args,
            Err(_) => return agent_envelope(Err("invalid_agent_input".into())),
        };
        let result = EXECUTOR
            .get()
            .expect("app executor")
            .spawn(async move { host.page(&app_id, &invocation_id, &method, args).await })
            .await;
        agent_envelope(result.unwrap_or_else(|_| Err("agent_host_unavailable".into())))
    }
    /// Trusted native UI only. The page SDK has no route to this method.
    pub async fn agent_host_json(self: Arc<Self>, request: String) -> String {
        // Trusted iOS `startNative` carries a complete bounded AG-UI JSON
        // request, including inline DataSource base64 values.
        if request.len() > 116 * 1024 * 1024 {
            return agent_envelope(Err("agent_input_too_large".into()));
        }
        let request: Value = match serde_json::from_str(&request) {
            Ok(request) => request,
            Err(_) => return agent_envelope(Err("invalid_agent_input".into())),
        };
        if request.get("op").and_then(Value::as_str) == Some("registerAuthoring") {
            return match self.register_authoring(request).await {
                Ok(value) => json!({"ok":true,"value":value}).to_string(),
                Err(error) => {
                    json!({"ok":false,"error":{"code":error.code,"message":error.message}})
                        .to_string()
                }
            };
        }
        let host = match self.agent_host() {
            Ok(host) => host,
            Err(error) => return agent_envelope(Err(error)),
        };
        let result = EXECUTOR
            .get()
            .expect("app executor")
            .spawn(async move { host.host(request).await })
            .await;
        agent_envelope(result.unwrap_or_else(|_| Err("agent_host_unavailable".into())))
    }
    /// Invoked after a real page closes, not when it reloads or opens a picker.
    pub async fn agent_page_closed(
        self: Arc<Self>,
        app_id: String,
        invocation_id: String,
    ) -> String {
        let host = match self.agent_host() {
            Ok(host) => host,
            // Closing an ordinary page or a never-used Agent must not boot a
            // model host or fail the native WebView teardown.
            Err(error) if error == "agent_model_host_not_ready" => {
                return agent_envelope(Ok(Value::Null));
            }
            Err(error) => return agent_envelope(Err(error)),
        };
        let result = EXECUTOR
            .get()
            .expect("app executor")
            .spawn(async move {
                host.close_page(&app_id, &invocation_id)
                    .await
                    .map(|_| Value::Null)
            })
            .await;
        agent_envelope(result.unwrap_or_else(|_| Err("agent_host_unavailable".into())))
    }
    /// A stable JSON ABI keeps native bindings independent of evolving release
    /// structs. Invalid operations always produce an explicit error envelope.
    pub async fn request_json(self: Arc<Self>, request: String) -> String {
        let result = async {
            let request: Value = serde_json::from_str(&request)?;
            self.request_for_tool(request).await
        }
        .await;
        match result {
            Ok(value) => json!({"ok":true,"value":value}).to_string(),
            Err(error) => {
                json!({"ok":false,"error":{"code":error.code,"message":error.message}}).to_string()
            }
        }
    }
}
fn agent_envelope(result: Result<Value, String>) -> String {
    match result {
        Ok(value) => json!({"ok":true,"value":value}).to_string(),
        Err(code) => {
            let message = match code.as_str() {
                "agent_model_host_not_ready" | "agent_host_unavailable" => {
                    "The application model service is unavailable. Check model settings and retry."
                }
                "model_unavailable" => {
                    "Select an available model in this application's Agent settings."
                }
                "agent_not_declared" => "This application has not declared the Agent capability.",
                "tool_catalog_changed" => {
                    "The tool configuration changed. Review and authorize the current tools again."
                }
                "background_not_supported" => {
                    "Background Agent execution is not supported by this mobile host."
                }
                "agent_input_too_large" => {
                    "The request is too large. Reduce the image size or prompt length."
                }
                "agent_storage_unavailable" => {
                    "Agent task storage is unavailable. Existing task data has been preserved."
                }
                "agent_protocol_store_marker_missing" => {
                    "This App has existing Agent data with a missing protocol marker. The existing data was preserved. Seek help before modifying it."
                }
                "agent_protocol_store_version_mismatch" => {
                    "This App's Agent data has an unsupported or invalid protocol marker. The existing data was preserved. Seek help before modifying it."
                }
                "chat_storage_uncertain" => {
                    "This App's Chat storage could not be verified. Keep the existing data and seek help before trying to modify it."
                }
                "app_invocation_closed" | "stale_app_invocation" => {
                    "This application run has closed. Reopen the application and retry."
                }
                "invalid_agent_input" => "The Agent request is invalid.",
                _ => local_app::v2::agent::error_message(&code),
            };
            json!({"ok":false,"error":{"code":code,"message":message}}).to_string()
        }
    }
}
fn required<'a>(value: &'a Value, key: &str) -> Result<&'a str, ServiceError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            ServiceError::new("invalidRequest", format!("{key} must be a nonempty string"))
        })
}
fn join_error(error: tokio::task::JoinError) -> ServiceError {
    ServiceError::new("internalFailure", error.to_string())
}
pub(super) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod agent_envelope_tests {
    use super::agent_envelope;
    use serde_json::Value;

    #[test]
    fn unresolved_chat_tool_has_actionable_safe_error_message() {
        let response: Value =
            serde_json::from_str(&agent_envelope(Err("chat_deferred_tools_pending".into())))
                .unwrap();
        assert_eq!(response["ok"], false);
        assert_eq!(response["error"]["code"], "chat_deferred_tools_pending");
        let message = response["error"]["message"].as_str().unwrap();
        assert_eq!(
            message,
            local_app::v2::agent::error_message("chat_deferred_tools_pending")
        );
        assert!(message.contains("unfinished tool operation"));
        assert!(message.contains("Review its effects"));
        assert!(message.contains("not be replayed automatically"));
    }

    #[test]
    fn app_storage_errors_distinguish_missing_index_from_corruption() {
        for (code, phrase) in [
            ("chat_storage_uncertain", "could not be verified"),
            (
                "agent_protocol_store_marker_missing",
                "missing protocol marker",
            ),
            (
                "agent_protocol_store_version_mismatch",
                "unsupported or invalid protocol marker",
            ),
        ] {
            let response: Value = serde_json::from_str(&agent_envelope(Err(code.into()))).unwrap();
            assert_eq!(response["error"]["code"], code);
            let message = response["error"]["message"].as_str().unwrap();
            assert!(message.contains(phrase));
            assert!(!message.contains("delete"));
        }
    }
}

#[cfg(test)]
mod process_tests;
