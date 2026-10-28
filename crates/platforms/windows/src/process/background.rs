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

async fn flush_capture(capture: &mut Capture, strict_host: bool) -> Result<(), ProcessError> {
    if strict_host {
        tokio::time::timeout(std::time::Duration::from_secs(5), capture.flush())
            .await
            .unwrap_or_else(|_| Err(ProcessError::Io("background output flush timed out".into())))
    } else {
        capture.flush().await
    }
}

async fn finalize_capture(capture: &mut Capture, strict_host: bool) -> Result<(), ProcessError> {
    if strict_host {
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            capture.finalize_persisted(),
        )
        .await
        .unwrap_or_else(|_| Err(ProcessError::Io("output finalization timed out".into())))
    } else {
        capture.finalize_persisted().await
    }
}

fn io_error(error: std::io::Error) -> ProcessError {
    ProcessError::Io(error.to_string())
}

#[cfg(all(test, windows))]
tokio::task_local! {
    static TEST_JOB_VERIFICATION_BLOCKED: std::sync::Arc<std::sync::atomic::AtomicBool>;
}

#[derive(Default)]
struct DrainDeadline {
    at: Option<tokio::time::Instant>,
    expired: bool,
    requested_stop: bool,
    strict_host: bool,
    terminated: bool,
    verify_lease: bool,
    verify_supervised_stop: bool,
    #[cfg(windows)]
    job: Option<super::supervisor_gate::CommandJob>,
    #[cfg(all(test, windows))]
    test_job_verification_blocked: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

async fn verify_stopped_job(deadline: &DrainDeadline) -> Result<(), ProcessError> {
    // Even legacy/non-strict callers may later publish a cleanup receipt.
    // No caller may turn an unchecked group into a successful proof.
    #[cfg(all(test, windows))]
    if deadline
        .test_job_verification_blocked
        .as_ref()
        .is_some_and(|blocked| blocked.load(std::sync::atomic::Ordering::SeqCst))
    {
        return Err(ProcessError::Io(
            "injected payload job verification failure".into(),
        ));
    }
    #[cfg(windows)]
    {
        match deadline.job.as_ref() {
            Some(job) => job.verify_complete_descendant_termination(),
            None => Err(ProcessError::Io(
                "host shell has no held payload job".into(),
            )),
        }
    }
    #[cfg(not(windows))]
    {
        Err(ProcessError::Unsupported)
    }
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
        #[cfg(windows)]
        if self.strict_host {
            return Err(ProcessError::Io(
                "host shell has no held payload job".into(),
            ));
        }
        if let Some(pid) = child.id() {
            let tree = super::kill_tree::kill_tree_windows(pid).await;
            if self.strict_host {
                let direct = child.start_kill().map_err(io_error);
                match (tree, direct) {
                    (Err(tree), Err(direct)) => {
                        return Err(ProcessError::Io(format!(
                            "tree termination failed: {tree}; child termination failed: {direct}"
                        )));
                    }
                    (Err(error), _) | (_, Err(error)) => return Err(error),
                    (Ok(()), Ok(())) => {}
                }
            } else {
                let _ = tree;
                child.kill().await.map_err(io_error)?;
            }
        }
        Ok(())
    }
}

/// Keep the held child until both termination and wait have settled. A failed
/// tree signal remains an error even if the direct child was subsequently
/// reaped; that does not prove its descendants stopped.
async fn terminate_and_reap(
    deadline: &DrainDeadline,
    child: &mut tokio::process::Child,
    guard: &mut StreamingProcessTreeGuard,
) -> Result<std::process::ExitStatus, ProcessError> {
    let limit = std::time::Duration::from_secs(5);
    let termination = if deadline.terminated {
        Ok(())
    } else {
        tokio::time::timeout(limit, deadline.terminate(child))
            .await
            .unwrap_or_else(|_| Err(ProcessError::Io("process termination timed out".into())))
    };
    if termination.is_err() {
        // Even when tree termination failed, try the held direct child before
        // waiting; preserve the tree error because descendants are unverified.
        let _ = child.start_kill();
    }
    let waited = tokio::time::timeout(limit, child.wait())
        .await
        .map_err(|_| ProcessError::Io("child wait timed out".into()))
        .and_then(|result| result.map_err(io_error));
    if waited.is_ok() {
        guard.0 = None;
    }
    #[cfg(windows)]
    if deadline.job.is_some() {
        // The held job remains authoritative even if waiting failed. Do not
        // fall back to a naked PID that may have been reused; closing the job
        // still terminates its members, while the failure is reported below.
        guard.0 = None;
    }
    let verified = if waited.is_ok() {
        verify_stopped_job(deadline).await
    } else {
        Ok(())
    };
    let mut failures = Vec::new();
    if let Err(error) = termination {
        failures.push(format!("termination failed: {error}"));
    }
    let status = match waited {
        Ok(status) => Some(status),
        Err(error) => {
            failures.push(format!("child wait failed: {error}"));
            None
        }
    };
    if let Err(error) = verified {
        failures.push(format!("payload job verification failed: {error}"));
    }
    if failures.is_empty() {
        Ok(status.expect("a successful cleanup has a waited child"))
    } else {
        Err(ProcessError::Io(failures.join("; ")))
    }
}

