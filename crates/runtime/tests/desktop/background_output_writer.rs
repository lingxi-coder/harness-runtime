//! Real child → managed output callback → TaskOutputManager failure recovery.
#![cfg(unix)]
use async_trait::async_trait;
use platform_api::filesystem::{
    FileAppendError, FileAppendStage, FileContent, FileEvent, FlockGuard, FsError,
};
use platform_api::task_registry::TaskRegistryHandle;
use platform_api::{
    BackgroundExitSink, BackgroundTaskBinding, FileSystem, ProcessCommand, ProcessError,
    ProcessRunner, SandboxedCommand, SandboxedTag,
};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

struct FaultFs {
    inner: platform_posix::PosixFileSystem,
    failures: AtomicUsize,
}
#[async_trait]
impl FileSystem for FaultFs {
    async fn read_file(
        &self,
        p: &str,
        o: Option<u64>,
        l: Option<u64>,
    ) -> Result<FileContent, FsError> {
        self.inner.read_file(p, o, l).await
    }
    async fn write_file(&self, p: &str, c: &str) -> Result<(), FsError> {
        self.inner.write_file(p, c).await
    }
    fn is_within_workspace(&self, p: &str) -> bool {
        self.inner.is_within_workspace(p)
    }
    async fn watch(
        &self,
        p: &str,
    ) -> Result<Pin<Box<dyn futures_core::Stream<Item = FileEvent> + Send>>, FsError> {
        self.inner.watch(p).await
    }
    async fn append_file(&self, p: &str, c: &str) -> Result<(), FsError> {
        self.inner.append_file(p, c).await
    }
    async fn create_new_file(&self, p: &str) -> Result<(), FsError> {
        self.inner.create_new_file(p).await
    }
    async fn truncate(&self, p: &str, n: u64) -> Result<(), FsError> {
        self.inner.truncate(p, n).await
    }
    async fn file_mtime(&self, p: &str) -> Result<std::time::SystemTime, FsError> {
        self.inner.file_mtime(p).await
    }
    async fn file_size(&self, p: &str) -> Result<u64, FsError> {
        self.inner.file_size(p).await
    }
    async fn delete_file(&self, p: &str) -> Result<(), FsError> {
        self.inner.delete_file(p).await
    }
    async fn symlink(&self, t: &str, p: &str) -> Result<(), FsError> {
        self.inner.symlink(t, p).await
    }
    async fn flock_exclusive(&self, p: &str) -> Result<Box<dyn FlockGuard>, FsError> {
        self.inner.flock_exclusive(p).await
    }
    async fn fsync(&self, p: &str) -> Result<(), FsError> {
        self.inner.fsync(p).await
    }
    async fn append_file_rooted_staged(
        &self,
        root: &Path,
        relative: &Path,
        content: &str,
        identity: Option<&platform_api::rooted_fs::RootIdentity>,
    ) -> Result<(), FileAppendError> {
        if self
            .failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return Err(FileAppendError {
                stage: FileAppendStage::Write,
                error: FsError::Io("injected ENOSPC".into()),
            });
        }
        self.inner
            .append_file_rooted_staged(root, relative, content, identity)
            .await
    }
}

struct ManagedSink {
    registry: Arc<tasks::registry::TaskRegistry>,
    output: Arc<tasks::output_manager::TaskOutputManager>,
    path: PathBuf,
    flushed: AtomicBool,
    lost: AtomicBool,
    done: tokio::sync::Notify,
}
#[async_trait]
impl BackgroundExitSink for ManagedSink {
    fn manages_output(&self) -> bool {
        true
    }
    async fn append_output(&self, id: &str, content: &str) -> Result<(), ProcessError> {
        TaskRegistryHandle::append_bash_output(self.registry.as_ref(), id, content)
            .await
            .map_err(|e| ProcessError::Io(e.to_string()))
    }
    async fn flush_output(&self, id: &str) -> Result<(), ProcessError> {
        TaskRegistryHandle::flush_bash_output(self.registry.as_ref(), id)
            .await
            .map_err(|e| ProcessError::Io(e.to_string()))?;
        self.flushed.store(true, Ordering::SeqCst);
        Ok(())
    }
    async fn on_exit(&self, id: &str, code: Option<i32>) {
        self.lost
            .store(self.output.lost_output(&self.path).await, Ordering::SeqCst);
        self.registry
            .settle_background_bash(id, code, false)
            .await
            .unwrap();
        self.done.notify_one();
    }
}

