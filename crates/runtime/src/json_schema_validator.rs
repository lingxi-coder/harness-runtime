//! Long-lived Bun/Ajv implementation of Core's source JSON Schema host seam.
//!
//! The worker receives schemas and instances only as flat DTOs made from Core
//! projections. Rust never normalizes arbitrary source payloads through a
//! recursive `serde_json::Value` or JSON text.

#[path = "source_value_dto.rs"]
mod source_value_dto;

use lingxi_core::host::json_schema_validator::{
    CompiledJsonSchema, JsonSchemaValidation, JsonSchemaValidator, JsonSchemaValidatorSession,
    SchemaCompileError, SchemaProfile, SchemaWorkerError,
};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use serde::Serialize;
use serde_json::Value;
use std::io::Write as _;
use std::mem;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex, OnceCell};

use self::source_value_dto::SourceValueDto;

const WORKER_SOURCE: &str = include_str!("json_schema_worker.mjs");
const AJV_SOURCE: &str = include_str!("ajv-8.18.0-profiles.bundle.mjs");
const REQUEST_BUDGET: Duration = Duration::from_secs(20);
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
static NEXT_BOOTSTRAP_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_VALIDATOR_SESSION_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_WORKER_GENERATION: AtomicU64 = AtomicU64::new(1);

/// Host-owned persistent Bun worker for profile-specific Ajv sessions.
pub struct BunJsonSchemaValidator {
    inner: Arc<WorkerInner>,
}

struct WorkerInner {
    executable: PathBuf,
    worker: Mutex<Option<WorkerProcess>>,
    pending_session_releases: StdMutex<Vec<String>>,
    // Drop the worker before removing its private entry/dependency files.
    bootstrap: OnceCell<Arc<WorkerBootstrap>>,
    next_request_id: AtomicU64,
    next_schema_id: AtomicU64,
}

struct WorkerBootstrap {
    directory: PathBuf,
    entry: PathBuf,
}

struct WorkerProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
    generation: u64,
}

#[derive(Serialize)]
struct CompileRequest<'a> {
    id: &'a str,
    kind: &'static str,
    #[serde(rename = "sessionId")]
    session_id: &'a str,
    #[serde(rename = "schemaId")]
    schema_id: &'a str,
    profile: SchemaProfileWire,
    schema: SourceValueDto,
}

#[derive(Serialize)]
struct ValidateRequest<'a> {
    id: &'a str,
    kind: &'static str,
    #[serde(rename = "schemaId")]
    schema_id: &'a str,
    input: SourceValueDto,
}

#[derive(Serialize)]
struct ReleaseSessionRequest<'a> {
    id: &'a str,
    kind: &'static str,
    #[serde(rename = "sessionId")]
    session_id: &'a str,
}

#[derive(Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum SchemaProfileWire {
    StructuredOutputDraft7,
    McpOutputSchema2020,
}

impl From<SchemaProfile> for SchemaProfileWire {
    fn from(value: SchemaProfile) -> Self {
        match value {
            SchemaProfile::StructuredOutputDraft7 => Self::StructuredOutputDraft7,
            SchemaProfile::McpOutputSchema2020 => Self::McpOutputSchema2020,
        }
    }
}

struct SchemaRecord {
    schema_id: String,
    schema: Utf16JsonProjection,
}

#[derive(Default)]
struct SessionState {
    schemas: Vec<SchemaRecord>,
    worker_generation: Option<u64>,
}

struct BunJsonSchemaSession {
    lease: Arc<BunSessionLease>,
}

struct BunSessionLease {
    inner: Arc<WorkerInner>,
    session_id: String,
    profile: SchemaProfileWire,
    state: Mutex<SessionState>,
}

struct BunCompiledJsonSchema {
    session: Arc<BunSessionLease>,
    schema_id: String,
}

impl BunJsonSchemaValidator {
    /// Construct a lazy validator using the host's configured Bun executable.
    ///
    /// The executable is resolved from `LINGXI_MOD_BUN_EXECUTABLE`, then the
    /// `bun` PATH command used by the ModHost developer profile. The Bun process
    /// is not spawned until the first schema compilation.
    pub fn new_from_host_environment() -> Arc<Self> {
        let executable = std::env::var_os(branding::MOD_BUN_EXECUTABLE_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("bun"));
        Self::new(executable)
    }