fn retain_until_reaped(
    mut child: tokio::process::Child,
    mut guard: StreamingProcessTreeGuard,
    deadline: DrainDeadline,
    lifetime_guard: Option<lingxi_core::host::BackendPlanHandle>,
    verified_sink: Option<(
        std::sync::Arc<dyn lingxi_core::host::BackgroundExitSink>,
        String,
    )>,
) {
    tokio::spawn(async move {
        #[cfg(windows)]
        if deadline.job.is_some() {
            // There is currently no way to upgrade an arbitrary resumed Job
            // to a complete held-descendant proof. Keep the exact owner and
            // lease, and keep later explicit stop requests actionable. The
            // original Job remains held even after a termination attempt.
            if let Some(stop) = verified_sink
                .as_ref()
                .and_then(|(sink, _)| sink.stop_notify())
            {
                loop {
                    stop.notified().await;
                    let _ = terminate_and_reap(&deadline, &mut child, &mut guard).await;
                }
            }
            // Runtime/process loss remains a separate durability boundary.
            std::future::pending::<()>().await;
        }
        loop {
            if terminate_and_reap(&deadline, &mut child, &mut guard)
                .await
                .is_ok()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        drop(guard);
        drop(child);
        #[cfg(windows)]
        drop(deadline);
        drop(lifetime_guard);
        if let Some((sink, task_id)) = verified_sink {
            sink.on_cleanup_verified(&task_id).await;
        }
    });
}

async fn foreground_failure(
    cmd: &SandboxedCommand,
    cause: ProcessError,
    cleanup: Result<std::process::ExitStatus, ProcessError>,
    child: tokio::process::Child,
    guard: StreamingProcessTreeGuard,
    mut deadline: DrainDeadline,
) -> ProcessError {
    match cleanup {
        Ok(_) => {
            // Output/I/O failures remain errors even if a second cleanup
            // attempt succeeds. Tell the supervisor that the error is still
            // real but the held process tree is now verified empty.
            if (deadline.strict_host || deadline.verify_lease)
                && !matches!(&cause, ProcessError::Io(message) if message == "supervisor command cancelled")
            {
                if let Some((binding, sink)) = cmd
                    .background_task()
                    .and_then(|binding| binding.on_exit.as_ref().map(|sink| (binding, sink)))
                {
                    sink.on_wait_failure(&binding.task_id, &cause.to_string())
                        .await;
                    sink.on_cleanup_verified(&binding.task_id).await;
                }
            }
            cause
        }
        Err(cleanup) => {
            let error = ProcessError::Io(format!("{cause}; process cleanup failed: {cleanup}"));
            if deadline.strict_host || deadline.verify_lease {
                let verified_sink = cmd.background_task().and_then(|binding| {
                    binding
                        .on_exit
                        .as_ref()
                        .map(|sink| (sink.clone(), binding.task_id.clone()))
                });
                if let Some((sink, task_id)) = &verified_sink {
                    sink.on_wait_failure(task_id, &error.to_string()).await;
                }
                let lifetime_guard = cmd.lifetime_guard().cloned();
                deadline.verify_lease |= lifetime_guard.is_some();
                retain_until_reaped(child, guard, deadline, lifetime_guard, verified_sink);
            }
            error
        }
    }
}

// A process created suspended has not run user code and cannot yet have
// descendants. A successful wait is authoritative even if its earlier kill
// raced with exit; an unsuccessful wait must never be reported as verified.
#[cfg(any(windows, test))]
fn suspended_cleanup_result(
    termination: Result<(), ProcessError>,
    waited: Result<(), ProcessError>,
) -> Result<(), ProcessError> {
    match (termination, waited) {
        (_, Ok(())) => Ok(()),
        (Ok(()), Err(wait)) => Err(ProcessError::Io(format!(
            "suspended child wait unverified: {wait}"
        ))),
        (Err(termination), Err(wait)) => Err(ProcessError::Io(format!(
            "suspended child termination failed: {termination}; wait unverified: {wait}"
        ))),
    }
}

#[cfg(windows)]
async fn terminate_suspended_and_verify(
    child: &mut tokio::process::Child,
    guard: &mut StreamingProcessTreeGuard,
    _deadline: &DrainDeadline,
) -> Result<(), ProcessError> {
    let limit = std::time::Duration::from_secs(5);
    let termination = tokio::time::timeout(limit, child.kill())
        .await
        .map_err(|_| ProcessError::Io("suspended child termination timed out".into()))
        .and_then(|result| result.map_err(io_error));
    let waited = tokio::time::timeout(limit, child.wait())
        .await
        .map_err(|_| ProcessError::Io("suspended child wait timed out".into()))
        .and_then(|result| result.map(|_| ()).map_err(io_error));
    if waited.is_ok() {
        guard.0 = None;
    }
    suspended_cleanup_result(termination, waited)?;
    // No ResumeThread call has been attempted on this path. The wait above
    // proves the sole suspended payload exited; Job accounting is unnecessary.
    Ok(())
}

#[cfg(windows)]
fn retain_suspended_until_reaped(
    mut child: tokio::process::Child,
    mut guard: StreamingProcessTreeGuard,
    deadline: DrainDeadline,
    lifetime_guard: Option<lingxi_core::host::BackendPlanHandle>,
    verified_sink: Option<(
        std::sync::Arc<dyn lingxi_core::host::BackgroundExitSink>,
        String,
    )>,
) {
    tokio::spawn(async move {
        while terminate_suspended_and_verify(&mut child, &mut guard, &deadline)
            .await
            .is_err()
        {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        drop(guard);
        drop(child);
        drop(deadline);
        drop(lifetime_guard);
        if let Some((sink, task_id)) = verified_sink {
            sink.on_cleanup_verified(&task_id).await;
        }
    });
}

fn background_completion(
    status: Result<std::process::ExitStatus, String>,
    flushed: Result<(), ProcessError>,
) -> Result<std::process::ExitStatus, String> {
    match (status, flushed) {
        (Ok(status), Ok(())) => Ok(status),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(format!("background output flush failed: {error}")),
        (Err(wait), Err(flush)) => Err(format!("{wait}; background output flush failed: {flush}")),
    }
}

async fn report_background_completion(
    sink: &dyn lingxi_core::host::BackgroundExitSink,
    task_id: &str,
    completion: Result<std::process::ExitStatus, String>,
    expired: bool,
    requested_stop: bool,
    group_cleanup_verified: bool,
) {
    match completion {
        Ok(status) if expired && !sink.requires_verified_supervised_stop() => {
            sink.on_background_deadline_exit(task_id, status.code())
                .await
        }
        // TerminateJobObject for a stop or deadline and an independently completed process can
        // produce the same exit code. A successful stop request and job wait
        // alone do not establish which caused this exit.
        Ok(status) if (expired || requested_stop) && sink.requires_verified_supervised_stop() => {
            sink.on_exit_with_proof(task_id, status.code(), None, group_cleanup_verified)
                .await
        }
        Ok(status) if expired || requested_stop => sink.on_exit(task_id, status.code()).await,
        Ok(status) => {
            sink.on_exit_with_proof(task_id, status.code(), Some(false), group_cleanup_verified)
                .await
        }
        Err(error) => sink.on_wait_failure(task_id, &error).await,
    }
}

#[derive(Default)]
struct DrainControl {
    deadline_handled: bool,
    stop_handled: bool,
    stop_drain_limit: Option<tokio::time::Instant>,
}

async fn stop_background(
    child: &mut tokio::process::Child,
    deadline: &mut DrainDeadline,
    control: &mut DrainControl,
    expired: bool,
) -> Result<(), ProcessError> {
    control.stop_handled = true;
    control.deadline_handled = true;
    deadline.requested_stop |= !expired;
    if deadline.verify_supervised_stop {
        // The Job has been held since suspended spawn. Keep it until every
        // member is verified gone before publishing an unknown outcome.
        deadline.verify_lease = true;
    }
    let result = if deadline.strict_host || deadline.verify_lease {
        tokio::time::timeout(std::time::Duration::from_secs(5), deadline.terminate(child))
            .await
            .unwrap_or_else(|_| Err(ProcessError::Io("process termination timed out".into())))
    } else {
        deadline.terminate(child).await
    };
    if result.is_ok() && expired {
        deadline.expired = true;
    }
    if result.is_ok() {
        deadline.terminated = true;
    }
    if deadline.strict_host || deadline.verify_lease {
        control.stop_drain_limit =
            Some(tokio::time::Instant::now() + std::time::Duration::from_secs(5));
    }
    result
}

async fn wait_for_stop(stop: &Option<std::sync::Arc<tokio::sync::Notify>>) {
    if let Some(stop) = stop {
        stop.notified().await;
    } else {
        std::future::pending::<()>().await;
    }
}

async fn wait_until(at: Option<tokio::time::Instant>) {
    if let Some(at) = at {
        tokio::time::sleep_until(at).await;
    } else {
        std::future::pending::<()>().await;
    }
}

/// Let an accepted output append finish without preventing a stop from reaching
/// the held job. A stuck append on a managed host is an explicit failure after
/// a bounded grace period, rather than an indefinitely unreported stop.
#[allow(clippy::too_many_arguments)]
async fn capture_chunk_with_stop(
    capture: &mut Capture,
    bytes: &[u8],
    stderr: bool,
    eof: bool,
    child: &mut tokio::process::Child,
    deadline: &mut DrainDeadline,
    control: &mut DrainControl,
    stop: &Option<std::sync::Arc<tokio::sync::Notify>>,
) -> Result<(), ProcessError> {
    output_with_stop(
        capture.chunk(bytes, stderr, eof),
        child,
        deadline,
        control,
        stop,
    )
    .await
}

async fn output_with_stop<F>(
    output: F,
    child: &mut tokio::process::Child,
    deadline: &mut DrainDeadline,
    control: &mut DrainControl,
    stop: &Option<std::sync::Arc<tokio::sync::Notify>>,
) -> Result<(), ProcessError>
where
    F: std::future::Future<Output = Result<(), ProcessError>>,
{
    tokio::pin!(output);
    let mut stop_error = None;
    loop {
        tokio::select! {
            biased;
            () = wait_for_stop(stop), if !control.stop_handled => {
                if let Err(error) = stop_background(child, deadline, control, false).await {
                    let _ = child.start_kill();
                    stop_error = Some(error);
                }
            }
            () = wait_until(deadline.at), if !control.deadline_handled => {
                if let Err(error) = stop_background(child, deadline, control, true).await {
                    let _ = child.start_kill();
                    stop_error = Some(error);
                }
            }
            () = wait_until(control.stop_drain_limit) => {
                return Err(ProcessError::Io(match stop_error {
                    Some(error) => format!("output append did not settle after stop; termination failed: {error}"),
                    None => "output append did not settle after stop".into(),
                }));
            }
            result = &mut output => {
                return match (result, stop_error) {
                    (Ok(()), None) => Ok(()),
                    (Err(error), None) | (Ok(()), Some(error)) => Err(error),
                    (Err(output), Some(stop)) => Err(ProcessError::Io(format!(
                        "output append failed: {output}; termination failed: {stop}"
                    ))),
                };
            }
        }
    }
}

async fn handoff_output_with_stop<F>(
    output: F,
    child: &mut tokio::process::Child,
    guard: &mut StreamingProcessTreeGuard,
    deadline: &mut DrainDeadline,
    control: &mut DrainControl,
    stop: &Option<std::sync::Arc<tokio::sync::Notify>>,
) -> Result<(), ProcessError>
where
    F: std::future::Future<Output = Result<(), ProcessError>>,
{
    let output_result = output_with_stop(output, child, deadline, control, stop).await;
    if output_result.is_ok() && !control.stop_handled {
        return Ok(());
    }
    let error = output_result
        .err()
        .unwrap_or_else(|| ProcessError::Io("supervisor command cancelled before handoff".into()));
    match terminate_and_reap(deadline, child, guard).await {
        Ok(_) => Err(error),
        Err(cleanup) => Err(ProcessError::Io(format!(
            "handoff output failed: {error}; cleanup failed: {cleanup}"
        ))),
    }
}

async fn finish_foreground(
    capture: &mut Capture,
    child: &mut tokio::process::Child,
    guard: &mut StreamingProcessTreeGuard,
    deadline: &mut DrainDeadline,
    stop: &Option<std::sync::Arc<tokio::sync::Notify>>,
) -> Result<(), ProcessError> {
    if !deadline.strict_host {
        flush_capture(capture, false).await?;
        finalize_capture(capture, false).await?;
        return deadline.completed_foreground();
    }

    // The child leader has exited, but its Job may still contain descendants.
    // Keep stop observable throughout flush and finalization before disarming.
    let mut control = DrainControl {
        deadline_handled: true,
        ..DrainControl::default()
    };
    let mut result = output_with_stop(
        flush_capture(capture, true),
        child,
        deadline,
        &mut control,
        stop,
    )
    .await;
    if result.is_ok() && !control.stop_handled {
        result = output_with_stop(
            finalize_capture(capture, true),
            child,
            deadline,
            &mut control,
            stop,
        )
        .await;
    }
    if result.is_ok() && !control.stop_handled {
        // Consume a pending notification before releasing job ownership.
        result = output_with_stop(async { Ok(()) }, child, deadline, &mut control, stop).await;
    }
    if result.is_err() || control.stop_handled {
        let cause = result
            .err()
            .unwrap_or_else(|| ProcessError::Io("supervisor command cancelled".into()));
        return Err(match terminate_and_reap(deadline, child, guard).await {
            Ok(_) => cause,
            Err(cleanup) => ProcessError::Io(format!(
                "foreground output failed: {cause}; cleanup failed: {cleanup}"
            )),
        });
    }
    #[cfg(windows)]
    {
        // EOF and the leader's exit are not a descendant-lifetime proof,
        // even when Job accounting has already reached zero. Attempt the
        // requested group termination and propagate its unverified result.
        terminate_and_reap(deadline, child, guard).await?;
    }
    deadline.completed_foreground()
}

async fn drain(
    child: &mut tokio::process::Child,
    stdout: &mut tokio::process::ChildStdout,
    stderr: &mut tokio::process::ChildStderr,
    capture: &mut Capture,
    mut eof: [bool; 2],
    deadline: &mut DrainDeadline,
    already_stopped: bool,
) -> Result<std::process::ExitStatus, ProcessError> {
    let mut out = [0; 8192];
    let mut err = [0; 8192];
    let stop = capture
        .binding
        .on_exit
        .as_ref()
        .and_then(|sink| sink.stop_notify());
    let mut control = DrainControl {
        deadline_handled: already_stopped,
        stop_handled: already_stopped,
        stop_drain_limit: (already_stopped && (deadline.strict_host || deadline.verify_lease))
            .then(|| tokio::time::Instant::now() + std::time::Duration::from_secs(5)),
    };
    loop {
        if eof.iter().all(|eof| *eof) {
            // Once the held child has already been reaped, a simultaneous
            // stop notification must not replace its observed completion.
            if let Some(status) = child.try_wait().map_err(io_error)? {
                if deadline.terminated {
                    verify_stopped_job(deadline).await?;
                }
                return Ok(status);
            }
        }
        tokio::select! {
            // Do not mistake an unverified stop for a completed child.
            biased;
            () = wait_for_stop(&stop), if !control.stop_handled => {
                stop_background(child, deadline, &mut control, false).await?;
            }
            () = wait_until(deadline.at), if !control.deadline_handled => {
                stop_background(child, deadline, &mut control, true).await?;
            }
            () = wait_until(control.stop_drain_limit) => {
                return Err(ProcessError::Io("stopped background output did not close".into()));
            }
            result = child.wait(), if eof.iter().all(|eof| *eof) => {
                let status = result.map_err(io_error)?;
                if deadline.terminated {
                    verify_stopped_job(deadline).await?;
                }
                return Ok(status);
            }
            result = stdout.read(&mut out), if !eof[0] => { let n = result.map_err(io_error)?; eof[0] = n == 0; capture_chunk_with_stop(capture, &out[..n], false, n == 0, child, deadline, &mut control, &stop).await?; }
            result = stderr.read(&mut err), if !eof[1] => { let n = result.map_err(io_error)?; eof[1] = n == 0; capture_chunk_with_stop(capture, &err[..n], true, n == 0, child, deadline, &mut control, &stop).await?; }
        }
    }
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
    command.creation_flags(0x0000_0004); // CREATE_SUSPENDED until payload job ownership is installed.
    let mut child = command.spawn().map_err(io_error)?;
    let pid = child
        .id()
        .ok_or_else(|| ProcessError::Io("spawned child has no pid".into()))?;
    let mut guard = StreamingProcessTreeGuard(Some(pid));
    let mut deadline = DrainDeadline {
        strict_host: cmd.requires_group_cleanup_on_exit(),
        verify_supervised_stop: cmd
            .background_task()
            .and_then(|binding| binding.on_exit.as_ref())
            .is_some_and(|sink| sink.requires_verified_supervised_stop()),
        #[cfg(all(test, windows))]
        test_job_verification_blocked: TEST_JOB_VERIFICATION_BLOCKED
            .try_with(std::sync::Arc::clone)
            .ok(),
        ..DrainDeadline::default()
    };
    // A strict foreground error must not drop the only owner of an unverified
    // Job. Retry with that same capability and notify the supervisor when the
    // previously unverified cleanup eventually succeeds.
    macro_rules! fail_foreground {
        ($cause:expr) => {{
            let cause = $cause;
            if deadline.strict_host || deadline.verify_supervised_stop {
                deadline.verify_lease |= deadline.verify_supervised_stop;
                let cleanup = terminate_and_reap(&deadline, &mut child, &mut guard).await;
                return Err(foreground_failure(cmd, cause, cleanup, child, guard, deadline).await);
            }
            return Err(cause);
        }};
    }
    #[cfg(windows)]
    let initial_thread = {
        let owned = super::supervisor_gate::CommandJob::assign_suspended(&child).and_then(|job| {
            deadline.job = Some(job);
            super::supervisor_gate::suspended_thread(&child)
        });
        match owned {
            Ok(thread) => thread,
            Err(error) => {
                match terminate_suspended_and_verify(&mut child, &mut guard, &deadline).await {
                    Ok(()) => return Err(error),
                    Err(cleanup) => {
                        let failure = ProcessError::Io(format!(
                            "suspended spawn failed: {error}; cleanup unverified: {cleanup}"
                        ));
                        let verified_sink = cmd.background_task().and_then(|binding| {
                            binding
                                .on_exit
                                .as_ref()
                                .map(|sink| (sink.clone(), binding.task_id.clone()))
                        });
                        if let Some((sink, task_id)) = &verified_sink {
                            sink.on_wait_failure(task_id, &failure.to_string()).await;
                        }
                        retain_suspended_until_reaped(
                            child,
                            guard,
                            deadline,
                            cmd.lifetime_guard().cloned(),
                            verified_sink,
                        );
                        return Err(failure);
                    }
                }
            }
        }
    };

    if let Some((binding, sink)) = cmd
        .background_task()
        .and_then(|binding| binding.on_exit.as_ref().map(|sink| (binding, sink)))
    {
        if let Err(error) = sink.on_spawn(&binding.task_id, pid).await {
            if deadline.strict_host || deadline.verify_supervised_stop {
                fail_foreground!(error);
            }
            let _ = super::kill_tree::kill_tree_windows(pid).await;
            let _ = child.kill().await;
            let _ = child.wait().await;
            guard.0 = None;
            return Err(error);
        }
    }

    #[cfg(windows)]
    {
        if let Err(error) = super::supervisor_gate::release(initial_thread) {
            if deadline.strict_host || deadline.verify_supervised_stop {
                fail_foreground!(error);
            }
            let _ = super::kill_tree::kill_tree_windows(pid).await;
            let _ = child.kill().await;
            let _ = child.wait().await;
            guard.0 = None;
            return Err(error);
        }
    }

    let agent_registration =
        lingxi_core::host::agent_processes::register_with_capability_requirement(
            cmd.process_owner(),
            Some(pid),
            cmd.requires_group_cleanup_on_exit(),
        );
    let mut stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => fail_foreground!(ProcessError::Io("no stdout".into())),
    };
    let mut stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => fail_foreground!(ProcessError::Io("no stderr".into())),
    };
    let stop = cmd
        .background_task()
        .and_then(|binding| binding.on_exit.as_ref())
        .and_then(|sink| sink.stop_notify());
    if let Some(mut stdin) = child.stdin.take() {
        if let Some(text) = &cmd.inner().stdin {
            // The child can leave its input pipe full forever.  Keep stop
            // observable while writing, including before background handoff.
            let written = tokio::select! {
                biased;
                () = wait_for_stop(&stop) => None,
                result = stdin.write_all(text.as_bytes()) => Some(result),
            };
            drop(stdin);
            let cause = match written {
                Some(Ok(())) => None,
                Some(Err(error)) if !deadline.strict_host && !deadline.verify_supervised_stop => {
                    return Err(io_error(error))
                }
                Some(Err(error)) => Some(io_error(error)),
                None => Some(ProcessError::Io("supervisor command cancelled".into())),
            };
            if let Some(cause) = cause {
                let cleanup = terminate_and_reap(&deadline, &mut child, &mut guard).await;
                return Err(foreground_failure(cmd, cause, cleanup, child, guard, deadline).await);
            }
        }
    }
    let mut capture = Capture::new(cmd, limit);
    let mut eof = [false; 2];
    let mut timed_out = false;
    let mut retain_after_output_eof = false;
    if !explicit {
        let foreground_deadline =
            tokio::time::Instant::now() + cmd.inner().timeout.unwrap_or(DEFAULT_TIMEOUT);
        let foreground_wait = tokio::time::sleep_until(foreground_deadline);
        if deadline.strict_host && !cmd.auto_background_on_timeout() {
            deadline.at = Some(foreground_deadline);
        }
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
        let mut out = [0; 8192];
        let mut err = [0; 8192];
        let mut control = DrainControl::default();
        let mut stopped = false;
        loop {
            tokio::select! {
                biased;
                () = wait_for_stop(&stop) => {
                    stopped = true;
                    break;
                }
                () = &mut requested => break,
                () = &mut foreground_wait => { timed_out = true; break; }
                result = child.wait(), if eof.iter().all(|eof| *eof) => {
                    let status = match result { Ok(status) => status, Err(error) => fail_foreground!(io_error(error)) };
                    guard.0 = None;
                    if let Err(error) = finish_foreground(&mut capture, &mut child, &mut guard, &mut deadline, &stop).await { fail_foreground!(error); }
                    return Ok(ForegroundRunResult {output_file: capture.metadata(), outcome: ForegroundOutcome::Completed(ProcessOutput {stdout: capture.stdout, stderr: capture.stderr, exit_code: status.code().unwrap_or(-1), timed_out: false})});
                }
                result = stdout.read(&mut out), if !eof[0] => {
                    let n = match result { Ok(n) => n, Err(error) => fail_foreground!(io_error(error)) };
                    eof[0] = n == 0;
                    if deadline.strict_host || deadline.verify_supervised_stop {
                        let captured = capture_chunk_with_stop(&mut capture, &out[..n], false, n == 0, &mut child, &mut deadline, &mut control, &stop).await;
                        if let Err(error) = captured {
                            fail_foreground!(error);
                        }
                        if control.stop_handled {
                            timed_out = deadline.expired;
                            stopped = !deadline.expired;
                            break;
                        }
                    } else { capture.chunk(&out[..n], false, n == 0).await?; }
                }
                result = stderr.read(&mut err), if !eof[1] => {
                    let n = match result { Ok(n) => n, Err(error) => fail_foreground!(io_error(error)) };
                    eof[1] = n == 0;
                    if deadline.strict_host || deadline.verify_supervised_stop {
                        let captured = capture_chunk_with_stop(&mut capture, &err[..n], true, n == 0, &mut child, &mut deadline, &mut control, &stop).await;
                        if let Err(error) = captured {
                            fail_foreground!(error);
                        }
                        if control.stop_handled {
                            timed_out = deadline.expired;
                            stopped = !deadline.expired;
                            break;
                        }
                    } else { capture.chunk(&err[..n], true, n == 0).await?; }
                }
            }
        }
        if stopped {
            deadline.verify_lease |= deadline.verify_supervised_stop;
            if deadline.strict_host || deadline.verify_lease {
                if let Err(error) = terminate_and_reap(&deadline, &mut child, &mut guard).await {
                    fail_foreground!(error);
                }
            } else {
                // Preserve the ordinary runner's tree-error contract.
                let tree = super::kill_tree::kill_tree_windows(pid).await;
                let killed = child.kill().await.map_err(io_error);
                let waited = child.wait().await.map_err(io_error);
                if waited.is_ok() {
                    guard.0 = None;
                }
                waited?;
                tree?;
                killed?;
            }
            drain(
                &mut child,
                &mut stdout,
                &mut stderr,
                &mut capture,
                eof,
                &mut deadline,
                true,
            )
            .await?;
            flush_capture(&mut capture, deadline.strict_host).await?;
            finalize_capture(&mut capture, deadline.strict_host).await?;
            return Err(ProcessError::Io("supervisor command cancelled".into()));
        }
        // A host deadline owns the whole payload job, even if the direct
        // shell has already exited while a descendant still holds its pipes.
        if timed_out && !cmd.auto_background_on_timeout() && deadline.strict_host {
            if let Err(error) = terminate_and_reap(&deadline, &mut child, &mut guard).await {
                fail_foreground!(error);
            }
            capture.chunk(&[], false, true).await?;
            capture.chunk(&[], true, true).await?;
            flush_capture(&mut capture, deadline.strict_host).await?;
            finalize_capture(&mut capture, deadline.strict_host).await?;
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
        // A supervisor may have a reaped leader whose descendants still own
        // stdout or stderr. Keep the real output owner visible as a background
        // task instead of returning Completed after a timed-out drain.
        let exited_status = match child.try_wait() {
            Ok(status) => status,
            Err(error) => fail_foreground!(io_error(error)),
        };
        if let Some(status) = exited_status {
            guard.0 = None;
            #[cfg(test)]
            let preserve_legacy = std::env::var_os("LINGXI_DETACHED_JOB_LEGACY_BLOCKING").is_some();
            #[cfg(not(test))]
            let preserve_legacy = false;
            retain_after_output_eof = cmd.is_supervisor_request()
                && !deadline.strict_host
                && !eof.iter().all(|closed| *closed)
                && !preserve_legacy;
            if !retain_after_output_eof {
                if let Ok(Err(error)) = tokio::time::timeout(
                    std::time::Duration::from_millis(50),
                    drain(
                        &mut child,
                        &mut stdout,
                        &mut stderr,
                        &mut capture,
                        eof,
                        &mut deadline,
                        false,
                    ),
                )
                .await
                {
                    fail_foreground!(error);
                }
                if deadline.strict_host && deadline.terminated {
                    if let Err(error) = terminate_and_reap(&deadline, &mut child, &mut guard).await
                    {
                        fail_foreground!(error);
                    }
                    if !deadline.expired {
                        return Err(ProcessError::Io("supervisor command cancelled".into()));
                    }
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
                if let Err(error) =
                    finish_foreground(&mut capture, &mut child, &mut guard, &mut deadline, &stop)
                        .await
                {
                    fail_foreground!(error);
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
        }
        if timed_out && !cmd.auto_background_on_timeout() && !retain_after_output_eof {
            let _ = super::kill_tree::kill_tree_windows(pid).await;
            let _ = child.kill().await;
            let _ = child.wait().await;
            guard.0 = None;
            capture.chunk(&[], false, true).await?;
            capture.chunk(&[], true, true).await?;
            flush_capture(&mut capture, deadline.strict_host).await?;
            finalize_capture(&mut capture, deadline.strict_host).await?;
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
    deadline.at = None;
    let lifetime_guard = cmd.lifetime_guard().cloned();
    deadline.verify_lease = lifetime_guard.is_some();
    if deadline.strict_host || deadline.verify_lease || deadline.verify_supervised_stop {
        let stop = capture
            .binding
            .on_exit
            .as_ref()
            .and_then(|sink| sink.stop_notify());
        let mut control = DrainControl::default();
        let prepared = async {
            handoff_output_with_stop(
                capture.spill(),
                &mut child,
                &mut guard,
                &mut deadline,
                &mut control,
                &stop,
            )
            .await?;
            handoff_output_with_stop(
                flush_capture(&mut capture, true),
                &mut child,
                &mut guard,
                &mut deadline,
                &mut control,
                &stop,
            )
            .await
        }
        .await;
        if let Err(error) = prepared {
            deadline.verify_lease |= deadline.verify_supervised_stop;
            let cleanup = terminate_and_reap(&deadline, &mut child, &mut guard).await;
            return Err(foreground_failure(cmd, error, cleanup, child, guard, deadline).await);
        }
    } else {
        capture.spill().await?;
        capture.flush().await?;
    }
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
        let result = drain(
            &mut child,
            &mut stdout,
            &mut stderr,
            &mut capture,
            eof,
            &mut deadline,
            false,
        )
        .await;
        let mut cleanup_verified = false;
        let status = match result {
            Ok(status) => {
                if retain_after_output_eof {
                    // EOF proves only that output writers closed. Do not
                    // release the Job or manufacture descendant proof.
                    Err(format!("foreground output closed after leader exit {:?}; process group cleanup remains unverified", status.code()))
                } else if deadline.strict_host || deadline.verify_lease || lifetime_guard.is_some()
                {
                    match terminate_and_reap(&deadline, &mut child, &mut guard).await {
                        Ok(_) => {
                            cleanup_verified = true;
                            Ok(status)
                        }
                        Err(error) => Err(format!("background process cleanup failed: {error}")),
                    }
                } else if deadline.verify_supervised_stop {
                    // A natural supervisor completion must not terminate
                    // descendants merely by dropping the payload Job.
                    match deadline.completed_foreground() {
                        Ok(()) => {
                            guard.0 = None;
                            Ok(status)
                        }
                        Err(error) => Err(format!("background job release failed: {error}")),
                    }
                } else {
                    guard.0 = None;
                    Ok(status)
                }
            }
            Err(error) => {
                deadline.verify_lease |= deadline.verify_supervised_stop;
                let cleanup = terminate_and_reap(&deadline, &mut child, &mut guard).await;
                cleanup_verified = cleanup.is_ok();
                Err(match cleanup {
                    Ok(_) => format!("background drain failed: {error}"),
                    Err(cleanup) => {
                        format!("background drain failed: {error}; cleanup failed: {cleanup}")
                    }
                })
            }
        };
        drop(agent_registration);
        let flushed = flush_capture(
            &mut capture,
            deadline.strict_host || deadline.verify_lease || deadline.verify_supervised_stop,
        )
        .await;
        let flushed = if deadline.strict_host
            || deadline.verify_lease
            || deadline.verify_supervised_stop
            || lifetime_guard.is_some()
        {
            flushed
        } else {
            // Legacy sinks historically received the exit even if their final
            // output flush failed. Strict host/lease owners retain the error.
            let _ = flushed;
            Ok(())
        };
        let completion = background_completion(status, flushed);
        let completion_failed = completion.is_err();
        // If output fails after a leader-only natural exit, a later cleanup
        // notification must still prove that the retained Job is empty.
        if completion_failed && deadline.verify_supervised_stop {
            deadline.verify_lease = true;
        }
        let verified_sink = capture
            .binding
            .on_exit
            .as_ref()
            .map(|sink| (sink.clone(), capture.binding.task_id.clone()));
        if let Some(sink) = capture.binding.on_exit.as_ref() {
            report_background_completion(
                sink.as_ref(),
                &capture.binding.task_id,
                completion,
                deadline.expired,
                deadline.requested_stop,
                cleanup_verified,
            )
            .await;
        }
        if completion_failed && cleanup_verified {
            if let Some((sink, task_id)) = &verified_sink {
                sink.on_cleanup_verified(task_id).await;
            }
        }
        if (deadline.strict_host
            || deadline.verify_lease
            || lifetime_guard.is_some()
            || completion_failed)
            && !cleanup_verified
        {
            // Reporting an unverified failure is bounded, but releasing a
            // backend mount while an owned process may still use it is not
            // safe. Keep the exact child/job/lease and retry independently.
            retain_until_reaped(child, guard, deadline, lifetime_guard, verified_sink);
        } else {
            // The lease outlives the held process, payload job, and terminal
            // output notification, even without a task registry.
            drop(guard);
            drop(child);
            #[cfg(windows)]
            drop(deadline);
            drop(lifetime_guard);
        }
    });
    Ok(ForegroundRunResult {
        outcome: ForegroundOutcome::MovedToBackground(handle),
        output_file: None,
    })
}

#[cfg(test)]
mod completion_failure_tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Sink {
        failures: Mutex<Vec<String>>,
        exits: Mutex<Vec<Option<i32>>>,
        known: Mutex<Vec<(Option<i32>, bool)>>,
    }

    #[async_trait::async_trait]
    impl lingxi_core::host::BackgroundExitSink for Sink {
        async fn on_exit(&self, _: &str, code: Option<i32>) {
            self.exits.lock().unwrap().push(code);
        }

        async fn on_wait_failure(&self, _: &str, error: &str) {
            self.failures.lock().unwrap().push(error.to_owned());
        }

        async fn on_exit_with_status(&self, _: &str, code: Option<i32>, killed: bool) {
            self.known.lock().unwrap().push((code, killed));
        }
    }

    #[tokio::test]
    async fn failed_wait_and_flush_never_report_a_verified_exit() {
        let sink = Sink::default();
        let completion = background_completion(
            Err("injected child wait failure".into()),
            Err(ProcessError::Io("injected flush failure".into())),
        );
        report_background_completion(&sink, "task", completion, true, false, false).await;
        assert!(sink.exits.lock().unwrap().is_empty());
        let failures = sink.failures.lock().unwrap();
        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains("injected child wait failure"));
        assert!(failures[0].contains("injected flush failure"));
    }

    #[tokio::test]
    async fn completed_stop_is_unknown_and_natural_completion_is_known() {
        #[cfg(unix)]
        use std::os::unix::process::ExitStatusExt as _;
        #[cfg(windows)]
        use std::os::windows::process::ExitStatusExt as _;

        let sink = Sink::default();
        report_background_completion(
            &sink,
            "task",
            Ok(std::process::ExitStatus::from_raw(0)),
            false,
            true,
            false,
        )
        .await;
        assert_eq!(*sink.exits.lock().unwrap(), [Some(0)]);
        assert!(sink.known.lock().unwrap().is_empty());
        report_background_completion(
            &sink,
            "task",
            Ok(std::process::ExitStatus::from_raw(0)),
            false,
            false,
            false,
        )
        .await;
        assert_eq!(*sink.known.lock().unwrap(), [(Some(0), false)]);
        assert!(sink.failures.lock().unwrap().is_empty());
    }

    #[test]
    fn suspended_cleanup_requires_a_successful_wait() {
        let failure = suspended_cleanup_result(
            Err(ProcessError::Io("injected kill failure".into())),
            Err(ProcessError::Io("injected wait failure".into())),
        )
        .unwrap_err()
        .to_string();
        assert!(failure.contains("injected kill failure"));
        assert!(failure.contains("wait unverified"));
        assert!(failure.contains("injected wait failure"));
        assert!(suspended_cleanup_result(
            Ok(()),
            Err(ProcessError::Io("injected wait failure".into()))
        )
        .is_err());
        // Exit between the kill request and wait is still a verified exit:
        // the initial thread was never resumed and has no descendants.
        assert!(
            suspended_cleanup_result(Err(ProcessError::Io("already exited".into())), Ok(()))
                .is_ok()
        );
    }
}

#[cfg(all(test, windows))]
mod native_background_stop_tests {
    use super::*;
    use lingxi_core::host::{BackgroundExitSink, ProcessCommand, SandboxedTag};
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio::sync::Notify;

    const REDIRECTED_FIXTURE: &str =
        "process::background::native_background_stop_tests::redirected_descendant_fixture";
    type ExitProof = (Option<i32>, Option<bool>, bool);

    struct ReleaseOnDrop(std::path::PathBuf);
    impl Drop for ReleaseOnDrop {
        fn drop(&mut self) {
            let _ = std::fs::write(self.0.join("release"), "release");
            let _ = std::fs::write(self.0.join("leader.release"), "release");
        }
    }

    #[derive(Default)]
    struct Sink {
        stop: Arc<Notify>,
        pause_append: AtomicBool,
        pause_empty: AtomicBool,
        pause_flush: AtomicBool,
        reject_flush: AtomicBool,
        append_started: Notify,
        append_resume: Notify,
        flush_started: Notify,
        flush_resume: Notify,
        cleanup_notified: Notify,
        cleanups: AtomicU32,
        terminal: Notify,
        pid: AtomicU32,
        output: Mutex<String>,
        exits: Mutex<Vec<Option<i32>>>,
        known: Mutex<Vec<(Option<i32>, bool)>>,
        proofs: Mutex<Vec<ExitProof>>,
        failures: Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl BackgroundExitSink for Sink {
        async fn on_spawn(&self, _: &str, pid: u32) -> Result<(), ProcessError> {
            self.pid.store(pid, Ordering::SeqCst);
            Ok(())
        }

        fn stop_notify(&self) -> Option<Arc<Notify>> {
            Some(self.stop.clone())
        }

        fn requires_verified_supervised_stop(&self) -> bool {
            true
        }

        fn manages_output(&self) -> bool {
            true
        }

        async fn append_output(&self, _: &str, text: &str) -> Result<(), ProcessError> {
            self.output.lock().unwrap().push_str(text);
            if (!text.is_empty() || self.pause_empty.load(Ordering::SeqCst))
                && self.pause_append.swap(false, Ordering::SeqCst)
            {
                self.append_started.notify_one();
                self.append_resume.notified().await;
            }
            Ok(())
        }

        async fn flush_output(&self, _: &str) -> Result<(), ProcessError> {
            if self.pause_flush.load(Ordering::SeqCst) {
                self.flush_started.notify_one();
                self.flush_resume.notified().await;
            }
            if self.reject_flush.load(Ordering::SeqCst) {
                Err(ProcessError::Io("injected output flush failure".into()))
            } else {
                Ok(())
            }
        }

        async fn finalize_persisted_output(
            &self,
            _: &str,
            _: u64,
        ) -> Result<Option<u64>, ProcessError> {
            Ok(Some(self.output.lock().unwrap().len() as u64))
        }

        async fn on_exit(&self, _: &str, code: Option<i32>) {
            self.exits.lock().unwrap().push(code);
            self.terminal.notify_one();
        }

        async fn on_exit_with_status(&self, _: &str, code: Option<i32>, killed: bool) {
            self.known.lock().unwrap().push((code, killed));
            self.exits.lock().unwrap().push(code);
            self.terminal.notify_one();
        }

        async fn on_exit_with_proof(
            &self,
            _: &str,
            code: Option<i32>,
            killed: Option<bool>,
            group: bool,
        ) {
            self.proofs.lock().unwrap().push((code, killed, group));
            if let Some(killed) = killed {
                self.known.lock().unwrap().push((code, killed));
            }
            self.exits.lock().unwrap().push(code);
            self.terminal.notify_one();
        }

        async fn on_wait_failure(&self, _: &str, error: &str) {
            self.failures.lock().unwrap().push(error.to_owned());
            self.terminal.notify_one();
        }

        async fn on_cleanup_verified(&self, _: &str) {
            self.cleanups.fetch_add(1, Ordering::SeqCst);
            self.cleanup_notified.notify_one();
        }
    }

    fn command(dir: &std::path::Path, sink: Arc<Sink>) -> SandboxedCommand {
        command_with_timeout(dir, sink, std::time::Duration::from_secs(60))
    }

    fn command_with_timeout(
        dir: &std::path::Path,
        sink: Arc<Sink>,
        timeout: std::time::Duration,
    ) -> SandboxedCommand {
        let script = r#"
            $child = Start-Process -FilePath powershell.exe -NoNewWindow -PassThru `
                -RedirectStandardOutput $env:BACKGROUND_FIXTURE_CHILD_OUT `
                -RedirectStandardError $env:BACKGROUND_FIXTURE_CHILD_ERR `
                -ArgumentList '-NoProfile','-NonInteractive','-Command','Start-Sleep -Seconds 60'
            [System.IO.File]::WriteAllText($env:BACKGROUND_FIXTURE_PID_FILE, [string]$child.Id)
            [Console]::Out.Write('payload')
            [Console]::Out.Flush()
            Start-Sleep -Seconds 60
        "#;
        command_with_script(dir, sink, timeout, script)
    }

    fn command_with_script(
        dir: &std::path::Path,
        sink: Arc<Sink>,
        timeout: std::time::Duration,
        script: &str,
    ) -> SandboxedCommand {
        SandboxedCommand::__new_sandboxed(
            ProcessCommand {
                command: "powershell.exe".into(),
                args: vec![
                    "-NoProfile".into(),
                    "-NonInteractive".into(),
                    "-Command".into(),
                    script.into(),
                ],
                cwd: Some(dir.to_path_buf()),
                env: std::collections::HashMap::from([
                    (
                        "BACKGROUND_FIXTURE_PID_FILE".into(),
                        dir.join("descendant.pid").to_string_lossy().into_owned(),
                    ),
                    (
                        "BACKGROUND_FIXTURE_CHILD_OUT".into(),
                        dir.join("descendant.stdout").to_string_lossy().into_owned(),
                    ),
                    (
                        "BACKGROUND_FIXTURE_CHILD_ERR".into(),
                        dir.join("descendant.stderr").to_string_lossy().into_owned(),
                    ),
                    (
                        "BACKGROUND_FIXTURE_READY".into(),
                        dir.join("descendant.ready").to_string_lossy().into_owned(),
                    ),
                    (
                        "BACKGROUND_FIXTURE_RELEASE".into(),
                        dir.join("release").to_string_lossy().into_owned(),
                    ),
                ]),
                stdin: None,
                timeout: Some(timeout),
            },
            SandboxedTag::BypassAuditedWithReason {
                reason: "isolated native Windows background stop fixture".into(),
            },
        )
        .with_process_owner(Some(format!("host-tool:test-{}", dir.display())))
        .with_require_group_cleanup_on_exit(true)
        .with_background_task(BackgroundTaskBinding {
            task_id: "native-background-stop".into(),
            output_path: dir.join("output"),
            on_exit: Some(sink),
            on_demand: None,
        })
    }

    fn redirected_descendant_command(
        dir: &std::path::Path,
        sink: Arc<Sink>,
        strict: bool,
    ) -> SandboxedCommand {
        SandboxedCommand::__new_sandboxed(
            ProcessCommand {
                command: std::env::current_exe()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                args: vec![
                    REDIRECTED_FIXTURE.into(),
                    "--exact".into(),
                    "--nocapture".into(),
                ],
                cwd: Some(dir.to_path_buf()),
                env: std::collections::HashMap::from([
                    ("BACKGROUND_REDIRECTED_FIXTURE_ROLE".into(), "leader".into()),
                    (
                        "BACKGROUND_REDIRECTED_FIXTURE_DIR".into(),
                        dir.to_string_lossy().into_owned(),
                    ),
                ]),
                stdin: None,
                timeout: Some(std::time::Duration::from_secs(60)),
            },
            SandboxedTag::BypassAuditedWithReason {
                reason: "isolated redirected descendant fixture".into(),
            },
        )
        .with_process_owner(strict.then(|| format!("host-tool:test-{}", dir.display())))
        .with_require_group_cleanup_on_exit(strict)
        .with_background_task(BackgroundTaskBinding {
            task_id: "native-background-stop".into(),
            output_path: dir.join("output"),
            on_exit: Some(sink),
            on_demand: None,
        })
        .with_auto_background_on_timeout(false)
    }

    // This fixture deliberately leaves its child running after the leader
    // exits; the parent test holds and verifies the child's process handle.
    #[allow(clippy::zombie_processes)]
    #[test]
    fn redirected_descendant_fixture() {
        let Some(role) = std::env::var_os("BACKGROUND_REDIRECTED_FIXTURE_ROLE") else {
            return;
        };
        let dir = std::path::PathBuf::from(
            std::env::var_os("BACKGROUND_REDIRECTED_FIXTURE_DIR").unwrap(),
        );
        if role == "child" {
            std::fs::write(dir.join("descendant.ready"), "ready").unwrap();
            let until = std::time::Instant::now() + std::time::Duration::from_secs(60);
            while !dir.join("release").exists() && std::time::Instant::now() < until {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            assert!(dir.join("release").exists(), "child was not released");
            std::fs::write(dir.join("descendant.completed"), "done").unwrap();
            return;
        }
        assert_eq!(role, "leader");
        // Redirecting the child's standard streams is insufficient on Windows:
        // an inheritable handle can survive as a non-standard handle. Clear the
        // leader's inherited pipe handles before spawning its redirected child.
        super::super::supervisor_gate::protect_standard_handles().unwrap();
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([REDIRECTED_FIXTURE, "--exact", "--nocapture"])
            .env("BACKGROUND_REDIRECTED_FIXTURE_ROLE", "child")
            .env("BACKGROUND_REDIRECTED_FIXTURE_DIR", &dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        std::fs::write(dir.join("descendant.pid"), child.id().to_string()).unwrap();
        let until = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while !dir.join("descendant.ready").exists() {
            assert!(std::time::Instant::now() < until, "child did not start");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        while !dir.join("leader.release").exists() {
            assert!(std::time::Instant::now() < until, "leader was not released");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    async fn descendant_pid(dir: &std::path::Path) -> u32 {
        tokio::time::timeout(std::time::Duration::from_secs(20), async {
            loop {
                if let Ok(text) = std::fs::read_to_string(dir.join("descendant.pid")) {
                    if let Ok(pid) = text.trim().parse() {
                        break pid;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("descendant did not start")
    }

    async fn exited(pid: u32) {
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                if super::super::supervisor_native::process_is_alive(pid) == Some(false) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("process survived the payload job stop");
    }

    async fn held_exited(handle: &std::os::windows::io::OwnedHandle) {
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                match super::super::supervisor_native::process_handle_is_alive_for_test(handle) {
                    Some(false) => break,
                    Some(true) => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
                    None => panic!("could not query held process handle"),
                }
            }
        })
        .await
        .expect("held process survived the payload job stop");
    }

    #[tokio::test]
    async fn stop_kills_background_job_while_accepted_output_append_is_pending() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(Sink::default());
        sink.pause_append.store(true, Ordering::SeqCst);
        let command = command(dir.path(), sink.clone());
        let owner = command.process_owner().unwrap().to_owned();
        let ForegroundOutcome::MovedToBackground(handle) =
            run(&command, None, true).await.unwrap().outcome
        else {
            panic!("explicit background command was not handed off");
        };
        let descendant = descendant_pid(dir.path()).await;
        tokio::time::timeout(
            std::time::Duration::from_secs(20),
            sink.append_started.notified(),
        )
        .await
        .expect("output append did not start");
        assert_eq!(
            super::super::supervisor_native::process_is_alive(handle.pid),
            Some(true)
        );
        assert_eq!(
            super::super::supervisor_native::process_is_alive(descendant),
            Some(true)
        );

        sink.stop.notify_one();
        exited(handle.pid).await;
        exited(descendant).await;
        assert!(sink.exits.lock().unwrap().is_empty());
        assert!(sink.failures.lock().unwrap().is_empty());
        sink.append_resume.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(15), sink.terminal.notified())
            .await
            .expect("background completion was not reported");
        assert_eq!(sink.exits.lock().unwrap().len(), 1);
        assert!(
            sink.known.lock().unwrap().is_empty(),
            "stop outcome must remain unknown"
        );
        assert!(matches!(
            &sink.proofs.lock().unwrap()[..],
            [(_, None, true)]
        ));
        assert!(sink.failures.lock().unwrap().is_empty());
        assert_eq!(*sink.output.lock().unwrap(), "payload");
        assert!(lingxi_core::host::agent_processes::snapshot(&owner).is_empty());
    }

    #[tokio::test]
    async fn ordinary_supervised_stop_verifies_the_job_before_unknown_outcome() {
        for deadline in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let sink = Arc::new(Sink::default());
            let command = command(dir.path(), sink.clone())
                .with_process_owner(None)
                .with_require_group_cleanup_on_exit(false)
                .with_background_timeout(deadline.then_some(std::time::Duration::from_secs(5)));
            let ForegroundOutcome::MovedToBackground(handle) =
                run(&command, None, true).await.unwrap().outcome
            else {
                panic!("explicit background command was not handed off");
            };
            let descendant = descendant_pid(dir.path()).await;
            assert_eq!(
                super::super::supervisor_native::process_is_alive(handle.pid),
                Some(true)
            );
            assert_eq!(
                super::super::supervisor_native::process_is_alive(descendant),
                Some(true)
            );
            if !deadline {
                sink.stop.notify_one();
            }
            tokio::time::timeout(std::time::Duration::from_secs(20), sink.terminal.notified())
                .await
                .expect("verified stop or deadline did not settle");
            assert_eq!(sink.exits.lock().unwrap().len(), 1);
            assert!(sink.known.lock().unwrap().is_empty());
            assert!(matches!(
                &sink.proofs.lock().unwrap()[..],
                [(_, None, true)]
            ));
            assert!(sink.failures.lock().unwrap().is_empty());
            exited(handle.pid).await;
            exited(descendant).await;
        }
    }

    #[tokio::test]
    async fn supervised_natural_exit_is_known_and_not_rewritten_by_later_stop() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(Sink::default());
        let command = SandboxedCommand::__new_sandboxed(
            ProcessCommand {
                command: "powershell.exe".into(),
                args: vec![
                    "-NoProfile".into(),
                    "-NonInteractive".into(),
                    "-Command".into(),
                    "[Console]::Out.Write('done'); exit 0".into(),
                ],
                cwd: Some(dir.path().to_path_buf()),
                env: std::collections::HashMap::new(),
                stdin: None,
                timeout: Some(std::time::Duration::from_secs(30)),
            },
            SandboxedTag::BypassAuditedWithReason {
                reason: "isolated supervised natural-exit fixture".into(),
            },
        )
        .with_background_task(BackgroundTaskBinding {
            task_id: "native-background-natural".into(),
            output_path: dir.path().join("output"),
            on_exit: Some(sink.clone()),
            on_demand: None,
        });
        let ForegroundOutcome::MovedToBackground(handle) =
            run(&command, None, true).await.unwrap().outcome
        else {
            panic!("explicit background command was not handed off");
        };
        tokio::time::timeout(std::time::Duration::from_secs(20), sink.terminal.notified())
            .await
            .expect("natural completion was not reported");
        sink.stop.notify_one();
        assert_eq!(*sink.known.lock().unwrap(), [(Some(0), false)]);
        assert_eq!(
            *sink.proofs.lock().unwrap(),
            [(Some(0), Some(false), false)]
        );
        assert_eq!(*sink.exits.lock().unwrap(), [Some(0)]);
        assert!(sink.failures.lock().unwrap().is_empty());
        exited(handle.pid).await;
    }

    #[tokio::test]
    async fn supervised_leader_and_group_proofs_follow_actual_job_cleanup() {
        for strict in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let _release = ReleaseOnDrop(dir.path().to_path_buf());
            let sink = Arc::new(Sink::default());
            let command = redirected_descendant_command(dir.path(), sink.clone(), strict)
                .with_process_owner(None);
            let ForegroundOutcome::MovedToBackground(handle) =
                run(&command, None, true).await.unwrap().outcome
            else {
                panic!("explicit background command was not handed off");
            };
            let descendant = descendant_pid(dir.path()).await;
            let held_descendant =
                super::super::supervisor_native::hold_process_handle_for_test(descendant)
                    .expect("open descendant process handle");
            assert_eq!(
                super::super::supervisor_native::process_handle_is_alive_for_test(&held_descendant),
                Some(true)
            );
            std::fs::write(dir.path().join("leader.release"), "release").unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(30), sink.terminal.notified())
                .await
                .expect("leader completion was not reported");
            assert_eq!(
                *sink.proofs.lock().unwrap(),
                [(Some(0), Some(false), strict)]
            );
            assert!(sink.failures.lock().unwrap().is_empty());
            if !strict {
                assert_eq!(
                    super::super::supervisor_native::process_handle_is_alive_for_test(
                        &held_descendant
                    ),
                    Some(true)
                );
                std::fs::write(dir.path().join("release"), "release").unwrap();
            } else {
                assert_eq!(
                    super::super::supervisor_native::process_handle_is_alive_for_test(
                        &held_descendant
                    ),
                    Some(false)
                );
                assert!(!dir.path().join("descendant.completed").exists());
            }
            exited(handle.pid).await;
            held_exited(&held_descendant).await;
            if !strict {
                assert!(dir.path().join("descendant.completed").exists());
            }
        }
    }

    #[tokio::test]
    async fn stopped_resumed_job_reports_unknown_without_cleanup_receipt() {
        let dir = tempfile::tempdir().unwrap();
        let _release = ReleaseOnDrop(dir.path().to_path_buf());
        let sink = Arc::new(Sink::default());
        let command = redirected_descendant_command(dir.path(), sink.clone(), true);
        let ForegroundOutcome::MovedToBackground(_) =
            run(&command, None, true).await.unwrap().outcome
        else {
            panic!("explicit background command was not handed off");
        };
        let descendant = descendant_pid(dir.path()).await;
        let held = super::super::supervisor_native::hold_process_handle_for_test(descendant)
            .expect("open original descendant process");
        assert_eq!(
            super::super::supervisor_native::process_handle_is_alive_for_test(&held),
            Some(true)
        );
        sink.stop.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(20), sink.terminal.notified())
            .await
            .expect("unknown cleanup result was not reported");
        let failures = sink.failures.lock().unwrap().clone();
        assert!(
            failures
                .iter()
                .any(|failure| failure.contains("no complete descendant handle proof")),
            "{failures:?}"
        );
        assert!(sink.exits.lock().unwrap().is_empty());
        assert!(sink.proofs.lock().unwrap().is_empty());
        assert_eq!(sink.cleanups.load(Ordering::SeqCst), 0);
        held_exited(&held).await;
        // This test happens to hold one descendant, but production still has
        // no proof that all possible descendants were captured.
        assert_eq!(sink.cleanups.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn strict_foreground_eof_does_not_manufacture_group_cleanup_proof() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(Sink::default());
        let command = command_with_script(
            dir.path(),
            sink.clone(),
            std::time::Duration::from_secs(30),
            "[Console]::Out.Write('completed'); exit 0",
        );
        let error = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            run(&command, None, false),
        )
        .await
        .expect("strict foreground did not settle")
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("no complete descendant handle proof"),
            "{error}"
        );
        assert!(sink.exits.lock().unwrap().is_empty());
        assert!(sink.proofs.lock().unwrap().is_empty());
        assert_eq!(sink.cleanups.load(Ordering::SeqCst), 0);
    }

    // The leader must exit before the inherited-pipe child is stopped.
    #[allow(clippy::zombie_processes)]
    #[test]
    fn inherited_pipe_fixture() {
        const TEST: &str =
            "process::background::native_background_stop_tests::inherited_pipe_fixture";
        let Some(role) = std::env::var_os("BACKGROUND_PIPE_FIXTURE_ROLE") else {
            return;
        };
        let dir =
            std::path::PathBuf::from(std::env::var_os("BACKGROUND_PIPE_FIXTURE_DIR").unwrap());
        if role == "child" {
            std::fs::write(dir.join("child.ready"), "ready").unwrap();
            std::thread::sleep(std::time::Duration::from_secs(60));
            return;
        }
        assert_eq!(role, "leader");
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([TEST, "--exact", "--nocapture"])
            .env("BACKGROUND_PIPE_FIXTURE_ROLE", "child")
            .env("BACKGROUND_PIPE_FIXTURE_DIR", &dir)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        std::fs::write(dir.join("descendant.pid"), child.id().to_string()).unwrap();
        let until = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while !dir.join("child.ready").exists() {
            assert!(std::time::Instant::now() < until, "child did not start");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[tokio::test]
    async fn supervised_stop_after_leader_exit_verifies_descendant_job() {
        const TEST: &str =
            "process::background::native_background_stop_tests::inherited_pipe_fixture";
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(Sink::default());
        let command = SandboxedCommand::__new_sandboxed(
            ProcessCommand {
                command: std::env::current_exe()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                args: vec![TEST.into(), "--exact".into(), "--nocapture".into()],
                cwd: Some(dir.path().to_path_buf()),
                env: std::collections::HashMap::from([
                    ("BACKGROUND_PIPE_FIXTURE_ROLE".into(), "leader".into()),
                    (
                        "BACKGROUND_PIPE_FIXTURE_DIR".into(),
                        dir.path().to_string_lossy().into_owned(),
                    ),
                ]),
                stdin: None,
                timeout: Some(std::time::Duration::from_secs(30)),
            },
            SandboxedTag::BypassAuditedWithReason {
                reason: "isolated inherited-pipe stop fixture".into(),
            },
        )
        .with_background_task(BackgroundTaskBinding {
            task_id: "leader-then-stop-proof".into(),
            output_path: dir.path().join("output"),
            on_exit: Some(sink.clone()),
            on_demand: None,
        });
        let ForegroundOutcome::MovedToBackground(handle) =
            run(&command, None, true).await.unwrap().outcome
        else {
            panic!("explicit background command was not handed off");
        };
        let descendant = descendant_pid(dir.path()).await;
        let held_descendant =
            super::super::supervisor_native::hold_process_handle_for_test(descendant)
                .expect("open descendant process handle");
        tokio::time::timeout(std::time::Duration::from_secs(20), async {
            while super::super::supervisor_native::process_is_alive(handle.pid) != Some(false) {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("leader did not exit before stop");
        assert_eq!(
            super::super::supervisor_native::process_handle_is_alive_for_test(&held_descendant),
            Some(true)
        );
        assert!(sink.proofs.lock().unwrap().is_empty());
        sink.stop.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(20), sink.terminal.notified())
            .await
            .expect("stop after leader exit must verify its Job");
        assert_eq!(*sink.proofs.lock().unwrap(), [(Some(0), None, true)]);
        assert!(sink.failures.lock().unwrap().is_empty());
        assert_eq!(
            super::super::supervisor_native::process_handle_is_alive_for_test(&held_descendant),
            Some(false)
        );
        held_exited(&held_descendant).await;
    }

    #[tokio::test]
    async fn background_flush_failure_is_not_reported_as_a_successful_exit() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(Sink::default());
        let command = command(dir.path(), sink.clone());
        let ForegroundOutcome::MovedToBackground(handle) =
            run(&command, None, true).await.unwrap().outcome
        else {
            panic!("explicit background command was not handed off");
        };
        let descendant = descendant_pid(dir.path()).await;
        sink.reject_flush.store(true, Ordering::SeqCst);
        sink.stop.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(15), sink.terminal.notified())
            .await
            .expect("background failure was not reported");
        exited(handle.pid).await;
        exited(descendant).await;
        assert!(sink.exits.lock().unwrap().is_empty());
        {
            let failures = sink.failures.lock().unwrap();
            assert_eq!(failures.len(), 1);
            assert!(failures[0].contains("injected output flush failure"));
        }
        tokio::time::timeout(
            std::time::Duration::from_secs(15),
            sink.cleanup_notified.notified(),
        )
        .await
        .expect("verified process cleanup was not reported after flush failure");
        assert_eq!(sink.cleanups.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn unverified_background_cleanup_retries_before_signalling_verified() {
        struct Lease(Arc<AtomicBool>);
        impl Drop for Lease {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(Sink::default());
        let released = Arc::new(AtomicBool::new(false));
        let command =
            command(dir.path(), sink.clone()).with_lifetime_guard(Lease(released.clone()));
        let verification_blocked = Arc::new(AtomicBool::new(true));
        let ForegroundOutcome::MovedToBackground(handle) = TEST_JOB_VERIFICATION_BLOCKED
            .scope(verification_blocked.clone(), run(&command, None, true))
            .await
            .unwrap()
            .outcome
        else {
            panic!("explicit background command was not handed off");
        };
        drop(command);
        let descendant = descendant_pid(dir.path()).await;
        let held_descendant =
            super::super::supervisor_native::hold_process_handle_for_test(descendant)
                .expect("open descendant process handle");
        sink.stop.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(20), sink.terminal.notified())
            .await
            .expect("initial unverified cleanup failure was not reported");
        assert!(sink.exits.lock().unwrap().is_empty());
        assert_eq!(sink.failures.lock().unwrap().len(), 1);
        assert!(
            sink.failures.lock().unwrap()[0].contains("injected payload job verification failure")
        );
        assert_eq!(sink.cleanups.load(Ordering::SeqCst), 0);
        assert!(!released.load(Ordering::SeqCst));
        verification_blocked.store(false, Ordering::SeqCst);
        tokio::time::timeout(
            std::time::Duration::from_secs(20),
            sink.cleanup_notified.notified(),
        )
        .await
        .expect("retry never reported verified cleanup");
        assert_eq!(sink.cleanups.load(Ordering::SeqCst), 1);
        assert!(released.load(Ordering::SeqCst));
        assert_eq!(
            super::super::supervisor_native::process_handle_is_alive_for_test(&held_descendant),
            Some(false)
        );
        exited(handle.pid).await;
        held_exited(&held_descendant).await;
    }

    #[tokio::test]
    async fn background_exit_follows_detached_descendant_termination() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(Sink::default());
        let command = command(dir.path(), sink.clone());
        let ForegroundOutcome::MovedToBackground(handle) =
            run(&command, None, true).await.unwrap().outcome
        else {
            panic!("explicit background command was not handed off");
        };
        let descendant = descendant_pid(dir.path()).await;
        let held_leader = super::super::supervisor_native::hold_process_handle_for_test(handle.pid)
            .expect("open leader process handle");
        let held_descendant =
            super::super::supervisor_native::hold_process_handle_for_test(descendant)
                .expect("open descendant process handle");
        assert_eq!(
            super::super::supervisor_native::process_handle_is_alive_for_test(&held_leader),
            Some(true)
        );
        assert_eq!(
            super::super::supervisor_native::process_handle_is_alive_for_test(&held_descendant),
            Some(true)
        );
        sink.stop.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(15), sink.terminal.notified())
            .await
            .expect("verified background exit was not reported");
        assert_eq!(sink.exits.lock().unwrap().len(), 1);
        assert!(sink.failures.lock().unwrap().is_empty());
        assert_eq!(
            super::super::supervisor_native::process_handle_is_alive_for_test(&held_leader),
            Some(false)
        );
        assert_eq!(
            super::super::supervisor_native::process_handle_is_alive_for_test(&held_descendant),
            Some(false)
        );
    }

    #[tokio::test]
    async fn backend_lease_lives_until_background_exit_and_output_settle() {
        struct Lease(Arc<AtomicBool>);
        impl Drop for Lease {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(Sink::default());
        let released = Arc::new(AtomicBool::new(false));
        let command =
            command(dir.path(), sink.clone()).with_lifetime_guard(Lease(released.clone()));
        let ForegroundOutcome::MovedToBackground(_) =
            run(&command, None, true).await.unwrap().outcome
        else {
            panic!("explicit background command was not handed off");
        };
        drop(command);
        descendant_pid(dir.path()).await;
        assert!(!released.load(Ordering::SeqCst));
        sink.stop.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(15), sink.terminal.notified())
            .await
            .expect("background exit was not reported");
        assert!(sink.failures.lock().unwrap().is_empty());
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !released.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("backend lease remained held after verified completion");
    }

    #[tokio::test]
    async fn stuck_output_after_stop_reports_failure_with_both_processes_dead() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(Sink::default());
        sink.pause_append.store(true, Ordering::SeqCst);
        let command = command(dir.path(), sink.clone());
        let ForegroundOutcome::MovedToBackground(handle) =
            run(&command, None, true).await.unwrap().outcome
        else {
            panic!("explicit background command was not handed off");
        };
        let descendant = descendant_pid(dir.path()).await;
        tokio::time::timeout(
            std::time::Duration::from_secs(20),
            sink.append_started.notified(),
        )
        .await
        .expect("output append did not start");
        sink.stop.notify_one();
        exited(handle.pid).await;
        exited(descendant).await;
        tokio::time::timeout(std::time::Duration::from_secs(15), sink.terminal.notified())
            .await
            .expect("stuck output did not produce a bounded failure");
        assert!(sink.exits.lock().unwrap().is_empty());
        let failures = sink.failures.lock().unwrap();
        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains("output append did not settle after stop"));
    }

    #[tokio::test]
    async fn stuck_flush_after_stop_reports_failure_with_both_processes_dead() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(Sink::default());
        let command = command(dir.path(), sink.clone());
        let ForegroundOutcome::MovedToBackground(handle) =
            run(&command, None, true).await.unwrap().outcome
        else {
            panic!("explicit background command was not handed off");
        };
        let descendant = descendant_pid(dir.path()).await;
        sink.pause_flush.store(true, Ordering::SeqCst);
        sink.stop.notify_one();
        exited(handle.pid).await;
        exited(descendant).await;
        tokio::time::timeout(std::time::Duration::from_secs(15), sink.terminal.notified())
            .await
            .expect("stuck flush did not produce a bounded failure");
        assert!(sink.exits.lock().unwrap().is_empty());
        let failures = sink.failures.lock().unwrap();
        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains("output flush timed out"));
    }

    #[tokio::test]
    async fn foreground_stop_reaches_job_while_output_append_is_pending() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(Sink::default());
        sink.pause_append.store(true, Ordering::SeqCst);
        let command = command(dir.path(), sink.clone()).with_auto_background_on_timeout(false);
        let task = tokio::spawn(async move { run(&command, Some(0), false).await });
        let descendant = descendant_pid(dir.path()).await;
        tokio::time::timeout(
            std::time::Duration::from_secs(20),
            sink.append_started.notified(),
        )
        .await
        .expect("output append did not start");
        let pid = sink.pid.load(Ordering::SeqCst);
        assert_ne!(pid, 0);
        sink.stop.notify_one();
        exited(pid).await;
        exited(descendant).await;
        assert!(!task.is_finished());
        sink.append_resume.notify_one();
        let result = tokio::time::timeout(std::time::Duration::from_secs(15), task)
            .await
            .expect("foreground stop did not settle")
            .unwrap();
        assert!(
            matches!(result, Err(ProcessError::Io(message)) if message.contains("supervisor command cancelled"))
        );
        assert_eq!(*sink.output.lock().unwrap(), "payload");
    }

    #[tokio::test]
    async fn foreground_stop_reaches_job_while_stdin_pipe_is_full() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(Sink::default());
        let template = command(dir.path(), sink.clone());
        let mut process = template.inner().clone();
        // The fixture never reads stdin; this exceeds a Windows anonymous pipe
        // buffer by orders of magnitude and leaves the write pending.
        process.stdin = Some("x".repeat(8 * 1024 * 1024));
        let command = SandboxedCommand::__new_sandboxed(process, template.tag().clone())
            .with_process_owner(template.process_owner().map(str::to_owned))
            .with_background_task(template.background_task().unwrap().clone())
            .with_auto_background_on_timeout(false);
        let task = tokio::spawn(async move { run(&command, None, false).await });
        let descendant = descendant_pid(dir.path()).await;
        let pid = sink.pid.load(Ordering::SeqCst);
        assert_ne!(pid, 0);
        assert!(!task.is_finished(), "stdin write should still be pending");
        assert_eq!(
            super::super::supervisor_native::process_is_alive(pid),
            Some(true)
        );
        assert_eq!(
            super::super::supervisor_native::process_is_alive(descendant),
            Some(true)
        );

        sink.stop.notify_one();
        let result = tokio::time::timeout(std::time::Duration::from_secs(20), task)
            .await
            .expect("stop did not interrupt the full stdin pipe")
            .unwrap();
        assert!(
            matches!(result, Err(ProcessError::Io(message)) if message == "supervisor command cancelled")
        );
        exited(pid).await;
        exited(descendant).await;
    }

    #[tokio::test]
    async fn foreground_stop_during_final_flush_reaps_detached_descendant() {
        let dir = tempfile::tempdir().unwrap();
        let _release = ReleaseOnDrop(dir.path().to_path_buf());
        let sink = Arc::new(Sink::default());
        sink.pause_flush.store(true, Ordering::SeqCst);
        let command = redirected_descendant_command(dir.path(), sink.clone(), true);
        let task = tokio::spawn(async move { run(&command, None, false).await });
        let descendant = descendant_pid(dir.path()).await;
        let held_descendant =
            super::super::supervisor_native::hold_process_handle_for_test(descendant)
                .expect("open descendant process handle");
        std::fs::write(dir.path().join("leader.release"), "release").unwrap();
        tokio::time::timeout(
            std::time::Duration::from_secs(20),
            sink.flush_started.notified(),
        )
        .await
        .expect("foreground output flush did not start");
        assert_eq!(
            super::super::supervisor_native::process_handle_is_alive_for_test(&held_descendant),
            Some(true)
        );
        sink.stop.notify_one();
        held_exited(&held_descendant).await;
        assert!(!task.is_finished(), "accepted flush must settle after stop");
        sink.flush_resume.notify_one();
        let result = tokio::time::timeout(std::time::Duration::from_secs(15), task)
            .await
            .expect("foreground flush cancellation did not settle")
            .unwrap();
        assert!(
            matches!(result, Err(ProcessError::Io(message)) if message == "supervisor command cancelled")
        );
        assert!(!dir.path().join("descendant.completed").exists());
    }

    #[tokio::test]
    async fn strict_foreground_completion_reaps_redirected_descendant() {
        let dir = tempfile::tempdir().unwrap();
        let _release = ReleaseOnDrop(dir.path().to_path_buf());
        let sink = Arc::new(Sink::default());
        let command = redirected_descendant_command(dir.path(), sink.clone(), true);
        let task = tokio::spawn(async move { run(&command, None, false).await });
        let descendant = descendant_pid(dir.path()).await;
        let held_descendant =
            super::super::supervisor_native::hold_process_handle_for_test(descendant)
                .expect("open descendant process handle");
        assert_eq!(
            super::super::supervisor_native::process_handle_is_alive_for_test(&held_descendant),
            Some(true)
        );
        std::fs::write(dir.path().join("leader.release"), "release").unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(20), task)
            .await
            .expect("strict foreground completion did not settle")
            .unwrap()
            .unwrap();
        assert!(
            matches!(result.outcome, ForegroundOutcome::Completed(output) if !output.timed_out)
        );
        assert_eq!(
            super::super::supervisor_native::process_handle_is_alive_for_test(&held_descendant),
            Some(false)
        );
        assert!(!dir.path().join("descendant.completed").exists());
        held_exited(&held_descendant).await;
    }

    #[tokio::test]
    async fn stop_reaches_job_before_background_handoff_is_published() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(Sink::default());
        sink.pause_append.store(true, Ordering::SeqCst);
        sink.pause_empty.store(true, Ordering::SeqCst);
        let command = command(dir.path(), sink.clone());
        let task = tokio::spawn(async move { run(&command, None, true).await });
        let descendant = descendant_pid(dir.path()).await;
        tokio::time::timeout(
            std::time::Duration::from_secs(20),
            sink.append_started.notified(),
        )
        .await
        .expect("initial output append did not start");
        let pid = sink.pid.load(Ordering::SeqCst);
        assert_ne!(pid, 0);
        sink.stop.notify_one();
        exited(pid).await;
        exited(descendant).await;
        assert!(!task.is_finished());
        sink.append_resume.notify_one();
        let result = tokio::time::timeout(std::time::Duration::from_secs(15), task)
            .await
            .expect("handoff cancellation did not settle")
            .unwrap();
        assert!(
            matches!(result, Err(ProcessError::Io(message)) if message.contains("cancelled before handoff"))
        );
        assert!(sink.exits.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn foreground_deadline_reaches_job_while_output_append_is_pending() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(Sink::default());
        sink.pause_append.store(true, Ordering::SeqCst);
        let command =
            command_with_timeout(dir.path(), sink.clone(), std::time::Duration::from_secs(15))
                .with_auto_background_on_timeout(false);
        let task = tokio::spawn(async move { run(&command, Some(0), false).await });
        let descendant = descendant_pid(dir.path()).await;
        tokio::time::timeout(
            std::time::Duration::from_secs(20),
            sink.append_started.notified(),
        )
        .await
        .expect("output append did not start");
        let pid = sink.pid.load(Ordering::SeqCst);
        assert_ne!(pid, 0);
        exited(pid).await;
        exited(descendant).await;
        assert!(!task.is_finished());
        sink.append_resume.notify_one();
        let result = tokio::time::timeout(std::time::Duration::from_secs(15), task)
            .await
            .expect("foreground deadline did not settle")
            .unwrap()
            .unwrap();
        assert!(matches!(result.outcome, ForegroundOutcome::Completed(output) if output.timed_out));
        assert_eq!(*sink.output.lock().unwrap(), "payload");
    }
}

#[cfg(all(test, unix))]
mod strict_stop_failure_tests {
    use super::*;
    use lingxi_core::host::{BackgroundExitSink, ProcessCommand, SandboxedTag};
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio::sync::Notify;

    #[derive(Default)]
    struct Sink {
        stop: Arc<Notify>,
        started: Notify,
        done: Notify,
        pid: AtomicU32,
        exits: Mutex<Vec<Option<i32>>>,
        failures: Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl BackgroundExitSink for Sink {
        fn stop_notify(&self) -> Option<Arc<Notify>> {
            Some(self.stop.clone())
        }

        async fn on_spawn(&self, _: &str, pid: u32) -> Result<(), ProcessError> {
            self.pid.store(pid, Ordering::SeqCst);
            self.started.notify_one();
            Ok(())
        }

        async fn on_exit(&self, _: &str, code: Option<i32>) {
            self.exits.lock().unwrap().push(code);
            self.done.notify_one();
        }

        async fn on_wait_failure(&self, _: &str, error: &str) {
            self.failures.lock().unwrap().push(error.to_owned());
            self.done.notify_one();
        }
    }

    fn command(dir: &std::path::Path, sink: Arc<Sink>, timeout_ms: u64) -> SandboxedCommand {
        SandboxedCommand::__new_sandboxed(
            ProcessCommand {
                command: "/bin/sh".into(),
                args: vec!["-c".into(), "exec sleep 30".into()],
                cwd: Some(dir.to_path_buf()),
                env: Default::default(),
                stdin: None,
                timeout: Some(std::time::Duration::from_millis(timeout_ms)),
            },
            SandboxedTag::BypassAuditedWithReason {
                reason: "strict Windows runner failure fixture".into(),
            },
        )
        .with_process_owner(Some("host-tool:strict-windows-fixture".into()))
        .with_require_group_cleanup_on_exit(true)
        .with_auto_background_on_timeout(false)
        .with_background_task(BackgroundTaskBinding {
            task_id: "strict-stop".into(),
            output_path: dir.join("output"),
            on_exit: Some(sink),
            on_demand: None,
        })
    }

    fn assert_reaped(pid: u32) {
        assert!(!std::process::Command::new("/bin/kill")
            .args(["-0", &pid.to_string()])
            .status()
            .unwrap()
            .success());
    }

    #[tokio::test]
    async fn background_tree_stop_failure_reaps_child_and_reports_failure() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(Sink::default());
        let command = command(dir.path(), sink.clone(), 30_000);
        let ForegroundOutcome::MovedToBackground(handle) =
            run(&command, None, true).await.unwrap().outcome
        else {
            panic!("explicit background command was not handed off");
        };
        sink.stop.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(3), sink.done.notified())
            .await
            .expect("failed stop did not report completion");
        assert_reaped(handle.pid);
        assert!(sink.exits.lock().unwrap().is_empty());
        let failures = sink.failures.lock().unwrap();
        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains("taskkill"), "{:?}", *failures);
    }

    #[tokio::test]
    async fn backend_lease_is_retained_when_background_cleanup_is_unverified() {
        struct Lease(Arc<AtomicBool>);
        impl Drop for Lease {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(Sink::default());
        let released = Arc::new(AtomicBool::new(false));
        let command =
            command(dir.path(), sink.clone(), 30_000).with_lifetime_guard(Lease(released.clone()));
        let ForegroundOutcome::MovedToBackground(handle) =
            run(&command, None, true).await.unwrap().outcome
        else {
            panic!("explicit background command was not handed off");
        };
        drop(command);
        assert!(!released.load(Ordering::SeqCst));
        sink.stop.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(3), sink.done.notified())
            .await
            .expect("failed stop did not report completion");
        assert_reaped(handle.pid);
        assert!(sink.exits.lock().unwrap().is_empty());
        assert_eq!(sink.failures.lock().unwrap().len(), 1);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !released.load(Ordering::SeqCst),
            "an unverified tree must retain the backend lease"
        );
    }

    #[tokio::test]
    async fn host_timeout_returns_tree_stop_failure_after_reaping_child() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(Sink::default());
        let command = command(dir.path(), sink.clone(), 30);
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            run(&command, None, false),
        )
        .await
        .expect("timeout cleanup did not settle");
        assert!(matches!(result, Err(ProcessError::Io(message)) if message.contains("taskkill")));
        assert_reaped(sink.pid.load(Ordering::SeqCst));
        assert!(sink.exits.lock().unwrap().is_empty());
    }
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

#[cfg(all(test, unix))]
mod foreground_stop_tests {
    use super::*;
    use lingxi_core::host::{
        BackgroundExitSink, BackgroundTaskBinding, ProcessCommand, SandboxedTag,
    };
    use std::sync::Arc;
    struct StopSink {
        stop: Arc<tokio::sync::Notify>,
        started: tokio::sync::Notify,
        pid: std::sync::atomic::AtomicU32,
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
        async fn on_exit(&self, _: &str, _: Option<i32>) {}
    }
    #[tokio::test]
    async fn foreground_stop_joins_child_even_when_tree_termination_reports_failure() {
        let root = tempfile::tempdir().unwrap();
        let sink = Arc::new(StopSink {
            stop: Arc::new(tokio::sync::Notify::new()),
            started: tokio::sync::Notify::new(),
            pid: std::sync::atomic::AtomicU32::new(0),
        });
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
        // The command is a direct exec with no descendants. Probe PID liveness
        // only after the driver's held Child has been explicitly waited/reaped.
        assert!(!std::process::Command::new("/bin/kill")
            .args(["-0", &pid.to_string()])
            .status()
            .unwrap()
            .success());
    }

    #[tokio::test]
    async fn foreground_stop_tree_failure_does_not_wait_for_descendant_held_pipes() {
        struct Descendant(u32);
        impl Drop for Descendant {
            fn drop(&mut self) {
                // This PID belongs to the fixture's still-live sleep. The
                // production runner must report its tree signal failure;
                // only the isolated fixture owner performs this cleanup.
                let _ = std::process::Command::new("/bin/kill")
                    .args(["-KILL", &self.0.to_string()])
                    .status();
            }
        }
        let root = tempfile::tempdir().unwrap();
        let sink = Arc::new(StopSink {
            stop: Arc::new(tokio::sync::Notify::new()),
            started: tokio::sync::Notify::new(),
            pid: std::sync::atomic::AtomicU32::new(0),
        });
        let cmd = SandboxedCommand::__new_sandboxed(
            ProcessCommand {
                command: "/bin/sh".into(),
                args: vec![
                    "-c".into(),
                    "sleep 30 & printf '%s' \"$!\" > descendant.pid; wait".into(),
                ],
                cwd: Some(root.path().to_path_buf()),
                env: Default::default(),
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
        let _cleanup = Descendant(descendant);
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
}