async fn real_background_writer_recovers(auto: bool) {
    let dir = tempfile::tempdir().unwrap();
    let fs = Arc::new(FaultFs {
        inner: platform_posix::PosixFileSystem::new(dir.path().into()),
        failures: AtomicUsize::new(2),
    });
    let output = Arc::new(tasks::output_manager::TaskOutputManager::new(
        dir.path().into(),
        fs.clone(),
    ));
    let registry = Arc::new(tasks::registry::TaskRegistry::new(
        Arc::new(platform_posix::PosixRuntime::new()),
        fs.clone(),
        output.clone(),
    ));
    let (id, path) = registry.allocate_bash_output().await.unwrap();
    registry
        .register_background_bash(
            id.clone(),
            "script".into(),
            "writer test".into(),
            None,
            None,
            None,
        )
        .await
        .unwrap();
    let sink = Arc::new(ManagedSink {
        registry,
        output,
        path: path.clone(),
        flushed: AtomicBool::new(false),
        lost: AtomicBool::new(false),
        done: tokio::sync::Notify::new(),
    });
    let command = SandboxedCommand::__new_sandboxed(ProcessCommand {
        command: "/bin/sh".into(),
        args: vec!["-c".into(), r"printf lost; printf '\344\270'; sleep 0.25; printf '\255'; printf '\360\237' >&2; sleep 0.1; printf '\246\200' >&2; printf recovered".into()],
        cwd: None, env: Default::default(), timeout: Some(Duration::from_millis(60)), stdin: None,
    }, SandboxedTag::BypassAuditedWithReason { reason: "task_output_writer_test".into() })
    .with_background_task(BackgroundTaskBinding { task_id:id,output_path:path.clone(),on_exit:Some(sink.clone()),on_demand:None });
    let process = platform_posix::PosixProcess::new();
    let handle = if auto {
        match process
            .run_foreground_with_output_limit(&command, Some(0))
            .await
            .unwrap()
            .outcome
        {
            platform_api::ForegroundOutcome::MovedToBackground(handle) => handle,
            other => panic!("must background: {other:?}"),
        }
    } else {
        process.spawn_background(&command).await.unwrap()
    };
    let done = tokio::time::timeout(Duration::from_secs(10), sink.done.notified()).await;
    if done.is_err() {
        let _ = process.kill(&handle).await;
    }
    done.unwrap();
    let body = std::fs::read_to_string(&path).unwrap();
    assert!(
        body.contains("[output omitted: it could not be written to disk]"),
        "{body:?}"
    );
    assert!(
        !body.contains("lost"),
        "failed payload must not be replayed: {body:?}"
    );
    assert!(
        body.contains('中') && body.contains("[stderr] 🦀") && body.contains("recovered"),
        "{body:?}"
    );
    assert!(
        !body.contains('\u{fffd}'),
        "cross-read UTF-8 must survive: {body:?}"
    );
    assert!(body.ends_with("[exited with code 0]\n"), "{body:?}");
    assert!(sink.flushed.load(Ordering::SeqCst));
    assert!(sink.lost.load(Ordering::SeqCst));
    assert_eq!(
        fs.failures.load(Ordering::SeqCst),
        0,
        "runner must actually use the failing manager"
    );
}

#[tokio::test]
async fn explicit_background_uses_managed_writer_failure_recovery() {
    real_background_writer_recovers(false).await;
}
#[tokio::test]
async fn automatic_background_uses_managed_writer_and_keeps_utf8_carry() {
    real_background_writer_recovers(true).await;
}