    /// Construct a lazy validator using an explicitly resolved Bun executable.
    pub fn new(executable: impl Into<PathBuf>) -> Arc<Self> {
        Arc::new(Self {
            inner: Arc::new(WorkerInner {
                executable: executable.into(),
                worker: Mutex::new(None),
                pending_session_releases: StdMutex::new(Vec::new()),
                bootstrap: OnceCell::new(),
                next_request_id: AtomicU64::new(1),
                next_schema_id: AtomicU64::new(1),
            }),
        })
    }

    /// Eagerly start the worker when a host needs startup failure to be
    /// reported immediately. Normal assembly uses [`Self::new`].
    pub async fn start(executable: impl Into<PathBuf>) -> Result<Arc<Self>, String> {
        let validator = Self::new(executable);
        validator.inner.ensure_started().await?;
        Ok(validator)
    }
}

impl JsonSchemaValidator for BunJsonSchemaValidator {
    fn new_session(&self, profile: SchemaProfile) -> Arc<dyn JsonSchemaValidatorSession> {
        let serial = NEXT_VALIDATOR_SESSION_ID.fetch_add(1, Ordering::Relaxed);
        Arc::new(BunJsonSchemaSession {
            lease: Arc::new(BunSessionLease {
                inner: Arc::clone(&self.inner),
                session_id: format!("{}-{serial}", std::process::id()),
                profile: SchemaProfileWire::from(profile),
                state: Mutex::new(SessionState::default()),
            }),
        })
    }
}

#[async_trait::async_trait]
impl JsonSchemaValidatorSession for BunJsonSchemaSession {
    async fn compile(
        &self,
        schema: &Utf16JsonProjection,
    ) -> Result<Arc<dyn CompiledJsonSchema>, SchemaCompileError> {
        self.lease.compile(schema).await
    }
}

impl BunSessionLease {
    async fn compile(
        self: &Arc<Self>,
        schema: &Utf16JsonProjection,
    ) -> Result<Arc<dyn CompiledJsonSchema>, SchemaCompileError> {
        let mut state = self.state.lock().await;
        let schema_id = self
            .inner
            .next_id(&self.inner.next_schema_id)
            .map_err(SchemaCompileError::Worker)?;

        loop {
            let generation = self.ensure_current_generation(&mut state).await?;
            let request_id = self
                .inner
                .next_id(&self.inner.next_request_id)
                .map_err(SchemaCompileError::Worker)?;
            let request = CompileRequest {
                id: &request_id,
                kind: "compile",
                session_id: &self.session_id,
                schema_id: &schema_id,
                profile: self.profile,
                schema: SourceValueDto::from_projection(schema)
                    .map_err(|error| SchemaCompileError::Worker(error.to_string()))?,
            };
            let reply = self
                .inner
                .exchange(&request, &request_id)
                .await
                .map_err(SchemaCompileError::Worker)?;
            match reply.get("kind").and_then(Value::as_str) {
                Some("compiled") => {
                    let current = self
                        .inner
                        .ensure_started()
                        .await
                        .map_err(SchemaCompileError::Worker)?;
                    if current != generation {
                        state.worker_generation = None;
                        continue;
                    }
                    state.schemas.push(SchemaRecord {
                        schema_id: schema_id.clone(),
                        schema: schema.clone(),
                    });
                    state.worker_generation = Some(generation);
                    return Ok(Arc::new(BunCompiledJsonSchema {
                        session: Arc::clone(self),
                        schema_id,
                    }));
                }
                Some("schema_error") => {
                    return Err(SchemaCompileError::InvalidSchema(
                        exact_detail(&reply).map_err(SchemaCompileError::Worker)?,
                    ));
                }
                Some("worker_error") => {
                    return Err(SchemaCompileError::Worker(
                        detail_display(&reply).map_err(SchemaCompileError::Worker)?,
                    ));
                }
                _ => {
                    return Err(SchemaCompileError::Worker(
                        "unexpected response while compiling schema".into(),
                    ));
                }
            }
        }
    }
}

