//! Process-owned shell supervision for acknowledged cross-host adoption.
//! Only CLI hosts that install the bootstrap explicitly enable this path.
use crate::host::process::ShellProcessHandoff;
use crate::host::{
    BackgroundExitSink, BackgroundTaskBinding, ForegroundOutcome, ForegroundRunResult,
    ProcessCommand, ProcessHandle, ProcessRunner, SandboxedCommand, SandboxedTag,
};
use mobile_linux_api::ProcessError;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

use tokio_util::sync::CancellationToken;

/// OS-specific capabilities; the protocol never imports a concrete platform.
pub trait Stream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + Sync {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + Sync> Stream for T {}
pub type BoxStream = Box<dyn Stream>;
#[async_trait::async_trait]
pub trait Listener: Send + Sync {
    async fn accept(&self) -> Result<BoxStream, ProcessError>;
}
#[async_trait::async_trait]
pub trait Platform: Send + Sync + 'static {
    /// None means the OS could not conclusively determine liveness.
    fn process_is_alive(&self, _pid: u32) -> Option<bool> {
        None
    }
    fn process_start_identity(&self, pid: u32) -> Option<String> {
        crate::host::live_sessions::process_start_identity(pid)
    }
    fn nonce(&self) -> Result<String, ProcessError>;
    fn create_directory(&self, nonce: &str) -> Result<PathBuf, ProcessError>;
    fn validate_directory(&self, path: &Path) -> Result<(), ProcessError>;
    fn endpoint(&self, directory: &Path) -> String;
    fn detach(&self, command: &mut tokio::process::Command);
    fn cancel_active_processes(&self);
    /// Release supervisor-wide cancellation ownership after a foreground
    /// result has been selected. Strict host runners must already have
    /// verified that their complete process tree has stopped.
    fn complete_foreground(&self, _strict_host: bool) -> Result<(), ProcessError> {
        Ok(())
    }
    async fn connect(&self, endpoint: &str, expected_pid: u32) -> Result<BoxStream, ProcessError>;
    async fn listen(&self, directory: &Path) -> Result<Box<dyn Listener>, ProcessError>;
    fn runner(&self, output: &Path) -> Arc<dyn ProcessRunner>;
    async fn cleanup_orphan(&self, handoff: &ShellProcessHandoff) -> Result<(), ProcessError>;
    async fn memory_pressure(&self) -> bool;
}

static PLATFORM: OnceLock<Arc<dyn Platform>> = OnceLock::new();
/// Install the platform adapter before using supervisor entry points.
pub fn configure(platform: Arc<dyn Platform>) {
    let _ = PLATFORM.set(platform);
}
fn platform() -> &'static dyn Platform {
    PLATFORM
        .get()
        .expect("shell supervisor adapter not configured")
        .as_ref()
}

const FLAG: &str = "--lingxi-shell-supervisor";
static EXECUTABLE: OnceLock<PathBuf> = OnceLock::new();
static IN_SUPERVISOR: AtomicBool = AtomicBool::new(false);
struct Job {
    handoff: ShellProcessHandoff,
    registration: Option<crate::host::agent_processes::RegistrationEntry>,
    cancel: CancellationToken,
    observer: Option<tokio::task::JoinHandle<()>>,
    pending_ack: Option<BoxStream>,
    backgrounded: bool,
    registered: bool,
    released: bool,
}
struct LiveJob {
    cancel: CancellationToken,
    _owner: Option<crate::host::agent_processes::Registration>,
}
impl Drop for LiveJob {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
fn track(handoff: ShellProcessHandoff, registered: bool) -> LiveJob {
    let cancel = CancellationToken::new();
    let owner = crate::host::agent_processes::register_with_capability_requirement(
        handoff.owner.as_deref(),
        Some(handoff.pid),
        handoff.require_group_cleanup_on_exit,
    );
    jobs()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            handoff.task_id.clone(),
            Job {
                registration: owner
                    .as_ref()
                    .map(crate::host::agent_processes::Registration::entry),
                handoff,
                cancel: cancel.clone(),
                observer: None,
                pending_ack: None,
                backgrounded: registered,
                registered,
                released: false,
            },
        );
    LiveJob {
        cancel,
        _owner: owner,
    }
}
// Retire a completed foreground job only if this exact generation is still
// installed. Background handoffs remain available for authenticated cleanup.
fn forget_foreground_job(handoff: &ShellProcessHandoff) {
    let mut rows = jobs()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if rows
        .get(&handoff.task_id)
        .is_some_and(|job| !job.backgrounded && same_identity(&job.handoff, handoff))
    {
        rows.remove(&handoff.task_id);
    }
}

fn retire_foreground_job(handoff: &ShellProcessHandoff, live: &mut Option<LiveJob>) {
    // A cancellation racing retirement must never see a registration after
    // its capability has been removed.
    drop(live.take());
    forget_foreground_job(handoff);
}

fn jobs() -> &'static Mutex<HashMap<String, Job>> {
    static JOBS: OnceLock<Mutex<HashMap<String, Job>>> = OnceLock::new();
    JOBS.get_or_init(|| Mutex::new(HashMap::new()))
}
/// Enable only after this executable has installed the early bootstrap.
pub fn enable_supervisor(executable: PathBuf) {
    let _ = EXECUTABLE.set(executable);
}
/// Must be checked before ordinary CLI/bridge argument processing.
pub fn is_supervisor_invocation() -> bool {
    std::env::args().nth(1).as_deref() == Some(FLAG)
}
pub fn enabled(command: &SandboxedCommand) -> bool {
    EXECUTABLE.get().is_some()
        && !IN_SUPERVISOR.load(Ordering::SeqCst)
        && command
            .background_task()
            .and_then(|b| b.on_exit.as_ref())
            .is_some_and(|s| s.manages_output())
        && command.backend_plan().is_none()
}
fn error(error: impl std::fmt::Display) -> ProcessError {
    ProcessError::Io(error.to_string())
}

fn unverified_shell_error(cause: impl std::fmt::Display) -> ProcessError {
    error(format!(
        "{} {cause}",
        crate::host::process::OWNED_SHELL_CLEANUP_UNVERIFIED
    ))
}

fn response_error_message(cause: ProcessError) -> String {
    match cause {
        // The receiving side matches the authenticated cancellation marker
        // and cleanup prefix as Io payloads, not their Display wrapper.
        ProcessError::Io(message) => message,
        other => other.to_string(),
    }
}

#[derive(Serialize, Deserialize)]
struct Start {
    command: ProcessCommand,
    tag: SandboxedTag,
    owner: Option<String>,
    #[serde(default = "require_group_cleanup_by_default")]
    require_group_cleanup_on_exit: bool,
    auto_background: bool,
    #[serde(default)]
    background_timeout: Option<Duration>,
    explicit: bool,
    limit: Option<usize>,
    task_id: String,
    output: PathBuf,
    nonce: String,
    supervisor_directory_identity: crate::host::rooted_fs::RootIdentity,
    output_root_identity: crate::host::rooted_fs::RootIdentity,
    output_file_identity: crate::host::rooted_fs::RootIdentity,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "request")]
enum Request {
    Start(Start),
    Inspect { nonce: String },
    Background { nonce: String },
    Kill { nonce: String },
    Accepted { nonce: String },
    AwaitSpawn { nonce: String },
    SpawnAccepted { nonce: String },
}
#[derive(Serialize, Deserialize)]
struct Receipt {
    handoff: ShellProcessHandoff,
    exit_code: Option<i32>,
    #[serde(default)]
    stop_cause: Option<String>,
    #[serde(default)]
    killed: Option<bool>,
    #[serde(default)]
    leader_exit_verified: bool,
    #[serde(default, rename = "group_cleanup_verified")]
    cleanup_verified: bool,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "response")]
enum Response {
    Spawned {
        handoff: ShellProcessHandoff,
    },
    Started {
        result: ForegroundRunResult,
        handoff: Option<ShellProcessHandoff>,
    },
    Inspect {
        handoff: ShellProcessHandoff,
        terminal: bool,
        exit_code: Option<i32>,
        #[serde(default)]
        stop_cause: Option<String>,
        #[serde(default)]
        killed: Option<bool>,
        #[serde(default)]
        leader_exit_verified: bool,
        #[serde(default, rename = "group_cleanup_verified")]
        cleanup_verified: bool,
    },
    Ack,
    Error {
        message: String,
        /// Asserted only for failures before the platform runner was started.
        #[serde(default)]
        no_child_started: bool,
    },
}

fn require_group_cleanup_by_default() -> bool {
    true
}

fn validate_start_identity(directory: &Path, start: &Start) -> Result<(), ProcessError> {
    platform().validate_directory(directory)?;
    let actual = crate::host::rooted_fs::root_identity(directory).map_err(error)?;
    if actual != start.supervisor_directory_identity {
        return Err(error("supervisor Start directory identity mismatch"));
    }
    if start.task_id.is_empty() || start.nonce.len() != 48 || !start.output.is_absolute() {
        return Err(error("invalid supervisor Start identity"));
    }
    let output_root = start.output.parent().ok_or(ProcessError::Unsupported)?;
    let output_leaf = start.output.file_name().ok_or(ProcessError::Unsupported)?;
    platform().validate_directory(output_root)?;
    crate::host::rooted_fs::open_recovery_file(
        output_root,
        Path::new(output_leaf),
        &start.output_root_identity,
        &start.output_file_identity,
    )
    .map_err(error)?;
    Ok(())
}

fn admit_supervisor_start(
    directory: &Path,
    start: &Start,
    supervisor_birth: Option<String>,
) -> Result<(SandboxedCommand, String), ProcessError> {
    let supervisor_birth = supervisor_birth
        .filter(|birth| !birth.is_empty())
        .ok_or_else(|| error("supervisor birth identity unavailable"))?;
    validate_start_identity(directory, start)?;
    Ok((
        SandboxedCommand::__new_sandboxed(start.command.clone(), start.tag.clone())
            .mark_supervisor_request()
            .with_process_owner(start.owner.clone())
            .with_require_group_cleanup_on_exit(start.require_group_cleanup_on_exit)
            .with_auto_background_on_timeout(start.auto_background)
            .with_background_timeout(start.background_timeout),
        supervisor_birth,
    ))
}

async fn send<T: Serialize>(stream: &mut BoxStream, value: &T) -> Result<(), ProcessError> {
    let mut bytes = serde_json::to_vec(value).map_err(error)?;
    bytes.push(b'\n');
    stream.write_all(&bytes).await.map_err(error)
}
async fn receive<T: serde::de::DeserializeOwned>(
    stream: &mut BoxStream,
) -> Result<T, ProcessError> {
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .await
        .map_err(error)?;
    serde_json::from_str(&line).map_err(error)
}
async fn disconnected(stream: &mut BoxStream) {
    let mut byte = [0u8; 1];
    let _ = stream.read(&mut byte).await;
}
async fn rpc(handoff: &ShellProcessHandoff, request: Request) -> Result<Response, ProcessError> {
    let mut stream = platform()
        .connect(&handoff.socket_path, handoff.supervisor_pid)
        .await
        .map_err(error)?;
    send(&mut stream, &request).await?;
    receive(&mut stream).await
}

// Keep the background request in the same select as the original Start
// response. A supervisor may accept that request and then stop replying to its
// RPC even though the independent Start stream already has a real result.
async fn await_start_response<Start, Spawn, Background, Accept, Accepted>(
    start: Start,
    spawn: Spawn,
    background: Background,
    accept_spawn: Accept,
) -> Result<Response, ProcessError>
where
    Start: std::future::Future<Output = Result<Response, ProcessError>>,
    Spawn: std::future::Future<Output = Result<Response, ProcessError>>,
    Background: std::future::Future<Output = Result<Response, ProcessError>>,
    Accept: FnOnce(ShellProcessHandoff) -> Accepted,
    Accepted: std::future::Future<Output = Result<(), ProcessError>>,
{
    tokio::pin!(start, spawn, background);
    let mut spawned = false;
    let mut background_done = false;
    let mut accept_spawn = Some(accept_spawn);
    loop {
        tokio::select! {
            response = &mut start => return response,
            result = &mut spawn, if !spawned => {
                spawned = true;
                match result {
                    Ok(Response::Spawned { handoff }) => {
                        accept_spawn.take().expect("spawn is accepted at most once")(handoff).await?;
                    }
                    Ok(_) => return Err(error("invalid spawn capability response")),
                    Err(cause) => return Err(cause),
                }
            }
            _ = &mut background, if !background_done => background_done = true,
        }
    }
}

fn same_identity(a: &ShellProcessHandoff, b: &ShellProcessHandoff) -> bool {
    a.task_id == b.task_id
        && a.pid == b.pid
        && a.supervisor_pid == b.supervisor_pid
        && a.supervisor_start_identity == b.supervisor_start_identity
        && a.supervisor_directory_identity == b.supervisor_directory_identity
        && a.output_root_identity == b.output_root_identity
        && a.output_file_identity == b.output_file_identity
        && a.process_start_identity == b.process_start_identity
        && a.owner == b.owner
        && a.require_group_cleanup_on_exit == b.require_group_cleanup_on_exit
        && a.socket_path == b.socket_path
        && a.receipt_path == b.receipt_path
        && a.output_path == b.output_path
        && a.nonce == b.nonce
}

// Before accepting a new child, bind every caller-selected identity and the
// cleanup policy to the capability returned by the supervisor. PID and birth
// identities are minted only after the payload is spawned.
fn matches_requested_identity(
    requested: &ShellProcessHandoff,
    actual: &ShellProcessHandoff,
) -> bool {
    requested.task_id == actual.task_id
        && requested.supervisor_pid == actual.supervisor_pid
        && requested.supervisor_directory_identity == actual.supervisor_directory_identity
        && requested.output_root_identity == actual.output_root_identity
        && requested.output_file_identity == actual.output_file_identity
        && requested.owner == actual.owner
        && requested.require_group_cleanup_on_exit == actual.require_group_cleanup_on_exit
        && requested.socket_path == actual.socket_path
        && requested.receipt_path == actual.receipt_path
        && requested.output_path == actual.output_path
        && requested.nonce == actual.nonce
        && actual.pid != 0
        && actual.supervisor_start_identity.is_some()
        && actual.process_start_identity.is_some()
}
fn validate_paths(handoff: &ShellProcessHandoff) -> Result<(), ProcessError> {
    let receipt = Path::new(&handoff.receipt_path);
    let directory = receipt.parent().ok_or(ProcessError::Unsupported)?;
    platform().validate_directory(directory)?;
    if receipt.file_name() != Some(std::ffi::OsStr::new("receipt"))
        || handoff.socket_path != platform().endpoint(directory)
        || !Path::new(&handoff.output_path).is_absolute()
        || handoff.nonce.len() != 48
    {
        return Err(error("invalid shell supervisor paths"));
    }
    Ok(())
}
#[derive(Debug, PartialEq, Eq)]
struct TerminalInfo {
    exit_code: Option<i32>,
    stop_cause: Option<String>,
    killed: Option<bool>,
    leader_exit_verified: bool,
    cleanup_verified: bool,
}

async fn inspect_details(
    handoff: &ShellProcessHandoff,
) -> Result<Option<TerminalInfo>, ProcessError> {
    validate_paths(handoff)?;
    match tokio::time::timeout(
        Duration::from_secs(3),
        rpc(
            handoff,
            Request::Inspect {
                nonce: handoff.nonce.clone(),
            },
        ),
    )
    .await
    {
        Ok(Ok(Response::Inspect {
            handoff: actual,
            terminal,
            exit_code,
            stop_cause,
            killed,
            leader_exit_verified,
            cleanup_verified,
        })) if same_identity(handoff, &actual) => Ok(terminal.then_some(TerminalInfo {
            exit_code,
            stop_cause,
            killed,
            leader_exit_verified,
            cleanup_verified,
        })),
        _ => {
            // Exact receipt remains valid after its supervisor exits. Read by
            // pinned/no-follow APIs, never infer completion from PID disappearance.
            let path = Path::new(&handoff.receipt_path);
            let parent = path.parent().ok_or(ProcessError::Unsupported)?;
            let name = path.file_name().ok_or(ProcessError::Unsupported)?;
            let text = crate::host::rooted_fs::read_to_string_pinned(
                parent,
                Path::new(name),
                handoff.supervisor_directory_identity.as_ref(),
            )
            .map_err(error)?;
            let receipt: Receipt = serde_json::from_str(&text).map_err(error)?;
            if !same_identity(handoff, &receipt.handoff) {
                return Err(error("shell receipt identity mismatch"));
            }
            Ok(Some(TerminalInfo {
                exit_code: receipt.exit_code,
                stop_cause: receipt.stop_cause,
                killed: receipt.killed,
                leader_exit_verified: receipt.leader_exit_verified,
                cleanup_verified: receipt.cleanup_verified,
            }))
        }
    }
}