// Runs inside separate source/supervisor test processes. The supervisor uses
// the production host factory, including the real bounded TaskOutputManager.
#[tokio::test]
async fn supervised_spill_process_helper() {
    let Ok(role) = std::env::var("LINGXI_SPILL_TEST_ROLE") else {
        return;
    };
    let directory = PathBuf::from(std::env::var_os("LINGXI_SPILL_TEST_DIR").unwrap());
    if role == "supervisor" {
        harness_runtime::desktop::shell_supervisor::serve_supervisor(
            directory,
            harness_runtime::desktop::supervisor_exit_sink,
        )
        .await
        .unwrap();
        return;
    }
    harness_runtime::desktop::shell_supervisor::enable_supervisor(PathBuf::from(
        std::env::var_os("LINGXI_SPILL_TEST_WRAPPER").unwrap(),
    ));
    if role == "bash" {
        supervised_real_bash_source(&directory).await;
        return;
    }
    let output = directory.join("b1234567.output");
    let command = SandboxedCommand::__new_sandboxed(
        ProcessCommand {
            command: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                "head -c 68157440 /dev/zero | tr '\\000' x".into(),
            ],
            cwd: Some(directory.clone()),
            env: Default::default(),
            timeout: Some(Duration::from_secs(30)),
            stdin: None,
        },
        SandboxedTag::BypassAuditedWithReason {
            reason: "supervisor spool regression".into(),
        },
    )
    .with_background_task(BackgroundTaskBinding {
        task_id: "b1234567".into(),
        output_path: output.clone(),
        on_exit: Some(harness_runtime::desktop::supervisor_exit_sink(&output)),
        on_demand: None,
    });
    let result = platform_posix::PosixProcess::new()
        .run_foreground_with_output_limit(&command, Some(16))
        .await
        .unwrap();
    assert!(matches!(
        result.outcome,
        platform_api::ForegroundOutcome::Completed(_)
    ));
    std::fs::write(
        directory.join("result.json"),
        serde_json::to_vec(&result).unwrap(),
    )
    .unwrap();
}