impl BunSessionLease {
    async fn ensure_current_generation(
        &self,
        state: &mut SessionState,
    ) -> Result<u64, SchemaCompileError> {
        loop {
            let generation = self
                .inner
                .ensure_started()
                .await
                .map_err(SchemaCompileError::Worker)?;
            if state.worker_generation == Some(generation) {
                return Ok(generation);
            }

            let mut replay_changed_generation = false;
            for record in &state.schemas {
                let request_id = self
                    .inner
                    .next_id(&self.inner.next_request_id)
                    .map_err(SchemaCompileError::Worker)?;
                let request = CompileRequest {
                    id: &request_id,
                    kind: "compile",
                    session_id: &self.session_id,
                    schema_id: &record.schema_id,
                    profile: self.profile,
                    schema: SourceValueDto::from_projection(&record.schema)
                        .map_err(|error| SchemaCompileError::Worker(error.to_string()))?,
                };
                let reply = self
                    .inner
                    .exchange(&request, &request_id)
                    .await
                    .map_err(SchemaCompileError::Worker)?;
                if reply.get("kind").and_then(Value::as_str) != Some("compiled") {
                    return Err(SchemaCompileError::Worker(
                        detail_display(&reply)
                            .unwrap_or_else(|_| "could not restore validator session".into()),
                    ));
                }
                if self
                    .inner
                    .ensure_started()
                    .await
                    .map_err(SchemaCompileError::Worker)?
                    != generation
                {
                    replay_changed_generation = true;
                    break;
                }
            }

            if !replay_changed_generation
                && self
                    .inner
                    .ensure_started()
                    .await
                    .map_err(SchemaCompileError::Worker)?
                    == generation
            {
                state.worker_generation = Some(generation);
                return Ok(generation);
            }
            state.worker_generation = None;
        }
    }
}

#[async_trait::async_trait]
impl CompiledJsonSchema for BunCompiledJsonSchema {
    async fn validate(
        &self,
        input: &Utf16JsonProjection,
    ) -> Result<JsonSchemaValidation, SchemaWorkerError> {
        let mut state = self.session.state.lock().await;
        let generation = self
            .session
            .ensure_current_generation(&mut state)
            .await
            .map_err(schema_compile_error_to_worker)?;
        let mut reply = self.validate_once(input).await?;
        if reply.get("kind").and_then(Value::as_str) == Some("worker_error")
            && matches!(detail_display(&reply), Ok(detail) if detail == "compiled schema handle is unknown")
        {
            state.worker_generation = None;
            let _ = self
                .session
                .ensure_current_generation(&mut state)
                .await
                .map_err(schema_compile_error_to_worker)?;
            reply = self.validate_once(input).await?;
        }
        if self
            .session
            .inner
            .ensure_started()
            .await
            .map_err(SchemaWorkerError::Unavailable)?
            != generation
        {
            state.worker_generation = None;
            let _ = self
                .session
                .ensure_current_generation(&mut state)
                .await
                .map_err(schema_compile_error_to_worker)?;
            reply = self.validate_once(input).await?;
        }
        parse_validation_reply(reply)
    }
}

impl BunCompiledJsonSchema {
    async fn validate_once(&self, input: &Utf16JsonProjection) -> Result<Value, SchemaWorkerError> {
        let request_id = self
            .session
            .inner
            .next_id(&self.session.inner.next_request_id)
            .map_err(SchemaWorkerError::Protocol)?;
        let request = ValidateRequest {
            id: &request_id,
            kind: "validate",
            schema_id: &self.schema_id,
            input: SourceValueDto::from_projection(input)
                .map_err(|error| SchemaWorkerError::Protocol(error.to_string()))?,
        };
        self.session
            .inner
            .exchange(&request, &request_id)
            .await
            .map_err(SchemaWorkerError::Unavailable)
    }
}

fn parse_validation_reply(reply: Value) -> Result<JsonSchemaValidation, SchemaWorkerError> {
    match reply.get("kind").and_then(Value::as_str) {
        Some("valid") => Ok(JsonSchemaValidation::Valid),
        Some("invalid") => {
            let detail = exact_detail(&reply).map_err(SchemaWorkerError::Protocol)?;
            let keywords = reply
                .get("keywords")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .split(',')
                .filter(|keyword| !keyword.is_empty())
                .map(str::to_owned)
                .collect();
            Ok(JsonSchemaValidation::Invalid { detail, keywords })
        }
        Some("worker_error") => Err(SchemaWorkerError::Unavailable(
            detail_display(&reply).unwrap_or_else(|_| "worker rejected the request".into()),
        )),
        _ => Err(SchemaWorkerError::Protocol(
            "unexpected validation response".into(),
        )),
    }
}

