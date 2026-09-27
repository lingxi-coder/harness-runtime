//! Process-owned shell supervision for acknowledged cross-host adoption.
//! Only CLI hosts that install the bootstrap explicitly enable this path.
use crate::process::ShellProcessHandoff;
use crate::{
    BackgroundExitSink, BackgroundTaskBinding, ForegroundOutcome, ForegroundRunResult,
    ProcessCommand, ProcessError, ProcessHandle, ProcessRunner, SandboxedCommand, SandboxedTag,
};
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
        crate::live_sessions::process_start_identity(pid)
    }
    fn nonce(&self) -> Result<String, ProcessError>;
    fn create_directory(&self, nonce: &str) -> Result<PathBuf, ProcessError>;
    fn validate_directory(&self, path: &Path) -> Result<(), ProcessError>;
    fn endpoint(&self, directory: &Path) -> String;
    fn detach(&self, command: &mut tokio::process::Command);
    fn cancel_active_processes(&self);
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
    registration: Option<crate::agent_processes::RegistrationEntry>,
    cancel: CancellationToken,
    observer: Option<tokio::task::JoinHandle<()>>,
    pending_ack: Option<BoxStream>,
    backgrounded: bool,
    registered: bool,
    released: bool,
}
struct LiveJob {
    cancel: CancellationToken,
    _owner: Option<crate::agent_processes::Registration>,
}
impl Drop for LiveJob {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
fn track(handoff: ShellProcessHandoff, registered: bool) -> LiveJob {
    let cancel = CancellationToken::new();
    let owner = crate::agent_processes::register(handoff.owner.as_deref(), Some(handoff.pid));
    jobs()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            handoff.task_id.clone(),
            Job {
                registration: owner
                    .as_ref()
                    .map(crate::agent_processes::Registration::entry),
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

#[derive(Serialize, Deserialize)]
struct Start {
    command: ProcessCommand,
    tag: SandboxedTag,
    owner: Option<String>,
    auto_background: bool,
    explicit: bool,
    limit: Option<usize>,
    task_id: String,
    output: PathBuf,
    nonce: String,
    supervisor_directory_identity: crate::rooted_fs::RootIdentity,
    output_root_identity: crate::rooted_fs::RootIdentity,
    output_file_identity: crate::rooted_fs::RootIdentity,
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
    },
    Ack,
    Error {
        message: String,
    },
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
        && a.socket_path == b.socket_path
        && a.receipt_path == b.receipt_path
        && a.output_path == b.output_path
        && a.nonce == b.nonce
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
async fn inspect(handoff: &ShellProcessHandoff) -> Result<Option<Option<i32>>, ProcessError> {
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
        })) if same_identity(handoff, &actual) => Ok(terminal.then_some(exit_code)),
        _ => {
            // Exact receipt remains valid after its supervisor exits. Read by
            // pinned/no-follow APIs, never infer completion from PID disappearance.
            let path = Path::new(&handoff.receipt_path);
            let parent = path.parent().ok_or(ProcessError::Unsupported)?;
            let name = path.file_name().ok_or(ProcessError::Unsupported)?;
            let text = crate::rooted_fs::read_to_string_pinned(
                parent,
                Path::new(name),
                handoff.supervisor_directory_identity.as_ref(),
            )
            .map_err(error)?;
            let receipt: Receipt = serde_json::from_str(&text).map_err(error)?;
            if !same_identity(handoff, &receipt.handoff) {
                return Err(error("shell receipt identity mismatch"));
            }
            Ok(Some(receipt.exit_code))
        }
    }
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
async fn recover_failed_output(
    handoff: &ShellProcessHandoff,
) -> Result<(Option<i32>, bool), ProcessError> {
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
        let mut file =
            crate::rooted_fs::open_recovery_file(root, leaf, &root_id, &file_id).map_err(error)?;
        fs2::FileExt::lock_exclusive(&file).map_err(error)?;
        let receipt_root = Path::new(&handoff.receipt_path)
            .parent()
            .ok_or(ProcessError::Unsupported)?
            .to_owned();
        match crate::rooted_fs::read_to_string_pinned(
            &receipt_root,
            Path::new("receipt"),
            Some(&receipt_id),
        ) {
            Ok(text) => {
                let receipt: Receipt = serde_json::from_str(&text).map_err(error)?;
                if !same_identity(&receipt.handoff, &handoff) {
                    return Err(error("terminal receipt identity mismatch"));
                }
                return Ok((receipt.exit_code, false));
            }
            Err(crate::FsError::NotFound(_)) => {}
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
        };
        crate::rooted_fs::atomic_write_pinned(
            &receipt_root,
            Path::new("receipt"),
            &serde_json::to_vec(&receipt).map_err(error)?,
            crate::rooted_fs::AtomicWriteOptions {
                overwrite: false,
                create_parents: false,
                dir_mode: 0o700,
                file_mode: 0o600,
            },
            &receipt_id,
        )
        .map_err(error)?;
        Ok((Some(-1), true))
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
    entry: crate::agent_processes::RegistrationEntry,
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
    entry: crate::agent_processes::RegistrationEntry,
) -> Option<Result<(), ProcessError>> {
    if !crate::agent_processes::is_current(owner, entry) {
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
    if matches!(inspect(&handoff).await, Ok(Some(_))) {
        return Some(Ok(()));
    }
    if cancel.is_cancelled() {
        return Some(Err(error("shell owner no longer observes this process")));
    }
    Some(stop_handoff(&handoff).await)
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
fn observe(handoff: ShellProcessHandoff, sink: Arc<dyn BackgroundExitSink>, live: Option<LiveJob>) {
    let live = live.unwrap_or_else(|| track(handoff.clone(), true));
    let cancel = live.cancel.clone();
    let observed_handoff = handoff.clone();
    let task = tokio::spawn(async move {
        let _live = live;
        let mut watchdog = crate::shell_watchdog::ShellWatchdog::new(tokio::time::Instant::now());
        let mut size = 0;
        let mut failures = 0;
        loop {
            tokio::select! {
                biased;
                ()=cancel.cancelled()=>break,
                ()=tokio::time::sleep(Duration::from_secs(1))=>{}
            }
            match inspect(&handoff).await {
                Ok(Some(code)) => {
                    if !cancel.is_cancelled() {
                        sink.on_supervised_exit(&handoff.task_id, code).await;
                    }
                    break;
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
                            Ok((code, false)) => {
                                sink.on_supervised_exit(&handoff.task_id, code).await
                            }
                            _ => sink.on_supervision_lost(&handoff.task_id).await,
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
                            crate::rooted_fs::read_tail_bytes(parent, Path::new(name), 1024)
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
                let current=platform().process_start_identity(handoff.pid);
                if matches!((&handoff.process_start_identity,current.as_ref()),(Some(expected),Some(actual)) if expected!=actual)
                    || (current.is_none()&&platform().process_is_alive(handoff.pid)==Some(false)){break;}
                if recover_dead_foreground(&handoff).await{break;}
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
        crate::rooted_fs::root_identity(&directory).map_err(error)?;
    let output_root = binding
        .output_path
        .parent()
        .ok_or(ProcessError::Unsupported)?;
    let root_parent = output_root.parent().ok_or(ProcessError::Unsupported)?;
    let root_leaf = Path::new(output_root.file_name().ok_or(ProcessError::Unsupported)?);
    let output_root_identity =
        crate::rooted_fs::ensure_private_directory(root_parent, root_leaf, 0o700).map_err(error)?;
    platform().validate_directory(output_root)?;
    let output_leaf = Path::new(
        binding
            .output_path
            .file_name()
            .ok_or(ProcessError::Unsupported)?,
    );
    let output_file = crate::rooted_fs::open_append_file_pinned(
        output_root,
        output_leaf,
        Some(&output_root_identity),
    )
    .map_err(error)?;
    let output_file_identity =
        crate::rooted_fs::opened_file_identity(&output_file).map_err(error)?;
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
        auto_background: command.auto_background_on_timeout(),
        explicit,
        limit,
        task_id: binding.task_id.clone(),
        output: binding.output_path.clone(),
        nonce: nonce.clone(),
    };
    send(&mut stream, &Request::Start(request)).await?;
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
        tokio::pin!(response_future, spawned_future);
        let mut spawned = false;
        let mut backgrounded = false;
        loop {
            tokio::select! {
                response=&mut response_future=>break response,
                spawn=&mut spawned_future,if !spawned=>{
                    spawned=true;
                    match spawn {
                        Ok(Response::Spawned{handoff}) => {
                            recovery_guard.handoff=Some(handoff.clone());
                            live=Some(track(handoff.clone(),false));
                            if let Err(cause)=rpc(&handoff,Request::SpawnAccepted{nonce:nonce.clone()}).await {
                                break Err(cause);
                            }
                        },
                        Ok(_) => break Err(error("invalid spawn capability response")),
                        Err(cause) => break Err(cause),
                    }
                },
                ()=async{if let Some(notify)=binding.on_demand.as_ref(){notify.notified().await;}else{std::future::pending::<()>().await;}},if !backgrounded=>{backgrounded=true;let _=rpc(&temporary,Request::Background{nonce:nonce.clone()}).await;}
            }
        }
    };
    let response = match response {
        Ok(response) => response,
        Err(cause) => {
            recovery_guard.recover_error().await;
            return Err(cause);
        }
    };
    // A detached child is reaped in this host while it lives. Source exit does
    // not kill it; the supervisor was created as an independent session leader.
    tokio::spawn(async move {
        let _ = supervisor.wait().await;
    });
    match response {
        Response::Started { result, handoff } => {
            if let (Some(handoff), Some(sink)) = (handoff, binding.on_exit.clone()) {
                sink.on_supervised_start(&binding.task_id).await;
                let live = live.unwrap_or_else(|| track(handoff.clone(), false));
                if let Some(job) = jobs().lock().map_err(error)?.get_mut(&handoff.task_id) {
                    job.handoff = handoff.clone();
                    job.backgrounded = true;
                    job.pending_ack = Some(stream);
                }
                observe(handoff, sink, Some(live));
            }
            recovery_guard.handoff = None;
            Ok(result)
        }
        Response::Error { message } => {
            recovery_guard.recover_error().await;
            Err(error(message))
        }
        _ => {
            recovery_guard.recover_error().await;
            Err(error("invalid shell supervisor response"))
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
}
#[derive(Default)]
struct ServerState {
    nonce: Option<String>,
    handoff: Option<ShellProcessHandoff>,
    exit_code: Option<Option<i32>>,
    delivered: bool,
    receipt_persisted: bool,
    killed: bool,
    backgrounded: bool,
    spawn_accepted: bool,
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
            if self.state.lock().await.spawn_accepted {
                return Ok(());
            }
            notified.await;
        }
    }
    fn manages_output(&self) -> bool {
        true
    }
    fn stop_notify(&self) -> Option<Arc<tokio::sync::Notify>> {
        Some(self.stop.clone())
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
    async fn on_exit(&self, id: &str, code: Option<i32>) {
        let killed = self.state.lock().await.killed;
        self.inner.on_exit_with_status(id, code, killed).await;
        let mut state = self.state.lock().await;
        state.exit_code = Some(code);
        if let Some(handoff) = state.handoff.clone() {
            state.receipt_persisted = write_receipt(
                &self.directory,
                &Receipt {
                    handoff,
                    exit_code: code,
                },
            )
            .is_ok();
            if state.delivered && state.receipt_persisted {
                self.done.cancel();
            }
        }
    }
}
fn write_receipt(directory: &Path, receipt: &Receipt) -> Result<(), ProcessError> {
    let identity = receipt
        .handoff
        .supervisor_directory_identity
        .as_ref()
        .ok_or(ProcessError::Unsupported)?;
    crate::rooted_fs::atomic_write_pinned(
        directory,
        Path::new("receipt"),
        &serde_json::to_vec(receipt).map_err(error)?,
        crate::rooted_fs::AtomicWriteOptions {
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
        let mut stream = tokio::select! { accepted=listener.accept()=>accepted.map_err(error)?,
        _=retry.tick()=>{let mut current=state.lock().await;if !current.receipt_persisted {if let (Some(handoff),Some(code))=(current.handoff.clone(),current.exit_code){current.receipt_persisted=write_receipt(&directory,&Receipt{handoff,exit_code:code}).is_ok();if current.receipt_persisted&&current.delivered{done.cancel();}}}continue;}, ()=tokio::time::sleep_until(bootstrap_deadline), if state.lock().await.nonce.is_none()=>{let _=std::fs::remove_file(&socket);return Ok(());}, ()=done.cancelled()=>{let _=std::fs::remove_file(&socket);return Ok(());} };
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
                    let inner = factory(&start.output);
                    let sink = Arc::new(ServerSink {
                        inner,
                        state: state.clone(),
                        directory: directory.clone(),
                        stop: kill.clone(),
                        done: done.clone(),
                        spawned: spawned.clone(),
                        template: ShellProcessHandoff {
                            supervisor_directory_identity: Some(
                                start.supervisor_directory_identity,
                            ),
                            output_root_identity: Some(start.output_root_identity),
                            output_file_identity: Some(start.output_file_identity),
                            task_id: start.task_id.clone(),
                            pid: 0,
                            supervisor_pid: std::process::id(),
                            supervisor_start_identity: platform()
                                .process_start_identity(std::process::id()),
                            process_start_identity: None,
                            owner: start.owner.clone(),
                            socket_path: platform().endpoint(&directory),
                            receipt_path: directory.join("receipt").to_string_lossy().into_owned(),
                            output_path: start.output.to_string_lossy().into_owned(),
                            nonce: start.nonce.clone(),
                        },
                    });
                    if sink.template.supervisor_start_identity.is_none() {
                        let _ = send(
                            &mut stream,
                            &Response::Error {
                                message: "supervisor birth identity unavailable".into(),
                            },
                        )
                        .await;
                        done.cancel();
                        return;
                    }
                    let output_root = start.output.parent().unwrap_or(Path::new("/"));
                    let output_leaf = Path::new(start.output.file_name().unwrap_or_default());
                    if let Err(cause) = crate::rooted_fs::open_recovery_file(
                        output_root,
                        output_leaf,
                        &start.output_root_identity,
                        &start.output_file_identity,
                    ) {
                        let _ = send(
                            &mut stream,
                            &Response::Error {
                                message: cause.to_string(),
                            },
                        )
                        .await;
                        done.cancel();
                        return;
                    }
                    let command = SandboxedCommand::__new_sandboxed(start.command, start.tag)
                        .with_process_owner(start.owner)
                        .with_auto_background_on_timeout(start.auto_background)
                        .with_background_task(BackgroundTaskBinding {
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
                    let result = tokio::select! { result=&mut run=>result,()=stop.cancelled()=>{platform().cancel_active_processes();Err(error("supervisor command cancelled"))},()=disconnected(&mut stream)=>{platform().cancel_active_processes();done.cancel();return;}};
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
                                if let Some(code) = state.exit_code {
                                    state.receipt_persisted = write_receipt(
                                        &directory,
                                        &Receipt {
                                            handoff: handoff.clone(),
                                            exit_code: code,
                                        },
                                    )
                                    .is_ok();
                                }
                                Some(handoff)
                            } else {
                                None
                            };
                            Response::Started { result, handoff }
                        }
                        Err(error) => Response::Error {
                            message: error.to_string(),
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
                        }
                    } else {
                        match request {
                            Request::Inspect { .. } => state.handoff.clone().map_or(
                                Response::Error {
                                    message: "shell still foreground".into(),
                                },
                                |handoff| Response::Inspect {
                                    handoff,
                                    terminal: state.exit_code.is_some(),
                                    exit_code: state.exit_code.flatten(),
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
                                state.killed = true;
                                if state.backgrounded {
                                    if state.exit_code.is_none() {
                                        kill.notify_one();
                                    }
                                } else {
                                    stop.cancel();
                                }
                                Response::Ack
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
                    current.receipt_persisted && current.delivered
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
                    current.killed = true;
                    kill.notify_one();
                }
                current.delivered = true;
                if current.receipt_persisted {
                    done.cancel();
                }
            }
            if finished {
                done.cancel();
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
            socket_path: String::new(),
            receipt_path: String::new(),
            output_path: String::new(),
            nonce: String::new(),
        }
    }
    #[tokio::test]
    async fn old_supervised_tombstone_does_not_shadow_reused_pid_registration() {
        let owner = "supervisor-registration-generation";
        let old = track(handoff("old-generation", owner), true);
        let stale = crate::agent_processes::snapshot_entries(owner)[0];
        drop(old);
        let direct = crate::agent_processes::register(Some(owner), Some(stale.pid)).unwrap();
        assert!(kill_owned_registration(owner, direct.entry())
            .await
            .is_none());
        assert!(kill_owned_registration(owner, stale).await.unwrap().is_ok());
        drop(direct);
        let new = track(handoff("new-generation", owner), true);
        let current = crate::agent_processes::snapshot_entries(owner)[0];
        assert_eq!(registered_job(current).unwrap().0.task_id, "new-generation");
        assert_ne!(current.token, stale.token);
        drop(new);
        let mut rows = jobs().lock().unwrap();
        rows.remove("old-generation");
        rows.remove("new-generation");
    }
}

#[cfg(all(test, unix))]
mod recovery_tests {
    use super::*;
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
            supervisor_directory_identity: Some(
                crate::rooted_fs::root_identity(&receipts).unwrap(),
            ),
            output_root_identity: Some(crate::rooted_fs::root_identity(&output).unwrap()),
            output_file_identity: Some(crate::rooted_fs::opened_file_identity(&file).unwrap()),
            socket_path: receipts.join("socket").to_string_lossy().into_owned(),
            receipt_path: receipts.join("receipt").to_string_lossy().into_owned(),
            output_path: path.to_string_lossy().into_owned(),
            nonce: "a".repeat(48),
        }
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
            },
        )
        .unwrap();
        assert_eq!(recover_failed_output(&h).await.unwrap(), (Some(7), false));
        std::fs::remove_file(&h.receipt_path).unwrap();
        assert_eq!(recover_failed_output(&h).await.unwrap(), (Some(-1), true));
        assert_eq!(recover_failed_output(&h).await.unwrap(), (Some(-1), false));
        assert_eq!(
            std::fs::read_to_string(&h.output_path).unwrap(),
            "body\n[exited with code 7]\n"
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