async fn inspect(
    handoff: &ShellProcessHandoff,
) -> Result<Option<(Option<i32>, Option<String>)>, ProcessError> {
    Ok(inspect_details(handoff)
        .await?
        .map(|terminal| (terminal.exit_code, terminal.stop_cause)))
}
/// Whether the original writer still exists, accounting for PID reuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupervisorLiveness {
    Alive,
    Dead,
    Unknown,
}
pub fn supervisor_liveness(handoff: &ShellProcessHandoff) -> SupervisorLiveness {
    let Some(expected) = handoff.supervisor_start_identity.as_ref() else {
        return SupervisorLiveness::Unknown;
    };
    match platform().process_start_identity(handoff.supervisor_pid) {
        Some(actual) if &actual == expected => SupervisorLiveness::Alive,
        Some(_) => SupervisorLiveness::Dead,
        None if platform().process_is_alive(handoff.supervisor_pid) == Some(false) => {
            SupervisorLiveness::Dead
        }
        None => SupervisorLiveness::Unknown,
    }
}
const LOST_SUPERVISOR_FOOTER: &str = "[supervisor lost; task failed]";
const UNVERIFIED_SUPERVISOR_LOSS: &str = "supervisor_lost_unverified";
const UNVERIFIED_WAIT_PREFIX: &str = "wait_unverified:";
async fn recover_failed_output(
    handoff: &ShellProcessHandoff,
) -> Result<(TerminalInfo, bool), ProcessError> {
    let output = Path::new(&handoff.output_path);
    let _ = output.parent().ok_or(ProcessError::Unsupported)?;
    let handoff = handoff.clone();
    tokio::task::spawn_blocking(move || {
        use std::io::{Read, Seek, SeekFrom, Write};
        let root_id = handoff
            .output_root_identity
            .ok_or(ProcessError::Unsupported)?;
        let file_id = handoff
            .output_file_identity
            .ok_or(ProcessError::Unsupported)?;
        let receipt_id = handoff
            .supervisor_directory_identity
            .ok_or(ProcessError::Unsupported)?;
        let output = Path::new(&handoff.output_path);
        let root = output.parent().ok_or(ProcessError::Unsupported)?;
        let leaf = Path::new(output.file_name().ok_or(ProcessError::Unsupported)?);
        let mut file = crate::host::rooted_fs::open_recovery_file(root, leaf, &root_id, &file_id)
            .map_err(error)?;
        fs2::FileExt::lock_exclusive(&file).map_err(error)?;
        let receipt_root = Path::new(&handoff.receipt_path)
            .parent()
            .ok_or(ProcessError::Unsupported)?
            .to_owned();
        match crate::host::rooted_fs::read_to_string_pinned(
            &receipt_root,
            Path::new("receipt"),
            Some(&receipt_id),
        ) {
            Ok(text) => {
                let receipt: Receipt = serde_json::from_str(&text).map_err(error)?;
                if !same_identity(&receipt.handoff, &handoff) {
                    return Err(error("terminal receipt identity mismatch"));
                }
                return Ok((
                    TerminalInfo {
                        exit_code: receipt.exit_code,
                        stop_cause: receipt.stop_cause,
                        killed: receipt.killed,
                        leader_exit_verified: receipt.leader_exit_verified,
                        cleanup_verified: receipt.cleanup_verified,
                    },
                    false,
                ));
            }
            Err(crate::host::FsError::NotFound(_)) => {}
            Err(cause) => return Err(error(cause)),
        }
        let length = file.metadata().map_err(error)?.len();
        file.seek(SeekFrom::Start(length.saturating_sub(4096)))
            .map_err(error)?;
        let mut tail = String::new();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).map_err(error)?;
        tail.push_str(&String::from_utf8_lossy(&bytes));
        let last = tail.trim_end().rsplit('\n').next().unwrap_or("");
        let terminal = last == "[killed]"
            || last == "[exited; outcome unknown]"
            || last == LOST_SUPERVISOR_FOOTER
            || (last.starts_with("[exited with code ") && last.ends_with(']'));
        if !terminal {
            file.seek(SeekFrom::End(0)).map_err(error)?;
            file.write_all(format!("\n{LOST_SUPERVISOR_FOOTER}\n").as_bytes())
                .map_err(error)?;
            file.sync_all().map_err(error)?;
        }
        let receipt = Receipt {
            handoff,
            exit_code: Some(-1),
            // Recovery can publish an output receipt without proving that the
            // orphan process tree was reaped. Do not confuse it with a wait.
            stop_cause: Some(UNVERIFIED_SUPERVISOR_LOSS.into()),
            killed: None,
            leader_exit_verified: false,
            cleanup_verified: false,
        };
        crate::host::rooted_fs::atomic_write_pinned(
            &receipt_root,
            Path::new("receipt"),
            &serde_json::to_vec(&receipt).map_err(error)?,
            crate::host::rooted_fs::AtomicWriteOptions {
                overwrite: false,
                create_parents: false,
                dir_mode: 0o700,
                file_mode: 0o600,
            },
            &receipt_id,
        )
        .map_err(error)?;
        Ok((
            TerminalInfo {
                exit_code: Some(-1),
                stop_cause: Some(UNVERIFIED_SUPERVISOR_LOSS.into()),
                killed: None,
                leader_exit_verified: false,
                cleanup_verified: false,
            },
            true,
        ))
    })
    .await
    .map_err(error)?
}
/// Validate the supervisor capability and immutable task/output identity.
pub async fn validate(handoff: &ShellProcessHandoff) -> Result<(), ProcessError> {
    inspect(handoff).await.map(|_| ())
}
/// Retrieve an already-supervised process; ordinary host-owned pipes are rejected.
pub fn export(handle: &ProcessHandle) -> Result<ShellProcessHandoff, ProcessError> {
    jobs()
        .lock()
        .map_err(error)?
        .get(&handle.task_id)
        .filter(|job| {
            job.handoff.pid == handle.pid
                && job.backgrounded
                && job.registered
                && !job.cancel.is_cancelled()
        })
        .map(|job| job.handoff.clone())
        .ok_or(ProcessError::Unsupported)
}
/// Complete the registration handshake only after the tool owns the handle.
pub async fn acknowledge(handle: &ProcessHandle) -> Result<(), ProcessError> {
    let pending = {
        let mut rows = jobs().lock().map_err(error)?;
        let Some(job) = rows.get_mut(&handle.task_id) else {
            return Ok(());
        };
        if job.handoff.pid != handle.pid {
            return Err(error("shell registration identity mismatch"));
        }
        if job.registered {
            return Ok(());
        }
        (
            job.pending_ack
                .take()
                .ok_or_else(|| error("shell registration is not ready"))?,
            job.handoff.nonce.clone(),
        )
    };
    let (mut stream, nonce) = pending;
    send(&mut stream, &Request::Accepted { nonce }).await?;
    if !matches!(receive::<Response>(&mut stream).await?, Response::Ack) {
        return Err(error("shell registration was not acknowledged"));
    }
    if let Some(job) = jobs().lock().map_err(error)?.get_mut(&handle.task_id) {
        job.registered = true;
    }
    Ok(())
}
/// Relinquish observation and wait for any in-flight owner callback to finish.
pub async fn release(handoff: &ShellProcessHandoff) -> Result<(), ProcessError> {
    let task = {
        let mut rows = jobs().lock().map_err(error)?;
        let Some(job) = rows.get_mut(&handoff.task_id) else {
            return Err(error("shell release identity mismatch"));
        };
        if !same_identity(&job.handoff, handoff) {
            return Err(error("shell release identity mismatch"));
        }
        job.released = true;
        job.cancel.cancel();
        job.pending_ack.take();
        job.observer.take()
    };
    if let Some(task) = task {
        let _ = task.await;
    }
    Ok(())
}
fn registered_job(
    entry: crate::host::agent_processes::RegistrationEntry,
) -> Option<(ShellProcessHandoff, bool)> {
    jobs()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .values()
        .find(|job| job.registration == Some(entry))
        .map(|job| {
            (
                job.handoff.clone(),
                job.cancel.is_cancelled() || job.released,
            )
        })
}
/// Match the registration captured by the caller, not any historical use of
/// its PID. A stale snapshot is consumed without falling back to native kill.
pub async fn kill_owned_registration(
    owner: &str,
    entry: crate::host::agent_processes::RegistrationEntry,
) -> Option<Result<(), ProcessError>> {
    if !crate::host::agent_processes::is_current(owner, entry) {
        return Some(Ok(()));
    }
    let (handoff, stopped) = registered_job(entry)?;
    if handoff.owner.as_deref() != Some(owner) {
        return Some(Err(error("shell owner mismatch")));
    }
    if stopped {
        return Some(Ok(()));
    }
    Some(stop_handoff(&handoff).await)
}
/// Kill through the capability-bearing supervisor, never a handoff's naked PID.
pub async fn kill(handle: &ProcessHandle) -> Option<Result<(), ProcessError>> {
    let (handoff, cancel, released) = jobs()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&handle.task_id)
        .map(|job| (job.handoff.clone(), job.cancel.clone(), job.released))?;
    if handoff.pid != handle.pid || released {
        return Some(Err(error("shell ownership was relinquished")));
    }
    let mut previous_unverified = None;
    if let Ok(Some(terminal)) = inspect_details(&handoff).await {
        if leader_only_natural(&terminal, handoff.require_group_cleanup_on_exit) {
            return Some(Err(error(
                "naturally exited shell released its process group without group cleanup proof",
            )));
        }
        match terminal_cleanup_is_verified(Some(&terminal)) {
            Ok(true) => return Some(Ok(())),
            Ok(false) => {
                previous_unverified = Some(error("supervised shell has no terminal cleanup proof"))
            }
            Err(cause) => previous_unverified = Some(cause),
        }
    }
    if cancel.is_cancelled() {
        return Some(Err(error("shell owner no longer observes this process")));
    }
    let request = stop_handoff(&handoff).await;
    Some(match (request, previous_unverified) {
        // An accepted Stop can initiate termination but cannot upgrade a
        // previously unverified receipt into a successful cleanup proof.
        (Ok(()), Some(cause)) => Err(cause),
        (Err(stop), Some(cause)) => Err(error(format!(
            "{cause}; supervisor stop request failed: {stop}"
        ))),
        (result, None) => result,
    })
}

fn terminal_observation_is_valid(terminal: &TerminalInfo) -> Result<(), ProcessError> {
    match (terminal.exit_code, terminal.stop_cause.as_deref()) {
        (Some(-1), None) if !terminal.leader_exit_verified => Err(error(
            "legacy recovery receipt does not verify process exit",
        )),
        (_, Some(cause))
            if cause == UNVERIFIED_SUPERVISOR_LOSS || cause.starts_with(UNVERIFIED_WAIT_PREFIX) =>
        {
            Err(error(format!(
                "supervised process termination unverified: {cause}"
            )))
        }
        _ => Ok(()),
    }
}

// A later verified cleanup resolves a wait failure for resource ownership,
// while the original cause remains in the receipt for observers to report.
// Do not treat a legacy or lost-supervisor receipt as proof of termination.
fn terminal_cleanup_is_verified(terminal: Option<&TerminalInfo>) -> Result<bool, ProcessError> {
    let Some(terminal) = terminal else {
        return Ok(false);
    };
    let original = terminal_observation_is_valid(terminal);
    if !terminal.leader_exit_verified || !terminal.cleanup_verified {
        return match original {
            Err(cause) => Err(cause),
            _ => Err(error("supervised process cleanup has no verified receipt")),
        };
    }
    if terminal
        .stop_cause
        .as_deref()
        .is_some_and(|cause| cause.starts_with(UNVERIFIED_WAIT_PREFIX))
    {
        return Ok(true);
    }
    original.map(|()| true)
}

fn leader_only_natural(terminal: &TerminalInfo, require_group: bool) -> bool {
    !require_group
        && terminal.leader_exit_verified
        && !terminal.cleanup_verified
        && terminal.killed == Some(false)
        && terminal.stop_cause.is_none()
}