fn schema_compile_error_to_worker(error: SchemaCompileError) -> SchemaWorkerError {
    match error {
        SchemaCompileError::InvalidSchema(detail) => SchemaWorkerError::Protocol(
            detail
                .string_at("")
                .unwrap_or("compiled schema became invalid")
                .to_owned(),
        ),
        SchemaCompileError::Worker(detail) => SchemaWorkerError::Unavailable(detail),
    }
}

impl Drop for BunSessionLease {
    fn drop(&mut self) {
        self.inner.queue_session_release(self.session_id.clone());
    }
}

impl WorkerInner {
    async fn ensure_started(&self) -> Result<u64, String> {
        let mut worker = self.worker.lock().await;
        Self::retire_exited_worker(&mut worker)?;
        if worker.is_none() {
            let bootstrap = self.bootstrap().await?;
            *worker = Some(spawn_worker(&self.executable, &bootstrap.entry).await?);
        }
        Ok(worker.as_ref().expect("worker started").generation)
    }

    fn retire_exited_worker(worker: &mut Option<WorkerProcess>) -> Result<(), String> {
        let Some(current) = worker.as_mut() else {
            return Ok(());
        };
        match current.child.try_wait() {
            Ok(None) => Ok(()),
            Ok(Some(_status)) => {
                worker.take();
                Ok(())
            }
            Err(error) => {
                worker.take();
                Err(format!("inspect Bun JSON Schema worker process: {error}"))
            }
        }
    }

    async fn ensure_started_in_slot(
        &self,
        worker: &mut Option<WorkerProcess>,
    ) -> Result<(), String> {
        Self::retire_exited_worker(worker)?;
        if worker.is_none() {
            let bootstrap = self.bootstrap().await?;
            *worker = Some(spawn_worker(&self.executable, &bootstrap.entry).await?);
        }
        Ok(())
    }

    async fn bootstrap(&self) -> Result<Arc<WorkerBootstrap>, String> {
        self.bootstrap
            .get_or_try_init(|| async { WorkerBootstrap::create().map(Arc::new) })
            .await
            .cloned()
    }

    fn next_id(&self, source: &AtomicU64) -> Result<String, String> {
        source
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .map(|value| value.to_string())
            .map_err(|_| "JSON Schema worker request id space exhausted".into())
    }

    async fn exchange<T: Serialize>(
        &self,
        request: &T,
        expected_id: &str,
    ) -> Result<Value, String> {
        self.exchange_inner(request, expected_id).await
    }

    async fn exchange_inner<T: Serialize>(
        &self,
        request: &T,
        expected_id: &str,
    ) -> Result<Value, String> {
        let mut slot = self.worker.lock().await;
        self.ensure_started_in_slot(&mut slot).await?;
        let mut exchange_guard = WorkerExchangeGuard::new(&mut slot);

        let mut pending = PendingSessionReleases::new(self, self.take_pending_session_releases());
        while let Some(session_id) = pending.next_session_id().map(str::to_owned) {
            let request_id = self.next_id(&self.next_request_id)?;
            let release = ReleaseSessionRequest {
                id: &request_id,
                kind: "release_session",
                session_id: &session_id,
            };
            let reply = exchange_worker_request(&mut exchange_guard, &release, &request_id).await?;
            if reply.get("kind").and_then(Value::as_str) != Some("session_released") {
                return Err("Bun worker did not acknowledge session release".into());
            }
            pending.mark_released();
        }
        let reply = exchange_worker_request(&mut exchange_guard, request, expected_id).await?;
        exchange_guard.disarm();
        Ok(reply)
    }

    fn queue_session_release(&self, session_id: String) {
        self.pending_session_releases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(session_id);
    }

    fn take_pending_session_releases(&self) -> Vec<String> {
        mem::take(
            &mut *self
                .pending_session_releases
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    fn requeue_session_releases(&self, mut session_ids: Vec<String>) {
        if session_ids.is_empty() {
            return;
        }
        let mut pending = self
            .pending_session_releases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        session_ids.append(&mut *pending);
        *pending = session_ids;
    }
}

struct WorkerExchangeGuard<'a> {
    slot: &'a mut Option<WorkerProcess>,
    armed: bool,
}

impl<'a> WorkerExchangeGuard<'a> {
    fn new(slot: &'a mut Option<WorkerProcess>) -> Self {
        Self { slot, armed: true }
    }