#[tokio::test]
async fn supervised_real_manager_caps_completed_spill_and_preserves_original_size() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let executable = std::env::current_exe().unwrap();
    let quoted = format!(
        "'{}'",
        executable.to_string_lossy().replace('\'', "'\"'\"'")
    );
    let wrapper = directory.path().join("supervisor-bootstrap");
    std::fs::write(&wrapper, format!(
        "#!/bin/sh\nexport LINGXI_SPILL_TEST_ROLE=supervisor\nexport LINGXI_SPILL_TEST_DIR=\"$2\"\nexec {quoted} --exact supervised_spill_process_helper --nocapture\n"
    )).unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    let status = tokio::time::timeout(
        Duration::from_secs(40),
        tokio::process::Command::new(executable)
            .args(["--exact", "supervised_spill_process_helper", "--nocapture"])
            .env("LINGXI_SPILL_TEST_ROLE", "source")
            .env("LINGXI_SPILL_TEST_DIR", directory.path())
            .env("LINGXI_SPILL_TEST_WRAPPER", wrapper)
            .status(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(status.success());
    let result: platform_api::ForegroundRunResult =
        serde_json::from_slice(&std::fs::read(directory.path().join("result.json")).unwrap())
            .unwrap();
    let file = result.output_file.expect("real managed output must spill");
    assert_eq!(
        file.size,
        65 * 1024 * 1024,
        "report the original, pre-cap byte count"
    );
    assert_eq!(
        std::fs::metadata(&file.path).unwrap().len(),
        64 * 1024 * 1024
    );
    use std::io::{Read, Seek, SeekFrom};
    let mut spool = std::fs::File::open(file.path).unwrap();
    let mut byte = [0u8; 1];
    spool.read_exact(&mut byte).unwrap();
    assert_eq!(byte, [b'x']);
    spool.seek(SeekFrom::End(-1)).unwrap();
    spool.read_exact(&mut byte).unwrap();
    assert_eq!(byte, [b'x']);
}

fn supervised_registry(
    directory: &Path,
) -> (
    Arc<tasks::registry::TaskRegistry>,
    tool_api::BuiltinToolContext,
) {
    let fs = Arc::new(platform_posix::PosixFileSystem::new(directory.to_owned()));
    let output = Arc::new(tasks::output_manager::TaskOutputManager::new(
        directory.to_owned(),
        fs.clone(),
    ));
    let mut registry = tasks::registry::TaskRegistry::new(
        Arc::new(platform_posix::PosixRuntime::new()),
        fs.clone(),
        output,
    );
    let mut ctx = tool_api::test_support::shell_test_ctx(platform_api::ProcessOutput {
        stdout: String::new(),
        stderr: String::new(),
        exit_code: 0,
        timed_out: false,
    });
    ctx.fs = fs;
    ctx.process = Arc::new(platform_posix::PosixProcess::new());
    ctx.session_cwd =
        tool_api::session_cwd::SessionCwd::new(directory.to_owned(), vec![directory.to_owned()]);
    let status = Arc::new(tasks::registry_status_sink::RegistryStatusSink::new());
    tasks::registry::register_self_contained_handlers(
        &mut registry,
        ctx.process.clone(),
        ctx.sandbox.clone(),
        Arc::new(mcp::McpRegistry::new(Arc::new(
            platform_posix::PosixMcpTransport::new(),
        ))),
        status.clone(),
    );
    let registry = Arc::new(registry);
    status.bind(registry.clone());
    ctx.task_registry = Some(registry.clone());
    (registry, ctx)
}
async fn supervised_real_bash_source(directory: &Path) {
    use tool_api::Tool;
    let (registry, ctx) = supervised_registry(directory);
    tool_shell::bash::BashTool::new(ctx).call(serde_json::json!({"command":"printf before; sleep 2; printf after", "run_in_background":true}),tool_api::test_support::fresh_ctx(),tool_api::test_support::fresh_tx()).await.unwrap();
    let handoff = registry.export_shell_handoff().await.unwrap();
    assert_eq!(
        handoff.len(),
        1,
        "a real Bash row must contain the native PID and be exportable"
    );
    let ids = vec![handoff[0].task_id.clone()];
    assert_eq!(registry.commit_shell_handoff(&ids).await.unwrap(), ids);
    std::fs::write(
        directory.join("bash-handoff.json"),
        serde_json::to_vec(&handoff).unwrap(),
    )
    .unwrap();
}
#[tokio::test]
async fn supervised_real_bash_registry_exports_after_source_exit_and_adopts() {
    use std::os::unix::fs::PermissionsExt;
    let source = tempfile::tempdir().unwrap();
    let executable = std::env::current_exe().unwrap();
    let quoted = format!(
        "'{}'",
        executable.to_string_lossy().replace('\'', "'\"'\"'")
    );
    let wrapper = source.path().join("supervisor-bootstrap");
    std::fs::write(&wrapper,format!("#!/bin/sh\nexport LINGXI_SPILL_TEST_ROLE=supervisor\nexport LINGXI_SPILL_TEST_DIR=\"$2\"\nexec {quoted} --exact supervised_spill_process_helper --nocapture\n")).unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    let status = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::process::Command::new(executable)
            .args(["--exact", "supervised_spill_process_helper", "--nocapture"])
            .env("LINGXI_SPILL_TEST_ROLE", "bash")
            .env("LINGXI_SPILL_TEST_DIR", source.path())
            .env("LINGXI_SPILL_TEST_WRAPPER", wrapper)
            .status(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(status.success());
    let handoff: Vec<platform_api::shell_handoff::ShellTaskHandoff> =
        serde_json::from_slice(&std::fs::read(source.path().join("bash-handoff.json")).unwrap())
            .unwrap();
    let target = tempfile::tempdir().unwrap();
    let (registry, _) = supervised_registry(target.path());
    registry.prepare_shell_handoff(&handoff).await.unwrap();
    registry.adopt_shell_handoff(&handoff).await.unwrap();
    let id = &handoff[0].task_id;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let row = TaskRegistryHandle::get(registry.as_ref(), id)
                .await
                .unwrap();
            if row.unwrap().status == "completed" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    let output = std::fs::read_to_string(&handoff[0].process.output_path).unwrap();
    assert!(output.contains("before") && output.contains("after"));
    assert_eq!(output.matches("[exited with code 0]").count(), 1);
}
