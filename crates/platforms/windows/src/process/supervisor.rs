//! Windows named-pipe adapter for the shared acknowledged shell supervisor.
use super::{runner::WindowsProcess, supervisor_native as native};
use lingxi_core::host::process::ShellProcessHandoff;
use lingxi_core::host::shell_supervisor::{self as shared, BoxStream, Listener, Platform};
use lingxi_core::host::{
    BackgroundExitSink, ForegroundRunResult, ProcessHandle, ProcessRunner, SandboxedCommand,
};
use mobile_linux_api::ProcessError;
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
    fn complete_foreground(&self, _strict_host: bool) -> Result<(), ProcessError> {
        // The runner commits both Jobs under ServerSink's stop gate before it
        // returns. Repeating an unguarded release here would reopen the race.
        Ok(())
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
        match lingxi_core::host::live_sessions::process_start_identity(handoff.pid) {
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
    if std::env::args().nth(2).as_deref() == Some(DRAIN_FLAG) {
        let nonce = std::env::args().nth(3).ok_or(ProcessError::Unsupported)?;
        return run_output_drainer(&nonce);
    }
    super::supervisor_gate::initialize()?;
    configure();
    shared::run_supervisor(factory).await
}

const DRAIN_FLAG: &str = "--lingxi-output-drain-v1";
const DRAIN_ACK: &str = "lingxi-output-drain-ready:";

fn valid_drain_nonce(nonce: &str) -> bool {
    nonce.len() == 48 && nonce.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Runs in a dedicated process after foreground output has been committed.
/// The inherited read ends are stdin and stderr; stdout is only the private
/// readiness pipe. Both streams are drained independently until their last
/// writer closes, with fixed-size buffers and no PID-based operations.
fn run_output_drainer(nonce: &str) -> Result<(), ProcessError> {
    use std::io::Write;

    if !valid_drain_nonce(nonce) {
        return Err(error("invalid output drainer nonce"));
    }
    let inputs = super::supervisor_gate::take_output_drainer_inputs()?
        .map(|input| Box::new(std::fs::File::from(input)) as Box<dyn std::io::Read + Send>);
    drain_output_streams(
        inputs,
        || {
            #[cfg(test)]
            if let Some(path) =
                std::env::var_os(format!("{}WINDOWS_DRAIN_PID_FILE", branding::ENV_PREFIX))
            {
                std::fs::write(path, std::process::id().to_string()).map_err(error)?;
            }
            let mut ack = std::io::stdout().lock();
            writeln!(ack, "{DRAIN_ACK}{nonce}").map_err(error)?;
            ack.flush().map_err(error)
        },
        #[cfg(test)]
        None,
    )
}

fn drain_output_streams(
    inputs: [Box<dyn std::io::Read + Send>; 2],
    ready: impl FnOnce() -> Result<(), ProcessError>,
    #[cfg(test)] failure_observed: Option<std::sync::mpsc::Sender<()>>,
) -> Result<(), ProcessError> {
    enum Event {
        Ready,
        Done(std::io::Result<()>),
    }
    let (sender, receiver) = std::sync::mpsc::channel();
    let mut workers = Vec::new();
    for mut input in inputs {
        let sender = sender.clone();
        workers.push(
            std::thread::Builder::new()
                .name("detached-output-drainer".into())
                .spawn(move || {
                    let _ = sender.send(Event::Ready);
                    let mut buffer = [0u8; 8192];
                    let result = loop {
                        match input.read(&mut buffer) {
                            Ok(0) => break Ok(()),
                            Ok(_) => {}
                            Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => {
                                break Ok(());
                            }
                            Err(error) => break Err(error),
                        }
                    };
                    drop(input);
                    let _ = sender.send(Event::Done(result));
                })
                .map_err(error)?,
        );
    }
    drop(sender);
    let mut started = 0;
    let mut finished = 0;
    while started < 2 {
        match receiver.recv().map_err(error)? {
            Event::Ready => started += 1,
            Event::Done(Ok(())) => finished += 1,
            Event::Done(Err(cause)) => return Err(error(cause)),
        }
    }
    // Detect failures already observed before acknowledging. A future OS
    // failure remains isolated to its own stream after readiness.
    while let Ok(event) = receiver.try_recv() {
        match event {
            Event::Ready => return Err(error("duplicate output drainer readiness")),
            Event::Done(Ok(())) => finished += 1,
            Event::Done(Err(cause)) => return Err(error(cause)),
        }
    }
    ready()?;
    let mut failures = Vec::new();
    while finished < 2 {
        match receiver.recv().map_err(error)? {
            Event::Ready => return Err(error("duplicate output drainer readiness")),
            Event::Done(result) => {
                finished += 1;
                if let Err(cause) = result {
                    #[cfg(test)]
                    if let Some(observed) = &failure_observed {
                        let _ = observed.send(());
                    }
                    failures.push(cause.to_string());
                }
            }
        }
    }
    for worker in workers {
        worker
            .join()
            .map_err(|_| error("output drainer thread panicked"))?;
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(error(format!(
            "output drainer failed: {}",
            failures.join("; ")
        )))
    }
}

pub(super) struct OutputDrainer {
    process: std::os::windows::io::OwnedHandle,
    committed: bool,
}

impl OutputDrainer {
    pub(super) fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for OutputDrainer {
    fn drop(&mut self) {
        if !self.committed {
            super::supervisor_gate::stop_output_drainer(&self.process);
        }
    }
}

pub(super) async fn start_output_drainer(
    stdout: &std::io::PipeReader,
    stderr: &std::io::PipeReader,
) -> Result<OutputDrainer, ProcessError> {
    use tokio::io::AsyncReadExt;

    let nonce = native::random_nonce().map_err(error)?;
    let executable = std::env::current_exe().map_err(error)?;
    #[cfg(not(test))]
    let args = ["--lingxi-shell-supervisor", DRAIN_FLAG, nonce.as_str()];
    #[cfg(test)]
    let args = [
        "process::supervisor::integration_tests::output_drainer_process_helper",
        "--exact",
        "--nocapture",
        "--skip",
        nonce.as_str(),
    ];
    let (process, ack) =
        super::supervisor_gate::spawn_output_drainer(stdout, stderr, &executable, &args)?;
    let guard = OutputDrainer {
        process,
        committed: false,
    };
    let mut ack =
        crate::stdio_cancel::CancelablePipeReader::new(ack, "output drainer ack").map_err(error)?;
    let expected = format!("{DRAIN_ACK}{nonce}\n");
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 512];
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while bytes.len() < 8192 {
            let read = ack.read(&mut buffer).await.map_err(error)?;
            if read == 0 {
                return Err(error("output drainer exited before its readiness receipt"));
            }
            bytes.extend_from_slice(&buffer[..read]);
            if bytes
                .windows(expected.len())
                .any(|part| part == expected.as_bytes())
            {
                return Ok(());
            }
        }
        Err(error("invalid output drainer readiness receipt"))
    })
    .await
    .map_err(|_| error("output drainer readiness timed out"))??;
    ack.shutdown(std::time::Duration::from_secs(5))
        .await
        .map_err(error)?;
    Ok(guard)
}