    fn worker(&mut self) -> &mut WorkerProcess {
        self.slot.as_mut().expect("worker exchange has a process")
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for WorkerExchangeGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.slot.take();
        }
    }
}

struct PendingSessionReleases<'a> {
    inner: &'a WorkerInner,
    session_ids: Vec<String>,
    released: usize,
}

impl<'a> PendingSessionReleases<'a> {
    fn new(inner: &'a WorkerInner, session_ids: Vec<String>) -> Self {
        Self {
            inner,
            session_ids,
            released: 0,
        }
    }

    fn next_session_id(&self) -> Option<&str> {
        self.session_ids.get(self.released).map(String::as_str)
    }

    fn mark_released(&mut self) {
        self.released += 1;
    }
}

impl Drop for PendingSessionReleases<'_> {
    fn drop(&mut self) {
        if self.released < self.session_ids.len() {
            self.inner
                .requeue_session_releases(self.session_ids[self.released..].to_vec());
        }
    }
}

async fn exchange_worker_request<T: Serialize>(
    guard: &mut WorkerExchangeGuard<'_>,
    request: &T,
    expected_id: &str,
) -> Result<Value, String> {
    let mut bytes =
        serde_json::to_vec(request).map_err(|error| format!("encode worker request: {error}"))?;
    bytes.push(b'\n');
    let line = match tokio::time::timeout(
        REQUEST_BUDGET,
        send_and_read_worker_line(guard.worker(), &bytes),
    )
    .await
    {
        Ok(Ok(line)) => line,
        Ok(Err(error)) => return Err(format!("Bun worker I/O failed: {error}")),
        Err(_) => {
            return Err(format!(
                "Bun worker request exceeded {} ms",
                REQUEST_BUDGET.as_millis()
            ));
        }
    };
    let reply: Value = serde_json::from_str(&line)
        .map_err(|error| format!("decode Bun worker response: {error}"))?;
    if reply.get("id").and_then(Value::as_str) != Some(expected_id) {
        return Err("Bun worker response id did not match request".into());
    }
    Ok(reply)
}

async fn send_and_read_worker_line(
    worker: &mut WorkerProcess,
    bytes: &[u8],
) -> std::io::Result<String> {
    worker.stdin.write_all(bytes).await?;
    worker.stdin.flush().await?;
    let line =
        worker.stdout.next_line().await?.ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "worker exited")
        })?;
    if line.len() > MAX_RESPONSE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "JSON Schema worker response exceeded the size limit",
        ));
    }
    Ok(line)
}

impl WorkerBootstrap {
    fn create() -> Result<Self, String> {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| format!("system clock is before Unix epoch: {error}"))?
            .as_nanos();
        let mut directory = None;
        for _ in 0..64 {
            let id = NEXT_BOOTSTRAP_ID.fetch_add(1, Ordering::Relaxed);
            let candidate = std::env::temp_dir().join(format!(
                "lingxi-json-schema-{}-{timestamp}-{id}",
                std::process::id()
            ));
            let mut builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt as _;
                builder.mode(0o700);
            }
            match builder.create(&candidate) {
                Ok(()) => {
                    directory = Some(candidate);
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(format!("create private Bun worker directory: {error}")),
            }
        }
        let directory = directory
            .ok_or_else(|| "could not allocate a private Bun worker directory".to_owned())?;
        let entry = directory.join("json_schema_worker.mjs");
        let ajv = directory.join("ajv-8.18.0-profiles.bundle.mjs");
        let result =
            write_source(&entry, WORKER_SOURCE).and_then(|()| write_source(&ajv, AJV_SOURCE));
        if let Err(error) = result {
            let _ = std::fs::remove_dir_all(&directory);
            return Err(error);
        }
        Ok(Self { directory, entry })
    }
}

impl Drop for WorkerBootstrap {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.directory) {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(path = %self.directory.display(), %error, "failed to remove owned JSON Schema worker directory");
            }
        }
    }
}

fn write_source(path: &Path, source: &str) -> Result<(), String> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| format!("create Bun worker file {}: {error}", path.display()))?;
    file.write_all(source.as_bytes())
        .map_err(|error| format!("write Bun worker file {}: {error}", path.display()))
}

