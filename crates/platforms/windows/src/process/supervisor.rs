//! Windows named-pipe adapter for the shared acknowledged shell supervisor.
use super::{runner::WindowsProcess, supervisor_native as native};
use platform_api::process::ShellProcessHandoff;
use platform_api::shell_supervisor::{self as shared, BoxStream, Listener, Platform};
use platform_api::{
    BackgroundExitSink, ForegroundRunResult, ProcessError, ProcessHandle, ProcessRunner,
    SandboxedCommand,
};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer};
fn error(error: impl std::fmt::Display) -> ProcessError {
    ProcessError::Io(error.to_string())
}
struct WindowsPlatform;
struct PipeListener {
    endpoint: String,
    next: tokio::sync::Mutex<NamedPipeServer>,
}
#[async_trait::async_trait]
impl Listener for PipeListener {
    async fn accept(&self) -> Result<BoxStream, ProcessError> {
        let mut next = self.next.lock().await;
        next.connect().await.map_err(error)?;
        // Create the next instance before releasing this one, so first-instance
        // protection has no unowned name interval between clients.
        let replacement = native::create_pipe(&self.endpoint, false).map_err(error)?;
        Ok(Box::new(std::mem::replace(&mut *next, replacement)))
    }
}
#[async_trait::async_trait]
impl Platform for WindowsPlatform {
    fn process_is_alive(&self, pid: u32) -> Option<bool> {
        native::process_is_alive(pid)
    }
    fn nonce(&self) -> Result<String, ProcessError> {
        native::random_nonce().map_err(error)
    }
    fn create_directory(&self, nonce: &str) -> Result<PathBuf, ProcessError> {
        native::create_private_directory(nonce).map_err(error)
    }
    fn validate_directory(&self, path: &Path) -> Result<(), ProcessError> {
        native::validate_private_directory(path).map_err(error)
    }
    fn endpoint(&self, directory: &Path) -> String {
        format!(
            r"\\.\pipe\lingxi-shell-{}",
            directory.file_name().unwrap_or_default().to_string_lossy()
        )
    }
    fn detach(&self, command: &mut tokio::process::Command) {
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP: host console exit does
        // not terminate the supervisor; its child/output remain held there.
        command.creation_flags(0x0000_0008 | 0x0000_0200);
    }
    fn cancel_active_processes(&self) {
        // Windows driver's held-child StreamingProcessTreeGuard synchronously
        // invokes taskkill when the cancelled run future is dropped.
    }
    async fn connect(&self, endpoint: &str, expected_pid: u32) -> Result<BoxStream, ProcessError> {
        // Tokio defaults to SECURITY_IDENTIFICATION (no impersonation privilege).
        let client = ClientOptions::new().open(endpoint).map_err(error)?;
        native::verify_server(client.as_raw_handle(), expected_pid).map_err(error)?;
        Ok(Box::new(client))
    }
    async fn listen(&self, directory: &Path) -> Result<Box<dyn Listener>, ProcessError> {
        let endpoint = self.endpoint(directory);
        let next = native::create_pipe(&endpoint, true).map_err(error)?;
        Ok(Box::new(PipeListener {
            endpoint,
            next: tokio::sync::Mutex::new(next),
        }))
    }
    fn runner(&self, _output: &Path) -> Arc<dyn ProcessRunner> {
        Arc::new(WindowsProcess::new())
    }
    async fn cleanup_orphan(&self, handoff: &ShellProcessHandoff) -> Result<(), ProcessError> {
        let expected = handoff
            .process_start_identity
            .as_ref()
            .ok_or(ProcessError::Unsupported)?;
        match platform_api::live_sessions::process_start_identity(handoff.pid) {
            Some(actual) if &actual == expected => {
                super::kill_tree::kill_tree_windows(handoff.pid).await
            }
            None => Ok(()),
            _ => Err(error("orphan shell birth identity mismatch")),
        }
    }
    async fn memory_pressure(&self) -> bool {
        native::memory_pressure()
    }
}
fn configure() {
    shared::configure(Arc::new(WindowsPlatform));
}
pub fn enable_supervisor(executable: PathBuf) {
    configure();
    shared::enable_supervisor(executable);
}
pub fn is_supervisor_invocation() -> bool {
    shared::is_supervisor_invocation()
}
pub async fn run_supervisor(
    factory: fn(&Path) -> Arc<dyn BackgroundExitSink>,
) -> Result<(), ProcessError> {
    super::supervisor_gate::initialize()?;
    configure();
    shared::run_supervisor(factory).await
}
pub async fn serve_supervisor(
    directory: PathBuf,
    factory: fn(&Path) -> Arc<dyn BackgroundExitSink>,
) -> Result<(), ProcessError> {
    super::supervisor_gate::initialize()?;
    configure();
    shared::run_at(directory, factory).await
}
pub(super) fn enabled(command: &SandboxedCommand) -> bool {
    configure();
    shared::enabled(command)
}
pub(super) async fn execute(
    command: &SandboxedCommand,
    limit: Option<usize>,
    explicit: bool,
) -> Result<ForegroundRunResult, ProcessError> {
    configure();
    shared::execute(command, limit, explicit).await
}
pub fn export(handle: &ProcessHandle) -> Result<ShellProcessHandoff, ProcessError> {
    configure();
    shared::export(handle)
}
pub async fn validate(handoff: &ShellProcessHandoff) -> Result<(), ProcessError> {
    configure();
    shared::validate(handoff).await
}
pub async fn release(handoff: &ShellProcessHandoff) -> Result<(), ProcessError> {
    configure();
    shared::release(handoff).await
}
pub async fn kill(handle: &ProcessHandle) -> Option<Result<(), ProcessError>> {
    configure();
    shared::kill(handle).await
}
pub async fn adopt(
    handoff: &ShellProcessHandoff,
    sink: Arc<dyn BackgroundExitSink>,
) -> Result<ProcessHandle, ProcessError> {
    configure();
    shared::adopt(handoff, sink).await
}