pub(super) fn drain_in_process(
    stdout: std::io::PipeReader,
    stderr: std::io::PipeReader,
) -> Result<(), ProcessError> {
    use std::io::Read;
    for mut input in [stdout, stderr] {
        std::thread::Builder::new()
            .name("detached-output-drainer".into())
            .spawn(move || {
                let mut buffer = [0u8; 8192];
                loop {
                    match input.read(&mut buffer) {
                        Ok(0) | Err(_) => return,
                        Ok(_) => {}
                    }
                }
            })
            .map_err(error)?;
    }
    Ok(())
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

#[cfg(all(test, windows))]
mod integration_tests {
    use super::*;
    use lingxi_core::host::{ForegroundOutcome, ProcessCommand, SandboxedTag};
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    const HELPER: &str = "process::supervisor::integration_tests::supervisor_process_helper";
    const DETACHED_HELPER: &str =
        "process::supervisor_gate::tests::windows_foreground_detached_helper";
    const JOB_LIFECYCLE_HELPER: &str =
        "process::supervisor_gate::tests::windows_job_lifecycle_fixture";

    // Keep the same test-only environment names as the supervisor-gate helpers.
    fn test_env(suffix: &str) -> String {
        format!("{}{suffix}", branding::ENV_PREFIX)
    }

    struct Sink;
    #[async_trait::async_trait]
    impl BackgroundExitSink for Sink {
        async fn on_exit(&self, _: &str, _: Option<i32>) {}
    }
    fn sink(_: &Path) -> Arc<dyn BackgroundExitSink> {
        Arc::new(Sink)
    }

    #[tokio::test]
    async fn supervisor_process_helper() {
        let Some(directory) = std::env::var_os(test_env("WINDOWS_SUPERVISOR_TEST_DIR")) else {
            return;
        };
        serve_supervisor(PathBuf::from(directory), sink)
            .await
            .unwrap();
    }

    #[test]
    fn output_drainer_process_helper() {
        // The same library test binary also runs ordinary tests, so only the
        // explicitly spawned helper with a nonce enters the blocking drainer.
        let args: Vec<_> = std::env::args().collect();
        let Some(index) = args.iter().position(|arg| arg == "--skip") else {
            return;
        };
        let Some(nonce) = args.get(index + 1) else {
            return;
        };
        if !valid_drain_nonce(nonce) {
            return;
        }
        let code = if run_output_drainer(nonce).is_ok() {
            0
        } else {
            1
        };
        // The libtest harness otherwise tries to write its final status to the
        // readiness pipe after the supervisor has closed its read end.
        std::process::exit(code);
    }

    #[test]
    fn output_drainer_keeps_other_stream_alive_after_read_error() {
        use std::io::{Read, Write};

        struct FailingInput {
            release: std::sync::mpsc::Receiver<()>,
            failed: std::sync::mpsc::Sender<()>,
        }
        impl Read for FailingInput {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                self.release.recv().map_err(std::io::Error::other)?;
                let _ = self.failed.send(());
                Err(std::io::Error::other("injected drainer read failure"))
            }
        }

        let (input, mut writer) = std::io::pipe().unwrap();
        let (release, release_rx) = std::sync::mpsc::channel();
        let (failed, failed_rx) = std::sync::mpsc::channel();
        let (ready, ready_rx) = std::sync::mpsc::channel();
        let (observed, observed_rx) = std::sync::mpsc::channel();
        let (completed, completed_rx) = std::sync::mpsc::channel();
        let drainer = std::thread::spawn(move || {
            let result = drain_output_streams(
                [
                    Box::new(FailingInput {
                        release: release_rx,
                        failed,
                    }),
                    Box::new(input),
                ],
                || ready.send(()).map_err(error),
                Some(observed),
            );
            let _ = completed.send(result);
        });
        ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        release.send(()).unwrap();
        failed_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        observed_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(
            completed_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        let (written, written_rx) = std::sync::mpsc::channel();
        let writer = std::thread::spawn(move || {
            let result = writer.write_all(&vec![0x51; 256 * 1024]);
            drop(writer);
            let _ = written.send(result);
        });
        written_rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        writer.join().unwrap();
        let result = completed_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        drainer.join().unwrap();
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("injected drainer read failure"));
    }

    #[test]
    fn output_drainer_reads_busy_stream_while_other_writer_stays_open() {
        use std::io::Write;

        let (idle_input, idle_writer) = std::io::pipe().unwrap();
        let (busy_input, mut busy_writer) = std::io::pipe().unwrap();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (drained_tx, drained_rx) = std::sync::mpsc::channel();
        let drainer = std::thread::spawn(move || {
            let result = drain_output_streams(
                [Box::new(idle_input), Box::new(busy_input)],
                || ready_tx.send(()).map_err(error),
                None,
            );
            let _ = drained_tx.send(result);
        });
        ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();

        let (written_tx, written_rx) = std::sync::mpsc::channel();
        let writer = std::thread::spawn(move || {
            // Exceed a pipe buffer while the unrelated writer remains open.
            let result = busy_writer.write_all(&vec![0x37; 256 * 1024]);
            drop(busy_writer);
            let _ = written_tx.send(result);
        });
        let written_while_idle_open = written_rx.recv_timeout(Duration::from_secs(5));
        let still_draining_idle = matches!(
            drained_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        );
        // Release before asserting, so the serial-reader negative control can
        // finish without leaving its writer or drainer threads behind.
        drop(idle_writer);
        let drain_result = drained_rx.recv_timeout(Duration::from_secs(5));
        if drain_result.is_err() {
            drop(writer);
            drop(drainer);
            panic!("output drainer did not stop after both writers closed");
        }
        writer.join().unwrap();
        drainer.join().unwrap();
        written_while_idle_open.unwrap().unwrap();
        assert!(
            still_draining_idle,
            "idle writer should keep the drainer alive"
        );
        drain_result.unwrap().unwrap();
    }

    struct Cleanup {
        release: PathBuf,
        directories: Vec<PathBuf>,
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::write(&self.release, "release");
            let _ = std::fs::write(self.release.with_file_name("exit"), "exit");
            for directory in &self.directories {
                let _ = std::fs::remove_dir_all(directory);
            }
        }
    }

    async fn connect(endpoint: &str, pid: u32) -> shared::BoxStream {
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if let Ok(stream) = WindowsPlatform.connect(endpoint, pid).await {
                    break stream;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("supervisor endpoint did not become ready")
    }
    async fn send(stream: &mut shared::BoxStream, value: serde_json::Value) {
        let mut bytes = serde_json::to_vec(&value).unwrap();
        bytes.push(b'\n');
        stream.write_all(&bytes).await.unwrap();
    }
    async fn receive(stream: &mut shared::BoxStream) -> serde_json::Value {
        let mut line = String::new();
        tokio::time::timeout(
            Duration::from_secs(30),
            BufReader::new(stream).read_line(&mut line),
        )
        .await
        .expect("supervisor response timed out")
        .unwrap();
        serde_json::from_str(&line).expect("invalid supervisor response")
    }
    async fn rpc(endpoint: &str, pid: u32, value: serde_json::Value) -> serde_json::Value {
        let mut stream = connect(endpoint, pid).await;
        send(&mut stream, value).await;
        receive(&mut stream).await
    }

    async fn start(
        command: ProcessCommand,
        owner: Option<&str>,
        require_group_cleanup_on_exit: bool,
        release: PathBuf,
    ) -> (
        tokio::process::Child,
        shared::BoxStream,
        String,
        String,
        u32,
        Cleanup,
    ) {
        start_with_explicit(
            command,
            owner,
            require_group_cleanup_on_exit,
            release,
            false,
        )
        .await
    }

    async fn start_with_explicit(
        command: ProcessCommand,
        owner: Option<&str>,
        require_group_cleanup_on_exit: bool,
        release: PathBuf,
        explicit: bool,
    ) -> (
        tokio::process::Child,
        shared::BoxStream,
        String,
        String,
        u32,
        Cleanup,
    ) {
        let nonce = WindowsPlatform.nonce().unwrap();
        let directory = WindowsPlatform.create_directory(&nonce).unwrap();
        let output_directory = WindowsPlatform
            .create_directory(&WindowsPlatform.nonce().unwrap())
            .unwrap();
        let cleanup = Cleanup {
            release,
            directories: vec![directory.clone(), output_directory.clone()],
        };
        let directory_identity = lingxi_core::host::rooted_fs::root_identity(&directory).unwrap();
        let output_root_identity =
            lingxi_core::host::rooted_fs::root_identity(&output_directory).unwrap();
        let output_path = output_directory.join("output");
        let file = lingxi_core::host::rooted_fs::open_append_file_pinned(
            &output_directory,
            Path::new("output"),
            Some(&output_root_identity),
        )
        .unwrap();
        let output_file_identity =
            lingxi_core::host::rooted_fs::opened_file_identity(&file).unwrap();
        drop(file);
        let endpoint = WindowsPlatform.endpoint(&directory);
        let supervisor = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([HELPER, "--exact", "--nocapture"])
            .env(test_env("WINDOWS_SUPERVISOR_TEST_DIR"), &directory)
            .env(
                test_env("WINDOWS_DRAIN_PID_FILE"),
                cleanup.release.with_file_name("drainer.pid"),
            )
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = supervisor.id().unwrap();
        let mut stream = connect(&endpoint, pid).await;
        send(&mut stream, serde_json::json!({
            "request": "Start", "command": command,
            "tag": SandboxedTag::BypassAuditedWithReason { reason: "Windows supervisor integration test".into() },
            "owner": owner, "auto_background": false, "background_timeout": null,
            "require_group_cleanup_on_exit": require_group_cleanup_on_exit,
            "explicit": explicit, "limit": null, "task_id": "windows-supervisor-test",
            "output": output_path, "nonce": nonce,
            "supervisor_directory_identity": directory_identity,
            "output_root_identity": output_root_identity,
            "output_file_identity": output_file_identity,
        })).await;
        let spawned = rpc(
            &endpoint,
            pid,
            serde_json::json!({
                "request": "AwaitSpawn", "nonce": nonce,
            }),
        )
        .await;
        assert_eq!(spawned["response"], "Spawned", "{spawned}");
        let payload = spawned["handoff"]["pid"].as_u64().unwrap() as u32;
        let accepted = rpc(
            &endpoint,
            pid,
            serde_json::json!({
                "request": "SpawnAccepted", "nonce": nonce,
            }),
        )
        .await;
        assert_eq!(accepted["response"], "Ack", "{accepted}");
        (supervisor, stream, endpoint, nonce, payload, cleanup)
    }

    async fn wait_file(path: &Path) {
        tokio::time::timeout(Duration::from_secs(30), async {
            while !path.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("fixture file did not appear: {}", path.display()));
    }

    #[tokio::test]
    async fn real_supervisor_preserves_detached_child_after_normal_foreground() {
        for (hold_pipe, hold_stderr, inherit_extra_handles) in [
            (false, false, false),
            (true, false, false),
            (false, true, false),
            (true, true, false),
            // Both standard streams are redirected, but Rust's Windows
            // Command still forwards the leader's other inheritable handles.
            // This reproduces the original supervisor timeout fixture.
            (false, false, true),
        ] {
            let fixture = tempfile::tempdir().unwrap();
            eprintln!(
                "detached fixture {}: hold_stdout={hold_pipe} hold_stderr={hold_stderr} extra_handles={inherit_extra_handles}",
                fixture.path().display()
            );
            let mut env = std::collections::HashMap::from([(
                test_env("DETACHED_JOB_TEST_DIR"),
                fixture.path().to_string_lossy().into_owned(),
            )]);
            if hold_pipe {
                env.insert(test_env("DETACHED_JOB_HOLD_PIPE"), "1".into());
            }
            if hold_stderr {
                env.insert(test_env("DETACHED_JOB_HOLD_STDERR"), "1".into());
            }
            if inherit_extra_handles {
                env.insert(test_env("DETACHED_JOB_INHERIT_EXTRA_HANDLES"), "1".into());
            }
            env.insert(test_env("DETACHED_JOB_LARGE_FOREGROUND_OUTPUT"), "1".into());
            let command = ProcessCommand {
                command: std::env::current_exe()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                args: vec![
                    DETACHED_HELPER.into(),
                    "--exact".into(),
                    "--nocapture".into(),
                ],
                cwd: Some(fixture.path().to_path_buf()),
                env,
                timeout: Some(Duration::from_secs(20)),
                stdin: None,
            };
            let (mut supervisor, mut stream, _, _, _, _cleanup) =
                start(command, None, false, fixture.path().join("release")).await;
            let response = tokio::time::timeout(Duration::from_secs(10), receive(&mut stream))
                .await
                .expect("foreground result waited for its detached child");
            assert_eq!(response["response"], "Started", "{response}");
            let result: ForegroundRunResult =
                serde_json::from_value(response["result"].clone()).unwrap();
            let ForegroundOutcome::Completed(output) = result.outcome else {
                panic!("detached foreground fixture unexpectedly backgrounded");
            };
            assert!(!output.timed_out && output.exit_code == 0, "{output:?}");
            assert!(
                output.stdout.contains(&"O".repeat(128 * 1024)),
                "foreground stdout was truncated"
            );
            assert!(
                output.stderr.contains(&"E".repeat(128 * 1024)),
                "foreground stderr was truncated"
            );
            if hold_pipe {
                assert!(
                    output.stdout.contains("detached-stdout-ready"),
                    "{output:?}"
                );
            }
            if hold_stderr {
                assert!(
                    output.stderr.contains("detached-stderr-ready"),
                    "{output:?}"
                );
            }
            let pid: u32 = std::fs::read_to_string(fixture.path().join("child.pid"))
                .unwrap()
                .parse()
                .unwrap();
            let descendant = native::hold_process_handle_for_test(pid).unwrap();
            let status = tokio::time::timeout(Duration::from_secs(30), supervisor.wait())
                .await
                .expect("supervisor did not exit while its detached child held output")
                .unwrap();
            assert!(status.success());
            assert_eq!(
                native::process_handle_is_alive_for_test(&descendant),
                Some(true)
            );
            let drainer = if hold_pipe || hold_stderr || inherit_extra_handles {
                let pid_file = fixture.path().join("drainer.pid");
                wait_file(&pid_file).await;
                let pid: u32 = std::fs::read_to_string(pid_file).unwrap().parse().unwrap();
                let process = native::hold_process_handle_for_test(pid).unwrap();
                assert_eq!(
                    native::process_handle_is_alive_for_test(&process),
                    Some(true)
                );
                Some(process)
            } else {
                None
            };
            // The descendant writes substantially more than a pipe buffer to
            // each inherited stream only after the real supervisor has exited.
            std::fs::write(fixture.path().join("release"), "release").unwrap();
            wait_file(&fixture.path().join("written")).await;
            assert_eq!(
                native::process_handle_is_alive_for_test(&descendant),
                Some(true)
            );
            std::fs::write(fixture.path().join("exit"), "exit").unwrap();
            wait_file(&fixture.path().join("finished")).await;
            assert_eq!(
                std::fs::read_to_string(fixture.path().join("finished")).unwrap(),
                "survived"
            );
            if let Some(drainer) = drainer {
                tokio::time::timeout(Duration::from_secs(30), async {
                    while native::process_handle_is_alive_for_test(&drainer) == Some(true) {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await
                .expect("output drainer did not exit after both writers closed");
                assert_eq!(
                    native::process_handle_is_alive_for_test(&drainer),
                    Some(false)
                );
            }
        }
    }

    #[tokio::test]
    async fn unaccepted_background_handoff_stops_naturally_exited_leaders_descendant() {
        let fixture = tempfile::tempdir().unwrap();
        let command = ProcessCommand {
            command: std::env::current_exe()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            args: vec![
                "process::background::native_background_stop_tests::redirected_descendant_fixture"
                    .into(),
                "--exact".into(),
                "--nocapture".into(),
            ],
            cwd: Some(fixture.path().to_path_buf()),
            env: std::collections::HashMap::from([
                ("BACKGROUND_REDIRECTED_FIXTURE_ROLE".into(), "leader".into()),
                (
                    "BACKGROUND_REDIRECTED_FIXTURE_DIR".into(),
                    fixture.path().to_string_lossy().into_owned(),
                ),
            ]),
            timeout: Some(Duration::from_secs(60)),
            stdin: None,
        };
        let (mut supervisor, mut stream, _, _, leader_pid, _cleanup) =
            start_with_explicit(command, None, false, fixture.path().join("release"), true).await;
        let response = receive(&mut stream).await;
        assert_eq!(response["response"], "Started", "{response}");
        assert!(!response["handoff"].is_null());
        let leader = native::hold_process_handle_for_test(leader_pid).unwrap();
        wait_file(&fixture.path().join("descendant.pid")).await;
        let pid: u32 = std::fs::read_to_string(fixture.path().join("descendant.pid"))
            .unwrap()
            .parse()
            .unwrap();
        let descendant = native::hold_process_handle_for_test(pid).unwrap();
        std::fs::write(fixture.path().join("leader.release"), "release").unwrap();
        tokio::time::timeout(Duration::from_secs(20), async {
            while native::process_handle_is_alive_for_test(&leader) == Some(true) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("background leader did not exit");
        assert_eq!(
            native::process_handle_is_alive_for_test(&leader),
            Some(false)
        );
        assert_eq!(
            native::process_handle_is_alive_for_test(&descendant),
            Some(true)
        );
        // Do not send Accepted: EOF must stop the still-owned background Job.
        drop(stream);
        tokio::time::timeout(Duration::from_secs(30), async {
            while native::process_handle_is_alive_for_test(&descendant) == Some(true) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("unaccepted handoff left its descendant running");
        assert_eq!(
            native::process_handle_is_alive_for_test(&descendant),
            Some(false)
        );
        assert!(!fixture.path().join("descendant.completed").exists());
        tokio::time::timeout(Duration::from_secs(30), supervisor.wait())
            .await
            .expect("supervisor retained unaccepted handoff")
            .unwrap();
    }

    #[tokio::test]
    async fn real_supervisor_strict_cancel_reaps_the_payload_tree() {
        let fixture = tempfile::tempdir().unwrap();
        let command = ProcessCommand {
            command: std::env::current_exe()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            args: vec![
                JOB_LIFECYCLE_HELPER.into(),
                "--exact".into(),
                "--nocapture".into(),
            ],
            cwd: Some(fixture.path().to_path_buf()),
            env: std::collections::HashMap::from([
                (test_env("JOB_LIFECYCLE_ROLE"), "leader".into()),
                (
                    test_env("JOB_LIFECYCLE_DIR"),
                    fixture.path().to_string_lossy().into_owned(),
                ),
            ]),
            timeout: Some(Duration::from_secs(60)),
            stdin: None,
        };
        let (mut supervisor, mut stream, endpoint, nonce, payload, _cleanup) = start(
            command,
            Some("host-tool:windows-supervisor-test"),
            true,
            fixture.path().join("release"),
        )
        .await;
        wait_file(&fixture.path().join("leader.ready")).await;
        assert!(fixture.path().join("descendant.ready").exists());
        let descendant_file = fixture.path().join("descendant.pid");
        let descendant: u32 = std::fs::read_to_string(descendant_file)
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(native::process_is_alive(payload), Some(true));
        assert_eq!(native::process_is_alive(descendant), Some(true));
        let ack = rpc(
            &endpoint,
            supervisor.id().unwrap(),
            serde_json::json!({
                "request": "Kill", "nonce": nonce,
            }),
        )
        .await;
        assert_eq!(ack["response"], "Ack", "{ack}");
        let response = receive(&mut stream).await;
        assert_eq!(response["response"], "Error", "{response}");
        assert!(response["message"]
            .as_str()
            .unwrap_or_default()
            .contains("cancelled"));
        let status = tokio::time::timeout(Duration::from_secs(30), supervisor.wait())
            .await
            .expect("supervisor did not exit")
            .unwrap();
        assert!(status.success());
        assert_eq!(native::process_is_alive(payload), Some(false));
        assert_eq!(native::process_is_alive(descendant), Some(false));
        assert!(!fixture.path().join("leader.completed").exists());
        assert!(!fixture.path().join("descendant.completed").exists());
    }
}