async fn spawn_worker(executable: &Path, entry: &Path) -> Result<WorkerProcess, String> {
    let mut child = Command::new(executable)
        .arg("--no-warnings")
        .arg(entry)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("start Bun JSON Schema worker: {error}"))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| "Bun worker stdin was unavailable".to_owned())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Bun worker stdout was unavailable".to_owned())?;
    Ok(WorkerProcess {
        child,
        stdin,
        stdout: BufReader::new(stdout).lines(),
        generation: NEXT_WORKER_GENERATION.fetch_add(1, Ordering::Relaxed),
    })
}

fn exact_detail(reply: &Value) -> Result<Utf16JsonProjection, String> {
    let detail = reply
        .get("detail")
        .ok_or_else(|| "Bun worker response omitted its exact detail".to_owned())?;
    let display = detail
        .get("display")
        .and_then(Value::as_str)
        .ok_or_else(|| "Bun worker detail display is malformed".to_owned())?;
    let code_units = detail
        .get("codeUnits")
        .and_then(Value::as_array)
        .ok_or_else(|| "Bun worker detail units are malformed".to_owned())?
        .iter()
        .map(|unit| {
            unit.as_u64()
                .and_then(|value| u16::try_from(value).ok())
                .ok_or_else(|| "Bun worker detail contains an invalid UTF-16 unit".to_owned())
        })
        .collect::<Result<Vec<_>, _>>()?;
    Utf16JsonProjection::root_string(display.to_owned(), code_units)
        .map_err(|error| format!("invalid exact Bun worker detail: {error}"))
}

fn detail_display(reply: &Value) -> Result<String, String> {
    exact_detail(reply).map(|detail| {
        detail
            .string_at("")
            .expect("root_string always has a string root")
            .to_owned()
    })
}

#[cfg(all(test, unix))]
mod lifecycle_tests {
    use super::*;
    use serde::Serialize;
    use std::os::unix::fs::PermissionsExt as _;
    use std::time::Duration;

