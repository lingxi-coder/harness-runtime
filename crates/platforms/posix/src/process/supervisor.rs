//! Unix transport and process boundary for the shared supervision protocol.
use super::runner::PosixProcess;
use lingxi_core::host::process::ShellProcessHandoff;
use lingxi_core::host::shell_supervisor::{self as shared, BoxStream, Listener, Platform};
use lingxi_core::host::{
    BackgroundExitSink, ForegroundRunResult, ProcessError, ProcessHandle, ProcessRunner,
    SandboxedCommand,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::net::{UnixListener, UnixStream};
fn error(error: impl std::fmt::Display) -> ProcessError {
    ProcessError::Io(error.to_string())
}
struct UnixPlatform;
struct UnixSocket(UnixListener);
#[async_trait::async_trait]
impl Listener for UnixSocket {
    async fn accept(&self) -> Result<BoxStream, ProcessError> {
        Ok(Box::new(self.0.accept().await.map_err(error)?.0))
    }
}
#[async_trait::async_trait]
impl Platform for UnixPlatform {
    fn process_is_alive(&self, pid: u32) -> Option<bool> {
        let pid = i32::try_from(pid).ok().filter(|pid| *pid > 1)?;
        match nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None) {
            Ok(()) => Some(true),
            Err(nix::errno::Errno::ESRCH) => Some(false),
            Err(_) => None,
        }
    }

    #[cfg(test)]
    fn process_start_identity(&self, pid: u32) -> Option<String> {
        if pid != std::process::id()
            && matches!(
                std::env::var("LXS_TEST_MODE").as_deref(),
                Ok("pre_cap") | Ok("pre_cap_delivery")
            )
        {
            std::fs::write(
                std::env::var_os("LXS_TEST_PRECAP_RECORD").unwrap(),
                pid.to_string(),
            )
            .unwrap();
            if std::env::var("LXS_TEST_MODE").as_deref() == Ok("pre_cap") {
                std::thread::sleep(std::time::Duration::from_secs(30));
            }
        }
        if std::env::var("LXS_TEST_HIDE_BIRTH_PID")
            .ok()
            .and_then(|value| value.parse::<u32>().ok())
            == Some(pid)
        {
            return None;
        }

        if std::env::var("LXS_TEST_MODE").as_deref() == Ok("birthfail") && pid != std::process::id()
        {
            None
        } else {
            lingxi_core::host::live_sessions::process_start_identity(pid)
        }
    }

    fn nonce(&self) -> Result<String, ProcessError> {
        use std::io::Read;
        let mut bytes = [0u8; 24];
        std::fs::File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(&mut bytes))
            .map_err(error)?;
        Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
    }
    fn create_directory(&self, nonce: &str) -> Result<PathBuf, ProcessError> {
        use std::os::unix::fs::PermissionsExt;
        let directory = std::env::temp_dir().join(format!("lxs-{}", &nonce[..16]));
        std::fs::create_dir(&directory).map_err(error)?;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
            .map_err(error)?;
        Ok(directory)
    }
    fn validate_directory(&self, path: &Path) -> Result<(), ProcessError> {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::symlink_metadata(path).map_err(error)?;
        if !path.is_absolute()
            || !metadata.is_dir()
            || metadata.uid() != super::spawn_unsafe::effective_uid()
            || metadata.mode() & 0o777 != 0o700
        {
            return Err(error("shell supervisor directory is not private and owned"));
        }
        Ok(())
    }
    fn endpoint(&self, directory: &Path) -> String {
        directory.join("socket").to_string_lossy().into_owned()
    }
    fn detach(&self, command: &mut tokio::process::Command) {
        super::spawn_unsafe::attach_setsid(command);
    }
    fn cancel_active_processes(&self) {
        super::active_children::kill_all_active_children();
    }
    async fn connect(&self, endpoint: &str, _expected_pid: u32) -> Result<BoxStream, ProcessError> {
        #[cfg(test)]
        if std::env::var("LXS_TEST_REFUSE_ENDPOINT").as_deref() == Ok(endpoint) {
            return Err(error("test control outage"));
        }

        #[cfg(test)]
        if std::env::var("LXS_TEST_DELAY_ENDPOINT").as_deref() == Ok(endpoint) {
            tests::delayed_connect_started().notify_one();
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        }

        let stream = UnixStream::connect(endpoint).await.map_err(error)?;
        #[cfg(test)]
        if std::env::var("LXS_TEST_MODE").as_deref() == Ok("pre_cap_delivery")
            && std::env::var("LXS_TEST_ROLE").as_deref() == Ok("source")
        {
            static CONNECTIONS: std::sync::atomic::AtomicUsize =
                std::sync::atomic::AtomicUsize::new(0);
            if CONNECTIONS.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 1 {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            }
        }
        Ok(Box::new(stream))
    }
    async fn listen(&self, directory: &Path) -> Result<Box<dyn Listener>, ProcessError> {
        use std::os::unix::fs::PermissionsExt;
        let socket = directory.join("socket");
        let listener = UnixListener::bind(&socket).map_err(error)?;
        std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600)).map_err(error)?;
        Ok(Box::new(UnixSocket(listener)))
    }
    fn runner(&self, output: &Path) -> Arc<dyn ProcessRunner> {
        super::active_children::enable_print_mode_child_cleanup();
        Arc::new(PosixProcess::with_task_output_dir(
            output.parent().unwrap_or(Path::new("/")).to_path_buf(),
        ))
    }
    async fn cleanup_orphan(&self, handoff: &ShellProcessHandoff) -> Result<(), ProcessError> {
        guarded_orphan_cleanup(handoff)
    }
    async fn memory_pressure(&self) -> bool {
        super::watchdog::memory_pressure().await
    }
}
fn configure() {
    shared::configure(Arc::new(UnixPlatform));
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
    configure();
    shared::run_supervisor(factory).await
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
fn guarded_orphan_cleanup(handoff: &ShellProcessHandoff) -> Result<(), ProcessError> {
    let expected = handoff
        .process_start_identity
        .as_ref()
        .ok_or(ProcessError::Unsupported)?;
    match lingxi_core::host::live_sessions::process_start_identity(handoff.pid) {
        Some(actual) if &actual == expected => super::kill_tree::kill_tree_force(handoff.pid),
        None => Ok(()),
        _ => Err(error("orphan shell birth identity mismatch")),
    }
}
pub async fn serve_supervisor(
    directory: PathBuf,
    factory: fn(&Path) -> Arc<dyn BackgroundExitSink>,
) -> Result<(), ProcessError> {
    configure();
    shared::run_at(directory, factory).await
}
#[cfg(test)]
use lingxi_core::host::{BackgroundTaskBinding, ForegroundOutcome, ProcessCommand, SandboxedTag};
#[cfg(test)]
use std::{collections::HashMap, sync::Mutex, time::Duration};
#[cfg(test)]
mod tests {
    use super::*;
    struct Sink {
        path: PathBuf,
        done: tokio::sync::Notify,
        code: Mutex<Option<Option<i32>>>,
    }
    impl Sink {
        fn new(path: PathBuf) -> Self {
            Self {
                path,
                done: tokio::sync::Notify::new(),
                code: Mutex::new(None),
            }
        }
    }
    #[async_trait::async_trait]
    impl BackgroundExitSink for Sink {
        fn manages_output(&self) -> bool {
            true
        }
        async fn append_output(&self, _: &str, text: &str) -> Result<(), ProcessError> {
            lingxi_core::host::rooted_fs::append_file(
                self.path.parent().unwrap(),
                Path::new(self.path.file_name().unwrap()),
                text,
            )
            .map_err(error)
        }
        async fn flush_output(&self, _: &str) -> Result<(), ProcessError> {
            Ok(())
        }
        async fn on_exit(&self, _: &str, code: Option<i32>) {
            *self.code.lock().unwrap() = Some(code);
            self.done.notify_one();
        }
        async fn on_exit_with_status(&self, id: &str, code: Option<i32>, killed: bool) {
            let marker = if killed {
                "\n[killed]\n".to_owned()
            } else {
                format!(
                    "\n[exited with code {}]\n",
                    code.map_or("unknown".into(), |v| v.to_string())
                )
            };
            self.append_output(id, &marker).await.unwrap();
        }
    }
    fn factory(path: &Path) -> Arc<dyn BackgroundExitSink> {
        Arc::new(Sink::new(path.to_owned()))
    }
    /// How long a cross-process poll may wait before the test gives up.
    ///
    /// 🚨 This is a SAFETY NET, not a performance assertion. Every use below is
    /// a `while !condition { sleep }` loop that exits the instant the condition
    /// holds, so a larger bound costs a passing run nothing and only changes
    /// how long a genuinely stuck test takes to fail.
    ///
    /// It was 3s, and that flaked under a loaded `cargo test --workspace`:
    /// these tests fork helper processes and wait for them to register, while
    /// 500+ other test binaries compete for the machine. The failure reads as
    /// `Elapsed(())` with nothing named — indistinguishable from a real
    /// regression, which is the expensive part. ⛔ Do not tighten this back to
    /// shave seconds off a run that is already passing.
    const CROSS_PROCESS_WAIT: Duration = Duration::from_secs(30);

    #[tokio::test]
    async fn supervisor_cross_process_helper() {
        let Ok(role) = std::env::var("LXS_TEST_ROLE") else {
            return;
        };
        let directory = PathBuf::from(std::env::var_os("LXS_TEST_DIR").unwrap());
        if role == "supervisor" {
            serve_supervisor(directory, factory).await.unwrap();
            return;
        }
        enable_supervisor(PathBuf::from(std::env::var_os("LXS_TEST_WRAPPER").unwrap()));
        let id = format!("b{:x}", std::process::id());
        let path = directory.join(format!("{id}.output"));
        let mode = std::env::var("LXS_TEST_MODE").unwrap_or_else(|_| "explicit".into());
        if mode == "owner" {
            owner_bridge_probe(&directory).await;
            return;
        }
        let background = Arc::new(tokio::sync::Notify::new());
        let sink = factory(&path);
        let command = SandboxedCommand::__new_sandboxed(
            ProcessCommand {
                command: "/bin/sh".into(),
                args: vec!["-c".into(), std::env::var("LXS_TEST_COMMAND").unwrap()],
                cwd: Some(directory.clone()),
                env: HashMap::new(),
                timeout: matches!(mode.as_str(), "timeout" | "deadline")
                    .then_some(Duration::from_millis(150)),
                stdin: None,
            },
            SandboxedTag::BypassAuditedWithReason {
                reason: "supervisor integration test".into(),
            },
        )
        .with_background_task(BackgroundTaskBinding {
            task_id: id,
            output_path: path.clone(),
            on_exit: Some(sink),
            on_demand: Some(background.clone()),
        })
        .with_auto_background_on_timeout(mode != "deadline");
        let runner = PosixProcess::new();
        if matches!(mode.as_str(), "pre_cap" | "pre_cap_delivery") {
            std::env::set_var("LXS_TEST_PRECAP_RECORD", directory.join("gate.pid"));
            let command = command.with_process_owner(Some("pre-cap".into()));
            let run = tokio::spawn(async move {
                PosixProcess::new()
                    .run_foreground_with_output_limit(&command, Some(10))
                    .await
            });
            tokio::time::timeout(CROSS_PROCESS_WAIT, async {
                while !directory.join("gate.pid").exists() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            let child = std::fs::read_to_string(directory.join("gate.pid"))
                .unwrap()
                .parse::<u32>()
                .unwrap();
            assert!(
                lingxi_core::host::agent_processes::snapshot("pre-cap").is_empty(),
                "source has no capability yet"
            );
            assert!(!directory.join("payload.started").exists());
            let supervisor = std::fs::read_to_string(directory.join("supervisor.pid"))
                .unwrap()
                .trim()
                .parse::<i32>()
                .unwrap();
            nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(supervisor),
                nix::sys::signal::Signal::SIGKILL,
            )
            .unwrap();
            assert!(tokio::time::timeout(Duration::from_secs(3), run)
                .await
                .unwrap()
                .unwrap()
                .is_err());
            tokio::time::timeout(CROSS_PROCESS_WAIT, async {
                while lingxi_core::host::live_sessions::process_start_identity(child).is_some() {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap();
            assert!(
                !directory.join("payload.started").exists(),
                "unacknowledged payload must never execute"
            );
            return;
        }
        if matches!(mode.as_str(), "fg_crash" | "fg_crash_abort") {
            let command = command.with_process_owner(Some("fg-crash".into()));
            let run = tokio::spawn(async move {
                PosixProcess::new()
                    .run_foreground_with_output_limit(&command, Some(10))
                    .await
            });
            tokio::time::timeout(CROSS_PROCESS_WAIT, async {
                while lingxi_core::host::agent_processes::snapshot("fg-crash").is_empty() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            let child = lingxi_core::host::agent_processes::snapshot("fg-crash")[0];
            let supervisor = std::fs::read_to_string(directory.join("supervisor.pid"))
                .unwrap()
                .trim()
                .parse::<i32>()
                .unwrap();
            nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(supervisor),
                nix::sys::signal::Signal::SIGKILL,
            )
            .unwrap();
            if mode == "fg_crash_abort" {
                run.abort();
                assert!(run.await.unwrap_err().is_cancelled());
            } else {
                assert!(run.await.unwrap().is_err());
            }
            tokio::time::timeout(CROSS_PROCESS_WAIT, async {
                while lingxi_core::host::live_sessions::process_start_identity(child).is_some() {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap();
            tokio::time::timeout(CROSS_PROCESS_WAIT, async {
                while !std::fs::read_to_string(&path)
                    .unwrap_or_default()
                    .contains("[supervisor lost; task failed]")
                {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap();
            return;
        }
        if mode == "birthfail" {
            let error = runner.spawn_background(&command).await.unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("shell birth identity unavailable"),
                "{error}"
            );
            return;
        }

        let handle = if matches!(mode.as_str(), "explicit" | "unregistered") {
            runner.spawn_background(&command).await.unwrap()
        } else {
            if mode == "demand" {
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    background.notify_one();
                });
            }
            let run = runner.run_foreground_with_output_limit(&command, Some(10));
            tokio::pin!(run);
            if mode == "cancel" {
                tokio::select! {result=&mut run=>panic!("cancel child exited first: {result:?}"), ()=async {while !directory.join("child.pid").exists(){tokio::time::sleep(Duration::from_millis(10)).await;}}=>{}}
                tokio::time::sleep(Duration::from_millis(150)).await;
                return;
            }
            let result = run.await;
            if mode == "deadline" {
                assert!(
                    matches!(result.unwrap().outcome,ForegroundOutcome::Completed(output) if output.timed_out&&output.exit_code==143)
                );
                return;
            }
            let result = result.unwrap();
            std::fs::write(
                directory.join("result.json"),
                serde_json::to_vec(&result).unwrap(),
            )
            .unwrap();
            match result.outcome {
                ForegroundOutcome::MovedToBackground(handle) => handle,
                ForegroundOutcome::Completed(_) => return,
            }
        };
        if mode == "unregistered" {
            std::fs::write(
                directory.join("native-handle.json"),
                serde_json::to_vec(&handle).unwrap(),
            )
            .unwrap();
            return;
        }
        shared::acknowledge(&handle).await.unwrap();
        let handoff = export(&handle).unwrap();
        std::fs::write(
            directory.join("handoff.json"),
            serde_json::to_vec(&handoff).unwrap(),
        )
        .unwrap();
        // Returning ends this source process and its Tokio runtime. The child
        // must retain its independent supervisor and output descriptors.
    }

    async fn owner_bridge_probe(directory: &Path) {
        fn make_command(directory: &Path, id: &str, owner: &str) -> SandboxedCommand {
            let output = directory.join(format!("{id}.output"));
            SandboxedCommand::__new_sandboxed(
                ProcessCommand {
                    command: "/bin/sh".into(),
                    args: vec!["-c".into(), "sleep 5".into()],
                    cwd: Some(directory.to_owned()),
                    env: HashMap::new(),
                    timeout: None,
                    stdin: None,
                },
                SandboxedTag::BypassAuditedWithReason {
                    reason: "owner bridge test".into(),
                },
            )
            .with_process_owner(Some(owner.into()))
            .with_background_task(BackgroundTaskBinding {
                task_id: id.into(),
                output_path: output.clone(),
                on_exit: Some(factory(&output)),
                on_demand: None,
            })
        }
        let a = make_command(directory, "b1111111", "owner-a");
        let b = make_command(directory, "b2222222", "owner-b");
        let a = tokio::spawn(async move {
            PosixProcess::new()
                .run_foreground_with_output_limit(&a, None)
                .await
        });
        let b = tokio::spawn(async move {
            PosixProcess::new()
                .run_foreground_with_output_limit(&b, None)
                .await
        });
        tokio::time::timeout(CROSS_PROCESS_WAIT, async {
            while lingxi_core::host::agent_processes::snapshot("owner-a").is_empty()
                || lingxi_core::host::agent_processes::snapshot("owner-b").is_empty()
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let owned = lingxi_core::host::agent_processes::snapshot("owner-a");
        assert_eq!(
            PosixProcess::new().kill_owner_processes("owner-a").await,
            owned
        );
        assert!(a.await.unwrap().is_err());
        assert!(!b.is_finished());
        assert!(lingxi_core::host::agent_processes::snapshot("owner-a").is_empty());
        PosixProcess::new().kill_owner_processes("owner-b").await;
        assert!(b.await.unwrap().is_err());
        assert!(lingxi_core::host::agent_processes::snapshot("owner-b").is_empty());
        let command = make_command(directory, "b3333333", "owner-a");
        let handle = PosixProcess::new()
            .spawn_background(&command)
            .await
            .unwrap();
        shared::acknowledge(&handle).await.unwrap();
        let handoff = export(&handle).unwrap();
        assert_eq!(
            lingxi_core::host::agent_processes::snapshot("owner-a"),
            vec![handle.pid]
        );
        release(&handoff).await.unwrap();
        assert!(lingxi_core::host::agent_processes::snapshot("owner-a").is_empty());
        let sink = Arc::new(Sink::new(PathBuf::from(&handoff.output_path)));
        adopt(&handoff, sink.clone()).await.unwrap();
        assert_eq!(
            lingxi_core::host::agent_processes::snapshot("owner-a"),
            vec![handle.pid]
        );
        PosixProcess::new().kill_owner_processes("owner-a").await;
        tokio::time::timeout(Duration::from_secs(3), sink.done.notified())
            .await
            .unwrap();
        release(&handoff).await.unwrap();
        let first_command = make_command(directory, "b4444444", "owner-race");
        let first = PosixProcess::new()
            .spawn_background(&first_command)
            .await
            .unwrap();
        shared::acknowledge(&first).await.unwrap();
        let first_cap = export(&first).unwrap();
        std::env::set_var("LXS_TEST_DELAY_ENDPOINT", &first_cap.socket_path);
        let stopping =
            tokio::spawn(async { PosixProcess::new().kill_owner_processes("owner-race").await });
        delayed_connect_started().notified().await;
        let next_command = make_command(directory, "b5555555", "owner-race");
        let next = PosixProcess::new()
            .spawn_background(&next_command)
            .await
            .unwrap();
        shared::acknowledge(&next).await.unwrap();
        assert_eq!(
            stopping.await.unwrap(),
            vec![first.pid],
            "new owner work must not join an already-started stop snapshot"
        );
        std::env::remove_var("LXS_TEST_DELAY_ENDPOINT");
        assert!(lingxi_core::host::live_sessions::process_start_identity(next.pid).is_some());
        let next_cap = export(&next).unwrap();
        PosixProcess::new().kill(&next).await.unwrap();
        release(&next_cap).await.unwrap();
    }
    pub(super) fn delayed_connect_started() -> &'static tokio::sync::Notify {
        static STARTED: std::sync::OnceLock<tokio::sync::Notify> = std::sync::OnceLock::new();
        STARTED.get_or_init(tokio::sync::Notify::new)
    }
    fn quoted(value: &str) -> String {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
    async fn launch_source(directory: &Path, command: &str) -> ShellProcessHandoff {
        launch_mode(directory, command, "explicit").await;
        serde_json::from_slice(&std::fs::read(directory.join("handoff.json")).unwrap()).unwrap()
    }
    async fn launch_mode(directory: &Path, command: &str, mode: &str) {
        std::fs::set_permissions(
            directory,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();

        use std::os::unix::fs::PermissionsExt;
        let executable = std::env::current_exe().unwrap();
        let wrapper = directory.join("supervisor-bootstrap");
        let script=format!("#!/bin/sh\necho $$ > {}\nexport LXS_TEST_ROLE=supervisor\nexport LXS_TEST_DIR=\"$2\"\nexec {} --exact process::supervisor::tests::supervisor_cross_process_helper --nocapture\n",quoted(directory.join("supervisor.pid").to_str().unwrap()),quoted(executable.to_str().unwrap()));
        std::fs::write(&wrapper, script).unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
        let status = tokio::process::Command::new(executable)
            .args([
                "--exact",
                "process::supervisor::tests::supervisor_cross_process_helper",
                "--nocapture",
            ])
            .env("LXS_TEST_ROLE", "source")
            .env("LXS_TEST_DIR", directory)
            .env("LXS_TEST_WRAPPER", wrapper)
            .env("LXS_TEST_COMMAND", command)
            .env("LXS_TEST_MODE", mode)
            .status()
            .await
            .unwrap();
        assert!(status.success());
        assert_eq!(
            std::fs::metadata(directory).unwrap().permissions().mode() & 0o777,
            0o700,
            "initial output root must be repaired, not refused"
        );
    }
    #[tokio::test]
    async fn supervisor_cross_process_source_exit_preserves_output_completion_and_stop() {
        let directory = tempfile::tempdir().unwrap();
        let handoff = launch_source(
            directory.path(),
            "printf before; sleep 2; printf '后'; printf error >&2; exit 7",
        )
        .await;
        let mut forged = handoff.clone();
        forged.pid += 1;
        assert!(validate(&forged).await.is_err());
        forged = handoff.clone();
        forged
            .nonce
            .replace_range(..1, if &handoff.nonce[..1] == "a" { "b" } else { "a" });
        assert!(validate(&forged).await.is_err());
        let sink = Arc::new(Sink::new(PathBuf::from(&handoff.output_path)));
        let handle = adopt(&handoff, sink.clone()).await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), sink.done.notified())
            .await
            .unwrap();
        assert_eq!(*sink.code.lock().unwrap(), Some(Some(7)));
        let text = std::fs::read_to_string(&handoff.output_path).unwrap();
        assert!(text.contains("before"));
        assert!(text.contains('后'));
        assert!(text.contains("[stderr] error"));
        assert_eq!(text.matches("[exited with code 7]").count(), 1);
        assert!(kill(&handle).await.unwrap().is_ok());
        release(&handoff).await.unwrap();
        assert!(kill(&handle).await.unwrap().is_err());
        // A copied receipt under a replacement directory is not the original
        // supervisor's durable record, even if its JSON body is identical.
        let receipt_root = Path::new(&handoff.receipt_path).parent().unwrap();
        let saved = receipt_root.with_extension("saved");
        let receipt = std::fs::read(&handoff.receipt_path).unwrap();
        std::fs::rename(receipt_root, &saved).unwrap();
        std::fs::create_dir(receipt_root).unwrap();
        std::fs::set_permissions(
            receipt_root,
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
        std::fs::write(&handoff.receipt_path, receipt).unwrap();
        assert!(validate(&handoff).await.is_err());
        std::fs::remove_dir_all(receipt_root).unwrap();
        std::fs::rename(saved, receipt_root).unwrap();
        assert!(validate(&handoff).await.is_ok());

        let second = tempfile::tempdir().unwrap();
        let handoff = launch_source(second.path(), "printf running; sleep 5").await;
        let sink = Arc::new(Sink::new(PathBuf::from(&handoff.output_path)));
        let handle = adopt(&handoff, sink.clone()).await.unwrap();
        kill(&handle).await.unwrap().unwrap();
        tokio::time::timeout(Duration::from_secs(10), sink.done.notified())
            .await
            .unwrap();
        assert_eq!(*sink.code.lock().unwrap(), Some(None));
        assert_eq!(
            std::fs::read_to_string(&handoff.output_path)
                .unwrap()
                .matches("[killed]")
                .count(),
            1
        );
        release(&handoff).await.unwrap();
    }
    #[tokio::test]
    async fn supervisor_cross_process_timeout_demand_utf8_and_cancellation() {
        for mode in ["timeout", "demand"] {
            let directory = tempfile::tempdir().unwrap();
            launch_mode(directory.path(),"echo $$ > child.pid; printf '\\345'; printf '\\345' >&2; sleep 0.4; printf '\\220\\216'; printf '\\220\\216' >&2; exit 0",mode).await;
            let handoff: ShellProcessHandoff = serde_json::from_slice(
                &std::fs::read(directory.path().join("handoff.json")).unwrap(),
            )
            .unwrap();
            tokio::time::timeout(Duration::from_secs(2), async {
                while !directory.path().join("child.pid").exists() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            assert_eq!(
                handoff.pid,
                std::fs::read_to_string(directory.path().join("child.pid"))
                    .unwrap()
                    .trim()
                    .parse::<u32>()
                    .unwrap()
            );
            let sink = Arc::new(Sink::new(PathBuf::from(&handoff.output_path)));
            adopt(&handoff, sink.clone()).await.unwrap();
            tokio::time::timeout(Duration::from_secs(10), sink.done.notified())
                .await
                .unwrap();
            let text = std::fs::read_to_string(&handoff.output_path).unwrap();
            assert!(text.contains("后"), "{mode}: {text:?}");
            assert!(text.contains("[stderr] 后"));
            assert!(!text.contains('�'));
            release(&handoff).await.unwrap();
        }
        for mode in ["cancel", "deadline"] {
            let directory = tempfile::tempdir().unwrap();
            let started = std::time::Instant::now();
            launch_mode(directory.path(), "echo $$ > child.pid; sleep 5", mode).await;
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "{mode} did not stop promptly"
            );
            tokio::time::sleep(Duration::from_millis(300)).await;
            let pid = std::fs::read_to_string(directory.path().join("child.pid"))
                .unwrap()
                .trim()
                .parse::<i32>()
                .unwrap();
            assert!(
                nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err(),
                "{mode} left child live"
            );
        }
    }
    #[tokio::test]
    async fn supervisor_cross_process_crash_cleans_only_matching_birth() {
        let directory = tempfile::tempdir().unwrap();
        let handoff = launch_source(directory.path(), "echo $$ > child.pid; sleep 60").await;
        assert!(handoff.process_start_identity.is_some());
        let mut forged = handoff.clone();
        forged.process_start_identity = Some("other process".into());
        assert!(guarded_orphan_cleanup(&forged).is_err());
        let sink = Arc::new(Sink::new(PathBuf::from(&handoff.output_path)));
        adopt(&handoff, sink.clone()).await.unwrap();
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(handoff.supervisor_pid as i32),
            nix::sys::signal::Signal::SIGKILL,
        )
        .unwrap();
        tokio::time::timeout(Duration::from_secs(10), sink.done.notified())
            .await
            .unwrap();
        assert_eq!(*sink.code.lock().unwrap(), Some(Some(-1)));
        let output = std::fs::read_to_string(&handoff.output_path).unwrap();
        assert_eq!(output.matches("[supervisor lost; task failed]").count(), 1);

        tokio::time::timeout(Duration::from_secs(5), async {
            while lingxi_core::host::live_sessions::process_start_identity(handoff.pid).is_some() {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .unwrap();
        release(&handoff).await.unwrap();
    }
    #[tokio::test]
    async fn supervisor_cross_process_owner_bridge_and_unregistered_source_exit() {
        let directory = tempfile::tempdir().unwrap();
        launch_mode(directory.path(), "sleep 5", "owner").await;
        let directory = tempfile::tempdir().unwrap();
        launch_mode(directory.path(), "sleep 5", "unregistered").await;
        let handle: ProcessHandle = serde_json::from_slice(
            &std::fs::read(directory.path().join("native-handle.json")).unwrap(),
        )
        .unwrap();
        tokio::time::timeout(CROSS_PROCESS_WAIT, async {
            while lingxi_core::host::live_sessions::process_start_identity(handle.pid).is_some() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn supervisor_cross_process_missing_birth_refuses_detachment() {
        let directory = tempfile::tempdir().unwrap();
        launch_mode(
            directory.path(),
            "echo $$ > child.pid; sleep 5",
            "birthfail",
        )
        .await;
        if let Ok(pid) = std::fs::read_to_string(directory.path().join("child.pid")) {
            let pid = pid.trim().parse::<u32>().unwrap();
            tokio::time::timeout(Duration::from_secs(2), async {
                while lingxi_core::host::live_sessions::process_start_identity(pid).is_some() {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap();
        }
    }
    #[tokio::test]
    async fn supervisor_cross_process_control_outage_does_not_take_live_or_unknown_writer() {
        let directory = tempfile::tempdir().unwrap();
        let handoff = launch_source(directory.path(), "sleep 10").await;
        let sink = Arc::new(Sink::new(PathBuf::from(&handoff.output_path)));
        let handle = adopt(&handoff, sink.clone()).await.unwrap();
        std::env::set_var("LXS_TEST_REFUSE_ENDPOINT", &handoff.socket_path);
        for unknown in [false, true] {
            if unknown {
                std::env::set_var(
                    "LXS_TEST_HIDE_BIRTH_PID",
                    handoff.supervisor_pid.to_string(),
                );
            }
            let expected = if unknown {
                shared::SupervisorLiveness::Unknown
            } else {
                shared::SupervisorLiveness::Alive
            };
            assert_eq!(shared::supervisor_liveness(&handoff), expected);
            tokio::time::sleep(Duration::from_millis(3300)).await;
            assert_eq!(
                *sink.code.lock().unwrap(),
                None,
                "control outage must not publish failure or take the writer"
            );
            assert!(!std::fs::read_to_string(&handoff.output_path)
                .unwrap()
                .contains("supervisor lost"));
        }
        std::env::remove_var("LXS_TEST_HIDE_BIRTH_PID");
        std::env::remove_var("LXS_TEST_REFUSE_ENDPOINT");
        kill(&handle).await.unwrap().unwrap();
        tokio::time::timeout(Duration::from_secs(3), sink.done.notified())
            .await
            .unwrap();
        release(&handoff).await.unwrap();
    }
    #[tokio::test]
    async fn supervisor_cross_process_foreground_crash_and_aborted_wait_do_not_leak_child() {
        for mode in ["fg_crash", "fg_crash_abort"] {
            let directory = tempfile::tempdir().unwrap();
            launch_mode(directory.path(), "sleep 5", mode).await;
        }
    }
    #[tokio::test]
    async fn supervisor_cross_process_pre_cap_death_never_starts_payload() {
        for mode in ["pre_cap", "pre_cap_delivery"] {
            let directory = tempfile::tempdir().unwrap();
            launch_mode(directory.path(), "touch payload.started; sleep 30", mode).await;
        }
    }
}
