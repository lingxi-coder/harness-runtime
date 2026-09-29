//! Single-spawn foreground/background capture shared by Windows Bash paths.
use super::runner::{StreamingProcessTreeGuard, WindowsProcess, DEFAULT_TIMEOUT};
use mobile_linux_api::{ProcessError, ProcessOutput};
use platform_api::task_output::Utf8StreamDecoder;
use platform_api::{
    BackgroundTaskBinding, ForegroundOutcome, ForegroundRunResult, ProcessHandle,
    ProcessOutputFile, SandboxedCommand,
};
use std::process::Stdio;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Capture {
    binding: BackgroundTaskBinding,
    file: Option<tokio::fs::File>,
    spilled: bool,
    size: u64,
    limit: Option<usize>,
    stdout: String,
    stderr: String,
    decoders: [Utf8StreamDecoder; 2],
}
impl Capture {
    fn new(cmd: &SandboxedCommand, limit: Option<usize>) -> Self {
        let binding = cmd.background_task().cloned().unwrap_or_else(|| {
            let task_id = super::runner::generate_task_id();
            BackgroundTaskBinding {
                output_path: platform_api::task_output::legacy_output_path(&task_id),
                task_id,
                on_exit: None,
                on_demand: None,
            }
        });
        Self {
            binding,
            file: None,
            spilled: false,
            size: 0,
            limit,
            stdout: String::new(),
            stderr: String::new(),
            decoders: Default::default(),
        }
    }
    async fn publish(&mut self, content: &str) -> Result<(), ProcessError> {
        if let Some(sink) = self
            .binding
            .on_exit
            .as_ref()
            .filter(|sink| sink.manages_output())
        {
            sink.append_output(&self.binding.task_id, content).await?;
        } else {
            if self.file.is_none() {
                let parent = self
                    .binding
                    .output_path
                    .parent()
                    .ok_or_else(|| ProcessError::Io("output path has no parent".into()))?;
                let name = self
                    .binding
                    .output_path
                    .file_name()
                    .ok_or_else(|| ProcessError::Io("output path has no name".into()))?;
                tokio::fs::create_dir_all(parent).await.map_err(io_error)?;
                let file = platform_api::rooted_fs::open_append_file_pinned(
                    parent,
                    std::path::Path::new(name),
                    None,
                )
                .map_err(|error| ProcessError::Io(error.to_string()))?;
                self.file = Some(tokio::fs::File::from_std(file));
            }
            self.file
                .as_mut()
                .unwrap()
                .write_all(content.as_bytes())
                .await
                .map_err(io_error)?;
        }
        self.size = self.size.saturating_add(content.len() as u64);
        Ok(())
    }
    async fn spill(&mut self) -> Result<(), ProcessError> {
        if self.spilled {
            return Ok(());
        }
        let prefix = format!(
            "{}{}{}",
            self.stdout,
            if self.stderr.is_empty() {
                ""
            } else {
                "[stderr] "
            },
            self.stderr
        );
        self.publish(&prefix).await?;
        self.spilled = true;
        Ok(())
    }
    async fn chunk(&mut self, bytes: &[u8], stderr: bool, eof: bool) -> Result<(), ProcessError> {
        let text = self.decoders[usize::from(stderr)].decode(bytes, eof);
        if self.spilled {
            self.publish(&if stderr && !text.is_empty() {
                format!("[stderr] {text}")
            } else {
                text.clone()
            })
            .await?;
        }
        let buffer = if stderr {
            &mut self.stderr
        } else {
            &mut self.stdout
        };
        buffer.push_str(&text);
        if !self.spilled
            && self
                .limit
                .is_some_and(|limit| self.stdout.len() + self.stderr.len() > limit)
        {
            self.spill().await?;
        }
        if self.spilled {
            // Keep a bounded tail for Bash's final cwd marker and inline hints.
            let retain = self.limit.unwrap_or(8192).max(8192);
            for buffer in [&mut self.stdout, &mut self.stderr] {
                if buffer.len() > retain {
                    let mut start = buffer.len() - retain;
                    while !buffer.is_char_boundary(start) {
                        start += 1;
                    }
                    buffer.drain(..start);
                }
            }
        }
        Ok(())
    }
    async fn flush(&mut self) -> Result<(), ProcessError> {
        if let Some(sink) = self
            .binding
            .on_exit
            .as_ref()
            .filter(|sink| sink.manages_output())
        {
            sink.flush_output(&self.binding.task_id).await?;
        }
        if let Some(file) = self.file.as_mut() {
            file.flush().await.map_err(io_error)?;
        }
        Ok(())
    }
    async fn finalize_persisted(&mut self) -> Result<(), ProcessError> {
        if !self.spilled {
            return Ok(());
        }
        let cap = platform_api::task_output::MAX_PERSISTED_OUTPUT_BYTES;
        if let Some(sink) = self
            .binding
            .on_exit
            .as_ref()
            .filter(|sink| sink.manages_output())
        {
            if let Some(size) = sink
                .finalize_persisted_output(&self.binding.task_id, cap)
                .await?
            {
                self.size = size;
                return Ok(());
            }
        }
        let file = if let Some(file) = self.file.as_ref() {
            file.try_clone().await.map_err(io_error)?
        } else {
            let parent = self
                .binding
                .output_path
                .parent()
                .ok_or_else(|| ProcessError::Io("output path has no parent".into()))?;
            let name = self
                .binding
                .output_path
                .file_name()
                .ok_or_else(|| ProcessError::Io("output path has no name".into()))?;
            tokio::fs::File::from_std(
                platform_api::rooted_fs::open_append_file_pinned(
                    parent,
                    std::path::Path::new(name),
                    None,
                )
                .map_err(|error| ProcessError::Io(error.to_string()))?,
            )
        };
        self.size = file.metadata().await.map_err(io_error)?.len();
        if self.size > cap {
            file.set_len(cap).await.map_err(io_error)?;
        }
        Ok(())
    }
    fn metadata(&self) -> Option<ProcessOutputFile> {
        self.spilled.then(|| ProcessOutputFile {
            task_id: self.binding.task_id.clone(),
            path: self.binding.output_path.to_string_lossy().into_owned(),
            size: self.size,
        })
    }
}
fn io_error(error: std::io::Error) -> ProcessError {
    ProcessError::Io(error.to_string())
}

