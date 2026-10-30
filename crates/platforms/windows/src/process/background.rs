//! Single-spawn foreground/background capture shared by Windows Bash paths.
use super::runner::{StreamingProcessTreeGuard, WindowsProcess, DEFAULT_TIMEOUT};
use lingxi_core::host::task_output::Utf8StreamDecoder;
use lingxi_core::host::{
    BackgroundTaskBinding, ForegroundOutcome, ForegroundRunResult, ProcessHandle,
    ProcessOutputFile, SandboxedCommand,
};
use mobile_linux_api::{ProcessError, ProcessOutput};
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
                output_path: lingxi_core::host::task_output::legacy_output_path(&task_id),
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
                let file = lingxi_core::host::rooted_fs::open_append_file_pinned(
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
    async fn persist_supervised_inline(&mut self) -> Result<(), ProcessError> {
        if !self.spilled
            && (!self.stdout.is_empty() || !self.stderr.is_empty())
            && self
                .binding
                .on_exit
                .as_ref()
                .and_then(|sink| sink.stop_notify())
                .is_some()
        {
            // A disconnected source cannot receive the inline result. Keep a
            // durable copy for the supervisor without changing the ordinary
            // foreground result's output-file metadata.
            self.spill().await?;
            let result = async {
                self.flush().await?;
                self.finalize_persisted().await
            }
            .await;
            self.spilled = false;
            result?;
        }
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
        let cap = lingxi_core::host::task_output::MAX_PERSISTED_OUTPUT_BYTES;
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
                lingxi_core::host::rooted_fs::open_append_file_pinned(
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

fn start_child_kill(child: &mut tokio::process::Child) -> Result<(), ProcessError> {
    #[cfg(all(test, unix))]
    {
        static FAILURES: std::sync::OnceLock<std::sync::atomic::AtomicUsize> =
            std::sync::OnceLock::new();
        let failures = FAILURES.get_or_init(|| {
            std::sync::atomic::AtomicUsize::new(
                std::env::var("HARNESS_TEST_CHILD_KILL_FAILURES")
                    .ok()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(0),
            )
        });
        if failures
            .fetch_update(
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
                |remaining| remaining.checked_sub(1),
            )
            .is_ok()
        {
            return Err(ProcessError::Io("injected child signal failure".into()));
        }
    }
    child.start_kill().map_err(io_error)
}

#[derive(Default)]
struct DrainDeadline {
    at: Option<tokio::time::Instant>,
    expired: bool,
    #[cfg(windows)]
    job: Option<super::supervisor_gate::CommandJob>,
}

impl DrainDeadline {
    fn completed_foreground(&self) -> Result<(), ProcessError> {
        #[cfg(windows)]
        if let Some(job) = &self.job {
            job.disarm_on_close()?;
        }
        Ok(())
    }

    async fn terminate(&self, child: &mut tokio::process::Child) -> Result<(), ProcessError> {
        #[cfg(windows)]
        if let Some(job) = &self.job {
            return job.terminate();
        }
        if let Some(pid) = child.id() {
            let _ = super::kill_tree::kill_tree_windows(pid).await;
            child.kill().await.map_err(io_error)?;
        }
        Ok(())
    }

    async fn stop_supervised_tree(
        &self,
        child: &mut tokio::process::Child,
    ) -> Result<(), ProcessError> {
        #[cfg(windows)]
        {
            let _ = child;
            self.job
                .as_ref()
                .ok_or_else(|| ProcessError::Io("missing supervised payload job".into()))?
                .terminate()
        }
        #[cfg(not(windows))]
        {
            let pid = child
                .id()
                .ok_or_else(|| ProcessError::Io("supervised child already reaped".into()))?;
            super::kill_tree::kill_tree_windows(pid).await
        }
    }
}

async fn drain(
    child: &mut tokio::process::Child,
    stdout: &mut tokio::process::ChildStdout,
    stderr: &mut tokio::process::ChildStderr,
    capture: &mut Capture,
    mut eof: [bool; 2],
    deadline: &mut DrainDeadline,
    stop: Option<std::sync::Arc<tokio::sync::Notify>>,
) -> Result<std::process::ExitStatus, ProcessError> {
    let mut out = [0; 8192];
    let mut err = [0; 8192];
    let mut deadline_handled = false;
    let mut stop_handled = false;
    loop {
        tokio::select! {
            // A completed publish always finishes before we poll the timer:
            // consumed bytes and the UTF-8 decoder state cannot be cancelled
            // halfway through an output-manager append.
            biased;
            () = async {
                if let Some(stop) = &stop { stop.notified().await; }
                else { std::future::pending::<()>().await; }
            }, if !stop_handled => {
                stop_handled = true;
                deadline_handled = true;
                let tree = deadline.stop_supervised_tree(child).await;
                #[cfg(windows)]
                let killed = if tree.is_ok() { Ok(()) } else { start_child_kill(child) };
                #[cfg(not(windows))]
                let killed = start_child_kill(child);
                // A failed tree stop may leave descendants holding the pipes.
                // Return the failure without waiting indefinitely for EOF.
                tree?;
                killed?;
            }
            () = async {
                if let Some(at) = deadline.at { tokio::time::sleep_until(at).await; }
                else { std::future::pending::<()>().await; }
            }, if !deadline_handled => {
                deadline_handled = true;
                deadline.terminate(child).await?;
                deadline.expired = true;
            }
            result = child.wait(), if eof.iter().all(|eof| *eof) => return result.map_err(io_error),
            result = stdout.read(&mut out), if !eof[0] => { let n = result.map_err(io_error)?; eof[0] = n == 0; capture.chunk(&out[..n], false, n == 0).await?; }
            result = stderr.read(&mut err), if !eof[1] => { let n = result.map_err(io_error)?; eof[1] = n == 0; capture.chunk(&err[..n], true, n == 0).await?; }
        }
    }
}

/// A failed stop must not block the request or relinquish its still-live
/// process capability. Keep the child and its job until a later wait succeeds.
struct RetainedOutput {
    stdout: tokio::process::ChildStdout,
    stderr: tokio::process::ChildStderr,
    capture: Capture,
    eof: [bool; 2],
    // Only a detached background reaper needs to send on_exit after the
    // failure has been reported. Foreground cleanup has no reaper callback.
    reported: Option<tokio::sync::oneshot::Receiver<()>>,
}

async fn retain_failed_child(
    mut child: tokio::process::Child,
    deadline: DrainDeadline,
    registration: Option<lingxi_core::host::agent_processes::Registration>,
    cleanup: Option<(
        std::sync::Arc<dyn lingxi_core::host::BackgroundExitSink>,
        String,
    )>,
    output: Option<RetainedOutput>,
) {
    if let Some((sink, task_id)) = &cleanup {
        sink.on_supervised_cleanup_pending(task_id).await;
    }
    // Cleanup now owns concrete handles, so the owner index must not retain a
    // PID after this task eventually reaps it.
    drop(registration);
    tokio::spawn(async move {
        loop {
            #[cfg(windows)]
            let tree_stopped = deadline
                .job
                .as_ref()
                .is_some_and(|job| job.terminate().is_ok());
            #[cfg(not(windows))]
            let tree_stopped = true;
            let _ = start_child_kill(&mut child);
            let waited = matches!(
                tokio::time::timeout(std::time::Duration::from_secs(1), child.wait()).await,
                Ok(Ok(_))
            );
            if tree_stopped && waited {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        // Close the job only after the held child was waited; its close also
        // terminates any remaining members. The failed operation stays failed.
        drop(deadline);
        if let Some(mut output) = output {
            let drained = drain(
                &mut child,
                &mut output.stdout,
                &mut output.stderr,
                &mut output.capture,
                output.eof,
                &mut DrainDeadline::default(),
                None,
            )
            .await;
            // Failed foreground operations cannot return their inline prefix.
            // Persist it together with any bytes drained after the failure.
            let spilled = output.capture.spill().await;
            let flushed = output.capture.flush().await;
            let finalized = output.capture.finalize_persisted().await;
            // The typed Err was already reported. A failed drain still needs
            // an exit notification so the supervisor can close its reaper;
            // the error prevents publishing a successful receipt.
            let _ = (drained, spilled, flushed, finalized);
            if let Some(reported) = output.reported {
                if reported.await.is_ok() {
                    if let Some(sink) = output.capture.binding.on_exit.as_ref() {
                        sink.on_exit(&output.capture.binding.task_id, None).await;
                    }
                }
            }
        }
        if let Some((sink, task_id)) = cleanup {
            sink.on_supervised_cleanup_finished(&task_id).await;
        }
    });
}

#[allow(clippy::too_many_arguments)]
async fn cancel_foreground(
    mut child: tokio::process::Child,
    mut guard: StreamingProcessTreeGuard,
    mut deadline: DrainDeadline,
    mut stdout: tokio::process::ChildStdout,
    mut stderr: tokio::process::ChildStderr,
    mut capture: Capture,
    eof: [bool; 2],
    registration: Option<lingxi_core::host::agent_processes::Registration>,
    settlement: &mut Option<Result<Option<i32>, ProcessError>>,
) -> Result<ForegroundRunResult, ProcessError> {
    let tree = deadline.stop_supervised_tree(&mut child).await;
    #[cfg(windows)]
    let killed = if tree.is_ok() {
        Ok(())
    } else {
        start_child_kill(&mut child)
    };
    #[cfg(not(windows))]
    let killed = start_child_kill(&mut child);
    if killed.is_err() && (cfg!(not(windows)) || tree.is_err()) {
        let error = tree.err().or_else(|| killed.err()).unwrap();
        *settlement = Some(Err(error.clone()));
        guard.0 = None;
        let cleanup = capture
            .binding
            .on_exit
            .clone()
            .map(|sink| (sink, capture.binding.task_id.clone()));
        retain_failed_child(
            child,
            deadline,
            registration,
            cleanup,
            Some(RetainedOutput {
                stdout,
                stderr,
                capture,
                eof,
                reported: None,
            }),
        )
        .await;
        return Err(error);
    }
    let status = match child.wait().await.map_err(io_error) {
        Ok(status) => status,
        Err(error) => {
            *settlement = Some(Err(error.clone()));
            guard.0 = None;
            let cleanup = capture
                .binding
                .on_exit
                .clone()
                .map(|sink| (sink, capture.binding.task_id.clone()));
            retain_failed_child(
                child,
                deadline,
                registration,
                cleanup,
                Some(RetainedOutput {
                    stdout,
                    stderr,
                    capture,
                    eof,
                    reported: None,
                }),
            )
            .await;
            return Err(error);
        }
    };
    // A failed tree stop may leave descendants holding the pipes. Report it
    // after joining the direct child, without waiting indefinitely for EOF.
    guard.0 = None;
    drop(registration);
    let settled = async {
        tree?;
        killed?;
        drain(
            &mut child,
            &mut stdout,
            &mut stderr,
            &mut capture,
            eof,
            &mut deadline,
            None,
        )
        .await?;
        // Cancellation returns an error, so the in-memory short output would
        // otherwise be discarded before the supervisor persists its receipt.
        capture.spill().await?;
        capture.flush().await?;
        capture.finalize_persisted().await?;
        Ok::<_, ProcessError>(status)
    }
    .await;
    *settlement = Some(
        settled
            .as_ref()
            .map(std::process::ExitStatus::code)
            .map_err(Clone::clone),
    );
    settled?;
    Err(ProcessError::Io("supervisor command cancelled".into()))
}

pub(super) async fn run(
    cmd: &SandboxedCommand,
    limit: Option<usize>,
    explicit: bool,
) -> Result<ForegroundRunResult, ProcessError> {
    let mut settlement = None;
    let result = run_inner(cmd, limit, explicit, &mut settlement).await;
    // A background handoff transfers reporting to its child-owning reaper.
    if !matches!(
        &result,
        Ok(ForegroundRunResult {
            outcome: ForegroundOutcome::MovedToBackground(_),
            ..
        })
    ) {
        if let Some((binding, sink)) = cmd
            .background_task()
            .and_then(|binding| binding.on_exit.as_ref().map(|sink| (binding, sink)))
        {
            let settlement = settlement.unwrap_or_else(|| match &result {
                Err(error) => Err(error.clone()),
                Ok(_) => Err(ProcessError::Io(
                    "foreground runner did not verify complete output settlement".into(),
                )),
            });
            sink.on_supervised_runner_settled(&binding.task_id, settlement)
                .await;
        }
    }
    result
}

async fn run_inner(
    cmd: &SandboxedCommand,
    limit: Option<usize>,
    explicit: bool,
    settlement: &mut Option<Result<Option<i32>, ProcessError>>,
) -> Result<ForegroundRunResult, ProcessError> {
    let mut command = WindowsProcess::build_command(cmd);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x0000_0004); // CREATE_SUSPENDED until payload job ownership is installed.
    let mut child = command.spawn().map_err(io_error)?;
    let pid = child
        .id()
        .ok_or_else(|| ProcessError::Io("spawned child has no pid".into()))?;
    let mut guard = StreamingProcessTreeGuard(Some(pid));
    let mut deadline = DrainDeadline::default();
    #[cfg(windows)]
    let initial_thread = {
        let owned = super::supervisor_gate::CommandJob::assign_suspended(&child).and_then(|job| {
            deadline.job = Some(job);
            super::supervisor_gate::suspended_thread(&child)
        });
        match owned {
            Ok(thread) => thread,
            Err(error) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                guard.0 = None;
                return Err(error);
            }
        }
    };

    if let Some((binding, sink)) = cmd
        .background_task()
        .and_then(|binding| binding.on_exit.as_ref().map(|sink| (binding, sink)))
    {
        if let Err(error) = sink.on_spawn(&binding.task_id, pid).await {
            // On Windows the initial thread is still suspended, so a waited
            // termination proves no payload or inherited output could run.
            // A failed signal or wait instead leaves cleanup holding the job
            // and Child; never turn that failure into a cancellation receipt.
            let stopped = match start_child_kill(&mut child) {
                Ok(()) => child.wait().await.map_err(io_error),
                Err(cause) => Err(cause),
            };
            guard.0 = None;
            match stopped {
                Ok(status) => {
                    #[cfg(windows)]
                    {
                        *settlement = Some(Ok(status.code()));
                        return Err(error);
                    }
                    #[cfg(not(windows))]
                    {
                        let _ = (status, error);
                        let cause = ProcessError::Io(
                            "unaccepted child terminated without a suspended payload".into(),
                        );
                        *settlement = Some(Err(cause.clone()));
                        return Err(cause);
                    }
                }
                Err(cause) => {
                    *settlement = Some(Err(cause.clone()));
                    retain_failed_child(
                        child,
                        deadline,
                        None,
                        Some((sink.clone(), binding.task_id.clone())),
                        None,
                    )
                    .await;
                    return Err(cause);
                }
            }
        }
    }

    #[cfg(windows)]
    {
        if let Err(error) = super::supervisor_gate::release(initial_thread) {
            let _ = super::kill_tree::kill_tree_windows(pid).await;
            let _ = child.kill().await;
            let _ = child.wait().await;
            guard.0 = None;
            return Err(error);
        }
    }

    let agent_registration =
        lingxi_core::host::agent_processes::register(cmd.process_owner(), Some(pid));
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| ProcessError::Io("no stdout".into()))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| ProcessError::Io("no stderr".into()))?;
    let mut capture = Capture::new(cmd, limit);
    let mut eof = [false; 2];
    if let Some(mut stdin) = child.stdin.take() {
        if let Some(text) = &cmd.inner().stdin {
            let stop = capture
                .binding
                .on_exit
                .as_ref()
                .and_then(|sink| sink.stop_notify());
            let cancelled = tokio::select! {
                biased;
                () = async {
                    if let Some(stop) = &stop { stop.notified().await; }
                    else { std::future::pending::<()>().await; }
                } => true,
                result = stdin.write_all(text.as_bytes()) => {
                    result.map_err(io_error)?;
                    false
                }
            };
            if cancelled {
                drop(stdin);
                return cancel_foreground(
                    child,
                    guard,
                    deadline,
                    stdout,
                    stderr,
                    capture,
                    eof,
                    agent_registration,
                    settlement,
                )
                .await;
            }
        }
    }
    let mut timed_out = false;
    if !explicit {
        let foreground_wait = tokio::time::sleep(cmd.inner().timeout.unwrap_or(DEFAULT_TIMEOUT));
        tokio::pin!(foreground_wait);
        let demand = capture.binding.on_demand.clone();
        let requested = async {
            match demand {
                Some(notify) => notify.notified().await,
                None => std::future::pending().await,
            }
        };
        tokio::pin!(requested);
        // A supervisor keeps this exact child/job owner alive while stopping.
        // Its per-command sink notification replaces the old reliance on
        // dropping StreamingProcessTreeGuard (the Windows platform callback
        // intentionally has no global process list to signal).
        let stop = capture
            .binding
            .on_exit
            .as_ref()
            .and_then(|sink| sink.stop_notify());
        let stopping = async {
            match stop {
                Some(notify) => notify.notified().await,
                None => std::future::pending().await,
            }
        };
        tokio::pin!(stopping);
        let mut out = [0; 8192];
        let mut err = [0; 8192];
        loop {
            tokio::select! {
                biased;
                () = &mut stopping => {
                    return cancel_foreground(child, guard, deadline, stdout, stderr, capture, eof, agent_registration, settlement).await;
                }
                () = &mut requested => break,
                () = &mut foreground_wait => { timed_out = true; break; }
                result = child.wait(), if eof.iter().all(|eof| *eof) => {
                    let status = result.map_err(io_error)?;
                    guard.0 = None;
                    capture.persist_supervised_inline().await?;
                    capture.flush().await?; capture.finalize_persisted().await?;
                    deadline.completed_foreground()?;
                    *settlement = Some(Ok(status.code()));
                    return Ok(ForegroundRunResult {output_file: capture.metadata(), outcome: ForegroundOutcome::Completed(ProcessOutput {stdout: capture.stdout, stderr: capture.stderr, exit_code: status.code().unwrap_or(-1), timed_out: false})});
                }
                result = stdout.read(&mut out), if !eof[0] => { let n = result.map_err(io_error)?; eof[0] = n == 0; capture.chunk(&out[..n], false, n == 0).await?; }
                result = stderr.read(&mut err), if !eof[1] => { let n = result.map_err(io_error)?; eof[1] = n == 0; capture.chunk(&err[..n], true, n == 0).await?; }
            }
        }
        // A starved deadline/request must not background an already exited PID.
        if let Some(status) = child.try_wait().map_err(io_error)? {
            guard.0 = None;
            let drained = if let Ok(result) = tokio::time::timeout(
                std::time::Duration::from_millis(50),
                drain(
                    &mut child,
                    &mut stdout,
                    &mut stderr,
                    &mut capture,
                    eof,
                    &mut deadline,
                    None,
                ),
            )
            .await
            {
                result?;
                true
            } else {
                false
            };
            if drained {
                capture.persist_supervised_inline().await?;
            }
            capture.flush().await?;
            capture.finalize_persisted().await?;
            deadline.completed_foreground()?;
            if drained {
                *settlement = Some(Ok(status.code()));
            }
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
    deadline.at = cmd
        .background_timeout()
        .map(|timeout| tokio::time::Instant::now() + timeout);
    tokio::spawn(async move {
        // Ownership crosses only after the complete prefix has been accepted.
        let mut guard = guard;
        let stop = capture
            .binding
            .on_exit
            .as_ref()
            .and_then(|sink| sink.stop_notify());
        let result = drain(
            &mut child,
            &mut stdout,
            &mut stderr,
            &mut capture,
            eof,
            &mut deadline,
            stop,
        )
        .await;
        let retry_unsettled = if result.is_err() {
            let tree = deadline.stop_supervised_tree(&mut child).await;
            #[cfg(windows)]
            let killed = if tree.is_ok() {
                Ok(())
            } else {
                start_child_kill(&mut child)
            };
            #[cfg(not(windows))]
            let killed = start_child_kill(&mut child);
            killed.is_err() && (cfg!(not(windows)) || tree.is_err())
        } else {
            false
        };
        let waited = if retry_unsettled {
            Err(result.as_ref().err().expect("failed drain").clone())
        } else {
            child.wait().await.map_err(io_error)
        };
        if let Err(wait_error) = &waited {
            let error = result.as_ref().err().unwrap_or(wait_error).clone();
            let sink = capture.binding.on_exit.clone();
            let task_id = capture.binding.task_id.clone();
            let (reported, ready) = tokio::sync::oneshot::channel();
            guard.0 = None;
            retain_failed_child(
                child,
                deadline,
                agent_registration,
                sink.clone().map(|sink| (sink, task_id.clone())),
                sink.as_ref().map(|_| RetainedOutput {
                    stdout,
                    stderr,
                    capture,
                    eof,
                    reported: Some(ready),
                }),
            )
            .await;
            if let Some(sink) = sink {
                sink.on_supervised_runner_settled(&task_id, Err(error))
                    .await;
                let _ = reported.send(());
            }
            return;
        }
        guard.0 = None;
        drop(agent_registration);
        let flushed = capture.flush().await;
        let finalized = capture.finalize_persisted().await;
        let settled = match (result, waited, flushed, finalized) {
            (Err(error), _, _, _)
            | (_, Err(error), _, _)
            | (_, _, Err(error), _)
            | (_, _, _, Err(error)) => Err(error),
            (Ok(_), Ok(status), Ok(()), Ok(())) => Ok(status.code()),
        };
        if let Some(sink) = capture.binding.on_exit.as_ref() {
            let code = settled.as_ref().ok().copied().flatten();
            sink.on_supervised_runner_settled(&capture.binding.task_id, settled)
                .await;
            if deadline.expired {
                sink.on_background_deadline_exit(&capture.binding.task_id, code)
                    .await;
            } else {
                sink.on_exit(&capture.binding.task_id, code).await;
            }
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
    use lingxi_core::host::{BackgroundExitSink, ProcessRunner};
    use std::sync::{Arc, Mutex};
    #[derive(Default)]
    struct Sink {
        reject_output: std::sync::atomic::AtomicBool,
        reject_spawn: std::sync::atomic::AtomicBool,
        pause_output: std::sync::atomic::AtomicBool,
        output_started: tokio::sync::Notify,
        output_resume: tokio::sync::Notify,
        deadlines: std::sync::atomic::AtomicUsize,
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
            let text = if !text.is_empty()
                && self
                    .pause_output
                    .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                // Simulate a writer that has accepted a prefix before waiting
                // for its next lock/write. Cancellation here would lose the
                // rest, and restarting the append could duplicate the prefix.
                let first = text.chars().next().unwrap().len_utf8();
                self.content.lock().unwrap().push_str(&text[..first]);
                self.output_started.notify_one();
                self.output_resume.notified().await;
                &text[first..]
            } else {
                text
            };
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
        async fn on_background_deadline_exit(&self, id: &str, code: Option<i32>) {
            self.deadlines
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.on_exit(id, code).await;
        }
        async fn on_exit(&self, id: &str, code: Option<i32>) {
            self.exits.lock().unwrap().push((id.into(), code));
        }
    }
    #[tokio::test]
    async fn windows_background_deadline_finishes_pending_output_publish_exactly_once() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(Sink::default());
        sink.pause_output
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let cmd = command(
            "printf '🦀payload'; exec sleep 60",
            5000,
            dir.path(),
            sink.clone(),
            Arc::new(tokio::sync::Notify::new()),
        )
        .with_background_timeout(Some(std::time::Duration::from_millis(100)));
        let process = WindowsProcess::new();
        let handle = process.spawn_background(&cmd).await.unwrap();
        let began = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            sink.output_started.notified(),
        )
        .await;
        if began.is_err() {
            let _ = process.kill(&handle).await;
        }
        began.expect("writer receives the captured bytes");
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let exited_while_pending = !sink.exits.lock().unwrap().is_empty();
        sink.output_resume.notify_one();
        finished(&sink).await;
        assert!(
            !exited_while_pending,
            "deadline cannot cancel an accepted output chunk"
        );
        assert_eq!(*sink.content.lock().unwrap(), "🦀payload");
        assert_eq!(sink.exits.lock().unwrap().len(), 1);
        assert_eq!(sink.deadlines.load(std::sync::atomic::Ordering::SeqCst), 1);
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
        assert!(lingxi_core::host::live_sessions::process_start_identity(pid).is_none());
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
            lingxi_core::host::sandbox::ProcessCommand {
                command: "/bin/sh".into(),
                args: vec!["-c".into(), script.into()],
                cwd: None,
                env: Default::default(),
                stdin: None,
                timeout: Some(std::time::Duration::from_millis(timeout)),
            },
            lingxi_core::host::sandbox::SandboxedTag::BypassAuditedWithReason {
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
        let cap = lingxi_core::host::task_output::MAX_PERSISTED_OUTPUT_BYTES as usize;
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
            lingxi_core::host::agent_processes::snapshot(&other_owner),
            vec![second_handle.pid]
        );
        finished(&first).await;
        finished(&second).await;
        assert!(lingxi_core::host::agent_processes::snapshot(&owner).is_empty());
        assert!(lingxi_core::host::agent_processes::snapshot(&other_owner).is_empty());
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

#[cfg(test)]
mod foreground_stop_tests {
    use super::*;
    use lingxi_core::host::{
        BackgroundExitSink, BackgroundTaskBinding, ProcessCommand, SandboxedTag,
    };
    use std::sync::Arc;
    #[derive(Default)]
    struct StopSink {
        stop: Arc<tokio::sync::Notify>,
        started: tokio::sync::Notify,
        pid: std::sync::atomic::AtomicU32,
        settlements: std::sync::Mutex<Vec<Result<Option<i32>, ProcessError>>>,
        settled: tokio::sync::Notify,
        cleanup_pending: std::sync::atomic::AtomicUsize,
        cleanup_finished: tokio::sync::Notify,
        exited: tokio::sync::Notify,
        exit_after_settlement: std::sync::atomic::AtomicBool,
    }
    #[async_trait::async_trait]
    impl BackgroundExitSink for StopSink {
        fn stop_notify(&self) -> Option<Arc<tokio::sync::Notify>> {
            Some(self.stop.clone())
        }
        async fn on_spawn(&self, _: &str, pid: u32) -> Result<(), ProcessError> {
            self.pid.store(pid, std::sync::atomic::Ordering::SeqCst);
            self.started.notify_one();
            Ok(())
        }
        async fn on_supervised_runner_settled(
            &self,
            _: &str,
            settlement: Result<Option<i32>, ProcessError>,
        ) {
            self.settlements.lock().unwrap().push(settlement);
            self.settled.notify_one();
        }
        async fn on_supervised_cleanup_pending(&self, _: &str) {
            self.cleanup_pending
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        async fn on_supervised_cleanup_finished(&self, _: &str) {
            self.cleanup_finished.notify_one();
        }
        async fn on_exit(&self, _: &str, _: Option<i32>) {
            self.exit_after_settlement.store(
                !self.settlements.lock().unwrap().is_empty(),
                std::sync::atomic::Ordering::SeqCst,
            );
            self.exited.notify_one();
        }
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn foreground_stop_joins_child_even_when_tree_termination_reports_failure() {
        let root = tempfile::tempdir().unwrap();
        let sink = Arc::new(StopSink::default());
        let cmd = SandboxedCommand::__new_sandboxed(
            ProcessCommand {
                command: "/bin/sh".into(),
                args: vec!["-c".into(), "exec sleep 30".into()],
                cwd: Some(root.path().to_path_buf()),
                env: Default::default(),
                timeout: Some(std::time::Duration::from_secs(30)),
                stdin: None,
            },
            SandboxedTag::BypassAuditedWithReason {
                reason: "isolated Windows driver cancellation fixture".into(),
            },
        )
        .with_background_task(BackgroundTaskBinding {
            task_id: "held-child".into(),
            output_path: root.path().join("output"),
            on_exit: Some(sink.clone()),
            on_demand: None,
        });
        let task = tokio::spawn(async move { run(&cmd, Some(100), false).await });
        sink.started.notified().await;
        let pid = sink.pid.load(std::sync::atomic::Ordering::SeqCst);
        sink.stop.notify_one();
        let result = tokio::time::timeout(std::time::Duration::from_secs(3), task)
            .await
            .unwrap()
            .unwrap();
        assert!(
            result.is_err(),
            "Unix has no taskkill; a tree termination failure must remain failure"
        );
        assert!(matches!(
            sink.settlements.lock().unwrap().as_slice(),
            [Err(_)]
        ));
        // The command is a direct exec with no descendants. Probe PID liveness
        // only after the driver's held Child has been explicitly waited/reaped.
        assert!(!std::process::Command::new("/bin/kill")
            .args(["-0", &pid.to_string()])
            .status()
            .unwrap()
            .success());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn foreground_stop_tree_failure_does_not_wait_for_descendant_held_pipes() {
        struct DescendantRelease(std::path::PathBuf);
        impl Drop for DescendantRelease {
            fn drop(&mut self) {
                // Let the fixture exit itself; a recorded PID is not a safe
                // cleanup capability after a test has been delayed.
                let _ = std::fs::write(&self.0, "release");
                let root = self.0.parent().unwrap();
                if root.join("descendant.pid").exists() {
                    for _ in 0..200 {
                        if root.join("descendant.done").exists() {
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                }
            }
        }
        let root = tempfile::tempdir().unwrap();
        let _cleanup = DescendantRelease(root.path().join("descendant.release"));
        let sink = Arc::new(StopSink::default());
        let cmd = SandboxedCommand::__new_sandboxed(
            ProcessCommand {
                command: "/bin/sh".into(),
                args: vec![
                    "-c".into(),
                    "(while [ -d \"$HARNESS_TEST_FIXTURE_DIR\" ] && [ ! -e \"$HARNESS_TEST_FIXTURE_DIR/descendant.release\" ]; do sleep 0.05; done; : > \"$HARNESS_TEST_FIXTURE_DIR/descendant.done\" 2>/dev/null) & printf '%s' \"$!\" > descendant.pid; wait".into(),
                ],
                cwd: Some(root.path().to_path_buf()),
                env: [("HARNESS_TEST_FIXTURE_DIR".into(), root.path().display().to_string())].into(),
                timeout: Some(std::time::Duration::from_secs(30)),
                stdin: None,
            },
            SandboxedTag::BypassAuditedWithReason {
                reason: "isolated inherited-pipe cancellation fixture".into(),
            },
        )
        .with_background_task(BackgroundTaskBinding {
            task_id: "held-pipes".into(),
            output_path: root.path().join("output"),
            on_exit: Some(sink.clone()),
            on_demand: None,
        });
        let task = tokio::spawn(async move { run(&cmd, Some(100), false).await });
        sink.started.notified().await;
        let child = sink.pid.load(std::sync::atomic::Ordering::SeqCst);
        let descendant: u32 = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if let Ok(text) = std::fs::read_to_string(root.path().join("descendant.pid")) {
                    if let Ok(pid) = text.parse() {
                        break pid;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        sink.stop.notify_one();
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), task)
            .await
            .expect("a tree signal failure cannot await still-open descendant pipes")
            .unwrap();
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains("taskkill"),
            "native tree failure is retained: {error}"
        );
        assert!(matches!(
            sink.settlements.lock().unwrap().as_slice(),
            [Err(_)]
        ));
        assert!(
            !std::process::Command::new("/bin/kill")
                .args(["-0", &child.to_string()])
                .status()
                .unwrap()
                .success(),
            "held direct child was reaped before failure returned"
        );
        assert!(
            std::process::Command::new("/bin/kill")
                .args(["-0", &descendant.to_string()])
                .status()
                .unwrap()
                .success(),
            "descendant still owns the output pipe at the failure return"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn supervised_settlement_covers_foreground_and_background_stop() {
        const CHILD: &str = "HARNESS_TEST_WINDOWS_SETTLEMENT_CHILD";
        const ROOT: &str = "HARNESS_TEST_WINDOWS_SETTLEMENT_ROOT";
        if std::env::var_os(CHILD).is_none() {
            // taskkill is Windows-only. This isolated no-descendant fixture
            // admits a successful tree operation while Child::kill/wait is real.
            // A subprocess keeps PATH changes out of concurrent tests.
            use std::os::unix::fs::PermissionsExt;
            let root = tempfile::tempdir().unwrap();
            let taskkill = root.path().join("taskkill");
            std::fs::write(&taskkill, "#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(&taskkill, std::fs::Permissions::from_mode(0o700)).unwrap();
            let mut paths = vec![root.path().to_path_buf()];
            paths.extend(std::env::split_paths(
                &std::env::var_os("PATH").unwrap_or_default(),
            ));
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "process::background::foreground_stop_tests::supervised_settlement_covers_foreground_and_background_stop",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .env(ROOT, root.path())
                .env("PATH", std::env::join_paths(paths).unwrap())
                .output()
                .unwrap();
            assert!(
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout).contains("running 1 test"),
                "isolated settlement test failed: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        let root = std::path::PathBuf::from(std::env::var_os(ROOT).unwrap());
        let make_command = |script: &str, sink: Arc<StopSink>, cwd: &std::path::Path| {
            SandboxedCommand::__new_sandboxed(
                ProcessCommand {
                    command: "/bin/sh".into(),
                    args: vec!["-c".into(), script.into()],
                    cwd: Some(cwd.to_path_buf()),
                    env: Default::default(),
                    timeout: Some(std::time::Duration::from_secs(30)),
                    stdin: None,
                },
                SandboxedTag::BypassAuditedWithReason {
                    reason: "isolated supervised settlement test".into(),
                },
            )
            .with_background_task(BackgroundTaskBinding {
                task_id: "settlement-test".into(),
                output_path: cwd.join("output"),
                on_exit: Some(sink),
                on_demand: None,
            })
        };

        for (name, background, limit) in [
            ("foreground-inline", false, None),
            ("foreground-spilled", false, Some(2)),
            ("background", true, Some(2)),
        ] {
            let directory = root.join(name);
            std::fs::create_dir(&directory).unwrap();
            let sink = Arc::new(StopSink::default());
            let command = make_command(
                "printf stdout-marker; printf stderr-marker >&2; : > ready; exec /bin/sleep 30",
                sink.clone(),
                &directory,
            );
            let mut task = tokio::spawn(async move { run(&command, limit, background).await });
            tokio::time::timeout(std::time::Duration::from_secs(5), sink.started.notified())
                .await
                .unwrap();
            if background {
                assert!(matches!(
                    tokio::time::timeout(std::time::Duration::from_secs(5), &mut task)
                        .await
                        .unwrap()
                        .unwrap()
                        .unwrap()
                        .outcome,
                    ForegroundOutcome::MovedToBackground(_)
                ));
            }
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while !directory.join("ready").exists() {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap();
            let pid = sink.pid.load(std::sync::atomic::Ordering::SeqCst);
            sink.stop.notify_one();
            if background {
                tokio::time::timeout(std::time::Duration::from_secs(5), sink.exited.notified())
                    .await
                    .expect("background stop must settle");
                assert!(sink
                    .exit_after_settlement
                    .load(std::sync::atomic::Ordering::SeqCst));
            } else {
                let result = tokio::time::timeout(std::time::Duration::from_secs(5), &mut task)
                    .await
                    .unwrap()
                    .unwrap();
                assert!(
                    matches!(result, Err(ProcessError::Io(message)) if message == "supervisor command cancelled")
                );
            }
            assert!(matches!(
                sink.settlements.lock().unwrap().as_slice(),
                [Ok(None)]
            ));
            assert!(!std::process::Command::new("/bin/kill")
                .args(["-0", &pid.to_string()])
                .status()
                .unwrap()
                .success());
            let output = std::fs::read_to_string(directory.join("output")).unwrap();
            assert!(output.contains("stdout-marker"));
            assert!(output.contains("stderr-marker"));
        }

        for background in [false, true] {
            let directory = root.join(if background { "stdin-bg" } else { "stdin-fg" });
            std::fs::create_dir(&directory).unwrap();
            let sink = Arc::new(StopSink::default());
            let command = make_command(
                "printf before-stdin; : > ready; exec /bin/sleep 30",
                sink.clone(),
                &directory,
            );
            let command = SandboxedCommand::__new_sandboxed(
                ProcessCommand {
                    stdin: Some("x".repeat(8 * 1024 * 1024)),
                    ..command.inner().clone()
                },
                command.tag().clone(),
            )
            .with_background_task(command.background_task().unwrap().clone());
            let task = tokio::spawn(async move { run(&command, None, background).await });
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while !directory.join("ready").exists() {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap();
            assert!(
                !task.is_finished(),
                "unread large stdin must remain blocked"
            );
            let pid = sink.pid.load(std::sync::atomic::Ordering::SeqCst);
            sink.stop.notify_one();
            let result = tokio::time::timeout(std::time::Duration::from_secs(5), task)
                .await
                .expect("stop must interrupt a blocked stdin write")
                .unwrap();
            assert!(
                matches!(result, Err(ProcessError::Io(message)) if message == "supervisor command cancelled")
            );
            assert!(matches!(
                sink.settlements.lock().unwrap().as_slice(),
                [Ok(None)]
            ));
            assert!(!std::process::Command::new("/bin/kill")
                .args(["-0", &pid.to_string()])
                .status()
                .unwrap()
                .success());
            assert!(std::fs::read_to_string(directory.join("output"))
                .unwrap()
                .contains("before-stdin"));
        }

        // Normal completion reports the actual exit; the pre-existing timeout
        // presentation uses a synthetic 143 and must not become proof of reap.
        let normal = Arc::new(StopSink::default());
        let command = make_command("printf normal-output; exit 23", normal.clone(), &root);
        let completed = run(&command, None, false).await.unwrap();
        assert!(
            completed.output_file.is_none(),
            "ordinary inline metadata stays unchanged"
        );
        assert!(matches!(completed.outcome,
            ForegroundOutcome::Completed(output) if output.exit_code == 23 && output.stdout == "normal-output"));
        assert!(std::fs::read_to_string(root.join("output"))
            .unwrap()
            .contains("normal-output"));
        assert!(matches!(
            normal.settlements.lock().unwrap().as_slice(),
            [Ok(Some(23))]
        ));
        let timeout = Arc::new(StopSink::default());
        let mut command = make_command("exec /bin/sleep 30", timeout.clone(), &root);
        // Construct a short timeout without changing the existing result shape.
        command = SandboxedCommand::__new_sandboxed(
            ProcessCommand {
                timeout: Some(std::time::Duration::from_millis(50)),
                ..command.inner().clone()
            },
            command.tag().clone(),
        )
        .with_background_task(command.background_task().unwrap().clone())
        .with_auto_background_on_timeout(false);
        assert!(
            matches!(run(&command, Some(2), false).await.unwrap().outcome,
            ForegroundOutcome::Completed(output) if output.timed_out && output.exit_code == 143)
        );
        assert!(matches!(
            timeout.settlements.lock().unwrap().as_slice(),
            [Err(_)]
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_stop_retains_child_for_retry_without_blocking_error() {
        const CHILD: &str = "HARNESS_TEST_WINDOWS_RETRY_CHILD";
        const ROOT: &str = "HARNESS_TEST_WINDOWS_RETRY_ROOT";
        if std::env::var_os(CHILD).is_none() {
            use std::os::unix::fs::PermissionsExt;
            for mode in ["foreground", "background"] {
                let root = tempfile::tempdir().unwrap();
                let taskkill = root.path().join("taskkill");
                std::fs::write(&taskkill, "#!/bin/sh\necho test-tree-failure >&2\nexit 1\n")
                    .unwrap();
                std::fs::set_permissions(&taskkill, std::fs::Permissions::from_mode(0o700))
                    .unwrap();
                let mut paths = vec![root.path().to_path_buf()];
                paths.extend(std::env::split_paths(
                    &std::env::var_os("PATH").unwrap_or_default(),
                ));
                let output = std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "process::background::foreground_stop_tests::failed_stop_retains_child_for_retry_without_blocking_error",
                        "--nocapture",
                    ])
                    .env(CHILD, mode)
                    .env(ROOT, root.path())
                    .env("HARNESS_TEST_CHILD_KILL_FAILURES", "2")
                    .env("PATH", std::env::join_paths(paths).unwrap())
                    .output()
                    .unwrap();
                assert!(
                    output.status.success()
                        && String::from_utf8_lossy(&output.stdout).contains("running 1 test"),
                    "isolated {mode} cleanup test failed: {}\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            return;
        }
        let root = std::path::PathBuf::from(std::env::var_os(ROOT).unwrap());
        let background = std::env::var(CHILD).unwrap() == "background";
        let sink = Arc::new(StopSink::default());
        let command = SandboxedCommand::__new_sandboxed(
            ProcessCommand {
                command: "/bin/sh".into(),
                args: vec![
                    "-c".into(),
                    "printf started; printf stderr-marker >&2; : > ready; exec /bin/sleep 30"
                        .into(),
                ],
                cwd: Some(root.clone()),
                env: Default::default(),
                timeout: Some(std::time::Duration::from_secs(30)),
                stdin: None,
            },
            SandboxedTag::BypassAuditedWithReason {
                reason: "isolated stop failure retry test".into(),
            },
        )
        .with_background_task(BackgroundTaskBinding {
            task_id: "retry-stop".into(),
            output_path: root.join("output"),
            on_exit: Some(sink.clone()),
            on_demand: None,
        });
        let mut task = tokio::spawn(async move { run(&command, None, background).await });
        tokio::time::timeout(std::time::Duration::from_secs(5), sink.started.notified())
            .await
            .unwrap();
        if background {
            assert!(matches!(
                tokio::time::timeout(std::time::Duration::from_secs(5), &mut task)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap()
                    .outcome,
                ForegroundOutcome::MovedToBackground(_)
            ));
        }
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !root.join("ready").exists() {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let pid = sink.pid.load(std::sync::atomic::Ordering::SeqCst);
        sink.stop.notify_one();
        if background {
            tokio::time::timeout(std::time::Duration::from_secs(3), sink.settled.notified())
                .await
                .expect("background failure must not wait for a live child");
        } else {
            let result = tokio::time::timeout(std::time::Duration::from_secs(3), &mut task)
                .await
                .expect("foreground failure must not wait for a live child")
                .unwrap();
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("test-tree-failure"));
        }
        assert!(matches!(
            sink.settlements.lock().unwrap().as_slice(),
            [Err(_)]
        ));
        if background {
            tokio::time::timeout(std::time::Duration::from_secs(6), sink.exited.notified())
                .await
                .expect("retained background child must eventually be reaped and flushed");
            assert!(sink
                .exit_after_settlement
                .load(std::sync::atomic::Ordering::SeqCst));
        }
        assert_eq!(
            sink.cleanup_pending
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        tokio::time::timeout(
            std::time::Duration::from_secs(6),
            sink.cleanup_finished.notified(),
        )
        .await
        .expect("retained cleanup must finish only after a real wait");
        let output = std::fs::read_to_string(root.join("output")).unwrap();
        assert!(output.contains("started"));
        assert!(output.contains("stderr-marker"));
        tokio::time::timeout(std::time::Duration::from_secs(6), async {
            loop {
                let alive = std::process::Command::new("/bin/kill")
                    .args(["-0", &pid.to_string()])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .unwrap()
                    .success();
                if !alive {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("retained child must eventually be killed and reaped");
    }

    #[derive(Default)]
    struct RejectSpawnSink {
        settlements: std::sync::Mutex<Vec<Result<Option<i32>, ProcessError>>>,
        cleanup_pending: std::sync::atomic::AtomicUsize,
        cleanup_finished: tokio::sync::Notify,
    }

    #[async_trait::async_trait]
    impl BackgroundExitSink for RejectSpawnSink {
        async fn on_spawn(&self, _: &str, _: u32) -> Result<(), ProcessError> {
            Err(ProcessError::Io(
                "shell capability cancelled before acceptance".into(),
            ))
        }

        async fn on_supervised_runner_settled(
            &self,
            _: &str,
            settlement: Result<Option<i32>, ProcessError>,
        ) {
            self.settlements.lock().unwrap().push(settlement);
        }

        async fn on_supervised_cleanup_pending(&self, _: &str) {
            self.cleanup_pending
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }

        async fn on_supervised_cleanup_finished(&self, _: &str) {
            self.cleanup_finished.notify_one();
        }

        async fn on_exit(&self, _: &str, _: Option<i32>) {}
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejected_spawn_signal_failure_is_not_cancelled_settlement() {
        const CHILD: &str = "HARNESS_TEST_REJECTED_WINDOWS_SPAWN_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "process::background::foreground_stop_tests::rejected_spawn_signal_failure_is_not_cancelled_settlement",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .env("HARNESS_TEST_CHILD_KILL_FAILURES", "2")
                .output()
                .unwrap();
            assert!(
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout).contains("running 1 test"),
                "isolated rejected-spawn test failed: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let sink = Arc::new(RejectSpawnSink::default());
        let command = SandboxedCommand::__new_sandboxed(
            ProcessCommand {
                command: "/bin/sh".into(),
                args: vec!["-c".into(), "exec /bin/sleep 30".into()],
                cwd: Some(root.path().to_path_buf()),
                env: Default::default(),
                timeout: Some(std::time::Duration::from_secs(30)),
                stdin: None,
            },
            SandboxedTag::BypassAuditedWithReason {
                reason: "rejected spawn cleanup failure".into(),
            },
        )
        .with_background_task(BackgroundTaskBinding {
            task_id: "rejected-spawn".into(),
            output_path: root.path().join("output"),
            on_exit: Some(sink.clone()),
            on_demand: None,
        });
        let result = run(&command, None, false).await.unwrap_err();
        assert!(result.to_string().contains("injected child signal failure"));
        assert!(matches!(
            sink.settlements.lock().unwrap().as_slice(),
            [Err(ProcessError::Io(message))] if message.contains("injected child signal failure")
        ));
        assert_eq!(
            sink.cleanup_pending
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        tokio::time::timeout(
            std::time::Duration::from_secs(6),
            sink.cleanup_finished.notified(),
        )
        .await
        .expect("failed rejected-spawn stop retains the child until waited");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn rejected_suspended_spawn_requires_a_waited_child() {
        let root = tempfile::tempdir().unwrap();
        let sink = Arc::new(RejectSpawnSink::default());
        let command = SandboxedCommand::__new_sandboxed(
            ProcessCommand {
                command: "cmd.exe".into(),
                args: vec!["/D".into(), "/C".into(), "echo ran > payload-ran".into()],
                cwd: Some(root.path().to_path_buf()),
                env: Default::default(),
                timeout: Some(std::time::Duration::from_secs(30)),
                stdin: None,
            },
            SandboxedTag::BypassAuditedWithReason {
                reason: "cancel rejected suspended spawn".into(),
            },
        )
        .with_background_task(BackgroundTaskBinding {
            task_id: "rejected-spawn".into(),
            output_path: root.path().join("output"),
            on_exit: Some(sink.clone()),
            on_demand: None,
        });
        assert!(matches!(
            run(&command, None, false).await,
            Err(ProcessError::Io(message)) if message == "shell capability cancelled before acceptance"
        ));
        assert!(matches!(
            sink.settlements.lock().unwrap().as_slice(),
            [Ok(_)]
        ));
        assert!(!root.path().join("payload-ran").exists());
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn native_job_stop_reports_waited_status_before_exit() {
        for (background, blocked_stdin) in
            [(false, false), (true, false), (false, true), (true, true)]
        {
            let directory = tempfile::tempdir().unwrap();
            std::fs::write(
                directory.path().join("fixture.cmd"),
                "@echo off\r\necho stdout-marker\r\necho stderr-marker 1>&2\r\necho ready > ready\r\n:wait\r\ngoto wait\r\n",
            )
            .unwrap();
            let sink = Arc::new(StopSink::default());
            let command = SandboxedCommand::__new_sandboxed(
                ProcessCommand {
                    command: "cmd.exe".into(),
                    args: vec!["/D".into(), "/C".into(), "fixture.cmd".into()],
                    cwd: Some(directory.path().to_path_buf()),
                    env: Default::default(),
                    timeout: Some(std::time::Duration::from_secs(30)),
                    stdin: blocked_stdin.then(|| "x".repeat(8 * 1024 * 1024)),
                },
                SandboxedTag::BypassAuditedWithReason {
                    reason: "native supervised job stop test".into(),
                },
            )
            .with_background_task(BackgroundTaskBinding {
                task_id: "native-stop".into(),
                output_path: directory.path().join("output"),
                on_exit: Some(sink.clone()),
                on_demand: None,
            });
            let mut task = tokio::spawn(async move { run(&command, None, background).await });
            tokio::time::timeout(std::time::Duration::from_secs(10), sink.started.notified())
                .await
                .unwrap();
            if background && !blocked_stdin {
                assert!(matches!(
                    tokio::time::timeout(std::time::Duration::from_secs(10), &mut task)
                        .await
                        .unwrap()
                        .unwrap()
                        .unwrap()
                        .outcome,
                    ForegroundOutcome::MovedToBackground(_)
                ));
            }
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                while !directory.path().join("ready").exists() {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap();
            if blocked_stdin {
                assert!(
                    !task.is_finished(),
                    "unread large stdin must remain blocked"
                );
            }
            let pid = sink.pid.load(std::sync::atomic::Ordering::SeqCst);
            let birth = lingxi_core::host::live_sessions::process_start_identity(pid)
                .expect("native child has a birth identity");
            sink.stop.notify_one();
            if background && !blocked_stdin {
                tokio::time::timeout(std::time::Duration::from_secs(10), sink.exited.notified())
                    .await
                    .expect("background job stop must settle");
                assert!(sink
                    .exit_after_settlement
                    .load(std::sync::atomic::Ordering::SeqCst));
            } else {
                let result = tokio::time::timeout(std::time::Duration::from_secs(10), &mut task)
                    .await
                    .unwrap()
                    .unwrap();
                assert!(
                    matches!(result, Err(ProcessError::Io(message)) if message == "supervisor command cancelled")
                );
            }
            assert!(matches!(
                sink.settlements.lock().unwrap().as_slice(),
                [Ok(Some(143))]
            ));
            assert_ne!(
                lingxi_core::host::live_sessions::process_start_identity(pid).as_deref(),
                Some(birth.as_str())
            );
            let output = std::fs::read_to_string(directory.path().join("output")).unwrap();
            assert!(output.contains("stdout-marker"));
            assert!(output.contains("stderr-marker"));
        }
    }
}
