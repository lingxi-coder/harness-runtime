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
    fn complete_foreground(&self, strict_host: bool) -> Result<(), ProcessError> {
        if strict_host {
            // A strict host only completes after its payload Job is empty.
            Ok(())
        } else {
            super::supervisor_gate::disarm_supervisor_on_close()
        }
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

    struct Cleanup {
        release: PathBuf,
        directories: Vec<PathBuf>,
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::write(&self.release, "release");
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
            "explicit": false, "limit": null, "task_id": "windows-supervisor-test",
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
        .expect("fixture file did not appear");
    }

    #[tokio::test]
    async fn real_supervisor_preserves_detached_child_after_normal_foreground() {
        let fixture = tempfile::tempdir().unwrap();
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
            env: std::collections::HashMap::from([(
                test_env("DETACHED_JOB_TEST_DIR"),
                fixture.path().to_string_lossy().into_owned(),
            )]),
            timeout: Some(Duration::from_secs(120)),
            stdin: None,
        };
        let (mut supervisor, mut stream, _, _, _, _cleanup) =
            start(command, None, false, fixture.path().join("release")).await;
        wait_file(&fixture.path().join("started")).await;
        let descendant: u32 = std::fs::read_to_string(fixture.path().join("descendant.pid"))
            .unwrap()
            .parse()
            .unwrap();
        let descendant = native::hold_process_handle_for_test(descendant).unwrap();
        assert_eq!(
            native::process_handle_is_alive_for_test(&descendant),
            Some(true)
        );
        eprintln!(
            "detached-probe: child alive, stdout inheritable={}",
            std::fs::read_to_string(fixture.path().join("stdout-inheritable")).unwrap(),
        );
        // The result must arrive before the fixture child is released and
        // before the command deadline; a deadline fallback is not EOF proof.
        let response = tokio::time::timeout(Duration::from_secs(15), receive(&mut stream))
            .await
            .expect("redirected descendant retained the supervisor output pipe");
        assert_eq!(
            std::fs::read_to_string(fixture.path().join("stdout-inheritable")).unwrap(),
            "1",
            "fixture did not start with an inheritable stdout pipe"
        );
        assert_eq!(response["response"], "Started", "{response}");
        let result: ForegroundRunResult =
            serde_json::from_value(response["result"].clone()).unwrap();
        assert!(
            matches!(result.outcome, ForegroundOutcome::Completed(output) if !output.timed_out && output.exit_code == 0)
        );
        let status = tokio::time::timeout(Duration::from_secs(30), supervisor.wait())
            .await
            .expect("supervisor did not exit")
            .unwrap();
        assert!(status.success());
        assert_eq!(
            native::process_handle_is_alive_for_test(&descendant),
            Some(true),
            "the original detached process must survive supervisor exit"
        );
        // The detached process must do useful work after the real supervisor has
        // exited; mere PID existence before that point would not test its Job.
        std::fs::write(fixture.path().join("release"), "release").unwrap();
        wait_file(&fixture.path().join("finished")).await;
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match native::process_handle_is_alive_for_test(&descendant) {
                    Some(false) => break,
                    Some(true) => tokio::time::sleep(Duration::from_millis(10)).await,
                    None => panic!("could not inspect held descendant process"),
                }
            }
        })
        .await
        .expect("detached process did not exit after writing its marker");
    }

    #[tokio::test]
    async fn real_supervisor_drains_a_deliberately_inherited_descendant_pipe() {
        let fixture = tempfile::tempdir().unwrap();
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
            env: std::collections::HashMap::from([
                (
                    test_env("DETACHED_JOB_TEST_DIR"),
                    fixture.path().to_string_lossy().into_owned(),
                ),
                (test_env("DETACHED_JOB_HOLD_PIPE"), "1".into()),
                (test_env("DETACHED_JOB_WRITE_AFTER_RELEASE"), "1".into()),
            ]),
            timeout: Some(Duration::from_secs(60)),
            stdin: None,
        };
        let (mut supervisor, mut stream, _, _, payload, _cleanup) =
            start(command, None, false, fixture.path().join("release")).await;
        // Retain the original process object; checking a recycled PID would
        // not establish which leader exited.
        let leader = native::hold_process_handle_for_test(payload).unwrap();
        wait_file(&fixture.path().join("started")).await;
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match native::process_handle_is_alive_for_test(&leader) {
                    Some(false) => break,
                    Some(true) => tokio::time::sleep(Duration::from_millis(10)).await,
                    None => panic!("could not inspect held leader process"),
                }
            }
        })
        .await
        .expect("leader did not exit");
        assert_eq!(
            std::fs::read_to_string(fixture.path().join("stdout-inheritable")).unwrap(),
            "1"
        );
        let response = receive(&mut stream);
        tokio::pin!(response);
        assert!(
            tokio::time::timeout(Duration::from_millis(250), &mut response)
                .await
                .is_err(),
            "foreground returned while the descendant still owned the output pipe"
        );
        // The child writes to the same inherited pipe before exiting. Preserve
        // the future across the timeout so a partial protocol frame is not lost.
        std::fs::write(fixture.path().join("release"), "release").unwrap();
        let response = response.await;
        assert_eq!(response["response"], "Started", "{response}");
        let result: ForegroundRunResult =
            serde_json::from_value(response["result"].clone()).unwrap();
        let ForegroundOutcome::Completed(output) = result.outcome else {
            panic!("foreground was unexpectedly moved to background");
        };
        assert!(!output.timed_out);
        assert_eq!(output.exit_code, 0);
        assert!(output
            .stdout
            .contains("detached-child-output-after-release"));
        wait_file(&fixture.path().join("finished")).await;
        assert!(
            tokio::time::timeout(Duration::from_secs(30), supervisor.wait())
                .await
                .expect("supervisor did not exit")
                .unwrap()
                .success()
        );
    }

    // This diagnostic records the current shutdown behavior; it is not a
    // regression acceptance test for a production fix.
    #[tokio::test]
    async fn real_supervisor_diagnostic_inherited_writer_blocks_shutdown() {
        let fixture = tempfile::tempdir().unwrap();
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
            env: std::collections::HashMap::from([
                (
                    test_env("DETACHED_JOB_TEST_DIR"),
                    fixture.path().to_string_lossy().into_owned(),
                ),
                (test_env("DETACHED_JOB_HOLD_PIPE"), "1".into()),
            ]),
            timeout: Some(Duration::from_secs(5)),
            stdin: None,
        };
        let (mut supervisor, mut stream, _, _, _, _cleanup) =
            start(command, None, false, fixture.path().join("release")).await;
        wait_file(&fixture.path().join("started")).await;
        let descendant: u32 = std::fs::read_to_string(fixture.path().join("descendant.pid"))
            .unwrap()
            .parse()
            .unwrap();
        let descendant = native::hold_process_handle_for_test(descendant).unwrap();
        let response = tokio::time::timeout(Duration::from_secs(12), receive(&mut stream))
            .await
            .expect("supervisor did not return the foreground result after its deadline");
        assert_eq!(response["response"], "Started", "{response}");
        let result: ForegroundRunResult =
            serde_json::from_value(response["result"].clone()).unwrap();
        assert!(
            matches!(result.outcome, ForegroundOutcome::Completed(output) if !output.timed_out && output.exit_code == 0)
        );
        assert_eq!(
            native::process_handle_is_alive_for_test(&descendant),
            Some(true)
        );
        let shutdown = tokio::time::timeout(Duration::from_secs(3), supervisor.wait()).await;
        assert!(
            shutdown.is_err(),
            "supervisor already exited while its descendant held stdout"
        );
        eprintln!("detached-probe: result returned; live writer blocks supervisor shutdown");
        std::fs::write(fixture.path().join("release"), "release").unwrap();
        wait_file(&fixture.path().join("finished")).await;
        assert!(
            tokio::time::timeout(Duration::from_secs(10), supervisor.wait())
                .await
                .expect("supervisor did not exit after the writer closed")
                .unwrap()
                .success()
        );
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match native::process_handle_is_alive_for_test(&descendant) {
                    Some(false) => break,
                    Some(true) => tokio::time::sleep(Duration::from_millis(10)).await,
                    None => panic!("could not inspect held descendant process"),
                }
            }
        })
        .await
        .expect("detached process did not exit after release");
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