async fn drain(
    child: &mut tokio::process::Child,
    stdout: &mut tokio::process::ChildStdout,
    stderr: &mut tokio::process::ChildStderr,
    capture: &mut Capture,
    mut eof: [bool; 2],
) -> Result<std::process::ExitStatus, ProcessError> {
    let mut out = [0; 8192];
    let mut err = [0; 8192];
    while !eof.iter().all(|eof| *eof) {
        tokio::select! {
            result = stdout.read(&mut out), if !eof[0] => { let n = result.map_err(io_error)?; eof[0] = n == 0; capture.chunk(&out[..n], false, n == 0).await?; }
            result = stderr.read(&mut err), if !eof[1] => { let n = result.map_err(io_error)?; eof[1] = n == 0; capture.chunk(&err[..n], true, n == 0).await?; }
        }
    }
    child.wait().await.map_err(io_error)
}

pub(super) async fn run(
    cmd: &SandboxedCommand,
    limit: Option<usize>,
    explicit: bool,
) -> Result<ForegroundRunResult, ProcessError> {
    let mut command = WindowsProcess::build_command(cmd);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    let gated = super::supervisor_gate::enabled();
    #[cfg(windows)]
    if gated {
        command.creation_flags(0x0000_0004);
    } // CREATE_SUSPENDED
    let mut child = command.spawn().map_err(io_error)?;
    let pid = child
        .id()
        .ok_or_else(|| ProcessError::Io("spawned child has no pid".into()))?;
    let mut guard = StreamingProcessTreeGuard(Some(pid));
    #[cfg(windows)]
    let initial_thread = if gated {
        match super::supervisor_gate::suspended_thread(&child) {
            Ok(thread) => Some(thread),
            Err(error) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                guard.0 = None;
                return Err(error);
            }
        }
    } else {
        None
    };

    if let Some((binding, sink)) = cmd
        .background_task()
        .and_then(|binding| binding.on_exit.as_ref().map(|sink| (binding, sink)))
    {
        if let Err(error) = sink.on_spawn(&binding.task_id, pid).await {
            let _ = super::kill_tree::kill_tree_windows(pid).await;
            let _ = child.kill().await;
            let _ = child.wait().await;
            guard.0 = None;
            return Err(error);
        }
    }

    #[cfg(windows)]
    if let Some(thread) = initial_thread {
        if let Err(error) = super::supervisor_gate::release(thread) {
            let _ = super::kill_tree::kill_tree_windows(pid).await;
            let _ = child.kill().await;
            let _ = child.wait().await;
            guard.0 = None;
            return Err(error);
        }
    }

    let agent_registration =
        platform_api::agent_processes::register(cmd.process_owner(), Some(pid));
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| ProcessError::Io("no stdout".into()))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| ProcessError::Io("no stderr".into()))?;
    if let Some(mut stdin) = child.stdin.take() {
        if let Some(text) = &cmd.inner().stdin {
            stdin.write_all(text.as_bytes()).await.map_err(io_error)?;
        }
    }
    let mut capture = Capture::new(cmd, limit);
    let mut eof = [false; 2];
    let mut timed_out = false;
    if !explicit {
        let deadline = tokio::time::sleep(cmd.inner().timeout.unwrap_or(DEFAULT_TIMEOUT));
        tokio::pin!(deadline);
        let demand = capture.binding.on_demand.clone();
        let requested = async {
            match demand {
                Some(notify) => notify.notified().await,
                None => std::future::pending().await,
            }
        };
        tokio::pin!(requested);
        let mut out = [0; 8192];
        let mut err = [0; 8192];
        loop {
            tokio::select! {
                biased;
                () = &mut requested => break,
                () = &mut deadline => { timed_out = true; break; }
                result = child.wait(), if eof.iter().all(|eof| *eof) => {
                    let status = result.map_err(io_error)?;
                    capture.flush().await?; capture.finalize_persisted().await?; guard.0 = None;
                    return Ok(ForegroundRunResult {output_file: capture.metadata(), outcome: ForegroundOutcome::Completed(ProcessOutput {stdout: capture.stdout, stderr: capture.stderr, exit_code: status.code().unwrap_or(-1), timed_out: false})});
                }
                result = stdout.read(&mut out), if !eof[0] => { let n = result.map_err(io_error)?; eof[0] = n == 0; capture.chunk(&out[..n], false, n == 0).await?; }
                result = stderr.read(&mut err), if !eof[1] => { let n = result.map_err(io_error)?; eof[1] = n == 0; capture.chunk(&err[..n], true, n == 0).await?; }
            }
        }
        // A starved deadline/request must not background an already exited PID.
        if let Some(status) = child.try_wait().map_err(io_error)? {
            if let Ok(result) = tokio::time::timeout(
                std::time::Duration::from_millis(50),
                drain(&mut child, &mut stdout, &mut stderr, &mut capture, eof),
            )
            .await
            {
                result?;
            }
            capture.flush().await?;
            capture.finalize_persisted().await?;
            guard.0 = None;
            return Ok(ForegroundRunResult {
                output_file: capture.metadata(),
                outcome: ForegroundOutcome::Completed(ProcessOutput {
                    stdout: capture.stdout,
                    stderr: capture.stderr,
                    exit_code: status.code().unwrap_or(-1),
                    timed_out: false,
                }),
            });
        }
        if timed_out && !cmd.auto_background_on_timeout() {
            let _ = super::kill_tree::kill_tree_windows(pid).await;
            let _ = child.kill().await;
            let _ = child.wait().await;
            guard.0 = None;
            capture.chunk(&[], false, true).await?;
            capture.chunk(&[], true, true).await?;
            capture.flush().await?;
            capture.finalize_persisted().await?;
            return Ok(ForegroundRunResult {
                output_file: capture.metadata(),
                outcome: ForegroundOutcome::Completed(ProcessOutput {
                    stdout: capture.stdout,
                    stderr: capture.stderr,
                    exit_code: 143,
                    timed_out: true,
                }),
            });
        }
    }
    capture.spill().await?;
    capture.flush().await?;
    let handle = ProcessHandle {
        task_id: capture.binding.task_id.clone(),
        pid,
    };
    tokio::spawn(async move {
        // Ownership crosses only after the complete prefix has been accepted.
        let mut guard = guard;
        let result = drain(&mut child, &mut stdout, &mut stderr, &mut capture, eof).await;
        if result.is_err() {
            let _ = super::kill_tree::kill_tree_windows(pid).await;
            let _ = child.kill().await;
        }
        let status = match result {
            Ok(status) => Some(status),
            Err(_) => child.wait().await.ok(),
        };
        drop(agent_registration);
        let flushed = capture.flush().await;
        guard.0 = None;
        if let Some(sink) = capture.binding.on_exit.as_ref() {
            sink.on_exit(
                &capture.binding.task_id,
                if flushed.is_ok() {
                    status.and_then(|status| status.code())
                } else {
                    None
                },
            )
            .await;
        }
    });
    Ok(ForegroundRunResult {
        outcome: ForegroundOutcome::MovedToBackground(handle),
        output_file: None,
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use platform_api::{BackgroundExitSink, ProcessRunner};
    use std::sync::{Arc, Mutex};
    #[derive(Default)]
    struct Sink {
        reject_output: std::sync::atomic::AtomicBool,
        reject_spawn: std::sync::atomic::AtomicBool,
        spawned: Mutex<Vec<(String, u32)>>,
        byte_cap: std::sync::atomic::AtomicUsize,
        content: Mutex<String>,
        exits: Mutex<Vec<(String, Option<i32>)>>,
    }
    #[async_trait::async_trait]
    impl BackgroundExitSink for Sink {
        async fn on_spawn(&self, id: &str, pid: u32) -> Result<(), ProcessError> {
            self.spawned.lock().unwrap().push((id.to_string(), pid));
            if self.reject_spawn.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(ProcessError::Io("birth identity unavailable".into()));
            }
            Ok(())
        }
        fn manages_output(&self) -> bool {
            true
        }
        async fn append_output(&self, _: &str, text: &str) -> Result<(), ProcessError> {
            if self.reject_output.load(std::sync::atomic::Ordering::SeqCst) && !text.is_empty() {
                return Err(ProcessError::Io("writer refused prefix".into()));
            }
            let mut content = self.content.lock().unwrap();
            content.push_str(text);
            let cap = self.byte_cap.load(std::sync::atomic::Ordering::SeqCst);
            if cap > 0 && content.len() > cap {
                content.truncate(cap);
            }
            Ok(())
        }
        async fn finalize_persisted_output(
            &self,
            _: &str,
            max_bytes: u64,
        ) -> Result<Option<u64>, ProcessError> {
            let mut content = self.content.lock().unwrap();
            let size = content.len() as u64;
            if size > max_bytes {
                content.truncate(max_bytes as usize);
            }
            Ok(Some(size))
        }
        async fn on_exit(&self, id: &str, code: Option<i32>) {
            self.exits.lock().unwrap().push((id.into(), code));
        }
    }
    #[tokio::test]
    async fn windows_spawn_identity_callback_precedes_handoff_and_rejection_reaps_child() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(Sink::default());
        let cmd = command(
            "sleep 0.1; printf done",
            5000,
            dir.path(),
            sink.clone(),
            Arc::new(tokio::sync::Notify::new()),
        );
        let handle = WindowsProcess::new().spawn_background(&cmd).await.unwrap();
        assert_eq!(
            *sink.spawned.lock().unwrap(),
            vec![(handle.task_id.clone(), handle.pid)]
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while sink.exits.lock().unwrap().is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let rejected = Arc::new(Sink::default());
        rejected
            .reject_spawn
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let cmd = command(
            "exec sleep 60",
            5000,
            dir.path(),
            rejected.clone(),
            Arc::new(tokio::sync::Notify::new()),
        );
        assert!(WindowsProcess::new().spawn_background(&cmd).await.is_err());
        let pid = rejected.spawned.lock().unwrap()[0].1;
        assert!(platform_api::live_sessions::process_start_identity(pid).is_none());
        assert!(rejected.exits.lock().unwrap().is_empty());
    }

    fn command(
        script: &str,
        timeout: u64,
        dir: &std::path::Path,
        sink: Arc<Sink>,
        demand: Arc<tokio::sync::Notify>,
    ) -> SandboxedCommand {
        SandboxedCommand::__new_sandboxed(
            platform_api::sandbox::ProcessCommand {
                command: "/bin/sh".into(),
                args: vec!["-c".into(), script.into()],
                cwd: None,
                env: Default::default(),
                stdin: None,
                timeout: Some(std::time::Duration::from_millis(timeout)),
            },
            platform_api::sandbox::SandboxedTag::BypassAuditedWithReason {
                reason: "portable Windows runner test".into(),
            },
        )
        .with_background_task(BackgroundTaskBinding {
            task_id: "bwin00001".into(),
            output_path: dir.join("bwin00001.output"),
            on_exit: Some(sink),
            on_demand: Some(demand),
        })
    }
    async fn finished(sink: &Sink) {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while sink.exits.lock().unwrap().is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn windows_explicit_background_uses_bound_identity_managed_output_and_one_exit() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(Sink::default());
        let command = command(
            "printf '%s\\n' $$; printf err >&2; sleep 0.05; printf done; exit 7",
            1000,
            dir.path(),
            sink.clone(),
            Arc::new(tokio::sync::Notify::new()),
        );
        let handle = WindowsProcess::new()
            .spawn_background(&command)
            .await
            .unwrap();
        assert_eq!(handle.task_id, "bwin00001");
        finished(&sink).await;
        let text = sink.content.lock().unwrap().clone();
        assert!(
            text.contains(&format!("{}\n", handle.pid)),
            "returned PID must be the one spawned child: {text}"
        );
        assert!(
            text.contains("done") && text.contains("[stderr] err"),
            "{text}"
        );
        assert_eq!(*sink.exits.lock().unwrap(), [("bwin00001".into(), Some(7))]);
        assert!(
            !dir.path().join("bwin00001.output").exists(),
            "managed writer cannot be bypassed by a second direct file"
        );
    }
    #[tokio::test]
    async fn windows_timeout_and_on_demand_detach_same_child_and_preserve_utf8_carry() {
        for on_demand in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let sink = Arc::new(Sink::default());
            let demand = Arc::new(tokio::sync::Notify::new());
            let command = command("printf '%s\\n' $$; printf '\\342'; sleep 0.1; printf '\\202\\254'; printf err >&2; exit 0", if on_demand {3000} else {30}, dir.path(), sink.clone(), demand.clone()).with_auto_background_on_timeout(!on_demand);
            let run =
                tokio::spawn(async move { WindowsProcess::new().run_foreground(&command).await });
            if on_demand {
                tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                demand.notify_one();
            }
            let result = tokio::time::timeout(std::time::Duration::from_secs(1), run)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let ForegroundOutcome::MovedToBackground(handle) = result else {
                panic!("expected same-child handoff")
            };
            assert_eq!(handle.task_id, "bwin00001");
            finished(&sink).await;
            let text = sink.content.lock().unwrap().clone();
            assert!(
                text.contains(&handle.pid.to_string())
                    && text.contains('€')
                    && !text.contains('\u{fffd}'),
                "{text}"
            );
            assert_eq!(sink.exits.lock().unwrap().len(), 1);
        }
    }
    #[tokio::test]
    async fn windows_completed_spill_uses_managed_writer_without_background_exit() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(Sink::default());
        let command = command(
            "printf abcdefghijklmnopqrstuvwxyz; printf err >&2",
            1000,
            dir.path(),
            sink.clone(),
            Arc::new(tokio::sync::Notify::new()),
        );
        let result = WindowsProcess::new()
            .run_foreground_with_output_limit(&command, Some(8))
            .await
            .unwrap();
        assert!(matches!(result.outcome, ForegroundOutcome::Completed(_)));
        let file = result.output_file.unwrap();
        assert_eq!(file.task_id, "bwin00001");
        assert!(sink
            .content
            .lock()
            .unwrap()
            .contains("abcdefghijklmnopqrstuvwxyz"));
        assert!(sink.exits.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn windows_persisted_size_reports_writer_cap_not_attempted_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(Sink::default());
        sink.byte_cap.store(10, std::sync::atomic::Ordering::SeqCst);
        let command = command(
            "printf abcdefghijklmnopqrstuvwxyz",
            1000,
            dir.path(),
            sink.clone(),
            Arc::new(tokio::sync::Notify::new()),
        );
        let result = WindowsProcess::new()
            .run_foreground_with_output_limit(&command, Some(8))
            .await
            .unwrap();
        assert_eq!(result.output_file.unwrap().size, 10);
        assert_eq!(sink.content.lock().unwrap().len(), 10);
    }

    #[tokio::test]
    async fn windows_completed_copy_cap_keeps_pretruncate_size() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(Sink::default());
        let cap = platform_api::task_output::MAX_PERSISTED_OUTPUT_BYTES as usize;
        *sink.content.lock().unwrap() = "a".repeat(cap + 3);
        let command = command(
            "true",
            1000,
            dir.path(),
            sink.clone(),
            Arc::new(tokio::sync::Notify::new()),
        );
        let mut capture = Capture::new(&command, Some(8));
        capture.spilled = true;
        capture.finalize_persisted().await.unwrap();
        assert_eq!(capture.metadata().unwrap().size, cap as u64 + 3);
        assert_eq!(
            sink.content.lock().unwrap().len(),
            cap,
            "N7e returns pre-truncation size but retains at most VAe bytes"
        );
    }

    #[tokio::test]
    async fn windows_failed_spool_handoff_never_acknowledges_detachment() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(Sink::default());
        sink.reject_output
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let marker = dir.path().join("continued");
        let command = command(
            &format!(
                "printf prefix; sleep 0.15; printf bad > '{}'",
                marker.display()
            ),
            30,
            dir.path(),
            sink.clone(),
            Arc::new(tokio::sync::Notify::new()),
        );
        assert!(
            matches!(WindowsProcess::new().run_foreground(&command).await, Err(ProcessError::Io(message)) if message.contains("writer refused"))
        );
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(
            !marker.exists(),
            "a failed prefix commit must not leave the shell detached"
        );
        assert!(sink.exits.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn windows_nonautomatic_timeout_returns_interrupted_foreground_result() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(Sink::default());
        let command = command(
            "printf before; sleep 1",
            20,
            dir.path(),
            sink.clone(),
            Arc::new(tokio::sync::Notify::new()),
        )
        .with_auto_background_on_timeout(false);
        let ForegroundOutcome::Completed(output) = WindowsProcess::new()
            .run_foreground(&command)
            .await
            .unwrap()
        else {
            panic!("deadline must not detach an ineligible command")
        };
        assert_eq!(output.exit_code, 143);
        assert!(output.timed_out);
        assert_eq!(output.stdout, "before");
        assert!(sink.exits.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn windows_process_owner_snapshot_isolates_real_children_and_releases_after_reap() {
        let dir = tempfile::tempdir().unwrap();
        let first = Arc::new(Sink::default());
        let second = Arc::new(Sink::default());
        let owner = format!("windows-owner-{}", dir.path().display());
        let other_owner = format!("windows-other-{}", dir.path().display());
        let first_cmd = command(
            "exec sleep 0.15",
            1000,
            dir.path(),
            first.clone(),
            Arc::new(tokio::sync::Notify::new()),
        )
        .with_process_owner(Some(owner.clone()));
        let second_cmd = command(
            "exec sleep 0.3",
            1000,
            dir.path(),
            second.clone(),
            Arc::new(tokio::sync::Notify::new()),
        )
        .with_process_owner(Some(other_owner.clone()));
        let runner = WindowsProcess::new();
        let first_handle = runner.spawn_background(&first_cmd).await.unwrap();
        let second_handle = runner.spawn_background(&second_cmd).await.unwrap();
        // On macOS taskkill is absent; the portable witness checks its real
        // PID snapshot and scope, while native Windows supplies tree-kill.
        assert_eq!(
            runner.kill_owner_processes(&owner).await,
            vec![first_handle.pid]
        );
        assert_eq!(
            platform_api::agent_processes::snapshot(&other_owner),
            vec![second_handle.pid]
        );
        finished(&first).await;
        finished(&second).await;
        assert!(platform_api::agent_processes::snapshot(&owner).is_empty());
        assert!(platform_api::agent_processes::snapshot(&other_owner).is_empty());
    }

    #[tokio::test]
    async fn windows_closed_pipes_do_not_disable_the_background_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(Sink::default());
        let command = command(
            "exec 1>&- 2>&-; sleep 0.2",
            20,
            dir.path(),
            sink.clone(),
            Arc::new(tokio::sync::Notify::new()),
        );
        assert!(matches!(
            WindowsProcess::new()
                .run_foreground(&command)
                .await
                .unwrap(),
            ForegroundOutcome::MovedToBackground(_)
        ));
        finished(&sink).await;
    }
}