/// Stop the exact supervised background process and verify its terminal
/// supervisor state or identity-checked receipt. An observer disappearing is
/// not evidence that the child was reaped.
pub async fn terminate_and_wait(
    handle: &ProcessHandle,
    owner: &str,
) -> Option<Result<(), ProcessError>> {
    let (handoff, released) = jobs()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&handle.task_id)
        .map(|job| (job.handoff.clone(), job.released))?;
    if handoff.pid != handle.pid || handoff.owner.as_deref() != Some(owner) || released {
        return Some(Err(error(
            "shell ownership was relinquished or does not match",
        )));
    }
    enum ReapState {
        Verified,
        Pending,
        Released,
        Unverified(ProcessError),
    }
    async fn reaped(handoff: &ShellProcessHandoff) -> ReapState {
        match inspect_details(handoff).await {
            Ok(Some(terminal))
                if leader_only_natural(&terminal, handoff.require_group_cleanup_on_exit) =>
            {
                ReapState::Released
            }
            Ok(terminal) => match terminal_cleanup_is_verified(terminal.as_ref()) {
                Ok(true) => ReapState::Verified,
                Ok(false) => ReapState::Pending,
                Err(cause) => ReapState::Unverified(cause),
            },
            Err(_) => ReapState::Pending,
        }
    }
    let mut previous_failure = None;
    Some(
        match tokio::time::timeout(Duration::from_secs(15), async {
            match reaped(&handoff).await {
                ReapState::Verified => return Ok(()),
                ReapState::Pending => {}
                ReapState::Released => return Err(error("naturally exited shell released its process group without group cleanup proof")),
                // An earlier unverified report is not terminal. Retry the
                // authenticated stop while its supervisor still owns the job.
                ReapState::Unverified(cause) => previous_failure = Some(cause.to_string()),
            }
            if let Err(cause) = stop_handoff(&handoff).await {
                return match reaped(&handoff).await {
                    ReapState::Verified => Ok(()),
                    ReapState::Unverified(unverified) => Err(error(format!(
                        "{unverified}; supervisor stop retry failed: {cause}"
                    ))),
                    ReapState::Released => Err(error("naturally exited shell released its process group without group cleanup proof")),
                    ReapState::Pending => Err(cause),
                };
            }
            loop {
                match reaped(&handoff).await {
                    ReapState::Verified => return Ok(()),
                    ReapState::Pending => {}
                    ReapState::Released => return Err(error("naturally exited shell released its process group without group cleanup proof")),
                    // A new failure should be reported promptly; a retry of
                    // an already-reported failure waits for a later proof.
                    ReapState::Unverified(cause) if previous_failure.is_none() => return Err(cause),
                    ReapState::Unverified(_) => {}
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        {
            Ok(result) => result,
            Err(_) => Err(error(match previous_failure {
                Some(cause) => format!(
                    "supervised shell did not produce a verified terminal receipt; earlier failure: {cause}"
                ),
                None => "supervised shell did not produce a terminal receipt".into(),
            })),
        },
    )
}
async fn stop_handoff(handoff: &ShellProcessHandoff) -> Result<(), ProcessError> {
    match rpc(
        handoff,
        Request::Kill {
            nonce: handoff.nonce.clone(),
        },
    )
    .await
    {
        Ok(Response::Ack) => Ok(()),
        _ => Err(error("supervisor refused stop")),
    }
}

// This is the same birth-string guard used by live sessions (oracle Gye),
// not an atomic OS handle: a narrow check/signal race remains after reparenting.
/// Attach completion observation after target-side ownership checks succeed.
pub async fn adopt(
    handoff: &ShellProcessHandoff,
    sink: Arc<dyn BackgroundExitSink>,
) -> Result<ProcessHandle, ProcessError> {
    validate(handoff).await?;
    let prior = jobs()
        .lock()
        .map_err(error)?
        .get(&handoff.task_id)
        .map(|job| (job.handoff.clone(), job.cancel.is_cancelled()));
    if let Some((prior, cancelled)) = prior {
        if !same_identity(&prior, handoff) {
            return Err(error("shell adoption identity mismatch"));
        }
        if !cancelled {
            return Ok(ProcessHandle {
                task_id: handoff.task_id.clone(),
                pid: handoff.pid,
            });
        }
        release(&prior).await?;
    }
    observe(handoff.clone(), sink, None);
    Ok(ProcessHandle {
        task_id: handoff.task_id.clone(),
        pid: handoff.pid,
    })
}
async fn notify_supervised_exit(
    sink: &dyn BackgroundExitSink,
    task_id: &str,
    code: Option<i32>,
    stop_cause: Option<&str>,
    killed: Option<bool>,
) {
    // The caller has already checked the leader/group proofs and recorded any
    // prior wait failure. Never classify a verified Windows exit code by value.
    if stop_cause == Some("deadline") && killed == Some(true) {
        sink.on_supervised_background_deadline_exit(task_id, code)
            .await;
    } else {
        match killed {
            Some(true) => {
                sink.on_supervised_exit_with_status(task_id, code, true)
                    .await;
            }
            Some(false) => sink.on_supervised_exit(task_id, code).await,
            None => sink.on_supervised_exit_unknown(task_id, code).await,
        }
    }
}

// An unverified report is delivered once, but observation remains alive until
// a later explicit proof allows the recipient to release quarantined resources.
async fn deliver_supervised_terminal(
    sink: &dyn BackgroundExitSink,
    task_id: &str,
    terminal: &TerminalInfo,
    require_group_cleanup_on_exit: bool,
    unverified_reported: &mut bool,
) -> bool {
    let original = terminal_observation_is_valid(terminal);
    let cleanup = terminal_cleanup_is_verified(Some(terminal));
    let leader_only_natural = terminal.leader_exit_verified
        && !require_group_cleanup_on_exit
        && terminal.killed == Some(false)
        && terminal.stop_cause.is_none();
    let failure = original.as_ref().err().or_else(|| {
        if leader_only_natural {
            None
        } else {
            cleanup.as_ref().err()
        }
    });
    if let Some(cause) = failure {
        if !*unverified_reported {
            sink.on_wait_failure(task_id, &cause.to_string()).await;
        }
        *unverified_reported = true;
    }
    if *unverified_reported {
        if matches!(cleanup, Ok(true)) {
            sink.on_cleanup_verified(task_id).await;
            return true;
        }
        return false;
    }
    if !leader_only_natural && !matches!(cleanup, Ok(true)) {
        return false;
    }
    notify_supervised_exit(
        sink,
        task_id,
        terminal.exit_code,
        terminal.stop_cause.as_deref(),
        terminal.killed,
    )
    .await;
    true
}

fn observe(handoff: ShellProcessHandoff, sink: Arc<dyn BackgroundExitSink>, live: Option<LiveJob>) {
    let live = live.unwrap_or_else(|| track(handoff.clone(), true));
    let cancel = live.cancel.clone();
    let observed_handoff = handoff.clone();
    let task = tokio::spawn(async move {
        let _live = live;
        let mut watchdog =
            crate::host::shell_watchdog::ShellWatchdog::new(tokio::time::Instant::now());
        let mut size = 0;
        let mut failures = 0;
        let mut unverified_reported = false;
        loop {
            tokio::select! {
                biased;
                ()=cancel.cancelled()=>break,
                ()=tokio::time::sleep(Duration::from_secs(1))=>{}
            }
            match inspect_details(&handoff).await {
                Ok(Some(terminal)) => {
                    failures = 0;
                    if !cancel.is_cancelled()
                        && deliver_supervised_terminal(
                            sink.as_ref(),
                            &handoff.task_id,
                            &terminal,
                            handoff.require_group_cleanup_on_exit,
                            &mut unverified_reported,
                        )
                        .await
                    {
                        break;
                    }
                }
                Ok(None) => {
                    failures = 0;
                }
                Err(_) => {
                    failures += 1;
                    if failures >= 3
                        && !cancel.is_cancelled()
                        && supervisor_liveness(&handoff) == SupervisorLiveness::Dead
                    {
                        if unverified_reported {
                            // The previously reported failure keeps its lease
                            // quarantined when no later proof is available.
                            break;
                        }
                        // Losing the control socket alone does not transfer the writer.
                        // Only proven death permits cleanup and terminal recovery.
                        let _ = platform().cleanup_orphan(&handoff).await;
                        let private = Path::new(&handoff.output_path)
                            .parent()
                            .is_some_and(|root| platform().validate_directory(root).is_ok());
                        let recovery = if private {
                            recover_failed_output(&handoff).await
                        } else {
                            Err(ProcessError::Unsupported)
                        };
                        match recovery {
                            Ok((terminal, _lost)) => {
                                deliver_supervised_terminal(
                                    sink.as_ref(),
                                    &handoff.task_id,
                                    &terminal,
                                    handoff.require_group_cleanup_on_exit,
                                    &mut unverified_reported,
                                )
                                .await;
                            }
                            Err(cause) => {
                                sink.on_wait_failure(
                                    &handoff.task_id,
                                    &format!("{UNVERIFIED_SUPERVISOR_LOSS}: {cause}"),
                                )
                                .await
                            }
                        }
                        break;
                    }
                    continue;
                }
            }

            if let Ok(meta) = tokio::fs::metadata(&handoff.output_path).await {
                if meta.len() > size {
                    let path = Path::new(&handoff.output_path);
                    if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
                        if let Ok(tail) =
                            crate::host::rooted_fs::read_tail_bytes(parent, Path::new(name), 1024)
                        {
                            watchdog.observe(&tail);
                        }
                    }
                    size = meta.len();
                }
            }
            if let Some(tail) = watchdog.poll(tokio::time::Instant::now()) {
                sink.on_stall(&handoff.task_id, &tail).await;
            }
            if platform().memory_pressure().await && sink.on_memory_pressure(&handoff.task_id).await
            {
                let _ = rpc(
                    &handoff,
                    Request::Kill {
                        nonce: handoff.nonce.clone(),
                    },
                )
                .await;
            }
        }
    });
    if let Some(job) = jobs()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get_mut(&observed_handoff.task_id)
    {
        job.observer = Some(task);
    }
}

async fn recover_dead_foreground(handoff: &ShellProcessHandoff) -> bool {
    if supervisor_liveness(handoff) != SupervisorLiveness::Dead {
        return false;
    }
    let _ = platform().cleanup_orphan(handoff).await;
    if Path::new(&handoff.output_path)
        .parent()
        .is_some_and(|root| platform().validate_directory(root).is_ok())
    {
        let _ = recover_failed_output(handoff).await;
    }
    true
}
#[derive(Default)]
struct ForegroundRecoveryGuard {
    handoff: Option<ShellProcessHandoff>,
}
impl ForegroundRecoveryGuard {
    async fn recover_error(&mut self) {
        if let Some(handoff) = &self.handoff {
            if recover_dead_foreground(handoff).await {
                self.handoff = None;
            }
        }
    }
}
impl Drop for ForegroundRecoveryGuard {
    fn drop(&mut self) {
        let Some(handoff) = self.handoff.take() else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        runtime.spawn(async move{
            loop {
                // Ordinary source cancellation closes the control stream and
                // lets the live supervisor kill its group. Do not steal its writer.
                // Recovery can be cancelled after killing the child but before
                // appending its terminal receipt. Finish that receipt whenever
                // supervisor death is proven, even if the child is already gone.
                if recover_dead_foreground(&handoff).await{break;}
                let current=platform().process_start_identity(handoff.pid);
                if matches!((&handoff.process_start_identity,current.as_ref()),(Some(expected),Some(actual)) if expected!=actual)
                    || (current.is_none()&&platform().process_is_alive(handoff.pid)==Some(false)){break;}
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        });
    }
}

/// Start a supervisor before spawning the shell. Foreground results preserve
/// the existing runner contract; background output belongs to the supervisor.
pub async fn execute(
    command: &SandboxedCommand,
    limit: Option<usize>,
    explicit: bool,
) -> Result<ForegroundRunResult, ProcessError> {
    let binding = command.background_task().ok_or(ProcessError::Unsupported)?;
    let nonce = platform().nonce()?;
    let directory = platform().create_directory(&nonce)?;
    let supervisor_directory_identity =
        crate::host::rooted_fs::root_identity(&directory).map_err(error)?;
    let output_root = binding
        .output_path
        .parent()
        .ok_or(ProcessError::Unsupported)?;
    let root_parent = output_root.parent().ok_or(ProcessError::Unsupported)?;
    let root_leaf = Path::new(output_root.file_name().ok_or(ProcessError::Unsupported)?);
    let output_root_identity =
        crate::host::rooted_fs::ensure_private_directory(root_parent, root_leaf, 0o700)
            .map_err(error)?;
    platform().validate_directory(output_root)?;
    let output_leaf = Path::new(
        binding
            .output_path
            .file_name()
            .ok_or(ProcessError::Unsupported)?,
    );
    let output_file = crate::host::rooted_fs::open_append_file_pinned(
        output_root,
        output_leaf,
        Some(&output_root_identity),
    )
    .map_err(error)?;
    let output_file_identity =
        crate::host::rooted_fs::opened_file_identity(&output_file).map_err(error)?;
    drop(output_file);

    let socket = platform().endpoint(&directory);
    let mut process =
        tokio::process::Command::new(EXECUTABLE.get().ok_or(ProcessError::Unsupported)?);
    process
        .arg(FLAG)
        .arg(&directory)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    platform().detach(&mut process);
    let mut supervisor = process.spawn().map_err(error)?;
    let mut stream = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match platform()
                .connect(&socket, supervisor.id().unwrap_or(0))
                .await
            {
                Ok(stream) => return Ok(stream),
                Err(_) => {
                    if supervisor.try_wait().map_err(error)?.is_some() {
                        return Err(error("shell supervisor exited during bootstrap"));
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        }
    })
    .await
    .map_err(error)??;
    let request = Start {
        supervisor_directory_identity,
        output_root_identity,
        output_file_identity,
        command: command.inner().clone(),
        tag: command.tag().clone(),
        owner: command.process_owner().map(str::to_string),
        require_group_cleanup_on_exit: command.requires_group_cleanup_on_exit(),
        auto_background: command.auto_background_on_timeout(),
        background_timeout: command.background_timeout(),
        explicit,
        limit,
        task_id: binding.task_id.clone(),
        output: binding.output_path.clone(),
        nonce: nonce.clone(),
    };
    // A partial write can already have delivered a complete Start frame.
    // Losing the connection here is therefore not proof that no child exists.
    send(&mut stream, &Request::Start(request))
        .await
        .map_err(|cause| unverified_shell_error(format!("start delivery failed: {cause}")))?;
    let temporary = ShellProcessHandoff {
        supervisor_directory_identity: Some(supervisor_directory_identity),
        output_root_identity: Some(output_root_identity),
        output_file_identity: Some(output_file_identity),
        task_id: binding.task_id.clone(),
        pid: 0,
        supervisor_pid: supervisor.id().unwrap_or(0),
        supervisor_start_identity: None,
        process_start_identity: None,
        owner: command.process_owner().map(str::to_owned),
        require_group_cleanup_on_exit: command.requires_group_cleanup_on_exit(),
        socket_path: socket.clone(),
        receipt_path: directory.join("receipt").to_string_lossy().into_owned(),
        output_path: binding.output_path.to_string_lossy().into_owned(),
        nonce: nonce.clone(),
    };
    let mut live = None;
    let mut recovery_guard = ForegroundRecoveryGuard::default();
    let response = {
        let response_future = receive::<Response>(&mut stream);
        let spawned_future = rpc(
            &temporary,
            Request::AwaitSpawn {
                nonce: nonce.clone(),
            },
        );
        let background_future = async {
            if let Some(notify) = binding.on_demand.as_ref() {
                notify.notified().await;
            } else {
                std::future::pending::<()>().await;
            }
            rpc(
                &temporary,
                Request::Background {
                    nonce: nonce.clone(),
                },
            )
            .await
        };
        let requested = &temporary;
        let spawn_nonce = nonce.clone();
        let spawned_recovery = &mut recovery_guard;
        let spawned_live = &mut live;
        await_start_response(
            response_future,
            spawned_future,
            background_future,
            move |handoff| async move {
                if !matches_requested_identity(requested, &handoff) {
                    return Err(error("spawned shell does not match requested capability"));
                }
                spawned_recovery.handoff = Some(handoff.clone());
                *spawned_live = Some(track(handoff.clone(), false));
                rpc(&handoff, Request::SpawnAccepted { nonce: spawn_nonce })
                    .await
                    .map(|_| ())
            },
        )
        .await
    };
    let response = match response {
        Ok(response) => response,
        Err(cause) => {
            recovery_guard.recover_error().await;
            return Err(unverified_shell_error(format!(
                "start or spawn acknowledgement failed: {cause}"
            )));
        }
    };
    // A detached child is reaped in this host while it lives. Source exit does
    // not kill it; the supervisor was created as an independent session leader.
    tokio::spawn(async move {
        let _ = supervisor.wait().await;
    });
    match response {
        Response::Started { result, handoff } => {
            if let ForegroundOutcome::MovedToBackground(handle) = &result.outcome {
                let valid = handoff.as_ref().is_some_and(|returned| {
                    recovery_guard
                        .handoff
                        .as_ref()
                        .is_some_and(|spawned| same_identity(spawned, returned))
                        && returned.task_id == handle.task_id
                        && returned.pid == handle.pid
                });
                if !valid {
                    recovery_guard.recover_error().await;
                    return Err(unverified_shell_error(
                        "background handoff changed the accepted shell capability",
                    ));
                }
            } else if handoff.is_some() {
                recovery_guard.recover_error().await;
                return Err(unverified_shell_error(
                    "completed shell unexpectedly returned a background handoff",
                ));
            }
            if let (Some(handoff), Some(sink)) = (handoff, binding.on_exit.clone()) {
                sink.on_supervised_start(&binding.task_id).await;
                let live = live.take().unwrap_or_else(|| track(handoff.clone(), false));
                if let Some(job) = jobs().lock().map_err(error)?.get_mut(&handoff.task_id) {
                    job.handoff = handoff.clone();
                    job.backgrounded = true;
                    job.pending_ack = Some(stream);
                }
                observe(handoff, sink, Some(live));
            }
            if matches!(&result.outcome, ForegroundOutcome::Completed(_)) {
                if let Some(handoff) = &recovery_guard.handoff {
                    retire_foreground_job(handoff, &mut live);
                }
            }
            recovery_guard.handoff = None;
            Ok(result)
        }
        Response::Error {
            message,
            no_child_started,
        } => {
            if message == "supervisor command cancelled" {
                if let Some(handoff) = &recovery_guard.handoff {
                    retire_foreground_job(handoff, &mut live);
                }
                recovery_guard.handoff = None;
                Err(error(message))
            } else if no_child_started && recovery_guard.handoff.is_none() {
                recovery_guard.handoff = None;
                Err(error(message))
            } else {
                recovery_guard.recover_error().await;
                Err(unverified_shell_error(message))
            }
        }
        _ => {
            recovery_guard.recover_error().await;
            Err(unverified_shell_error("invalid shell supervisor response"))
        }
    }
}

struct ServerSink {
    inner: Arc<dyn BackgroundExitSink>,
    state: Arc<tokio::sync::Mutex<ServerState>>,
    directory: PathBuf,
    stop: Arc<tokio::sync::Notify>,
    done: CancellationToken,
    template: ShellProcessHandoff,
    spawned: Arc<tokio::sync::Notify>,
    cancelled: CancellationToken,
}
#[derive(Default)]
struct ServerState {
    nonce: Option<String>,
    handoff: Option<ShellProcessHandoff>,
    exit_code: Option<Option<i32>>,
    delivered: bool,
    receipt_persisted: bool,
    terminal_killed: Option<bool>,
    stop_cause: Option<String>,
    leader_exit_verified: bool,
    cleanup_verified: bool,
    terminal_notification_pending: bool,
    backgrounded: bool,
    spawn_accepted: bool,
}

fn persist_server_receipt(state: &mut ServerState, directory: &Path) {
    if state.terminal_notification_pending {
        return;
    }
    if let (Some(handoff), Some(exit_code)) = (state.handoff.clone(), state.exit_code) {
        state.receipt_persisted = write_receipt(
            directory,
            &Receipt {
                handoff,
                exit_code,
                stop_cause: state.stop_cause.clone(),
                killed: state.terminal_killed,
                leader_exit_verified: state.leader_exit_verified,
                cleanup_verified: state.cleanup_verified,
            },
        )
        .is_ok();
    }
}

fn state_leader_only_natural(state: &ServerState) -> bool {
    state.leader_exit_verified
        && !state.cleanup_verified
        && !state.terminal_notification_pending
        && state.terminal_killed == Some(false)
        && state.stop_cause.is_none()
        && state
            .handoff
            .as_ref()
            .is_some_and(|handoff| !handoff.require_group_cleanup_on_exit)
}

fn server_can_finish(state: &ServerState) -> bool {
    // A suspended child can fail before a handoff is published; in that case
    // no identity-bearing receipt exists, but cleanup still requires proof.
    state.delivered
        && !state.terminal_notification_pending
        && state.exit_code.is_some()
        && state.leader_exit_verified
        && (state.cleanup_verified || state_leader_only_natural(state))
        && (state.handoff.is_none() || state.receipt_persisted)
}

fn signal_server_stop(
    state: &ServerState,
    kill: &tokio::sync::Notify,
    stop: &CancellationToken,
) -> Result<(), ProcessError> {
    if state.backgrounded {
        if state_leader_only_natural(state) {
            return Err(error("naturally exited shell released its process group"));
        }
        // A recorded failure is an observation, not a finished job. The
        // retained runner may use another notification to retry its cleanup.
        if !state.cleanup_verified {
            kill.notify_one();
        }
    } else {
        stop.cancel();
    }
    Ok(())
}

#[async_trait::async_trait]
impl BackgroundExitSink for ServerSink {
    async fn on_spawn(&self, _: &str, pid: u32) -> Result<(), ProcessError> {
        let birth = platform()
            .process_start_identity(pid)
            .ok_or_else(|| error("shell birth identity unavailable"))?;
        let mut handoff = self.template.clone();
        handoff.pid = pid;
        handoff.process_start_identity = Some(birth);
        self.state.lock().await.handoff = Some(handoff);
        self.spawned.notify_waiters();
        loop {
            let notified = self.spawned.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.cancelled.is_cancelled() {
                return Err(error("shell capability cancelled before acceptance"));
            }
            if self.state.lock().await.spawn_accepted {
                return Ok(());
            }
            tokio::select! {
                biased;
                _ = self.cancelled.cancelled() => return Err(error("shell capability cancelled before acceptance")),
                _ = notified => {},
            }
        }
    }
    fn manages_output(&self) -> bool {
        true
    }
    fn stop_notify(&self) -> Option<Arc<tokio::sync::Notify>> {
        Some(self.stop.clone())
    }
    fn requires_verified_supervised_stop(&self) -> bool {
        true
    }
    async fn finalize_persisted_output(
        &self,
        id: &str,
        max: u64,
    ) -> Result<Option<u64>, ProcessError> {
        self.inner.finalize_persisted_output(id, max).await
    }
    async fn append_output(&self, id: &str, text: &str) -> Result<(), ProcessError> {
        self.inner.append_output(id, text).await
    }
    async fn flush_output(&self, id: &str) -> Result<(), ProcessError> {
        self.inner.flush_output(id).await
    }
    async fn on_background_deadline_exit(&self, id: &str, code: Option<i32>) {
        {
            let mut state = self.state.lock().await;
            if state.cleanup_verified
                || state.terminal_notification_pending
                || state_leader_only_natural(&state)
                || state
                    .stop_cause
                    .as_deref()
                    .is_some_and(|cause| cause.starts_with(UNVERIFIED_WAIT_PREFIX))
            {
                return;
            }
            state.stop_cause = Some("deadline".into());
        }
        self.on_exit_with_status(id, code, true).await;
    }
    async fn on_wait_failure(&self, id: &str, cause: &str) {
        {
            let mut state = self.state.lock().await;
            if state.cleanup_verified
                || state.terminal_notification_pending
                || state_leader_only_natural(&state)
                || state
                    .stop_cause
                    .as_deref()
                    .is_some_and(|old| old.starts_with(UNVERIFIED_WAIT_PREFIX))
            {
                return;
            }
            state.stop_cause = Some(format!("{UNVERIFIED_WAIT_PREFIX}{cause}"));
            state.exit_code = Some(None);
            state.terminal_killed = None;
            state.cleanup_verified = false;
            persist_server_receipt(&mut state, &self.directory);
        }
        // Preserve the failure for the output owner; in particular, do not
        // write a killed trailer merely because a stop was requested.
        self.inner.on_wait_failure(id, cause).await;
    }
    async fn on_cleanup_verified(&self, id: &str) {
        // The process is now confirmed stopped, but the earlier failed wait
        // cannot tell us whether it exited naturally or was killed.
        {
            let mut state = self.state.lock().await;
            if state.cleanup_verified
                || state.terminal_notification_pending
                || state_leader_only_natural(&state)
            {
                return;
            }
            state.terminal_notification_pending = true;
        }
        self.inner.on_exit_outcome_unknown(id, None).await;
        let mut state = self.state.lock().await;
        state.terminal_notification_pending = false;
        state.leader_exit_verified = true;
        state.terminal_killed = None;
        state.cleanup_verified = true;
        // Preserve the original error while durably publishing the later proof.
        persist_server_receipt(&mut state, &self.directory);
        if server_can_finish(&state) {
            self.done.cancel();
        }
    }
    async fn on_exit(&self, id: &str, code: Option<i32>) {
        self.on_exit_with_proof(id, code, None, false).await;
    }
    async fn on_exit_with_status(&self, id: &str, code: Option<i32>, killed: bool) {
        self.on_exit_with_proof(id, code, Some(killed), false).await;
    }
    async fn on_exit_with_proof(
        &self,
        id: &str,
        code: Option<i32>,
        killed: Option<bool>,
        group_cleanup_verified: bool,
    ) {
        let mut state = self.state.lock().await;
        if state.cleanup_verified
            || state.terminal_notification_pending
            || state_leader_only_natural(&state)
        {
            return;
        }
        let earlier_failure = state
            .stop_cause
            .as_deref()
            .is_some_and(|cause| cause.starts_with(UNVERIFIED_WAIT_PREFIX));
        if earlier_failure {
            if group_cleanup_verified {
                drop(state);
                self.on_cleanup_verified(id).await;
            } else {
                // Leader evidence can arrive after a failed cleanup, but it
                // must not replace the earlier diagnostic or publish exit.
                state.exit_code = Some(code);
                state.leader_exit_verified = true;
                persist_server_receipt(&mut state, &self.directory);
            }
            return;
        }
        let leader_only_natural = !self.template.require_group_cleanup_on_exit
            && killed == Some(false)
            && state.stop_cause.is_none();
        if !group_cleanup_verified && !leader_only_natural {
            let cause = "leader exited without required process group/Job cleanup proof";
            state.stop_cause = Some(format!("{UNVERIFIED_WAIT_PREFIX}{cause}"));
            state.exit_code = Some(code);
            state.leader_exit_verified = true;
            state.terminal_killed = killed;
            persist_server_receipt(&mut state, &self.directory);
            drop(state);
            self.inner.on_wait_failure(id, cause).await;
            return;
        }
        state.exit_code = Some(code);
        state.terminal_killed = killed;
        state.leader_exit_verified = true;
        state.terminal_notification_pending = true;
        drop(state);
        match killed {
            Some(killed) => self.inner.on_exit_with_status(id, code, killed).await,
            None => self.inner.on_exit_outcome_unknown(id, code).await,
        }
        let mut state = self.state.lock().await;
        state.terminal_notification_pending = false;
        state.cleanup_verified = group_cleanup_verified;
        persist_server_receipt(&mut state, &self.directory);
        if server_can_finish(&state) {
            self.done.cancel();
        }
    }
}
fn write_receipt(directory: &Path, receipt: &Receipt) -> Result<(), ProcessError> {
    let identity = receipt
        .handoff
        .supervisor_directory_identity
        .as_ref()
        .ok_or(ProcessError::Unsupported)?;
    crate::host::rooted_fs::atomic_write_pinned(
        directory,
        Path::new("receipt"),
        &serde_json::to_vec(receipt).map_err(error)?,
        crate::host::rooted_fs::AtomicWriteOptions {
            overwrite: true,
            create_parents: false,
            dir_mode: 0o700,
            file_mode: 0o600,
        },
        identity,
    )
    .map_err(error)
}

/// Early executable bootstrap. The factory provides the same bounded output
/// writer used by the host; no registry/model/session is constructed here.
pub async fn run_supervisor(
    factory: fn(&Path) -> Arc<dyn BackgroundExitSink>,
) -> Result<(), ProcessError> {
    let directory = PathBuf::from(std::env::args().nth(2).ok_or(ProcessError::Unsupported)?);
    run_at(directory, factory).await
}
// Kill is a receipt, not a completion proof. Keep the same child-owning
// runner alive until its native wait/output drain finishes before replying to
// Start or closing a disconnected foreground host.
async fn cancel_and_join_runner<F>(
    mut running: std::pin::Pin<&mut F>,
    state: &tokio::sync::Mutex<ServerState>,
    mut signal: impl FnMut(),
) -> Result<(), ProcessError>
where
    F: std::future::Future<Output = Result<ForegroundRunResult, ProcessError>>,
{
    let result = loop {
        signal();
        tokio::select! {
            biased;
            result = &mut running => break result?,
            _ = tokio::time::sleep(Duration::from_millis(10)) => {},
        }
    };
    let ForegroundOutcome::MovedToBackground(handle) = result.outcome else {
        // Strict runners return Completed only after verifying the entire
        // owned process tree. Ordinary completion retains its existing rules.
        return Ok(());
    };
    // The runner's future may hand the process to its background reaper at
    // exactly the same time cancellation wins. Keep signalling that reaper and
    // wait for its identity-matched terminal report; the handoff alone is not
    // proof that the process has stopped.
    loop {
        signal();
        {
            let current = state.lock().await;
            let Some(handoff) = current.handoff.as_ref() else {
                return Err(unverified_shell_error(
                    "cancelled handoff has no spawn identity",
                ));
            };
            if handoff.pid != handle.pid || handoff.task_id != handle.task_id {
                return Err(unverified_shell_error("cancelled handoff identity changed"));
            }
            if let Some(code) = current.exit_code {
                let terminal = TerminalInfo {
                    exit_code: code,
                    stop_cause: current.stop_cause.clone(),
                    killed: current.terminal_killed,
                    leader_exit_verified: current.leader_exit_verified,
                    cleanup_verified: current.cleanup_verified,
                };
                return match terminal_cleanup_is_verified(Some(&terminal)) {
                    Ok(true) => Ok(()),
                    Ok(false) => Err(unverified_shell_error("missing terminal evidence")),
                    Err(cause) => Err(unverified_shell_error(cause)),
                };
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
pub async fn run_at(
    directory: PathBuf,
    factory: fn(&Path) -> Arc<dyn BackgroundExitSink>,
) -> Result<(), ProcessError> {
    IN_SUPERVISOR.store(true, Ordering::SeqCst);
    platform().validate_directory(&directory)?;
    let socket = platform().endpoint(&directory);
    let listener = platform().listen(&directory).await?;
    let state = Arc::new(tokio::sync::Mutex::new(ServerState::default()));
    let background = Arc::new(tokio::sync::Notify::new());
    let stop = CancellationToken::new();
    let spawned = Arc::new(tokio::sync::Notify::new());
    let kill = Arc::new(tokio::sync::Notify::new());
    let done = CancellationToken::new();
    let mut retry = tokio::time::interval(Duration::from_secs(1));
    let bootstrap_deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let mut stream = tokio::select! {
            accepted = listener.accept() => accepted.map_err(error)?,
            _ = retry.tick() => {
                let mut current = state.lock().await;
                if !current.receipt_persisted {
                    persist_server_receipt(&mut current, &directory);
                }
                if server_can_finish(&current) {
                    done.cancel();
                }
                continue;
            },
            () = tokio::time::sleep_until(bootstrap_deadline), if state.lock().await.nonce.is_none() => {
                let _ = std::fs::remove_file(&socket);
                return Ok(());
            },
            () = done.cancelled() => {
                let _ = std::fs::remove_file(&socket);
                return Ok(());
            },
        };
        let state = state.clone();
        let background = background.clone();
        let stop = stop.clone();
        let directory = directory.clone();
        let kill = kill.clone();
        let done = done.clone();
        let spawned = spawned.clone();
        tokio::spawn(async move {
            let Ok(request) = receive::<Request>(&mut stream).await else {
                return;
            };
            let is_start = matches!(&request, Request::Start(_));
            let response = match request {
                Request::AwaitSpawn { nonce } => loop {
                    let notified = spawned.notified();
                    tokio::pin!(notified);
                    notified.as_mut().enable();
                    let current = state.lock().await;
                    if current
                        .nonce
                        .as_ref()
                        .is_some_and(|actual| actual != &nonce)
                    {
                        break Response::Error {
                            message: "invalid supervisor capability".into(),
                            no_child_started: false,
                        };
                    }
                    if let Some(handoff) = &current.handoff {
                        break Response::Spawned {
                            handoff: handoff.clone(),
                        };
                    }
                    drop(current);
                    notified.await;
                },

                Request::Start(start) => {
                    {
                        let mut state = state.lock().await;
                        if state.nonce.is_some() {
                            return;
                        }
                        state.nonce = Some(start.nonce.clone());
                    }
                    let (command, supervisor_birth) = match admit_supervisor_start(
                        &directory,
                        &start,
                        platform().process_start_identity(std::process::id()),
                    ) {
                        Ok(admitted) => admitted,
                        Err(cause) => {
                            let _ = send(
                                &mut stream,
                                &Response::Error {
                                    message: cause.to_string(),
                                    no_child_started: true,
                                },
                            )
                            .await;
                            done.cancel();
                            return;
                        }
                    };
                    let inner = factory(&start.output);
                    let sink = Arc::new(ServerSink {
                        inner,
                        state: state.clone(),
                        directory: directory.clone(),
                        stop: kill.clone(),
                        done: done.clone(),
                        spawned: spawned.clone(),
                        cancelled: stop.clone(),
                        template: ShellProcessHandoff {
                            supervisor_directory_identity: Some(
                                start.supervisor_directory_identity,
                            ),
                            output_root_identity: Some(start.output_root_identity),
                            output_file_identity: Some(start.output_file_identity),
                            task_id: start.task_id.clone(),
                            pid: 0,
                            supervisor_pid: std::process::id(),
                            supervisor_start_identity: Some(supervisor_birth),
                            process_start_identity: None,
                            owner: start.owner.clone(),
                            require_group_cleanup_on_exit: start.require_group_cleanup_on_exit,
                            socket_path: platform().endpoint(&directory),
                            receipt_path: directory.join("receipt").to_string_lossy().into_owned(),
                            output_path: start.output.to_string_lossy().into_owned(),
                            nonce: start.nonce.clone(),
                        },
                    });
                    let strict_host = start.require_group_cleanup_on_exit;
                    let command = command.with_background_task(BackgroundTaskBinding {
                        task_id: start.task_id.clone(),
                        output_path: start.output.clone(),
                        on_exit: Some(sink),
                        on_demand: Some(background),
                    });
                    let process = platform().runner(&start.output);
                    let run = async {
                        if start.explicit {
                            process.spawn_background(&command).await.map(|handle| {
                                ForegroundRunResult {
                                    outcome: ForegroundOutcome::MovedToBackground(handle),
                                    output_file: None,
                                }
                            })
                        } else {
                            process
                                .run_foreground_with_output_limit(&command, start.limit)
                                .await
                        }
                    };
                    tokio::pin!(run);
                    let result = tokio::select! {
                        result = &mut run => result,
                        () = stop.cancelled() => {
                            let settled = cancel_and_join_runner(run.as_mut(), &state, || { kill.notify_one(); platform().cancel_active_processes(); }).await;
                            match settled {
                                Ok(_) => Err(error("supervisor command cancelled")),
                                Err(cause) => Err(cause),
                            }
                        }
                        () = disconnected(&mut stream) => {
                            // A pre-exec capability wait has no active child
                            // registration yet. Cancel that acceptance wait so
                            // the platform closes its gate and joins spawn.
                            stop.cancel();
                            let _ = cancel_and_join_runner(run.as_mut(), &state, || { kill.notify_one(); platform().cancel_active_processes(); }).await;
                            let mut current = state.lock().await;
                            current.delivered = true;
                            if current.exit_code.is_none() || server_can_finish(&current) {
                                done.cancel();
                            }
                            return;
                        }
                    };
                    let result = match result {
                        Ok(result) if matches!(&result.outcome, ForegroundOutcome::Completed(output) if !output.timed_out) => {
                            platform().complete_foreground(strict_host).map(|()| result)
                        }
                        other => other,
                    };
                    match result {
                        Ok(result) => {
                            let handoff = if let ForegroundOutcome::MovedToBackground(handle) =
                                &result.outcome
                            {
                                let mut state = state.lock().await;
                                let handoff = state
                                    .handoff
                                    .clone()
                                    .ok_or_else(|| error("runner did not report shell spawn"));
                                let Ok(handoff) = handoff else {
                                    return;
                                };
                                if handoff.pid != handle.pid {
                                    return;
                                }
                                state.backgrounded = true;
                                if state.exit_code.is_some() {
                                    persist_server_receipt(&mut state, &directory);
                                }
                                Some(handoff)
                            } else {
                                None
                            };
                            Response::Started { result, handoff }
                        }
                        Err(error) => Response::Error {
                            message: response_error_message(error),
                            no_child_started: false,
                        },
                    }
                }
                request => {
                    let nonce = match &request {
                        Request::Inspect { nonce }
                        | Request::Background { nonce }
                        | Request::Kill { nonce }
                        | Request::Accepted { nonce }
                        | Request::AwaitSpawn { nonce }
                        | Request::SpawnAccepted { nonce } => nonce,
                        _ => unreachable!(),
                    };
                    let mut state = state.lock().await;
                    if state.nonce.as_ref() != Some(nonce) {
                        Response::Error {
                            message: "invalid supervisor capability".into(),
                            no_child_started: false,
                        }
                    } else {
                        match request {
                            Request::Inspect { .. } => state.handoff.clone().map_or(
                                Response::Error {
                                    message: "shell still foreground".into(),
                                    no_child_started: false,
                                },
                                |handoff| Response::Inspect {
                                    handoff,
                                    terminal: state.exit_code.is_some()
                                        && !state.terminal_notification_pending,
                                    exit_code: state.exit_code.flatten(),
                                    stop_cause: state.stop_cause.clone(),
                                    killed: state.terminal_killed,
                                    leader_exit_verified: state.leader_exit_verified,
                                    cleanup_verified: state.cleanup_verified,
                                },
                            ),
                            Request::SpawnAccepted { .. } => {
                                state.spawn_accepted = true;
                                spawned.notify_waiters();
                                Response::Ack
                            }
                            Request::Background { .. } => {
                                background.notify_one();
                                Response::Ack
                            }
                            Request::Kill { .. } => {
                                match signal_server_stop(&state, &kill, &stop) {
                                    Ok(()) => Response::Ack,
                                    Err(cause) => Response::Error {
                                        message: response_error_message(cause),
                                        no_child_started: false,
                                    },
                                }
                            }
                            _ => unreachable!(),
                        }
                    }
                }
            };
            let finished = (is_start
                && matches!(
                    &response,
                    Response::Started { handoff: None, .. } | Response::Error { .. }
                ))
                || {
                    let current = state.lock().await;
                    server_can_finish(&current)
                };
            let sent = send(&mut stream, &response).await;
            if is_start
                && matches!(
                    &response,
                    Response::Started {
                        handoff: Some(_),
                        ..
                    }
                )
            {
                let accepted = if sent.is_ok() {
                    tokio::time::timeout(Duration::from_secs(10), receive::<Request>(&mut stream))
                        .await
                        .ok()
                        .and_then(Result::ok)
                } else {
                    None
                };
                let valid = {
                    let current = state.lock().await;
                    matches!(accepted,Some(Request::Accepted{nonce}) if current.nonce.as_ref()==Some(&nonce))
                };
                if valid {
                    let _ = send(&mut stream, &Response::Ack).await;
                }
                let mut current = state.lock().await;
                if !valid {
                    kill.notify_one();
                }
                current.delivered = true;
                if server_can_finish(&current) {
                    done.cancel();
                }
            }
            if finished {
                let mut current = state.lock().await;
                current.delivered = true;
                if current.exit_code.is_none() || server_can_finish(&current) {
                    done.cancel();
                }
            }
        });
    }
}

#[cfg(test)]
mod registration_tests {
    use super::*;
    fn handoff(id: &str, owner: &str) -> ShellProcessHandoff {
        ShellProcessHandoff {
            supervisor_directory_identity: None,
            output_root_identity: None,
            output_file_identity: None,
            task_id: id.into(),
            pid: 98765,
            supervisor_pid: 98764,
            supervisor_start_identity: Some("supervisor birth".into()),
            process_start_identity: Some("birth".into()),
            owner: Some(owner.into()),
            require_group_cleanup_on_exit: true,
            socket_path: String::new(),
            receipt_path: String::new(),
            output_path: String::new(),
            nonce: String::new(),
        }
    }
    #[test]
    fn verified_wait_distinguishes_signals_from_wait_failure() {
        assert!(!terminal_cleanup_is_verified(None).unwrap());
        assert!(terminal_cleanup_is_verified(Some(&TerminalInfo {
            exit_code: None,
            stop_cause: None,
            killed: None,
            leader_exit_verified: true,
            cleanup_verified: true,
        }))
        .unwrap());
        assert!(terminal_cleanup_is_verified(Some(&TerminalInfo {
            exit_code: None,
            stop_cause: Some("wait_unverified:io error".into()),
            killed: None,
            leader_exit_verified: false,
            cleanup_verified: false,
        }))
        .is_err());
        assert!(terminal_cleanup_is_verified(Some(&TerminalInfo {
            exit_code: Some(-1),
            stop_cause: Some(UNVERIFIED_SUPERVISOR_LOSS.into()),
            killed: None,
            leader_exit_verified: true,
            cleanup_verified: true,
        }))
        .is_err());
        assert!(terminal_cleanup_is_verified(Some(&TerminalInfo {
            exit_code: Some(-1),
            stop_cause: None,
            killed: None,
            leader_exit_verified: true,
            cleanup_verified: true,
        }))
        .unwrap());
        assert!(terminal_cleanup_is_verified(Some(&TerminalInfo {
            exit_code: Some(-1),
            stop_cause: None,
            killed: None,
            leader_exit_verified: false,
            cleanup_verified: false,
        }))
        .is_err());
        assert!(terminal_cleanup_is_verified(Some(&TerminalInfo {
            exit_code: Some(0),
            stop_cause: None,
            killed: Some(false),
            leader_exit_verified: false,
            cleanup_verified: true,
        }))
        .is_err());
        let mut legacy = serde_json::to_value(handoff("policy", "owner")).unwrap();
        legacy
            .as_object_mut()
            .unwrap()
            .remove("require_group_cleanup_on_exit");
        let legacy: ShellProcessHandoff = serde_json::from_value(legacy).unwrap();
        assert!(legacy.require_group_cleanup_on_exit);
    }

    #[test]
    fn accepted_spawn_cannot_change_the_requested_cleanup_policy_or_identity() {
        let mut requested = handoff("requested-task", "requested-owner");
        requested.pid = 0;
        requested.supervisor_start_identity = None;
        requested.process_start_identity = None;
        let mut actual = requested.clone();
        actual.pid = 98765;
        actual.supervisor_start_identity = Some("supervisor birth".into());
        actual.process_start_identity = Some("child birth".into());
        assert!(matches_requested_identity(&requested, &actual));
        let mut changed = actual.clone();
        changed.require_group_cleanup_on_exit = false;
        assert!(!matches_requested_identity(&requested, &changed));
        assert!(!same_identity(&actual, &changed));
        changed = actual.clone();
        changed.owner = Some("different-owner".into());
        assert!(!matches_requested_identity(&requested, &changed));
        changed = actual.clone();
        changed.nonce = "different".into();
        assert!(!matches_requested_identity(&requested, &changed));
        changed = actual;
        changed.process_start_identity = None;
        assert!(!matches_requested_identity(&requested, &changed));
    }

    #[test]
    fn foreground_retirement_cannot_remove_replaced_or_background_job() {
        let mut old = handoff("retirement-generation", "retirement-owner");
        old.nonce = "old".into();
        let old_live = track(old.clone(), false);
        let mut new = old.clone();
        new.nonce = "new".into();
        let new_live = track(new.clone(), false);
        forget_foreground_job(&old);
        assert_eq!(
            jobs()
                .lock()
                .unwrap()
                .get(&new.task_id)
                .unwrap()
                .handoff
                .nonce,
            "new"
        );
        jobs()
            .lock()
            .unwrap()
            .get_mut(&new.task_id)
            .unwrap()
            .backgrounded = true;
        forget_foreground_job(&new);
        assert!(jobs().lock().unwrap().contains_key(&new.task_id));
        jobs()
            .lock()
            .unwrap()
            .get_mut(&new.task_id)
            .unwrap()
            .backgrounded = false;
        forget_foreground_job(&new);
        assert!(!jobs().lock().unwrap().contains_key(&new.task_id));
        drop(old_live);
        drop(new_live);
    }

    #[tokio::test]
    async fn verified_stop_never_accepts_missing_or_released_ownership() {
        let owner = "supervisor-cleanup-identity";
        let task_id = "supervisor-cleanup-identity-task";
        let handle = ProcessHandle {
            task_id: task_id.into(),
            pid: 98765,
        };
        assert!(terminate_and_wait(&handle, owner).await.is_none());
        let live = track(handoff(task_id, owner), true);
        assert!(terminate_and_wait(&handle, "different-owner")
            .await
            .unwrap()
            .is_err());
        jobs().lock().unwrap().get_mut(task_id).unwrap().released = true;
        assert!(terminate_and_wait(&handle, owner).await.unwrap().is_err());
        drop(live);
        jobs().lock().unwrap().remove(task_id);
    }

    #[tokio::test]
    async fn old_supervised_tombstone_does_not_shadow_reused_pid_registration() {
        let owner = "supervisor-registration-generation";
        let old = track(handoff("old-generation", owner), true);
        let stale = crate::host::agent_processes::snapshot_entries(owner)[0];
        drop(old);
        let direct = crate::host::agent_processes::register(Some(owner), Some(stale.pid)).unwrap();
        assert!(kill_owned_registration(owner, direct.entry())
            .await
            .is_none());
        assert!(kill_owned_registration(owner, stale).await.unwrap().is_ok());
        drop(direct);
        let new = track(handoff("new-generation", owner), true);
        let current = crate::host::agent_processes::snapshot_entries(owner)[0];
        assert_eq!(registered_job(current).unwrap().0.task_id, "new-generation");
        assert_ne!(current.token, stale.token);
        drop(new);
        let mut rows = jobs().lock().unwrap();
        rows.remove("old-generation");
        rows.remove("new-generation");
    }

    #[test]
    fn foreground_retirement_withdraws_registration_and_capability() {
        let owner = "retire-foreground-owner";
        let handoff = handoff("retire-foreground-task", owner);
        let mut live = Some(track(handoff.clone(), false));
        let entry = live.as_ref().unwrap()._owner.as_ref().unwrap().entry();
        assert!(crate::host::agent_processes::is_current(owner, entry));
        assert!(registered_job(entry).is_some());
        retire_foreground_job(&handoff, &mut live);
        assert!(live.is_none());
        assert!(!crate::host::agent_processes::is_current(owner, entry));
        assert!(registered_job(entry).is_none());
    }
}

#[cfg(all(test, unix))]
mod recovery_tests {
    use super::*;

    // The retry must validate the pinned receipt even when the supervisor's
    // control socket is gone. This adapter forces that real fallback path.
    struct OfflineReceiptPlatform;

    #[async_trait::async_trait]
    impl Platform for OfflineReceiptPlatform {
        fn nonce(&self) -> Result<String, ProcessError> {
            Err(ProcessError::Unsupported)
        }
        fn create_directory(&self, _: &str) -> Result<PathBuf, ProcessError> {
            Err(ProcessError::Unsupported)
        }
        fn validate_directory(&self, path: &Path) -> Result<(), ProcessError> {
            if path.is_dir() {
                Ok(())
            } else {
                Err(ProcessError::Unsupported)
            }
        }
        fn endpoint(&self, directory: &Path) -> String {
            directory.join("socket").to_string_lossy().into_owned()
        }
        fn detach(&self, _: &mut tokio::process::Command) {}
        fn cancel_active_processes(&self) {}
        async fn connect(&self, endpoint: &str, _: u32) -> Result<BoxStream, ProcessError> {
            // Most receipt tests have no socket and exercise offline recovery.
            // A retry test installs a local socket to exercise the actual RPC.
            tokio::net::UnixStream::connect(endpoint)
                .await
                .map(|stream| Box::new(stream) as BoxStream)
                .map_err(error)
        }
        async fn listen(&self, _: &Path) -> Result<Box<dyn Listener>, ProcessError> {
            Err(ProcessError::Unsupported)
        }
        fn runner(&self, _: &Path) -> Arc<dyn ProcessRunner> {
            panic!("receipt-only platform cannot start processes")
        }
        async fn cleanup_orphan(&self, _: &ShellProcessHandoff) -> Result<(), ProcessError> {
            Err(ProcessError::Unsupported)
        }
        async fn memory_pressure(&self) -> bool {
            false
        }
    }

    fn fixture(root: &Path, body: &str) -> ShellProcessHandoff {
        use std::os::unix::fs::PermissionsExt;
        let output = root.join("output");
        let receipts = root.join("supervisor");
        std::fs::create_dir_all(&output).unwrap();
        std::fs::create_dir_all(&receipts).unwrap();
        std::fs::set_permissions(&output, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&receipts, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = output.join("b1234567.output");
        std::fs::write(&path, body).unwrap();
        let file = std::fs::File::open(&path).unwrap();
        ShellProcessHandoff {
            task_id: "b1234567".into(),
            pid: 12345,
            supervisor_pid: 12344,
            supervisor_start_identity: Some("writer-birth".into()),
            process_start_identity: Some("child-birth".into()),
            owner: None,
            require_group_cleanup_on_exit: true,
            supervisor_directory_identity: Some(
                crate::host::rooted_fs::root_identity(&receipts).unwrap(),
            ),
            output_root_identity: Some(crate::host::rooted_fs::root_identity(&output).unwrap()),
            output_file_identity: Some(
                crate::host::rooted_fs::opened_file_identity(&file).unwrap(),
            ),
            socket_path: receipts.join("socket").to_string_lossy().into_owned(),
            receipt_path: receipts.join("receipt").to_string_lossy().into_owned(),
            output_path: path.to_string_lossy().into_owned(),
            nonce: "a".repeat(48),
        }
    }

    #[test]
    fn supervisor_command_marker_requires_valid_start_resources() {
        PLATFORM.get_or_init(|| Arc::new(OfflineReceiptPlatform));
        let root = tempfile::tempdir().unwrap();
        let handoff = fixture(root.path(), "body");
        let directory = Path::new(&handoff.receipt_path).parent().unwrap();
        let start = Start {
            command: ProcessCommand {
                command: "shell".into(),
                args: Vec::new(),
                cwd: None,
                env: HashMap::new(),
                timeout: None,
                stdin: None,
            },
            tag: SandboxedTag::BypassAuditedWithReason {
                reason: "test".into(),
            },
            owner: Some("test-owner".into()),
            require_group_cleanup_on_exit: true,
            auto_background: false,
            background_timeout: None,
            explicit: true,
            limit: None,
            task_id: handoff.task_id.clone(),
            output: PathBuf::from(&handoff.output_path),
            nonce: handoff.nonce.clone(),
            supervisor_directory_identity: handoff.supervisor_directory_identity.unwrap(),
            output_root_identity: handoff.output_root_identity.unwrap(),
            output_file_identity: handoff.output_file_identity.unwrap(),
        };
        let ordinary = SandboxedCommand::__new_sandboxed(start.command.clone(), start.tag.clone())
            .with_require_group_cleanup_on_exit(true);
        assert!(!ordinary.is_supervisor_request());
        let (admitted, birth) =
            admit_supervisor_start(directory, &start, Some("supervisor-birth".into())).unwrap();
        assert_eq!(birth, "supervisor-birth");
        assert!(admitted.is_supervisor_request());
        assert!(admitted.clone().is_supervisor_request());
        assert!(
            !ordinary.is_supervisor_request(),
            "admission is per command"
        );

        let mut wire = serde_json::to_value(&start).unwrap();
        wire.as_object_mut()
            .unwrap()
            .insert("supervisor_request".into(), true.into());
        let decoded: Start = serde_json::from_value(wire).unwrap();
        assert!(
            !SandboxedCommand::__new_sandboxed(decoded.command, decoded.tag)
                .is_supervisor_request()
        );

        let mut bad_directory = serde_json::to_value(&start).unwrap();
        bad_directory["supervisor_directory_identity"] =
            serde_json::to_value(handoff.output_root_identity.unwrap()).unwrap();
        let bad_directory: Start = serde_json::from_value(bad_directory).unwrap();
        assert!(admit_supervisor_start(directory, &bad_directory, Some("birth".into())).is_err());
        let mut bad_output = serde_json::to_value(&start).unwrap();
        bad_output["output_file_identity"] =
            serde_json::to_value(handoff.output_root_identity.unwrap()).unwrap();
        let bad_output: Start = serde_json::from_value(bad_output).unwrap();
        assert!(admit_supervisor_start(directory, &bad_output, Some("birth".into())).is_err());
        let mut bad_nonce = serde_json::to_value(&start).unwrap();
        bad_nonce["nonce"] = "short".into();
        let bad_nonce: Start = serde_json::from_value(bad_nonce).unwrap();
        assert!(admit_supervisor_start(directory, &bad_nonce, Some("birth".into())).is_err());
        assert!(admit_supervisor_start(directory, &start, None).is_err());
        assert!(admit_supervisor_start(directory, &start, Some(String::new())).is_err());
    }

    #[tokio::test]
    async fn failed_wait_is_persisted_as_unverified_not_signal_exit() {
        struct Sink(std::sync::atomic::AtomicBool);
        #[async_trait::async_trait]
        impl BackgroundExitSink for Sink {
            async fn on_exit(&self, _: &str, _: Option<i32>) {
                panic!("an unverified stop must not use the ordinary exit callback");
            }
            async fn on_wait_failure(&self, _: &str, cause: &str) {
                assert_eq!(cause, "injected wait failure");
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let handoff = fixture(directory.path(), "body");
        let receipt_directory = Path::new(&handoff.receipt_path)
            .parent()
            .unwrap()
            .to_path_buf();
        let state = Arc::new(tokio::sync::Mutex::new(ServerState {
            handoff: Some(handoff.clone()),
            delivered: true,
            ..Default::default()
        }));
        let inner = Arc::new(Sink(std::sync::atomic::AtomicBool::new(false)));
        let sink = ServerSink {
            inner: inner.clone(),
            state,
            directory: receipt_directory,
            stop: Arc::new(tokio::sync::Notify::new()),
            done: CancellationToken::new(),
            template: handoff.clone(),
            spawned: Arc::new(tokio::sync::Notify::new()),
            cancelled: CancellationToken::new(),
        };
        sink.on_wait_failure(&handoff.task_id, "injected wait failure")
            .await;
        assert!(inner.0.load(std::sync::atomic::Ordering::SeqCst));
        assert!(
            !sink.done.is_cancelled(),
            "unverified cleanup must retain the supervisor"
        );
        let receipt: Receipt =
            serde_json::from_slice(&std::fs::read(&handoff.receipt_path).unwrap()).unwrap();
        assert!(!receipt.cleanup_verified);
        assert!(receipt
            .stop_cause
            .as_deref()
            .unwrap()
            .contains("injected wait failure"));
        assert!(terminal_observation_is_valid(&TerminalInfo {
            exit_code: receipt.exit_code,
            stop_cause: receipt.stop_cause,
            killed: receipt.killed,
            leader_exit_verified: receipt.leader_exit_verified,
            cleanup_verified: receipt.cleanup_verified,
        })
        .is_err());
        assert!(
            terminal_cleanup_is_verified(Some(&TerminalInfo {
                exit_code: None,
                stop_cause: None,
                killed: None,
                leader_exit_verified: true,
                cleanup_verified: true,
            }))
            .unwrap(),
            "successful signal wait remains terminal"
        );
        sink.on_cleanup_verified(&handoff.task_id).await;
        assert!(sink.done.is_cancelled());
        let receipt: Receipt =
            serde_json::from_slice(&std::fs::read(&handoff.receipt_path).unwrap()).unwrap();
        assert!(receipt.cleanup_verified);
        assert!(receipt
            .stop_cause
            .unwrap()
            .contains("injected wait failure"));
    }

    #[tokio::test]
    async fn verified_cleanup_retry_uses_identity_matched_receipt_without_losing_cause() {
        struct Sink;
        #[async_trait::async_trait]
        impl BackgroundExitSink for Sink {
            async fn on_exit(&self, _: &str, _: Option<i32>) {
                panic!("a wait failure is not an ordinary exit");
            }
            async fn on_wait_failure(&self, _: &str, cause: &str) {
                assert_eq!(cause, "injected wait failure");
            }
        }

        PLATFORM.get_or_init(|| Arc::new(OfflineReceiptPlatform));
        let directory = tempfile::tempdir().unwrap();
        let mut handoff = fixture(directory.path(), "body");
        handoff.task_id = format!("verified-retry-{}", uuid::Uuid::new_v4());
        handoff.owner = Some(format!("host-tool:{}", uuid::Uuid::new_v4()));
        let owner = handoff.owner.clone().unwrap();
        let handle = ProcessHandle {
            task_id: handoff.task_id.clone(),
            pid: handoff.pid,
        };
        let live = track(handoff.clone(), true);
        let receipt_directory = Path::new(&handoff.receipt_path)
            .parent()
            .unwrap()
            .to_path_buf();
        let sink = ServerSink {
            inner: Arc::new(Sink),
            state: Arc::new(tokio::sync::Mutex::new(ServerState {
                handoff: Some(handoff.clone()),
                delivered: true,
                ..Default::default()
            })),
            directory: receipt_directory.clone(),
            stop: Arc::new(tokio::sync::Notify::new()),
            done: CancellationToken::new(),
            template: handoff.clone(),
            spawned: Arc::new(tokio::sync::Notify::new()),
            cancelled: CancellationToken::new(),
        };

        // A matching receipt from a legacy supervisor can omit the proof bit.
        // Its ordinary-looking exit code is not proof that the owned group or
        // Job was reaped, even though its other identities all match.
        let legacy = Receipt {
            handoff: handoff.clone(),
            exit_code: Some(-1),
            stop_cause: None,
            killed: None,
            leader_exit_verified: false,
            cleanup_verified: false,
        };
        let mut legacy_json = serde_json::to_value(legacy).unwrap();
        let legacy_fields = legacy_json.as_object_mut().unwrap();
        legacy_fields.remove("leader_exit_verified");
        legacy_fields.remove("group_cleanup_verified");
        // Old supervisors used this bit even for leader-only completion.
        legacy_fields.insert("cleanup_verified".into(), true.into());
        std::fs::write(
            &handoff.receipt_path,
            serde_json::to_vec(&legacy_json).unwrap(),
        )
        .unwrap();
        let legacy_terminal = inspect_details(&handoff).await.unwrap().unwrap();
        assert!(!legacy_terminal.leader_exit_verified);
        assert!(!legacy_terminal.cleanup_verified);
        assert!(terminal_cleanup_is_verified(Some(&legacy_terminal)).is_err());
        assert!(terminate_and_wait(&handle, &owner).await.unwrap().is_err());
        assert!(kill(&handle).await.unwrap().is_err());

        sink.on_wait_failure(&handoff.task_id, "injected wait failure")
            .await;
        let error = terminate_and_wait(&handle, &owner)
            .await
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("injected wait failure"));
        assert!(kill(&handle).await.unwrap().is_err());

        sink.on_cleanup_verified(&handoff.task_id).await;
        assert!(terminate_and_wait(&handle, &owner).await.unwrap().is_ok());
        assert!(kill(&handle).await.unwrap().is_ok());
        let receipt: Receipt =
            serde_json::from_slice(&std::fs::read(&handoff.receipt_path).unwrap()).unwrap();
        assert!(receipt.cleanup_verified);
        assert_eq!(
            receipt.stop_cause.as_deref(),
            Some("wait_unverified:injected wait failure")
        );

        let mut wrong_identity = handoff.clone();
        wrong_identity.nonce = "b".repeat(48);
        write_receipt(
            &receipt_directory,
            &Receipt {
                handoff: wrong_identity,
                exit_code: None,
                stop_cause: Some("wait_unverified:injected wait failure".into()),
                killed: None,
                leader_exit_verified: true,
                cleanup_verified: true,
            },
        )
        .unwrap();
        assert!(terminate_and_wait(&handle, &owner).await.unwrap().is_err());

        drop(live);
        jobs().lock().unwrap().remove(&handle.task_id);
    }

    #[tokio::test]
    async fn unverified_stop_retries_the_live_supervisor_and_waits_for_proof() {
        #[derive(Default)]
        struct Sink(Mutex<Vec<String>>);
        #[async_trait::async_trait]
        impl BackgroundExitSink for Sink {
            async fn on_exit(&self, _: &str, _: Option<i32>) {
                panic!("unverified cleanup is not an ordinary exit");
            }
            async fn on_wait_failure(&self, _: &str, reason: &str) {
                self.0.lock().unwrap().push(format!("failure:{reason}"));
            }
            async fn on_exit_outcome_unknown(&self, _: &str, _: Option<i32>) {
                self.0.lock().unwrap().push("verified unknown".into());
            }
        }

        PLATFORM.get_or_init(|| Arc::new(OfflineReceiptPlatform));
        let directory = tempfile::tempdir().unwrap();
        let mut handoff = fixture(directory.path(), "body");
        handoff.task_id = format!("retry-live-{}", uuid::Uuid::new_v4());
        handoff.owner = Some(format!("host-tool:{}", uuid::Uuid::new_v4()));
        let owner = handoff.owner.clone().unwrap();
        let handle = ProcessHandle {
            task_id: handoff.task_id.clone(),
            pid: handoff.pid,
        };
        let live = track(handoff.clone(), true);
        let state = Arc::new(tokio::sync::Mutex::new(ServerState {
            handoff: Some(handoff.clone()),
            backgrounded: true,
            delivered: true,
            ..Default::default()
        }));
        let kill = Arc::new(tokio::sync::Notify::new());
        let inner = Arc::new(Sink::default());
        let sink = ServerSink {
            inner: inner.clone(),
            state: state.clone(),
            directory: Path::new(&handoff.receipt_path).parent().unwrap().into(),
            stop: kill.clone(),
            done: CancellationToken::new(),
            template: handoff.clone(),
            spawned: Arc::new(tokio::sync::Notify::new()),
            cancelled: CancellationToken::new(),
        };
        sink.on_wait_failure(&handoff.task_id, "injected first stop failure")
            .await;

        let listener = tokio::net::UnixListener::bind(&handoff.socket_path).unwrap();
        let server_state = state.clone();
        let server_kill = kill.clone();
        let server_nonce = handoff.nonce.clone();
        let server = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let mut stream: BoxStream = Box::new(stream);
                let request: Request = receive(&mut stream).await.unwrap();
                let current = server_state.lock().await;
                let response = match request {
                    Request::Inspect { nonce } if nonce == server_nonce => Response::Inspect {
                        handoff: current.handoff.clone().unwrap(),
                        terminal: current.exit_code.is_some()
                            && !current.terminal_notification_pending,
                        exit_code: current.exit_code.flatten(),
                        stop_cause: current.stop_cause.clone(),
                        killed: current.terminal_killed,
                        leader_exit_verified: current.leader_exit_verified,
                        cleanup_verified: current.cleanup_verified,
                    },
                    Request::Kill { nonce } if nonce == server_nonce => {
                        signal_server_stop(&current, &server_kill, &CancellationToken::new())
                            .unwrap();
                        Response::Ack
                    }
                    _ => panic!("unexpected supervisor request"),
                };
                drop(current);
                send(&mut stream, &response).await.unwrap();
            }
        });

        let retry_handle = handle.clone();
        let retry_owner = owner.clone();
        let mut retry = tokio::spawn(async move {
            terminate_and_wait(&retry_handle, &retry_owner)
                .await
                .unwrap()
        });
        tokio::time::timeout(Duration::from_secs(1), kill.notified())
            .await
            .expect("a retry must signal the held runner");
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut retry)
                .await
                .is_err(),
            "a repeated stop alone is not verified cleanup"
        );
        sink.on_cleanup_verified(&handoff.task_id).await;
        tokio::time::timeout(Duration::from_secs(1), retry)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let receipt: Receipt =
            serde_json::from_slice(&std::fs::read(&handoff.receipt_path).unwrap()).unwrap();
        assert!(receipt.cleanup_verified);
        assert_eq!(
            receipt.stop_cause.as_deref(),
            Some("wait_unverified:injected first stop failure")
        );
        assert_eq!(
            *inner.0.lock().unwrap(),
            vec!["failure:injected first stop failure", "verified unknown"]
        );
        server.abort();
        drop(live);
        jobs().lock().unwrap().remove(&handle.task_id);
    }

    #[tokio::test]
    async fn leader_only_natural_exit_is_not_group_cleanup_proof() {
        #[derive(Default)]
        struct Sink(Mutex<Vec<&'static str>>);
        #[async_trait::async_trait]
        impl BackgroundExitSink for Sink {
            async fn on_exit(&self, _: &str, _: Option<i32>) {
                self.0.lock().unwrap().push("natural");
            }
            async fn on_wait_failure(&self, _: &str, _: &str) {
                self.0.lock().unwrap().push("unverified");
            }
            async fn on_cleanup_verified(&self, _: &str) {
                self.0.lock().unwrap().push("verified unknown");
            }
        }

        PLATFORM.get_or_init(|| Arc::new(OfflineReceiptPlatform));
        for require_group in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let mut handoff = fixture(directory.path(), "body");
            handoff.task_id = format!("leader-only-{}", uuid::Uuid::new_v4());
            handoff.owner = Some(format!("test-owner:{}", uuid::Uuid::new_v4()));
            handoff.require_group_cleanup_on_exit = require_group;
            let handle = ProcessHandle {
                task_id: handoff.task_id.clone(),
                pid: handoff.pid,
            };
            let live = track(handoff.clone(), true);
            let inner = Arc::new(Sink::default());
            let sink = ServerSink {
                inner: inner.clone(),
                state: Arc::new(tokio::sync::Mutex::new(ServerState {
                    handoff: Some(handoff.clone()),
                    backgrounded: true,
                    delivered: true,
                    ..Default::default()
                })),
                directory: Path::new(&handoff.receipt_path).parent().unwrap().into(),
                stop: Arc::new(tokio::sync::Notify::new()),
                done: CancellationToken::new(),
                template: handoff.clone(),
                spawned: Arc::new(tokio::sync::Notify::new()),
                cancelled: CancellationToken::new(),
            };
            sink.on_exit_with_proof(&handoff.task_id, Some(0), Some(false), false)
                .await;
            let receipt: Receipt =
                serde_json::from_slice(&std::fs::read(&handoff.receipt_path).unwrap()).unwrap();
            assert!(receipt.leader_exit_verified);
            assert!(!receipt.cleanup_verified);
            assert_eq!(sink.done.is_cancelled(), !require_group);
            let terminal = inspect_details(&handoff).await.unwrap().unwrap();
            assert!(terminal_cleanup_is_verified(Some(&terminal)).is_err());
            let observer = Sink::default();
            let mut unverified = false;
            let delivered = deliver_supervised_terminal(
                &observer,
                &handoff.task_id,
                &terminal,
                require_group,
                &mut unverified,
            )
            .await;
            assert_eq!(delivered, !require_group);
            if require_group {
                assert_eq!(*observer.0.lock().unwrap(), vec!["unverified"]);
                sink.on_cleanup_verified(&handoff.task_id).await;
                let verified = inspect_details(&handoff).await.unwrap().unwrap();
                assert!(terminal_cleanup_is_verified(Some(&verified)).unwrap());
                assert!(
                    deliver_supervised_terminal(
                        &observer,
                        &handoff.task_id,
                        &verified,
                        true,
                        &mut unverified,
                    )
                    .await
                );
                assert_eq!(
                    *observer.0.lock().unwrap(),
                    vec!["unverified", "verified unknown"]
                );
            } else {
                assert_eq!(*observer.0.lock().unwrap(), vec!["natural"]);
                sink.on_exit_with_proof(&handoff.task_id, Some(0), Some(false), false)
                    .await;
                sink.on_wait_failure(&handoff.task_id, "late failure").await;
                assert_eq!(*inner.0.lock().unwrap(), vec!["natural"]);
                let failure = terminate_and_wait(&handle, handoff.owner.as_deref().unwrap())
                    .await
                    .unwrap()
                    .unwrap_err();
                assert!(failure.to_string().contains("without group cleanup proof"));
                assert!(kill(&handle).await.unwrap().is_err());
            }
            let mut wrong_policy = handoff.clone();
            wrong_policy.require_group_cleanup_on_exit = !require_group;
            assert!(inspect_details(&wrong_policy).await.is_err());
            drop(live);
            jobs().lock().unwrap().remove(&handle.task_id);
        }
    }

    #[tokio::test]
    async fn verified_negative_one_exit_is_not_a_legacy_recovery_sentinel() {
        #[derive(Default)]
        struct Sink(Mutex<Vec<Option<i32>>>);
        #[async_trait::async_trait]
        impl BackgroundExitSink for Sink {
            async fn on_exit(&self, _: &str, code: Option<i32>) {
                self.0.lock().unwrap().push(code);
            }
            async fn on_wait_failure(&self, _: &str, cause: &str) {
                panic!("a verified exit was reported as unverified: {cause}");
            }
        }

        PLATFORM.get_or_init(|| Arc::new(OfflineReceiptPlatform));
        for (require_group, group_verified) in [(false, false), (true, true)] {
            let directory = tempfile::tempdir().unwrap();
            let mut handoff = fixture(directory.path(), "body");
            handoff.task_id = format!("negative-exit-{}", uuid::Uuid::new_v4());
            handoff.owner = Some(format!("exit-owner:{}", uuid::Uuid::new_v4()));
            handoff.require_group_cleanup_on_exit = require_group;
            let handle = ProcessHandle {
                task_id: handoff.task_id.clone(),
                pid: handoff.pid,
            };
            let live = track(handoff.clone(), true);
            let sink = ServerSink {
                inner: Arc::new(Sink::default()),
                state: Arc::new(tokio::sync::Mutex::new(ServerState {
                    handoff: Some(handoff.clone()),
                    backgrounded: true,
                    delivered: true,
                    ..Default::default()
                })),
                directory: Path::new(&handoff.receipt_path).parent().unwrap().into(),
                stop: Arc::new(tokio::sync::Notify::new()),
                done: CancellationToken::new(),
                template: handoff.clone(),
                spawned: Arc::new(tokio::sync::Notify::new()),
                cancelled: CancellationToken::new(),
            };
            sink.on_exit_with_proof(&handoff.task_id, Some(-1), Some(false), group_verified)
                .await;
            let terminal = inspect_details(&handoff).await.unwrap().unwrap();
            assert!(terminal.leader_exit_verified);
            assert_eq!(terminal.cleanup_verified, group_verified);
            let observer = Sink::default();
            let mut unverified = false;
            assert!(
                deliver_supervised_terminal(
                    &observer,
                    &handoff.task_id,
                    &terminal,
                    require_group,
                    &mut unverified,
                )
                .await
            );
            assert_eq!(*observer.0.lock().unwrap(), vec![Some(-1)]);
            let stop = terminate_and_wait(&handle, handoff.owner.as_deref().unwrap())
                .await
                .unwrap();
            assert_eq!(stop.is_ok(), group_verified);
            drop(live);
            jobs().lock().unwrap().remove(&handle.task_id);
        }
    }

    #[tokio::test]
    async fn group_proof_is_published_once_after_terminal_output_finishes() {
        struct Sink {
            entered: tokio::sync::Notify,
            release: tokio::sync::Notify,
            calls: std::sync::atomic::AtomicUsize,
        }
        #[async_trait::async_trait]
        impl BackgroundExitSink for Sink {
            async fn on_exit(&self, _: &str, _: Option<i32>) {}
            async fn on_exit_with_status(&self, _: &str, _: Option<i32>, _: bool) {
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.entered.notify_one();
                self.release.notified().await;
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let handoff = fixture(directory.path(), "body");
        let inner = Arc::new(Sink {
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let sink = Arc::new(ServerSink {
            inner: inner.clone(),
            state: Arc::new(tokio::sync::Mutex::new(ServerState {
                handoff: Some(handoff.clone()),
                backgrounded: true,
                delivered: true,
                ..Default::default()
            })),
            directory: Path::new(&handoff.receipt_path).parent().unwrap().into(),
            stop: Arc::new(tokio::sync::Notify::new()),
            done: CancellationToken::new(),
            template: handoff.clone(),
            spawned: Arc::new(tokio::sync::Notify::new()),
            cancelled: CancellationToken::new(),
        });
        let first = {
            let sink = sink.clone();
            let task_id = handoff.task_id.clone();
            tokio::spawn(async move {
                sink.on_exit_with_proof(&task_id, Some(0), Some(false), true)
                    .await;
            })
        };
        inner.entered.notified().await;
        {
            let mut state = sink.state.lock().await;
            assert!(state.terminal_notification_pending);
            assert!(!state.cleanup_verified);
            assert!(!server_can_finish(&state));
            persist_server_receipt(&mut state, &sink.directory);
        }
        assert!(!Path::new(&handoff.receipt_path).exists());
        sink.on_exit_with_proof(&handoff.task_id, Some(0), Some(false), true)
            .await;
        assert_eq!(inner.calls.load(Ordering::SeqCst), 1);
        inner.release.notify_one();
        first.await.unwrap();
        let receipt: Receipt =
            serde_json::from_slice(&std::fs::read(&handoff.receipt_path).unwrap()).unwrap();
        assert!(receipt.leader_exit_verified && receipt.cleanup_verified);
        assert!(sink.done.is_cancelled());
    }

    #[tokio::test]
    async fn deadline_and_natural_callbacks_cannot_publish_an_unproved_exit() {
        #[derive(Default)]
        struct Sink(Mutex<Vec<&'static str>>);
        #[async_trait::async_trait]
        impl BackgroundExitSink for Sink {
            async fn on_exit(&self, _: &str, _: Option<i32>) {
                self.0.lock().unwrap().push("natural");
            }
            async fn on_wait_failure(&self, _: &str, _: &str) {
                self.0.lock().unwrap().push("unverified");
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let mut handoff = fixture(directory.path(), "body");
        handoff.require_group_cleanup_on_exit = false;
        let inner = Arc::new(Sink::default());
        let sink = Arc::new(ServerSink {
            inner: inner.clone(),
            state: Arc::new(tokio::sync::Mutex::new(ServerState {
                handoff: Some(handoff.clone()),
                backgrounded: true,
                delivered: true,
                ..Default::default()
            })),
            directory: Path::new(&handoff.receipt_path).parent().unwrap().into(),
            stop: Arc::new(tokio::sync::Notify::new()),
            done: CancellationToken::new(),
            template: handoff.clone(),
            spawned: Arc::new(tokio::sync::Notify::new()),
            cancelled: CancellationToken::new(),
        });
        // Queue the natural callback between the deadline's cause publication
        // and its own terminal callback. Tokio's mutex admits waiters FIFO.
        let locked = sink.state.lock().await;
        let deadline_ready = Arc::new(tokio::sync::Notify::new());
        let deadline = {
            let sink = sink.clone();
            let ready = deadline_ready.clone();
            let id = handoff.task_id.clone();
            tokio::spawn(async move {
                ready.notify_one();
                sink.on_background_deadline_exit(&id, Some(0)).await;
            })
        };
        deadline_ready.notified().await;
        let natural_ready = Arc::new(tokio::sync::Notify::new());
        let natural = {
            let sink = sink.clone();
            let ready = natural_ready.clone();
            let id = handoff.task_id.clone();
            tokio::spawn(async move {
                ready.notify_one();
                sink.on_exit_with_proof(&id, Some(0), Some(false), false)
                    .await;
            })
        };
        natural_ready.notified().await;
        drop(locked);
        deadline.await.unwrap();
        natural.await.unwrap();
        assert_eq!(*inner.0.lock().unwrap(), vec!["unverified"]);
        let state = sink.state.lock().await;
        assert!(!server_can_finish(&state));
        assert!(!state.cleanup_verified);
        assert!(state
            .stop_cause
            .as_deref()
            .is_some_and(|cause| cause.starts_with(UNVERIFIED_WAIT_PREFIX)));
        drop(state);

        // Conversely, an already published natural completion must not be
        // rewritten by a late deadline callback.
        let other = tempfile::tempdir().unwrap();
        let mut h = fixture(other.path(), "body");
        h.require_group_cleanup_on_exit = false;
        let inner = Arc::new(Sink::default());
        let sink = ServerSink {
            inner: inner.clone(),
            state: Arc::new(tokio::sync::Mutex::new(ServerState {
                handoff: Some(h.clone()),
                backgrounded: true,
                delivered: true,
                ..Default::default()
            })),
            directory: Path::new(&h.receipt_path).parent().unwrap().into(),
            stop: Arc::new(tokio::sync::Notify::new()),
            done: CancellationToken::new(),
            template: h.clone(),
            spawned: Arc::new(tokio::sync::Notify::new()),
            cancelled: CancellationToken::new(),
        };
        sink.on_exit_with_proof(&h.task_id, Some(0), Some(false), false)
            .await;
        sink.on_background_deadline_exit(&h.task_id, Some(0)).await;
        let state = sink.state.lock().await;
        assert!(server_can_finish(&state));
        assert_eq!(state.stop_cause, None);
        assert_eq!(*inner.0.lock().unwrap(), vec!["natural"]);
    }

    #[tokio::test]
    async fn verified_supervisor_outcome_survives_receipt_and_observer_delivery() {
        #[derive(Default)]
        struct Sink(Mutex<Vec<String>>);
        #[async_trait::async_trait]
        impl BackgroundExitSink for Sink {
            async fn on_supervised_start(&self, _: &str) {
                self.0.lock().unwrap().push("supervised".into());
            }
            async fn on_exit(&self, _: &str, code: Option<i32>) {
                self.0.lock().unwrap().push(format!("exit:{code:?}"));
            }
            async fn on_exit_with_status(&self, _: &str, code: Option<i32>, killed: bool) {
                self.0
                    .lock()
                    .unwrap()
                    .push(format!("status:{code:?}:{killed}"));
            }
            async fn on_exit_outcome_unknown(&self, _: &str, code: Option<i32>) {
                self.0.lock().unwrap().push(format!("unknown:{code:?}"));
            }
        }

        for (killed, code) in [(Some(true), None), (Some(false), Some(0)), (None, Some(0))] {
            let directory = tempfile::tempdir().unwrap();
            let handoff = fixture(directory.path(), "body");
            let source = Arc::new(Sink::default());
            let server = ServerSink {
                inner: source,
                state: Arc::new(tokio::sync::Mutex::new(ServerState {
                    handoff: Some(handoff.clone()),
                    delivered: true,
                    ..Default::default()
                })),
                directory: Path::new(&handoff.receipt_path).parent().unwrap().into(),
                stop: Arc::new(tokio::sync::Notify::new()),
                done: CancellationToken::new(),
                template: handoff.clone(),
                spawned: Arc::new(tokio::sync::Notify::new()),
                cancelled: CancellationToken::new(),
            };
            assert!(!server.inner.requires_verified_supervised_stop());
            assert!(server.requires_verified_supervised_stop());
            // A queued stop is not proof the native runner handled it: the
            // runner's verified result, including unknown, decides the receipt.
            server.stop.notify_one();
            server
                .on_exit_with_proof(&handoff.task_id, code, killed, true)
                .await;
            let receipt: Receipt =
                serde_json::from_slice(&std::fs::read(&handoff.receipt_path).unwrap()).unwrap();
            assert_eq!(receipt.killed, killed);
            assert!(receipt.cleanup_verified);
            let wire = serde_json::to_vec(&Response::Inspect {
                handoff: handoff.clone(),
                terminal: true,
                exit_code: code,
                stop_cause: None,
                killed,
                leader_exit_verified: true,
                cleanup_verified: true,
            })
            .unwrap();
            match serde_json::from_slice::<Response>(&wire).unwrap() {
                Response::Inspect { killed: actual, .. } => assert_eq!(actual, killed),
                _ => panic!("inspection response changed variant"),
            }
            let (terminal, lost) = recover_failed_output(&handoff).await.unwrap();
            assert!(!lost);
            assert_eq!(terminal.killed, killed);

            let observer = Sink::default();
            let mut unverified = false;
            assert!(
                deliver_supervised_terminal(
                    &observer,
                    &handoff.task_id,
                    &terminal,
                    handoff.require_group_cleanup_on_exit,
                    &mut unverified,
                )
                .await
            );
            let events = observer.0.lock().unwrap();
            match killed {
                Some(true) => assert_eq!(*events, ["supervised", "status:None:true"]),
                Some(false) => assert_eq!(*events, ["exit:Some(0)"]),
                None => assert_eq!(*events, ["supervised", "unknown:Some(0)"]),
            }
        }
    }

    #[tokio::test]
    async fn deadline_history_does_not_override_the_verified_exit_outcome() {
        #[derive(Default)]
        struct Sink(Mutex<Vec<&'static str>>);
        #[async_trait::async_trait]
        impl BackgroundExitSink for Sink {
            async fn on_exit(&self, _: &str, _: Option<i32>) {
                self.0.lock().unwrap().push("natural");
            }
            async fn on_exit_outcome_unknown(&self, _: &str, _: Option<i32>) {
                self.0.lock().unwrap().push("unknown");
            }
            async fn on_supervised_background_deadline_exit(&self, _: &str, _: Option<i32>) {
                self.0.lock().unwrap().push("deadline killed");
            }
        }
        for (outcome, expected) in [
            (Some(true), "deadline killed"),
            (Some(false), "natural"),
            (None, "unknown"),
        ] {
            let sink = Sink::default();
            notify_supervised_exit(&sink, "task", Some(0), Some("deadline"), outcome).await;
            assert_eq!(*sink.0.lock().unwrap(), vec![expected]);
        }
    }

    #[tokio::test]
    async fn background_deadline_survives_receipt_recovery_and_supervised_delivery() {
        struct Sink(std::sync::atomic::AtomicBool);
        #[async_trait::async_trait]
        impl BackgroundExitSink for Sink {
            async fn on_exit(&self, _: &str, _: Option<i32>) {
                panic!("deadline must retain its cause");
            }
            async fn on_background_deadline_exit(&self, _: &str, code: Option<i32>) {
                assert_eq!(code, None);
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let h = fixture(dir.path(), "before\n[killed]\n");
        write_receipt(
            Path::new(&h.receipt_path).parent().unwrap(),
            &Receipt {
                handoff: h.clone(),
                exit_code: None,
                stop_cause: Some("deadline".into()),
                killed: Some(true),
                leader_exit_verified: true,
                cleanup_verified: true,
            },
        )
        .unwrap();
        let (terminal, lost) = recover_failed_output(&h).await.unwrap();
        assert!(!lost);
        assert_eq!(terminal.stop_cause.as_deref(), Some("deadline"));
        assert_eq!(terminal.killed, Some(true));
        let sink = Sink(std::sync::atomic::AtomicBool::new(false));
        notify_supervised_exit(
            &sink,
            &h.task_id,
            terminal.exit_code,
            terminal.stop_cause.as_deref(),
            terminal.killed,
        )
        .await;
        assert!(sink.0.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn recovery_preserves_late_receipt_and_deduplicates_terminal_output() {
        let dir = tempfile::tempdir().unwrap();
        let h = fixture(dir.path(), "body\n[exited with code 7]\n");
        let parent = Path::new(&h.receipt_path).parent().unwrap();
        write_receipt(
            parent,
            &Receipt {
                handoff: h.clone(),
                exit_code: Some(7),
                stop_cause: None,
                killed: Some(false),
                leader_exit_verified: true,
                cleanup_verified: true,
            },
        )
        .unwrap();
        assert_eq!(
            recover_failed_output(&h).await.unwrap(),
            (
                TerminalInfo {
                    exit_code: Some(7),
                    stop_cause: None,
                    killed: Some(false),
                    leader_exit_verified: true,
                    cleanup_verified: true,
                },
                false,
            )
        );
        std::fs::remove_file(&h.receipt_path).unwrap();
        assert_eq!(
            recover_failed_output(&h).await.unwrap(),
            (
                TerminalInfo {
                    exit_code: Some(-1),
                    stop_cause: Some(UNVERIFIED_SUPERVISOR_LOSS.into()),
                    killed: None,
                    leader_exit_verified: false,
                    cleanup_verified: false,
                },
                true,
            )
        );
        assert_eq!(
            recover_failed_output(&h).await.unwrap(),
            (
                TerminalInfo {
                    exit_code: Some(-1),
                    stop_cause: Some(UNVERIFIED_SUPERVISOR_LOSS.into()),
                    killed: None,
                    leader_exit_verified: false,
                    cleanup_verified: false,
                },
                false,
            )
        );
        assert_eq!(
            std::fs::read_to_string(&h.output_path).unwrap(),
            "body\n[exited with code 7]\n"
        );
        let unknown = tempfile::tempdir().unwrap();
        let h = fixture(unknown.path(), "body\n[exited; outcome unknown]\n");
        let (terminal, lost) = recover_failed_output(&h).await.unwrap();
        assert!(lost);
        assert_eq!(
            terminal.stop_cause.as_deref(),
            Some(UNVERIFIED_SUPERVISOR_LOSS)
        );
        assert!(!terminal.cleanup_verified);
        assert_eq!(
            std::fs::read_to_string(&h.output_path).unwrap(),
            "body\n[exited; outcome unknown]\n"
        );
        let fresh = tempfile::tempdir().unwrap();
        let h = fixture(fresh.path(), "body");
        let (a, b) = tokio::join!(recover_failed_output(&h), recover_failed_output(&h));
        assert!(a.is_ok() && b.is_ok());
        assert_eq!(
            std::fs::read_to_string(&h.output_path)
                .unwrap()
                .matches(LOST_SUPERVISOR_FOOTER)
                .count(),
            1
        );
    }
    #[tokio::test]
    async fn recovery_rejects_root_file_and_receipt_replacement_without_writes() {
        for replacement in ["root", "file", "receipt"] {
            let dir = tempfile::tempdir().unwrap();
            let h = fixture(dir.path(), "original");
            let output = Path::new(&h.output_path);
            let root = output.parent().unwrap();
            match replacement {
                "root" => {
                    std::fs::rename(root, dir.path().join("saved-root")).unwrap();
                    std::fs::create_dir(root).unwrap();
                    std::fs::write(output, "sentinel").unwrap();
                }
                "file" => {
                    std::fs::rename(output, root.join("saved-file")).unwrap();
                    std::fs::write(output, "sentinel").unwrap();
                }
                _ => {
                    let mut conflict = h.clone();
                    conflict.nonce = "different".into();
                    write_receipt(
                        Path::new(&h.receipt_path).parent().unwrap(),
                        &Receipt {
                            handoff: conflict,
                            exit_code: Some(0),
                            stop_cause: None,
                            killed: Some(false),
                            leader_exit_verified: true,
                            cleanup_verified: true,
                        },
                    )
                    .unwrap();
                }
            }
            let before: std::collections::BTreeSet<_> = std::fs::read_dir(root)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
            let body = std::fs::read(output).unwrap();
            assert!(recover_failed_output(&h).await.is_err(), "{replacement}");
            assert_eq!(std::fs::read(output).unwrap(), body, "{replacement}");
            assert_eq!(
                std::fs::read_dir(root)
                    .unwrap()
                    .map(|entry| entry.unwrap().file_name())
                    .collect::<std::collections::BTreeSet<_>>(),
                before,
                "no replacement file or lock may be created"
            );
        }
    }
}

#[cfg(test)]
mod cancellation_join_tests {
    use super::*;

    fn handoff() -> ShellProcessHandoff {
        ShellProcessHandoff {
            task_id: "cancel-race".into(),
            pid: 123,
            supervisor_pid: 456,
            supervisor_start_identity: None,
            process_start_identity: None,
            owner: Some("host-tool:test".into()),
            require_group_cleanup_on_exit: true,
            supervisor_directory_identity: None,
            output_root_identity: None,
            output_file_identity: None,
            socket_path: String::new(),
            receipt_path: String::new(),
            output_path: String::new(),
            nonce: String::new(),
        }
    }

    fn moved() -> ForegroundRunResult {
        ForegroundRunResult {
            outcome: ForegroundOutcome::MovedToBackground(ProcessHandle {
                task_id: "cancel-race".into(),
                pid: 123,
            }),
            output_file: None,
        }
    }

    #[tokio::test]
    async fn stalled_background_rpc_does_not_block_spawn_or_start_receipts() {
        let (start_client, start_server) = tokio::io::duplex(4096);
        let (background_client, background_server) = tokio::io::duplex(4096);
        let mut start_client: BoxStream = Box::new(start_client);
        let mut start_server: BoxStream = Box::new(start_server);
        let mut background_client: BoxStream = Box::new(background_client);
        let mut background_server: BoxStream = Box::new(background_server);
        let background_received = Arc::new(tokio::sync::Notify::new());
        let server_received = background_received.clone();
        let server = tokio::spawn(async move {
            assert!(matches!(
                receive::<Request>(&mut background_server).await,
                Ok(Request::Background { .. })
            ));
            server_received.notify_one();
            // A delivered request may never receive its RPC acknowledgement.
            std::future::pending::<()>().await;
        });
        let (spawn_tx, spawn_rx) = tokio::sync::oneshot::channel();
        let spawn_accepted = Arc::new(tokio::sync::Notify::new());
        let accepted = spawn_accepted.clone();
        let driver = tokio::spawn(async move {
            await_start_response(
                receive::<Response>(&mut start_client),
                async { spawn_rx.await.map_err(error) },
                async {
                    send(
                        &mut background_client,
                        &Request::Background {
                            nonce: "test".into(),
                        },
                    )
                    .await?;
                    receive::<Response>(&mut background_client).await
                },
                move |_| async move {
                    accepted.notify_one();
                    Ok(())
                },
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(1), background_received.notified())
            .await
            .expect("background request must be delivered before its RPC stalls");
        assert!(spawn_tx
            .send(Response::Spawned { handoff: handoff() })
            .is_ok());
        tokio::time::timeout(Duration::from_secs(1), spawn_accepted.notified())
            .await
            .expect("a stalled background RPC must not block spawn acceptance");
        assert!(!driver.is_finished());
        send(
            &mut start_server,
            &Response::Started {
                result: ForegroundRunResult {
                    outcome: ForegroundOutcome::Completed(mobile_linux_api::ProcessOutput {
                        stdout: "done".into(),
                        stderr: String::new(),
                        exit_code: 0,
                        timed_out: false,
                    }),
                    output_file: None,
                },
                handoff: None,
            },
        )
        .await
        .unwrap();
        let response = tokio::time::timeout(Duration::from_secs(1), driver)
            .await
            .expect("the real Start receipt must remain readable")
            .unwrap()
            .unwrap();
        assert!(matches!(
            response,
            Response::Started {
                result: ForegroundRunResult {
                    outcome: ForegroundOutcome::Completed(_),
                    ..
                },
                handoff: None,
            }
        ));
        server.abort();
    }

    #[test]
    fn wire_errors_preserve_owned_cleanup_markers() {
        assert_eq!(
            response_error_message(ProcessError::Io("supervisor command cancelled".into())),
            "supervisor command cancelled"
        );
        let ProcessError::Io(message) = unverified_shell_error("connection failed") else {
            panic!("expected an Io marker");
        };
        assert!(message.starts_with(crate::host::process::OWNED_SHELL_CLEANUP_UNVERIFIED));
        assert!(message.contains("connection failed"));

        let older: Response =
            serde_json::from_str(r#"{"response":"Error","message":"missing evidence"}"#).unwrap();
        assert!(matches!(
            older,
            Response::Error {
                no_child_started: false,
                ..
            }
        ));
        let before_spawn: Response = serde_json::from_str(
            r#"{"response":"Error","message":"invalid output","no_child_started":true}"#,
        )
        .unwrap();
        assert!(matches!(
            before_spawn,
            Response::Error {
                no_child_started: true,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn cancellation_receipt_waits_for_the_retained_runner_to_settle() {
        let completed = Arc::new(tokio::sync::Notify::new());
        let release = completed.clone();
        let run = async move {
            release.notified().await;
            Ok(ForegroundRunResult {
                outcome: ForegroundOutcome::Completed(mobile_linux_api::ProcessOutput {
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: 0,
                    timed_out: false,
                }),
                output_file: None,
            })
        };
        tokio::pin!(run);
        let signals = std::sync::atomic::AtomicUsize::new(0);
        let state = tokio::sync::Mutex::new(ServerState::default());
        let stopping = cancel_and_join_runner(run.as_mut(), &state, || {
            signals.fetch_add(1, Ordering::SeqCst);
        });
        tokio::pin!(stopping);
        assert!(
            tokio::time::timeout(Duration::from_millis(25), &mut stopping)
                .await
                .is_err()
        );
        assert!(signals.load(Ordering::SeqCst) > 0);
        completed.notify_one();
        assert!(tokio::time::timeout(Duration::from_secs(1), stopping)
            .await
            .unwrap()
            .is_ok());
    }

    #[tokio::test]
    async fn cancellation_after_background_handoff_waits_for_the_real_terminal_report() {
        struct Sink;
        #[async_trait::async_trait]
        impl BackgroundExitSink for Sink {
            async fn on_exit(&self, _: &str, _: Option<i32>) {}
        }

        let state = Arc::new(tokio::sync::Mutex::new(ServerState {
            handoff: Some(handoff()),
            ..Default::default()
        }));
        let directory = tempfile::tempdir().unwrap();
        let sink = ServerSink {
            inner: Arc::new(Sink),
            state: state.clone(),
            directory: directory.path().into(),
            stop: Arc::new(tokio::sync::Notify::new()),
            done: CancellationToken::new(),
            template: handoff(),
            spawned: Arc::new(tokio::sync::Notify::new()),
            cancelled: CancellationToken::new(),
        };
        let run = async { Ok(moved()) };
        tokio::pin!(run);
        let signals = std::sync::atomic::AtomicUsize::new(0);
        let stopping = cancel_and_join_runner(run.as_mut(), &state, || {
            signals.fetch_add(1, Ordering::SeqCst);
        });
        tokio::pin!(stopping);
        assert!(
            tokio::time::timeout(Duration::from_millis(30), &mut stopping)
                .await
                .is_err()
        );
        assert!(signals.load(Ordering::SeqCst) > 0);
        assert!(!state.lock().await.cleanup_verified);
        // The production callback records the terminal value and the proof
        // together. A bare exit_code assignment is not a cleanup receipt.
        sink.on_exit_with_proof("cancel-race", None, None, true)
            .await;
        assert!(state.lock().await.cleanup_verified);
        tokio::time::timeout(Duration::from_secs(1), stopping)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn cancelled_handoff_rejects_unverified_or_wrong_identity() {
        for wrong_identity in [false, true] {
            let mut h = handoff();
            if wrong_identity {
                h.pid += 1;
            }
            let state = tokio::sync::Mutex::new(ServerState {
                handoff: Some(h),
                exit_code: Some(None),
                stop_cause: Some("wait_unverified:injected".into()),
                ..Default::default()
            });
            let run = async { Ok(moved()) };
            tokio::pin!(run);
            let result = cancel_and_join_runner(run.as_mut(), &state, || {}).await;
            let Err(ProcessError::Io(message)) = result else {
                panic!("unverified handoff must fail");
            };
            assert!(message.starts_with(crate::host::process::OWNED_SHELL_CLEANUP_UNVERIFIED));
            assert!(message.contains(if wrong_identity {
                "identity"
            } else {
                "injected"
            }));
        }
    }

    #[tokio::test]
    async fn unpublished_spawn_failure_finishes_only_after_delivery_and_verified_cleanup() {
        struct Sink;
        #[async_trait::async_trait]
        impl BackgroundExitSink for Sink {
            async fn on_exit(&self, _: &str, _: Option<i32>) {
                panic!("an unverified spawn is not an ordinary exit");
            }
            async fn on_wait_failure(&self, _: &str, cause: &str) {
                assert_eq!(cause, "suspended child cleanup failed");
            }
        }

        for verified_before_delivery in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let state = Arc::new(tokio::sync::Mutex::new(ServerState::default()));
            let done = CancellationToken::new();
            let sink = ServerSink {
                inner: Arc::new(Sink),
                state: state.clone(),
                directory: directory.path().to_path_buf(),
                stop: Arc::new(tokio::sync::Notify::new()),
                done: done.clone(),
                template: handoff(),
                spawned: Arc::new(tokio::sync::Notify::new()),
                cancelled: CancellationToken::new(),
            };

            sink.on_wait_failure("cancel-race", "suspended child cleanup failed")
                .await;
            assert!(!done.is_cancelled());
            if verified_before_delivery {
                sink.on_cleanup_verified("cancel-race").await;
                assert!(!done.is_cancelled(), "the Start response is not finished");
            }
            {
                let mut current = state.lock().await;
                current.delivered = true;
                if verified_before_delivery {
                    assert!(server_can_finish(&current));
                    done.cancel(); // The Start response's finish branch.
                } else {
                    assert!(!server_can_finish(&current));
                }
            }
            if !verified_before_delivery {
                sink.on_cleanup_verified("cancel-race").await;
            }
            assert!(done.is_cancelled());
            assert!(!directory.path().join("receipt").exists());
            let current = state.lock().await;
            assert!(current.handoff.is_none());
            assert!(current
                .stop_cause
                .as_deref()
                .unwrap()
                .contains("suspended child cleanup failed"));
        }
    }

    #[test]
    fn published_handoff_still_requires_a_persisted_receipt_after_cleanup() {
        let mut state = ServerState {
            handoff: Some(handoff()),
            exit_code: Some(None),
            delivered: true,
            stop_cause: Some("wait_unverified:injected".into()),
            leader_exit_verified: true,
            cleanup_verified: true,
            ..Default::default()
        };
        assert!(!server_can_finish(&state));
        state.receipt_persisted = true;
        assert!(server_can_finish(&state));
        state.cleanup_verified = false;
        state.stop_cause = None;
        assert!(!server_can_finish(&state), "a receipt alone is not proof");

        state.handoff = None;
        state.exit_code = None;
        assert!(
            !server_can_finish(&state),
            "a prior failure must be recorded"
        );
    }

    #[tokio::test]
    async fn unverified_observation_is_reported_once_and_kept_until_verified() {
        #[derive(Default)]
        struct Sink(Mutex<Vec<String>>);
        #[async_trait::async_trait]
        impl BackgroundExitSink for Sink {
            async fn on_exit(&self, _: &str, _: Option<i32>) {
                self.0.lock().unwrap().push("exit".into());
            }
            async fn on_wait_failure(&self, _: &str, cause: &str) {
                self.0.lock().unwrap().push(format!("failure:{cause}"));
            }
            async fn on_cleanup_verified(&self, _: &str) {
                self.0.lock().unwrap().push("verified".into());
            }
        }
        let sink = Sink::default();
        let mut reported = false;
        let mut terminal = TerminalInfo {
            exit_code: None,
            stop_cause: Some("wait_unverified:injected".into()),
            killed: None,
            leader_exit_verified: false,
            cleanup_verified: false,
        };
        assert!(!deliver_supervised_terminal(&sink, "task", &terminal, true, &mut reported).await);
        assert!(!deliver_supervised_terminal(&sink, "task", &terminal, true, &mut reported).await);
        terminal.leader_exit_verified = true;
        terminal.cleanup_verified = true;
        assert!(deliver_supervised_terminal(&sink, "task", &terminal, true, &mut reported).await);
        let events = sink.0.lock().unwrap();
        assert_eq!(events.len(), 2);
        assert!(events[0].contains("injected"));
        assert_eq!(events[1], "verified");
        drop(events);

        let sink = Sink::default();
        let mut reported = false;
        let mut terminal = TerminalInfo {
            exit_code: Some(0),
            stop_cause: None,
            killed: None,
            leader_exit_verified: false,
            cleanup_verified: false,
        };
        assert!(
            !deliver_supervised_terminal(&sink, "legacy", &terminal, true, &mut reported).await
        );
        assert!(
            !deliver_supervised_terminal(&sink, "legacy", &terminal, true, &mut reported).await
        );
        terminal.leader_exit_verified = true;
        terminal.cleanup_verified = true;
        assert!(deliver_supervised_terminal(&sink, "legacy", &terminal, true, &mut reported).await);
        let events = sink.0.lock().unwrap();
        assert_eq!(events.len(), 2);
        assert!(events[0].contains("no verified receipt"));
        assert_eq!(events[1], "verified");
        drop(events);

        let sink = Sink::default();
        let mut reported = false;
        terminal.exit_code = Some(-1);
        terminal.stop_cause = Some(UNVERIFIED_SUPERVISOR_LOSS.into());
        assert!(!deliver_supervised_terminal(&sink, "lost", &terminal, true, &mut reported).await);
        assert_eq!(sink.0.lock().unwrap().len(), 1);
    }
}