    #[derive(Serialize)]
    struct TestRequest<'a> {
        id: &'a str,
        kind: &'a str,
    }

    struct FakeWorkerFiles {
        directory: PathBuf,
        executable: PathBuf,
        log: PathBuf,
    }

    impl FakeWorkerFiles {
        fn create() -> Self {
            let directory = std::env::temp_dir().join(format!(
                "lingxi-json-schema-worker-test-{}-{}",
                std::process::id(),
                NEXT_BOOTSTRAP_ID.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&directory).expect("create fake worker directory");
            let executable = directory.join("fake-bun");
            let log = directory.join("requests.log");
            let log_path = shell_quote(&log.to_string_lossy());
            let script = format!(
                r#"#!/bin/sh
while IFS= read -r line; do
  printf '%s\n' "$line" >> {log_path}
  id=$(printf '%s\n' "$line" | /usr/bin/sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  case "$line" in
    *'"kind":"release_session"'*) kind=session_released ;;
    *'"kind":"wait"'*) IFS= read -r ignored || exit 0; kind=ok ;;
    *) kind=ok ;;
  esac
  printf '{{"id":"%s","kind":"%s"}}\n' "$id" "$kind"
  case "$line" in *'"kind":"exit"'*) exit 0 ;; esac
done
"#,
                log_path = log_path,
            );
            std::fs::write(&executable, script).expect("write fake worker executable");
            let mut permissions = std::fs::metadata(&executable)
                .expect("fake worker metadata")
                .permissions();
            permissions.set_mode(0o700);
            std::fs::set_permissions(&executable, permissions)
                .expect("make fake worker executable");
            Self {
                directory,
                executable,
                log,
            }
        }

        fn request_lines(&self) -> Vec<String> {
            std::fs::read_to_string(&self.log)
                .unwrap_or_default()
                .lines()
                .map(str::to_owned)
                .collect()
        }
    }

    impl Drop for FakeWorkerFiles {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }

    fn shell_quote(value: &str) -> String {
        format!("'{}'", value.replace('\'', "'\\''"))
    }

    #[tokio::test]
    async fn dropped_session_is_queued_without_runtime_and_drained_before_next_exchange() {
        let files = FakeWorkerFiles::create();
        let validator = BunJsonSchemaValidator::new(&files.executable);
        let inner = Arc::clone(&validator.inner);
        let lease = Arc::new(BunSessionLease {
            inner: Arc::clone(&inner),
            session_id: "dropped-session".into(),
            profile: SchemaProfileWire::StructuredOutputDraft7,
            state: Mutex::new(SessionState::default()),
        });

        std::thread::spawn(move || drop(lease))
            .join()
            .expect("drop session outside Tokio runtime");
        assert!(inner.worker.lock().await.is_none());
        let queued = inner
            .pending_session_releases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(queued.as_slice(), &[String::from("dropped-session")]);
        drop(queued);

        let reply = inner
            .exchange(
                &TestRequest {
                    id: "request-1",
                    kind: "call",
                },
                "request-1",
            )
            .await
            .expect("real exchange drains the queued release");
        assert_eq!(reply.get("kind").and_then(Value::as_str), Some("ok"));
        let lines = files.request_lines();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"kind\":\"release_session\""));
        assert!(lines[0].contains("\"sessionId\":\"dropped-session\""));
        assert!(lines[1].contains("\"kind\":\"call\""));

        drop(validator);
    }

    #[tokio::test]
    async fn cancelled_exchange_retires_worker_before_the_next_request() {
        let files = FakeWorkerFiles::create();
        let validator = BunJsonSchemaValidator::new(&files.executable);
        let inner = Arc::clone(&validator.inner);
        let request = TestRequest {
            id: "request-hold",
            kind: "wait",
        };
        let exchange = tokio::spawn(async move { inner.exchange(&request, "request-hold").await });

        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if !files.request_lines().is_empty() {
                break;
            }
            if exchange.is_finished() {
                panic!(
                    "fake worker exchange ended before readiness: {:?}",
                    exchange.await
                );
            }
            if tokio::time::Instant::now() >= deadline {
                exchange.abort();
                let _ = exchange.await;
                panic!("fake worker did not report readiness within 10 seconds");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            files.request_lines().len(),
            1,
            "fake worker received request"
        );
        exchange.abort();
        let _ = exchange.await;
        assert!(validator.inner.worker.lock().await.is_none());

        let reply = validator
            .inner
            .exchange(
                &TestRequest {
                    id: "request-after-cancel",
                    kind: "call",
                },
                "request-after-cancel",
            )
            .await
            .expect("next exchange starts a fresh worker");
        assert_eq!(reply.get("kind").and_then(Value::as_str), Some("ok"));
        assert_eq!(files.request_lines().len(), 2);

        drop(validator);
    }

    #[tokio::test]
    async fn ensure_started_replaces_an_exited_worker() {
        let files = FakeWorkerFiles::create();
        let validator = BunJsonSchemaValidator::new(&files.executable);
        let first_generation = validator
            .inner
            .ensure_started()
            .await
            .expect("start first worker");
        validator
            .inner
            .exchange(
                &TestRequest {
                    id: "request-exit",
                    kind: "exit",
                },
                "request-exit",
            )
            .await
            .expect("receive reply before worker exits");

        for _ in 0..100 {
            let generation = validator
                .inner
                .ensure_started()
                .await
                .expect("restart exited worker");
            if generation != first_generation {
                drop(validator);
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("ensure_started retained an exited worker generation");
    }
}

#[cfg(test)]
mod native_draft7_semantics_tests {
    use super::*;

    fn projection(json: &str) -> Utf16JsonProjection {
        Utf16JsonProjection::parse(json).expect("valid source JSON fixture")
    }

    async fn compile(
        session: &Arc<dyn JsonSchemaValidatorSession>,
        schema: &str,
    ) -> Arc<dyn CompiledJsonSchema> {
        session
            .compile(&projection(schema))
            .await
            .expect("Native Draft-07 schema compiles")
    }

    async fn validate(compiled: &Arc<dyn CompiledJsonSchema>, input: &str) -> JsonSchemaValidation {
        compiled
            .validate(&projection(input))
            .await
            .expect("Bun worker validates the source input")
    }

    #[tokio::test]
    async fn draft7_all_of_and_local_refs_keep_native_error_order() {
        let validator = BunJsonSchemaValidator::new_from_host_environment();
        let session = validator.new_session(SchemaProfile::StructuredOutputDraft7);
        let schema = compile(
            &session,
            r##"{"type":"object","definitions":{"number":{"type":"integer","minimum":5}},"allOf":[{"required":["x"]},{"properties":{"a":{"$ref":"#/definitions/number"}}}],"additionalProperties":false}"##,
        )
        .await;

        let JsonSchemaValidation::Invalid { detail, keywords } =
            validate(&schema, r#"{"a":2,"b":4}"#).await
        else {
            panic!("invalid allOf instance was accepted")
        };
        assert_eq!(
            keywords,
            [
                "required",
                "minimum",
                "additionalProperties",
                "additionalProperties"
            ]
        );
        assert_eq!(
            detail.string_at(""),
            Some("root: must have required property 'x', /a: must be >= 5, root: must NOT have additional properties ('a' is not allowed), root: must NOT have additional properties ('b' is not allowed)")
        );
    }

    #[tokio::test]
    async fn draft7_ajv_rule_order_unions_dependencies_and_conditions_are_retained() {
        let validator = BunJsonSchemaValidator::new_from_host_environment();
        let session = validator.new_session(SchemaProfile::StructuredOutputDraft7);

        let property_schema = compile(
            &session,
            r#"{"type":"object","maxProperties":1,"minProperties":5,"required":["x"],"properties":{"a":{"type":"integer"}},"additionalProperties":false}"#,
        )
        .await;
        let JsonSchemaValidation::Invalid { detail, keywords } =
            validate(&property_schema, r#"{"a":"bad","b":2}"#).await
        else {
            panic!("invalid property instance was accepted")
        };
        assert_eq!(
            keywords,
            [
                "maxProperties",
                "minProperties",
                "required",
                "additionalProperties",
                "type"
            ]
        );
        assert_eq!(
            detail.string_at(""),
            Some("root: must NOT have more than 1 properties (got 2), root: must NOT have fewer than 5 properties (got 2), root: must have required property 'x', root: must NOT have additional properties ('b' is not allowed), /a: must be integer")
        );

        let union_schema = compile(
            &session,
            r#"{"type":"object","properties":{"x":{"oneOf":[{"type":"string"},{"type":"number"}]},"y":{"anyOf":[{"type":"integer","minimum":2},{"type":"string","minLength":3}]}},"dependencies":{"a":["b","c"]},"propertyNames":{"pattern":"^[a-z]+$"}}"#,
        )
        .await;
        let JsonSchemaValidation::Invalid { detail, keywords } =
            validate(&union_schema, r#"{"x":true,"y":false,"a":1,"BAD":2}"#).await
        else {
            panic!("invalid union/dependency instance was accepted")
        };
        assert_eq!(
            keywords,
            [
                "pattern",
                "propertyNames",
                "dependencies",
                "dependencies",
                "type",
                "type",
                "oneOf",
                "type",
                "type",
                "anyOf"
            ]
        );
        assert_eq!(
            detail.string_at(""),
            Some("root: must match pattern \"^[a-z]+$\", root: property name must be valid, root: must have properties b, c when property a is present, root: must have properties b, c when property a is present, /x: must be string, /x: must be number, /x: must match exactly one schema in oneOf, /y: must be integer, /y: must be string, /y: must match a schema in anyOf")
        );

        let conditional_schema = compile(
            &session,
            r#"{"if":{"required":["a"]},"then":{"required":["b"]}}"#,
        )
        .await;
        let JsonSchemaValidation::Invalid { detail, keywords } =
            validate(&conditional_schema, r#"{"a":1}"#).await
        else {
            panic!("invalid conditional instance was accepted")
        };
        assert_eq!(keywords, ["required", "if"]);
        assert_eq!(
            detail.string_at(""),
            Some("root: must have required property 'b', root: must match \"then\" schema")
        );
    }

    #[tokio::test]
    async fn draft7_required_surrogates_and_enum_replacement_values_keep_distinct_identity() {
        let validator = BunJsonSchemaValidator::new_from_host_environment();
        let session = validator.new_session(SchemaProfile::StructuredOutputDraft7);
        let schema = compile(
            &session,
            r#"{"type":"object","required":["\ud800"],"properties":{"x":{"enum":["�"]}}}"#,
        )
        .await;

        let JsonSchemaValidation::Invalid { detail, keywords } =
            validate(&schema, r#"{"x":"\ud800"}"#).await
        else {
            panic!("the unpaired source value was conflated with U+FFFD")
        };
        assert_eq!(keywords, ["required", "enum"]);
        assert!(detail.string_units("").unwrap().contains(&0xd800));
        assert!(detail.string_at("").unwrap().contains("[\"�\"]"));

        assert_eq!(
            validate(&schema, r#"{"x":"�","\ud800":true}"#).await,
            JsonSchemaValidation::Valid,
            "the exact required key and the distinct replacement character are both accepted"
        );
    }
}
